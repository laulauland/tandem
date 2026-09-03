//! Helpers every subprocess test used to keep its own copy of.
//!
//! Five files had a `settle`, four had a `find_commit_id`, and three had a
//! retry wrapper that each knew a slightly different set of errors to retry.
//! One copy each, here, so that a newly retriable error is fixed once.

#![allow(dead_code)]

use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::Output;
use std::thread;
use std::time::{Duration, Instant};

use super::{assert_ok, run_tandem_in, stderr_str, stdout_str};

/// How many times a command may be retried before the test calls it a
/// failure. Tests assert against it: a retry budget nobody checks is not a
/// bound, it is a hope.
pub const MAX_RETRIES: usize = 10;

/// Run a tandem command, retrying the errors that concurrency legitimately
/// produces. Returns the output and how many retries it took, because a test
/// that never retried and a test that retried ten times are not equally happy.
pub fn run_tandem_resilient(dir: &Path, args: &[&str], home: &Path) -> (Output, usize) {
    for attempt in 0..=MAX_RETRIES {
        let output = run_tandem_in(dir, args, home);
        if output.status.success() {
            return (output, attempt);
        }

        let err = stderr_str(&output);
        if !is_retriable_workspace_state(&err) || attempt == MAX_RETRIES {
            return (output, attempt);
        }

        if let Some(op_id) = hinted_op_integrate_id(&err) {
            let _ = run_tandem_in(dir, &["op", "integrate", &op_id], home);
        }
        let _ = run_tandem_in(dir, &["workspace", "update-stale"], home);

        thread::sleep(Duration::from_millis(20 * (attempt as u64 + 1)));
    }

    unreachable!("the retry loop returns on success and on the last attempt")
}

/// Whether an error means "your view of the repo moved under you", which is
/// the normal outcome of somebody else publishing, not a failure.
pub fn is_retriable_workspace_state(stderr: &str) -> bool {
    stderr.contains("working copy is stale")
        || stderr.contains("update-stale")
        || stderr.contains("seems to be a sibling of the working copy's operation")
        || (stderr.contains("reconcile divergent operation heads")
            && stderr.contains("already exists"))
}

/// jj sometimes names the operation to integrate in its error. Take it.
pub fn hinted_op_integrate_id(stderr: &str) -> Option<String> {
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

/// Bring a workspace up to date before asking it anything.
pub fn settle_workspace(dir: &Path, home: &Path) {
    let update = run_tandem_in(dir, &["workspace", "update-stale"], home);
    if !update.status.success() {
        let err = stderr_str(&update);
        if !err.contains("nothing to do") && !err.contains("already up to date") {
            assert_ok(&update, "workspace update-stale");
        }
    }
}

/// The commit id a `log -T commit_id` printed, insisting there is one.
///
/// Every caller wants the same three things — the command succeeded, the
/// output has a line in it, and that line is the answer — and `what` names the
/// question so a failure says which read came back empty.
pub fn first_commit_id(output: &Output, what: &str) -> String {
    assert_ok(output, what);
    let text = stdout_str(output);
    let commit_id = text
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default()
        .to_string();
    assert!(
        !commit_id.is_empty(),
        "{what}: no commit id came back:\n{text}"
    );
    commit_id
}

/// Find a commit by a substring of its description.
///
/// By description and not by change id on purpose: a change id can be
/// divergent, and a test that resolves one is testing the divergence as much
/// as whatever it meant to test.
pub fn find_commit_id(dir: &Path, description: &str, home: &Path) -> String {
    find_commit_id_maybe_ignoring_working_copy(dir, description, home, false)
}

/// The same, without touching the working copy.
///
/// That is what a command run against the *server's* repo has to do: the server
/// has a workspace, but nobody has ever checked it out, and snapshotting it
/// would be a write where the test only asked a question.
pub fn find_commit_id_on_server(dir: &Path, description: &str, home: &Path) -> String {
    find_commit_id_maybe_ignoring_working_copy(dir, description, home, true)
}

fn find_commit_id_maybe_ignoring_working_copy(
    dir: &Path,
    description: &str,
    home: &Path,
    ignore_working_copy: bool,
) -> String {
    let revset = format!("description(substring:\"{description}\")");
    let mut args = vec!["log", "--no-graph"];
    if ignore_working_copy {
        args.push("--ignore-working-copy");
    }
    args.extend(["-r", &revset, "-T", "commit_id ++ \"\\n\""]);
    let out = run_tandem_in(dir, &args, home);
    first_commit_id(&out, &format!("find the commit for '{description}'"))
}

/// Wait for something to become true, waking on the server's head events
/// rather than on a timer.
///
/// The old shape of this was a loop that spawned a `jj log` every fifty
/// milliseconds until it liked the answer — a hundred processes to observe one
/// change, and a fixed delay between the change and noticing it. The server
/// already says when its heads move. Subscribing costs one connection, wakes
/// on the event, and spawns a process only when there is a reason to.
pub fn wait_for<T>(
    addr: &str,
    token: &str,
    timeout: Duration,
    mut probe: impl FnMut() -> Option<T>,
) -> Option<T> {
    let deadline = Instant::now() + timeout;

    // Probe once: the thing may already have happened, and an event that has
    // already gone past will never arrive.
    if let Some(found) = probe() {
        return Some(found);
    }

    let stream = super::http_client()
        .get(format!("http://{addr}/api/events"))
        .header(reqwest::header::ACCEPT, "text/event-stream")
        .bearer_auth(token)
        .timeout(timeout)
        .send();

    let Ok(response) = stream else {
        // No event stream: fall back to asking, slowly, until the deadline.
        while Instant::now() < deadline {
            if let Some(found) = probe() {
                return Some(found);
            }
            thread::sleep(Duration::from_millis(50));
        }
        return None;
    };

    let mut reader = BufReader::new(response);
    let mut line = String::new();
    while Instant::now() < deadline {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => {}
            Err(_) => break,
        }
        // Only a data line is a change; a comment is the keep-alive.
        if !line.starts_with("data:") {
            continue;
        }
        if let Some(found) = probe() {
            return Some(found);
        }
    }

    // One last look: the event that mattered may have arrived as the deadline
    // passed, and a false negative here is a flake nobody can reproduce.
    probe()
}
