//! GraphQL admin operation handlers for `gap.proposals`, `gapMutation.proposeSource`, and
//! `gapMutation.decideProposal` (`WP-M4-07`; `spec/02-gap-ql.md` §3, `ING-001`..`ING-003`, `RUN-025`).

use std::collections::BTreeMap;

use gap_core::error::{ErrorCode, GapError};
use gap_core::ids::{ProposalId, TenantScope};

use crate::cisg::CisgValue;
use crate::proposals::service::{ProposalService, format_protocol, format_trust_tier};
use crate::proposals::types::{
    ADMIN_INGEST_SCOPE, ADMIN_SCHEMA_SCOPE, AdminCaller, ApprovalDecision, Proposal,
    ProposalConnection, ProposalFilter, ProposeSourceInput,
};

/// Serializes a [`Proposal`] into its canonical GraphQL JSON AST matching `spec/02-gap-ql.md` §3.
#[must_use]
pub fn render_proposal_cisg(proposal: &Proposal) -> CisgValue {
    let mut map = BTreeMap::new();
    map.insert("id".to_owned(), CisgValue::String(proposal.id.to_string()));
    map.insert(
        "status".to_owned(),
        CisgValue::String(proposal.status.as_str().to_owned()),
    );
    map.insert(
        "branch".to_owned(),
        match &proposal.branch {
            Some(b) => CisgValue::String(b.as_str().to_owned()),
            None => CisgValue::Null,
        },
    );
    map.insert(
        "kind".to_owned(),
        CisgValue::String(format_protocol(proposal.kind).to_owned()),
    );
    map.insert(
        "location".to_owned(),
        CisgValue::String(proposal.location.clone()),
    );
    map.insert(
        "trust".to_owned(),
        CisgValue::String(format_trust_tier(proposal.trust).to_owned()),
    );
    map.insert(
        "notes".to_owned(),
        match &proposal.notes {
            Some(n) => CisgValue::String(n.clone()),
            None => CisgValue::Null,
        },
    );
    map.insert(
        "report".to_owned(),
        match &proposal.report {
            Some(r) => r.to_cisg_value(),
            None => CisgValue::Null,
        },
    );
    map.insert(
        "approvals".to_owned(),
        CisgValue::Array(
            proposal
                .approvals
                .iter()
                .cloned()
                .map(CisgValue::String)
                .collect(),
        ),
    );
    map.insert(
        "shadowSnapshotId".to_owned(),
        match &proposal.shadow_snapshot_id {
            Some(s) => CisgValue::String(s.clone()),
            None => CisgValue::Null,
        },
    );
    map.insert(
        "createdAt".to_owned(),
        CisgValue::String(proposal.created_at.to_string()),
    );
    map.insert(
        "updatedAt".to_owned(),
        CisgValue::String(proposal.updated_at.to_string()),
    );
    CisgValue::Object(map)
}

/// Serializes a [`ProposalConnection`] into Relay-style `{ nodes, pageInfo }` GraphQL AST.
#[must_use]
pub fn render_proposal_connection_cisg(conn: &ProposalConnection) -> CisgValue {
    let mut page_info = BTreeMap::new();
    page_info.insert(
        "hasNextPage".to_owned(),
        CisgValue::Bool(conn.page_info.has_next_page),
    );
    page_info.insert(
        "endCursor".to_owned(),
        match &conn.page_info.end_cursor {
            Some(c) => CisgValue::String(c.clone()),
            None => CisgValue::Null,
        },
    );

    let mut root = BTreeMap::new();
    root.insert(
        "nodes".to_owned(),
        CisgValue::Array(conn.nodes.iter().map(render_proposal_cisg).collect()),
    );
    root.insert("pageInfo".to_owned(), CisgValue::Object(page_info));
    CisgValue::Object(root)
}

/// Executes `gapMutation.proposeSource(input: ...)` (`spec/02-gap-ql.md` §3, `ING-002`).
pub fn propose_source_graphql(
    service: &ProposalService,
    tenant: &TenantScope,
    actor: &str,
    input: ProposeSourceInput,
    correlation_id: &str,
) -> Result<CisgValue, GapError> {
    service
        .propose_source(tenant, actor, input)
        .map(|p| render_proposal_cisg(&p))
        .map_err(|err| err.to_gap_error(correlation_id))
}

/// Input arguments for `gapMutation.decideProposal` (`spec/02-gap-ql.md` §3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecideProposalInput {
    /// Target proposal UUIDv7.
    pub id: ProposalId,
    /// Administrative `APPROVE` or `DENY` decision.
    pub decision: ApprovalDecision,
    /// Explicit breaking-change acknowledgement (`ING-051`).
    pub breaking_change_ack: bool,
    /// Optional rationale recorded in notes and audit log.
    pub reason: Option<String>,
}

/// Executes `gapMutation.decideProposal(id, decision, breakingChangeAck, reason)` (`ING-003`, `ING-051`).
pub fn decide_proposal_graphql(
    service: &ProposalService,
    tenant: &TenantScope,
    caller: &AdminCaller,
    input: DecideProposalInput,
    correlation_id: &str,
) -> Result<CisgValue, GapError> {
    service
        .decide_proposal(
            tenant,
            caller,
            input.id,
            input.decision,
            input.breaking_change_ack,
            input.reason,
        )
        .map(|p| render_proposal_cisg(&p))
        .map_err(|err| err.to_gap_error(correlation_id))
}

/// Executes `gap.proposals(status: ..., first: ..., after: ...)` requiring `admin:ingest` or `admin:schema` (`spec/02-gap-ql.md` §3).
pub fn proposals_graphql(
    service: &ProposalService,
    tenant: &TenantScope,
    caller: &AdminCaller,
    filter: ProposalFilter,
    correlation_id: &str,
) -> Result<CisgValue, GapError> {
    if !caller.has_scope(ADMIN_INGEST_SCOPE) && !caller.has_scope(ADMIN_SCHEMA_SCOPE) {
        return Err(GapError::new(
            ErrorCode::Forbidden,
            "scope `admin:ingest` or `admin:schema` is required to query proposals",
        )
        .with_correlation_id(correlation_id));
    }
    service
        .list_proposals(tenant, filter)
        .map(|conn| render_proposal_connection_cisg(&conn))
        .map_err(|err| err.to_gap_error(correlation_id))
}
