//! What a signal does to a running `tandem serve`.
//!
//! What is pinned here:
//! - SIGINT and SIGTERM both exit 0, not 130 and not signal-killed
//! - A second SIGINT stops the process without waiting for the graceful path
//! - `--log-level` and `--log-format` are accepted by `serve`
//! - Work in flight before the signal is not lost by it

use crate::common;

use std::process::{Child, ExitStatus};
use std::time::{Duration, Instant};

use tempfile::TempDir;

/// Wait for a child to exit, killing it and failing the test if it will not.
fn wait_for_exit(child: &mut Child, timeout: Duration, context: &str) -> ExitStatus {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait().expect("poll the server") {
            return status;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("{context}: the server did not exit within {timeout:?}");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[cfg(unix)]
fn signal(child: &Child, sig: libc::c_int) {
    unsafe {
        libc::kill(child.id() as libc::pid_t, sig);
    }
}

/// Every way of asking the server to stop, in one place.
///
/// This used to be five tests that each spawned a server to send it one signal.
/// The signals do not interact, so they do not need five processes — they need
/// one loop and one honest assertion each. The logging flags ride along: a
/// server that would not accept them never reaches the signal.
#[test]
#[cfg(unix)]
fn signals_shut_the_server_down() {
    let tmp = TempDir::new().unwrap();
    let home = common::isolated_home(tmp.path());
    let server_repo = tmp.path().join("server-repo");
    std::fs::create_dir_all(&server_repo).unwrap();

    // Both logging flags on every run: accepting them is part of starting.
    let args = ["--log-level", "debug", "--log-format", "json"];

    for (name, sig) in [("SIGINT", libc::SIGINT), ("SIGTERM", libc::SIGTERM)] {
        let addr = common::free_addr();
        let mut server = common::spawn_server_with_args(&server_repo, &addr, &args, &home);
        common::wait_for_server(&addr, &mut server);

        signal(&server, sig);
        let status = wait_for_exit(&mut server, Duration::from_secs(10), name);
        assert!(
            status.success(),
            "the server should exit 0 on {name}, got {:?}",
            status.code()
        );
    }

    // A second signal is an operator saying "now". It does not have to exit 0 —
    // it has to exit.
    let addr = common::free_addr();
    let mut server = common::spawn_server_with_args(&server_repo, &addr, &args, &home);
    common::wait_for_server(&addr, &mut server);
    signal(&server, libc::SIGINT);
    std::thread::sleep(Duration::from_millis(100));
    signal(&server, libc::SIGINT);
    wait_for_exit(&mut server, Duration::from_secs(5), "double SIGINT");
}

/// A signal must not cost the client what it already did.
///
/// The round trip before the signal is the point: a graceful shutdown that
/// discarded the last publish would still exit 0.
#[test]
#[cfg(unix)]
fn a_signal_does_not_undo_the_work_before_it() {
    let tmp = TempDir::new().unwrap();
    let home = common::isolated_home(tmp.path());
    let server_repo = tmp.path().join("server-repo");
    std::fs::create_dir_all(&server_repo).unwrap();
    let workspace_dir = tmp.path().join("workspace");
    std::fs::create_dir_all(&workspace_dir).unwrap();

    let addr = common::free_addr();
    let mut server = common::spawn_server_with_args(&server_repo, &addr, &[], &home);
    common::wait_for_server(&addr, &mut server);

    let init = common::run_tandem_in(&workspace_dir, &["init", "--server", &addr, "."], &home);
    common::assert_ok(&init, "tandem init");

    std::fs::write(workspace_dir.join("test.txt"), b"shutdown test\n").unwrap();
    let new_out = common::run_tandem_in(&workspace_dir, &["new", "-m", "before shutdown"], &home);
    common::assert_ok(&new_out, "tandem new");

    let cat = common::run_tandem_in(
        &workspace_dir,
        &["file", "show", "-r", "@-", "test.txt"],
        &home,
    );
    common::assert_ok(&cat, "file show");
    assert_eq!(cat.stdout, b"shutdown test\n");

    signal(&server, libc::SIGTERM);
    let status = wait_for_exit(&mut server, Duration::from_secs(10), "SIGTERM after a round trip");
    assert!(
        status.success(),
        "the server should exit 0 after SIGTERM, got {:?}",
        status.code()
    );

    // Restart and ask again: what was acknowledged is still there.
    let mut server = common::spawn_server_with_args(&server_repo, &addr, &[], &home);
    common::wait_for_server(&addr, &mut server);
    let _ = common::run_tandem_in(&workspace_dir, &["workspace", "update-stale"], &home);
    let cat = common::run_tandem_in(
        &workspace_dir,
        &["file", "show", "-r", "@-", "test.txt"],
        &home,
    );
    common::assert_ok(&cat, "file show after the restart");
    assert_eq!(
        cat.stdout, b"shutdown test\n",
        "the shutdown must not have cost the client its last publish"
    );

    signal(&server, libc::SIGTERM);
    wait_for_exit(&mut server, Duration::from_secs(10), "final SIGTERM");
}
