//! `tandem clone`, and the two things it can mean.
//!
//! A clone of a name the server has never heard of makes the workspace. A
//! clone of a name it already has attaches to it, and the files that come back
//! are the ones that name last published — which is the whole reason a
//! workspace can outlive the machine it was on.
//!
//! A subprocess is the only honest way to check either: what is being asserted
//! is what is on disk after the binary ran, and an in-process test would be
//! asserting about a `MergedTree` instead of about files.

use crate::common;
use crate::common::ServerFixture;

use std::path::{Path, PathBuf};

/// Clone a workspace into a new directory under the fixture, insisting it
/// worked, and answer what the clone said it did — `created` or `attached`.
pub fn clone_workspace(fx: &ServerFixture, dir_name: &str, workspace: &str) -> (PathBuf, String) {
    let dir = fx.dir(dir_name);
    let out = common::run_tandem_in(
        &dir,
        &[
            "clone",
            &fx.addr,
            ".",
            "--workspace",
            workspace,
            "--token",
            fx.token(),
        ],
        &fx.home,
    );
    common::assert_ok(&out, &format!("clone {workspace} into {dir_name}"));
    let origin = common::stdout_str(&out)
        .lines()
        .find_map(|line| common::field_in(line, "origin="))
        .unwrap_or_else(|| {
            panic!(
                "clone printed no origin=\nstdout:\n{}\nstderr:\n{}",
                common::stdout_str(&out),
                common::stderr_str(&out)
            )
        });
    (dir, origin)
}

/// Every file in a workspace, with its bytes, ignoring the repository's own.
///
/// This is what "byte-identical" is asserted on: the paths and the contents,
/// and nothing about how either got there.
pub fn tree_of(root: &Path) -> Vec<(String, Vec<u8>)> {
    fn walk(root: &Path, dir: &Path, into: &mut Vec<(String, Vec<u8>)>) {
        let entries = std::fs::read_dir(dir).unwrap_or_else(|e| panic!("read {dir:?}: {e}"));
        for entry in entries {
            let entry = entry.expect("read a directory entry");
            let path = entry.path();
            let name = entry.file_name();
            if name == ".jj" || name == ".git" {
                continue;
            }
            if path.is_dir() {
                walk(root, &path, into);
            } else {
                let relative = path
                    .strip_prefix(root)
                    .expect("a path under the root")
                    .to_string_lossy()
                    .into_owned();
                into.push((relative, std::fs::read(&path).expect("read a file")));
            }
        }
    }
    let mut files = Vec::new();
    walk(root, root, &mut files);
    files.sort();
    files
}

#[test]
fn a_clone_creates_the_workspace_and_materializes_what_the_server_has() {
    let fx = ServerFixture::start();
    let home = fx.home.clone();

    // Something for the clone to materialize. It goes into the server's own
    // default workspace, which is where a new workspace starts from.
    let seeded = b"the server had this first\n";
    std::fs::write(fx.repo.join("seed.txt"), seeded).unwrap();
    common::assert_ok(
        &common::run_tandem_in(&fx.repo, &["new", "-m", "seed the server"], &home),
        "seed the server's default workspace",
    );

    let (dir, origin) = clone_workspace(&fx, "fresh", "agent-a");
    assert_eq!(origin, "created", "a name nobody has used is created");

    assert!(dir.join(".jj").is_dir(), "a clone leaves a jj workspace");
    let store_type = std::fs::read_to_string(dir.join(".jj/repo/store/type")).unwrap();
    assert_eq!(
        store_type.trim(),
        "tandem",
        "a cloned workspace is backed by the server, not by a local store"
    );

    assert_eq!(
        std::fs::read(dir.join("seed.txt")).expect("the seeded file is on disk"),
        seeded,
        "a clone materializes the files, byte for byte"
    );
}

#[test]
fn cloning_a_name_the_server_knows_attaches_to_its_last_snapshot() {
    let fx = ServerFixture::start();
    let home = fx.home.clone();

    let (first, origin) = clone_workspace(&fx, "first", "agent-a");
    assert_eq!(origin, "created");

    // Publish something under that name. A `jj` command does it here on
    // purpose: what is under test is the attach, not how the snapshot got
    // published — the daemon's own path is `workspace_daemon.rs`.
    std::fs::create_dir_all(first.join("src")).unwrap();
    std::fs::write(first.join("src/lib.rs"), b"pub fn one() -> u8 { 1 }\n").unwrap();
    std::fs::write(first.join("README"), b"agent-a was here\n").unwrap();
    common::assert_ok(
        &common::run_tandem_in(&first, &["describe", "-m", "agent-a's work"], &home),
        "describe agent-a's work",
    );

    let expected = tree_of(&first);
    assert!(!expected.is_empty(), "the first workspace has files");

    // Somewhere else entirely, under the same name.
    let (second, origin) = clone_workspace(&fx, "second", "agent-a");
    assert_eq!(
        origin, "attached",
        "a name the server already has is attached to, not created again"
    );
    assert_ne!(first, second, "the two clones are different directories");

    assert_eq!(
        tree_of(&second),
        expected,
        "an attached clone reproduces the last published snapshot byte for byte"
    );
}

#[test]
fn an_attached_clone_can_publish_on_top_of_what_it_attached_to() {
    let fx = ServerFixture::start();
    let home = fx.home.clone();

    let (first, _) = clone_workspace(&fx, "first", "agent-a");
    std::fs::write(first.join("one.txt"), b"one\n").unwrap();
    common::assert_ok(
        &common::run_tandem_in(&first, &["describe", "-m", "the first machine"], &home),
        "publish from the first machine",
    );

    let (second, origin) = clone_workspace(&fx, "second", "agent-a");
    assert_eq!(origin, "attached");

    // The attach left a workspace that works: it can commit, and what it
    // commits sits on top of what it attached to rather than beside it.
    std::fs::write(second.join("two.txt"), b"two\n").unwrap();
    common::assert_ok(
        &common::run_tandem_in(&second, &["new", "-m", "the second machine"], &home),
        "publish from the second machine",
    );

    let log = common::run_tandem_in(
        &second,
        &["log", "--no-graph", "-n", "10", "-T", "description"],
        &home,
    );
    common::assert_ok(&log, "read the second machine's log");
    let text = common::stdout_str(&log);
    assert!(
        text.contains("the first machine"),
        "the attached workspace's history still has what it attached to:\n{text}"
    );

    // And both files are there — the second machine did not start over.
    assert_eq!(std::fs::read(second.join("one.txt")).unwrap(), b"one\n");
    assert_eq!(std::fs::read(second.join("two.txt")).unwrap(), b"two\n");
}

/// A clock far enough behind that the operation an interrupted clone leaves
/// looks older than the snapshot it is competing with.
///
/// Two machines, two clocks. The one that is behind is not misbehaving — it is
/// a laptop that suspended, a container with no NTP, a VM restored from a
/// snapshot — and nothing in the protocol makes it agree with anybody. What is
/// under test is that a stale clock cannot be used to overwrite a workspace,
/// and a fixed timestamp is the only way to ask that question the same way
/// twice.
const SKEWED_CLOCK: &str = "2001-02-03T04:05:06+00:00";

/// Publish two files under `agent-a`, and answer what is on disk afterwards.
///
/// A `jj` command does the publishing: what these tests are about is what a
/// later clone of the name reproduces, not how the snapshot got there.
fn publish_agent_a_work(fx: &ServerFixture) -> Vec<(String, Vec<u8>)> {
    let home = fx.home.clone();
    let (first, origin) = clone_workspace(fx, "first", "agent-a");
    assert_eq!(origin, "created");
    std::fs::write(first.join("keep.txt"), b"the last published snapshot\n").unwrap();
    std::fs::write(first.join("also.txt"), b"and this\n").unwrap();
    common::assert_ok(
        &common::run_tandem_in(&first, &["describe", "-m", "agent-a's work"], &home),
        "publish agent-a's work",
    );
    let expected = tree_of(&first);
    assert_eq!(expected.len(), 2, "two files were published: {expected:?}");
    expected
}

/// Begin cloning a name on a machine whose clock is behind, and kill it
/// halfway.
///
/// jj's own workspace creation has published an operation pointing the name at
/// a fresh empty commit, and the operation that would have pointed it back at
/// the snapshot was never written. The clock is what makes that leftover
/// dangerous — the server merges divergent heads oldest first, and the oldest
/// side is the one that wins a working-copy pointer both sides moved.
fn kill_a_clone_halfway(fx: &ServerFixture, dir_name: &str, workspace: &str) {
    let dead = fx.dir(dir_name);
    let killed = common::run_tandem_in_with_env(
        &dead,
        &[
            "clone",
            &fx.addr,
            ".",
            "--workspace",
            workspace,
            "--token",
            fx.token(),
        ],
        &[
            ("TANDEM_TEST_ABORT_AFTER_WORKSPACE_INIT", "1"),
            ("JJ_OP_TIMESTAMP", SKEWED_CLOCK),
        ],
        &fx.home,
    );
    assert!(
        !killed.status.success(),
        "the clone was supposed to stop halfway\nstdout:\n{}\nstderr:\n{}",
        common::stdout_str(&killed),
        common::stderr_str(&killed)
    );
}

#[test]
fn a_clone_killed_between_its_two_operations_does_not_cost_the_next_one_its_files() {
    let fx = ServerFixture::start();
    let home = fx.home.clone();

    // Real work published under the name.
    let expected = publish_agent_a_work(&fx);

    // A second machine begins cloning the same name and dies halfway.
    kill_a_clone_halfway(&fx, "dead", "agent-a");

    // A third machine, on a third disk, clones the same name. The empty commit
    // the dead clone published is not what it may attach to.
    let (third, origin) = clone_workspace(&fx, "third", "agent-a");
    assert_eq!(
        origin, "attached",
        "the name still belongs to the last published snapshot"
    );
    assert_eq!(
        tree_of(&third),
        expected,
        "a clone after an interrupted one reproduces the last published snapshot, not the empty \
         commit the interrupted clone left"
    );

    // And the workspace it left is usable: it can publish on top of what it
    // attached to, which is the thing the empty commit would have broken.
    std::fs::write(third.join("third.txt"), b"third\n").unwrap();
    common::assert_ok(
        &common::run_tandem_in(&third, &["new", "-m", "the third machine"], &home),
        "publish from the third machine",
    );
    let log = common::run_tandem_in(
        &third,
        &["log", "--no-graph", "-n", "10", "-T", "description"],
        &home,
    );
    common::assert_ok(&log, "read the third machine's log");
    assert!(
        common::stdout_str(&log).contains("agent-a's work"),
        "the recovered workspace still has the history it attached to:\n{}",
        common::stdout_str(&log)
    );
}

/// Publish two files under `agent-a`, then kill a clone of that name halfway
/// on a clock that is behind, with the reconcile that would have merged the
/// leftover away armed to degrade. Answers what was published, so the caller
/// can assert a later clone reproduces it byte for byte.
///
/// `reconcile_jj_op_heads` is best-effort by design — it runs after the publish
/// is durable and already acknowledged, so it hands back the unmerged heads
/// whenever it cannot read, order or merge them, rather than failing a publish
/// that has already landed. The degrade is what leaves the leftover alive as a
/// sibling head instead of an ancestor of the settled one, and every way of
/// losing a workspace to it needs that state first.
fn a_leftover_the_server_could_not_merge(fx: &mut ServerFixture) -> Vec<(String, Vec<u8>)> {
    let expected = publish_agent_a_work(fx);

    // Two, because a clone publishes twice before it gets that far: the root
    // operation the fresh repo starts at, and the `add workspace` operation
    // that names this workspace. Both trigger a reconcile, and the leftover
    // only survives if neither of them merges it away.
    fx.restart_with_env(&[("TANDEM_TEST_FAIL_RECONCILES", "2")]);

    kill_a_clone_halfway(fx, "dead", "agent-a");

    expected
}

/// The leftover survives the reconcile, and then a *client* merges it in.
///
/// The machine that died did not die for good: it came back, and somebody ran
/// a command in the half-cloned directory. That client is this workspace's own
/// client, so its op-heads store hands jj both the settled head and the
/// operation the server records as this workspace's last one — the leftover.
/// jj then merges the two itself: `jj_lib::op_heads_store::resolve_op_heads`
/// orders heads by the end time in each operation's metadata and merges them in
/// that order, with nothing pinned. The leftover's clock is behind, so the
/// leftover is the base, and the base side wins a working-copy pointer both
/// sides moved. The server's own ordering never gets a say — by the time the
/// operation reaches it, the merge is already made.
#[test]
fn a_client_merging_a_leftover_in_cannot_hand_it_the_workspace() {
    let mut fx = ServerFixture::start();
    let home = fx.home.clone();
    let expected = a_leftover_the_server_could_not_merge(&mut fx);

    // The command is allowed to fail — a half-cloned working copy is stale and
    // jj says so. What matters is that loading the repo merged the two heads
    // and published the merge, which happens first.
    let _ = common::run_tandem_in_with_env(
        &fx.dir("dead"),
        &["log", "--no-graph", "-n", "5", "-T", "description"],
        &[("JJ_OP_TIMESTAMP", SKEWED_CLOCK)],
        &home,
    );

    let (third, origin) = clone_workspace(&fx, "third", "agent-a");
    assert_eq!(
        origin, "attached",
        "the name still belongs to the last published snapshot"
    );
    assert_eq!(
        tree_of(&third),
        expected,
        "a clone after a client merged an interrupted clone's leftover in reproduces the last \
         published snapshot, not the empty commit"
    );
}

/// The leftover survives the reconcile, and then the *server* merges it in, on
/// somebody else's publish.
///
/// `order_op_heads` pins the arriving operation last, so a leftover can never
/// be the base of the merge its own publish triggers. That is the only merge
/// the pin covers. Here the leftover is already sitting there when an unrelated
/// workspace publishes: the arriving operation is that workspace's, the pin
/// spends itself on it, and the leftover — oldest by a clock nobody checked —
/// takes the base seat after all.
#[test]
fn a_leftover_merged_in_by_somebody_elses_publish_cannot_take_the_workspace() {
    let mut fx = ServerFixture::start();
    let home = fx.home.clone();
    let expected = a_leftover_the_server_could_not_merge(&mut fx);

    // An unrelated workspace, doing ordinary work. Its publish is what makes
    // the server merge the heads it could not merge before.
    let (other, origin) = clone_workspace(&fx, "other", "agent-b");
    assert_eq!(origin, "created");
    std::fs::write(other.join("elsewhere.txt"), b"agent-b was busy\n").unwrap();
    common::assert_ok(
        &common::run_tandem_in(&other, &["describe", "-m", "agent-b's work"], &home),
        "publish agent-b's work",
    );

    let (third, origin) = clone_workspace(&fx, "third", "agent-a");
    assert_eq!(
        origin, "attached",
        "the name still belongs to the last published snapshot"
    );
    assert_eq!(
        tree_of(&third),
        expected,
        "a clone after somebody else's publish merged the leftover in reproduces the last \
         published snapshot, not the empty commit"
    );
}

/// The other direction: work a workspace threw away on purpose stays thrown
/// away.
///
/// A workspace that abandons everything it had lands on an empty commit on the
/// root commit — the same shape as the commit an interrupted clone leaves
/// behind. So a repair that read only the shape of the settled commit, or that
/// went looking through the merged operations for any commit with files in it,
/// would put the abandoned work back and call that a fix. That is the same data
/// loss in the other direction, and it is silent in the same way. The merge here
/// is the one the repair does examine — an interrupted clone of the same name,
/// on a clock that is behind, with the reconcile degraded — so the guard is
/// being asked the question, not stepped around.
#[test]
fn work_a_workspace_abandoned_is_not_put_back_by_the_repair() {
    let mut fx = ServerFixture::start();
    let home = fx.home.clone();

    let (first, origin) = clone_workspace(&fx, "first", "agent-a");
    assert_eq!(origin, "created");
    std::fs::write(first.join("keep.txt"), b"for now\n").unwrap();
    std::fs::write(first.join("also.txt"), b"and this\n").unwrap();
    common::assert_ok(
        &common::run_tandem_in(&first, &["describe", "-m", "agent-a's work"], &home),
        "publish agent-a's work",
    );

    // And then decides it wants none of it.
    common::assert_ok(
        &common::run_tandem_in(&first, &["abandon"], &home),
        "abandon agent-a's work",
    );
    let abandoned = tree_of(&first);
    assert!(
        !abandoned.iter().any(|(path, _)| path == "keep.txt"),
        "the abandon took the files with it: {abandoned:?}"
    );

    fx.restart_with_env(&[("TANDEM_TEST_FAIL_RECONCILES", "2")]);

    kill_a_clone_halfway(&fx, "dead", "agent-a");

    let (third, origin) = clone_workspace(&fx, "third", "agent-a");
    assert_eq!(origin, "attached");
    assert_eq!(
        tree_of(&third),
        abandoned,
        "a clone after the abandon reproduces what the workspace chose to keep, and does not \
         resurrect what it threw away"
    );
}

#[test]
fn a_clone_fills_the_cache_it_is_pointed_at() {
    let fx = ServerFixture::start();
    let home = fx.home.clone();

    std::fs::write(fx.repo.join("payload.txt"), b"something worth caching\n").unwrap();
    common::assert_ok(
        &common::run_tandem_in(&fx.repo, &["new", "-m", "seed"], &home),
        "seed the server",
    );

    // The cache has no CLI surface on purpose: `tandem clone` *is* the action
    // that warms it, which is what makes it the thing to run at image-bake
    // time. So the only handle a test has is the directory it names.
    let cache = fx.path().join("shared-cache");
    let dir = fx.dir("cloned");
    let out = common::run_tandem_in_with_env(
        &dir,
        &[
            "clone",
            &fx.addr,
            ".",
            "--workspace",
            "agent-a",
            "--token",
            fx.token(),
        ],
        &[("TANDEM_CACHE_DIR", cache.to_str().unwrap())],
        &home,
    );
    common::assert_ok(&out, "clone with a cache directory");

    let cached = count_files(&cache);
    assert!(
        cached > 0,
        "a clone leaves the objects it fetched in the cache at {}",
        cache.display()
    );
}

fn count_files(dir: &Path) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    entries
        .flatten()
        .map(|entry| {
            if entry.path().is_dir() {
                count_files(&entry.path())
            } else {
                1
            }
        })
        .sum()
}
