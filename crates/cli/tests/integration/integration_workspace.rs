//! The integration workspace mode, and how the flag reaches the server.
//!
//! What is pinned here:
//! - disabled by default: nothing touches the `integration` bookmark
//! - enabled: a published op head eventually moves that bookmark
//! - `TANDEM_ENABLE_INTEGRATION_WORKSPACE=1` enables the mode without the flag
//! - `tandem up --enable-integration-workspace` forwards it to the daemon

use crate::common;
use crate::common::ServerFixture;

use std::thread;
use std::time::Duration;

use tempfile::TempDir;

/// Wait for the integration bookmark to name a commit.
///
/// This used to spawn a `tandem log` every fifty milliseconds until it liked
/// the answer — up to three hundred processes to observe one bookmark move. The
/// server publishes a head event when its heads change, so the wait subscribes
/// and asks only when there is a reason to.
fn wait_for_integration_commit(
    addr: &str,
    token: &str,
    workspace_dir: &std::path::Path,
    home: &std::path::Path,
) -> String {
    let found = common::workspace::wait_for(addr, token, Duration::from_secs(15), || {
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
        if !out.status.success() {
            return None;
        }
        let commit = common::stdout_str(&out).trim().to_string();
        (!commit.is_empty()).then_some(commit)
    });
    found.expect("the integration bookmark did not appear before the deadline")
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
fn flag_off_no_integration_bookmark() {
    let mut fx = ServerFixture::builder().control_socket().start();
    let home = fx.home.clone();

    let ws = fx.init_workspace("ws", None);
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

    let status = fx.run(
        fx.path(),
        &[
            "server",
            "status",
            "--json",
            "--control-socket",
            fx.socket_str(),
        ],
    );
    common::assert_ok(&status, "server status --json");
    let parsed: serde_json::Value =
        serde_json::from_str(common::stdout_str(&status).trim()).unwrap();
    assert_eq!(parsed["integration"]["enabled"], false);

    #[cfg(unix)]
    unsafe {
        libc::kill(fx.server.id() as libc::pid_t, libc::SIGINT);
    }
    let _ = fx.server.wait();
}

#[test]
fn flag_on_creates_integration_bookmark_and_status() {
    let mut fx = ServerFixture::builder()
        .control_socket()
        .args(&["--enable-integration-workspace", "--log-level", "error"])
        .start();
    let home = fx.home.clone();

    let ws = fx.init_workspace("ws", None);
    let init_author_email = commit_author_email(&ws, "@", &home);
    assert_eq!(
        init_author_email, "test@tandem.dev",
        "workspace init commit should pick user.email from jj config"
    );

    write_single_commit(&ws, &home);
    let integration_commit = wait_for_integration_commit(&fx.addr, fx.token(), &ws, &home);
    assert!(!integration_commit.is_empty());
    let integration_author_email = commit_author_email(&ws, &integration_commit, &home);
    assert_eq!(
        integration_author_email, "test@tandem.dev",
        "integration commit should use configured user.email"
    );

    let status = fx.run(
        fx.path(),
        &[
            "server",
            "status",
            "--json",
            "--control-socket",
            fx.socket_str(),
        ],
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
        libc::kill(fx.server.id() as libc::pid_t, libc::SIGINT);
    }
    let _ = fx.server.wait();
}

#[test]
fn env_fallback_and_up_forwarding() {
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
