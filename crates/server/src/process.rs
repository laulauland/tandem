use crate::control;

/// Successful startup; the bearer must only be delivered to the operator.
pub struct StartedServer {
    pub listen_addr: String,
    pub pid: u32,
    pub admin_token: String,
}

fn default_control_socket() -> String {
    let dir = std::env::temp_dir().join("tandem");
    std::fs::create_dir_all(&dir).ok();
    dir.join("control.sock").to_string_lossy().to_string()
}

pub fn resolve_control_socket(explicit: Option<&str>) -> String {
    explicit
        .map(|s| s.to_string())
        .unwrap_or_else(default_control_socket)
}

const DEFAULT_UP_HOST: &str = "0.0.0.0";
const DEFAULT_UP_PORT_START: u16 = 13013;
const DEFAULT_UP_PORT_END: u16 = 13063;

fn up_state_dir() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join("tandem").join("up-state");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

fn hash_repo_identity(repo: &str) -> u64 {
    use std::hash::{Hash, Hasher};

    let canonical = std::fs::canonicalize(repo).unwrap_or_else(|_| std::path::PathBuf::from(repo));
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    canonical.to_string_lossy().hash(&mut hasher);
    hasher.finish()
}

fn last_listen_path(repo: &str) -> std::path::PathBuf {
    let key = format!("{:016x}", hash_repo_identity(repo));
    up_state_dir().join(format!("last-listen-{key}.txt"))
}

fn read_last_listen(repo: &str) -> Option<String> {
    let path = last_listen_path(repo);
    std::fs::read_to_string(path)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

fn write_last_listen(repo: &str, listen: &str) {
    let path = last_listen_path(repo);
    let _ = std::fs::write(path, listen);
}

fn can_bind_listen_addr(addr: &str) -> bool {
    std::net::TcpListener::bind(addr).is_ok()
}

fn find_auto_listen_addr(repo: &str) -> Option<String> {
    let span = (DEFAULT_UP_PORT_END - DEFAULT_UP_PORT_START + 1) as usize;
    let start_offset = (hash_repo_identity(repo) as usize) % span;

    for i in 0..span {
        let port = DEFAULT_UP_PORT_START + ((start_offset + i) % span) as u16;
        let candidate = format!("{DEFAULT_UP_HOST}:{port}");
        if can_bind_listen_addr(&candidate) {
            return Some(candidate);
        }
    }
    None
}

fn resolve_up_listen(repo: &str, explicit: Option<&str>) -> Result<String, String> {
    if let Some(addr) = explicit.map(|s| s.trim()).filter(|s| !s.is_empty()) {
        return Ok(addr.to_string());
    }

    if let Some(last) = read_last_listen(repo) {
        if can_bind_listen_addr(&last) {
            return Ok(last);
        }
    }

    find_auto_listen_addr(repo).ok_or_else(|| {
        format!(
            "could not find a free listen address in {DEFAULT_UP_HOST}:{DEFAULT_UP_PORT_START}-{DEFAULT_UP_PORT_END}; pass --listen <addr>"
        )
    })
}

/// Re-exec the calling Tandem binary in `serve --daemon` mode.
///
/// This launch API is for the Tandem executable, not arbitrary embedding
/// processes. Embedders should construct `Server` and use `router` instead.
pub fn start_background(
    repo: &str,
    listen: Option<&str>,
    log_level: &str,
    log_file: Option<&str>,
    control_socket: Option<&str>,
    bucket: Option<&str>,
    admin_token: Option<&str>,
) -> Result<StartedServer, String> {
    let sock_path = resolve_control_socket(control_socket);

    // Check if already running by trying to connect to control socket
    if let Ok(status) = control::client_status(&sock_path) {
        if status.running {
            return Err(format!(
                "tandem is already running (PID {}). Use `tandem down` first.",
                status.pid
            ));
        }
    }

    let listen_addr = resolve_up_listen(repo, listen).map_err(|e| format!("error: {e}"))?;

    // Determine log file
    let log_file_path = log_file.map(|s| s.to_string()).unwrap_or_else(|| {
        let dir = std::env::temp_dir().join("tandem");
        std::fs::create_dir_all(&dir).ok();
        dir.join("daemon.log").to_string_lossy().to_string()
    });

    // Spawn tandem serve --daemon
    let exe = std::env::current_exe()
        .map_err(|e| format!("error: cannot determine executable path: {e}"))?;
    // The daemon needs an admin token, and whoever ran `tandem up` needs to
    // know it — the log the daemon writes is not where a person looks. So the
    // token is decided here, handed to the daemon, and printed below.
    let admin_token = match admin_token {
        Some(given) => given.to_string(),
        None => crate::auth::generate_admin_token(),
    };

    let mut cmd = std::process::Command::new(exe);
    cmd.args([
        "serve",
        "--listen",
        &listen_addr,
        "--repo",
        repo,
        "--log-level",
        log_level,
        "--control-socket",
        &sock_path,
        "--log-file",
        &log_file_path,
        "--daemon",
    ]);
    if let Some(bucket) = bucket {
        cmd.args(["--bucket", bucket]);
    }
    // Through the environment, not the argument list: a command line is
    // readable by every process on the machine, and this is a secret.
    cmd.env("TANDEM_ADMIN_TOKEN", &admin_token);

    // Redirect stdout/stderr to log file for daemon
    let log_file_handle = std::fs::File::create(&log_file_path)
        .map_err(|e| format!("error: cannot create log file {log_file_path}: {e}"))?;
    let stderr_file = log_file_handle
        .try_clone()
        .map_err(|e| format!("error: cannot clone log file handle: {e}"))?;
    cmd.stdout(std::process::Stdio::from(log_file_handle));
    cmd.stderr(std::process::Stdio::from(stderr_file));
    cmd.stdin(std::process::Stdio::null());

    // Inherit HOME/XDG env from current process for isolation in tests
    let child = cmd
        .spawn()
        .map_err(|e| format!("error: failed to start daemon: {e}"))?;

    let pid = child.id();

    // Wait for control socket to become available
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let sock = std::path::Path::new(&sock_path);
        if sock.exists() {
            #[cfg(unix)]
            if std::os::unix::net::UnixStream::connect(sock).is_ok() {
                // Verify healthy via status
                if let Ok(status) = control::client_status(&sock_path) {
                    if status.running {
                        write_last_listen(repo, &listen_addr);
                        return Ok(StartedServer {
                            listen_addr,
                            pid,
                            admin_token,
                        });
                    }
                }
            }
        }
        if std::time::Instant::now() > deadline {
            return Err("error: daemon failed to start within timeout".into());
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

pub fn stop_background(control_socket: Option<&str>) -> Result<(), String> {
    let sock_path = resolve_control_socket(control_socket);

    // Try to get status first
    let status =
        control::client_status(&sock_path).map_err(|_| "tandem is not running".to_string())?;

    if !status.running {
        return Err("tandem is not running".into());
    }

    let pid = status.pid;

    // Send shutdown
    control::client_shutdown(&sock_path)
        .map_err(|e| format!("error: shutdown request failed: {e}"))?;

    // Wait for process to exit
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        // Check if process is still alive
        #[cfg(unix)]
        {
            let alive = unsafe { libc::kill(pid as libc::pid_t, 0) } == 0;
            if !alive {
                return Ok(());
            }
        }
        #[cfg(not(unix))]
        {
            return Ok(());
        }
        if std::time::Instant::now() > deadline {
            return Err("warning: daemon did not exit within timeout".into());
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}
