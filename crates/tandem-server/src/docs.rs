use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tandem_core::sync::ForgeDoc;
use tokio::sync::RwLock;
use yrs::{ReadTxn, StateVector, Transact};

#[derive(Debug, thiserror::Error)]
pub enum DocError {
    #[error("Repository not found: {0}")]
    NotFound(String),
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Serialization error: {0}")]
    Serialization(String),
}

/// Manages Y.Doc instances for all repositories
pub struct DocManager {
    /// Directory where doc files are stored
    data_dir: PathBuf,
    /// In-memory cache of loaded docs
    docs: RwLock<HashMap<String, Arc<RwLock<ForgeDoc>>>>,
}

impl DocManager {
    pub fn new(data_dir: impl AsRef<Path>) -> Self {
        Self {
            data_dir: data_dir.as_ref().to_path_buf(),
            docs: RwLock::new(HashMap::new()),
        }
    }

    /// Get or load a ForgeDoc for a repository
    pub async fn get_or_load(&self, repo_id: &str) -> Result<Arc<RwLock<ForgeDoc>>, DocError> {
        // Check cache first
        {
            let docs = self.docs.read().await;
            if let Some(doc) = docs.get(repo_id) {
                return Ok(Arc::clone(doc));
            }
        }

        // Load from disk or create new
        let doc = self.load_or_create(repo_id).await?;
        let doc = Arc::new(RwLock::new(doc));

        // Cache it
        {
            let mut docs = self.docs.write().await;
            docs.insert(repo_id.to_string(), Arc::clone(&doc));
        }

        Ok(doc)
    }

    /// Create a new doc for a repository
    pub async fn create(&self, repo_id: &str) -> Result<Arc<RwLock<ForgeDoc>>, DocError> {
        let doc = ForgeDoc::new();
        let doc = Arc::new(RwLock::new(doc));

        // Cache it
        {
            let mut docs = self.docs.write().await;
            docs.insert(repo_id.to_string(), Arc::clone(&doc));
        }

        // Save to disk
        self.save(repo_id).await?;

        Ok(doc)
    }

    /// Save a doc to disk
    pub async fn save(&self, repo_id: &str) -> Result<(), DocError> {
        let docs = self.docs.read().await;
        let doc = docs
            .get(repo_id)
            .ok_or_else(|| DocError::NotFound(repo_id.to_string()))?;

        let doc = doc.read().await;

        // Encode the full document state
        let state = {
            let txn = doc.doc().transact();
            txn.encode_diff_v1(&StateVector::default())
        };

        // Write to file
        let path = self.doc_path(repo_id);
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::write(&path, state).await?;

        Ok(())
    }

    /// Save all docs to disk
    pub async fn save_all(&self) -> Result<(), DocError> {
        let repo_ids: Vec<String> = {
            let docs = self.docs.read().await;
            docs.keys().cloned().collect()
        };

        for repo_id in repo_ids {
            self.save(&repo_id).await?;
        }

        Ok(())
    }

    /// Load doc from disk or create new
    async fn load_or_create(&self, repo_id: &str) -> Result<ForgeDoc, DocError> {
        let path = self.doc_path(repo_id);

        if path.exists() {
            let data = tokio::fs::read(&path).await?;
            let doc = ForgeDoc::new();
            doc.apply_update(&data)
                .map_err(|e| DocError::Serialization(e.to_string()))?;
            Ok(doc)
        } else {
            Ok(ForgeDoc::new())
        }
    }

    fn doc_path(&self, repo_id: &str) -> PathBuf {
        self.data_dir.join(format!("{}.yrs", repo_id))
    }
}
