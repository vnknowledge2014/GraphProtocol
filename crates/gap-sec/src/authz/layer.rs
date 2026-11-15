//! `AuthzLayer` plan evaluation (incl. joined RHS fields), query `NULL_WITH_ERROR` / redaction,
//! discovery visibility bitset, and `SEC-024` decision logging (`SEC-021`, `SEC-023`, `SEC-024`,
//! `INV-05`, `RTE-010`).

use std::collections::BTreeMap;
use std::sync::Mutex;

use gap_core::context::Principal;
use gap_core::error::{ErrorCode, GapError};
use gap_core::snapshot::OpKind;

use super::engine::{AuthzContext, CompiledPolicySet, DecisionKind, ResourceAttributes};
use super::schema::AuthzAction;
use crate::authn::crypto::blake3_hash;

/// Generic client-facing authorization error message (`SEC-023`, never leaks policy rule IDs).
pub const GENERIC_FORBIDDEN_MESSAGE: &str = "access denied";

/// Query behavior when a field inside a query selection fails Cedar authorization (`SEC-023`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum QueryDenyPolicy {
    /// Nullable denied query fields resolve to `null` with `GAP_FORBIDDEN` (`SEC-023` default).
    #[default]
    NullWithError,
    /// Fail the entire GraphQL document if any field is denied.
    FailDocument,
}

/// Sensitivity annotation attached to a schema field (`@pii`, `@auth`, etc.; `SEC-022`, `SEC-111`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FieldClassification {
    /// Normal domain field.
    #[default]
    Normal,
    /// Annotated `@pii`; redacted to `"***"` unless principal holds `"pii:read"`.
    Pii,
    /// Annotated `@auth` / confidential.
    Confidential,
}

/// Single touched field coordinate inside a compiled plan — root `Fetch`, joined RHS `Fetch`, or `Mutate` (`SEC-021`, `TM-21`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TouchedFieldRef {
    /// GraphQL response path (for example `"orders.totalCents"` or `"orders.customer.email"` across join).
    pub path: String,
    /// Enclosing GraphQL object type name.
    pub type_name: String,
    /// Leaf or object field name.
    pub field_name: String,
    /// Whether the field is nullable in the composed supergraph schema (`SEC-023`).
    pub nullable: bool,
    /// Whether this field belongs to a joined right-hand-side branch (`TM-21`).
    pub joined_rhs: bool,
    /// Sensitivity classification (`@pii`, etc.).
    pub classification: FieldClassification,
    /// Resource attributes used for Cedar `Execute` / `Mutate` policy evaluation.
    pub resource: ResourceAttributes,
}

/// All operations and fields touched by a plan prior to execution (`SEC-021`, `TM-21`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TouchedPlanSurface {
    /// Unique correlation ID used for `SEC-024` decision log sampling.
    pub request_id: String,
    /// Operation kind (`Query` vs `Mutation`).
    pub operation_kind: OpKind,
    /// Plan cache policy version pinned into this plan (`TM-22`).
    pub plan_policy_version: u64,
    /// Every touched field/operation including joined RHS fields.
    pub fields: Vec<TouchedFieldRef>,
}

/// Per-field authorization verdict (`SEC-023`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldDecisionOutcome {
    /// Field may be executed and emitted normally.
    Allowed,
    /// Query field is nulled out with generic `GAP_FORBIDDEN` (`NULL_WITH_ERROR`, `SEC-023`).
    NullWithError,
    /// Field is permitted semantically but carries `@pii` without `"pii:read"` scope -> mask `"***"`.
    RedactedPii,
    /// Whole operation / document execution is rejected before calling connectors (`SEC-023`).
    FailWhole,
}

/// Per-field entry in a [`PlanAuthzReport`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldDecision {
    /// GraphQL path.
    pub path: String,
    /// Authorization verdict.
    pub outcome: FieldDecisionOutcome,
    /// Optional client-safe generic error (`GAP_FORBIDDEN` without rule IDs, `SEC-023`).
    pub client_error: Option<GapError>,
}

/// Deterministic authorization & redaction plan computed before `ExecLayer` executes connectors (`SEC-021`, `SEC-023`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanAuthzReport {
    /// `true` iff the plan may execute against connectors (may still null/redact individual query fields).
    pub allowed_execution: bool,
    /// `true` if `surface.plan_policy_version` matched the active `CompiledPolicySet` (`TM-22`).
    pub policy_version_matches: bool,
    /// Active policy digest (`SEC-020`).
    pub policy_version_u64: u64,
    /// Per-field decisions indexed in `surface.fields` order (includes RHS join fields, `TM-21`).
    pub field_decisions: Vec<FieldDecision>,
    /// Lookup map of `"type_name.field_name"` -> `FieldDecisionOutcome` for response assembly (`RUN-061`).
    pub redaction_map: BTreeMap<String, FieldDecisionOutcome>,
    /// Client-facing generic error when `allowed_execution == false` (`SEC-023`).
    pub document_error: Option<GapError>,
}

/// Structured Cedar authorization audit decision entry (`SEC-024`, `SEC-012`, `TM-22`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecisionLogEntry {
    /// Request correlation identifier.
    pub request_id: String,
    /// Evaluated principal chain (actor followed by delegators, `SEC-012`).
    pub principal_chain: Vec<String>,
    /// Action name (`Discover`, `Execute`, `Mutate`, etc.).
    pub action: AuthzAction,
    /// Target resource identifier.
    pub resource_id: String,
    /// Allow or Deny.
    pub decision: DecisionKind,
    /// Canonical BLAKE3 hex version of policy set (`SEC-020`).
    pub policy_version_hex: String,
    /// Policy rule IDs that matched (strictly internal; never exposed to clients, `SEC-023`).
    pub determining_policy_ids: Vec<String>,
}

/// Sink receiving sampled / mandatory [`DecisionLogEntry`] records (`SEC-024`).
pub trait DecisionLogger: Send + Sync {
    /// Records `entry`.
    fn record(&self, entry: DecisionLogEntry);
}

/// Thread-safe in-memory [`DecisionLogger`] implementation for gateway & tests (`SEC-024`).
#[derive(Debug, Default)]
pub struct InMemoryDecisionLog {
    entries: Mutex<Vec<DecisionLogEntry>>,
}

impl InMemoryDecisionLog {
    /// Creates an empty decision log.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns a cloned snapshot of all recorded entries.
    #[must_use]
    pub fn entries(&self) -> Vec<DecisionLogEntry> {
        match self.entries.lock() {
            Ok(g) => g.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }
}

impl DecisionLogger for InMemoryDecisionLog {
    fn record(&self, entry: DecisionLogEntry) {
        match self.entries.lock() {
            Ok(mut g) => g.push(entry),
            Err(mut poisoned) => poisoned.get_mut().push(entry),
        }
    }
}

/// Determines whether a decision must be recorded under `SEC-024`:
/// - 100% of `Deny` decisions,
/// - 100% of `Mutate`, `Decide*`, and `Admin*` `Allow` decisions,
/// - deterministic 1% hash-sampled subset of read `Allow` decisions.
#[must_use]
pub fn should_log_decision(
    decision: DecisionKind,
    action: AuthzAction,
    request_id: &str,
    resource_id: &str,
) -> bool {
    if decision == DecisionKind::Deny || action.is_always_logged_on_allow() {
        return true;
    }
    let mut seed = Vec::with_capacity(request_id.len() + 1 + resource_id.len());
    seed.extend_from_slice(request_id.as_bytes());
    seed.push(b'|');
    seed.extend_from_slice(resource_id.as_bytes());
    let digest = blake3_hash(&seed);
    let bucket = u64::from_be_bytes([
        digest[0], digest[1], digest[2], digest[3], digest[4], digest[5], digest[6], digest[7],
    ]);
    bucket.is_multiple_of(100)
}

/// Evaluates every field and operation touched by a plan — including joined RHS fields (`SEC-021`, `TM-21`).
pub fn authorize_plan_surface(
    policy: &CompiledPolicySet,
    principal: &Principal,
    delegators: &[Principal],
    surface: &TouchedPlanSurface,
    context: &AuthzContext,
    query_deny: QueryDenyPolicy,
    logger: Option<&dyn DecisionLogger>,
) -> PlanAuthzReport {
    let policy_version_matches = surface.plan_policy_version == policy.policy_version_u64();
    let action = match surface.operation_kind {
        OpKind::Mutation => AuthzAction::Mutate,
        OpKind::Query | OpKind::Subscription | OpKind::Task => AuthzAction::Execute,
    };

    let has_pii_scope = principal.scopes.contains("pii:read")
        && delegators.iter().all(|d| d.scopes.contains("pii:read"));

    let mut allowed_execution = true;
    let mut document_error: Option<GapError> = None;
    let mut field_decisions = Vec::with_capacity(surface.fields.len());
    let mut redaction_map = BTreeMap::new();

    for field in &surface.fields {
        let eval = policy.evaluate_with_delegators(
            principal,
            delegators,
            action,
            &field.resource,
            context,
        );

        if let Some(sink) = logger
            && should_log_decision(
                eval.decision,
                action,
                &surface.request_id,
                &field.resource.id,
            )
        {
            sink.record(DecisionLogEntry {
                request_id: surface.request_id.clone(),
                principal_chain: eval.evaluated_actors.clone(),
                action,
                resource_id: field.resource.id.clone(),
                decision: eval.decision,
                policy_version_hex: eval.policy_version_hex.clone(),
                determining_policy_ids: eval.determining_policies.clone(),
            });
        }

        let (outcome, client_error) = match eval.decision {
            DecisionKind::Allow => {
                if field.classification == FieldClassification::Pii && !has_pii_scope {
                    (FieldDecisionOutcome::RedactedPii, None)
                } else {
                    (FieldDecisionOutcome::Allowed, None)
                }
            }
            DecisionKind::Deny => {
                let err = GapError::new(ErrorCode::Forbidden, GENERIC_FORBIDDEN_MESSAGE);
                let can_null_field = surface.operation_kind == OpKind::Query
                    && query_deny == QueryDenyPolicy::NullWithError
                    && field.nullable;
                if can_null_field {
                    (FieldDecisionOutcome::NullWithError, Some(err))
                } else {
                    allowed_execution = false;
                    if document_error.is_none() {
                        document_error = Some(err.clone());
                    }
                    (FieldDecisionOutcome::FailWhole, Some(err))
                }
            }
        };

        let key = format!("{}.{}", field.type_name, field.field_name);
        redaction_map.insert(key, outcome);
        field_decisions.push(FieldDecision {
            path: field.path.clone(),
            outcome,
            client_error,
        });
    }

    PlanAuthzReport {
        allowed_execution,
        policy_version_matches,
        policy_version_u64: policy.policy_version_u64(),
        field_decisions,
        redaction_map,
        document_error,
    }
}

/// Computes a deterministic discovery visibility bitset over candidate elements (`RTE-010`, `INV-05`, `TM-10`).
///
/// Each element is evaluated against [`AuthzAction::Discover`]; returning `true` at index `i`
/// iff both the actor and all delegators are authorized to discover candidate `i`.
#[must_use]
pub fn visibility_bitset(
    policy: &CompiledPolicySet,
    principal: &Principal,
    delegators: &[Principal],
    context: &AuthzContext,
    candidates: &[ResourceAttributes],
) -> Vec<bool> {
    candidates
        .iter()
        .map(|res| {
            policy
                .evaluate_with_delegators(
                    principal,
                    delegators,
                    AuthzAction::Discover,
                    res,
                    context,
                )
                .decision
                == DecisionKind::Allow
        })
        .collect()
}
