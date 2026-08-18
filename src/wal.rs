//! WAL entry and index object formats for the bucket.
//!
//! One publish = one immutable WAL entry, keyed by the operation id, holding
//! the operation, its view, and the blobs that arrived since the previous
//! publish. The op-heads set is one index object, updated by compare-and-swap.
//!
//! On blob attribution: the blob list of an entry is "everything written to the
//! server since the last publish", not "everything this operation introduced".
//! The three client store traits each hold their own connection, so there is no
//! session to attribute blobs to, and under concurrency one agent's entry can
//! carry another agent's not-yet-published blobs. That is intentional. The
//! invariant is that every blob reachable from a published head is durable in
//! the bucket before that head's index write is acknowledged — not a one-to-one
//! mapping between operations and the blobs they introduce. Blobs are
//! content-addressed, so a duplicate write is a no-op.
//!
//! The framing is hand-rolled rather than Cap'n Proto: this is a storage format
//! that outlives the RPC schema.

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

// ─── Bucket keys ──────────────────────────────────────────────────────────────

/// The single compare-and-swapped object naming the current op heads.
pub const INDEX_KEY: &str = "index/heads.json";

/// Key of the immutable WAL entry for an operation.
pub fn wal_key(op_hex: &str) -> String {
    format!("wal/{op_hex}")
}

// ─── WAL entry ────────────────────────────────────────────────────────────────

const MAGIC: &[u8; 8] = b"TDMWAL\0\x01";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordKind {
    Operation,
    View,
    File,
    Tree,
    Commit,
    Symlink,
}

impl RecordKind {
    fn tag(self) -> u8 {
        match self {
            RecordKind::Operation => 1,
            RecordKind::View => 2,
            RecordKind::File => 3,
            RecordKind::Tree => 4,
            RecordKind::Commit => 5,
            RecordKind::Symlink => 6,
        }
    }

    fn from_tag(tag: u8) -> Result<Self> {
        Ok(match tag {
            1 => RecordKind::Operation,
            2 => RecordKind::View,
            3 => RecordKind::File,
            4 => RecordKind::Tree,
            5 => RecordKind::Commit,
            6 => RecordKind::Symlink,
            other => bail!("unknown WAL record kind: {other}"),
        })
    }

    /// The object-store kind name used by the server's put/get paths, for the
    /// record kinds that are backend objects rather than op-store entries.
    pub fn object_kind(self) -> Option<&'static str> {
        match self {
            RecordKind::File => Some("file"),
            RecordKind::Tree => Some("tree"),
            RecordKind::Commit => Some("commit"),
            RecordKind::Symlink => Some("symlink"),
            RecordKind::Operation | RecordKind::View => None,
        }
    }

    pub fn from_object_kind(kind: &str) -> Option<Self> {
        match kind {
            "file" => Some(RecordKind::File),
            "tree" => Some(RecordKind::Tree),
            "commit" => Some(RecordKind::Commit),
            "symlink" => Some(RecordKind::Symlink),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WalRecord {
    pub kind: RecordKind,
    pub id: Vec<u8>,
    pub data: Vec<u8>,
}

/// One publish, as it is stored in the bucket.
///
/// Records are kept in write order — blobs first, then the view, then the
/// operation — so replaying them writes dependencies before dependents.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WalEntry {
    pub op_id: Vec<u8>,
    pub parents: Vec<Vec<u8>>,
    pub records: Vec<WalRecord>,
}

impl WalEntry {
    /// Fails rather than truncating when a count or a payload does not fit the
    /// 32-bit framing. A silently truncated length would write an entry that
    /// decodes into different bytes than it was given — corruption that only
    /// shows up on replay.
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        out.extend_from_slice(MAGIC);
        push_bytes(&mut out, &self.op_id)?;
        push_u32(&mut out, self.parents.len())?;
        for parent in &self.parents {
            push_bytes(&mut out, parent)?;
        }
        push_u32(&mut out, self.records.len())?;
        for record in &self.records {
            out.push(record.kind.tag());
            push_bytes(&mut out, &record.id)?;
            push_bytes(&mut out, &record.data)?;
        }
        Ok(out)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut cursor = Cursor { bytes, pos: 0 };
        let magic = cursor.take(MAGIC.len())?;
        if magic != MAGIC {
            bail!("not a tandem WAL entry (bad magic)");
        }
        let op_id = cursor.take_bytes()?.to_vec();
        // Counts come off the wire, so they are only a hint until the bytes
        // behind them are read: reserve against what is actually there.
        let parent_count = cursor.take_u32()?;
        let mut parents = Vec::with_capacity(cursor.plausible_count(parent_count));
        for _ in 0..parent_count {
            parents.push(cursor.take_bytes()?.to_vec());
        }
        let record_count = cursor.take_u32()?;
        let mut records = Vec::with_capacity(cursor.plausible_count(record_count));
        for _ in 0..record_count {
            let kind = RecordKind::from_tag(cursor.take_u8()?)?;
            let id = cursor.take_bytes()?.to_vec();
            let data = cursor.take_bytes()?.to_vec();
            records.push(WalRecord { kind, id, data });
        }
        if cursor.pos != bytes.len() {
            bail!("trailing bytes after WAL entry");
        }
        Ok(Self {
            op_id,
            parents,
            records,
        })
    }
}

fn push_u32(out: &mut Vec<u8>, value: usize) -> Result<()> {
    let value = u32::try_from(value)
        .map_err(|_| anyhow!("WAL entry field does not fit 32-bit framing: {value}"))?;
    out.extend_from_slice(&value.to_be_bytes());
    Ok(())
}

fn push_bytes(out: &mut Vec<u8>, value: &[u8]) -> Result<()> {
    push_u32(out, value.len())?;
    out.extend_from_slice(value);
    Ok(())
}

struct Cursor<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn take(&mut self, len: usize) -> Result<&'a [u8]> {
        let end = self
            .pos
            .checked_add(len)
            .ok_or_else(|| anyhow!("WAL entry length overflow"))?;
        if end > self.bytes.len() {
            bail!("WAL entry truncated");
        }
        let slice = &self.bytes[self.pos..end];
        self.pos = end;
        Ok(slice)
    }

    fn take_u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    fn take_u32(&mut self) -> Result<usize> {
        let raw = self.take(4)?;
        Ok(u32::from_be_bytes([raw[0], raw[1], raw[2], raw[3]]) as usize)
    }

    fn take_bytes(&mut self) -> Result<&'a [u8]> {
        let len = self.take_u32()?;
        self.take(len)
    }

    /// An allocation hint that a corrupt or hostile count cannot inflate: every
    /// item costs at least a length prefix, so no more items can follow than
    /// there are quarter-words left in the buffer.
    fn plausible_count(&self, claimed: usize) -> usize {
        let remaining = self.bytes.len().saturating_sub(self.pos);
        claimed.min(remaining / 4)
    }
}

// ─── Index object ─────────────────────────────────────────────────────────────

/// The bucket's view of the repository: which operations are heads, at which
/// CAS version, and which workspace points at which operation.
///
/// The field names mirror `.jj/repo/tandem/heads.json` so the wire protocol's
/// `version` keeps its meaning on both sides.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IndexObject {
    pub version: u64,
    #[serde(default)]
    pub op_heads: Vec<String>,
    #[serde(default)]
    pub workspace_heads: BTreeMap<String, String>,
}

impl IndexObject {
    pub fn encode(&self) -> Result<Vec<u8>> {
        serde_json::to_vec_pretty(self).context("encode index object")
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        serde_json::from_slice(bytes).context("decode index object")
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wal_entry_round_trips() {
        let entry = WalEntry {
            op_id: vec![0xab, 0xcd],
            parents: vec![vec![0x01], vec![0x02, 0x03]],
            records: vec![
                WalRecord {
                    kind: RecordKind::File,
                    id: vec![9, 9],
                    data: b"hello".to_vec(),
                },
                WalRecord {
                    kind: RecordKind::View,
                    id: vec![7],
                    data: b"view-proto".to_vec(),
                },
                WalRecord {
                    kind: RecordKind::Operation,
                    id: vec![0xab, 0xcd],
                    data: b"op-proto".to_vec(),
                },
            ],
        };
        let encoded = entry.encode().unwrap();
        assert_eq!(WalEntry::decode(&encoded).unwrap(), entry);
    }

    #[test]
    fn wal_entry_with_no_records_round_trips() {
        let entry = WalEntry {
            op_id: vec![1, 2, 3],
            parents: Vec::new(),
            records: Vec::new(),
        };
        assert_eq!(WalEntry::decode(&entry.encode().unwrap()).unwrap(), entry);
    }

    #[test]
    fn wal_entry_rejects_corruption() {
        let entry = WalEntry {
            op_id: vec![1],
            parents: Vec::new(),
            records: Vec::new(),
        };
        let encoded = entry.encode().unwrap();
        assert!(WalEntry::decode(&encoded[..encoded.len() - 1]).is_err());

        let mut wrong_magic = encoded.clone();
        wrong_magic[0] = b'X';
        assert!(WalEntry::decode(&wrong_magic).is_err());

        let mut trailing = encoded.clone();
        trailing.push(0);
        assert!(WalEntry::decode(&trailing).is_err());
    }

    /// A count read off the wire must not be trusted as an allocation size.
    #[test]
    fn wal_entry_does_not_allocate_on_a_claimed_count() {
        let mut hostile = Vec::new();
        hostile.extend_from_slice(MAGIC);
        hostile.extend_from_slice(&0u32.to_be_bytes()); // empty op id
        hostile.extend_from_slice(&u32::MAX.to_be_bytes()); // "four billion parents"
        assert!(WalEntry::decode(&hostile).is_err());

        let mut hostile_records = Vec::new();
        hostile_records.extend_from_slice(MAGIC);
        hostile_records.extend_from_slice(&0u32.to_be_bytes());
        hostile_records.extend_from_slice(&0u32.to_be_bytes());
        hostile_records.extend_from_slice(&u32::MAX.to_be_bytes());
        assert!(WalEntry::decode(&hostile_records).is_err());
    }

    /// The 32-bit framing is a hard limit, not a truncation point.
    #[test]
    fn wal_entry_rejects_a_field_too_large_for_the_framing() {
        let mut lengths = Vec::new();
        // A record id of exactly u32::MAX + 1 bytes cannot be framed. Building
        // one is not worth 4 GiB of RAM, so exercise the check directly.
        assert!(push_u32(&mut lengths, u32::MAX as usize + 1).is_err());
        assert!(push_u32(&mut lengths, u32::MAX as usize).is_ok());
    }

    #[test]
    fn index_object_round_trips() {
        let index = IndexObject {
            version: 42,
            op_heads: vec!["aa".into(), "bb".into()],
            workspace_heads: BTreeMap::from([("agent-a".to_string(), "aa".to_string())]),
        };
        let encoded = index.encode().unwrap();
        assert_eq!(IndexObject::decode(&encoded).unwrap(), index);

        // Older/partial objects still parse.
        let minimal = IndexObject::decode(br#"{"version":3}"#).unwrap();
        assert_eq!(minimal.version, 3);
        assert!(minimal.op_heads.is_empty());
    }

    #[test]
    fn wal_key_is_the_operation_id() {
        assert_eq!(wal_key("deadbeef"), "wal/deadbeef");
    }
}
