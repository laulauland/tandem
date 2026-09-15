//! A snapshot-scoped write set shared by stock jj's three remote stores.
//! Native jj computes identities locally; only a successful bucket-backed
//! publish makes these bytes eligible for the ordinary immutable read cache.
use std::collections::BTreeMap;
use std::sync::Arc;

use anyhow::{ensure, Context, Result};
use futures::executor::block_on;
use jj_lib::backend::Backend as _;
use jj_lib::git_backend::GitBackend;
use jj_lib::object_id::ObjectId as _;
use jj_lib::repo_path::RepoPath;
use jj_lib::settings::UserSettings;
use jj_tandem_jj::proto_convert;
use jj_tandem_protocol::wire;
use prost::Message as _;

use crate::cache::{NAMESPACE_OPERATION, NAMESPACE_VIEW};
use crate::TandemClient;

pub struct PreparedSnapshot {
    client: Arc<TandemClient>,
}
impl Drop for PreparedSnapshot {
    fn drop(&mut self) {
        if let Ok(mut preparation) = self.client.preparation.lock() {
            preparation.active = None;
            preparation.native = None;
        }
    }
}

struct Native {
    backend: GitBackend,
    _directory: tempfile::TempDir,
}

#[derive(Default)]
pub(crate) struct Preparation {
    native: Option<Native>,
    pub(crate) active: Option<Graph>,
}

#[derive(Default)]
pub(crate) struct Graph {
    pub(crate) objects: Vec<wire::PreparedObject>,
    object_index: BTreeMap<(u16, Vec<u8>), usize>,
    pub(crate) metadata: BTreeMap<(String, Vec<u8>), Vec<u8>>,
    bytes: usize,
}

impl Preparation {
    pub(crate) fn begin(&mut self, settings: &UserSettings) -> Result<()> {
        ensure!(
            self.active.is_none(),
            "a snapshot is already being prepared"
        );
        if self.native.is_none() {
            let directory =
                tempfile::tempdir().context("create disposable native preparation store")?;
            let backend = GitBackend::init_internal(settings, directory.path())?;
            self.native = Some(Native {
                backend,
                _directory: directory,
            });
        }
        self.active = Some(Graph::default());
        Ok(())
    }

    pub(crate) fn write(&mut self, kind: u16, data: &[u8]) -> Result<Option<(Vec<u8>, Vec<u8>)>> {
        let Some(graph) = self.active.as_mut() else {
            return Ok(None);
        };
        let backend = &self
            .native
            .as_ref()
            .context("missing native preparation store")?
            .backend;
        let (id, normalized) = match kind {
            wire::KIND_FILE => (
                block_on(backend.write_file(RepoPath::root(), &mut std::io::Cursor::new(data)))?
                    .to_bytes(),
                data.to_vec(),
            ),
            wire::KIND_SYMLINK => (
                block_on(backend.write_symlink(RepoPath::root(), std::str::from_utf8(data)?))?
                    .to_bytes(),
                data.to_vec(),
            ),
            wire::KIND_TREE => {
                let tree = proto_convert::tree_from_proto(
                    jj_lib::protos::simple_store::Tree::decode(data)?,
                );
                (
                    block_on(backend.write_tree(RepoPath::root(), &tree))?.to_bytes(),
                    data.to_vec(),
                )
            }
            wire::KIND_COMMIT => {
                let commit = proto_convert::commit_from_proto(
                    jj_lib::protos::simple_store::Commit::decode(data)?,
                );
                ensure!(
                    commit.secure_sig.is_none(),
                    "signed snapshots are not supported"
                );
                let (id, normalized) = block_on(backend.write_commit(commit, None))?;
                (
                    id.to_bytes(),
                    jj_lib::simple_backend::commit_to_proto(&normalized).encode_to_vec(),
                )
            }
            _ => anyhow::bail!("unsupported prepared object kind"),
        };
        let key = (kind, id.clone());
        if !graph.object_index.contains_key(&key) {
            graph.charge(normalized.len() + id.len() + 10)?;
            graph.object_index.insert(key, graph.objects.len());
            graph.objects.push(wire::PreparedObject {
                kind,
                id: id.clone(),
                data: normalized.clone(),
            });
        }
        Ok(Some((id, normalized)))
    }
}

impl Graph {
    fn charge(&mut self, bytes: usize) -> Result<()> {
        ensure!(
            bytes <= wire::MAX_REQUEST_BODY_BYTES.saturating_sub(self.bytes),
            "snapshot exceeds prepared publish request limit"
        );
        self.bytes += bytes;
        Ok(())
    }

    pub(crate) fn metadata(&mut self, namespace: &str, id: &[u8], data: &[u8]) -> Result<()> {
        let key = (namespace.to_owned(), id.to_vec());
        if !self.metadata.contains_key(&key) {
            self.charge(data.len() + id.len() + 4)?;
            self.metadata.insert(key, data.to_vec());
        }
        Ok(())
    }

    pub(crate) fn read(&self, namespace: &str, id: &[u8]) -> Option<Vec<u8>> {
        if let Some(kind) = wire::kind_from_name(namespace) {
            self.object_index
                .get(&(kind, id.to_vec()))
                .map(|index| self.objects[*index].data.clone())
        } else {
            self.metadata
                .get(&(namespace.to_owned(), id.to_vec()))
                .cloned()
        }
    }

    pub(crate) fn request(&self, heads: wire::UpdateHeadsBody) -> Result<wire::PreparedPublish> {
        let id = jj_tandem_protocol::hex::from_hex(&heads.new_id)?;
        let operation = self
            .read(NAMESPACE_OPERATION, &id)
            .context("snapshot operation was not prepared")?;
        let proto = jj_lib::protos::simple_op_store::Operation::decode(operation.as_slice())?;
        let view = self
            .read(NAMESPACE_VIEW, &proto.view_id)
            .context("snapshot view was not prepared")?;
        Ok(wire::PreparedPublish {
            objects: self.objects.clone(),
            view,
            operation,
            heads,
        })
    }
}

impl TandemClient {
    pub fn prepare_snapshot(self: &Arc<Self>, settings: &UserSettings) -> Result<PreparedSnapshot> {
        self.preparation
            .lock()
            .map_err(|_| anyhow::anyhow!("preparation lock poisoned"))?
            .begin(settings)?;
        Ok(PreparedSnapshot {
            client: self.clone(),
        })
    }
}
