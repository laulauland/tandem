//! Content-addressed object storage for trees and blobs (git-compatible)

use crate::types::{BlobHash, TreeHash};
use serde::{Deserialize, Serialize};
use sha1::{Digest, Sha1};
use std::collections::HashMap;

/// File mode (simplified, git-compatible)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FileMode {
    Regular,    // 100644
    Executable, // 100755
    Symlink,    // 120000
    Directory,  // 040000
}

impl FileMode {
    pub fn as_str(&self) -> &str {
        match self {
            FileMode::Regular => "100644",
            FileMode::Executable => "100755",
            FileMode::Symlink => "120000",
            FileMode::Directory => "040000",
        }
    }
}

/// Reference to either a tree or blob
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ObjectRef {
    Tree(TreeHash),
    Blob(BlobHash),
}

/// Entry in a tree
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TreeEntry {
    pub name: String,
    pub mode: FileMode,
    pub hash: ObjectRef,
}

/// A tree object (directory)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tree {
    pub entries: Vec<TreeEntry>,
}

impl Tree {
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    pub fn add_entry(&mut self, name: String, mode: FileMode, hash: ObjectRef) {
        self.entries.push(TreeEntry { name, mode, hash });
    }

    /// Compute git-compatible hash of this tree
    pub fn hash(&self) -> TreeHash {
        let mut content = Vec::new();

        // Sort entries by name (git requirement)
        let mut sorted_entries = self.entries.clone();
        sorted_entries.sort_by(|a, b| a.name.cmp(&b.name));

        // Build tree content in git format
        for entry in sorted_entries {
            // Mode (as string)
            content.extend_from_slice(entry.mode.as_str().as_bytes());
            content.push(b' ');

            // Name
            content.extend_from_slice(entry.name.as_bytes());
            content.push(b'\0');

            // Hash (as raw bytes, 20 bytes for SHA1)
            match entry.hash {
                ObjectRef::Tree(h) => content.extend_from_slice(&h.0),
                ObjectRef::Blob(h) => content.extend_from_slice(&h.0),
            }
        }

        // Git format: "tree {size}\0{content}"
        let header = format!("tree {}\0", content.len());
        let mut full_content = Vec::new();
        full_content.extend_from_slice(header.as_bytes());
        full_content.extend_from_slice(&content);

        // Compute SHA1
        let mut hasher = Sha1::new();
        hasher.update(&full_content);
        let result = hasher.finalize();

        // Convert to [u8; 20]
        let mut hash_bytes = [0u8; 20];
        hash_bytes.copy_from_slice(&result);

        TreeHash(hash_bytes)
    }
}

impl Default for Tree {
    fn default() -> Self {
        Self::new()
    }
}

/// Compute git-compatible hash of blob content
pub fn hash_blob(content: &[u8]) -> BlobHash {
    // Git format: "blob {size}\0{content}"
    let header = format!("blob {}\0", content.len());
    let mut full_content = Vec::new();
    full_content.extend_from_slice(header.as_bytes());
    full_content.extend_from_slice(content);

    // Compute SHA1
    let mut hasher = Sha1::new();
    hasher.update(&full_content);
    let result = hasher.finalize();

    // Convert to [u8; 20]
    let mut hash_bytes = [0u8; 20];
    hash_bytes.copy_from_slice(&result);

    BlobHash(hash_bytes)
}

/// Trait for content-addressed object storage
pub trait ObjectStore: Send + Sync {
    fn get_tree(&self, hash: &TreeHash) -> Option<Tree>;
    fn get_blob(&self, hash: &BlobHash) -> Option<Vec<u8>>;
    fn put_tree(&mut self, tree: &Tree) -> TreeHash;
    fn put_blob(&mut self, content: &[u8]) -> BlobHash;
    fn has_tree(&self, hash: &TreeHash) -> bool;
    fn has_blob(&self, hash: &BlobHash) -> bool;
}

/// In-memory implementation for testing/prototyping
#[derive(Debug, Default)]
pub struct MemoryObjectStore {
    trees: HashMap<TreeHash, Tree>,
    blobs: HashMap<BlobHash, Vec<u8>>,
}

impl MemoryObjectStore {
    pub fn new() -> Self {
        Self::default()
    }
}

impl ObjectStore for MemoryObjectStore {
    fn get_tree(&self, hash: &TreeHash) -> Option<Tree> {
        self.trees.get(hash).cloned()
    }

    fn get_blob(&self, hash: &BlobHash) -> Option<Vec<u8>> {
        self.blobs.get(hash).cloned()
    }

    fn put_tree(&mut self, tree: &Tree) -> TreeHash {
        let hash = tree.hash();
        self.trees.insert(hash, tree.clone());
        hash
    }

    fn put_blob(&mut self, content: &[u8]) -> BlobHash {
        let hash = hash_blob(content);
        self.blobs.insert(hash, content.to_vec());
        hash
    }

    fn has_tree(&self, hash: &TreeHash) -> bool {
        self.trees.contains_key(hash)
    }

    fn has_blob(&self, hash: &BlobHash) -> bool {
        self.blobs.contains_key(hash)
    }
}
