//! Gate for preparing a complete publish with the native local jj engine.
use jj_lib::backend::{CopyId, TreeValue};
use jj_lib::merge::Merge;
use jj_lib::merged_tree_builder::MergedTreeBuilder;
use jj_lib::object_id::ObjectId as _;
use jj_lib::ref_name::WorkspaceNameBuf;
use jj_lib::repo::Repo as _;
use jj_lib::repo_path::RepoPathBuf;
use jj_lib::workspace::Workspace;
use jj_tandem_jj::proto_convert;
use jj_tandem_repository::Repository;
use pollster::FutureExt as _;
use prost::Message as _;

#[test]
fn independent_rewrite_predecessors_can_change_server_commit_identity(
) -> Result<(), Box<dyn std::error::Error>> {
    let settings = jj_tandem_test_support::test_settings()?;
    let server_dir = tempfile::tempdir()?;
    let server = Repository::new(&settings, server_dir.path().to_owned(), None)?;
    let signature = jj_lib::backend::Signature {
        name: "Test User".into(),
        email: "test@tandem.dev".into(),
        timestamp: jj_lib::backend::Timestamp {
            timestamp: jj_lib::backend::MillisSinceEpoch(1_800_000_000_000),
            tz_offset: 0,
        },
    };
    let change_id = jj_lib::backend::ChangeId::new(vec![42; 16]);
    let mut first_rewrite = None;
    for (writer, previous_text) in [b"previous A".as_slice(), b"previous B".as_slice()]
        .into_iter()
        .enumerate()
    {
        let local_dir = tempfile::tempdir()?;
        let (_workspace, repo) = Workspace::init_colocated_git(&settings, local_dir.path())?;
        let store = repo.store();
        let root = store.get_commit(store.root_commit_id())?;
        let mut tx = repo.start_transaction();
        let mut commits = Vec::new();
        for text in [previous_text, b"identical edited file\n".as_slice()] {
            let path = RepoPathBuf::from_internal_string("edited.txt")?;
            let file_id = store
                .write_file(&path, &mut std::io::Cursor::new(text))
                .block_on()?;
            assert_eq!(server.put_object_sync("file", text)?.0, file_id.to_bytes());
            let mut tree = MergedTreeBuilder::new(root.tree());
            tree.set_or_remove(
                path,
                Merge::normal(TreeValue::File {
                    id: file_id,
                    executable: false,
                    copy_id: CopyId::placeholder(),
                }),
            );
            let tree = tree.write_tree()?;
            let tree_id = tree.tree_ids().as_resolved().unwrap();
            let contents = store
                .backend()
                .read_tree(jj_lib::repo_path::RepoPath::root(), tree_id)
                .block_on()?;
            let tree_bytes = proto_convert::tree_to_proto(&contents).encode_to_vec();
            assert_eq!(
                server.put_object_sync("tree", &tree_bytes)?.0,
                tree_id.to_bytes()
            );
            let builder = match commits.last() {
                Some(previous) => tx.repo_mut().rewrite_commit(previous),
                None => tx
                    .repo_mut()
                    .new_commit(vec![root.id().clone()], tree.clone()),
            };
            let commit = builder
                .set_tree(tree)
                .set_change_id(change_id.clone())
                .set_description("same shared change")
                .set_author(signature.clone())
                .set_committer(signature.clone())
                .write()?;
            let data =
                jj_lib::simple_backend::commit_to_proto(commit.store_commit()).encode_to_vec();
            let (remote_id, normalized) = server.put_object_sync("commit", &data)?;
            if writer == 1 && !commits.is_empty() {
                // Both native repositories use the same settings. Git hashes do
                // not include jj's rewrite predecessors, so their final commit
                // IDs agree while their metadata does not. The server has seen
                // A's metadata and must move B to a different Git commit ID.
                assert_eq!(Some(commit.id().clone()), first_rewrite);
                assert_ne!(remote_id, commit.id().to_bytes());
                assert_eq!(
                    commit.id().hex(),
                    "8f03b6cd689d09f704f7451c62f0747b67334050"
                );
                assert_eq!(
                    jj_lib::backend::CommitId::new(remote_id.clone()).hex(),
                    "e44f9810b723634225bd41ba6142b41b0e4ad430"
                );
                let local = jj_lib::protos::simple_store::Commit::decode(data.as_slice())?;
                let remote = jj_lib::protos::simple_store::Commit::decode(normalized.as_slice())?;
                assert_eq!(remote.predecessors, local.predecessors);
                let mut expected = local.clone();
                expected
                    .committer
                    .as_mut()
                    .unwrap()
                    .timestamp
                    .as_mut()
                    .unwrap()
                    .millis_since_epoch -= 1000;
                assert_eq!(remote, expected);
                // The client's original ID resolves successfully, but to A's
                // rewrite metadata. A small ACK cannot mean that B's prepared
                // view/operation now identify the server-normalized commit.
                let at_local_id = server.get_object_sync("commit", commit.id().as_bytes())?;
                let at_local_id =
                    jj_lib::protos::simple_store::Commit::decode(at_local_id.as_slice())?;
                assert_ne!(at_local_id.predecessors, local.predecessors);
                assert_eq!(
                    server.put_object_sync("commit", &data)?,
                    (remote_id, normalized)
                );
            } else {
                assert_eq!(remote_id, commit.id().to_bytes());
                assert_eq!(normalized, data);
                if !commits.is_empty() {
                    first_rewrite = Some(commit.id().clone());
                }
            }
            commits.push(commit);
        }
    }
    Ok(())
}

#[test]
fn locally_prepared_file_tree_commit_view_and_operation_match_server(
) -> Result<(), Box<dyn std::error::Error>> {
    let settings = jj_tandem_test_support::test_settings()?;
    let local_dir = tempfile::tempdir()?;
    let server_dir = tempfile::tempdir()?;
    let (_workspace, repo) = Workspace::init_colocated_git(&settings, local_dir.path())?;
    let server = Repository::new(&settings, server_dir.path().to_owned(), None)?;
    let store = repo.store();
    let parent = store.get_commit(store.root_commit_id())?;
    let path = RepoPathBuf::from_internal_string("edited.txt")?;
    let bytes = b"one edited file\nexact bytes\0\xff";
    let file_id = store
        .write_file(&path, &mut std::io::Cursor::new(bytes))
        .block_on()?;
    let (remote_file, returned) = server.put_object_sync("file", bytes)?;
    assert_eq!(remote_file, file_id.to_bytes());
    assert_eq!(returned, bytes);
    let mut tree = MergedTreeBuilder::new(parent.tree());
    tree.set_or_remove(
        path,
        Merge::normal(TreeValue::File {
            id: file_id,
            executable: false,
            copy_id: CopyId::placeholder(),
        }),
    );
    let tree = tree.write_tree()?;
    let tree_ids = tree.tree_ids();
    let tree_id = tree_ids.as_resolved().unwrap();
    let contents = store
        .backend()
        .read_tree(jj_lib::repo_path::RepoPath::root(), tree_id)
        .block_on()?;
    let tree_bytes = proto_convert::tree_to_proto(&contents).encode_to_vec();
    let (remote_tree, returned) = server.put_object_sync("tree", &tree_bytes)?;
    assert_eq!(remote_tree, tree_id.to_bytes());
    assert_eq!(returned, tree_bytes);
    let mut tx = repo.start_transaction();
    let commit = tx
        .repo_mut()
        .new_commit(vec![parent.id().clone()], tree)
        .set_description("locally prepared edited file")
        .write()?;
    let commit_bytes =
        jj_lib::simple_backend::commit_to_proto(commit.store_commit()).encode_to_vec();
    let (remote_commit, returned) = server.put_object_sync("commit", &commit_bytes)?;
    assert_eq!(remote_commit, commit.id().to_bytes());
    assert_eq!(returned, commit_bytes);
    tx.repo_mut()
        .edit(WorkspaceNameBuf::from("default"), &commit)?;
    tx.repo_mut().rebase_descendants()?;
    let unpublished = tx.write("prepare one edited file locally")?;
    let operation = unpublished.operation();
    let view_bytes = proto_convert::view_to_proto(operation.view()?.store_view()).encode_to_vec();
    let operation_bytes =
        proto_convert::operation_to_proto(operation.store_operation()).encode_to_vec();
    let (view_id, operation_id) =
        server.put_operation_with_view_sync(&view_bytes, &operation_bytes)?;
    assert_eq!(view_id, operation.view_id().to_bytes());
    assert_eq!(operation_id, operation.id().to_bytes());
    assert_eq!(server.get_view_sync(&view_id)?, view_bytes);
    assert_eq!(server.get_operation_sync(&operation_id)?, operation_bytes);
    Ok(())
}
