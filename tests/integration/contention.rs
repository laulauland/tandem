//! Real concurrency: several `tandem` processes writing at the same time.
//!
//! The simulation covers interleaving that a seed can reproduce. It cannot
//! cover what an operating system does with five processes contending for the
//! same head — that is genuinely non-deterministic, and it is exactly the case
//! where the retry loop either stays bounded or does not. So this test stays a
//! subprocess test, and it is the only one of its kind left.
//!
//! It also checks that the server says what it is doing while it happens: the
//! contention fields in the log are how an operator sees a retry storm.

use crate::common;
use crate::common::lines::start_log_stream;
use crate::common::workspace::{find_commit_id, run_tandem_resilient, settle_workspace, MAX_RETRIES};
use crate::common::ServerFixture;

use std::path::PathBuf;
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::Duration;

fn log_field_str<'a>(entry: &'a serde_json::Value, key: &str) -> Option<&'a str> {
    entry.get("fields")?.get(key)?.as_str()
}

/// Whether a streamed log line is an `updateOpHeads` response with this outcome.
fn is_update_response(line: &str, ok: Option<&str>) -> bool {
    let Ok(entry) = serde_json::from_str::<serde_json::Value>(line) else {
        return false;
    };
    let is_response = entry.get("msg").and_then(|v| v.as_str()) == Some("rpc response")
        && log_field_str(&entry, "rpc_method") == Some("updateOpHeads");
    match ok {
        Some(expected) => is_response && log_field_str(&entry, "ok") == Some(expected),
        None => is_response,
    }
}

/// A server and the agent workspaces contending over it.
struct TestHarness {
    server: ServerFixture,
    agent_dirs: Vec<PathBuf>,
}

/// So that `harness.home` still reads as the harness's own, now that the server
/// and the temp root it owns live in the shared fixture.
impl std::ops::Deref for TestHarness {
    type Target = ServerFixture;

    fn deref(&self) -> &Self::Target {
        &self.server
    }
}

impl TestHarness {
    fn new(agent_count: usize) -> Self {
        let server = ServerFixture::builder()
            .args(&["--log-level", "error"])
            .start();
        let agent_dirs = (0..agent_count)
            .map(|i| {
                let name = format!("agent-{i}");
                server.init_workspace(&name, Some(&name))
            })
            .collect();
        Self {
            server,
            agent_dirs,
        }
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
        max_retries_seen <= MAX_RETRIES,
        "workspace-state retries exceeded configured bound: {max_retries_seen} > {MAX_RETRIES}"
    );

    // Settling is one subprocess per agent and they do not depend on each
    // other, so do not pay for them one after another.
    let settles: Vec<_> = harness
        .agent_dirs
        .iter()
        .map(|dir| {
            let dir = dir.clone();
            let home = harness.home.clone();
            thread::spawn(move || settle_workspace(&dir, &home))
        })
        .collect();
    for handle in settles {
        handle.join().expect("settle thread");
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
        .map(|desc| find_commit_id(&harness.agent_dirs[0], desc, &harness.home))
        .collect();

    // Every agent must see every agent's bytes. That is `agent_count` squared
    // subprocesses, and the agents do not share anything the reads could
    // disturb, so read them in parallel.
    let readers: Vec<_> = harness
        .agent_dirs
        .iter()
        .map(|agent_dir| {
            let agent_dir = agent_dir.clone();
            let home = harness.home.clone();
            let commit_ids = commit_ids.clone();
            let filenames = filenames.clone();
            let contents = contents.clone();
            thread::spawn(move || {
                for idx in 0..commit_ids.len() {
                    let (show, _) = run_tandem_resilient(
                        &agent_dir,
                        &["file", "show", "-r", &commit_ids[idx], &filenames[idx]],
                        &home,
                    );
                    common::assert_ok(&show, &format!("file show {}", filenames[idx]));
                    assert_eq!(
                        show.stdout, contents[idx],
                        "byte mismatch for {} as seen from {}",
                        filenames[idx],
                        agent_dir.display()
                    );
                }
            })
        })
        .collect();
    for handle in readers {
        handle.join().expect("reader thread");
    }
}

#[test]
fn log_stream_contains_contention_observability_fields() {
    let mut fx = ServerFixture::builder()
        .control_socket()
        .args(&["--log-level", "debug"])
        .start();
    let home = fx.home.clone();

    let (mut logs_child, mut logs) = start_log_stream(&fx.socket, "debug", &home);

    // The first workspace init is both setup and a probe: it makes the server
    // log, so a line arriving proves the subscription is open. That replaces a
    // fixed sleep that was either too short to be safe or too long to be free.
    let ws1 = fx.init_workspace("ws-a", Some("obs-a"));
    assert!(
        logs.wait_for(Duration::from_secs(10), |line| !line.trim().is_empty())
            .is_some(),
        "the log stream produced nothing while the server served an init"
    );

    let ws2 = fx.init_workspace("ws-b", Some("obs-b"));

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

    // A contention-failed response is the last thing to arrive, and it is the
    // one this test exists for. Wait for it by deadline rather than sleeping a
    // second and hoping the buffer had flushed.
    let contended = logs.wait_for(Duration::from_secs(10), |line| {
        is_update_response(line, Some("false"))
    });

    let _ = logs_child.kill();
    let _ = logs_child.wait();

    #[cfg(unix)]
    unsafe {
        libc::kill(fx.server.id() as libc::pid_t, libc::SIGINT);
    }
    let _ = fx.server.wait();

    let transcript = logs.transcript();
    assert!(
        contended.is_some(),
        "expected at least one contention-failed updateOpHeads log\nstdout:\n{transcript}"
    );

    let update_responses: Vec<serde_json::Value> = logs
        .seen()
        .iter()
        .filter(|line| is_update_response(line, None))
        .map(|line| serde_json::from_str(line).expect("already parsed once"))
        .collect();

    assert!(
        !update_responses.is_empty(),
        "expected updateOpHeads rpc response logs\nstdout:\n{transcript}"
    );
    assert!(
        update_responses
            .iter()
            .any(|entry| log_field_str(entry, "ok") == Some("true")),
        "expected at least one successful updateOpHeads log\nstdout:\n{transcript}"
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
fn repeated_five_agent_contention_cycles_converge() {
    let harness = TestHarness::new(5);
    for cycle in 0..3 {
        run_contention_cycle(&harness, cycle);
    }
}
