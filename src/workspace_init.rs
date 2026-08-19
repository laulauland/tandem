//! Creating a tandem workspace on disk.
//!
//! This is the body of `tandem init`. It lives in the library rather than in
//! the CLI so that the simulation harness can create its agents through the
//! same code the product runs — an in-process test that hand-rolls its own
//! workspace setup is testing the hand-rolled setup, not tandem.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use jj_lib::backend::CommitId;
use jj_lib::ref_name::{WorkspaceName, WorkspaceNameBuf};
use jj_lib::repo::{Repo as _, ReadonlyRepo, StoreFactories};
use jj_lib::settings::UserSettings;
use jj_lib::signing::Signer;
use jj_lib::workspace::{default_working_copy_factory, Workspace};

use crate::{backend, op_heads_store, op_store};

/// What `Workspace::init_with_factories` wants for each store it creates: a
/// closure from settings and a store path to a boxed implementation. Naming the
/// three shapes keeps the call site readable.
type BackendInit<'a> = &'a dyn Fn(
    &UserSettings,
    &Path,
) -> Result<Box<dyn jj_lib::backend::Backend>, jj_lib::backend::BackendInitError>;

type OpStoreInit<'a> = &'a dyn Fn(
    &UserSettings,
    &Path,
    jj_lib::op_store::RootOperationData,
) -> Result<Box<dyn jj_lib::op_store::OpStore>, jj_lib::backend::BackendInitError>;

type OpHeadsInit<'a> = &'a dyn Fn(
    &UserSettings,
    &Path,
) -> Result<Box<dyn jj_lib::op_heads_store::OpHeadsStore>, jj_lib::backend::BackendInitError>;

/// Register the tandem backend, op store and op-heads store so that jj can
/// load a repo whose store type is `tandem`.
pub fn tandem_factories() -> StoreFactories {
    let mut factories = StoreFactories::empty();

    factories.add_backend(
        "tandem",
        Box::new(|settings, store_path| {
            Ok(Box::new(backend::TandemBackend::load(
                settings, store_path,
            )?))
        }),
    );

    factories.add_op_store(
        "tandem_op_store",
        Box::new(|settings, store_path, root_data| {
            Ok(Box::new(op_store::TandemOpStore::load(
                settings, store_path, root_data,
            )?))
        }),
    );

    factories.add_op_heads_store(
        "tandem_op_heads_store",
        Box::new(|settings, store_path| {
            Ok(Box::new(op_heads_store::TandemOpHeadsStore::load(
                settings, store_path,
            )?))
        }),
    );

    factories
}

/// Create a tandem workspace at `workspace_path` and give it a working-copy
/// commit in the same context as the server's default workspace.
///
/// Returns the canonical path of the new workspace.
pub fn init_tandem_workspace(
    settings: &UserSettings,
    server_addr: &str,
    workspace_name: &str,
    workspace_path: &Path,
) -> Result<PathBuf> {
    std::fs::create_dir_all(workspace_path).context("cannot create workspace directory")?;
    let workspace_path = workspace_path
        .canonicalize()
        .context("cannot resolve workspace path")?;

    let signer = Signer::from_settings(settings).context("cannot create signer")?;

    let backend_addr = server_addr.to_string();
    let op_store_addr = server_addr.to_string();
    let op_heads_addr = server_addr.to_string();
    let op_heads_name = workspace_name.to_string();

    let backend_init: BackendInit = &|_settings, store_path| {
        Ok(Box::new(backend::TandemBackend::init(
            store_path,
            &backend_addr,
        )?))
    };

    let op_store_init: OpStoreInit = &|_settings, store_path, root_data| {
        Ok(Box::new(op_store::TandemOpStore::init(
            store_path,
            &op_store_addr,
            root_data,
        )?))
    };

    let op_heads_init: OpHeadsInit = &|_settings, store_path| {
        Ok(Box::new(op_heads_store::TandemOpHeadsStore::init(
            store_path,
            &op_heads_addr,
            &op_heads_name,
        )?))
    };

    let (mut workspace, repo) = Workspace::init_with_factories(
        settings,
        &workspace_path,
        backend_init,
        signer,
        op_store_init,
        op_heads_init,
        ReadonlyRepo::default_index_store_initializer(),
        ReadonlyRepo::default_submodule_store_initializer(),
        &*default_working_copy_factory(),
        WorkspaceNameBuf::from(workspace_name.to_string()),
    )
    .context("workspace init failed")?;

    let head_repo = repo
        .loader()
        .load_at_head()
        .context("workspace init failed: cannot load repository head")?;

    // A new workspace starts where the server's default workspace already is,
    // not at the root commit — otherwise every agent would begin on its own
    // island and the first publish would look like a fork.
    let source_parent_commits =
        match head_repo.view().get_wc_commit_id(WorkspaceName::DEFAULT) {
            Some(source_wc_commit_id) => {
                let source_wc_commit = head_repo
                    .store()
                    .get_commit(source_wc_commit_id)
                    .context("workspace init failed: cannot load source workspace commit")?;
                let mut parents = Vec::new();
                for parent_id in source_wc_commit.parent_ids() {
                    let parent = head_repo.store().get_commit(parent_id).with_context(|| {
                        format!("workspace init failed: cannot load source workspace parent {parent_id}")
                    })?;
                    parents.push(parent);
                }
                if parents.is_empty() {
                    vec![head_repo.store().root_commit()]
                } else {
                    parents
                }
            }
            None => vec![head_repo.store().root_commit()],
        };

    let merged_tree = pollster::block_on(jj_lib::rewrite::merge_commit_trees(
        head_repo.as_ref(),
        &source_parent_commits,
    ))
    .context("workspace init failed: cannot merge source workspace parents")?;

    let mut tx = head_repo.start_transaction();
    let parent_ids: Vec<CommitId> = source_parent_commits
        .iter()
        .map(|commit| commit.id().clone())
        .collect();
    let new_wc_commit = tx
        .repo_mut()
        .new_commit(parent_ids, merged_tree)
        .detach()
        .write(tx.repo_mut())
        .context("workspace init failed: cannot create initial working-copy commit")?;

    tx.repo_mut()
        .edit(
            WorkspaceNameBuf::from(workspace_name.to_string()),
            &new_wc_commit,
        )
        .context("workspace init failed: cannot move workspace to source context")?;
    tx.repo_mut()
        .rebase_descendants()
        .context("workspace init failed: cannot rebase rewritten descendants")?;

    let updated_repo = tx
        .commit(format!(
            "create initial working-copy commit in workspace {workspace_name}"
        ))
        .context("workspace init failed: cannot publish initial operation")?;

    workspace
        .check_out(updated_repo.op_id().clone(), None, &new_wc_commit)
        .context("workspace init failed: cannot update working copy checkout")?;

    Ok(workspace_path)
}

/// The tandem factories on top of jj's built-in ones.
///
/// The jj CLI merges the defaults in itself, so `tandem_factories` is right
/// there. Anything else that loads a repo — the simulation harness, a tool —
/// needs the index and submodule stores too.
pub fn tandem_factories_with_defaults() -> StoreFactories {
    let mut factories = StoreFactories::default();
    factories.merge(tandem_factories());
    factories
}
