//! TandemOpStore — jj-lib OpStore impl that routes operations and views
//! to a remote tandem server over Cap'n Proto RPC.

use std::fmt;
use std::path::Path;
use std::sync::Arc;
use std::time::SystemTime;

use async_trait::async_trait;
use jj_lib::backend::{BackendLoadError, CommitId};
use jj_lib::object_id::{HexPrefix, ObjectId as _, PrefixResolution};
use jj_lib::op_store::*;
use jj_lib::settings::UserSettings;
use prost::Message as _;

use crate::proto_convert;
use crate::rpc::{PrefixResult, TandemClient};

const OPERATION_ID_LENGTH: usize = 64;
const VIEW_ID_LENGTH: usize = 64;

/// OpStore implementation that proxies all reads/writes to a tandem server.
pub struct TandemOpStore {
    client: Arc<TandemClient>,
    root_operation_id: OperationId,
    root_view_id: ViewId,
    root_commit_id: CommitId,
}

impl fmt::Debug for TandemOpStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TandemOpStore").finish()
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

impl TandemOpStore {
    /// Initialize a new tandem op store (called during workspace init).
    pub fn init(
        store_path: &Path,
        server_addr: &str,
        root_data: RootOperationData,
    ) -> Result<Self, jj_lib::backend::BackendInitError> {
        std::fs::write(store_path.join("server_address"), server_addr)
            .map_err(|e| jj_lib::backend::BackendInitError(e.into()))?;

        let client = TandemClient::connect(server_addr)
            .map_err(|e| jj_lib::backend::BackendInitError(e.into()))?;
        let info = client
            .get_repo_info()
            .map_err(|e| jj_lib::backend::BackendInitError(e.into()))?;

        Ok(Self {
            client,
            root_operation_id: OperationId::new(info.root_operation_id),
            root_view_id: ViewId::from_bytes(&[0u8; VIEW_ID_LENGTH]),
            root_commit_id: root_data.root_commit_id,
        })
    }

    /// Load an existing tandem op store from `store_path`.
    pub fn load(
        _settings: &UserSettings,
        store_path: &Path,
        root_data: RootOperationData,
    ) -> Result<Self, BackendLoadError> {
        let server_addr = read_server_address(store_path)?;
        let client = TandemClient::connect(&server_addr).map_err(|e| BackendLoadError(e.into()))?;
        let info = client
            .get_repo_info()
            .map_err(|e| BackendLoadError(e.into()))?;

        Ok(Self {
            client,
            root_operation_id: OperationId::new(info.root_operation_id),
            root_view_id: ViewId::from_bytes(&[0u8; VIEW_ID_LENGTH]),
            root_commit_id: root_data.root_commit_id,
        })
    }
}

fn to_op_err(err: anyhow::Error) -> OpStoreError {
    OpStoreError::Other(err.into())
}

#[async_trait]
impl OpStore for TandemOpStore {
    fn name(&self) -> &str {
        "tandem_op_store"
    }

    fn root_operation_id(&self) -> &OperationId {
        &self.root_operation_id
    }

    async fn read_view(&self, id: &ViewId) -> OpStoreResult<View> {
        if *id == self.root_view_id {
            return Ok(View::make_root(self.root_commit_id.clone()));
        }

        let data = self
            .client
            .get_view(id.as_bytes())
            .map_err(|e| OpStoreError::ReadObject {
                object_type: id.object_type(),
                hash: id.hex(),
                source: e.into(),
            })?;

        let proto = jj_lib::protos::simple_op_store::View::decode(&*data)
            .map_err(|e| to_op_err(e.into()))?;
        proto_convert::view_from_proto(proto).map_err(to_op_err)
    }

    async fn write_view(&self, contents: &View) -> OpStoreResult<ViewId> {
        let proto = proto_convert::view_to_proto(contents);
        let data = proto.encode_to_vec();
        let id = self.client.put_view(&data).map_err(to_op_err)?;
        Ok(ViewId::new(id))
    }

    async fn read_operation(&self, id: &OperationId) -> OpStoreResult<Operation> {
        if *id == self.root_operation_id {
            return Ok(Operation::make_root(self.root_view_id.clone()));
        }

        let data =
            self.client
                .get_operation(id.as_bytes())
                .map_err(|e| OpStoreError::ReadObject {
                    object_type: id.object_type(),
                    hash: id.hex(),
                    source: e.into(),
                })?;

        let proto = jj_lib::protos::simple_op_store::Operation::decode(&*data)
            .map_err(|e| to_op_err(e.into()))?;
        let mut operation = proto_convert::operation_from_proto(proto).map_err(to_op_err)?;

        // Repos created before root operation support will have parentless operations
        if operation.parents.is_empty() {
            operation.parents.push(self.root_operation_id.clone());
        }

        Ok(operation)
    }

    async fn write_operation(&self, contents: &Operation) -> OpStoreResult<OperationId> {
        assert!(!contents.parents.is_empty());
        let proto = proto_convert::operation_to_proto(contents);
        let data = proto.encode_to_vec();
        let id = self.client.put_operation(&data).map_err(to_op_err)?;
        Ok(OperationId::new(id))
    }

    async fn resolve_operation_id_prefix(
        &self,
        prefix: &HexPrefix,
    ) -> OpStoreResult<PrefixResolution<OperationId>> {
        let hex = prefix.hex();

        // Check if it matches the root operation
        let matches_root = prefix.matches(&self.root_operation_id);

        // Full-length fast path
        if hex.len() == OPERATION_ID_LENGTH * 2 && matches_root {
            return Ok(PrefixResolution::SingleMatch(
                self.root_operation_id.clone(),
            ));
        }

        let (result, matched) = self.client.resolve_op_prefix(&hex).map_err(to_op_err)?;

        match result {
            PrefixResult::NoMatch => {
                if matches_root {
                    Ok(PrefixResolution::SingleMatch(
                        self.root_operation_id.clone(),
                    ))
                } else {
                    Ok(PrefixResolution::NoMatch)
                }
            }
            PrefixResult::SingleMatch => {
                if matches_root {
                    // Both root and a stored operation match → ambiguous
                    Ok(PrefixResolution::AmbiguousMatch)
                } else if let Some(id_bytes) = matched {
                    Ok(PrefixResolution::SingleMatch(OperationId::new(id_bytes)))
                } else {
                    Ok(PrefixResolution::NoMatch)
                }
            }
            PrefixResult::Ambiguous => Ok(PrefixResolution::AmbiguousMatch),
        }
    }

    fn gc(&self, _head_ids: &[OperationId], _keep_newer: SystemTime) -> OpStoreResult<()> {
        Ok(())
    }
}
