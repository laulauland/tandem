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

const RESILIENT_MAX_RETRIES: usize = 10;

fn hinted_op_integrate_id(stderr: &str) -> Option<String> {
    let marker = "jj op integrate ";
    let line = stderr.lines().find(|line| line.contains(marker))?;
    let suffix = line.split(marker).nth(1)?;
    let op_id = suffix.split('`').next()?.trim();
    if op_id.is_empty() {
        None
    } else {
        Some(op_id.to_string())
    }
}

fn reconcile_workspace_state(dir: &std::path::Path, home: &std::path::Path, stderr: &str) -> bool {
    let lower = stderr.to_lowercase();
    let retriable = lower.contains("working copy is stale")
        || lower.contains("update-stale")
        || lower.contains("seems to be a sibling of the working copy's operation")
        || (lower.contains("reconcile divergent operation heads")
            && lower.contains("already exists"));
    if !retriable {
        return false;
    }

    if let Some(op_id) = hinted_op_integrate_id(stderr) {
        let _ = common::run_tandem_in(dir, &["op", "integrate", &op_id], home);
    }
    let _ = common::run_tandem_in(dir, &["workspace", "update-stale"], home);
    true
}

fn run_tandem_in_resilient(
    dir: &std::path::Path,
    args: &[&str],
    home: &std::path::Path,
) -> std::process::Output {
    for attempt in 0..=RESILIENT_MAX_RETRIES {
        let out = common::run_tandem_in(dir, args, home);
        if out.status.success() {
            return out;
        }

        let stderr = common::stderr_str(&out);
        let retriable = reconcile_workspace_state(dir, home, &stderr);
        if !retriable || attempt == RESILIENT_MAX_RETRIES {
            return out;
        }

        thread::sleep(Duration::from_millis(20 * (attempt as u64 + 1)));
    }

    unreachable!("retry loop must return")
}

fn wait_for_conflicted_integration_commit_id(
    workspace_dir: &std::path::Path,
    home: &std::path::Path,
) -> String {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let out = common::run_tandem_in(
            workspace_dir,
            &[
                "log",
                "-r",
                "bookmarks(integration)",
                "--no-graph",
                "-T",
                "if(conflict, commit_id ++ \"\\n\", \"\")",
            ],
            home,
        );
        if out.status.success() {
            if let Some(commit_id) = common::stdout_str(&out)
                .lines()
                .map(str::trim)
                .find(|line| !line.is_empty())
            {
                return commit_id.to_string();
            }
        } else {
            let stderr = common::stderr_str(&out);
            let _ = reconcile_workspace_state(workspace_dir, home, &stderr);
        }
        if Instant::now() > deadline {
            panic!(
                "integration bookmark did not expose conflicted revisions\nstdout:\n{}\nstderr:\n{}",
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
        let describe =
            run_tandem_in_resilient(&a_dir, &["describe", "-m", "agent-a conflict"], &home_a);
        common::assert_ok(&describe, "agent-a describe");
        let new = run_tandem_in_resilient(&a_dir, &["new"], &home_a);
        common::assert_ok(&new, "agent-a new");
    });

    let b_dir = ws_b.clone();
    let home_b = home.clone();
    let barrier_b = barrier.clone();
    let handle_b = thread::spawn(move || {
        std::fs::write(b_dir.join("conflict.txt"), b"value from agent B\n").unwrap();
        barrier_b.wait();
        let describe =
            run_tandem_in_resilient(&b_dir, &["describe", "-m", "agent-b conflict"], &home_b);
        common::assert_ok(&describe, "agent-b describe");
        let new = run_tandem_in_resilient(&b_dir, &["new"], &home_b);
        common::assert_ok(&new, "agent-b new");
    });

    handle_a.join().expect("agent-a thread");
    handle_b.join().expect("agent-b thread");

    let conflicted_integration_commit = wait_for_conflicted_integration_commit_id(&ws_a, &home);

    // The conflicted integration commit should materialize conflict markers in file output.
    let cat = run_tandem_in_resilient(
        &ws_a,
        &[
            "file",
            "show",
            "-r",
            &conflicted_integration_commit,
            "conflict.txt",
        ],
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
