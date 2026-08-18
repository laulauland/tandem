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
use jj_lib::object_id::ObjectId as _;
use jj_lib::op_store::OperationId;
use std::collections::{BTreeMap, HashSet};
use std::sync::atomic::Ordering;

use super::{
    from_hex, head_ids_for_wire, is_root_operation_hex, to_hex, write_bytes_if_missing,
    HeadsMetadata, Server, UpdateResult,
};
use crate::object_store::CasError;
use crate::wal;

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
    order: std::collections::VecDeque<String>,
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
            Err(err) => tracing::error!(error = %err, "cannot restage objects after a failed WAL write"),
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

    /// The operation and its view, as the tail records of a WAL entry, plus
    /// the operation's parents.
    fn operation_records(&self, op_hex: &str) -> Result<(Vec<wal::WalRecord>, Vec<Vec<u8>>)> {
        let op_id = OperationId::new(from_hex(op_hex)?);
        let op_bytes = self
            .get_operation_sync(op_id.as_bytes())
            .with_context(|| format!("read operation {op_hex} for WAL entry"))?;
        let operation = pollster::block_on(self.repo_loader.op_store().read_operation(&op_id))
            .map_err(|e| anyhow!("read operation {op_hex}: {e}"))?;
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
        let op_id = OperationId::new(from_hex(op_hex)?);
        let operation = pollster::block_on(self.repo_loader.op_store().read_operation(&op_id))
            .map_err(|e| anyhow!("read operation {op_hex}: {e}"))?;
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
            let result = match derived_head_wal_fault() {
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
                tracing::warn!(version, "index CAS conflict while mirroring reconciled heads");
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
                let replayed = self
                    .adopt_index(&index)
                    .context("replay the bucket's op heads after an index CAS conflict")?;
                version = index.version;
                workspace_heads = index.workspace_heads.clone();
                tracing::debug!(replayed, version, "adopted the bucket index after a conflict");
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

    /// Test-only: force the next N index writes to look like CAS conflicts, so
    /// the retryable path can be exercised without a second writer.
    pub(super) fn inject_index_conflict(&self) -> bool {
        let remaining = self.test_index_conflicts.load(Ordering::Relaxed);
        if remaining == 0 {
            return false;
        }
        self.test_index_conflicts
            .fetch_sub(1, Ordering::Relaxed);
        tracing::warn!(remaining, "injecting an index CAS conflict (test hook)");
        self.stage_test_object_during_conflict();
        true
    }

    /// Test-only: stand in for a second client whose object write lands between
    /// two attempts of a retried publish, which is the window in which a
    /// drained staging buffer can lose objects. Inert unless the var is set.
    fn stage_test_object_during_conflict(&self) {
        let Ok(content) = std::env::var("TANDEM_TEST_STAGE_OBJECT_ON_INDEX_CONFLICT") else {
            return;
        };
        if content.is_empty() {
            return;
        }
        match self.put_object_sync("file", content.as_bytes()) {
            Ok((id, _)) => tracing::warn!(
                object = %to_hex(&id),
                "staged a test object during an injected index conflict"
            ),
            Err(err) => {
                tracing::error!(error = %err, "could not stage the test object")
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

    /// Apply every op head the index names that the local repo does not have
    /// yet, from the bucket's WAL entries. Returns how many were replayed.
    ///
    /// Idempotent: everything a WAL entry carries is content-addressed, and
    /// making an operation a head twice is a no-op.
    fn replay_index_heads(&self, index: &wal::IndexObject) -> Result<usize> {
        let local_heads: HashSet<String> = self.read_jj_op_heads()?.into_iter().collect();
        let mut replayed = 0usize;
        for head in &index.op_heads {
            if local_heads.contains(head) || is_root_operation_hex(head) {
                continue;
            }
            let key = wal::wal_key(head);
            let bytes = self.bucket.get(&key)?.ok_or_else(|| {
                anyhow!("bucket index names op head {head} but WAL entry {key} is missing")
            })?;
            let entry =
                wal::WalEntry::decode(&bytes).with_context(|| format!("decode WAL entry {key}"))?;
            self.apply_wal_entry(&entry)
                .with_context(|| format!("replay WAL entry {key}"))?;
            replayed += 1;
        }
        Ok(replayed)
    }

    /// Take the bucket's index as local state: replay every op head it names,
    /// then record its version and workspace map. Returns how many heads the
    /// replay applied.
    ///
    /// The order is load-bearing. The heads go in first and the version is only
    /// recorded if they all landed, because every head set this server computes
    /// next comes from the local repo's heads: a version taken without the
    /// heads behind it lets the next write CAS a set that silently drops
    /// another writer's head, against invariant 6.
    fn adopt_index(&self, index: &wal::IndexObject) -> Result<usize> {
        let replayed = self.replay_index_heads(index)?;
        self.write_heads_metadata(&HeadsMetadata {
            version: index.version,
            workspace_heads: index.workspace_heads.clone(),
        })?;
        Ok(replayed)
    }

    /// Put the local repo's head set into the index at the version the local
    /// metadata records. Returns false on a CAS conflict; what that means is
    /// the caller's to say.
    fn republish_local_heads(&self, local: &HeadsMetadata) -> Result<bool> {
        let heads = self.read_jj_op_heads()?;
        self.publish_index(local.version, &heads, &local.workspace_heads)
    }

    /// Bring the local repo back in line with the bucket at startup.
    ///
    /// The narrow case this stage owns: the server acknowledged nothing but did
    /// commit an index write before dying, so the bucket names an op head the
    /// repo has not applied. Booting from an empty disk is stage 2's job; this
    /// replays only what the index still points at.
    pub(super) fn recover_from_bucket(&self) -> Result<()> {
        let local = self.read_heads_metadata()?;
        let index = self.reload_index()?;

        let Some(index) = index else {
            // Fresh bucket. Seed it from local state so the first publish has
            // something to compare against.
            if !self.republish_local_heads(&local)? {
                tracing::warn!("could not seed the bucket index; another writer got there first");
                self.reload_index()?;
            }
            return Ok(());
        };

        if index.version > local.version {
            let replayed = self.adopt_index(&index)?;
            tracing::info!(
                replayed,
                from_version = local.version,
                to_version = index.version,
                "recovered local repo from the bucket"
            );
        } else if index.version < local.version {
            // The repo is ahead of the bucket — a repo that predates its
            // bucket, or a crash before the index write landed. Publish what
            // is here; older operations are stage 2's replay problem.
            tracing::info!(
                index_version = index.version,
                local_version = local.version,
                "bucket index is behind the local repo; republishing local heads"
            );
            if !self.republish_local_heads(&local)? {
                tracing::warn!("could not republish local heads into the bucket index");
            }
        }

        Ok(())
    }
}

// ─── Test hooks ───────────────────────────────────────────────────────────────

/// Read a test hook's counter from the environment. Anything unset, empty or
/// unparseable reads as zero, which is every hook's inert setting.
pub(super) fn test_env_u64(name: &str) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(0)
}

/// Test-only fault injection for the window this stage's recovery path exists
/// for: the index write is durable in the bucket, the local repo has not
/// applied it yet, and the client has not been acknowledged.
pub(super) fn crash_point_after_index_write() {
    if test_env_u64("TANDEM_TEST_CRASH_AFTER_INDEX_WRITE") == 0 {
        return;
    }
    tracing::warn!("TANDEM_TEST_CRASH_AFTER_INDEX_WRITE set; exiting before the local apply");
    std::process::exit(99);
}

/// Test-only: stand in for a bucket that fails while the server is writing the
/// WAL entry of a merge operation it minted itself. That write happens after
/// the publish is durable and applied, so the failure must never reach the
/// client. Inert unless the var is set.
fn derived_head_wal_fault() -> Option<anyhow::Error> {
    if test_env_u64("TANDEM_TEST_FAIL_DERIVED_HEAD_WAL") == 0 {
        return None;
    }
    Some(anyhow!(
        "injected bucket failure while writing a derived op head (test hook)"
    ))
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
