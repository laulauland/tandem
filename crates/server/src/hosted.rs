use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};

use anyhow::{bail, Context, Result};
use axum::body::Body;
use axum::extract::{Path, Request, State};
use axum::http::{header, HeaderValue, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use blake2::{Blake2b512, Digest as _};
use jj_tandem_protocol::names::RepositoryName;
use jj_tandem_storage::{CasError, ObjectStore};
use rand::TryRngCore as _;
use serde::{Deserialize, Serialize};
use tower::ServiceExt as _;
use tracing::Instrument as _;

use crate::Server;

const CATALOG_PREFIX: &str = "_hosting/namespaces";

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct NamespaceRecord {
    owner_fingerprint: String,
    repositories: BTreeMap<String, RepositoryState>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
enum RepositoryState {
    Provisioning,
    Ready,
}

#[derive(Serialize, Deserialize)]
struct OwnerBody {
    token: String,
}

pub struct HostedServer {
    cache_root: PathBuf,
    bucket_spec: String,
    bucket: Arc<dyn ObjectStore>,
    signing_keys: SigningKeys,
    repositories: LoadingSlots<Server>,
    ready_owners: Mutex<HashMap<String, String>>,
    catalog_reads: AtomicU64,
    catalog_bytes: AtomicU64,
    open_limit: OpenLimit,
    distribution_dir: PathBuf,
    public_url: String,
    faults: Arc<jj_tandem_repository::FaultPoints>,
    staging_budget: Arc<jj_tandem_repository::StagingBudget>,
    body_admission: Arc<tokio::sync::Semaphore>,
    control_body_admission: Arc<tokio::sync::Semaphore>,
    publish_admission: Arc<tokio::sync::Semaphore>,
}

#[derive(Clone)]
pub(crate) struct SigningKeys {
    active: String,
    retained: Vec<String>,
}

impl SigningKeys {
    pub(crate) fn parse(active: &str, retained: Option<&str>) -> Result<Self> {
        anyhow::ensure!(!active.trim().is_empty(), "active signing key is empty");
        let active = active.trim().to_string();
        let mut keys = Vec::new();
        if let Some(retained) = retained {
            for raw in retained.split(',') {
                let key = raw.trim();
                anyhow::ensure!(!key.is_empty(), "retained signing key is empty");
                anyhow::ensure!(key != active, "active signing key is also retained");
                anyhow::ensure!(
                    !keys.iter().any(|existing| existing == key),
                    "retained signing key is duplicated"
                );
                keys.push(key.to_string());
            }
        }
        Ok(Self {
            active,
            retained: keys,
        })
    }
}

impl HostedServer {
    #[cfg(test)]
    fn new(cache_root: PathBuf, bucket_spec: &str, host_secret: &str) -> Result<Self> {
        let signing_keys = SigningKeys::parse(host_secret, None)?;
        Self::new_with_signing_keys(cache_root, bucket_spec, signing_keys)
    }

    pub(crate) fn new_with_signing_keys(
        cache_root: PathBuf,
        bucket_spec: &str,
        signing_keys: SigningKeys,
    ) -> Result<Self> {
        Self::new_with_faults_and_keys(
            cache_root,
            bucket_spec,
            signing_keys,
            jj_tandem_repository::FaultPoints::from_environment(),
        )
    }

    #[cfg(test)]
    fn new_with_faults(
        cache_root: PathBuf,
        bucket_spec: &str,
        host_secret: &str,
        faults: Arc<jj_tandem_repository::FaultPoints>,
    ) -> Result<Self> {
        Self::new_with_faults_and_keys(
            cache_root,
            bucket_spec,
            SigningKeys::parse(host_secret, None)?,
            faults,
        )
    }

    fn new_with_faults_and_keys(
        cache_root: PathBuf,
        bucket_spec: &str,
        signing_keys: SigningKeys,
        faults: Arc<jj_tandem_repository::FaultPoints>,
    ) -> Result<Self> {
        let bucket = jj_tandem_storage::open(bucket_spec).context("open hosted bucket")?;
        if !jj_tandem_storage::probe_conditional_put(bucket.as_ref())? {
            bail!("hosted repositories require a bucket with conditional puts");
        }
        let public_url = std::env::var("TANDEM_PUBLIC_URL")
            .unwrap_or_else(|_| "https://tandem.land".to_string());
        let public_url = validate_public_url(&public_url)?;
        Ok(Self {
            cache_root,
            bucket_spec: bucket_spec.to_string(),
            bucket,
            signing_keys,
            repositories: LoadingSlots::new(),
            ready_owners: Mutex::new(HashMap::new()),
            catalog_reads: AtomicU64::new(0),
            catalog_bytes: AtomicU64::new(0),
            open_limit: OpenLimit::new(4),
            distribution_dir: std::env::var_os("TANDEM_DISTRIBUTION_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("/opt/tandem/releases")),
            public_url,
            faults,
            staging_budget: Arc::new(jj_tandem_repository::StagingBudget::default()),
            body_admission: Arc::new(tokio::sync::Semaphore::new(3)),
            control_body_admission: Arc::new(tokio::sync::Semaphore::new(1)),
            publish_admission: Arc::new(tokio::sync::Semaphore::new(4)),
        })
    }

    fn namespace(&self, namespace: &str) -> Result<Option<(NamespaceRecord, String)>> {
        let key = format!("{CATALOG_PREFIX}/{namespace}.json");
        let value = self.bucket.get_with_etag(&key)?;
        self.catalog_reads.fetch_add(1, Ordering::Relaxed);
        if let Some((bytes, _)) = &value {
            self.catalog_bytes
                .fetch_add(bytes.len() as u64, Ordering::Relaxed);
            tracing::debug!(catalog_bucket_calls = 1, catalog_bucket_bytes = bytes.len(), catalog = %key, "hosted catalog read");
        } else {
            tracing::debug!(catalog_bucket_calls = 1, catalog_bucket_bytes = 0, catalog = %key, "hosted catalog read");
        }
        value
            .map(|(bytes, etag)| {
                let record = serde_json::from_slice(&bytes).context("decode namespace catalog")?;
                Ok((record, etag))
            })
            .transpose()
    }

    fn repository(
        &self,
        name: &RepositoryName,
        record: &NamespaceRecord,
        recover_incomplete: bool,
    ) -> Result<Arc<Server>> {
        let name_text = name.path();
        self.repositories.load(name_text.clone(), || {
            let _permit = self.open_limit.acquire()?;
            let (signing_key, retained_signing_keys) =
                self.repository_signing_keys(&record.owner_fingerprint, &name_text);
            let cache = self
                .cache_root
                .join("repositories")
                .join(name.namespace())
                .join(name.repository());
            if cache.exists() && recover_incomplete {
                std::fs::remove_dir_all(&cache).context("discard incomplete repository cache")?;
            }
            let bucket =
                repository_bucket_spec(&self.bucket_spec, name.namespace(), name.repository());
            let cache_existed = cache.exists();
            let open = || -> Result<Arc<Server>> {
                let server = Arc::new(Server::new_with_signing_keys_and_budget(
                    cache.clone(),
                    Some(&bucket),
                    &signing_key,
                    retained_signing_keys.clone(),
                    self.faults.clone(),
                    self.staging_budget.clone(),
                    self.body_admission.clone(),
                    self.control_body_admission.clone(),
                    self.publish_admission.clone(),
                )?);
                server.durably_initialize()?;
                Ok(server)
            };
            match open() {
                Ok(server) => Ok(server),
                Err(error) if cache_existed && !recover_incomplete => {
                    tracing::warn!(
                        %error,
                        repository = %name_text,
                        "discarding unusable repository cache and reconstructing from bucket"
                    );
                    std::fs::remove_dir_all(&cache).context("discard unusable repository cache")?;
                    open().context("reconstruct repository cache from bucket")
                }
                Err(error) => Err(error),
            }
        })
    }

    fn derive_repository_signing_key(host_key: &str, owner: &str, name: &str) -> String {
        let mut hash = Blake2b512::new();
        hash.update(host_key.as_bytes());
        hash.update([0]);
        hash.update(owner.as_bytes());
        hash.update([0]);
        hash.update(name.as_bytes());
        format!("tdma_{}", hex(&hash.finalize()))
    }

    fn repository_signing_keys(&self, owner: &str, name: &str) -> (String, Vec<String>) {
        (
            Self::derive_repository_signing_key(&self.signing_keys.active, owner, name),
            self.signing_keys
                .retained
                .iter()
                .map(|key| Self::derive_repository_signing_key(key, owner, name))
                .collect(),
        )
    }

    fn remember_ready(&self, name: &RepositoryName, owner_fingerprint: &str) -> Result<()> {
        self.ready_owners
            .lock()
            .map_err(|error| anyhow::anyhow!("ready repository registry lock: {error}"))?
            .insert(name.path(), owner_fingerprint.to_string());
        Ok(())
    }

    fn warm_repository(&self, name: &RepositoryName) -> Result<Option<(String, Arc<Server>)>> {
        let name_text = name.path();
        let owner = self
            .ready_owners
            .lock()
            .map_err(|error| anyhow::anyhow!("ready repository registry lock: {error}"))?
            .get(&name_text)
            .cloned();
        Ok(owner.zip(self.repositories.ready(&name_text)?))
    }

    fn owner_token(&self, entropy: &[u8; 32]) -> String {
        let body = hex(entropy);
        let mut hash = Blake2b512::new();
        hash.update(self.signing_keys.active.as_bytes());
        hash.update([0]);
        hash.update(body.as_bytes());
        format!("tdmo_{body}_{}", hex(&hash.finalize()[..32]))
    }

    fn verifies_owner_token(&self, token: &str) -> bool {
        let Some(body) = token.strip_prefix("tdmo_") else {
            return false;
        };
        let Some((entropy, presented_tag)) = body.split_once('_') else {
            return false;
        };
        if entropy.len() != 64
            || presented_tag.len() != 64
            || !entropy
                .bytes()
                .chain(presented_tag.bytes())
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        {
            return false;
        }
        std::iter::once(&self.signing_keys.active)
            .chain(self.signing_keys.retained.iter())
            .any(|key| {
                let mut hash = Blake2b512::new();
                hash.update(key.as_bytes());
                hash.update([0]);
                hash.update(entropy.as_bytes());
                constant_time_eq(
                    presented_tag.as_bytes(),
                    hex(&hash.finalize()[..32]).as_bytes(),
                )
            })
    }
}

struct LoadingSlots<T> {
    entries: Mutex<HashMap<String, Arc<LoadSlot<T>>>>,
}

const MAX_RESIDENT_REPOSITORIES: usize = 16;

#[derive(Debug)]
struct RepositoryCapacity;

impl std::fmt::Display for RepositoryCapacity {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "host repository capacity is full")
    }
}

impl std::error::Error for RepositoryCapacity {}

struct LoadSlot<T> {
    state: Mutex<LoadState<T>>,
    changed: Condvar,
}

enum LoadState<T> {
    Loading { waiters: usize },
    Ready(Arc<T>),
    Failed(Arc<str>),
}

impl<T> LoadingSlots<T> {
    fn new() -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
        }
    }

    fn load(&self, name: String, open: impl FnOnce() -> Result<Arc<T>>) -> Result<Arc<T>> {
        let (slot, leader) = {
            let mut entries = self
                .entries
                .lock()
                .map_err(|error| anyhow::anyhow!("repository registry lock: {error}"))?;
            match entries.get(&name) {
                Some(slot) => (slot.clone(), false),
                None => {
                    if entries.len() >= MAX_RESIDENT_REPOSITORIES {
                        return Err(RepositoryCapacity.into());
                    }
                    let slot = Arc::new(LoadSlot {
                        state: Mutex::new(LoadState::Loading { waiters: 0 }),
                        changed: Condvar::new(),
                    });
                    entries.insert(name.clone(), slot.clone());
                    (slot, true)
                }
            }
        };
        if leader {
            let opened = std::panic::catch_unwind(std::panic::AssertUnwindSafe(open))
                .map_err(|_| anyhow::anyhow!("repository loader panicked"))
                .and_then(|result| result);
            match opened {
                Ok(value) => {
                    *slot
                        .state
                        .lock()
                        .map_err(|error| anyhow::anyhow!("repository load slot lock: {error}"))? =
                        LoadState::Ready(value.clone());
                    slot.changed.notify_all();
                    Ok(value)
                }
                Err(error) => {
                    let message: Arc<str> = error.to_string().into();
                    *slot
                        .state
                        .lock()
                        .map_err(|error| anyhow::anyhow!("repository load slot lock: {error}"))? =
                        LoadState::Failed(message.clone());
                    slot.changed.notify_all();
                    let mut entries = self
                        .entries
                        .lock()
                        .map_err(|error| anyhow::anyhow!("repository registry lock: {error}"))?;
                    if entries
                        .get(&name)
                        .is_some_and(|current| Arc::ptr_eq(current, &slot))
                    {
                        entries.remove(&name);
                    }
                    Err(anyhow::anyhow!(message.to_string()))
                }
            }
        } else {
            let mut state = slot
                .state
                .lock()
                .map_err(|error| anyhow::anyhow!("repository load slot lock: {error}"))?;
            let mut registered = false;
            loop {
                match &mut *state {
                    LoadState::Loading { waiters } => {
                        if !registered {
                            *waiters += 1;
                            registered = true;
                            slot.changed.notify_all();
                        }
                        state = slot.changed.wait(state).map_err(|error| {
                            anyhow::anyhow!("repository load slot wait: {error}")
                        })?;
                    }
                    LoadState::Ready(value) => return Ok(value.clone()),
                    LoadState::Failed(message) => return Err(anyhow::anyhow!(message.to_string())),
                }
            }
        }
    }

    fn ready(&self, name: &str) -> Result<Option<Arc<T>>> {
        let slot = self
            .entries
            .lock()
            .map_err(|error| anyhow::anyhow!("repository registry lock: {error}"))?
            .get(name)
            .cloned();
        let Some(slot) = slot else { return Ok(None) };
        let state = slot
            .state
            .lock()
            .map_err(|error| anyhow::anyhow!("repository load slot lock: {error}"))?;
        Ok(match &*state {
            LoadState::Ready(value) => Some(value.clone()),
            LoadState::Loading { .. } | LoadState::Failed(_) => None,
        })
    }

    fn is_capacity_error(error: &anyhow::Error) -> bool {
        error.downcast_ref::<RepositoryCapacity>().is_some()
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.entries.lock().unwrap().len()
    }

    #[cfg(test)]
    fn wait_for_waiter(&self, name: &str) {
        let slot = self.entries.lock().unwrap().get(name).unwrap().clone();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
        let mut state = slot.state.lock().unwrap();
        loop {
            if matches!(&*state, LoadState::Loading { waiters } if *waiters > 0) {
                return;
            }
            let remaining = deadline
                .checked_duration_since(std::time::Instant::now())
                .expect("same-name caller did not enter the load wait set");
            let (next, timeout) = slot.changed.wait_timeout(state, remaining).unwrap();
            state = next;
            assert!(
                !timeout.timed_out(),
                "same-name load waiter was not established"
            );
        }
    }
}

struct OpenLimit {
    available: Mutex<usize>,
    changed: Condvar,
}

impl OpenLimit {
    fn new(maximum: usize) -> Self {
        Self {
            available: Mutex::new(maximum),
            changed: Condvar::new(),
        }
    }

    fn acquire(&self) -> Result<OpenPermit<'_>> {
        let mut available = self
            .available
            .lock()
            .map_err(|error| anyhow::anyhow!("repository open limit lock: {error}"))?;
        while *available == 0 {
            available = self
                .changed
                .wait(available)
                .map_err(|error| anyhow::anyhow!("repository open limit wait: {error}"))?;
        }
        *available -= 1;
        Ok(OpenPermit { limit: self })
    }
}

struct OpenPermit<'a> {
    limit: &'a OpenLimit,
}

impl Drop for OpenPermit<'_> {
    fn drop(&mut self) {
        if let Ok(mut available) = self.limit.available.lock() {
            *available += 1;
            self.limit.changed.notify_one();
        }
    }
}

pub fn router(server: Arc<HostedServer>) -> Router {
    Router::new()
        .route("/", get(homepage))
        .route("/architecture", get(architecture))
        .route("/site.css", get(site_css))
        .route("/healthz", get(health))
        .route("/install", get(installer))
        .route("/install.sh", get(installer))
        .route("/install/token", post(create_public_owner))
        .route("/install/token/verify", post(verify_public_owner))
        .route("/dl/{artifact}", get(download))
        .route("/api/owners", post(create_owner))
        .route("/{namespace}/{repository}", put(create_repository))
        .fallback(dispatch_repository)
        .with_state(server)
}

async fn homepage(State(server): State<Arc<HostedServer>>) -> Response {
    (
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        render_html_asset(include_str!("../assets/homepage.html"), &server.public_url),
    )
        .into_response()
}

async fn architecture(State(server): State<Arc<HostedServer>>) -> Response {
    (
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        render_html_asset(
            include_str!("../assets/architecture.html"),
            &server.public_url,
        ),
    )
        .into_response()
}

async fn site_css() -> Response {
    (
        [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
        include_str!("../assets/site.css"),
    )
        .into_response()
}

async fn health() -> &'static str {
    "ok\n"
}

async fn installer(State(server): State<Arc<HostedServer>>) -> Response {
    (
        [(header::CONTENT_TYPE, "text/x-shellscript; charset=utf-8")],
        render_shell_asset(include_str!("../assets/install.sh"), &server.public_url),
    )
        .into_response()
}

fn render_html_asset(asset: &str, public_url: &str) -> String {
    asset.replace("@@TANDEM_PUBLIC_URL@@", &escape_html(public_url))
}

fn render_shell_asset(asset: &str, public_url: &str) -> String {
    asset.replace(
        "@@TANDEM_PUBLIC_URL@@",
        &public_url
            .replace('\\', "\\\\")
            .replace('$', "\\$")
            .replace('`', "\\`")
            .replace('"', "\\\""),
    )
}

fn escape_html(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

fn validate_public_url(value: &str) -> Result<String> {
    if !value.chars().all(|character| {
        character.is_ascii_alphanumeric() || matches!(character, '.' | '-' | ':' | '/' | '[' | ']')
    }) {
        bail!("TANDEM_PUBLIC_URL contains a character that is not valid in a public origin");
    }
    let parsed = url::Url::parse(value).context("parse TANDEM_PUBLIC_URL")?;
    if !matches!(parsed.scheme(), "http" | "https")
        || parsed.host().is_none()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
        || parsed.path() != "/"
    {
        bail!("TANDEM_PUBLIC_URL must be an HTTP(S) origin without userinfo, path or query");
    }
    Ok(parsed.origin().ascii_serialization())
}

async fn create_public_owner(State(server): State<Arc<HostedServer>>) -> Response {
    owner_response(&server)
}

async fn verify_public_owner(
    State(server): State<Arc<HostedServer>>,
    request: Request,
) -> Response {
    match bearer(&request).filter(|token| server.verifies_owner_token(token)) {
        Some(_) => StatusCode::NO_CONTENT.into_response(),
        None => StatusCode::UNAUTHORIZED.into_response(),
    }
}

async fn download(
    State(server): State<Arc<HostedServer>>,
    Path(artifact): Path<String>,
) -> Response {
    if !matches!(
        artifact.as_str(),
        "td-x86_64-unknown-linux-gnu" | "td-aarch64-apple-darwin"
    ) {
        return StatusCode::NOT_FOUND.into_response();
    }
    match tokio::fs::read(server.distribution_dir.join(&artifact)).await {
        Ok(bytes) => ([(header::CONTENT_TYPE, "application/octet-stream")], bytes).into_response(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            StatusCode::NOT_FOUND.into_response()
        }
        Err(error) => {
            tracing::error!(%error, artifact, "distribution artifact read failed");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

async fn create_owner(State(server): State<Arc<HostedServer>>, request: Request) -> Response {
    if bearer(&request) != Some(server.signing_keys.active.as_str()) {
        return StatusCode::NOT_FOUND.into_response();
    }
    owner_response(&server)
}

fn owner_response(server: &HostedServer) -> Response {
    let mut entropy = [0u8; 32];
    if rand::rngs::OsRng.try_fill_bytes(&mut entropy).is_err() {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    (
        StatusCode::CREATED,
        Json(OwnerBody {
            token: server.owner_token(&entropy),
        }),
    )
        .into_response()
}

async fn create_repository(
    State(server): State<Arc<HostedServer>>,
    Path((namespace, repository)): Path<(String, String)>,
    request: Request,
) -> Response {
    let Ok(name) = RepositoryName::parse_hosted(&namespace, &repository) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    if request.uri().path() != format!("/{}", name.path()) {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let Some(token) = bearer(&request).map(str::to_string) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if !server.verifies_owner_token(&token) {
        return StatusCode::NOT_FOUND.into_response();
    }
    tokio::task::spawn_blocking(move || create_repository_parsed(&server, name, token, || {}))
        .await
        .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR)
        .into_response()
}

#[cfg(test)]
fn create_repository_sync(
    server: &HostedServer,
    namespace: String,
    repository: String,
    token: String,
) -> StatusCode {
    create_repository_with_hook(server, namespace, repository, token, || {})
}

#[cfg(test)]
fn create_repository_with_hook(
    server: &HostedServer,
    namespace: String,
    repository: String,
    token: String,
    after_provisioning: impl FnMut(),
) -> StatusCode {
    let name = match RepositoryName::parse_hosted(&namespace, &repository) {
        Ok(name) => name,
        Err(_) => return StatusCode::BAD_REQUEST,
    };
    create_repository_parsed(server, name, token, after_provisioning)
}

fn create_repository_parsed(
    server: &HostedServer,
    name: RepositoryName,
    token: String,
    mut after_provisioning: impl FnMut(),
) -> StatusCode {
    let namespace = name.namespace().to_string();
    let repository = name.repository().to_string();
    let fingerprint = fingerprint(&token);
    let key = format!("{CATALOG_PREFIX}/{namespace}.json");
    for _ in 0..8 {
        let current = match server.namespace(&namespace) {
            Ok(value) => value,
            Err(error) => {
                tracing::error!(%error, namespace, repository, "namespace catalog read failed");
                return StatusCode::INTERNAL_SERVER_ERROR;
            }
        };
        let (mut record, etag, created) = match current {
            Some((record, etag)) if record.owner_fingerprint == fingerprint => {
                (record, Some(etag), false)
            }
            Some(_) => return StatusCode::NOT_FOUND,
            None => (
                NamespaceRecord {
                    owner_fingerprint: fingerprint.clone(),
                    repositories: BTreeMap::new(),
                },
                None,
                true,
            ),
        };
        let repository_created = !record.repositories.contains_key(&repository);
        record
            .repositories
            .entry(repository.clone())
            .or_insert(RepositoryState::Provisioning);
        let bytes = match serde_json::to_vec(&record) {
            Ok(value) => value,
            Err(_) => return StatusCode::INTERNAL_SERVER_ERROR,
        };
        match server.bucket.compare_and_put(&key, &bytes, etag.as_deref()) {
            Ok(provisioning_etag) => {
                if let Err(error) = server.repository(&name, &record, true) {
                    tracing::error!(%error, namespace, repository, "repository provisioning failed");
                    return if LoadingSlots::<Server>::is_capacity_error(&error) {
                        StatusCode::SERVICE_UNAVAILABLE
                    } else {
                        StatusCode::INTERNAL_SERVER_ERROR
                    };
                }
                after_provisioning();
                record
                    .repositories
                    .insert(repository.clone(), RepositoryState::Ready);
                let ready = serde_json::to_vec(&record).unwrap();
                match server
                    .bucket
                    .compare_and_put(&key, &ready, Some(&provisioning_etag))
                {
                    Ok(_) => {}
                    Err(CasError::Conflict) => continue,
                    Err(CasError::Other(error)) => {
                        tracing::error!(%error, namespace, repository, "repository ready commit failed");
                        return StatusCode::INTERNAL_SERVER_ERROR;
                    }
                }
                if let Err(error) = server.remember_ready(&name, &record.owner_fingerprint) {
                    tracing::error!(%error, namespace, repository, "repository ready cache failed");
                    return StatusCode::INTERNAL_SERVER_ERROR;
                }
                return if created || repository_created {
                    StatusCode::CREATED
                } else {
                    StatusCode::OK
                };
            }
            Err(CasError::Conflict) => continue,
            Err(CasError::Other(error)) => {
                tracing::error!(%error, namespace, repository, "repository provisioning catalog commit failed");
                return StatusCode::INTERNAL_SERVER_ERROR;
            }
        }
    }
    StatusCode::CONFLICT
}

async fn dispatch_repository(
    State(hosted): State<Arc<HostedServer>>,
    mut request: Request,
) -> Response {
    let profile_host_started = std::time::Instant::now();
    let path = request.uri().path().trim_start_matches('/').to_string();
    let mut parts = path.splitn(3, '/');
    let (Some(namespace), Some(repository), Some(rest)) =
        (parts.next(), parts.next(), parts.next())
    else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let Ok(name) = RepositoryName::parse(namespace, repository) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let repository_identity = name.path();
    let namespace = name.namespace().to_string();
    let repository = name.repository().to_string();
    let loaded = tokio::task::spawn_blocking({
        let hosted = hosted.clone();
        let namespace = namespace.clone();
        let repository = repository.clone();
        move || {
            if let Some((owner, server)) = hosted.warm_repository(&name)? {
                return Ok(Some((owner, server)));
            }
            let Some((record, _)) = hosted.namespace(&namespace)? else {
                return Ok(None);
            };
            if record.repositories.get(&repository) != Some(&RepositoryState::Ready) {
                return Ok(None);
            }
            let server = hosted.repository(&name, &record, false)?;
            hosted.remember_ready(&name, &record.owner_fingerprint)?;
            Ok::<_, anyhow::Error>(Some((record.owner_fingerprint, server)))
        }
    })
    .await;
    let (owner_fingerprint, server) = match loaded {
        Ok(Ok(Some(value))) => value,
        Ok(Ok(None)) => return StatusCode::NOT_FOUND.into_response(),
        Ok(Err(error)) => {
            tracing::error!(%error, namespace, repository, "hosted repository load failed");
            return if LoadingSlots::<Server>::is_capacity_error(&error) {
                StatusCode::SERVICE_UNAVAILABLE.into_response()
            } else {
                StatusCode::INTERNAL_SERVER_ERROR.into_response()
            };
        }
        Err(error) => {
            tracing::error!(%error, namespace, repository, "hosted repository task failed");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    let Some(presented) = bearer(&request).map(str::to_string) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if presented.starts_with("tdmo_") {
        if !hosted.verifies_owner_token(&presented) {
            return StatusCode::NOT_FOUND.into_response();
        }
        if fingerprint(&presented) != owner_fingerprint {
            return StatusCode::NOT_FOUND.into_response();
        }
        let (key, _) = hosted
            .repository_signing_keys(&owner_fingerprint, &format!("{namespace}/{repository}"));
        request.headers_mut().insert(
            header::AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {key}")).unwrap(),
        );
    }
    let query = request
        .uri()
        .query()
        .map(|q| format!("?{q}"))
        .unwrap_or_default();
    tracing::debug!(
        repository = %repository_identity,
        http_method = %request.method(),
        request_path = %format!("/{rest}"),
        "hosted request"
    );
    *request.uri_mut() = format!("/{rest}{query}").parse::<Uri>().unwrap();
    let profile_dispatch_us = profile_host_started.elapsed().as_micros() as u64;
    let profile_method = request.method().to_string();
    let profile_request_bytes = request
        .headers()
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(0);
    let response = crate::http::router(server)
        .oneshot(request.map(Body::new))
        .instrument(
            tracing::info_span!("hosted repository request", repository = %repository_identity),
        )
        .await
        .unwrap_or_else(|never| match never {});
    use axum::body::HttpBody as _;
    tracing::debug!(repository = %repository_identity, request_path = %format!("/{rest}"),
        http_method = %profile_method, request_bytes = profile_request_bytes,
        response_bytes = response.body().size_hint().exact(), status = response.status().as_u16(),
        profile_dispatch_us,
        profile_host_request_us = profile_host_started.elapsed().as_micros() as u64, "host HTTP response ready");
    response
}

fn bearer(request: &Request) -> Option<&str> {
    request
        .headers()
        .get(header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
}
fn fingerprint(token: &str) -> String {
    hex(&Blake2b512::digest(token.as_bytes()))
}
fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    left.len() == right.len()
        && left
            .iter()
            .zip(right)
            .fold(0u8, |difference, (left, right)| difference | (left ^ right))
            == 0
}
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}
fn repository_bucket_spec(root: &str, namespace: &str, repository: &str) -> String {
    let suffix = format!("repositories/{namespace}/{repository}");
    if let Some((base, query)) = root.split_once('?') {
        format!("{}/{suffix}?{query}", base.trim_end_matches('/'))
    } else {
        format!("{}/{suffix}", root.trim_end_matches('/'))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;
    use std::sync::Barrier;

    fn host() -> (tempfile::TempDir, Arc<HostedServer>) {
        let temporary = tempfile::tempdir().unwrap();
        let cache = temporary.path().join("cache");
        let bucket = temporary
            .path()
            .join("bucket")
            .to_string_lossy()
            .to_string();
        let host = Arc::new(HostedServer::new(cache, &bucket, "host-secret").unwrap());
        (temporary, host)
    }

    #[test]
    fn rotation_preserves_owner_identity_and_old_scopes_but_uses_the_active_repo_key() {
        let temporary = tempfile::tempdir().unwrap();
        let bucket = temporary
            .path()
            .join("bucket")
            .to_string_lossy()
            .to_string();
        let old = HostedServer::new_with_faults_and_keys(
            temporary.path().join("old-cache"),
            &bucket,
            SigningKeys::parse("key-one", None).unwrap(),
            jj_tandem_repository::FaultPoints::inert(),
        )
        .unwrap();
        let owner = old.owner_token(&[17; 32]);
        let owner_fingerprint = fingerprint(&owner);
        assert!(
            create_repository_sync(&old, "owner".into(), "repo".into(), owner.clone()).is_success()
        );
        let name = RepositoryName::parse("owner", "repo").unwrap();
        let record = old.namespace("owner").unwrap().unwrap().0;
        let old_server = old.repository(&name, &record, false).unwrap();
        let old_scope = old_server
            .mint_token_sync("agent-a", std::time::Duration::from_secs(60))
            .token;
        drop(old_server);
        drop(old);

        let rotated = HostedServer::new_with_faults_and_keys(
            temporary.path().join("new-cache"),
            &bucket,
            SigningKeys::parse("key-two", Some("key-one")).unwrap(),
            jj_tandem_repository::FaultPoints::inert(),
        )
        .unwrap();
        assert!(rotated.verifies_owner_token(&owner));
        let record = rotated.namespace("owner").unwrap().unwrap().0;
        assert_eq!(record.owner_fingerprint, owner_fingerprint);
        let server = rotated.repository(&name, &record, false).unwrap();
        assert_eq!(
            server.authority_for(&old_scope),
            Some(crate::auth::Authority::Workspace("agent-a".into()))
        );
        let (active_repo_key, retained_repo_keys) =
            rotated.repository_signing_keys(&owner_fingerprint, "owner/repo");
        assert_eq!(
            server.authority_for(&active_repo_key),
            Some(crate::auth::Authority::Admin)
        );
        assert_eq!(server.authority_for(&retained_repo_keys[0]), None);
        let new_scope = server
            .mint_token_sync("agent-a", std::time::Duration::from_secs(60))
            .token;
        assert_eq!(
            crate::auth::TokenStore::new(&retained_repo_keys[0]).authority_for(&new_scope),
            None
        );
    }

    #[test]
    fn retained_signing_key_configuration_fails_closed() {
        assert!(SigningKeys::parse("active", Some("")).is_err());
        assert!(SigningKeys::parse("active", Some("active")).is_err());
        assert!(SigningKeys::parse("active", Some("old,old")).is_err());
        assert_eq!(
            SigningKeys::parse(" active ", Some(" old-one, old-two "))
                .unwrap()
                .retained,
            vec!["old-one", "old-two"]
        );
    }

    #[tokio::test]
    async fn public_install_surface_serves_native_bytes_and_a_signed_owner() {
        let temporary = tempfile::tempdir().unwrap();
        let bucket = temporary.path().join("bucket");
        let distribution = temporary.path().join("distribution");
        std::fs::create_dir_all(&distribution).unwrap();
        let artifact: &[u8] = b"native-gnu-binary\0\xff";
        std::fs::write(distribution.join("td-x86_64-unknown-linux-gnu"), artifact).unwrap();
        let mut server = HostedServer::new(
            temporary.path().join("cache"),
            &bucket.to_string_lossy(),
            "secret",
        )
        .unwrap();
        server.distribution_dir = distribution;
        server.public_url = "https://native.example".to_string();
        let server = Arc::new(server);

        let download = router(server.clone())
            .oneshot(
                Request::builder()
                    .uri("/dl/td-x86_64-unknown-linux-gnu")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(download.status(), StatusCode::OK);
        assert_eq!(
            to_bytes(download.into_body(), usize::MAX).await.unwrap(),
            artifact
        );

        let mac_artifact: &[u8] = b"native-apple-silicon-binary\0\xff";
        std::fs::write(
            server.distribution_dir.join("td-aarch64-apple-darwin"),
            mac_artifact,
        )
        .unwrap();
        let download = router(server.clone())
            .oneshot(
                Request::builder()
                    .uri("/dl/td-aarch64-apple-darwin")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(download.status(), StatusCode::OK);
        assert_eq!(
            to_bytes(download.into_body(), usize::MAX).await.unwrap(),
            mac_artifact
        );

        let install = router(server.clone())
            .oneshot(
                Request::builder()
                    .uri("/install.sh")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let install = to_bytes(install.into_body(), usize::MAX).await.unwrap();
        let install = std::str::from_utf8(&install).unwrap();
        assert!(install.contains("x86_64-unknown-linux-gnu"));
        assert!(!install.contains("linux-musl"));
        assert!(install.contains("https://native.example"));
        assert!(!install.contains("@@TANDEM_PUBLIC_URL@@"));

        let homepage = router(server.clone())
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        let homepage = to_bytes(homepage.into_body(), usize::MAX).await.unwrap();
        let homepage = std::str::from_utf8(&homepage).unwrap();
        assert!(homepage.contains("https://native.example/install"));
        assert!(!homepage.contains("--token"));

        let minted = router(server.clone())
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/install/token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(minted.status(), StatusCode::CREATED);
        let owner: OwnerBody =
            serde_json::from_slice(&to_bytes(minted.into_body(), usize::MAX).await.unwrap())
                .unwrap();
        assert!(server.verifies_owner_token(&owner.token));
        let created = router(server.clone())
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri("/public/repository")
                    .header(header::AUTHORIZATION, format!("Bearer {}", owner.token))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(created.status(), StatusCode::CREATED);
        let reads_after_creation = server.catalog_reads.load(Ordering::Relaxed);
        for _ in 0..2 {
            let info = router(server.clone())
                .oneshot(
                    Request::builder()
                        .uri("/public/repository/api/info")
                        .header(header::AUTHORIZATION, format!("Bearer {}", owner.token))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(info.status(), StatusCode::OK);
        }
        assert_eq!(
            server.catalog_reads.load(Ordering::Relaxed),
            reads_after_creation,
            "a durably validated warm repository must not reread its catalog per RPC"
        );

        let recovered = Arc::new(
            HostedServer::new(
                temporary.path().join("cold-cache"),
                &bucket.to_string_lossy(),
                "secret",
            )
            .unwrap(),
        );
        for expected_reads in [1, 1] {
            let info = router(recovered.clone())
                .oneshot(
                    Request::builder()
                        .uri("/public/repository/api/info")
                        .header(header::AUTHORIZATION, format!("Bearer {}", owner.token))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(info.status(), StatusCode::OK);
            assert_eq!(
                recovered.catalog_reads.load(Ordering::Relaxed),
                expected_reads,
                "cold recovery validates durable catalog once, then serves warm"
            );
        }
    }

    #[test]
    fn public_origin_is_an_explicit_origin_not_a_request_path() {
        assert_eq!(
            validate_public_url("https://native.example/").unwrap(),
            "https://native.example"
        );
        for invalid in [
            "native.example",
            "https://native.example/path",
            "https://native.example?query",
            "https://native.example bad",
            "https://user@native.example",
            "https://$(id)",
            "https://`id`",
            "https://native.example\"bad",
        ] {
            assert!(validate_public_url(invalid).is_err(), "{invalid}");
        }
        assert_eq!(
            render_shell_asset("\"@@TANDEM_PUBLIC_URL@@\"", "https://x/\"$(`"),
            "\"https://x/\\\"\\$(\\`\""
        );
        assert_eq!(
            render_html_asset("@@TANDEM_PUBLIC_URL@@", "<&\"'>"),
            "&lt;&amp;&quot;&#39;&gt;"
        );
    }

    #[test]
    fn simultaneous_cold_opens_share_one_engine() {
        let (_temporary, host) = host();
        let record = NamespaceRecord {
            owner_fingerprint: "owner".to_string(),
            repositories: BTreeMap::from([("repo".to_string(), RepositoryState::Ready)]),
        };
        let barrier = Arc::new(Barrier::new(3));
        let handles: Vec<_> = (0..2)
            .map(|_| {
                let host = host.clone();
                let record = record.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    let name = RepositoryName::parse("namespace", "repo").unwrap();
                    host.repository(&name, &record, false).unwrap()
                })
            })
            .collect();
        barrier.wait();
        let first = handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect::<Vec<_>>();
        assert!(Arc::ptr_eq(&first[0], &first[1]));
    }

    #[test]
    fn namespace_claim_race_has_one_owner() {
        let (_temporary, host) = host();
        let first = host.owner_token(&[1; 32]);
        let second = host.owner_token(&[2; 32]);
        let barrier = Arc::new(Barrier::new(3));
        let handles: Vec<_> = [first, second]
            .into_iter()
            .map(|token| {
                let host = host.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    create_repository_sync(
                        &host,
                        "namespace".to_string(),
                        "repo".to_string(),
                        token,
                    )
                })
            })
            .collect();
        barrier.wait();
        let statuses: Vec<_> = handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect();
        assert_eq!(
            statuses
                .iter()
                .filter(|status| **status == StatusCode::CREATED)
                .count(),
            1
        );
        assert_eq!(
            statuses
                .iter()
                .filter(|status| **status == StatusCode::NOT_FOUND)
                .count(),
            1
        );
    }

    #[test]
    fn corrupt_namespace_catalog_fails_closed() {
        let (_temporary, host) = host();
        host.bucket
            .compare_and_put("_hosting/namespaces/namespace.json", b"{not-json", None)
            .unwrap();
        let token = host.owner_token(&[6; 32]);
        assert_eq!(
            create_repository_sync(&host, "namespace".into(), "repo".into(), token),
            StatusCode::INTERNAL_SERVER_ERROR
        );
        assert_eq!(
            host.bucket
                .get("_hosting/namespaces/namespace.json")
                .unwrap()
                .unwrap(),
            b"{not-json"
        );
    }

    #[tokio::test]
    async fn encoded_repository_aliases_are_rejected_before_catalog_access() {
        let (_temporary, host) = host();
        let token = host.owner_token(&[7; 32]);
        for path in ["/namespace/repo%2ename", "/namespace%2frepo/name"] {
            let response = router(host.clone())
                .oneshot(
                    Request::builder()
                        .method("PUT")
                        .uri(path)
                        .header(header::AUTHORIZATION, format!("Bearer {token}"))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert!(
                response.status().is_client_error(),
                "encoded alias {path:?} reached repository creation"
            );
        }
        assert!(host.namespace("namespace").unwrap().is_none());
    }

    #[tokio::test]
    async fn hosted_content_policy_rejects_creation_before_catalog_access() {
        let (_temporary, host) = host();
        let token = host.owner_token(&[11; 32]);
        let response = router(host.clone())
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri("/namespace/f-u-c-k")
                    .header(header::AUTHORIZATION, format!("Bearer {token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert!(host.namespace("namespace").unwrap().is_none());
    }

    #[test]
    fn overlapping_same_owner_creates_open_one_engine() {
        let (_temporary, host) = host();
        let token = host.owner_token(&[8; 32]);
        let barrier = Arc::new(Barrier::new(3));
        let handles: Vec<_> = (0..2)
            .map(|_| {
                let host = host.clone();
                let token = token.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    create_repository_sync(
                        &host,
                        "namespace".to_string(),
                        "repo".to_string(),
                        token,
                    )
                })
            })
            .collect();
        barrier.wait();
        let statuses: Vec<_> = handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect();
        assert!(statuses.iter().all(StatusCode::is_success));
        assert_eq!(host.repositories.len(), 1);
    }

    #[test]
    fn a_slow_repository_load_does_not_block_another_name() {
        let slots = Arc::new(LoadingSlots::new());
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let slow_slots = slots.clone();
        let slow = std::thread::spawn(move || {
            slow_slots
                .load("slow/repo".into(), || {
                    entered_tx.send("slow").unwrap();
                    release_rx.recv().unwrap();
                    Ok(Arc::new(1))
                })
                .unwrap()
        });
        assert_eq!(entered_rx.recv().unwrap(), "slow");
        let fast_slots = slots.clone();
        let (fast_tx, fast_rx) = std::sync::mpsc::channel();
        let fast = std::thread::spawn(move || {
            fast_slots
                .load("fast/repo".into(), || {
                    fast_tx.send(()).unwrap();
                    Ok(Arc::new(2))
                })
                .unwrap()
        });
        fast_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("the independent repository should open while the first is blocked");
        release_tx.send(()).unwrap();
        assert_eq!(*slow.join().unwrap(), 1);
        assert_eq!(*fast.join().unwrap(), 2);
    }

    #[test]
    fn repository_registry_has_a_hard_resident_engine_cap() {
        let slots = LoadingSlots::new();
        assert!(slots
            .load("namespace/failed".into(), || anyhow::bail!(
                "injected failure"
            ))
            .is_err());
        assert_eq!(slots.len(), 0, "failed loads must not consume capacity");
        for index in 0..MAX_RESIDENT_REPOSITORIES {
            slots
                .load(format!("namespace/repo-{index}"), || Ok(Arc::new(index)))
                .unwrap();
        }
        let error = slots
            .load("namespace/overflow".into(), || Ok(Arc::new(99)))
            .unwrap_err();
        assert!(LoadingSlots::<usize>::is_capacity_error(&error));
        assert_eq!(slots.len(), MAX_RESIDENT_REPOSITORIES);
        assert_eq!(
            *slots
                .load("namespace/repo-0".into(), || panic!("must remain warm"))
                .unwrap(),
            0
        );
    }

    #[test]
    fn a_failed_load_wakes_an_existing_waiter_and_can_be_retried() {
        assert_failed_attempt_wakes_waiter(false);
    }

    #[test]
    fn repository_open_limit_is_bounded() {
        let limit = Arc::new(OpenLimit::new(1));
        let first = limit.acquire().unwrap();
        let waiting_limit = limit.clone();
        let (acquired_tx, acquired_rx) = std::sync::mpsc::channel();
        let waiting = std::thread::spawn(move || {
            let _permit = waiting_limit.acquire().unwrap();
            acquired_tx.send(()).unwrap();
        });
        assert!(
            acquired_rx
                .recv_timeout(std::time::Duration::from_millis(100))
                .is_err(),
            "a second open exceeded the configured bound"
        );
        drop(first);
        acquired_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("a released permit should wake a bounded open");
        waiting.join().unwrap();
    }

    #[test]
    fn a_panicking_load_wakes_an_existing_waiter_and_can_be_retried() {
        assert_failed_attempt_wakes_waiter(true);
    }

    fn assert_failed_attempt_wakes_waiter(panics: bool) {
        let slots = Arc::new(LoadingSlots::new());
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let leader_slots = slots.clone();
        let leader = std::thread::spawn(move || {
            leader_slots.load("same/repo".into(), || -> Result<Arc<u8>> {
                entered_tx.send(()).unwrap();
                release_rx.recv().unwrap();
                if panics {
                    panic!("injected loader panic");
                }
                anyhow::bail!("injected open failure")
            })
        });
        entered_rx.recv().unwrap();
        let waiter_slots = slots.clone();
        let waiter = std::thread::spawn(move || {
            waiter_slots.load("same/repo".into(), || -> Result<Arc<u8>> {
                panic!("waiter started a duplicate load")
            })
        });
        slots.wait_for_waiter("same/repo");
        release_tx.send(()).unwrap();
        assert!(leader.join().unwrap().is_err());
        assert!(waiter.join().unwrap().is_err());
        assert_eq!(slots.len(), 0);
        assert_eq!(
            *slots.load("same/repo".into(), || Ok(Arc::new(4))).unwrap(),
            4
        );
    }

    #[test]
    fn provisioning_record_discards_partial_local_initialization_and_resumes() {
        let (temporary, host) = host();
        let token = host.owner_token(&[3; 32]);
        let record = NamespaceRecord {
            owner_fingerprint: fingerprint(&token),
            repositories: BTreeMap::from([("repo".to_string(), RepositoryState::Provisioning)]),
        };
        host.bucket
            .compare_and_put(
                "_hosting/namespaces/namespace.json",
                &serde_json::to_vec(&record).unwrap(),
                None,
            )
            .unwrap();
        let partial = temporary
            .path()
            .join("cache/repositories/namespace/repo/.jj");
        std::fs::create_dir_all(&partial).unwrap();
        std::fs::write(partial.join("partial"), b"incomplete").unwrap();
        assert_eq!(
            create_repository_sync(&host, "namespace".to_string(), "repo".to_string(), token),
            StatusCode::OK
        );
        assert!(!partial.join("partial").exists());
        assert_eq!(
            host.namespace("namespace").unwrap().unwrap().0.repositories["repo"],
            RepositoryState::Ready
        );
    }

    #[test]
    fn ready_conflict_retries_without_dropping_another_catalog_entry() {
        let (_temporary, host) = host();
        let token = host.owner_token(&[4; 32]);
        let mut injected = false;
        let status = create_repository_with_hook(
            &host,
            "namespace".to_string(),
            "repo".to_string(),
            token,
            || {
                if injected {
                    return;
                }
                injected = true;
                let (mut record, etag) = host.namespace("namespace").unwrap().unwrap();
                record
                    .repositories
                    .insert("other".to_string(), RepositoryState::Ready);
                host.bucket
                    .compare_and_put(
                        "_hosting/namespaces/namespace.json",
                        &serde_json::to_vec(&record).unwrap(),
                        Some(&etag),
                    )
                    .unwrap();
            },
        );
        assert_eq!(status, StatusCode::OK);
        let record = host.namespace("namespace").unwrap().unwrap().0;
        assert_eq!(record.repositories["repo"], RepositoryState::Ready);
        assert_eq!(record.repositories["other"], RepositoryState::Ready);
    }

    #[test]
    fn initial_publication_crashes_never_acknowledge_ready_and_resume_cold() {
        for window in jj_tandem_repository::CrashWindow::ALL {
            let temporary = tempfile::tempdir().unwrap();
            let cache = temporary.path().join("cache");
            let bucket = temporary
                .path()
                .join("bucket")
                .to_string_lossy()
                .to_string();
            let faults = jj_tandem_repository::FaultPoints::inert();
            faults.crash_at(Some(window));
            let failed =
                HostedServer::new_with_faults(cache.clone(), &bucket, "secret", faults).unwrap();
            let token = failed.owner_token(&[9; 32]);
            assert_eq!(
                create_repository_sync(&failed, "namespace".into(), "repo".into(), token.clone()),
                StatusCode::INTERNAL_SERVER_ERROR,
                "{}",
                window.as_str()
            );
            let state = failed
                .namespace("namespace")
                .unwrap()
                .unwrap()
                .0
                .repositories["repo"]
                .clone();
            assert_eq!(state, RepositoryState::Provisioning, "{}", window.as_str());
            drop(failed);

            let recovered = HostedServer::new_with_faults(
                cache,
                &bucket,
                "secret",
                jj_tandem_repository::FaultPoints::inert(),
            )
            .unwrap();
            assert!(
                create_repository_sync(&recovered, "namespace".into(), "repo".into(), token)
                    .is_success(),
                "{}",
                window.as_str()
            );
            assert_eq!(
                recovered
                    .namespace("namespace")
                    .unwrap()
                    .unwrap()
                    .0
                    .repositories["repo"],
                RepositoryState::Ready,
                "{}",
                window.as_str()
            );
        }
    }

    #[test]
    fn failed_ready_catalog_write_resumes_after_restart() {
        let temporary = tempfile::tempdir().unwrap();
        let cache = temporary.path().join("cache");
        let bucket_path = temporary.path().join("bucket");
        let bucket = bucket_path.to_string_lossy().to_string();
        let host = HostedServer::new(cache.clone(), &bucket, "secret").unwrap();
        let token = host.owner_token(&[10; 32]);
        let unavailable = temporary.path().join("bucket-unavailable");
        let mut injected = false;
        let status = create_repository_with_hook(
            &host,
            "namespace".into(),
            "repo".into(),
            token.clone(),
            || {
                if !injected && bucket_path.exists() {
                    injected = true;
                    std::fs::rename(&bucket_path, &unavailable).unwrap();
                    std::fs::write(&bucket_path, b"bucket unavailable").unwrap();
                }
            },
        );
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        drop(host);
        std::fs::remove_file(&bucket_path).unwrap();
        std::fs::create_dir_all(&bucket_path).unwrap();
        std::fs::rename(&unavailable, &bucket_path).unwrap();

        let recovered = HostedServer::new(cache, &bucket, "secret").unwrap();
        assert!(
            create_repository_sync(&recovered, "namespace".into(), "repo".into(), token)
                .is_success()
        );
        assert_eq!(
            recovered
                .namespace("namespace")
                .unwrap()
                .unwrap()
                .0
                .repositories["repo"],
            RepositoryState::Ready
        );
    }
}
