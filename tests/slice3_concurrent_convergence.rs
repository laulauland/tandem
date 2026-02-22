//! Slice 3: Concurrent file writes converge
//!
//! Acceptance criteria:
//! - Agent A writes src/a.rs and commits simultaneously with Agent B writing src/b.rs
//! - CAS contention triggers retries
//! - After convergence: both commits exist as heads
//! - jj cat src/a.rs works from both agents' perspectives
//! - jj cat src/b.rs works from both agents' perspectives
//! - No file content is lost or corrupted
//! - 5-agent variant: each writes a unique file, all 5 files survive

mod common;

use std::path::{Path, PathBuf};
use std::process::{Child, Output};
use std::sync::{Arc, Barrier};
use std::thread;

use tempfile::TempDir;

/// Run a tandem command, handling the "working copy is stale" condition
/// that arises naturally during concurrent operations. If the command
/// fails with a stale working copy, run `workspace update-stale` and retry.
fn run_tandem_in_resilient(dir: &Path, args: &[&str], home: &Path) -> Output {
    let output = common::run_tandem_in(dir, args, home);
    if output.status.success() {
        return output;
    }
    let err = common::stderr_str(&output);
    if err.contains("working copy is stale") || err.contains("update-stale") {
        let update = common::run_tandem_in(dir, &["workspace", "update-stale"], home);
        common::assert_ok(&update, "workspace update-stale (resilient)");
        // Retry the original command
        common::run_tandem_in(dir, args, home)
    } else {
        output
    }
}

/// Ensure the workspace is not stale before running queries.
fn settle(dir: &Path, label: &str, home: &Path) {
    let update = common::run_tandem_in(dir, &["workspace", "update-stale"], home);
    // update-stale is a no-op if already fresh; we don't fail if it says "nothing to do"
    if !update.status.success() {
        let err = common::stderr_str(&update);
        if !err.contains("nothing to do") && !err.contains("already up to date") {
            common::assert_ok(&update, &format!("{label} settle"));
        }
    }
}

/// Find the commit_id for a commit matching a description substring.
/// Uses `all()` revset to search all commits, returns the first matching commit_id.
/// This avoids issues with divergent change_ids during concurrent ops.
fn find_commit_id_by_description(dir: &Path, desc_substring: &str, home: &Path) -> String {
    let revset = format!("description(substring:\"{desc_substring}\")");
    let out = common::run_tandem_in(
        dir,
        &[
            "log",
            "--no-graph",
            "-r",
            &revset,
            "-T",
            "commit_id ++ \"\\n\"",
        ],
        home,
    );
    common::assert_ok(&out, &format!("find commit_id for '{desc_substring}'"));
    let text = common::stdout_str(&out);
    // May return multiple commit_ids if divergent; take the first non-empty one
    let commit_id = text
        .lines()
        .map(|l| l.trim())
        .find(|l| !l.is_empty())
        .unwrap_or("")
        .to_string();
    assert!(
        !commit_id.is_empty(),
        "should find commit_id for '{desc_substring}', got log output:\n{text}"
    );
    commit_id
}

/// Shared test infrastructure: start server, init N agent workspaces.
/// Uses a single TempDir with isolated HOME to prevent jj config pollution.
struct TestHarness {
    _root: TempDir,
    home: PathBuf,
    agent_dirs: Vec<PathBuf>,
    server: Child,
    #[allow(dead_code)]
    addr: String,
}

impl TestHarness {
    fn new(agent_count: usize) -> Self {
        Self::new_with_server_args(agent_count, &[])
    }

    fn new_with_server_args(agent_count: usize, server_extra_args: &[&str]) -> Self {
        let root = TempDir::new().unwrap();
        let home = common::isolated_home(root.path());
        let server_repo = root.path().join("server-repo");
        std::fs::create_dir_all(&server_repo).unwrap();

        let addr = common::free_addr();
        let mut server = if server_extra_args.is_empty() {
            common::spawn_server(&server_repo, &addr)
        } else {
            common::spawn_server_with_args(&server_repo, &addr, server_extra_args, &home)
        };
        common::wait_for_server(&addr, &mut server);

        let mut agent_dirs = Vec::new();
        for i in 0..agent_count {
            let workspace_name = format!("agent-{}", (b'a' + i as u8) as char);
            let dir = root.path().join(&workspace_name);
            std::fs::create_dir_all(&dir).unwrap();
            let init = common::run_tandem_in(
                &dir,
                &[
                    "init",
                    "--server",
                    &addr,
                    "--workspace",
                    &workspace_name,
                    ".",
                ],
                &home,
            );
            common::assert_ok(&init, &format!("init {workspace_name}"));
            agent_dirs.push(dir);
        }

        TestHarness {
            _root: root,
            home,
            agent_dirs,
            server,
            addr,
        }
    }
}

impl Drop for TestHarness {
    fn drop(&mut self) {
        let _ = self.server.kill();
        let _ = self.server.wait();
    }
}

// ─── Test: Two agents write distinct files concurrently ──────────────────────

#[test]
fn v1_slice3_two_agents_concurrent_file_writes_converge() {
    let harness = TestHarness::new(2);
    let agent_a = harness.agent_dirs[0].clone();
    let agent_b = harness.agent_dirs[1].clone();
    let home = harness.home.clone();

    let content_a = b"pub fn from_a() -> &'static str {\n    \"written by agent A\"\n}\n";
    let content_b = b"pub fn from_b() -> &'static str {\n    \"written by agent B\"\n}\n";

    // Use a barrier so both agents describe at the same time, maximizing CAS contention.
    let barrier = Arc::new(Barrier::new(2));

    let ba = barrier.clone();
    let a_dir = agent_a.clone();
    let home_a = home.clone();
    let handle_a = thread::spawn(move || {
        // Write file
        let src = a_dir.join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("a.rs"), content_a).unwrap();

        // Synchronize: both agents ready before describe
        ba.wait();

        // describe snapshots the working copy and updates the description.
        // CAS retries handle concurrent op head updates.
        let describe = run_tandem_in_resilient(&a_dir, &["describe", "-m", "add a.rs"], &home_a);
        common::assert_ok(&describe, "agent-a describe");

        // new creates a new empty child change; @- becomes the described change.
        // May need workspace update-stale if other agent committed concurrently.
        let new = run_tandem_in_resilient(&a_dir, &["new"], &home_a);
        common::assert_ok(&new, "agent-a new");
    });

    let bb = barrier.clone();
    let b_dir = agent_b.clone();
    let home_b = home.clone();
    let handle_b = thread::spawn(move || {
        let src = b_dir.join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("b.rs"), content_b).unwrap();

        bb.wait();

        let describe = run_tandem_in_resilient(&b_dir, &["describe", "-m", "add b.rs"], &home_b);
        common::assert_ok(&describe, "agent-b describe");

        let new = run_tandem_in_resilient(&b_dir, &["new"], &home_b);
        common::assert_ok(&new, "agent-b new");
    });

    handle_a.join().expect("agent-a thread");
    handle_b.join().expect("agent-b thread");

    // ── Settle: update stale working copies after concurrent ops ──────
    settle(&agent_a, "agent-a", &home);
    settle(&agent_b, "agent-b", &home);

    // ── Verify: both commits exist ────────────────────────────────────
    let log_a = common::run_tandem_in(&agent_a, &["log", "--no-graph", "-r", "all()"], &home);
    common::assert_ok(&log_a, "agent-a final log");
    let log_a_text = common::stdout_str(&log_a);
    assert!(
        log_a_text.contains("add a.rs"),
        "agent-a log should contain 'add a.rs'\n{log_a_text}"
    );
    assert!(
        log_a_text.contains("add b.rs"),
        "agent-a log should contain 'add b.rs'\n{log_a_text}"
    );

    let log_b = common::run_tandem_in(&agent_b, &["log", "--no-graph", "-r", "all()"], &home);
    common::assert_ok(&log_b, "agent-b final log");
    let log_b_text = common::stdout_str(&log_b);
    assert!(
        log_b_text.contains("add a.rs"),
        "agent-b log should contain 'add a.rs'\n{log_b_text}"
    );
    assert!(
        log_b_text.contains("add b.rs"),
        "agent-b log should contain 'add b.rs'\n{log_b_text}"
    );

    // ── Extract commit IDs for cross-workspace file reads ─────────────
    // Use commit_id (not change_id) to avoid divergent change_id issues
    // that naturally arise during concurrent operations.
    let commit_a = find_commit_id_by_description(&agent_a, "add a.rs", &home);
    let commit_b = find_commit_id_by_description(&agent_a, "add b.rs", &home);

    // ── Verify: Agent A can read both files (exact bytes) ─────────────
    let cat_a_from_a = common::run_tandem_in(
        &agent_a,
        &["file", "show", "-r", &commit_a, "src/a.rs"],
        &home,
    );
    common::assert_ok(&cat_a_from_a, "agent-a reads src/a.rs");
    assert_eq!(
        cat_a_from_a.stdout, content_a,
        "agent-a: src/a.rs content mismatch"
    );

    let cat_b_from_a = common::run_tandem_in(
        &agent_a,
        &["file", "show", "-r", &commit_b, "src/b.rs"],
        &home,
    );
    common::assert_ok(&cat_b_from_a, "agent-a reads src/b.rs");
    assert_eq!(
        cat_b_from_a.stdout, content_b,
        "agent-a: src/b.rs content mismatch"
    );

    // ── Verify: Agent B can read both files (exact bytes) ─────────────
    let cat_a_from_b = common::run_tandem_in(
        &agent_b,
        &["file", "show", "-r", &commit_a, "src/a.rs"],
        &home,
    );
    common::assert_ok(&cat_a_from_b, "agent-b reads src/a.rs");
    assert_eq!(
        cat_a_from_b.stdout, content_a,
        "agent-b: src/a.rs content mismatch"
    );

    let cat_b_from_b = common::run_tandem_in(
        &agent_b,
        &["file", "show", "-r", &commit_b, "src/b.rs"],
        &home,
    );
    common::assert_ok(&cat_b_from_b, "agent-b reads src/b.rs");
    assert_eq!(
        cat_b_from_b.stdout, content_b,
        "agent-b: src/b.rs content mismatch"
    );
}

// ─── Test: 5 agents each write a unique file concurrently ────────────────────

#[test]
fn v1_slice3_five_agents_concurrent_file_writes_all_survive() {
    let agent_count = 5;
    // Keep server logging minimal in this stress test. The test harness pipes
    // server stdout/stderr and does not continuously drain them; under 5-agent
    // contention, info-level logs can fill the pipe and cause false hangs.
    let harness = TestHarness::new_with_server_args(agent_count, &["--log-level", "error"]);
    let home = harness.home.clone();

    // Each agent writes src/agent_N.rs with unique content
    let contents: Vec<Vec<u8>> = (0..agent_count)
        .map(|i| {
            format!(
                "pub fn agent_{i}() -> &'static str {{\n    \"file written by agent {i}\"\n}}\n"
            )
            .into_bytes()
        })
        .collect();
    let filenames: Vec<String> = (0..agent_count)
        .map(|i| format!("src/agent_{i}.rs"))
        .collect();
    let descriptions: Vec<String> = (0..agent_count)
        .map(|i| format!("add agent_{i}.rs"))
        .collect();

    // Barrier synchronizes all 5 agents to describe at the same time
    let barrier = Arc::new(Barrier::new(agent_count));

    let handles: Vec<_> = (0..agent_count)
        .map(|i| {
            let bar = barrier.clone();
            let dir = harness.agent_dirs[i].clone();
            let content = contents[i].clone();
            let filename = filenames[i].clone();
            let desc = descriptions[i].clone();
            let home = home.clone();

            thread::spawn(move || {
                // Write file
                let src = dir.join("src");
                std::fs::create_dir_all(&src).unwrap();
                std::fs::write(dir.join(&filename), &content).unwrap();

                // Synchronize
                bar.wait();

                // Commit — describe snapshots the working copy, CAS retries converge
                let describe = run_tandem_in_resilient(&dir, &["describe", "-m", &desc], &home);
                common::assert_ok(&describe, &format!("agent-{i} describe"));

                // new — may need update-stale due to concurrent ops
                let new = run_tandem_in_resilient(&dir, &["new"], &home);
                common::assert_ok(&new, &format!("agent-{i} new"));
            })
        })
        .collect();

    for (i, h) in handles.into_iter().enumerate() {
        h.join()
            .unwrap_or_else(|_| panic!("agent-{i} thread panicked"));
    }

    // ── Settle: update stale working copies after concurrent ops ──────
    for (i, dir) in harness.agent_dirs.iter().enumerate() {
        settle(dir, &format!("agent-{i}"), &home);
    }

    // ── Verify: all commits visible from agent-0 ──────────────────────
    let log = common::run_tandem_in(
        &harness.agent_dirs[0],
        &["log", "--no-graph", "-r", "all()"],
        &home,
    );
    common::assert_ok(&log, "agent-0 log all");
    let log_text = common::stdout_str(&log);
    for desc in &descriptions {
        assert!(
            log_text.contains(desc.as_str()),
            "log should contain '{desc}'\n{log_text}"
        );
    }

    // ── Verify: all files readable from every agent (exact bytes) ─────
    // Collect commit_ids for each commit (from agent-0's perspective)
    let commit_ids: Vec<String> = descriptions
        .iter()
        .map(|desc| find_commit_id_by_description(&harness.agent_dirs[0], desc, &home))
        .collect();

    // From each agent, read every file
    for agent_idx in 0..agent_count {
        let agent_dir = &harness.agent_dirs[agent_idx];
        for file_idx in 0..agent_count {
            let cat = common::run_tandem_in(
                agent_dir,
                &[
                    "file",
                    "show",
                    "-r",
                    &commit_ids[file_idx],
                    &filenames[file_idx],
                ],
                &home,
            );
            common::assert_ok(
                &cat,
                &format!("agent-{agent_idx} reads {}", filenames[file_idx]),
            );
            assert_eq!(
                cat.stdout, contents[file_idx],
                "agent-{}: {} content mismatch",
                agent_idx, filenames[file_idx]
            );
        }
    }
}
