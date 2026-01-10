use std::collections::HashSet;
use tandem_core::sync::ForgeDoc;
use tokio::sync::RwLock;
use std::sync::Arc;

#[derive(Debug, thiserror::Error)]
pub enum ContentError {
    #[error("Content not available: {0}")]
    NotAvailable(String),
    #[error("Fetch failed: {0}")]
    FetchFailed(String),
    #[error("Network error: {0}")]
    Network(String),
}

/// Manages lazy content fetching
pub struct ContentManager {
    doc: Arc<RwLock<ForgeDoc>>,
    forge_url: String,
    repo_id: String,
    /// Hashes we're currently fetching to avoid duplicate requests
    pending: RwLock<HashSet<String>>,
}

impl ContentManager {
    pub fn new(doc: Arc<RwLock<ForgeDoc>>, forge_url: String, repo_id: String) -> Self {
        Self {
            doc,
            forge_url,
            repo_id,
            pending: RwLock::new(HashSet::new()),
        }
    }

    /// Check if content is available locally
    pub async fn has_content(&self, hash: &str) -> bool {
        let doc = self.doc.read().await;
        doc.has_content(hash)
    }

    /// Get content, fetching from forge if needed
    pub async fn get_content(&self, hash: &str) -> Result<Vec<u8>, ContentError> {
        {
            let doc = self.doc.read().await;
            if let Some(content) = doc.get_content(hash) {
                return Ok(content);
            }
        }

        self.fetch_content(hash).await
    }

    /// Fetch content from forge
    async fn fetch_content(&self, hash: &str) -> Result<Vec<u8>, ContentError> {
        {
            let mut pending = self.pending.write().await;
            if pending.contains(hash) {
                return Err(ContentError::NotAvailable("Fetch in progress".to_string()));
            }
            pending.insert(hash.to_string());
        }

        let _cleanup = PendingCleanup {
            pending: &self.pending,
            hash: hash.to_string(),
        };

        let client = reqwest::Client::new();
        let url = format!("{}/api/repos/{}/content/{}",
            self.forge_url,
            self.repo_id,
            hash
        );

        let response = client.get(&url)
            .send()
            .await
            .map_err(|e| ContentError::Network(e.to_string()))?;

        if !response.status().is_success() {
            return Err(ContentError::FetchFailed(
                format!("HTTP {}", response.status())
            ));
        }

        let content = response.bytes().await
            .map_err(|e| ContentError::Network(e.to_string()))?;

        {
            let doc = self.doc.read().await;
            doc.put_content(hash, content.to_vec());
        }

        Ok(content.to_vec())
    }

    /// Preload content for a set of hashes
    pub async fn preload(&self, hashes: &[String]) -> Vec<String> {
        let mut failed = Vec::new();

        for hash in hashes {
            if !self.has_content(hash).await {
                if self.fetch_content(hash).await.is_err() {
                    failed.push(hash.clone());
                }
            }
        }

        failed
    }

    /// Preload content for recent changes
    pub async fn preload_recent(&self, count_limit: usize) -> Result<usize, ContentError> {
        let doc = self.doc.read().await;
        let records = doc.get_all_change_records();

        let mut records: Vec<_> = records.into_iter()
            .filter(|r| r.visible)
            .collect();
        records.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));

        let hashes: Vec<String> = records
            .into_iter()
            .take(count_limit)
            .map(|r| r.tree.to_string())
            .collect();

        drop(doc);

        let failed = self.preload(&hashes).await;
        Ok(hashes.len() - failed.len())
    }

    /// Preload content for bookmarked changes
    pub async fn preload_bookmarks(&self) -> Result<usize, ContentError> {
        let doc = self.doc.read().await;
        let bookmarks = doc.get_all_bookmarks();

        let mut hashes = Vec::new();
        for (_name, change_id) in bookmarks {
            let records = doc.get_change_records(&change_id);
            if let Some(record) = records.first() {
                hashes.push(record.tree.to_string());
            }
        }

        drop(doc);

        let failed = self.preload(&hashes).await;
        Ok(hashes.len() - failed.len())
    }
}

/// Helper to clean up pending set
struct PendingCleanup<'a> {
    pending: &'a RwLock<HashSet<String>>,
    hash: String,
}

impl Drop for PendingCleanup<'_> {
    fn drop(&mut self) {
        if let Ok(mut pending) = self.pending.try_write() {
            pending.remove(&self.hash);
        }
    }
}
