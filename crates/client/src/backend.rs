//! TandemBackend — jj-lib Backend impl that routes all object I/O
//! to a remote tandem server over HTTP.

use std::fmt;
use std::io::Cursor;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::time::SystemTime;

use async_trait::async_trait;
use futures::stream::BoxStream;
use jj_lib::backend::*;
use jj_lib::index::Index;
use jj_lib::object_id::ObjectId as _;
use jj_lib::repo_path::{RepoPath, RepoPathBuf};
use jj_lib::settings::UserSettings;
use prost::Message as _;
use tokio::io::AsyncRead;

use crate::http_client::TandemClient;
use crate::repo_link;
use jj_tandem_jj::{ids, proto_convert};
// Object kind discriminants. `wire` owns them because they also name the
// `/api/objects/{kind}` path segments and the batch-frame records.
use jj_tandem_protocol::wire::{KIND_COMMIT, KIND_FILE, KIND_SYMLINK, KIND_TREE};

/// Backend implementation that proxies all reads/writes to a tandem server.
pub struct TandemBackend {
    client: Arc<TandemClient>,
    commit_id_len: usize,
    change_id_len: usize,
    root_commit_id: CommitId,
    root_change_id: ChangeId,
    empty_tree_id: TreeId,
}

impl fmt::Debug for TandemBackend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TandemBackend")
            .field("server", &self.client.server_addr())
            .finish()
    }
}

impl TandemBackend {
    /// Initialize a new tandem backend (called during workspace init).
    pub fn init(
        store_path: &Path,
        server_addr: &str,
        token: &str,
    ) -> Result<Self, BackendInitError> {
        // Write the link for future loads.
        repo_link::write_link(store_path, server_addr, token)?;

        let client =
            TandemClient::connect(server_addr, token).map_err(|e| BackendInitError(e.into()))?;
        let info = client.repo_info().clone();

        Ok(Self {
            client,
            commit_id_len: info.commit_id_length,
            change_id_len: info.change_id_length,
            root_commit_id: ids::commit(info.root_commit_id),
            root_change_id: ids::change(info.root_change_id),
            empty_tree_id: ids::tree(info.empty_tree_id),
        })
    }

    /// Load an existing tandem backend from `store_path`.
    pub fn load(_settings: &UserSettings, store_path: &Path) -> Result<Self, BackendLoadError> {
        let server_addr = repo_link::read_server_address(store_path)?;
        let token = repo_link::read_token(store_path)?;
        let client =
            TandemClient::connect(&server_addr, &token).map_err(|e| BackendLoadError(e.into()))?;
        let info = client.repo_info().clone();

        Ok(Self {
            client,
            commit_id_len: info.commit_id_length,
            change_id_len: info.change_id_length,
            root_commit_id: ids::commit(info.root_commit_id),
            root_change_id: ids::change(info.root_change_id),
            empty_tree_id: ids::tree(info.empty_tree_id),
        })
    }
}

fn to_backend_err(err: anyhow::Error) -> BackendError {
    BackendError::Other(err.into())
}

#[async_trait]
impl Backend for TandemBackend {
    fn name(&self) -> &str {
        "tandem"
    }

    fn commit_id_length(&self) -> usize {
        self.commit_id_len
    }

    fn change_id_length(&self) -> usize {
        self.change_id_len
    }

    fn root_commit_id(&self) -> &CommitId {
        &self.root_commit_id
    }

    fn root_change_id(&self) -> &ChangeId {
        &self.root_change_id
    }

    fn empty_tree_id(&self) -> &TreeId {
        &self.empty_tree_id
    }

    fn concurrency(&self) -> usize {
        64
    }

    async fn read_file(
        &self,
        _path: &RepoPath,
        id: &FileId,
    ) -> BackendResult<Pin<Box<dyn AsyncRead + Send>>> {
        let data = self
            .client
            .get_object(KIND_FILE, id.as_bytes())
            .map_err(|e| BackendError::ReadObject {
                object_type: "file".into(),
                hash: id.hex(),
                source: e.into(),
            })?;
        Ok(Box::pin(Cursor::new(data)))
    }

    async fn write_file(
        &self,
        _path: &RepoPath,
        contents: &mut (dyn AsyncRead + Send + Unpin),
    ) -> BackendResult<FileId> {
        let mut buf = Vec::new();
        tokio::io::AsyncReadExt::read_to_end(contents, &mut buf)
            .await
            .map_err(|e| to_backend_err(e.into()))?;
        let (id, _) = self
            .client
            .put_object(KIND_FILE, &buf)
            .map_err(to_backend_err)?;
        Ok(FileId::new(id))
    }

    async fn read_symlink(&self, _path: &RepoPath, id: &SymlinkId) -> BackendResult<String> {
        let data = self
            .client
            .get_object(KIND_SYMLINK, id.as_bytes())
            .map_err(|e| BackendError::ReadObject {
                object_type: "symlink".into(),
                hash: id.hex(),
                source: e.into(),
            })?;
        String::from_utf8(data).map_err(|e| to_backend_err(e.into()))
    }

    async fn write_symlink(&self, _path: &RepoPath, target: &str) -> BackendResult<SymlinkId> {
        let (id, _) = self
            .client
            .put_object(KIND_SYMLINK, target.as_bytes())
            .map_err(to_backend_err)?;
        Ok(SymlinkId::new(id))
    }

    async fn read_copy(&self, _id: &CopyId) -> BackendResult<CopyHistory> {
        Err(BackendError::Unsupported(
            "Copy tracking not yet supported".into(),
        ))
    }

    async fn write_copy(&self, _copy: &CopyHistory) -> BackendResult<CopyId> {
        Err(BackendError::Unsupported(
            "Copy tracking not yet supported".into(),
        ))
    }

    async fn get_related_copies(&self, copy_id: &CopyId) -> BackendResult<Vec<CopyHistory>> {
        let maybe_wire_copies = self
            .client
            .get_related_copies(copy_id.as_bytes())
            .map_err(to_backend_err)?;

        if maybe_wire_copies.is_none() {
            return Err(BackendError::Unsupported(
                "Copy tracking not supported by server capabilities".into(),
            ));
        }

        Err(BackendError::Unsupported(
            "Copy tracking decoding not yet implemented".into(),
        ))
    }

    async fn read_tree(&self, _path: &RepoPath, id: &TreeId) -> BackendResult<Tree> {
        let data = self
            .client
            .get_object(KIND_TREE, id.as_bytes())
            .map_err(|e| BackendError::ReadObject {
                object_type: "tree".into(),
                hash: id.hex(),
                source: e.into(),
            })?;
        let proto = jj_lib::protos::simple_store::Tree::decode(&*data)
            .map_err(|e| to_backend_err(e.into()))?;
        Ok(proto_convert::tree_from_proto(proto))
    }

    async fn write_tree(&self, _path: &RepoPath, contents: &Tree) -> BackendResult<TreeId> {
        let proto = proto_convert::tree_to_proto(contents);
        let data = proto.encode_to_vec();
        let (id, _) = self
            .client
            .put_object(KIND_TREE, &data)
            .map_err(to_backend_err)?;
        Ok(TreeId::new(id))
    }

    async fn read_commit(&self, id: &CommitId) -> BackendResult<Commit> {
        if *id == self.root_commit_id {
            return Ok(make_root_commit(
                self.root_change_id.clone(),
                self.empty_tree_id.clone(),
            ));
        }
        let data = self
            .client
            .get_object(KIND_COMMIT, id.as_bytes())
            .map_err(|e| BackendError::ReadObject {
                object_type: "commit".into(),
                hash: id.hex(),
                source: e.into(),
            })?;
        let proto = jj_lib::protos::simple_store::Commit::decode(&*data)
            .map_err(|e| to_backend_err(e.into()))?;
        Ok(proto_convert::commit_from_proto(proto))
    }

    async fn write_commit(
        &self,
        mut commit: Commit,
        sign_with: Option<&mut SigningFn>,
    ) -> BackendResult<(CommitId, Commit)> {
        assert!(commit.secure_sig.is_none(), "commit.secure_sig was set");

        if commit.parents.is_empty() {
            return Err(BackendError::Other(
                "Cannot write a commit with no parents".into(),
            ));
        }

        let mut proto = jj_lib::simple_backend::commit_to_proto(&commit);
        if let Some(sign) = sign_with {
            let data = proto.encode_to_vec();
            let sig = sign(&data).map_err(|e| BackendError::Other(e.into()))?;
            proto.secure_sig = Some(sig.clone());
            commit.secure_sig = Some(SecureSig { data, sig });
        }

        let data = proto.encode_to_vec();
        let (id, normalized_data) = self
            .client
            .put_object(KIND_COMMIT, &data)
            .map_err(to_backend_err)?;

        // Decode the normalized data to get the commit as stored
        let stored_proto = jj_lib::protos::simple_store::Commit::decode(&*normalized_data)
            .map_err(|e| to_backend_err(e.into()))?;
        let stored_commit = proto_convert::commit_from_proto(stored_proto);
        Ok((CommitId::new(id), stored_commit))
    }

    fn get_copy_records(
        &self,
        _paths: Option<&[RepoPathBuf]>,
        _root: &CommitId,
        _head: &CommitId,
    ) -> BackendResult<BoxStream<'_, BackendResult<CopyRecord>>> {
        Ok(Box::pin(futures::stream::empty()))
    }

    fn gc(&self, _index: &dyn Index, _keep_newer: SystemTime) -> BackendResult<()> {
        Ok(())
    }
}
