//! File upload batching through the real client, HTTP host, and Git backend.

use crate::common;
use crate::common::ServerFixture;

const NO_CACHE: [(&str, &str); 1] = [("TANDEM_DISABLE_CACHE", "1")];

#[test]
fn a_snapshot_uploads_eight_files_in_one_batch() {
    let fx = ServerFixture::builder().log_to_file().start();
    let dir = fx.init_workspace_with_env("batched", Some("batched"), &NO_CACHE);
    let files: Vec<_> = (0..8)
        .map(|index| {
            let path = format!("file-{index}.bin");
            let bytes = vec![0, 255, index, b'\n'];
            std::fs::write(dir.join(&path), &bytes).expect("write binary file");
            (path, bytes)
        })
        .collect();
    let batches_before = fx.rpc_request_count("putObjectsBatch");
    let objects_before = fx.rpc_request_count("putObject");

    let out = common::run_tandem_in_with_env(&dir, &["status"], &NO_CACHE, &fx.home);
    common::assert_ok(&out, "snapshot eight files");
    assert_eq!(fx.rpc_request_count("putObjectsBatch") - batches_before, 1);
    assert_eq!(
        fx.rpc_request_count("putObject") - objects_before,
        2,
        "only the tree and commit need individual uploads"
    );
    for (path, bytes) in files {
        let out = common::run_tandem_in_with_env(
            &dir,
            &["file", "show", "--ignore-working-copy", &path],
            &NO_CACHE,
            &fx.home,
        );
        common::assert_ok(&out, "read published binary file without the cache");
        assert_eq!(out.stdout, bytes);
    }
}

#[test]
fn pending_files_are_readable_and_flush_before_trees_and_commits() {
    use futures::io::AsyncReadExt as _;
    use jj_lib::backend::{Backend as _, CopyId, Tree, TreeValue};
    use jj_lib::object_id::ObjectId as _;
    use jj_lib::repo_path::{RepoPath, RepoPathComponentBuf};
    use jj_tandem_client::backend::TandemBackend;
    use pollster::FutureExt as _;

    let fx = ServerFixture::builder().log_to_file().start();
    let backend = TandemBackend::init(&fx.dir("store"), &fx.addr, fx.token()).unwrap();
    // A fresh fixture-specific payload cannot have been warmed into an
    // ambient disk cache by an earlier test run.
    let mut payload = fx
        .dir("unique-payload")
        .to_string_lossy()
        .as_bytes()
        .to_vec();
    payload.extend_from_slice(b"pending\0\xff\n");
    let id = backend
        .write_file(RepoPath::root(), &mut futures::io::Cursor::new(&payload))
        .block_on()
        .unwrap();
    assert_eq!(fx.rpc_request_count("putObjectsBatch"), 0);
    assert_eq!(fx.rpc_request_count("putObject"), 0);
    let mut reader = backend.read_file(RepoPath::root(), &id).block_on().unwrap();
    let mut actual = Vec::new();
    reader.read_to_end(&mut actual).block_on().unwrap();
    assert_eq!(actual, payload);
    assert_eq!(
        fx.rpc_request_count("getObject"),
        0,
        "pending reads do not ask the server"
    );

    let tree = Tree::from_sorted_entries(vec![(
        RepoPathComponentBuf::new("file").unwrap(),
        TreeValue::File {
            id: id.clone(),
            executable: false,
            copy_id: CopyId::placeholder(),
        },
    )]);
    backend
        .write_tree(RepoPath::root(), &tree)
        .block_on()
        .unwrap();
    assert_eq!(fx.rpc_request_count("putObjectsBatch"), 1);
    assert_eq!(
        common::api_get(
            &fx.addr,
            fx.token(),
            &format!("/api/objects/file/{}", id.hex())
        )
        .bytes()
        .unwrap()
        .as_ref(),
        payload,
        "the tree's file is on the server, independently of client caches"
    );

    let later = b"also flush before commit";
    let later_id = backend
        .write_file(RepoPath::root(), &mut futures::io::Cursor::new(later))
        .block_on()
        .unwrap();
    let mut commit = backend
        .read_commit(backend.root_commit_id())
        .block_on()
        .unwrap();
    commit.parents = vec![backend.root_commit_id().clone()];
    backend.write_commit(commit, None).block_on().unwrap();
    assert_eq!(fx.rpc_request_count("putObjectsBatch"), 2);
    assert_eq!(
        common::api_get(
            &fx.addr,
            fx.token(),
            &format!("/api/objects/file/{}", later_id.hex())
        )
        .bytes()
        .unwrap()
        .as_ref(),
        later
    );
}

#[test]
fn a_file_larger_than_the_batch_budget_uploads_without_being_buffered() {
    use jj_lib::backend::Backend as _;
    use jj_lib::object_id::ObjectId as _;
    use jj_lib::repo_path::RepoPath;
    use jj_tandem_client::backend::TandemBackend;
    use pollster::FutureExt as _;

    let fx = ServerFixture::builder().log_to_file().start();
    let backend = TandemBackend::init(&fx.dir("store"), &fx.addr, fx.token()).unwrap();
    let first = b"queued first";
    backend
        .write_file(RepoPath::root(), &mut futures::io::Cursor::new(first))
        .block_on()
        .unwrap();
    let large = vec![0xa5; 8 * 1024 * 1024];
    let id = backend
        .write_file(RepoPath::root(), &mut futures::io::Cursor::new(&large))
        .block_on()
        .unwrap();
    assert_eq!(
        fx.rpc_request_count("putObjectsBatch"),
        1,
        "previous queued files flush first"
    );
    assert_eq!(
        fx.rpc_request_count("putObject"),
        1,
        "the large file uses the ordinary endpoint"
    );
    let actual = common::api_get(
        &fx.addr,
        fx.token(),
        &format!("/api/objects/file/{}", id.hex()),
    )
    .bytes()
    .unwrap();
    assert_eq!(actual.as_ref(), large);
}
