//! tandem serve — HTTP server hosting a jj+git backend.
//!
//! The server stores objects through jj's Git backend so that `jj git push`
//! on the server repo just works. Operations and views are stored in the
//! standard jj op_store directory. Op heads are managed through jj-lib's
//! op-heads store; `.jj/repo/tandem/heads.json` stores tandem metadata only
//! (CAS version + workspace head attribution).
//!
//! This file holds the state, the object and operation stores, and the heads
//! logic. Three siblings hold the rest: `http` is the API that exposes it,
//! `bucket` is the durability half — WAL entries, the index object, and the
//! recovery that replays them — and `integration` is the off-request worker
//! that keeps the `integration` bookmark up to date.

mod bucket;
mod http;
mod integration;

use anyhow::{anyhow, bail, Context, Result};
// blake2 is available if needed for raw hashing, but we use jj_lib::content_hash
use jj_lib::backend::{CommitId, TreeId};
use jj_lib::object_id::ObjectId as _;
use jj_lib::op_store::OperationId;
use jj_lib::repo_path::RepoPath;
use prost::Message as _;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex};
use tokio::sync::broadcast;

use self::bucket::{crash_point_after_index_write, test_env_u64, DurableOps, PendingBlobs};
use crate::control;
use crate::hex::{from_hex, to_hex};
use crate::logging;
use crate::object_store::{self, ObjectStore};
use crate::proto_convert;
use crate::wal;
use crate::wire;

// ─── Public entry point ───────────────────────────────────────────────────────

#[allow(dead_code)]
pub struct ServeOptions {
    pub listen_addr: String,
    pub repo_path: String,
    pub log_level: String,
    pub log_format: String,
    pub control_socket: Option<String>,
    pub daemon: bool,
    pub log_file: Option<String>,
    pub enable_integration_workspace: bool,
    /// Bucket location: a directory path, `file://…`, or `s3://…`. When absent
    /// the server keeps its bucket inside the repo, which is enough for dev
    /// and tests but gives up the durability inversion.
    pub bucket: Option<String>,
}

pub async fn run_serve(opts: ServeOptions) -> Result<()> {
    let (log_tx, _) = broadcast::channel::<control::LogEvent>(1024);
    logging::init_tracing(&opts.log_level, &opts.log_format, log_tx.clone())?;

    tracing::info!(
        listen_addr = %opts.listen_addr,
        repo = %opts.repo_path,
        daemon = opts.daemon,
        log_level = %opts.log_level,
        log_format = %opts.log_format,
        integration_workspace = opts.enable_integration_workspace,
        bucket = opts.bucket.as_deref().unwrap_or("<repo-local>"),
        "starting tandem server"
    );
    if let Some(path) = opts.log_file.as_deref() {
        tracing::debug!(log_file = %path, "serve log file argument");
    }

    let repo = PathBuf::from(&opts.repo_path);
    let server = Arc::new(Server::new(
        repo,
        opts.enable_integration_workspace,
        opts.bucket.as_deref(),
    )?);
    server.start_integration_worker();
    let listener = tokio::net::TcpListener::bind(&opts.listen_addr)
        .await
        .with_context(|| format!("failed to bind {}", opts.listen_addr))?;
    let local_addr = listener.local_addr()?;
    tracing::info!(listen_addr = %local_addr, "tandem server listening on");

    // Set up shutdown signaling
    let (shutdown_tx, mut shutdown_rx) = tokio::sync::mpsc::channel::<()>(1);

    // Set up control socket if requested
    let control_socket_path = opts.control_socket.clone();
    if let Some(ref sock_path) = control_socket_path {
        let control_state = Arc::new(control::ControlState {
            pid: std::process::id(),
            start_time: std::time::Instant::now(),
            repo: opts.repo_path.clone(),
            listen: local_addr.to_string(),
            shutdown_tx: shutdown_tx.clone(),
            log_tx: log_tx.clone(),
            integration_enabled: opts.enable_integration_workspace,
            integration_metadata_path: server
                .integration_metadata_path()
                .to_string_lossy()
                .to_string(),
            bucket: server.bucket_status(),
        });

        let sock = sock_path.clone();
        tokio::spawn(async move {
            if let Err(e) = control::run_control_socket(sock.clone(), control_state).await {
                tracing::error!(socket_path = %sock, error = %e, "control socket error");
            }
        });
    }

    // Signal handling
    let (signal_tx, mut signal_rx) = tokio::sync::mpsc::channel::<()>(2);

    // Spawn signal handler (multi-threaded tokio task for signal handling)
    let signal_tx_clone = signal_tx.clone();
    tokio::spawn(async move {
        let mut sigint = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
            .expect("install SIGINT handler");
        let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("install SIGTERM handler");

        let mut first_signal = true;
        loop {
            tokio::select! {
                _ = sigint.recv() => {},
                _ = sigterm.recv() => {},
            }
            if first_signal {
                first_signal = false;
                tracing::warn!("signal received, shutting down gracefully");
                let _ = signal_tx_clone.send(()).await;
            } else {
                tracing::error!("second signal received, forcing shutdown");
                std::process::exit(0);
            }
        }
    });

    // Serve until a signal or the control socket asks to stop.
    let (drain_tx, drain_rx) = tokio::sync::oneshot::channel::<()>();
    let app = http::router(Arc::clone(&server));
    let serve = tokio::spawn(std::future::IntoFuture::into_future(
        axum::serve(listener, app).with_graceful_shutdown(async move {
            let _ = drain_rx.await;
        }),
    ));

    tokio::select! {
        _ = signal_rx.recv() => {
            tracing::info!("signal received, draining connections");
        }
        _ = shutdown_rx.recv() => {
            tracing::info!("shutdown requested via control socket, draining connections");
        }
    }
    let _ = drain_tx.send(());

    // Axum finishes the requests already in flight before it returns. A
    // watcher on `/api/events` holds its connection open indefinitely, so the
    // drain is capped rather than waited out.
    match tokio::time::timeout(tokio::time::Duration::from_secs(5), serve).await {
        Ok(Ok(Ok(()))) => {}
        Ok(Ok(Err(err))) => tracing::error!(error = %err, "http server error"),
        Ok(Err(err)) => tracing::error!(error = %err, "http server task failed"),
        Err(_) => tracing::warn!("drain timeout reached"),
    }

    // Clean up control socket
    if let Some(ref sock_path) = control_socket_path {
        if let Err(e) = std::fs::remove_file(sock_path) {
            if e.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!(socket_path = %sock_path, error = %e, "failed to remove control socket");
            }
        }
    }

    tracing::info!("tandem server stopped");
    Ok(())
}

// ─── Server state ─────────────────────────────────────────────────────────────

/// How many wake-ups a watcher may fall behind before the oldest are dropped.
///
/// Dropping them is safe: an event says only that a version happened, and the
/// watcher's next read of `/api/heads` carries everything it missed.
const HEADS_EVENT_BUFFER: usize = 64;

pub struct Server {
    /// jj Store wrapping the GitBackend — used for all object I/O.
    store: Arc<jj_lib::store::Store>,
    /// Repo loader for jj-lib reads/transactions.
    repo_loader: jj_lib::repo::RepoLoader,
    /// Path to `.jj/repo/op_store/` for operations and views.
    op_store_path: PathBuf,
    /// jj-lib op heads store — single authority for operation heads.
    op_heads_store: Arc<dyn jj_lib::op_heads_store::OpHeadsStore>,
    /// Path to `.jj/repo/tandem/` for tandem metadata sidecar (CAS/workspace map).
    tandem_dir: PathBuf,
    /// The durable source of truth: WAL entries plus the CAS'd index object.
    bucket: Arc<dyn ObjectStore>,
    /// Whether the bucket was proven to enforce conditional puts at startup.
    bucket_conditional_put: bool,
    /// Etag of the index object as this server last saw it.
    index_etag: Mutex<Option<String>>,
    /// Objects written since the last publish, drained into the next WAL entry.
    pending_blobs: Mutex<PendingBlobs>,
    /// Operation ids this process has already written a WAL entry for.
    durable_ops: Mutex<DurableOps>,
    /// Whether this process created the repo directory it is serving. An empty
    /// disk has no local state worth comparing against the bucket.
    bootstrapped: bool,
    /// Op heads that repo init left behind on an empty disk, before any replay.
    bootstrap_op_heads: Vec<String>,
    /// What materializing the repo from the bucket cost at startup.
    boot_replay: Mutex<bucket::BootReplay>,
    /// Test hook: index writes still to be failed artificially.
    test_index_conflicts: AtomicU64,
    integration_enabled: bool,
    integration_trigger: Mutex<Option<tokio::sync::mpsc::UnboundedSender<()>>>,
    lock: Mutex<()>,
    /// Wake-ups for every `/api/events` subscriber.
    heads_events: broadcast::Sender<u64>,
}

// ─── Read failures worth telling apart ────────────────────────────────────────

/// The thing a read asked for is not here.
///
/// Every other read failure — a permission denied, a short read, a corrupt
/// file — is the server's fault, and the two must not answer alike. A 404
/// under the immutable-cache model is a conclusion a client may keep, so a
/// disk fault that wore one would poison a cache with "this object does not
/// exist". Reads raise this marker, `http::ApiError::from_read` matches on it,
/// and everything else becomes a 500 the caller can retry.
#[derive(Debug)]
pub struct NotFound(pub String);

impl std::fmt::Display for NotFound {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for NotFound {}

/// The id a read was given cannot name an object in this backend.
///
/// A wrong-length hash is a malformed request, not a missing object: no
/// amount of writing will ever make that id resolve.
#[derive(Debug)]
pub struct MalformedId(pub String);

impl std::fmt::Display for MalformedId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for MalformedId {}

/// Classify a backend read failure so the API can answer it honestly.
fn backend_read_error(
    error: jj_lib::backend::BackendError,
    what: &str,
    id: &[u8],
) -> anyhow::Error {
    use jj_lib::backend::BackendError;
    let hex = to_hex(id);
    match error {
        BackendError::ObjectNotFound { .. } => {
            anyhow::Error::new(NotFound(format!("{what} not found: {hex}")))
        }
        BackendError::InvalidHashLength { .. } => {
            anyhow::Error::new(MalformedId(format!("{what} id is not a {what} id: {hex}")))
        }
        other => anyhow::Error::new(other).context(format!("read {what} {hex}")),
    }
}

/// The same classification for the operation and view stores, which are plain
/// files rather than a jj backend.
fn file_read_error(error: std::io::Error, what: &str, hex: &str) -> anyhow::Error {
    if error.kind() == std::io::ErrorKind::NotFound {
        anyhow::Error::new(NotFound(format!("{what} not found: {hex}")))
    } else {
        anyhow::Error::new(error).context(format!("read {what} {hex}"))
    }
}

/// Operation ids for an RPC response.
///
/// A head that will not parse is dropped with a warning rather than sent as a
/// zero-length operation id: an empty id fails the client somewhere far from
/// the corrupt string that caused it.
fn head_ids_for_wire(heads: &[String]) -> Vec<Vec<u8>> {
    heads
        .iter()
        .filter_map(|hex| match from_hex(hex) {
            Ok(bytes) => Some(bytes),
            Err(err) => {
                tracing::warn!(head = %hex, error = %err, "dropping an unreadable op head id from the response");
                None
            }
        })
        .collect()
}

impl Server {
    fn new(repo: PathBuf, integration_enabled: bool, bucket_spec: Option<&str>) -> Result<Self> {
        fs::create_dir_all(&repo)?;

        // An empty directory is not an empty repo: with a bucket behind it, it
        // is a repo whose whole history is somewhere else. Remember which of
        // the two this is, because everything about the boot depends on it.
        let bootstrapped = !repo.join(".jj").exists();
        if bootstrapped {
            tracing::info!(
                repo = %repo.display(),
                "no repo on disk; creating one to materialize into"
            );
            Self::init_jj_git_repo(&repo)?;
        }

        let repo_dir = dunce::canonicalize(repo.join(".jj/repo"))
            .with_context(|| format!("cannot canonicalize .jj/repo at {}", repo.display()))?;
        let op_store_path = repo_dir.join("op_store");

        let settings = Self::user_settings()?;
        let factories = jj_lib::repo::StoreFactories::default();
        let loader =
            jj_lib::repo::RepoLoader::init_from_file_system(&settings, &repo_dir, &factories)
                .context("load jj repo state")?;

        // Create tandem-specific directory for CAS/version/workspace metadata.
        let tandem_dir = repo_dir.join("tandem");
        fs::create_dir_all(&tandem_dir)?;

        let metadata_path = tandem_dir.join("heads.json");
        if !metadata_path.exists() {
            let initial = HeadsMetadata {
                version: 0,
                workspace_heads: BTreeMap::new(),
            };
            fs::write(&metadata_path, serde_json::to_vec_pretty(&initial)?)?;
        }

        let bucket = match bucket_spec {
            Some(spec) => {
                object_store::open(spec).with_context(|| format!("open bucket {spec}"))?
            }
            None => object_store::open_filesystem(&tandem_dir.join("bucket"))
                .context("open repo-local bucket")?,
        };

        let bucket_conditional_put = match object_store::probe_conditional_put(bucket.as_ref()) {
            Ok(supported) => supported,
            Err(err) => {
                tracing::warn!(
                    bucket = %bucket.describe(),
                    error = %err,
                    "conditional-put probe failed; assuming unsupported"
                );
                false
            }
        };
        tracing::info!(
            bucket = %bucket.describe(),
            backend = bucket.backend_name(),
            conditional_put = bucket_conditional_put,
            "bucket ready"
        );
        if !bucket_conditional_put {
            tracing::warn!(
                bucket = %bucket.describe(),
                "bucket does not enforce conditional puts; index writes rely on \
                 this server being the only writer"
            );
        }

        let op_heads_store = loader.op_heads_store().clone();
        let mut server = Self {
            store: loader.store().clone(),
            repo_loader: loader,
            op_store_path,
            op_heads_store,
            tandem_dir,
            bucket,
            bucket_conditional_put,
            index_etag: Mutex::new(None),
            pending_blobs: Mutex::new(PendingBlobs::default()),
            durable_ops: Mutex::new(DurableOps::default()),
            bootstrapped,
            bootstrap_op_heads: Vec::new(),
            boot_replay: Mutex::new(bucket::BootReplay::default()),
            test_index_conflicts: AtomicU64::new(test_env_u64("TANDEM_TEST_INDEX_CAS_CONFLICTS")),
            integration_enabled,
            integration_trigger: Mutex::new(None),
            lock: Mutex::new(()),
            heads_events: broadcast::channel(HEADS_EVENT_BUFFER).0,
        };
        server.initialize_integration_metadata()?;
        if bootstrapped {
            // Read before the replay, so recovery can tell the operation this
            // init just minted from the ones the bucket is about to hand back.
            server.bootstrap_op_heads = server.read_jj_op_heads()?;
        }
        server.recover_from_bucket()?;
        Ok(server)
    }

    /// Where the durable history lives and what this boot had to fetch from it.
    fn bucket_status(&self) -> control::BucketStatus {
        let replay = self.boot_replay();
        control::BucketStatus {
            backend: self.bucket.backend_name().to_string(),
            location: self.bucket.describe(),
            conditional_put: self.bucket_conditional_put,
            materialized: self.bootstrapped,
            replayed_heads: replay.heads,
            replayed_entries: replay.entries,
            replay_ms: replay.millis,
        }
    }

    fn user_settings() -> Result<jj_lib::settings::UserSettings> {
        let config_env = jj_cli::config::ConfigEnv::from_environment();
        let mut raw_config =
            jj_cli::config::config_from_environment(jj_cli::config::default_config_layers());
        config_env
            .reload_user_config(&mut raw_config)
            .context("load jj user config")?;
        let resolved = config_env
            .resolve_config(&raw_config)
            .context("resolve jj config")?;
        jj_lib::settings::UserSettings::from_config(resolved).context("create jj settings")
    }

    /// Initialize a new jj+git colocated repo.
    fn init_jj_git_repo(repo_path: &Path) -> Result<()> {
        let settings = Self::user_settings()?;
        jj_lib::workspace::Workspace::init_colocated_git(&settings, repo_path)
            .context("init colocated git repo")?;
        Ok(())
    }

    fn read_jj_op_heads(&self) -> Result<Vec<String>> {
        let ids = pollster::block_on(self.op_heads_store.get_op_heads())
            .map_err(|e| anyhow!("read op heads: {e}"))?;
        let mut heads: Vec<String> = ids.into_iter().map(|id| id.hex()).collect();
        heads.sort();
        Ok(heads)
    }

    /// Merge every operation head into one, and record that one as the head.
    ///
    /// The candidates are the operations the op-heads store holds plus the last
    /// operation each workspace published. The second half matters because a
    /// head the store has lost — a crash between two writes, a client that
    /// published and never came back — is otherwise unreachable, and the
    /// workspace record is the only place it is still named.
    fn reconcile_jj_op_heads(
        &self,
        workspace_heads: &BTreeMap<String, String>,
    ) -> Result<Vec<String>> {
        let before = self.read_jj_op_heads()?;

        let mut candidate_hex = before.clone();
        for op_hex in workspace_heads.values() {
            if !candidate_hex.contains(op_hex) {
                candidate_hex.push(op_hex.clone());
            }
        }
        candidate_hex.sort();
        candidate_hex.dedup();

        if candidate_hex.len() <= 1 {
            return Ok(before);
        }

        let mut operations = Vec::new();
        for op_hex in &candidate_hex {
            let op_id = match from_hex(op_hex) {
                Ok(bytes) => OperationId::new(bytes),
                Err(err) => {
                    tracing::warn!(op_id = %op_hex, error = %err, "skipping invalid workspace head id");
                    continue;
                }
            };
            match self.repo_loader.load_operation(&op_id) {
                Ok(op) => operations.push(op),
                Err(err) => {
                    tracing::warn!(op_id = %op_id.hex(), error = %err, "skipping missing workspace head operation");
                }
            }
        }

        if operations.len() <= 1 {
            return Ok(before);
        }

        // ── Ancestors first, then the merge ──
        //
        // A candidate that another candidate already descends from is not a
        // branch; it is the same branch seen earlier. `workspace_heads` is full
        // of those, because it records the operation each workspace published
        // last and the server has usually merged that operation since.
        //
        // Merging an operation with its own descendant is what jj-lib's head
        // resolution goes out of its way to avoid, and for good reason: the
        // merge is computed against a base that is older than the rewrite the
        // descendant carries, so a commit the descendant replaced comes back
        // beside its replacement — two commits, one change id, a divergent
        // change. That is the whole of the slice14 flake.
        //
        // So the candidates are filtered down to real heads, exactly as
        // `jj_lib::op_heads_store` does it. What is emphatically *not* dropped
        // is the removal: every candidate that is not the settled head is still
        // named to `update_op_heads`, so an ancestor that is sitting in the
        // op-heads store is taken out of it. Filtering the merge without
        // filtering the removal is what leaves stale heads in the store, and
        // that is what makes clients see a divergent operation history.
        let ordered = match order_op_heads(operations) {
            Ok(ordered) => ordered,
            Err(err) => {
                tracing::warn!(
                    candidates = candidate_hex.len(),
                    error = %err,
                    "could not order divergent operation heads; leaving them unmerged"
                );
                return Ok(before);
            }
        };

        tracing::debug!(
            candidates = candidate_hex.len(),
            heads = ordered.len(),
            workspace_heads = workspace_heads.len(),
            "reconciling divergent operation heads"
        );

        let settled_op = if ordered.len() == 1 {
            // One real head, and every other candidate is behind it. There is
            // nothing to merge, but there may still be stale ids to retire.
            ordered.into_iter().next().expect("one head")
        } else {
            match self
                .repo_loader
                .merge_operations(ordered, Some("reconcile divergent operations"))
            {
                Ok(op) => op,
                // Merging views is a convenience, not a correctness requirement:
                // multiple op heads are a state every jj client already resolves on
                // its own. Some head pairs cannot be merged at all — a workspace
                // whose working-copy commit differs across the two sides and is the
                // root commit on one of them needs a merge commit the git backend
                // refuses to write. Failing here would fail the publish that has
                // already been made durable and already been applied, and the
                // client's retry would rewrite the same change a second time, which
                // is how a change id goes divergent. Hand back the unmerged heads.
                Err(err) => {
                    tracing::warn!(
                        candidates = candidate_hex.len(),
                        error = %err,
                        "could not merge divergent operation heads; leaving them unmerged"
                    );
                    return Ok(before);
                }
            }
        };

        // ── Keep every workspace operation next to the head ──
        //
        // A client refuses to run when its working-copy operation is neither
        // the operation the repo loaded at nor an ancestor of it. It decides
        // which by `dag_walk::closest_common_node_ok`, a breadth-first search
        // from both ends that returns the first node the two sides have both
        // seen. That is an approximation: when the working-copy operation is a
        // *distant* ancestor of the head, the search from the working-copy side
        // reaches the operation's own ancestors before the search from the head
        // side has walked back far enough, so it settles on a common ancestor
        // that is neither end, and the client calls a plain ancestor a sibling:
        //
        //   Internal error: The repo was loaded at operation X, which seems to
        //   be a sibling of the working copy's operation Y
        //
        // A workspace that sits idle while other workspaces publish drifts
        // exactly that far. So the settled head records the merged-away
        // candidates as extra parents, which puts every workspace's last
        // operation one step from the head and inside what the search can see.
        // The operation's view is the settled view unchanged — these parents
        // were already merged, so there is nothing left to merge, and building
        // it by hand rather than through another `merge_operations` is what
        // keeps a view merge against an outdated base from resurrecting the
        // commits that base has since had rewritten.
        let settled_op = match self.record_merged_parents(settled_op, &candidate_hex) {
            Ok(op) => op,
            Err(err) => {
                tracing::warn!(
                    error = %err,
                    "could not record the merged operations as parents; \
                     acknowledging the merge without them"
                );
                return Ok(before);
            }
        };

        let mut old_ids = Vec::new();
        for op_hex in candidate_hex {
            if let Ok(bytes) = from_hex(&op_hex) {
                let op_id = OperationId::new(bytes);
                if op_id != *settled_op.id() {
                    old_ids.push(op_id);
                }
            }
        }

        if old_ids.is_empty() && before == [settled_op.id().hex()] {
            return Ok(before);
        }

        pollster::block_on(
            self.op_heads_store
                .update_op_heads(&old_ids, settled_op.id()),
        )
        .map_err(|e| anyhow!("reconcile op heads update failed: {e}"))?;

        let after = self.read_jj_op_heads()?;
        if after != before {
            tracing::debug!(
                before_heads = before.len(),
                after_heads = after.len(),
                "reconciled operation heads"
            );
        }
        Ok(after)
    }

    /// Name the already-merged candidates as parents of the settled head.
    ///
    /// The written operation carries the settled operation's view byte for
    /// byte. Nothing about the repository changes; what changes is how far a
    /// client has to walk to find its own working-copy operation, which is the
    /// whole point (see the caller). If every candidate is already the head or
    /// one of its parents, the head is handed back untouched.
    fn record_merged_parents(
        &self,
        settled: jj_lib::operation::Operation,
        candidate_hex: &[String],
    ) -> Result<jj_lib::operation::Operation> {
        let mut parents = vec![settled.id().clone()];
        for op_hex in candidate_hex {
            let Ok(bytes) = from_hex(op_hex) else {
                continue;
            };
            let op_id = OperationId::new(bytes);
            if op_id == *settled.id()
                || settled.parent_ids().contains(&op_id)
                || parents.contains(&op_id)
            {
                continue;
            }
            parents.push(op_id);
        }
        if parents.len() == 1 {
            return Ok(settled);
        }

        let now = jj_lib::backend::Timestamp::now();
        let metadata = jj_lib::op_store::OperationMetadata {
            time: jj_lib::op_store::TimestampRange {
                start: now,
                end: now,
            },
            description: "record merged operations".to_string(),
            hostname: settled.metadata().hostname.clone(),
            username: settled.metadata().username.clone(),
            is_snapshot: false,
            tags: std::collections::HashMap::new(),
        };
        let data = jj_lib::op_store::Operation {
            view_id: settled.view_id().clone(),
            parents,
            metadata,
            commit_predecessors: None,
        };

        let op_store = self.repo_loader.op_store().clone();
        let id = pollster::block_on(op_store.write_operation(&data))
            .map_err(|e| anyhow!("write the operation recording merged parents: {e}"))?;
        Ok(jj_lib::operation::Operation::new(op_store, id, data))
    }

    // ─── Object operations (through git backend) ─────────────────────

    fn get_object_sync(&self, kind: &str, id: &[u8]) -> Result<Vec<u8>> {
        let backend = self.store.backend();

        match kind {
            "file" => {
                let file_id = jj_lib::backend::FileId::new(id.to_vec());
                let mut reader = pollster::block_on(backend.read_file(RepoPath::root(), &file_id))
                    .map_err(|e| backend_read_error(e, "file", id))?;
                let mut buf = Vec::new();
                pollster::block_on(tokio::io::AsyncReadExt::read_to_end(&mut reader, &mut buf))
                    .map_err(|e| anyhow!("read file bytes: {e}"))?;
                Ok(buf)
            }
            "tree" => {
                let tree_id = TreeId::new(id.to_vec());
                let tree = pollster::block_on(backend.read_tree(RepoPath::root(), &tree_id))
                    .map_err(|e| backend_read_error(e, "tree", id))?;
                let proto = proto_convert::tree_to_proto(&tree);
                Ok(proto.encode_to_vec())
            }
            "commit" => {
                let commit_id = CommitId::new(id.to_vec());
                if commit_id == *backend.root_commit_id() {
                    let commit = jj_lib::backend::make_root_commit(
                        backend.root_change_id().clone(),
                        backend.empty_tree_id().clone(),
                    );
                    let proto = jj_lib::simple_backend::commit_to_proto(&commit);
                    return Ok(proto.encode_to_vec());
                }
                let commit = pollster::block_on(backend.read_commit(&commit_id))
                    .map_err(|e| backend_read_error(e, "commit", id))?;
                let proto = jj_lib::simple_backend::commit_to_proto(&commit);
                Ok(proto.encode_to_vec())
            }
            "symlink" => {
                let symlink_id = jj_lib::backend::SymlinkId::new(id.to_vec());
                let target =
                    pollster::block_on(backend.read_symlink(RepoPath::root(), &symlink_id))
                        .map_err(|e| backend_read_error(e, "symlink", id))?;
                Ok(target.into_bytes())
            }
            "copy" => {
                bail!("copy objects not yet supported")
            }
            _ => bail!("unknown object kind: {kind}"),
        }
    }

    /// Write an object and stage it for the next WAL entry.
    fn put_object_sync(&self, kind: &str, data: &[u8]) -> Result<(Vec<u8>, Vec<u8>)> {
        let (id, normalized) = self.write_object_sync(kind, data)?;
        if let Some(record_kind) = wal::RecordKind::from_object_kind(kind) {
            self.pending_blobs
                .lock()
                .map_err(|e| anyhow!("pending blobs lock: {e}"))?
                .stage(wal::WalRecord {
                    kind: record_kind,
                    id: id.clone(),
                    data: normalized.clone(),
                })
                .context("stage the object for the next WAL entry")?;
        }
        Ok((id, normalized))
    }

    /// Write an object to the local git backend. Content-addressed, so writing
    /// the same bytes twice is a no-op.
    fn write_object_sync(&self, kind: &str, data: &[u8]) -> Result<(Vec<u8>, Vec<u8>)> {
        let backend = self.store.backend();

        match kind {
            "file" => {
                let mut cursor = Cursor::new(data.to_vec());
                let file_id = pollster::block_on(backend.write_file(RepoPath::root(), &mut cursor))
                    .map_err(|e| anyhow!("write file: {e}"))?;
                Ok((file_id.as_bytes().to_vec(), data.to_vec()))
            }
            "tree" => {
                let proto = jj_lib::protos::simple_store::Tree::decode(data)
                    .context("decode tree proto")?;
                let tree = proto_convert::tree_from_proto(proto);
                let tree_id = pollster::block_on(backend.write_tree(RepoPath::root(), &tree))
                    .map_err(|e| anyhow!("write tree: {e}"))?;
                // Return the original proto data as normalized (the tree is the same)
                Ok((tree_id.as_bytes().to_vec(), data.to_vec()))
            }
            "commit" => {
                let proto = jj_lib::protos::simple_store::Commit::decode(data)
                    .context("decode commit proto")?;
                let commit = proto_convert::commit_from_proto(proto);
                let (commit_id, stored_commit) =
                    pollster::block_on(backend.write_commit(commit, None))
                        .map_err(|e| anyhow!("write commit: {e}"))?;
                // Re-encode the stored commit (may have normalized fields)
                let stored_proto = jj_lib::simple_backend::commit_to_proto(&stored_commit);
                let normalized_data = stored_proto.encode_to_vec();
                Ok((commit_id.as_bytes().to_vec(), normalized_data))
            }
            "symlink" => {
                let target =
                    std::str::from_utf8(data).context("symlink target is not valid UTF-8")?;
                let symlink_id =
                    pollster::block_on(backend.write_symlink(RepoPath::root(), target))
                        .map_err(|e| anyhow!("write symlink: {e}"))?;
                Ok((symlink_id.as_bytes().to_vec(), data.to_vec()))
            }
            "copy" => {
                bail!("copy objects not yet supported")
            }
            _ => bail!("unknown object kind: {kind}"),
        }
    }

    // ─── Operation/View operations ────────────────────────────────────
    //
    // Operations and views are stored in jj's op_store directory using
    // ContentHash-based IDs (compatible with jj's SimpleOpStore).

    fn get_operation_sync(&self, id: &[u8]) -> Result<Vec<u8>> {
        let hex = to_hex(id);
        let path = self.op_store_path.join("operations").join(&hex);
        fs::read(&path).map_err(|e| file_read_error(e, "operation", &hex))
    }

    fn put_operation_sync(&self, data: &[u8]) -> Result<Vec<u8>> {
        // Decode proto → Operation struct → compute ContentHash-based ID
        let proto = jj_lib::protos::simple_op_store::Operation::decode(data)
            .context("decode operation proto")?;
        let operation =
            proto_convert::operation_from_proto(proto).context("convert operation from proto")?;

        let hash = jj_lib::content_hash::blake2b_hash(&operation);
        let id: Vec<u8> = hash.to_vec();
        let hex = to_hex(&id);

        let dir = self.op_store_path.join("operations");
        let path = dir.join(&hex);
        write_bytes_if_missing(&path, data)?;
        Ok(id)
    }

    fn get_view_sync(&self, id: &[u8]) -> Result<Vec<u8>> {
        let hex = to_hex(id);
        let path = self.op_store_path.join("views").join(&hex);
        fs::read(&path).map_err(|e| file_read_error(e, "view", &hex))
    }

    fn put_view_sync(&self, data: &[u8]) -> Result<Vec<u8>> {
        // Decode proto → View struct → compute ContentHash-based ID
        let proto =
            jj_lib::protos::simple_op_store::View::decode(data).context("decode view proto")?;
        let view = proto_convert::view_from_proto(proto).context("convert view from proto")?;

        let hash = jj_lib::content_hash::blake2b_hash(&view);
        let id: Vec<u8> = hash.to_vec();
        let hex = to_hex(&id);

        let dir = self.op_store_path.join("views");
        let path = dir.join(&hex);
        write_bytes_if_missing(&path, data)?;
        Ok(id)
    }

    // ─── Operation prefix resolution ──────────────────────────────────

    fn resolve_operation_id_prefix_sync(
        &self,
        hex_prefix: &str,
    ) -> Result<(String, Option<Vec<u8>>)> {
        let mut matches = Vec::new();
        let dir = self.op_store_path.join("operations");
        if let Ok(entries) = fs::read_dir(dir) {
            for entry in entries {
                let entry = entry?;
                let file_name = entry.file_name();
                let file_name = file_name.to_string_lossy();
                if file_name.starts_with(hex_prefix) {
                    matches.push(file_name.to_string());
                }
            }
        }
        matches.sort();
        match matches.len() {
            0 => Ok(("noMatch".to_string(), None)),
            1 => {
                let id_bytes = from_hex(&matches[0])?;
                Ok(("singleMatch".to_string(), Some(id_bytes)))
            }
            _ => Ok(("ambiguous".to_string(), None)),
        }
    }

    // ─── Heads management ─────────────────────────────────────────────

    /// Read the head state. A read only reads: it does not reconcile.
    ///
    /// Reconciling here used to mint a merge operation, bump the version and
    /// write the index, all inside a call a client makes in the middle of a
    /// command. The command that then published its own operation found the
    /// version moved, retried its transaction, and rewrote the same change a
    /// second time — which is how a change id ends up divergent. Multiple heads
    /// are not an error state: every jj client resolves them itself and
    /// publishes the merge back through `update_op_heads`, where the head set
    /// is reconciled on the write path, in the bucket, before the ack.
    fn get_heads_sync(&self) -> Result<HeadsState> {
        let _guard = self.lock.lock().map_err(|e| anyhow!("lock: {e}"))?;
        let metadata = self.read_heads_metadata()?;
        let heads = self.read_jj_op_heads()?;

        // The workspace entries are reported as recorded. A client uses its own
        // entry to find the operation its working copy was written at, and that
        // operation is not always a head — dropping it makes the client load a
        // repo that its own working copy is a sibling of.
        Ok(HeadsState {
            version: metadata.version,
            heads,
            workspace_heads: metadata.workspace_heads,
        })
    }

    fn update_op_heads_sync(
        &self,
        old_ids: Vec<Vec<u8>>,
        new_id: Vec<u8>,
        expected_version: u64,
        workspace_id: Option<String>,
    ) -> Result<UpdateResult> {
        let _guard = self.lock.lock().map_err(|e| anyhow!("lock: {e}"))?;
        let metadata = self.read_heads_metadata()?;

        if metadata.version != expected_version {
            // Same rule as get_heads: reporting a stale version is a read, and
            // a read does not reconcile. The client is about to retry, and the
            // retry's publish reconciles on the write path.
            let current_heads = self.read_jj_op_heads()?;
            tracing::debug!(
                expected_version,
                actual_version = metadata.version,
                "update_op_heads version mismatch"
            );
            return Ok(UpdateResult {
                ok: false,
                heads: head_ids_for_wire(&current_heads),
                version: metadata.version,
                workspace_heads: metadata.workspace_heads,
            });
        }

        let provided_old_op_ids: Vec<jj_lib::op_store::OperationId> = old_ids
            .into_iter()
            .map(jj_lib::op_store::OperationId::new)
            .collect();
        let new_op_id = jj_lib::op_store::OperationId::new(new_id.clone());

        let mut old_op_ids = match pollster::block_on(
            self.repo_loader.op_store().read_operation(&new_op_id),
        ) {
            Ok(new_op) => new_op.parents,
            Err(err) => {
                tracing::warn!(
                    new_id = %new_op_id.hex(),
                    error = %err,
                    "could not read new operation parents; falling back to client-provided old_ids"
                );
                provided_old_op_ids.clone()
            }
        };

        if old_op_ids.is_empty() {
            old_op_ids = provided_old_op_ids;
        }

        old_op_ids.retain(|id| id != &new_op_id);

        let new_hex = to_hex(&new_id);
        let next_workspace_heads =
            updated_workspace_heads(&metadata.workspace_heads, workspace_id.as_deref(), &new_hex);
        let next_version = metadata.version + 1;

        // ── Durable before ack ──
        //
        // 1. The WAL entry: this operation, its view, and every blob written
        //    since the last publish.
        if let Err(err) = self.write_publish_wal_entry(&new_hex) {
            // Nothing was acknowledged and nothing was applied locally, so the
            // client's transaction retry is free to start over.
            return Err(err.context("write WAL entry before acknowledging head update"));
        }

        // 2. The index object, naming the head set this update produces. The
        //    set is computed before the local apply so the bucket commits
        //    first: a crash after this point is replayed at the next start.
        let prospective_heads = self.prospective_op_heads(&old_op_ids, &new_op_id)?;
        if self.inject_index_conflict()
            || !self.publish_index(next_version, &prospective_heads, &next_workspace_heads)?
        {
            return self.index_conflict_result(metadata);
        }

        crash_point_after_index_write();

        // ── Local apply ──
        //
        // This is the last step that may still fail the RPC, and it is the
        // boundary on purpose. The bucket has committed, so nothing is lost:
        // the failure leaves exactly the state the crash window leaves, which
        // the next start replays. Acknowledging instead would promise a head
        // the local repo cannot serve, since recovery runs only at startup.
        pollster::block_on(self.op_heads_store.update_op_heads(&old_op_ids, &new_op_id))
            .map_err(|e| anyhow!("update op heads via jj-lib: {e}"))?;

        // ── Past the point of no return ──
        //
        // The update is durable in the bucket and applied to the local repo.
        // Everything below only tidies up, so nothing below may return an
        // error: the client would see a failed RPC for an operation that had
        // already landed, retry its transaction, and rewrite the same change —
        // which is how a change id goes divergent. Each step degrades to the
        // last state known to be both durable and correct, and says so.
        let next_heads = match self.reconcile_jj_op_heads(&next_workspace_heads) {
            Ok(heads) => heads,
            Err(err) => {
                tracing::warn!(
                    error = %err,
                    "could not reconcile heads after publishing; acknowledging the unmerged set"
                );
                self.read_jj_op_heads().unwrap_or_else(|err| {
                    tracing::warn!(
                        error = %err,
                        "could not re-read op heads after publishing; acknowledging the published set"
                    );
                    prospective_heads.clone()
                })
            }
        };

        let next_metadata = HeadsMetadata {
            version: next_version,
            workspace_heads: next_workspace_heads.clone(),
        };
        if let Err(err) = self.write_heads_metadata(&next_metadata) {
            // The bucket index carries this version already, and recovery reads
            // the version from the bucket, so the local file is a cache of it.
            tracing::error!(
                version = next_version,
                error = %err,
                "could not record the new heads metadata locally; the bucket index still holds it"
            );
        }

        // Reconciling divergent heads mints a merge operation the index does
        // not know about yet. Make it durable, then mirror the settled head set
        // back. If it cannot be made durable, acknowledge the set the index
        // already committed rather than a head the bucket has never seen.
        let acked_heads = if next_heads != prospective_heads {
            self.publish_derived_heads(
                next_version,
                &next_heads,
                &prospective_heads,
                &next_workspace_heads,
            )
        } else {
            next_heads
        };

        tracing::debug!(
            previous_version = metadata.version,
            new_version = next_metadata.version,
            heads = acked_heads.len(),
            workspace_heads = next_workspace_heads.len(),
            "updated heads state"
        );

        let heads_bytes = head_ids_for_wire(&acked_heads);

        self.notify_watchers(next_metadata.version);
        if self.integration_enabled {
            self.enqueue_integration_recompute();
        }

        Ok(UpdateResult {
            ok: true,
            heads: heads_bytes,
            version: next_metadata.version,
            workspace_heads: next_workspace_heads,
        })
    }

    /// A new subscriber on `/api/events`.
    fn subscribe_heads(&self) -> broadcast::Receiver<u64> {
        self.heads_events.subscribe()
    }

    /// Tell every watcher that the head set moved.
    ///
    /// The event carries the version and nothing else. A watcher answers it
    /// by reading `/api/heads`, which is why a wake-up that is dropped, or
    /// coalesced with the one behind it, costs a watcher nothing.
    fn notify_watchers(&self, version: u64) {
        match self.heads_events.send(version) {
            Ok(watchers) => {
                tracing::trace!(watchers, version, "woke watchers");
            }
            // No subscribers is the normal case, not a failure.
            Err(_) => tracing::trace!(version, "no watchers to wake"),
        }
    }

    /// What a client checks before it trusts this server with its repo.
    ///
    /// The `TANDEM_TEST_REPO_INFO_*` variables let a test stand up a server
    /// that claims to be something a client must refuse.
    fn repo_info_body(&self) -> wire::RepoInfoBody {
        let backend = self.store.backend();
        wire::RepoInfoBody {
            protocol_major: test_repo_info_u16(
                "TANDEM_TEST_REPO_INFO_PROTOCOL_MAJOR",
                wire::PROTOCOL_MAJOR,
            ),
            protocol_minor: test_repo_info_u16(
                "TANDEM_TEST_REPO_INFO_PROTOCOL_MINOR",
                wire::PROTOCOL_MINOR,
            ),
            tandem_version: env!("CARGO_PKG_VERSION").to_string(),
            backend_name: test_repo_info_text(
                "TANDEM_TEST_REPO_INFO_BACKEND_NAME",
                wire::BACKEND_NAME,
            ),
            op_store_name: test_repo_info_text(
                "TANDEM_TEST_REPO_INFO_OP_STORE_NAME",
                wire::OP_STORE_NAME,
            ),
            commit_id_length: backend.commit_id_length() as u32,
            change_id_length: backend.change_id_length() as u32,
            root_commit_id: to_hex(backend.root_commit_id().as_bytes()),
            root_change_id: to_hex(backend.root_change_id().as_bytes()),
            empty_tree_id: to_hex(backend.empty_tree_id().as_bytes()),
            root_operation_id: to_hex(&[0u8; 64]),
            capabilities: test_repo_info_capabilities(),
        }
    }

    fn read_heads_metadata(&self) -> Result<HeadsMetadata> {
        let bytes = fs::read(self.tandem_dir.join("heads.json"))?;
        let metadata = serde_json::from_slice(&bytes)?;
        Ok(metadata)
    }

    fn write_heads_metadata(&self, metadata: &HeadsMetadata) -> Result<()> {
        fs::write(
            self.tandem_dir.join("heads.json"),
            serde_json::to_vec_pretty(metadata)?,
        )?;
        Ok(())
    }
}

// ─── Data types ───────────────────────────────────────────────────────────────

struct UpdateResult {
    ok: bool,
    heads: Vec<Vec<u8>>,
    version: u64,
    workspace_heads: BTreeMap<String, String>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct HeadsMetadata {
    version: u64,
    #[serde(default)]
    workspace_heads: BTreeMap<String, String>, // hex-encoded
}

struct HeadsState {
    version: u64,
    heads: Vec<String>, // hex-encoded op IDs from jj-lib op-heads store
    workspace_heads: BTreeMap<String, String>, // hex-encoded
}

// ─── Repo-info test hooks ─────────────────────────────────────────────────────
//
// The compatibility handshake is the one place a client refuses a server
// outright, so a test needs a way to make a server claim to be the wrong
// thing. These read the claim out of the environment.

fn test_repo_info_u16(var: &str, default: u16) -> u16 {
    std::env::var(var)
        .ok()
        .and_then(|v| v.trim().parse::<u16>().ok())
        .unwrap_or(default)
}

fn test_repo_info_text(var: &str, default: &'static str) -> String {
    std::env::var(var).unwrap_or_else(|_| default.to_string())
}

/// What this server advertises. `watchHeads` is the only one it implements;
/// the rest are here so a test can make a server claim otherwise.
const ADVERTISED_CAPABILITIES: &[wire::RepoCapability] = &[wire::RepoCapability::WatchHeads];

fn test_repo_info_capabilities() -> Vec<String> {
    let Ok(raw) = std::env::var("TANDEM_TEST_REPO_INFO_CAPABILITIES") else {
        return ADVERTISED_CAPABILITIES
            .iter()
            .map(|cap| cap.as_str().to_string())
            .collect();
    };

    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }

    // Parsing through the same enum the client parses with is the point: a
    // name neither side knows is dropped here rather than advertised.
    let mut caps: Vec<String> = Vec::new();
    for token in trimmed.split(',') {
        let Some(capability) = wire::RepoCapability::from_name(token.trim()) else {
            continue;
        };
        let name = capability.as_str().to_string();
        if !caps.contains(&name) {
            caps.push(name);
        }
    }
    caps
}

// ─── Helpers ──────────────────────────────────────────────────────────────────

/// Keep the operations nothing else in the set descends from, oldest first.
///
/// Both halves come from `jj_lib::op_heads_store::resolve_op_heads`, which is
/// the reference implementation for this: ancestors are dropped so that no
/// operation is ever merged with its own descendant, and what is left is
/// ordered by end time so the merge starts from the oldest state and applies
/// the later ones onto it. `merge_operations` takes the first entry as the
/// base, so the order is not cosmetic.
fn order_op_heads(
    operations: Vec<jj_lib::operation::Operation>,
) -> Result<Vec<jj_lib::operation::Operation>> {
    let heads = jj_lib::dag_walk::heads_ok(
        operations.into_iter().map(Ok),
        |op: &jj_lib::operation::Operation| op.id().clone(),
        |op: &jj_lib::operation::Operation| op.parents().collect::<Vec<_>>(),
    )
    .map_err(|e: jj_lib::op_store::OpStoreError| anyhow!("walk operation parents: {e}"))?;

    let mut heads: Vec<_> = heads.into_iter().collect();
    heads.sort_by_key(|op| op.metadata().time.end.timestamp);
    Ok(heads)
}

fn updated_workspace_heads(
    current: &BTreeMap<String, String>,
    workspace_id: Option<&str>,
    new_id: &str,
) -> BTreeMap<String, String> {
    let mut next = current.clone();
    if let Some(ws_id) = workspace_id {
        if !ws_id.is_empty() {
            next.insert(ws_id.to_string(), new_id.to_string());
        }
    }
    next
}

/// jj's root operation id is all zeros. It names an empty repository rather
/// than a stored operation, so it never gets a WAL entry.
fn is_root_operation_hex(op_hex: &str) -> bool {
    !op_hex.is_empty() && op_hex.bytes().all(|b| b == b'0')
}

/// Write a content-addressed file, once.
///
/// Requests now run in parallel, so two clients can write the same operation
/// at the same moment while a third reads it. The write therefore lands in a
/// scratch file and is renamed into place: a reader sees either no file or
/// the whole file, never half of one.
fn write_bytes_if_missing(path: &Path, bytes: &[u8]) -> Result<()> {
    if path.exists() {
        return Ok(());
    }
    let Some(parent) = path.parent() else {
        bail!(
            "cannot write {} — it has no parent directory",
            path.display()
        );
    };
    fs::create_dir_all(parent)?;

    static SCRATCH_SEQUENCE: AtomicU64 = AtomicU64::new(0);
    let scratch = parent.join(format!(
        ".{}.{}.{}.tmp",
        path.file_name().unwrap_or_default().to_string_lossy(),
        std::process::id(),
        SCRATCH_SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    fs::write(&scratch, bytes)?;
    match fs::rename(&scratch, path) {
        Ok(()) => Ok(()),
        Err(err) => {
            let _ = fs::remove_file(&scratch);
            Err(err.into())
        }
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// A head id that will not parse is dropped, not sent as an empty id.
    #[test]
    fn unreadable_head_ids_are_dropped_from_the_wire() {
        let heads = vec![
            "00ff".to_string(),
            "not-hex".to_string(),
            "abc".to_string(),
            "1234".to_string(),
        ];
        assert_eq!(
            head_ids_for_wire(&heads),
            vec![vec![0x00, 0xff], vec![0x12, 0x34]]
        );
    }
}
