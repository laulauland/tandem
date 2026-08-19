//! The HTTP transport's serialization layer.
//!
//! Two shapes travel over the wire: JSON bodies, which serde owns, and the
//! hand-rolled batch frame, which tandem owns. The JSON bodies are checked for
//! round-tripping because a field that silently renames itself breaks every
//! older client. The batch frame is checked for that and for what it does with
//! bytes it did not write, because it is the one decoder facing a length field
//! it has no reason to trust.

use jj_tandem::wire::*;
use proptest::prelude::*;

fn batch_item() -> impl Strategy<Value = BatchItem> {
    (0u16..8, proptest::collection::vec(any::<u8>(), 0..256))
        .prop_map(|(kind, data)| BatchItem { kind, data })
}

fn batch_outcome() -> impl Strategy<Value = BatchOutcome> {
    prop_oneof![
        (
            proptest::collection::vec(any::<u8>(), 0..64),
            proptest::collection::vec(any::<u8>(), 0..256),
        )
            .prop_map(|(id, normalized)| BatchOutcome::Written { id, normalized }),
        ".{0,120}".prop_map(|message| BatchOutcome::Failed { message }),
    ]
}

proptest! {
    #[test]
    fn batch_request_round_trips(items in proptest::collection::vec(batch_item(), 0..24)) {
        let encoded = encode_batch_request(&items);
        prop_assert_eq!(decode_batch_request(&encoded).unwrap(), items);
    }

    #[test]
    fn batch_response_round_trips(
        outcomes in proptest::collection::vec(batch_outcome(), 0..24),
    ) {
        let encoded = encode_batch_response(&outcomes);
        prop_assert_eq!(decode_batch_response(&encoded).unwrap(), outcomes);
    }

    /// Arbitrary bytes are a decode error, never a panic and never a
    /// half-gigabyte allocation.
    #[test]
    fn batch_decoders_survive_arbitrary_bytes(
        bytes in proptest::collection::vec(any::<u8>(), 0..512),
    ) {
        let _ = decode_batch_request(&bytes);
        let _ = decode_batch_response(&bytes);
    }

    /// The same, for bytes that got the magic right and are therefore carried
    /// further into the decoder than random noise ever reaches.
    #[test]
    fn batch_decoders_survive_plausible_frames(
        tail in proptest::collection::vec(any::<u8>(), 0..512),
    ) {
        let mut request = REQUEST_MAGIC.to_vec();
        request.extend_from_slice(&tail);
        let _ = decode_batch_request(&request);

        let mut response = RESPONSE_MAGIC.to_vec();
        response.extend_from_slice(&tail);
        let _ = decode_batch_response(&response);
    }

    /// A truncated frame is an error, wherever the cut fell.
    #[test]
    fn a_truncated_batch_request_is_rejected(
        items in proptest::collection::vec(batch_item(), 1..8),
        cut in 0.0f64..1.0,
    ) {
        let encoded = encode_batch_request(&items);
        let keep = (encoded.len() as f64 * cut) as usize;
        prop_assume!(keep < encoded.len());
        prop_assert!(decode_batch_request(&encoded[..keep]).is_err());
    }

    #[test]
    fn repo_info_round_trips(
        protocol_major in any::<u16>(),
        protocol_minor in any::<u16>(),
        backend_name in "[a-z_]{0,16}",
        capabilities in proptest::collection::vec("[a-zA-Z]{1,12}", 0..4),
    ) {
        let body = RepoInfoBody {
            protocol_major,
            protocol_minor,
            tandem_version: "0.0.0".to_string(),
            backend_name,
            op_store_name: "tandem_op_store".to_string(),
            commit_id_length: 20,
            change_id_length: 16,
            root_commit_id: "00".repeat(20),
            root_change_id: "00".repeat(16),
            empty_tree_id: "00".repeat(20),
            root_operation_id: "00".repeat(64),
            capabilities,
        };
        let json = serde_json::to_vec(&body).unwrap();
        prop_assert_eq!(serde_json::from_slice::<RepoInfoBody>(&json).unwrap(), body);
    }

    #[test]
    fn heads_body_round_trips(
        version in any::<u64>(),
        heads in proptest::collection::vec("[0-9a-f]{0,32}", 0..8),
        workspaces in proptest::collection::vec(("[a-z]{1,8}", "[0-9a-f]{0,32}"), 0..8),
    ) {
        let body = HeadsBody {
            version,
            heads,
            workspace_heads: workspaces.into_iter().collect(),
        };
        let json = serde_json::to_vec(&body).unwrap();
        prop_assert_eq!(serde_json::from_slice::<HeadsBody>(&json).unwrap(), body);
    }

    #[test]
    fn update_heads_body_round_trips(
        old_ids in proptest::collection::vec("[0-9a-f]{0,32}", 0..8),
        new_id in "[0-9a-f]{0,32}",
        workspace_id in "[a-z]{0,8}",
    ) {
        let body = UpdateHeadsBody { old_ids, new_id, workspace_id };
        let json = serde_json::to_vec(&body).unwrap();
        prop_assert_eq!(serde_json::from_slice::<UpdateHeadsBody>(&json).unwrap(), body);
    }

    /// Arbitrary JSON never panics a body decoder.
    #[test]
    fn body_decoders_survive_arbitrary_bytes(
        bytes in proptest::collection::vec(any::<u8>(), 0..256),
    ) {
        let _ = serde_json::from_slice::<RepoInfoBody>(&bytes);
        let _ = serde_json::from_slice::<HeadsBody>(&bytes);
        let _ = serde_json::from_slice::<UpdateHeadsBody>(&bytes);
        let _ = serde_json::from_slice::<HeadsEventBody>(&bytes);
        let _ = serde_json::from_slice::<ErrorBody>(&bytes);
        let _ = serde_json::from_slice::<PrefixBody>(&bytes);
    }
}

/// A frame that claims a million records in twelve bytes is rejected on the
/// count, before anything is reserved for the records it promises.
#[test]
fn a_lying_record_count_is_rejected_without_allocating() {
    let mut frame = REQUEST_MAGIC.to_vec();
    frame.extend_from_slice(&u32::MAX.to_le_bytes());
    frame.extend_from_slice(&[0u8; 4]);
    let error = decode_batch_request(&frame).expect_err("must reject");
    assert!(
        error.to_string().contains("too many"),
        "unexpected error: {error}"
    );

    let mut smaller = REQUEST_MAGIC.to_vec();
    smaller.extend_from_slice(&1000u32.to_le_bytes());
    smaller.extend_from_slice(&[0u8; 8]);
    let error = decode_batch_request(&smaller).expect_err("must reject");
    assert!(
        error.to_string().contains("holds only"),
        "unexpected error: {error}"
    );
}

/// A blob length longer than the frame is a truncation error, not a
/// four-gigabyte reservation.
#[test]
fn a_lying_blob_length_is_rejected() {
    let mut frame = REQUEST_MAGIC.to_vec();
    frame.extend_from_slice(&1u32.to_le_bytes());
    frame.extend_from_slice(&0u16.to_le_bytes());
    frame.extend_from_slice(&u32::MAX.to_le_bytes());
    let error = decode_batch_request(&frame).expect_err("must reject");
    assert!(
        error.to_string().contains("truncated"),
        "unexpected error: {error}"
    );
}

#[test]
fn object_kind_names_round_trip() {
    for kind in 0..5u16 {
        let name = kind_name(kind).expect("known kind");
        assert_eq!(kind_from_name(name), Some(kind));
    }
    assert_eq!(kind_name(9), None);
    assert_eq!(kind_from_name("banana"), None);
    assert_eq!(canonical_kind_name("file"), Some("file"));
    assert_eq!(canonical_kind_name("banana"), None);
}

#[test]
fn capability_names_round_trip() {
    for capability in [
        RepoCapability::WatchHeads,
        RepoCapability::HeadsSnapshot,
        RepoCapability::CopyTracking,
    ] {
        assert_eq!(
            RepoCapability::from_name(capability.as_str()),
            Some(capability)
        );
    }
    assert_eq!(RepoCapability::from_name("watchheads"), None);
}
