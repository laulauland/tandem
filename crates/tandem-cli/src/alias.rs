use std::process::{Command, ExitStatus, Stdio};
use std::path::Path;
use crate::repo::JjRepo;
use tandem_core::types::ChangeId;

/// Run jj command with presence warnings
pub async fn run_with_presence(args: &[String]) -> Result<ExitStatus, std::io::Error> {
    let cwd = std::env::current_dir()?;

    // Check if this is a command that should trigger presence checks
    let check_presence = should_check_presence(args);

    if check_presence {
        if let Some(change_id) = get_target_change(args) {
            // Check for conflicts
            if let Err(e) = check_and_warn(&cwd, &change_id).await {
                tracing::warn!("Presence check failed: {}", e);
                // Continue anyway - don't block on presence check failures
            }
        }
    }

    // Run the actual jj command
    run_jj(args)
}

/// Check if this command should trigger a presence check
fn should_check_presence(args: &[String]) -> bool {
    if args.is_empty() {
        return false;
    }

    // Commands that edit a specific change
    matches!(args[0].as_str(), "edit" | "checkout" | "co" | "new" | "squash" | "amend")
}

/// Extract target change ID from command args
fn get_target_change(args: &[String]) -> Option<String> {
    if args.len() < 2 {
        return None;
    }

    match args[0].as_str() {
        "edit" | "checkout" | "co" => {
            // jj edit <change_id>
            Some(args[1].clone())
        }
        "new" => {
            // jj new <parent> - the parent might have conflicts
            Some(args[1].clone())
        }
        _ => None,
    }
}

/// Check for presence conflicts and warn user
async fn check_and_warn(repo_path: &Path, change_id_str: &str) -> Result<(), Box<dyn std::error::Error>> {
    // Open repo and check forge config
    let repo = JjRepo::open(repo_path)?;
    let config = repo.forge_config()?;

    if config.is_none() {
        // Not linked to forge, no presence to check
        return Ok(());
    }

    // TODO: Connect to daemon and check presence
    // For now, this is a stub that would integrate with the running daemon

    // Parse change ID
    let _change_id: ChangeId = change_id_str.parse()
        .map_err(|_| "Invalid change ID")?;

    // In a real implementation:
    // 1. Connect to daemon socket
    // 2. Query presence for this change
    // 3. Show warning if conflicts exist
    // 4. Prompt user to continue

    Ok(())
}

/// Run jj command directly
fn run_jj(args: &[String]) -> Result<ExitStatus, std::io::Error> {
    Command::new("jj")
        .args(args)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
}

/// Wrap jj log to show presence information
pub async fn run_log_with_presence(args: &[String]) -> Result<ExitStatus, std::io::Error> {
    // TODO: Intercept jj log output and inject presence information
    // For now, just run jj log directly
    run_jj(args)
}

/// Main entry point for alias mode
pub async fn run_alias(args: Vec<String>) -> Result<i32, Box<dyn std::error::Error>> {
    if args.is_empty() {
        // No args, just run jj
        let status = run_jj(&[])?;
        return Ok(status.code().unwrap_or(1));
    }

    let status = match args[0].as_str() {
        "log" | "l" => run_log_with_presence(&args).await?,
        _ => run_with_presence(&args).await?,
    };

    Ok(status.code().unwrap_or(1))
}
