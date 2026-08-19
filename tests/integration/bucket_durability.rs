//! The bucket is the durable source of truth.
//!
//! What stays here is the part that needs a real process against a real bucket:
//! the shape a publish leaves behind, what a read is allowed to do, how many
//! round trips a boot pays, and what an unclean death costs. The crash windows
//! themselves belong to the simulation in `tests/dst.rs`, which arms every one
//! of them from a seed instead of the three a hand-written test picked.
//!
//! Tier 1 runs against the filesystem backend. Tier 2 runs the same shape
//! against SeaweedFS; set `TANDEM_TEST_S3_BUCKET` to opt in, e.g.
//! `s3://tandem-test?endpoint=http://127.0.0.1:8333&anonymous=true`.

use crate::common;

use std::collections::BTreeMap;
use std::path::Path;

use common::bucket_harness::{using_s3, BucketHarness as Harness};

// ─── Harness ──────────────────────────────────────────────────────────────────
//
// The shared parts live in `tests/common/bucket_harness.rs`. What follows is
// only what these tests need on top of them.

impl Harness {
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
fn publish_writes_a_wal_entry_and_the_index_object() {
    if using_s3() {
        eprintln!("skipping: this test reads the bucket off disk (filesystem backend only)");
        return;
    }

    let mut harness = Harness::new("wal-shape");
    harness.start_server();
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

/// A server that is killed, not asked to stop, must lose nothing it had already
/// acknowledged — and must be able to keep going afterwards.
///
/// The simulation drives the same property through every window in the
/// durability order, but it drives an in-process server over a filesystem
/// bucket. This one kills a real process, and in tier 2 the bucket on the other
/// side of the kill is a real S3 API.
#[test]
fn an_unclean_death_loses_nothing_that_was_acknowledged() {
    let mut harness = Harness::new("unclean-death");
    harness.start_server();
    let ws = harness.init_workspace("agent-a");

    std::fs::write(ws.join("acknowledged.txt"), "this publish was acked\n").expect("write file");
    common::assert_ok(
        &harness.run(&ws, &["describe", "-m", "acknowledged before the kill"]),
        "describe",
    );
    let acked_version = harness.local_version();
    let acked_heads = harness.op_head_files();
    assert!(!acked_heads.is_empty(), "the publish should have left heads");

    // SIGKILL: no shutdown hook, no flush, nothing written on the way out.
    harness.stop_server();
    harness.start_server();

    assert_eq!(
        harness.local_version(),
        acked_version,
        "the acknowledged version must survive an unclean death"
    );
    assert_eq!(
        harness.op_head_files(),
        acked_heads,
        "the acknowledged op heads must survive an unclean death"
    );
    for head in &acked_heads {
        assert!(
            harness.operation_is_stored(head),
            "operation {head} is a head after the restart but is not stored"
        );
    }

    // Everything the index names is still backed by a WAL entry.
    if !using_s3() {
        let index = read_fs_index(&harness.bucket_dir);
        for head in &index.op_heads {
            assert!(
                harness.bucket_dir.join("wal").join(head).exists(),
                "the index names op head {head} but the bucket has no WAL entry for it"
            );
        }
    }

    // And the workspace keeps working across the gap.
    let _ = harness.run(&ws, &["workspace", "update-stale"]);
    common::assert_ok(
        &harness.run(&ws, &["describe", "-m", "published after the kill"]),
        "describe after the kill",
    );
    assert!(
        harness.local_version() > acked_version,
        "publishing must resume after an unclean death"
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
fn reading_heads_does_not_publish() {
    let mut harness = Harness::new("read-only-reads");
    harness.start_server();
    let ws_a = harness.init_workspace("agent-a");
    let ws_b = harness.init_workspace("agent-b");

    common::assert_ok(
        &harness.run(&ws_a, &["describe", "-m", "a writes"]),
        "a describe",
    );
    // After b publishes, the metadata entry for a names an operation that is
    // no longer a head. That stale entry is what the reconcile used to seize.
    common::assert_ok(
        &harness.run(&ws_b, &["describe", "-m", "b writes"]),
        "b describe",
    );

    // Let both workspaces settle, so anything below is a pure read.
    common::assert_ok(
        &harness.run(&ws_a, &["log", "--no-graph", "-T", "description"]),
        "a settles",
    );
    common::assert_ok(
        &harness.run(&ws_b, &["log", "--no-graph", "-T", "description"]),
        "b settles",
    );

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
fn publishing_after_a_restart_does_not_rewalk_the_history() {
    if using_s3() {
        eprintln!("skipping: this test counts bucket writes off disk (filesystem backend only)");
        return;
    }

    let mut harness = Harness::new("restart-walk");
    harness.start_server();
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
    harness.start_server_logging(Some(&log));

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
