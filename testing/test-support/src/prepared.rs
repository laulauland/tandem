//! Native jj preparation cache for the isolated combined-request experiment.
//! Imports existing history once; measurements reuse this local repository.
use std::collections::BTreeSet;
use std::sync::Arc;

use anyhow::{ensure, Context, Result};
use jj_lib::backend::{CommitId, CopyId, TreeValue};
use jj_lib::merge::Merge;
use jj_lib::merged_tree_builder::MergedTreeBuilder;
use jj_lib::object_id::ObjectId as _;
use jj_lib::op_store::OperationId;
use jj_lib::ref_name::WorkspaceNameBuf;
use jj_lib::repo::{ReadonlyRepo, Repo as _};
use jj_lib::repo_path::{RepoPath, RepoPathBuf};
use jj_lib::transaction::UnpublishedOperation;
use jj_lib::workspace::Workspace;
use jj_tandem_client::TandemClient;
use jj_tandem_jj::proto_convert;
use jj_tandem_protocol::{hex, wire};
use pollster::FutureExt as _;
use prost::Message as _;

pub struct LocalPreparation {
    _directory: tempfile::TempDir,
    workspace: Workspace,
    repo: Arc<ReadonlyRepo>,
    name: WorkspaceNameBuf,
}

pub struct PreparedChange {
    pub request: wire::PreparedPublish,
    pub commit_id: CommitId,
    operation: UnpublishedOperation,
}

impl LocalPreparation {
    pub fn import(client: &TandemClient, name: &str) -> Result<Self> {
        let directory = tempfile::tempdir()?;
        let (workspace, repo) = Workspace::init_colocated_git(
            &super::test_settings()?,
            directory.path(),
            gix_hash::Kind::Sha1,
        )
        .block_on()?;
        let heads = client.get_heads_state()?;
        ensure!(
            heads.heads.len() == 1,
            "preparation fixture requires one initial head"
        );
        let mut seen = BTreeSet::new();
        import_operation(client, &repo, &heads.heads[0], &mut seen)?;
        let operation = workspace
            .repo_loader()
            .load_operation(&OperationId::new(heads.heads[0].clone()))
            .block_on()?;
        let repo = workspace.repo_loader().load_at(&operation).block_on()?;
        Ok(Self {
            _directory: directory,
            workspace,
            repo,
            name: name.into(),
        })
    }

    pub fn prepare(&self, bytes: &[u8]) -> Result<PreparedChange> {
        let store = self.repo.store();
        let parent_id = self
            .repo
            .view()
            .get_wc_commit_id(&self.name)
            .unwrap_or(store.root_commit_id());
        let parent = store.get_commit(parent_id)?;
        let path = RepoPathBuf::from_internal_string("dir/edited.txt")?;
        let id = store
            .write_file(&path, &mut futures::io::Cursor::new(bytes))
            .block_on()?;
        let mut objects = vec![wire::PreparedObject {
            kind: wire::KIND_FILE,
            id: id.to_bytes(),
            data: bytes.to_vec(),
        }];
        let mut builder = MergedTreeBuilder::new(parent.tree());
        builder.set_or_remove(
            path,
            Merge::normal(TreeValue::File {
                id,
                executable: false,
                copy_id: CopyId::placeholder(),
            }),
        );
        let tree = builder.write_tree().block_on()?;
        collect_trees(store, tree.tree_ids().as_resolved().unwrap(), &mut objects)?;
        let mut tx = self.repo.start_transaction();
        let commit = tx
            .repo_mut()
            .new_commit(vec![parent.id().clone()], tree)
            .set_description("prepared file edit")
            .write()
            .block_on()?;
        objects.push(wire::PreparedObject {
            kind: wire::KIND_COMMIT,
            id: commit.id().to_bytes(),
            data: jj_lib::simple_backend::commit_to_proto(commit.store_commit()).encode_to_vec(),
        });
        tx.repo_mut().edit(self.name.clone(), &commit).block_on()?;
        tx.repo_mut().rebase_descendants().block_on()?;
        let operation = tx.write("prepare edited file").block_on()?;
        let op = operation.operation();
        let request = wire::PreparedPublish {
            objects,
            view: proto_convert::view_to_proto(op.view().block_on()?.store_view()).encode_to_vec(),
            operation: proto_convert::operation_to_proto(op.store_operation()).encode_to_vec(),
            heads: wire::UpdateHeadsBody {
                old_ids: op.parent_ids().iter().map(|id| id.hex()).collect(),
                new_id: op.id().hex(),
                workspace_id: self.name.as_str().to_owned(),
            },
        };
        Ok(PreparedChange {
            request,
            commit_id: commit.id().clone(),
            operation,
        })
    }

    pub fn accept(&mut self, change: PreparedChange) -> Result<()> {
        self.repo = self
            .workspace
            .repo_loader()
            .load_at(change.operation.operation())
            .block_on()?;
        Ok(())
    }
}

fn collect_trees(
    store: &Arc<jj_lib::store::Store>,
    id: &jj_lib::backend::TreeId,
    objects: &mut Vec<wire::PreparedObject>,
) -> Result<()> {
    let tree = store.backend().read_tree(RepoPath::root(), id).block_on()?;
    for entry in tree.entries() {
        if let TreeValue::Tree(child) = entry.value() {
            collect_trees(store, child, objects)?;
        }
    }
    objects.push(wire::PreparedObject {
        kind: wire::KIND_TREE,
        id: id.to_bytes(),
        data: proto_convert::tree_to_proto(&tree).encode_to_vec(),
    });
    Ok(())
}

fn import_object(
    client: &TandemClient,
    repo: &ReadonlyRepo,
    kind: u16,
    id: &[u8],
    seen: &mut BTreeSet<(u16, Vec<u8>)>,
) -> Result<()> {
    if kind == wire::KIND_COMMIT && id == repo.store().root_commit_id().as_bytes() {
        return Ok(());
    }
    if !seen.insert((kind, id.to_vec())) {
        return Ok(());
    }
    let data = client.get_object(kind, id)?;
    let backend = repo.store().backend();
    let stored = match kind {
        wire::KIND_FILE => backend
            .write_file(RepoPath::root(), &mut futures::io::Cursor::new(&data))
            .block_on()?
            .to_bytes(),
        wire::KIND_TREE => {
            let tree = proto_convert::tree_from_proto(jj_lib::protos::simple_store::Tree::decode(
                data.as_slice(),
            )?);
            for entry in tree.entries() {
                match entry.value() {
                    TreeValue::Tree(id) => {
                        import_object(client, repo, wire::KIND_TREE, id.as_bytes(), seen)?
                    }
                    TreeValue::File { id, .. } => {
                        import_object(client, repo, wire::KIND_FILE, id.as_bytes(), seen)?
                    }
                    _ => anyhow::bail!("fixture only supports files and trees"),
                }
            }
            backend
                .write_tree(RepoPath::root(), &tree)
                .block_on()?
                .to_bytes()
        }
        wire::KIND_COMMIT => {
            let commit = proto_convert::commit_from_proto(
                jj_lib::protos::simple_store::Commit::decode(data.as_slice())?,
            );
            for parent in &commit.parents {
                import_object(client, repo, wire::KIND_COMMIT, parent.as_bytes(), seen)?;
            }
            for tree in commit.root_tree.iter() {
                import_object(client, repo, wire::KIND_TREE, tree.as_bytes(), seen)?;
            }
            backend.write_commit(commit, None).block_on()?.0.to_bytes()
        }
        _ => anyhow::bail!("unsupported fixture object"),
    };
    ensure!(stored == id, "native cache import changed identity");
    Ok(())
}

fn import_operation(
    client: &TandemClient,
    repo: &ReadonlyRepo,
    id: &[u8],
    seen: &mut BTreeSet<(u16, Vec<u8>)>,
) -> Result<()> {
    if id == repo.op_store().root_operation_id().as_bytes() || !seen.insert((100, id.to_vec())) {
        return Ok(());
    }
    let data = client.get_operation(id)?;
    let operation = proto_convert::operation_from_proto(
        jj_lib::protos::simple_op_store::Operation::decode(data.as_slice())?,
    )?;
    for parent in &operation.parents {
        import_operation(client, repo, parent.as_bytes(), seen)?;
    }
    let view = proto_convert::view_from_proto(jj_lib::protos::simple_op_store::View::decode(
        client.get_view(operation.view_id.as_bytes())?.as_slice(),
    )?)?;
    for commit in view.head_ids.iter().chain(view.wc_commit_ids.values()) {
        import_object(client, repo, wire::KIND_COMMIT, commit.as_bytes(), seen)?;
    }
    ensure!(
        repo.op_store().write_view(&view).block_on()? == operation.view_id,
        "view import mismatch"
    );
    ensure!(
        repo.op_store()
            .write_operation(&operation)
            .block_on()?
            .as_bytes()
            == id,
        "operation import mismatch"
    );
    Ok(())
}

/// Read the nested path using the caller-selected client cache policy.
pub fn read_edited_file(client: &TandemClient, commit: &CommitId) -> Result<Vec<u8>> {
    let commit = jj_lib::protos::simple_store::Commit::decode(
        client
            .get_object(wire::KIND_COMMIT, commit.as_bytes())?
            .as_slice(),
    )?;
    let mut tree_id = commit.root_tree[0].clone();
    for component in ["dir", "edited.txt"] {
        let tree = proto_convert::tree_from_proto(jj_lib::protos::simple_store::Tree::decode(
            client.get_object(wire::KIND_TREE, &tree_id)?.as_slice(),
        )?);
        let value = tree
            .entries()
            .find(|e| e.name().as_internal_str() == component)
            .unwrap()
            .value()
            .clone();
        match value {
            TreeValue::Tree(id) => tree_id = id.to_bytes(),
            TreeValue::File { id, .. } => return client.get_object(wire::KIND_FILE, id.as_bytes()),
            _ => anyhow::bail!("unexpected entry"),
        }
    }
    anyhow::bail!("file not found")
}

pub fn operation_id(request: &wire::PreparedPublish) -> Vec<u8> {
    hex::from_hex(&request.heads.new_id).unwrap()
}

/// Prove the attributed operation is reachable from current jj heads, then
/// follow its view to the expected commit and read exact file bytes.
pub fn verify_published(
    client: &TandemClient,
    name: &str,
    expected_operation: &[u8],
    commit: &CommitId,
    bytes: &[u8],
) -> Result<()> {
    let state = client.get_heads_state()?;
    let wanted = state
        .workspace_heads
        .get(name)
        .context("missing published workspace operation")?;
    ensure!(
        wanted == expected_operation,
        "server attributed a different operation identity"
    );
    let mut queue = state.heads;
    let mut seen = BTreeSet::new();
    while let Some(id) = queue.pop() {
        if id == client.repo_info().root_operation_id || !seen.insert(id.clone()) {
            continue;
        }
        let operation = proto_convert::operation_from_proto(
            jj_lib::protos::simple_op_store::Operation::decode(
                client.get_operation(&id)?.as_slice(),
            )?,
        )?;
        if &id == wanted {
            let view =
                proto_convert::view_from_proto(jj_lib::protos::simple_op_store::View::decode(
                    client.get_view(operation.view_id.as_bytes())?.as_slice(),
                )?)?;
            ensure!(
                view.wc_commit_ids.get(&WorkspaceNameBuf::from(name)) == Some(commit),
                "published view identifies a different commit"
            );
            ensure!(
                read_edited_file(client, commit)? == bytes,
                "published file bytes differ"
            );
            return Ok(());
        }
        queue.extend(operation.parents.iter().map(|id| id.to_bytes()));
    }
    anyhow::bail!("workspace operation is unreachable from current heads")
}
