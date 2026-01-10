//! Core types for Tandem

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;
use uuid::Uuid;

/// Stable identifier for a change, persists across rebases
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ChangeId(pub [u8; 32]);

impl ChangeId {
    /// Create a new random ChangeId
    pub fn new() -> Self {
        let mut bytes = [0u8; 32];
        rand::Rng::fill(&mut rand::thread_rng(), &mut bytes);
        ChangeId(bytes)
    }
}

impl Default for ChangeId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for ChangeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", hex::encode(self.0))
    }
}

impl FromStr for ChangeId {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let bytes = hex::decode(s).map_err(|e| format!("Invalid hex: {}", e))?;
        if bytes.len() != 32 {
            return Err(format!("Expected 32 bytes, got {}", bytes.len()));
        }
        let mut arr = [0u8; 32];
        arr.copy_from_slice(&bytes);
        Ok(ChangeId(arr))
    }
}

/// Content-addressed tree hash
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TreeHash(pub [u8; 20]);

impl TreeHash {
    /// Create TreeHash from hex string (for compatibility with object_store)
    pub fn new(hash: String) -> Self {
        assert_eq!(hash.len(), 40, "TreeHash must be 40 characters");
        TreeHash::from_str(&hash).expect("Invalid hex string")
    }

    /// Get hex string representation (for compatibility with object_store)
    pub fn as_str(&self) -> String {
        self.to_string()
    }
}

impl fmt::Display for TreeHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", hex::encode(self.0))
    }
}

impl FromStr for TreeHash {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let bytes = hex::decode(s).map_err(|e| format!("Invalid hex: {}", e))?;
        if bytes.len() != 20 {
            return Err(format!("Expected 20 bytes, got {}", bytes.len()));
        }
        let mut arr = [0u8; 20];
        arr.copy_from_slice(&bytes);
        Ok(TreeHash(arr))
    }
}

/// Content-addressed blob hash
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct BlobHash(pub [u8; 20]);

impl BlobHash {
    /// Create BlobHash from hex string (for compatibility with object_store)
    pub fn new(hash: String) -> Self {
        assert_eq!(hash.len(), 40, "BlobHash must be 40 characters");
        BlobHash::from_str(&hash).expect("Invalid hex string")
    }

    /// Get hex string representation (for compatibility with object_store)
    pub fn as_str(&self) -> String {
        self.to_string()
    }
}

impl fmt::Display for BlobHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", hex::encode(self.0))
    }
}

impl FromStr for BlobHash {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let bytes = hex::decode(s).map_err(|e| format!("Invalid hex: {}", e))?;
        if bytes.len() != 20 {
            return Err(format!("Expected 20 bytes, got {}", bytes.len()));
        }
        let mut arr = [0u8; 20];
        arr.copy_from_slice(&bytes);
        Ok(BlobHash(arr))
    }
}

/// User identity
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Identity {
    pub email: String,
    pub name: Option<String>,
}

/// The fundamental unit - identity persists across rebases
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Change {
    pub id: ChangeId,
    pub tree: TreeHash,
    pub parents: Vec<ChangeId>,
    pub description: String,
    pub author: Identity,
    pub timestamp: DateTime<Utc>,
}

/// Record stored in Y.Doc - append-only with unique keys
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChangeRecord {
    pub record_id: Uuid,
    pub change_id: ChangeId,
    pub tree: TreeHash,
    pub parents: Vec<ChangeId>,
    pub description: String,
    pub author: Identity,
    pub timestamp: DateTime<Utc>,
    pub visible: bool, // false = abandoned/hidden
}

impl ChangeRecord {
    /// Create a ChangeRecord from a Change
    pub fn from_change(change: &Change) -> Self {
        ChangeRecord {
            record_id: Uuid::new_v4(),
            change_id: change.id,
            tree: change.tree,
            parents: change.parents.clone(),
            description: change.description.clone(),
            author: change.author.clone(),
            timestamp: change.timestamp,
            visible: true,
        }
    }
}

/// Rules for bookmark protection
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BookmarkRules {
    pub require_ci: bool,
    pub require_review: bool,
}

/// Named pointer to a change
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Bookmark {
    pub name: String,
    pub target: ChangeId,
    pub protected: bool,
    pub rules: BookmarkRules,
}

/// Presence information for a user
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PresenceInfo {
    pub user_id: String,
    pub change_id: ChangeId,
    pub device: String,
    pub timestamp: DateTime<Utc>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_change_id_new() {
        let id1 = ChangeId::new();
        let id2 = ChangeId::new();
        assert_ne!(id1, id2);
    }

    #[test]
    fn test_change_id_display_parse() {
        let id = ChangeId::new();
        let hex_str = id.to_string();
        assert_eq!(hex_str.len(), 64); // 32 bytes * 2 chars per byte
        let parsed = ChangeId::from_str(&hex_str).unwrap();
        assert_eq!(id, parsed);
    }

    #[test]
    fn test_tree_hash_display_parse() {
        let hash = TreeHash([1u8; 20]);
        let hex_str = hash.to_string();
        assert_eq!(hex_str.len(), 40); // 20 bytes * 2 chars per byte
        let parsed = TreeHash::from_str(&hex_str).unwrap();
        assert_eq!(hash, parsed);
    }

    #[test]
    fn test_blob_hash_display_parse() {
        let hash = BlobHash([2u8; 20]);
        let hex_str = hash.to_string();
        assert_eq!(hex_str.len(), 40); // 20 bytes * 2 chars per byte
        let parsed = BlobHash::from_str(&hex_str).unwrap();
        assert_eq!(hash, parsed);
    }

    #[test]
    fn test_change_record_from_change() {
        let change = Change {
            id: ChangeId::new(),
            tree: TreeHash([0u8; 20]),
            parents: vec![],
            description: "Test change".to_string(),
            author: Identity {
                email: "test@example.com".to_string(),
                name: Some("Test User".to_string()),
            },
            timestamp: Utc::now(),
        };

        let record = ChangeRecord::from_change(&change);
        assert_eq!(record.change_id, change.id);
        assert_eq!(record.tree, change.tree);
        assert_eq!(record.parents, change.parents);
        assert_eq!(record.description, change.description);
        assert_eq!(record.author, change.author);
        assert_eq!(record.timestamp, change.timestamp);
        assert_eq!(record.visible, true);
    }
}
