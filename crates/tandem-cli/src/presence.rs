use tandem_core::types::{ChangeId, PresenceInfo};
use tandem_core::sync::ForgeDoc;
use chrono::{Utc, Duration};
use std::sync::Arc;
use tokio::sync::RwLock;

/// Presence manager for tracking who's editing what
pub struct PresenceManager {
    doc: Arc<RwLock<ForgeDoc>>,
    user_id: String,
    device: String,
}

impl PresenceManager {
    pub fn new(doc: Arc<RwLock<ForgeDoc>>, user_id: String, device: String) -> Self {
        Self { doc, user_id, device }
    }

    /// Update our presence (call when user edits a change)
    pub async fn update_presence(&self, change_id: &ChangeId) {
        let info = PresenceInfo {
            user_id: self.user_id.clone(),
            change_id: *change_id,
            device: self.device.clone(),
            timestamp: Utc::now(),
        };

        let doc = self.doc.read().await;
        doc.update_presence(&info);
    }

    /// Clear our presence (call when leaving a change)
    pub async fn clear_presence(&self) {
        let doc = self.doc.read().await;
        doc.remove_presence(&self.user_id);
    }

    /// Get all active presence (excluding stale entries > 5 min)
    pub async fn get_active_presence(&self) -> Vec<PresenceInfo> {
        let doc = self.doc.read().await;
        let all = doc.get_all_presence();
        let cutoff = Utc::now() - Duration::minutes(5);

        all.into_iter()
            .filter(|p| p.timestamp > cutoff)
            .collect()
    }

    /// Check if anyone else is editing this change
    pub async fn check_conflict(&self, change_id: &ChangeId) -> Vec<PresenceInfo> {
        self.get_active_presence().await
            .into_iter()
            .filter(|p| &p.change_id == change_id && p.user_id != self.user_id)
            .collect()
    }
}

/// Format presence warning message
pub fn format_presence_warning(conflicts: &[PresenceInfo]) -> String {
    if conflicts.is_empty() {
        return String::new();
    }

    if conflicts.len() == 1 {
        let p = &conflicts[0];
        format!(
            "⚠ This change is currently being edited by {}@{}",
            p.user_id, p.device
        )
    } else {
        let users: Vec<String> = conflicts
            .iter()
            .map(|p| format!("{}@{}", p.user_id, p.device))
            .collect();
        format!(
            "⚠ This change is currently being edited by: {}",
            users.join(", ")
        )
    }
}

/// Format presence info for jj log output
pub fn format_log_presence(presences: &[PresenceInfo], change_id: &ChangeId) -> Option<String> {
    let editing: Vec<&PresenceInfo> = presences
        .iter()
        .filter(|p| &p.change_id == change_id)
        .collect();

    if editing.is_empty() {
        return None;
    }

    if editing.len() == 1 {
        Some(format!("({} editing)", editing[0].user_id))
    } else {
        let count = editing.len();
        Some(format!("({} users editing)", count))
    }
}

/// Prompt user to continue when there's a conflict
pub fn prompt_continue(warning: &str) -> bool {
    use std::io::{self, Write};

    println!("{}", warning);
    print!("Continue anyway? [y/N] ");
    io::stdout().flush().unwrap();

    let mut input = String::new();
    io::stdin().read_line(&mut input).unwrap();

    matches!(input.trim().to_lowercase().as_str(), "y" | "yes")
}
