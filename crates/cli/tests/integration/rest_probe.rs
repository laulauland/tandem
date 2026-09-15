//! Frozen request-count experiment: real CLI, isolated caches, exact bytes.
use crate::common::{self, ServerFixture};
use std::path::Path;

fn requests(fx: &ServerFixture) -> usize {
    fx.log_text()
        .lines()
        .filter(|line| line.contains("rpc request"))
        .count()
}

fn run(fx: &ServerFixture, dir: &Path, cache: &Path, args: &[&str]) -> std::process::Output {
    let output = common::run_tandem_in_with_env(
        dir,
        args,
        &[("TANDEM_CACHE_DIR", cache.to_str().unwrap())],
        &fx.home,
    );
    common::assert_ok(&output, "REST study command");
    output
}

#[test]
fn rest_probe_request_counts_and_bytes() {
    let fx = ServerFixture::builder().log_to_file().start();
    let writer = fx.init_workspace("writer", Some("writer"));
    let reader = fx.init_workspace("reader", Some("reader"));
    let writer_cache = fx.dir("writer-cache");
    let reader_cache = fx.dir("reader-cache");
    // Explicit user-side setup, outside measurement. Never a daemon repair.
    common::workspace::settle_workspace(&writer, &fx.home);
    common::workspace::settle_workspace(&reader, &fx.home);
    run(
        &fx,
        &writer,
        &writer_cache,
        &["status", "--ignore-working-copy"],
    );
    run(
        &fx,
        &reader,
        &reader_cache,
        &["status", "--ignore-working-copy"],
    );

    let before = requests(&fx);
    run(
        &fx,
        &writer,
        &writer_cache,
        &["status", "--ignore-working-copy"],
    );
    let startup = requests(&fx) - before;

    let files: Vec<_> = (0..8u8)
        .map(|index| {
            let name = format!("file-{index}.bin");
            let bytes = vec![0, 255, index, b'\n'];
            std::fs::write(writer.join(&name), &bytes).unwrap();
            (name, bytes)
        })
        .collect();
    let before = requests(&fx);
    run(&fx, &writer, &writer_cache, &["status"]);
    let publish = requests(&fx) - before;
    let id = run(
        &fx,
        &writer,
        &writer_cache,
        &[
            "log",
            "--ignore-working-copy",
            "--no-graph",
            "-r",
            "@",
            "-T",
            "commit_id",
        ],
    );
    let id = String::from_utf8(id.stdout).unwrap();

    let before = requests(&fx);
    let first = run(
        &fx,
        &reader,
        &reader_cache,
        &[
            "file",
            "show",
            "--ignore-working-copy",
            "-r",
            id.trim(),
            &files[0].0,
        ],
    );
    let catchup = requests(&fx) - before;
    assert_eq!(first.stdout, files[0].1);
    for (name, bytes) in &files[1..] {
        let read = run(
            &fx,
            &reader,
            &reader_cache,
            &[
                "file",
                "show",
                "--ignore-working-copy",
                "-r",
                id.trim(),
                name,
            ],
        );
        assert_eq!(&read.stdout, bytes);
    }
    println!(
        "REST_STUDY {}",
        serde_json::json!({
            "startup": startup, "publish": publish, "catchup": catchup,
            "score": startup + publish + catchup,
        })
    );
    // jj 0.45.1 reads the same exact file bytes with 16 catch-up requests.
    assert_eq!(
        (startup, publish, catchup),
        (2, 8, 16),
        "the frozen CLI workload request shape regressed"
    );
}
