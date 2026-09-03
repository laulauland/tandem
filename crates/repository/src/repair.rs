//! Putting back a workspace a merge settled on an interrupted clone's
//! placeholder.
//!
//! The rule itself — what a fresh workspace's placeholder commit looks like —
//! is [`jj_tandem_jj::placeholder`], shared with the client that writes one. This is
//! the server's half: it runs over whatever operation the server is about to
//! serve, and it moves a working-copy pointer *off* a placeholder and back onto
//! the work another merged side still has.
//!
//! Everything here is best effort by construction. It runs past the point of no
//! return, after a publish is durable and acknowledged, so a head it cannot
//! read or repair is served exactly as it would have been without this.

use anyhow::{anyhow, Result};
use jj_lib::backend::CommitId;
use jj_lib::object_id::ObjectId as _;
use jj_lib::op_store::OperationId;

use super::Repository;
use jj_tandem_protocol::hex::from_hex;

impl Repository {
    /// Put back a working-copy pointer a merge settled on a fresh clone's
    /// placeholder.
    ///
    /// ── Why a merge can lose a workspace ──
    ///
    /// When both sides of a merge move the same workspace's working-copy
    /// pointer and their common ancestor knows nothing about that workspace,
    /// jj has no ancestry argument to settle it with and keeps the *base*
    /// side's answer (`MutableRepo::merge_wc_commit`). The base is whichever
    /// operation sorted first, and the sort key is an end time written by a
    /// client's clock. A clone that died between its two operations leaves a
    /// head pointing the name at the empty commit workspace creation makes; on
    /// a machine whose clock is behind, that head sorts first and the workspace
    /// is handed an empty tree. The next clone of the name then sees one head,
    /// agreeing, saying empty — and materializes nothing.
    ///
    /// ── Why ordering alone is not the fix ──
    ///
    /// `order_op_heads` pins the arriving operation last, which settles the
    /// case where this server is the one merging and the leftover is what just
    /// arrived. It cannot settle the other two. `reconcile_jj_op_heads` is
    /// best-effort and documented to leave heads unmerged when it cannot order
    /// or merge them; the leftover then survives as a sibling head, and the
    /// next merge — this server's, with some *other* operation arriving, or a
    /// client's own, because `jj_lib::op_heads_store::resolve_op_heads` orders
    /// by end time with no pin at all and publishes the result — puts the
    /// leftover in the base seat after all.
    ///
    /// So the rule is stated about the answer instead of about the order: a
    /// merge may not settle a workspace on the placeholder of a side that is
    /// *nothing but a fresh workspace creation* while another side it merged
    /// still has that workspace on something real. This runs on whatever head
    /// the server is about to serve, so it catches a merge whoever made it.
    ///
    /// What keeps this from undoing legitimate work is the shape of the side it
    /// acts on, not the shape of the commit — see `fresh_workspace_creation`. A
    /// workspace that abandons everything it had also lands on an empty commit,
    /// and a stale head can still hold what it abandoned, so a rule that read
    /// the commit alone would put the abandoned work back. Only merges are
    /// examined, for the same reason.
    pub(super) fn repair_merged_workspace_pointers(
        &self,
        settled: &jj_lib::operation::Operation,
    ) -> Result<Option<jj_lib::operation::Operation>> {
        if settled.parent_ids().len() < 2 {
            return Ok(None);
        }

        let op_store = self.repo_loader.op_store().clone();
        let read_view = |op: &jj_lib::operation::Operation| -> Result<jj_lib::op_store::View> {
            pollster::block_on(op_store.read_view(op.view_id()))
                .map_err(|e| anyhow!("read the view of operation {}: {e}", op.id().hex()))
        };

        let settled_view = read_view(settled)?;

        // The parents, and — for a parent that carries the settled view
        // unchanged — that parent's parents too. `record_merged_parents` wraps
        // a merge in an operation whose view is the merge's own, so the merged
        // sides sit one step further back; without this the wrapper would be
        // examined against a set of views that no longer holds the answer.
        let mut parent_views = Vec::new();
        for parent in settled.parents() {
            let parent = parent.map_err(|e| anyhow!("read a parent operation: {e}"))?;
            if parent.view_id() == settled.view_id() {
                for grandparent in parent.parents() {
                    let grandparent =
                        grandparent.map_err(|e| anyhow!("read a parent operation: {e}"))?;
                    let view = read_view(&grandparent)?;
                    parent_views.push((grandparent, view));
                }
            }
            let view = read_view(&parent)?;
            parent_views.push((parent, view));
        }

        let mut repaired = settled_view.clone();
        let mut restored: Vec<String> = Vec::new();
        let interrupted_clones: Vec<_> = parent_views
            .iter()
            .filter_map(|(op, view)| self.fresh_workspace_creation(op, view))
            .collect();
        for (name, placeholder) in interrupted_clones {
            // Only the pointer that side actually won is moved.
            if settled_view.wc_commit_ids.get(name) != Some(placeholder) {
                continue;
            }
            // The first parent that still has this workspace on work. Parent
            // order is the operation's own, so the answer does not depend on
            // anybody's clock.
            let Some(real) = parent_views
                .iter()
                .filter_map(|(_, view)| view.wc_commit_ids.get(name))
                .find(|candidate| {
                    *candidate != placeholder && !self.is_fresh_clone_placeholder(candidate)
                })
            else {
                continue;
            };
            repaired.wc_commit_ids.insert(name.clone(), real.clone());
            // The merge dropped this commit from the workspace, and it may
            // have dropped it from the head set with it. Nothing else would
            // put it back.
            repaired.head_ids.insert(real.clone());
            restored.push(format!("{} -> {}", name.as_str(), real.hex()));
        }

        if restored.is_empty() {
            return Ok(None);
        }

        tracing::warn!(
            settled = %settled.id().hex(),
            restored = restored.join(", "),
            "a merge settled a workspace on an interrupted clone's empty commit; putting it back"
        );

        let view_id = pollster::block_on(op_store.write_view(&repaired))
            .map_err(|e| anyhow!("write the repaired view: {e}"))?;
        let now = jj_lib::backend::Timestamp::now();
        let data = jj_lib::op_store::Operation {
            view_id,
            parents: vec![settled.id().clone()],
            metadata: jj_lib::op_store::OperationMetadata {
                time: jj_lib::op_store::TimestampRange {
                    start: now,
                    end: now,
                },
                description: "restore a workspace a merge settled on an empty commit".to_string(),
                hostname: settled.metadata().hostname.clone(),
                username: settled.metadata().username.clone(),
                is_snapshot: false,
                tags: std::collections::HashMap::new(),
            },
            commit_predecessors: None,
        };
        let id = pollster::block_on(op_store.write_operation(&data))
            .map_err(|e| anyhow!("write the operation restoring a workspace pointer: {e}"))?;
        Ok(Some(jj_lib::operation::Operation::new(op_store, id, data)))
    }

    /// The workspace an operation names, when that operation is nothing but a
    /// workspace being created.
    ///
    /// This is the whole safety argument of the repair, so it is deliberately
    /// narrow. `Workspace::init_with_factories` starts a fresh repo at the root
    /// operation, checks the new workspace out, and commits the transaction
    /// under a description jj writes itself. The operation a clone publishes
    /// next is therefore recognisable three times over: it is described `add
    /// workspace '<name>'`, and it carries a view with exactly one working-copy
    /// pointer, exactly one head — the placeholder commit that pointer names —
    /// and no bookmark, tag, or git reference at all.
    ///
    /// The description is what a view alone cannot supply. A workspace that
    /// abandons everything it had lands on the same shape of empty commit, and
    /// in a repo whose only workspace is that one, on the same shape of view.
    /// Undoing such an abandon would be the same data loss in the other
    /// direction, so the repair acts only on an operation jj itself wrote for a
    /// workspace that did not exist a moment earlier.
    fn fresh_workspace_creation<'a>(
        &self,
        op: &jj_lib::operation::Operation,
        view: &'a jj_lib::op_store::View,
    ) -> Option<(&'a jj_lib::ref_name::WorkspaceNameBuf, &'a CommitId)> {
        if !view.local_bookmarks.is_empty()
            || !view.local_tags.is_empty()
            || !view.remote_views.is_empty()
            || !view.git_refs.is_empty()
            || view.git_head.is_present()
            || view.wc_commit_ids.len() != 1
            || view.head_ids.len() != 1
        {
            return None;
        }
        let (name, commit_id) = view.wc_commit_ids.iter().next()?;
        // The name is taken from the view rather than parsed back out of the
        // description: jj writes it there as a revset symbol, which quotes and
        // escapes, and the view already holds it unambiguously.
        if op.metadata().description != format!("add workspace '{}'", name.as_symbol()) {
            return None;
        }
        if !view.head_ids.contains(commit_id) || !self.is_fresh_clone_placeholder(commit_id) {
            return None;
        }
        Some((name, commit_id))
    }

    /// Whether a commit is the one a clone's workspace creation leaves behind.
    ///
    /// The rule is [`jj_tandem_jj::placeholder`]; this reads the commit out of the
    /// server's own store to ask it. A commit this server cannot read is not
    /// treated as a placeholder: the repair only ever moves a pointer *off*
    /// one, so not knowing has to mean leaving the merge alone.
    fn is_fresh_clone_placeholder(&self, commit_id: &CommitId) -> bool {
        let backend = self.store.backend();
        if commit_id == backend.root_commit_id() {
            return true;
        }
        let Ok(commit) = pollster::block_on(self.store.get_commit_async(commit_id)) else {
            return false;
        };
        let parents: Vec<&[u8]> = commit.parent_ids().iter().map(|id| id.as_bytes()).collect();
        let root_tree: Vec<&[u8]> = commit.tree_ids().iter().map(|id| id.as_bytes()).collect();
        jj_tandem_jj::placeholder::is_fresh_workspace_placeholder(
            commit.description(),
            &parents,
            &root_tree,
            backend.root_commit_id().as_bytes(),
            backend.empty_tree_id().as_bytes(),
        )
    }

    /// Run the repair over every head the server is about to serve.
    ///
    /// Best effort, and deliberately so: it runs past the point of no return,
    /// where nothing may fail the publish. A head it cannot repair is served
    /// as it is, exactly as it would have been without this.
    pub(super) fn repair_placeholder_merges(&self, heads: &[String]) -> Vec<String> {
        let op_store = self.repo_loader.op_store().clone();
        let mut settled: Vec<String> = heads.to_vec();
        for head_hex in heads {
            let Ok(bytes) = from_hex(head_hex) else {
                continue;
            };
            let op_id = OperationId::new(bytes);
            let head = match pollster::block_on(op_store.read_operation(&op_id)) {
                Ok(data) => {
                    jj_lib::operation::Operation::new(op_store.clone(), op_id.clone(), data)
                }
                Err(err) => {
                    tracing::warn!(op_id = %head_hex, error = %err, "cannot read a head to check it for a lost workspace");
                    continue;
                }
            };
            let repaired = match self.repair_merged_workspace_pointers(&head) {
                Ok(Some(repaired)) => repaired,
                Ok(None) => continue,
                Err(err) => {
                    tracing::warn!(op_id = %head_hex, error = %err, "could not restore a workspace a merge lost; serving the merge as it is");
                    continue;
                }
            };
            if let Err(err) = pollster::block_on(
                self.op_heads_store
                    .update_op_heads(std::slice::from_ref(&op_id), repaired.id()),
            ) {
                tracing::warn!(op_id = %head_hex, error = %err, "could not record the restored operation as a head");
                continue;
            }
            settled = match self.read_jj_op_heads() {
                Ok(heads) => heads,
                Err(err) => {
                    tracing::warn!(error = %err, "could not re-read the op heads after a restore");
                    return settled;
                }
            };
        }
        settled
    }
}
