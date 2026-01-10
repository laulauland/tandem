//! Tandem CLI (jjf)
//!
//! Command-line interface for the Tandem Forge

use clap::{Parser, Subcommand};
use tandem_cli::repo::{JjRepo, ForgeConfig, ForgeSettings};
use std::env;
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "jjf")]
#[command(about = "Jujutsu Forge CLI - Manage code reviews and changes", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Initialize a new repository
    Init {
        /// Repository name
        #[arg(short, long)]
        name: String,
    },
    /// List all changes
    List,
    /// Show status
    Status,
    /// Link this repository to a forge
    Link {
        /// Forge URL (e.g., https://forge.example.com/org/repo)
        url: String,

        /// Auth token (if not provided, will prompt or use keychain)
        #[arg(long)]
        token: Option<String>,
    },
    /// Clone a repository from a forge
    Clone {
        /// Forge URL (e.g., https://forge.example.com/org/repo)
        url: String,

        /// Target directory (defaults to repo name)
        #[arg(short, long)]
        directory: Option<PathBuf>,

        /// Auth token
        #[arg(long)]
        token: Option<String>,
    },
    /// Daemon management
    Daemon {
        #[command(subcommand)]
        action: DaemonAction,
    },
    /// Wrapper for jj with presence warnings (use as: alias jj='jjf alias')
    Alias {
        /// Arguments to pass to jj
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
}

#[derive(Subcommand)]
enum DaemonAction {
    /// Start daemon in foreground
    Start,
    /// Check daemon status
    Status,
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    match cli.command {
        Commands::Alias { args } => {
            use tandem_cli::alias;
            match alias::run_alias(args).await {
                Ok(code) => std::process::exit(code),
                Err(e) => {
                    eprintln!("Error: {}", e);
                    std::process::exit(1);
                }
            }
        }
        _ => {
            let result = match cli.command {
                Commands::Init { name } => handle_init(&name),
                Commands::List => handle_list(),
                Commands::Status => handle_status(),
                Commands::Link { url, token } => handle_link(&url, token.as_deref()).await,
                Commands::Clone { url, directory, token } => handle_clone(&url, directory.as_deref(), token.as_deref()).await,
                Commands::Daemon { action } => handle_daemon(action).await,
                Commands::Alias { .. } => unreachable!(),
            };

            if let Err(e) = result {
                eprintln!("Error: {}", e);
                std::process::exit(1);
            }
        }
    }
}

fn handle_init(name: &str) -> Result<(), Box<dyn std::error::Error>> {
    let current_dir = env::current_dir()?;

    // Check if .jj directory exists
    let jj_dir = current_dir.join(".jj");
    if !jj_dir.exists() {
        return Err("Not a jj repository. Run 'jj init' or 'jj git clone' first.".into());
    }

    let repo = JjRepo::open(&current_dir)?;

    // Check if forge is already configured
    if let Some(existing_config) = repo.forge_config()? {
        println!("Forge already configured: {}", existing_config.forge.url);
        return Ok(());
    }

    // Create forge configuration
    let config = ForgeConfig {
        forge: ForgeSettings {
            url: format!("https://forge.example.com/{}", name),
        },
    };

    repo.set_forge_config(&config)?;
    println!("Initialized forge configuration for repository: {}", name);
    println!("Forge URL: {}", config.forge.url);

    Ok(())
}

fn handle_list() -> Result<(), Box<dyn std::error::Error>> {
    let current_dir = env::current_dir()?;
    let repo = JjRepo::open(&current_dir)?;

    let changes = repo.list_changes()?;

    if changes.is_empty() {
        println!("No changes found.");
    } else {
        println!("Changes:");
        for change in changes {
            println!("  {} - {}", change.id, change.description);
        }
    }

    Ok(())
}

fn handle_status() -> Result<(), Box<dyn std::error::Error>> {
    let current_dir = env::current_dir()?;

    // Check if we're in a jj repository
    let jj_dir = current_dir.join(".jj");
    if !jj_dir.exists() {
        println!("Not a jj repository");
        return Ok(());
    }

    let repo = JjRepo::open(&current_dir)?;

    // Check forge configuration
    match repo.forge_config()? {
        Some(config) => {
            println!("Repository: {}", repo.path().display());
            println!("Forge URL: {}", config.forge.url);
            println!("Status: Connected");
        }
        None => {
            println!("Repository: {}", repo.path().display());
            println!("Forge: Not configured");
            println!("Run 'jjf init --name <repo-name>' to configure");
        }
    }

    Ok(())
}

async fn handle_link(url: &str, token: Option<&str>) -> Result<(), Box<dyn std::error::Error>> {
    use tandem_cli::link;

    let cwd = env::current_dir()?;
    link::link_repo(&cwd, url, token).await?;
    Ok(())
}

async fn handle_clone(
    url: &str,
    directory: Option<&std::path::Path>,
    token: Option<&str>,
) -> Result<(), Box<dyn std::error::Error>> {
    use tandem_cli::clone;

    clone::clone_repo(url, directory, token).await?;
    Ok(())
}

async fn handle_daemon(action: DaemonAction) -> Result<(), Box<dyn std::error::Error>> {
    use tandem_cli::daemon;

    let current_dir = env::current_dir()?;
    let repo = JjRepo::open(&current_dir)?;

    let config = repo.forge_config()?
        .ok_or("Forge not configured. Run 'jjf init --name <repo-name>' first.")?;

    match action {
        DaemonAction::Start => {
            println!("Starting daemon...");
            println!("Repo: {}", repo.path().display());
            println!("Forge: {}", config.forge.url);

            let handle = daemon::spawn_daemon(
                repo.path().to_path_buf(),
                config.forge.url.clone()
            );

            println!("Daemon started. Press Ctrl+C to stop.");

            tokio::signal::ctrl_c().await?;

            println!("\nShutting down daemon...");
            handle.shutdown().await?;
            println!("Daemon stopped.");
        }
        DaemonAction::Status => {
            println!("Daemon status: Not implemented yet");
        }
    }

    Ok(())
}
