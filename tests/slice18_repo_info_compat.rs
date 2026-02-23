mod common;

use tempfile::TempDir;

fn assert_init_fails_with_env(env: &[(&str, &str)], expected_field: &str) {
    let tmp = TempDir::new().unwrap();
    let home = common::isolated_home(tmp.path());

    let server_repo = tmp.path().join("server-repo");
    std::fs::create_dir_all(&server_repo).unwrap();

    let workspace = tmp.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();

    let addr = common::free_addr();
    let mut server = common::spawn_server_with_args_and_env(&server_repo, &addr, &[], env, &home);
    common::wait_for_server(&addr, &mut server);

    let init = common::run_tandem_in(&workspace, &["init", "--server", &addr, "."], &home);
    assert!(
        !init.status.success(),
        "init unexpectedly succeeded\nstdout:\n{}\nstderr:\n{}",
        common::stdout_str(&init),
        common::stderr_str(&init)
    );

    let stderr = common::stderr_str(&init);
    assert!(
        stderr.contains(expected_field),
        "stderr should mention mismatched field {expected_field:?}\nstderr:\n{stderr}"
    );

    let _ = server.kill();
    let _ = server.wait();
}

#[test]
fn slice18_protocol_major_mismatch_fails_fast() {
    assert_init_fails_with_env(
        &[("TANDEM_TEST_REPO_INFO_PROTOCOL_MAJOR", "9")],
        "protocol_major",
    );
}

#[test]
fn slice18_backend_and_op_store_mismatch_fails_fast() {
    assert_init_fails_with_env(
        &[("TANDEM_TEST_REPO_INFO_BACKEND_NAME", "not_tandem")],
        "backend_name",
    );
    assert_init_fails_with_env(
        &[("TANDEM_TEST_REPO_INFO_OP_STORE_NAME", "not_tandem_op_store")],
        "op_store_name",
    );
}

#[test]
fn slice18_missing_watch_capability_is_gated() {
    let tmp = TempDir::new().unwrap();
    let home = common::isolated_home(tmp.path());
    let server_repo = tmp.path().join("server-repo");
    std::fs::create_dir_all(&server_repo).unwrap();

    let addr = common::free_addr();
    let mut server = common::spawn_server_with_args_and_env(
        &server_repo,
        &addr,
        &[],
        &[("TANDEM_TEST_REPO_INFO_CAPABILITIES", "")],
        &home,
    );
    common::wait_for_server(&addr, &mut server);

    let watch = common::run_tandem_in(tmp.path(), &["watch", "--server", &addr], &home);
    assert!(
        !watch.status.success(),
        "watch unexpectedly succeeded\nstdout:\n{}\nstderr:\n{}",
        common::stdout_str(&watch),
        common::stderr_str(&watch)
    );

    let stderr = common::stderr_str(&watch);
    assert!(
        stderr.contains("missing required capability watchHeads"),
        "watch stderr should explain capability gating\nstderr:\n{stderr}"
    );
    assert!(
        !stderr.to_lowercase().contains("panic"),
        "watch should fail cleanly, not panic\nstderr:\n{stderr}"
    );

    let _ = server.kill();
    let _ = server.wait();
}
