//! Network host around the headless Tandem repository authority.

mod auth;
pub mod control;
mod hosted;
mod http;
mod logging;
mod process;
mod writer;

pub use auth::generate_admin_token;
pub use http::router;
pub use process::{resolve_control_socket, start_background, stop_background, StartedServer};

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use jj_tandem_protocol::wire;
use jj_tandem_repository::{PublishAuthority, Repository, UpdateResult};
use tokio::sync::broadcast;

use jj_tandem_repository::{MalformedId, NotFound, ScopeDenied};

pub struct Server {
    repository: Arc<Repository>,
    tokens: auth::TokenStore,
    writer_roles: writer::WriterRoles,
    body_admission: Arc<tokio::sync::Semaphore>,
    control_body_admission: Arc<tokio::sync::Semaphore>,
    publish_admission: PublishAdmission,
}

const MAX_DECODED_BODIES: usize = 4;
const MAX_ACTIVE_PUBLISHES: usize = 4;
const MAX_QUEUED_PUBLISHES_PER_REPOSITORY: usize = 8;

struct PublishAdmission {
    repository: Arc<tokio::sync::Semaphore>,
    host: Arc<tokio::sync::Semaphore>,
    queued: AtomicUsize,
}

#[derive(Debug)]
struct PublishPermit {
    _repository: tokio::sync::OwnedSemaphorePermit,
    _host: tokio::sync::OwnedSemaphorePermit,
    wait_ms: u64,
    queue_depth: usize,
}

struct PublishPermitCell(std::sync::Mutex<Option<PublishPermit>>);

struct QueueReservation<'a>(&'a AtomicUsize);

#[derive(Debug)]
struct PublishQueueFull;

impl std::fmt::Display for PublishQueueFull {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "repository publish queue is full")
    }
}

impl std::error::Error for PublishQueueFull {}

impl Drop for QueueReservation<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

impl PublishAdmission {
    fn new(host: Arc<tokio::sync::Semaphore>) -> Self {
        Self {
            repository: Arc::new(tokio::sync::Semaphore::new(1)),
            host,
            queued: AtomicUsize::new(0),
        }
    }

    async fn acquire(&self) -> Result<PublishPermit> {
        let started = std::time::Instant::now();
        let (repository, queue_depth) = match self.repository.clone().try_acquire_owned() {
            Ok(permit) => (permit, 0),
            Err(_) => {
                let previous = self
                    .queued
                    .fetch_update(Ordering::AcqRel, Ordering::Acquire, |queued| {
                        (queued < MAX_QUEUED_PUBLISHES_PER_REPOSITORY).then_some(queued + 1)
                    })
                    .map_err(|_| anyhow::Error::new(PublishQueueFull))?;
                let reservation = QueueReservation(&self.queued);
                let permit = self
                    .repository
                    .clone()
                    .acquire_owned()
                    .await
                    .map_err(|_| anyhow::anyhow!("repository publish admission closed"));
                drop(reservation);
                (permit?, previous + 1)
            }
        };
        let host = self
            .host
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| anyhow::anyhow!("host publish admission closed"))?;
        Ok(PublishPermit {
            _repository: repository,
            _host: host,
            wait_ms: started.elapsed().as_millis() as u64,
            queue_depth,
        })
    }
}

impl Server {
    async fn acquire_body(&self, control: bool) -> Result<tokio::sync::OwnedSemaphorePermit> {
        (if control {
            &self.control_body_admission
        } else {
            &self.body_admission
        })
        .clone()
        .acquire_owned()
        .await
        .map_err(|_| anyhow::anyhow!("request body admission closed"))
    }

    async fn acquire_publish(&self) -> Result<PublishPermit> {
        self.publish_admission.acquire().await
    }
    pub fn new(repo: PathBuf, bucket: Option<&str>, admin_token: &str) -> Result<Self> {
        let settings = user_settings()?;
        Ok(Self::from_repository_with_admission(
            Repository::new(&settings, repo, bucket)?,
            admin_token,
            Arc::new(tokio::sync::Semaphore::new(MAX_DECODED_BODIES - 1)),
            Arc::new(tokio::sync::Semaphore::new(1)),
            Arc::new(tokio::sync::Semaphore::new(MAX_ACTIVE_PUBLISHES)),
        ))
    }

    pub(crate) fn new_with_faults_and_budget(
        repo: PathBuf,
        bucket: Option<&str>,
        admin_token: &str,
        faults: Arc<jj_tandem_repository::FaultPoints>,
        staging_budget: Arc<jj_tandem_repository::StagingBudget>,
        body_admission: Arc<tokio::sync::Semaphore>,
        control_body_admission: Arc<tokio::sync::Semaphore>,
        publish_admission: Arc<tokio::sync::Semaphore>,
    ) -> Result<Self> {
        let settings = user_settings()?;
        Ok(Self::from_repository_with_admission(
            jj_tandem_repository::Repository::new_with_faults_and_budget(
                &settings,
                repo,
                bucket,
                faults,
                staging_budget,
            )?,
            admin_token,
            body_admission,
            control_body_admission,
            publish_admission,
        ))
    }

    /// In-process integration seam for driving an exact repository fault.
    #[doc(hidden)]
    pub fn new_with_faults_for_test(
        repo: PathBuf,
        bucket: Option<&str>,
        admin_token: &str,
        faults: Arc<jj_tandem_repository::FaultPoints>,
    ) -> Result<Self> {
        Self::new_with_faults_and_budget(
            repo,
            bucket,
            admin_token,
            faults,
            Arc::new(jj_tandem_repository::StagingBudget::default()),
            Arc::new(tokio::sync::Semaphore::new(MAX_DECODED_BODIES - 1)),
            Arc::new(tokio::sync::Semaphore::new(1)),
            Arc::new(tokio::sync::Semaphore::new(MAX_ACTIVE_PUBLISHES)),
        )
    }

    pub fn from_repository(repository: Repository, admin_token: &str) -> Self {
        Self::from_repository_with_admission(
            repository,
            admin_token,
            Arc::new(tokio::sync::Semaphore::new(MAX_DECODED_BODIES - 1)),
            Arc::new(tokio::sync::Semaphore::new(1)),
            Arc::new(tokio::sync::Semaphore::new(MAX_ACTIVE_PUBLISHES)),
        )
    }

    fn from_repository_with_admission(
        repository: Repository,
        admin_token: &str,
        body_admission: Arc<tokio::sync::Semaphore>,
        control_body_admission: Arc<tokio::sync::Semaphore>,
        host_publish: Arc<tokio::sync::Semaphore>,
    ) -> Self {
        Self {
            repository: Arc::new(repository),
            tokens: auth::TokenStore::new(admin_token),
            writer_roles: writer::WriterRoles::new(),
            body_admission,
            control_body_admission,
            publish_admission: PublishAdmission::new(host_publish),
        }
    }

    #[doc(hidden)]
    pub fn durably_initialize(&self) -> Result<()> {
        self.repository.durably_initialize()
    }

    fn authority_for(&self, presented: &str) -> Option<auth::Authority> {
        self.tokens.authority_for(presented)
    }

    pub(crate) fn repo_info_body(&self) -> wire::RepoInfoBody {
        let info = self.repository.info();
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
            commit_id_length: info.commit_id_length,
            change_id_length: info.change_id_length,
            root_commit_id: info.root_commit_id,
            root_change_id: info.root_change_id,
            empty_tree_id: info.empty_tree_id,
            root_operation_id: info.root_operation_id,
            capabilities: test_repo_info_capabilities(),
        }
    }

    pub(crate) fn mint_token_sync(&self, workspace_id: &str, ttl: Duration) -> wire::TokenBody {
        let minted = self.tokens.mint(workspace_id, ttl);
        tracing::info!(workspace_id = %minted.workspace_id, ttl_seconds = minted.ttl.as_secs(), "minted a workspace token");
        wire::TokenBody {
            token: minted.token,
            workspace_id: minted.workspace_id,
            ttl_seconds: minted.ttl.as_secs(),
        }
    }

    pub(crate) fn claim_writer_role_sync(
        &self,
        workspace_id: &str,
        holder: &str,
        ttl: Duration,
    ) -> Result<writer::WriterRole, writer::WriterConflict> {
        let outcome = self.writer_roles.claim(workspace_id, holder, ttl);
        match &outcome {
            Ok(role) => {
                tracing::info!(workspace_id = %role.workspace_id, holder = %role.holder, expires_in_seconds = role.expires_in.as_secs(), "writer role claimed")
            }
            Err(conflict) => {
                tracing::info!(workspace_id = %conflict.workspace_id, holder = %conflict.holder, "writer role claim refused; another client holds it")
            }
        }
        outcome
    }

    pub(crate) fn update_op_heads_sync(
        &self,
        old_ids: Vec<Vec<u8>>,
        new_id: Vec<u8>,
        expected_version: u64,
        workspace_id: Option<String>,
        authority: &auth::Authority,
    ) -> Result<UpdateResult> {
        let authority = match authority {
            auth::Authority::Admin => PublishAuthority::Admin,
            auth::Authority::Workspace(name) => PublishAuthority::Workspace(name.clone()),
        };
        self.repository.update_op_heads_sync(
            old_ids,
            new_id,
            expected_version,
            workspace_id,
            &authority,
        )
    }
}

// Compatibility test hooks belong to the host, not the repository authority.
fn test_repo_info_u16(var: &str, default: u16) -> u16 {
    std::env::var(var)
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(default)
}

fn test_repo_info_text(var: &str, default: &'static str) -> String {
    std::env::var(var).unwrap_or_else(|_| default.to_string())
}

fn test_repo_info_capabilities() -> Vec<String> {
    let Ok(raw) = std::env::var("TANDEM_TEST_REPO_INFO_CAPABILITIES") else {
        return vec![wire::RepoCapability::WatchHeads.as_str().to_string()];
    };
    let mut caps = Vec::new();
    for token in raw.trim().split(',') {
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

fn user_settings() -> Result<jj_lib::settings::UserSettings> {
    let config_env = jj_cli::config::ConfigEnv::from_environment();
    let mut config =
        jj_cli::config::config_from_environment(jj_cli::config::default_config_layers());
    config_env
        .reload_user_config(&mut config)
        .context("load jj user config")?;
    let config = config_env
        .resolve_config(&config)
        .context("resolve jj config")?;
    jj_lib::settings::UserSettings::from_config(config).context("create jj settings")
}

#[allow(dead_code)]
pub struct ServeOptions {
    pub listen_addr: String,
    pub repo_path: String,
    pub log_level: String,
    pub log_format: String,
    pub control_socket: Option<String>,
    pub daemon: bool,
    pub log_file: Option<String>,
    pub bucket: Option<String>,
    pub hosted: bool,
    pub admin_token: Option<String>,
}

pub async fn run_serve(opts: ServeOptions) -> Result<()> {
    let (log_tx, _) = broadcast::channel::<control::LogEvent>(1024);
    logging::init_tracing(&opts.log_level, &opts.log_format, log_tx.clone())?;
    tracing::info!(listen_addr = %opts.listen_addr, repo = %opts.repo_path, daemon = opts.daemon, log_level = %opts.log_level, log_format = %opts.log_format, bucket = opts.bucket.as_deref().unwrap_or("<repo-local>"), "starting tandem server");
    if let Some(path) = opts.log_file.as_deref() {
        tracing::debug!(log_file = %path, "serve log file argument");
    }
    let admin_token = match opts.admin_token.clone() {
        Some(token) if !token.trim().is_empty() => token,
        _ => anyhow::bail!("serve requires TANDEM_ADMIN_TOKEN; configure a protected service secret or use `tandem up` for local startup"),
    };
    let hosted = opts
        .hosted
        .then(|| {
            opts.bucket
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("--hosted requires --bucket"))
        })
        .transpose()?
        .map(|bucket| {
            hosted::HostedServer::new(PathBuf::from(&opts.repo_path), bucket, &admin_token)
        })
        .transpose()?;
    let server = if hosted.is_none() {
        Some(Arc::new(Server::new(
            PathBuf::from(&opts.repo_path),
            opts.bucket.as_deref(),
            &admin_token,
        )?))
    } else {
        None
    };
    let listener = tokio::net::TcpListener::bind(&opts.listen_addr)
        .await
        .with_context(|| format!("failed to bind {}", opts.listen_addr))?;
    let local_addr = listener.local_addr()?;
    tracing::info!(listen_addr = %local_addr, "tandem server listening on");
    let (shutdown_tx, mut shutdown_rx) = tokio::sync::mpsc::channel::<()>(1);
    let control_socket_path = opts.control_socket.clone();
    if let Some(sock_path) = control_socket_path.clone() {
        let state = Arc::new(control::ControlState {
            pid: std::process::id(),
            start_time: std::time::Instant::now(),
            repo: opts.repo_path.clone(),
            listen: local_addr.to_string(),
            shutdown_tx: shutdown_tx.clone(),
            log_tx,
            bucket: match &server {
                Some(server) => server.repository.bucket_status().into(),
                None => control::BucketStatus {
                    backend: "hosted".to_string(),
                    location: opts.bucket.clone().unwrap_or_default(),
                    conditional_put: true,
                    materialized: false,
                    replayed_heads: 0,
                    replayed_entries: 0,
                    replay_ms: 0,
                },
            },
        });
        tokio::spawn(async move {
            if let Err(error) = control::run_control_socket(sock_path.clone(), state).await {
                tracing::error!(socket_path = %sock_path, %error, "control socket error");
            }
        });
    }
    let (signal_tx, mut signal_rx) = tokio::sync::mpsc::channel::<()>(2);
    let signal_tx_task = signal_tx.clone();
    tokio::spawn(async move {
        let mut interrupt =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
                .expect("SIGINT handler");
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .expect("SIGTERM handler");
        let mut first = true;
        loop {
            tokio::select! { _ = interrupt.recv() => {}, _ = terminate.recv() => {} }
            if first {
                first = false;
                tracing::warn!("signal received, shutting down gracefully");
                let _ = signal_tx_task.send(()).await;
            } else {
                tracing::error!("second signal received, forcing shutdown");
                std::process::exit(0);
            }
        }
    });
    let (drain_tx, drain_rx) = tokio::sync::oneshot::channel();
    let serve = tokio::spawn(std::future::IntoFuture::into_future(
        axum::serve(
            listener,
            match hosted {
                Some(hosted) => hosted::router(Arc::new(hosted)),
                None => http::router(server.expect("single repository server")),
            },
        )
        .with_graceful_shutdown(async move {
            let _ = drain_rx.await;
        }),
    ));
    tokio::select! {
        _ = signal_rx.recv() => tracing::info!("signal received, draining connections"),
        _ = shutdown_rx.recv() => tracing::info!("shutdown requested via control socket, draining connections"),
    }
    let _ = drain_tx.send(());
    match tokio::time::timeout(Duration::from_secs(5), serve).await {
        Ok(Ok(Ok(()))) => {}
        Ok(Ok(Err(error))) => tracing::error!(%error, "http server error"),
        Ok(Err(error)) => tracing::error!(%error, "http server task failed"),
        Err(_) => tracing::warn!("drain timeout reached"),
    }
    if let Some(path) = control_socket_path {
        if let Err(error) = std::fs::remove_file(&path) {
            if error.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!(socket_path = %path, %error, "failed to remove control socket");
            }
        }
    }
    tracing::info!("tandem server stopped");
    Ok(())
}

impl From<jj_tandem_repository::BucketStatus> for control::BucketStatus {
    fn from(value: jj_tandem_repository::BucketStatus) -> Self {
        Self {
            backend: value.backend,
            location: value.location,
            conditional_put: value.conditional_put,
            materialized: value.materialized,
            replayed_heads: value.replayed_heads,
            replayed_entries: value.replayed_entries,
            replay_ms: value.replay_ms,
        }
    }
}

#[cfg(test)]
mod admission_tests {
    use super::*;

    #[tokio::test]
    async fn decoded_body_admission_is_bounded_to_four() {
        let admission = Arc::new(tokio::sync::Semaphore::new(MAX_DECODED_BODIES));
        let held: Vec<_> = (0..MAX_DECODED_BODIES)
            .map(|_| admission.clone().try_acquire_owned().unwrap())
            .collect();
        assert!(admission.clone().try_acquire_owned().is_err());
        drop(held);
        assert!(admission.try_acquire_owned().is_ok());
    }

    #[tokio::test]
    async fn publish_queue_refuses_the_ninth_waiter_and_recovers() {
        let admission = Arc::new(PublishAdmission::new(Arc::new(
            tokio::sync::Semaphore::new(MAX_ACTIVE_PUBLISHES),
        )));
        let active = admission.acquire().await.unwrap();
        let mut waiters = Vec::new();
        for _ in 0..MAX_QUEUED_PUBLISHES_PER_REPOSITORY {
            let admission = admission.clone();
            waiters.push(tokio::spawn(async move { admission.acquire().await }));
        }
        tokio::time::timeout(Duration::from_secs(1), async {
            while admission.queued.load(Ordering::Acquire) != MAX_QUEUED_PUBLISHES_PER_REPOSITORY {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("all eight waiters should enter the bounded queue");
        assert!(admission
            .acquire()
            .await
            .unwrap_err()
            .to_string()
            .contains("queue is full"));
        drop(active);
        for waiter in waiters {
            drop(waiter.await.unwrap().unwrap());
        }
        assert_eq!(admission.queued.load(Ordering::Acquire), 0);
        drop(admission.acquire().await.unwrap());
    }

    #[tokio::test]
    async fn cancelled_publish_waiters_release_queue_capacity() {
        let admission = Arc::new(PublishAdmission::new(Arc::new(
            tokio::sync::Semaphore::new(MAX_ACTIVE_PUBLISHES),
        )));
        let active = admission.acquire().await.unwrap();
        let mut waiters = Vec::new();
        for _ in 0..MAX_QUEUED_PUBLISHES_PER_REPOSITORY {
            let admission = admission.clone();
            waiters.push(tokio::spawn(async move { admission.acquire().await }));
        }
        tokio::time::timeout(Duration::from_secs(1), async {
            while admission.queued.load(Ordering::Acquire) != MAX_QUEUED_PUBLISHES_PER_REPOSITORY {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        for waiter in waiters {
            waiter.abort();
            let _ = waiter.await;
        }
        assert_eq!(admission.queued.load(Ordering::Acquire), 0);
        drop(active);
        drop(admission.acquire().await.unwrap());
    }

    #[tokio::test]
    async fn cancelled_handler_does_not_release_an_active_publish() {
        let admission = Arc::new(PublishAdmission::new(Arc::new(
            tokio::sync::Semaphore::new(MAX_ACTIVE_PUBLISHES),
        )));
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let task_admission = admission.clone();
        let handler = tokio::spawn(async move {
            let permit = task_admission.acquire().await.unwrap();
            tokio::task::spawn_blocking(move || {
                let _permit = permit;
                entered_tx.send(()).unwrap();
                release_rx.recv().unwrap();
            })
            .await
            .unwrap();
        });
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                match entered_rx.try_recv() {
                    Ok(()) => break,
                    Err(std::sync::mpsc::TryRecvError::Empty) => tokio::task::yield_now().await,
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        panic!("blocking publish ended before entering")
                    }
                }
            }
        })
        .await
        .unwrap();
        handler.abort();
        let _ = handler.await;
        assert!(admission.repository.clone().try_acquire_owned().is_err());
        release_tx.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(1), admission.acquire())
            .await
            .expect("finished blocking work must return admission")
            .unwrap();
    }

    #[tokio::test]
    async fn one_slow_repository_leaves_publish_capacity_for_another() {
        let host = Arc::new(tokio::sync::Semaphore::new(MAX_ACTIVE_PUBLISHES));
        let slow = PublishAdmission::new(host.clone());
        let unrelated = PublishAdmission::new(host);
        let _held = slow.acquire().await.unwrap();
        let other = tokio::time::timeout(Duration::from_secs(1), unrelated.acquire())
            .await
            .expect("an unrelated repository should not wait for the slow one")
            .unwrap();
        assert_eq!(other.queue_depth, 0);
    }
}
