//! Slice 12: tandem up and tandem down
//!
//! Acceptance criteria:
//! - `tandem up --repo ... --listen ...` returns immediately, daemon is running.
//! - `tandem status` shows running after `tandem up`.
//! - `tandem down` stops daemon, `tandem status` shows not running.
//! - `tandem up` twice: second invocation errors with "already running".
//! - PID file and control socket cleaned up after `tandem down`.

mod common;

use std::time::Duration;
use tempfile::TempDir;

/// tandem up starts daemon, status shows running, down stops it.
#[test]
fn slice12_up_status_down() {
    let tmp = TempDir::new().unwrap();
    let home = common::isolated_home(tmp.path());
    let server_repo = tmp.path().join("server-repo");
    std::fs::create_dir_all(&server_repo).unwrap();

    let addr = common::free_addr();
    let sock = common::control_socket_path(tmp.path());
    let sock_str = sock.to_str().unwrap();
    let log_file = tmp.path().join("daemon.log");
    let log_file_str = log_file.to_str().unwrap();

    // tandem up
    let up_out = common::run_tandem_in(
        tmp.path(),
        &[
            "up",
            "--repo",
            server_repo.to_str().unwrap(),
            "--listen",
            &addr,
            "--control-socket",
            sock_str,
            "--log-file",
            log_file_str,
        ],
        &home,
    );
    common::assert_ok(&up_out, "tandem up");
    let up_text = common::stdout_str(&up_out);
    assert!(
        up_text.contains("tandem running"),
        "should print 'tandem running'\noutput: {up_text}"
    );

    // Server should be listening
    common::wait_for_addr(&addr, Duration::from_secs(10));

    // tandem status should show running
    let status_out = common::run_tandem_in(
        tmp.path(),
        &["server", "status", "--json", "--control-socket", sock_str],
        &home,
    );
    common::assert_ok(&status_out, "tandem status after up");
    let json_str = common::stdout_str(&status_out);
    let parsed: serde_json::Value = serde_json::from_str(json_str.trim())
        .unwrap_or_else(|e| panic!("invalid JSON: {e}\nraw: {json_str}"));
    assert_eq!(parsed["running"], true);

    // tandem down
    let down_out =
        common::run_tandem_in(tmp.path(), &["down", "--control-socket", sock_str], &home);
    common::assert_ok(&down_out, "tandem down");
    let down_text = format!(
        "{}{}",
        common::stdout_str(&down_out),
        common::stderr_str(&down_out)
    );
    assert!(
        down_text.contains("tandem stopped"),
        "should print 'tandem stopped'\noutput: {down_text}"
    );

    // Wait a moment for cleanup
    std::thread::sleep(Duration::from_millis(500));

    // tandem status should show not running
    let status_after = common::run_tandem_in(
        tmp.path(),
        &["server", "status", "--control-socket", sock_str],
        &home,
    );
    assert!(
        !status_after.status.success(),
        "status should exit 1 after down"
    );

    // Control socket should be cleaned up
    assert!(
        !sock.exists(),
        "control socket should be removed after down"
    );
}

/// tandem up twice returns error on second invocation.
#[test]
fn slice12_up_twice_errors() {
    let tmp = TempDir::new().unwrap();
    let home = common::isolated_home(tmp.path());
    let server_repo = tmp.path().join("server-repo");
    std::fs::create_dir_all(&server_repo).unwrap();

    let addr = common::free_addr();
    let sock = common::control_socket_path(tmp.path());
    let sock_str = sock.to_str().unwrap();
    let log_file = tmp.path().join("daemon.log");
    let log_file_str = log_file.to_str().unwrap();

    // First up
    let up1 = common::run_tandem_in(
        tmp.path(),
        &[
            "up",
            "--repo",
            server_repo.to_str().unwrap(),
            "--listen",
            &addr,
            "--control-socket",
            sock_str,
            "--log-file",
            log_file_str,
        ],
        &home,
    );
    common::assert_ok(&up1, "first tandem up");

    // Second up should fail
    let addr2 = common::free_addr();
    let up2 = common::run_tandem_in(
        tmp.path(),
        &[
            "up",
            "--repo",
            server_repo.to_str().unwrap(),
            "--listen",
            &addr2,
            "--control-socket",
            sock_str,
            "--log-file",
            log_file_str,
        ],
        &home,
    );
    assert!(!up2.status.success(), "second tandem up should fail");
    let combined = format!("{}{}", common::stdout_str(&up2), common::stderr_str(&up2));
    assert!(
        combined.contains("already running"),
        "should say 'already running'\noutput: {combined}"
    );

    // Cleanup: bring it down
    let _ = common::run_tandem_in(tmp.path(), &["down", "--control-socket", sock_str], &home);
    std::thread::sleep(Duration::from_millis(500));
}

/// Full round-trip: up → init workspace → write file → read file → down.
#[test]
fn slice12_up_roundtrip_down() {
    let tmp = TempDir::new().unwrap();
    let home = common::isolated_home(tmp.path());
    let server_repo = tmp.path().join("server-repo");
    std::fs::create_dir_all(&server_repo).unwrap();
    let workspace_dir = tmp.path().join("workspace");
    std::fs::create_dir_all(&workspace_dir).unwrap();

    let addr = common::free_addr();
    let sock = common::control_socket_path(tmp.path());
    let sock_str = sock.to_str().unwrap();
    let log_file = tmp.path().join("daemon.log");
    let log_file_str = log_file.to_str().unwrap();

    // Start daemon
    let up_out = common::run_tandem_in(
        tmp.path(),
        &[
            "up",
            "--repo",
            server_repo.to_str().unwrap(),
            "--listen",
            &addr,
            "--control-socket",
            sock_str,
            "--log-file",
            log_file_str,
        ],
        &home,
    );
    common::assert_ok(&up_out, "tandem up");

    // Init workspace
    let init = common::run_tandem_in(&workspace_dir, &["init", "--server", &addr, "."], &home);
    common::assert_ok(&init, "tandem init");

    // Write a file
    std::fs::write(workspace_dir.join("hello.txt"), b"daemon test\n").unwrap();
    let new_out = common::run_tandem_in(&workspace_dir, &["new", "-m", "daemon write"], &home);
    common::assert_ok(&new_out, "tandem new");

    // Read the file back
    let cat = common::run_tandem_in(
        &workspace_dir,
        &["file", "show", "-r", "@-", "hello.txt"],
        &home,
    );
    common::assert_ok(&cat, "file show");
    assert_eq!(cat.stdout, b"daemon test\n", "file content round-trip");

    // Bring it down
    let down_out =
        common::run_tandem_in(tmp.path(), &["down", "--control-socket", sock_str], &home);
    common::assert_ok(&down_out, "tandem down");
    std::thread::sleep(Duration::from_millis(500));

    // Verify stopped
    let status = common::run_tandem_in(
        tmp.path(),
        &["server", "status", "--control-socket", sock_str],
        &home,
    );
    assert!(!status.status.success(), "status should fail after down");
}

/// tandem down with no daemon running exits 1.
#[test]
fn slice12_down_not_running() {
    let tmp = TempDir::new().unwrap();
    let home = common::isolated_home(tmp.path());

    let sock = tmp.path().join("nonexistent.sock");
    let sock_str = sock.to_str().unwrap();

    let down_out =
        common::run_tandem_in(tmp.path(), &["down", "--control-socket", sock_str], &home);
    assert!(
        !down_out.status.success(),
        "tandem down with no daemon should exit 1"
    );
    let combined = format!(
        "{}{}",
        common::stdout_str(&down_out),
        common::stderr_str(&down_out)
    );
    assert!(
        combined.contains("not running"),
        "should say not running\noutput: {combined}"
    );
}
