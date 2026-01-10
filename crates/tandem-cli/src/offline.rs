use std::path::{Path, PathBuf};
use serde::{Serialize, Deserialize};
use chrono::{DateTime, Utc};
use tandem_core::types::{ChangeRecord, ChangeId};

#[derive(Debug, thiserror::Error)]
pub enum OfflineError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
}

/// Types of operations that can be queued offline
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum QueuedOperation {
    /// A change was created or modified
    ChangeUpdated {
        record: ChangeRecord,
        timestamp: DateTime<Utc>,
    },
    /// A bookmark was moved
    BookmarkMoved {
        name: String,
        target: ChangeId,
        timestamp: DateTime<Utc>,
    },
    /// Presence was updated
    PresenceUpdated {
        change_id: ChangeId,
        timestamp: DateTime<Utc>,
    },
}

/// Queue for offline operations
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct OperationQueue {
    operations: Vec<QueuedOperation>,
}

impl OperationQueue {
    pub fn new() -> Self {
        Self::default()
    }

    /// Load queue from disk
    pub fn load(repo_path: &Path) -> Result<Self, OfflineError> {
        let queue_path = Self::queue_path(repo_path);

        if !queue_path.exists() {
            return Ok(Self::new());
        }

        let content = std::fs::read_to_string(&queue_path)?;
        let queue: Self = serde_json::from_str(&content)?;
        Ok(queue)
    }

    /// Save queue to disk
    pub fn save(&self, repo_path: &Path) -> Result<(), OfflineError> {
        let queue_path = Self::queue_path(repo_path);

        // Create parent directory if needed
        if let Some(parent) = queue_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let content = serde_json::to_string_pretty(self)?;
        std::fs::write(&queue_path, content)?;
        Ok(())
    }

    /// Add operation to queue
    pub fn enqueue(&mut self, op: QueuedOperation) {
        self.operations.push(op);
    }

    /// Get number of queued operations
    pub fn len(&self) -> usize {
        self.operations.len()
    }

    /// Check if queue is empty
    pub fn is_empty(&self) -> bool {
        self.operations.is_empty()
    }

    /// Take all operations (clears queue)
    pub fn drain(&mut self) -> Vec<QueuedOperation> {
        std::mem::take(&mut self.operations)
    }

    /// Clear the queue and delete the file
    pub fn clear(&mut self, repo_path: &Path) -> Result<(), OfflineError> {
        self.operations.clear();
        let queue_path = Self::queue_path(repo_path);
        if queue_path.exists() {
            std::fs::remove_file(&queue_path)?;
        }
        Ok(())
    }

    fn queue_path(repo_path: &Path) -> PathBuf {
        repo_path.join(".jj").join("forge-queue.json")
    }
}

/// Replay queued operations to forge
pub async fn replay_queue(
    repo_path: &Path,
    doc: &tandem_core::sync::ForgeDoc,
) -> Result<usize, OfflineError> {
    let mut queue = OperationQueue::load(repo_path)?;

    if queue.is_empty() {
        return Ok(0);
    }

    let operations = queue.drain();
    let count = operations.len();

    for op in operations {
        match op {
            QueuedOperation::ChangeUpdated { record, .. } => {
                doc.insert_change(&record);
            }
            QueuedOperation::BookmarkMoved { name, target, .. } => {
                doc.set_bookmark(&name, &target);
            }
            QueuedOperation::PresenceUpdated { .. } => {
                // Presence updates are ephemeral, skip old ones
            }
        }
    }

    // Clear the queue file
    queue.clear(repo_path)?;

    Ok(count)
}

/// Check if we're in offline mode (no connection to forge)
pub fn is_offline(repo_path: &Path) -> bool {
    // Check for offline marker file
    let marker = repo_path.join(".jj").join("forge-offline");
    marker.exists()
}

/// Set offline mode
pub fn set_offline(repo_path: &Path, offline: bool) -> Result<(), OfflineError> {
    let marker = repo_path.join(".jj").join("forge-offline");

    if offline {
        std::fs::write(&marker, "")?;
    } else if marker.exists() {
        std::fs::remove_file(&marker)?;
    }

    Ok(())
}
