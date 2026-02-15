//! tandem serve — Cap'n Proto RPC server hosting a jj+git backend.
//!
//! The server stores objects through jj's Git backend so that `jj git push`
//! on the server repo just works. Operations and views are stored in the
//! standard jj op_store directory. Op heads are managed via CAS in
//! `.tandem/heads.json` and synced to jj's op_heads directory.

use anyhow::{anyhow, bail, Context, Result};
// blake2 is available if needed for raw hashing, but we use jj_lib::content_hash
use capnp::capability::Promise;
use capnp_rpc::pry;
use capnp_rpc::{rpc_twoparty_capnp, twoparty, RpcSystem};
use jj_lib::backend::{CommitId, TreeId};
use jj_lib::object_id::ObjectId as _;
use jj_lib::repo::Repo as _;
use jj_lib::repo_path::RepoPath;
use prost::Message as _;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use crate::proto_convert;
use crate::tandem_capnp::{cancel, head_watcher, store};

// ─── Public entry point ───────────────────────────────────────────────────────

pub async fn run_serve(listen_addr: &str, repo_path: &str) -> Result<()> {
    let repo = PathBuf::from(repo_path);
    let server = Rc::new(Server::new(repo)?);
    let listener = tokio::net::TcpListener::bind(listen_addr)
        .await
        .with_context(|| format!("failed to bind {listen_addr}"))?;
    eprintln!("tandem server listening on {}", listener.local_addr()?);

    loop {
        let (stream, _) = listener.accept().await?;
        let server = Rc::clone(&server);
        tokio::task::spawn_local(async move {
            if let Err(err) = handle_capnp_connection(server, stream).await {
                eprintln!("rpc connection error: {err:#}");
            }
        });
    }
}

// ─── Connection handler ───────────────────────────────────────────────────────

async fn handle_capnp_connection(
    server: Rc<Server>,
    stream: tokio::net::TcpStream,
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
    };
    let store_client: store::Client = capnp_rpc::new_client(store_impl);
    let rpc_system = RpcSystem::new(Box::new(network), Some(store_client.client));
    rpc_system.await?;
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
    /// Path to `.jj/repo/op_heads/heads/` for syncing op heads.
    op_heads_dir: PathBuf,
    /// Path to `.tandem/` for CAS heads management.
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

        let jj_dir = repo.join(".jj");
        let store = if jj_dir.exists() {
            // Load existing jj+git repo
            Self::load_store(&repo)?
        } else {
            // Initialize a new jj+git colocated repo
            Self::init_jj_git_repo(&repo)?
        };

        let repo_dir = dunce::canonicalize(repo.join(".jj/repo"))
            .with_context(|| format!("cannot canonicalize .jj/repo at {}", repo.display()))?;
        let op_store_path = repo_dir.join("op_store");
        let op_heads_dir = repo_dir.join("op_heads").join("heads");

        // Create tandem-specific directory for CAS heads management
        let tandem_dir = repo.join(".tandem");
        fs::create_dir_all(&tandem_dir)?;

        let heads_path = tandem_dir.join("heads.json");
        if !heads_path.exists() {
            // Initialize heads from jj's op_heads directory
            let initial_heads = Self::read_jj_op_heads(&op_heads_dir)?;
            let initial = HeadsState {
                version: 0,
                heads: initial_heads,
                workspace_heads: BTreeMap::new(),
            };
            fs::write(&heads_path, serde_json::to_vec_pretty(&initial)?)?;
        }

        Ok(Self {
            store,
            op_store_path,
            op_heads_dir,
            tandem_dir,
            lock: Mutex::new(()),
            watchers: Mutex::new(Vec::new()),
        })
    }

    /// Initialize a new jj+git colocated repo and return its Store.
    fn init_jj_git_repo(repo_path: &Path) -> Result<Arc<jj_lib::store::Store>> {
        let config = jj_lib::config::StackedConfig::with_defaults();
        let settings = jj_lib::settings::UserSettings::from_config(config)
            .context("create jj settings")?;

        let (_workspace, jj_repo) =
            jj_lib::workspace::Workspace::init_colocated_git(&settings, repo_path)
                .context("init colocated git repo")?;

        Ok(jj_repo.store().clone())
    }

    /// Load an existing jj+git repo's Store.
    fn load_store(repo_path: &Path) -> Result<Arc<jj_lib::store::Store>> {
        let config = jj_lib::config::StackedConfig::with_defaults();
        let settings = jj_lib::settings::UserSettings::from_config(config)
            .context("create jj settings")?;

        let store_path = dunce::canonicalize(repo_path.join(".jj/repo/store"))
            .context("canonicalize store path")?;

        let git_backend = jj_lib::git_backend::GitBackend::load(&settings, &store_path)
            .map_err(|e| anyhow!("load git backend: {e}"))?;

        let signer = jj_lib::signing::Signer::from_settings(&settings)
            .context("create signer")?;
        let merge_options = jj_lib::tree_merge::MergeOptions::from_settings(&settings)
            .map_err(|e| anyhow!("merge options: {e}"))?;

        Ok(jj_lib::store::Store::new(
            Box::new(git_backend),
            signer,
            merge_options,
        ))
    }

    /// Read op heads from jj's op_heads/heads/ directory.
    fn read_jj_op_heads(op_heads_dir: &Path) -> Result<Vec<String>> {
        let mut heads = Vec::new();
        if let Ok(entries) = fs::read_dir(op_heads_dir) {
            for entry in entries {
                let entry = entry?;
                let name = entry.file_name();
                let name = name.to_string_lossy();
                // Skip non-hex filenames
                if name.chars().all(|c| c.is_ascii_hexdigit()) {
                    heads.push(name.to_string());
                }
            }
        }
        Ok(heads)
    }

    /// Sync the tandem heads to jj's op_heads/heads/ directory.
    fn sync_op_heads_to_jj(&self, heads: &[String]) -> Result<()> {
        // Clear existing head files
        if let Ok(entries) = fs::read_dir(&self.op_heads_dir) {
            for entry in entries {
                if let Ok(entry) = entry {
                    let _ = fs::remove_file(entry.path());
                }
            }
        }
        // Write new head files (empty files named by hex ID)
        for head_hex in heads {
            fs::write(self.op_heads_dir.join(head_hex), "")?;
        }
        Ok(())
    }

    // ─── Object operations (through git backend) ─────────────────────

    fn get_object_sync(&self, kind: &str, id: &[u8]) -> Result<Vec<u8>> {
        let backend = self.store.backend();

        match kind {
            "file" => {
                let file_id = jj_lib::backend::FileId::new(id.to_vec());
                let mut reader = pollster::block_on(
                    backend.read_file(&RepoPath::root(), &file_id),
                )
                .map_err(|e| anyhow!("read file {}: {e}", to_hex(id)))?;
                let mut buf = Vec::new();
                pollster::block_on(
                    tokio::io::AsyncReadExt::read_to_end(&mut reader, &mut buf),
                )
                .map_err(|e| anyhow!("read file bytes: {e}"))?;
                Ok(buf)
            }
            "tree" => {
                let tree_id = TreeId::new(id.to_vec());
                let tree = pollster::block_on(
                    backend.read_tree(&RepoPath::root(), &tree_id),
                )
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
                let target = pollster::block_on(
                    backend.read_symlink(&RepoPath::root(), &symlink_id),
                )
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
                let file_id = pollster::block_on(
                    backend.write_file(&RepoPath::root(), &mut cursor),
                )
                .map_err(|e| anyhow!("write file: {e}"))?;
                Ok((file_id.as_bytes().to_vec(), data.to_vec()))
            }
            "tree" => {
                let proto = jj_lib::protos::simple_store::Tree::decode(data)
                    .context("decode tree proto")?;
                let tree = proto_convert::tree_from_proto(proto);
                let tree_id = pollster::block_on(
                    backend.write_tree(&RepoPath::root(), &tree),
                )
                .map_err(|e| anyhow!("write tree: {e}"))?;
                // Return the original proto data as normalized (the tree is the same)
                Ok((tree_id.as_bytes().to_vec(), data.to_vec()))
            }
            "commit" => {
                let proto = jj_lib::protos::simple_store::Commit::decode(data)
                    .context("decode commit proto")?;
                let commit = proto_convert::commit_from_proto(proto);
                let (commit_id, stored_commit) = pollster::block_on(
                    backend.write_commit(commit, None),
                )
                .map_err(|e| anyhow!("write commit: {e}"))?;
                // Re-encode the stored commit (may have normalized fields)
                let stored_proto = jj_lib::simple_backend::commit_to_proto(&stored_commit);
                let normalized_data = stored_proto.encode_to_vec();
                Ok((commit_id.as_bytes().to_vec(), normalized_data))
            }
            "symlink" => {
                let target = std::str::from_utf8(data)
                    .context("symlink target is not valid UTF-8")?;
                let symlink_id = pollster::block_on(
                    backend.write_symlink(&RepoPath::root(), target),
                )
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
        let operation = proto_convert::operation_from_proto(proto)
            .context("convert operation from proto")?;

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
        let proto = jj_lib::protos::simple_op_store::View::decode(data)
            .context("decode view proto")?;
        let view = proto_convert::view_from_proto(proto)
            .context("convert view from proto")?;

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
        self.read_heads_state()
    }

    fn update_op_heads_sync(
        &self,
        old_ids: Vec<Vec<u8>>,
        new_id: Vec<u8>,
        expected_version: u64,
        workspace_id: Option<String>,
    ) -> Result<UpdateResult> {
        let _guard = self.lock.lock().map_err(|e| anyhow!("lock: {e}"))?;
        let state = self.read_heads_state()?;

        if state.version != expected_version {
            return Ok(UpdateResult {
                ok: false,
                heads: state.heads.iter().map(|h| from_hex(h).unwrap_or_default()).collect(),
                version: state.version,
                workspace_heads: state.workspace_heads,
            });
        }

        // Convert raw bytes to hex for storage
        let old_hex: Vec<String> = old_ids.iter().map(|id| to_hex(id)).collect();
        let new_hex = to_hex(&new_id);

        let next_heads = updated_heads(&state.heads, &old_hex, &new_hex);
        let next_workspace_heads = updated_workspace_heads(
            &state.workspace_heads,
            workspace_id.as_deref(),
            &new_hex,
        );

        let next_state = HeadsState {
            version: state.version + 1,
            heads: next_heads.clone(),
            workspace_heads: next_workspace_heads.clone(),
        };
        self.write_heads_state(&next_state)?;

        // Sync to jj's op_heads directory so `jj` commands work on the server
        if let Err(e) = self.sync_op_heads_to_jj(&next_heads) {
            eprintln!("warning: failed to sync op heads to jj: {e:#}");
        }

        // Convert hex back to raw bytes for the response
        let heads_bytes: Vec<Vec<u8>> = next_heads
            .iter()
            .map(|h| from_hex(h).unwrap_or_default())
            .collect();

        self.notify_watchers(next_state.version, &heads_bytes);

        Ok(UpdateResult {
            ok: true,
            heads: heads_bytes,
            version: next_state.version,
            workspace_heads: next_workspace_heads,
        })
    }

    fn register_watcher(&self, watcher: head_watcher::Client, after_version: u64) {
        let mut watchers = self.watchers.lock().unwrap();
        watchers.push(WatcherEntry {
            watcher,
            after_version,
        });
    }

    fn notify_watchers(&self, version: u64, heads: &[Vec<u8>]) {
        let mut watchers = self.watchers.lock().unwrap();
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

    fn read_heads_state(&self) -> Result<HeadsState> {
        let bytes = fs::read(self.tandem_dir.join("heads.json"))?;
        let state = serde_json::from_slice(&bytes)?;
        Ok(state)
    }

    fn write_heads_state(&self, state: &HeadsState) -> Result<()> {
        fs::write(
            self.tandem_dir.join("heads.json"),
            serde_json::to_vec_pretty(state)?,
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
struct HeadsState {
    version: u64,
    heads: Vec<String>, // hex-encoded IDs
    #[serde(default)]
    workspace_heads: BTreeMap<String, String>, // hex-encoded
}

// ─── Cap'n Proto Store implementation ─────────────────────────────────────────

struct StoreImpl {
    server: Rc<Server>,
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

        match self.server.get_object_sync(kind_str, id_bytes) {
            Ok(data) => {
                results.get().set_data(&data);
                Promise::ok(())
            }
            Err(e) => Promise::err(capnp_err(e)),
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

        match self.server.put_object_sync(kind_str, &data) {
            Ok((id, normalized)) => {
                let mut r = results.get();
                r.set_id(&id);
                r.set_normalized_data(&normalized);
                Promise::ok(())
            }
            Err(e) => Promise::err(capnp_err(e)),
        }
    }

    fn get_operation(
        &mut self,
        params: store::GetOperationParams,
        mut results: store::GetOperationResults,
    ) -> Promise<(), capnp::Error> {
        let reader = pry!(params.get());
        let id_bytes = pry!(reader.get_id());

        match self.server.get_operation_sync(id_bytes) {
            Ok(data) => {
                results.get().set_data(&data);
                Promise::ok(())
            }
            Err(e) => Promise::err(capnp_err(e)),
        }
    }

    fn put_operation(
        &mut self,
        params: store::PutOperationParams,
        mut results: store::PutOperationResults,
    ) -> Promise<(), capnp::Error> {
        let reader = pry!(params.get());
        let data = pry!(reader.get_data()).to_vec();

        match self.server.put_operation_sync(&data) {
            Ok(id) => {
                results.get().set_id(&id);
                Promise::ok(())
            }
            Err(e) => Promise::err(capnp_err(e)),
        }
    }

    fn get_view(
        &mut self,
        params: store::GetViewParams,
        mut results: store::GetViewResults,
    ) -> Promise<(), capnp::Error> {
        let reader = pry!(params.get());
        let id_bytes = pry!(reader.get_id());

        match self.server.get_view_sync(id_bytes) {
            Ok(data) => {
                results.get().set_data(&data);
                Promise::ok(())
            }
            Err(e) => Promise::err(capnp_err(e)),
        }
    }

    fn put_view(
        &mut self,
        params: store::PutViewParams,
        mut results: store::PutViewResults,
    ) -> Promise<(), capnp::Error> {
        let reader = pry!(params.get());
        let data = pry!(reader.get_data()).to_vec();

        match self.server.put_view_sync(&data) {
            Ok(id) => {
                results.get().set_id(&id);
                Promise::ok(())
            }
            Err(e) => Promise::err(capnp_err(e)),
        }
    }

    fn resolve_operation_id_prefix(
        &mut self,
        params: store::ResolveOperationIdPrefixParams,
        mut results: store::ResolveOperationIdPrefixResults,
    ) -> Promise<(), capnp::Error> {
        let reader = pry!(params.get());
        let prefix = pry!(reader.get_hex_prefix()).to_string().unwrap();

        match self.server.resolve_operation_id_prefix_sync(&prefix) {
            Ok((resolution, matched)) => {
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
            Err(e) => Promise::err(capnp_err(e)),
        }
    }

    fn get_heads(
        &mut self,
        _params: store::GetHeadsParams,
        mut results: store::GetHeadsResults,
    ) -> Promise<(), capnp::Error> {
        match self.server.get_heads_sync() {
            Ok(state) => {
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
            Err(e) => Promise::err(capnp_err(e)),
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

        match self
            .server
            .update_op_heads_sync(old_ids, new_id, expected_version, workspace_id)
        {
            Ok(result) => {
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
                    let mut wh =
                        r.init_workspace_heads(result.workspace_heads.len() as u32);
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
            Err(e) => Promise::err(capnp_err(e)),
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

        let current_state = match self.server.get_heads_sync() {
            Ok(s) => s,
            Err(e) => return Promise::err(capnp_err(e)),
        };

        if after_version < current_state.version {
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

        self.server
            .register_watcher(watcher, current_state.version);

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
        watchers.clear();
        Promise::ok(())
    }
}

// ─── Helpers ──────────────────────────────────────────────────────────────────

use std::collections::HashSet;

fn updated_heads(current_heads: &[String], old_ids: &[String], new_id: &str) -> Vec<String> {
    let removed_heads: HashSet<&str> = old_ids.iter().map(String::as_str).collect();
    let mut next_heads = current_heads
        .iter()
        .filter(|head| !removed_heads.contains(head.as_str()))
        .cloned()
        .collect::<Vec<_>>();

    if !next_heads.iter().any(|head| head == new_id) {
        next_heads.push(new_id.to_string());
    }

    next_heads
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
