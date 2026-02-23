//! Shared RPC client wrapper for connecting to a tandem server.
//!
//! Manages a Cap'n Proto connection on a dedicated thread (because capnp-rpc
//! types are !Send). Communication from Backend/OpStore/OpHeadsStore happens
//! through std::sync::mpsc channels.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use anyhow::{anyhow, bail, Context, Result};
use capnp_rpc::{rpc_twoparty_capnp, twoparty, RpcSystem};
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

use crate::tandem_capnp::store;

// ─── Public types ─────────────────────────────────────────────────────────────

const PROTOCOL_MAJOR: u16 = 0;
const PROTOCOL_MINOR: u16 = 1;
const EXPECTED_BACKEND_NAME: &str = "tandem";
const EXPECTED_OP_STORE_NAME: &str = "tandem_op_store";
const ROOT_OPERATION_ID_LENGTH: usize = 64;
const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
const BENCH_INJECT_RTT_MS_ENV: &str = "TANDEM_BENCH_INJECT_RTT_MS";
const BENCH_DISABLE_RPC_INFLIGHT_ENV: &str = "TANDEM_BENCH_DISABLE_RPC_INFLIGHT";
const RPC_MAX_INFLIGHT_ENV: &str = "TANDEM_RPC_MAX_INFLIGHT";
const DEFAULT_RPC_MAX_INFLIGHT: usize = 32;

#[derive(Debug, Clone)]
enum ConnectorTarget {
    Tcp { addr: String },
}

impl ConnectorTarget {
    fn parse(endpoint: &str) -> Result<Self> {
        if let Some((scheme, rest)) = endpoint.split_once("://") {
            if scheme.eq_ignore_ascii_case("tcp") {
                if rest.is_empty() {
                    bail!("invalid tcp endpoint: missing host:port in {endpoint:?}");
                }
                return Ok(Self::Tcp {
                    addr: rest.to_string(),
                });
            }

            bail!(
                "unsupported tandem transport scheme {scheme:?}; only raw TCP host:port endpoints are currently supported"
            );
        }

        Ok(Self::Tcp {
            addr: endpoint.to_string(),
        })
    }

    fn display_addr(&self) -> &str {
        match self {
            Self::Tcp { addr } => addr,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum RepoCapability {
    WatchHeads,
    HeadsSnapshot,
    CopyTracking,
}

impl RepoCapability {
    fn as_str(self) -> &'static str {
        match self {
            RepoCapability::WatchHeads => "watchHeads",
            RepoCapability::HeadsSnapshot => "headsSnapshot",
            RepoCapability::CopyTracking => "copyTracking",
        }
    }

    fn from_capnp(cap: crate::tandem_capnp::Capability) -> Self {
        match cap {
            crate::tandem_capnp::Capability::WatchHeads => RepoCapability::WatchHeads,
            crate::tandem_capnp::Capability::HeadsSnapshot => RepoCapability::HeadsSnapshot,
            crate::tandem_capnp::Capability::CopyTracking => RepoCapability::CopyTracking,
        }
    }
}

#[derive(Debug, Clone)]
pub struct RepoInfoResponse {
    pub protocol_major: u16,
    pub protocol_minor: u16,
    pub backend_name: String,
    pub op_store_name: String,
    pub commit_id_length: usize,
    pub change_id_length: usize,
    pub root_commit_id: Vec<u8>,
    pub root_change_id: Vec<u8>,
    pub empty_tree_id: Vec<u8>,
    pub root_operation_id: Vec<u8>,
    pub capabilities: BTreeSet<RepoCapability>,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct UpdateHeadsResult {
    pub ok: bool,
    pub heads: Vec<Vec<u8>>,
    pub version: u64,
}

#[derive(Debug, Clone)]
pub struct HeadsState {
    pub heads: Vec<Vec<u8>>,
    pub version: u64,
    pub workspace_heads: std::collections::BTreeMap<String, Vec<u8>>,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct HeadsSnapshot {
    pub heads: Vec<Vec<u8>>,
    pub version: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub enum PrefixResult {
    NoMatch,
    SingleMatch,
    Ambiguous,
}

// ─── RPC message types ────────────────────────────────────────────────────────

type Reply<T> = std::sync::mpsc::Sender<Result<T>>;

#[allow(dead_code)]
enum RpcMsg {
    GetObject {
        kind: u16,
        id: Vec<u8>,
        reply: Reply<Vec<u8>>,
    },
    PutObject {
        kind: u16,
        data: Vec<u8>,
        reply: Reply<(Vec<u8>, Vec<u8>)>,
    },
    GetOperation {
        id: Vec<u8>,
        reply: Reply<Vec<u8>>,
    },
    PutOperation {
        data: Vec<u8>,
        reply: Reply<Vec<u8>>,
    },
    GetView {
        id: Vec<u8>,
        reply: Reply<Vec<u8>>,
    },
    PutView {
        data: Vec<u8>,
        reply: Reply<Vec<u8>>,
    },
    GetHeads {
        reply: Reply<HeadsState>,
    },
    UpdateOpHeads {
        old_ids: Vec<Vec<u8>>,
        new_id: Vec<u8>,
        expected_version: u64,
        workspace_id: String,
        reply: Reply<UpdateHeadsResult>,
    },
    GetHeadsSnapshot {
        reply: Reply<Option<HeadsSnapshot>>,
    },
    GetRelatedCopies {
        copy_id: Vec<u8>,
        reply: Reply<Option<Vec<Vec<u8>>>>,
    },
    ResolveOpPrefix {
        hex_prefix: String,
        reply: Reply<(PrefixResult, Option<Vec<u8>>)>,
    },
}

// ─── TandemClient ─────────────────────────────────────────────────────────────

/// Cap'n Proto RPC client to a tandem server.
///
/// All three trait implementations (TandemBackend, TandemOpStore,
/// TandemOpHeadsStore) share a connection through this client via Arc.
pub struct TandemClient {
    tx: tokio::sync::mpsc::UnboundedSender<RpcMsg>,
    _thread: std::thread::JoinHandle<()>,
    server_addr: String,
    repo_info: RepoInfoResponse,
}

impl std::fmt::Debug for TandemClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TandemClient")
            .field("server_addr", &self.server_addr)
            .finish()
    }
}

impl TandemClient {
    /// Connect to a tandem server at the given address.
    /// Starts a background thread for the Cap'n Proto RPC event loop.
    pub fn connect(addr: &str) -> Result<Arc<Self>> {
        Self::connect_with_requirements(addr, &[])
    }

    pub fn connect_with_requirements(
        addr: &str,
        required_capabilities: &[RepoCapability],
    ) -> Result<Arc<Self>> {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<RpcMsg>();
        let addr_owned = addr.to_string();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<RepoInfoResponse>>();

        let addr_for_thread = addr_owned.clone();
        let required_caps = required_capabilities.to_vec();
        let thread = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("build tokio runtime for RPC thread");
            let local = tokio::task::LocalSet::new();
            local.block_on(&rt, rpc_loop(addr_for_thread, required_caps, rx, ready_tx));
        });

        // Wait for connection to be established + compatibility validated.
        let repo_info = ready_rx
            .recv()
            .map_err(|_| anyhow!("RPC thread died before signaling readiness"))??;

        Ok(Arc::new(TandemClient {
            tx,
            _thread: thread,
            server_addr: addr_owned,
            repo_info,
        }))
    }

    /// Get the server address this client is connected to.
    pub fn server_addr(&self) -> &str {
        &self.server_addr
    }

    pub fn repo_info(&self) -> &RepoInfoResponse {
        &self.repo_info
    }

    pub fn supports_capability(&self, capability: RepoCapability) -> bool {
        self.repo_info.capabilities.contains(&capability)
    }

    // ─── Blocking RPC methods ─────────────────────────────────────────

    #[allow(dead_code)]
    pub fn get_repo_info(&self) -> Result<RepoInfoResponse> {
        Ok(self.repo_info.clone())
    }

    pub fn get_object(&self, kind: u16, id: &[u8]) -> Result<Vec<u8>> {
        let (reply_tx, reply_rx) = std::sync::mpsc::channel();
        self.tx
            .send(RpcMsg::GetObject {
                kind,
                id: id.to_vec(),
                reply: reply_tx,
            })
            .map_err(|_| anyhow!("RPC channel closed"))?;
        reply_rx.recv().map_err(|_| anyhow!("RPC reply dropped"))?
    }

    pub fn put_object(&self, kind: u16, data: &[u8]) -> Result<(Vec<u8>, Vec<u8>)> {
        let (reply_tx, reply_rx) = std::sync::mpsc::channel();
        self.tx
            .send(RpcMsg::PutObject {
                kind,
                data: data.to_vec(),
                reply: reply_tx,
            })
            .map_err(|_| anyhow!("RPC channel closed"))?;
        reply_rx.recv().map_err(|_| anyhow!("RPC reply dropped"))?
    }

    pub fn get_operation(&self, id: &[u8]) -> Result<Vec<u8>> {
        let (reply_tx, reply_rx) = std::sync::mpsc::channel();
        self.tx
            .send(RpcMsg::GetOperation {
                id: id.to_vec(),
                reply: reply_tx,
            })
            .map_err(|_| anyhow!("RPC channel closed"))?;
        reply_rx.recv().map_err(|_| anyhow!("RPC reply dropped"))?
    }

    pub fn put_operation(&self, data: &[u8]) -> Result<Vec<u8>> {
        let (reply_tx, reply_rx) = std::sync::mpsc::channel();
        self.tx
            .send(RpcMsg::PutOperation {
                data: data.to_vec(),
                reply: reply_tx,
            })
            .map_err(|_| anyhow!("RPC channel closed"))?;
        reply_rx.recv().map_err(|_| anyhow!("RPC reply dropped"))?
    }

    pub fn get_view(&self, id: &[u8]) -> Result<Vec<u8>> {
        let (reply_tx, reply_rx) = std::sync::mpsc::channel();
        self.tx
            .send(RpcMsg::GetView {
                id: id.to_vec(),
                reply: reply_tx,
            })
            .map_err(|_| anyhow!("RPC channel closed"))?;
        reply_rx.recv().map_err(|_| anyhow!("RPC reply dropped"))?
    }

    pub fn put_view(&self, data: &[u8]) -> Result<Vec<u8>> {
        let (reply_tx, reply_rx) = std::sync::mpsc::channel();
        self.tx
            .send(RpcMsg::PutView {
                data: data.to_vec(),
                reply: reply_tx,
            })
            .map_err(|_| anyhow!("RPC channel closed"))?;
        reply_rx.recv().map_err(|_| anyhow!("RPC reply dropped"))?
    }

    pub fn get_heads_state(&self) -> Result<HeadsState> {
        let (reply_tx, reply_rx) = std::sync::mpsc::channel();
        self.tx
            .send(RpcMsg::GetHeads { reply: reply_tx })
            .map_err(|_| anyhow!("RPC channel closed"))?;
        reply_rx.recv().map_err(|_| anyhow!("RPC reply dropped"))?
    }

    pub fn update_op_heads(
        &self,
        old_ids: &[Vec<u8>],
        new_id: &[u8],
        expected_version: u64,
        workspace_id: &str,
    ) -> Result<UpdateHeadsResult> {
        let (reply_tx, reply_rx) = std::sync::mpsc::channel();
        self.tx
            .send(RpcMsg::UpdateOpHeads {
                old_ids: old_ids.to_vec(),
                new_id: new_id.to_vec(),
                expected_version,
                workspace_id: workspace_id.to_string(),
                reply: reply_tx,
            })
            .map_err(|_| anyhow!("RPC channel closed"))?;
        reply_rx.recv().map_err(|_| anyhow!("RPC reply dropped"))?
    }

    #[allow(dead_code)]
    pub fn get_heads_snapshot(&self) -> Result<Option<HeadsSnapshot>> {
        if !self.supports_capability(RepoCapability::HeadsSnapshot) {
            return Ok(None);
        }

        let (reply_tx, reply_rx) = std::sync::mpsc::channel();
        self.tx
            .send(RpcMsg::GetHeadsSnapshot { reply: reply_tx })
            .map_err(|_| anyhow!("RPC channel closed"))?;
        reply_rx.recv().map_err(|_| anyhow!("RPC reply dropped"))?
    }

    pub fn get_related_copies(&self, copy_id: &[u8]) -> Result<Option<Vec<Vec<u8>>>> {
        if !self.supports_capability(RepoCapability::CopyTracking) {
            return Ok(None);
        }

        let (reply_tx, reply_rx) = std::sync::mpsc::channel();
        self.tx
            .send(RpcMsg::GetRelatedCopies {
                copy_id: copy_id.to_vec(),
                reply: reply_tx,
            })
            .map_err(|_| anyhow!("RPC channel closed"))?;
        reply_rx.recv().map_err(|_| anyhow!("RPC reply dropped"))?
    }

    pub fn resolve_op_prefix(&self, hex_prefix: &str) -> Result<(PrefixResult, Option<Vec<u8>>)> {
        let (reply_tx, reply_rx) = std::sync::mpsc::channel();
        self.tx
            .send(RpcMsg::ResolveOpPrefix {
                hex_prefix: hex_prefix.to_string(),
                reply: reply_tx,
            })
            .map_err(|_| anyhow!("RPC channel closed"))?;
        reply_rx.recv().map_err(|_| anyhow!("RPC reply dropped"))?
    }
}

// ─── RPC event loop (runs on dedicated thread) ───────────────────────────────

pub(crate) async fn connect_stream(endpoint: &str) -> Result<tokio::net::TcpStream> {
    let target = ConnectorTarget::parse(endpoint)?;
    let addr = target.display_addr().to_string();

    match target {
        ConnectorTarget::Tcp { .. } => {
            let stream =
                tokio::time::timeout(CONNECT_TIMEOUT, tokio::net::TcpStream::connect(&addr))
                    .await
                    .map_err(|_| {
                        anyhow!(
                            "connection timed out after {}s to {addr}",
                            CONNECT_TIMEOUT.as_secs()
                        )
                    })?
                    .with_context(|| format!("failed to connect to tandem server at {addr}"))?;
            stream.set_nodelay(true).ok();
            Ok(stream)
        }
    }
}

fn spawn_store_client(stream: tokio::net::TcpStream) -> store::Client {
    let (reader, writer) = stream.into_split();
    let network = twoparty::VatNetwork::new(
        reader.compat(),
        writer.compat_write(),
        rpc_twoparty_capnp::Side::Client,
        Default::default(),
    );
    let mut rpc_system = RpcSystem::new(Box::new(network), None);
    let client: store::Client = rpc_system.bootstrap(rpc_twoparty_capnp::Side::Server);
    tokio::task::spawn_local(rpc_system);
    client
}

async fn connect_store_client(
    addr: &str,
    required_capabilities: &[RepoCapability],
) -> Result<(store::Client, RepoInfoResponse)> {
    let stream = connect_stream(addr).await?;
    let client = spawn_store_client(stream);

    let repo_info = do_get_repo_info(&client)
        .await
        .map_err(|e| anyhow!("failed to read repo compatibility info from {addr}: {e:#}"))?;
    validate_repo_info(&repo_info, required_capabilities)
        .map_err(|e| anyhow!("server {addr} is incompatible: {e:#}"))?;

    Ok((client, repo_info))
}

fn bench_injected_rtt_delay() -> std::time::Duration {
    let Some(raw_value) = std::env::var(BENCH_INJECT_RTT_MS_ENV).ok() else {
        return std::time::Duration::ZERO;
    };

    match raw_value.trim().parse::<u64>() {
        Ok(0) => std::time::Duration::ZERO,
        Ok(ms) => std::time::Duration::from_millis(ms),
        Err(_) => {
            tracing::warn!(
                env = BENCH_INJECT_RTT_MS_ENV,
                value = %raw_value,
                "ignoring invalid bench RTT injection value"
            );
            std::time::Duration::ZERO
        }
    }
}

fn env_truthy(var: &str) -> bool {
    std::env::var(var)
        .map(|value| {
            let normalized = value.trim().to_ascii_lowercase();
            matches!(normalized.as_str(), "1" | "true" | "yes" | "on")
        })
        .unwrap_or(false)
}

fn rpc_max_inflight() -> usize {
    if env_truthy(BENCH_DISABLE_RPC_INFLIGHT_ENV) {
        return 1;
    }

    let parsed = std::env::var(RPC_MAX_INFLIGHT_ENV)
        .ok()
        .and_then(|value| value.trim().parse::<usize>().ok());

    parsed.unwrap_or(DEFAULT_RPC_MAX_INFLIGHT).max(1)
}

async fn rpc_loop(
    addr: String,
    required_capabilities: Vec<RepoCapability>,
    mut rx: tokio::sync::mpsc::UnboundedReceiver<RpcMsg>,
    ready_tx: std::sync::mpsc::Sender<Result<RepoInfoResponse>>,
) {
    let connect_result = connect_store_client(&addr, &required_capabilities).await;

    let (client, _repo_info) = match connect_result {
        Ok(v) => {
            let _ = ready_tx.send(Ok(v.1.clone()));
            v
        }
        Err(e) => {
            let _ = ready_tx.send(Err(e));
            return;
        }
    };

    let injected_rtt = bench_injected_rtt_delay();
    let max_inflight = rpc_max_inflight();

    if max_inflight <= 1 {
        while let Some(msg) = rx.recv().await {
            if !injected_rtt.is_zero() {
                tokio::time::sleep(injected_rtt).await;
            }
            handle_msg(&client, msg).await;
        }
        return;
    }

    let permits = Arc::new(tokio::sync::Semaphore::new(max_inflight));
    while let Some(msg) = rx.recv().await {
        let Ok(permit) = permits.clone().acquire_owned().await else {
            break;
        };

        let client = client.clone();
        tokio::task::spawn_local(async move {
            if !injected_rtt.is_zero() {
                tokio::time::sleep(injected_rtt).await;
            }
            handle_msg(&client, msg).await;
            drop(permit);
        });
    }
}

async fn handle_msg(client: &store::Client, msg: RpcMsg) {
    match msg {
        RpcMsg::GetObject { kind, id, reply } => {
            let _ = reply.send(do_get_object(client, kind, &id).await);
        }
        RpcMsg::PutObject { kind, data, reply } => {
            let _ = reply.send(do_put_object(client, kind, &data).await);
        }
        RpcMsg::GetOperation { id, reply } => {
            let _ = reply.send(do_get_operation(client, &id).await);
        }
        RpcMsg::PutOperation { data, reply } => {
            let _ = reply.send(do_put_operation(client, &data).await);
        }
        RpcMsg::GetView { id, reply } => {
            let _ = reply.send(do_get_view(client, &id).await);
        }
        RpcMsg::PutView { data, reply } => {
            let _ = reply.send(do_put_view(client, &data).await);
        }
        RpcMsg::GetHeads { reply } => {
            let _ = reply.send(do_get_heads(client).await);
        }
        RpcMsg::UpdateOpHeads {
            old_ids,
            new_id,
            expected_version,
            workspace_id,
            reply,
        } => {
            let _ = reply.send(
                do_update_op_heads(client, &old_ids, &new_id, expected_version, &workspace_id)
                    .await,
            );
        }
        RpcMsg::GetHeadsSnapshot { reply } => {
            let _ = reply.send(do_get_heads_snapshot(client).await.map(Some));
        }
        RpcMsg::GetRelatedCopies { copy_id, reply } => {
            let _ = reply.send(do_get_related_copies(client, &copy_id).await.map(Some));
        }
        RpcMsg::ResolveOpPrefix { hex_prefix, reply } => {
            let _ = reply.send(do_resolve_op_prefix(client, &hex_prefix).await);
        }
    }
}

// ─── Individual RPC handlers ──────────────────────────────────────────────────

async fn do_get_repo_info(client: &store::Client) -> Result<RepoInfoResponse> {
    let request = client.get_repo_info_request();
    let response = request.send().promise.await?;
    let info = response.get()?.get_info()?;

    let mut capabilities = BTreeSet::new();
    let caps_reader = info.get_capabilities()?;
    for i in 0..caps_reader.len() {
        capabilities.insert(RepoCapability::from_capnp(caps_reader.get(i)?));
    }

    Ok(RepoInfoResponse {
        protocol_major: info.get_protocol_major(),
        protocol_minor: info.get_protocol_minor(),
        backend_name: info.get_backend_name()?.to_string()?,
        op_store_name: info.get_op_store_name()?.to_string()?,
        commit_id_length: info.get_commit_id_length() as usize,
        change_id_length: info.get_change_id_length() as usize,
        root_commit_id: info.get_root_commit_id()?.to_vec(),
        root_change_id: info.get_root_change_id()?.to_vec(),
        empty_tree_id: info.get_empty_tree_id()?.to_vec(),
        root_operation_id: info.get_root_operation_id()?.to_vec(),
        capabilities,
    })
}

async fn do_get_object(client: &store::Client, kind: u16, id: &[u8]) -> Result<Vec<u8>> {
    let mut request = client.get_object_request();
    {
        let mut params = request.get();
        params.set_kind(capnp_kind(kind)?);
        params.set_id(id);
    }
    let response = request.send().promise.await?;
    let data = response.get()?.get_data()?;
    Ok(data.to_vec())
}

async fn do_put_object(
    client: &store::Client,
    kind: u16,
    data: &[u8],
) -> Result<(Vec<u8>, Vec<u8>)> {
    let mut request = client.put_object_request();
    {
        let mut params = request.get();
        params.set_kind(capnp_kind(kind)?);
        params.set_data(data);
    }
    let response = request.send().promise.await?;
    let reader = response.get()?;
    let id = reader.get_id()?.to_vec();
    let normalized = reader.get_normalized_data()?.to_vec();
    Ok((id, normalized))
}

async fn do_get_operation(client: &store::Client, id: &[u8]) -> Result<Vec<u8>> {
    let mut request = client.get_operation_request();
    request.get().set_id(id);
    let response = request.send().promise.await?;
    Ok(response.get()?.get_data()?.to_vec())
}

async fn do_put_operation(client: &store::Client, data: &[u8]) -> Result<Vec<u8>> {
    let mut request = client.put_operation_request();
    request.get().set_data(data);
    let response = request.send().promise.await?;
    Ok(response.get()?.get_id()?.to_vec())
}

async fn do_get_view(client: &store::Client, id: &[u8]) -> Result<Vec<u8>> {
    let mut request = client.get_view_request();
    request.get().set_id(id);
    let response = request.send().promise.await?;
    Ok(response.get()?.get_data()?.to_vec())
}

async fn do_put_view(client: &store::Client, data: &[u8]) -> Result<Vec<u8>> {
    let mut request = client.put_view_request();
    request.get().set_data(data);
    let response = request.send().promise.await?;
    Ok(response.get()?.get_id()?.to_vec())
}

async fn do_get_heads(client: &store::Client) -> Result<HeadsState> {
    let request = client.get_heads_request();
    let response = request.send().promise.await?;
    let reader = response.get()?;
    let version = reader.get_version();
    let heads_reader = reader.get_heads()?;
    let mut heads = Vec::with_capacity(heads_reader.len() as usize);
    for i in 0..heads_reader.len() {
        heads.push(heads_reader.get(i)?.to_vec());
    }

    let workspace_heads_reader = reader.get_workspace_heads()?;
    let mut workspace_heads = BTreeMap::new();
    for i in 0..workspace_heads_reader.len() {
        let entry = workspace_heads_reader.get(i);
        let workspace_id = entry.get_workspace_id()?.to_string()?;
        let op_id = entry.get_commit_id()?.to_vec();
        if !workspace_id.is_empty() && !op_id.is_empty() {
            workspace_heads.insert(workspace_id, op_id);
        }
    }

    Ok(HeadsState {
        heads,
        version,
        workspace_heads,
    })
}

async fn do_get_heads_snapshot(client: &store::Client) -> Result<HeadsSnapshot> {
    let request = client.get_heads_snapshot_request();
    let response = request.send().promise.await?;
    let reader = response.get()?;

    let heads_reader = reader.get_heads()?;
    let mut heads = Vec::with_capacity(heads_reader.len() as usize);
    for i in 0..heads_reader.len() {
        heads.push(heads_reader.get(i)?.to_vec());
    }

    Ok(HeadsSnapshot {
        heads,
        version: reader.get_version(),
    })
}

async fn do_get_related_copies(client: &store::Client, copy_id: &[u8]) -> Result<Vec<Vec<u8>>> {
    let mut request = client.get_related_copies_request();
    request.get().set_copy_id(copy_id);
    let response = request.send().promise.await?;
    let reader = response.get()?;
    let copies_reader = reader.get_copies()?;

    let mut copies = Vec::with_capacity(copies_reader.len() as usize);
    for i in 0..copies_reader.len() {
        copies.push(copies_reader.get(i)?.to_vec());
    }

    Ok(copies)
}

async fn do_update_op_heads(
    client: &store::Client,
    old_ids: &[Vec<u8>],
    new_id: &[u8],
    expected_version: u64,
    workspace_id: &str,
) -> Result<UpdateHeadsResult> {
    let mut request = client.update_op_heads_request();
    {
        let mut params = request.get();
        let mut old_list = params.reborrow().init_old_ids(old_ids.len() as u32);
        for (i, oid) in old_ids.iter().enumerate() {
            old_list.set(i as u32, oid);
        }
        params.set_new_id(new_id);
        params.set_expected_version(expected_version);
        params.set_workspace_id(workspace_id);
    }
    let response = request.send().promise.await?;
    let reader = response.get()?;
    let ok = reader.get_ok();
    let version = reader.get_version();
    let heads_reader = reader.get_heads()?;
    let mut heads = Vec::with_capacity(heads_reader.len() as usize);
    for i in 0..heads_reader.len() {
        heads.push(heads_reader.get(i)?.to_vec());
    }
    Ok(UpdateHeadsResult { ok, heads, version })
}

async fn do_resolve_op_prefix(
    client: &store::Client,
    hex_prefix: &str,
) -> Result<(PrefixResult, Option<Vec<u8>>)> {
    let mut request = client.resolve_operation_id_prefix_request();
    request.get().set_hex_prefix(hex_prefix);
    let response = request.send().promise.await?;
    let reader = response.get()?;
    let resolution = reader.get_resolution()?;
    let result = match resolution {
        crate::tandem_capnp::PrefixResolution::NoMatch => PrefixResult::NoMatch,
        crate::tandem_capnp::PrefixResolution::SingleMatch => PrefixResult::SingleMatch,
        crate::tandem_capnp::PrefixResolution::Ambiguous => PrefixResult::Ambiguous,
    };
    let matched = if result == PrefixResult::SingleMatch {
        let m = reader.get_match()?;
        if m.is_empty() {
            None
        } else {
            Some(m.to_vec())
        }
    } else {
        None
    };
    Ok((result, matched))
}

// ─── Helpers ──────────────────────────────────────────────────────────────────

fn validate_repo_info(
    info: &RepoInfoResponse,
    required_capabilities: &[RepoCapability],
) -> Result<()> {
    if info.protocol_major != PROTOCOL_MAJOR {
        bail!(
            "repo compatibility mismatch: protocol_major expected {PROTOCOL_MAJOR} but server advertised {}",
            info.protocol_major
        );
    }

    if info.protocol_minor != PROTOCOL_MINOR {
        bail!(
            "repo compatibility mismatch: protocol_minor expected {PROTOCOL_MINOR} but server advertised {}",
            info.protocol_minor
        );
    }

    if info.backend_name != EXPECTED_BACKEND_NAME {
        bail!(
            "repo compatibility mismatch: backend_name expected {:?} but server advertised {:?}",
            EXPECTED_BACKEND_NAME,
            info.backend_name
        );
    }

    if info.op_store_name != EXPECTED_OP_STORE_NAME {
        bail!(
            "repo compatibility mismatch: op_store_name expected {:?} but server advertised {:?}",
            EXPECTED_OP_STORE_NAME,
            info.op_store_name
        );
    }

    if info.commit_id_length == 0 {
        bail!("repo compatibility mismatch: commit_id_length must be > 0");
    }

    if info.change_id_length == 0 {
        bail!("repo compatibility mismatch: change_id_length must be > 0");
    }

    if info.root_commit_id.len() != info.commit_id_length {
        bail!(
            "repo compatibility mismatch: root_commit_id length {} does not match commit_id_length {}",
            info.root_commit_id.len(),
            info.commit_id_length
        );
    }

    if info.root_change_id.len() != info.change_id_length {
        bail!(
            "repo compatibility mismatch: root_change_id length {} does not match change_id_length {}",
            info.root_change_id.len(),
            info.change_id_length
        );
    }

    if info.empty_tree_id.len() != info.commit_id_length {
        bail!(
            "repo compatibility mismatch: empty_tree_id length {} does not match commit_id_length {}",
            info.empty_tree_id.len(),
            info.commit_id_length
        );
    }

    if info.root_operation_id.len() != ROOT_OPERATION_ID_LENGTH {
        bail!(
            "repo compatibility mismatch: root_operation_id length {} does not match expected {}",
            info.root_operation_id.len(),
            ROOT_OPERATION_ID_LENGTH
        );
    }

    for capability in required_capabilities {
        if !info.capabilities.contains(capability) {
            bail!(
                "repo compatibility mismatch: missing required capability {}",
                capability.as_str()
            );
        }
    }

    Ok(())
}

fn capnp_kind(kind: u16) -> Result<crate::tandem_capnp::ObjectKind> {
    match kind {
        0 => Ok(crate::tandem_capnp::ObjectKind::Commit),
        1 => Ok(crate::tandem_capnp::ObjectKind::Tree),
        2 => Ok(crate::tandem_capnp::ObjectKind::File),
        3 => Ok(crate::tandem_capnp::ObjectKind::Symlink),
        4 => Ok(crate::tandem_capnp::ObjectKind::Copy),
        _ => Err(anyhow!("unknown object kind: {kind}")),
    }
}

#[cfg(test)]
mod tests {
    use super::ConnectorTarget;

    #[test]
    fn connector_target_parses_raw_host_port_as_tcp() {
        let parsed = ConnectorTarget::parse("127.0.0.1:12345").expect("parse endpoint");
        match parsed {
            ConnectorTarget::Tcp { addr } => assert_eq!(addr, "127.0.0.1:12345"),
        }
    }

    #[test]
    fn connector_target_rejects_unknown_transport_scheme() {
        let err = ConnectorTarget::parse("wss://example.com:443").expect_err("must reject wss");
        assert!(
            err.to_string()
                .contains("unsupported tandem transport scheme"),
            "unexpected error: {err:#}"
        );
    }
}
