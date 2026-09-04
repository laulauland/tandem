//! The control socket, and what `tandem server status` reads over it.
//!
//! What is pinned here:
//! - `tandem serve` creates control socket when --control-socket is passed.
//! - `tandem status` prints human-readable output while server runs.
//! - `tandem status --json` returns valid JSON with pid, uptime, repo, listen fields.
//! - `tandem status` exits 1 when no server is running.
//! - Control socket is cleaned up on server exit.

use crate::common;
use crate::common::ServerFixture;

use tempfile::TempDir;

/// Server creates control socket, tandem status --json returns valid data.
#[test]
fn status_json_while_running() {
    let mut fx = ServerFixture::builder().control_socket().start();

    // Run tandem status --json
    let status_out = fx.run(
        fx.path(),
        &[
            "server",
            "status",
            "--json",
            "--control-socket",
            fx.socket_str(),
        ],
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
    assert!(
        parsed.get("integration").is_none(),
        "automatic integration was removed"
    );

    // Cleanup
    #[cfg(unix)]
    unsafe {
        libc::kill(fx.server.id() as libc::pid_t, libc::SIGINT);
    }
    let _ = fx.server.wait();
}

/// tandem status (human-readable) while server is running.
#[test]
fn status_human_while_running() {
    let mut fx = ServerFixture::builder().control_socket().start();

    let status_out = fx.run(
        fx.path(),
        &["server", "status", "--control-socket", fx.socket_str()],
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
        libc::kill(fx.server.id() as libc::pid_t, libc::SIGINT);
    }
    let _ = fx.server.wait();
}

/// tandem status exits 1 when no server is running.
#[test]
fn status_not_running() {
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
fn socket_cleaned_up_on_exit() {
    let mut fx = ServerFixture::builder().control_socket().start();

    assert!(
        fx.socket.exists(),
        "control socket should exist while running"
    );

    // Send SIGINT
    #[cfg(unix)]
    unsafe {
        libc::kill(fx.server.id() as libc::pid_t, libc::SIGINT);
    }

    let _ = fx.server.wait();

    // Socket should be cleaned up
    assert!(
        !fx.socket.exists(),
        "control socket should be removed after server exit"
    );
}

/// Control socket status endpoint reports correct repo and listen address.
#[test]
fn status_reports_correct_info() {
    let mut fx = ServerFixture::builder().control_socket().start();

    let status_out = fx.run(
        fx.path(),
        &[
            "server",
            "status",
            "--json",
            "--control-socket",
            fx.socket_str(),
        ],
    );
    common::assert_ok(&status_out, "tandem status --json");

    let json_str = common::stdout_str(&status_out);
    let parsed: serde_json::Value = serde_json::from_str(json_str.trim()).unwrap();

    // The listen address should match what we passed
    let listen_val = parsed["listen"].as_str().unwrap();
    assert!(
        listen_val.contains(&fx.addr) || fx.addr.contains(listen_val),
        "listen should match addr {}, got {listen_val}",
        fx.addr
    );

    // Cleanup
    #[cfg(unix)]
    unsafe {
        libc::kill(fx.server.id() as libc::pid_t, libc::SIGINT);
    }
    let _ = fx.server.wait();
}
