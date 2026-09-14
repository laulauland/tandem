#![allow(dead_code)]

pub mod bucket_harness;
pub mod lines;
pub use jj_tandem_test_support::readiness;
pub mod server_fixture;
pub mod workspace;

pub use server_fixture::ServerFixture;

/// The readiness handshake, shared verbatim with the benches — see
/// `readiness.rs` for why it is a handshake and not a bare connect.
pub use readiness::{answers_the_tandem_protocol, server_is_answering};

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

/// The jj configuration every test runs under, shared with `tests/support` so
/// that the two suites cannot drift into different identities.
pub use jj_tandem_test_support::JJ_TEST_CONFIG;

pub fn tandem_bin() -> &'static str {
    env!("CARGO_BIN_EXE_tandem")
}

/// Ports this process has already handed out.
///
/// Asking the kernel for port 0 and then dropping the listener leaves a window
/// where the port is free again. That was survivable when every test file was
/// its own process; the whole integration suite is now one process with dozens
/// of threads, so the kernel will happily hand the same port to two of them
/// inside that window. Remembering what was handed out closes the half of the
/// race this process controls.
static HANDED_OUT_PORTS: std::sync::Mutex<Option<std::collections::HashSet<u16>>> =
    std::sync::Mutex::new(None);

pub fn free_addr() -> String {
    for _ in 0..64 {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind random port");
        let port = listener.local_addr().unwrap().port();
        drop(listener);

        let mut taken = HANDED_OUT_PORTS.lock().expect("port registry");
        if taken.get_or_insert_with(Default::default).insert(port) {
            return format!("127.0.0.1:{port}");
        }
    }
    panic!("could not find a port this process has not already used");
}

/// Create a temporary HOME directory for test isolation.
/// jj writes to ~/.config/jj/repos/ — this prevents test pollution.
/// Returns the path; caller should keep TempDir alive.
pub fn isolated_home(tmp: &Path) -> PathBuf {
    let home = tmp.join("fake-home");
    std::fs::create_dir_all(&home).expect("create fake home");
    home
}

#[cfg(unix)]
pub fn write_test_executable(path: &Path, contents: &str) {
    use std::os::unix::fs::PermissionsExt as _;

    std::fs::write(path, contents).expect("write test executable");
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
        .expect("make test executable");
}

/// Apply test isolation env vars to a Command.
/// Sets HOME and XDG_CONFIG_HOME to a temp dir so jj doesn't
/// pollute the real ~/.config/jj/repos/ registry.
///
/// `TANDEM_CACHE_DIR` is pinned for the same reason, one level deeper: the
/// client cache falls back through `XDG_CACHE_HOME` and then `HOME`, and a
/// developer with `XDG_CACHE_HOME` set would otherwise have every test in the
/// suite writing into their real cache — and reading each other's entries out
/// of it. A test that wants a cache shared between two workspaces overrides
/// this with an explicit value.
pub fn isolate_env(cmd: &mut Command, home: &Path) {
    cmd.env("HOME", home);
    cmd.env("XDG_CONFIG_HOME", home.join(".config"));
    cmd.env("TANDEM_CACHE_DIR", home.join(".cache").join("tandem"));
    // Write a minimal jj config if not present
    let config_dir = home.join(".config").join("jj");
    if !config_dir.exists() {
        std::fs::create_dir_all(&config_dir).ok();
        std::fs::write(config_dir.join("config.toml"), JJ_TEST_CONFIG).ok();
    }
}

pub fn wait_for_server(addr: &str, child: &mut Child, token: Option<&str>) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if server_is_answering(addr, token) {
            return;
        }
        // A server that has already exited is never going to answer, and the
        // reason it exited is worth more than a timeout ten seconds later.
        // Losing the race for the port is the likeliest one.
        if let Ok(Some(status)) = child.try_wait() {
            panic!("the server for {addr} exited before it answered ({status})");
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

/// The admin token `tandem up` printed.
///
/// The daemon generates one when the operator names none, and prints it —
/// which is the only way a test (or a person) can then run `tandem init`.
pub fn admin_token_from_up(output: &Output) -> String {
    let text = stdout_str(output);
    text.lines()
        .find_map(|line| line.strip_prefix("admin token: "))
        .map(|token| token.trim().to_string())
        .unwrap_or_else(|| panic!("`tandem up` printed no admin token\noutput:\n{text}"))
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

/// One `key=value` field out of a line of tool output, when the line has it.
///
/// The commands under test report themselves in whitespace-separated
/// `key=value` fields — `published op=…`, `clone … origin=…` — so this is the
/// one parser for all of them.
pub fn field_in(line: &str, key: &str) -> Option<String> {
    line.split_whitespace()
        .find_map(|field| field.strip_prefix(key))
        .map(str::to_string)
}

/// The same, on a line that is required to carry the field.
pub fn field(line: &str, key: &str) -> String {
    field_in(line, key).unwrap_or_else(|| panic!("no {key} in: {line}"))
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
    spawn_server_with_args_env_and_log(repo, addr, extra_args, env, home, None)
}

/// Spawn a server with stderr available as a line stream.
///
/// A few process tests need to wait for a server's own startup event rather
/// than probing in a sleep loop. The stream is drained on a background thread
/// by `Lines`, so the child cannot block on a full stderr pipe.
pub fn spawn_server_with_args_and_env_with_lines(
    repo: &Path,
    addr: &str,
    extra_args: &[&str],
    env: &[(&str, &str)],
    home: &Path,
) -> (Child, lines::Lines) {
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
    cmd.stdout(Stdio::null()).stderr(Stdio::piped());
    let mut child = cmd.spawn().expect("spawn tandem serve");
    let stderr = child.stderr.take().expect("server stderr");
    (child, lines::Lines::from(stderr))
}

/// Same, but with the server's log written to a file the test can read.
///
/// Used by tests that assert on what the server did rather than only on what
/// it left behind — how many bucket writes a publish made, for one.
pub fn spawn_server_with_args_env_and_log(
    repo: &Path,
    addr: &str,
    extra_args: &[&str],
    env: &[(&str, &str)],
    home: &Path,
    log_path: Option<&Path>,
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
    cmd.stdout(Stdio::null());
    match log_path {
        Some(path) => {
            let file = std::fs::File::create(path).expect("create server log file");
            cmd.stderr(Stdio::from(file));
        }
        None => {
            cmd.stderr(Stdio::null());
        }
    }
    cmd.spawn().expect("spawn tandem serve")
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

/// Wait for a tandem server to answer at an address (no child process to
/// manage). Readiness is the same handshake `wait_for_server` uses, and for
/// the same reason — see `answers_the_tandem_protocol`.
pub fn wait_for_addr(addr: &str, timeout: Duration, token: Option<&str>) {
    let deadline = Instant::now() + timeout;
    loop {
        if server_is_answering(addr, token) {
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

// ─── HTTP API helpers ─────────────────────────────────────────────────────────

/// A blocking HTTP client aimed at a tandem server under test.
///
/// `no_proxy` matters: a developer machine with a system proxy set would
/// otherwise route a request for 127.0.0.1 through it.
pub fn http_client() -> reqwest::blocking::Client {
    reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(30))
        .no_proxy()
        .build()
        .expect("build test HTTP client")
}

pub fn api_url(addr: &str, path: &str) -> String {
    format!("http://{addr}{path}")
}

/// `GET` one of the server's endpoints and insist it answered.
///
/// Every endpoint wants a bearer, so the token is an argument rather than
/// something a caller can forget.
pub fn api_get(addr: &str, token: &str, path: &str) -> reqwest::blocking::Response {
    let response = http_client()
        .get(api_url(addr, path))
        .bearer_auth(token)
        .send()
        .unwrap_or_else(|err| panic!("GET {path} on {addr}: {err}"));
    assert!(
        response.status().is_success(),
        "GET {path} answered HTTP {}",
        response.status().as_u16()
    );
    response
}
