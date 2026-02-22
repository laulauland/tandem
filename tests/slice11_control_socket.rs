//! Slice 11: Control socket and tandem status
//!
//! Acceptance criteria:
//! - `tandem serve` creates control socket when --control-socket is passed.
//! - `tandem status` prints human-readable output while server runs.
//! - `tandem status --json` returns valid JSON with pid, uptime, repo, listen fields.
//! - `tandem status` exits 1 when no server is running.
//! - Control socket is cleaned up on server exit.

mod common;

use std::time::Duration;
use tempfile::TempDir;

/// Server creates control socket, tandem status --json returns valid data.
#[test]
fn slice11_status_json_while_running() {
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

    // Run tandem status --json
    let status_out = common::run_tandem_in(
        tmp.path(),
        &["server", "status", "--json", "--control-socket", sock_str],
        &home,
    );
    common::assert_ok(&status_out, "tandem status --json");

    let json_str = common::stdout_str(&status_out);
    let parsed: serde_json::Value = serde_json::from_str(json_str.trim())
        .unwrap_or_else(|e| panic!("invalid JSON from status: {e}\nraw: {json_str}"));

    assert_eq!(parsed["running"], true, "should report running=true");
    assert!(parsed["pid"].is_number(), "should have numeric pid");
    assert!(
        parsed["uptime_secs"].is_number(),
        "should have numeric uptime_secs"
    );
    assert!(parsed["repo"].is_string(), "should have repo string");
    assert!(parsed["listen"].is_string(), "should have listen string");
    assert!(parsed["version"].is_string(), "should have version string");

    // Cleanup
    #[cfg(unix)]
    unsafe {
        libc::kill(server.id() as libc::pid_t, libc::SIGINT);
    }
    let _ = server.wait();
}

/// tandem status (human-readable) while server is running.
#[test]
fn slice11_status_human_while_running() {
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

    let status_out = common::run_tandem_in(
        tmp.path(),
        &["server", "status", "--control-socket", sock_str],
        &home,
    );
    common::assert_ok(&status_out, "tandem status");

    let out = common::stdout_str(&status_out);
    assert!(
        out.contains("tandem is running"),
        "should say 'tandem is running'\noutput: {out}"
    );
    assert!(out.contains("PID"), "should show PID\noutput: {out}");

    // Cleanup
    #[cfg(unix)]
    unsafe {
        libc::kill(server.id() as libc::pid_t, libc::SIGINT);
    }
    let _ = server.wait();
}

/// tandem status exits 1 when no server is running.
#[test]
fn slice11_status_not_running() {
    let tmp = TempDir::new().unwrap();
    let home = common::isolated_home(tmp.path());

    // Use a non-existent socket path
    let sock = tmp.path().join("nonexistent.sock");
    let sock_str = sock.to_str().unwrap();

    let status_out = common::run_tandem_in(
        tmp.path(),
        &["server", "status", "--control-socket", sock_str],
        &home,
    );

    assert!(
        !status_out.status.success(),
        "tandem status should exit 1 when no server is running"
    );

    let combined = format!(
        "{}{}",
        common::stdout_str(&status_out),
        common::stderr_str(&status_out)
    );
    assert!(
        combined.contains("not running"),
        "should say 'not running'\noutput: {combined}"
    );
}

/// Control socket is cleaned up after server exits.
#[test]
fn slice11_socket_cleaned_up_on_exit() {
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

    assert!(sock.exists(), "control socket should exist while running");

    // Send SIGINT
    #[cfg(unix)]
    unsafe {
        libc::kill(server.id() as libc::pid_t, libc::SIGINT);
    }

    let _ = server.wait();

    // Socket should be cleaned up
    assert!(
        !sock.exists(),
        "control socket should be removed after server exit"
    );
}

/// Control socket status endpoint reports correct repo and listen address.
#[test]
fn slice11_status_reports_correct_info() {
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

    let status_out = common::run_tandem_in(
        tmp.path(),
        &["server", "status", "--json", "--control-socket", sock_str],
        &home,
    );
    common::assert_ok(&status_out, "tandem status --json");

    let json_str = common::stdout_str(&status_out);
    let parsed: serde_json::Value = serde_json::from_str(json_str.trim()).unwrap();

    // The listen address should match what we passed
    let listen_val = parsed["listen"].as_str().unwrap();
    assert!(
        listen_val.contains(&addr) || addr.contains(listen_val),
        "listen should match addr {addr}, got {listen_val}"
    );

    // Cleanup
    #[cfg(unix)]
    unsafe {
        libc::kill(server.id() as libc::pid_t, libc::SIGINT);
    }
    let _ = server.wait();
}
