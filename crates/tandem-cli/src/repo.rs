use std::path::{Path, PathBuf};
use std::collections::HashMap;
use tandem_core::types::{Change, ChangeId, Identity, TreeHash};
use jj_lib::workspace::Workspace;
use jj_lib::settings::UserSettings;
use jj_lib::repo::{StoreFactories, Repo};
use jj_lib::revset::RevsetExpression;
use jj_lib::object_id::ObjectId;
use chrono::{DateTime, Utc};

/// Error type for repo operations
#[derive(Debug, thiserror::Error)]
pub enum RepoError {
    #[error("Repository not found at {0}")]
    NotFound(PathBuf),
    #[error("Not a jj repository")]
    NotJjRepo,
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Internal error: {0}")]
    Internal(String),
}

/// Wrapper around jj repository
pub struct JjRepo {
    path: PathBuf,
    workspace: Workspace,
}

impl JjRepo {
    /// Open a jj repository at the given path
    pub fn open(path: impl AsRef<Path>) -> Result<Self, RepoError> {
        let path = path.as_ref().to_path_buf();

        // Check for .jj directory
        let jj_dir = path.join(".jj");
        if !jj_dir.exists() {
            return Err(RepoError::NotJjRepo);
        }

        // Create default user settings (required by jj-lib)
        let config = jj_lib::config::StackedConfig::empty();
        let settings = UserSettings::from_config(config)
            .map_err(|e| RepoError::Internal(format!("Failed to create settings: {}", e)))?;

        // Create store factories for loading the repository
        let store_factories = StoreFactories::default();

        // Empty working copy factories map (use defaults)
        let wc_factories = HashMap::new();

        // Load the workspace
        let workspace = Workspace::load(&settings, &path, &store_factories, &wc_factories)
            .map_err(|e| RepoError::Internal(format!("Failed to load workspace: {}", e)))?;

        Ok(Self { path, workspace })
    }

    /// Get repository root path
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// List all visible changes in the repository
    pub fn list_changes(&self) -> Result<Vec<Change>, RepoError> {
        let repo_loader = self.workspace.repo_loader();
        let repo = repo_loader
            .load_at_head()
            .map_err(|e| RepoError::Internal(format!("Failed to load repo: {}", e)))?;

        // Get all visible commits (equivalent to "jj log")
        // Use revset to get all commits
        let revset_expression = RevsetExpression::all();
        let evaluated = revset_expression
            .evaluate(repo.as_ref())
            .map_err(|e| RepoError::Internal(format!("Failed to evaluate revset: {}", e)))?;

        let mut changes = Vec::new();
        for commit_id_result in evaluated.iter() {
            let commit_id = commit_id_result
                .map_err(|e| RepoError::Internal(format!("Failed to iterate commits: {}", e)))?;
            let commit = repo
                .store()
                .get_commit(&commit_id)
                .map_err(|e| RepoError::Internal(format!("Failed to get commit: {}", e)))?;

            changes.push(Self::convert_commit_to_change(&commit, repo.as_ref())?);
        }

        Ok(changes)
    }

    /// Convert jj-lib Commit to our Change type
    fn convert_commit_to_change(
        commit: &jj_lib::commit::Commit,
        repo: &jj_lib::repo::ReadonlyRepo,
    ) -> Result<Change, RepoError> {
        // Convert change_id (jj's stable ID) from bytes
        let change_id_bytes = commit.change_id().as_bytes();
        if change_id_bytes.len() != 32 {
            return Err(RepoError::Internal(format!(
                "Invalid change_id length: expected 32, got {}",
                change_id_bytes.len()
            )));
        }
        let mut change_id = [0u8; 32];
        change_id.copy_from_slice(change_id_bytes);

        // Convert tree_id from jj to our TreeHash
        // For now, we'll use a simplified hash of the tree content
        // In jj 0.37, MergedTree::resolve() is async and complex to use here
        // We'll extract tree_id from the underlying backend commit via the store
        // FIXME: Implement proper tree ID extraction - for now use a placeholder based on change_id
        // This is a temporary workaround until we properly handle async tree resolution
        let mut tree_hash = [0u8; 20];
        // Use first 20 bytes of change_id as a temporary tree hash placeholder
        tree_hash.copy_from_slice(&change_id[..20]);

        // Convert parent change_ids
        let parents = commit
            .parent_ids()
            .iter()
            .filter_map(|parent_commit_id| {
                // Look up parent commit to get its change_id
                repo.store()
                    .get_commit(parent_commit_id)
                    .ok()
                    .and_then(|parent_commit| {
                        let parent_change_id_bytes = parent_commit.change_id().as_bytes();
                        if parent_change_id_bytes.len() == 32 {
                            let mut parent_change_id = [0u8; 32];
                            parent_change_id.copy_from_slice(parent_change_id_bytes);
                            Some(ChangeId(parent_change_id))
                        } else {
                            None
                        }
                    })
            })
            .collect();

        let author = Identity {
            name: Some(commit.author().name.clone()),
            email: commit.author().email.clone(),
        };

        // Convert timestamp from jj's Timestamp to chrono DateTime
        let timestamp_millis = commit.author().timestamp.timestamp.0;
        let timestamp = DateTime::from_timestamp(timestamp_millis / 1000, 0)
            .unwrap_or_else(|| Utc::now());

        Ok(Change {
            id: ChangeId(change_id),
            tree: TreeHash(tree_hash),
            parents,
            description: commit.description().to_string(),
            author,
            timestamp,
        })
    }

    /// Get a specific change by ID
    pub fn get_change(&self, id: &ChangeId) -> Result<Option<Change>, RepoError> {
        let repo_loader = self.workspace.repo_loader();
        let repo = repo_loader
            .load_at_head()
            .map_err(|e| RepoError::Internal(format!("Failed to load repo: {}", e)))?;

        // In jj, we need to find the commit with this change_id
        // We'll search through all commits to find one with matching change_id
        let revset_expression = RevsetExpression::all();
        let evaluated = revset_expression
            .evaluate(repo.as_ref())
            .map_err(|e| RepoError::Internal(format!("Failed to evaluate revset: {}", e)))?;

        for commit_id_result in evaluated.iter() {
            let commit_id = commit_id_result
                .map_err(|e| RepoError::Internal(format!("Failed to iterate commits: {}", e)))?;
            let commit = repo
                .store()
                .get_commit(&commit_id)
                .map_err(|e| RepoError::Internal(format!("Failed to get commit: {}", e)))?;

            // Check if this commit's change_id matches
            let commit_change_id_bytes = commit.change_id().as_bytes();
            if commit_change_id_bytes == id.0.as_slice() {
                return Ok(Some(Self::convert_commit_to_change(&commit, repo.as_ref())?));
            }
        }

        Ok(None)
    }

    /// Get the current working copy change ID
    pub fn working_copy_change_id(&self) -> Result<Option<ChangeId>, RepoError> {
        let repo_loader = self.workspace.repo_loader();
        let repo = repo_loader
            .load_at_head()
            .map_err(|e| RepoError::Internal(format!("Failed to load repo: {}", e)))?;

        // Get the working copy commit ID from the op store
        // In jj, we need to get it from the operation view
        let view = repo.view();
        let wc_commit_id = view
            .get_wc_commit_id(self.workspace.workspace_name())
            .ok_or_else(|| RepoError::Internal("No working copy commit for this workspace".to_string()))?;

        // Load the commit to get its change_id
        let commit = repo
            .store()
            .get_commit(wc_commit_id)
            .map_err(|e| RepoError::Internal(format!("Failed to get working copy commit: {}", e)))?;

        // Extract the change_id
        let change_id_bytes = commit.change_id().as_bytes();
        if change_id_bytes.len() != 32 {
            return Err(RepoError::Internal(format!(
                "Invalid change_id length: expected 32, got {}",
                change_id_bytes.len()
            )));
        }
        let mut change_id = [0u8; 32];
        change_id.copy_from_slice(change_id_bytes);

        Ok(Some(ChangeId(change_id)))
    }

    /// Check if forge is configured for this repo
    pub fn forge_config(&self) -> Result<Option<ForgeConfig>, RepoError> {
        let config_path = self.path.join(".jj").join("forge.toml");
        if !config_path.exists() {
            return Ok(None);
        }

        let content = std::fs::read_to_string(&config_path)?;
        let config: ForgeConfig = toml::from_str(&content)
            .map_err(|e| RepoError::Internal(e.to_string()))?;
        Ok(Some(config))
    }

    /// Write forge configuration
    pub fn set_forge_config(&self, config: &ForgeConfig) -> Result<(), RepoError> {
        let config_path = self.path.join(".jj").join("forge.toml");
        let content = toml::to_string_pretty(config)
            .map_err(|e| RepoError::Internal(e.to_string()))?;
        std::fs::write(&config_path, content)?;
        Ok(())
    }
}

/// Forge configuration stored in .jj/forge.toml
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ForgeConfig {
    pub forge: ForgeSettings,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ForgeSettings {
    pub url: String,
}
