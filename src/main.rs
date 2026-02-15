//! tandem — jj workspaces over the network.
//!
//! Single binary:
//!   tandem serve --listen <addr> --repo <path>   → server mode
//!   tandem init --tandem-server <addr> [path]    → initialize tandem workspace
//!   tandem <jj args>                              → stock jj via CliRunner

#[allow(unused_parens, dead_code)]
mod tandem_capnp {
    include!(concat!(env!("OUT_DIR"), "/tandem_capnp.rs"));
}

mod backend;
mod op_heads_store;
mod op_store;
mod proto_convert;
mod rpc;
mod server;
mod watch;

use std::path::Path;
use std::process::ExitCode;

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
    TANDEM_WORKSPACE        Workspace name (default: \"default\")

SETUP:
    # Start a server
    tandem serve --listen 0.0.0.0:13013 --repo /path/to/repo

    # Initialize a workspace backed by the server
    tandem init --tandem-server server:13013 my-workspace

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
    tandem init --tandem-server server:13013 my-workspace
    tandem init --tandem-server server:13013 --workspace agent-a .
    TANDEM_SERVER=server:13013 tandem init .";

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
    /// Start the tandem server
    #[command(after_help = SERVE_AFTER_HELP)]
    Serve {
        /// Address to listen on (e.g. 0.0.0.0:13013)
        #[arg(long)]
        listen: String,
        /// Path to the repository directory
        #[arg(long)]
        repo: String,
    },

    /// Initialize a tandem-backed workspace
    #[command(after_help = INIT_AFTER_HELP)]
    Init {
        /// Server address (host:port)
        #[arg(long, env = "TANDEM_SERVER")]
        tandem_server: String,
        /// Workspace name
        #[arg(long, default_value = "default", env = "TANDEM_WORKSPACE")]
        workspace: String,
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
}

// ─── Dispatch ─────────────────────────────────────────────────────────────────

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();

    // Route tandem-specific commands through clap.
    // Everything else falls through to jj's CliRunner which does its own
    // argument parsing — this avoids conflicts with jj global flags like
    // --no-pager, --color, -R that appear before the subcommand.
    match args.get(1).map(|s| s.as_str()) {
        None | Some("serve" | "init" | "watch" | "--help" | "-h") => {}
        _ => return run_jj(),
    }

    let cli = Cli::parse();
    match cli.command {
        None => {
            Cli::command().print_help().ok();
            println!();
            ExitCode::SUCCESS
        }
        Some(Commands::Serve { listen, repo }) => run_serve(&listen, &repo),
        Some(Commands::Init {
            tandem_server,
            workspace,
            path,
        }) => run_tandem_init(&tandem_server, &workspace, &path),
        Some(Commands::Watch { server }) => run_watch(&server),
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

fn run_serve(listen_addr: &str, repo_path: &str) -> ExitCode {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let local = tokio::task::LocalSet::new();

    if let Err(err) = local.block_on(&rt, server::run_serve(listen_addr, repo_path)) {
        eprintln!("error: {err:#}");
        return ExitCode::FAILURE;
    }

    ExitCode::SUCCESS
}

// ─── Tandem init ──────────────────────────────────────────────────────────────

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

    let backend_init: &dyn Fn(
        &jj_lib::settings::UserSettings,
        &Path,
    ) -> Result<Box<dyn jj_lib::backend::Backend>, jj_lib::backend::BackendInitError> =
        &|_settings, store_path| {
            Ok(Box::new(backend::TandemBackend::init(store_path, &sa1)?))
        };

    let op_store_init: &dyn Fn(
        &jj_lib::settings::UserSettings,
        &Path,
        jj_lib::op_store::RootOperationData,
    ) -> Result<Box<dyn jj_lib::op_store::OpStore>, jj_lib::backend::BackendInitError> =
        &|_settings, store_path, root_data| {
            Ok(Box::new(op_store::TandemOpStore::init(
                store_path, &sa2, root_data,
            )?))
        };

    let op_heads_init: &dyn Fn(
        &jj_lib::settings::UserSettings,
        &Path,
    )
        -> Result<Box<dyn jj_lib::op_heads_store::OpHeadsStore>, jj_lib::backend::BackendInitError> =
        &|_settings, store_path| {
            Ok(Box::new(
                op_heads_store::TandemOpHeadsStore::init(store_path, &sa3)?,
            ))
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
            Ok(Box::new(backend::TandemBackend::load(settings, store_path)?))
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
            Ok(Box::new(
                op_heads_store::TandemOpHeadsStore::load(settings, store_path)?,
            ))
        }),
    );

    factories
}
