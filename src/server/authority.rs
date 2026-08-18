//! Who is asking, and whether they may.
//!
//! Two questions, kept apart. [`Server::authority_for`] answers "who is this?" —
//! the API asks it once per request and refuses the request outright when the
//! answer is nobody. Everything else here answers "may they?", which is a
//! question about one workspace and is asked where the work happens.
//!
//! The rules a publish is measured against live in [`super::scope`]; what this
//! module does is assemble the bases it measures against, and prove that each
//! one is state the server itself vouches for.

use anyhow::{anyhow, Context as _, Result};
use jj_lib::object_id::ObjectId as _;
use jj_lib::op_store::{OperationId, View};
use std::collections::BTreeSet;

use super::{scope, writer, ScopeDenied, Server};
use crate::wire;

impl Server {
    /// What a presented bearer authorizes, or `None` for anything else.
    pub fn authority_for(&self, presented: &str) -> Option<crate::auth::Authority> {
        self.tokens.authority_for(presented)
    }

    /// Mint a workspace-scoped bearer. Only the admin token gets here.
    pub(super) fn mint_token_sync(
        &self,
        workspace_id: &str,
        ttl: std::time::Duration,
    ) -> wire::TokenBody {
        let minted = self.tokens.mint(workspace_id, ttl);
        tracing::info!(
            workspace_id = %minted.workspace_id,
            ttl_seconds = minted.ttl.as_secs(),
            "minted a workspace token"
        );
        wire::TokenBody {
            token: minted.token,
            workspace_id: minted.workspace_id,
            ttl_seconds: minted.ttl.as_secs(),
        }
    }

    /// Claim or renew the writer role for one workspace.
    pub(super) fn claim_writer_role_sync(
        &self,
        workspace_id: &str,
        holder: &str,
        ttl: std::time::Duration,
    ) -> std::result::Result<writer::WriterRole, writer::WriterConflict> {
        let outcome = self.writer_roles.claim(workspace_id, holder, ttl);
        match &outcome {
            Ok(role) => tracing::info!(
                workspace_id = %role.workspace_id,
                holder = %role.holder,
                expires_in_seconds = role.expires_in.as_secs(),
                "writer role claimed"
            ),
            Err(conflict) => tracing::info!(
                workspace_id = %conflict.workspace_id,
                holder = %conflict.holder,
                "writer role claim refused; another client holds it"
            ),
        }
        outcome
    }

    /// How many operations a scope check will read to answer a question about
    /// the operation graph.
    ///
    /// An honest stale parent is a handful of operations behind the head it
    /// lost the race to, and the fork point of two divergent heads is a handful
    /// of operations back as well. A walk that has gone further than this is
    /// either a client too far behind to publish anyway or a caller making the
    /// server read the whole operation log for nothing.
    const MAX_HISTORY_WALK: usize = 1024;

    /// Whether every parent of a published operation is one this server serves.
    ///
    /// "Serves" means reachable from a current operation head, which is also
    /// the definition of "an operation this server accepted": nothing gets into
    /// that history except by passing this check, so the history cannot contain
    /// anything a client wrote and never got published.
    ///
    /// The walk goes down from the heads, because that is the direction the
    /// answer lies in — an honest stale parent is an *ancestor* of a head, so
    /// walking up from the parent would go further away from it. It is bounded:
    /// an honest parent is a handful of operations behind the head it lost the
    /// race to, and a walk that has gone further than that is either a client
    /// too far behind to publish anyway or a caller making the server read the
    /// whole operation log for nothing.
    fn check_parents_are_served_history(
        &self,
        new_op: &jj_lib::op_store::Operation,
        head_ids: &[OperationId],
    ) -> std::result::Result<(), ScopeDenied> {
        let op_store = self.repo_loader.op_store();
        let root_operation_id = op_store.root_operation_id().clone();

        let mut wanted: BTreeSet<&OperationId> = new_op
            .parents
            .iter()
            .filter(|parent| **parent != root_operation_id && !head_ids.contains(parent))
            .collect();
        if wanted.is_empty() {
            return Ok(());
        }

        let mut seen: BTreeSet<OperationId> = head_ids.iter().cloned().collect();
        let mut frontier: Vec<OperationId> = head_ids.to_vec();
        let mut read = 0usize;
        while let Some(id) = frontier.pop() {
            if read >= Self::MAX_HISTORY_WALK {
                break;
            }
            read += 1;
            let Ok(op) = pollster::block_on(op_store.read_operation(&id)) else {
                continue;
            };
            for parent in op.parents {
                wanted.remove(&parent);
                if wanted.is_empty() {
                    return Ok(());
                }
                if parent != root_operation_id && seen.insert(parent.clone()) {
                    frontier.push(parent);
                }
            }
        }

        let unreachable = wanted.iter().next().expect("a wanted parent").hex();
        Err(ScopeDenied(format!(
            "this token may not publish an operation built on {unreachable} — \
             that operation is not part of the history this server serves"
        )))
    }

    /// The view of the closest operation every one of `parents` descends from.
    ///
    /// This is the base of the merge the client is publishing, and the scope
    /// check reads it to tell a *removal* from a *never-had*. Leaving a value
    /// out of a merge means one thing when the base had it — some side deleted
    /// it, and jj's merge carries the deletion forward — and another thing when
    /// it did not, in which case the side that lacks it simply predates it and
    /// jj's merge would keep it. Without the base those two look identical, and
    /// a token could delete anything it liked by naming an old enough operation
    /// as a second parent.
    ///
    /// Only a merge needs one: with a single parent the parent *is* the base,
    /// and the check's other rules already say what that means. So the ordinary
    /// publish reads nothing here.
    ///
    /// The walk is a lockstep breadth-first search from each parent, so the
    /// first operation every lane has reached is the closest one they share, up
    /// to the usual tie between two equally close candidates. It is bounded like
    /// every other walk in this file; `None` — no common ancestor found, or none
    /// within the bound — leaves the check with its strict rule, which refuses
    /// rather than admits.
    fn merge_base_view(&self, parents: &[OperationId]) -> Option<View> {
        if parents.len() < 2 {
            return None;
        }
        let op_store = self.repo_loader.op_store();

        let mut reached: Vec<BTreeSet<OperationId>> = parents
            .iter()
            .map(|id| BTreeSet::from([id.clone()]))
            .collect();
        let mut frontier: Vec<Vec<OperationId>> =
            parents.iter().map(|id| vec![id.clone()]).collect();
        let mut budget = Self::MAX_HISTORY_WALK;

        loop {
            if let Some(shared) = first_shared_by_all(&reached) {
                let view_id = pollster::block_on(op_store.read_operation(shared))
                    .ok()?
                    .view_id;
                return pollster::block_on(op_store.read_view(&view_id)).ok();
            }
            if budget == 0 || frontier.iter().all(|lane| lane.is_empty()) {
                return None;
            }
            for lane in 0..frontier.len() {
                let mut next = Vec::new();
                for id in std::mem::take(&mut frontier[lane]) {
                    if budget == 0 {
                        break;
                    }
                    budget -= 1;
                    let Ok(op) = pollster::block_on(op_store.read_operation(&id)) else {
                        continue;
                    };
                    for parent in op.parents {
                        if reached[lane].insert(parent.clone()) {
                            next.push(parent);
                        }
                    }
                }
                frontier[lane] = next;
            }
        }
    }

    /// Whether `workspace_id` is allowed to publish `new_op_id`.
    ///
    /// ── The base, and where it is allowed to come from ──
    ///
    /// The check is a diff, so it needs a base: the state the operation started
    /// from. Two things count, and the second one is the whole of the security
    /// of this check.
    ///
    /// The first is what the server currently serves — the views of its
    /// operation heads. A value that matches one of those introduces no change,
    /// whoever presents it.
    ///
    /// The second is the views of the operation's own parents, and that is
    /// needed: a client that loses a CAS race retries the *same* operation
    /// against the new version, so an honest publish routinely carries values
    /// as of a head that has since been superseded. Its parent is where those
    /// values came from.
    ///
    /// But a parent is only evidence if the server put it in the history
    /// itself. Operations and views are content-addressed and `POST /api/ops`
    /// asks nobody's permission, so a token can write an operation saying
    /// whatever it likes. It used to be able to write one whose view moves
    /// `main`, never publish *it*, and publish a child of it instead — whose
    /// base, read straight out of that parent, agreed that `main` was already
    /// there. The base was the attacker's to choose and the check decided
    /// nothing.
    ///
    /// So every parent has to be an operation this server already serves:
    /// reachable from a current operation head, or the root operation. That
    /// makes the two rules hold each other up — the history reachable from the
    /// heads contains only operations that passed this check, because an
    /// operation that did not pass it never became anybody's parent — and it
    /// closes the other half of the same hole. An operation that descends from
    /// a head *supersedes* it: `reconcile_jj_op_heads` retires any head another
    /// head descends from, so a forged link in the chain is a way to replace
    /// the served view outright, not merely to argue about it.
    pub(super) fn check_publish_scope(
        &self,
        workspace_id: &str,
        new_op_id: &OperationId,
    ) -> Result<()> {
        let op_store = self.repo_loader.op_store();
        let cannot_check = |err| {
            anyhow::Error::new(ScopeDenied(format!(
                "cannot check what operation {} publishes: {err}",
                new_op_id.hex()
            )))
        };
        let new_op =
            pollster::block_on(op_store.read_operation(new_op_id)).map_err(cannot_check)?;
        let new_view =
            pollster::block_on(op_store.read_view(&new_op.view_id)).map_err(cannot_check)?;

        let root_commit_id = self.store.backend().root_commit_id().clone();

        // ── The base: what this server currently serves ──
        let mut head_ids = Vec::new();
        for head_hex in &self.read_jj_op_heads()? {
            head_ids.push(
                crate::hex::from_hex(head_hex)
                    .map(OperationId::new)
                    .with_context(|| format!("operation head {head_hex} is not a hex id"))?,
            );
        }

        // Every parent has to be part of the history this server serves, and
        // it is refused rather than quietly dropped from the base. Dropping it
        // would let an unaccepted operation into the graph as somebody's
        // ancestor, and the next publish would find it reachable and believe
        // it.
        self.check_parents_are_served_history(&new_op, &head_ids)?;

        // A head that cannot be read is not quietly skipped. Skipping one drops
        // a base, and dropping a base only ever makes the check stricter — but
        // it also makes it *wrong*, and an honest client would be refused for a
        // fault on the server's own disk. So a head this server cannot read
        // stops the publish with a 500 the client may retry.
        let view_of = |op_id: &OperationId| -> Result<View> {
            let op = pollster::block_on(op_store.read_operation(op_id))
                .map_err(|err| anyhow!("cannot read operation {}: {err}", op_id.hex()))?;
            pollster::block_on(op_store.read_view(&op.view_id))
                .map_err(|err| anyhow!("cannot read the view of operation {}: {err}", op_id.hex()))
        };

        let inherited: Vec<View> = new_op.parents.iter().map(&view_of).collect::<Result<_>>()?;
        let mut served: Vec<View> = head_ids.iter().map(&view_of).collect::<Result<_>>()?;

        // A repo with no heads and no parents at all — the empty repo is then
        // the only thing there is to measure against. This is the operation
        // `tandem init` records first: jj's root operation itself, which has no
        // parents and whose view is the empty repo.
        if inherited.is_empty() && served.is_empty() {
            served.push(View::make_root(root_commit_id.clone()));
        }
        let merge_base = self.merge_base_view(&new_op.parents);
        let bases = scope::Bases {
            inherited: &inherited,
            merge_base: merge_base.as_ref(),
            served: &served,
        };

        // Whether the second commit is the first one rewritten: same change,
        // different commit. A commit this server cannot read is not a rewrite
        // of anything — an unreadable commit is not evidence for a client.
        let is_rewrite_of = |before: &jj_lib::backend::CommitId,
                             after: &jj_lib::backend::CommitId| {
            let store = &self.store;
            match (
                pollster::block_on(store.get_commit_async(before)),
                pollster::block_on(store.get_commit_async(after)),
            ) {
                (Ok(before), Ok(after)) => before.change_id() == after.change_id(),
                _ => false,
            }
        };

        scope::check_publish(
            workspace_id,
            &bases,
            &new_view,
            &root_commit_id,
            &is_rewrite_of,
        )
        .map_err(|denied| {
            tracing::warn!(
                workspace_id,
                new_id = %new_op_id.hex(),
                reason = %denied,
                "refused a publish outside the token's scope"
            );
            anyhow::Error::new(denied)
        })
    }
}

/// The one operation every lane of a walk has reached, if there is one yet.
///
/// Two lanes can reach two shared operations on the same step, and then either
/// answers the question the caller is asking — both are ancestors of every
/// parent. Picking the smaller id keeps the answer the same on every replay.
fn first_shared_by_all(reached: &[BTreeSet<OperationId>]) -> Option<&OperationId> {
    let (first, rest) = reached.split_first()?;
    first
        .iter()
        .find(|id| rest.iter().all(|lane| lane.contains(*id)))
}
