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
}

impl Server {
    pub fn new(repo: PathBuf, bucket: Option<&str>, admin_token: &str) -> Result<Self> {
        let settings = user_settings()?;
        Ok(Self::from_repository(
            Repository::new(&settings, repo, bucket)?,
            admin_token,
        ))
    }

    pub(crate) fn new_with_faults(
        repo: PathBuf,
        bucket: Option<&str>,
        admin_token: &str,
        faults: Arc<jj_tandem_repository::FaultPoints>,
    ) -> Result<Self> {
        let settings = user_settings()?;
        Ok(Self::from_repository(
            jj_tandem_repository::Repository::new_with_faults(&settings, repo, bucket, faults)?,
            admin_token,
        ))
    }

    pub fn from_repository(repository: Repository, admin_token: &str) -> Self {
        Self {
            repository: Arc::new(repository),
            tokens: auth::TokenStore::new(admin_token),
            writer_roles: writer::WriterRoles::new(),
        }
    }

    pub(crate) fn durably_initialize(&self) -> Result<()> {
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
