//! Baking a sandbox image, and what a container booted from it still has to
//! fetch.
//!
//! The recipe is `docs/images/Dockerfile`: two commands at image build time,
//! with `TANDEM_CACHE_DIR` pointed at a path inside the image. `tandem clone`
//! leaves a materialized working copy and the objects it was built from;
//! `tandem workspace update-stale` after it leaves jj's commit index and the
//! operations and views that index was built from. A container started from
//! that image attaches to the same workspace name, and the claim the recipe
//! rests on is that the attach costs only what changed since the bake.
//!
//! That claim is a number, so it is asserted as one. The counting is done at
//! the server, from its own log, for the reason `client_cache.rs` gives:
//! counting inside the client would be counting the client's opinion of
//! itself. Every measurement here is paired with the same boot run against no
//! cache at all — without that control, "the boot asked for almost nothing"
//! could just as well mean the boot needed almost nothing.
//!
//! And every measurement counts operations and views alongside objects. A
//! count of objects alone answers "how much of the repository did this pull"
//! and says nothing about how much of the *history* it walked — which is where
//! this recipe was once O(everything the server had ever done) while every
//! object count in this file looked healthy.
//!
//! Nothing here builds a container. What a container adds over a directory is
//! a filesystem namespace, and the image's warmth is a directory either way;
//! the Dockerfile is verified by building and booting it, which is a recorded
//! act rather than a `cargo test` (see `docs/images/README.md`).

use crate::clone::tree_of;
use crate::common;
use crate::common::ServerFixture;
use crate::workspace_daemon::DaemonProcess;

use std::path::{Path, PathBuf};

/// The read the server logs by name when a client asks for a file, a tree or
/// a commit. All three are the same endpoint, which is what makes one count
/// answer "how much of the repository did this command pull".
const OBJECT_READ: &str = "getObject";

/// The two reads that carry the operation log. Counting only [`OBJECT_READ`]
/// answers "how much of the *repository* did this command pull" and says
/// nothing about how much of the *history* it walked, and a boot pays for both
/// on the same serial connection.
const OPERATION_READ: &str = "getOperation";
const VIEW_READ: &str = "getView";

/// Every read a boot can spend a round trip on. Kept apart as well as added up,
/// so that a failure says which of the three grew — the difference between
/// "this boot pulled the repository" and "this boot walked the history".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Reads {
    objects: usize,
    operations: usize,
    views: usize,
}

impl Reads {
    /// What the server has been asked for so far.
    fn so_far(fx: &ServerFixture) -> Self {
        Self {
            objects: fx.rpc_request_count(OBJECT_READ),
            operations: fx.rpc_request_count(OPERATION_READ),
            views: fx.rpc_request_count(VIEW_READ),
        }
    }

    /// What one command cost: the counts now, less the counts before it ran.
    fn since(self, before: Self) -> Self {
        Self {
            objects: self.objects - before.objects,
            operations: self.operations - before.operations,
            views: self.views - before.views,
        }
    }

    /// The number a cold start is actually made of: one serial round trip each.
    fn total(self) -> usize {
        self.objects + self.operations + self.views
    }
}

impl std::fmt::Display for Reads {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} reads ({} objects, {} operations, {} views)",
            self.total(),
            self.objects,
            self.operations,
            self.views
        )
    }
}

/// The name the image is baked against, and the name a container booted from
/// it attaches to. Namespaced, because a workspace token may only move
/// bookmarks under its own prefix.
const WORKSPACE: &str = "agent-a";

/// How many files the repository holds at bake time.
///
/// Enough that a boot which refetched the tree could not be mistaken for one
/// that fetched a delta: the two differ by an order of magnitude rather than
/// by a few reads that a retry could account for.
const BAKED_FILES: usize = 24;

/// How many files are published after the bake, from somewhere else. This is
/// the delta a booted container has to go and get.
const DELTA_FILES: usize = 2;

/// Point a command at one cache directory — the stand-in for the path the
/// Dockerfile bakes into the image.
fn at(cache: &Path) -> [(&str, &str); 1] {
    [("TANDEM_CACHE_DIR", cache.to_str().expect("a cache path"))]
}

/// Point a command at no cache at all: a container with no baked layer.
const NO_CACHE: [(&str, &str); 1] = [("TANDEM_DISABLE_CACHE", "1")];

/// Clone `WORKSPACE` into a new directory under the fixture.
///
/// Build time and boot time run the same command, which is the point: the
/// recipe adds no verb, and there is nothing for an image to do at boot that a
/// person could not do by hand.
fn clone_workspace_with(fx: &ServerFixture, dir_name: &str, env: &[(&str, &str)]) -> PathBuf {
    clone_named_workspace_with(fx, WORKSPACE, dir_name, env)
}

/// The same clone, under a name of the caller's choosing.
///
/// Only the workspace that has nothing to do with the bake needs this — one
/// place owns the argument list either way.
fn clone_named_workspace_with(
    fx: &ServerFixture,
    workspace: &str,
    dir_name: &str,
    env: &[(&str, &str)],
) -> PathBuf {
    let dir = fx.dir(dir_name);
    let out = common::run_tandem_in_with_env(
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
        env,
        &fx.home,
    );
    common::assert_ok(&out, &format!("clone {workspace} into {dir_name}"));
    dir
}

/// Every arrival ends at the same files: the repository as it was baked, plus
/// what was published after the bake.
///
/// `unbaked` is the control — the same arrival with no image behind it — so
/// this is not a count against a constant but the two trees against each other,
/// byte for byte. Without it, "the boot asked for almost nothing" could mean
/// the boot arrived at almost nothing.
fn assert_boot_ends_at_the_bake_plus_the_delta(booted: &Path, unbaked: &Path) {
    let expected = tree_of(unbaked);
    assert_eq!(
        expected.len(),
        BAKED_FILES + DELTA_FILES,
        "a boot ends at the bake plus the delta: {expected:?}"
    );
    assert_eq!(
        tree_of(booted),
        expected,
        "the booted container reproduces the same files as one with no image, byte for byte"
    );
}

/// Put a repository worth of files on the server, each with contents of its
/// own so that no two of them are one object.
fn seed_the_server(fx: &ServerFixture, count: usize) {
    let src = fx.repo.join("src");
    std::fs::create_dir_all(&src).expect("create the seeded source directory");
    for file in 0..count {
        std::fs::write(
            src.join(format!("module_{file:02}.rs")),
            format!("pub fn module_{file:02}() -> usize {{ {file} }}\n"),
        )
        .expect("write a seeded file");
    }
    common::assert_ok(
        &common::run_tandem_in(
            &fx.repo,
            &["new", "-m", "the repository at bake time"],
            &fx.home,
        ),
        "seed the server's default workspace",
    );
}

/// Publish `DELTA_FILES` new files under the same workspace name, from a
/// checkout with a cache of its own.
///
/// The cache matters: the delta has to be work the baked image has never seen.
/// Publishing it from the bake's own directory would leave its objects in the
/// bake's cache, and the boot would then be reading its own writes back.
fn publish_the_delta_from_elsewhere(fx: &ServerFixture) {
    let elsewhere_cache = fx.path().join("somebody-elses-cache");
    let elsewhere = clone_workspace_with(fx, "elsewhere", &at(&elsewhere_cache));
    for file in 0..DELTA_FILES {
        std::fs::write(
            elsewhere.join(format!("since_the_bake_{file}.rs")),
            format!("pub fn since_the_bake_{file}() {{}}\n"),
        )
        .expect("write a file published after the bake");
    }
    common::assert_ok(
        &common::run_tandem_in_with_env(
            &elsewhere,
            &["describe", "-m", "work published after the bake"],
            &at(&elsewhere_cache),
            &fx.home,
        ),
        "publish the delta",
    );
}

/// The two operation-log lengths the delta-only claim is measured at. The
/// claim is that the boot costs the same at both — the numbers themselves only
/// have to differ by enough that a walk of the history could not hide in the
/// noise.
const A_SHORT_HISTORY: usize = 4;
const A_LONG_HISTORY: usize = 40;

/// Publish something under the baked workspace's own name, from somewhere
/// else, before the bake happens.
///
/// This is what makes the bake an *attach* rather than a creation: the name
/// already has a published snapshot to come back to.
fn give_the_workspace_something_to_come_back_to(fx: &ServerFixture) {
    let seed_cache = fx.path().join("seed-cache");
    let seeded = clone_workspace_with(fx, "seed", &at(&seed_cache));
    common::assert_ok(
        &common::run_tandem_in_with_env(
            &seeded,
            &[
                "describe",
                "-m",
                "what the workspace published before the bake",
            ],
            &at(&seed_cache),
            &fx.home,
        ),
        "publish the workspace's pre-bake snapshot",
    );
}

/// Run `commands` commands in a workspace that has nothing to do with the one
/// being baked, so that the operation log is already long when the bake
/// happens.
///
/// Each `describe` publishes an operation, and each operation names a view.
fn give_the_server_a_history(fx: &ServerFixture, commands: usize) {
    let busy_cache = fx.path().join("busy-cache");
    let busy = clone_named_workspace_with(fx, "agent-busy", "busy", &at(&busy_cache));
    for round in 0..commands {
        common::assert_ok(
            &common::run_tandem_in_with_env(
                &busy,
                &["describe", "-m", &format!("busy work {round}")],
                &at(&busy_cache),
                &fx.home,
            ),
            "a command in the busy workspace",
        );
    }
}

/// The second half of the bake: catch the freshly cloned workspace up to the
/// server's head, at build time, so that the boot does not have to.
///
/// This is the `RUN tandem workspace update-stale` line of the Dockerfile, and
/// the whole reason it is there. A clone reads one operation and one view — its
/// own — so the operation log is a cache miss from end to end afterwards, and
/// jj's index is built from whichever ancestor operation already has one. The
/// clone's operation is a *sibling* of everything the server has done, so at
/// the next load jj merges the two and has to walk every operation on the other
/// side to index it. Running the catch-up here pays that walk at build time,
/// where it is a layer: it leaves an index that covers the server's history and
/// a cache holding the operations and views it was built from, and the boot
/// then walks only what came after.
fn warm_the_bake(fx: &ServerFixture, dir: &Path, env: &[(&str, &str)]) {
    common::assert_ok(
        &common::run_tandem_in_with_env(dir, &["workspace", "update-stale"], env, &fx.home),
        "warm the bake",
    );
}

/// Bring a booted workspace to the last snapshot its name published.
///
/// This is the first half of the image's entrypoint. The daemon will not do it
/// — moving files under whoever is editing them is a decision, not a reflex —
/// and a container that has just started is the one moment when nobody is
/// editing them yet, which is what makes it the entrypoint's job and not the
/// daemon's.
fn catch_the_boot_up(fx: &ServerFixture, dir: &Path, env: &[(&str, &str)]) {
    common::assert_ok(
        &common::run_tandem_in_with_env(dir, &["workspace", "update-stale"], env, &fx.home),
        "catch the booted workspace up to what its name published",
    );
}

/// What one bake-and-boot cost, against a server carrying `history` commands
/// of unrelated work.
struct BootCost {
    /// What the boot spent, from the bake's layers.
    warm: Reads,
    /// What the same arrival cost with no image at all — the control.
    cold: Reads,
}

/// Bake an image against a server with `history` unrelated commands behind it,
/// boot from it, and count what each of the two arrivals cost.
///
/// The whole recipe is here in order: seed the repository, give the baked name
/// something to come back to (so the bake *attaches*, which is the shape a
/// sandbox image is baked in — and a different code path from creating a name),
/// give the server a history that has nothing to do with either, bake, publish
/// a delta from elsewhere, boot.
fn bake_and_boot_against_a_history(history: usize) -> BootCost {
    let fx = ServerFixture::builder().log_to_file().start();

    seed_the_server(&fx, BAKED_FILES);
    give_the_workspace_something_to_come_back_to(&fx);
    give_the_server_a_history(&fx, history);

    // The two `RUN` lines of the Dockerfile's bake, in the order the Dockerfile
    // has them.
    let baked_cache = fx.path().join("image-cache");
    let booted = clone_workspace_with(&fx, "boot", &at(&baked_cache));
    warm_the_bake(&fx, &booted, &at(&baked_cache));

    publish_the_delta_from_elsewhere(&fx);

    // The boot: the image's entrypoint, on the image's layers.
    let before = Reads::so_far(&fx);
    catch_the_boot_up(&fx, &booted, &at(&baked_cache));
    let warm = Reads::so_far(&fx).since(before);

    // The control the image exists to beat: an arrival with no bake in it at
    // all, which has to fetch the repository before it can do anything.
    let before = Reads::so_far(&fx);
    let unbaked = clone_workspace_with(&fx, "boot-with-no-image", &NO_CACHE);
    let cold = Reads::so_far(&fx).since(before);

    assert_boot_ends_at_the_bake_plus_the_delta(&booted, &unbaked);

    BootCost { warm, cold }
}

#[test]
fn what_a_boot_costs_is_the_delta_and_not_the_server_s_history() {
    // The claim the recipe rests on, in the only form that can be falsified:
    // the same bake and the same delta against two servers whose operation logs
    // differ by an order of magnitude, and the same price both times.
    //
    // Every other test in this file bakes against a server whose entire history
    // is what the bake itself just made. That is the one shape of repository in
    // which "the boot walked the history" and "the boot walked the delta" are
    // the same measurement, so it is the one shape that cannot tell them apart
    // — and it is not the shape any real server is in.
    //
    // The reads counted here are all three kinds, not objects alone. An
    // operation and a view are each a round trip like any other, and it was
    // exactly there that this recipe used to be O(the server's whole history):
    // a clone reads one operation and one view, so the operation log was a cache
    // miss from end to end, and jj indexes from whichever ancestor operation
    // already has an index — which for a clone's sibling operation meant
    // walking every operation on the other side. Counting objects alone made
    // that invisible.
    let short = bake_and_boot_against_a_history(A_SHORT_HISTORY);
    let long = bake_and_boot_against_a_history(A_LONG_HISTORY);

    assert_eq!(
        short.warm, long.warm,
        "booting from a bake cost {} against a server with {A_SHORT_HISTORY} commands of history \
         and {} against one with {A_LONG_HISTORY}. What a container pays to arrive has to be a \
         function of what changed since the bake, and of nothing else — a cost that tracks the \
         history is the cold start this image exists to remove, charged again at every boot and \
         growing every day the server stays up.",
        short.warm, long.warm
    );

    assert!(
        long.warm.total() < long.cold.total(),
        "against a server with {A_LONG_HISTORY} commands of history, booting from the bake cost \
         {} and arriving with no bake at all cost {}. The image is supposed to be the cheap one.",
        long.warm,
        long.cold
    );

    // And the sharp form: the price is within a few round trips of the delta
    // itself — the two files published after the bake, the tree that names
    // them, the commit that points at that tree, and the handful of operations
    // and views that publish and the boot's own merge of the heads added.
    let delta_objects = DELTA_FILES + 2;
    let allowance = delta_objects + 20;
    assert!(
        long.warm.total() <= allowance,
        "the booted container spent {} on a delta of {delta_objects} objects. The bound here is \
         {allowance}, which is the delta plus what it costs to merge two operation heads and \
         index the operations between them; a boot that goes past it is reading something it was \
         supposed to have baked.",
        long.warm
    );
}

#[test]
fn a_container_booted_from_a_bake_fetches_only_what_changed_since_it() {
    let fx = ServerFixture::builder().log_to_file().start();

    seed_the_server(&fx, BAKED_FILES);

    // The bake, twice. These are the two `RUN` lines of the Dockerfile's bake,
    // and what they leave is the image: a materialized working copy, an index
    // that covers the server's history, and the objects, operations and views
    // all three were built from. Two copies of it because two containers are
    // about to be started from the same image, and a container gets the layer
    // to itself.
    let baked_cache = fx.path().join("image-cache");
    let booted = clone_workspace_with(&fx, "boot", &at(&baked_cache));
    warm_the_bake(&fx, &booted, &at(&baked_cache));
    let booted_without_the_cache = clone_workspace_with(&fx, "boot-cache-off", &at(&baked_cache));
    warm_the_bake(&fx, &booted_without_the_cache, &at(&baked_cache));
    let at_bake = tree_of(&booted);
    assert_eq!(
        at_bake.len(),
        BAKED_FILES,
        "the bake materialized the repository: {at_bake:?}"
    );

    publish_the_delta_from_elsewhere(&fx);

    // The boot: the image's entrypoint, on the image's layers. Every read is
    // counted, not objects alone: an operation and a view cost a round trip
    // like anything else, and a boot that walked the operation log would be
    // invisible in an object count. See
    // `what_a_boot_costs_is_the_delta_and_not_the_server_s_history`.
    let before = Reads::so_far(&fx);
    catch_the_boot_up(&fx, &booted, &at(&baked_cache));
    let warm_reads = Reads::so_far(&fx).since(before).total();

    // The first control: the same image, booted with the cache switched off.
    // It still has the working copy, so it pays only for what it has to read
    // twice — which is the point of the comparison.
    let before = Reads::so_far(&fx);
    catch_the_boot_up(&fx, &booted_without_the_cache, &NO_CACHE);
    let cache_off_reads = Reads::so_far(&fx).since(before).total();

    // The second control, and the one the image is actually for: a container
    // with no bake in it at all, which has to fetch the repository before it
    // can do anything.
    let before = Reads::so_far(&fx);
    let unbaked = clone_workspace_with(&fx, "boot-with-no-image", &NO_CACHE);
    let unbaked_reads = Reads::so_far(&fx).since(before).total();

    // Every one of them arrived at the same files, so the three counts are
    // three prices for one thing and may be compared.
    assert_boot_ends_at_the_bake_plus_the_delta(&booted, &unbaked);

    assert!(
        unbaked_reads >= BAKED_FILES,
        "a container with no bake has to fetch the whole repository, and asked for only \
         {unbaked_reads} times with {BAKED_FILES} files in it"
    );
    assert!(
        warm_reads < unbaked_reads,
        "booting from the bake cost {warm_reads} reads and booting with no bake cost \
         {unbaked_reads}; the image saved nothing"
    );
    assert!(
        warm_reads < cache_off_reads,
        "booting from the bake cost {warm_reads} reads and the same boot with the cache \
         switched off cost {cache_off_reads}; whatever made the boot cheap, it was not the \
         baked cache"
    );
}

#[test]
fn a_boot_that_clones_rather_than_catching_up_fetches_only_the_delta_too() {
    // The other shape of container: one that keeps the baked cache and clones
    // into a fresh directory at boot, rather than starting in the working copy
    // the image carries. It is the shape to reach for when the workspace name
    // is decided at boot rather than at build. The claim is the same one, and
    // it is the sharper measurement of the two: a clone reads the whole tree,
    // so what it does *not* read is visible.
    let fx = ServerFixture::builder().log_to_file().start();
    seed_the_server(&fx, BAKED_FILES);
    give_the_workspace_something_to_come_back_to(&fx);
    // Against a server that has been used, like the other measurement of the
    // claim: this shape has an operation log to walk too, and a fresh directory
    // means a fresh index, so the walk cannot be avoided here — only paid for
    // out of the baked cache instead of over the wire.
    give_the_server_a_history(&fx, A_LONG_HISTORY);

    let baked_cache = fx.path().join("image-cache");
    let bake = clone_workspace_with(&fx, "bake", &at(&baked_cache));
    warm_the_bake(&fx, &bake, &at(&baked_cache));
    publish_the_delta_from_elsewhere(&fx);

    let before = Reads::so_far(&fx);
    let booted = clone_workspace_with(&fx, "boot", &at(&baked_cache));
    let warm = Reads::so_far(&fx).since(before);

    // The first command a container of this shape runs, which is where the
    // operation-log walk lands for it: the clone itself never loads the
    // server's head, so the index of the fresh directory is built by whatever
    // runs next. It is bounded here because the walk reads out of the baked
    // cache rather than over the wire.
    let before = Reads::so_far(&fx);
    common::assert_ok(
        &common::run_tandem_in_with_env(
            &booted,
            &["log", "-r", "@", "--no-graph", "--ignore-working-copy"],
            &at(&baked_cache),
            &fx.home,
        ),
        "the first command inside a container of this shape",
    );
    let first_command = Reads::so_far(&fx).since(before);

    let before = Reads::so_far(&fx);
    let unbaked = clone_workspace_with(&fx, "boot-with-no-image", &NO_CACHE);
    let unbaked_reads = Reads::so_far(&fx).since(before);

    assert_eq!(
        tree_of(&booted),
        tree_of(&unbaked),
        "the baked clone reproduces the same files as the unbaked one, byte for byte"
    );
    assert!(
        unbaked_reads.objects >= BAKED_FILES,
        "a clone with no cache behind it has to fetch the whole repository, and asked for only \
         {unbaked_reads} with {BAKED_FILES} files in it"
    );
    // The delta itself, counted out: the files that were published after the
    // bake, the one tree that names them, and the commit that points at that
    // tree. Nothing else about the repository changed, so nothing else is
    // owed. The bound is written this way rather than as a round number
    // because a round number would go on passing while the cost crept up
    // inside it.
    let delta_objects = DELTA_FILES + 2;
    assert!(
        warm.objects <= delta_objects,
        "the booted container asked for {warm}; the delta since the bake is {delta_objects} \
         objects — {DELTA_FILES} files, the tree that names them and the commit that points at \
         it (a boot with no image asked for {unbaked_reads})"
    );
    assert!(
        first_command.total() <= delta_objects + 20,
        "the first command in a container of this shape spent {first_command}, with \
         {A_LONG_HISTORY} commands of unrelated history on the server. This shape defers the \
         index build to the first command, and the bake is supposed to have left the operations \
         and views that build needs on disk."
    );
}

#[test]
fn what_a_bake_leaves_behind_is_a_workspace_its_objects_and_an_index() {
    let fx = ServerFixture::start();
    seed_the_server(&fx, BAKED_FILES);
    give_the_workspace_something_to_come_back_to(&fx);
    give_the_server_a_history(&fx, A_SHORT_HISTORY);

    let baked_cache = fx.path().join("image-cache");
    let baked = clone_workspace_with(&fx, "bake", &at(&baked_cache));
    warm_the_bake(&fx, &baked, &at(&baked_cache));

    // All three have to be in the image. The working copy alone would boot to
    // files with a cold cache behind them; the cache alone would boot to an
    // empty directory; and without the index, the first load at head walks the
    // server's operation log to build one.
    assert!(
        baked.join(".jj").is_dir(),
        "the bake leaves a jj workspace at the path the image will start in"
    );
    let namespaces = common::bucket_harness::read_dir_names(&baked_cache);
    assert!(
        namespaces.contains(&"file".to_string())
            && namespaces.contains(&"op".to_string())
            && namespaces.contains(&"view".to_string()),
        "the bake fills the cache directory the image bakes in — objects, operations and views: \
         {namespaces:?}"
    );

    // The index is per workspace directory and lives next to the store, so it
    // is a layer of the image rather than a line in the cache. What makes it
    // worth asserting is that it is keyed by operation: the boot's own merge
    // starts from whichever ancestor operation already has one, and the whole
    // saving is that this is that operation.
    let indexed_operations = baked.join(".jj/repo/index/op_links");
    let indexed = common::bucket_harness::read_dir_names(&indexed_operations);
    assert!(
        !indexed.is_empty(),
        "the bake leaves jj's commit index next to the workspace, keyed by the operation it was \
         built at; {} holds {indexed:?}",
        indexed_operations.display()
    );
}

#[test]
fn a_booted_container_publishes_through_the_daemon_its_image_starts() {
    let fx = ServerFixture::start();
    seed_the_server(&fx, DELTA_FILES);

    // Bake, then boot: the container's working copy came from the image's
    // bake, not from anything the container did.
    let baked_cache = fx.path().join("image-cache");
    let baked = clone_workspace_with(&fx, "bake", &at(&baked_cache));
    warm_the_bake(&fx, &baked, &at(&baked_cache));
    let booted = clone_workspace_with(&fx, "boot", &at(&baked_cache));

    // The image's ENTRYPOINT. Nothing tells it what to publish or when.
    let mut daemon = DaemonProcess::start(&booted, &fx.home);
    std::fs::write(
        booted.join("written_in_the_container.rs"),
        b"pub fn written_in_the_container() {}\n",
    )
    .expect("write a file inside the booted container");
    daemon.wait_for_publish();
    daemon.stop();

    // And it is durable: a checkout of the same name somewhere else has it.
    let elsewhere = clone_workspace_with(&fx, "elsewhere", &NO_CACHE);
    let files = tree_of(&elsewhere);
    assert!(
        files
            .iter()
            .any(|(path, _)| path == "written_in_the_container.rs"),
        "what the booted container's daemon published is what the workspace name now holds: \
         {files:?}"
    );
}
