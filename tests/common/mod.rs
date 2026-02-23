#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

pub fn tandem_bin() -> &'static str {
    env!("CARGO_BIN_EXE_tandem")
}

pub fn free_addr() -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind random port");
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    format!("127.0.0.1:{port}")
}

/// Create a temporary HOME directory for test isolation.
/// jj writes to ~/.config/jj/repos/ — this prevents test pollution.
/// Returns the path; caller should keep TempDir alive.
pub fn isolated_home(tmp: &Path) -> PathBuf {
    let home = tmp.join("fake-home");
    std::fs::create_dir_all(&home).expect("create fake home");
    home
}

/// Apply test isolation env vars to a Command.
/// Sets HOME and XDG_CONFIG_HOME to a temp dir so jj doesn't
/// pollute the real ~/.config/jj/repos/ registry.
pub fn isolate_env(cmd: &mut Command, home: &Path) {
    cmd.env("HOME", home);
    cmd.env("XDG_CONFIG_HOME", home.join(".config"));
    // Write a minimal jj config if not present
    let config_dir = home.join(".config").join("jj");
    if !config_dir.exists() {
        std::fs::create_dir_all(&config_dir).ok();
        std::fs::write(
            config_dir.join("config.toml"),
            "user.name = \"Test User\"\nuser.email = \"test@tandem.dev\"\n\
             [fsmonitor]\nbackend = \"none\"\n",
        )
        .ok();
    }
}

pub fn spawn_server(repo: &Path, addr: &str) -> Child {
    Command::new(tandem_bin())
        .args([
            "serve",
            "--listen",
            addr,
            "--repo",
            repo.to_str().unwrap(),
            "--log-level",
            "warn",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn tandem serve")
}

pub fn wait_for_server(addr: &str, child: &mut Child) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if std::net::TcpStream::connect(addr).is_ok() {
            return;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("server failed to start before deadline");
        }
        thread::sleep(Duration::from_millis(50));
    }
}

/// Run a tandem command in a given directory with HOME isolation.
pub fn run_tandem_in(dir: &Path, args: &[&str], home: &Path) -> Output {
    run_tandem_in_with_env(dir, args, &[], home)
}

/// Run a tandem command in a given directory with HOME isolation and extra env vars.
pub fn run_tandem_in_with_env(
    dir: &Path,
    args: &[&str],
    env: &[(&str, &str)],
    home: &Path,
) -> Output {
    let mut cmd = Command::new(tandem_bin());
    cmd.current_dir(dir);
    isolate_env(&mut cmd, home);
    for arg in args {
        cmd.arg(arg);
    }
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.output().expect("run tandem")
}

pub fn assert_ok(output: &Output, context: &str) {
    assert!(
        output.status.success(),
        "{context} failed (status {:?})\nstdout:\n{}\nstderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

pub fn stdout_str(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).to_string()
}

pub fn stderr_str(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).to_string()
}

/// Run a jj command in a given repo directory. Returns stdout on success.
pub fn run_jj_in(repo: &Path, args: &[&str]) -> Output {
    let mut cmd = Command::new("jj");
    cmd.arg("--repository").arg(repo);
    for arg in args {
        cmd.arg(arg);
    }
    cmd.output().expect("run jj command")
}

/// Spawn a server with extra args and HOME isolation.
pub fn spawn_server_with_args(repo: &Path, addr: &str, extra_args: &[&str], home: &Path) -> Child {
    spawn_server_with_args_and_env(repo, addr, extra_args, &[], home)
}

pub fn spawn_server_with_args_and_env(
    repo: &Path,
    addr: &str,
    extra_args: &[&str],
    env: &[(&str, &str)],
    home: &Path,
) -> Child {
    let mut cmd = Command::new(tandem_bin());
    cmd.args(["serve", "--listen", addr, "--repo", repo.to_str().unwrap()]);
    let has_explicit_log_level = extra_args.iter().copied().any(|arg| arg == "--log-level");
    if !has_explicit_log_level {
        cmd.args(["--log-level", "warn"]);
    }
    for arg in extra_args {
        cmd.arg(arg);
    }
    isolate_env(&mut cmd, home);
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn tandem serve")
}

/// Generate a unique control socket path inside a temp directory.
pub fn control_socket_path(tmp: &Path) -> PathBuf {
    tmp.join("control.sock")
}

/// Wait for a Unix socket to appear on disk.
#[cfg(unix)]
pub fn wait_for_socket(path: &Path, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    loop {
        if path.exists() {
            // Try connecting to verify it's listening
            if std::os::unix::net::UnixStream::connect(path).is_ok() {
                return;
            }
        }
        if Instant::now() > deadline {
            panic!(
                "socket {} did not appear within {:?}",
                path.display(),
                timeout
            );
        }
        thread::sleep(Duration::from_millis(50));
    }
}

/// Wait for a TCP address to become connectable (no child process to manage).
pub fn wait_for_addr(addr: &str, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    loop {
        if std::net::TcpStream::connect(addr).is_ok() {
            return;
        }
        if Instant::now() > deadline {
            panic!("address {addr} not connectable within {timeout:?}");
        }
        thread::sleep(Duration::from_millis(50));
    }
}

/// Send a JSON request to the control socket and read the response line.
#[cfg(unix)]
pub fn control_request(socket_path: &Path, request: &str) -> String {
    use std::io::{BufRead, BufReader, Write};
    let mut stream =
        std::os::unix::net::UnixStream::connect(socket_path).expect("connect to control socket");
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
    stream.write_all(request.as_bytes()).expect("write request");
    stream.write_all(b"\n").expect("write newline");
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).expect("read response");
    line
}

/// Run a raw git command (bypassing jj's git wrapper).
pub fn run_git(args: &[&str]) -> Output {
    Command::new("/usr/bin/git")
        .args(args)
        .output()
        .expect("run git command")
}

/// Run a raw git command in a given directory.
pub fn run_git_in(dir: &Path, args: &[&str]) -> Output {
    Command::new("/usr/bin/git")
        .current_dir(dir)
        .args(args)
        .output()
        .expect("run git command")
}
