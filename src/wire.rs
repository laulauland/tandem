//! The HTTP wire format.
//!
//! Three shapes cross the wire, and each is carried the cheapest way that
//! still reads well in a proxy log:
//!
//! * **Object, operation and view payloads** are already prost-encoded jj
//!   protos. They ride as `application/octet-stream` with no re-encoding at
//!   all — the transport carries the bytes jj already agreed on.
//! * **Control-plane structs** (repo info, head state, head updates, prefix
//!   resolution, errors) are JSON with camelCase fields and hex-encoded ids,
//!   the same convention the control socket in `control.rs` uses.
//! * **The object batch** is a hand-rolled length-prefixed binary frame. It
//!   carries many opaque blobs, which JSON would have to base64 first.
//!
//! Only the last of the three is new code, so it is the one the property
//! tests at the bottom of this file target: `decode(encode(x)) == x`, and
//! `decode` of arbitrary bytes neither panics nor allocates on the strength
//! of a length field it has not read yet.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

// ─── Object kinds ─────────────────────────────────────────────────────────────

pub const KIND_COMMIT: u16 = 0;
pub const KIND_TREE: u16 = 1;
pub const KIND_FILE: u16 = 2;
pub const KIND_SYMLINK: u16 = 3;
#[allow(dead_code)]
pub const KIND_COPY: u16 = 4;

/// The path segment that names an object kind in `/api/objects/{kind}/{id}`.
///
/// Kinds travel as words rather than numbers so that a URL stays readable in
/// a cache, a proxy log, or a `curl` by hand.
pub fn kind_name(kind: u16) -> Option<&'static str> {
    match kind {
        KIND_COMMIT => Some("commit"),
        KIND_TREE => Some("tree"),
        KIND_FILE => Some("file"),
        KIND_SYMLINK => Some("symlink"),
        KIND_COPY => Some("copy"),
        _ => None,
    }
}

pub fn kind_from_name(name: &str) -> Option<u16> {
    match name {
        "commit" => Some(KIND_COMMIT),
        "tree" => Some(KIND_TREE),
        "file" => Some(KIND_FILE),
        "symlink" => Some(KIND_SYMLINK),
        "copy" => Some(KIND_COPY),
        _ => None,
    }
}

/// The same segment back, once it has been proven to name a kind.
///
/// A validated path segment is one of five known words, so it needs no
/// allocation and no lifetime tied to the request that carried it.
pub fn canonical_kind_name(name: &str) -> Option<&'static str> {
    kind_from_name(name).and_then(kind_name)
}

// ─── The compatibility handshake ──────────────────────────────────────────────
//
// A client refuses a server outright over these, so both sides have to spell
// them the same way. They live here, next to the kind names, for the same
// reason: a typo on one side of a string that is never compared at compile
// time is a silent mismatch, and one definition cannot be mistyped twice.

pub const PROTOCOL_MAJOR: u16 = 0;
pub const PROTOCOL_MINOR: u16 = 1;
pub const BACKEND_NAME: &str = "tandem";
pub const OP_STORE_NAME: &str = "tandem_op_store";

/// An optional feature a server advertises in `/api/info`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum RepoCapability {
    WatchHeads,
    HeadsSnapshot,
    CopyTracking,
}

impl RepoCapability {
    pub fn as_str(self) -> &'static str {
        match self {
            RepoCapability::WatchHeads => "watchHeads",
            RepoCapability::HeadsSnapshot => "headsSnapshot",
            RepoCapability::CopyTracking => "copyTracking",
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "watchHeads" => Some(RepoCapability::WatchHeads),
            "headsSnapshot" => Some(RepoCapability::HeadsSnapshot),
            "copyTracking" => Some(RepoCapability::CopyTracking),
            _ => None,
        }
    }
}

// ─── Headers and media types ──────────────────────────────────────────────────

pub const CONTENT_TYPE_OCTETS: &str = "application/octet-stream";
pub const CONTENT_TYPE_BATCH: &str = "application/vnd.tandem.batch";

/// Content-addressed responses never change, so they are cacheable forever.
pub const IMMUTABLE_CACHE_CONTROL: &str = "public, max-age=31536000, immutable";

/// The id a write produced. A write answers with the normalized bytes in the
/// body, so the id it hashed to travels in a header next to them.
pub const HEADER_OBJECT_ID: &str = "tandem-object-id";
pub const HEADER_OPERATION_ID: &str = "tandem-operation-id";
pub const HEADER_VIEW_ID: &str = "tandem-view-id";

// ─── Control-plane structs ────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RepoInfoBody {
    pub protocol_major: u16,
    pub protocol_minor: u16,
    pub tandem_version: String,
    pub backend_name: String,
    pub op_store_name: String,
    pub commit_id_length: u32,
    pub change_id_length: u32,
    /// Hex-encoded.
    pub root_commit_id: String,
    pub root_change_id: String,
    pub empty_tree_id: String,
    pub root_operation_id: String,
    pub capabilities: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HeadsBody {
    pub version: u64,
    /// Hex-encoded operation ids.
    pub heads: Vec<String>,
    /// Workspace name → hex-encoded operation id.
    #[serde(default)]
    pub workspace_heads: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateHeadsBody {
    /// Hex-encoded operation ids this update replaces.
    #[serde(default)]
    pub old_ids: Vec<String>,
    /// Hex-encoded operation id this update publishes.
    pub new_id: String,
    /// The workspace the publishing client speaks for, if it named one.
    #[serde(default)]
    pub workspace_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PrefixBody {
    /// `noMatch`, `singleMatch` or `ambiguous`.
    pub resolution: String,
    /// Hex-encoded, present only for `singleMatch`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ErrorBody {
    pub error: String,
}

/// One wake-up on `/api/events`.
///
/// The version is a hint about how far the server has moved, not the head
/// data: a watcher reads `/api/heads` to learn the head set. Wake-ups may
/// coalesce, so a watcher must be able to skip versions it never saw.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HeadsEventBody {
    pub version: u64,
}

// ─── The object batch frame ───────────────────────────────────────────────────

const REQUEST_MAGIC: &[u8; 4] = b"TBQ1";
const RESPONSE_MAGIC: &[u8; 4] = b"TBS1";

/// A cap on how many records one frame may claim, checked before any
/// per-record allocation. It exists so that a corrupt or hostile count field
/// cannot make the decoder reserve memory for records the frame is far too
/// short to contain.
const MAX_RECORDS: usize = 1 << 20;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BatchItem {
    pub kind: u16,
    pub data: Vec<u8>,
}

/// What writing one batch item produced. A failure is per-item: one bad blob
/// does not fail the blobs next to it in the same frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BatchOutcome {
    Written { id: Vec<u8>, normalized: Vec<u8> },
    Failed { message: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WireError(pub String);

impl std::fmt::Display for WireError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for WireError {}

fn err(message: impl Into<String>) -> WireError {
    WireError(message.into())
}

/// A cursor that will not hand out more bytes than it actually holds.
///
/// Every length in a frame is read through `take`, which checks the length
/// against the bytes still in front of it before it copies anything. That is
/// the whole defence against a length field that claims four gigabytes: the
/// decoder never sizes an allocation from a number it has not first proven
/// the frame can back.
struct Reader<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, pos: 0 }
    }

    fn remaining(&self) -> usize {
        self.bytes.len() - self.pos
    }

    fn take(&mut self, len: usize) -> Result<&'a [u8], WireError> {
        if len > self.remaining() {
            return Err(err(format!(
                "truncated frame: wanted {len} bytes, {} left",
                self.remaining()
            )));
        }
        let slice = &self.bytes[self.pos..self.pos + len];
        self.pos += len;
        Ok(slice)
    }

    fn u8(&mut self) -> Result<u8, WireError> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16, WireError> {
        let raw = self.take(2)?;
        Ok(u16::from_le_bytes([raw[0], raw[1]]))
    }

    fn u32(&mut self) -> Result<u32, WireError> {
        let raw = self.take(4)?;
        Ok(u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]))
    }

    /// A length-prefixed byte string.
    fn blob(&mut self) -> Result<Vec<u8>, WireError> {
        let len = self.u32()? as usize;
        Ok(self.take(len)?.to_vec())
    }

    /// The record count, checked against what the rest of the frame could
    /// possibly hold before anyone reserves memory for it.
    ///
    /// The count is consumed, so the reader is left on the first record.
    fn record_count(&mut self, min_record_bytes: usize) -> Result<usize, WireError> {
        let count = self.u32()? as usize;
        if count > MAX_RECORDS {
            return Err(err(format!("frame claims {count} records; too many")));
        }
        let smallest_possible = count.saturating_mul(min_record_bytes);
        if smallest_possible > self.remaining() {
            return Err(err(format!(
                "frame claims {count} records but holds only {} bytes",
                self.remaining()
            )));
        }
        Ok(count)
    }

    fn finish(self) -> Result<(), WireError> {
        if self.remaining() != 0 {
            return Err(err(format!("{} trailing bytes in frame", self.remaining())));
        }
        Ok(())
    }
}

fn push_blob(out: &mut Vec<u8>, bytes: &[u8]) {
    out.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
    out.extend_from_slice(bytes);
}

/// `TBQ1 | count:u32 | { kind:u16, len:u32, data }*`
pub fn encode_batch_request(items: &[BatchItem]) -> Vec<u8> {
    let mut out = Vec::with_capacity(8 + items.iter().map(|i| i.data.len() + 6).sum::<usize>());
    out.extend_from_slice(REQUEST_MAGIC);
    out.extend_from_slice(&(items.len() as u32).to_le_bytes());
    for item in items {
        out.extend_from_slice(&item.kind.to_le_bytes());
        push_blob(&mut out, &item.data);
    }
    out
}

pub fn decode_batch_request(bytes: &[u8]) -> Result<Vec<BatchItem>, WireError> {
    let mut reader = Reader::new(bytes);
    if reader.take(4)? != REQUEST_MAGIC {
        return Err(err("not a tandem batch request frame"));
    }
    let count = reader.record_count(6)?;

    let mut items = Vec::with_capacity(count);
    for _ in 0..count {
        let kind = reader.u16()?;
        let data = reader.blob()?;
        items.push(BatchItem { kind, data });
    }
    reader.finish()?;
    Ok(items)
}

/// `TBS1 | count:u32 | { status:u8, blob, blob }*`
///
/// A written record carries its id and its normalized bytes; a failed record
/// carries an empty id and the message in the second blob.
pub fn encode_batch_response(outcomes: &[BatchOutcome]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(RESPONSE_MAGIC);
    out.extend_from_slice(&(outcomes.len() as u32).to_le_bytes());
    for outcome in outcomes {
        match outcome {
            BatchOutcome::Written { id, normalized } => {
                out.push(0);
                push_blob(&mut out, id);
                push_blob(&mut out, normalized);
            }
            BatchOutcome::Failed { message } => {
                out.push(1);
                push_blob(&mut out, &[]);
                push_blob(&mut out, message.as_bytes());
            }
        }
    }
    out
}

pub fn decode_batch_response(bytes: &[u8]) -> Result<Vec<BatchOutcome>, WireError> {
    let mut reader = Reader::new(bytes);
    if reader.take(4)? != RESPONSE_MAGIC {
        return Err(err("not a tandem batch response frame"));
    }
    let count = reader.record_count(9)?;

    let mut outcomes = Vec::with_capacity(count);
    for _ in 0..count {
        let status = reader.u8()?;
        let id = reader.blob()?;
        let payload = reader.blob()?;
        match status {
            0 => outcomes.push(BatchOutcome::Written {
                id,
                normalized: payload,
            }),
            1 => outcomes.push(BatchOutcome::Failed {
                message: String::from_utf8(payload)
                    .map_err(|_| err("batch failure message is not valid UTF-8"))?,
            }),
            other => return Err(err(format!("unknown batch record status {other}"))),
        }
    }
    reader.finish()?;
    Ok(outcomes)
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
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

        /// The same, for bytes that got the magic right and are therefore
        /// carried further into the decoder than random noise ever reaches.
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
    }

    /// A frame that claims a million records in twelve bytes is rejected on
    /// the count, before anything is reserved for the records it promises.
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
}
