//! Per-request context, principal identity, cost budget, and W3C trace context (`L0`).

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::Arc;
use std::time::{Duration, Instant};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

use crate::error::{ErrorCode, GapError, GapResult};
use crate::ids::{IdempotencyKey, TenantId, TenantScope};
use crate::snapshot::Snapshot;

/// Default peer-to-peer hop ceiling (`INV-20`).
pub const DEFAULT_MAX_HOPS: u8 = 4;

/// Principal category (`spec/07-security.md` §4, `proto/gap/ngc/v1/ngc.proto`).
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum PrincipalKind {
    /// Interactive human user (`HUMAN`).
    Human,
    /// Autonomous or delegated AI agent (`AGENT`).
    Agent,
    /// Machine/service workload (`SERVICE`).
    Service,
}

impl PrincipalKind {
    /// Canonical lowercase label for telemetry attributes (`gap.principal_kind`).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Human => "human",
            Self::Agent => "agent",
            Self::Service => "service",
        }
    }
}

/// Typed attribute value on a [`Principal`] (`spec/07-security.md` §4).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum AttrValue {
    /// Boolean claim.
    Bool(bool),
    /// Signed 64-bit integer claim.
    Int(i64),
    /// UTF-8 string claim.
    Text(String),
    /// List of UTF-8 string claims.
    List(Vec<String>),
}

/// Entry in a principal's delegation chain (`spec/07-security.md` §4, `SEC-012`).
#[derive(
    Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(deny_unknown_fields)]
pub struct ActorRef {
    /// Authenticated actor identifier.
    pub id: String,
    /// Kind of delegated actor.
    pub kind: PrincipalKind,
}

/// Authenticated caller identity (`spec/07-security.md` §4, `SEC-010`..`SEC-012`).
///
/// Never stores raw bearer tokens (`SEC-011`, `INV-15`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Principal {
    /// Subject identifier from authentication (`sub` / SPIFFE ID / key ID).
    pub id: String,
    /// Caller classification.
    pub kind: PrincipalKind,
    /// Authenticated tenant binding (`SEC-010`, `INV-09`).
    pub tenant: TenantId,
    /// Granted scopes.
    pub scopes: BTreeSet<String>,
    /// Additional verified attributes exposed to Cedar policies.
    pub attrs: BTreeMap<String, AttrValue>,
    /// Delegation chain, outermost actor first (`SEC-012`).
    pub delegation: Vec<ActorRef>,
}

impl Principal {
    /// Creates a [`Principal`] with empty scopes, attributes, and delegation chain.
    #[must_use]
    pub fn new(id: impl Into<String>, kind: PrincipalKind, tenant: TenantId) -> Self {
        Self {
            id: id.into(),
            kind,
            tenant,
            scopes: BTreeSet::new(),
            attrs: BTreeMap::new(),
            delegation: Vec::new(),
        }
    }
}

/// Per-request query cost budget (`GQL-041`, `spec/01-architecture.md` §4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Budget {
    /// Configured ceiling for the request.
    pub max_cost_units: u32,
    /// Remaining unspent cost units.
    pub remaining_cost_units: u32,
}

impl Budget {
    /// Creates an unspent [`Budget`] of `max_cost_units`.
    #[must_use]
    pub const fn new(max_cost_units: u32) -> Self {
        Self {
            max_cost_units,
            remaining_cost_units: max_cost_units,
        }
    }

    /// Consumes `units` from the remaining budget, failing with `GAP_BUDGET_EXCEEDED` on overflow.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorCode::BudgetExceeded`] if `units > self.remaining_cost_units`.
    pub fn try_charge(&mut self, units: u32) -> GapResult<()> {
        if units > self.remaining_cost_units {
            return Err(GapError::new(
                ErrorCode::BudgetExceeded,
                format!(
                    "budget exceeded: requested {units} cost units with {} remaining (max {})",
                    self.remaining_cost_units, self.max_cost_units
                ),
            ));
        }
        self.remaining_cost_units -= units;
        Ok(())
    }
}

/// Error returned when parsing an invalid W3C `traceparent` header (`OPS-010`).
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("invalid W3C traceparent `{value}`: {reason}")]
pub struct TraceParseError {
    /// Rejected header value.
    pub value: String,
    /// Validation failure reason.
    pub reason: &'static str,
}

/// W3C Trace Context and peer-traversal path (`OPS-010`, `INV-20`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TraceCtx {
    /// 16-byte W3C trace-id (never all-zero).
    pub trace_id: [u8; 16],
    /// 8-byte W3C parent/span-id (never all-zero).
    pub span_id: [u8; 8],
    /// W3C 8-bit trace-flags (`0x01` = sampled).
    pub trace_flags: u8,
    /// Optional W3C `tracestate` header value.
    pub tracestate: Option<String>,
    /// Ordered GAP peer node IDs traversed so far (`INV-20`, `NGC-042`).
    pub path: Vec<String>,
}

fn decode_lower_hex<const N: usize>(hex: &str) -> Option<[u8; N]> {
    if hex.len() != N * 2 {
        return None;
    }
    let bytes = hex.as_bytes();
    let mut out = [0_u8; N];
    let mut i = 0;
    while i < N {
        let hi = match bytes[i * 2] {
            b @ b'0'..=b'9' => b - b'0',
            b @ b'a'..=b'f' => b - b'a' + 10,
            _ => return None,
        };
        let lo = match bytes[i * 2 + 1] {
            b @ b'0'..=b'9' => b - b'0',
            b @ b'a'..=b'f' => b - b'a' + 10,
            _ => return None,
        };
        out[i] = (hi << 4) | lo;
        i += 1;
    }
    Some(out)
}

fn encode_lower_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(char::from(HEX[usize::from(b >> 4)]));
        out.push(char::from(HEX[usize::from(b & 0x0f)]));
    }
    out
}

impl TraceCtx {
    /// Generates a fresh root [`TraceCtx`] using random v4 UUID bytes.
    #[must_use]
    pub fn generate_root(sampled: bool) -> Self {
        let mut trace_id = *Uuid::new_v4().as_bytes();
        if trace_id == [0; 16] {
            trace_id[15] = 1;
        }
        let span_uuid = Uuid::new_v4();
        let mut span_id = [0_u8; 8];
        span_id.copy_from_slice(&span_uuid.as_bytes()[..8]);
        if span_id == [0; 8] {
            span_id[7] = 1;
        }
        Self {
            trace_id,
            span_id,
            trace_flags: u8::from(sampled),
            tracestate: None,
            path: Vec::new(),
        }
    }

    /// Parses a W3C Trace Context `traceparent` header (`00-<trace-id>-<parent-id>-<flags>`).
    ///
    /// # Errors
    ///
    /// Returns [`TraceParseError`] if syntax, version, hex casing, or non-zero ID rules fail.
    pub fn parse_traceparent(raw: &str) -> Result<Self, TraceParseError> {
        let invalid = |reason: &'static str| TraceParseError {
            value: raw.to_owned(),
            reason,
        };
        let parts: Vec<&str> = raw.split('-').collect();
        if parts.len() < 4 {
            return Err(invalid("expected 4 hyphen-separated fields"));
        }
        let [ver] = decode_lower_hex::<1>(parts[0]).ok_or_else(|| invalid("invalid version"))?;
        if ver == 0xff {
            return Err(invalid("version ff is forbidden by W3C Trace Context"));
        }
        if ver == 0x00 && parts.len() != 4 {
            return Err(invalid("version 00 forbids trailing fields"));
        }
        let trace_id =
            decode_lower_hex::<16>(parts[1]).ok_or_else(|| invalid("invalid trace-id hex"))?;
        if trace_id == [0_u8; 16] {
            return Err(invalid("all-zero trace-id is forbidden"));
        }
        let span_id =
            decode_lower_hex::<8>(parts[2]).ok_or_else(|| invalid("invalid parent-id hex"))?;
        if span_id == [0_u8; 8] {
            return Err(invalid("all-zero parent-id is forbidden"));
        }
        let [trace_flags] =
            decode_lower_hex::<1>(parts[3]).ok_or_else(|| invalid("invalid trace-flags hex"))?;

        Ok(Self {
            trace_id,
            span_id,
            trace_flags,
            tracestate: None,
            path: Vec::new(),
        })
    }

    /// Formats the W3C `traceparent` header string (`OPS-010`).
    #[must_use]
    pub fn traceparent(&self) -> String {
        format!(
            "00-{}-{}-{:02x}",
            encode_lower_hex(&self.trace_id),
            encode_lower_hex(&self.span_id),
            self.trace_flags
        )
    }

    /// Lowercase 32-char hex W3C `trace_id`.
    #[must_use]
    pub fn trace_id_hex(&self) -> String {
        encode_lower_hex(&self.trace_id)
    }

    /// Lowercase 16-char hex W3C `span_id`.
    #[must_use]
    pub fn span_id_hex(&self) -> String {
        encode_lower_hex(&self.span_id)
    }

    /// Returns `true` when the W3C sampled bit (`0x01`) is set.
    #[must_use]
    pub const fn is_sampled(&self) -> bool {
        (self.trace_flags & 0x01) != 0
    }
}

impl fmt::Display for TraceCtx {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.traceparent())
    }
}

/// Immutable per-request execution context with a pinned [`Snapshot`] (`spec/01-architecture.md` §4,
/// `INV-01`, `INV-09`, `INV-12`, `INV-20`, `ARC-010`).
#[derive(Debug, Clone)]
pub struct RequestContext {
    /// Correlation/request UUIDv7 (`spec/01-architecture.md` §4).
    pub request_id: Uuid,
    /// Authenticated tenant isolation domain (`INV-09`, `ADR-018`).
    pub tenant: TenantId,
    /// Authenticated caller principal (`SEC-010`).
    pub principal: Principal,
    /// Monotonic request deadline (`INV-12`).
    pub deadline: Instant,
    /// Request cost budget (`GQL-041`).
    pub budget: Budget,
    /// Distributed W3C trace context (`OPS-010`).
    pub trace: TraceCtx,
    /// Peer-to-peer hop counter (`INV-20`).
    pub hop: u8,
    /// Optional client idempotency key for mutations (`NGC-030`).
    pub idempotency_key: Option<IdempotencyKey>,
    /// Pinned supergraph snapshot for the lifetime of this request (`INV-01`, `ARC-010`).
    pub snapshot: Arc<Snapshot>,
}

impl RequestContext {
    /// Constructs a [`RequestContext`], verifying that `principal.tenant == tenant` (`INV-09`,
    /// `SEC-010`).
    ///
    /// # Errors
    ///
    /// Returns [`ErrorCode::TenantMismatch`] if `principal.tenant != tenant`.
    #[allow(clippy::too_many_arguments)]
    pub fn try_new(
        request_id: Uuid,
        tenant: TenantId,
        principal: Principal,
        deadline: Instant,
        budget: Budget,
        trace: TraceCtx,
        hop: u8,
        idempotency_key: Option<IdempotencyKey>,
        snapshot: Arc<Snapshot>,
    ) -> GapResult<Self> {
        if principal.tenant != tenant {
            return Err(GapError::new(
                ErrorCode::TenantMismatch,
                "principal tenant does not match request tenant",
            ));
        }
        Ok(Self {
            request_id,
            tenant,
            principal,
            deadline,
            budget,
            trace,
            hop,
            idempotency_key,
            snapshot,
        })
    }

    /// Mints the type-level [`TenantScope`] for cache/store lookups (`INV-09`, `INV-19`).
    #[must_use]
    pub fn tenant_scope(&self) -> TenantScope {
        self.tenant.scope()
    }

    /// Computes a downstream connector deadline `<= self.deadline - margin` (`INV-12`).
    ///
    /// # Errors
    ///
    /// Returns [`ErrorCode::DeadlineExceeded`] if `self.deadline - margin <= now`.
    pub fn connector_deadline(&self, now: Instant, margin: Duration) -> GapResult<Instant> {
        let Some(sub_deadline) = self.deadline.checked_sub(margin) else {
            return Err(GapError::new(
                ErrorCode::DeadlineExceeded,
                "request deadline cannot accommodate connector safety margin",
            ));
        };
        if sub_deadline <= now {
            return Err(GapError::new(
                ErrorCode::DeadlineExceeded,
                "request deadline exceeded before connector call",
            ));
        }
        Ok(sub_deadline)
    }

    /// Advances peer-to-peer forwarding state (`hop + 1`, appends `node_id` to `trace.path`),
    /// enforcing loop detection and `max_hops` (`INV-20`, `NGC-042`).
    ///
    /// # Errors
    ///
    /// Returns [`ErrorCode::LoopDetected`] if `node_id` is already in `trace.path`, or
    /// [`ErrorCode::HopLimit`] if `hop + 1 > max_hops`.
    pub fn advance_peer_hop(&self, node_id: &str, max_hops: u8) -> GapResult<Self> {
        if self.trace.path.iter().any(|visited| visited == node_id) {
            return Err(GapError::new(
                ErrorCode::LoopDetected,
                format!("peer loop detected at node `{node_id}`"),
            ));
        }
        let next_hop = self
            .hop
            .checked_add(1)
            .ok_or_else(|| GapError::new(ErrorCode::HopLimit, "peer hop counter overflowed u8"))?;
        if next_hop > max_hops {
            return Err(GapError::new(
                ErrorCode::HopLimit,
                format!("peer hop count {next_hop} exceeds max_hops {max_hops}"),
            ));
        }
        let mut next = self.clone();
        next.hop = next_hop;
        next.trace.path.push(node_id.to_owned());
        Ok(next)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // spec: INV-09, SEC-010 (ADR-018)
    #[test]
    fn inv_09_request_context_enforces_tenant_match_and_mints_tenant_scope() {
        let acme: TenantId = "acme".parse().expect("valid tenant");
        let globex: TenantId = "globex".parse().expect("valid tenant");
        let snap = Arc::new(Snapshot::empty(7));
        let now = Instant::now();

        let mismatched = RequestContext::try_new(
            Uuid::now_v7(),
            acme.clone(),
            Principal::new("user-1", PrincipalKind::Human, globex),
            now + Duration::from_secs(30),
            Budget::new(10_000),
            TraceCtx::generate_root(true),
            0,
            None,
            Arc::clone(&snap),
        );
        let err = mismatched.expect_err("mismatched principal tenant must fail");
        assert_eq!(err.code(), ErrorCode::TenantMismatch);

        let ctx = RequestContext::try_new(
            Uuid::now_v7(),
            acme.clone(),
            Principal::new("agent-7", PrincipalKind::Agent, acme.clone()),
            now + Duration::from_secs(30),
            Budget::new(500),
            TraceCtx::generate_root(true),
            0,
            Some("idem-01".parse().expect("valid idempotency key")),
            Arc::clone(&snap),
        )
        .expect("matching tenant succeeds");

        assert_eq!(ctx.tenant_scope().tenant(), &acme);
        assert_eq!(ctx.snapshot.generation, 7);
    }

    // spec: OPS-010
    #[test]
    fn w3c_traceparent_parses_roundtrips_and_rejects_invalid_headers() {
        let raw = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
        let trace = TraceCtx::parse_traceparent(raw).expect("valid traceparent");
        assert_eq!(trace.traceparent(), raw);
        assert!(trace.is_sampled());
        assert_eq!(trace.trace_id_hex(), "4bf92f3577b34da6a3ce929d0e0e4736");
        assert_eq!(trace.span_id_hex(), "00f067aa0ba902b7");

        for bad in [
            "00-00000000000000000000000000000000-00f067aa0ba902b7-01",
            "00-4bf92f3577b34da6a3ce929d0e0e4736-0000000000000000-01",
            "ff-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01",
            "00-4BF92F3577B34DA6A3CE929D0E0E4736-00f067aa0ba902b7-01",
            "00-short-00f067aa0ba902b7-01",
        ] {
            assert!(
                TraceCtx::parse_traceparent(bad).is_err(),
                "expected `{bad}` to fail W3C traceparent validation"
            );
        }
    }

    // spec: INV-12, INV-20, GQL-041, NGC-042
    #[test]
    fn inv_12_and_inv_20_connector_deadline_budget_and_peer_hop_guards() {
        let tenant = TenantId::default_tenant();
        let now = Instant::now();
        let mut ctx = RequestContext::try_new(
            Uuid::now_v7(),
            tenant.clone(),
            Principal::new("svc-a", PrincipalKind::Service, tenant),
            now + Duration::from_millis(500),
            Budget::new(100),
            TraceCtx::generate_root(false),
            0,
            None,
            Arc::new(Snapshot::empty(1)),
        )
        .expect("valid context");

        // Budget charging
        ctx.budget.try_charge(60).expect("60 <= 100");
        assert_eq!(ctx.budget.remaining_cost_units, 40);
        let budget_err = ctx
            .budget
            .try_charge(41)
            .expect_err("41 > 40 must fail with GAP_BUDGET_EXCEEDED");
        assert_eq!(budget_err.code(), ErrorCode::BudgetExceeded);

        // Connector deadline <= request.deadline - margin (INV-12)
        let sub = ctx
            .connector_deadline(now, Duration::from_millis(50))
            .expect("deadline with 50ms margin succeeds");
        assert!(sub <= ctx.deadline - Duration::from_millis(50));
        let expired = ctx
            .connector_deadline(now, Duration::from_millis(500))
            .expect_err("zero remaining time must fail");
        assert_eq!(expired.code(), ErrorCode::DeadlineExceeded);

        // Peer hop progression, loop detection, and hop cap (INV-20)
        let hop1 = ctx.advance_peer_hop("node-a", 2).expect("hop 1 allowed");
        assert_eq!(hop1.hop, 1);
        let loop_err = hop1
            .advance_peer_hop("node-a", 2)
            .expect_err("revisiting node-a must fail with GAP_LOOP_DETECTED");
        assert_eq!(loop_err.code(), ErrorCode::LoopDetected);

        let hop2 = hop1.advance_peer_hop("node-b", 2).expect("hop 2 allowed");
        let hop_limit_err = hop2
            .advance_peer_hop("node-c", 2)
            .expect_err("hop 3 > max_hops 2 must fail with GAP_HOP_LIMIT");
        assert_eq!(hop_limit_err.code(), ErrorCode::HopLimit);
    }
}
