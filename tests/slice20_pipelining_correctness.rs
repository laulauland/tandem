//! Slice 20: Pipelining correctness under rapid sequential commits.
//!
//! Acceptance focus:
//! - rapid commit flow remains byte-correct
//! - no stale-head corruption during fast describe/new cycles

mod common;

use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};
use tempfile::TempDir;

fn run_tandem_with_timeout(dir: &Path, args: &[&str], home: &Path) -> Output {
    let mut cmd = Command::new(common::tandem_bin());
    cmd.current_dir(dir);
    common::isolate_env(&mut cmd, home);
    for arg in args {
        cmd.arg(arg);
    }
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());

    let mut child = cmd.spawn().expect("spawn tandem command");
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Some(_status) = child.try_wait().expect("try_wait tandem command") {
            return child
                .wait_with_output()
                .expect("wait tandem command output");
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let out = child
                .wait_with_output()
                .expect("wait timed out tandem command");
            panic!(
                "tandem command timed out: {:?}\nstdout:\n{}\nstderr:\n{}",
                args,
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
        }
        thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn slice20_rapid_sequential_commits_round_trip() {
    let commit_count = 12;
    let tmp = TempDir::new().unwrap();
    let home = common::isolated_home(tmp.path());
    let server_repo = tmp.path().join("server-repo");
    std::fs::create_dir_all(&server_repo).unwrap();
    let workspace_dir = tmp.path().join("workspace");
    std::fs::create_dir_all(&workspace_dir).unwrap();

    let addr = common::free_addr();
    let mut server =
        common::spawn_server_with_args(&server_repo, &addr, &["--log-level", "error"], &home);
    common::wait_for_server(&addr, &mut server);

    let init = run_tandem_with_timeout(&workspace_dir, &["init", "--server", &addr, "."], &home);
    common::assert_ok(&init, "tandem init");

    let src_dir = workspace_dir.join("src");
    std::fs::create_dir_all(&src_dir).unwrap();

    let mut descriptions = Vec::with_capacity(commit_count);
    let mut filenames = Vec::with_capacity(commit_count);
    let mut contents = Vec::with_capacity(commit_count);

    for i in 0..commit_count {
        let filename = format!("seq_{i}.txt");
        let description = format!("slice20-rapid-seq-{i:02}");
        let content = format!("slice20 content {i} :: {:08}\n", i * 17).into_bytes();

        std::fs::write(src_dir.join(&filename), &content).unwrap();

        let describe =
            run_tandem_with_timeout(&workspace_dir, &["describe", "-m", &description], &home);
        common::assert_ok(&describe, &format!("describe {description}"));

        let new = run_tandem_with_timeout(&workspace_dir, &["new"], &home);
        common::assert_ok(&new, &format!("new after {description}"));

        descriptions.push(description);
        filenames.push(filename);
        contents.push(content);
    }

    for i in 0..commit_count {
        let revset = format!("description(substring:\"{}\")", descriptions[i]);
        let path = format!("src/{}", filenames[i]);
        let cat = run_tandem_with_timeout(
            &workspace_dir,
            &["file", "show", "-r", &revset, &path],
            &home,
        );
        common::assert_ok(&cat, &format!("file show {path}"));
        assert_eq!(cat.stdout, contents[i], "{path} content mismatch");
    }

    let _ = server.kill();
    let _ = server.wait();
}
