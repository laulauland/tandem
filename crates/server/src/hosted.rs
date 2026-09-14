use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex};

use anyhow::{bail, Context, Result};
use axum::body::Body;
use axum::extract::{Path, Request, State};
use axum::http::{header, HeaderValue, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::{post, put};
use axum::{Json, Router};
use blake2::{Blake2b512, Digest as _};
use jj_tandem_protocol::names::RepositoryName;
use jj_tandem_storage::{CasError, ObjectStore};
use rand::TryRngCore as _;
use serde::{Deserialize, Serialize};
use tower::ServiceExt as _;

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

#[derive(Serialize)]
struct OwnerBody {
    token: String,
}

pub struct HostedServer {
    cache_root: PathBuf,
    bucket_spec: String,
    bucket: Arc<dyn ObjectStore>,
    host_secret: String,
    repositories: LoadingSlots<Server>,
    open_limit: OpenLimit,
    faults: Arc<jj_tandem_repository::FaultPoints>,
}

impl HostedServer {
    pub fn new(cache_root: PathBuf, bucket_spec: &str, host_secret: &str) -> Result<Self> {
        Self::new_with_faults(
            cache_root,
            bucket_spec,
            host_secret,
            jj_tandem_repository::FaultPoints::from_environment(),
        )
    }

    fn new_with_faults(
        cache_root: PathBuf,
        bucket_spec: &str,
        host_secret: &str,
        faults: Arc<jj_tandem_repository::FaultPoints>,
    ) -> Result<Self> {
        let bucket = jj_tandem_storage::open(bucket_spec).context("open hosted bucket")?;
        if !jj_tandem_storage::probe_conditional_put(bucket.as_ref())? {
            bail!("hosted repositories require a bucket with conditional puts");
        }
        Ok(Self {
            cache_root,
            bucket_spec: bucket_spec.to_string(),
            bucket,
            host_secret: host_secret.to_string(),
            repositories: LoadingSlots::new(),
            open_limit: OpenLimit::new(4),
            faults,
        })
    }

    fn namespace(&self, namespace: &str) -> Result<Option<(NamespaceRecord, String)>> {
        let key = format!("{CATALOG_PREFIX}/{namespace}.json");
        self.bucket
            .get_with_etag(&key)?
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
            let signing_key = self.repository_signing_key(&record.owner_fingerprint, &name_text);
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
                let server = Arc::new(Server::new_with_faults(
                    cache.clone(),
                    Some(&bucket),
                    &signing_key,
                    self.faults.clone(),
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

    fn repository_signing_key(&self, owner: &str, name: &str) -> String {
        let mut hash = Blake2b512::new();
        hash.update(self.host_secret.as_bytes());
        hash.update([0]);
        hash.update(owner.as_bytes());
        hash.update([0]);
        hash.update(name.as_bytes());
        format!("tdma_{}", hex(&hash.finalize()))
    }

    fn owner_token(&self, entropy: &[u8; 32]) -> String {
        let body = hex(entropy);
        let mut hash = Blake2b512::new();
        hash.update(self.host_secret.as_bytes());
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
        let mut hash = Blake2b512::new();
        hash.update(self.host_secret.as_bytes());
        hash.update([0]);
        hash.update(entropy.as_bytes());
        constant_time_eq(
            presented_tag.as_bytes(),
            hex(&hash.finalize()[..32]).as_bytes(),
        )
    }
}

struct LoadingSlots<T> {
    entries: Mutex<HashMap<String, Arc<LoadSlot<T>>>>,
}

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
        .route("/api/owners", post(create_owner))
        .route("/{namespace}/{repository}", put(create_repository))
        .fallback(dispatch_repository)
        .with_state(server)
}

async fn create_owner(State(server): State<Arc<HostedServer>>, request: Request) -> Response {
    if bearer(&request) != Some(server.host_secret.as_str()) {
        return StatusCode::NOT_FOUND.into_response();
    }
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
                    return StatusCode::INTERNAL_SERVER_ERROR;
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
    let namespace = name.namespace().to_string();
    let repository = name.repository().to_string();
    let loaded = tokio::task::spawn_blocking({
        let hosted = hosted.clone();
        let namespace = namespace.clone();
        let repository = repository.clone();
        move || {
            let Some((record, _)) = hosted.namespace(&namespace)? else {
                return Ok(None);
            };
            if record.repositories.get(&repository) != Some(&RepositoryState::Ready) {
                return Ok(None);
            }
            let server = hosted.repository(&name, &record, false)?;
            Ok::<_, anyhow::Error>(Some((record, server)))
        }
    })
    .await;
    let (record, server) = match loaded {
        Ok(Ok(Some(value))) => value,
        Ok(Ok(None)) => return StatusCode::NOT_FOUND.into_response(),
        Ok(Err(error)) => {
            tracing::error!(%error, namespace, repository, "hosted repository load failed");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
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
        if fingerprint(&presented) != record.owner_fingerprint {
            return StatusCode::NOT_FOUND.into_response();
        }
        let key = hosted.repository_signing_key(
            &record.owner_fingerprint,
            &format!("{namespace}/{repository}"),
        );
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
    *request.uri_mut() = format!("/{rest}{query}").parse::<Uri>().unwrap();
    crate::http::router(server)
        .oneshot(request.map(Body::new))
        .await
        .unwrap_or_else(|never| match never {})
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
