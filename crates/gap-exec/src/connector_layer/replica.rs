//! Per-branch replica type: one client + bulkhead + telemetry slot per physical endpoint (`RTE-073`, `RTE-078`).

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;

use gap_balance::slot::{SlotParams, TelemetrySlot};
use gap_ngc_sdk::connector::is_sensitive_metadata_key;
use tokio::sync::Semaphore;

use super::BranchClient;

/// Specification for one replica when calling [`super::ConnectorRegistryBuilder::register_replicas`].
pub struct ReplicaSpec {
    /// Live connector client for this endpoint.
    pub client: Arc<dyn BranchClient>,
    /// Static per-call metadata (e.g. vault-resolved connector credentials; redacted in `Debug`, `INV-15`).
    pub metadata: BTreeMap<String, String>,
}

impl fmt::Debug for ReplicaSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ReplicaSpec").finish_non_exhaustive()
    }
}

/// One live endpoint of a registered branch (`RTE-073`, `INV-02`, `INV-12`).
pub struct Replica {
    /// Live connector client.
    pub client: Arc<dyn BranchClient>,
    /// Non-queueing in-flight bulkhead (`INV-02`).
    pub bulkhead: Arc<Semaphore>,
    /// Shared telemetry slot (EWMA, breaker, retry budget).
    pub slot: Arc<TelemetrySlot>,
    /// Stable tiebreak UUIDv7 for deterministic P2C (`RTE-079`).
    pub tiebreak: gap_core::ids::BranchUid,
    /// Static outbound call metadata for this replica.
    pub metadata: BTreeMap<String, String>,
    /// Cached `max_in_flight` from the slot params (slot field is `pub(crate)`).
    pub max_in_flight: u32,
    /// Cached label from the slot params (slot field is `pub(crate)`).
    pub label: std::sync::Arc<str>,
}

impl fmt::Debug for Replica {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Replica")
            .field("tiebreak", &self.tiebreak)
            .field("max_in_flight", &self.max_in_flight)
            .field("available_permits", &self.bulkhead.available_permits())
            .field("metadata", &RedactedMeta(&self.metadata))
            .finish()
    }
}

impl Replica {
    /// Builds a new replica from a tiebreak uid, slot params, and spec.
    pub fn new(tiebreak: gap_core::ids::BranchUid, params: SlotParams, spec: ReplicaSpec) -> Self {
        let permits = usize::try_from(params.max_in_flight).unwrap_or(usize::MAX);
        let max_in_flight = params.max_in_flight;
        let label = Arc::clone(&params.label);
        Self {
            client: spec.client,
            bulkhead: Arc::new(Semaphore::new(permits)),
            slot: Arc::new(TelemetrySlot::new(params)),
            tiebreak,
            metadata: spec.metadata,
            max_in_flight,
            label,
        }
    }

    /// Returns all sensitive metadata values registered for this replica (`INV-15`).
    pub fn sensitive_values(&self) -> Vec<&str> {
        let mut out = Vec::new();
        for (k, v) in &self.metadata {
            if is_sensitive_metadata_key(k) && !v.is_empty() {
                out.push(v.as_str());
                if let Some((_scheme, rest)) = v.split_once(' ') {
                    let trimmed = rest.trim();
                    if !trimmed.is_empty() {
                        out.push(trimmed);
                    }
                }
            }
        }
        out
    }
}

struct RedactedMeta<'a>(&'a BTreeMap<String, String>);

impl fmt::Debug for RedactedMeta<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut map = f.debug_map();
        for (k, v) in self.0 {
            if is_sensitive_metadata_key(k) {
                map.entry(k, &"***");
            } else {
                map.entry(k, v);
            }
        }
        map.finish()
    }
}
