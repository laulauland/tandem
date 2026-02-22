//! Slice 18: tandem init base commit matches server workspace context
//!
//! Acceptance criteria:
//! - When the server default workspace has a non-root parent target,
//!   `tandem init` creates the new workspace working-copy commit on top of that target.
//! - New workspace `@-` matches the server default workspace `@-` (jj workspace-add default style).
//! - The seeded file bytes are readable from `@-` in the new workspace.

mod common;

use tempfile::TempDir;

#[test]
fn slice18_init_uses_server_workspace_parent_context_by_default() {
    let tmp = TempDir::new().unwrap();
    let home = common::isolated_home(tmp.path());

    let server_repo = tmp.path().join("server-repo");
    std::fs::create_dir_all(&server_repo).unwrap();

    let addr = common::free_addr();
    let mut server = common::spawn_server_with_args(&server_repo, &addr, &[], &home);
    common::wait_for_server(&addr, &mut server);

    // Seed the server default workspace with a real commit.
    let seed_bytes = b"seed from server workspace\n";
    std::fs::write(server_repo.join("seed.txt"), seed_bytes).unwrap();

    let describe_server = common::run_tandem_in(
        &server_repo,
        &["describe", "-m", "seed server workspace"],
        &home,
    );
    common::assert_ok(&describe_server, "describe server seed commit");

    let new_server = common::run_tandem_in(&server_repo, &["new"], &home);
    common::assert_ok(&new_server, "new server seed commit");

    let expected_parent = common::run_tandem_in(
        &server_repo,
        &[
            "log",
            "-r",
            "@-",
            "--no-graph",
            "-T",
            "commit_id ++ \"\\n\"",
        ],
        &home,
    );
    common::assert_ok(&expected_parent, "read server default @-");
    let expected_parent_id = common::stdout_str(&expected_parent).trim().to_string();
    assert!(
        !expected_parent_id.is_empty(),
        "server @- commit id should exist"
    );

    // Initialize a new remote workspace.
    let workspace_dir = tmp.path().join("agent-workspace");
    std::fs::create_dir_all(&workspace_dir).unwrap();

    let init = common::run_tandem_in(
        &workspace_dir,
        &["init", "--server", &addr, "--workspace", "agent-a", "."],
        &home,
    );
    common::assert_ok(&init, "tandem init from seeded server");

    // New workspace @- should match server default @- (jj workspace-add style default).
    let actual_parent = common::run_tandem_in(
        &workspace_dir,
        &[
            "log",
            "-r",
            "@-",
            "--no-graph",
            "-T",
            "commit_id ++ \"\\n\"",
        ],
        &home,
    );
    common::assert_ok(&actual_parent, "read new workspace @-");
    let actual_parent_id = common::stdout_str(&actual_parent).trim().to_string();

    assert_eq!(
        actual_parent_id, expected_parent_id,
        "new workspace @- should match server default workspace parent target"
    );

    // Verify exact file bytes are present at @-.
    let cat_seed = common::run_tandem_in(
        &workspace_dir,
        &["file", "show", "-r", "@-", "seed.txt"],
        &home,
    );
    common::assert_ok(&cat_seed, "read seed.txt from new workspace @-");
    assert_eq!(
        cat_seed.stdout, seed_bytes,
        "new workspace should inherit seeded file bytes from server context"
    );

    let _ = server.kill();
    let _ = server.wait();
}
