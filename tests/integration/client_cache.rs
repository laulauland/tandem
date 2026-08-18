//! The client's disk cache, from outside the process that owns it.
//!
//! The cache's own unit tests prove the entry format and the client's unit
//! tests prove that a hit sends no request. What only a subprocess can prove is
//! the part the acceptance criteria are actually about: that the directory is
//! what does the sharing — between one command and the next, and between two
//! workspaces that have never heard of each other — and that the location is a
//! thing an operator can choose, which is what baking a warm cache into an
//! image will come down to.
//!
//! The counting is done at the server, from its own log. Counting inside the
//! client would be counting the client's opinion of itself; the server's log
//! says what actually arrived.

use crate::common;
use crate::common::bucket_harness::read_dir_names;
use crate::common::workspace::first_commit_id;
use crate::common::ServerFixture;

use std::path::{Path, PathBuf};

/// The three content-addressed reads the server logs by name.
const OBJECT_READ: &str = "getObject";
const CONTENT_READS: [&str; 3] = ["getObject", "getOperation", "getView"];

const HELLO: &[u8] = b"fn main() { println!(\"cached\"); }\n";

/// Point a command at one cache directory.
fn at(cache_dir: &Path) -> [(&str, &str); 1] {
    [("TANDEM_CACHE_DIR", cache_dir.to_str().expect("cache dir"))]
}

/// Point a command at no cache at all — the control every "it was the cache"
/// claim needs.
const NO_CACHE: [(&str, &str); 1] = [("TANDEM_DISABLE_CACHE", "1")];

/// A workspace with one committed file in it, and that commit's id.
fn workspace_with_a_commit(fx: &ServerFixture, name: &str, cache_dir: &Path) -> (PathBuf, String) {
    let dir = fx.init_workspace_with_env(name, Some(name), &at(cache_dir));
    std::fs::create_dir_all(dir.join("src")).expect("create src");
    std::fs::write(dir.join("src/hello.rs"), HELLO).expect("write the file");
    let out =
        common::run_tandem_in_with_env(&dir, &["new", "-m", "add hello"], &at(cache_dir), &fx.home);
    common::assert_ok(&out, "commit the file");

    let log = common::run_tandem_in_with_env(
        &dir,
        &[
            "log",
            "--no-graph",
            "-r",
            "description(substring:\"add hello\")",
            "-T",
            "commit_id ++ \"\\n\"",
        ],
        &at(cache_dir),
        &fx.home,
    );
    let commit = first_commit_id(&log, "find the commit that added hello");
    (dir, commit)
}

/// Read the file back out of a commit without touching the working copy, so
/// the command is a read and nothing else.
fn read_the_file(fx: &ServerFixture, dir: &Path, commit: &str, env: &[(&str, &str)]) -> Vec<u8> {
    let out = common::run_tandem_in_with_env(
        dir,
        &[
            "file",
            "show",
            "--ignore-working-copy",
            "-r",
            commit,
            "src/hello.rs",
        ],
        env,
        &fx.home,
    );
    common::assert_ok(&out, "read the file back");
    out.stdout
}

#[test]
fn a_repeated_read_asks_the_server_for_no_objects() {
    let fx = ServerFixture::builder().log_to_file().start();
    let cache = fx.dir("cache");
    let (dir, commit) = workspace_with_a_commit(&fx, "agent", &cache);

    // The control: the same read with the cache switched off. Without it,
    // "the server was asked for nothing" could just as well mean the command
    // never needed an object in the first place.
    let before = fx.rpc_request_count(OBJECT_READ);
    assert_eq!(read_the_file(&fx, &dir, &commit, &NO_CACHE), HELLO);
    let uncached_reads = fx.rpc_request_count(OBJECT_READ) - before;
    assert!(
        uncached_reads > 0,
        "reading a file must cost object reads when there is no cache"
    );

    // The first cached read is what warms the directory; it may cost whatever
    // it costs. The second one is the claim — and not for objects alone:
    // operations and views are content-addressed on the same terms, and a
    // command that had to walk back to the server for either would not be much
    // better off.
    assert_eq!(read_the_file(&fx, &dir, &commit, &at(&cache)), HELLO);
    let before = counts(&fx);
    assert_eq!(read_the_file(&fx, &dir, &commit, &at(&cache)), HELLO);
    let warm = counts(&fx);

    for (method, (before, after)) in CONTENT_READS.iter().zip(before.iter().zip(warm.iter())) {
        assert_eq!(
            after - before,
            0,
            "the repeated read asked the server for {} {method} reads; the same read with no \
             cache asked for {uncached_reads} objects",
            after - before
        );
    }
}

/// How many of each content-addressed read the server has been asked for.
fn counts(fx: &ServerFixture) -> [usize; 3] {
    CONTENT_READS.map(|method| fx.rpc_request_count(method))
}

#[test]
fn one_cache_directory_serves_a_workspace_that_never_fetched_anything() {
    let fx = ServerFixture::builder().log_to_file().start();

    // The writer fills one directory. Nothing else ever writes to it.
    let shared = fx.dir("shared-cache");
    let (writer_dir, commit) = workspace_with_a_commit(&fx, "writer", &shared);
    assert_eq!(
        read_the_file(&fx, &writer_dir, &commit, &at(&shared)),
        HELLO
    );

    // The reader is set up with a cache directory of its own, so whatever its
    // own `init` fetched went somewhere else. It has never written a byte into
    // `shared`.
    let private = fx.dir("private-cache");
    let reader_dir = fx.init_workspace_with_env("reader", Some("reader"), &at(&private));

    // Control first: with no cache, this read costs the reader object fetches.
    let before = fx.rpc_request_count(OBJECT_READ);
    assert_eq!(read_the_file(&fx, &reader_dir, &commit, &NO_CACHE), HELLO);
    let uncached_reads = fx.rpc_request_count(OBJECT_READ) - before;
    assert!(
        uncached_reads > 0,
        "the reader's read must cost something without a cache"
    );

    // Now the same read, in the same reader workspace, pointed at the writer's
    // directory. Anything it finds there, the writer put there.
    let before = fx.rpc_request_count(OBJECT_READ);
    assert_eq!(
        read_the_file(&fx, &reader_dir, &commit, &at(&shared)),
        HELLO
    );
    let shared_reads = fx.rpc_request_count(OBJECT_READ) - before;

    assert_eq!(
        shared_reads, 0,
        "the reader asked for {shared_reads} objects out of the writer's cache; the same read \
         with no cache asked for {uncached_reads}"
    );
}

#[test]
fn the_cache_lands_where_the_environment_says() {
    let fx = ServerFixture::start();
    let chosen = fx.dir("somewhere-else");
    let (_dir, _commit) = workspace_with_a_commit(&fx, "agent", &chosen);

    // Objects, operations and views all land under the chosen root, each in
    // the namespace that keeps two ids of different kinds apart.
    let namespaces = read_dir_names(&chosen);
    assert!(
        namespaces.contains(&"file".to_string()),
        "no file objects under {}: {namespaces:?}",
        chosen.display()
    );
    assert!(
        namespaces.contains(&"op".to_string()) && namespaces.contains(&"view".to_string()),
        "operations and views must be cached too: {namespaces:?}"
    );
}

#[test]
fn a_cache_that_cannot_be_written_does_not_break_a_command() {
    // The cache is a performance layer and nothing else. Point it at a path
    // that cannot become a directory and every command must still work.
    let fx = ServerFixture::start();
    let blocked = fx.path().join("not-a-directory");
    std::fs::write(&blocked, b"in the way").expect("write the blocking file");

    let (dir, commit) = workspace_with_a_commit(&fx, "agent", &blocked);
    assert_eq!(read_the_file(&fx, &dir, &commit, &at(&blocked)), HELLO);
    assert_eq!(read_the_file(&fx, &dir, &commit, &at(&blocked)), HELLO);
}
