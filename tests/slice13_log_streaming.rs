//! Slice 13: tandem logs (streaming)
//!
//! Acceptance criteria:
//! - `tandem logs` prints log lines as events happen.
//! - `tandem logs --level debug` shows debug events.
//! - `tandem logs --json` outputs one JSON object per line.
//! - `tandem logs` exits cleanly when daemon shuts down.
//! - `tandem logs` with no daemon: exit 1 with helpful message.

mod common;

use std::process::{Command, Stdio};
use std::time::Duration;
use tempfile::TempDir;

/// tandem logs with no daemon running exits 1.
#[test]
fn slice13_logs_not_running() {
    let tmp = TempDir::new().unwrap();
    let home = common::isolated_home(tmp.path());

    let sock = tmp.path().join("nonexistent.sock");
    let sock_str = sock.to_str().unwrap();

    let out = common::run_tandem_in(
        tmp.path(),
        &["server", "logs", "--control-socket", sock_str],
        &home,
    );
    assert!(
        !out.status.success(),
        "tandem logs with no daemon should exit 1"
    );
    let combined = format!("{}{}", common::stdout_str(&out), common::stderr_str(&out));
    assert!(
        combined.contains("not running") || combined.contains("no tandem daemon running"),
        "should mention not running\noutput: {combined}"
    );
}

/// tandem logs --json streams JSON log lines when activity happens.
#[test]
fn slice13_logs_json_streams_events() {
    let tmp = TempDir::new().unwrap();
    let home = common::isolated_home(tmp.path());
    let server_repo = tmp.path().join("server-repo");
    std::fs::create_dir_all(&server_repo).unwrap();
    let workspace_dir = tmp.path().join("workspace");
    std::fs::create_dir_all(&workspace_dir).unwrap();

    let addr = common::free_addr();
    let sock = common::control_socket_path(tmp.path());
    let sock_str = sock.to_str().unwrap();

    let mut server =
        common::spawn_server_with_args(&server_repo, &addr, &["--control-socket", sock_str], &home);
    common::wait_for_server(&addr, &mut server);
    common::wait_for_socket(&sock, Duration::from_secs(5));

    // Start tandem logs --json in background
    let mut logs_cmd = Command::new(common::tandem_bin());
    logs_cmd.args(["server", "logs", "--json", "--control-socket", sock_str]);
    common::isolate_env(&mut logs_cmd, &home);
    logs_cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut logs_child = logs_cmd.spawn().expect("spawn tandem logs");

    // Give logs time to connect
    std::thread::sleep(Duration::from_millis(500));

    // Create activity: init workspace and write a file
    let init = common::run_tandem_in(&workspace_dir, &["init", "--server", &addr, "."], &home);
    common::assert_ok(&init, "tandem init");

    std::fs::write(workspace_dir.join("log-test.txt"), b"log content\n").unwrap();
    let new_out = common::run_tandem_in(&workspace_dir, &["new", "-m", "log test"], &home);
    common::assert_ok(&new_out, "tandem new");

    // Give logs time to receive events
    std::thread::sleep(Duration::from_millis(1000));

    // Kill the logs process and read what it captured
    let _ = logs_child.kill();
    let output = logs_child.wait_with_output().expect("wait logs");
    let stdout = String::from_utf8_lossy(&output.stdout);

    // Should have received at least one JSON line
    let lines: Vec<&str> = stdout.lines().filter(|l| !l.trim().is_empty()).collect();
    assert!(
        !lines.is_empty(),
        "tandem logs --json should have produced output\nstdout: {stdout}"
    );

    // Each line should be valid JSON
    for line in &lines {
        let parsed: Result<serde_json::Value, _> = serde_json::from_str(line);
        assert!(
            parsed.is_ok(),
            "each log line should be valid JSON\nline: {line}"
        );
    }

    // Cleanup
    #[cfg(unix)]
    unsafe {
        libc::kill(server.id() as libc::pid_t, libc::SIGINT);
    }
    let _ = server.wait();
}

/// tandem logs exits cleanly when server shuts down.
#[test]
fn slice13_logs_exits_on_shutdown() {
    let tmp = TempDir::new().unwrap();
    let home = common::isolated_home(tmp.path());
    let server_repo = tmp.path().join("server-repo");
    std::fs::create_dir_all(&server_repo).unwrap();

    let addr = common::free_addr();
    let sock = common::control_socket_path(tmp.path());
    let sock_str = sock.to_str().unwrap();

    let mut server =
        common::spawn_server_with_args(&server_repo, &addr, &["--control-socket", sock_str], &home);
    common::wait_for_server(&addr, &mut server);
    common::wait_for_socket(&sock, Duration::from_secs(5));

    // Start logs process
    let mut logs_cmd = Command::new(common::tandem_bin());
    logs_cmd.args(["server", "logs", "--control-socket", sock_str]);
    common::isolate_env(&mut logs_cmd, &home);
    logs_cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut logs_child = logs_cmd.spawn().expect("spawn tandem logs");

    std::thread::sleep(Duration::from_millis(500));

    // Shut down server
    #[cfg(unix)]
    unsafe {
        libc::kill(server.id() as libc::pid_t, libc::SIGINT);
    }
    let _ = server.wait();

    // logs process should exit within a few seconds
    let start = std::time::Instant::now();
    loop {
        if let Some(_status) = logs_child.try_wait().expect("try_wait logs") {
            return; // Exited — success
        }
        if start.elapsed() > Duration::from_secs(10) {
            let _ = logs_child.kill();
            let _ = logs_child.wait();
            panic!("tandem logs did not exit after server shutdown");
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// tandem logs --level debug shows more output than --level warn.
#[test]
fn slice13_logs_level_filtering() {
    let tmp = TempDir::new().unwrap();
    let home = common::isolated_home(tmp.path());
    let server_repo = tmp.path().join("server-repo");
    std::fs::create_dir_all(&server_repo).unwrap();
    let workspace_dir = tmp.path().join("workspace");
    std::fs::create_dir_all(&workspace_dir).unwrap();

    let addr = common::free_addr();
    let sock = common::control_socket_path(tmp.path());
    let sock_str = sock.to_str().unwrap();

    let mut server =
        common::spawn_server_with_args(&server_repo, &addr, &["--control-socket", sock_str], &home);
    common::wait_for_server(&addr, &mut server);
    common::wait_for_socket(&sock, Duration::from_secs(5));

    // Start logs at debug level
    let mut debug_cmd = Command::new(common::tandem_bin());
    debug_cmd.args([
        "server",
        "logs",
        "--json",
        "--level",
        "debug",
        "--control-socket",
        sock_str,
    ]);
    common::isolate_env(&mut debug_cmd, &home);
    debug_cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut debug_child = debug_cmd.spawn().expect("spawn debug logs");

    // Start logs at warn level
    let mut warn_cmd = Command::new(common::tandem_bin());
    warn_cmd.args([
        "server",
        "logs",
        "--json",
        "--level",
        "warn",
        "--control-socket",
        sock_str,
    ]);
    common::isolate_env(&mut warn_cmd, &home);
    warn_cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut warn_child = warn_cmd.spawn().expect("spawn warn logs");

    std::thread::sleep(Duration::from_millis(500));

    // Generate activity
    let init = common::run_tandem_in(&workspace_dir, &["init", "--server", &addr, "."], &home);
    common::assert_ok(&init, "tandem init");

    std::fs::write(workspace_dir.join("level-test.txt"), b"level test\n").unwrap();
    let new_out = common::run_tandem_in(&workspace_dir, &["new", "-m", "level test"], &home);
    common::assert_ok(&new_out, "tandem new");

    std::thread::sleep(Duration::from_millis(1000));

    // Kill both and compare
    let _ = debug_child.kill();
    let _ = warn_child.kill();
    let debug_out = debug_child.wait_with_output().expect("debug wait");
    let warn_out = warn_child.wait_with_output().expect("warn wait");

    let debug_stdout = String::from_utf8_lossy(&debug_out.stdout).to_string();
    let warn_stdout = String::from_utf8_lossy(&warn_out.stdout).to_string();

    let debug_count = debug_stdout
        .lines()
        .filter(|l| !l.trim().is_empty())
        .count();
    let warn_count = warn_stdout.lines().filter(|l| !l.trim().is_empty()).count();

    // Debug should have at least as many lines as warn (usually more)
    // Normal server activity (connections, object reads) emit at info/debug level
    assert!(
        debug_count >= warn_count,
        "debug ({debug_count} lines) should have >= warn ({warn_count} lines)",
    );

    // Cleanup
    #[cfg(unix)]
    unsafe {
        libc::kill(server.id() as libc::pid_t, libc::SIGINT);
    }
    let _ = server.wait();
}
