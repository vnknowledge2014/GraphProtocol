//! Bounded legacy `2025-11-25` HTTP session liveness store (`CMP-001`, `INV-01`).

use std::time::Duration;

/// Bounded legacy session store holding only session IDs and last-seen timestamps (`INV-01`).
#[derive(Debug)]
pub(crate) struct SessionStore {
    max_sessions: usize,
    idle_ttl: Duration,
}

impl SessionStore {
    pub(crate) fn new(max_sessions: usize, idle_ttl: Duration) -> Self {
        Self {
            max_sessions,
            idle_ttl,
        }
    }

    #[allow(dead_code)]
    pub(crate) const fn max_sessions(&self) -> usize {
        self.max_sessions
    }

    #[allow(dead_code)]
    pub(crate) const fn idle_ttl(&self) -> Duration {
        self.idle_ttl
    }
}
