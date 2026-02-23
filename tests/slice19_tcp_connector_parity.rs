mod common;

use std::process::{Command, Stdio};
use std::time::Duration;

use tempfile::TempDir;

#[test]
fn slice19_tcp_connector_command_path_parity() {
    let tmp = TempDir::new().unwrap();
    let home = common::isolated_home(tmp.path());
    let server_repo = tmp.path().join("server-repo");
    std::fs::create_dir_all(&server_repo).unwrap();
    let workspace = tmp.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();

    let addr = common::free_addr();
    let mut server = common::spawn_server(&server_repo, &addr);
    common::wait_for_server(&addr, &mut server);

    let init = common::run_tandem_in(&workspace, &["init", "--server", &addr, "."], &home);
    common::assert_ok(&init, "tandem init");

    let file_bytes = b"connector parity bytes\n";
    std::fs::write(workspace.join("hello.txt"), file_bytes).unwrap();

    let new_commit = common::run_tandem_in(&workspace, &["new", "-m", "connector parity"], &home);
    common::assert_ok(&new_commit, "jj new");

    let log = common::run_tandem_in(&workspace, &["log", "--no-graph", "-n", "5"], &home);
    common::assert_ok(&log, "jj log");
    assert!(
        common::stdout_str(&log).contains("connector parity"),
        "log output missing commit description\nstdout:\n{}\nstderr:\n{}",
        common::stdout_str(&log),
        common::stderr_str(&log)
    );

    let show = common::run_tandem_in(
        &workspace,
        &["file", "show", "-r", "@-", "hello.txt"],
        &home,
    );
    common::assert_ok(&show, "file show");
    assert_eq!(show.stdout, file_bytes, "file show bytes should round-trip");

    let _ = server.kill();
    let _ = server.wait();
}

#[test]
fn slice19_tcp_connector_watch_registration_and_notification() {
    let tmp = TempDir::new().unwrap();
    let home = common::isolated_home(tmp.path());
    let server_repo = tmp.path().join("server-repo");
    std::fs::create_dir_all(&server_repo).unwrap();
    let workspace = tmp.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();

    let addr = common::free_addr();
    let mut server = common::spawn_server(&server_repo, &addr);
    common::wait_for_server(&addr, &mut server);

    let init = common::run_tandem_in(&workspace, &["init", "--server", &addr, "."], &home);
    common::assert_ok(&init, "tandem init");

    let mut watch_cmd = Command::new(common::tandem_bin());
    watch_cmd
        .current_dir(tmp.path())
        .args(["watch", "--server", &addr])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    common::isolate_env(&mut watch_cmd, &home);
    let mut watch = watch_cmd.spawn().expect("spawn tandem watch");

    std::thread::sleep(Duration::from_millis(500));

    std::fs::write(workspace.join("watch.txt"), b"watch event\n").unwrap();
    let new_commit = common::run_tandem_in(&workspace, &["new", "-m", "watch parity"], &home);
    common::assert_ok(&new_commit, "jj new for watch parity");

    std::thread::sleep(Duration::from_millis(800));

    let _ = watch.kill();
    let output = watch.wait_with_output().expect("collect watch output");

    let stdout = common::stdout_str(&output);
    let stderr = common::stderr_str(&output);
    assert!(
        stderr.contains("watching heads on"),
        "watch stderr should confirm registration\nstderr:\n{stderr}"
    );
    assert!(
        stdout.lines().any(|line| line.starts_with("version=")),
        "watch should emit at least one notification\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );

    let _ = server.kill();
    let _ = server.wait();
}
