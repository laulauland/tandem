//! Slice 17: integration bookmark conflict visibility
//!
//! Acceptance criteria:
//! - Conflicting workspace inputs produce a conflicted integration commit
//! - The conflicted result is visible via the `integration` bookmark

mod common;

use std::sync::{Arc, Barrier};
use std::thread;
use std::time::{Duration, Instant};

use tempfile::TempDir;

fn run_tandem_in_resilient(dir: &std::path::Path, args: &[&str], home: &std::path::Path) {
    let out = common::run_tandem_in(dir, args, home);
    if out.status.success() {
        return;
    }
    let err = common::stderr_str(&out).to_lowercase();
    if err.contains("working copy is stale") || err.contains("update-stale") {
        let update = common::run_tandem_in(dir, &["workspace", "update-stale"], home);
        common::assert_ok(&update, "workspace update-stale for resilient run");
        let retry = common::run_tandem_in(dir, args, home);
        common::assert_ok(&retry, "retry tandem command after update-stale");
    } else {
        common::assert_ok(&out, "tandem command");
    }
}

fn wait_for_conflicted_integration(workspace_dir: &std::path::Path, home: &std::path::Path) {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let out = common::run_tandem_in(
            workspace_dir,
            &[
                "log",
                "-r",
                "integration",
                "--no-graph",
                "-T",
                "conflict ++ \"\\n\"",
            ],
            home,
        );
        if out.status.success() {
            let value = common::stdout_str(&out).trim().to_string();
            if value == "true" {
                return;
            }
        }
        if Instant::now() > deadline {
            panic!(
                "integration bookmark did not become conflicted\nstdout:\n{}\nstderr:\n{}",
                common::stdout_str(&out),
                common::stderr_str(&out)
            );
        }
        thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn slice17_conflicting_workspace_inputs_surface_integration_conflict() {
    let tmp = TempDir::new().unwrap();
    let home = common::isolated_home(tmp.path());
    let server_repo = tmp.path().join("server-repo");
    std::fs::create_dir_all(&server_repo).unwrap();

    let ws_a = tmp.path().join("agent-a");
    let ws_b = tmp.path().join("agent-b");
    std::fs::create_dir_all(&ws_a).unwrap();
    std::fs::create_dir_all(&ws_b).unwrap();

    let addr = common::free_addr();
    let mut server = common::spawn_server_with_args(
        &server_repo,
        &addr,
        &["--enable-integration-workspace", "--log-level", "error"],
        &home,
    );
    common::wait_for_server(&addr, &mut server);

    let init_a = common::run_tandem_in(
        &ws_a,
        &["init", "--server", &addr, "--workspace", "agent-a", "."],
        &home,
    );
    common::assert_ok(&init_a, "init agent-a");
    let init_b = common::run_tandem_in(
        &ws_b,
        &["init", "--server", &addr, "--workspace", "agent-b", "."],
        &home,
    );
    common::assert_ok(&init_b, "init agent-b");

    let barrier = Arc::new(Barrier::new(2));

    let a_dir = ws_a.clone();
    let home_a = home.clone();
    let barrier_a = barrier.clone();
    let handle_a = thread::spawn(move || {
        std::fs::write(a_dir.join("conflict.txt"), b"value from agent A\n").unwrap();
        barrier_a.wait();
        run_tandem_in_resilient(&a_dir, &["describe", "-m", "agent-a conflict"], &home_a);
        run_tandem_in_resilient(&a_dir, &["new"], &home_a);
    });

    let b_dir = ws_b.clone();
    let home_b = home.clone();
    let barrier_b = barrier.clone();
    let handle_b = thread::spawn(move || {
        std::fs::write(b_dir.join("conflict.txt"), b"value from agent B\n").unwrap();
        barrier_b.wait();
        run_tandem_in_resilient(&b_dir, &["describe", "-m", "agent-b conflict"], &home_b);
        run_tandem_in_resilient(&b_dir, &["new"], &home_b);
    });

    handle_a.join().expect("agent-a thread");
    handle_b.join().expect("agent-b thread");

    wait_for_conflicted_integration(&ws_a, &home);

    // The conflicted integration commit should materialize conflict markers in file output.
    let cat = common::run_tandem_in(
        &ws_a,
        &["file", "show", "-r", "integration", "conflict.txt"],
        &home,
    );
    common::assert_ok(&cat, "show conflict file from integration bookmark");
    let text = common::stdout_str(&cat);
    assert!(
        text.contains("<<<<<<<") || text.contains("%%%%%%%") || text.contains("+++++++"),
        "expected conflict materialization markers in integration file\n{text}"
    );

    let _ = server.kill();
    let _ = server.wait();
}
