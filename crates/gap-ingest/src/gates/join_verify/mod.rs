//! Salted `blake3` bottom-k sketch containment, Gate `G4` join verification, and shadow-canary
//! lifecycle state machine (`WP-M4-06`).
//!
//! Normative references:
//! - `NGC-050`, `NGC-051`, `NGC-052` (`spec/03-ngc.md` §6)
//! - `ING-021`, `ING-040`, `ING-041` (`spec/05-ingestion.md` §§3, 5)
//! - `INV-06` (`spec/01-architecture.md` §6), `ADR-012` (`docs/01-DECISIONS.md`)

use std::collections::BTreeMap;
use std::fmt;

use gap_core::snapshot::TrustTier;
use gap_proto::ngc::v1::{CallContext, Principal, PrincipalKind, SketchRequest, SketchResponse};

use crate::cisg::{CandidateJoin, CisgValue, ExtractLimits, JoinCardinality, JoinEvidence};
use crate::extractors::mcp::parse_bounded_json;
use crate::gates::{
    AdminAck, GateFailAction, GateKind, GateOutcome, MIN_G3_UNIQUENESS_RATIO, VerificationReport,
    can_activate_join,
};

/// Default bottom-k sketch cardinality (`NGC-050`).
pub const DEFAULT_BOTTOM_K: u32 = 1024;
/// Default maximum sample size capped by `NGC-050`.
pub const DEFAULT_SKETCH_SAMPLE_SIZE: u32 = 100_000;
/// Gate `G4` minimum containment ratio (`ADR-012`, `spec/05-ingestion.md` §5).
pub const DEFAULT_MIN_CONTAINMENT: f64 = 0.95;
/// Gate `G4` maximum null ratio (`ADR-012`, `spec/05-ingestion.md` §5).
pub const DEFAULT_MAX_NULL_RATIO: f64 = 0.50;
/// Gate `G4` minimum sample distinct/row count when domain is larger (`ADR-012`).
pub const DEFAULT_MIN_SAMPLE_COUNT: u32 = 1_000;
/// Minimum shadow canary executions required before promoting to `ACTIVE` (`ING-041`).
pub const MIN_CANARY_EXECUTIONS: u64 = 100;
/// Minimum shadow canary soak duration in milliseconds (`10 minutes`, `ING-041`).
pub const MIN_CANARY_WINDOW_MS: u64 = 600_000;
/// Maximum allowed drop in key coverage relative to sketch expectation (`10` percentage points, `ING-041`).
pub const MAX_CANARY_COVERAGE_DROP: f64 = 0.10;
/// Synthetic gate identifier tag recorded in structured `G4` findings.
pub const GATE_G4_LABEL: &str = "G4";

/// Execution/authorization context for issuing `NGC::Sketch` RPCs (`NGC-051`, `NGC-052`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JoinSketchContext {
    /// Owning tenant identifier (`INV-09`).
    pub tenant_id: String,
    /// Per-tenant opaque salt supplied to `blake3(salt || canonical_value)`.
    pub salt: Vec<u8>,
    /// Correlation request identifier.
    pub request_id: String,
    /// Ingestion service principal ID (`NGC-052`).
    pub principal_id: String,
    /// Read-only scopes granted to the ingestion service principal (`NGC-052`).
    pub read_only_scopes: Vec<String>,
    /// Requested bottom-k size (default `1024`).
    pub bottom_k: u32,
    /// Requested sample row cap (clamped `1..=100_000`, `NGC-050`).
    pub sample_size: u32,
    /// Absolute request deadline in Unix epoch milliseconds (`INV-12`).
    pub deadline_unix_ms: i64,
}

impl Default for JoinSketchContext {
    fn default() -> Self {
        Self {
            tenant_id: "default-tenant".to_owned(),
            salt: b"default-tenant-salt".to_vec(),
            request_id: "sketch-req-0".to_owned(),
            principal_id: "svc:gap-ingest".to_owned(),
            read_only_scopes: vec!["ngc:read".to_owned(), "ngc:sketch".to_owned()],
            bottom_k: DEFAULT_BOTTOM_K,
            sample_size: DEFAULT_SKETCH_SAMPLE_SIZE,
            deadline_unix_ms: 0,
        }
    }
}

/// Typed failure while validating `SketchRequest` / `SketchResponse` inputs (`NGC-050`..`NGC-052`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SketchValidationError {
    /// Tenant ID was empty (`INV-09`).
    MissingTenant,
    /// Salt was empty (`NGC-051`).
    MissingSalt,
    /// Service principal ID or read-only scopes violated `NGC-052`.
    InvalidPrincipalOrScope(String),
    /// Operation or field path was empty.
    InvalidTargetField(String),
    /// `bottom_k` or `sample_size` out of bounds.
    InvalidParameters(String),
    /// Connector returned unsorted, duplicate, or non-finite sketch values.
    MalformedSketchResponse(String),
}

impl fmt::Display for SketchValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingTenant => write!(f, "sketch context missing tenant_id"),
            Self::MissingSalt => write!(f, "sketch context missing non-empty tenant salt"),
            Self::InvalidPrincipalOrScope(msg) => write!(f, "sketch auth invalid: {msg}"),
            Self::InvalidTargetField(msg) => write!(f, "sketch target invalid: {msg}"),
            Self::InvalidParameters(msg) => write!(f, "sketch parameters invalid: {msg}"),
            Self::MalformedSketchResponse(msg) => write!(f, "malformed SketchResponse: {msg}"),
        }
    }
}

impl std::error::Error for SketchValidationError {}

/// Validates `ctx` and constructs an `NGC` [`SketchRequest`] running as a read-only ingestion
/// service principal with a non-empty tenant-scoped salt (`NGC-050`, `NGC-051`, `NGC-052`).
pub fn build_sketch_request(
    operation: &str,
    field_path: &str,
    ctx: &JoinSketchContext,
) -> Result<SketchRequest, SketchValidationError> {
    if ctx.tenant_id.trim().is_empty() {
        return Err(SketchValidationError::MissingTenant);
    }
    if ctx.salt.is_empty() {
        return Err(SketchValidationError::MissingSalt);
    }
    if ctx.principal_id.trim().is_empty() {
        return Err(SketchValidationError::InvalidPrincipalOrScope(
            "principal_id must not be empty".to_owned(),
        ));
    }
    if ctx.read_only_scopes.is_empty() {
        return Err(SketchValidationError::InvalidPrincipalOrScope(
            "at least one read-only scope is required (NGC-052)".to_owned(),
        ));
    }
    for scope in &ctx.read_only_scopes {
        let lower = scope.to_ascii_lowercase();
        if lower.is_empty()
            || lower.contains("write")
            || lower.contains("mutate")
            || lower.contains("admin")
            || lower.contains("delete")
        {
            return Err(SketchValidationError::InvalidPrincipalOrScope(format!(
                "sketch request cannot carry non-read-only scope `{scope}` (NGC-052)"
            )));
        }
    }
    if operation.trim().is_empty() || field_path.trim().is_empty() {
        return Err(SketchValidationError::InvalidTargetField(
            "operation and field_path must be non-empty".to_owned(),
        ));
    }
    if ctx.bottom_k == 0 {
        return Err(SketchValidationError::InvalidParameters(
            "bottom_k must be >= 1".to_owned(),
        ));
    }
    if ctx.sample_size == 0 {
        return Err(SketchValidationError::InvalidParameters(
            "sample_size must be >= 1".to_owned(),
        ));
    }

    let principal = Principal {
        id: ctx.principal_id.clone(),
        kind: PrincipalKind::Service as i32,
        delegation_chain: Vec::new(),
        scopes: ctx.read_only_scopes.clone(),
        attributes: BTreeMap::from([("role".to_owned(), "ingestion_sketch".to_owned())])
            .into_iter()
            .collect(),
    };
    let call_ctx = CallContext {
        tenant_id: ctx.tenant_id.clone(),
        request_id: ctx.request_id.clone(),
        traceparent: String::new(),
        deadline_unix_ms: ctx.deadline_unix_ms,
        remaining_cost_units: 0,
        principal: Some(principal),
        hop_count: 0,
        trace_path: Vec::new(),
        idempotency_key: String::new(),
        snapshot_generation: 0,
    };

    Ok(SketchRequest {
        ctx: Some(call_ctx),
        operation: operation.to_owned(),
        field_path: field_path.to_owned(),
        arguments: None,
        sample_size: ctx.sample_size.min(DEFAULT_SKETCH_SAMPLE_SIZE),
        salt: ctx.salt.clone(),
        bottom_k: ctx.bottom_k,
    })
}

/// Normalizes a scalar value to the canonical wire string hashed by connectors (`NGC-050`, `NGC-051`).
#[must_use]
pub fn normalize_sketch_value(value_kind: &str, raw: &str) -> String {
    let trimmed = raw.trim();
    match value_kind {
        "integer" => trimmed
            .parse::<i128>()
            .map_or_else(|_| trimmed.to_owned(), |v| v.to_string()),
        "uuid" => trimmed
            .chars()
            .filter(char::is_ascii_hexdigit)
            .map(|c| c.to_ascii_lowercase())
            .collect(),
        _ => trimmed.to_owned(),
    }
}

/// Result of estimating `|LHS ∩ RHS| / |LHS|` from two `SketchResponse`s (`NGC-051`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BottomKContainment {
    /// Estimated containment ratio in `[0.0, 1.0]`.
    pub containment: f64,
    /// Conservative standard error bound (`0.0` when exact full-domain sets are compared).
    pub error_bound: f64,
    /// Union error bound `1 / sqrt(k_eff)` when using bottom-k sketches.
    pub union_error_bound: f64,
    /// Effective union prefix size `k_eff = min(k, |A|, |B|)`.
    pub k_eff: usize,
    /// Number of union prefix elements belonging to `LHS`.
    pub lhs_in_union: usize,
    /// Number of union prefix elements belonging to both `LHS` and `RHS`.
    pub intersection_in_union: usize,
    /// Whether an exact full-domain calculation was possible (`lhs.exact && rhs.exact`).
    pub exact_used: bool,
}

fn validate_sketch_response(
    label: &str,
    resp: &SketchResponse,
) -> Result<(), SketchValidationError> {
    if !resp.null_ratio.is_finite() || !(0.0..=1.0).contains(&resp.null_ratio) {
        return Err(SketchValidationError::MalformedSketchResponse(format!(
            "{label}.null_ratio {} must be finite in [0.0, 1.0]",
            resp.null_ratio
        )));
    }
    if !resp.uniqueness_ratio.is_finite() || !(0.0..=1.0).contains(&resp.uniqueness_ratio) {
        return Err(SketchValidationError::MalformedSketchResponse(format!(
            "{label}.uniqueness_ratio {} must be finite in [0.0, 1.0]",
            resp.uniqueness_ratio
        )));
    }
    for pair in resp.bottom_k_hashes.windows(2) {
        if pair[0] >= pair[1] {
            return Err(SketchValidationError::MalformedSketchResponse(format!(
                "{label}.bottom_k_hashes must be strictly ascending without duplicates"
            )));
        }
    }
    Ok(())
}

/// Estimates containment of `lhs` in `rhs` from their sorted bottom-k hash sketches (`NGC-051`).
///
/// Implements `|bottomk(A) ∩ bottomk(B)|` over the smallest `k_eff = min(k, |A|, |B|)` elements
/// of the union normalized by the `LHS` samples inside that union prefix, guaranteeing an unbiased
/// estimator even when `|B| >> |A|`, and computes exact containment when both sketches cover the
/// entire non-sampled domain (`exact == true`).
pub fn estimate_bottom_k_containment(
    lhs: &SketchResponse,
    rhs: &SketchResponse,
    k: u32,
) -> Result<BottomKContainment, SketchValidationError> {
    if k == 0 {
        return Err(SketchValidationError::InvalidParameters(
            "bottom_k must be >= 1".to_owned(),
        ));
    }
    validate_sketch_response("lhs", lhs)?;
    validate_sketch_response("rhs", rhs)?;

    let a = &lhs.bottom_k_hashes;
    let b = &rhs.bottom_k_hashes;
    if a.is_empty() || b.is_empty() {
        return Ok(BottomKContainment {
            containment: 0.0,
            error_bound: 1.0,
            union_error_bound: 1.0,
            k_eff: 0,
            lhs_in_union: 0,
            intersection_in_union: 0,
            exact_used: false,
        });
    }

    // If both connectors scanned the full domain and returned every distinct hash without
    // truncation, we can compute exact containment with zero sampling error.
    let lhs_complete = lhs.exact && u64::try_from(a.len()).ok() == Some(lhs.distinct_estimate);
    let rhs_complete = rhs.exact && u64::try_from(b.len()).ok() == Some(rhs.distinct_estimate);
    if lhs_complete && rhs_complete {
        let mut ia = 0_usize;
        let mut ib = 0_usize;
        let mut inter = 0_usize;
        while ia < a.len() && ib < b.len() {
            match a[ia].cmp(&b[ib]) {
                std::cmp::Ordering::Less => ia += 1,
                std::cmp::Ordering::Greater => ib += 1,
                std::cmp::Ordering::Equal => {
                    inter += 1;
                    ia += 1;
                    ib += 1;
                }
            }
        }
        let k_eff = usize::try_from(k)
            .unwrap_or(usize::MAX)
            .min(a.len())
            .min(b.len());
        return Ok(BottomKContainment {
            containment: inter as f64 / a.len() as f64,
            error_bound: 0.0,
            union_error_bound: 0.0,
            k_eff,
            lhs_in_union: a.len(),
            intersection_in_union: inter,
            exact_used: true,
        });
    }

    let k_eff = usize::try_from(k)
        .unwrap_or(usize::MAX)
        .min(a.len())
        .min(b.len());

    let mut ia = 0_usize;
    let mut ib = 0_usize;
    let mut union_seen = 0_usize;
    let mut lhs_in_union = 0_usize;
    let mut intersection_in_union = 0_usize;

    while union_seen < k_eff && (ia < a.len() || ib < b.len()) {
        let take_from_a;
        let take_from_b;
        match (a.get(ia), b.get(ib)) {
            (Some(&va), Some(&vb)) => match va.cmp(&vb) {
                std::cmp::Ordering::Less => {
                    take_from_a = true;
                    take_from_b = false;
                }
                std::cmp::Ordering::Greater => {
                    take_from_a = false;
                    take_from_b = true;
                }
                std::cmp::Ordering::Equal => {
                    take_from_a = true;
                    take_from_b = true;
                }
            },
            (Some(_), None) => {
                take_from_a = true;
                take_from_b = false;
            }
            (None, Some(_)) => {
                take_from_a = false;
                take_from_b = true;
            }
            (None, None) => break,
        }

        if take_from_a {
            lhs_in_union += 1;
            if take_from_b {
                intersection_in_union += 1;
            }
        }
        if take_from_a {
            ia += 1;
        }
        if take_from_b {
            ib += 1;
        }
        union_seen += 1;
    }

    let containment = if lhs_in_union == 0 {
        0.0
    } else {
        intersection_in_union as f64 / lhs_in_union as f64
    };
    let error_bound = 1.0 / (lhs_in_union.max(1) as f64).sqrt();
    let union_error_bound = 1.0 / (union_seen.max(1) as f64).sqrt();

    Ok(BottomKContainment {
        containment,
        error_bound,
        union_error_bound,
        k_eff: union_seen,
        lhs_in_union,
        intersection_in_union,
        exact_used: false,
    })
}

/// Gate `G4` verification thresholds (`ADR-012`, `spec/05-ingestion.md` §5).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct G4Thresholds {
    /// Minimum estimated containment `|LHS ∩ RHS| / |LHS|` (default `0.95`).
    pub min_containment: f64,
    /// Maximum null ratio permitted on either key (default `0.50`).
    pub max_null_ratio: f64,
    /// Minimum RHS uniqueness ratio when RHS is not an explicit PK/unique key (default `0.999`).
    pub min_rhs_uniqueness: f64,
    /// Minimum sample distinct count / rows inspected when full domain not scanned (default `1_000`).
    pub min_sample_count: u32,
    /// Target bottom-k cardinality used when estimating containment (`NGC-050`).
    pub bottom_k: u32,
}

impl Default for G4Thresholds {
    fn default() -> Self {
        Self {
            min_containment: DEFAULT_MIN_CONTAINMENT,
            max_null_ratio: DEFAULT_MAX_NULL_RATIO,
            min_rhs_uniqueness: MIN_G3_UNIQUENESS_RATIO,
            min_sample_count: DEFAULT_MIN_SAMPLE_COUNT,
            bottom_k: DEFAULT_BOTTOM_K,
        }
    }
}

/// Lifecycle state of a join edge across Gate `G4`, shadow canary, and runtime (`ADR-012`, `ING-040`, `INV-06`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum JoinVerificationStatus {
    /// Initial candidate or insufficient/empty sketch sample; invisible to planning.
    Proposed,
    /// Passed `G1`–`G4` data-driven verification; still invisible to client planning.
    Verified,
    /// Passed `G1`–`G8` and approval; serving shadow-only traffic comparison (`ING-041`).
    Canary,
    /// Passed canary soak (`>= 100` calls or `>= 10 min`); ONLY state visible to client planner (`INV-06`).
    Active,
    /// Quarantined due to runtime rollback attribution or canary drift.
    Quarantined,
    /// Failed periodic re-verification on drift (`ADR-012`).
    Deprecated,
    /// Failed hard `G4` thresholds (low containment, duplicates, nulls, mismatched value kind).
    Rejected,
}

impl JoinVerificationStatus {
    /// Returns `true` ONLY for [`Self::Active`], enforcing `INV-06`.
    #[must_use]
    pub const fn visible_to_planner(self) -> bool {
        matches!(self, Self::Active)
    }

    /// Canonical uppercase status label.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Proposed => "PROPOSED",
            Self::Verified => "VERIFIED",
            Self::Canary => "CANARY",
            Self::Active => "ACTIVE",
            Self::Quarantined => "QUARANTINED",
            Self::Deprecated => "DEPRECATED",
            Self::Rejected => "REJECTED",
        }
    }
}

/// Evidence supplied for verifying a single [`CandidateJoin`] under Gate `G4`.
#[derive(Debug, Clone, PartialEq)]
pub struct JoinVerificationInput<'a> {
    /// Candidate join being verified.
    pub candidate: &'a CandidateJoin,
    /// Optional LHS sketch (`None` when connector lacks `capabilities.sketch`).
    pub lhs_sketch: Option<&'a SketchResponse>,
    /// Optional RHS sketch (`None` when connector lacks `capabilities.sketch`).
    pub rhs_sketch: Option<&'a SketchResponse>,
    /// Whether the LHS field is a declared primary/unique key in schema metadata.
    pub lhs_declared_unique: bool,
    /// Whether the RHS target field is a declared primary/unique key in `CISG`.
    pub rhs_declared_unique: bool,
    /// Whether the RHS connector supports batched `foreach` / `IN` lookup (`G3`).
    pub rhs_supports_batch: bool,
}

/// Detailed result of evaluating a [`CandidateJoin`] against Gate `G4` (`ADR-012`, `ING-040`).
#[derive(Debug, Clone, PartialEq)]
pub struct JoinG4Evaluation {
    /// Candidate identifier `<from_type>.<from_field>-><to_type>.<to_field>`.
    pub join_id: String,
    /// Resulting lifecycle state (`Proposed`, `Verified`, or `Rejected`).
    pub status: JoinVerificationStatus,
    /// Estimated containment (`|LHS ∩ RHS| / |LHS|`), if sketches were available.
    pub containment: Option<BottomKContainment>,
    /// Advisory S1/semantic similarity score if computed (`ADR-012`: advisory only).
    pub semantic_advisory_score: Option<f64>,
    /// Machine-readable diagnostic reasons explaining rejection or deferral.
    pub reasons: Vec<String>,
}

/// Verifies a candidate join against `ADR-012` / Gate `G4` thresholds (`ING-040`, `NGC-050`, `NGC-051`).
#[must_use]
pub fn evaluate_join_g4(
    input: &JoinVerificationInput<'_>,
    thresholds: &G4Thresholds,
) -> JoinG4Evaluation {
    let join_id = format!(
        "{}.{}->{}.{}",
        input.candidate.from.type_name,
        input.candidate.from.field_path,
        input.candidate.to.type_name,
        input.candidate.to.field_path
    );
    let mut reasons = Vec::new();

    if !input.rhs_supports_batch {
        reasons.push(
            "RHS connector does not support batched lookup (`max_batch_size == 0`)".to_owned(),
        );
        return JoinG4Evaluation {
            join_id,
            status: JoinVerificationStatus::Rejected,
            containment: None,
            semantic_advisory_score: None,
            reasons,
        };
    }

    let (Some(lhs), Some(rhs)) = (input.lhs_sketch, input.rhs_sketch) else {
        reasons.push("sketch samples unavailable; join remains PROPOSED (ADR-012)".to_owned());
        return JoinG4Evaluation {
            join_id,
            status: JoinVerificationStatus::Proposed,
            containment: None,
            semantic_advisory_score: None,
            reasons,
        };
    };

    // Empty domains never verify or activate (`tests/fixtures/joins/empty_domain.json`).
    if lhs.sample_count == 0
        || lhs.distinct_estimate == 0
        || rhs.sample_count == 0
        || rhs.distinct_estimate == 0
    {
        reasons
            .push("empty sample/distinct domain cannot verify join; remains PROPOSED".to_owned());
        return JoinG4Evaluation {
            join_id,
            status: JoinVerificationStatus::Proposed,
            containment: None,
            semantic_advisory_score: None,
            reasons,
        };
    }

    let containment = match estimate_bottom_k_containment(lhs, rhs, thresholds.bottom_k) {
        Ok(c) => c,
        Err(err) => {
            reasons.push(err.to_string());
            return JoinG4Evaluation {
                join_id,
                status: JoinVerificationStatus::Rejected,
                containment: None,
                semantic_advisory_score: None,
                reasons,
            };
        }
    };

    // Insufficient sample without full-domain `exact` scan stays `PROPOSED`.
    let lhs_sample_ok = lhs.exact
        || lhs.sample_count >= thresholds.min_sample_count
        || lhs.distinct_estimate >= u64::from(thresholds.min_sample_count);
    let rhs_sample_ok = rhs.exact
        || rhs.sample_count >= thresholds.min_sample_count
        || rhs.distinct_estimate >= u64::from(thresholds.min_sample_count);
    if !lhs_sample_ok || !rhs_sample_ok {
        reasons.push(format!(
            "insufficient sketch sample without exact scan (lhs sample={}, exact={}; rhs sample={}, exact={})",
            lhs.sample_count, lhs.exact, rhs.sample_count, rhs.exact
        ));
        return JoinG4Evaluation {
            join_id,
            status: JoinVerificationStatus::Proposed,
            containment: Some(containment),
            semantic_advisory_score: None,
            reasons,
        };
    }

    // Value kind compatibility from sketches.
    if !sketch_value_kinds_compatible(&lhs.value_kind, &rhs.value_kind) {
        reasons.push(format!(
            "incompatible sketch value_kind: lhs `{}` vs rhs `{}`",
            lhs.value_kind, rhs.value_kind
        ));
    }

    if lhs.null_ratio > thresholds.max_null_ratio {
        reasons.push(format!(
            "lhs null_ratio {:.4} exceeds max {:.4}",
            lhs.null_ratio, thresholds.max_null_ratio
        ));
    }
    if rhs.null_ratio > thresholds.max_null_ratio {
        reasons.push(format!(
            "rhs null_ratio {:.4} exceeds max {:.4}",
            rhs.null_ratio, thresholds.max_null_ratio
        ));
    }

    // Cardinality consistency & uniqueness checks (`ADR-012`).
    let rhs_unique_ok =
        input.rhs_declared_unique || rhs.uniqueness_ratio >= thresholds.min_rhs_uniqueness;
    let lhs_unique_ok =
        input.lhs_declared_unique || lhs.uniqueness_ratio >= thresholds.min_rhs_uniqueness;
    match input.candidate.cardinality {
        JoinCardinality::ManyToOne => {
            if !rhs_unique_ok {
                reasons.push(format!(
                    "MANY_TO_ONE requires RHS uniqueness >= {:.3} or declared PK/unique (got {:.4})",
                    thresholds.min_rhs_uniqueness, rhs.uniqueness_ratio
                ));
            }
        }
        JoinCardinality::OneToOne => {
            if !rhs_unique_ok {
                reasons.push(format!(
                    "ONE_TO_ONE requires RHS uniqueness >= {:.3} (got {:.4})",
                    thresholds.min_rhs_uniqueness, rhs.uniqueness_ratio
                ));
            }
            if !lhs_unique_ok {
                reasons.push(format!(
                    "ONE_TO_ONE requires LHS uniqueness >= {:.3} (got {:.4})",
                    thresholds.min_rhs_uniqueness, lhs.uniqueness_ratio
                ));
            }
        }
        JoinCardinality::OneToMany => {
            if !lhs_unique_ok {
                reasons.push(format!(
                    "ONE_TO_MANY requires LHS source uniqueness >= {:.3} (got {:.4})",
                    thresholds.min_rhs_uniqueness, lhs.uniqueness_ratio
                ));
            }
        }
    }

    if containment.containment < thresholds.min_containment {
        reasons.push(format!(
            "containment {:.4} below G4 threshold {:.4} (+/- {:.4})",
            containment.containment, thresholds.min_containment, containment.error_bound
        ));
    }

    let status = if reasons.is_empty() {
        JoinVerificationStatus::Verified
    } else {
        JoinVerificationStatus::Rejected
    };

    JoinG4Evaluation {
        join_id,
        status,
        containment: Some(containment),
        semantic_advisory_score: None,
        reasons,
    }
}

fn sketch_value_kinds_compatible(lhs: &str, rhs: &str) -> bool {
    let a = lhs.trim().to_ascii_lowercase();
    let b = rhs.trim().to_ascii_lowercase();
    if a.is_empty() || b.is_empty() {
        return false;
    }
    if a == b {
        return true;
    }
    matches!(
        (a.as_str(), b.as_str()),
        ("uuid", "string") | ("string", "uuid")
    )
}

/// Evaluates a batch of candidate joins and produces a structured G4 audit summary (`GateResult` shape).
#[must_use]
pub fn evaluate_gate_g4_batch(
    inputs: &[JoinVerificationInput<'_>],
    thresholds: &G4Thresholds,
) -> (
    Vec<JoinG4Evaluation>,
    BTreeMap<String, CisgValue>,
    Option<GateFailAction>,
) {
    let evals: Vec<JoinG4Evaluation> = inputs
        .iter()
        .map(|item| evaluate_join_g4(item, thresholds))
        .collect();

    let verified_count = evals
        .iter()
        .filter(|e| e.status == JoinVerificationStatus::Verified)
        .count();
    let proposed_count = evals
        .iter()
        .filter(|e| e.status == JoinVerificationStatus::Proposed)
        .count();
    let rejected_count = evals
        .iter()
        .filter(|e| e.status == JoinVerificationStatus::Rejected)
        .count();

    let mut evidence = BTreeMap::new();
    evidence.insert(
        "evaluated".to_owned(),
        CisgValue::Int(i64::try_from(evals.len()).unwrap_or(i64::MAX)),
    );
    evidence.insert(
        "verified".to_owned(),
        CisgValue::Int(i64::try_from(verified_count).unwrap_or(i64::MAX)),
    );
    evidence.insert(
        "proposed".to_owned(),
        CisgValue::Int(i64::try_from(proposed_count).unwrap_or(i64::MAX)),
    );
    evidence.insert(
        "rejected".to_owned(),
        CisgValue::Int(i64::try_from(rejected_count).unwrap_or(i64::MAX)),
    );

    let fail_action = if rejected_count > 0 || proposed_count > 0 {
        // Hard for ACTIVE (`spec/05-ingestion.md` §5): rejected/unverified joins never activate.
        Some(GateFailAction::CandidateDropped)
    } else {
        None
    };

    (evals, evidence, fail_action)
}

/// Builds a canonical report `GateResult` proxy when callers want G4 represented alongside G1–G3, G6–G9.
#[must_use]
pub fn summarize_g4_findings(evals: &[JoinG4Evaluation]) -> Vec<String> {
    evals
        .iter()
        .flat_map(|e| {
            e.reasons
                .iter()
                .map(move |r| format!("[{GATE_G4_LABEL}] {}: {r}", e.join_id))
        })
        .collect()
}

/// Shadow-canary alert emitted when a canary join diverges beyond tolerance (`ING-041`).
#[derive(Debug, Clone, PartialEq)]
pub struct CanaryRevertAlert {
    /// Join identifier `<from_type>.<from_field>-><to_type>.<to_field>`.
    pub join_id: String,
    /// Baseline sketch containment observed during G4.
    pub expected_containment: f64,
    /// Observed key coverage during shadow canary execution.
    pub observed_key_coverage: f64,
    /// Execution count accumulated before revert.
    pub executions: u64,
    /// Human-readable alert message.
    pub reason: String,
}

/// Error returned on invalid lifecycle transitions (`ING-021`, `ING-040`, `ING-041`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JoinLifecycleError {
    /// Attempted transition from an incompatible source state.
    InvalidTransition {
        /// Current status.
        from: JoinVerificationStatus,
        /// Target status.
        to: JoinVerificationStatus,
        /// Explanation.
        reason: String,
    },
    /// Activation prerequisites (G1–G8, human approval for LLM/T2/cross-trust) were not met.
    ApprovalOrGateRequired(String),
}

impl fmt::Display for JoinLifecycleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidTransition { from, to, reason } => {
                write!(
                    f,
                    "invalid join transition {} -> {}: {reason}",
                    from.as_str(),
                    to.as_str()
                )
            }
            Self::ApprovalOrGateRequired(msg) => write!(f, "activation blocked: {msg}"),
        }
    }
}

impl std::error::Error for JoinLifecycleError {}

/// Rolling canary soak state tracked for a join in [`JoinVerificationStatus::Canary`] (`ING-041`).
#[derive(Debug, Clone, PartialEq)]
pub struct CanaryTracker {
    /// Baseline G4 sketch containment expected from production traffic.
    pub baseline_containment: f64,
    /// Unix epoch millis when canary mode started.
    pub started_at_ms: u64,
    /// Total sampled shadow executions observed so far.
    pub executions: u64,
    /// Total LHS keys sampled.
    pub sampled_keys: u64,
    /// Total matched RHS keys observed.
    pub matched_keys: u64,
    /// Last observed per-batch key coverage.
    pub last_batch_coverage: f64,
}

/// Persistent verification & promotion record for a join edge (`ING-021`, `ING-040`, `ING-041`, `INV-06`).
#[derive(Debug, Clone, PartialEq)]
pub struct JoinLifecycleRecord {
    /// Underlying candidate join metadata.
    pub candidate: CandidateJoin,
    /// Current lifecycle state (`INV-06`).
    pub status: JoinVerificationStatus,
    /// Source trust tier (`ADR-013`).
    pub trust_tier: TrustTier,
    /// Whether this join spans different trust domains/tiers (`ING-040`).
    pub cross_trust: bool,
    /// Audited administrator approval, if granted.
    pub approval: Option<AdminAck>,
    /// Sketch containment estimated during Gate `G4`.
    pub g4_containment: Option<BottomKContainment>,
    /// Active shadow canary tracking state while in `CANARY`.
    pub canary: Option<CanaryTracker>,
    /// Recorded transition reason.
    pub reason: String,
}

impl JoinLifecycleRecord {
    /// Creates a new lifecycle record starting in [`JoinVerificationStatus::Proposed`] (`INV-06`).
    #[must_use]
    pub fn new(candidate: CandidateJoin, trust_tier: TrustTier, cross_trust: bool) -> Self {
        Self {
            candidate,
            status: JoinVerificationStatus::Proposed,
            trust_tier,
            cross_trust,
            approval: None,
            g4_containment: None,
            canary: None,
            reason: "newly proposed candidate join".to_owned(),
        }
    }

    /// Returns `true` ONLY when this join is [`JoinVerificationStatus::Active`] (`INV-06`).
    #[must_use]
    pub const fn visible_to_planner(&self) -> bool {
        self.status.visible_to_planner()
    }

    /// Attaches a human administrator approval record.
    pub fn set_approval(&mut self, approval: AdminAck) {
        self.approval = Some(approval);
    }

    /// Applies a Gate `G4` evaluation result, transitioning `PROPOSED -> VERIFIED`, keeping
    /// `PROPOSED` on missing/empty samples, or moving to `REJECTED` on threshold violations.
    /// If the join was already `ACTIVE` and fails periodic drift re-verification, transitions to
    /// `DEPRECATED` (`ADR-012`).
    pub fn apply_g4_evaluation(&mut self, eval: &JoinG4Evaluation) {
        self.g4_containment = eval.containment;
        if self.status == JoinVerificationStatus::Active {
            if eval.status != JoinVerificationStatus::Verified {
                self.status = JoinVerificationStatus::Deprecated;
                self.canary = None;
                self.reason = format!(
                    "active join failed G4 drift re-verification: {}",
                    eval.reasons.join("; ")
                );
            }
            return;
        }

        self.status = eval.status;
        self.reason = if eval.reasons.is_empty() {
            "passed Gate G4 data-driven verification".to_owned()
        } else {
            eval.reasons.join("; ")
        };
    }

    /// Transitions `VERIFIED -> CANARY` once G1–G8 pass and approval rules (`ING-021`, `ING-040`)
    /// are satisfied (`Report` + `g8_passed` + `LLM` human-approval rule).
    pub fn enter_canary(
        &mut self,
        report: &VerificationReport,
        g8_passed: bool,
        now_ms: u64,
    ) -> Result<(), JoinLifecycleError> {
        if self.status != JoinVerificationStatus::Verified {
            return Err(JoinLifecycleError::InvalidTransition {
                from: self.status,
                to: JoinVerificationStatus::Canary,
                reason: "join must be VERIFIED before entering shadow canary".to_owned(),
            });
        }

        let Some(containment) = self.g4_containment else {
            return Err(JoinLifecycleError::ApprovalOrGateRequired(
                "G4 containment measurement is required before canary".to_owned(),
            ));
        };

        // `ING-021`: LLM candidates can never auto-promote past VERIFIED without explicit approval.
        if self.candidate.evidence == JoinEvidence::Llm
            && !self.approval.as_ref().is_some_and(AdminAck::is_valid)
        {
            return Err(JoinLifecycleError::ApprovalOrGateRequired(
                "LLM-proposed candidate joins require explicit human approval before activation (ING-021)"
                    .to_owned(),
            ));
        }

        if !can_activate_join(
            report,
            true,
            g8_passed,
            self.trust_tier,
            self.cross_trust,
            self.approval.as_ref(),
        ) {
            return Err(JoinLifecycleError::ApprovalOrGateRequired(
                "G1-G8 gates or cross-trust/T2 admin approval not satisfied (ING-040)".to_owned(),
            ));
        }

        self.status = JoinVerificationStatus::Canary;
        self.canary = Some(CanaryTracker {
            baseline_containment: containment.containment,
            started_at_ms: now_ms,
            executions: 0,
            sampled_keys: 0,
            matched_keys: 0,
            last_batch_coverage: containment.containment,
        });
        self.reason = "entered shadow canary mode".to_owned();
        Ok(())
    }

    /// Records a sampled shadow-canary execution batch (`ING-041`).
    ///
    /// - Reverts `CANARY -> VERIFIED` and returns a [`CanaryRevertAlert`] if key coverage drops
    ///   by `> 10` percentage points (`0.10`) below the G4 baseline containment.
    /// - Promotes `CANARY -> ACTIVE` once `>= 100` executions OR `>= 10 minutes` (`600_000 ms`)
    ///   have completed within tolerance.
    pub fn record_canary_execution(
        &mut self,
        lhs_keys_sampled: u64,
        rhs_keys_matched: u64,
        now_ms: u64,
    ) -> Result<Option<CanaryRevertAlert>, JoinLifecycleError> {
        if self.status != JoinVerificationStatus::Canary {
            return Err(JoinLifecycleError::InvalidTransition {
                from: self.status,
                to: JoinVerificationStatus::Active,
                reason: "join is not currently in CANARY state".to_owned(),
            });
        }

        let Some(tracker) = self.canary.as_mut() else {
            return Err(JoinLifecycleError::ApprovalOrGateRequired(
                "missing canary tracker state".to_owned(),
            ));
        };

        tracker.executions = tracker.executions.saturating_add(1);
        tracker.sampled_keys = tracker.sampled_keys.saturating_add(lhs_keys_sampled);
        tracker.matched_keys = tracker.matched_keys.saturating_add(rhs_keys_matched);

        let batch_coverage = if lhs_keys_sampled == 0 {
            tracker.last_batch_coverage
        } else {
            (rhs_keys_matched as f64 / lhs_keys_sampled as f64).min(1.0)
        };
        tracker.last_batch_coverage = batch_coverage;

        let aggregate_coverage = if tracker.sampled_keys == 0 {
            batch_coverage
        } else {
            (tracker.matched_keys as f64 / tracker.sampled_keys as f64).min(1.0)
        };

        let drop = tracker.baseline_containment - aggregate_coverage;
        if drop > MAX_CANARY_COVERAGE_DROP {
            let alert = CanaryRevertAlert {
                join_id: format!(
                    "{}.{}->{}.{}",
                    self.candidate.from.type_name,
                    self.candidate.from.field_path,
                    self.candidate.to.type_name,
                    self.candidate.to.field_path
                ),
                expected_containment: tracker.baseline_containment,
                observed_key_coverage: aggregate_coverage,
                executions: tracker.executions,
                reason: format!(
                    "shadow canary key coverage {:.4} dropped {:.4} (> 0.10) below G4 sketch expectation {:.4}",
                    aggregate_coverage, drop, tracker.baseline_containment
                ),
            };
            self.status = JoinVerificationStatus::Verified;
            self.reason = alert.reason.clone();
            self.canary = None;
            return Ok(Some(alert));
        }

        let elapsed_ms = now_ms.saturating_sub(tracker.started_at_ms);
        if tracker.executions >= MIN_CANARY_EXECUTIONS || elapsed_ms >= MIN_CANARY_WINDOW_MS {
            self.status = JoinVerificationStatus::Active;
            self.reason = format!(
                "shadow canary passed ({} executions, {} ms elapsed, observed coverage {:.4})",
                tracker.executions, elapsed_ms, aggregate_coverage
            );
        }

        Ok(None)
    }

    /// Quarantines the join manually or from rollback attribution.
    pub fn quarantine(&mut self, reason: impl Into<String>) {
        self.status = JoinVerificationStatus::Quarantined;
        self.canary = None;
        self.reason = reason.into();
    }
}

/// Parsed adversarial fixture loaded from `tests/fixtures/joins/*.json`.
#[derive(Debug, Clone, PartialEq)]
pub struct JoinFixture {
    /// Fixture name.
    pub name: String,
    /// Expected post-G4 status (`"VERIFIED"`, `"REJECTED"`, `"PROPOSED"`).
    pub expected_status: JoinVerificationStatus,
    /// Tenant identifier.
    pub tenant_id: String,
    /// Candidate join specification.
    pub candidate: CandidateJoin,
    /// Whether target key is declared PK/unique.
    pub rhs_declared_unique: bool,
    /// Whether RHS connector supports batching.
    pub rhs_supports_batch: bool,
    /// LHS column sketch response.
    pub lhs_sketch: SketchResponse,
    /// RHS column sketch response.
    pub rhs_sketch: SketchResponse,
}

/// Parses a JSON join verification fixture from `tests/fixtures/joins/*.json`.
pub fn parse_join_fixture(json_src: &str) -> Result<JoinFixture, String> {
    let val = parse_bounded_json(
        json_src.as_bytes(),
        &ExtractLimits::default(),
        std::time::Instant::now(),
    )
    .map_err(|e| format!("fixture JSON parse failed: {e}"))?;

    let CisgValue::Object(root) = val else {
        return Err("fixture root must be a JSON object".to_owned());
    };

    let name = get_str(&root, "name")?;
    let expected_status = match get_str(&root, "expected_status")?.as_str() {
        "VERIFIED" => JoinVerificationStatus::Verified,
        "REJECTED" => JoinVerificationStatus::Rejected,
        "PROPOSED" => JoinVerificationStatus::Proposed,
        other => return Err(format!("unknown expected_status `{other}`")),
    };
    let tenant_id = get_str(&root, "tenant_id")?;
    let rhs_declared_unique = get_bool(&root, "rhs_declared_unique")?;
    let rhs_supports_batch = get_bool(&root, "rhs_supports_batch")?;

    let cand_obj = get_obj(&root, "candidate")?;
    let candidate = CandidateJoin {
        from: crate::cisg::FieldRef {
            type_name: get_str(cand_obj, "from_type")?,
            field_path: get_str(cand_obj, "from_field")?,
        },
        to: crate::cisg::FieldRef {
            type_name: get_str(cand_obj, "to_type")?,
            field_path: get_str(cand_obj, "to_field")?,
        },
        cardinality: match get_str(cand_obj, "cardinality")?.as_str() {
            "ONE_TO_ONE" => JoinCardinality::OneToOne,
            "MANY_TO_ONE" => JoinCardinality::ManyToOne,
            "ONE_TO_MANY" => JoinCardinality::OneToMany,
            other => return Err(format!("unknown cardinality `{other}`")),
        },
        evidence: match get_str(cand_obj, "evidence")?.as_str() {
            "FK" => JoinEvidence::Fk,
            "DECLARED" => JoinEvidence::Declared,
            "NAME_TYPE" => JoinEvidence::NameType,
            "LLM" => JoinEvidence::Llm,
            other => return Err(format!("unknown evidence `{other}`")),
        },
    };

    let lhs_sketch = parse_sketch_obj(get_obj(&root, "lhs_sketch")?)?;
    let rhs_sketch = parse_sketch_obj(get_obj(&root, "rhs_sketch")?)?;

    Ok(JoinFixture {
        name,
        expected_status,
        tenant_id,
        candidate,
        rhs_declared_unique,
        rhs_supports_batch,
        lhs_sketch,
        rhs_sketch,
    })
}

fn get_obj<'a>(
    map: &'a BTreeMap<String, CisgValue>,
    key: &str,
) -> Result<&'a BTreeMap<String, CisgValue>, String> {
    match map.get(key) {
        Some(CisgValue::Object(o)) => Ok(o),
        _ => Err(format!("missing object field `{key}`")),
    }
}

fn get_str(map: &BTreeMap<String, CisgValue>, key: &str) -> Result<String, String> {
    match map.get(key) {
        Some(CisgValue::String(s)) => Ok(s.clone()),
        _ => Err(format!("missing string field `{key}`")),
    }
}

fn get_bool(map: &BTreeMap<String, CisgValue>, key: &str) -> Result<bool, String> {
    match map.get(key) {
        Some(CisgValue::Bool(b)) => Ok(*b),
        _ => Err(format!("missing bool field `{key}`")),
    }
}

fn get_u64(map: &BTreeMap<String, CisgValue>, key: &str) -> Result<u64, String> {
    match map.get(key) {
        Some(CisgValue::Int(i)) if *i >= 0 => {
            u64::try_from(*i).map_err(|_| format!("invalid integer for `{key}`"))
        }
        _ => Err(format!("missing non-negative int field `{key}`")),
    }
}

fn get_f64(map: &BTreeMap<String, CisgValue>, key: &str) -> Result<f64, String> {
    match map.get(key) {
        Some(CisgValue::Float(f)) => Ok(*f),
        Some(CisgValue::Int(i)) => Ok(*i as f64),
        _ => Err(format!("missing numeric field `{key}`")),
    }
}

fn parse_sketch_obj(obj: &BTreeMap<String, CisgValue>) -> Result<SketchResponse, String> {
    let hashes = match obj.get("bottom_k_hashes") {
        Some(CisgValue::Array(arr)) => arr
            .iter()
            .map(|item| match item {
                CisgValue::Int(i) if *i >= 0 => {
                    u64::try_from(*i).map_err(|_| "invalid u64 hash".to_owned())
                }
                CisgValue::String(s) => {
                    if let Some(hex) = s.strip_prefix("0x") {
                        u64::from_str_radix(hex, 16).map_err(|e| format!("bad hex hash: {e}"))
                    } else {
                        s.parse::<u64>()
                            .map_err(|e| format!("bad decimal hash: {e}"))
                    }
                }
                _ => Err("bottom_k_hashes items must be ints or numeric strings".to_owned()),
            })
            .collect::<Result<Vec<u64>, String>>()?,
        _ => return Err("missing bottom_k_hashes array".to_owned()),
    };

    Ok(SketchResponse {
        distinct_estimate: get_u64(obj, "distinct_estimate")?,
        null_ratio: get_f64(obj, "null_ratio")?,
        uniqueness_ratio: get_f64(obj, "uniqueness_ratio")?,
        bottom_k_hashes: hashes,
        sample_count: u32::try_from(get_u64(obj, "sample_count")?)
            .map_err(|_| "sample_count overflow u32".to_owned())?,
        value_kind: get_str(obj, "value_kind")?,
        exact: get_bool(obj, "exact")?,
    })
}

/// Ensures Gate `G4` summary converts cleanly into a `GateResult` wrapper if requested by tests.
#[must_use]
pub fn g4_summary_outcome(evals: &[JoinG4Evaluation]) -> GateOutcome {
    let rejected: Vec<String> = evals
        .iter()
        .filter(|e| e.status == JoinVerificationStatus::Rejected)
        .map(|e| e.join_id.clone())
        .collect();
    if rejected.is_empty() {
        GateOutcome::Passed
    } else {
        GateOutcome::CandidateDropped {
            dropped_joins: rejected,
        }
    }
}

/// Placeholder helper showing G4 gate metadata for reporting (`GateKind::Hard`).
#[must_use]
pub const fn g4_gate_kind() -> GateKind {
    GateKind::Hard
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::cisg::FieldRef;
    use crate::gates::{GateId, GateOutcome, GateResult, GenerationDiff, VerificationReport};

    const FIXTURE_VALID: &str =
        include_str!("../../../../../tests/fixtures/joins/valid_customer_orders.json");
    const FIXTURE_SAME_TYPE_DIFF_ENTITY: &str =
        include_str!("../../../../../tests/fixtures/joins/same_type_different_entity.json");
    const FIXTURE_PARTIAL: &str =
        include_str!("../../../../../tests/fixtures/joins/partial_overlap.json");
    const FIXTURE_SKEWED: &str =
        include_str!("../../../../../tests/fixtures/joins/skewed_duplicates.json");
    const FIXTURE_EMPTY: &str =
        include_str!("../../../../../tests/fixtures/joins/empty_domain.json");

    fn dummy_passing_report() -> VerificationReport {
        let gate_ids = [
            GateId::G1SchemaValidity,
            GateId::G2TypeCoercion,
            GateId::G3KeyLookup,
            GateId::G6Boundedness,
            GateId::G7SecurityLint,
            GateId::G9ResourceBudget,
        ];
        let gates = gate_ids
            .into_iter()
            .map(|gate| GateResult {
                gate,
                kind: gate.kind(),
                outcome: GateOutcome::Passed,
                evidence: BTreeMap::new(),
                findings: Vec::new(),
            })
            .collect();
        VerificationReport {
            source_uri: "mem://orders.sql".to_owned(),
            spec_hash: "0".repeat(64),
            extractor_id: "test".to_owned(),
            extractor_version: "0.1.0".to_owned(),
            tool_versions: BTreeMap::new(),
            model_ids: Vec::new(),
            gates,
            diff: GenerationDiff::default(),
            audit_acks: Vec::new(),
            promotable: true,
        }
    }

    // spec: NGC-050, NGC-051, NGC-052
    #[test]
    fn join_verify_sketch_request_enforces_tenant_salt_and_readonly_service_principal() {
        let ok_ctx = JoinSketchContext {
            tenant_id: "tenant-alpha".to_owned(),
            salt: vec![1, 2, 3, 4],
            request_id: "req-42".to_owned(),
            principal_id: "svc:ingest".to_owned(),
            read_only_scopes: vec!["ngc:read".to_owned()],
            bottom_k: 1024,
            sample_size: 200_000, // clamped to MAX_SAMPLE (100_000) per NGC-050
            deadline_unix_ms: 1_800_000_000_000,
        };
        let req = build_sketch_request("listCustomers", "id", &ok_ctx)
            .expect("valid sketch context builds request");
        assert_eq!(req.sample_size, DEFAULT_SKETCH_SAMPLE_SIZE);
        assert_eq!(req.salt, vec![1, 2, 3, 4]);
        let call_ctx = req.ctx.as_ref().expect("CallContext present");
        assert_eq!(call_ctx.tenant_id, "tenant-alpha");
        let principal = call_ctx.principal.as_ref().expect("Principal present");
        assert_eq!(principal.kind, PrincipalKind::Service as i32);

        // Missing salt fails (NGC-051).
        let mut no_salt = ok_ctx.clone();
        no_salt.salt.clear();
        assert!(matches!(
            build_sketch_request("listCustomers", "id", &no_salt),
            Err(SketchValidationError::MissingSalt)
        ));

        // Write/mutate/admin scope rejected (NGC-052).
        let mut write_scope = ok_ctx.clone();
        write_scope.read_only_scopes = vec!["ngc:mutate".to_owned()];
        assert!(matches!(
            build_sketch_request("listCustomers", "id", &write_scope),
            Err(SketchValidationError::InvalidPrincipalOrScope(_))
        ));

        // Canonical value normalization removes formatting noise cleanly.
        assert_eq!(
            normalize_sketch_value("uuid", "550E8400-E29B-41D4-A716-446655440000"),
            "550e8400e29b41d4a716446655440000"
        );
        assert_eq!(normalize_sketch_value("integer", " 0042 "), "42");
    }

    // spec: NGC-051
    #[test]
    fn join_verify_union_bottom_k_containment_handles_skewed_rhs_sizes_and_malformed_hashes() {
        // LHS has 50 distinct hashes (`10, 20, ..., 500`); RHS is 4x larger (`200` hashes interleaved)
        // containing ALL of LHS plus 150 extra distinct values. True containment |A ∩ B| / |A| = 1.0.
        let lhs_hashes: Vec<u64> = (1..=50_u64).map(|i| i * 10).collect();
        let mut rhs_hashes = Vec::new();
        for i in 1..=200_u64 {
            rhs_hashes.push(i * 10);
            if rhs_hashes.len() < 200 && i <= 150 {
                rhs_hashes.push(i * 10 + 5);
            }
        }
        rhs_hashes.sort_unstable();
        rhs_hashes.dedup();
        rhs_hashes.truncate(200);

        let lhs = SketchResponse {
            distinct_estimate: 500,
            null_ratio: 0.0,
            uniqueness_ratio: 1.0,
            bottom_k_hashes: lhs_hashes,
            sample_count: 1_000,
            value_kind: "integer".to_owned(),
            exact: false,
        };
        let rhs = SketchResponse {
            distinct_estimate: 2_000,
            null_ratio: 0.0,
            uniqueness_ratio: 1.0,
            bottom_k_hashes: rhs_hashes,
            sample_count: 2_000,
            value_kind: "integer".to_owned(),
            exact: false,
        };

        let est =
            estimate_bottom_k_containment(&lhs, &rhs, 50).expect("containment estimate succeeds");
        assert_eq!(est.k_eff, 50);
        assert!(est.lhs_in_union > 0);
        assert!(
            (est.containment - 1.0).abs() < 1e-9,
            "expected full subset containment 1.0 despite larger RHS, got {}",
            est.containment
        );
        assert!(est.union_error_bound > 0.0);

        // Unsorted bottom_k_hashes rejected cleanly.
        let mut unsorted = lhs.clone();
        unsorted.bottom_k_hashes = vec![20, 10];
        assert!(matches!(
            estimate_bottom_k_containment(&unsorted, &rhs, 50),
            Err(SketchValidationError::MalformedSketchResponse(_))
        ));
    }

    // spec: ING-040, NGC-050, NGC-051, INV-06
    #[test]
    fn join_verify_adversarial_fixtures_match_expected_g4_verdicts() {
        let fixtures = [
            FIXTURE_VALID,
            FIXTURE_SAME_TYPE_DIFF_ENTITY,
            FIXTURE_PARTIAL,
            FIXTURE_SKEWED,
            FIXTURE_EMPTY,
        ];
        let thresholds = G4Thresholds::default();

        for raw in fixtures {
            let fx = parse_join_fixture(raw).expect("fixture JSON parses");
            let input = JoinVerificationInput {
                candidate: &fx.candidate,
                lhs_sketch: Some(&fx.lhs_sketch),
                rhs_sketch: Some(&fx.rhs_sketch),
                lhs_declared_unique: false,
                rhs_declared_unique: fx.rhs_declared_unique,
                rhs_supports_batch: fx.rhs_supports_batch,
            };
            let eval = evaluate_join_g4(&input, &thresholds);
            assert_eq!(
                eval.status, fx.expected_status,
                "fixture `{}` produced unexpected status {:?}: reasons={:?}",
                fx.name, eval.status, eval.reasons
            );
            assert!(
                !eval.status.visible_to_planner(),
                "INV-06: G4 output status `{:?}` must never be directly planner-visible",
                eval.status
            );
        }
    }

    // spec: ING-040, ING-041, INV-06
    #[test]
    fn join_verify_canary_soak_promotes_and_divergence_reverts_with_alert() {
        let valid_fx = parse_join_fixture(FIXTURE_VALID).expect("valid fixture parses");
        let report = dummy_passing_report();
        let eval = evaluate_join_g4(
            &JoinVerificationInput {
                candidate: &valid_fx.candidate,
                lhs_sketch: Some(&valid_fx.lhs_sketch),
                rhs_sketch: Some(&valid_fx.rhs_sketch),
                lhs_declared_unique: false,
                rhs_declared_unique: valid_fx.rhs_declared_unique,
                rhs_supports_batch: valid_fx.rhs_supports_batch,
            },
            &G4Thresholds::default(),
        );
        assert_eq!(eval.status, JoinVerificationStatus::Verified);

        // 1. Happy path: PROPOSED -> VERIFIED -> CANARY -> ACTIVE after 100 executions.
        let mut record = JoinLifecycleRecord::new(valid_fx.candidate.clone(), TrustTier::T1, false);
        assert!(!record.visible_to_planner());
        record.apply_g4_evaluation(&eval);
        assert_eq!(record.status, JoinVerificationStatus::Verified);
        assert!(!record.visible_to_planner());

        record
            .enter_canary(&report, true, 1_000)
            .expect("verified T1 join enters canary");
        assert_eq!(record.status, JoinVerificationStatus::Canary);
        assert!(!record.visible_to_planner());

        for step in 1..100 {
            let revert = record
                .record_canary_execution(100, 97, 1_000 + step * 1_000)
                .expect("canary sample accepted");
            assert!(revert.is_none());
            assert_eq!(record.status, JoinVerificationStatus::Canary);
        }
        let revert = record
            .record_canary_execution(100, 97, 1_000 + 100 * 1_000)
            .expect("100th sample promotes");
        assert!(revert.is_none());
        assert_eq!(record.status, JoinVerificationStatus::Active);
        assert!(record.visible_to_planner());

        // 2. Time-window soak path: 10 minutes (600_000 ms) promotes even with < 100 executions.
        let mut timed_record =
            JoinLifecycleRecord::new(valid_fx.candidate.clone(), TrustTier::T1, false);
        timed_record.apply_g4_evaluation(&eval);
        timed_record
            .enter_canary(&report, true, 10_000)
            .expect("enter canary");
        assert!(
            timed_record
                .record_canary_execution(50, 49, 610_000)
                .expect("10-min sample promotes")
                .is_none()
        );
        assert_eq!(timed_record.status, JoinVerificationStatus::Active);

        // 3. Divergence > 10 percentage points reverts CANARY -> VERIFIED with alert.
        let mut bad_canary =
            JoinLifecycleRecord::new(valid_fx.candidate.clone(), TrustTier::T1, false);
        bad_canary.apply_g4_evaluation(&eval);
        bad_canary
            .enter_canary(&report, true, 1_000)
            .expect("enter canary");
        let alert = bad_canary
            .record_canary_execution(100, 75, 2_000)
            .expect("divergence handled")
            .expect("alert emitted for 0.98 -> 0.75 drop");
        assert_eq!(bad_canary.status, JoinVerificationStatus::Verified);
        assert!(!bad_canary.visible_to_planner());
        assert!(alert.observed_key_coverage < 0.80);

        // 4. LLM-proposed joins NEVER enter canary/active without human approval (ING-021).
        let mut llm_cand = valid_fx.candidate;
        llm_cand.evidence = JoinEvidence::Llm;
        llm_cand.from = FieldRef {
            type_name: "Order".to_owned(),
            field_path: "customer_id".to_owned(),
        };
        let mut llm_record = JoinLifecycleRecord::new(llm_cand, TrustTier::T1, false);
        llm_record.apply_g4_evaluation(&eval);
        let err = llm_record
            .enter_canary(&report, true, 1_000)
            .expect_err("LLM join without approval must fail");
        assert!(matches!(err, JoinLifecycleError::ApprovalOrGateRequired(_)));
        llm_record.set_approval(AdminAck {
            actor: "admin@acme.test".to_owned(),
            reason: "verified FK relationship in runbook".to_owned(),
            timestamp_utc: "2026-11-14T12:00:00Z".to_owned(),
        });
        llm_record
            .enter_canary(&report, true, 1_000)
            .expect("approved LLM join enters canary");
    }
}
