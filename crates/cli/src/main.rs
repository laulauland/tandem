//! tandem — jj workspaces over the network.
//!
//! Single binary:
//!   tandem serve --listen <addr> --repo <path>   → server mode
//!   tandem clone <addr> <dir> --workspace <name> → create or attach a workspace
//!   tandem daemon [dir]                          → watch, snapshot, publish
//!   tandem <jj args>                             → stock jj via CliRunner

use jj_tandem_server::{self as server, control, resolve_control_socket};
use jj_tandem_workspace::{
    clone_tandem_workspace, init_tandem_workspace, is_running, read_status, resolve_debounce,
    run_daemon, run_watch as watch_heads, user_settings_from_environment, DaemonOptions,
    WorkspaceOrigin,
};

use std::path::Path;
use std::process::ExitCode;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use clap::{CommandFactory, Parser, Subcommand};

mod address;
mod credentials;
use address::CloneTarget;

// ─── Help text ────────────────────────────────────────────────────────────────

const AFTER_HELP: &str = "\
JJ COMMANDS:
    All standard jj commands work transparently:
      td log            Show commit history
      td new            Create a new change
      td diff           Show changes in a revision
      td file show      Print file contents at a revision
      td bookmark       Manage bookmarks
      td describe       Update change description
      ... and every other jj command

ENVIRONMENT:
    TANDEM_SERVER           Server address (host:port) — used by the tandem
                            backend when connecting to a remote store
    TANDEM_WORKSPACE        Workspace name for `td init` when --workspace
                            is not provided
    TANDEM_ADMIN_TOKEN      The token `td serve` and `td up` accept as
                            the administrator's. It is what mints workspace
                            tokens. Required for `td serve`. If unset,
                            `td up` generates and prints it; treat that
                            terminal output as secret
    TANDEM_TOKEN            The token `td init`, `td clone`, and
                            `td watch` present.
                            Either the admin token or one already scoped to the
                            workspace. Hosted clone also reads the exact host
                            from $XDG_CONFIG_HOME/td/credentials, else
                            $HOME/.config/td/credentials
    TANDEM_LISTEN           Listen address for `td up` (host:port).
                            If unset, tandem auto-selects a free port
                            in 0.0.0.0:13013-13063
    TANDEM_DEBOUNCE_MS      How long `td daemon` collects file changes
                            before it snapshots. It is a durability window:
                            work done inside one is work a dying machine takes
                            with it. Defaults to 1000
    TANDEM_CACHE_DIR        Where the client keeps its cache of objects,
                            operations and views. Everything in it is named by
                            a hash of its contents, so the directory can be
                            shared between workspaces and baked into an image.
                            Defaults to $XDG_CACHE_HOME/tandem, else
                            $HOME/.cache/tandem
    TANDEM_DISABLE_CACHE    Set to 1/true to read everything from the server

SETUP:
    # Set TANDEM_ADMIN_TOKEN through a protected environment, then start a server
    td serve --listen 0.0.0.0:13013 --repo /path/to/repo

    # Set TANDEM_TOKEN through a protected environment, then create a workspace
    td clone server:13013 my-workspace --workspace agent-a

    # Let file changes publish themselves
    cd my-workspace
    td daemon &
    echo 'hello' > hello.txt

    # Use jj normally
    td describe -m 'add hello'
    td log";

const SERVE_AFTER_HELP: &str = "\
EXAMPLES:
    td serve --listen 0.0.0.0:13013 --repo /srv/project
    td serve --listen 127.0.0.1:13013 --repo .";

const INIT_AFTER_HELP: &str = "\
EXAMPLES:
    TANDEM_SERVER=server:13013 TANDEM_TOKEN=tdma_… td init .";

const CLONE_AFTER_HELP: &str = "\
EXAMPLES:
    td clone tandem.example/you/my-project ./work --workspace agent-a
    TANDEM_TOKEN=tdma_… td clone server:13013 ./work --workspace agent-a

A clone of a workspace name the server already knows attaches to it: the files
that come back are the last snapshot that name published, wherever the machine
that published them has gone. It also fills the local cache, which is what
makes it the thing to run when baking an image.";

const DAEMON_AFTER_HELP: &str = "\
EXAMPLES:
    td daemon
    td daemon /path/to/workspace --debounce-ms 300
    td daemon --status

The daemon watches the workspace for file changes and publishes each burst of
them as one jj operation. Nothing has to ask it to: there is no checkpoint
command, and no `jj` command has to be run for work to be durable.

A head change published anywhere else marks this workspace stale and stops
there. `td workspace update-stale` is never run for you — it moves files
under whoever is editing them, and that is a decision, not a reflex.";

const SERVER_AFTER_HELP: &str = "\
EXAMPLES:
    td server status
    td server logs --level debug
    td server logs --json";

// ─── CLI definition ───────────────────────────────────────────────────────────

#[derive(Parser)]
#[command(
    name = "td",
    version,
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
        /// Run as daemon (internal, set by `td up`)
        #[arg(long, hide = true)]
        daemon: bool,
        /// Log file path (used in daemon mode)
        #[arg(long)]
        log_file: Option<String>,
        /// Bucket holding the write-ahead log: a directory path, file://…, or
        /// s3://<bucket>[/<prefix>][?endpoint=…&region=…&anonymous=true].
        /// Defaults to a directory inside the repo.
        #[arg(long, env = "TANDEM_BUCKET")]
        bucket: Option<String>,
        /// The token that mints workspace tokens. Generated and logged if
        /// omitted.
        #[arg(long, env = "TANDEM_ADMIN_TOKEN")]
        admin_token: Option<String>,
        /// Serve named repositories from the bucket
        #[arg(long, env = "TANDEM_HOSTED")]
        hosted: bool,
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
        /// The server's admin token, or a token already scoped to this
        /// workspace
        #[arg(long, env = "TANDEM_TOKEN")]
        token: String,
        /// Workspace directory
        #[arg(default_value = ".")]
        path: String,
    },

    /// Create a tandem workspace, or attach to one the server already has
    #[command(after_help = CLONE_AFTER_HELP)]
    Clone {
        /// Server address (host:port)
        #[arg(env = "TANDEM_SERVER")]
        server: String,
        /// Workspace directory
        #[arg(default_value = ".")]
        dir: String,
        /// Workspace name (auto-generated if omitted)
        #[arg(long, env = "TANDEM_WORKSPACE")]
        workspace: Option<String>,
        /// Owner or workspace token; hosted clone can read its credential file
        #[arg(long, env = "TANDEM_TOKEN")]
        token: Option<String>,
    },

    /// Watch a workspace and publish its file changes as operations
    #[command(after_help = DAEMON_AFTER_HELP)]
    Daemon {
        /// Workspace directory
        #[arg(default_value = ".")]
        path: String,
        /// How long a burst of file changes is collected before it is
        /// snapshotted. This is the durability window
        #[arg(long)]
        debounce_ms: Option<u64>,
        /// How long each writer-role claim lasts
        #[arg(long, default_value_t = 30)]
        writer_ttl_seconds: u64,
        /// Print what this workspace's daemon is doing, and exit
        #[arg(long)]
        status: bool,
        /// Output the status as JSON
        #[arg(long)]
        json: bool,
    },

    /// Stream head change notifications (requires server)
    Watch {
        /// Server address (host:port)
        #[arg(long, env = "TANDEM_SERVER")]
        server: String,
        /// A token the server accepts
        #[arg(long, env = "TANDEM_TOKEN")]
        token: String,
    },

    /// Start tandem server as a background daemon
    Up {
        /// Path to the repository directory
        #[arg(long)]
        repo: String,
        /// Address to listen on (e.g. 0.0.0.0:13013). If omitted, tandem auto-selects.
        #[arg(long, env = "TANDEM_LISTEN")]
        listen: Option<String>,
        /// Log level for the daemon (trace, debug, info, warn, error)
        #[arg(long, default_value = "info")]
        log_level: String,
        /// Daemon log file path
        #[arg(long)]
        log_file: Option<String>,
        /// Path to control socket
        #[arg(long)]
        control_socket: Option<String>,
        /// Bucket holding the write-ahead log (see `td serve --bucket`)
        #[arg(long, env = "TANDEM_BUCKET")]
        bucket: Option<String>,
        /// The token that mints workspace tokens. Generated and printed if
        /// omitted.
        #[arg(long, env = "TANDEM_ADMIN_TOKEN")]
        admin_token: Option<String>,
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

pub fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();

    // Route tandem-specific commands through clap.
    // Everything else falls through to jj's CliRunner which does its own
    // argument parsing — this avoids conflicts with jj global flags like
    // --no-pager, --color, -R that appear before the subcommand.
    match args.get(1).map(|s| s.as_str()) {
        None
        | Some(
            "serve" | "init" | "clone" | "daemon" | "watch" | "up" | "down" | "server" | "--help"
            | "-h" | "--version" | "-V",
        ) => {}
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
            bucket,
            admin_token,
            hosted,
        }) => run_serve(
            &listen,
            &repo,
            &log_level,
            &log_format,
            control_socket.as_deref(),
            daemon,
            log_file.as_deref(),
            bucket.as_deref(),
            admin_token.as_deref(),
            hosted,
        ),
        Some(Commands::Init {
            server,
            workspace,
            token,
            path,
        }) => {
            let workspace_name = resolve_init_workspace_name(workspace.as_deref());
            run_tandem_init(&server, &token, &workspace_name, &path)
        }
        Some(Commands::Clone {
            server,
            dir,
            workspace,
            token,
        }) => {
            let workspace_name = resolve_init_workspace_name(workspace.as_deref());
            run_clone(&server, token.as_deref(), &workspace_name, &dir)
        }
        Some(Commands::Daemon {
            path,
            debounce_ms,
            writer_ttl_seconds,
            status,
            json,
        }) => {
            if status {
                run_daemon_status(&path, json)
            } else {
                run_workspace_daemon(&path, debounce_ms, writer_ttl_seconds)
            }
        }
        Some(Commands::Watch { server, token }) => run_watch(&server, &token),
        Some(Commands::Up {
            repo,
            listen,
            log_level,
            log_file,
            control_socket,
            bucket,
            admin_token,
        }) => run_up(
            &repo,
            listen.as_deref(),
            &log_level,
            log_file.as_deref(),
            control_socket.as_deref(),
            bucket.as_deref(),
            admin_token.as_deref(),
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

fn run_watch(server_addr: &str, token: &str) -> ExitCode {
    if let Err(err) = watch_heads(server_addr, token) {
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
    bucket: Option<&str>,
    admin_token: Option<&str>,
    hosted: bool,
) -> ExitCode {
    // In daemon mode, stdout/stderr are already redirected to the log file
    // by `run_up` before spawning this process. Nothing extra needed here.

    // A multi-threaded runtime, because the HTTP handlers hand their work to
    // blocking threads: a jj-lib read or a bucket write must not sit on the
    // reactor while another request waits behind it.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();

    let opts = server::ServeOptions {
        listen_addr: listen_addr.to_string(),
        repo_path: repo_path.to_string(),
        log_level: log_level.to_string(),
        log_format: log_format.to_string(),
        control_socket: control_socket.map(|s| s.to_string()),
        daemon,
        log_file: log_file.map(|s| s.to_string()),
        bucket: bucket.map(|s| s.to_string()),
        admin_token: admin_token.map(|s| s.to_string()),
        hosted,
    };

    if let Err(err) = rt.block_on(server::run_serve(opts)) {
        eprintln!("error: {err:#}");
        return ExitCode::FAILURE;
    }

    ExitCode::SUCCESS
}

// ─── Up / Down / Status / Logs ────────────────────────────────────────────────

fn run_up(
    repo: &str,
    listen: Option<&str>,
    log_level: &str,
    log_file: Option<&str>,
    control_socket: Option<&str>,
    bucket: Option<&str>,
    admin_token: Option<&str>,
) -> ExitCode {
    match server::start_background(
        repo,
        listen,
        log_level,
        log_file,
        control_socket,
        bucket,
        admin_token,
    ) {
        Ok(started) => {
            println!(
                "tandem running on {}, PID {}",
                started.listen_addr, started.pid
            );
            println!("admin token: {}", started.admin_token);
            ExitCode::SUCCESS
        }
        Err(message) => {
            eprintln!("{message}");
            ExitCode::FAILURE
        }
    }
}

fn run_down(control_socket: Option<&str>) -> ExitCode {
    match server::stop_background(control_socket) {
        Ok(()) => {
            println!("tandem stopped");
            ExitCode::SUCCESS
        }
        Err(message) => {
            eprintln!("{message}");
            ExitCode::FAILURE
        }
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
                if let Some(bucket) = status.bucket.as_ref() {
                    println!("  Bucket:   {} ({})", bucket.location, bucket.backend);
                    if !bucket.conditional_put {
                        println!("  Bucket warning: no conditional puts; single writer only");
                    }
                    if bucket.materialized || bucket.replayed_entries > 0 {
                        let verb = if bucket.materialized {
                            "Materialized"
                        } else {
                            "Recovered"
                        };
                        println!(
                            "  {verb} from the bucket: {} op heads, {} WAL entries in {}ms",
                            bucket.replayed_heads, bucket.replayed_entries, bucket.replay_ms
                        );
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
        eprintln!("no tandem daemon running. Start one with `td up`.");
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

fn load_user_settings_from_environment() -> Result<jj_lib::settings::UserSettings, String> {
    user_settings_from_environment().map_err(|e| format!("{e:#}"))
}

fn run_tandem_init(
    server_addr: &str,
    token: &str,
    workspace_name: &str,
    workspace_path_str: &str,
) -> ExitCode {
    let settings = match load_user_settings_from_environment() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::FAILURE;
        }
    };

    match init_tandem_workspace(
        &settings,
        server_addr,
        token,
        workspace_name,
        Path::new(workspace_path_str),
    ) {
        Ok(workspace_path) => {
            eprintln!(
                "Initialized tandem workspace '{}' at {} (server: {})",
                workspace_name,
                workspace_path.display(),
                server_addr
            );
            ExitCode::SUCCESS
        }
        Err(e) => {
            // The whole chain, not just the top context. Every step of
            // `workspace_init` used to print its own message and the cause
            // under it — `error: workspace init failed: <step>: <cause>` — and
            // the steps are `context` calls now, so `{e:#}` is what puts that
            // line back together. It is not character-for-character the old
            // text: where a cause has causes of its own, this prints those too,
            // and the old code stopped at the first.
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

// ─── Clone ────────────────────────────────────────────────────────────────────

fn run_clone(
    server_addr: &str,
    token: Option<&str>,
    workspace_name: &str,
    workspace_path_str: &str,
) -> ExitCode {
    let target = match CloneTarget::parse(server_addr) {
        Ok(target) => target,
        Err(error) => {
            eprintln!("error: {error}");
            return ExitCode::FAILURE;
        }
    };
    let token = match credentials::resolve(token, target.credential_host.as_deref()) {
        Ok(token) => token,
        Err(error) => {
            eprintln!("error: {error}");
            return ExitCode::FAILURE;
        }
    };
    if target.name.is_some() {
        let client = reqwest::blocking::Client::new();
        let info = client
            .get(format!("{}/api/info", target.base_url))
            .bearer_auth(&token)
            .send();
        match info {
            Ok(response) if response.status().is_success() => {}
            Ok(response) if response.status() == reqwest::StatusCode::NOT_FOUND => {
                match client
                    .put(&target.base_url)
                    .bearer_auth(&token)
                    .body(Vec::new())
                    .send()
                {
                    Ok(created) if created.status().is_success() => {}
                    Ok(created) => {
                        eprintln!(
                            "error: repository creation answered HTTP {}",
                            created.status()
                        );
                        return ExitCode::FAILURE;
                    }
                    Err(error) => {
                        eprintln!("error: cannot create {}: {error}", target.base_url);
                        return ExitCode::FAILURE;
                    }
                }
            }
            Ok(response) => {
                eprintln!(
                    "error: {} answered HTTP {}",
                    target.base_url,
                    response.status()
                );
                return ExitCode::FAILURE;
            }
            Err(error) => {
                eprintln!("error: cannot reach {}: {error}", target.base_url);
                return ExitCode::FAILURE;
            }
        }
    }
    let settings = match load_user_settings_from_environment() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::FAILURE;
        }
    };

    match clone_tandem_workspace(
        &settings,
        &target.base_url,
        &token,
        workspace_name,
        Path::new(workspace_path_str),
    ) {
        Ok((workspace_path, origin)) => {
            eprintln!(
                "{} tandem workspace '{}' at {} (server: {})",
                match origin {
                    WorkspaceOrigin::Created => "Created",
                    WorkspaceOrigin::Attached => "Attached to",
                },
                workspace_name,
                workspace_path.display(),
                server_addr
            );
            println!("workspace={workspace_name} origin={}", origin.as_str());
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

// ─── The workspace daemon ─────────────────────────────────────────────────────

fn run_workspace_daemon(
    workspace_path_str: &str,
    debounce_ms: Option<u64>,
    writer_ttl_seconds: u64,
) -> ExitCode {
    let settings = match load_user_settings_from_environment() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::FAILURE;
        }
    };

    let options = DaemonOptions {
        workspace_path: std::path::PathBuf::from(workspace_path_str),
        debounce: resolve_debounce(debounce_ms),
        writer_ttl: std::time::Duration::from_secs(writer_ttl_seconds),
    };

    if let Err(e) = run_daemon(&settings, &options) {
        eprintln!("error: {e:#}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

/// How long ago a status was written, in words, for the line that says the
/// daemon behind it is gone.
fn age_of(updated_at_unix_ms: u64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_millis() as u64)
        .unwrap_or(0);
    let Some(ms) = now.checked_sub(updated_at_unix_ms) else {
        return "at a time this machine's clock is behind".to_string();
    };
    let seconds = ms / 1_000;
    match seconds {
        0 => "less than a second ago".to_string(),
        1 => "1 second ago".to_string(),
        2..=90 => format!("{seconds} seconds ago"),
        _ => format!("{} minutes ago", seconds / 60),
    }
}

fn run_daemon_status(workspace_path_str: &str, json: bool) -> ExitCode {
    let root = match Path::new(workspace_path_str).canonicalize() {
        Ok(root) => root,
        Err(e) => {
            eprintln!("error: cannot resolve {workspace_path_str}: {e}");
            return ExitCode::FAILURE;
        }
    };

    match read_status(&root) {
        Ok(status) => {
            // The file is what the daemon last said, not proof it is still
            // there to say it. A killed daemon leaves a status claiming the
            // writer role and a fresh workspace, and reporting that as current
            // is how a person concludes their machine is publishing when it is
            // not.
            let running = is_running(&status);
            if json {
                let mut value = serde_json::to_value(&status).unwrap();
                if let Some(object) = value.as_object_mut() {
                    object.insert("running".to_string(), serde_json::Value::Bool(running));
                }
                println!("{}", serde_json::to_string_pretty(&value).unwrap());
            } else if !running {
                println!("workspace: {}", status.workspace);
                println!("  Root:      {}", status.workspace_root);
                println!(
                    "  Daemon:    not running — process {} is gone. What follows is what it said \
                     last, {}.",
                    status.pid,
                    age_of(status.updated_at_unix_ms)
                );
                println!("  Published: {} operations", status.published_ops);
                if let Some(op) = status.last_published_op.as_deref() {
                    println!("  Last op:   {op}");
                }
                println!("  Start one with `td daemon {}`.", status.workspace_root);
            } else {
                println!("workspace: {}", status.workspace);
                println!("  Root:      {}", status.workspace_root);
                println!("  Server:    {}", status.server);
                println!("  PID:       {}", status.pid);
                println!("  Debounce:  {}ms", status.debounce_ms);
                println!("  Published: {} operations", status.published_ops);
                if let Some(op) = status.last_published_op.as_deref() {
                    println!("  Last op:   {op}");
                }
                println!(
                    "  Writer:    {}",
                    if status.writer {
                        "held".to_string()
                    } else {
                        format!(
                            "not held ({})",
                            status.writer_detail.as_deref().unwrap_or("unknown")
                        )
                    }
                );
                println!("  Stale:     {}", status.stale);
                if status.stale {
                    println!(
                        "  The heads moved elsewhere. Run `td workspace update-stale` when \
                         you want the files moved — nothing does it for you."
                    );
                }
            }
            if running {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            }
        }
        Err(e) => {
            if json {
                println!("{{\"running\":false}}");
            } else {
                eprintln!("error: {e:#}");
            }
            ExitCode::FAILURE
        }
    }
}

// ─── jj CLI mode ──────────────────────────────────────────────────────────────

fn run_jj() -> ExitCode {
    use jj_cli::cli_util::CliRunner;

    CliRunner::init()
        .version(env!("CARGO_PKG_VERSION"))
        .add_store_factories(jj_tandem_client::tandem_factories())
        .run()
        .into()
}
