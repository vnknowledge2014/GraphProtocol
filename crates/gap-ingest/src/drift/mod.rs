//! Hash-pinned connector description/schema drift detection, classification, pinned serving,
//! signature probing, and re-approval clearing (`WP-M4-09`, `ING-052`, `ING-053`, `SEC-082`,
//! `INV-08`).

pub mod classify;
pub mod monitor;

#[cfg(test)]
mod tests;

pub use classify::{
    DriftClassification, DriftClassificationError, DriftKind, PinnedConnectorSnapshot,
    PinnedOperation, classify_connector_drift, pin_connector_description,
};
pub use monitor::{
    DEFAULT_POLL_INTERVAL_SECS, DEFAULT_POLL_JITTER_BPS, DescribePoller, DriftBranchState,
    DriftError, DriftMonitor, DriftPollConfig, DriftPollOutcome, MonitoredBranchConfig,
    PinnedOperationView, ProposalEmissionContext, SignatureProbeResult, SignatureProber,
    compute_jittered_interval,
};
