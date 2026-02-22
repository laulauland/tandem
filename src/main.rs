//! tandem — jj workspaces over the network.
//!
//! Single binary:
//!   tandem serve --listen <addr> --repo <path>   → server mode
//!   tandem init --server <addr> [path]           → initialize tandem workspace
//!   tandem <jj args>                             → stock jj via CliRunner

#[allow(unused_parens, dead_code)]
mod tandem_capnp {
    include!(concat!(env!("OUT_DIR"), "/tandem_capnp.rs"));
}

mod backend;
mod control;
mod logging;
mod op_heads_store;
mod op_store;
mod proto_convert;
mod rpc;
mod server;
mod watch;

use std::path::Path;
use std::process::ExitCode;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use clap::{CommandFactory, Parser, Subcommand};

// ─── Help text ────────────────────────────────────────────────────────────────

const AFTER_HELP: &str = "\
JJ COMMANDS:
    All standard jj commands work transparently:
      tandem log            Show commit history
      tandem new            Create a new change
      tandem diff           Show changes in a revision
      tandem file show      Print file contents at a revision
      tandem bookmark       Manage bookmarks
      tandem describe       Update change description
      ... and every other jj command

ENVIRONMENT:
    TANDEM_SERVER           Server address (host:port) — used by the tandem
                            backend when connecting to a remote store
    TANDEM_WORKSPACE        Workspace name for `tandem init` when --workspace
                            is not provided
    TANDEM_ENABLE_INTEGRATION_WORKSPACE
                            Set to 1/true to enable server-side integration
                            workspace recompute mode

SETUP:
    # Start a server
    tandem serve --listen 0.0.0.0:13013 --repo /path/to/repo

    # Initialize a workspace backed by the server
    tandem init --server server:13013 my-workspace

    # Use jj normally
    cd my-workspace
    echo 'hello' > hello.txt
    tandem new -m 'add hello'
    tandem log";

const SERVE_AFTER_HELP: &str = "\
EXAMPLES:
    tandem serve --listen 0.0.0.0:13013 --repo /srv/project
    tandem serve --listen 127.0.0.1:13013 --repo .";

const INIT_AFTER_HELP: &str = "\
EXAMPLES:
    tandem init --server server:13013 my-workspace
    tandem init --server server:13013 --workspace agent-a .
    TANDEM_SERVER=server:13013 tandem init .";

const SERVER_AFTER_HELP: &str = "\
EXAMPLES:
    tandem server status
    tandem server logs --level debug
    tandem server logs --json";

// ─── CLI definition ───────────────────────────────────────────────────────────

#[derive(Parser)]
#[command(
    name = "tandem",
    about = "tandem — jj workspaces over the network",
    after_help = AFTER_HELP,
    disable_help_subcommand = true
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Start the tandem server (foreground)
    #[command(after_help = SERVE_AFTER_HELP)]
    Serve {
        /// Address to listen on (e.g. 0.0.0.0:13013)
        #[arg(long)]
        listen: String,
        /// Path to the repository directory
        #[arg(long)]
        repo: String,
        /// Log level (trace, debug, info, warn, error)
        #[arg(long, default_value = "info")]
        log_level: String,
        /// Log format (text, json)
        #[arg(long, default_value = "text")]
        log_format: String,
        /// Path to control socket
        #[arg(long)]
        control_socket: Option<String>,
        /// Run as daemon (internal, set by `tandem up`)
        #[arg(long, hide = true)]
        daemon: bool,
        /// Log file path (used in daemon mode)
        #[arg(long)]
        log_file: Option<String>,
        /// Enable server-side integration workspace recompute mode
        #[arg(long)]
        enable_integration_workspace: bool,
    },

    /// Initialize a tandem-backed workspace
    #[command(after_help = INIT_AFTER_HELP)]
    Init {
        /// Server address (host:port)
        #[arg(long, env = "TANDEM_SERVER")]
        server: String,
        /// Workspace name (auto-generated if omitted)
        #[arg(long, env = "TANDEM_WORKSPACE")]
        workspace: Option<String>,
        /// Workspace directory
        #[arg(default_value = ".")]
        path: String,
    },

    /// Stream head change notifications (requires server)
    Watch {
        /// Server address (host:port)
        #[arg(long, env = "TANDEM_SERVER")]
        server: String,
    },

    /// Start tandem server as a background daemon
    Up {
        /// Path to the repository directory
        #[arg(long)]
        repo: String,
        /// Address to listen on (e.g. 0.0.0.0:13013)
        #[arg(long)]
        listen: String,
        /// Log level for the daemon (trace, debug, info, warn, error)
        #[arg(long, default_value = "info")]
        log_level: String,
        /// Daemon log file path
        #[arg(long)]
        log_file: Option<String>,
        /// Path to control socket
        #[arg(long)]
        control_socket: Option<String>,
        /// Enable server-side integration workspace recompute mode
        #[arg(long)]
        enable_integration_workspace: bool,
    },

    /// Stop the tandem daemon
    Down {
        /// Path to control socket
        #[arg(long)]
        control_socket: Option<String>,
    },

    /// Tandem daemon status/log streaming commands
    #[command(after_help = SERVER_AFTER_HELP)]
    Server {
        #[command(subcommand)]
        command: ServerCommands,
    },
}

#[derive(Subcommand)]
enum ServerCommands {
    /// Show tandem daemon status
    Status {
        /// Output as JSON
        #[arg(long)]
        json: bool,
        /// Path to control socket
        #[arg(long)]
        control_socket: Option<String>,
    },

    /// Stream logs from a running tandem daemon
    Logs {
        /// Log level filter (trace, debug, info, warn, error)
        #[arg(long, default_value = "info")]
        level: String,
        /// Output raw JSON log lines
        #[arg(long)]
        json: bool,
        /// Path to control socket
        #[arg(long)]
        control_socket: Option<String>,
    },
}

// ─── Dispatch ─────────────────────────────────────────────────────────────────

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();

    // Route tandem-specific commands through clap.
    // Everything else falls through to jj's CliRunner which does its own
    // argument parsing — this avoids conflicts with jj global flags like
    // --no-pager, --color, -R that appear before the subcommand.
    match args.get(1).map(|s| s.as_str()) {
        None | Some("serve" | "init" | "watch" | "up" | "down" | "server" | "--help" | "-h") => {}
        _ => return run_jj(),
    }

    let cli = Cli::parse();
    match cli.command {
        None => {
            Cli::command().print_help().ok();
            println!();
            ExitCode::SUCCESS
        }
        Some(Commands::Serve {
            listen,
            repo,
            log_level,
            log_format,
            control_socket,
            daemon,
            log_file,
            enable_integration_workspace,
        }) => run_serve(
            &listen,
            &repo,
            &log_level,
            &log_format,
            control_socket.as_deref(),
            daemon,
            log_file.as_deref(),
            enable_integration_workspace,
        ),
        Some(Commands::Init {
            server,
            workspace,
            path,
        }) => {
            let workspace_name = resolve_init_workspace_name(workspace.as_deref());
            run_tandem_init(&server, &workspace_name, &path)
        }
        Some(Commands::Watch { server }) => run_watch(&server),
        Some(Commands::Up {
            repo,
            listen,
            log_level,
            log_file,
            control_socket,
            enable_integration_workspace,
        }) => run_up(
            &repo,
            &listen,
            &log_level,
            log_file.as_deref(),
            control_socket.as_deref(),
            enable_integration_workspace,
        ),
        Some(Commands::Down { control_socket }) => run_down(control_socket.as_deref()),
        Some(Commands::Server { command }) => match command {
            ServerCommands::Status {
                json,
                control_socket,
            } => run_status(json, control_socket.as_deref()),
            ServerCommands::Logs {
                level,
                json,
                control_socket,
            } => run_logs(&level, json, control_socket.as_deref()),
        },
    }
}

// ─── Watch mode ───────────────────────────────────────────────────────────────

fn run_watch(server_addr: &str) -> ExitCode {
    if let Err(err) = watch::run_watch(server_addr) {
        eprintln!("error: {err:#}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

// ─── Server mode ──────────────────────────────────────────────────────────────

fn run_serve(
    listen_addr: &str,
    repo_path: &str,
    log_level: &str,
    log_format: &str,
    control_socket: Option<&str>,
    daemon: bool,
    log_file: Option<&str>,
    enable_integration_workspace_flag: bool,
) -> ExitCode {
    // In daemon mode, stdout/stderr are already redirected to the log file
    // by `run_up` before spawning this process. Nothing extra needed here.

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let local = tokio::task::LocalSet::new();

    let opts = server::ServeOptions {
        listen_addr: listen_addr.to_string(),
        repo_path: repo_path.to_string(),
        log_level: log_level.to_string(),
        log_format: log_format.to_string(),
        control_socket: control_socket.map(|s| s.to_string()),
        daemon,
        log_file: log_file.map(|s| s.to_string()),
        enable_integration_workspace: resolve_integration_workspace_enabled(
            enable_integration_workspace_flag,
        ),
    };

    if let Err(err) = local.block_on(&rt, server::run_serve(opts)) {
        eprintln!("error: {err:#}");
        return ExitCode::FAILURE;
    }

    ExitCode::SUCCESS
}

// ─── Up / Down / Status / Logs ────────────────────────────────────────────────

fn default_control_socket() -> String {
    let dir = std::env::temp_dir().join("tandem");
    std::fs::create_dir_all(&dir).ok();
    dir.join("control.sock").to_string_lossy().to_string()
}

fn resolve_control_socket(explicit: Option<&str>) -> String {
    explicit
        .map(|s| s.to_string())
        .unwrap_or_else(default_control_socket)
}

fn env_flag_enabled(name: &str) -> bool {
    std::env::var(name)
        .ok()
        .map(|v| {
            matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(false)
}

fn resolve_integration_workspace_enabled(flag: bool) -> bool {
    flag || env_flag_enabled("TANDEM_ENABLE_INTEGRATION_WORKSPACE")
}

fn run_up(
    repo: &str,
    listen: &str,
    log_level: &str,
    log_file: Option<&str>,
    control_socket: Option<&str>,
    enable_integration_workspace_flag: bool,
) -> ExitCode {
    let sock_path = resolve_control_socket(control_socket);
    let enable_integration_workspace =
        resolve_integration_workspace_enabled(enable_integration_workspace_flag);

    // Check if already running by trying to connect to control socket
    if let Ok(status) = control::client_status(&sock_path) {
        if status.running {
            eprintln!(
                "tandem is already running (PID {}). Use `tandem down` first.",
                status.pid
            );
            return ExitCode::FAILURE;
        }
    }

    // Determine log file
    let log_file_path = log_file.map(|s| s.to_string()).unwrap_or_else(|| {
        let dir = std::env::temp_dir().join("tandem");
        std::fs::create_dir_all(&dir).ok();
        dir.join("daemon.log").to_string_lossy().to_string()
    });

    // Spawn tandem serve --daemon
    let exe = match std::env::current_exe() {
        Ok(e) => e,
        Err(e) => {
            eprintln!("error: cannot determine executable path: {e}");
            return ExitCode::FAILURE;
        }
    };
    let mut cmd = std::process::Command::new(exe);
    cmd.args([
        "serve",
        "--listen",
        listen,
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
    if enable_integration_workspace {
        cmd.arg("--enable-integration-workspace");
    }

    // Redirect stdout/stderr to log file for daemon
    let log_file_handle = match std::fs::File::create(&log_file_path) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("error: cannot create log file {log_file_path}: {e}");
            return ExitCode::FAILURE;
        }
    };
    let stderr_file = match log_file_handle.try_clone() {
        Ok(f) => f,
        Err(e) => {
            eprintln!("error: cannot clone log file handle: {e}");
            return ExitCode::FAILURE;
        }
    };
    cmd.stdout(std::process::Stdio::from(log_file_handle));
    cmd.stderr(std::process::Stdio::from(stderr_file));
    cmd.stdin(std::process::Stdio::null());

    // Inherit HOME/XDG env from current process for isolation in tests
    let child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: failed to start daemon: {e}");
            return ExitCode::FAILURE;
        }
    };

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
                        println!("tandem running, PID {pid}");
                        return ExitCode::SUCCESS;
                    }
                }
            }
        }
        if std::time::Instant::now() > deadline {
            eprintln!("error: daemon failed to start within timeout");
            return ExitCode::FAILURE;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

fn run_down(control_socket: Option<&str>) -> ExitCode {
    let sock_path = resolve_control_socket(control_socket);

    // Try to get status first
    let status = match control::client_status(&sock_path) {
        Ok(s) => s,
        Err(_) => {
            eprintln!("tandem is not running");
            return ExitCode::FAILURE;
        }
    };

    if !status.running {
        eprintln!("tandem is not running");
        return ExitCode::FAILURE;
    }

    let pid = status.pid;

    // Send shutdown
    if let Err(e) = control::client_shutdown(&sock_path) {
        eprintln!("error: shutdown request failed: {e}");
        return ExitCode::FAILURE;
    }

    // Wait for process to exit
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        // Check if process is still alive
        #[cfg(unix)]
        {
            let alive = unsafe { libc::kill(pid as libc::pid_t, 0) } == 0;
            if !alive {
                println!("tandem stopped");
                return ExitCode::SUCCESS;
            }
        }
        #[cfg(not(unix))]
        {
            println!("tandem stopped");
            return ExitCode::SUCCESS;
        }
        if std::time::Instant::now() > deadline {
            eprintln!("warning: daemon did not exit within timeout");
            return ExitCode::FAILURE;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

fn run_status(json: bool, control_socket: Option<&str>) -> ExitCode {
    let sock_path = resolve_control_socket(control_socket);

    match control::client_status(&sock_path) {
        Ok(status) if status.running => {
            if json {
                println!("{}", serde_json::to_string_pretty(&status).unwrap());
            } else {
                println!("tandem is running");
                println!("  PID:      {}", status.pid);
                let uptime = status.uptime_secs;
                if uptime >= 3600 {
                    println!("  Uptime:   {}h {}m", uptime / 3600, (uptime % 3600) / 60);
                } else if uptime >= 60 {
                    println!("  Uptime:   {}m {}s", uptime / 60, uptime % 60);
                } else {
                    println!("  Uptime:   {}s", uptime);
                }
                println!("  Repo:     {}", status.repo);
                println!("  Listen:   {}", status.listen);
                println!("  Version:  {}", status.version);
                println!(
                    "  Integration workspace: {}",
                    if status.integration.enabled {
                        "enabled"
                    } else {
                        "disabled"
                    }
                );
                if status.integration.enabled {
                    println!("  Integration status: {}", status.integration.last_status);
                    if let Some(commit) = status.integration.last_integration_commit.as_deref() {
                        println!("  Integration commit: {commit}");
                    }
                    if let Some(error) = status.integration.last_error.as_deref() {
                        println!("  Integration error:  {error}");
                    }
                }
            }
            ExitCode::SUCCESS
        }
        _ => {
            if json {
                println!("{{\"running\":false}}");
            } else {
                eprintln!("tandem is not running");
            }
            ExitCode::FAILURE
        }
    }
}

fn run_logs(level: &str, json: bool, control_socket: Option<&str>) -> ExitCode {
    let sock_path = resolve_control_socket(control_socket);

    if control::client_status(&sock_path).is_err() {
        eprintln!("no tandem daemon running. Start one with `tandem up`.");
        return ExitCode::FAILURE;
    }

    if let Err(e) = control::client_logs(&sock_path, level, json) {
        // Connection closed = server shut down, not an error
        let msg = format!("{e}");
        if msg.contains("broken pipe")
            || msg.contains("connection reset")
            || msg.contains("end of file")
            || msg.contains("Connection reset")
        {
            return ExitCode::SUCCESS;
        }
        eprintln!("error: {e}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

// ─── Tandem init ──────────────────────────────────────────────────────────────

static WORKSPACE_NAME_COUNTER: AtomicU64 = AtomicU64::new(0);

fn resolve_init_workspace_name(explicit_or_env: Option<&str>) -> String {
    match explicit_or_env {
        Some(name) if !name.trim().is_empty() => name.to_string(),
        _ => generate_workspace_name(),
    }
}

fn generate_workspace_name() -> String {
    let now_ns = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let pid = std::process::id();
    let counter = WORKSPACE_NAME_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("ws-{pid:x}-{now_ns:x}-{counter:x}")
}

fn run_tandem_init(server_addr: &str, workspace_name: &str, workspace_path_str: &str) -> ExitCode {
    let workspace_path = Path::new(workspace_path_str);

    // Create workspace directory if needed
    if let Err(e) = std::fs::create_dir_all(workspace_path) {
        eprintln!("error: cannot create workspace directory: {e}");
        return ExitCode::FAILURE;
    }

    // Convert to absolute path
    let workspace_path = match workspace_path.canonicalize() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("error: cannot resolve workspace path: {e}");
            return ExitCode::FAILURE;
        }
    };

    // Use jj-lib's workspace init with our custom factories
    let config = jj_lib::config::StackedConfig::with_defaults();
    let settings = match jj_lib::settings::UserSettings::from_config(config) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: cannot create settings: {e}");
            return ExitCode::FAILURE;
        }
    };

    let signer = match jj_lib::signing::Signer::from_settings(&settings) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: cannot create signer: {e}");
            return ExitCode::FAILURE;
        }
    };

    let server_addr_owned = server_addr.to_string();
    let sa1 = server_addr_owned.clone();
    let sa2 = server_addr_owned.clone();
    let sa3 = server_addr_owned.clone();
    let workspace_name_owned = workspace_name.to_string();
    let wn1 = workspace_name_owned.clone();

    let backend_init: &dyn Fn(
        &jj_lib::settings::UserSettings,
        &Path,
    ) -> Result<
        Box<dyn jj_lib::backend::Backend>,
        jj_lib::backend::BackendInitError,
    > = &|_settings, store_path| Ok(Box::new(backend::TandemBackend::init(store_path, &sa1)?));

    let op_store_init: &dyn Fn(
        &jj_lib::settings::UserSettings,
        &Path,
        jj_lib::op_store::RootOperationData,
    ) -> Result<
        Box<dyn jj_lib::op_store::OpStore>,
        jj_lib::backend::BackendInitError,
    > = &|_settings, store_path, root_data| {
        Ok(Box::new(op_store::TandemOpStore::init(
            store_path, &sa2, root_data,
        )?))
    };

    let op_heads_init: &dyn Fn(
        &jj_lib::settings::UserSettings,
        &Path,
    ) -> Result<
        Box<dyn jj_lib::op_heads_store::OpHeadsStore>,
        jj_lib::backend::BackendInitError,
    > = &|_settings, store_path| {
        Ok(Box::new(op_heads_store::TandemOpHeadsStore::init(
            store_path, &sa3, &wn1,
        )?))
    };

    match jj_lib::workspace::Workspace::init_with_factories(
        &settings,
        &workspace_path,
        backend_init,
        signer,
        op_store_init,
        op_heads_init,
        jj_lib::repo::ReadonlyRepo::default_index_store_initializer(),
        jj_lib::repo::ReadonlyRepo::default_submodule_store_initializer(),
        &*jj_lib::workspace::default_working_copy_factory(),
        jj_lib::ref_name::WorkspaceNameBuf::from(workspace_name.to_string()),
    ) {
        Ok(_) => {
            eprintln!(
                "Initialized tandem workspace '{}' at {} (server: {})",
                workspace_name,
                workspace_path.display(),
                server_addr
            );
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("error: workspace init failed: {e}");
            ExitCode::FAILURE
        }
    }
}

// ─── jj CLI mode ──────────────────────────────────────────────────────────────

fn run_jj() -> ExitCode {
    use jj_cli::cli_util::CliRunner;

    CliRunner::init()
        .version(env!("CARGO_PKG_VERSION"))
        .add_store_factories(tandem_factories())
        .run()
        .into()
}

/// Register tandem backend/opstore/opheadsstore factories so that jj
/// can load repos with store/type = "tandem".
fn tandem_factories() -> jj_lib::repo::StoreFactories {
    let mut factories = jj_lib::repo::StoreFactories::empty();

    factories.add_backend(
        "tandem",
        Box::new(|settings, store_path| {
            Ok(Box::new(backend::TandemBackend::load(
                settings, store_path,
            )?))
        }),
    );

    factories.add_op_store(
        "tandem_op_store",
        Box::new(|settings, store_path, root_data| {
            Ok(Box::new(op_store::TandemOpStore::load(
                settings, store_path, root_data,
            )?))
        }),
    );

    factories.add_op_heads_store(
        "tandem_op_heads_store",
        Box::new(|settings, store_path| {
            Ok(Box::new(op_heads_store::TandemOpHeadsStore::load(
                settings, store_path,
            )?))
        }),
    );

    factories
}
