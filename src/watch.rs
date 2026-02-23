//! tandem watch — stream head-change notifications from a tandem server.
//!
//! Connects via Cap'n Proto, calls watchHeads with a HeadWatcher callback,
//! and prints each notification as: version=<N> heads=<hex1>,<hex2>,...

use anyhow::{Context, Result};
use capnp::capability::Promise;
use capnp_rpc::{rpc_twoparty_capnp, twoparty, RpcSystem};
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

use crate::rpc::{connect_stream, RepoCapability, TandemClient};
use crate::tandem_capnp::{head_watcher, store};

// ─── HeadWatcher callback implementation ──────────────────────────────────────

struct WatcherImpl {
    /// Sender to push notification lines to the main loop for printing.
    tx: tokio::sync::mpsc::UnboundedSender<String>,
}

impl head_watcher::Server for WatcherImpl {
    fn notify(
        &mut self,
        params: head_watcher::NotifyParams,
        _results: head_watcher::NotifyResults,
    ) -> Promise<(), capnp::Error> {
        let reader = match params.get() {
            Ok(r) => r,
            Err(e) => return Promise::err(e),
        };
        let version = reader.get_version();
        let heads_reader = match reader.get_heads() {
            Ok(h) => h,
            Err(e) => return Promise::err(e),
        };

        let mut hex_heads = Vec::with_capacity(heads_reader.len() as usize);
        for i in 0..heads_reader.len() {
            match heads_reader.get(i) {
                Ok(bytes) => {
                    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
                    hex_heads.push(hex);
                }
                Err(e) => return Promise::err(e),
            }
        }

        let line = format!("version={version} heads={}", hex_heads.join(","));
        let _ = self.tx.send(line);
        Promise::ok(())
    }
}

// ─── Public entry point ───────────────────────────────────────────────────────

pub fn run_watch(server_addr: &str) -> Result<()> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let local = tokio::task::LocalSet::new();

    local.block_on(&rt, watch_loop(server_addr))
}

async fn watch_loop(addr: &str) -> Result<()> {
    // Preflight compatibility + required capability before starting long-lived watch.
    let preflight = TandemClient::connect_with_requirements(addr, &[RepoCapability::WatchHeads])
        .with_context(|| format!("watch preflight failed for {addr}"))?;
    drop(preflight);

    // Connect to server using the shared connector abstraction.
    let stream = connect_stream(addr)
        .await
        .with_context(|| format!("watch connection failed for {addr}"))?;

    let (reader, writer) = stream.into_split();
    let network = twoparty::VatNetwork::new(
        reader.compat(),
        writer.compat_write(),
        rpc_twoparty_capnp::Side::Client,
        Default::default(),
    );
    let mut rpc_system = RpcSystem::new(Box::new(network), None);
    let client: store::Client = rpc_system.bootstrap(rpc_twoparty_capnp::Side::Server);
    let mut rpc_task = tokio::task::spawn_local(rpc_system);

    // Create notification channel
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();

    // Create HeadWatcher callback
    let watcher_impl = WatcherImpl { tx };
    let watcher_client: head_watcher::Client = capnp_rpc::new_client(watcher_impl);

    // Call watchHeads(watcher, afterVersion=0) to get all notifications from the start
    let mut request = client.watch_heads_request();
    {
        let mut params = request.get();
        params.set_watcher(watcher_client);
        params.set_after_version(0);
    }
    let _response = request.send().promise.await?;

    eprintln!("watching heads on {addr}...");

    // Print notifications until channel closes or RPC disconnects
    loop {
        tokio::select! {
            line = rx.recv() => {
                match line {
                    Some(l) => println!("{l}"),
                    None => break,
                }
            }
            result = &mut rpc_task => {
                match result {
                    Ok(Ok(())) => break,
                    Ok(Err(e)) => {
                        eprintln!("rpc error: {e}");
                        break;
                    }
                    Err(e) => {
                        eprintln!("rpc task panicked: {e}");
                        break;
                    }
                }
            }
        }
    }

    Ok(())
}
