//! Slice 21: contention retry stability
//!
//! Soak-style integration coverage for repeated contention cycles.
//! We assert that repeated concurrent write cycles converge, file bytes remain
//! intact, and workspace-state retry loops stay bounded.

mod common;

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::Duration;

use tempfile::TempDir;

const COMMAND_MAX_RETRIES: usize = 10;

fn run_tandem_resilient(dir: &Path, args: &[&str], home: &Path) -> (Output, usize) {
    for attempt in 0..=COMMAND_MAX_RETRIES {
        let output = common::run_tandem_in(dir, args, home);
        if output.status.success() {
            return (output, attempt);
        }

        let err = common::stderr_str(&output);
        let retriable = err.contains("working copy is stale")
            || err.contains("update-stale")
            || err.contains("seems to be a sibling of the working copy's operation")
            || (err.contains("reconcile divergent operation heads")
                && err.contains("already exists"));

        if !retriable || attempt == COMMAND_MAX_RETRIES {
            return (output, attempt);
        }

        if let Some(op_id) = hinted_op_integrate_id(&err) {
            let _ = common::run_tandem_in(dir, &["op", "integrate", &op_id], home);
        }
        let _ = common::run_tandem_in(dir, &["workspace", "update-stale"], home);

        thread::sleep(std::time::Duration::from_millis(20 * (attempt as u64 + 1)));
    }

    unreachable!("retry loop exits on success/failure")
}

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

fn settle_workspace(dir: &Path, home: &Path) {
    let update = common::run_tandem_in(dir, &["workspace", "update-stale"], home);
    if !update.status.success() {
        let err = common::stderr_str(&update);
        if !err.contains("nothing to do") && !err.contains("already up to date") {
            common::assert_ok(&update, "workspace update-stale settle");
        }
    }
}

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
    let commit_id = text
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("")
        .to_string();
    assert!(
        !commit_id.is_empty(),
        "missing commit id for {desc_substring}"
    );
    commit_id
}

fn log_field_str<'a>(entry: &'a serde_json::Value, key: &str) -> Option<&'a str> {
    entry.get("fields")?.get(key)?.as_str()
}

fn start_log_stream(home: &Path, socket: &Path, output_path: &Path) -> Child {
    let mut cmd = Command::new(common::tandem_bin());
    cmd.args([
        "server",
        "logs",
        "--json",
        "--level",
        "debug",
        "--control-socket",
        socket.to_str().expect("socket path"),
    ]);
    common::isolate_env(&mut cmd, home);
    let output_file = std::fs::File::create(output_path).expect("create logs output file");
    cmd.stdout(Stdio::from(output_file)).stderr(Stdio::null());
    cmd.spawn().expect("spawn tandem server logs")
}

fn spawn_observability_server(repo: &Path, addr: &str, socket: &Path, home: &Path) -> Child {
    let mut cmd = Command::new(common::tandem_bin());
    cmd.args([
        "serve",
        "--listen",
        addr,
        "--repo",
        repo.to_str().expect("repo path"),
        "--control-socket",
        socket.to_str().expect("socket path"),
        "--log-level",
        "debug",
    ]);
    common::isolate_env(&mut cmd, home);
    cmd.stdout(Stdio::null()).stderr(Stdio::null());
    cmd.spawn().expect("spawn observability server")
}

struct TestHarness {
    _root: TempDir,
    home: PathBuf,
    agent_dirs: Vec<PathBuf>,
    server: Child,
}

impl TestHarness {
    fn new(agent_count: usize) -> Self {
        let root = TempDir::new().expect("tempdir");
        let home = common::isolated_home(root.path());
        let server_repo = root.path().join("server-repo");
        std::fs::create_dir_all(&server_repo).expect("create server repo");

        let addr = common::free_addr();
        let mut server =
            common::spawn_server_with_args(&server_repo, &addr, &["--log-level", "error"], &home);
        common::wait_for_server(&addr, &mut server);

        let mut agent_dirs = Vec::with_capacity(agent_count);
        for i in 0..agent_count {
            let workspace_name = format!("agent-{}", i);
            let dir = root.path().join(&workspace_name);
            std::fs::create_dir_all(&dir).expect("create workspace dir");
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

        Self {
            _root: root,
            home,
            agent_dirs,
            server,
        }
    }
}

impl Drop for TestHarness {
    fn drop(&mut self) {
        let _ = self.server.kill();
        let _ = self.server.wait();
    }
}

fn run_contention_cycle(harness: &TestHarness, cycle: usize) {
    let agent_count = harness.agent_dirs.len();
    let barrier = Arc::new(Barrier::new(agent_count));

    let contents: Vec<Vec<u8>> = (0..agent_count)
        .map(|agent| {
            format!(
                "pub fn cycle_{cycle}_agent_{agent}() -> &'static str {{\n    \"cycle {cycle} agent {agent}\"\n}}\n"
            )
            .into_bytes()
        })
        .collect();
    let filenames: Vec<String> = (0..agent_count)
        .map(|agent| format!("src/cycle_{cycle}_agent_{agent}.rs"))
        .collect();
    let descriptions: Vec<String> = (0..agent_count)
        .map(|agent| format!("cycle {cycle} agent {agent}"))
        .collect();

    let handles: Vec<_> = (0..agent_count)
        .map(|agent| {
            let dir = harness.agent_dirs[agent].clone();
            let home = harness.home.clone();
            let bar = barrier.clone();
            let filename = filenames[agent].clone();
            let content = contents[agent].clone();
            let desc = descriptions[agent].clone();

            thread::spawn(move || {
                std::fs::create_dir_all(dir.join("src")).expect("create src");
                std::fs::write(dir.join(&filename), &content).expect("write cycle file");

                bar.wait();

                let (describe, describe_retries) =
                    run_tandem_resilient(&dir, &["describe", "-m", &desc], &home);
                common::assert_ok(&describe, &format!("describe {desc}"));
                let (new, new_retries) = run_tandem_resilient(&dir, &["new"], &home);
                common::assert_ok(&new, &format!("new after {desc}"));
                let combined_stderr = format!(
                    "{}{}",
                    common::stderr_str(&describe),
                    common::stderr_str(&new)
                );
                assert!(
                    !combined_stderr.contains("CAS retry limit exceeded"),
                    "command path should not hit CAS retry limit during {desc}:\n{combined_stderr}"
                );

                std::cmp::max(describe_retries, new_retries)
            })
        })
        .collect();

    let mut max_retries_seen = 0usize;
    for handle in handles {
        let retries = handle.join().expect("worker thread");
        max_retries_seen = max_retries_seen.max(retries);
    }

    assert!(
        max_retries_seen <= COMMAND_MAX_RETRIES,
        "workspace-state retries exceeded configured bound: {max_retries_seen} > {COMMAND_MAX_RETRIES}"
    );

    for dir in &harness.agent_dirs {
        settle_workspace(dir, &harness.home);
    }

    let (log, _) = run_tandem_resilient(
        &harness.agent_dirs[0],
        &["log", "--no-graph", "-r", "all()"],
        &harness.home,
    );
    common::assert_ok(&log, "log after contention cycle");
    let log_text = common::stdout_str(&log);
    for desc in &descriptions {
        assert!(
            log_text.contains(desc),
            "cycle log missing description '{desc}'\n{log_text}"
        );
    }

    let commit_ids: Vec<String> = descriptions
        .iter()
        .map(|desc| find_commit_id_by_description(&harness.agent_dirs[0], desc, &harness.home))
        .collect();

    for agent_dir in &harness.agent_dirs {
        for idx in 0..agent_count {
            let (show, _) = run_tandem_resilient(
                agent_dir,
                &["file", "show", "-r", &commit_ids[idx], &filenames[idx]],
                &harness.home,
            );
            common::assert_ok(&show, &format!("file show {}", filenames[idx]));
            assert_eq!(
                show.stdout, contents[idx],
                "byte mismatch for {}",
                filenames[idx]
            );
        }
    }
}

#[test]
fn slice21_log_stream_contains_contention_observability_fields() {
    let root = TempDir::new().expect("tempdir");
    let home = common::isolated_home(root.path());
    let server_repo = root.path().join("server-repo");
    std::fs::create_dir_all(&server_repo).expect("create server repo");

    let ws1 = root.path().join("ws-a");
    let ws2 = root.path().join("ws-b");
    std::fs::create_dir_all(&ws1).expect("create ws1");
    std::fs::create_dir_all(&ws2).expect("create ws2");

    let addr = common::free_addr();
    let socket = common::control_socket_path(root.path());

    let mut server = spawn_observability_server(&server_repo, &addr, &socket, &home);
    common::wait_for_server(&addr, &mut server);
    common::wait_for_socket(&socket, Duration::from_secs(5));

    let logs_path = root.path().join("server-logs.jsonl");
    let mut logs_child = start_log_stream(&home, &socket, &logs_path);
    thread::sleep(Duration::from_millis(400));

    let init_a = common::run_tandem_in(
        &ws1,
        &["init", "--server", &addr, "--workspace", "obs-a", "."],
        &home,
    );
    common::assert_ok(&init_a, "init ws-a");
    let init_b = common::run_tandem_in(
        &ws2,
        &["init", "--server", &addr, "--workspace", "obs-b", "."],
        &home,
    );
    common::assert_ok(&init_b, "init ws-b");

    std::fs::create_dir_all(ws1.join("src")).expect("create ws1/src");
    std::fs::create_dir_all(ws2.join("src")).expect("create ws2/src");

    for cycle in 0..5 {
        std::fs::write(
            ws1.join(format!("src/obs_a_{cycle}.txt")),
            format!("obs-a cycle {cycle}\n"),
        )
        .expect("write ws1 payload");
        std::fs::write(
            ws2.join(format!("src/obs_b_{cycle}.txt")),
            format!("obs-b cycle {cycle}\n"),
        )
        .expect("write ws2 payload");

        let ws1_clone = ws1.clone();
        let ws2_clone = ws2.clone();
        let home_clone_a = home.clone();
        let home_clone_b = home.clone();

        let handle_a = thread::spawn(move || {
            let message = format!("obs cycle {cycle} a");
            let args = ["new", "-m", message.as_str()];
            let (out, _) = run_tandem_resilient(&ws1_clone, &args, &home_clone_a);
            common::assert_ok(&out, "obs cycle ws-a new");
        });
        let handle_b = thread::spawn(move || {
            let message = format!("obs cycle {cycle} b");
            let args = ["new", "-m", message.as_str()];
            let (out, _) = run_tandem_resilient(&ws2_clone, &args, &home_clone_b);
            common::assert_ok(&out, "obs cycle ws-b new");
        });

        handle_a.join().expect("ws-a thread");
        handle_b.join().expect("ws-b thread");
    }

    thread::sleep(Duration::from_millis(900));
    let _ = logs_child.kill();
    let _ = logs_child.wait();

    #[cfg(unix)]
    unsafe {
        libc::kill(server.id() as libc::pid_t, libc::SIGINT);
    }
    let _ = server.wait();

    let stdout = std::fs::read_to_string(&logs_path).expect("read logs output");
    let mut update_responses = Vec::new();

    for line in stdout.lines().filter(|line| !line.trim().is_empty()) {
        let Ok(entry) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let is_update_response = entry
            .get("msg")
            .and_then(|v| v.as_str())
            .map(|msg| msg == "rpc response")
            .unwrap_or(false)
            && log_field_str(&entry, "rpc_method") == Some("updateOpHeads");
        if is_update_response {
            update_responses.push(entry);
        }
    }

    assert!(
        !update_responses.is_empty(),
        "expected updateOpHeads rpc response logs\nstdout:\n{stdout}"
    );

    let saw_ok_true = update_responses
        .iter()
        .any(|entry| log_field_str(entry, "ok") == Some("true"));
    let saw_ok_false = update_responses
        .iter()
        .any(|entry| log_field_str(entry, "ok") == Some("false"));
    assert!(
        saw_ok_true,
        "expected at least one successful updateOpHeads log"
    );
    assert!(
        saw_ok_false,
        "expected at least one contention-failed updateOpHeads log"
    );

    for entry in &update_responses {
        for field in [
            "rpc_method",
            "attempt",
            "cas_retries",
            "latency_ms",
            "queue_depth",
        ] {
            assert!(
                log_field_str(entry, field).is_some(),
                "missing field '{field}' in log entry: {entry}"
            );
        }
    }
}

#[test]
fn slice21_repeated_two_agent_contention_cycles_converge() {
    let harness = TestHarness::new(2);
    for cycle in 0..5 {
        run_contention_cycle(&harness, cycle);
    }
}

#[test]
fn slice21_repeated_five_agent_contention_cycles_converge() {
    let harness = TestHarness::new(5);
    for cycle in 0..3 {
        run_contention_cycle(&harness, cycle);
    }
}
