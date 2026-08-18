//! Slice 4: rapid successive writes round-trip byte for byte.
//!
//! This file used to be about Cap'n Proto promise pipelining, which the HTTP
//! transport has no equivalent of. What survives is the part that was never
//! about the wire: writing many files back to back and getting every byte of
//! every one back, from the client and from the server's own repo.

mod common;

use std::time::Instant;
use tempfile::TempDir;

/// Write N files, each in its own commit, and verify all round-trip
/// correctly — through the workspace and through the server repo.
#[test]
fn slice4_ten_files_rapid_fire_round_trip() {
    let file_count = 10;
    let tmp = TempDir::new().unwrap();
    let home = common::isolated_home(tmp.path());
    let server_repo = tmp.path().join("server-repo");
    std::fs::create_dir_all(&server_repo).unwrap();
    let workspace_dir = tmp.path().join("workspace");
    std::fs::create_dir_all(&workspace_dir).unwrap();

    let addr = common::free_addr();
    let mut server = common::spawn_server(&server_repo, &addr);
    common::wait_for_server(&addr, &mut server);

    // Initialize workspace
    let init = common::run_tandem_in(&workspace_dir, &["init", "--server", &addr, "."], &home);
    common::assert_ok(&init, "tandem init");

    // Prepare file contents — each file has unique, verifiable content
    let src_dir = workspace_dir.join("src");
    std::fs::create_dir_all(&src_dir).unwrap();

    let contents: Vec<Vec<u8>> = (0..file_count)
        .map(|i| {
            format!(
                "pub fn file_{i}() -> &'static str {{\n    \
                 \"content from file {i} — rapid-write test\"\n}}\n"
            )
            .into_bytes()
        })
        .collect();
    let filenames: Vec<String> = (0..file_count).map(|i| format!("file_{i}.rs")).collect();
    let descriptions: Vec<String> = (0..file_count).map(|i| format!("add file_{i}")).collect();

    // ── Rapid-fire: write + commit 10 files in quick succession ──────
    let start = Instant::now();

    for i in 0..file_count {
        std::fs::write(src_dir.join(&filenames[i]), &contents[i]).unwrap();

        let describe =
            common::run_tandem_in(&workspace_dir, &["describe", "-m", &descriptions[i]], &home);
        common::assert_ok(&describe, &format!("describe file_{i}"));

        let new = common::run_tandem_in(&workspace_dir, &["new"], &home);
        common::assert_ok(&new, &format!("new after file_{i}"));
    }

    let elapsed = start.elapsed();
    eprintln!(
        "wrote and committed {file_count} files in {:.2}s ({:.0}ms/file)",
        elapsed.as_secs_f64(),
        elapsed.as_secs_f64() * 1000.0 / file_count as f64,
    );

    // ── Verify all commits visible in log ────────────────────────────
    let log = common::run_tandem_in(&workspace_dir, &["log", "--no-graph", "-r", "all()"], &home);
    common::assert_ok(&log, "jj log all");
    let log_text = common::stdout_str(&log);
    for desc in &descriptions {
        assert!(
            log_text.contains(desc.as_str()),
            "log should contain '{desc}'\nlog output:\n{log_text}"
        );
    }

    // ── Verify every file round-trips with exact bytes ───────────────
    for i in 0..file_count {
        let revset = format!("description(substring:\"{}\")", descriptions[i]);
        let cat = common::run_tandem_in(
            &workspace_dir,
            &[
                "file",
                "show",
                "-r",
                &revset,
                &format!("src/{}", filenames[i]),
            ],
            &home,
        );
        common::assert_ok(&cat, &format!("file show src/{}", filenames[i]));
        assert_eq!(
            cat.stdout, contents[i],
            "src/{} content mismatch",
            filenames[i]
        );
    }

    // ── Verify server also has all files ─────────────────────────────
    for i in 0..file_count {
        let revset = format!("description(substring:\"{}\")", descriptions[i]);
        let server_cat = common::run_tandem_in_with_env(
            &server_repo,
            &[
                "file",
                "show",
                "--ignore-working-copy",
                "-r",
                &revset,
                &format!("src/{}", filenames[i]),
            ],
            &[],
            &home,
        );
        common::assert_ok(
            &server_cat,
            &format!("server file show src/{}", filenames[i]),
        );
        assert_eq!(
            server_cat.stdout, contents[i],
            "server src/{} content mismatch",
            filenames[i]
        );
    }

    let _ = server.kill();
    let _ = server.wait();
}

/// Bigger payloads round-trip too: a ~10KB blob is a different code path in
/// the transport than a two-line one.
#[test]
fn slice4_large_files_round_trip() {
    let tmp = TempDir::new().unwrap();
    let home = common::isolated_home(tmp.path());
    let server_repo = tmp.path().join("server-repo");
    std::fs::create_dir_all(&server_repo).unwrap();
    let workspace_dir = tmp.path().join("workspace");
    std::fs::create_dir_all(&workspace_dir).unwrap();

    let addr = common::free_addr();
    let mut server = common::spawn_server(&server_repo, &addr);
    common::wait_for_server(&addr, &mut server);

    let init = common::run_tandem_in(&workspace_dir, &["init", "--server", &addr, "."], &home);
    common::assert_ok(&init, "tandem init");

    let src_dir = workspace_dir.join("src");
    std::fs::create_dir_all(&src_dir).unwrap();

    // Generate 5 files, each ~10KB of unique content
    let file_count = 5;
    let contents: Vec<Vec<u8>> = (0..file_count)
        .map(|i| {
            let mut content = format!("// Large file {i}\n");
            for line in 0..200 {
                content.push_str(&format!(
                    "pub const LINE_{line}: &str = \"file {i} line {line} padding\";\n"
                ));
            }
            content.into_bytes()
        })
        .collect();

    for i in 0..file_count {
        std::fs::write(src_dir.join(format!("large_{i}.rs")), &contents[i]).unwrap();
    }

    let describe = common::run_tandem_in(
        &workspace_dir,
        &["describe", "-m", "add large files"],
        &home,
    );
    common::assert_ok(&describe, "describe large files");

    let new = common::run_tandem_in(&workspace_dir, &["new"], &home);
    common::assert_ok(&new, "new after large files");

    for i in 0..file_count {
        let path = format!("src/large_{i}.rs");
        let cat =
            common::run_tandem_in(&workspace_dir, &["file", "show", "-r", "@-", &path], &home);
        common::assert_ok(&cat, &format!("file show {path}"));
        assert_eq!(cat.stdout, contents[i], "{path} content mismatch");
    }

    let _ = server.kill();
    let _ = server.wait();
}

/// Files accumulate across successive commits: each commit adds a file and
/// keeps the ones before it.
#[test]
fn slice4_cumulative_tree_growth() {
    let file_count = 5;
    let tmp = TempDir::new().unwrap();
    let home = common::isolated_home(tmp.path());
    let server_repo = tmp.path().join("server-repo");
    std::fs::create_dir_all(&server_repo).unwrap();
    let workspace_dir = tmp.path().join("workspace");
    std::fs::create_dir_all(&workspace_dir).unwrap();

    let addr = common::free_addr();
    let mut server = common::spawn_server(&server_repo, &addr);
    common::wait_for_server(&addr, &mut server);

    let init = common::run_tandem_in(&workspace_dir, &["init", "--server", &addr, "."], &home);
    common::assert_ok(&init, "tandem init");

    let src_dir = workspace_dir.join("src");
    std::fs::create_dir_all(&src_dir).unwrap();

    let contents: Vec<Vec<u8>> = (0..file_count)
        .map(|i| format!("pub fn cumulative_{i}() {{}}\n").into_bytes())
        .collect();

    for i in 0..file_count {
        std::fs::write(src_dir.join(format!("mod_{i}.rs")), &contents[i]).unwrap();

        let describe = common::run_tandem_in(
            &workspace_dir,
            &["describe", "-m", &format!("add mod_{i}")],
            &home,
        );
        common::assert_ok(&describe, &format!("describe mod_{i}"));

        let new = common::run_tandem_in(&workspace_dir, &["new"], &home);
        common::assert_ok(&new, &format!("new after mod_{i}"));
    }

    // The last described commit should hold every file, because each `new`
    // creates a child that inherits its parent's tree.
    let revset = format!("description(substring:\"add mod_{}\")", file_count - 1);
    for i in 0..file_count {
        let path = format!("src/mod_{i}.rs");
        let cat = common::run_tandem_in(
            &workspace_dir,
            &["file", "show", "-r", &revset, &path],
            &home,
        );
        common::assert_ok(&cat, &format!("file show {path} from final commit"));
        assert_eq!(
            cat.stdout, contents[i],
            "final commit should contain {path} with correct content"
        );
    }

    let _ = server.kill();
    let _ = server.wait();
}
