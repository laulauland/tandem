//! Synchronization primitives using Yrs CRDT

use crate::types::{ChangeId, ChangeRecord, PresenceInfo};
use serde_json;
use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use yrs::updates::decoder::Decode;
use yrs::updates::encoder::Encode;
use yrs::{Doc, Map, ReadTxn, StateVector, Transact, TransactionMut, Update, WriteTxn};

/// Y.Doc structure for forge sync
///
/// Structure:
/// - Y.Map("changes") → {record_id: ChangeRecord}
/// - Y.Map("bookmarks") → {name: ChangeId}
/// - Y.Map("presence") → {user_id: PresenceInfo}
/// - Subdocuments keyed by hash → Y.Map("data") → base64-encoded blob content
pub struct ForgeDoc {
    doc: Doc,
    subdocs: Arc<RwLock<HashMap<String, Arc<Doc>>>>,
}

impl ForgeDoc {
    pub fn new() -> Self {
        Self {
            doc: Doc::new(),
            subdocs: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Get the underlying Y.Doc for sync operations
    pub fn doc(&self) -> &Doc {
        &self.doc
    }

    // Change operations

    /// Insert a change record into the CRDT
    pub fn insert_change(&self, record: &ChangeRecord) {
        let mut txn = self.doc.transact_mut();
        let changes = txn.get_or_insert_map("changes");
        let record_id = record.record_id.to_string();
        let json = serde_json::to_string(record).expect("Failed to serialize ChangeRecord");
        changes.insert(&mut txn, record_id, json);
    }

    /// Get all records for a specific change_id (handles divergence)
    pub fn get_change_records(&self, change_id: &ChangeId) -> Vec<ChangeRecord> {
        let txn = self.doc.transact();
        let changes = match txn.get_map("changes") {
            Some(map) => map,
            None => return Vec::new(),
        };

        let mut records = Vec::new();
        for (_key, value) in changes.iter(&txn) {
            if let Ok(json) = value.cast::<String>() {
                if let Ok(record) = serde_json::from_str::<ChangeRecord>(&json) {
                    if record.change_id == *change_id {
                        records.push(record);
                    }
                }
            }
        }
        records
    }

    /// Get all change records
    pub fn get_all_change_records(&self) -> Vec<ChangeRecord> {
        let txn = self.doc.transact();
        let changes = match txn.get_map("changes") {
            Some(map) => map,
            None => return Vec::new(),
        };

        let mut records = Vec::new();
        for (_key, value) in changes.iter(&txn) {
            if let Ok(json) = value.cast::<String>() {
                if let Ok(record) = serde_json::from_str::<ChangeRecord>(&json) {
                    records.push(record);
                }
            }
        }
        records
    }

    /// Mark a change record as hidden (for abandoned changes)
    pub fn mark_change_hidden(&self, record_id: &str) {
        let mut txn = self.doc.transact_mut();
        let changes = txn.get_or_insert_map("changes");

        if let Some(value) = changes.get(&txn, record_id) {
            if let Ok(json) = value.cast::<String>() {
                if let Ok(mut record) = serde_json::from_str::<ChangeRecord>(&json) {
                    record.visible = false;
                    let updated_json = serde_json::to_string(&record)
                        .expect("Failed to serialize ChangeRecord");
                    changes.insert(&mut txn, record_id, updated_json);
                }
            }
        }
    }

    // Bookmark operations

    /// Set a bookmark to point at a change
    pub fn set_bookmark(&self, name: &str, target: &ChangeId) {
        let mut txn = self.doc.transact_mut();
        let bookmarks = txn.get_or_insert_map("bookmarks");
        let target_str = target.to_string();
        bookmarks.insert(&mut txn, name, target_str);
    }

    /// Get the change a bookmark points to
    pub fn get_bookmark(&self, name: &str) -> Option<ChangeId> {
        let txn = self.doc.transact();
        let bookmarks = txn.get_map("bookmarks")?;
        let value = bookmarks.get(&txn, name)?;
        let target_str = value.cast::<String>().ok()?;
        target_str.parse().ok()
    }

    /// Get all bookmarks
    pub fn get_all_bookmarks(&self) -> Vec<(String, ChangeId)> {
        let txn = self.doc.transact();
        let bookmarks = match txn.get_map("bookmarks") {
            Some(map) => map,
            None => return Vec::new(),
        };

        let mut result = Vec::new();
        for (key, value) in bookmarks.iter(&txn) {
            if let Ok(target_str) = value.cast::<String>() {
                if let Ok(change_id) = target_str.parse() {
                    result.push((key.to_string(), change_id));
                }
            }
        }
        result
    }

    /// Remove a bookmark
    pub fn remove_bookmark(&self, name: &str) {
        let mut txn = self.doc.transact_mut();
        let bookmarks = txn.get_or_insert_map("bookmarks");
        bookmarks.remove(&mut txn, name);
    }

    // Presence operations

    /// Update presence information for a user
    pub fn update_presence(&self, info: &PresenceInfo) {
        let mut txn = self.doc.transact_mut();
        let presence = txn.get_or_insert_map("presence");
        let json = serde_json::to_string(info).expect("Failed to serialize PresenceInfo");
        presence.insert(&mut txn, info.user_id.as_str(), json);
    }

    /// Get presence information for a user
    pub fn get_presence(&self, user_id: &str) -> Option<PresenceInfo> {
        let txn = self.doc.transact();
        let presence = txn.get_map("presence")?;
        let value = presence.get(&txn, user_id)?;
        let json = value.cast::<String>().ok()?;
        serde_json::from_str(&json).ok()
    }

    /// Get all presence information
    pub fn get_all_presence(&self) -> Vec<PresenceInfo> {
        let txn = self.doc.transact();
        let presence = match txn.get_map("presence") {
            Some(map) => map,
            None => return Vec::new(),
        };

        let mut result = Vec::new();
        for (_key, value) in presence.iter(&txn) {
            if let Ok(json) = value.cast::<String>() {
                if let Ok(info) = serde_json::from_str::<PresenceInfo>(&json) {
                    result.push(info);
                }
            }
        }
        result
    }

    /// Remove presence information for a user
    pub fn remove_presence(&self, user_id: &str) {
        let mut txn = self.doc.transact_mut();
        let presence = txn.get_or_insert_map("presence");
        presence.remove(&mut txn, user_id);
    }

    // Content/subdocument operations

    /// Check if content for a hash is available locally
    pub fn has_content(&self, hash: &str) -> bool {
        let subdocs = self.subdocs.read().unwrap();
        if let Some(subdoc) = subdocs.get(hash) {
            let txn = subdoc.transact();
            if let Some(data_map) = txn.get_map("data") {
                return data_map.get(&txn, "content").is_some();
            }
        }
        false
    }

    /// Get content if available locally (doesn't fetch)
    pub fn get_content(&self, hash: &str) -> Option<Vec<u8>> {
        let subdocs = self.subdocs.read().unwrap();
        let subdoc = subdocs.get(hash)?;
        let txn = subdoc.transact();
        let data_map = txn.get_map("data")?;
        let base64_str = data_map.get(&txn, "content")?.cast::<String>().ok()?;
        base64::Engine::decode(&base64::engine::general_purpose::STANDARD, &base64_str).ok()
    }

    /// Store content locally (for content we've fetched or created)
    pub fn put_content(&self, hash: &str, content: Vec<u8>) {
        let subdoc = self.get_subdoc(hash);
        let mut txn = subdoc.transact_mut();
        let data_map = txn.get_or_insert_map("data");
        let base64_str = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &content);
        data_map.insert(&mut txn, "content", base64_str);
    }

    /// Get a subdocument for a specific hash (creates if doesn't exist)
    /// The subdocument can be synced independently
    pub fn get_subdoc(&self, hash: &str) -> Arc<Doc> {
        let mut subdocs = self.subdocs.write().unwrap();
        subdocs
            .entry(hash.to_string())
            .or_insert_with(|| Arc::new(Doc::new()))
            .clone()
    }

    /// List all subdocument hashes we have locally
    pub fn list_local_content(&self) -> Vec<String> {
        let subdocs = self.subdocs.read().unwrap();
        subdocs.keys().cloned().collect()
    }

    /// Encode subdoc state vector for requesting content
    pub fn encode_subdoc_state_vector(&self, hash: &str) -> Option<Vec<u8>> {
        let subdocs = self.subdocs.read().unwrap();
        let subdoc = subdocs.get(hash)?;
        let txn = subdoc.transact();
        Some(txn.state_vector().encode_v1())
    }

    /// Apply update to a subdocument
    pub fn apply_subdoc_update(&self, hash: &str, update: &[u8]) -> Result<(), yrs::encoding::read::Error> {
        let subdoc = self.get_subdoc(hash);
        let mut txn = subdoc.transact_mut();
        let update = Update::decode_v1(update)?;
        let _result = txn.apply_update(update);
        Ok(())
    }

    // Sync protocol

    /// Encode the current state vector for sync
    pub fn encode_state_vector(&self) -> Vec<u8> {
        let txn = self.doc.transact();
        txn.state_vector().encode_v1()
    }

    /// Encode an update based on a remote state vector
    pub fn encode_update_from(&self, state_vector: &[u8]) -> Vec<u8> {
        let txn = self.doc.transact();
        let remote_sv = StateVector::decode_v1(state_vector)
            .expect("Failed to decode state vector");
        txn.encode_diff_v1(&remote_sv)
    }

    /// Apply an update from a remote peer
    pub fn apply_update(&self, update: &[u8]) -> Result<(), yrs::encoding::read::Error> {
        let mut txn = self.doc.transact_mut();
        let update = Update::decode_v1(update)?;
        let _result = txn.apply_update(update);
        Ok(())
    }

    // Transactions for atomic operations

    /// Execute a function within a transaction for atomic updates
    pub fn transact<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&mut TransactionMut) -> R,
    {
        let mut txn = self.doc.transact_mut();
        f(&mut txn)
    }
}

impl Default for ForgeDoc {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Identity, TreeHash};
    use chrono::Utc;
    use uuid::Uuid;

    fn create_test_record(change_id: ChangeId) -> ChangeRecord {
        ChangeRecord {
            record_id: Uuid::new_v4(),
            change_id,
            tree: TreeHash([0u8; 20]),
            parents: vec![],
            description: "Test change".to_string(),
            author: Identity {
                email: "test@example.com".to_string(),
                name: Some("Test User".to_string()),
            },
            timestamp: Utc::now(),
            visible: true,
        }
    }

    #[test]
    fn test_insert_and_retrieve_change_records() {
        let doc = ForgeDoc::new();
        let change_id = ChangeId::new();
        let record = create_test_record(change_id);

        doc.insert_change(&record);

        let retrieved = doc.get_change_records(&change_id);
        assert_eq!(retrieved.len(), 1);
        assert_eq!(retrieved[0].change_id, change_id);
        assert_eq!(retrieved[0].record_id, record.record_id);
    }

    #[test]
    fn test_multiple_records_same_change_id() {
        let doc = ForgeDoc::new();
        let change_id = ChangeId::new();

        let record1 = create_test_record(change_id);
        let record2 = create_test_record(change_id);

        doc.insert_change(&record1);
        doc.insert_change(&record2);

        let retrieved = doc.get_change_records(&change_id);
        assert_eq!(retrieved.len(), 2);

        let record_ids: Vec<_> = retrieved.iter().map(|r| r.record_id).collect();
        assert!(record_ids.contains(&record1.record_id));
        assert!(record_ids.contains(&record2.record_id));
    }

    #[test]
    fn test_get_all_change_records() {
        let doc = ForgeDoc::new();
        let change_id1 = ChangeId::new();
        let change_id2 = ChangeId::new();

        let record1 = create_test_record(change_id1);
        let record2 = create_test_record(change_id2);

        doc.insert_change(&record1);
        doc.insert_change(&record2);

        let all = doc.get_all_change_records();
        assert_eq!(all.len(), 2);
    }

    #[test]
    fn test_mark_change_hidden() {
        let doc = ForgeDoc::new();
        let change_id = ChangeId::new();
        let record = create_test_record(change_id);
        let record_id = record.record_id.to_string();

        doc.insert_change(&record);
        doc.mark_change_hidden(&record_id);

        let retrieved = doc.get_change_records(&change_id);
        assert_eq!(retrieved.len(), 1);
        assert_eq!(retrieved[0].visible, false);
    }

    #[test]
    fn test_bookmark_operations() {
        let doc = ForgeDoc::new();
        let change_id = ChangeId::new();

        doc.set_bookmark("main", &change_id);

        let retrieved = doc.get_bookmark("main");
        assert_eq!(retrieved, Some(change_id));

        let all = doc.get_all_bookmarks();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].0, "main");
        assert_eq!(all[0].1, change_id);

        doc.remove_bookmark("main");
        assert_eq!(doc.get_bookmark("main"), None);
    }

    #[test]
    fn test_presence_operations() {
        let doc = ForgeDoc::new();
        let change_id = ChangeId::new();
        let info = PresenceInfo {
            user_id: "user1".to_string(),
            change_id,
            device: "laptop".to_string(),
            timestamp: Utc::now(),
        };

        doc.update_presence(&info);

        let retrieved = doc.get_presence("user1");
        assert!(retrieved.is_some());
        assert_eq!(retrieved.unwrap().user_id, "user1");

        let all = doc.get_all_presence();
        assert_eq!(all.len(), 1);

        doc.remove_presence("user1");
        assert_eq!(doc.get_presence("user1"), None);
    }

    #[test]
    fn test_sync_between_docs() {
        let doc1 = ForgeDoc::new();
        let doc2 = ForgeDoc::new();

        let change_id = ChangeId::new();
        let record = create_test_record(change_id);

        // Insert into doc1
        doc1.insert_change(&record);
        doc1.set_bookmark("main", &change_id);

        // Sync from doc1 to doc2
        let sv2 = doc2.encode_state_vector();
        let update = doc1.encode_update_from(&sv2);
        doc2.apply_update(&update).unwrap();

        // Verify doc2 has the data
        let retrieved = doc2.get_change_records(&change_id);
        assert_eq!(retrieved.len(), 1);
        assert_eq!(retrieved[0].change_id, change_id);

        let bookmark = doc2.get_bookmark("main");
        assert_eq!(bookmark, Some(change_id));
    }

    #[test]
    fn test_bidirectional_sync() {
        let doc1 = ForgeDoc::new();
        let doc2 = ForgeDoc::new();

        let change_id1 = ChangeId::new();
        let change_id2 = ChangeId::new();
        let record1 = create_test_record(change_id1);
        let record2 = create_test_record(change_id2);

        // Insert different records into each doc
        doc1.insert_change(&record1);
        doc2.insert_change(&record2);

        // Sync doc1 -> doc2
        let sv2 = doc2.encode_state_vector();
        let update1 = doc1.encode_update_from(&sv2);
        doc2.apply_update(&update1).unwrap();

        // Sync doc2 -> doc1
        let sv1 = doc1.encode_state_vector();
        let update2 = doc2.encode_update_from(&sv1);
        doc1.apply_update(&update2).unwrap();

        // Both docs should have both records
        let all1 = doc1.get_all_change_records();
        let all2 = doc2.get_all_change_records();
        assert_eq!(all1.len(), 2);
        assert_eq!(all2.len(), 2);
    }

    #[test]
    fn test_atomic_transaction() {
        let doc = ForgeDoc::new();
        let change_id1 = ChangeId::new();
        let change_id2 = ChangeId::new();

        // Use transaction to atomically insert multiple records
        doc.transact(|txn| {
            let changes = txn.get_or_insert_map("changes");

            let record1 = create_test_record(change_id1);
            let json1 = serde_json::to_string(&record1).unwrap();
            changes.insert(txn, record1.record_id.to_string(), json1);

            let record2 = create_test_record(change_id2);
            let json2 = serde_json::to_string(&record2).unwrap();
            changes.insert(txn, record2.record_id.to_string(), json2);
        });

        let all = doc.get_all_change_records();
        assert_eq!(all.len(), 2);
    }

    #[test]
    fn test_state_vector_encoding() {
        let doc = ForgeDoc::new();
        let change_id = ChangeId::new();
        let record = create_test_record(change_id);

        doc.insert_change(&record);

        let sv = doc.encode_state_vector();
        assert!(!sv.is_empty());
    }

    #[test]
    fn test_put_and_get_content() {
        let doc = ForgeDoc::new();
        let hash = "abc123";
        let content = b"Hello, World!".to_vec();

        doc.put_content(hash, content.clone());

        assert!(doc.has_content(hash));
        let retrieved = doc.get_content(hash);
        assert_eq!(retrieved, Some(content));
    }

    #[test]
    fn test_has_content_returns_false_for_missing() {
        let doc = ForgeDoc::new();
        assert!(!doc.has_content("nonexistent"));
    }

    #[test]
    fn test_get_content_returns_none_for_missing() {
        let doc = ForgeDoc::new();
        assert_eq!(doc.get_content("nonexistent"), None);
    }

    #[test]
    fn test_list_local_content() {
        let doc = ForgeDoc::new();
        let hash1 = "hash1";
        let hash2 = "hash2";

        doc.put_content(hash1, b"content1".to_vec());
        doc.put_content(hash2, b"content2".to_vec());

        let mut hashes = doc.list_local_content();
        hashes.sort();

        assert_eq!(hashes.len(), 2);
        assert!(hashes.contains(&hash1.to_string()));
        assert!(hashes.contains(&hash2.to_string()));
    }

    #[test]
    fn test_get_subdoc_creates_new() {
        let doc = ForgeDoc::new();
        let hash = "test_hash";

        let subdoc = doc.get_subdoc(hash);
        assert!(Arc::strong_count(&subdoc) >= 1);

        // Getting the same subdoc returns the same instance
        let subdoc2 = doc.get_subdoc(hash);
        assert!(Arc::ptr_eq(&subdoc, &subdoc2));
    }

    #[test]
    fn test_encode_subdoc_state_vector() {
        let doc = ForgeDoc::new();
        let hash = "test_hash";

        doc.put_content(hash, b"some content".to_vec());

        let sv = doc.encode_subdoc_state_vector(hash);
        assert!(sv.is_some());
        assert!(!sv.unwrap().is_empty());
    }

    #[test]
    fn test_encode_subdoc_state_vector_nonexistent() {
        let doc = ForgeDoc::new();
        let sv = doc.encode_subdoc_state_vector("nonexistent");
        assert_eq!(sv, None);
    }

    #[test]
    fn test_subdoc_sync_between_docs() {
        let doc1 = ForgeDoc::new();
        let doc2 = ForgeDoc::new();
        let hash = "sync_test";
        let content = b"sync this content".to_vec();

        // Put content in doc1
        doc1.put_content(hash, content.clone());

        // Get state vector from doc2 for this hash
        let _subdoc2 = doc2.get_subdoc(hash);
        let sv2 = doc2.encode_subdoc_state_vector(hash).unwrap();

        // Generate update from doc1
        let subdoc1 = doc1.get_subdoc(hash);
        let txn1 = subdoc1.transact();
        let sv2_decoded = StateVector::decode_v1(&sv2).unwrap();
        let update = txn1.encode_diff_v1(&sv2_decoded);
        drop(txn1);

        // Apply update to doc2
        doc2.apply_subdoc_update(hash, &update).unwrap();

        // Verify content is synced
        assert!(doc2.has_content(hash));
        let retrieved = doc2.get_content(hash);
        assert_eq!(retrieved, Some(content));
    }

    #[test]
    fn test_apply_subdoc_update_creates_subdoc_if_missing() {
        let doc = ForgeDoc::new();
        let hash = "new_hash";

        // Create a dummy update (empty update)
        let temp_doc = Doc::new();
        let txn = temp_doc.transact();
        let sv = txn.state_vector();
        let update = txn.encode_diff_v1(&sv);
        drop(txn);

        // Should not error even though subdoc doesn't exist yet
        let result = doc.apply_subdoc_update(hash, &update);
        assert!(result.is_ok());

        // Subdoc should now exist
        assert!(doc.list_local_content().contains(&hash.to_string()));
    }

    #[test]
    fn test_content_base64_encoding() {
        let doc = ForgeDoc::new();
        let hash = "binary_test";
        let content = vec![0u8, 1, 2, 3, 255, 254, 253];

        doc.put_content(hash, content.clone());

        let retrieved = doc.get_content(hash);
        assert_eq!(retrieved, Some(content));
    }
}
