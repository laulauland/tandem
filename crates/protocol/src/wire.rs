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
//! tests in `tests/wire.rs` target: `decode(encode(x)) == x`, and `decode` of
//! arbitrary bytes neither panics nor allocates on the strength of a length
//! field it has not read yet.

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

// ─── Tokens and the writer role ───────────────────────────────────────────────

/// `POST /api/tokens` — the admin token asking for a workspace-scoped one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MintTokenBody {
    /// The workspace the new token speaks for.
    pub workspace_id: String,
    /// How long it should live. The server clamps it and answers with what it
    /// actually granted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl_seconds: Option<u64>,
}

/// What a mint answers with. The bearer appears here and nowhere else — the
/// server writes it down nowhere, so this response is the only copy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenBody {
    pub token: String,
    pub workspace_id: String,
    /// Seconds from the moment the server answered.
    pub ttl_seconds: u64,
}

/// `POST /api/workspaces/{id}/writer` — claiming or renewing the writer role.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClaimWriterBody {
    /// Who is asking. The same holder asking again is renewing; anybody else
    /// is taking over, which only an expired claim allows.
    pub holder: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl_seconds: Option<u64>,
}

/// Who holds the writer role for a workspace, and for how much longer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WriterRoleBody {
    pub workspace_id: String,
    pub holder: String,
    /// Seconds from the moment the server answered. A holder that wants to
    /// keep the role asks again before this runs out.
    pub expires_in_seconds: u64,
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

pub const REQUEST_MAGIC: &[u8; 4] = b"TBQ1";
pub const RESPONSE_MAGIC: &[u8; 4] = b"TBS1";

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

#[cfg(test)]
mod operation_upload_tests {
    #[test]
    fn combined_upload_preserves_bytes_and_rejects_truncation() {
        let encoded = super::encode_operation_upload(b"view", b"operation");
        assert_eq!(
            super::decode_operation_upload(&encoded).unwrap(),
            (&b"view"[..], &b"operation"[..])
        );
        for length in 0..8 {
            assert!(super::decode_operation_upload(&encoded[..length]).is_err());
        }
        assert!(super::decode_operation_upload(&[255; 4]).is_err());
    }
}

/// HTTP bodies stay bounded on both sides of the transport.
pub const MAX_REQUEST_BODY_BYTES: usize = 64 * 1024 * 1024;

/// Include framing and reject arithmetic overflow before allocating a pair.
pub fn operation_upload_fits(view_bytes: usize, operation_bytes: usize) -> bool {
    view_bytes
        .checked_add(operation_bytes)
        .and_then(|bytes| bytes.checked_add(4))
        .is_some_and(|bytes| bytes <= MAX_REQUEST_BODY_BYTES)
}

/// A view length followed by the unchanged view and operation protobufs.
pub fn encode_operation_upload(view: &[u8], operation: &[u8]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(4 + view.len() + operation.len());
    bytes.extend_from_slice(
        &u32::try_from(view.len())
            .expect("bounded view")
            .to_be_bytes(),
    );
    bytes.extend_from_slice(view);
    bytes.extend_from_slice(operation);
    bytes
}

/// Borrow payloads only after checking the length against the received body.
pub fn decode_operation_upload(bytes: &[u8]) -> Result<(&[u8], &[u8]), &'static str> {
    let prefix = bytes.get(..4).ok_or("missing view length")?;
    let length = u32::from_be_bytes(prefix.try_into().unwrap()) as usize;
    let payload = &bytes[4..];
    if length >= payload.len() {
        return Err("truncated view or missing operation");
    }
    Ok(payload.split_at(length))
}

#[cfg(test)]
mod operation_upload_limit_tests {
    #[test]
    fn paired_upload_accounts_for_framing_at_the_body_limit() {
        use super::{operation_upload_fits, MAX_REQUEST_BODY_BYTES};
        assert!(operation_upload_fits(1, MAX_REQUEST_BODY_BYTES - 5));
        assert!(!operation_upload_fits(1, MAX_REQUEST_BODY_BYTES - 4));
        assert!(!operation_upload_fits(usize::MAX, 1));
        assert!(!operation_upload_fits(1, usize::MAX));
    }
}
