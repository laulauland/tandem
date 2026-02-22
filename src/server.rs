//! tandem serve — Cap'n Proto RPC server hosting a jj+git backend.
//!
//! The server stores objects through jj's Git backend so that `jj git push`
//! on the server repo just works. Operations and views are stored in the
//! standard jj op_store directory. Op heads are managed through jj-lib's
//! op-heads store; `.jj/repo/tandem/heads.json` stores tandem metadata only
//! (CAS version + workspace head attribution).

use anyhow::{anyhow, bail, Context, Result};
// blake2 is available if needed for raw hashing, but we use jj_lib::content_hash
use capnp::capability::Promise;
use capnp_rpc::pry;
use capnp_rpc::{rpc_twoparty_capnp, twoparty, RpcSystem};
use jj_lib::backend::{CommitId, TreeId};
use jj_lib::object_id::ObjectId as _;
use jj_lib::repo_path::RepoPath;
use prost::Message as _;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::broadcast;

use crate::control;
use crate::logging;
use crate::proto_convert;
use crate::tandem_capnp::{cancel, head_watcher, store};

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
        "starting tandem server"
    );
    if let Some(path) = opts.log_file.as_deref() {
        tracing::debug!(log_file = %path, "serve log file argument");
    }

    let repo = PathBuf::from(&opts.repo_path);
    let server = Rc::new(Server::new(repo)?);
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

    // Track in-flight connections
    let inflight = Rc::new(std::cell::Cell::new(0u32));
    let connection_ids = Arc::new(AtomicU64::new(1));

    // Accept loop with shutdown
    loop {
        tokio::select! {
            result = listener.accept() => {
                let (stream, addr) = result?;
                let server = Rc::clone(&server);
                let inflight = Rc::clone(&inflight);
                let conn_id = connection_ids.fetch_add(1, Ordering::Relaxed);

                let next = inflight.get() + 1;
                inflight.set(next);
                tracing::info!(conn_id, peer = %addr, inflight = next, "client connected");

                tokio::task::spawn_local(async move {
                    if let Err(err) = handle_capnp_connection(server, stream, conn_id).await {
                        tracing::error!(conn_id, peer = %addr, error = %err, "rpc connection error");
                    }
                    let remaining = inflight.get().saturating_sub(1);
                    inflight.set(remaining);
                    tracing::info!(conn_id, peer = %addr, inflight = remaining, "client disconnected");
                });
            }
            _ = signal_rx.recv() => {
                tracing::info!("signal received, draining connections");
                break;
            }
            _ = shutdown_rx.recv() => {
                tracing::info!("shutdown requested via control socket, draining connections");
                break;
            }
        }
    }

    // Drain in-flight connections (5s timeout)
    if inflight.get() > 0 {
        tracing::info!(
            inflight = inflight.get(),
            "waiting for in-flight connections to drain"
        );
        let drain_deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(5);
        while inflight.get() > 0 {
            if tokio::time::Instant::now() > drain_deadline {
                tracing::warn!(inflight = inflight.get(), "drain timeout reached");
                break;
            }
            tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
        }
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

// ─── Connection handler ───────────────────────────────────────────────────────

async fn handle_capnp_connection(
    server: Rc<Server>,
    stream: tokio::net::TcpStream,
    conn_id: u64,
) -> Result<()> {
    use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

    let (reader, writer) = stream.into_split();
    let network = twoparty::VatNetwork::new(
        reader.compat(),
        writer.compat_write(),
        rpc_twoparty_capnp::Side::Server,
        Default::default(),
    );
    let store_impl = StoreImpl {
        server: server.clone(),
        conn_id,
    };
    let store_client: store::Client = capnp_rpc::new_client(store_impl);
    let rpc_system = RpcSystem::new(Box::new(network), Some(store_client.client));
    tracing::debug!(conn_id, "rpc session started");
    rpc_system.await?;
    tracing::debug!(conn_id, "rpc session ended");
    Ok(())
}

// ─── Server state ─────────────────────────────────────────────────────────────

struct WatcherEntry {
    watcher: head_watcher::Client,
    after_version: u64,
}

struct Server {
    /// jj Store wrapping the GitBackend — used for all object I/O.
    store: Arc<jj_lib::store::Store>,
    /// Path to `.jj/repo/op_store/` for operations and views.
    op_store_path: PathBuf,
    /// jj-lib op heads store — single authority for operation heads.
    op_heads_store: Arc<dyn jj_lib::op_heads_store::OpHeadsStore>,
    /// Path to `.jj/repo/tandem/` for tandem metadata sidecar (CAS/workspace map).
    tandem_dir: PathBuf,
    lock: Mutex<()>,
    watchers: Mutex<Vec<WatcherEntry>>,
}

/// Convert raw bytes to hex string (for filesystem paths)
fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Convert hex string to raw bytes
fn from_hex(hex: &str) -> Result<Vec<u8>> {
    if !hex.len().is_multiple_of(2) {
        bail!("odd-length hex string");
    }
    (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).map_err(|e| anyhow!("bad hex: {e}")))
        .collect()
}

impl Server {
    fn new(repo: PathBuf) -> Result<Self> {
        fs::create_dir_all(&repo)?;

        if !repo.join(".jj").exists() {
            Self::init_jj_git_repo(&repo)?;
        }

        let repo_dir = dunce::canonicalize(repo.join(".jj/repo"))
            .with_context(|| format!("cannot canonicalize .jj/repo at {}", repo.display()))?;
        let op_store_path = repo_dir.join("op_store");

        let settings = Self::user_settings()?;
        let factories = jj_lib::repo::StoreFactories::default();
        let loader = jj_lib::repo::RepoLoader::init_from_file_system(&settings, &repo_dir, &factories)
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

        Ok(Self {
            store: loader.store().clone(),
            op_store_path,
            op_heads_store: loader.op_heads_store().clone(),
            tandem_dir,
            lock: Mutex::new(()),
            watchers: Mutex::new(Vec::new()),
        })
    }

    fn user_settings() -> Result<jj_lib::settings::UserSettings> {
        let config = jj_lib::config::StackedConfig::with_defaults();
        jj_lib::settings::UserSettings::from_config(config).context("create jj settings")
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

    // ─── Object operations (through git backend) ─────────────────────

    fn get_object_sync(&self, kind: &str, id: &[u8]) -> Result<Vec<u8>> {
        let backend = self.store.backend();

        match kind {
            "file" => {
                let file_id = jj_lib::backend::FileId::new(id.to_vec());
                let mut reader = pollster::block_on(backend.read_file(&RepoPath::root(), &file_id))
                    .map_err(|e| anyhow!("read file {}: {e}", to_hex(id)))?;
                let mut buf = Vec::new();
                pollster::block_on(tokio::io::AsyncReadExt::read_to_end(&mut reader, &mut buf))
                    .map_err(|e| anyhow!("read file bytes: {e}"))?;
                Ok(buf)
            }
            "tree" => {
                let tree_id = TreeId::new(id.to_vec());
                let tree = pollster::block_on(backend.read_tree(&RepoPath::root(), &tree_id))
                    .map_err(|e| anyhow!("read tree {}: {e}", to_hex(id)))?;
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
                    .map_err(|e| anyhow!("read commit {}: {e}", to_hex(id)))?;
                let proto = jj_lib::simple_backend::commit_to_proto(&commit);
                Ok(proto.encode_to_vec())
            }
            "symlink" => {
                let symlink_id = jj_lib::backend::SymlinkId::new(id.to_vec());
                let target =
                    pollster::block_on(backend.read_symlink(&RepoPath::root(), &symlink_id))
                        .map_err(|e| anyhow!("read symlink {}: {e}", to_hex(id)))?;
                Ok(target.into_bytes())
            }
            "copy" => {
                bail!("copy objects not yet supported")
            }
            _ => bail!("unknown object kind: {kind}"),
        }
    }

    fn put_object_sync(&self, kind: &str, data: &[u8]) -> Result<(Vec<u8>, Vec<u8>)> {
        let backend = self.store.backend();

        match kind {
            "file" => {
                let mut cursor = Cursor::new(data.to_vec());
                let file_id =
                    pollster::block_on(backend.write_file(&RepoPath::root(), &mut cursor))
                        .map_err(|e| anyhow!("write file: {e}"))?;
                Ok((file_id.as_bytes().to_vec(), data.to_vec()))
            }
            "tree" => {
                let proto = jj_lib::protos::simple_store::Tree::decode(data)
                    .context("decode tree proto")?;
                let tree = proto_convert::tree_from_proto(proto);
                let tree_id = pollster::block_on(backend.write_tree(&RepoPath::root(), &tree))
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
                    pollster::block_on(backend.write_symlink(&RepoPath::root(), target))
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
        fs::read(&path).with_context(|| format!("operation not found: {hex}"))
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
        fs::read(&path).with_context(|| format!("view not found: {hex}"))
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

    fn get_heads_sync(&self) -> Result<HeadsState> {
        let _guard = self.lock.lock().map_err(|e| anyhow!("lock: {e}"))?;
        let metadata = self.read_heads_metadata()?;
        let heads = self.read_jj_op_heads()?;
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
            let current_heads = self.read_jj_op_heads()?;
            tracing::debug!(
                expected_version,
                actual_version = metadata.version,
                "update_op_heads version mismatch"
            );
            return Ok(UpdateResult {
                ok: false,
                heads: current_heads
                    .iter()
                    .map(|h| from_hex(h).unwrap_or_default())
                    .collect(),
                version: metadata.version,
                workspace_heads: metadata.workspace_heads,
            });
        }

        let mut old_op_ids: Vec<jj_lib::op_store::OperationId> =
            old_ids.into_iter().map(jj_lib::op_store::OperationId::new).collect();
        let new_op_id = jj_lib::op_store::OperationId::new(new_id.clone());
        old_op_ids.retain(|id| id != &new_op_id);
        pollster::block_on(self.op_heads_store.update_op_heads(&old_op_ids, &new_op_id))
            .map_err(|e| anyhow!("update op heads via jj-lib: {e}"))?;

        let next_heads = self.read_jj_op_heads()?;
        let new_hex = to_hex(&new_id);
        let next_workspace_heads =
            updated_workspace_heads(&metadata.workspace_heads, workspace_id.as_deref(), &new_hex);

        let next_metadata = HeadsMetadata {
            version: metadata.version + 1,
            workspace_heads: next_workspace_heads.clone(),
        };
        self.write_heads_metadata(&next_metadata)?;

        tracing::info!(
            previous_version = metadata.version,
            new_version = next_metadata.version,
            heads = next_heads.len(),
            workspace_heads = next_workspace_heads.len(),
            "updated heads state"
        );

        let heads_bytes: Vec<Vec<u8>> = next_heads
            .iter()
            .map(|h| from_hex(h).unwrap_or_default())
            .collect();

        self.notify_watchers(next_metadata.version, &heads_bytes);

        Ok(UpdateResult {
            ok: true,
            heads: heads_bytes,
            version: next_metadata.version,
            workspace_heads: next_workspace_heads,
        })
    }

    fn register_watcher(&self, watcher: head_watcher::Client, after_version: u64) {
        let mut watchers = self.watchers.lock().unwrap();
        watchers.push(WatcherEntry {
            watcher,
            after_version,
        });
        tracing::debug!(
            watchers = watchers.len(),
            after_version,
            "watcher registered"
        );
    }

    fn notify_watchers(&self, version: u64, heads: &[Vec<u8>]) {
        let mut watchers = self.watchers.lock().unwrap();
        tracing::trace!(
            watchers = watchers.len(),
            version,
            heads = heads.len(),
            "notifying watchers"
        );
        for entry in watchers.iter_mut() {
            if entry.after_version >= version {
                continue;
            }
            let watcher = entry.watcher.clone();
            let heads_clone: Vec<Vec<u8>> = heads.to_vec();
            entry.after_version = version;

            tokio::task::spawn_local(async move {
                let mut req = watcher.notify_request();
                {
                    let mut params = req.get();
                    params.set_version(version);
                    let mut heads_builder = params.init_heads(heads_clone.len() as u32);
                    for (i, head) in heads_clone.iter().enumerate() {
                        heads_builder.set(i as u32, head);
                    }
                }
                let _ = req.send().promise.await;
            });
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

// ─── Cap'n Proto Store implementation ─────────────────────────────────────────

struct StoreImpl {
    server: Rc<Server>,
    conn_id: u64,
}

fn capnp_err(e: anyhow::Error) -> capnp::Error {
    capnp::Error::failed(format!("{e:#}"))
}

fn object_kind_str(kind: crate::tandem_capnp::ObjectKind) -> &'static str {
    match kind {
        crate::tandem_capnp::ObjectKind::Commit => "commit",
        crate::tandem_capnp::ObjectKind::Tree => "tree",
        crate::tandem_capnp::ObjectKind::File => "file",
        crate::tandem_capnp::ObjectKind::Symlink => "symlink",
        crate::tandem_capnp::ObjectKind::Copy => "copy",
    }
}

impl store::Server for StoreImpl {
    fn get_repo_info(
        &mut self,
        _params: store::GetRepoInfoParams,
        mut results: store::GetRepoInfoResults,
    ) -> Promise<(), capnp::Error> {
        tracing::trace!(conn_id = self.conn_id, rpc = "getRepoInfo", "rpc request");
        let backend = self.server.store.backend();
        let mut info = results.get().init_info();
        info.set_protocol_major(0);
        info.set_protocol_minor(1);
        info.set_jj_version(env!("CARGO_PKG_VERSION"));
        info.set_backend_name("tandem");
        info.set_op_store_name("tandem_op_store");
        info.set_commit_id_length(backend.commit_id_length() as u16);
        info.set_change_id_length(backend.change_id_length() as u16);
        info.set_root_commit_id(backend.root_commit_id().as_bytes());
        info.set_root_change_id(backend.root_change_id().as_bytes());
        info.set_empty_tree_id(backend.empty_tree_id().as_bytes());
        info.set_root_operation_id(&[0u8; 64]);
        {
            let mut caps = info.init_capabilities(1);
            caps.set(0, crate::tandem_capnp::Capability::WatchHeads);
        }
        Promise::ok(())
    }

    fn get_object(
        &mut self,
        params: store::GetObjectParams,
        mut results: store::GetObjectResults,
    ) -> Promise<(), capnp::Error> {
        let reader = pry!(params.get());
        let kind = pry!(reader.get_kind());
        let id_bytes = pry!(reader.get_id());
        let kind_str = object_kind_str(kind);

        tracing::debug!(
            conn_id = self.conn_id,
            rpc = "getObject",
            kind = kind_str,
            object_id = %to_hex(id_bytes),
            "rpc request"
        );

        match self.server.get_object_sync(kind_str, id_bytes) {
            Ok(data) => {
                tracing::debug!(
                    conn_id = self.conn_id,
                    rpc = "getObject",
                    kind = kind_str,
                    object_id = %to_hex(id_bytes),
                    bytes = data.len(),
                    "rpc response"
                );
                results.get().set_data(&data);
                Promise::ok(())
            }
            Err(e) => {
                tracing::error!(
                    conn_id = self.conn_id,
                    rpc = "getObject",
                    kind = kind_str,
                    object_id = %to_hex(id_bytes),
                    error = %e,
                    "rpc error"
                );
                Promise::err(capnp_err(e))
            }
        }
    }

    fn put_object(
        &mut self,
        params: store::PutObjectParams,
        mut results: store::PutObjectResults,
    ) -> Promise<(), capnp::Error> {
        let reader = pry!(params.get());
        let kind = pry!(reader.get_kind());
        let data = pry!(reader.get_data()).to_vec();
        let kind_str = object_kind_str(kind);

        tracing::info!(
            conn_id = self.conn_id,
            rpc = "putObject",
            kind = kind_str,
            bytes = data.len(),
            "rpc request"
        );

        match self.server.put_object_sync(kind_str, &data) {
            Ok((id, normalized)) => {
                tracing::info!(
                    conn_id = self.conn_id,
                    rpc = "putObject",
                    kind = kind_str,
                    object_id = %to_hex(&id),
                    bytes = data.len(),
                    normalized_bytes = normalized.len(),
                    "rpc response"
                );
                let mut r = results.get();
                r.set_id(&id);
                r.set_normalized_data(&normalized);
                Promise::ok(())
            }
            Err(e) => {
                tracing::error!(
                    conn_id = self.conn_id,
                    rpc = "putObject",
                    kind = kind_str,
                    bytes = data.len(),
                    error = %e,
                    "rpc error"
                );
                Promise::err(capnp_err(e))
            }
        }
    }

    fn get_operation(
        &mut self,
        params: store::GetOperationParams,
        mut results: store::GetOperationResults,
    ) -> Promise<(), capnp::Error> {
        let reader = pry!(params.get());
        let id_bytes = pry!(reader.get_id());

        tracing::debug!(
            conn_id = self.conn_id,
            rpc = "getOperation",
            operation_id = %to_hex(id_bytes),
            "rpc request"
        );

        match self.server.get_operation_sync(id_bytes) {
            Ok(data) => {
                tracing::debug!(
                    conn_id = self.conn_id,
                    rpc = "getOperation",
                    operation_id = %to_hex(id_bytes),
                    bytes = data.len(),
                    "rpc response"
                );
                results.get().set_data(&data);
                Promise::ok(())
            }
            Err(e) => {
                tracing::error!(
                    conn_id = self.conn_id,
                    rpc = "getOperation",
                    operation_id = %to_hex(id_bytes),
                    error = %e,
                    "rpc error"
                );
                Promise::err(capnp_err(e))
            }
        }
    }

    fn put_operation(
        &mut self,
        params: store::PutOperationParams,
        mut results: store::PutOperationResults,
    ) -> Promise<(), capnp::Error> {
        let reader = pry!(params.get());
        let data = pry!(reader.get_data()).to_vec();

        tracing::info!(
            conn_id = self.conn_id,
            rpc = "putOperation",
            bytes = data.len(),
            "rpc request"
        );

        match self.server.put_operation_sync(&data) {
            Ok(id) => {
                tracing::info!(
                    conn_id = self.conn_id,
                    rpc = "putOperation",
                    operation_id = %to_hex(&id),
                    bytes = data.len(),
                    "rpc response"
                );
                results.get().set_id(&id);
                Promise::ok(())
            }
            Err(e) => {
                tracing::error!(
                    conn_id = self.conn_id,
                    rpc = "putOperation",
                    bytes = data.len(),
                    error = %e,
                    "rpc error"
                );
                Promise::err(capnp_err(e))
            }
        }
    }

    fn get_view(
        &mut self,
        params: store::GetViewParams,
        mut results: store::GetViewResults,
    ) -> Promise<(), capnp::Error> {
        let reader = pry!(params.get());
        let id_bytes = pry!(reader.get_id());

        tracing::debug!(
            conn_id = self.conn_id,
            rpc = "getView",
            view_id = %to_hex(id_bytes),
            "rpc request"
        );

        match self.server.get_view_sync(id_bytes) {
            Ok(data) => {
                tracing::debug!(
                    conn_id = self.conn_id,
                    rpc = "getView",
                    view_id = %to_hex(id_bytes),
                    bytes = data.len(),
                    "rpc response"
                );
                results.get().set_data(&data);
                Promise::ok(())
            }
            Err(e) => {
                tracing::error!(
                    conn_id = self.conn_id,
                    rpc = "getView",
                    view_id = %to_hex(id_bytes),
                    error = %e,
                    "rpc error"
                );
                Promise::err(capnp_err(e))
            }
        }
    }

    fn put_view(
        &mut self,
        params: store::PutViewParams,
        mut results: store::PutViewResults,
    ) -> Promise<(), capnp::Error> {
        let reader = pry!(params.get());
        let data = pry!(reader.get_data()).to_vec();

        tracing::info!(
            conn_id = self.conn_id,
            rpc = "putView",
            bytes = data.len(),
            "rpc request"
        );

        match self.server.put_view_sync(&data) {
            Ok(id) => {
                tracing::info!(
                    conn_id = self.conn_id,
                    rpc = "putView",
                    view_id = %to_hex(&id),
                    bytes = data.len(),
                    "rpc response"
                );
                results.get().set_id(&id);
                Promise::ok(())
            }
            Err(e) => {
                tracing::error!(
                    conn_id = self.conn_id,
                    rpc = "putView",
                    bytes = data.len(),
                    error = %e,
                    "rpc error"
                );
                Promise::err(capnp_err(e))
            }
        }
    }

    fn resolve_operation_id_prefix(
        &mut self,
        params: store::ResolveOperationIdPrefixParams,
        mut results: store::ResolveOperationIdPrefixResults,
    ) -> Promise<(), capnp::Error> {
        let reader = pry!(params.get());
        let prefix = pry!(reader.get_hex_prefix()).to_string().unwrap();
        tracing::debug!(
            conn_id = self.conn_id,
            rpc = "resolveOperationIdPrefix",
            prefix = %prefix,
            "rpc request"
        );

        match self.server.resolve_operation_id_prefix_sync(&prefix) {
            Ok((resolution, matched)) => {
                tracing::debug!(
                    conn_id = self.conn_id,
                    rpc = "resolveOperationIdPrefix",
                    prefix = %prefix,
                    resolution = %resolution,
                    "rpc response"
                );
                let mut r = results.get();
                match resolution.as_str() {
                    "noMatch" => r.set_resolution(crate::tandem_capnp::PrefixResolution::NoMatch),
                    "singleMatch" => {
                        r.set_resolution(crate::tandem_capnp::PrefixResolution::SingleMatch);
                        if let Some(m) = matched {
                            r.set_match(&m);
                        }
                    }
                    "ambiguous" => {
                        r.set_resolution(crate::tandem_capnp::PrefixResolution::Ambiguous)
                    }
                    _ => r.set_resolution(crate::tandem_capnp::PrefixResolution::NoMatch),
                }
                Promise::ok(())
            }
            Err(e) => {
                tracing::error!(
                    conn_id = self.conn_id,
                    rpc = "resolveOperationIdPrefix",
                    prefix = %prefix,
                    error = %e,
                    "rpc error"
                );
                Promise::err(capnp_err(e))
            }
        }
    }

    fn get_heads(
        &mut self,
        _params: store::GetHeadsParams,
        mut results: store::GetHeadsResults,
    ) -> Promise<(), capnp::Error> {
        tracing::debug!(conn_id = self.conn_id, rpc = "getHeads", "rpc request");
        match self.server.get_heads_sync() {
            Ok(state) => {
                tracing::debug!(
                    conn_id = self.conn_id,
                    rpc = "getHeads",
                    version = state.version,
                    heads = state.heads.len(),
                    workspace_heads = state.workspace_heads.len(),
                    "rpc response"
                );
                let mut r = results.get();
                // Convert hex heads to raw bytes
                let head_bytes: Vec<Vec<u8>> = state
                    .heads
                    .iter()
                    .filter_map(|h| from_hex(h).ok())
                    .collect();
                {
                    let mut heads = r.reborrow().init_heads(head_bytes.len() as u32);
                    for (i, head) in head_bytes.iter().enumerate() {
                        heads.set(i as u32, head);
                    }
                }
                r.set_version(state.version);
                {
                    let mut wh = r.init_workspace_heads(state.workspace_heads.len() as u32);
                    for (i, (ws_id, commit_hex)) in state.workspace_heads.iter().enumerate() {
                        let mut entry = wh.reborrow().get(i as u32);
                        entry.set_workspace_id(ws_id);
                        if let Ok(commit_bytes) = from_hex(commit_hex) {
                            entry.set_commit_id(&commit_bytes);
                        }
                    }
                }
                Promise::ok(())
            }
            Err(e) => {
                tracing::error!(
                    conn_id = self.conn_id,
                    rpc = "getHeads",
                    error = %e,
                    "rpc error"
                );
                Promise::err(capnp_err(e))
            }
        }
    }

    fn update_op_heads(
        &mut self,
        params: store::UpdateOpHeadsParams,
        mut results: store::UpdateOpHeadsResults,
    ) -> Promise<(), capnp::Error> {
        let reader = pry!(params.get());

        let old_ids_reader = pry!(reader.get_old_ids());
        let mut old_ids = Vec::new();
        for i in 0..old_ids_reader.len() {
            old_ids.push(pry!(old_ids_reader.get(i)).to_vec());
        }

        let new_id = pry!(reader.get_new_id()).to_vec();
        let expected_version = reader.get_expected_version();
        let workspace_id_text = pry!(reader.get_workspace_id());
        let workspace_id_str = workspace_id_text.to_str().unwrap_or("");
        let workspace_id = if workspace_id_str.is_empty() {
            None
        } else {
            Some(workspace_id_str.to_string())
        };

        tracing::info!(
            conn_id = self.conn_id,
            rpc = "updateOpHeads",
            expected_version,
            old_ids = old_ids.len(),
            new_id = %to_hex(&new_id),
            workspace_id = workspace_id.as_deref().unwrap_or(""),
            "rpc request"
        );

        match self
            .server
            .update_op_heads_sync(old_ids, new_id, expected_version, workspace_id)
        {
            Ok(result) => {
                tracing::info!(
                    conn_id = self.conn_id,
                    rpc = "updateOpHeads",
                    ok = result.ok,
                    version = result.version,
                    heads = result.heads.len(),
                    workspace_heads = result.workspace_heads.len(),
                    "rpc response"
                );
                let mut r = results.get();
                r.set_ok(result.ok);
                {
                    let mut heads = r.reborrow().init_heads(result.heads.len() as u32);
                    for (i, head) in result.heads.iter().enumerate() {
                        heads.set(i as u32, head);
                    }
                }
                r.set_version(result.version);
                {
                    let mut wh = r.init_workspace_heads(result.workspace_heads.len() as u32);
                    for (i, (ws_id, commit_hex)) in result.workspace_heads.iter().enumerate() {
                        let mut entry = wh.reborrow().get(i as u32);
                        entry.set_workspace_id(ws_id);
                        if let Ok(commit_bytes) = from_hex(commit_hex) {
                            entry.set_commit_id(&commit_bytes);
                        }
                    }
                }
                Promise::ok(())
            }
            Err(e) => {
                tracing::error!(
                    conn_id = self.conn_id,
                    rpc = "updateOpHeads",
                    expected_version,
                    error = %e,
                    "rpc error"
                );
                Promise::err(capnp_err(e))
            }
        }
    }

    fn watch_heads(
        &mut self,
        params: store::WatchHeadsParams,
        mut results: store::WatchHeadsResults,
    ) -> Promise<(), capnp::Error> {
        let reader = pry!(params.get());
        let watcher = pry!(reader.get_watcher());
        let after_version = reader.get_after_version();

        tracing::info!(
            conn_id = self.conn_id,
            rpc = "watchHeads",
            after_version,
            "rpc request"
        );

        let current_state = match self.server.get_heads_sync() {
            Ok(s) => s,
            Err(e) => {
                tracing::error!(
                    conn_id = self.conn_id,
                    rpc = "watchHeads",
                    error = %e,
                    "rpc error"
                );
                return Promise::err(capnp_err(e));
            }
        };

        if after_version < current_state.version {
            tracing::debug!(
                conn_id = self.conn_id,
                rpc = "watchHeads",
                after_version,
                current_version = current_state.version,
                "sending catch-up notification"
            );
            let catch_up_watcher = watcher.clone();
            let heads: Vec<Vec<u8>> = current_state
                .heads
                .iter()
                .filter_map(|h| from_hex(h).ok())
                .collect();
            let version = current_state.version;
            tokio::task::spawn_local(async move {
                let mut req = catch_up_watcher.notify_request();
                {
                    let mut p = req.get();
                    p.set_version(version);
                    let mut h = p.init_heads(heads.len() as u32);
                    for (i, head) in heads.iter().enumerate() {
                        h.set(i as u32, head);
                    }
                }
                let _ = req.send().promise.await;
            });
        }

        self.server.register_watcher(watcher, current_state.version);
        tracing::info!(
            conn_id = self.conn_id,
            rpc = "watchHeads",
            version = current_state.version,
            "watcher registered"
        );

        let cancel_impl = CancelImpl {
            server: self.server.clone(),
        };
        let cancel_client: cancel::Client = capnp_rpc::new_client(cancel_impl);
        results.get().set_cancel(cancel_client);

        Promise::ok(())
    }

    fn get_heads_snapshot(
        &mut self,
        _params: store::GetHeadsSnapshotParams,
        _results: store::GetHeadsSnapshotResults,
    ) -> Promise<(), capnp::Error> {
        Promise::err(capnp::Error::unimplemented(
            "getHeadsSnapshot not yet implemented".to_string(),
        ))
    }

    fn get_related_copies(
        &mut self,
        _params: store::GetRelatedCopiesParams,
        _results: store::GetRelatedCopiesResults,
    ) -> Promise<(), capnp::Error> {
        Promise::err(capnp::Error::unimplemented(
            "getRelatedCopies not yet implemented".to_string(),
        ))
    }
}

// ─── Cancel implementation ────────────────────────────────────────────────────

struct CancelImpl {
    server: Rc<Server>,
}

impl cancel::Server for CancelImpl {
    fn cancel(
        &mut self,
        _params: cancel::CancelParams,
        _results: cancel::CancelResults,
    ) -> Promise<(), capnp::Error> {
        let mut watchers = self.server.watchers.lock().unwrap();
        let count = watchers.len();
        watchers.clear();
        tracing::info!(cleared_watchers = count, "watchers cancelled");
        Promise::ok(())
    }
}

// ─── Helpers ──────────────────────────────────────────────────────────────────

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

fn write_bytes_if_missing(path: &Path, bytes: &[u8]) -> Result<()> {
    if path.exists() {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, bytes)?;
    Ok(())
}
