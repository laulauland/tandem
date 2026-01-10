//! Tandem Core Library
//!
//! Shared data model and synchronization primitives for the Tandem project.

pub mod content;
pub mod model;
pub mod object_store;
pub mod sync;
pub mod types;

// Re-export model types
pub use model::Repository;

// Re-export object store types
pub use object_store::{
    hash_blob, FileMode, MemoryObjectStore, ObjectRef, ObjectStore, Tree, TreeEntry,
};

// Re-export sync types
pub use sync::*;

// Re-export core types
pub use types::{
    BlobHash, Bookmark, BookmarkRules, Change, ChangeId, ChangeRecord, Identity, PresenceInfo,
    TreeHash,
};

// Re-export content types
pub use content::{ContentRequest, ContentResponse};
