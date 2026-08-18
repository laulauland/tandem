//! Slice 22: the bucket is the durable source of truth.
//!
//! Acceptance criteria (stage 1 of the target architecture):
//! - Publishing an op writes one WAL entry and then CAS-updates the index
//!   object; the head update acks only after both are durable
//! - An index CAS conflict surfaces as a retryable failure and jj's
//!   transaction retry converges
//! - A server killed after the index write but before the local apply brings
//!   the local repo back to the bucket's state on restart
//!
//! Tier 1 runs against the filesystem backend. Tier 2 runs the same shape
//! against SeaweedFS; set `TANDEM_TEST_S3_BUCKET` to opt in, e.g.
//! `s3://tandem-test?endpoint=http://127.0.0.1:8333&anonymous=true`.

mod common;

use std::collections::BTreeMap;
use std::path::Path;
use std::thread;
use std::time::Duration;

use common::bucket_harness::{using_s3, BucketHarness as Harness};

// ─── Harness ──────────────────────────────────────────────────────────────────
//
// The shared parts live in `tests/common/bucket_harness.rs`. What follows is
// only what this slice needs on top of them.

impl Harness {
    /// Wait for a server that is expected to kill itself.
    fn wait_for_server_exit(&mut self) {
        let mut child = self.server.take().expect("server running");
        for _ in 0..200 {
            match child.try_wait().expect("poll server") {
                Some(_) => return,
                None => thread::sleep(Duration::from_millis(50)),
            }
        }
        let _ = child.kill();
        let _ = child.wait();
        panic!("server did not exit after the injected crash");
    }

    fn operation_is_stored(&self, op_hex: &str) -> bool {
        self.repo
            .join(".jj/repo/op_store/operations")
            .join(op_hex)
            .exists()
    }

    /// Every WAL entry in a filesystem-backend bucket, as raw bytes.
    fn wal_entries(&self) -> Vec<(String, Vec<u8>)> {
        let dir = self.bucket_dir.join("wal");
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return Vec::new();
        };
        entries
            .map(|entry| {
                let entry = entry.expect("WAL directory entry");
                let name = entry.file_name().to_string_lossy().to_string();
                let bytes = std::fs::read(entry.path()).expect("read WAL entry");
                (name, bytes)
            })
            .collect()
    }
}

// ─── Reading the bucket ───────────────────────────────────────────────────────

#[derive(Debug)]
struct Index {
    version: u64,
    op_heads: Vec<String>,
    workspace_heads: BTreeMap<String, String>,
}

/// Filesystem-backend buckets are readable straight off disk. The S3 tests
/// assert on server-observable state instead, so this is only used in tier 1.
fn read_fs_index(bucket_dir: &Path) -> Index {
    let bytes = std::fs::read(bucket_dir.join("index/heads.json")).expect("read bucket index");
    let value: serde_json::Value = serde_json::from_slice(&bytes).expect("parse bucket index");
    Index {
        version: value["version"].as_u64().expect("index version"),
        op_heads: value["opHeads"]
            .as_array()
            .expect("index opHeads")
            .iter()
            .map(|v| v.as_str().expect("op head hex").to_string())
            .collect(),
        workspace_heads: value["workspaceHeads"]
            .as_object()
            .expect("index workspaceHeads")
            .iter()
            .map(|(k, v)| (k.clone(), v.as_str().expect("hex").to_string()))
            .collect(),
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

/// A publish leaves one WAL entry per op head and one index object naming them.
#[test]
fn slice22_publish_writes_a_wal_entry_and_the_index_object() {
    if using_s3() {
        eprintln!("skipping: this test reads the bucket off disk (filesystem backend only)");
        return;
    }

    let mut harness = Harness::new("wal-shape");
    harness.start_server(&[]);
    let ws = harness.init_workspace("agent-a");

    std::fs::write(ws.join("greeting.txt"), "hello from the WAL\n").expect("write file");
    common::assert_ok(
        &harness.run(&ws, &["describe", "-m", "wal shape"]),
        "describe",
    );

    let index = read_fs_index(&harness.bucket_dir);
    assert!(index.version > 0, "index version should have advanced");
    assert!(!index.op_heads.is_empty(), "index should name op heads");
    assert!(
        index.workspace_heads.contains_key("agent-a"),
        "index should track the workspace head: {index:?}"
    );

    for head in &index.op_heads {
        let entry = harness.bucket_dir.join("wal").join(head);
        assert!(
            entry.exists(),
            "every op head named by the index needs a WAL entry: {head}"
        );
        let bytes = std::fs::read(&entry).expect("read WAL entry");
        assert!(
            bytes.starts_with(b"TDMWAL"),
            "WAL entry {head} is not in the WAL framing"
        );
    }

    // The index tracks the local repo, and the local repo tracks the index.
    assert_eq!(harness.local_version(), index.version);
    let mut heads = index.op_heads.clone();
    heads.sort();
    assert_eq!(harness.op_head_files(), heads);
}

/// An index CAS conflict must look like an ordinary version mismatch, so the
/// client's existing retry loop converges without changes.
#[test]
fn slice22_index_cas_conflict_is_retryable() {
    let mut harness = Harness::new("cas-conflict");
    harness.start_server(&[]);
    let ws = harness.init_workspace("agent-a");
    common::assert_ok(&harness.run(&ws, &["describe", "-m", "before"]), "describe");

    // Restart with the next few index writes rigged to fail their CAS.
    harness.stop_server();
    harness.start_server(&[("TANDEM_TEST_INDEX_CAS_CONFLICTS", "3")]);

    let out = harness.run(&ws, &["describe", "-m", "through contention"]);
    common::assert_ok(&out, "describe through injected CAS conflicts");

    let log = harness.run(&ws, &["log", "--no-graph", "-T", "description"]);
    common::assert_ok(&log, "log");
    assert!(
        common::stdout_str(&log).contains("through contention"),
        "the retried operation should have landed:\n{}",
        common::stdout_str(&log)
    );
}

/// An object written while a publish is between two attempts must survive.
///
/// The retried publish already has a WAL entry in the bucket, and that entry is
/// immutable. If the retry drains the staging buffer again, everything a second
/// client wrote in the meantime goes into an entry the bucket refuses to
/// replace: the objects are then durable nowhere, while the head that reaches
/// them gets acknowledged. `TANDEM_TEST_STAGE_OBJECT_ON_INDEX_CONFLICT` puts
/// exactly one such object into the window.
#[test]
fn slice22_objects_written_between_retries_stay_durable() {
    if using_s3() {
        eprintln!("skipping: this test reads the bucket off disk (filesystem backend only)");
        return;
    }

    const MARKER: &str = "tandem-object-written-between-index-cas-retries\n";

    let mut harness = Harness::new("retry-staging");
    harness.start_server(&[]);
    let ws = harness.init_workspace("agent-a");
    common::assert_ok(&harness.run(&ws, &["describe", "-m", "before"]), "describe");

    // Arm the hooks only now, so the conflict lands on an ordinary publish
    // rather than on the synthetic root operation, which has no WAL entry.
    harness.stop_server();
    harness.start_server(&[
        ("TANDEM_TEST_INDEX_CAS_CONFLICTS", "1"),
        ("TANDEM_TEST_STAGE_OBJECT_ON_INDEX_CONFLICT", MARKER),
    ]);

    std::fs::write(ws.join("contended.txt"), "published through contention\n")
        .expect("write file");
    common::assert_ok(
        &harness.run(&ws, &["describe", "-m", "through contention"]),
        "describe through the injected CAS conflict",
    );

    // A later publish has to carry the staged object into a WAL entry.
    std::fs::write(ws.join("later.txt"), "published after the retry\n").expect("write file");
    common::assert_ok(
        &harness.run(&ws, &["describe", "-m", "after the retry"]),
        "describe after the retried publish",
    );

    let entries = harness.wal_entries();
    assert!(
        !entries.is_empty(),
        "the publishes should have written WAL entries"
    );
    let carrier = entries
        .iter()
        .find(|(_, bytes)| contains(bytes, MARKER.as_bytes()));
    assert!(
        carrier.is_some(),
        "the object staged between two attempts of a retried publish is in no WAL entry \
         (it would be durable nowhere); entries: {:?}",
        entries.iter().map(|(name, _)| name).collect::<Vec<_>>()
    );
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

/// Killed after the index write, before the local apply: the bucket is ahead of
/// the repo, and the next start closes the gap.
#[test]
fn slice22_crash_after_index_write_converges_on_restart() {
    if using_s3() {
        eprintln!("skipping: this test reads the bucket off disk (filesystem backend only)");
        return;
    }

    let mut harness = Harness::new("crash-window");
    harness.start_server(&[]);
    let ws = harness.init_workspace("agent-a");
    common::assert_ok(&harness.run(&ws, &["describe", "-m", "before"]), "describe");

    let before = read_fs_index(&harness.bucket_dir);
    assert_eq!(harness.local_version(), before.version);

    // Restart with the crash hook armed, then publish.
    harness.stop_server();
    harness.start_server(&[("TANDEM_TEST_CRASH_AFTER_INDEX_WRITE", "1")]);

    std::fs::write(ws.join("in-flight.txt"), "written while the server died\n")
        .expect("write file");
    let out = harness.run(&ws, &["describe", "-m", "in flight"]);
    assert!(
        !out.status.success(),
        "the publish must fail: the server dies before acknowledging it"
    );
    harness.wait_for_server_exit();

    // The bucket committed; the repo did not.
    let stranded = read_fs_index(&harness.bucket_dir);
    assert!(
        stranded.version > before.version,
        "the index must have advanced before the crash ({} -> {})",
        before.version,
        stranded.version
    );
    assert!(
        harness.local_version() < stranded.version,
        "the local repo must still be behind the bucket ({} vs {})",
        harness.local_version(),
        stranded.version
    );
    for head in &stranded.op_heads {
        assert!(
            harness.bucket_dir.join("wal").join(head).exists(),
            "the stranded index must be backed by WAL entries: {head}"
        );
    }

    // Restart: recovery replays what the index still points at.
    harness.start_server(&[]);
    assert_eq!(
        harness.local_version(),
        stranded.version,
        "the local repo should adopt the bucket's version on restart"
    );
    let local_heads = harness.op_head_files();
    for head in &stranded.op_heads {
        assert!(
            local_heads.contains(head),
            "op head {head} named by the bucket is missing locally (lost op); local: {local_heads:?}"
        );
        assert!(
            harness.operation_is_stored(head),
            "operation {head} named by the bucket was not replayed into the op store"
        );
    }
    for head in &local_heads {
        assert!(
            stranded.op_heads.contains(head),
            "local op head {head} is not in the bucket (phantom op)"
        );
    }

    // And the workspace still works afterwards.
    let _ = harness.run(&ws, &["workspace", "update-stale"]);
    common::assert_ok(
        &harness.run(&ws, &["describe", "-m", "after recovery"]),
        "describe after recovery",
    );
    let after = read_fs_index(&harness.bucket_dir);
    assert!(
        after.version > stranded.version,
        "publishing must resume after recovery"
    );
}

/// Tier 2: the same durability path against a real S3 API (SeaweedFS).
/// Opt in with `TANDEM_TEST_S3_BUCKET`.
#[test]
fn slice22_seaweedfs_backed_publish_and_crash_recovery() {
    if !using_s3() {
        eprintln!("skipping: set TANDEM_TEST_S3_BUCKET to run the SeaweedFS test");
        return;
    }

    let mut harness = Harness::new("s3-crash");
    harness.start_server(&[]);
    let ws = harness.init_workspace("agent-a");
    common::assert_ok(&harness.run(&ws, &["describe", "-m", "before"]), "describe");
    let before_version = harness.local_version();

    harness.stop_server();
    harness.start_server(&[("TANDEM_TEST_CRASH_AFTER_INDEX_WRITE", "1")]);
    std::fs::write(ws.join("in-flight.txt"), "s3 in flight\n").expect("write file");
    let out = harness.run(&ws, &["describe", "-m", "in flight"]);
    assert!(!out.status.success(), "the publish must fail");
    harness.wait_for_server_exit();

    let stranded_local = harness.local_version();
    assert_eq!(
        stranded_local, before_version,
        "the local repo must not have advanced past the crash point"
    );

    harness.start_server(&[]);
    let recovered = harness.local_version();
    assert!(
        recovered > stranded_local,
        "the bucket's committed version must be adopted on restart ({stranded_local} -> {recovered})"
    );
    for head in harness.op_head_files() {
        assert!(
            harness.operation_is_stored(&head),
            "operation {head} is a head but is not stored"
        );
    }

    let _ = harness.run(&ws, &["workspace", "update-stale"]);
    common::assert_ok(
        &harness.run(&ws, &["describe", "-m", "after recovery"]),
        "describe after recovery",
    );
}

/// Reading the head state must not publish anything.
///
/// The server used to reconcile divergent op heads inside `getHeads`. Every
/// workspace keeps an entry in the head metadata naming the operation it
/// published last, so as soon as a second workspace publishes, the first
/// workspace's entry names an operation that is no longer a head. The
/// reconcile folded that entry in as a second head, minted a merge operation
/// for it, bumped the index version and wrote the index — all inside a call a
/// client makes in the middle of a command. The command that then published
/// its own operation found the version moved, retried its transaction and
/// rewrote the same change a second time, which is how a change id goes
/// divergent.
///
/// A read is a read: after the repo has settled, running read-only commands
/// must leave the index version, the op-head set and the WAL untouched.
#[test]
fn slice22_reading_heads_does_not_publish() {
    let mut harness = Harness::new("read-only-reads");
    harness.start_server(&[]);
    let ws_a = harness.init_workspace("agent-a");
    let ws_b = harness.init_workspace("agent-b");

    common::assert_ok(&harness.run(&ws_a, &["describe", "-m", "a writes"]), "a describe");
    // After b publishes, the metadata entry for a names an operation that is
    // no longer a head. That stale entry is what the reconcile used to seize.
    common::assert_ok(&harness.run(&ws_b, &["describe", "-m", "b writes"]), "b describe");

    // Let both workspaces settle, so anything below is a pure read.
    common::assert_ok(&harness.run(&ws_a, &["log", "--no-graph", "-T", "description"]), "a settles");
    common::assert_ok(&harness.run(&ws_b, &["log", "--no-graph", "-T", "description"]), "b settles");

    let version_before = harness.local_version();
    let heads_before = harness.op_head_files();
    let wal_before = harness.wal_entries().len();

    for _ in 0..3 {
        common::assert_ok(
            &harness.run(&ws_a, &["log", "--no-graph", "-T", "description"]),
            "a reads",
        );
        common::assert_ok(
            &harness.run(&ws_b, &["log", "--no-graph", "-T", "description"]),
            "b reads",
        );
    }

    assert_eq!(
        harness.local_version(),
        version_before,
        "reading the head state must not advance the index version"
    );
    assert_eq!(
        harness.op_head_files(),
        heads_before,
        "reading the head state must not mint an operation"
    );
    if !using_s3() {
        assert_eq!(
            harness.wal_entries().len(),
            wal_before,
            "reading the head state must not write a WAL entry"
        );
    }
}

/// How many WAL-entry puts the server attempted, split by outcome.
///
/// `put_wal_entry` logs both outcomes at debug level, so the log is a faithful
/// record of the bucket round-trips a publish made.
fn wal_put_attempts(log: &Path) -> (usize, usize) {
    let text = std::fs::read_to_string(log).expect("read server log");
    let written = text.matches("wrote WAL entry").count();
    let already_present = text.matches("WAL entry was already in the bucket").count();
    (written, already_present)
}

/// The ancestry walk must ask the bucket, not only this process's cache.
///
/// The cache is empty at every process start. A walk that prunes on the cache
/// alone therefore descends from the new head to the root operation on the
/// first publish after every restart, and puts a full entry body for every
/// operation in history — each one rejected as already present. Over a real
/// bucket that is one sequential round trip per operation, inside the server
/// mutex, with every other client blocked behind it.
#[test]
fn slice22_publishing_after_a_restart_does_not_rewalk_the_history() {
    if using_s3() {
        eprintln!("skipping: this test counts bucket writes off disk (filesystem backend only)");
        return;
    }

    let mut harness = Harness::new("restart-walk");
    harness.start_server(&[]);
    let ws = harness.init_workspace("agent-a");

    // Enough history that walking all of it is unmistakable.
    for n in 0..8 {
        common::assert_ok(
            &harness.run(&ws, &["describe", "-m", &format!("operation {n}")]),
            "build history",
        );
    }
    let history = harness.wal_entries().len();
    assert!(
        history >= 8,
        "the setup should have left a history to walk, got {history} WAL entries"
    );

    // Restart: whatever this process remembered about the bucket is gone.
    harness.stop_server();
    let log = harness.tmp.path().join("after-restart.log");
    harness.start_server_logging(&[], Some(&log));

    common::assert_ok(
        &harness.run(&ws, &["describe", "-m", "the first publish after a restart"]),
        "publish after restart",
    );

    let (written, already_present) = wal_put_attempts(&log);
    assert!(
        written >= 1,
        "the publish should have written its own WAL entry"
    );
    assert_eq!(
        already_present, 0,
        "the first publish after a restart re-uploaded {already_present} WAL entries the bucket \
         already held; the walk must stop at the first ancestor the bucket has (history is \
         {history} entries)"
    );
    assert!(
        written <= 3,
        "the first publish after a restart wrote {written} WAL entries; it should write only the \
         operations the bucket is missing (history is {history} entries)"
    );
}

/// Nothing after the index write may fail the publish.
///
/// Once the WAL entry and the index are durable and the operation is applied
/// locally, the RPC must be acknowledged. The server still has tidying to do —
/// it reconciles the head set, which mints a merge operation, and that merge
/// needs a WAL entry of its own — but a bucket that fails during that tidying
/// must not turn an operation that has already landed into a failed command:
/// the client would retry its transaction and rewrite the same change, which is
/// how a change id goes divergent.
///
/// `TANDEM_TEST_FAIL_DERIVED_HEAD_WAL` stands in for that bucket failure.
#[test]
fn slice22_a_failing_derived_head_write_does_not_fail_a_landed_publish() {
    let mut harness = Harness::new("derived-head-fault");
    harness.start_server(&[]);
    let ws_a = harness.init_workspace("agent-a");
    let ws_b = harness.init_workspace("agent-b");

    common::assert_ok(
        &harness.run(&ws_a, &["describe", "-m", "a writes"]),
        "a describe",
    );
    // After b publishes, a's metadata entry names an operation that is no
    // longer a head, so the next publish reconciles and mints a merge.
    common::assert_ok(
        &harness.run(&ws_b, &["describe", "-m", "b writes"]),
        "b describe",
    );

    harness.stop_server();
    let log = harness.tmp.path().join("derived-fault.log");
    harness.start_server_logging(&[("TANDEM_TEST_FAIL_DERIVED_HEAD_WAL", "1")], Some(&log));

    let version_before = harness.local_version();
    common::assert_ok(
        &harness.run(&ws_a, &["describe", "-m", "a writes through the fault"]),
        "publish while the derived head write fails",
    );

    assert!(
        harness.local_version() > version_before,
        "the publish must still have advanced the version ({} -> {})",
        version_before,
        harness.local_version()
    );

    // The fault has to have fired, or this test proves nothing.
    let text = std::fs::read_to_string(&log).expect("read server log");
    assert!(
        text.contains("could not make a derived op head durable"),
        "the injected derived-head failure never happened, so the acknowledged path was not \
         exercised"
    );

    // What was acknowledged is what the bucket holds: every head the index
    // names is backed by a WAL entry.
    if !using_s3() {
        let index = read_fs_index(&harness.bucket_dir);
        for head in &index.op_heads {
            assert!(
                harness.bucket_dir.join("wal").join(head).exists(),
                "the index names op head {head} but the bucket has no WAL entry for it"
            );
        }
    }

    // And the workspace keeps working.
    common::assert_ok(
        &harness.run(&ws_a, &["describe", "-m", "a writes again"]),
        "publish after the fault",
    );
}
