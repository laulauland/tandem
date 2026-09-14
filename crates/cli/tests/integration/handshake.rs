use crate::common;
use crate::common::ServerFixture;

fn assert_init_fails_with_env(env: &[(&str, &str)], expected_field: &str) {
    let fx = ServerFixture::builder().envs(env).start();
    let workspace = fx.dir("workspace");

    let init = fx.run(
        &workspace,
        &["init", "--server", &fx.addr, "--token", fx.token(), "."],
    );
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

    let watch = fx.run(
        fx.path(),
        &["watch", "--server", &fx.addr, "--token", fx.token()],
    );
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

#[test]
fn repository_load_shares_one_compatibility_handshake() {
    let fx = ServerFixture::builder().log_to_file().start();
    let workspace = fx.init_workspace("workspace", Some("workspace"));
    let before = fx.rpc_request_count("getInfo");
    common::assert_ok(
        &fx.run(&workspace, &["status", "--ignore-working-copy"]),
        "load repository",
    );
    assert_eq!(fx.rpc_request_count("getInfo") - before, 1);
}

#[test]
fn concurrent_repository_loads_keep_independent_sessions_and_fresh_heads() {
    let fx = ServerFixture::builder().log_to_file().start();
    let workspace = fx.init_workspace("workspace", Some("workspace"));
    let settings =
        jj_lib::settings::UserSettings::from_config(jj_lib::config::StackedConfig::with_defaults())
            .unwrap();
    let factories = jj_tandem_client::tandem_factories();
    let path = workspace.join(".jj/repo/store");
    let before = fx.rpc_request_count("getInfo");
    let barrier = std::sync::Barrier::new(8);
    let backends = std::thread::scope(|scope| {
        (0..8)
            .map(|_| {
                scope.spawn(|| {
                    barrier.wait();
                    jj_tandem_client::tandem_factories()
                        .load_backend(&settings, &path)
                        .unwrap()
                })
            })
            .collect::<Vec<_>>()
            .into_iter()
            .map(|thread| thread.join().unwrap())
            .collect::<Vec<_>>()
    });
    assert_eq!(fx.rpc_request_count("getInfo") - before, 8);
    let backend = factories.load_backend(&settings, &path).unwrap();
    let before = fx.rpc_request_count("getInfo");
    let heads = factories
        .load_op_heads_store(&settings, &workspace.join(".jj/repo/op_heads"))
        .unwrap();
    assert_eq!(fx.rpc_request_count("getInfo") - before, 0);
    let old_heads = pollster::block_on(heads.get_op_heads()).unwrap();
    std::fs::write(workspace.join("fresh.bin"), [0, 255, 3, 10]).unwrap();
    common::assert_ok(
        &fx.run(&workspace, &["status"]),
        "publish after session load",
    );
    assert_ne!(pollster::block_on(heads.get_op_heads()).unwrap(), old_heads);
    let shown = fx.run(&workspace, &["file", "show", "fresh.bin"]);
    common::assert_ok(&shown, "read published bytes");
    assert_eq!(shown.stdout, [0, 255, 3, 10]);
    drop(heads);
    drop(backends);
    drop(backend);
    let before = fx.rpc_request_count("getInfo");
    let _new_load = factories.load_backend(&settings, &path).unwrap();
    assert_eq!(
        fx.rpc_request_count("getInfo") - before,
        1,
        "released stores must not keep a stale session alive"
    );
}

#[test]
fn factory_sessions_separate_repository_paths_addresses_and_credentials() {
    let fx = ServerFixture::builder().log_to_file().start();
    let first = fx.init_workspace("first", Some("first"));
    let second = fx.init_workspace("second", Some("second"));
    let other = ServerFixture::builder().log_to_file().start();
    let settings =
        jj_lib::settings::UserSettings::from_config(jj_lib::config::StackedConfig::with_defaults())
            .unwrap();
    let factories = jj_tandem_client::tandem_factories();
    let first_path = first.join(".jj/repo/store");
    let second_path = second.join(".jj/repo/store");
    let before = fx.rpc_request_count("getInfo");
    let _first = factories.load_backend(&settings, &first_path).unwrap();
    let _second = factories.load_backend(&settings, &second_path).unwrap();
    assert_eq!(fx.rpc_request_count("getInfo") - before, 2);

    // A live session authenticated with the old bearer must not authorize a
    // load after the caller changes its credentials, even at the same path.
    std::fs::write(first_path.join("token"), "invalid-credential").unwrap();
    assert!(factories.load_backend(&settings, &first_path).is_err());
    jj_tandem_client::repo_link::write_link(&first_path, &other.addr, other.token()).unwrap();
    let before = other.rpc_request_count("getInfo");
    let _other = factories.load_backend(&settings, &first_path).unwrap();
    assert_eq!(other.rpc_request_count("getInfo") - before, 1);
    assert!(!fx.log_text().contains(fx.token()));
    assert!(!other.log_text().contains(other.token()));
}
