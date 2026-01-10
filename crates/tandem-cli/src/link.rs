use std::path::Path;
use crate::repo::{JjRepo, ForgeConfig, ForgeSettings, RepoError};

#[derive(Debug, thiserror::Error)]
pub enum LinkError {
    #[error("Repository error: {0}")]
    Repo(#[from] RepoError),
    #[error("Already linked to forge: {0}")]
    AlreadyLinked(String),
    #[error("Forge unreachable: {0}")]
    Unreachable(String),
    #[error("Authentication failed")]
    AuthFailed,
    #[error("HTTP error: {0}")]
    Http(String),
}

pub async fn link_repo(
    repo_path: &Path,
    forge_url: &str,
    token: Option<&str>,
) -> Result<(), LinkError> {
    let repo = JjRepo::open(repo_path)?;

    if let Some(existing) = repo.forge_config()? {
        return Err(LinkError::AlreadyLinked(existing.forge.url));
    }

    let url = normalize_forge_url(forge_url);

    test_forge_connection(&url, token).await?;

    let config = ForgeConfig {
        forge: ForgeSettings {
            url: url.clone(),
        },
    };
    repo.set_forge_config(&config)?;

    if token.is_some() {
        println!("Token provided - in production, this would be stored in system keychain");
    }

    println!("✓ Linked to forge: {}", url);
    println!("  Run 'jjf daemon start' to begin syncing");

    Ok(())
}

fn normalize_forge_url(url: &str) -> String {
    let mut url = url.to_string();

    if !url.starts_with("http://") && !url.starts_with("https://") {
        url = format!("https://{}", url);
    }

    url.trim_end_matches('/').to_string()
}

async fn test_forge_connection(url: &str, token: Option<&str>) -> Result<(), LinkError> {
    let client = reqwest::Client::new();

    let mut req = client.get(format!("{}/health", url));
    if let Some(token) = token {
        req = req.header("Authorization", format!("Bearer {}", token));
    }

    let response = req.send().await
        .map_err(|e| LinkError::Unreachable(e.to_string()))?;

    if !response.status().is_success() {
        if response.status().as_u16() == 401 {
            return Err(LinkError::AuthFailed);
        }
        return Err(LinkError::Http(format!("Status: {}", response.status())));
    }

    Ok(())
}
