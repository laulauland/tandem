//! One workspace, one writer.
//!
//! Invariant 1 of the design doc: a workspace has at most one client writing
//! to it at a time. The server is what makes that true, and this is the whole
//! of the mechanism — a map from workspace to the client that currently holds
//! the role, and the moment that hold runs out.
//!
//! The hold expires rather than being released, because the thing it protects
//! against is a client that stopped answering: a crashed agent cannot hand the
//! role back, and a workspace that stayed locked until it did would need an
//! operator to unwedge it. So a holder keeps the role by asking for it again
//! before its own expiry, and a client that stops asking loses it without
//! anybody having to notice.
//!
//! The clock is `Instant`: the server only ever compares this against itself,
//! and a wall clock that a daemon steps backwards would hand two clients the
//! same workspace.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// How long a claim lasts when the caller does not say.
pub const DEFAULT_WRITER_TTL: Duration = Duration::from_secs(30);

/// The longest a claim may last. A holder that dies takes the workspace with
/// it for this long, so it is minutes rather than hours.
pub const MAX_WRITER_TTL: Duration = Duration::from_secs(10 * 60);

/// The role as it stands after a successful claim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriterRole {
    pub workspace_id: String,
    /// The client holding it, named by the client itself.
    pub holder: String,
    /// What is left of this hold.
    pub expires_in: Duration,
}

/// Somebody else holds the role, and their hold has not run out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriterConflict {
    pub workspace_id: String,
    pub holder: String,
    pub expires_in: Duration,
}

impl std::fmt::Display for WriterConflict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "workspace {} already has a writer ({}); its claim expires in {} ms",
            self.workspace_id,
            self.holder,
            self.expires_in.as_millis()
        )
    }
}

impl std::error::Error for WriterConflict {}

struct Held {
    holder: String,
    expires_at: Instant,
}

/// Who currently writes each workspace.
#[derive(Default)]
pub struct WriterRoles {
    held: Mutex<HashMap<String, Held>>,
}

impl WriterRoles {
    pub fn new() -> Self {
        Self::default()
    }

    /// Take the writer role for `workspace_id`, or keep it.
    ///
    /// The same holder asking again is a renewal and always succeeds: that is
    /// how a client keeps the role. A different holder succeeds only once the
    /// current hold has run out.
    pub fn claim(
        &self,
        workspace_id: &str,
        holder: &str,
        ttl: Duration,
    ) -> Result<WriterRole, WriterConflict> {
        let ttl = ttl.min(MAX_WRITER_TTL);
        let now = Instant::now();
        let mut held = self.held.lock().expect("writer role lock");
        held.retain(|_, entry| entry.expires_at > now);

        if let Some(current) = held.get(workspace_id) {
            if current.holder != holder {
                return Err(WriterConflict {
                    workspace_id: workspace_id.to_string(),
                    holder: current.holder.clone(),
                    expires_in: current.expires_at.saturating_duration_since(now),
                });
            }
        }

        held.insert(
            workspace_id.to_string(),
            Held {
                holder: holder.to_string(),
                expires_at: now + ttl,
            },
        );
        Ok(WriterRole {
            workspace_id: workspace_id.to_string(),
            holder: holder.to_string(),
            expires_in: ttl,
        })
    }

    /// Who holds the role right now, if anybody does.
    pub fn current(&self, workspace_id: &str) -> Option<WriterRole> {
        let now = Instant::now();
        let held = self.held.lock().expect("writer role lock");
        let entry = held.get(workspace_id)?;
        if entry.expires_at <= now {
            return None;
        }
        Some(WriterRole {
            workspace_id: workspace_id.to_string(),
            holder: entry.holder.clone(),
            expires_in: entry.expires_at.saturating_duration_since(now),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unheld_workspace_goes_to_whoever_asks() {
        let roles = WriterRoles::new();
        let role = roles
            .claim("agent-a", "daemon-1", DEFAULT_WRITER_TTL)
            .expect("nobody held it");
        assert_eq!(role.holder, "daemon-1");
        assert_eq!(
            roles.current("agent-a").map(|r| r.holder).as_deref(),
            Some("daemon-1")
        );
    }

    #[test]
    fn the_holder_renews_and_a_second_client_is_refused() {
        let roles = WriterRoles::new();
        roles
            .claim("agent-a", "daemon-1", DEFAULT_WRITER_TTL)
            .expect("first claim");

        roles
            .claim("agent-a", "daemon-1", DEFAULT_WRITER_TTL)
            .expect("the holder renews");

        let conflict = roles
            .claim("agent-a", "daemon-2", DEFAULT_WRITER_TTL)
            .expect_err("a second client is refused");
        assert_eq!(conflict.holder, "daemon-1");
        assert_eq!(conflict.workspace_id, "agent-a");
    }

    #[test]
    fn an_unrenewed_claim_expires_and_the_next_client_takes_it() {
        let roles = WriterRoles::new();
        roles
            .claim("agent-a", "daemon-1", Duration::from_millis(0))
            .expect("first claim");

        assert_eq!(roles.current("agent-a"), None, "the hold has run out");
        let role = roles
            .claim("agent-a", "daemon-2", DEFAULT_WRITER_TTL)
            .expect("an expired claim blocks nobody");
        assert_eq!(role.holder, "daemon-2");
    }

    #[test]
    fn one_workspace_being_held_says_nothing_about_another() {
        let roles = WriterRoles::new();
        roles
            .claim("agent-a", "daemon-1", DEFAULT_WRITER_TTL)
            .expect("agent-a");
        roles
            .claim("agent-b", "daemon-2", DEFAULT_WRITER_TTL)
            .expect("agent-b is a different workspace");
        let conflict = roles
            .claim("agent-a", "daemon-2", DEFAULT_WRITER_TTL)
            .expect_err("holding agent-b does not hand over agent-a");
        assert_eq!(conflict.holder, "daemon-1");
    }

    #[test]
    fn a_ttl_past_the_ceiling_is_clamped() {
        let roles = WriterRoles::new();
        let role = roles
            .claim("agent-a", "daemon-1", MAX_WRITER_TTL * 3)
            .expect("claim");
        assert_eq!(role.expires_in, MAX_WRITER_TTL);
    }
}
