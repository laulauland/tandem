//! The commit a clone's workspace creation leaves behind, and how to tell it
//! from work.
//!
//! `Workspace::init_with_factories` checks a new workspace name out at an empty
//! commit on the root commit before anything else happens. A clone that dies
//! there leaves that commit as the whole of what the name says it is, and every
//! defence against losing a workspace to an interrupted clone comes down to
//! recognising it. Both the client (deciding what to attach to) and the server
//! (deciding what a merge may settle on) have to recognise the same commit, so
//! the rule lives in one place and each side brings its own commit shape to it.

/// Whether a commit is the placeholder jj's workspace creation makes.
///
/// Three facts, and all three are needed. Nothing a workspace goes on to
/// publish looks like this: the first snapshot rewrites the commit and gives it
/// a tree, a change somebody describes gives it a description, and anything
/// later has a different parent. A merge — two parents — is not one either.
///
/// The commit is identified by shape and not by id because there is no id to
/// identify it by: `check_out` mints a new commit, with a new change id, every
/// time a workspace is created.
pub fn is_fresh_workspace_placeholder(
    description: &str,
    parents: &[&[u8]],
    root_tree: &[&[u8]],
    root_commit_id: &[u8],
    empty_tree_id: &[u8],
) -> bool {
    description.is_empty()
        && parents.len() == 1
        && parents[0] == root_commit_id
        && root_tree.len() == 1
        && root_tree[0] == empty_tree_id
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROOT_COMMIT: &[u8] = &[0u8; 32];
    const EMPTY_TREE: &[u8] = &[1u8; 32];

    fn is_placeholder(description: &str, parents: &[&[u8]], root_tree: &[&[u8]]) -> bool {
        is_fresh_workspace_placeholder(
            description,
            parents,
            root_tree,
            ROOT_COMMIT,
            EMPTY_TREE,
        )
    }

    #[test]
    fn the_commit_a_half_finished_clone_leaves_is_recognised() {
        assert!(is_placeholder("", &[ROOT_COMMIT], &[EMPTY_TREE]));
    }

    #[test]
    fn anything_a_workspace_published_is_not_a_placeholder() {
        const OTHER: &[u8] = &[7u8; 32];
        const A_TREE: &[u8] = &[9u8; 32];

        // A first snapshot: still on the root commit, but it has a tree now.
        assert!(!is_placeholder("", &[ROOT_COMMIT], &[A_TREE]));

        // An empty change somebody described. Empty is not the same as unused.
        assert!(!is_placeholder(
            "what I am about to do",
            &[ROOT_COMMIT],
            &[EMPTY_TREE]
        ));

        // An empty commit further along the history — a workspace that
        // attached, published, and then had its change squashed away.
        assert!(!is_placeholder("", &[OTHER], &[EMPTY_TREE]));

        // A merge, which workspace creation never makes.
        assert!(!is_placeholder("", &[ROOT_COMMIT, OTHER], &[EMPTY_TREE]));

        // A conflicted tree, which it never makes either.
        assert!(!is_placeholder(
            "",
            &[ROOT_COMMIT],
            &[EMPTY_TREE, A_TREE, EMPTY_TREE]
        ));
    }
}
