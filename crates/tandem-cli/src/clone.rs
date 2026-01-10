use std::path::{Path, PathBuf};
use crate::link::LinkError;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use futures_util::{SinkExt, StreamExt};
use tandem_core::sync::ForgeDoc;
use yrs::{Transact, StateVector, ReadTxn};

#[derive(Debug, thiserror::Error)]
pub enum CloneError {
    #[error("Directory already exists: {0}")]
    DirectoryExists(PathBuf),
    #[error("Failed to create directory: {0}")]
    CreateDir(#[from] std::io::Error),
    #[error("Failed to initialize jj: {0}")]
    JjInit(String),
    #[error("Link error: {0}")]
    Link(#[from] LinkError),
    #[error("Sync error: {0}")]
    Sync(String),
    #[error("HTTP error: {0}")]
    Http(String),
}

/// Clone a repository from forge
pub async fn clone_repo(
    forge_url: &str,
    target_dir: Option<&Path>,
    token: Option<&str>,
) -> Result<PathBuf, CloneError> {
    // Parse repo name from URL
    let repo_name = parse_repo_name(forge_url)?;

    // Determine target directory
    let target = match target_dir {
        Some(p) => p.to_path_buf(),
        None => std::env::current_dir()?.join(&repo_name),
    };

    // Check if directory exists
    if target.exists() {
        return Err(CloneError::DirectoryExists(target));
    }

    println!("Cloning into '{}'...", target.display());

    // Create directory
    std::fs::create_dir_all(&target)?;

    // Initialize jj repo
    init_jj_repo(&target)?;

    // Link to forge
    crate::link::link_repo(&target, forge_url, token).await?;

    // Pull initial state
    pull_initial_state(&target, forge_url, token).await?;

    println!("✓ Cloned repository to {}", target.display());

    Ok(target)
}

/// Parse repository name from forge URL
fn parse_repo_name(url: &str) -> Result<String, CloneError> {
    // URL format: https://forge.example.com/org/repo
    let url = url.trim_end_matches('/');
    let name = url.rsplit('/').next()
        .ok_or_else(|| CloneError::Sync("Invalid URL format".to_string()))?;
    Ok(name.to_string())
}

/// Initialize a new jj repository
fn init_jj_repo(path: &Path) -> Result<(), CloneError> {
    let output = std::process::Command::new("jj")
        .arg("init")
        .current_dir(path)
        .output()
        .map_err(|e| CloneError::JjInit(e.to_string()))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(CloneError::JjInit(stderr.to_string()));
    }

    Ok(())
}

/// Pull initial state from forge
async fn pull_initial_state(
    path: &Path,
    forge_url: &str,
    _token: Option<&str>,
) -> Result<(), CloneError> {
    println!("  Syncing initial state...");

    // Extract repo ID from URL
    let repo_id = forge_url
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .ok_or_else(|| CloneError::Sync("Invalid URL".to_string()))?;

    // Build WebSocket URL
    let ws_url = format!("{}/sync/{}",
        forge_url.replace("https://", "wss://").replace("http://", "ws://"),
        repo_id
    );

    // Connect to forge
    let (ws_stream, _) = connect_async(&ws_url).await
        .map_err(|e| CloneError::Sync(format!("Connection failed: {}", e)))?;

    let (mut write, mut read) = ws_stream.split();

    // Create empty ForgeDoc
    let doc = ForgeDoc::new();

    // Send empty state vector to get full state
    let sv = doc.encode_state_vector();
    write.send(Message::Binary(sv.into())).await
        .map_err(|e| CloneError::Sync(format!("Send failed: {}", e)))?;

    // Receive initial state
    let mut received_update = false;
    let timeout = tokio::time::timeout(
        tokio::time::Duration::from_secs(30),
        async {
            while let Some(msg) = read.next().await {
                match msg {
                    Ok(Message::Binary(data)) => {
                        doc.apply_update(&data)
                            .map_err(|e| CloneError::Sync(format!("Apply failed: {:?}", e)))?;
                        received_update = true;

                        // After receiving update, we have the initial state
                        // In a more sophisticated impl, we'd wait for multiple updates
                        break;
                    }
                    Ok(Message::Close(_)) => break,
                    Err(e) => return Err(CloneError::Sync(format!("Receive error: {}", e))),
                    _ => continue,
                }
            }
            Ok::<(), CloneError>(())
        }
    ).await;

    match timeout {
        Ok(Ok(())) if received_update => {
            println!("  ✓ Received {} changes, {} bookmarks",
                doc.get_all_change_records().len(),
                doc.get_all_bookmarks().len()
            );
        }
        Ok(Ok(())) => {
            println!("  ⚠ No data received (empty repository?)");
        }
        Ok(Err(e)) => return Err(e),
        Err(_) => {
            return Err(CloneError::Sync("Timeout waiting for initial state".to_string()));
        }
    }

    // Save the ForgeDoc state to a local file for the daemon to use
    let doc_path = path.join(".jj").join("forge-doc.bin");
    let txn = doc.doc().transact();
    let state = txn.encode_diff_v1(&StateVector::default());
    std::fs::write(&doc_path, state)
        .map_err(|e| CloneError::Sync(format!("Failed to save state: {}", e)))?;

    // Close connection
    let _ = write.send(Message::Close(None)).await;

    Ok(())
}
