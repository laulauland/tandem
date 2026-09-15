//! Bounded, process-local admission for hosted repository and workspace setup.
use std::collections::HashMap;
use std::time::{Duration, Instant};

const WINDOW: Duration = Duration::from_secs(60);

pub(crate) struct CloneLimit {
    started: Instant,
    per_owner: u32,
    host_limit: u32,
    admitted: u32,
    owners: HashMap<String, u32>,
}

impl CloneLimit {
    pub(crate) fn new(now: Instant, per_owner: u32, host_limit: u32) -> Self {
        Self {
            started: now,
            per_owner,
            host_limit,
            admitted: 0,
            owners: HashMap::new(),
        }
    }

    pub(crate) fn admit(&mut self, owner: &str, now: Instant) -> Result<(), Duration> {
        let elapsed = now.saturating_duration_since(self.started);
        if elapsed >= WINDOW {
            self.started = now;
            self.admitted = 0;
            self.owners.clear();
        }
        if self.admitted >= self.host_limit
            || self.owners.get(owner).copied().unwrap_or(0) >= self.per_owner
        {
            return Err(WINDOW.saturating_sub(now.saturating_duration_since(self.started)));
        }
        // Only admitted owners enter the map, bounding it by the host limit.
        *self.owners.entry(owner.to_owned()).or_default() += 1;
        self.admitted += 1;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owner_and_host_budgets_expire_without_extending_on_rejection() {
        let now = Instant::now();
        let mut limit = CloneLimit::new(now, 2, 3);
        assert!(limit.admit("one", now).is_ok());
        assert!(limit.admit("one", now).is_ok());
        assert_eq!(
            limit.admit("one", now + Duration::from_secs(7)),
            Err(Duration::from_secs(53))
        );
        assert!(limit.admit("two", now).is_ok());
        assert_eq!(limit.admit("three", now), Err(WINDOW));
        assert!(limit.admit("one", now + WINDOW).is_ok());
        assert!(limit.admit("three", now + WINDOW).is_ok());
    }

    #[test]
    fn rotating_owners_cannot_grow_state_after_host_budget_is_spent() {
        let now = Instant::now();
        let mut limit = CloneLimit::new(now, 1, 4);
        for index in 0..1000 {
            assert_eq!(limit.admit(&index.to_string(), now).is_ok(), index < 4);
        }
        assert_eq!(limit.owners.len(), 4);
    }
}
