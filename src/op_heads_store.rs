//! TandemOpHeadsStore — jj-lib OpHeadsStore impl that routes head
//! management to a remote tandem server over Cap'n Proto RPC.

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use jj_lib::backend::BackendLoadError;
use jj_lib::object_id::ObjectId as _;
use jj_lib::op_heads_store::*;
use jj_lib::op_store::OperationId;
use jj_lib::settings::UserSettings;
use prost::Message as _;

use crate::rpc::TandemClient;

const WORKSPACE_ID_FILE: &str = "workspace_id";
const CAS_MAX_ATTEMPTS: usize = 80;
const CAS_BACKOFF_BASE_MS: u64 = 2;
const CAS_BACKOFF_MAX_MS: u64 = 256;
const BENCH_DISABLE_OPTIMISTIC_VERSION_ENV: &str =
    "TANDEM_BENCH_DISABLE_OPTIMISTIC_OP_HEAD_VERSION_CACHE";
const VERSION_CACHE_FILE: &str = "heads_version_cache";

/// OpHeadsStore implementation that proxies all reads/writes to a tandem server.
pub struct TandemOpHeadsStore {
    client: Arc<TandemClient>,
    workspace_id: String,
    cached_version: Mutex<Option<u64>>,
    version_cache_path: PathBuf,
    optimistic_version_cache: bool,
    update_guard: Mutex<()>,
    pending_updates: AtomicUsize,
}

impl fmt::Debug for TandemOpHeadsStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TandemOpHeadsStore")
            .field("workspace_id", &self.workspace_id)
            .finish()
    }
}

/// Read server address from env var or file.
fn read_server_address(store_path: &Path) -> Result<String, BackendLoadError> {
    if let Ok(addr) = std::env::var("TANDEM_SERVER") {
        if !addr.is_empty() {
            return Ok(addr);
        }
    }
    let addr_path = store_path.join("server_address");
    std::fs::read_to_string(&addr_path).map_err(|e| {
        BackendLoadError(
            anyhow::anyhow!(
                "cannot read tandem server address from {} or TANDEM_SERVER env: {e}",
                addr_path.display()
            )
            .into(),
        )
    })
}

fn read_workspace_id(store_path: &Path) -> Result<String, BackendLoadError> {
    if let Ok(workspace_id) = std::env::var("TANDEM_WORKSPACE") {
        let trimmed = workspace_id.trim();
        if !trimmed.is_empty() {
            return Ok(trimmed.to_string());
        }
    }

    let workspace_path = store_path.join(WORKSPACE_ID_FILE);
    match std::fs::read_to_string(&workspace_path) {
        Ok(id) => {
            let trimmed = id.trim();
            if trimmed.is_empty() {
                Ok("default".to_string())
            } else {
                Ok(trimmed.to_string())
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok("default".to_string()),
        Err(e) => Err(BackendLoadError(
            anyhow::anyhow!(
                "cannot read tandem workspace identity from {}: {e}",
                workspace_path.display()
            )
            .into(),
        )),
    }
}

fn cas_retry_backoff(attempt: usize, new_id: &[u8]) -> Duration {
    let shift = (attempt.saturating_sub(1)).min(5) as u32;
    let exp_ms = CAS_BACKOFF_BASE_MS.saturating_mul(1u64 << shift);
    let base_ms = exp_ms.min(CAS_BACKOFF_MAX_MS);
    let jitter_window = (base_ms / 2).max(1);
    let seed = new_id
        .iter()
        .fold(0u64, |acc, b| acc.wrapping_mul(131).wrapping_add(*b as u64));
    let jitter_ms = seed.wrapping_add(attempt as u64) % jitter_window;
    Duration::from_millis(base_ms + jitter_ms)
}

fn optimistic_version_cache_enabled() -> bool {
    !std::env::var(BENCH_DISABLE_OPTIMISTIC_VERSION_ENV)
        .map(|value| {
            let normalized = value.trim().to_ascii_lowercase();
            matches!(normalized.as_str(), "1" | "true" | "yes" | "on")
        })
        .unwrap_or(false)
}

fn load_cached_version(path: &Path) -> Option<u64> {
    let raw = std::fs::read_to_string(path).ok()?;
    raw.trim().parse::<u64>().ok()
}

fn persist_cached_version(path: &Path, version: u64) {
    if let Err(err) = std::fs::write(path, version.to_string()) {
        tracing::debug!(path = %path.display(), error = %err, "failed to persist heads version cache");
    }
}

struct PendingUpdateGuard<'a> {
    pending_updates: &'a AtomicUsize,
}

impl Drop for PendingUpdateGuard<'_> {
    fn drop(&mut self) {
        self.pending_updates.fetch_sub(1, Ordering::SeqCst);
    }
}

impl TandemOpHeadsStore {
    /// Initialize a new tandem op heads store (called during workspace init).
    pub fn init(
        store_path: &Path,
        server_addr: &str,
        workspace_id: &str,
    ) -> Result<Self, jj_lib::backend::BackendInitError> {
        std::fs::write(store_path.join("server_address"), server_addr)
            .map_err(|e| jj_lib::backend::BackendInitError(e.into()))?;
        std::fs::write(store_path.join(WORKSPACE_ID_FILE), workspace_id)
            .map_err(|e| jj_lib::backend::BackendInitError(e.into()))?;

        let client = TandemClient::connect(server_addr)
            .map_err(|e| jj_lib::backend::BackendInitError(e.into()))?;
        let version_cache_path = store_path.join(VERSION_CACHE_FILE);
        let optimistic_version_cache = optimistic_version_cache_enabled();
        let cached_version = if optimistic_version_cache {
            load_cached_version(&version_cache_path)
        } else {
            None
        };

        Ok(Self {
            client,
            workspace_id: workspace_id.to_string(),
            cached_version: Mutex::new(cached_version),
            version_cache_path,
            optimistic_version_cache,
            update_guard: Mutex::new(()),
            pending_updates: AtomicUsize::new(0),
        })
    }

    /// Load an existing tandem op heads store from `store_path`.
    pub fn load(_settings: &UserSettings, store_path: &Path) -> Result<Self, BackendLoadError> {
        let server_addr = read_server_address(store_path)?;
        let workspace_id = read_workspace_id(store_path)?;
        let client = TandemClient::connect(&server_addr).map_err(|e| BackendLoadError(e.into()))?;
        let version_cache_path = store_path.join(VERSION_CACHE_FILE);
        let optimistic_version_cache = optimistic_version_cache_enabled();
        let cached_version = if optimistic_version_cache {
            load_cached_version(&version_cache_path)
        } else {
            None
        };
        Ok(Self {
            client,
            workspace_id,
            cached_version: Mutex::new(cached_version),
            version_cache_path,
            optimistic_version_cache,
            update_guard: Mutex::new(()),
            pending_updates: AtomicUsize::new(0),
        })
    }

    fn cached_version(&self) -> Option<u64> {
        *self.cached_version.lock().expect("cached version lock")
    }

    fn remember_version(&self, version: u64) {
        let mut guard = self.cached_version.lock().expect("cached version lock");
        if guard.is_some_and(|cached| cached == version) {
            return;
        }
        *guard = Some(version);
        drop(guard);

        if self.optimistic_version_cache {
            persist_cached_version(&self.version_cache_path, version);
        }
    }

    fn clear_cached_version(&self) {
        let mut guard = self.cached_version.lock().expect("cached version lock");
        *guard = None;
        drop(guard);

        if self.optimistic_version_cache {
            if let Err(err) = std::fs::remove_file(&self.version_cache_path) {
                if err.kind() != std::io::ErrorKind::NotFound {
                    tracing::debug!(
                        path = %self.version_cache_path.display(),
                        error = %err,
                        "failed to clear heads version cache"
                    );
                }
            }
        }
    }

    fn operation_view_id(&self, op_id: &[u8]) -> Option<Vec<u8>> {
        let data = self.client.get_operation(op_id).ok()?;
        let proto = jj_lib::protos::simple_op_store::Operation::decode(&*data).ok()?;
        Some(proto.view_id)
    }

    fn heads_for_workspace(&self, state: crate::rpc::HeadsState) -> Vec<OperationId> {
        let mut ids = state.heads;
        let workspace_head = state.workspace_heads.get(&self.workspace_id).cloned();

        if let Some(workspace_head) = workspace_head {
            if ids.len() == 1 && ids[0].as_slice() != workspace_head.as_slice() {
                let global_head = ids[0].clone();
                let global_view = self.operation_view_id(&global_head);
                let workspace_view = self.operation_view_id(&workspace_head);
                let same_view = global_view.is_some() && global_view == workspace_view;
                if same_view {
                    // During workspace init we can end up with sibling operations
                    // that carry equivalent view state. Letting jj resolve those
                    // heads can pick the global head and trip sibling-operation
                    // checks before the first user command. Prefer the workspace
                    // op only for this equivalent-view case.
                    ids = vec![workspace_head];
                } else {
                    ids.push(workspace_head);
                }
            } else {
                let already_present = ids
                    .iter()
                    .any(|head| head.as_slice() == workspace_head.as_slice());
                if !already_present {
                    ids.push(workspace_head);
                }
            }
        }

        ids.into_iter().map(OperationId::new).collect()
    }
}

#[async_trait]
impl OpHeadsStore for TandemOpHeadsStore {
    fn name(&self) -> &str {
        "tandem_op_heads_store"
    }

    async fn update_op_heads(
        &self,
        old_ids: &[OperationId],
        new_id: &OperationId,
    ) -> Result<(), OpHeadsStoreError> {
        let old_bytes: Vec<Vec<u8>> = old_ids.iter().map(|id| id.as_bytes().to_vec()).collect();
        let new_bytes = new_id.as_bytes().to_vec();

        let pending_now = self.pending_updates.fetch_add(1, Ordering::SeqCst) + 1;
        let queue_depth = pending_now.saturating_sub(1);
        let _pending_guard = PendingUpdateGuard {
            pending_updates: &self.pending_updates,
        };

        // Guard head updates per workspace instance so retries for a single
        // workspace remain ordered and measurable.
        let _ordering_guard = self.update_guard.lock().expect("update guard lock");

        // Retry loop for CAS conflicts. Start with a cached version when
        // available to avoid an unconditional get_heads() RTT on hot commit paths.
        let mut expected_version = if self.optimistic_version_cache {
            if let Some(version) = self.cached_version() {
                version
            } else {
                let state =
                    self.client
                        .get_heads_state()
                        .map_err(|e| OpHeadsStoreError::Write {
                            new_op_id: new_id.clone(),
                            source: e.into(),
                        })?;
                self.remember_version(state.version);
                state.version
            }
        } else {
            let state = self
                .client
                .get_heads_state()
                .map_err(|e| OpHeadsStoreError::Write {
                    new_op_id: new_id.clone(),
                    source: e.into(),
                })?;
            self.remember_version(state.version);
            state.version
        };

        let mut cas_retries = 0usize;
        let mut saw_contention = false;
        let started_at = Instant::now();

        for attempt in 1..=CAS_MAX_ATTEMPTS {
            let result = self
                .client
                .update_op_heads(&old_bytes, &new_bytes, expected_version, &self.workspace_id)
                .map_err(|e| {
                    tracing::error!(
                        rpc_method = "updateOpHeads",
                        workspace_id = %self.workspace_id,
                        attempt,
                        cas_retries,
                        queue_depth,
                        latency_ms = started_at.elapsed().as_millis() as u64,
                        error = %e,
                        "op-head update failed"
                    );
                    OpHeadsStoreError::Write {
                        new_op_id: new_id.clone(),
                        source: e.into(),
                    }
                })?;

            if result.ok {
                if saw_contention {
                    self.clear_cached_version();
                } else {
                    self.remember_version(result.version);
                }
                tracing::debug!(
                    rpc_method = "updateOpHeads",
                    workspace_id = %self.workspace_id,
                    attempt,
                    cas_retries,
                    queue_depth,
                    latency_ms = started_at.elapsed().as_millis() as u64,
                    "op-head update succeeded"
                );
                return Ok(());
            }

            cas_retries += 1;
            saw_contention = true;
            expected_version = result.version;
            if attempt == CAS_MAX_ATTEMPTS {
                break;
            }

            let backoff = cas_retry_backoff(attempt, &new_bytes);
            tracing::warn!(
                rpc_method = "updateOpHeads",
                workspace_id = %self.workspace_id,
                attempt,
                cas_retries,
                queue_depth,
                latency_ms = started_at.elapsed().as_millis() as u64,
                backoff_ms = backoff.as_millis() as u64,
                "CAS contention detected; retrying"
            );
            std::thread::sleep(backoff);
        }

        tracing::error!(
            rpc_method = "updateOpHeads",
            workspace_id = %self.workspace_id,
            attempt = CAS_MAX_ATTEMPTS,
            cas_retries,
            queue_depth,
            latency_ms = started_at.elapsed().as_millis() as u64,
            "CAS retry limit exceeded"
        );

        self.clear_cached_version();

        Err(OpHeadsStoreError::Write {
            new_op_id: new_id.clone(),
            source: anyhow::anyhow!("CAS retry limit exceeded after {CAS_MAX_ATTEMPTS} attempts")
                .into(),
        })
    }

    async fn get_op_heads(&self) -> Result<Vec<OperationId>, OpHeadsStoreError> {
        let state = self
            .client
            .get_heads_state()
            .map_err(|e| OpHeadsStoreError::Read(e.into()))?;
        let workspace_head_present = state.workspace_heads.contains_key(&self.workspace_id);
        let effective_heads = self.heads_for_workspace(state.clone());
        tracing::debug!(
            rpc_method = "getHeads",
            workspace_id = %self.workspace_id,
            server_heads = state.heads.len(),
            workspace_heads = state.workspace_heads.len(),
            workspace_head_present,
            effective_heads = effective_heads.len(),
            "loaded op heads for workspace"
        );
        self.remember_version(state.version);
        Ok(effective_heads)
    }

    async fn lock(&self) -> Result<Box<dyn OpHeadsStoreLock + '_>, OpHeadsStoreError> {
        Ok(Box::new(NoopLock))
    }
}

/// No-op lock — tandem uses server-side CAS instead of client-side locking.
struct NoopLock;

impl OpHeadsStoreLock for NoopLock {}

#[cfg(test)]
mod tests {
    use super::{load_cached_version, persist_cached_version};

    #[test]
    fn heads_version_cache_round_trip() {
        let temp = tempfile::tempdir().expect("tempdir");
        let cache_path = temp.path().join("heads_version_cache");

        assert_eq!(load_cached_version(&cache_path), None);
        persist_cached_version(&cache_path, 42);
        assert_eq!(load_cached_version(&cache_path), Some(42));
    }

    #[test]
    fn heads_version_cache_ignores_invalid_content() {
        let temp = tempfile::tempdir().expect("tempdir");
        let cache_path = temp.path().join("heads_version_cache");

        std::fs::write(&cache_path, "not-a-version").expect("write invalid cache file");
        assert_eq!(load_cached_version(&cache_path), None);
    }
}
