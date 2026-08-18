//! The server repo is a cache; the bucket is the repo.
//!
//! What is pinned here:
//! - `tandem up --bucket` on an empty directory replays the WAL and serves
//! - Destroying the server directory and re-upping yields identical op heads
//!   and byte-identical file content
//! - `jj git push` works from the materialized repo
//! - Replay is incremental on warm boot: only the entries past the local state
//!
//! Tier 1 runs against the filesystem backend. Tier 2 runs the same shape
//! against SeaweedFS; set `TANDEM_TEST_S3_BUCKET` to opt in, e.g.
//! `s3://tandem-test?endpoint=http://127.0.0.1:8333&anonymous=true`. The bucket
//! must exist first (`curl -X PUT http://127.0.0.1:8333/tandem-test`); against a
//! missing bucket the server dies at boot instead of reporting the cause.

use crate::common;
use crate::common::workspace::first_commit_id;

use std::path::{Path, PathBuf};

use tempfile::TempDir;

use common::bucket_harness::{read_dir_names, using_s3, BucketHarness as Harness};

// ─── Harness ──────────────────────────────────────────────────────────────────
//
// The shared parts live in `tests/common/bucket_harness.rs`. What follows is
// only what these tests need on top of them.

impl Harness {
    /// Throw the server's disk away. The bucket is untouched: everything the
    /// next start serves has to come back from there.
    fn destroy_repo(&mut self) {
        self.stop_server();
        std::fs::remove_dir_all(&self.repo).expect("remove the server repo directory");
        std::fs::create_dir_all(&self.repo).expect("recreate an empty server repo directory");
    }

    /// Run a command against the server's own repo directory, as an operator
    /// would: the server's working copy is never checked out, so every such
    /// command ignores it.
    fn run_on_server(&self, args: &[&str]) -> std::process::Output {
        common::run_tandem_in_with_env(&self.repo, args, &[], &self.home)
    }

    fn stored_operations(&self) -> Vec<String> {
        let mut ops = read_dir_names(&self.repo.join(".jj/repo/op_store/operations"));
        ops.sort();
        ops
    }

    /// The bytes of a file at a revision, read out of the server's own repo.
    fn server_file_bytes(&self, commit_id: &str, path: &str) -> Vec<u8> {
        let out = self.run_on_server(&[
            "file",
            "show",
            "--ignore-working-copy",
            "-r",
            commit_id,
            path,
        ]);
        common::assert_ok(&out, &format!("server file show {path} at {commit_id}"));
        out.stdout
    }

    /// The commit id of a revision, as the client sees it.
    fn client_commit_id(&self, ws: &Path, rev: &str) -> String {
        let out = self.run(
            ws,
            &["log", "--no-graph", "-r", rev, "-T", "commit_id ++ \"\\n\""],
        );
        first_commit_id(&out, &format!("client log for {rev}"))
    }
}

/// A history worth replaying: two workspaces, several generations each, so the
/// op DAG has a merge in it and the file content spans more than one publish.
struct Published {
    a: PathBuf,
    b: PathBuf,
    files: Vec<(String, String, Vec<u8>)>, // (commit id, path, bytes)
}

fn publish_a_history(harness: &Harness, ws_a: &Path, ws_b: &Path) -> Published {
    let mut files = Vec::new();

    for round in 0..3 {
        for (ws, agent) in [(ws_a, "agent-a"), (ws_b, "agent-b")] {
            let path = format!("{agent}/round-{round}.txt");
            let content = format!("{agent} wrote round {round}\n").into_bytes();
            std::fs::create_dir_all(ws.join(agent)).expect("create agent dir");
            std::fs::write(ws.join(&path), &content).expect("write file");
            common::assert_ok(
                &common::run_tandem_in(
                    ws,
                    &["describe", "-m", &format!("{agent} round {round}")],
                    &harness.home,
                ),
                "describe",
            );
            common::assert_ok(
                &common::run_tandem_in(ws, &["new"], &harness.home),
                "new change",
            );
            let commit = harness.client_commit_id(ws, "@-");
            files.push((commit, path, content));
        }
    }

    Published {
        a: ws_a.to_path_buf(),
        b: ws_b.to_path_buf(),
        files,
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

/// The server disk is disposable. Throw it away, start on an empty directory
/// with the same bucket, and the repo comes back: same op heads, same version,
/// same operations, same file bytes.
#[test]
fn destroying_the_server_repo_and_reupping_replays_the_history() {
    let mut harness = Harness::new("full-replay");
    harness.start_server();
    let ws_a = harness.init_workspace("agent-a");
    let ws_b = harness.init_workspace("agent-b");
    let published = publish_a_history(&harness, &ws_a, &ws_b);

    let heads_before = harness.op_head_files();
    let version_before = harness.local_version();
    let operations_before = harness.stored_operations();
    let bytes_before: Vec<Vec<u8>> = published
        .files
        .iter()
        .map(|(commit, path, _)| harness.server_file_bytes(commit, path))
        .collect();
    assert!(
        operations_before.len() > published.files.len(),
        "the setup should have left a multi-generation history: {} operations",
        operations_before.len()
    );

    // ── Destroy and re-up ─────────────────────────────────────────────
    harness.destroy_repo();
    assert!(
        !harness.repo.join(".jj").exists(),
        "the server directory must be empty before the replay"
    );
    harness.start_server();

    assert_eq!(
        harness.local_version(),
        version_before,
        "the materialized repo must come back at the bucket's version"
    );
    assert_eq!(
        harness.op_head_files(),
        heads_before,
        "the materialized repo must have exactly the op heads the bucket names"
    );

    let operations_after = harness.stored_operations();
    for op in &operations_before {
        assert!(
            operations_after.contains(op),
            "operation {op} was not replayed; the materialized op log has a gap"
        );
    }

    for ((commit, path, expected), before) in published.files.iter().zip(&bytes_before) {
        let after = harness.server_file_bytes(commit, path);
        assert_eq!(
            &after, expected,
            "replayed {path} at {commit} does not match what was written"
        );
        assert_eq!(
            &after, before,
            "replayed {path} at {commit} is not byte-identical to the original repo"
        );
    }

    // The op log itself has to be walkable, not just the files: a gap in the
    // ancestry only shows up once something asks for the whole DAG.
    let op_log = harness.run_on_server(&[
        "op",
        "log",
        "--ignore-working-copy",
        "--no-graph",
        "-T",
        "description",
    ]);
    common::assert_ok(&op_log, "op log on the materialized repo");

    let log = harness.run_on_server(&[
        "log",
        "--ignore-working-copy",
        "--no-graph",
        "-r",
        "all()",
        "-T",
        "description",
    ]);
    common::assert_ok(&log, "log on the materialized repo");
    let text = common::stdout_str(&log);
    for round in 0..3 {
        for agent in ["agent-a", "agent-b"] {
            let expected = format!("{agent} round {round}");
            assert!(
                text.contains(&expected),
                "the materialized repo is missing {expected:?}:\n{text}"
            );
        }
    }

    // And the server still serves: a client publishes onto the replayed state.
    let _ = harness.run(&published.a, &["workspace", "update-stale"]);
    std::fs::write(
        published.a.join("after-replay.txt"),
        "written after replay\n",
    )
    .expect("write file");
    common::assert_ok(
        &harness.run(&published.a, &["describe", "-m", "after the replay"]),
        "publish onto the materialized repo",
    );
    assert!(
        harness.local_version() > version_before,
        "publishing must resume after the replay"
    );
    let _ = published.b;
}

/// A materialized repo is a real colocated git repo: bookmark it and push it.
#[test]
fn git_push_works_from_a_materialized_repo() {
    let mut harness = Harness::new("replay-push");
    harness.start_server();
    let ws = harness.init_workspace("agent-a");

    let content = b"pub fn feature() -> &'static str {\n    \"materialized\"\n}\n";
    std::fs::create_dir_all(ws.join("src")).expect("create src dir");
    std::fs::write(ws.join("src/feature.rs"), content).expect("write file");
    common::assert_ok(
        &harness.run(&ws, &["describe", "-m", "add feature module"]),
        "describe",
    );
    common::assert_ok(&harness.run(&ws, &["new"]), "new change");
    let commit_id = harness.client_commit_id(&ws, "@-");

    harness.destroy_repo();
    harness.start_server();

    assert!(
        harness.repo.join(".git").exists(),
        "the materialized repo must be colocated with git"
    );
    assert_eq!(
        harness.server_file_bytes(&commit_id, "src/feature.rs"),
        content,
        "the materialized repo must hold the file bytes"
    );

    // ── Push the materialized repo to a bare remote ───────────────────
    let bare_remote = harness.tmp.path().join("bare-remote.git");
    common::assert_ok(
        &common::run_git_in(
            harness.tmp.path(),
            &["init", "--bare", bare_remote.to_str().unwrap()],
        ),
        "git init --bare",
    );
    common::assert_ok(
        &common::run_git_in(
            &harness.repo,
            &["remote", "add", "origin", bare_remote.to_str().unwrap()],
        ),
        "git remote add",
    );
    common::assert_ok(
        &harness.run_on_server(&[
            "bookmark",
            "create",
            "--ignore-working-copy",
            "main",
            "-r",
            &commit_id,
        ]),
        "jj bookmark create on the materialized repo",
    );
    common::assert_ok(
        &harness.run_on_server(&["git", "push", "--ignore-working-copy", "--bookmark", "main"]),
        "jj git push from the materialized repo",
    );

    let clone_dir = harness.tmp.path().join("clone");
    common::assert_ok(
        &common::run_git_in(
            harness.tmp.path(),
            &[
                "clone",
                bare_remote.to_str().unwrap(),
                clone_dir.to_str().unwrap(),
            ],
        ),
        "git clone",
    );
    let cloned = std::fs::read(clone_dir.join("src/feature.rs")).expect("cloned feature.rs");
    assert_eq!(
        cloned, content,
        "the pushed content must be byte-identical to what the agent wrote"
    );
}

/// `tandem up --bucket` on an empty directory: the daemon materializes the
/// repo before it serves, and says so.
#[test]
fn up_on_an_empty_directory_materializes_and_serves() {
    if using_s3() {
        eprintln!("skipping: this test keeps its bucket on disk (filesystem backend only)");
        return;
    }

    let tmp = TempDir::new().expect("temp dir");
    let home = common::isolated_home(tmp.path());
    let repo = tmp.path().join("server-repo");
    let bucket = tmp.path().join("bucket");
    let bucket_spec = bucket.to_string_lossy().to_string();
    let sock = common::control_socket_path(tmp.path());
    let sock_str = sock.to_str().unwrap();
    let addr = common::free_addr();
    // Fixed across both lives of the daemon: the workspace keeps the token it
    // was given, and a restart has to keep accepting it.
    let admin_token = jj_tandem::auth::generate_admin_token();

    let up = |repo: &Path| {
        common::run_tandem_in(
            tmp.path(),
            &[
                "up",
                "--repo",
                repo.to_str().unwrap(),
                "--listen",
                &addr,
                "--control-socket",
                sock_str,
                "--log-file",
                tmp.path().join("daemon.log").to_str().unwrap(),
                "--bucket",
                &bucket_spec,
                "--admin-token",
                &admin_token,
            ],
            &home,
        )
    };
    let status = || {
        let out = common::run_tandem_in(
            tmp.path(),
            &["server", "status", "--json", "--control-socket", sock_str],
            &home,
        );
        common::assert_ok(&out, "tandem server status --json");
        let text = common::stdout_str(&out);
        serde_json::from_str::<serde_json::Value>(text.trim())
            .unwrap_or_else(|e| panic!("invalid status JSON: {e}\nraw: {text}"))
    };
    let down = || {
        common::assert_ok(
            &common::run_tandem_in(tmp.path(), &["down", "--control-socket", sock_str], &home),
            "tandem down",
        );
        std::thread::sleep(std::time::Duration::from_millis(500));
    };

    // ── First life: an empty directory and an empty bucket ────────────
    std::fs::create_dir_all(&repo).expect("create repo dir");
    common::assert_ok(&up(&repo), "tandem up on an empty directory");
    common::wait_for_addr(&addr, std::time::Duration::from_secs(10));

    let first = status();
    assert_eq!(first["bucket"]["location"], bucket_spec);
    assert_eq!(
        first["bucket"]["replayedEntries"], 0,
        "an empty bucket has nothing to replay: {first}"
    );

    let ws = tmp.path().join("agent-a");
    std::fs::create_dir_all(&ws).expect("create workspace dir");
    common::assert_ok(
        &common::run_tandem_in(
            &ws,
            &[
                "init",
                "--server",
                &addr,
                "--token",
                &admin_token,
                "--workspace",
                "agent-a",
                ".",
            ],
            &home,
        ),
        "tandem init",
    );
    std::fs::write(ws.join("note.txt"), "materialize me\n").expect("write file");
    common::assert_ok(
        &common::run_tandem_in(&ws, &["describe", "-m", "up with a bucket"], &home),
        "describe",
    );
    down();

    // ── Second life: the same bucket, no directory at all ─────────────
    std::fs::remove_dir_all(&repo).expect("remove the server repo directory");
    common::assert_ok(&up(&repo), "tandem up on a destroyed directory");
    common::wait_for_addr(&addr, std::time::Duration::from_secs(10));

    let second = status();
    assert_eq!(
        second["bucket"]["materialized"], true,
        "the second start should report that it built the repo from the bucket: {second}"
    );
    assert!(
        second["bucket"]["replayedEntries"].as_u64().unwrap_or(0) > 0,
        "the second start should have replayed WAL entries: {second}"
    );
    assert!(
        second["bucket"]["replayedHeads"].as_u64().unwrap_or(0) > 0,
        "the second start should have replayed op heads: {second}"
    );

    // And it serves: the workspace reads the replayed history back.
    let _ = common::run_tandem_in(&ws, &["workspace", "update-stale"], &home);
    let log = common::run_tandem_in(
        &ws,
        &["log", "--no-graph", "-r", "all()", "-T", "description"],
        &home,
    );
    common::assert_ok(&log, "log against the materialized server");
    assert!(
        common::stdout_str(&log).contains("up with a bucket"),
        "the materialized server should serve the published history:\n{}",
        common::stdout_str(&log)
    );

    down();
}

/// How many WAL entries a boot pulled out of the bucket.
fn replayed_entries(log: &Path) -> usize {
    let text = std::fs::read_to_string(log).expect("read server log");
    text.matches("replaying a WAL entry").count()
}

/// Replay is a gap-filler, not a rebuild. A warm boot — the same disk, nothing
/// published since — must not fetch a single WAL entry, or every restart pays
/// one bucket round trip per operation in history, inside the boot path.
#[test]
fn replay_is_incremental_on_a_warm_boot() {
    if using_s3() {
        eprintln!("skipping: this test counts bucket reads in the log (filesystem backend only)");
        return;
    }

    let mut harness = Harness::new("warm-boot");
    harness.start_server();
    let ws = harness.init_workspace("agent-a");
    for n in 0..6 {
        common::assert_ok(
            &harness.run(&ws, &["describe", "-m", &format!("operation {n}")]),
            "build history",
        );
    }
    let history = harness.stored_operations().len();
    assert!(
        history >= 6,
        "the setup should have left a history to replay, got {history} operations"
    );

    // Warm: the disk still holds everything the bucket names.
    harness.stop_server();
    let warm_log = harness.tmp.path().join("warm-boot.log");
    harness.start_server_logging(Some(&warm_log));
    assert_eq!(
        replayed_entries(&warm_log),
        0,
        "a warm boot replayed WAL entries it already had on disk (history is {history} operations)"
    );

    // Cold: the same bucket, an empty disk. This is the comparison that makes
    // the warm count mean something.
    harness.destroy_repo();
    let cold_log = harness.tmp.path().join("cold-boot.log");
    harness.start_server_logging(Some(&cold_log));
    let cold = replayed_entries(&cold_log);
    assert!(
        cold >= history,
        "a cold boot replayed {cold} WAL entries but the history is {history} operations; \
         the walk must reach every ancestor"
    );

    // One more warm boot, now on the materialized disk: still nothing to fetch.
    harness.stop_server();
    let second_warm_log = harness.tmp.path().join("warm-boot-2.log");
    harness.start_server_logging(Some(&second_warm_log));
    assert_eq!(
        replayed_entries(&second_warm_log),
        0,
        "a warm boot after a replay refetched WAL entries the replay had already applied"
    );
}
