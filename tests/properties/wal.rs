//! The WAL framing: what the durable log is written in.
//!
//! This is the one format tandem reads back from storage it does not control,
//! so it is the one format where "arbitrary bytes" is a realistic input. Two
//! properties matter. Anything encoded decodes to itself, or the log is a lie.
//! And anything else is an error — not a panic, and not a reservation the
//! length field asked for and the file could never fill.

use std::collections::BTreeMap;

use jj_tandem::wal::{wal_key, IndexObject, RecordKind, WalEntry, WalRecord, MAGIC};
use proptest::prelude::*;

fn record_kind() -> impl Strategy<Value = RecordKind> {
    prop_oneof![
        Just(RecordKind::File),
        Just(RecordKind::Symlink),
        Just(RecordKind::Tree),
        Just(RecordKind::Commit),
        Just(RecordKind::View),
        Just(RecordKind::Operation),
    ]
}

fn wal_record() -> impl Strategy<Value = WalRecord> {
    (
        record_kind(),
        proptest::collection::vec(any::<u8>(), 0..40),
        proptest::collection::vec(any::<u8>(), 0..2048),
    )
        .prop_map(|(kind, id, data)| WalRecord { kind, id, data })
}

fn wal_entry() -> impl Strategy<Value = WalEntry> {
    (
        proptest::collection::vec(any::<u8>(), 0..64),
        proptest::collection::vec(proptest::collection::vec(any::<u8>(), 0..64), 0..4),
        proptest::collection::vec(wal_record(), 0..8),
    )
        .prop_map(|(op_id, parents, records)| WalEntry {
            op_id,
            parents,
            records,
        })
}

proptest! {
    #[test]
    fn a_wal_entry_decodes_to_itself(entry in wal_entry()) {
        let encoded = entry.encode().expect("encode");
        prop_assert_eq!(WalEntry::decode(&encoded).expect("decode"), entry);
    }

    /// Arbitrary bytes are an error. Never a panic, and never an allocation
    /// the input only claimed to justify.
    #[test]
    fn arbitrary_bytes_never_panic(bytes in proptest::collection::vec(any::<u8>(), 0..512)) {
        let _ = WalEntry::decode(&bytes);
    }

    /// The same, for bytes that got the magic right. Random noise almost never
    /// reaches the length fields; these always do.
    #[test]
    fn plausible_frames_never_panic(tail in proptest::collection::vec(any::<u8>(), 0..512)) {
        let mut frame = MAGIC.to_vec();
        frame.extend_from_slice(&tail);
        let _ = WalEntry::decode(&frame);
    }

    /// A truncated entry is rejected, whatever it was truncated in the middle
    /// of. Silently returning the prefix would replay half an operation.
    #[test]
    fn a_truncated_entry_is_rejected(
        entry in wal_entry(),
        cut in 0.0f64..1.0,
    ) {
        let encoded = entry.encode().expect("encode");
        let keep = (encoded.len() as f64 * cut) as usize;
        prop_assume!(keep < encoded.len());
        prop_assert!(WalEntry::decode(&encoded[..keep]).is_err());
    }

    /// Trailing bytes are rejected too: an entry that ignored them would let
    /// a corrupt file decode as a valid one.
    #[test]
    fn trailing_bytes_are_rejected(
        entry in wal_entry(),
        tail in proptest::collection::vec(any::<u8>(), 1..16),
    ) {
        let mut encoded = entry.encode().expect("encode");
        encoded.extend_from_slice(&tail);
        prop_assert!(WalEntry::decode(&encoded).is_err());
    }

    #[test]
    fn an_index_object_decodes_to_itself(
        version in any::<u64>(),
        op_heads in proptest::collection::vec("[0-9a-f]{0,64}", 0..6),
        workspaces in proptest::collection::vec(("[a-z]{1,10}", "[0-9a-f]{0,64}"), 0..6),
    ) {
        let index = IndexObject {
            version,
            op_heads,
            workspace_heads: workspaces.into_iter().collect::<BTreeMap<_, _>>(),
        };
        let encoded = index.encode().expect("encode");
        prop_assert_eq!(IndexObject::decode(&encoded).expect("decode"), index);
    }

    /// Arbitrary JSON-shaped bytes are an error, not a panic.
    #[test]
    fn an_arbitrary_index_object_never_panics(
        bytes in proptest::collection::vec(any::<u8>(), 0..256),
    ) {
        let _ = IndexObject::decode(&bytes);
    }

    /// Record kinds and object kinds are the same set seen from two sides.
    #[test]
    fn record_kinds_round_trip_through_object_kinds(kind in record_kind()) {
        if let Some(name) = kind.object_kind() {
            prop_assert_eq!(RecordKind::from_object_kind(name), Some(kind));
        }
    }
}

/// A count read off the wire must not be trusted as an allocation size. The
/// generated cases above cannot reach four billion; this one names it.
#[test]
fn a_claimed_count_is_not_an_allocation() {
    let mut hostile = MAGIC.to_vec();
    hostile.extend_from_slice(&0u32.to_be_bytes()); // empty op id
    hostile.extend_from_slice(&u32::MAX.to_be_bytes()); // "four billion parents"
    assert!(WalEntry::decode(&hostile).is_err());

    let mut hostile_records = MAGIC.to_vec();
    hostile_records.extend_from_slice(&0u32.to_be_bytes());
    hostile_records.extend_from_slice(&0u32.to_be_bytes());
    hostile_records.extend_from_slice(&u32::MAX.to_be_bytes());
    assert!(WalEntry::decode(&hostile_records).is_err());
}

#[test]
fn wrong_magic_is_rejected() {
    let entry = WalEntry {
        op_id: vec![1],
        parents: Vec::new(),
        records: Vec::new(),
    };
    let mut encoded = entry.encode().expect("encode");
    encoded[0] = b'X';
    assert!(WalEntry::decode(&encoded).is_err());
}

/// Older objects, written before the workspace map existed, still parse.
#[test]
fn a_partial_index_object_still_parses() {
    let minimal = IndexObject::decode(br#"{"version":3}"#).expect("decode");
    assert_eq!(minimal.version, 3);
    assert!(minimal.op_heads.is_empty());
    assert!(minimal.workspace_heads.is_empty());
}

#[test]
fn a_wal_key_is_the_operation_id() {
    assert_eq!(wal_key("deadbeef"), "wal/deadbeef");
}
