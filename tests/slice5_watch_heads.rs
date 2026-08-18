mod common;

use std::process::{Command, Stdio};
use std::time::Duration;
use tempfile::TempDir;

/// A watcher says on stderr that it is watching, and then prints what it
/// sees on stdout.
///
/// This assertion arrived from slice 19, which used to check that the TCP
/// connector registered a watch. There is one client path now, so what is
/// left worth checking is that `tandem watch` reports the subscription it
/// opened and then reports the head changes that follow it.
#[test]
fn slice5_watch_reports_its_registration_and_then_notifies() {
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
    let new_commit = common::run_tandem_in(&workspace, &["new", "-m", "watch registration"], &home);
    common::assert_ok(&new_commit, "jj new for watch registration");

    std::thread::sleep(Duration::from_millis(800));

    let _ = watch.kill();
    let output = watch.wait_with_output().expect("collect watch output");

    let stdout = common::stdout_str(&output);
    let stderr = common::stderr_str(&output);
    assert!(
        stderr.contains("watching heads on"),
        "watch stderr should confirm registration\nstderr:\n{stderr}"
    );

    // `tandem watch` prints the head state it starts from before it reads a
    // single event, so one `version=` line proves nothing about the stream.
    // The commit above moved the heads; a working subscription therefore owes
    // a second line at a higher version.
    let versions: Vec<u64> = stdout
        .lines()
        .filter_map(|line| line.strip_prefix("version="))
        .filter_map(|rest| rest.split_whitespace().next())
        .filter_map(|value| value.parse().ok())
        .collect();
    assert!(
        versions.len() >= 2,
        "watch should print the state it started from and then the commit's \
         version, got {versions:?}\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        versions.last() > versions.first(),
        "the notification after the commit should carry a higher version than \
         the one watch started from, got {versions:?}\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );

    let _ = server.kill();
    let _ = server.wait();
}

#[test]
fn slice5_watch_heads_notifications() {
    let tmp = TempDir::new().unwrap();
    let home = common::isolated_home(tmp.path());
    let server_repo = tmp.path().join("server-repo");
    std::fs::create_dir_all(&server_repo).unwrap();
    let workspace_a = tmp.path().join("workspace-a");
    std::fs::create_dir_all(&workspace_a).unwrap();

    let addr = common::free_addr();
    let mut server = common::spawn_server(&server_repo, &addr);
    common::wait_for_server(&addr, &mut server);

    // Initialize workspace A
    let init = common::run_tandem_in(&workspace_a, &["init", "--server", &addr, "."], &home);
    common::assert_ok(&init, "tandem init workspace A");

    // Start `tandem watch` as a background process
    let mut watch_proc = Command::new(common::tandem_bin())
        .args(["watch", "--server", &addr])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn tandem watch");

    // Give the watcher a moment to connect and register
    std::thread::sleep(Duration::from_millis(500));

    // Agent A: write a file and commit
    std::fs::write(workspace_a.join("hello.txt"), b"hello world\n").unwrap();
    let new1 = common::run_tandem_in(&workspace_a, &["new", "-m", "first commit"], &home);
    common::assert_ok(&new1, "jj new (first commit)");

    // Give the notification a moment to propagate
    std::thread::sleep(Duration::from_millis(500));

    // Agent A: write another file and commit
    std::fs::write(workspace_a.join("goodbye.txt"), b"goodbye world\n").unwrap();
    let new2 = common::run_tandem_in(&workspace_a, &["new", "-m", "second commit"], &home);
    common::assert_ok(&new2, "jj new (second commit)");

    // Give the notification a moment to propagate
    std::thread::sleep(Duration::from_millis(500));

    // Kill the watch process and collect its output
    let _ = watch_proc.kill();
    let output = watch_proc
        .wait_with_output()
        .expect("wait for watch process");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    eprintln!("watch stdout:\n{stdout}");
    eprintln!("watch stderr:\n{stderr}");

    // Parse notification lines
    let notifications: Vec<&str> = stdout
        .lines()
        .filter(|l| l.starts_with("version="))
        .collect();

    // We expect at least 2 notifications (one per commit).
    // The init itself may produce notifications too, but we should see
    // at least 2 from our explicit commits.
    assert!(
        notifications.len() >= 2,
        "expected at least 2 notifications, got {}: {:?}\nstdout:\n{stdout}\nstderr:\n{stderr}",
        notifications.len(),
        notifications,
    );

    // Verify notification format: version=<N> heads=<hex,...>
    for line in &notifications {
        assert!(
            line.starts_with("version="),
            "notification should start with version=: {line}"
        );
        assert!(
            line.contains(" heads="),
            "notification should contain heads=: {line}"
        );
    }

    // Verify versions are monotonically increasing
    let versions: Vec<u64> = notifications
        .iter()
        .filter_map(|l| {
            l.strip_prefix("version=")
                .and_then(|rest| rest.split_whitespace().next())
                .and_then(|v| v.parse().ok())
        })
        .collect();

    for window in versions.windows(2) {
        assert!(
            window[1] > window[0],
            "versions should be monotonically increasing: {:?}",
            versions
        );
    }

    // Clean up server
    let _ = server.kill();
    let _ = server.wait();
}

#[test]
fn slice5_watch_catches_up_on_existing_heads() {
    // Test that a watcher connecting after some commits already happened
    // gets a catch-up notification with the current state.
    let tmp = TempDir::new().unwrap();
    let home = common::isolated_home(tmp.path());
    let server_repo = tmp.path().join("server-repo");
    std::fs::create_dir_all(&server_repo).unwrap();
    let workspace_a = tmp.path().join("workspace-a");
    std::fs::create_dir_all(&workspace_a).unwrap();

    let addr = common::free_addr();
    let mut server = common::spawn_server(&server_repo, &addr);
    common::wait_for_server(&addr, &mut server);

    // Initialize workspace and make a commit BEFORE starting watch
    let init = common::run_tandem_in(&workspace_a, &["init", "--server", &addr, "."], &home);
    common::assert_ok(&init, "tandem init");

    std::fs::write(workspace_a.join("before.txt"), b"before watch\n").unwrap();
    let new1 = common::run_tandem_in(&workspace_a, &["new", "-m", "before watch"], &home);
    common::assert_ok(&new1, "jj new (before watch)");

    // Now start watching — should get a catch-up notification
    let mut watch_proc = Command::new(common::tandem_bin())
        .args(["watch", "--server", &addr])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn tandem watch");

    // Give enough time for catch-up notification
    std::thread::sleep(Duration::from_millis(1000));

    let _ = watch_proc.kill();
    let output = watch_proc
        .wait_with_output()
        .expect("wait for watch process");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    eprintln!("catch-up stdout:\n{stdout}");
    eprintln!("catch-up stderr:\n{stderr}");

    let notifications: Vec<&str> = stdout
        .lines()
        .filter(|l| l.starts_with("version="))
        .collect();

    // Should have at least 1 catch-up notification
    assert!(
        !notifications.is_empty(),
        "expected at least 1 catch-up notification, got none\nstdout:\n{stdout}\nstderr:\n{stderr}",
    );

    // The catch-up notification should have heads (non-empty)
    let first = notifications[0];
    let heads_part = first
        .split("heads=")
        .nth(1)
        .expect("heads= in notification");
    assert!(
        !heads_part.is_empty(),
        "catch-up notification should have non-empty heads"
    );

    let _ = server.kill();
    let _ = server.wait();
}
