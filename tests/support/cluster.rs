//! One tandem server, in this process.
//!
//! The simulation needs to stop a server between two named points of a publish
//! and start it again over the same repo and the same bucket. A subprocess
//! could do that, but only for the crash points a command-line flag exposes,
//! and only at the cost of a process spawn per step. Holding the `Server` here
//! makes every window reachable and every schedule cheap enough to run
//! hundreds of.

use std::future::IntoFuture;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use jj_tandem::server::{FaultPoints, Server};
use tempfile::TempDir;
use tokio::runtime::Runtime;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

pub struct Cluster {
    /// Everything this cluster owns on disk. Dropped last.
    root: TempDir,
    /// The server's repo — the materialized copy, not the durable one.
    pub repo: PathBuf,
    /// The bucket: the durable copy, and what a restart recovers from.
    pub bucket: PathBuf,
    /// Where clients reach the server. Stable across a restart.
    pub addr: String,
    /// The faults this server is under. Shared with every restart of it.
    pub faults: Arc<FaultPoints>,
    /// The token that mints workspace tokens. Fixed for the life of the
    /// cluster, so that a restart keeps accepting what the agents hold.
    pub admin_token: String,
    runtime: Runtime,
    running: Option<Running>,
}

struct Running {
    drain: oneshot::Sender<()>,
    serve: JoinHandle<()>,
}

impl Cluster {
    /// A started server with an empty repo and an empty bucket.
    pub fn start() -> Result<Self> {
        super::isolate_process_environment();

        let root = tempfile::tempdir().context("create cluster directory")?;
        let repo = root.path().join("server-repo");
        let bucket = root.path().join("bucket");
        std::fs::create_dir_all(&bucket).context("create bucket directory")?;

        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .context("build the server runtime")?;

        let mut cluster = Self {
            root,
            repo,
            bucket,
            addr: "127.0.0.1:0".to_string(),
            faults: FaultPoints::inert(),
            admin_token: jj_tandem::auth::generate_admin_token(),
            runtime,
            running: None,
        };
        cluster.boot()?;
        Ok(cluster)
    }

    pub fn workspace_root(&self) -> &Path {
        self.root.path()
    }

    /// Bring a server up over the existing repo and bucket.
    fn boot(&mut self) -> Result<()> {
        let server = Server::new_with_faults(
            self.repo.clone(),
            false,
            Some(&self.bucket.to_string_lossy()),
            &self.admin_token,
            Arc::clone(&self.faults),
        )
        .context("start the in-process server")?;

        let app = jj_tandem::server::http::router(Arc::new(server));

        let std_listener = bind(&self.addr)?;
        self.addr = std_listener.local_addr()?.to_string();
        std_listener.set_nonblocking(true)?;

        let (drain, drain_rx) = oneshot::channel::<()>();
        let guard = self.runtime.enter();
        let listener = tokio::net::TcpListener::from_std(std_listener)?;
        drop(guard);

        let serve = self.runtime.spawn(async move {
            let served = axum::serve(listener, app)
                .with_graceful_shutdown(async move {
                    let _ = drain_rx.await;
                })
                .into_future()
                .await;
            if let Err(err) = served {
                eprintln!("in-process server stopped: {err}");
            }
        });

        self.running = Some(Running { drain, serve });
        Ok(())
    }

    /// Take the server away, as a dead process would.
    pub fn stop(&mut self) {
        let Some(running) = self.running.take() else {
            return;
        };
        let _ = running.drain.send(());
        let _ = self.runtime.block_on(async {
            tokio::time::timeout(std::time::Duration::from_secs(5), running.serve).await
        });
    }

    /// What happens after a crash: a new process over the same durable state,
    /// with no fault armed, because a fault is a thing a test does to one run.
    pub fn restart(&mut self) -> Result<()> {
        self.stop();
        self.faults.crash_at(None);
        self.boot()
    }

    /// A restart that keeps nothing but the bucket.
    ///
    /// A warm restart reuses the materialized repo, so an object that reached
    /// the server's disk but never reached a WAL entry stays readable and the
    /// gap stays invisible. Throwing the disk away is what makes the WAL the
    /// only source of the answer — which is what the design doc says it is.
    pub fn cold_restart(&mut self) -> Result<()> {
        self.stop();
        self.faults.crash_at(None);
        std::fs::remove_dir_all(&self.repo)
            .with_context(|| format!("remove the server repo at {}", self.repo.display()))?;
        std::fs::create_dir_all(&self.repo)
            .with_context(|| format!("recreate the server repo at {}", self.repo.display()))?;
        self.boot()
    }

    /// Whether an armed crash has fired and the server is refusing work.
    pub fn halted(&self) -> bool {
        self.faults.halted()
    }

    pub fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }
}

/// Claim a listening socket, retrying a concrete port for a moment.
///
/// Bind through std so that the port is claimed before it is announced:
/// learning the port from one listener and binding a second one leaves a window
/// in which something else can take it.
///
/// A restart asks for the port the first boot was given, because the agents
/// wrote that address into their store configuration and cannot be told a new
/// one. Between the stop and the bind that port is free, and another lane of
/// the simulation binding `:0` in the same process can be handed it. That is
/// rare and it is not the seed's fault, so it is retried rather than reported:
/// the other lane's server holds its port for the length of a case, so if it
/// really is gone, the retries end and the error says so.
fn bind(addr: &str) -> Result<std::net::TcpListener> {
    const ATTEMPTS: usize = 50;
    let mut last = None;
    for attempt in 0..ATTEMPTS {
        match std::net::TcpListener::bind(addr) {
            Ok(listener) => return Ok(listener),
            Err(err) if addr.ends_with(":0") => {
                return Err(err).with_context(|| format!("bind {addr}"))
            }
            Err(err) => {
                last = Some(err);
                if attempt + 1 < ATTEMPTS {
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
            }
        }
    }
    Err(last.expect("at least one attempt")).with_context(|| {
        format!("bind {addr}: still taken after {ATTEMPTS} attempts; another lane took the port")
    })
}

impl Drop for Cluster {
    fn drop(&mut self) {
        self.stop();
    }
}
