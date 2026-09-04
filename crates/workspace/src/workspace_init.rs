//! Putting a tandem workspace on disk.
//!
//! This is the body of `tandem clone` (and of `tandem init`, its older
//! spelling). It lives in the library rather than in the CLI so that the
//! simulation harness can create its agents through the same code the product
//! runs — an in-process test that hand-rolls its own workspace setup is
//! testing the hand-rolled setup, not tandem.
//!
//! A clone either creates the workspace or attaches to one the server already
//! has. Attaching is what makes a workspace outlive the machine it was on: the
//! files come back from the last published snapshot, not from a fresh start.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use jj_lib::backend::CommitId;
use jj_lib::object_id::ObjectId as _;
use jj_lib::ref_name::{WorkspaceName, WorkspaceNameBuf};
use jj_lib::repo::{ReadonlyRepo, Repo as _};
use jj_lib::settings::UserSettings;
use jj_lib::signing::Signer;
use jj_lib::workspace::{default_working_copy_factory, Workspace};
use prost::Message as _;

use jj_tandem_client::{backend, op_heads_store, op_store, TandemClient};
use jj_tandem_jj::{placeholder, proto_convert};

/// What `Workspace::init_with_factories` wants for each store it creates: a
/// closure from settings and a store path to a boxed implementation. Naming the
/// three shapes keeps the call site readable.
type BackendInit<'a> =
    &'a dyn Fn(
        &UserSettings,
        &Path,
    ) -> Result<Box<dyn jj_lib::backend::Backend>, jj_lib::backend::BackendInitError>;

type OpStoreInit<'a> =
    &'a dyn Fn(
        &UserSettings,
        &Path,
        jj_lib::op_store::RootOperationData,
    )
        -> Result<Box<dyn jj_lib::op_store::OpStore>, jj_lib::backend::BackendInitError>;

type OpHeadsInit<'a> = &'a dyn Fn(
    &UserSettings,
    &Path,
) -> Result<
    Box<dyn jj_lib::op_heads_store::OpHeadsStore>,
    jj_lib::backend::BackendInitError,
>;

/// Turn whatever token the caller was given into the one this workspace keeps.
///
/// A person setting up a workspace has the admin token — that is the one the
/// server printed when it started — so the first thing init does is trade it
/// for a token scoped to this workspace, and that scoped one is what gets
/// written to disk. Handing an admin token to a store trait would give every
/// later `jj` command in the workspace the authority to move `main`.
///
/// A caller who already holds a workspace token is refused the trade, and
/// keeps what it has: that is how a workspace is set up somewhere the admin
/// token is deliberately not present.
pub fn workspace_token(server_addr: &str, token: &str, workspace_name: &str) -> Result<String> {
    let client = TandemClient::connect(server_addr, token)
        .with_context(|| format!("cannot reach the tandem server at {server_addr}"))?;
    match client
        .mint_workspace_token(workspace_name, None)
        .context("cannot mint a workspace token")?
    {
        Some(minted) => Ok(minted.token),
        None => Ok(token.to_string()),
    }
}

/// Whether a clone made the workspace or found it already published.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkspaceOrigin {
    /// Nothing on the server spoke for this name, so the clone created it.
    Created,
    /// The server already had a working-copy commit for this name, and the
    /// clone materialized that instead of starting over.
    Attached,
}

impl WorkspaceOrigin {
    pub fn as_str(self) -> &'static str {
        match self {
            WorkspaceOrigin::Created => "created",
            WorkspaceOrigin::Attached => "attached",
        }
    }
}

/// The environment variable that stops a clone halfway, where a kill would.
///
/// A clone of an existing name publishes two operations: the one jj's own
/// workspace creation makes, and the one that points the name at what it is
/// attaching to. Between them the server holds two heads that disagree about
/// this workspace, and that window is what [`published_workspace_head`] has to
/// survive. Only a test sets this; it leaves the process the way a kill would,
/// with the first operation published and the second one never written.
pub const ABORT_AFTER_WORKSPACE_INIT_ENV: &str = "TANDEM_TEST_ABORT_AFTER_WORKSPACE_INIT";

/// The commit a workspace's last published snapshot left on the server, if it
/// has one.
///
/// Asked before anything is written to disk, and for a reason: creating a jj
/// workspace publishes an operation that points the name at a fresh empty
/// commit, so after that the answer is always "a new one". The heads are read
/// rather than a repo loaded because there is no repo yet — a view carries the
/// working-copy commit of every workspace, so one read of one head answers it.
///
/// The heads do not always agree. A clone that died between its two operations
/// leaves the name pointing at its own fresh empty commit on one head and at
/// the last real snapshot on another, and a merge of the two would pick between
/// them by nothing better than iteration order — jj resolves a working-copy
/// pointer that moved on both sides by taking one side, because a working copy
/// has no ancestry argument to settle it with. So the disagreement is settled
/// here, on the one thing that distinguishes the two: an interrupted clone's
/// commit is the empty commit on the root commit that jj's workspace creation
/// makes, and a published snapshot is anything else. See
/// `tests/integration/clone.rs::a_clone_killed_between_its_two_operations_does_not_cost_the_next_one_its_files`.
fn published_workspace_head(
    server_addr: &str,
    token: &str,
    workspace_name: &str,
) -> Result<Option<CommitId>> {
    let client = TandemClient::connect(server_addr, token)
        .with_context(|| format!("cannot reach the tandem server at {server_addr}"))?;
    let name = WorkspaceNameBuf::from(workspace_name.to_string());

    let heads = client
        .get_heads_state()
        .context("cannot read the server's operation heads")?;

    // Every distinct commit any head names for this workspace, in the order the
    // server listed the heads.
    let mut candidates: Vec<CommitId> = Vec::new();
    for head in &heads.heads {
        // The synthetic root has no stored operation/view or workspace pointer.
        if head == &client.repo_info().root_operation_id {
            continue;
        }
        let operation_bytes = client
            .get_operation(head)
            .context("cannot read an operation the server named as a head")?;
        let operation_proto = jj_lib::protos::simple_op_store::Operation::decode(&*operation_bytes)
            .context("cannot decode an operation")?;
        let operation = proto_convert::operation_from_proto(operation_proto)
            .context("cannot read operation")?;

        let view_bytes = client
            .get_view(operation.view_id.as_bytes())
            .context("cannot read the view an operation points at")?;
        let view_proto = jj_lib::protos::simple_op_store::View::decode(&*view_bytes)
            .context("cannot decode a view")?;
        let view = proto_convert::view_from_proto(view_proto).context("cannot read view")?;

        if let Some(commit_id) = view.wc_commit_ids.get(&name) {
            if !candidates.contains(commit_id) {
                candidates.push(commit_id.clone());
            }
        }
    }

    match candidates.len() {
        0 => return Ok(None),
        // The ordinary case, and the only one that costs nothing extra: every
        // head that speaks for this name says the same thing.
        1 => return Ok(Some(candidates.remove(0))),
        _ => {}
    }

    // The heads disagree. Drop the ones that are an unfinished clone's
    // placeholder — if that leaves anything, it is the real snapshot.
    let mut real = Vec::new();
    for candidate in &candidates {
        if !is_fresh_workspace_placeholder(&client, candidate)
            .context("cannot read a working-copy commit the server named")?
        {
            real.push(candidate.clone());
        }
    }

    let chosen = real.first().or_else(|| candidates.first());
    if real.len() != candidates.len() {
        eprintln!(
            "note: {} operation heads disagree about workspace {workspace_name}; attaching to the \
             last published snapshot and ignoring {} placeholder(s) left by an interrupted clone",
            candidates.len(),
            candidates.len() - real.len()
        );
    }
    Ok(chosen.cloned())
}

/// Whether a commit is the placeholder jj's workspace creation makes, rather
/// than something a workspace published.
///
/// The rule itself is [`jj_tandem_jj::placeholder`]; this is the client's way of
/// bringing a commit to it, over HTTP and without a repo.
fn is_fresh_workspace_placeholder(client: &TandemClient, commit_id: &CommitId) -> Result<bool> {
    let info = client.repo_info();
    if commit_id.as_bytes() == info.root_commit_id.as_slice() {
        // The root commit itself is nobody's published snapshot either.
        return Ok(true);
    }
    let data = client
        .get_object(jj_tandem_protocol::wire::KIND_COMMIT, commit_id.as_bytes())
        .with_context(|| format!("cannot read commit {}", commit_id.hex()))?;
    let commit = jj_lib::protos::simple_store::Commit::decode(&*data)
        .with_context(|| format!("cannot decode commit {}", commit_id.hex()))?;
    let parents: Vec<&[u8]> = commit.parents.iter().map(Vec::as_slice).collect();
    let root_tree: Vec<&[u8]> = commit.root_tree.iter().map(Vec::as_slice).collect();
    Ok(placeholder::is_fresh_workspace_placeholder(
        &commit.description,
        &parents,
        &root_tree,
        &info.root_commit_id,
        &info.empty_tree_id,
    ))
}

/// Create a tandem workspace at `workspace_path` and give it a working-copy
/// commit in the same context as the server's default workspace.
///
/// The older spelling of [`clone_tandem_workspace`], kept because `tandem
/// init` and the simulation harness both still say it.
pub fn init_tandem_workspace(
    settings: &UserSettings,
    server_addr: &str,
    token: &str,
    workspace_name: &str,
    workspace_path: &Path,
) -> Result<PathBuf> {
    let (path, _origin) =
        clone_tandem_workspace(settings, server_addr, token, workspace_name, workspace_path)?;
    Ok(path)
}

/// Put the workspace named `workspace_name` on disk at `workspace_path` and
/// materialize its files.
///
/// If the server already has a working-copy commit for the name, this attaches
/// to it: the files that come back are the ones the last published snapshot
/// held, byte for byte, wherever the machine that published them has gone.
/// Otherwise it creates the workspace where the server's default workspace
/// already is — not at the root commit, or every agent would begin on its own
/// island and the first publish would look like a fork.
///
/// `token` is either the server's admin token or a token already scoped to
/// this workspace; see [`workspace_token`].
///
/// Returns the canonical path of the workspace and which of the two happened.
pub fn clone_tandem_workspace(
    settings: &UserSettings,
    server_addr: &str,
    token: &str,
    workspace_name: &str,
    workspace_path: &Path,
) -> Result<(PathBuf, WorkspaceOrigin)> {
    std::fs::create_dir_all(workspace_path).context("cannot create workspace directory")?;
    let workspace_path = workspace_path
        .canonicalize()
        .context("cannot resolve workspace path")?;

    let signer = Signer::from_settings(settings).context("cannot create signer")?;

    let scoped_token = workspace_token(server_addr, token, workspace_name)?;

    // Asked before a byte is written, because creating the workspace below
    // publishes an operation that points this name at a new empty commit.
    let published_head = published_workspace_head(server_addr, &scoped_token, workspace_name)
        .context("cannot ask the server whether this workspace already exists")?;

    let backend_addr = server_addr.to_string();
    let op_store_addr = server_addr.to_string();
    let op_heads_addr = server_addr.to_string();
    let op_heads_name = workspace_name.to_string();
    let backend_token = scoped_token.clone();
    let op_store_token = scoped_token.clone();
    let op_heads_token = scoped_token.clone();

    let backend_init: BackendInit = &|_settings, store_path| {
        Ok(Box::new(backend::TandemBackend::init(
            store_path,
            &backend_addr,
            &backend_token,
        )?))
    };

    let op_store_init: OpStoreInit = &|_settings, store_path, root_data| {
        Ok(Box::new(op_store::TandemOpStore::init(
            store_path,
            &op_store_addr,
            &op_store_token,
            root_data,
        )?))
    };

    let op_heads_init: OpHeadsInit = &|_settings, store_path| {
        Ok(Box::new(op_heads_store::TandemOpHeadsStore::init(
            store_path,
            &op_heads_addr,
            &op_heads_token,
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

    // The fault injector for the window between this clone's two operations.
    // `exit` and not an error: what it stands in for is the machine going away,
    // and an error would unwind and tidy up, which a kill does not.
    if std::env::var_os(ABORT_AFTER_WORKSPACE_INIT_ENV).is_some() {
        eprintln!("stopping after workspace init: {ABORT_AFTER_WORKSPACE_INIT_ENV} is set");
        std::process::exit(137);
    }

    // ── Attach ────────────────────────────────────────────────────────
    //
    // The name was already somebody's. Point the workspace back at the commit
    // that name last published and check it out: the files on disk are then
    // the last snapshot, which is the whole promise of re-cloning a workspace
    // somewhere else.
    //
    // The transaction starts from `repo` — what workspace creation just made —
    // and not from the server's merged head. That is a scope requirement and
    // not a preference. Creating the workspace published an operation pointing
    // this name at a fresh empty commit; moving the name off it retires that
    // commit, and a workspace token may only retire a head some *base* of the
    // operation calls its own. In the merged head that base is gone — the merge
    // already resolved the name to the published commit — so the same
    // transaction built there reads as this workspace deleting somebody else's
    // head, and the server refuses it. Built here, the empty commit is still
    // this workspace's own, and retiring it is exactly what a workspace token
    // is for. The operation lands as a sibling of the published head and jj's
    // own reconcile merges the two on the next load, which is the normal way
    // two workspaces publish at once.
    if let Some(published_head) = published_head {
        let existing_commit = repo
            .store()
            .get_commit(&published_head)
            .context("workspace attach failed: cannot load the published working-copy commit")?;

        let mut tx = repo.start_transaction();
        // `edit` and not `set_wc_commit`: it retires the empty commit creation
        // made, which the paragraph above is about. It only retires one that is
        // still empty and undescribed, so a directory somebody had already
        // written in keeps its work.
        tx.repo_mut()
            .edit(
                WorkspaceNameBuf::from(workspace_name.to_string()),
                &existing_commit,
            )
            .context("workspace attach failed: cannot point the workspace at its last snapshot")?;
        tx.repo_mut()
            .rebase_descendants()
            .context("workspace attach failed: cannot rebase rewritten descendants")?;
        let updated_repo = tx
            .commit(format!("attach workspace {workspace_name}"))
            .context("workspace attach failed: cannot publish the attach operation")?;

        workspace
            .check_out(updated_repo.op_id().clone(), None, &existing_commit)
            .context("workspace attach failed: cannot materialize the working copy")?;

        return Ok((workspace_path, WorkspaceOrigin::Attached));
    }

    // A new workspace starts where the server's default workspace already is,
    // not at the root commit — otherwise every agent would begin on its own
    // island and the first publish would look like a fork. Which means reading
    // the server's merged head, where the default workspace's pointer is.
    let head_repo = repo
        .loader()
        .load_at_head()
        .context("workspace init failed: cannot load repository head")?;

    let source_parent_commits = match head_repo.view().get_wc_commit_id(WorkspaceName::DEFAULT) {
        Some(source_wc_commit_id) => {
            let source_wc_commit = head_repo
                .store()
                .get_commit(source_wc_commit_id)
                .context("workspace init failed: cannot load source workspace commit")?;
            let mut parents = Vec::new();
            for parent_id in source_wc_commit.parent_ids() {
                let parent = head_repo.store().get_commit(parent_id).with_context(|| {
                    format!(
                        "workspace init failed: cannot load source workspace parent {parent_id}"
                    )
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

    Ok((workspace_path, WorkspaceOrigin::Created))
}

/// The jj configuration this machine's user has, as jj itself resolves it.
///
/// Anything that touches a workspace outside the jj CLI needs this: a commit
/// is signed and attributed from it, and a working copy is snapshotted under
/// the rules in it.
pub fn user_settings_from_environment() -> Result<UserSettings> {
    let config_env = jj_cli::config::ConfigEnv::from_environment();
    let mut raw_config =
        jj_cli::config::config_from_environment(jj_cli::config::default_config_layers());
    config_env
        .reload_user_config(&mut raw_config)
        .map_err(|e| anyhow::anyhow!("cannot load jj user config: {e}"))?;
    let resolved = config_env
        .resolve_config(&raw_config)
        .map_err(|e| anyhow::anyhow!("cannot resolve jj config: {e}"))?;
    UserSettings::from_config(resolved).map_err(|e| anyhow::anyhow!("cannot create settings: {e}"))
}
