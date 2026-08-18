//! The bucket half of the server: WAL entries and the index object.
//!
//! The bucket is the durable source of truth (see
//! `docs/design-docs/target-architecture.md`). Everything in this file exists
//! to keep one ordering: a WAL entry, then the index compare-and-swap, and only
//! then any local state — so a crash anywhere in between leaves the bucket
//! ahead of the repo, which `recover_from_bucket` replays on the next start.
//!
//! The rest of `Server` — the RPC surface and the local jj repo — lives in the
//! parent module and is reached only through a handful of its methods:
//! `read_jj_op_heads`, the heads-metadata pair, the object/operation/view
//! accessors, and the bucket fields themselves.

use anyhow::{anyhow, bail, Context, Result};
use jj_lib::backend::{CommitId, TreeId, TreeValue};
use jj_lib::object_id::ObjectId as _;
use jj_lib::op_store::{Operation, OperationId};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::time::Instant;

use super::{
    from_hex, head_ids_for_wire, is_root_operation_hex, to_hex, write_bytes_if_missing,
    HeadsMetadata, Server, UpdateResult,
};
use crate::object_store::CasError;
use crate::wal;

// ─── Boot-time replay ─────────────────────────────────────────────────────────

/// What materializing the repo from the bucket cost at startup.
///
/// A server whose disk is a cache boots by reading history it does not have, so
/// how much of it had to come back is the one number that tells an operator
/// whether this start was a warm restart or a rebuild from nothing.
#[derive(Debug, Clone, Copy, Default)]
pub(super) struct BootReplay {
    /// Op heads the bucket named that the local repo did not have.
    pub heads: u64,
    /// WAL entries fetched and applied, ancestors included.
    pub entries: u64,
    /// How long the whole recovery took.
    pub millis: u64,
}

/// How much decoded WAL the replay may hold between reading an entry and
/// applying it. A WAL entry carries content, not just ids, so an unbounded
/// cache means a cold boot holds the whole repository history in memory.
const REPLAY_CACHE_MAX_BYTES: usize = 64 * 1024 * 1024;

/// Entry bodies held between the read that discovers an operation's parents and
/// the write that applies them.
///
/// The replay reads a child to learn its parents and applies it only after
/// them, so every entry is read some time before it is needed. The deepest
/// operation is applied first, which makes the most recently read entry the
/// next one wanted: evicting the oldest costs at most one extra read of an
/// entry that was not going to be applied for a while.
#[derive(Default)]
struct ReplayCache {
    entries: HashMap<String, wal::WalEntry>,
    order: VecDeque<String>,
    bytes: usize,
}

impl ReplayCache {
    fn size_of(entry: &wal::WalEntry) -> usize {
        entry.records.iter().map(|record| record.data.len()).sum()
    }

    fn insert(&mut self, op_hex: String, entry: wal::WalEntry) {
        self.bytes += Self::size_of(&entry);
        self.order.push_back(op_hex.clone());
        self.entries.insert(op_hex, entry);
        while self.bytes > REPLAY_CACHE_MAX_BYTES && self.order.len() > 1 {
            let Some(evicted) = self.order.pop_front() else {
                break;
            };
            if let Some(entry) = self.entries.remove(&evicted) {
                self.bytes -= Self::size_of(&entry);
            }
        }
    }

    fn take(&mut self, op_hex: &str) -> Option<wal::WalEntry> {
        let entry = self.entries.remove(op_hex)?;
        self.bytes -= Self::size_of(&entry);
        self.order.retain(|hex| hex != op_hex);
        Some(entry)
    }
}

// ─── Staging buffers ──────────────────────────────────────────────────────────

/// How many operation ids the durable-WAL-entry cache remembers. It is only a
/// cache: a miss costs one extra conditional put, which the bucket rejects as
/// already-present, so the memory ceiling is worth more than the perfect recall.
const DURABLE_OPS_CAPACITY: usize = 4096;

/// Operation ids whose WAL entry this process has already put in the bucket,
/// kept to a fixed size in insertion order.
#[derive(Default)]
pub(super) struct DurableOps {
    seen: HashSet<String>,
    order: VecDeque<String>,
}

impl DurableOps {
    fn contains(&self, op_hex: &str) -> bool {
        self.seen.contains(op_hex)
    }

    fn insert(&mut self, op_hex: &str) {
        if !self.seen.insert(op_hex.to_string()) {
            return;
        }
        self.order.push_back(op_hex.to_string());
        while self.order.len() > DURABLE_OPS_CAPACITY {
            if let Some(evicted) = self.order.pop_front() {
                self.seen.remove(&evicted);
            }
        }
    }
}

/// Objects written since the last publish, waiting to be folded into the next
/// WAL entry. Objects are content-addressed, so the same id staged twice is the
/// same bytes twice: stage it once and keep the buffer proportional to distinct
/// content rather than to request count.
#[derive(Default)]
pub(super) struct PendingBlobs {
    records: Vec<wal::WalRecord>,
    staged: HashSet<Vec<u8>>,
    bytes: usize,
}

/// The staging buffer holds objects until the next publish, so its size is set
/// by how much a client writes before it commits. Past this much, say so once:
/// a silent buffer is worse than a loud one.
const PENDING_BLOBS_WARN_BYTES: usize = 512 * 1024 * 1024;

/// And past this much, refuse. A client that writes without ever publishing —
/// or a bucket outage that makes every publish fail and restage — otherwise
/// grows the server's memory without limit. Refusing the write is backpressure
/// the client can act on; running the server out of memory is not.
const PENDING_BLOBS_MAX_BYTES: usize = 1024 * 1024 * 1024;

impl PendingBlobs {
    /// Accept a newly written object. Fails once the buffer is over its cap.
    pub(super) fn stage(&mut self, record: wal::WalRecord) -> Result<()> {
        if self.bytes + record.data.len() > PENDING_BLOBS_MAX_BYTES {
            bail!(
                "the server is holding {} bytes of objects that no publish has made durable yet \
                 (limit {PENDING_BLOBS_MAX_BYTES}); commit an operation to flush them",
                self.bytes
            );
        }
        self.accept(record);
        Ok(())
    }

    fn accept(&mut self, record: wal::WalRecord) {
        if !self.staged.insert(record.id.clone()) {
            return;
        }
        let before = self.bytes;
        self.bytes += record.data.len();
        self.records.push(record);
        if before <= PENDING_BLOBS_WARN_BYTES && self.bytes > PENDING_BLOBS_WARN_BYTES {
            tracing::warn!(
                bytes = self.bytes,
                objects = self.records.len(),
                "staged objects are large and not yet durable; no publish has drained them"
            );
        }
    }

    fn take(&mut self) -> Vec<wal::WalRecord> {
        self.staged.clear();
        self.bytes = 0;
        std::mem::take(&mut self.records)
    }

    /// Put objects back at the front, so a publish that could not carry them
    /// leaves them ahead of anything written since.
    ///
    /// The cap does not apply: these objects were accepted already, and the
    /// client has been told nothing about them yet. Dropping them here would
    /// lose content a later head can reach.
    fn restage(&mut self, records: Vec<wal::WalRecord>) {
        let tail = self.take();
        for record in records.into_iter().chain(tail) {
            self.accept(record);
        }
    }
}

// ─── Durable writes and recovery ──────────────────────────────────────────────

impl Server {
    /// Take the objects written since the last publish.
    fn drain_pending_blobs(&self) -> Result<Vec<wal::WalRecord>> {
        Ok(self
            .pending_blobs
            .lock()
            .map_err(|e| anyhow!("pending blobs lock: {e}"))?
            .take())
    }

    /// Put staged objects back on the queue after a WAL write that did not
    /// carry them, ahead of anything written since, so the next publish does.
    fn restage_blobs(&self, records: Vec<wal::WalRecord>) {
        let blobs: Vec<wal::WalRecord> = records
            .into_iter()
            .filter(|record| record.kind.object_kind().is_some())
            .collect();
        if blobs.is_empty() {
            return;
        }
        match self.pending_blobs.lock() {
            Ok(mut pending) => pending.restage(blobs),
            Err(err) => {
                tracing::error!(error = %err, "cannot restage objects after a failed WAL write")
            }
        }
    }

    fn wal_entry_already_written(&self, op_hex: &str) -> Result<bool> {
        Ok(self
            .durable_ops
            .lock()
            .map_err(|e| anyhow!("durable ops lock: {e}"))?
            .contains(op_hex))
    }

    fn remember_durable_wal_entry(&self, op_hex: &str) -> Result<()> {
        self.durable_ops
            .lock()
            .map_err(|e| anyhow!("durable ops lock: {e}"))?
            .insert(op_hex);
        Ok(())
    }

    /// Whether the bucket already holds this operation's WAL entry.
    ///
    /// The in-process cache answers first, because it is free. On a miss the
    /// bucket is asked, and a hit is cached: a fresh process starts with an
    /// empty cache, and without this the ancestry walk would descend to the
    /// root operation on the first publish after every restart and re-put an
    /// entry body for each operation in history.
    fn wal_entry_is_durable(&self, op_hex: &str) -> Result<bool> {
        if self.wal_entry_already_written(op_hex)? {
            return Ok(true);
        }
        let key = wal::wal_key(op_hex);
        if !self
            .bucket
            .exists(&key)
            .with_context(|| format!("check WAL entry {key}"))?
        {
            return Ok(false);
        }
        self.remember_durable_wal_entry(op_hex)?;
        Ok(true)
    }

    /// An operation out of the local op store, by hex id.
    fn read_operation_by_hex(&self, op_hex: &str) -> Result<(OperationId, Operation)> {
        let op_id = OperationId::new(from_hex(op_hex)?);
        let operation = pollster::block_on(self.repo_loader.op_store().read_operation(&op_id))
            .map_err(|e| anyhow!("read operation {op_hex}: {e}"))?;
        Ok((op_id, operation))
    }

    /// The operation and its view, as the tail records of a WAL entry, plus
    /// the operation's parents.
    fn operation_records(&self, op_hex: &str) -> Result<(Vec<wal::WalRecord>, Vec<Vec<u8>>)> {
        let (op_id, operation) = self.read_operation_by_hex(op_hex)?;
        let op_bytes = self
            .get_operation_sync(op_id.as_bytes())
            .with_context(|| format!("read operation {op_hex} for WAL entry"))?;
        let view_bytes = self
            .get_view_sync(operation.view_id.as_bytes())
            .with_context(|| format!("read view for operation {op_hex}"))?;

        // Order matters on replay: blobs, then the view, then the operation.
        let records = vec![
            wal::WalRecord {
                kind: wal::RecordKind::View,
                id: operation.view_id.as_bytes().to_vec(),
                data: view_bytes,
            },
            wal::WalRecord {
                kind: wal::RecordKind::Operation,
                id: op_id.as_bytes().to_vec(),
                data: op_bytes,
            },
        ];
        let parents = operation
            .parents
            .iter()
            .map(|id| id.as_bytes().to_vec())
            .collect();
        Ok((records, parents))
    }

    /// Returns whether this call stored the entry. `false` means the bucket
    /// already held an entry for this operation — immutable, so the records
    /// just built are not in it.
    fn put_wal_entry(&self, op_hex: &str, entry: &wal::WalEntry) -> Result<bool> {
        let encoded = entry.encode()?;
        let key = wal::wal_key(op_hex);
        if self.faults.take_wal_write_failure() {
            bail!("injected bucket failure while writing WAL entry {key}");
        }
        let stored = self
            .bucket
            .put_immutable(&key, &encoded)
            .with_context(|| format!("write WAL entry {key}"))?;
        if stored {
            tracing::debug!(op_id = %op_hex, bytes = encoded.len(), records = entry.records.len(), "wrote WAL entry");
        } else {
            tracing::debug!(op_id = %op_hex, "WAL entry was already in the bucket");
        }
        self.remember_durable_wal_entry(op_hex)?;
        Ok(stored)
    }

    /// Write the WAL entry for a publish: the operation, its view, and every
    /// object staged since the last publish.
    pub(super) fn write_publish_wal_entry(&self, op_hex: &str) -> Result<()> {
        if is_root_operation_hex(op_hex) {
            // jj's root operation is synthetic: no stored operation, no view,
            // nothing to make durable. Staged objects stay staged.
            return Ok(());
        }

        // A retried publish — the index CAS conflicted, the client came back
        // with the same operation. The entry for it is already in the bucket
        // and immutable, so it cannot take anything more. Objects staged since
        // the first attempt belong to whichever publish comes next; draining
        // them here would drop them into an entry that refuses them, and they
        // would be durable nowhere while a later head that reaches them gets
        // acknowledged.
        if self.wal_entry_is_durable(op_hex)? {
            tracing::debug!(
                op_id = %op_hex,
                "publish retried; keeping staged objects for the next publish"
            );
            return Ok(());
        }

        // Any parent the bucket does not hold yet — a merge this server minted
        // while reconciling heads is the usual one — goes in first, so this
        // entry is never acknowledged over a gap in its own ancestry.
        for parent in self.operation_parent_hexes(op_hex)? {
            self.ensure_wal_entry(&parent)
                .with_context(|| format!("make parent of {op_hex} durable"))?;
        }

        let blobs = self.drain_pending_blobs()?;
        if !self.put_operation_wal_entry(op_hex, blobs)? {
            // Another writer, or an earlier life of this process, wrote this
            // entry. Its record list is not the one just built, and the entry
            // is immutable — so the drained objects went back on the staging
            // queue instead of being swallowed by the bucket's refusal to
            // overwrite.
            tracing::warn!(
                op_id = %op_hex,
                "WAL entry existed already; restaging this publish's objects"
            );
        }
        Ok(())
    }

    /// Build and store one operation's WAL entry: the leading records the
    /// caller supplies, then the operation's view, then the operation. That
    /// order is the replay order, and it is the whole shape of a WAL entry —
    /// staged blobs ahead of the operation that makes them reachable.
    ///
    /// Returns whether this call stored the entry. Anything that stops the
    /// entry from being stored — a read that fails, a bucket that refuses, an
    /// entry already there — puts the leading records back on the staging
    /// queue, so no object the caller drained is left durable nowhere.
    fn put_operation_wal_entry(
        &self,
        op_hex: &str,
        leading_records: Vec<wal::WalRecord>,
    ) -> Result<bool> {
        let (tail, parents) = match self.operation_records(op_hex) {
            Ok(read) => read,
            Err(err) => {
                self.restage_blobs(leading_records);
                return Err(err);
            }
        };

        let mut records = leading_records;
        records.extend(tail);
        let entry = wal::WalEntry {
            op_id: from_hex(op_hex)?,
            parents,
            records,
        };

        match self.put_wal_entry(op_hex, &entry) {
            Ok(true) => Ok(true),
            Ok(false) => {
                self.restage_blobs(entry.records);
                Ok(false)
            }
            Err(err) => {
                self.restage_blobs(entry.records);
                Err(err)
            }
        }
    }

    /// Make sure an operation has a WAL entry, writing one if this process has
    /// not already. Covers merge operations the server creates itself when it
    /// reconciles divergent heads.
    fn ensure_wal_entry(&self, op_hex: &str) -> Result<()> {
        // The whole missing ancestry, not just this operation. A reconcile
        // merge the server mints on its own never reaches a WAL entry through
        // a publish of its own: by the time the next operation is published,
        // the merge is no longer a head, so a head-only pass walks straight
        // past it. Its child names it as a parent, so replay would stop at an
        // operation the bucket never received.
        //
        // Parents are written before children, so an interrupted walk leaves a
        // shorter but still connected chain instead of an entry whose parent is
        // unreachable. The walk stops at the first ancestor the bucket already
        // holds — asked of the bucket, not only of this process's cache, so a
        // restart does not turn the first publish into a full history re-upload
        // — and the steady-state cost is one existence check.
        enum Step {
            Visit(String),
            Write(String),
        }

        let mut seen: HashSet<String> = HashSet::new();
        let mut stack = vec![Step::Visit(op_hex.to_string())];
        while let Some(step) = stack.pop() {
            match step {
                Step::Visit(hex) => {
                    if is_root_operation_hex(&hex) || !seen.insert(hex.clone()) {
                        continue;
                    }
                    if self.wal_entry_is_durable(&hex)? {
                        continue;
                    }
                    let parents = self.operation_parent_hexes(&hex)?;
                    stack.push(Step::Write(hex));
                    for parent in parents {
                        stack.push(Step::Visit(parent));
                    }
                }
                Step::Write(hex) => {
                    // No leading records: an ancestor's entry carries only the
                    // operation and its view. The blobs belong to the publish
                    // that drains them, not to this walk.
                    self.put_operation_wal_entry(&hex, Vec::new())?;
                }
            }
        }
        Ok(())
    }

    /// The parents of an operation, as hex ids.
    fn operation_parent_hexes(&self, op_hex: &str) -> Result<Vec<String>> {
        let (_, operation) = self.read_operation_by_hex(op_hex)?;
        Ok(operation.parents.iter().map(|id| id.hex()).collect())
    }

    /// Publish an op-head set to the bucket index. Returns false on a CAS
    /// conflict, which the caller surfaces as a retryable version mismatch.
    pub(super) fn publish_index(
        &self,
        version: u64,
        op_heads: &[String],
        workspace_heads: &BTreeMap<String, String>,
    ) -> Result<bool> {
        for head in op_heads {
            self.ensure_wal_entry(head)?;
        }

        let index = wal::IndexObject {
            version,
            op_heads: op_heads.to_vec(),
            workspace_heads: workspace_heads.clone(),
        };
        let encoded = index.encode()?;

        if !self.bucket_conditional_put {
            // No conditional put: this server's mutex is the only arbiter.
            let etag = self.bucket.put_overwrite(wal::INDEX_KEY, &encoded)?;
            self.set_index_etag(Some(etag))?;
            return Ok(true);
        }

        let expected = self.index_etag()?;
        match self
            .bucket
            .compare_and_put(wal::INDEX_KEY, &encoded, expected.as_deref())
        {
            Ok(etag) => {
                self.set_index_etag(Some(etag))?;
                tracing::debug!(version, heads = op_heads.len(), "index object updated");
                Ok(true)
            }
            Err(CasError::Conflict) => {
                tracing::debug!(version, "index object CAS conflict");
                Ok(false)
            }
            Err(CasError::Other(err)) => Err(err.context("update index object")),
        }
    }

    /// Publish a head set the server derived on its own — the merge operation a
    /// reconcile mints — and return the head set that may be acknowledged.
    ///
    /// This runs after the publish is durable in the bucket and applied to the
    /// local repo, so it cannot fail anything. A head is only acknowledged once
    /// the bucket holds the operation behind it, so if the merge operation's
    /// WAL entry cannot be written — one 500 from the bucket is enough — the
    /// answer is `already_durable`, the set the index has just committed, not
    /// an error. The merge is re-derivable from its parents, which are durable,
    /// and the next publish restates the settled head set at a higher version.
    /// Failing here instead would fail an RPC whose operation had already
    /// landed, and the client's retry would rewrite the same change.
    ///
    /// The index write that names the derived heads is best effort for the same
    /// reason.
    pub(super) fn publish_derived_heads(
        &self,
        version: u64,
        op_heads: &[String],
        already_durable: &[String],
        workspace_heads: &BTreeMap<String, String>,
    ) -> Vec<String> {
        for head in op_heads {
            let result = match self.faults.derived_head_wal_fault() {
                Some(err) => Err(err),
                None => self
                    .ensure_wal_entry(head)
                    .with_context(|| format!("make derived op head {head} durable")),
            };
            if let Err(err) = result {
                tracing::warn!(
                    op_id = %head,
                    error = %err,
                    "could not make a derived op head durable; acknowledging the head set the index already holds"
                );
                return already_durable.to_vec();
            }
        }
        self.mirror_reconciled_index(version, op_heads, workspace_heads);
        op_heads.to_vec()
    }

    /// Mirror a head set the server derived on its own — a reconcile merge, or
    /// the head set after a publish settled — into the index. Best effort: the
    /// operations it names are already durable, and the next publish resyncs.
    fn mirror_reconciled_index(
        &self,
        version: u64,
        op_heads: &[String],
        workspace_heads: &BTreeMap<String, String>,
    ) {
        match self.publish_index(version, op_heads, workspace_heads) {
            Ok(true) => {}
            Ok(false) => {
                tracing::warn!(
                    version,
                    "index CAS conflict while mirroring reconciled heads"
                );
                if let Err(err) = self.reload_index() {
                    tracing::warn!(error = %err, "could not reload the index object");
                }
            }
            Err(err) => {
                tracing::warn!(version, error = %err, "could not mirror reconciled heads to the index");
            }
        }
    }

    /// The op-head set the pending update produces, computed before the local
    /// apply so the bucket can commit ahead of the repo.
    pub(super) fn prospective_op_heads(
        &self,
        old_ids: &[OperationId],
        new_id: &OperationId,
    ) -> Result<Vec<String>> {
        let retired: HashSet<String> = old_ids.iter().map(|id| id.hex()).collect();
        let mut heads: Vec<String> = self
            .read_jj_op_heads()?
            .into_iter()
            .filter(|head| !retired.contains(head))
            .collect();
        heads.push(new_id.hex());
        heads.sort();
        heads.dedup();
        Ok(heads)
    }

    /// The index moved under us. Resynchronize from the bucket and report the
    /// same retryable version mismatch a local version conflict reports, so
    /// the client's existing retry loop handles it unchanged.
    pub(super) fn index_conflict_result(&self, metadata: HeadsMetadata) -> Result<UpdateResult> {
        let mut version = metadata.version;
        let mut workspace_heads = metadata.workspace_heads;

        if let Some(index) = self.reload_index()? {
            if index.version > version {
                // If the adoption fails, do not take the version: reporting the
                // stale one costs another retry, and losing a head costs the
                // head. The client's retry recomputes the head set from the
                // local repo's heads, so a version adopted without the heads
                // behind it would let the retry CAS a set that silently drops
                // the other writer's head — last writer wins, against
                // invariant 6.
                let (heads, entries) = self
                    .adopt_index(&index)
                    .context("replay the bucket's op heads after an index CAS conflict")?;
                version = index.version;
                workspace_heads = index.workspace_heads.clone();
                tracing::debug!(
                    heads,
                    entries,
                    version,
                    "adopted the bucket index after a conflict"
                );
            }
        }

        let heads = self.read_jj_op_heads()?;
        tracing::debug!(version, "index CAS conflict; asking the client to retry");
        Ok(UpdateResult {
            ok: false,
            heads: head_ids_for_wire(&heads),
            version,
            workspace_heads,
        })
    }

    /// Whether the fault seam wants this index write to look like a CAS
    /// conflict, so the retryable path can be exercised without a second
    /// writer.
    pub(super) fn inject_index_conflict(&self) -> bool {
        if !self.faults.take_index_cas_conflict() {
            return false;
        }
        self.stage_injected_object_during_conflict();
        true
    }

    /// The object a second client "wrote" between two attempts of a retried
    /// publish — the window in which a drained staging buffer can lose objects.
    fn stage_injected_object_during_conflict(&self) {
        let Some(content) = self.faults.object_for_index_conflict() else {
            return;
        };
        match self.put_object_sync("file", &content) {
            Ok((id, _)) => tracing::warn!(
                object = %to_hex(&id),
                "staged an injected object during an injected index conflict"
            ),
            Err(err) => {
                tracing::error!(error = %err, "could not stage the injected object")
            }
        }
    }

    fn index_etag(&self) -> Result<Option<String>> {
        Ok(self
            .index_etag
            .lock()
            .map_err(|e| anyhow!("index etag lock: {e}"))?
            .clone())
    }

    fn set_index_etag(&self, etag: Option<String>) -> Result<()> {
        *self
            .index_etag
            .lock()
            .map_err(|e| anyhow!("index etag lock: {e}"))? = etag;
        Ok(())
    }

    /// Re-read the index from the bucket, refreshing the cached etag.
    fn reload_index(&self) -> Result<Option<wal::IndexObject>> {
        match self.bucket.get_with_etag(wal::INDEX_KEY)? {
            Some((bytes, etag)) => {
                self.set_index_etag(Some(etag))?;
                Ok(Some(wal::IndexObject::decode(&bytes)?))
            }
            None => {
                self.set_index_etag(None)?;
                Ok(None)
            }
        }
    }

    /// Write a WAL entry's contents into the local repo and make its operation
    /// a head. Idempotent — everything it writes is content-addressed.
    fn apply_wal_entry(&self, entry: &wal::WalEntry) -> Result<()> {
        for record in &entry.records {
            match record.kind {
                wal::RecordKind::Operation => {
                    let path = self
                        .op_store_path
                        .join("operations")
                        .join(to_hex(&record.id));
                    write_bytes_if_missing(&path, &record.data)?;
                }
                wal::RecordKind::View => {
                    let path = self.op_store_path.join("views").join(to_hex(&record.id));
                    write_bytes_if_missing(&path, &record.data)?;
                }
                other => {
                    let kind = other
                        .object_kind()
                        .ok_or_else(|| anyhow!("WAL record kind has no object kind"))?;
                    let (id, _) = self
                        .write_object_sync(kind, &record.data)
                        .with_context(|| format!("replay {kind} object"))?;
                    if id != record.id {
                        bail!(
                            "replayed {kind} object hashed to {} but WAL recorded {}",
                            to_hex(&id),
                            to_hex(&record.id)
                        );
                    }
                }
            }
        }

        let op_id = OperationId::new(entry.op_id.clone());
        let parents: Vec<OperationId> = entry
            .parents
            .iter()
            .cloned()
            .map(OperationId::new)
            .collect();
        pollster::block_on(self.op_heads_store.update_op_heads(&parents, &op_id))
            .map_err(|e| anyhow!("replay op head {}: {e}", op_id.hex()))?;
        Ok(())
    }

    /// Whether the local op store already holds this operation.
    ///
    /// This is the replay's stop condition, and it is asked of the disk rather
    /// than of the head set on purpose: an operation is only ever written after
    /// its own ancestry, so finding one means the history below it is there
    /// too. That makes the same walk serve a cold boot and a warm one — on a
    /// warm boot it stops at the first ancestor and costs nothing.
    fn operation_is_local(&self, op_hex: &str) -> bool {
        self.op_store_path.join("operations").join(op_hex).exists()
    }

    /// Read one operation's WAL entry out of the bucket.
    fn fetch_wal_entry(&self, op_hex: &str) -> Result<wal::WalEntry> {
        let key = wal::wal_key(op_hex);
        let bytes = self
            .bucket
            .get(&key)
            .with_context(|| format!("read WAL entry {key}"))?
            .ok_or_else(|| {
                anyhow!(
                    "WAL entry {key} is missing from the bucket, so the operation history has a \
                     gap the local repo cannot be rebuilt across"
                )
            })?;
        tracing::debug!(op_id = %op_hex, bytes = bytes.len(), "replaying a WAL entry");
        wal::WalEntry::decode(&bytes).with_context(|| format!("decode WAL entry {key}"))
    }

    /// Replay one op head and every ancestor the local repo is missing, parents
    /// before children. Returns how many WAL entries it applied.
    ///
    /// This is the mirror image of `ensure_wal_entry`: that walk makes an
    /// operation's ancestry durable in the bucket, this one brings it back. The
    /// ordering is not a nicety. A WAL entry carries only the blobs staged
    /// since the previous publish, so the objects an operation makes reachable
    /// are spread across its whole ancestry, and jj-lib walks the operation DAG
    /// to the root the first time it builds an index over a repo with no index
    /// segment. Applying a head without its ancestors gives a repo that answers
    /// `getHeads` and fails everything else.
    ///
    /// The head itself is always applied, even when its operation is already on
    /// disk: that is exactly the crash window recovery exists for — the client
    /// wrote the operation before asking for the head update, and only the head
    /// pointer is missing.
    fn replay_ancestry(&self, head_hex: &str) -> Result<usize> {
        enum Step {
            Visit(String),
            Apply(String),
        }

        let mut seen: HashSet<String> = HashSet::new();
        let mut cache = ReplayCache::default();
        let mut stack = vec![Step::Visit(head_hex.to_string())];
        let mut applied = 0usize;

        while let Some(step) = stack.pop() {
            match step {
                Step::Visit(hex) => {
                    if is_root_operation_hex(&hex) || !seen.insert(hex.clone()) {
                        continue;
                    }
                    if hex != head_hex && self.operation_is_local(&hex) {
                        continue;
                    }
                    let entry = self
                        .fetch_wal_entry(&hex)
                        .with_context(|| format!("replay the ancestry of op head {head_hex}"))?;
                    let parents: Vec<String> = entry.parents.iter().map(|id| to_hex(id)).collect();
                    cache.insert(hex.clone(), entry);
                    stack.push(Step::Apply(hex));
                    for parent in parents {
                        stack.push(Step::Visit(parent));
                    }
                }
                Step::Apply(hex) => {
                    let entry = match cache.take(&hex) {
                        Some(entry) => entry,
                        // Evicted while the walk was deeper in the history.
                        None => self.fetch_wal_entry(&hex)?,
                    };
                    self.apply_wal_entry(&entry)
                        .with_context(|| format!("apply WAL entry {hex}"))?;
                    applied += 1;
                }
            }
        }
        Ok(applied)
    }

    /// Rebuild every op head the index names that the local repo does not have,
    /// with the ancestry behind it. Returns how many WAL entries were applied.
    ///
    /// Idempotent: everything a WAL entry carries is content-addressed, and
    /// making an operation a head twice is a no-op.
    fn replay_index_heads(&self, index: &wal::IndexObject) -> Result<(usize, usize)> {
        let local_heads: HashSet<String> = self.read_jj_op_heads()?.into_iter().collect();
        let mut heads = 0usize;
        let mut entries = 0usize;
        for head in &index.op_heads {
            if local_heads.contains(head) || is_root_operation_hex(head) {
                continue;
            }
            entries += self.replay_ancestry(head)?;
            heads += 1;
        }
        Ok((heads, entries))
    }

    /// Take the bucket's index as local state: replay every op head it names,
    /// then record its version and workspace map. Returns how many heads and
    /// how many WAL entries the replay applied.
    ///
    /// The order is load-bearing. The heads go in first and the version is only
    /// recorded if they all landed, because every head set this server computes
    /// next comes from the local repo's heads: a version taken without the
    /// heads behind it lets the next write CAS a set that silently drops
    /// another writer's head, against invariant 6.
    fn adopt_index(&self, index: &wal::IndexObject) -> Result<(usize, usize)> {
        let replayed = self.replay_index_heads(index)?;
        self.retire_bootstrap_heads(index)?;
        self.write_heads_metadata(&HeadsMetadata {
            version: index.version,
            workspace_heads: index.workspace_heads.clone(),
        })?;
        Ok(replayed)
    }

    /// Drop the operation this boot's own repo init minted.
    ///
    /// Materializing on an empty disk starts by creating a colocated repo, and
    /// creating one mints an "initialize repo" operation whose id is new every
    /// time — its metadata carries a timestamp. Nothing in the bucket names it.
    /// Left alone it survives the replay as a second head: a fabricated one,
    /// against an empty repo, that no client ever published. jj would then
    /// merge the real history with nothing, two servers materialized from the
    /// same bucket would disagree about the head set, and the op log — the
    /// audit trail invariant 7 rests on — would carry an entry that never
    /// happened.
    ///
    /// Only heads this process minted at init are ever retired, and only once
    /// the bucket's own heads are on disk to take their place. A head that came
    /// from anywhere else is another writer's, and dropping one of those is
    /// last-writer-wins, against invariant 6.
    fn retire_bootstrap_heads(&self, index: &wal::IndexObject) -> Result<()> {
        if self.bootstrap_op_heads.is_empty() {
            return Ok(());
        }
        let named: HashSet<&str> = index.op_heads.iter().map(String::as_str).collect();
        let stale: Vec<OperationId> = self
            .bootstrap_op_heads
            .iter()
            .filter(|hex| !named.contains(hex.as_str()))
            .map(|hex| from_hex(hex).map(OperationId::new))
            .collect::<Result<Vec<_>>>()?;
        if stale.is_empty() {
            return Ok(());
        }

        // Retiring a head takes a head to retire it in favour of, and it has to
        // be one the replay actually landed: dropping the init operation before
        // the bucket's history is on disk would leave the repo with no head at
        // all.
        let local: HashSet<String> = self.read_jj_op_heads()?.into_iter().collect();
        let Some(anchor) = index.op_heads.iter().find(|head| local.contains(*head)) else {
            tracing::warn!(
                "no op head from the bucket landed locally; keeping the operation this boot's \
                 repo init minted so the repo still has a head"
            );
            return Ok(());
        };
        let anchor_id = OperationId::new(from_hex(anchor)?);
        pollster::block_on(self.op_heads_store.update_op_heads(&stale, &anchor_id))
            .map_err(|e| anyhow!("retire the repo-init op heads: {e}"))?;
        tracing::debug!(
            retired = stale.len(),
            "retired the operations this boot's repo init minted"
        );
        Ok(())
    }

    /// Put the local repo's head set into the index at the version the local
    /// metadata records. Returns false on a CAS conflict; what that means is
    /// the caller's to say.
    fn republish_local_heads(&self, local: &HeadsMetadata) -> Result<bool> {
        let heads = self.read_jj_op_heads()?;
        self.publish_index(local.version, &heads, &local.workspace_heads)
    }

    /// Materialize the local repo from the bucket at startup.
    ///
    /// Two shapes of the same job. On an empty disk the whole history comes
    /// back: the repo is a cache, and this is what makes it a disposable one.
    /// On a warm disk only the gap does — the server acknowledged nothing but
    /// did commit an index write before dying, so the bucket names an op head
    /// the repo has not applied. Both are the same walk; a warm boot stops at
    /// the first ancestor it already has.
    pub(super) fn recover_from_bucket(&self) -> Result<()> {
        let started = Instant::now();
        let local = self.read_heads_metadata()?;
        let index = self.reload_index()?;

        let Some(index) = index else {
            // Fresh bucket. Seed it from local state so the first publish has
            // something to compare against.
            self.seed_bucket_from_local(&local)?;
            self.record_boot_replay(BootReplay {
                millis: started.elapsed().as_millis() as u64,
                ..BootReplay::default()
            });
            return Ok(());
        };

        let mut replay = BootReplay::default();
        if index.version > local.version || self.bootstrapped {
            // A repo that was just created has no history at all, so its
            // version-0 metadata says nothing about what the bucket holds: the
            // comparison that guards a warm boot would read an empty disk as
            // up to date at version 0 and serve an empty repo over a bucket
            // full of history.
            let (heads, entries) = self.adopt_index(&index)?;
            replay.heads = heads as u64;
            replay.entries = entries as u64;
            if entries > 0 {
                tracing::info!(
                    heads,
                    entries,
                    from_version = local.version,
                    to_version = index.version,
                    bootstrapped = self.bootstrapped,
                    "materialized the local repo from the bucket"
                );
            }
        } else if index.version < local.version {
            // The repo is ahead of the bucket — a crash before the index write
            // landed, or a repo that predates its bucket. Publish what is here.
            tracing::info!(
                index_version = index.version,
                local_version = local.version,
                "bucket index is behind the local repo; republishing local heads"
            );
            if !self.republish_local_heads(&local)? {
                tracing::warn!("could not republish local heads into the bucket index");
            }
        }

        replay.millis = started.elapsed().as_millis() as u64;
        self.record_boot_replay(replay);
        Ok(())
    }

    /// Put a brand-new repo's starting state into an empty bucket.
    ///
    /// `ensure_wal_entry` carries an operation and its view but no objects: on
    /// the ordinary path the objects were staged by the client writes that
    /// preceded the publish. A repo init has no such publish behind it — jj
    /// creates the working-copy commit and its tree through the backend
    /// directly — so unless they are staged here, the first entry in the WAL
    /// names a view whose commit exists on this disk and nowhere else, and a
    /// repo materialized from that bucket is missing the one object every
    /// later view still points at.
    fn seed_bucket_from_local(&self, local: &HeadsMetadata) -> Result<()> {
        // Only a repo this process just created gets its objects seeded. An
        // existing repo pointed at an empty bucket is a different job —
        // backfilling a whole history — and doing it here would read the
        // repo's entire tip tree into the staging buffer during boot. Seed the
        // head set and let the next publish carry its own objects.
        if self.bootstrapped {
            for head in self.read_jj_op_heads()? {
                if let Err(err) = self.stage_operation_objects(&head) {
                    tracing::warn!(
                        op_id = %head,
                        error = %err,
                        "could not stage the objects this repo starts from; a repo materialized \
                         from this bucket may be missing them"
                    );
                }
                self.write_publish_wal_entry(&head)
                    .with_context(|| format!("seed the bucket with op head {head}"))?;
            }
        }

        if !self.republish_local_heads(local)? {
            tracing::warn!("could not seed the bucket index; another writer got there first");
            self.reload_index()?;
        }
        Ok(())
    }

    /// Stage every object an operation's view reaches, so the next WAL entry
    /// carries them. Used only for state this server created on its own.
    fn stage_operation_objects(&self, op_hex: &str) -> Result<()> {
        if is_root_operation_hex(op_hex) {
            return Ok(());
        }
        let (_, operation) = self.read_operation_by_hex(op_hex)?;
        let view = pollster::block_on(self.repo_loader.op_store().read_view(&operation.view_id))
            .map_err(|e| anyhow!("read view for operation {op_hex}: {e}"))?;

        let mut commits: Vec<CommitId> = view.wc_commit_ids.values().cloned().collect();
        for target in view.local_bookmarks.values() {
            commits.extend(target.added_ids().cloned());
        }
        for commit_id in commits {
            self.stage_commit_objects(&commit_id)?;
        }
        Ok(())
    }

    /// Stage a commit, the trees under it, and their file and symlink content.
    ///
    /// Parents are deliberately not followed. This exists for state the server
    /// minted locally — one empty commit at repo init — and a walk that
    /// recursed through history would read a whole existing repo into the
    /// staging buffer on the first boot that names a bucket.
    fn stage_commit_objects(&self, commit_id: &CommitId) -> Result<()> {
        let backend = self.store.backend();
        if commit_id == backend.root_commit_id() {
            return Ok(());
        }
        let commit = pollster::block_on(backend.read_commit(commit_id))
            .map_err(|e| anyhow!("read commit {}: {e}", commit_id.hex()))?;
        self.stage_object("commit", commit_id.as_bytes())?;

        let mut seen: HashSet<Vec<u8>> = HashSet::new();
        let mut trees: Vec<TreeId> = commit.root_tree.iter().cloned().collect();
        while let Some(tree_id) = trees.pop() {
            if !seen.insert(tree_id.as_bytes().to_vec()) {
                continue;
            }
            let tree = pollster::block_on(
                backend.read_tree(jj_lib::repo_path::RepoPath::root(), &tree_id),
            )
            .map_err(|e| anyhow!("read tree {}: {e}", tree_id.hex()))?;
            self.stage_object("tree", tree_id.as_bytes())?;
            for entry in tree.entries() {
                match entry.value() {
                    TreeValue::File { id, .. } => self.stage_object("file", id.as_bytes())?,
                    TreeValue::Symlink(id) => self.stage_object("symlink", id.as_bytes())?,
                    TreeValue::Tree(id) => trees.push(id.clone()),
                    TreeValue::GitSubmodule(_) => {}
                }
            }
        }
        Ok(())
    }

    /// Put one already-stored object into the staging buffer, in the same form
    /// a client write would have left it in.
    fn stage_object(&self, kind: &str, id: &[u8]) -> Result<()> {
        let Some(record_kind) = wal::RecordKind::from_object_kind(kind) else {
            return Ok(());
        };
        let data = self
            .get_object_sync(kind, id)
            .with_context(|| format!("read {kind} {} to stage it", to_hex(id)))?;
        self.pending_blobs
            .lock()
            .map_err(|e| anyhow!("pending blobs lock: {e}"))?
            .stage(wal::WalRecord {
                kind: record_kind,
                id: id.to_vec(),
                data,
            })
    }

    fn record_boot_replay(&self, replay: BootReplay) {
        match self.boot_replay.lock() {
            Ok(mut slot) => *slot = replay,
            Err(err) => tracing::warn!(error = %err, "cannot record what boot replay did"),
        }
    }

    /// What the boot-time replay did, for the status surface.
    pub(super) fn boot_replay(&self) -> BootReplay {
        self.boot_replay
            .lock()
            .map(|slot| *slot)
            .unwrap_or_default()
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn blob(tag: u8, len: usize) -> wal::WalRecord {
        wal::WalRecord {
            kind: wal::RecordKind::File,
            id: vec![tag],
            data: vec![tag; len],
        }
    }

    /// The staging buffer is a hard cap, not a warning. A client that writes
    /// and never publishes must be refused rather than grow the server's
    /// memory without limit.
    #[test]
    fn staging_refuses_writes_past_its_cap() {
        let mut pending = PendingBlobs::default();
        let chunk = PENDING_BLOBS_MAX_BYTES / 4;

        for tag in 0..4u8 {
            pending
                .stage(blob(tag, chunk))
                .expect("staging under the cap must be accepted");
        }
        let err = pending
            .stage(blob(9, chunk))
            .expect_err("staging past the cap must be refused");
        assert!(
            err.to_string().contains("no publish has made durable"),
            "the refusal should say why: {err}"
        );
        assert_eq!(
            pending.records.len(),
            4,
            "a refused write must not be buffered"
        );
    }

    fn entry(tag: u8, len: usize) -> wal::WalEntry {
        wal::WalEntry {
            op_id: vec![tag],
            parents: Vec::new(),
            records: vec![blob(tag, len)],
        }
    }

    /// A cold boot walks the whole history, and a WAL entry carries content.
    /// The cache has to give way rather than hold the repository in memory —
    /// and it has to give way at the oldest end, because the walk applies the
    /// newest entry first.
    #[test]
    fn the_replay_cache_evicts_the_entry_it_needs_last() {
        let mut cache = ReplayCache::default();
        let chunk = REPLAY_CACHE_MAX_BYTES / 2;

        cache.insert("oldest".to_string(), entry(1, chunk));
        cache.insert("middle".to_string(), entry(2, chunk));
        cache.insert("newest".to_string(), entry(3, chunk));

        assert!(
            cache.take("oldest").is_none(),
            "the entry applied last is the one to drop"
        );
        assert!(
            cache.take("newest").is_some(),
            "the entry applied next must survive"
        );
        assert!(cache.take("middle").is_some());
    }

    /// One entry larger than the whole budget must still be kept: dropping it
    /// as it goes in would make every apply re-read it.
    #[test]
    fn the_replay_cache_keeps_an_oversized_entry() {
        let mut cache = ReplayCache::default();
        cache.insert("huge".to_string(), entry(1, REPLAY_CACHE_MAX_BYTES * 2));
        assert!(cache.take("huge").is_some());
    }

    /// Taking an entry has to give its bytes back, or a long replay slowly
    /// evicts everything it is still holding.
    #[test]
    fn taking_an_entry_frees_its_budget() {
        let mut cache = ReplayCache::default();
        let chunk = REPLAY_CACHE_MAX_BYTES / 2;

        cache.insert("first".to_string(), entry(1, chunk));
        assert!(cache.take("first").is_some());
        assert_eq!(cache.bytes, 0, "a taken entry must not still be charged");
        assert!(cache.order.is_empty(), "a taken entry must leave the queue");

        cache.insert("second".to_string(), entry(2, chunk));
        cache.insert("third".to_string(), entry(3, chunk));
        assert!(
            cache.take("second").is_some(),
            "two half-budget entries must both fit"
        );
    }

    /// Restaging is not a new write. The objects were accepted already and the
    /// client has been told nothing about them, so dropping them at the cap
    /// would lose content a later head can reach.
    #[test]
    fn restaging_is_not_subject_to_the_cap() {
        let mut pending = PendingBlobs::default();
        let chunk = PENDING_BLOBS_MAX_BYTES / 2;

        pending.stage(blob(1, chunk)).expect("first stage");
        let drained = pending.take();
        pending.stage(blob(2, chunk)).expect("second stage");

        pending.restage(drained);
        assert_eq!(
            pending.records.len(),
            2,
            "restaged objects must survive even when the buffer is at its cap"
        );
        assert_eq!(
            pending.records[0].id,
            vec![1],
            "restaged objects go ahead of anything written since"
        );
    }
}
