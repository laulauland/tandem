//! `tandem server logs`: streaming the daemon's log over the control socket.
//!
//! What is pinned here:
//! - with no daemon, it exits non-zero and says why
//! - `--json` emits one valid JSON object per line, as events happen
//! - `--level` filters: a warn stream carries nothing below warn
//! - the stream ends when the daemon does, instead of hanging
//!
//! The waits are deadline reads on the child's own stdout. The old shape slept
//! a second per step and then asserted on whatever had arrived, which made the
//! test both slow and unable to tell "not yet" from "never".

use crate::common;
use crate::common::lines::start_log_stream;
use crate::common::ServerFixture;

use std::time::{Duration, Instant};

use tempfile::TempDir;

const LINE_TIMEOUT: Duration = Duration::from_secs(15);

fn level_rank(level: &str) -> u8 {
    match level.to_lowercase().as_str() {
        "trace" => 0,
        "debug" => 1,
        "info" => 2,
        "warn" | "warning" => 3,
        "error" => 4,
        _ => 2,
    }
}

/// With nothing to connect to, the command must fail loudly rather than wait.
#[test]
fn logs_without_a_daemon_exit_nonzero_and_say_so() {
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
        "tandem server logs with no daemon should exit non-zero"
    );
    let combined = format!("{}{}", common::stdout_str(&out), common::stderr_str(&out));
    assert!(
        combined.contains("not running") || combined.contains("no tandem daemon running"),
        "the failure should say the daemon is not running\noutput: {combined}"
    );
}

/// One daemon, two subscribers, one shutdown.
///
/// This used to be three tests that each stood a daemon up to look at one
/// property of the same stream. The properties do not interact, so one daemon
/// serves all of them: subscribe at two levels, make something happen, and then
/// take the daemon away and watch both streams end.
#[test]
fn the_log_stream_carries_json_respects_the_level_and_ends_with_the_daemon() {
    let mut fx = ServerFixture::builder()
        .control_socket()
        .args(&["--log-level", "debug"])
        .start();
    let home = fx.home.clone();

    let (mut debug_child, mut debug_out) = start_log_stream(&fx.socket, "debug", &home);
    let (mut warn_child, mut warn_out) = start_log_stream(&fx.socket, "warn", &home);

    // Make something happen. The subscribers may still be connecting, so the
    // work is repeated until the debug stream shows a line — this is the one
    // race a deadline read cannot remove, because a subscription that is not
    // open yet legitimately misses what it did not hear.
    let deadline = Instant::now() + LINE_TIMEOUT;
    let workspace_dir = fx.init_workspace("workspace", None);

    let mut first = None;
    let mut round = 0;
    while Instant::now() < deadline && first.is_none() {
        std::fs::write(
            workspace_dir.join(format!("log-test-{round}.txt")),
            b"log content\n",
        )
        .unwrap();
        let new_out = common::run_tandem_in(
            &workspace_dir,
            &["new", "-m", &format!("log test {round}")],
            &home,
        );
        common::assert_ok(&new_out, "tandem new");
        round += 1;

        first = debug_out.wait_for(Duration::from_secs(2), |line| !line.trim().is_empty());
    }

    let first = first.unwrap_or_else(|| {
        panic!(
            "the debug stream produced nothing while the server worked\nstdout:\n{}",
            debug_out.transcript()
        )
    });

    // ── One JSON object per line ──────────────────────────────────────
    for line in debug_out.seen().iter().chain(warn_out.seen().iter()) {
        if line.trim().is_empty() {
            continue;
        }
        let parsed: Result<serde_json::Value, _> = serde_json::from_str(line);
        assert!(
            parsed.is_ok(),
            "every streamed line must be one JSON object\nline: {line}"
        );
    }
    assert!(
        serde_json::from_str::<serde_json::Value>(&first).is_ok(),
        "the first streamed line must be JSON: {first}"
    );

    // ── The level filter is a filter ──────────────────────────────────
    let warn_lines: Vec<String> = warn_out.seen().to_vec();
    for line in &warn_lines {
        if line.trim().is_empty() {
            continue;
        }
        let entry: serde_json::Value = serde_json::from_str(line).expect("warn line JSON");
        let level = entry["level"].as_str().unwrap_or("info");
        assert!(
            level_rank(level) >= level_rank("warn"),
            "a warn stream must not carry a {level} line: {line}"
        );
    }
    let debug_count = debug_out.seen().len();
    assert!(
        debug_count >= warn_lines.len(),
        "the debug stream ({debug_count} lines) cannot be smaller than the warn stream ({} lines)",
        warn_lines.len()
    );

    // ── The stream ends with the daemon ───────────────────────────────
    #[cfg(unix)]
    unsafe {
        libc::kill(fx.server.id() as libc::pid_t, libc::SIGINT);
    }
    let _ = fx.server.wait();

    for (name, child) in [("debug", &mut debug_child), ("warn", &mut warn_child)] {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if child.try_wait().expect("poll the log stream").is_some() {
                break;
            }
            if Instant::now() > deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!("the {name} log stream did not exit after the daemon shut down");
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}
