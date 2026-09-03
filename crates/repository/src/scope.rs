//! What a workspace token is allowed to publish.
//!
//! Invariant 4 of the design doc: a workspace token may add commits, move its
//! own workspace pointer, and move bookmarks in its own namespace — nothing
//! else. The check is a diff of the operation's view against a set of *base*
//! views: outside its own namespace, a token may publish only values that some
//! base already carries.
//!
//! What counts as a base is decided by the caller, and it is the whole of the
//! security of this check. Operations and views are content-addressed and any
//! token may `POST` one, so an object the publisher wrote is not evidence of
//! anything: a base read out of an unpublished operation is a base the
//! attacker chose, and a check measured against it decides nothing. Every base
//! has to be state the server vouches for — a view of an operation head it
//! serves, or of an operation it published earlier and can still reach from
//! one. `Repository::check_publish_scope` is where that set is assembled, and it
//! says so at greater length.
//!
//! Several bases are the ordinary case: a server serves several operation
//! heads whenever two clients publish concurrently, and a client publishes the
//! merge of them. A value already in *any* base was not introduced by this
//! operation, whoever put it there, so that is the test for a value. The test
//! for an *absence* is the opposite quantifier — see `Bases`, which is where
//! the difference is set out.

use std::collections::{BTreeMap, BTreeSet};

use jj_lib::backend::CommitId;
use jj_lib::object_id::ObjectId as _;
use jj_lib::op_store::{RefTarget, View};

fn namespace_prefix(workspace_id: &str) -> String {
    format!("{workspace_id}/")
}

/// A publish this token is not allowed to make.
///
/// Its own error type so the API can answer it 403 rather than 500, and so
/// that nothing on the durable path mistakes it for a fault.
#[derive(Debug)]
pub struct ScopeDenied(pub String);

impl std::fmt::Display for ScopeDenied {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ScopeDenied {}

fn denied(message: impl Into<String>) -> ScopeDenied {
    ScopeDenied(message.into())
}

/// A ref target as an optional value: an absent target is no value at all,
/// which is how the rest of this module reads a name that is not there.
fn present(target: &RefTarget) -> Option<&RefTarget> {
    target.is_present().then_some(target)
}

/// The two kinds of base a publish is measured against.
///
/// The distinction is not bookkeeping — it is what tells a stale value from a
/// deletion.
///
/// `inherited` is the views of the operation's own parents: the state it
/// descends from, and therefore the state it *replaces*. Anything it leaves out
/// of its view is left out for good, because the reconcile retires a head that
/// another head descends from.
///
/// `served` is the views of the operation heads the server holds right now.
/// Those survive the publish: an operation that is not their descendant sits
/// beside them and the reconcile merges the two. So a value only they carry
/// cannot be lost by leaving it out, and a value they carry is one the server
/// is already serving to everybody.
///
/// `merge_base` is the view of the closest common ancestor of those parents,
/// when there is more than one of them. It carries no permission of its own; it
/// is there to answer one question that the parents alone cannot — see below.
///
/// Hence the asymmetry. A *value* may match either kind — carrying forward what
/// the server already serves changes nothing, whoever presents it.
///
/// An *absence* is harder, because leaving a thing out of a merge is how jj
/// records a deletion and also how it records "the other side never had this".
/// Three things make one legitimate:
///
/// 1. No parent had it. Then the operation never had the thing to begin with
///    and is not what took it away.
/// 2. The merge base had it and some parent does not. Then that parent removed
///    it, in an operation this server already accepted, and jj's own merge
///    would carry the removal forward — this publish is only propagating it.
/// 3. No head the server serves has it. Then it is not in the state being
///    served at all, and leaving it out subtracts nothing from anybody.
///
/// Rule 2 is the whole reason the merge base is read. Without it the rule would
/// have to be "some parent lacks it", and that is forgeable: a token may pick
/// as a second parent any operation the server serves — including one old
/// enough to predate somebody else's bookmark — and call the pair a merge. jj
/// would merge them by keeping the bookmark, because the side that lacks it
/// never had it; a hand-built "merge" that drops it would read as a deletion
/// inherited from a parent. Asking whether the merge base had the thing is
/// exactly what tells a removal from a never-had.
///
/// And rule 3 is "no head" rather than "some head" for the same reason in the
/// other direction. A server serves several heads whenever the reconcile could
/// not order or merge them, and then they disagree about what exists. One of
/// them having lost a value is not evidence the value is gone — the reconcile
/// carries it forward from the head that still has it.
pub struct Bases<'a> {
    pub inherited: &'a [View],
    pub merge_base: Option<&'a View>,
    pub served: &'a [View],
}

impl Bases<'_> {
    fn all(&self) -> impl Iterator<Item = &View> {
        self.inherited.iter().chain(self.served.iter())
    }
}

/// Whether `workspace_id` may publish `new`, given what it is measured against.
///
/// `root_commit_id` is the repo's root commit, which is a head of the root
/// view and stops being one as soon as anything is committed — a workspace
/// that leaves it behind is not taking anything from anybody.
pub fn check_publish(
    workspace_id: &str,
    bases: &Bases<'_>,
    new: &View,
    root_commit_id: &CommitId,
    is_rewrite_of: &dyn Fn(&CommitId, &CommitId) -> bool,
) -> Result<(), ScopeDenied> {
    let namespace = namespace_prefix(workspace_id);

    // ── The workspace pointers ──
    //
    // Its own entry is the one thing a workspace token is here to move.
    //
    // Another workspace's entry may move only where jj itself moves it without
    // being asked: when this publish rebases a commit somebody else is sitting
    // on, their pointer has to follow the rewrite or their working copy would
    // be looking at a commit that is no longer in the repo. A rewrite keeps the
    // change id, so that is the test — pointing another workspace at a commit
    // of one's own is a different change id, and refused.
    check_workspace_pointers(workspace_id, bases, new, is_rewrite_of)?;

    // ── The bookmarks ──
    //
    // `agent-a/task-42` is agent-a's. `main` is nobody's: it moves by an
    // integrator rebase on the server, never by a publish.
    check_map(
        "bookmark",
        bases,
        &new.local_bookmarks,
        |base| &base.local_bookmarks,
        |name| name.starts_with(&namespace),
    )?;

    // ── Everything a client has no business writing ──
    //
    // Tags and the git-interop state belong to the server process, which is
    // the only thing that talks to a git remote. There is no namespace carve-
    // out here: an agent's publish must carry these forward unchanged.
    check_map(
        "tag",
        bases,
        &new.local_tags,
        |base| &base.local_tags,
        |_| false,
    )?;
    check_map(
        "remote",
        bases,
        &new.remote_views,
        |base| &base.remote_views,
        |_| false,
    )?;
    check_map(
        "git ref",
        bases,
        &new.git_refs,
        |base| &base.git_refs,
        |_| false,
    )?;
    // The git HEAD is one value rather than a map, so it is checked as a value:
    // an absent HEAD is an absence like any other, and a first operation that
    // inherited nothing did not unset anybody's.
    check_value("git HEAD", bases, present(&new.git_head), |base| {
        present(&base.git_head)
    })?;

    // ── The head set ──
    //
    // Adding a head is the ordinary thing a publish does and needs no
    // permission: a commit nobody else can see yet takes nothing away. Making
    // a head disappear does: the work behind it stops being reachable. A
    // workspace retires its own working-copy commit every time it commits on
    // top of it, and that is the only head it may retire.
    //
    // A head is dropped on the same terms as a bookmark: only if no parent had
    // it, or a parent retired it after the merge base, or every head set the
    // server serves is already without it.
    let mut known_heads: BTreeSet<&CommitId> = BTreeSet::new();
    let mut own_previous: BTreeSet<&CommitId> = BTreeSet::new();
    for base in bases.all() {
        known_heads.extend(base.head_ids.iter());
        if let Some((_, commit)) = base
            .wc_commit_ids
            .iter()
            .find(|(name, _)| name.as_str() == workspace_id)
        {
            own_previous.insert(commit);
        }
    }
    for removed in known_heads {
        // A set membership *is* the presence encoding `unchanged` reads: `get`
        // answers `Some` for a head the view carries and `None` for one it does
        // not. So the head set gets the same three-grounds test as every other
        // name, rather than a second copy of it. The `Some` branch subsumes the
        // "the publish still carries it" case: every head in `known_heads` came
        // out of some base, so a head the publish keeps matches that base.
        if unchanged(
            new.head_ids.get(removed),
            bases.inherited.iter().map(|base| base.head_ids.get(removed)),
            bases.merge_base.and_then(|base| base.head_ids.get(removed)),
            bases.served.iter().map(|base| base.head_ids.get(removed)),
        )
            || own_previous.contains(removed)
            || removed == root_commit_id
            // A rebase retires the commit it rewrote: the work is still
            // reachable, under a new commit id and the same change id.
            || new
                .head_ids
                .iter()
                .any(|published| is_rewrite_of(removed, published))
        {
            continue;
        }
        return Err(denied(format!(
            "this token may not remove head {} — it belongs to another workspace",
            removed.hex()
        )));
    }

    Ok(())
}

/// The workspace pointers: its own is free, everybody else's follows a rewrite
/// or does not move at all.
fn check_workspace_pointers(
    workspace_id: &str,
    bases: &Bases<'_>,
    new: &View,
    is_rewrite_of: &dyn Fn(&CommitId, &CommitId) -> bool,
) -> Result<(), ScopeDenied> {
    let mut names: BTreeSet<&jj_lib::ref_name::WorkspaceNameBuf> =
        new.wc_commit_ids.keys().collect();
    for base in bases.all() {
        names.extend(base.wc_commit_ids.keys());
    }

    for name in names {
        if name.as_str() == workspace_id {
            continue;
        }
        let published = new.wc_commit_ids.get(name);
        if unchanged(
            published,
            bases
                .inherited
                .iter()
                .map(|base| base.wc_commit_ids.get(name)),
            bases
                .merge_base
                .and_then(|base| base.wc_commit_ids.get(name)),
            bases.served.iter().map(|base| base.wc_commit_ids.get(name)),
        ) {
            continue;
        }
        if let Some(published) = published {
            if bases
                .all()
                .filter_map(|base| base.wc_commit_ids.get(name))
                .any(|before| is_rewrite_of(before, published))
            {
                continue;
            }
        }
        return Err(denied(format!(
            "this token may not change the workspace pointer {}",
            name.as_str()
        )));
    }
    Ok(())
}

/// Every key that is not this token's own has to be unchanged.
///
/// Absence counts as a value: adding a bookmark somebody else's namespace owns
/// is as much a change as moving one, and deleting one is too.
/// `project` picks the map out of a view, and is applied to all three kinds of
/// base here rather than at the call site: one projection, so no call site can
/// read `local_bookmarks` from the parents and `local_tags` from the served
/// heads.
fn check_map<'a, K, V>(
    what: &str,
    bases: &Bases<'a>,
    new: &BTreeMap<K, V>,
    project: impl Fn(&'a View) -> &'a BTreeMap<K, V>,
    mine: impl Fn(&str) -> bool,
) -> Result<(), ScopeDenied>
where
    K: Ord + AsRef<str> + 'a,
    V: PartialEq + 'a,
{
    let inherited: Vec<&BTreeMap<K, V>> = bases.inherited.iter().map(&project).collect();
    let merge_base: Option<&BTreeMap<K, V>> = bases.merge_base.map(&project);
    let served: Vec<&BTreeMap<K, V>> = bases.served.iter().map(&project).collect();

    let mut names: BTreeSet<&K> = new.keys().collect();
    for base in inherited.iter().chain(served.iter()) {
        names.extend(base.keys());
    }

    for name in names {
        if mine(name.as_ref()) {
            continue;
        }
        if unchanged(
            new.get(name),
            inherited.iter().map(|base| base.get(name)),
            merge_base.and_then(|base| base.get(name)),
            served.iter().map(|base| base.get(name)),
        ) {
            continue;
        }
        return Err(denied(format!(
            "this token may not change the {what} {}",
            name.as_ref()
        )));
    }
    Ok(())
}

/// The same test for a single value rather than a map of them.
///
/// The git HEAD is the only one, and it gets the same three-grounds treatment
/// as a name in a map: an absent HEAD is an absence like any other.
fn check_value<'a, V: PartialEq + 'a>(
    what: &str,
    bases: &Bases<'a>,
    published: Option<&V>,
    project: impl Fn(&'a View) -> Option<&'a V>,
) -> Result<(), ScopeDenied> {
    if unchanged(
        published,
        bases.inherited.iter().map(&project),
        bases.merge_base.and_then(&project),
        bases.served.iter().map(&project),
    ) {
        return Ok(());
    }
    Err(denied(format!("this token may not move the {what}")))
}

/// Whether publishing `published` for one name changes anything.
///
/// A value is unchanged if some base already carries it. An absence is
/// unchanged on the three grounds `Bases` sets out: no parent had it, the merge
/// base had it and a parent removed it, or no served head has it.
///
/// `merge_base` is the value the closest common ancestor of the parents carried,
/// if there is such an ancestor and it carried one — "no merge base" and "the
/// merge base did not have this either" answer the one question it is asked the
/// same way, so they are the same `None` here.
///
/// The quantifiers differ on purpose. "No parent had it" is `all`, because one
/// parent that still has it means the operation is dropping it. "A parent
/// removed it" is `any`, because a removal by one side of a merge wins — but
/// only once the merge base says there was something there to remove. "No
/// served head has it" is `all` too: a value survives while one served head
/// still carries it, so gone has to mean gone from every one of them, and an
/// empty served set is no evidence of anything.
fn unchanged<'a, V: PartialEq + 'a>(
    published: Option<&V>,
    mut inherited: impl Iterator<Item = Option<&'a V>> + Clone,
    merge_base: Option<&'a V>,
    served: impl Iterator<Item = Option<&'a V>>,
) -> bool {
    let mut served = served.peekable();
    match published {
        Some(published) => inherited
            .chain(served)
            .any(|base| base.is_some_and(|base| base == published)),
        None => {
            let removed_by_a_parent =
                merge_base.is_some() && inherited.clone().any(|base| base.is_none());
            let never_inherited = inherited.all(|base| base.is_none());
            let already_gone = served.peek().is_some() && served.all(|base| base.is_none());
            never_inherited || removed_by_a_parent || already_gone
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_namespace_is_the_workspace_name_and_a_slash() {
        assert_eq!(namespace_prefix("agent-a"), "agent-a/");
        assert!(!"main".starts_with(&namespace_prefix("agent-a")));
        assert!("agent-a/task-42".starts_with(&namespace_prefix("agent-a")));
        assert!(
            !"agent-ab/task".starts_with(&namespace_prefix("agent-a")),
            "a prefix of a name is not the same namespace"
        );
    }
    use super::*;

    use jj_lib::op_store::{RefTarget, RemoteRef, RemoteView};
    use jj_lib::ref_name::{RefNameBuf, RemoteNameBuf, WorkspaceNameBuf};

    fn commit(byte: u8) -> CommitId {
        CommitId::new(vec![byte; 20])
    }

    fn root() -> CommitId {
        commit(0)
    }

    /// The view a workspace sees before it does anything: one head, one
    /// pointer of its own, one pointer belonging to somebody else, and `main`.
    fn base_view() -> View {
        let mut view = View::make_root(root());
        view.head_ids = [commit(1), commit(2)].into_iter().collect();
        view.wc_commit_ids = BTreeMap::from([
            (WorkspaceNameBuf::from("agent-a".to_string()), commit(1)),
            (WorkspaceNameBuf::from("agent-b".to_string()), commit(2)),
        ]);
        view.local_bookmarks = BTreeMap::from([(
            RefNameBuf::from("main".to_string()),
            RefTarget::normal(commit(1)),
        )]);
        view
    }

    /// The check, with nothing counting as a rewrite of anything. Rewrites are
    /// a question about commits, which these views do not carry, so the tests
    /// that care about them say so themselves.
    ///
    /// The bases here are the operation's own parents, which is the shape
    /// almost every one of these cases is about; the ones that care what the
    /// server is serving beside them call `check_publish` themselves.
    fn check(workspace_id: &str, parents: &[View], new: &View) -> Result<(), ScopeDenied> {
        let bases = Bases {
            inherited: parents,
            merge_base: None,
            served: &[],
        };
        check_publish(workspace_id, &bases, new, &root(), &|_, _| false)
    }

    #[test]
    fn committing_in_its_own_workspace_is_allowed() {
        let base = base_view();
        let mut new = base.clone();
        // The ordinary publish: a new commit on top of its own, and the old
        // working-copy commit stops being a head.
        new.head_ids = [commit(3), commit(2)].into_iter().collect();
        new.wc_commit_ids
            .insert(WorkspaceNameBuf::from("agent-a".to_string()), commit(3));

        check("agent-a", &[base], &new).expect("its own workspace");
    }

    #[test]
    fn a_bookmark_in_its_own_namespace_is_allowed() {
        let base = base_view();
        let mut new = base.clone();
        new.local_bookmarks.insert(
            RefNameBuf::from("agent-a/task-42".to_string()),
            RefTarget::normal(commit(1)),
        );

        check("agent-a", &[base], &new).expect("its own namespace");
    }

    #[test]
    fn moving_main_is_refused() {
        let base = base_view();
        let mut new = base.clone();
        new.local_bookmarks.insert(
            RefNameBuf::from("main".to_string()),
            RefTarget::normal(commit(2)),
        );

        let denied = check("agent-a", &[base], &new).expect_err("main is nobody's");
        assert!(denied.to_string().contains("main"), "{denied}");
    }

    #[test]
    fn deleting_main_is_refused_too() {
        let base = base_view();
        let mut new = base.clone();
        new.local_bookmarks
            .remove(&RefNameBuf::from("main".to_string()));

        let denied = check("agent-a", &[base], &new).expect_err("deleting is changing");
        assert!(denied.to_string().contains("main"), "{denied}");
    }

    #[test]
    fn a_bookmark_in_another_workspaces_namespace_is_refused() {
        let base = base_view();
        let mut new = base.clone();
        new.local_bookmarks.insert(
            RefNameBuf::from("agent-b/task-7".to_string()),
            RefTarget::normal(commit(1)),
        );

        let denied = check("agent-a", &[base], &new).expect_err("agent-b's namespace is agent-b's");
        assert!(denied.to_string().contains("agent-b/task-7"), "{denied}");
    }

    #[test]
    fn moving_another_workspaces_pointer_is_refused() {
        let base = base_view();
        let mut new = base.clone();
        new.wc_commit_ids
            .insert(WorkspaceNameBuf::from("agent-b".to_string()), commit(9));

        let denied = check("agent-a", &[base], &new).expect_err("agent-b's pointer is agent-b's");
        assert!(denied.to_string().contains("agent-b"), "{denied}");
    }

    #[test]
    fn another_workspaces_pointer_may_follow_a_rewrite_of_its_own_commit() {
        let base = base_view();
        let mut new = base.clone();
        // agent-a rebases what agent-b is sitting on: commit 2 becomes
        // commit 7, and agent-b's pointer has to come along.
        new.head_ids = [commit(1), commit(7)].into_iter().collect();
        new.wc_commit_ids
            .insert(WorkspaceNameBuf::from("agent-b".to_string()), commit(7));

        let rewritten =
            |before: &CommitId, after: &CommitId| *before == commit(2) && *after == commit(7);
        let bases = Bases {
            inherited: std::slice::from_ref(&base),
            merge_base: None,
            served: &[],
        };
        check_publish("agent-a", &bases, &new, &root(), &rewritten)
            .expect("a rebase moves the pointer of the workspace it rebased");

        // The same publish, with nothing rewritten: then it is just agent-a
        // moving somebody else's working copy.
        check("agent-a", &[base], &new).expect_err("not a rewrite, not allowed");
    }

    #[test]
    fn a_new_workspace_may_add_its_own_pointer() {
        let base = base_view();
        let mut new = base.clone();
        new.wc_commit_ids
            .insert(WorkspaceNameBuf::from("agent-c".to_string()), commit(4));
        new.head_ids.insert(commit(4));

        check("agent-c", &[base], &new).expect("its own first pointer");
    }

    #[test]
    fn removing_another_workspaces_head_is_refused() {
        let base = base_view();
        let mut new = base.clone();
        new.head_ids.remove(&commit(2));

        let denied = check("agent-a", &[base], &new).expect_err("agent-b's head is agent-b's");
        assert!(denied.to_string().contains(&commit(2).hex()), "{denied}");
    }

    #[test]
    fn leaving_the_root_commit_behind_is_allowed() {
        let base = View::make_root(root());
        let mut new = base.clone();
        new.head_ids = [commit(5)].into_iter().collect();
        new.wc_commit_ids
            .insert(WorkspaceNameBuf::from("agent-a".to_string()), commit(5));

        check("agent-a", &[base], &new).expect("the first commit of a repo");
    }

    #[test]
    fn touching_the_git_state_is_refused() {
        let base = base_view();

        let mut with_tag = base.clone();
        with_tag.local_tags.insert(
            RefNameBuf::from("v1".to_string()),
            RefTarget::normal(commit(1)),
        );
        assert!(check("agent-a", std::slice::from_ref(&base), &with_tag).is_err());

        let mut with_git_ref = base.clone();
        with_git_ref
            .git_refs
            .insert("refs/heads/main".into(), RefTarget::normal(commit(1)));
        assert!(check("agent-a", std::slice::from_ref(&base), &with_git_ref).is_err());

        let mut with_git_head = base.clone();
        with_git_head.git_head = RefTarget::normal(commit(1));
        assert!(check("agent-a", std::slice::from_ref(&base), &with_git_head).is_err());

        let mut with_remote = base.clone();
        with_remote.remote_views.insert(
            RemoteNameBuf::from("origin".to_string()),
            RemoteView {
                bookmarks: BTreeMap::from([(
                    RefNameBuf::from("main".to_string()),
                    RemoteRef {
                        target: RefTarget::normal(commit(1)),
                        state: jj_lib::op_store::RemoteRefState::Tracked,
                    },
                )]),
                tags: BTreeMap::new(),
            },
        );
        assert!(check("agent-a", &[base], &with_remote).is_err());
    }

    #[test]
    fn a_value_inherited_from_either_parent_of_a_merge_is_allowed() {
        // What a client publishes after it merges two divergent heads: one
        // parent moved agent-b's pointer, the other moved `main`, and the
        // merge carries both forward. Neither is this token's doing.
        let mut left = base_view();
        left.wc_commit_ids
            .insert(WorkspaceNameBuf::from("agent-b".to_string()), commit(7));
        left.head_ids.insert(commit(7));

        let mut right = base_view();
        right.local_bookmarks.insert(
            RefNameBuf::from("main".to_string()),
            RefTarget::normal(commit(2)),
        );

        let mut merged = left.clone();
        merged.local_bookmarks = right.local_bookmarks.clone();
        merged.head_ids.insert(commit(8));
        merged
            .wc_commit_ids
            .insert(WorkspaceNameBuf::from("agent-a".to_string()), commit(8));
        merged.head_ids.remove(&commit(1));

        check("agent-a", &[left, right], &merged).expect("a merge of two heads");
    }

    #[test]
    fn a_value_in_no_parent_at_all_is_refused_even_on_a_merge() {
        let left = base_view();
        let right = base_view();
        let mut merged = base_view();
        merged.local_bookmarks.insert(
            RefNameBuf::from("main".to_string()),
            RefTarget::normal(commit(9)),
        );

        assert!(check("agent-a", &[left, right], &merged).is_err());
    }

    // ── The two kinds of base ──

    /// A value the server already serves may be carried forward by anybody.
    ///
    /// This is how a client records somebody else's operation: jj prunes an
    /// operation head that turns out to be an ancestor of another, so agent-b
    /// routinely asks the server to record an operation agent-a wrote, and that
    /// operation moved agent-a's pointer.
    #[test]
    fn a_value_a_served_head_already_carries_is_allowed() {
        let base = base_view();

        let mut served = base_view();
        served
            .wc_commit_ids
            .insert(WorkspaceNameBuf::from("agent-b".to_string()), commit(7));
        served.head_ids.insert(commit(7));

        let new = served.clone();

        let bases = Bases {
            inherited: std::slice::from_ref(&base),
            merge_base: None,
            served: std::slice::from_ref(&served),
        };
        check_publish("agent-a", &bases, &new, &root(), &|_, _| false)
            .expect("a value the server already serves");
    }

    /// The other direction is not symmetric: a *deletion* may not be justified
    /// by a base the operation replaces while another base it replaces still
    /// carries the thing.
    ///
    /// This is the shape of the attack the split exists for. A token publishes
    /// an operation whose parents are a superseded state that lacks agent-b's
    /// bookmark and the current head that has it, and calls the result a merge.
    /// jj's own merge would put the bookmark back; this "merge" drops it, and
    /// because the operation descends from the head, the head is retired and
    /// the bookmark is gone for good.
    #[test]
    fn a_deletion_is_not_inherited_from_one_parent_when_another_still_has_it() {
        let mut has_it = base_view();
        has_it.local_bookmarks.insert(
            RefNameBuf::from("agent-b/keep-me".to_string()),
            RefTarget::normal(commit(2)),
        );

        // The parent that lacks the bookmark: an older state, or a fork from
        // the empty repo. Either way it is not evidence that anybody deleted
        // anything.
        let lacks_it = base_view();

        let mut new = has_it.clone();
        new.local_bookmarks
            .remove(&RefNameBuf::from("agent-b/keep-me".to_string()));

        let denied = check("agent-a", &[lacks_it.clone(), has_it.clone()], &new)
            .expect_err("agent-b's bookmark is agent-b's");
        assert!(denied.to_string().contains("agent-b/keep-me"), "{denied}");

        // But it is allowed once the server is serving a head without it —
        // then it is already gone, and this publish is not what took it away.
        let bases = Bases {
            inherited: &[lacks_it.clone(), has_it],
            merge_base: None,
            served: std::slice::from_ref(&lacks_it),
        };
        check_publish("agent-a", &bases, &new, &root(), &|_, _| false)
            .expect("a bookmark the server has already stopped serving");
    }

    /// The same asymmetry for a head: one that a replaced base still carries
    /// may not be dropped by presenting a base that never had it.
    #[test]
    fn a_head_is_not_dropped_by_presenting_a_base_that_never_had_it() {
        let has_it = base_view();

        let mut lacks_it = base_view();
        lacks_it.head_ids.remove(&commit(2));

        let mut new = base_view();
        new.head_ids.remove(&commit(2));

        let denied = check("agent-a", &[lacks_it.clone(), has_it.clone()], &new)
            .expect_err("agent-b's head is agent-b's");
        assert!(denied.to_string().contains(&commit(2).hex()), "{denied}");

        let bases = Bases {
            inherited: &[lacks_it.clone(), has_it],
            merge_base: None,
            served: std::slice::from_ref(&lacks_it),
        };
        check_publish("agent-a", &bases, &new, &root(), &|_, _| false)
            .expect("a head the server has already stopped serving");
    }

    /// A value one served head has already lost does not make it deletable
    /// while another served head still carries it.
    ///
    /// This is the multi-head case, and it is a real state: when
    /// `reconcile_jj_op_heads` cannot order or merge two divergent heads it
    /// hands back the unmerged set, so the server goes on serving both. Say
    /// `Oa` is an older branch that predates `agent-b/keep-me` and `Ob` is
    /// agent-b's head that has it. agent-a publishes an operation whose only
    /// parent is `Ob` and whose view is `Ob`'s minus the bookmark. Because it
    /// descends from `Ob`, the reconcile retires `Ob`, and the surviving heads
    /// both lack the bookmark: it is gone for good. So "already gone" has to
    /// mean gone from *every* head the server serves, not from one of them.
    #[test]
    fn a_deletion_is_refused_when_only_one_of_several_served_heads_lacks_it() {
        let lacks_it = base_view();

        let mut has_it = base_view();
        has_it.local_bookmarks.insert(
            RefNameBuf::from("agent-b/keep-me".to_string()),
            RefTarget::normal(commit(2)),
        );

        // The publish descends from the head that has it and drops it.
        let mut new = has_it.clone();
        new.local_bookmarks
            .remove(&RefNameBuf::from("agent-b/keep-me".to_string()));

        let bases = Bases {
            inherited: std::slice::from_ref(&has_it),
            merge_base: None,
            served: &[lacks_it.clone(), has_it.clone()],
        };
        let denied = check_publish("agent-a", &bases, &new, &root(), &|_, _| false)
            .expect_err("one served head lacking it is not evidence it is gone");
        assert!(denied.to_string().contains("agent-b/keep-me"), "{denied}");

        // Once every served head has lost it, it really is gone, and dropping
        // it takes nothing away.
        let bases = Bases {
            inherited: std::slice::from_ref(&has_it),
            merge_base: None,
            served: std::slice::from_ref(&lacks_it),
        };
        check_publish("agent-a", &bases, &new, &root(), &|_, _| false)
            .expect("a bookmark no served head carries any more");
    }

    /// The same for a head: one served head having dropped it does not let
    /// another workspace's head be retired while a second served head still
    /// carries it.
    #[test]
    fn a_head_is_not_dropped_when_only_one_of_several_served_heads_lacks_it() {
        let has_it = base_view();

        let mut lacks_it = base_view();
        lacks_it.head_ids.remove(&commit(2));

        let mut new = base_view();
        new.head_ids.remove(&commit(2));

        let bases = Bases {
            inherited: std::slice::from_ref(&has_it),
            merge_base: None,
            served: &[lacks_it.clone(), has_it.clone()],
        };
        let denied = check_publish("agent-a", &bases, &new, &root(), &|_, _| false)
            .expect_err("one served head lacking it is not evidence it is gone");
        assert!(denied.to_string().contains(&commit(2).hex()), "{denied}");

        let bases = Bases {
            inherited: std::slice::from_ref(&has_it),
            merge_base: None,
            served: std::slice::from_ref(&lacks_it),
        };
        check_publish("agent-a", &bases, &new, &root(), &|_, _| false)
            .expect("a head no served head carries any more");
    }

    // ── The merge base ──

    /// The honest merge of two divergent heads, one of which retired a head
    /// belonging to another workspace.
    ///
    /// agent-b committed on top of its working copy, so its operation dropped
    /// `commit(2)` and added `commit(7)`. agent-a's client finds two operation
    /// heads, merges them, and publishes the merge — which does not carry
    /// `commit(2)`, because agent-b retired it. The merge base is what says so:
    /// it had `commit(2)`, and one parent has since dropped it.
    #[test]
    fn a_merge_may_carry_forward_a_removal_a_parent_made() {
        let merge_base = base_view();

        let still_has_it = base_view();
        let mut removed_it = base_view();
        removed_it.head_ids.remove(&commit(2));
        removed_it.head_ids.insert(commit(7));
        removed_it
            .wc_commit_ids
            .insert(WorkspaceNameBuf::from("agent-b".to_string()), commit(7));

        let new = removed_it.clone();

        let bases = Bases {
            inherited: &[still_has_it.clone(), removed_it.clone()],
            merge_base: Some(&merge_base),
            served: &[still_has_it, removed_it],
        };
        check_publish("agent-a", &bases, &new, &root(), &|_, _| false)
            .expect("a merge that carries forward what another workspace retired");
    }

    /// The same shape, forged: the second parent does not lack the bookmark
    /// because anybody deleted it, but because it predates it.
    ///
    /// This is the attack the merge base exists to stop. A token may name as a
    /// second parent any operation the server serves, and an old enough one has
    /// never heard of `agent-b/keep-me`. jj's own merge of the two would keep
    /// the bookmark — a side that never had a value does not vote against it —
    /// so a hand-built "merge" that drops it is a deletion, and reading the
    /// merge base is what shows that: it does not have the bookmark either, so
    /// no parent removed anything.
    #[test]
    fn a_merge_with_a_parent_that_predates_a_bookmark_may_not_drop_it() {
        let merge_base = base_view();
        let predates_it = base_view();

        let mut has_it = base_view();
        has_it.local_bookmarks.insert(
            RefNameBuf::from("agent-b/keep-me".to_string()),
            RefTarget::normal(commit(2)),
        );

        let mut new = has_it.clone();
        new.local_bookmarks
            .remove(&RefNameBuf::from("agent-b/keep-me".to_string()));

        let bases = Bases {
            inherited: &[predates_it.clone(), has_it.clone()],
            merge_base: Some(&merge_base),
            served: &[predates_it, has_it],
        };
        let denied = check_publish("agent-a", &bases, &new, &root(), &|_, _| false)
            .expect_err("a parent that never had it did not delete it");
        assert!(denied.to_string().contains("agent-b/keep-me"), "{denied}");
    }

    /// And once the merge base does have the bookmark, the same publish is the
    /// honest propagation of somebody's `jj bookmark delete`.
    #[test]
    fn a_merge_may_carry_forward_a_bookmark_a_parent_deleted() {
        let mut merge_base = base_view();
        merge_base.local_bookmarks.insert(
            RefNameBuf::from("agent-b/keep-me".to_string()),
            RefTarget::normal(commit(2)),
        );

        let deleted_it = base_view();
        let has_it = merge_base.clone();

        let mut new = has_it.clone();
        new.local_bookmarks
            .remove(&RefNameBuf::from("agent-b/keep-me".to_string()));

        let bases = Bases {
            inherited: &[deleted_it.clone(), has_it.clone()],
            merge_base: Some(&merge_base),
            served: &[deleted_it, has_it],
        };
        check_publish("agent-a", &bases, &new, &root(), &|_, _| false)
            .expect("a deletion one parent already made");
    }

    /// An operation with no parents at all inherited nothing, so it takes
    /// nothing away. This is `tandem init`, whose first operation is jj's root
    /// operation and whose view is the empty repo.
    #[test]
    fn an_operation_that_inherited_nothing_takes_nothing_away() {
        let served = base_view();
        let new = View::make_root(root());

        let bases = Bases {
            inherited: &[],
            merge_base: None,
            served: std::slice::from_ref(&served),
        };
        check_publish("agent-c", &bases, &new, &root(), &|_, _| false)
            .expect("a new repo's first operation");
    }
}
