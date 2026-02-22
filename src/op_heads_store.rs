//! TandemOpHeadsStore — jj-lib OpHeadsStore impl that routes head
//! management to a remote tandem server over Cap'n Proto RPC.

use std::fmt;
use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use jj_lib::backend::BackendLoadError;
use jj_lib::object_id::ObjectId as _;
use jj_lib::op_heads_store::*;
use jj_lib::op_store::OperationId;
use jj_lib::settings::UserSettings;

use crate::rpc::TandemClient;

const WORKSPACE_ID_FILE: &str = "workspace_id";

/// OpHeadsStore implementation that proxies all reads/writes to a tandem server.
pub struct TandemOpHeadsStore {
    client: Arc<TandemClient>,
    workspace_id: String,
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

        Ok(Self {
            client,
            workspace_id: workspace_id.to_string(),
        })
    }

    /// Load an existing tandem op heads store from `store_path`.
    pub fn load(_settings: &UserSettings, store_path: &Path) -> Result<Self, BackendLoadError> {
        let server_addr = read_server_address(store_path)?;
        let workspace_id = read_workspace_id(store_path)?;
        let client = TandemClient::connect(&server_addr).map_err(|e| BackendLoadError(e.into()))?;
        Ok(Self {
            client,
            workspace_id,
        })
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

        // Retry loop for CAS conflicts
        for _attempt in 0..20 {
            let (_current_heads, version) =
                self.client
                    .get_heads()
                    .map_err(|e| OpHeadsStoreError::Write {
                        new_op_id: new_id.clone(),
                        source: e.into(),
                    })?;

            let result = self
                .client
                .update_op_heads(&old_bytes, &new_bytes, version, &self.workspace_id)
                .map_err(|e| OpHeadsStoreError::Write {
                    new_op_id: new_id.clone(),
                    source: e.into(),
                })?;

            if result.ok {
                return Ok(());
            }
            // CAS conflict — retry with new version
        }

        Err(OpHeadsStoreError::Write {
            new_op_id: new_id.clone(),
            source: anyhow::anyhow!("CAS retry limit exceeded").into(),
        })
    }

    async fn get_op_heads(&self) -> Result<Vec<OperationId>, OpHeadsStoreError> {
        let (heads, _version) = self
            .client
            .get_heads()
            .map_err(|e| OpHeadsStoreError::Read(e.into()))?;
        Ok(heads.into_iter().map(OperationId::new).collect())
    }

    async fn lock(&self) -> Result<Box<dyn OpHeadsStoreLock + '_>, OpHeadsStoreError> {
        Ok(Box::new(NoopLock))
    }
}

/// No-op lock — tandem uses server-side CAS instead of client-side locking.
struct NoopLock;

impl OpHeadsStoreLock for NoopLock {}
