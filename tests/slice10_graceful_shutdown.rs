//! Slice 10: Signal handling and graceful shutdown
//!
//! Acceptance criteria:
//! - `tandem serve` + SIGINT exits 0 (not 130).
//! - In-flight `getObject` call during shutdown completes (not dropped).
//! - `--log-level debug` produces debug output to stderr.
//! - Existing slice 1-7 tests still pass.

mod common;

use std::time::Duration;
use tempfile::TempDir;

/// SIGINT causes clean exit with code 0.
#[test]
fn slice10_sigint_clean_exit() {
    let tmp = TempDir::new().unwrap();
    let home = common::isolated_home(tmp.path());
    let server_repo = tmp.path().join("server-repo");
    std::fs::create_dir_all(&server_repo).unwrap();

    let addr = common::free_addr();
    let mut server = common::spawn_server_with_args(&server_repo, &addr, &[], &home);
    common::wait_for_server(&addr, &mut server);

    // Send SIGINT
    #[cfg(unix)]
    {
        unsafe {
            libc::kill(server.id() as libc::pid_t, libc::SIGINT);
        }
    }

    // Wait for exit (with timeout)
    let start = std::time::Instant::now();
    loop {
        if let Some(status) = server.try_wait().expect("try_wait") {
            // Should exit 0 (not 130 or signal-killed)
            assert!(
                status.success(),
                "server should exit 0 on SIGINT, got {:?}",
                status.code()
            );
            return;
        }
        if start.elapsed() > Duration::from_secs(10) {
            let _ = server.kill();
            let _ = server.wait();
            panic!("server did not exit within 10 seconds of SIGINT");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// SIGTERM causes clean exit with code 0.
#[test]
fn slice10_sigterm_clean_exit() {
    let tmp = TempDir::new().unwrap();
    let home = common::isolated_home(tmp.path());
    let server_repo = tmp.path().join("server-repo");
    std::fs::create_dir_all(&server_repo).unwrap();

    let addr = common::free_addr();
    let mut server = common::spawn_server_with_args(&server_repo, &addr, &[], &home);
    common::wait_for_server(&addr, &mut server);

    // Send SIGTERM
    #[cfg(unix)]
    {
        unsafe {
            libc::kill(server.id() as libc::pid_t, libc::SIGTERM);
        }
    }

    let start = std::time::Instant::now();
    loop {
        if let Some(status) = server.try_wait().expect("try_wait") {
            assert!(
                status.success(),
                "server should exit 0 on SIGTERM, got {:?}",
                status.code()
            );
            return;
        }
        if start.elapsed() > Duration::from_secs(10) {
            let _ = server.kill();
            let _ = server.wait();
            panic!("server did not exit within 10 seconds of SIGTERM");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Second SIGINT causes immediate exit.
#[test]
fn slice10_double_sigint_immediate_exit() {
    let tmp = TempDir::new().unwrap();
    let home = common::isolated_home(tmp.path());
    let server_repo = tmp.path().join("server-repo");
    std::fs::create_dir_all(&server_repo).unwrap();

    let addr = common::free_addr();
    let mut server = common::spawn_server_with_args(&server_repo, &addr, &[], &home);
    common::wait_for_server(&addr, &mut server);

    #[cfg(unix)]
    {
        let pid = server.id() as libc::pid_t;
        unsafe {
            libc::kill(pid, libc::SIGINT);
        }
        // Small delay then second signal
        std::thread::sleep(Duration::from_millis(100));
        unsafe {
            libc::kill(pid, libc::SIGINT);
        }
    }

    // Should exit quickly (within 2s)
    let start = std::time::Instant::now();
    loop {
        if server.try_wait().expect("try_wait").is_some() {
            return; // exited — we don't require code 0 for double-signal
        }
        if start.elapsed() > Duration::from_secs(5) {
            let _ = server.kill();
            let _ = server.wait();
            panic!("server did not exit after double SIGINT");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// --log-level flag is accepted by serve.
#[test]
fn slice10_log_level_flag_accepted() {
    let tmp = TempDir::new().unwrap();
    let home = common::isolated_home(tmp.path());
    let server_repo = tmp.path().join("server-repo");
    std::fs::create_dir_all(&server_repo).unwrap();

    let addr = common::free_addr();
    let mut server =
        common::spawn_server_with_args(&server_repo, &addr, &["--log-level", "debug"], &home);
    common::wait_for_server(&addr, &mut server);

    // Server started successfully with --log-level debug
    // Send SIGINT to stop it
    #[cfg(unix)]
    {
        unsafe {
            libc::kill(server.id() as libc::pid_t, libc::SIGINT);
        }
    }

    let output = server.wait_with_output().expect("wait_with_output");
    let stderr = String::from_utf8_lossy(&output.stderr);
    // Should have started (the "listening on" message proves the flag was accepted)
    assert!(
        stderr.contains("listening on"),
        "server should start with --log-level debug\nstderr: {stderr}"
    );
}

/// --log-format flag is accepted by serve.
#[test]
fn slice10_log_format_flag_accepted() {
    let tmp = TempDir::new().unwrap();
    let home = common::isolated_home(tmp.path());
    let server_repo = tmp.path().join("server-repo");
    std::fs::create_dir_all(&server_repo).unwrap();

    let addr = common::free_addr();
    let mut server =
        common::spawn_server_with_args(&server_repo, &addr, &["--log-format", "json"], &home);
    common::wait_for_server(&addr, &mut server);

    #[cfg(unix)]
    {
        unsafe {
            libc::kill(server.id() as libc::pid_t, libc::SIGINT);
        }
    }

    let output = server.wait_with_output().expect("wait_with_output");
    assert!(
        output.status.success(),
        "server should accept --log-format json"
    );
}

/// Server can still handle a full client round-trip then shutdown cleanly.
#[test]
fn slice10_client_roundtrip_then_shutdown() {
    let tmp = TempDir::new().unwrap();
    let home = common::isolated_home(tmp.path());
    let server_repo = tmp.path().join("server-repo");
    std::fs::create_dir_all(&server_repo).unwrap();
    let workspace_dir = tmp.path().join("workspace");
    std::fs::create_dir_all(&workspace_dir).unwrap();

    let addr = common::free_addr();
    let mut server = common::spawn_server_with_args(&server_repo, &addr, &[], &home);
    common::wait_for_server(&addr, &mut server);

    // Init workspace
    let init = common::run_tandem_in(&workspace_dir, &["init", "--server", &addr, "."], &home);
    common::assert_ok(&init, "tandem init");

    // Write a file, commit
    std::fs::write(workspace_dir.join("test.txt"), b"shutdown test\n").unwrap();
    let new_out = common::run_tandem_in(&workspace_dir, &["new", "-m", "before shutdown"], &home);
    common::assert_ok(&new_out, "tandem new");

    // Verify file round-trips
    let cat = common::run_tandem_in(
        &workspace_dir,
        &["file", "show", "-r", "@-", "test.txt"],
        &home,
    );
    common::assert_ok(&cat, "file show");
    assert_eq!(cat.stdout, b"shutdown test\n");

    // Now signal shutdown
    #[cfg(unix)]
    {
        unsafe {
            libc::kill(server.id() as libc::pid_t, libc::SIGTERM);
        }
    }

    let start = std::time::Instant::now();
    loop {
        if let Some(status) = server.try_wait().expect("try_wait") {
            assert!(
                status.success(),
                "server should exit 0 after SIGTERM, got {:?}",
                status.code()
            );
            return;
        }
        if start.elapsed() > Duration::from_secs(10) {
            let _ = server.kill();
            let _ = server.wait();
            panic!("server did not exit after SIGTERM");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}
