use crate::common;
use crate::common::ServerFixture;

fn assert_init_fails_with_env(env: &[(&str, &str)], expected_field: &str) {
    let fx = ServerFixture::builder().envs(env).start();
    let workspace = fx.dir("workspace");

    let init = fx.run(&workspace, &["init", "--server", &fx.addr, "."]);
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
}

#[test]
fn protocol_major_mismatch_fails_fast() {
    assert_init_fails_with_env(
        &[("TANDEM_TEST_REPO_INFO_PROTOCOL_MAJOR", "9")],
        "protocol_major",
    );
}

#[test]
fn backend_and_op_store_mismatch_fails_fast() {
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
fn missing_watch_capability_is_gated() {
    let fx = ServerFixture::builder()
        .envs(&[("TANDEM_TEST_REPO_INFO_CAPABILITIES", "")])
        .start();

    let watch = fx.run(fx.path(), &["watch", "--server", &fx.addr]);
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
}
