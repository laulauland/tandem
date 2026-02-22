//! Shared RPC client wrapper for connecting to a tandem server.
//!
//! Manages a Cap'n Proto connection on a dedicated thread (because capnp-rpc
//! types are !Send). Communication from Backend/OpStore/OpHeadsStore happens
//! through std::sync::mpsc channels.

use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use capnp_rpc::{rpc_twoparty_capnp, twoparty, RpcSystem};
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

use crate::tandem_capnp::store;

// ─── Public types ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct RepoInfoResponse {
    pub commit_id_length: usize,
    pub change_id_length: usize,
    pub root_commit_id: Vec<u8>,
    pub root_change_id: Vec<u8>,
    pub empty_tree_id: Vec<u8>,
    pub root_operation_id: Vec<u8>,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct UpdateHeadsResult {
    pub ok: bool,
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

enum RpcMsg {
    GetRepoInfo {
        reply: Reply<RepoInfoResponse>,
    },
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
        reply: Reply<(Vec<Vec<u8>>, u64)>,
    },
    UpdateOpHeads {
        old_ids: Vec<Vec<u8>>,
        new_id: Vec<u8>,
        expected_version: u64,
        workspace_id: String,
        reply: Reply<UpdateHeadsResult>,
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
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<RpcMsg>();
        let addr_owned = addr.to_string();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<()>>();

        let addr_for_thread = addr_owned.clone();
        let thread = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("build tokio runtime for RPC thread");
            let local = tokio::task::LocalSet::new();
            local.block_on(&rt, rpc_loop(addr_for_thread, rx, ready_tx));
        });

        // Wait for connection to be established
        ready_rx
            .recv()
            .map_err(|_| anyhow!("RPC thread died before signaling readiness"))??;

        Ok(Arc::new(TandemClient {
            tx,
            _thread: thread,
            server_addr: addr_owned,
        }))
    }

    /// Get the server address this client is connected to.
    pub fn server_addr(&self) -> &str {
        &self.server_addr
    }

    // ─── Blocking RPC methods ─────────────────────────────────────────

    pub fn get_repo_info(&self) -> Result<RepoInfoResponse> {
        let (reply_tx, reply_rx) = std::sync::mpsc::channel();
        self.tx
            .send(RpcMsg::GetRepoInfo { reply: reply_tx })
            .map_err(|_| anyhow!("RPC channel closed"))?;
        reply_rx.recv().map_err(|_| anyhow!("RPC reply dropped"))?
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

    pub fn get_heads(&self) -> Result<(Vec<Vec<u8>>, u64)> {
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

async fn rpc_loop(
    addr: String,
    mut rx: tokio::sync::mpsc::UnboundedReceiver<RpcMsg>,
    ready_tx: std::sync::mpsc::Sender<Result<()>>,
) {
    let connect_result = async {
        let stream = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            tokio::net::TcpStream::connect(&addr),
        )
        .await
        .map_err(|_| anyhow!("connection timed out after 5s to {addr}"))?
        .with_context(|| format!("failed to connect to tandem server at {addr}"))?;
        stream.set_nodelay(true).ok();
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
        Ok::<_, anyhow::Error>(client)
    }
    .await;

    let client = match connect_result {
        Ok(c) => {
            let _ = ready_tx.send(Ok(()));
            c
        }
        Err(e) => {
            let _ = ready_tx.send(Err(e));
            return;
        }
    };

    while let Some(msg) = rx.recv().await {
        handle_msg(&client, msg).await;
    }
}

async fn handle_msg(client: &store::Client, msg: RpcMsg) {
    match msg {
        RpcMsg::GetRepoInfo { reply } => {
            let _ = reply.send(do_get_repo_info(client).await);
        }
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
    Ok(RepoInfoResponse {
        commit_id_length: info.get_commit_id_length() as usize,
        change_id_length: info.get_change_id_length() as usize,
        root_commit_id: info.get_root_commit_id()?.to_vec(),
        root_change_id: info.get_root_change_id()?.to_vec(),
        empty_tree_id: info.get_empty_tree_id()?.to_vec(),
        root_operation_id: info.get_root_operation_id()?.to_vec(),
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

async fn do_get_heads(client: &store::Client) -> Result<(Vec<Vec<u8>>, u64)> {
    let request = client.get_heads_request();
    let response = request.send().promise.await?;
    let reader = response.get()?;
    let version = reader.get_version();
    let heads_reader = reader.get_heads()?;
    let mut heads = Vec::with_capacity(heads_reader.len() as usize);
    for i in 0..heads_reader.len() {
        heads.push(heads_reader.get(i)?.to_vec());
    }
    Ok((heads, version))
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
