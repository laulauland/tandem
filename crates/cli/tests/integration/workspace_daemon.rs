//! `tandem daemon`: a file changes, and an operation is published.
//!
//! Everything here needs a real process watching a real directory. The
//! filesystem event is the input under test, and there is no such thing as a
//! simulated inotify — an in-process test would have to call the snapshot
//! itself, which is the one thing the daemon exists to do without being asked.
//!
//! Nothing below sleeps for a fixed period waiting for work to happen. Every
//! wait is a deadline read on the daemon's own stdout, so a test costs what the
//! daemon costs. The one exception is the idle test, which has to let time
//! pass: proving that nothing was published means giving it a chance to be.

use crate::clone::{clone_workspace, tree_of};
use crate::common;
use crate::common::field;
use crate::common::lines::Lines;
use crate::common::ServerFixture;

use notify::Watcher as _;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

/// How long any one line may take to arrive. Generous on purpose: what these
/// tests guard against is "never", not "slowly". The latency question is the
/// bench's (`benches/snapshot_publish_latency.rs`), not a test's.
const LINE_TIMEOUT: Duration = Duration::from_secs(20);

/// Short enough that no test waits on it. The one test that is *about* the
/// window asks for a longer one of its own.
const DEBOUNCE_MS: &str = "150";

/// A running `tandem daemon`, and the two pipes it talks through.
///
/// `pub` for `baked_image.rs`, which starts one the same way a container's
/// entrypoint does. A second copy of the start-and-wait-for-`watching` dance
/// would be a second thing to get wrong about the race it exists to close.
pub struct DaemonProcess {
    child: Child,
    out: Lines,
    err: Lines,
    root: PathBuf,
    home: PathBuf,
}

impl DaemonProcess {
    /// Start a daemon on a workspace and wait until it says it is watching.
    ///
    /// Waiting for that line matters: a write that lands before the watcher is
    /// registered produces no event, and a test that raced it would fail for a
    /// reason that has nothing to do with what it is checking.
    pub fn start(root: &Path, home: &Path) -> Self {
        Self::start_with_debounce(root, home, DEBOUNCE_MS)
    }

    pub fn start_with_debounce(root: &Path, home: &Path, debounce_ms: &str) -> Self {
        Self::start_with_options(root, home, debounce_ms, "30")
    }

    fn start_with_options(root: &Path, home: &Path, debounce_ms: &str, writer_ttl: &str) -> Self {
        let mut cmd = Command::new(common::tandem_bin());
        cmd.current_dir(root)
            .args([
                "daemon",
                ".",
                "--debounce-ms",
                debounce_ms,
                "--writer-ttl-seconds",
                writer_ttl,
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        common::isolate_env(&mut cmd, home);
        let mut child = cmd.spawn().expect("spawn tandem daemon");

        let mut daemon = Self {
            out: Lines::from(child.stdout.take().expect("daemon stdout")),
            err: Lines::from(child.stderr.take().expect("daemon stderr")),
            child,
            root: root.to_path_buf(),
            home: home.to_path_buf(),
        };
        let watching = daemon
            .err
            .wait_for(LINE_TIMEOUT, |line| line.starts_with("watching "));
        assert!(
            watching.is_some(),
            "the daemon never said it was watching\nstderr:\n{}",
            daemon.err.transcript()
        );
        daemon
    }

    /// Wait for the daemon to report a published operation, and answer its id.
    pub fn wait_for_publish(&mut self) -> String {
        let line = self
            .out
            .wait_for(LINE_TIMEOUT, |line| line.starts_with("published op="))
            .unwrap_or_else(|| {
                panic!(
                    "the daemon published nothing\nstdout:\n{}\nstderr:\n{}",
                    self.out.transcript(),
                    self.err.transcript()
                )
            });
        field(&line, "op=")
    }

    /// The status the daemon keeps on disk, as `tandem daemon --status --json`
    /// reads it.
    fn status(&self) -> serde_json::Value {
        let out = common::run_tandem_in(
            &self.root,
            &["daemon", ".", "--status", "--json"],
            &self.home,
        );
        common::assert_ok(&out, "read the daemon's status");
        serde_json::from_str(&common::stdout_str(&out)).expect("the status is JSON")
    }

    pub fn stop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn renewal_runs_during_a_debounce_window_longer_than_the_writer_ttl() {
    let fx = ServerFixture::start();
    let (root, _) = clone_workspace(&fx, "renewal", "agent-a");
    let mut daemon = DaemonProcess::start_with_options(&root, &fx.home, "3000", "1");
    std::fs::write(
        root.join("over-ttl.txt"),
        b"renew while events are pending\n",
    )
    .unwrap();
    daemon.wait_for_publish();
    let before_status = daemon.status();
    let before_window = before_status["updatedAtUnixMs"].as_u64().unwrap();
    let published_before = before_status["publishedOps"].as_u64().unwrap();
    let (status_tx, status_rx) = std::sync::mpsc::channel();
    let mut status_watcher = notify::recommended_watcher(move |event| {
        let _ = status_tx.send(event);
    })
    .unwrap();
    status_watcher
        .watch(&root, notify::RecursiveMode::Recursive)
        .unwrap();
    std::fs::write(
        root.join("over-ttl.txt"),
        b"renew again while the debounce window is active\n",
    )
    .unwrap();

    let deadline = std::time::Instant::now() + LINE_TIMEOUT;
    loop {
        status_rx
            .recv_timeout(deadline.saturating_duration_since(std::time::Instant::now()))
            .expect("the daemon did not persist a renewal during debounce")
            .expect("watch daemon status updates");
        let renewed_at = daemon.status()["updatedAtUnixMs"].as_u64().unwrap();
        if renewed_at >= before_window + 1_100 {
            break;
        }
    }

    let response = common::http_client()
        .post(format!("http://{}/api/workspaces/agent-a/writer", fx.addr))
        .bearer_auth(fx.token())
        .json(&serde_json::json!({"holder": "competing-daemon", "ttlSeconds": 1}))
        .send()
        .unwrap();
    assert_eq!(
        daemon.status()["publishedOps"].as_u64(),
        Some(published_before),
        "the competing claim must happen while the second publish is pending"
    );
    assert_eq!(
        response.status().as_u16(),
        409,
        "the role expired during sustained debounce"
    );
    daemon.wait_for_publish();
}

#[test]
fn snapshot_revalidates_an_expired_writer_role_before_publishing() {
    let fx = ServerFixture::start();
    let (root, _) = clone_workspace(&fx, "expired-role", "agent-a");
    let settings =
        jj_lib::settings::UserSettings::from_config(jj_lib::config::StackedConfig::with_defaults())
            .unwrap();
    let mut options = jj_tandem_workspace::DaemonOptions::new(&root);
    options.writer_ttl = Duration::ZERO;
    let mut daemon = jj_tandem_workspace::Daemon::open(&settings, &options).unwrap();

    let response = common::http_client()
        .post(format!("http://{}/api/workspaces/agent-a/writer", fx.addr))
        .bearer_auth(fx.token())
        .json(&serde_json::json!({"holder": "replacement-daemon", "ttlSeconds": 30}))
        .send()
        .unwrap();
    assert_eq!(response.status().as_u16(), 200);
    let version_before = server_heads(&fx).0;
    std::fs::write(
        root.join("must-not-publish.txt"),
        b"replacement owns the role\n",
    )
    .unwrap();
    assert!(matches!(
        daemon.snapshot_once().unwrap(),
        jj_tandem_workspace::SnapshotOutcome::NotTheWriter { .. }
    ));
    assert_eq!(server_heads(&fx).0, version_before);
}

#[test]
fn a_daemon_renews_while_its_snapshot_is_blocked_in_the_wal() {
    let temporary = tempfile::tempdir().unwrap();
    let home = common::isolated_home(temporary.path());
    let faults = jj_tandem_repository::FaultPoints::inert();
    let bucket = temporary.path().join("bucket");
    let server = jj_tandem_server::Server::new_with_faults_for_test(
        temporary.path().join("server-cache"),
        Some(bucket.to_str().unwrap()),
        "renewal-secret",
        faults.clone(),
    )
    .unwrap();
    server.durably_initialize().unwrap();
    let (claims_tx, claims_rx) = std::sync::mpsc::channel();
    let reject_renewals = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let reject_in_middleware = reject_renewals.clone();
    let gate_heads = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let gate_heads_in_middleware = gate_heads.clone();
    let (heads_entered_tx, heads_entered_rx) = std::sync::mpsc::channel();
    let heads_release = std::sync::Arc::new(tokio::sync::Notify::new());
    let heads_release_in_middleware = heads_release.clone();
    let app =
        jj_tandem_server::router(std::sync::Arc::new(server)).layer(axum::middleware::from_fn(
            move |request: axum::extract::Request, next: axum::middleware::Next| {
                let claims_tx = claims_tx.clone();
                let reject_in_middleware = reject_in_middleware.clone();
                let gate_heads = gate_heads_in_middleware.clone();
                let heads_entered_tx = heads_entered_tx.clone();
                let heads_release = heads_release_in_middleware.clone();
                async move {
                    if request.method() == axum::http::Method::GET
                        && request.uri().path() == "/api/heads"
                        && gate_heads.swap(false, std::sync::atomic::Ordering::AcqRel)
                    {
                        let released = heads_release.notified();
                        let _ = heads_entered_tx.send(());
                        released.await;
                    }
                    if request.uri().path().ends_with("/writer") {
                        if reject_in_middleware
                            .fetch_update(
                                std::sync::atomic::Ordering::AcqRel,
                                std::sync::atomic::Ordering::Acquire,
                                |remaining| remaining.checked_sub(1),
                            )
                            .is_ok()
                        {
                            let _ = claims_tx.send(());
                            return axum::response::IntoResponse::into_response(
                                axum::http::StatusCode::SERVICE_UNAVAILABLE,
                            );
                        }
                        let response = next.run(request).await;
                        let _ = claims_tx.send(());
                        return response;
                    }
                    next.run(request).await
                }
            },
        ));
    let address = common::free_addr();
    let listener_address = address.clone();
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let server_thread = std::thread::spawn(move || {
        tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(async move {
                let listener = tokio::net::TcpListener::bind(&listener_address)
                    .await
                    .unwrap();
                ready_tx.send(()).unwrap();
                axum::serve(listener, app)
                    .with_graceful_shutdown(async {
                        let _ = shutdown_rx.await;
                    })
                    .await
                    .unwrap();
            });
    });
    struct StopServer(
        Option<tokio::sync::oneshot::Sender<()>>,
        Option<std::thread::JoinHandle<()>>,
    );
    impl Drop for StopServer {
        fn drop(&mut self) {
            if let Some(sender) = self.0.take() {
                let _ = sender.send(());
            }
            if let Some(thread) = self.1.take() {
                let _ = thread.join();
            }
        }
    }
    let _server = StopServer(Some(shutdown_tx), Some(server_thread));
    ready_rx.recv_timeout(LINE_TIMEOUT).unwrap();

    let root = temporary.path().join("workspace");
    std::fs::create_dir(&root).unwrap();
    let init = common::run_tandem_in(
        &root,
        &[
            "init",
            "--server",
            &address,
            "--token",
            "renewal-secret",
            "--workspace",
            "agent-a",
            ".",
        ],
        &home,
    );
    common::assert_ok(&init, "initialize renewal workspace");
    while claims_rx.try_recv().is_ok() {}
    let settings =
        jj_lib::settings::UserSettings::from_config(jj_lib::config::StackedConfig::with_defaults())
            .unwrap();
    let mut options = jj_tandem_workspace::DaemonOptions::new(&root);
    options.writer_ttl = Duration::from_secs(1);
    let mut daemon = jj_tandem_workspace::Daemon::open(&settings, &options).unwrap();
    std::fs::write(root.join("blocked.txt"), b"durable after the gate\n").unwrap();
    faults.hold_next_wal_write();
    struct ReleaseWal(std::sync::Arc<jj_tandem_repository::FaultPoints>);
    impl Drop for ReleaseWal {
        fn drop(&mut self) {
            self.0.release_wal_write();
        }
    }
    let release = ReleaseWal(faults.clone());
    let snapshot = std::thread::spawn(move || {
        let result = daemon.snapshot_once();
        (daemon, result)
    });
    faults.wait_for_held_wal_write();
    while claims_rx.try_recv().is_ok() {}

    // Four observed renewal requests take more than the original one-second
    // lease at ttl/3, without a timing sleep in the fixture.
    for _ in 0..4 {
        claims_rx
            .recv_timeout(LINE_TIMEOUT)
            .expect("the blocked daemon stopped renewing its writer role");
    }
    let response = common::http_client()
        .post(format!("http://{address}/api/workspaces/agent-a/writer"))
        .bearer_auth("renewal-secret")
        .json(&serde_json::json!({"holder": "competing-daemon", "ttlSeconds": 1}))
        .send()
        .unwrap();
    assert_eq!(response.status().as_u16(), 409);
    drop(release);
    let (mut daemon, outcome) = snapshot.join().unwrap();
    assert!(matches!(
        outcome.unwrap(),
        jj_tandem_workspace::SnapshotOutcome::Published(_)
    ));

    std::fs::write(root.join("blocked.txt"), b"must remain unacknowledged\n").unwrap();
    let version_before = common::api_get(&address, "renewal-secret", "/api/heads")
        .json::<serde_json::Value>()
        .unwrap()["version"]
        .as_u64()
        .unwrap();
    while claims_rx.try_recv().is_ok() {}
    gate_heads.store(true, std::sync::atomic::Ordering::Release);
    struct ReleaseHeads(std::sync::Arc<tokio::sync::Notify>);
    impl Drop for ReleaseHeads {
        fn drop(&mut self) {
            self.0.notify_waiters();
        }
    }
    let release_heads = ReleaseHeads(heads_release);
    let second = std::thread::spawn(move || {
        let outcome = daemon.snapshot_once();
        (daemon, outcome)
    });
    heads_entered_rx
        .recv_timeout(LINE_TIMEOUT)
        .expect("snapshot never reached the head read after its early role check");
    while claims_rx.try_recv().is_ok() {}
    reject_renewals.store(1, std::sync::atomic::Ordering::Release);
    claims_rx
        .recv_timeout(LINE_TIMEOUT)
        .expect("the daemon did not attempt renewal while its preparation head read was blocked");
    claims_rx
        .recv_timeout(LINE_TIMEOUT)
        .expect("the daemon did not retry renewal after a transient failure");
    drop(release_heads);
    let (mut daemon, outcome) = second.join().unwrap();
    assert!(matches!(
        outcome.unwrap(),
        jj_tandem_workspace::SnapshotOutcome::NotTheWriter { .. }
    ));
    let version_after = common::api_get(&address, "renewal-secret", "/api/heads")
        .json::<serde_json::Value>()
        .unwrap()["version"]
        .as_u64()
        .unwrap();
    assert_eq!(version_after, version_before, "a known-lost role published");
    let jj_tandem_workspace::SnapshotOutcome::Published(published) =
        daemon.snapshot_once().unwrap()
    else {
        panic!("same daemon must retry the same bytes")
    };
    let read = common::run_tandem_in_with_env(
        &root,
        &[
            "file",
            "show",
            "--ignore-working-copy",
            "-r",
            &published.commit_id,
            "blocked.txt",
        ],
        &[("TANDEM_DISABLE_CACHE", "1")],
        &home,
    );
    common::assert_ok(&read, "read successful retry after lease loss");
    assert_eq!(read.stdout, b"must remain unacknowledged\n");
    for (index_failure, bytes) in [
        (false, b"retry WAL failure\0".as_slice()),
        (true, b"retry index failure\xff".as_slice()),
    ] {
        std::fs::write(root.join("blocked.txt"), bytes).unwrap();
        if index_failure {
            faults.fail_index_writes(1);
        } else {
            faults.fail_wal_writes(1);
        }
        assert!(
            daemon.snapshot_once().is_err(),
            "failed storage must not acknowledge"
        );
        let jj_tandem_workspace::SnapshotOutcome::Published(published) =
            daemon.snapshot_once().unwrap()
        else {
            panic!("same daemon must retry failed storage")
        };
        let read = common::run_tandem_in_with_env(
            &root,
            &[
                "file",
                "show",
                "--ignore-working-copy",
                "-r",
                &published.commit_id,
                "blocked.txt",
            ],
            &[("TANDEM_DISABLE_CACHE", "1")],
            &home,
        );
        common::assert_ok(&read, "read retry after failed durable write");
        assert_eq!(read.stdout, bytes);
    }
}

impl Drop for DaemonProcess {
    fn drop(&mut self) {
        self.stop();
    }
}

/// What the server currently serves: the head version, and each workspace's
/// last published operation.
fn server_heads(fx: &ServerFixture) -> (u64, BTreeMap<String, String>) {
    let body: serde_json::Value = common::api_get(&fx.addr, fx.token(), "/api/heads")
        .json()
        .expect("the heads endpoint answers JSON");
    let version = body["version"].as_u64().expect("a head version");
    let workspaces = body
        .get("workspaceHeads")
        .and_then(|value| value.as_object())
        .map(|map| {
            map.iter()
                .map(|(name, id)| (name.clone(), id.as_str().unwrap_or_default().to_string()))
                .collect()
        })
        .unwrap_or_default();
    (version, workspaces)
}

/// Every operation description in a workspace's op log, newest first.
fn op_descriptions(root: &Path, home: &Path) -> Vec<String> {
    let out = common::run_tandem_in(
        root,
        &[
            "op",
            "log",
            "--no-graph",
            "--ignore-working-copy",
            "-n",
            "40",
            "-T",
            "description ++ \"\\n\"",
        ],
        home,
    );
    common::assert_ok(&out, "read the op log");
    common::stdout_str(&out)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect()
}

// ─── The criteria ─────────────────────────────────────────────────────────────

#[test]
fn a_file_change_is_published_without_any_jj_command_being_run() {
    let fx = ServerFixture::start();
    let (root, _) = clone_workspace(&fx, "workspace", "agent-a");
    let mut daemon = DaemonProcess::start(&root, &fx.home);

    let (_, before) = server_heads(&fx);

    // The whole of the input. No subprocess, no verb, no checkpoint — a
    // program that knows nothing about tandem writing a file.
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(
        root.join("src/main.rs"),
        b"fn main() { println!(\"hi\"); }\n",
    )
    .unwrap();

    let op_id = daemon.wait_for_publish();
    assert!(!op_id.is_empty(), "the published line names an operation");

    let (_, after) = server_heads(&fx);
    assert_eq!(
        after.get("agent-a").map(String::as_str),
        Some(op_id.as_str()),
        "the server serves the operation the daemon says it published; before: {before:?}"
    );

    // And the bytes really went: read them back through the server, from a
    // workspace that never saw the file on disk.
    let (reader, _) = clone_workspace(&fx, "reader", "agent-b");
    let shown = common::run_tandem_in(
        &reader,
        &["file", "show", "-r", "agent-a@", "src/main.rs"],
        &fx.home,
    );
    common::assert_ok(&shown, "read the daemon's file from another workspace");
    assert_eq!(
        shown.stdout, b"fn main() { println!(\"hi\"); }\n",
        "the published operation carries the bytes that were written"
    );
}

/// How many files the debounce test writes, and how far apart.
///
/// Spread out on purpose. Twenty writes in a tight loop coalesce whether there
/// is a debounce window or not, because one snapshot reads whatever is on disk
/// by the time it runs — a test written that way passes with the window set to
/// zero, and proves nothing. Writes spaced further apart than a snapshot takes
/// are separate events, and then only the window can join them.
const SPREAD_WRITES: usize = 24;
const SPREAD_GAP: Duration = Duration::from_millis(50);
const SPREAD_DEBOUNCE_MS: &str = "600";

/// The most operations those writes may become.
///
/// A literal and not a formula over the constants above. The bound has to stay
/// still while the window moves, or shrinking the window would shrink the
/// assertion with it and the test would pass at any setting — which is exactly
/// what it did before this comment was here. 1.2 seconds of writing through a
/// 600 ms window is two windows and a tail; six is that with room for a busy
/// machine, and far below the twenty-four an undebounced daemon would publish.
const SPREAD_MAX_OPERATIONS: u64 = 6;

#[test]
fn writes_spread_across_a_debounce_window_are_one_operation() {
    let fx = ServerFixture::start();
    let (root, _) = clone_workspace(&fx, "workspace", "agent-a");
    let mut daemon = DaemonProcess::start_with_debounce(&root, &fx.home, SPREAD_DEBOUNCE_MS);

    // Roughly 1.2 seconds of steady writing, which is what an agent editing a
    // directory looks like. Two windows' worth, give or take.
    for i in 0..SPREAD_WRITES {
        std::fs::write(root.join(format!("file-{i:02}.txt")), format!("{i}\n")).unwrap();
        std::thread::sleep(SPREAD_GAP);
    }

    daemon.wait_for_publish();

    // Let one more full window close, so a daemon still catching up has room
    // to finish before it is counted.
    std::thread::sleep(Duration::from_millis(1_500));

    let published = daemon.status()["publishedOps"]
        .as_u64()
        .expect("a publish count");
    assert!(
        published <= SPREAD_MAX_OPERATIONS,
        "{SPREAD_WRITES} writes {}ms apart through a {SPREAD_DEBOUNCE_MS}ms window must not be \
         {SPREAD_WRITES} operations, got {published}",
        SPREAD_GAP.as_millis()
    );
    assert!(
        published >= 1,
        "the writes have to have been published at all"
    );

    // And every file is in the repo, not just the ones a window happened to
    // end on: a snapshot carries the whole tree, not a diff of the burst.
    let descriptions = op_descriptions(&root, &fx.home);
    assert!(
        descriptions
            .iter()
            .any(|d| d == "tandem daemon: snapshot working copy"),
        "the op log has the daemon's snapshot in it: {descriptions:?}"
    );
    for i in 0..SPREAD_WRITES {
        let name = format!("file-{i:02}.txt");
        let shown = common::run_tandem_in(&root, &["file", "show", "-r", "@", &name], &fx.home);
        common::assert_ok(&shown, &format!("read {name} back out of the repo"));
    }
}

#[test]
fn an_idle_workspace_publishes_nothing() {
    let fx = ServerFixture::start();
    let (root, _) = clone_workspace(&fx, "workspace", "agent-a");
    let daemon = DaemonProcess::start(&root, &fx.home);

    let (version_before, heads_before) = server_heads(&fx);

    // Several debounce windows, and several writer-role renewals inside the
    // shortest tick the daemon uses. Nothing touches the tree. A daemon on a
    // timer, or one that snapshotted whenever it woke up, would publish here.
    std::thread::sleep(Duration::from_millis(2_500));

    let (version_after, heads_after) = server_heads(&fx);
    assert_eq!(
        version_after, version_before,
        "an untouched workspace moves no heads"
    );
    assert_eq!(
        heads_after, heads_before,
        "an untouched workspace publishes no operation"
    );
    assert_eq!(
        daemon.status()["publishedOps"].as_u64(),
        Some(0),
        "the daemon agrees it published nothing"
    );
}

#[test]
fn killing_the_client_and_cloning_the_name_elsewhere_reproduces_the_files() {
    let fx = ServerFixture::start();
    let (root, origin) = clone_workspace(&fx, "first", "agent-a");
    assert_eq!(origin, "created");

    let mut daemon = DaemonProcess::start(&root, &fx.home);

    // Edit. No `jj` command anywhere in this test between here and the kill.
    std::fs::create_dir_all(root.join("deep/nested")).unwrap();
    std::fs::write(
        root.join("deep/nested/data.bin"),
        [0u8, 1, 2, 250, 251, 255],
    )
    .unwrap();
    std::fs::write(root.join("notes.md"), "# notes\n\nsomething\n").unwrap();
    daemon.wait_for_publish();

    // A second round, so that what is reproduced is the *last* snapshot and
    // not merely the first one.
    std::fs::write(root.join("notes.md"), "# notes\n\nsomething else\n").unwrap();
    daemon.wait_for_publish();

    let expected = tree_of(&root);
    assert_eq!(expected.len(), 2, "two files were written: {expected:?}");

    // The machine dies.
    daemon.stop();

    // Somewhere else, under the same name.
    let (second, origin) = clone_workspace(&fx, "second", "agent-a");
    assert_eq!(origin, "attached", "the name is attached to, not recreated");
    assert_eq!(
        tree_of(&second),
        expected,
        "a re-clone reproduces the last published snapshot byte for byte"
    );
}

#[test]
fn a_head_published_elsewhere_marks_the_workspace_stale_and_nothing_else() {
    let fx = ServerFixture::start();
    let (root, _) = clone_workspace(&fx, "watcher", "agent-a");
    let (other, _) = clone_workspace(&fx, "other", "agent-b");

    let mut daemon = DaemonProcess::start(&root, &fx.home);

    // Something of its own first, so that "stale" cannot be an artefact of a
    // daemon that has never published.
    std::fs::write(root.join("mine.txt"), b"mine\n").unwrap();
    daemon.wait_for_publish();
    let before = tree_of(&root);
    let published_before = daemon.status()["publishedOps"].as_u64().unwrap();

    // Another workspace publishes. The daemon hears it over SSE.
    std::fs::write(other.join("theirs.txt"), b"theirs\n").unwrap();
    common::assert_ok(
        &common::run_tandem_in(&other, &["describe", "-m", "agent-b publishes"], &fx.home),
        "publish from the other workspace",
    );

    let line = daemon
        .out
        .wait_for(LINE_TIMEOUT, |line| line.starts_with("stale=true"))
        .unwrap_or_else(|| {
            panic!(
                "the daemon never reported the workspace stale\nstdout:\n{}\nstderr:\n{}",
                daemon.out.transcript(),
                daemon.err.transcript()
            )
        });
    assert!(
        line.contains("update-stale"),
        "a stale report tells a person what they may do about it: {line}"
    );

    // ── And nothing else happened ─────────────────────────────────────
    assert_eq!(
        tree_of(&root),
        before,
        "marking a workspace stale must not move a single file under it"
    );
    assert!(
        !root.join("theirs.txt").exists(),
        "the other workspace's file must not appear here: update-stale was not run"
    );
    assert_eq!(
        daemon.status()["publishedOps"].as_u64(),
        Some(published_before),
        "a head change is a wake-up, not a reason to publish"
    );
    assert_eq!(
        daemon.status()["stale"].as_bool(),
        Some(true),
        "the daemon records the staleness it reported"
    );

    // No operation in this workspace's log recovers or updates a working copy.
    // `workspace update-stale` writes one; nothing here may have.
    for description in op_descriptions(&root, &fx.home) {
        let lowered = description.to_lowercase();
        assert!(
            !lowered.contains("update-stale") && !lowered.contains("recover"),
            "an operation that resolves staleness must never appear: {description:?}"
        );
    }
}

#[test]
fn a_second_daemon_on_one_workspace_is_refused_the_writer_role() {
    let fx = ServerFixture::start();
    let (first, _) = clone_workspace(&fx, "first", "agent-a");
    let mut leader = DaemonProcess::start(&first, &fx.home);

    // Prove the first one has the role by using it.
    std::fs::write(first.join("leader.txt"), b"leader\n").unwrap();
    leader.wait_for_publish();
    assert_eq!(leader.status()["writer"].as_bool(), Some(true));

    // A second machine attaches to the same name and starts its own daemon.
    // One workspace has one writer; this one loses.
    let (second, origin) = clone_workspace(&fx, "second", "agent-a");
    assert_eq!(origin, "attached");
    let mut follower = DaemonProcess::start(&second, &fx.home);

    let refused = follower
        .out
        .wait_for(LINE_TIMEOUT, |line| line.starts_with("writer=refused"))
        .unwrap_or_else(|| {
            panic!(
                "the second daemon was never refused the writer role\nstdout:\n{}\nstderr:\n{}",
                follower.out.transcript(),
                follower.err.transcript()
            )
        });
    assert!(
        refused.contains("detail="),
        "a refusal says who holds the role: {refused}"
    );

    // It stays up and stays quiet: a daemon that exited would need somebody to
    // restart it at exactly the moment the other one died.
    std::fs::write(second.join("follower.txt"), b"follower\n").unwrap();
    std::thread::sleep(Duration::from_millis(1_000));
    let status = follower.status();
    assert_eq!(
        status["writer"].as_bool(),
        Some(false),
        "the second daemon knows it is not the writer"
    );
    assert_eq!(
        status["publishedOps"].as_u64(),
        Some(0),
        "a daemon without the writer role publishes nothing"
    );
    assert!(
        status["writerDetail"].as_str().is_some(),
        "and it says why: {status}"
    );

    // The leader still holds it after a renewal or two.
    assert_eq!(
        leader.status()["writer"].as_bool(),
        Some(true),
        "the holder keeps the role by renewing it"
    );
}

#[test]
fn every_daemon_snapshot_carries_the_same_operation_description() {
    let fx = ServerFixture::start();
    let (root, _) = clone_workspace(&fx, "workspace", "agent-a");
    let mut daemon = DaemonProcess::start(&root, &fx.home);

    // Three changes that have nothing in common: a new file, an edit, a
    // delete. If a description could vary with what changed, it would here.
    std::fs::write(root.join("a.txt"), b"first\n").unwrap();
    daemon.wait_for_publish();
    std::fs::write(root.join("a.txt"), b"second\n").unwrap();
    daemon.wait_for_publish();
    std::fs::remove_file(root.join("a.txt")).unwrap();
    daemon.wait_for_publish();

    let descriptions = op_descriptions(&root, &fx.home);
    let snapshots: Vec<&String> = descriptions
        .iter()
        .filter(|d| d.contains("tandem daemon"))
        .collect();
    assert_eq!(
        snapshots.len(),
        3,
        "three snapshots, three operations: {descriptions:?}"
    );
    for description in &snapshots {
        assert_eq!(
            description.as_str(),
            "tandem daemon: snapshot working copy",
            "every daemon operation carries the same mechanical description"
        );
    }

    // The semantic label is a different field, and it is a person's to set.
    // `tandem describe` is stock jj and needs no tandem code — what matters is
    // that it does not disturb the operation descriptions above.
    common::assert_ok(
        &common::run_tandem_in(&root, &["describe", "-m", "what this is about"], &fx.home),
        "describe the change",
    );
    let shown = common::run_tandem_in(
        &root,
        &["log", "--no-graph", "-r", "@", "-T", "description"],
        &fx.home,
    );
    common::assert_ok(&shown, "read the change description");
    assert!(
        common::stdout_str(&shown).contains("what this is about"),
        "the semantic label lives in the change description"
    );
    for description in op_descriptions(&root, &fx.home) {
        if description.contains("tandem daemon") {
            assert_eq!(description, "tandem daemon: snapshot working copy");
        }
        assert!(
            !description.contains("what this is about"),
            "a change description must not leak into an operation description: {description:?}"
        );
    }
}

#[test]
fn the_status_of_a_killed_daemon_says_it_is_not_running() {
    let fx = ServerFixture::start();
    let (root, _) = clone_workspace(&fx, "workspace", "agent-a");
    let mut daemon = DaemonProcess::start(&root, &fx.home);

    // Publish something, so the status on disk is full of claims worth being
    // wrong about: the writer role held, an operation published, not stale.
    std::fs::write(root.join("mine.txt"), b"mine\n").unwrap();
    daemon.wait_for_publish();
    let alive = daemon.status();
    assert_eq!(alive["writer"].as_bool(), Some(true));
    assert_eq!(alive["running"].as_bool(), Some(true), "status: {alive}");

    // The machine dies. The status file is exactly as it was.
    daemon.stop();
    assert!(
        root.join(".jj/tandem-daemon.json").exists(),
        "a killed daemon leaves its status behind — that is the whole problem"
    );

    let out = common::run_tandem_in(&root, &["daemon", ".", "--status", "--json"], &fx.home);
    let status: serde_json::Value =
        serde_json::from_str(&common::stdout_str(&out)).expect("the status is JSON");
    assert_eq!(
        status["running"].as_bool(),
        Some(false),
        "the status of a daemon that is gone says so: {status}"
    );
    assert!(
        !out.status.success(),
        "and asking about a daemon that is not running is not a success"
    );

    // The same in the words a person reads.
    let human = common::run_tandem_in(&root, &["daemon", ".", "--status"], &fx.home);
    let text = common::stdout_str(&human);
    assert!(
        text.contains("not running"),
        "the human status says the daemon is gone:\n{text}"
    );
    assert!(
        !text.contains("Writer:    held"),
        "and it does not report a dead daemon's claims as current:\n{text}"
    );
}

#[test]
fn a_daemon_that_starts_after_the_edit_still_publishes_it() {
    let fx = ServerFixture::start();
    let (root, _) = clone_workspace(&fx, "workspace", "agent-a");

    // Written while nothing was watching — the machine was down, or the daemon
    // had crashed. There is no second file change coming to trigger a window,
    // so a daemon that only ever reacted to events would hold this forever.
    std::fs::write(root.join("written-while-down.txt"), b"still mine\n").unwrap();

    let mut daemon = DaemonProcess::start(&root, &fx.home);
    daemon.wait_for_publish();
    daemon.stop();

    let (second, origin) = clone_workspace(&fx, "second", "agent-a");
    assert_eq!(origin, "attached");
    assert_eq!(
        std::fs::read(second.join("written-while-down.txt")).expect("the file was published"),
        b"still mine\n"
    );
}

#[test]
fn normal_snapshot_publishes_one_prepared_graph() {
    let fx = ServerFixture::builder().log_to_file().start();
    let (root, _) = clone_workspace(&fx, "combined-snapshot", "agent-a");
    let settings =
        jj_lib::settings::UserSettings::from_config(jj_lib::config::StackedConfig::with_defaults())
            .unwrap();
    let mut daemon = jj_tandem_workspace::Daemon::open(
        &settings,
        &jj_tandem_workspace::DaemonOptions::new(&root),
    )
    .unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink("dir/edited.txt", root.join("link")).unwrap();
    let before = fx.rpc_request_count("publishPrepared");
    let writes = fx.rpc_request_count("putObject")
        + fx.rpc_request_count("putObjectsBatch")
        + fx.rpc_request_count("putOperationWithView");
    std::fs::create_dir(root.join("dir")).unwrap();
    std::fs::write(
        root.join("dir/edited.txt"),
        b"ordinary snapshot exact\0\xff",
    )
    .unwrap();
    let jj_tandem_workspace::SnapshotOutcome::Published(published) =
        daemon.snapshot_once().unwrap()
    else {
        panic!("edited snapshot must publish")
    };
    assert_eq!(fx.rpc_request_count("publishPrepared") - before, 1);
    assert_eq!(
        fx.rpc_request_count("putObject")
            + fx.rpc_request_count("putObjectsBatch")
            + fx.rpc_request_count("putOperationWithView"),
        writes
    );
    let fresh = fx.init_workspace("fresh-reader", Some("reader"));
    let read = common::run_tandem_in_with_env(
        &fresh,
        &[
            "file",
            "show",
            "--ignore-working-copy",
            "-r",
            &published.commit_id,
            "dir/edited.txt",
        ],
        &[("TANDEM_DISABLE_CACHE", "1")],
        &fx.home,
    );
    common::assert_ok(&read, "read normal snapshot from a fresh client");
    assert_eq!(read.stdout, b"ordinary snapshot exact\0\xff");
    #[cfg(unix)]
    {
        let (attached, _) = clone_workspace(&fx, "fresh-attached", "agent-a");
        assert_eq!(
            std::fs::read_link(attached.join("link")).unwrap(),
            std::path::Path::new("dir/edited.txt")
        );
        assert_eq!(
            std::fs::read(attached.join("link")).unwrap(),
            b"ordinary snapshot exact\0\xff"
        );
    }
}

#[test]
fn stacked_snapshot_rewrites_preserve_descendant_edits_and_leave_files_owned_by_b() {
    let fx = ServerFixture::builder().log_to_file().start();
    let a = fx.init_workspace("stack-a", Some("agent-a"));
    let settings =
        jj_lib::settings::UserSettings::from_config(jj_lib::config::StackedConfig::with_defaults())
            .unwrap();
    let mut daemon_a =
        jj_tandem_workspace::Daemon::open(&settings, &jj_tandem_workspace::DaemonOptions::new(&a))
            .unwrap();
    std::fs::write(a.join("a.txt"), b"A first\n").unwrap();
    let jj_tandem_workspace::SnapshotOutcome::Published(first) = daemon_a.snapshot_once().unwrap()
    else {
        panic!("A must publish")
    };
    let b = fx.init_workspace("stack-b", Some("agent-b"));
    common::assert_ok(
        &common::run_tandem_in(&b, &["new", &first.commit_id], &fx.home),
        "stack B on A",
    );
    std::fs::write(b.join("b.txt"), b"B independent\0\xff").unwrap();
    let mut daemon_b =
        jj_tandem_workspace::Daemon::open(&settings, &jj_tandem_workspace::DaemonOptions::new(&b))
            .unwrap();
    assert!(matches!(
        daemon_b.snapshot_once().unwrap(),
        jj_tandem_workspace::SnapshotOutcome::Published(_)
    ));
    std::fs::write(a.join("a.txt"), b"A rewritten\n").unwrap();
    assert!(matches!(
        daemon_a.snapshot_once().unwrap(),
        jj_tandem_workspace::SnapshotOutcome::Published(_)
    ));
    assert_eq!(std::fs::read(b.join("a.txt")).unwrap(), b"A first\n");
    assert_eq!(
        std::fs::read(b.join("b.txt")).unwrap(),
        b"B independent\0\xff"
    );
    assert!(matches!(
        daemon_b.snapshot_once().unwrap(),
        jj_tandem_workspace::SnapshotOutcome::Stale
    ));
    common::assert_ok(
        &common::run_tandem_in(&b, &["workspace", "update-stale"], &fx.home),
        "B explicitly updates its stale directory",
    );
    assert_eq!(std::fs::read(b.join("a.txt")).unwrap(), b"A rewritten\n");
    assert_eq!(
        std::fs::read(b.join("b.txt")).unwrap(),
        b"B independent\0\xff"
    );
    // An explicit jj rebase remains an ordinary jj command. The next daemon
    // snapshot must use the resulting stacked history without losing B's bytes.
    common::assert_ok(
        &common::run_tandem_in(&b, &["rebase", "-r", "@", "-d", "root()"], &fx.home),
        "B rebases independently",
    );
    std::fs::write(b.join("b.txt"), b"B after rebase\0\xff").unwrap();
    let jj_tandem_workspace::SnapshotOutcome::Published(last) = daemon_b.snapshot_once().unwrap()
    else {
        panic!("B must publish after rebase")
    };
    let fresh = fx.init_workspace("stack-reader", Some("reader"));
    let read = common::run_tandem_in_with_env(
        &fresh,
        &[
            "file",
            "show",
            "--ignore-working-copy",
            "-r",
            &last.commit_id,
            "b.txt",
        ],
        &[("TANDEM_DISABLE_CACHE", "1")],
        &fx.home,
    );
    common::assert_ok(&read, "read B after stacked rewrites and rebase");
    assert_eq!(read.stdout, b"B after rebase\0\xff");
}

#[test]
fn a_prepared_snapshot_preserves_an_existing_jj_tree_conflict() {
    let fx = ServerFixture::start();
    let root = fx.init_workspace("conflicted-snapshot", Some("agent-a"));
    std::fs::write(root.join("conflict.txt"), b"base\n").unwrap();
    common::assert_ok(&fx.run(&root, &["status"]), "snapshot base");
    let base = fx.run(&root, &["log", "--no-graph", "-r", "@", "-T", "commit_id"]);
    common::assert_ok(&base, "read base identity");
    let base = String::from_utf8(base.stdout).unwrap();
    common::assert_ok(&fx.run(&root, &["new", &base]), "create left side");
    std::fs::write(root.join("conflict.txt"), b"left\n").unwrap();
    common::assert_ok(&fx.run(&root, &["status"]), "snapshot left side");
    let left = fx.run(&root, &["log", "--no-graph", "-r", "@", "-T", "commit_id"]);
    common::assert_ok(&left, "read left identity");
    let left = String::from_utf8(left.stdout).unwrap();
    common::assert_ok(&fx.run(&root, &["new", &base]), "create right side");
    std::fs::write(root.join("conflict.txt"), b"right\n").unwrap();
    common::assert_ok(&fx.run(&root, &["status"]), "snapshot right side");
    common::assert_ok(
        &fx.run(&root, &["new", &left, "@"]),
        "create conflicted merge",
    );
    let conflict = fx.run(
        &root,
        &["file", "show", "--ignore-working-copy", "conflict.txt"],
    );
    common::assert_ok(&conflict, "read original conflict");
    assert!(conflict.stdout.windows(5).any(|s| s == b"left\n"));
    assert!(conflict.stdout.windows(6).any(|s| s == b"right\n"));
    let settings =
        jj_lib::settings::UserSettings::from_config(jj_lib::config::StackedConfig::with_defaults())
            .unwrap();
    let mut daemon = jj_tandem_workspace::Daemon::open(
        &settings,
        &jj_tandem_workspace::DaemonOptions::new(&root),
    )
    .unwrap();
    std::fs::write(root.join("independent.txt"), b"unrelated edit\0\xff").unwrap();
    let jj_tandem_workspace::SnapshotOutcome::Published(published) =
        daemon.snapshot_once().unwrap()
    else {
        panic!("snapshot with conflict must publish")
    };
    for (file, expected) in [
        ("conflict.txt", conflict.stdout.as_slice()),
        ("independent.txt", b"unrelated edit\0\xff".as_slice()),
    ] {
        let read = common::run_tandem_in_with_env(
            &root,
            &[
                "file",
                "show",
                "--ignore-working-copy",
                "-r",
                &published.commit_id,
                file,
            ],
            &[("TANDEM_DISABLE_CACHE", "1")],
            &fx.home,
        );
        common::assert_ok(&read, "read prepared conflicted tree");
        assert_eq!(read.stdout, expected);
    }
}
