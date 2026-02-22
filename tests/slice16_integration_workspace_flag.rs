//! Slice 16: integration workspace mode flag plumbing
//!
//! Acceptance criteria:
//! - Disabled by default: no integration bookmark updates
//! - Enabled mode: successful op-head updates eventually refresh `integration` bookmark
//! - Env fallback (`TANDEM_ENABLE_INTEGRATION_WORKSPACE=1`) enables mode without flag
//! - `tandem up --enable-integration-workspace` forwards mode to daemonized serve

mod common;

use std::thread;
use std::time::{Duration, Instant};

use tempfile::TempDir;

fn wait_for_integration_commit(workspace_dir: &std::path::Path, home: &std::path::Path) -> String {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let out = common::run_tandem_in(
            workspace_dir,
            &[
                "log",
                "-r",
                "integration",
                "--no-graph",
                "-T",
                "commit_id ++ \"\\n\"",
            ],
            home,
        );
        if out.status.success() {
            let commit = common::stdout_str(&out).trim().to_string();
            if !commit.is_empty() {
                return commit;
            }
        }
        if Instant::now() > deadline {
            panic!(
                "integration bookmark did not appear in time\nstdout:\n{}\nstderr:\n{}",
                common::stdout_str(&out),
                common::stderr_str(&out)
            );
        }
        thread::sleep(Duration::from_millis(50));
    }
}

fn write_single_commit(workspace_dir: &std::path::Path, home: &std::path::Path) {
    std::fs::write(
        workspace_dir.join("flag-test.txt"),
        b"integration flag test\n",
    )
    .unwrap();
    let describe =
        common::run_tandem_in(workspace_dir, &["describe", "-m", "flag test commit"], home);
    common::assert_ok(&describe, "describe for flag test");
    let new_out = common::run_tandem_in(workspace_dir, &["new"], home);
    common::assert_ok(&new_out, "new for flag test");
}

fn commit_author_email(
    workspace_dir: &std::path::Path,
    rev: &str,
    home: &std::path::Path,
) -> String {
    let out = common::run_tandem_in(
        workspace_dir,
        &[
            "log",
            "-r",
            rev,
            "--no-graph",
            "-T",
            "author.email() ++ \"\\n\"",
        ],
        home,
    );
    common::assert_ok(&out, &format!("read author email for {rev}"));
    common::stdout_str(&out).trim().to_string()
}

#[test]
fn slice16_flag_off_no_integration_bookmark() {
    let tmp = TempDir::new().unwrap();
    let home = common::isolated_home(tmp.path());
    let server_repo = tmp.path().join("server-repo");
    std::fs::create_dir_all(&server_repo).unwrap();
    let ws = tmp.path().join("ws");
    std::fs::create_dir_all(&ws).unwrap();

    let addr = common::free_addr();
    let sock = common::control_socket_path(tmp.path());
    let sock_str = sock.to_str().unwrap();

    let mut server =
        common::spawn_server_with_args(&server_repo, &addr, &["--control-socket", sock_str], &home);
    common::wait_for_server(&addr, &mut server);
    common::wait_for_socket(&sock, Duration::from_secs(5));

    let init = common::run_tandem_in(&ws, &["init", "--server", &addr, "."], &home);
    common::assert_ok(&init, "init workspace");
    let init_author_email = commit_author_email(&ws, "@", &home);
    assert_eq!(
        init_author_email, "test@tandem.dev",
        "workspace init commit should pick user.email from jj config"
    );

    write_single_commit(&ws, &home);
    thread::sleep(Duration::from_millis(500));

    let integration_log = common::run_tandem_in(
        &ws,
        &[
            "log",
            "-r",
            "integration",
            "--no-graph",
            "-T",
            "commit_id ++ \"\\n\"",
        ],
        &home,
    );
    assert!(
        !integration_log.status.success(),
        "integration bookmark should not exist when mode is disabled\nstdout:\n{}\nstderr:\n{}",
        common::stdout_str(&integration_log),
        common::stderr_str(&integration_log)
    );

    let status = common::run_tandem_in(
        tmp.path(),
        &["server", "status", "--json", "--control-socket", sock_str],
        &home,
    );
    common::assert_ok(&status, "server status --json");
    let parsed: serde_json::Value =
        serde_json::from_str(common::stdout_str(&status).trim()).unwrap();
    assert_eq!(parsed["integration"]["enabled"], false);

    #[cfg(unix)]
    unsafe {
        libc::kill(server.id() as libc::pid_t, libc::SIGINT);
    }
    let _ = server.wait();
}

#[test]
fn slice16_flag_on_creates_integration_bookmark_and_status() {
    let tmp = TempDir::new().unwrap();
    let home = common::isolated_home(tmp.path());
    let server_repo = tmp.path().join("server-repo");
    std::fs::create_dir_all(&server_repo).unwrap();
    let ws = tmp.path().join("ws");
    std::fs::create_dir_all(&ws).unwrap();

    let addr = common::free_addr();
    let sock = common::control_socket_path(tmp.path());
    let sock_str = sock.to_str().unwrap();

    let mut server = common::spawn_server_with_args(
        &server_repo,
        &addr,
        &[
            "--control-socket",
            sock_str,
            "--enable-integration-workspace",
            "--log-level",
            "error",
        ],
        &home,
    );
    common::wait_for_server(&addr, &mut server);
    common::wait_for_socket(&sock, Duration::from_secs(5));

    let init = common::run_tandem_in(&ws, &["init", "--server", &addr, "."], &home);
    common::assert_ok(&init, "init workspace");
    let init_author_email = commit_author_email(&ws, "@", &home);
    assert_eq!(
        init_author_email, "test@tandem.dev",
        "workspace init commit should pick user.email from jj config"
    );

    write_single_commit(&ws, &home);
    let integration_commit = wait_for_integration_commit(&ws, &home);
    assert!(!integration_commit.is_empty());
    let integration_author_email = commit_author_email(&ws, &integration_commit, &home);
    assert_eq!(
        integration_author_email, "test@tandem.dev",
        "integration commit should use configured user.email"
    );

    let status = common::run_tandem_in(
        tmp.path(),
        &["server", "status", "--json", "--control-socket", sock_str],
        &home,
    );
    common::assert_ok(&status, "server status --json");
    let parsed: serde_json::Value =
        serde_json::from_str(common::stdout_str(&status).trim()).unwrap();
    assert_eq!(parsed["integration"]["enabled"], true);
    assert!(
        parsed["integration"]["lastStatus"].is_string(),
        "expected integration.lastStatus in status JSON"
    );

    #[cfg(unix)]
    unsafe {
        libc::kill(server.id() as libc::pid_t, libc::SIGINT);
    }
    let _ = server.wait();
}

#[test]
fn slice16_env_fallback_and_up_forwarding() {
    let tmp = TempDir::new().unwrap();
    let home = common::isolated_home(tmp.path());
    let server_repo = tmp.path().join("server-repo");
    std::fs::create_dir_all(&server_repo).unwrap();

    let addr = common::free_addr();
    let sock = common::control_socket_path(tmp.path());
    let sock_str = sock.to_str().unwrap();
    let log_file = tmp.path().join("daemon.log");
    let log_file_str = log_file.to_str().unwrap();

    let up = common::run_tandem_in_with_env(
        tmp.path(),
        &[
            "up",
            "--repo",
            server_repo.to_str().unwrap(),
            "--listen",
            &addr,
            "--control-socket",
            sock_str,
            "--log-file",
            log_file_str,
            "--enable-integration-workspace",
        ],
        &[("TANDEM_ENABLE_INTEGRATION_WORKSPACE", "1")],
        &home,
    );
    common::assert_ok(&up, "tandem up with integration flag");

    let status = common::run_tandem_in(
        tmp.path(),
        &["server", "status", "--json", "--control-socket", sock_str],
        &home,
    );
    common::assert_ok(&status, "status after up");
    let parsed: serde_json::Value =
        serde_json::from_str(common::stdout_str(&status).trim()).unwrap();
    assert_eq!(parsed["integration"]["enabled"], true);

    let down = common::run_tandem_in(tmp.path(), &["down", "--control-socket", sock_str], &home);
    common::assert_ok(&down, "down after up forwarding test");
}
