//! Proposal state machine, persistent `ContextStore` storage, GraphQL admin ops,
//! unmapped domain event intake, and allow-listed `DiscoveryWorker` (`WP-M4-07`).

pub mod audit_adapter;
pub mod discovery;
pub mod graphql;
pub mod service;
pub mod types;

#[cfg(test)]
mod tests;

pub use audit_adapter::ContextStoreAuditLog;
pub use discovery::{
    DiscoveryWorker, IntakeOutcome, RegistrySourceEntry, UnmappedDomainEvent, UnmappedEventIntake,
    UnmappedIntakeConfig, insufficient_evidence_error, redact_query,
};
pub use graphql::{
    DecideProposalInput, decide_proposal_graphql, proposals_graphql, propose_source_graphql,
    render_proposal_cisg, render_proposal_connection_cisg,
};
pub use service::{
    ProposalService, authorize_shadow_header, canonicalize_location, format_protocol,
    format_trust_tier, parse_protocol, parse_trust_tier,
};
pub use types::{
    ADMIN_INGEST_SCOPE, ADMIN_SCHEMA_SCOPE, AdminCaller, ApprovalDecision, ApprovalPolicyConfig,
    DEFAULT_PROPOSALS_PAGE_SIZE, GateCheckResult, MAX_PROPOSALS_PAGE_SIZE,
    PROPOSAL_APPROVAL_TTL_SECS, PROPOSAL_WATCH_WINDOW_SECS, PageInfo, Proposal, ProposalConnection,
    ProposalError, ProposalFilter, ProposalReport, ProposalStatus, ProposeSourceInput,
    UNMAPPED_QUERY_MAX_TTL_SECS, UNMAPPED_RATE_LIMIT_PER_MIN, UNMAPPED_SINGLEFLIGHT_WINDOW_SECS,
};
