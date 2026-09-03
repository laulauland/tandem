//! One tandem workspace, driven through jj-lib rather than through `jj`.
//!
//! An agent is what a person's checkout is: its own `.jj` directory, its own
//! store objects, its own cached view of the head version. Everything it does
//! goes over the same HTTP the CLI uses — the only thing missing is the
//! subprocess, and with it the ten milliseconds and the unrepeatable timing
//! that made the old suite slow and flaky at once.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use anyhow::{anyhow, Context, Result};
use jj_lib::backend::{CommitId, CopyId, FileId, TreeValue};
use jj_lib::commit::Commit;
use jj_lib::merge::Merge;
use jj_lib::merged_tree_builder::MergedTreeBuilder;
use jj_lib::object_id::ObjectId as _;
use jj_lib::ref_name::WorkspaceNameBuf;
use jj_lib::repo::{ReadonlyRepo, Repo as _, RepoLoader};
use jj_lib::repo_path::RepoPathBuf;
use jj_lib::settings::UserSettings;
use jj_tandem_client::tandem_factories_with_defaults;
use jj_tandem_workspace::init_tandem_workspace;
use pollster::FutureExt as _;

use super::cluster::Cluster;

pub struct Agent {
    pub name: String,
    pub path: PathBuf,
    /// Every (path, bytes) this agent has written, and the commit it wrote
    /// them in. The oracle reads these back and compares.
    pub written: Vec<WrittenFile>,
    settings: UserSettings,
    workspace_name: WorkspaceNameBuf,
    repo_dir: PathBuf,
}

#[derive(Debug, Clone)]
pub struct WrittenFile {
    pub commit: CommitId,
    pub path: String,
    pub bytes: Vec<u8>,
}

impl Agent {
    /// Create a workspace against `cluster` and return the agent that drives it.
    pub fn join(cluster: &Cluster, name: &str) -> Result<Self> {
        let settings = super::test_settings()?;
        let path = cluster.workspace_root().join(name);
        let path =
            init_tandem_workspace(&settings, &cluster.addr, &cluster.admin_token, name, &path)
                .with_context(|| format!("initialize workspace {name}"))?;
        let repo_dir = dunce::canonicalize(path.join(".jj/repo"))?;
        Ok(Self {
            name: name.to_string(),
            path,
            written: Vec::new(),
            settings,
            workspace_name: WorkspaceNameBuf::from(name.to_string()),
            repo_dir,
        })
    }

    /// A repo loaded from scratch.
    ///
    /// Deliberately not cached: a `Store` memoizes every object it has read, so
    /// an agent that keeps one would answer a read-back check out of its own
    /// memory. A fresh loader means a fresh cache, and a read that really goes
    /// to the server.
    fn loader(&self) -> Result<RepoLoader> {
        RepoLoader::init_from_file_system(
            &self.settings,
            &self.repo_dir,
            &tandem_factories_with_defaults(),
        )
        .with_context(|| format!("{}: load the repo", self.name))
    }

    /// The repo as it is right now, which means as the server says it is.
    pub fn head(&self) -> Result<std::sync::Arc<ReadonlyRepo>> {
        self.loader()?
            .load_at_head()
            .with_context(|| format!("{}: load at head", self.name))
    }

    fn working_commit(&self, repo: &ReadonlyRepo) -> Result<Commit> {
        let id = repo
            .view()
            .get_wc_commit_id(&self.workspace_name)
            .cloned()
            .unwrap_or_else(|| repo.store().root_commit_id().clone());
        repo.store()
            .get_commit(&id)
            .with_context(|| format!("{}: load the working-copy commit", self.name))
    }

    pub fn working_commit_id(&self) -> Result<CommitId> {
        let repo = self.head()?;
        Ok(self.working_commit(&repo)?.id().clone())
    }

    /// Write files into a new commit on top of this agent's working commit and
    /// publish the operation.
    pub fn commit_files(
        &mut self,
        files: &[(String, Vec<u8>)],
        description: &str,
    ) -> Result<CommitId> {
        let repo = self.head()?;
        let store = repo.store().clone();
        let parent = self.working_commit(&repo)?;

        let mut builder = MergedTreeBuilder::new(parent.tree());
        let mut staged: Vec<(String, Vec<u8>)> = Vec::new();
        for (path, bytes) in files {
            let repo_path = RepoPathBuf::from_internal_string(path.clone())
                .map_err(|err| anyhow!("{}: bad repo path {path}: {err}", self.name))?;
            let mut contents = std::io::Cursor::new(bytes.clone());
            let id = store
                .write_file(&repo_path, &mut contents)
                .block_on()
                .with_context(|| format!("{}: write {path}", self.name))?;
            builder.set_or_remove(
                repo_path,
                Merge::normal(TreeValue::File {
                    id,
                    executable: false,
                    copy_id: CopyId::placeholder(),
                }),
            );
            staged.push((path.clone(), bytes.clone()));
        }
        let tree = builder
            .write_tree()
            .with_context(|| format!("{}: write the tree", self.name))?;

        let mut tx = repo.start_transaction();
        let commit = tx
            .repo_mut()
            .new_commit(vec![parent.id().clone()], tree)
            .set_description(description)
            .write()
            .with_context(|| format!("{}: write the commit", self.name))?;
        let commit_id = commit.id().clone();
        tx.repo_mut()
            .edit(self.workspace_name.clone(), &commit)
            .with_context(|| format!("{}: move the working copy", self.name))?;
        tx.repo_mut().rebase_descendants()?;
        tx.commit(format!("{} commits {description}", self.name))
            .with_context(|| format!("{}: publish the operation", self.name))?;

        for (path, bytes) in staged {
            self.written.push(WrittenFile {
                commit: commit_id.clone(),
                path,
                bytes,
            });
        }
        Ok(commit_id)
    }

    /// Rewrite the working commit's description. The interesting part is the
    /// rewrite: two agents describing the same commit is how a change id ends
    /// up on two commit ids if anything in the publish path loses an update.
    pub fn describe(&mut self, description: &str) -> Result<CommitId> {
        let repo = self.head()?;
        let target = self.working_commit(&repo)?;
        if target.id() == repo.store().root_commit_id() {
            return Ok(target.id().clone());
        }

        let mut tx = repo.start_transaction();
        let rewritten = tx
            .repo_mut()
            .rewrite_commit(&target)
            .set_description(description)
            .write()
            .with_context(|| format!("{}: rewrite the commit", self.name))?;
        tx.repo_mut().rebase_descendants()?;
        let new_id = rewritten.id().clone();
        tx.commit(format!("{} describes {description}", self.name))
            .with_context(|| format!("{}: publish the operation", self.name))?;

        // The bytes travel with the rewrite: same tree, new commit id.
        for file in &mut self.written {
            if file.commit == *target.id() {
                file.commit = new_id.clone();
            }
        }
        Ok(new_id)
    }

    /// Everything the oracle needs from one agent, off one repo load.
    ///
    /// Loading a repo is not free — it is several round trips and, when heads
    /// have diverged, a merge — so the oracle takes one picture per agent per
    /// check rather than one per question it asks.
    pub fn snapshot(&self) -> Result<Snapshot> {
        let loader = self.loader()?;
        // Load first: a load is what settles diverged heads, so op heads read
        // before it would describe a state that no longer exists by the time
        // anything is asserted about it.
        let repo = loader
            .load_at_head()
            .with_context(|| format!("{}: load at head", self.name))?;
        let op_heads: BTreeSet<String> = loader
            .op_heads_store()
            .get_op_heads()
            .block_on()
            .with_context(|| format!("{}: read op heads", self.name))?
            .into_iter()
            .map(|id| id.hex())
            .collect();
        let view_heads = repo.view().heads().iter().map(|id| id.hex()).collect();
        let working_copies = repo
            .view()
            .wc_commit_ids()
            .iter()
            .map(|(name, id)| (name.as_str().to_string(), id.hex()))
            .collect();
        Ok(Snapshot {
            name: self.name.clone(),
            op_heads,
            view_heads,
            working_copies,
            repo,
        })
    }

    /// Read one path out of one commit, through the store, over the wire.
    pub fn read_file(&self, commit: &CommitId, path: &str) -> Result<Vec<u8>> {
        let repo = self.head()?;
        read_file_at(&repo, &self.name, commit, path)
    }

    /// The operation heads this agent sees, hex-encoded.
    pub fn op_heads(&self) -> Result<Vec<String>> {
        let loader = self.loader()?;
        let heads = loader
            .op_heads_store()
            .get_op_heads()
            .block_on()
            .with_context(|| format!("{}: read op heads", self.name))?;
        let mut heads: Vec<String> = heads.into_iter().map(|id| id.hex()).collect();
        heads.sort();
        Ok(heads)
    }

    /// Every commit this agent believes it wrote, grouped by commit.
    pub fn expected_files(&self) -> BTreeMap<CommitId, BTreeMap<String, Vec<u8>>> {
        let mut by_commit: BTreeMap<CommitId, BTreeMap<String, Vec<u8>>> = BTreeMap::new();
        for file in &self.written {
            by_commit
                .entry(file.commit.clone())
                .or_default()
                .insert(file.path.clone(), file.bytes.clone());
        }
        by_commit
    }
}

/// One agent's picture of the repo at one moment.
pub struct Snapshot {
    pub name: String,
    /// The operation heads this agent's store reports. It may hold this
    /// agent's own workspace head in addition to the server's global set.
    pub op_heads: BTreeSet<String>,
    /// The commit heads of the view this agent resolved to.
    pub view_heads: BTreeSet<String>,
    /// Workspace name → working-copy commit, hex.
    pub working_copies: BTreeMap<String, String>,
    repo: std::sync::Arc<ReadonlyRepo>,
}

impl Snapshot {
    /// What the agent that took this snapshot sees at `path` in `commit`.
    pub fn read_file(&self, commit: &CommitId, path: &str) -> Result<Vec<u8>> {
        read_file_at(&self.repo, &self.name, commit, path)
    }

    /// The object id of the file at `path` in `commit`, hex-encoded — the
    /// handle a caller needs to ask the server for the same bytes without a
    /// client in the way.
    pub fn file_id(&self, commit: &CommitId, path: &str) -> Result<String> {
        let (_, id) = file_at(&self.repo, &self.name, commit, path)?;
        Ok(id.hex())
    }

    /// The commits reachable from this view, and the change id of each.
    pub fn reachable_commits(&self) -> Result<BTreeMap<String, BTreeSet<String>>> {
        let mut by_change: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        let mut seen: BTreeSet<String> = BTreeSet::new();
        let mut frontier: Vec<CommitId> = self.repo.view().heads().iter().cloned().collect();
        while let Some(id) = frontier.pop() {
            if !seen.insert(id.hex()) {
                continue;
            }
            let commit = self
                .repo
                .store()
                .get_commit(&id)
                .with_context(|| format!("{}: load commit {}", self.name, id.hex()))?;
            by_change
                .entry(commit.change_id().hex())
                .or_default()
                .insert(id.hex());
            frontier.extend(commit.parent_ids().iter().cloned());
        }
        Ok(by_change)
    }
}

/// The file object at `path` in `commit`, and the repo path that names it.
///
/// `who` is the agent asking, and it is in every message: a failure here is
/// read back off a schedule that ran six agents, and "cannot read src/a.txt"
/// without a name does not say which one.
fn file_at(
    repo: &ReadonlyRepo,
    who: &str,
    commit: &CommitId,
    path: &str,
) -> Result<(RepoPathBuf, FileId)> {
    let commit = repo
        .store()
        .get_commit(commit)
        .with_context(|| format!("{who}: load commit {}", commit.hex()))?;
    let repo_path = RepoPathBuf::from_internal_string(path.to_string())
        .map_err(|err| anyhow!("{who}: bad repo path {path}: {err}"))?;
    let value = commit
        .tree()
        .path_value(&repo_path)
        .with_context(|| format!("{who}: look up {path}"))?;
    let Some(TreeValue::File { id, .. }) = value.as_normal() else {
        return Err(anyhow!(
            "{who}: {path} is not a plain file in {}",
            commit.id().hex()
        ));
    };
    Ok((repo_path, id.clone()))
}

/// The bytes of that file, read through the store — which means over the wire,
/// out of whatever this repo's store has not already cached.
fn read_file_at(repo: &ReadonlyRepo, who: &str, commit: &CommitId, path: &str) -> Result<Vec<u8>> {
    let (repo_path, id) = file_at(repo, who, commit, path)?;
    let mut reader = repo
        .store()
        .read_file(&repo_path, &id)
        .block_on()
        .with_context(|| format!("{who}: read {path}"))?;
    let mut bytes = Vec::new();
    {
        use tokio::io::AsyncReadExt as _;
        reader
            .read_to_end(&mut bytes)
            .block_on()
            .with_context(|| format!("{who}: drain {path}"))?;
    }
    Ok(bytes)
}
