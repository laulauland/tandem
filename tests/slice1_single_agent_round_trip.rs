mod common;

use tempfile::TempDir;

#[test]
fn slice1_single_agent_file_round_trip() {
    let tmp = TempDir::new().unwrap();
    let home = common::isolated_home(tmp.path());
    let server_repo = tmp.path().join("server-repo");
    std::fs::create_dir_all(&server_repo).unwrap();
    let workspace_dir = tmp.path().join("workspace");
    std::fs::create_dir_all(&workspace_dir).unwrap();

    let addr = common::free_addr();
    let mut server = common::spawn_server(&server_repo, &addr);
    common::wait_for_server(&addr, &mut server);

    // Initialize workspace with tandem backend
    let init = common::run_tandem_in(&workspace_dir, &["init", "--server", &addr, "."], &home);
    common::assert_ok(&init, "tandem init");

    // Verify .jj structure was created
    assert!(workspace_dir.join(".jj").exists(), ".jj dir should exist");
    let store_type = std::fs::read_to_string(workspace_dir.join(".jj/repo/store/type")).unwrap();
    assert_eq!(store_type.trim(), "tandem", "store type should be tandem");

    // Write a file
    let src_dir = workspace_dir.join("src");
    std::fs::create_dir_all(&src_dir).unwrap();
    let file_content = b"fn main() { println!(\"hello tandem\"); }\n";
    std::fs::write(src_dir.join("hello.rs"), file_content).unwrap();

    // Create a commit: jj new creates a new empty change, making the previous
    // working copy (with the file) become @-.
    let new_out = common::run_tandem_in(&workspace_dir, &["new", "-m", "add hello"], &home);
    common::assert_ok(&new_out, "jj new");

    // Check log
    let log = common::run_tandem_in(&workspace_dir, &["log", "--no-graph", "-n", "5"], &home);
    common::assert_ok(&log, "jj log");
    let log_text = String::from_utf8_lossy(&log.stdout);
    assert!(
        log_text.contains("add hello"),
        "log should show commit description\n{log_text}"
    );

    // Check file show (read file from parent commit)
    let cat = common::run_tandem_in(
        &workspace_dir,
        &["file", "show", "-r", "@-", "src/hello.rs"],
        &home,
    );
    common::assert_ok(&cat, "jj file show");
    assert_eq!(
        cat.stdout, file_content,
        "file show should return exact file bytes"
    );

    // Check diff
    let diff = common::run_tandem_in(&workspace_dir, &["diff", "-r", "@-"], &home);
    common::assert_ok(&diff, "jj diff");
    let diff_text = String::from_utf8_lossy(&diff.stdout);
    assert!(
        diff_text.contains("hello.rs"),
        "diff should mention hello.rs\n{diff_text}"
    );

    // ── Server restart ────────────────────────────────────────────────
    let _ = server.kill();
    let _ = server.wait();

    let addr2 = common::free_addr();
    let mut server2 = common::spawn_server(&server_repo, &addr2);
    common::wait_for_server(&addr2, &mut server2);

    // After restart, use TANDEM_SERVER env to point to new address
    let log2 = common::run_tandem_in_with_env(
        &workspace_dir,
        &["log", "--no-graph", "-n", "5"],
        &[("TANDEM_SERVER", &addr2)],
        &home,
    );
    common::assert_ok(&log2, "jj log after restart");
    let log2_text = String::from_utf8_lossy(&log2.stdout);
    assert!(
        log2_text.contains("add hello"),
        "log after restart\n{log2_text}"
    );

    let cat2 = common::run_tandem_in_with_env(
        &workspace_dir,
        &["file", "show", "-r", "@-", "src/hello.rs"],
        &[("TANDEM_SERVER", &addr2)],
        &home,
    );
    common::assert_ok(&cat2, "jj file show after restart");
    assert_eq!(
        cat2.stdout, file_content,
        "file show after restart should return exact bytes"
    );

    let _ = server2.kill();
    let _ = server2.wait();
}
