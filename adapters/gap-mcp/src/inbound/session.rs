//! Bounded legacy `2025-11-25` HTTP session liveness store (`CMP-001`, `INV-01`, `INV-02`).

use std::collections::{HashMap, VecDeque};
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use tokio::time::Instant;
use uuid::Uuid;

#[derive(Debug, Clone, Copy)]
struct SessionEntry {
    last_seen: Instant,
}

#[derive(Debug, Default)]
struct SessionInner {
    order: VecDeque<String>,
    entries: HashMap<String, SessionEntry>,
}

/// Bounded legacy session store holding only session IDs and last-seen timestamps (`INV-01`).
///
/// Never holds any request context, principal credentials, or snapshot reference.
/// Locks are short, synchronous, and never held across `.await` (`INV-02`).
#[derive(Debug)]
pub(crate) struct SessionStore {
    max_sessions: usize,
    idle_ttl: Duration,
    inner: Mutex<SessionInner>,
}

impl SessionStore {
    /// Creates a bounded session store with capacity `max_sessions` and idle timeout `idle_ttl`.
    pub(crate) fn new(max_sessions: usize, idle_ttl: Duration) -> Self {
        Self {
            max_sessions,
            idle_ttl,
            inner: Mutex::new(SessionInner::default()),
        }
    }

    fn lock_inner(&self) -> std::sync::MutexGuard<'_, SessionInner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn purge_expired(state: &mut SessionInner, now: Instant, idle_ttl: Duration) {
        state
            .entries
            .retain(|_, entry| now.saturating_duration_since(entry.last_seen) < idle_ttl);
        state.order.retain(|id| state.entries.contains_key(id));
    }

    /// Mints a new UUIDv7 legacy session ID, evicting expired entries first and then the oldest
    /// live session(s) when `max_sessions` is reached.
    pub(crate) fn mint(&self) -> String {
        let now = Instant::now();
        let mut guard = self.lock_inner();
        Self::purge_expired(&mut guard, now, self.idle_ttl);

        while guard.entries.len() >= self.max_sessions {
            let Some(oldest) = guard.order.pop_front() else {
                guard.entries.clear();
                break;
            };
            guard.entries.remove(&oldest);
        }

        let id = Uuid::now_v7().to_string();
        guard
            .entries
            .insert(id.clone(), SessionEntry { last_seen: now });
        guard.order.push_back(id.clone());
        id
    }

    /// Refreshes `last_seen` if `id` is active and within `idle_ttl`.
    pub(crate) fn touch(&self, id: &str) -> bool {
        let now = Instant::now();
        let mut guard = self.lock_inner();
        Self::purge_expired(&mut guard, now, self.idle_ttl);
        if let Some(entry) = guard.entries.get_mut(id) {
            entry.last_seen = now;
            true
        } else {
            false
        }
    }

    /// Terminates session `id` (`DELETE /mcp`), returning `true` iff `id` was an active session.
    pub(crate) fn remove(&self, id: &str) -> bool {
        let now = Instant::now();
        let mut guard = self.lock_inner();
        Self::purge_expired(&mut guard, now, self.idle_ttl);
        if guard.entries.remove(id).is_some() {
            guard.order.retain(|s| s != id);
            true
        } else {
            false
        }
    }
}
