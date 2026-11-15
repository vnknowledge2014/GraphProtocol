//! Deterministic Cedar policy evaluation engine, delegation intersection, and atomic hot reload
//! (`SEC-012`, `SEC-020`, `SEC-022`, `INV-02`, `INV-04`).

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use gap_core::context::{ActorRef, AttrValue, Principal, RequestContext};
use gap_core::error::{ErrorCode, GapError, GapResult};
use gap_core::ids::TenantId;
use gap_core::snapshot::{BranchStatus, SideEffectLevel, TrustTier};

use super::parser::{
    ActionConstraint, AttrRoot, ClauseKind, CmpOp, Effect, EntityUid, Expr, Literal, Policy,
    PrincipalConstraint, ResourceConstraint, parse_and_validate_policies,
};
use super::schema::{
    AuthzAction, CedarSchema, DEFAULT_CEDAR_POLICIES, DEFAULT_CEDAR_SCHEMA, ResourceKind,
};
use crate::authn::crypto::{blake3_hash, hex_encode};

/// Evaluation decision outcome (`INV-04`, `SEC-020`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecisionKind {
    /// Action permitted (`permit` matched and no `forbid` matched).
    Allow,
    /// Action denied (either default deny or at least one `forbid` matched).
    Deny,
}

/// Canonical resource attributes exposed to Cedar policies (`SEC-022`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResourceAttributes {
    /// Cedar resource entity type (`Element`, `Operation`, `Branch`, etc.).
    pub kind: ResourceKind,
    /// Stable resource identifier (`operation_id`, `branch.field`, `task_id`, etc.).
    pub id: String,
    /// Owning tenant (`SEC-022`).
    pub tenant: TenantId,
    /// Effective trust tier (`T0`, `T1`, `T2`).
    pub trust: TrustTier,
    /// Verified side-effect level (`NONE`, `REVERSIBLE`, `IRREVERSIBLE`, `UNKNOWN`).
    pub side_effect: SideEffectLevel,
    /// Capability group identifier (if applicable).
    pub group: String,
    /// Owning branch slug (if applicable).
    pub branch: String,
    /// Sensitivity classification (`"normal"`, `"pii"`, `"confidential"`, `"restricted"`).
    pub classification: String,
    /// Lifecycle status (`ACTIVE`, `DRIFT`, `QUARANTINED`, `DISABLED`).
    pub status: BranchStatus,
    /// Requester principal ID when resource is an `Approval`.
    pub requester: String,
}

impl ResourceAttributes {
    /// Builds resource attributes for a tenant-scoped operation or field (`SEC-022`).
    #[must_use]
    pub fn operation(
        id: impl Into<String>,
        tenant: TenantId,
        branch: impl Into<String>,
        trust: TrustTier,
        side_effect: SideEffectLevel,
    ) -> Self {
        Self {
            kind: ResourceKind::Operation,
            id: id.into(),
            tenant,
            trust,
            side_effect,
            group: String::new(),
            branch: branch.into(),
            classification: "normal".to_owned(),
            status: BranchStatus::Active,
            requester: String::new(),
        }
    }

    #[must_use]
    fn trust_str(&self) -> &'static str {
        match self.trust {
            TrustTier::T0 => "T0",
            TrustTier::T1 => "T1",
            TrustTier::T2 => "T2",
        }
    }

    #[must_use]
    fn side_effect_str(&self) -> &'static str {
        match self.side_effect {
            SideEffectLevel::None => "NONE",
            SideEffectLevel::Reversible => "REVERSIBLE",
            SideEffectLevel::Irreversible => "IRREVERSIBLE",
            SideEffectLevel::Unknown => "UNKNOWN",
        }
    }

    #[must_use]
    fn status_str(&self) -> &'static str {
        match self.status {
            BranchStatus::Active => "ACTIVE",
            BranchStatus::Drift => "DRIFT",
            BranchStatus::Quarantined => "QUARANTINED",
            BranchStatus::Disabled => "DISABLED",
        }
    }
}

/// Request-level Cedar `context` attributes (`SEC-022`, `SEC-084`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthzContext {
    /// Authenticated tenant identifier.
    pub tenant: TenantId,
    /// Evaluation timestamp in UNIX milliseconds.
    pub time: i64,
    /// Peer-to-peer delegation hop counter (`INV-20`).
    pub hop: i64,
    /// Deterministic session risk score computed by Rule-of-Two tracker (`0..=100`, `SEC-084`).
    pub risk_score: i64,
    /// Whether an approval record exists for this mutation.
    pub approved: bool,
    /// Whether approver differs from requester (`four-eyes`, `RUN-071`).
    pub approver_is_different: bool,
    /// Context-level effective scopes.
    pub scopes: BTreeSet<String>,
}

impl AuthzContext {
    /// Constructs an evaluation context pinned to `req_ctx` (`INV-01`, `SEC-010`).
    #[must_use]
    pub fn from_request(req_ctx: &RequestContext, now_unix_ms: i64, risk_score: i64) -> Self {
        Self {
            tenant: req_ctx.tenant.clone(),
            time: now_unix_ms,
            hop: i64::from(req_ctx.hop),
            risk_score,
            approved: false,
            approver_is_different: false,
            scopes: req_ctx.principal.scopes.clone(),
        }
    }

    /// Creates a minimal read-only evaluation context for `tenant` (`risk_score = 0`).
    #[must_use]
    pub fn minimal(tenant: TenantId) -> Self {
        Self {
            tenant,
            time: 0,
            hop: 0,
            risk_score: 0,
            approved: false,
            approver_is_different: false,
            scopes: BTreeSet::new(),
        }
    }
}

/// Result of evaluating a single Cedar policy request (`SEC-020`, `SEC-024`, `TM-22`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvalDecision {
    /// Final decision (`Allow` or `Deny`).
    pub decision: DecisionKind,
    /// Policy IDs that matched and determined the outcome (recorded strictly in audit logs).
    pub determining_policies: Vec<String>,
    /// Full 64-char BLAKE3 hex version of the compiled policy set (`SEC-020`).
    pub policy_version_hex: String,
    /// Deterministic 64-bit policy digest stored in `Snapshot.policy_version` (`TM-22`).
    pub policy_version_u64: u64,
    /// Principal ID chain evaluated (actor followed by delegators under `SEC-012`).
    pub evaluated_actors: Vec<String>,
}

/// Immutable, compiled Cedar policy set pinned per request without locking (`SEC-020`, `INV-02`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompiledPolicySet {
    schema: CedarSchema,
    policies: Vec<Policy>,
    policy_version_hex: String,
    policy_version_u64: u64,
}

impl CompiledPolicySet {
    /// Compiles and validates the repository default `policies/schema.cedarschema` + `policies/default.cedar`.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorCode::ValidationError`] if built-in schema/policy assets fail validation.
    pub fn default_builtin() -> GapResult<Self> {
        Self::compile(DEFAULT_CEDAR_SCHEMA, &[DEFAULT_CEDAR_POLICIES])
    }

    /// Compiles and schema-validates `policy_sources` against `schema_source` (`SEC-020`).
    ///
    /// Computes `policy_version = blake3(canonical_schema || canonical_policies)`.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorCode::ValidationError`] on schema or policy validation errors.
    pub fn compile(schema_source: &str, policy_sources: &[&str]) -> GapResult<Self> {
        let schema = CedarSchema::parse(schema_source)?;
        let mut policies = Vec::new();
        for src in policy_sources {
            let parsed = parse_and_validate_policies(src, &schema)?;
            policies.extend(parsed);
        }

        let mut digest_input = String::from(schema.canonical_text.trim());
        digest_input.push('\n');
        let mut sorted_srcs: Vec<&str> = policy_sources.iter().map(|s| s.trim()).collect();
        sorted_srcs.sort_unstable();
        for src in sorted_srcs {
            digest_input.push_str("---\n");
            digest_input.push_str(src);
            digest_input.push('\n');
        }
        let digest = blake3_hash(digest_input.as_bytes());
        let policy_version_hex = hex_encode(&digest);
        let policy_version_u64 = u64::from_be_bytes([
            digest[0], digest[1], digest[2], digest[3], digest[4], digest[5], digest[6], digest[7],
        ]);

        Ok(Self {
            schema,
            policies,
            policy_version_hex,
            policy_version_u64,
        })
    }

    /// Returns the schema associated with this compiled policy set.
    #[must_use]
    pub const fn schema(&self) -> &CedarSchema {
        &self.schema
    }

    /// Returns the number of loaded Cedar policies.
    #[must_use]
    pub const fn policy_count(&self) -> usize {
        self.policies.len()
    }

    /// Returns the 64-character lowercase hex BLAKE3 digest of this policy set (`SEC-020`).
    #[must_use]
    pub fn policy_version_hex(&self) -> &str {
        &self.policy_version_hex
    }

    /// Returns the 64-bit policy version stored inside `Snapshot.policy_version` (`TM-22`).
    #[must_use]
    pub const fn policy_version_u64(&self) -> u64 {
        self.policy_version_u64
    }

    /// Evaluates a single principal (`actor`) without delegators (`INV-04`).
    #[must_use]
    pub fn evaluate_single(
        &self,
        actor: &Principal,
        action: AuthzAction,
        resource: &ResourceAttributes,
        context: &AuthzContext,
    ) -> EvalDecision {
        let mut forbid_ids = Vec::new();
        let mut permit_ids = Vec::new();

        for policy in &self.policies {
            if policy_matches(policy, actor, action, resource, context) {
                match policy.effect {
                    Effect::Forbid => forbid_ids.push(policy.id.clone()),
                    Effect::Permit => permit_ids.push(policy.id.clone()),
                }
            }
        }

        let (decision, determining_policies) = if !forbid_ids.is_empty() {
            (DecisionKind::Deny, forbid_ids)
        } else if !permit_ids.is_empty() {
            (DecisionKind::Allow, permit_ids)
        } else {
            (DecisionKind::Deny, Vec::new())
        };

        EvalDecision {
            decision,
            determining_policies,
            policy_version_hex: self.policy_version_hex.clone(),
            policy_version_u64: self.policy_version_u64,
            evaluated_actors: vec![actor.id.clone()],
        }
    }

    /// Evaluates an actor together with an explicit list of delegation subjects (`SEC-012`).
    ///
    /// An action is allowed iff policy permits it for `actor` **and** every delegator in
    /// `delegators` (strict intersection semantics). Both sides are recorded in `evaluated_actors`.
    #[must_use]
    pub fn evaluate_with_delegators(
        &self,
        actor: &Principal,
        delegators: &[Principal],
        action: AuthzAction,
        resource: &ResourceAttributes,
        context: &AuthzContext,
    ) -> EvalDecision {
        let mut all_actors = Vec::with_capacity(1 + delegators.len());
        all_actors.push(actor.id.clone());
        let actor_res = self.evaluate_single(actor, action, resource, context);
        if actor_res.decision == DecisionKind::Deny {
            for d in delegators {
                all_actors.push(d.id.clone());
            }
            return EvalDecision {
                decision: DecisionKind::Deny,
                determining_policies: actor_res.determining_policies,
                policy_version_hex: self.policy_version_hex.clone(),
                policy_version_u64: self.policy_version_u64,
                evaluated_actors: all_actors,
            };
        }

        let mut permits = actor_res.determining_policies;
        for delegator in delegators {
            all_actors.push(delegator.id.clone());
            let sub_res = self.evaluate_single(delegator, action, resource, context);
            if sub_res.decision == DecisionKind::Deny {
                return EvalDecision {
                    decision: DecisionKind::Deny,
                    determining_policies: sub_res.determining_policies,
                    policy_version_hex: self.policy_version_hex.clone(),
                    policy_version_u64: self.policy_version_u64,
                    evaluated_actors: all_actors,
                };
            }
            permits.extend(sub_res.determining_policies);
        }

        EvalDecision {
            decision: DecisionKind::Allow,
            determining_policies: permits,
            policy_version_hex: self.policy_version_hex.clone(),
            policy_version_u64: self.policy_version_u64,
            evaluated_actors: all_actors,
        }
    }

    /// Evaluates `principal` alongside any embedded `principal.delegation` entries (`SEC-012`).
    ///
    /// For embedded [`ActorRef`] entries where full group membership is not supplied separately,
    /// synthetic subject principals inherit `principal.tenant` and any `delegator:<id>:groups`
    /// / `delegated_scopes` attributes, never elevated admin/operator groups from the outer actor.
    #[must_use]
    pub fn evaluate_principal(
        &self,
        principal: &Principal,
        action: AuthzAction,
        resource: &ResourceAttributes,
        context: &AuthzContext,
    ) -> EvalDecision {
        if principal.delegation.is_empty() {
            return self.evaluate_single(principal, action, resource, context);
        }
        let delegators: Vec<Principal> = principal
            .delegation
            .iter()
            .map(|actor_ref| synthesize_delegator_principal(principal, actor_ref))
            .collect();
        self.evaluate_with_delegators(principal, &delegators, action, resource, context)
    }
}

fn synthesize_delegator_principal(outer: &Principal, actor_ref: &ActorRef) -> Principal {
    let mut sub = Principal::new(actor_ref.id.clone(), actor_ref.kind, outer.tenant.clone());
    // Scopes embedded in `RequestContext` after RFC 8693 exchange are already intersected; keep
    // read/traverse scopes while stripping group elevation so agents cannot widen human rights.
    if let Some(AttrValue::List(delegated_scopes)) = outer.attrs.get("delegated_scopes") {
        sub.scopes = delegated_scopes.iter().cloned().collect();
    } else {
        sub.scopes = outer.scopes.clone();
    }
    let group_key = format!("delegator:{}:groups", actor_ref.id);
    if let Some(groups) = outer.attrs.get(&group_key) {
        sub.attrs.insert("groups".to_owned(), groups.clone());
    }
    sub
}

/// Atomic policy store supporting hot reload with rollback-on-failure (`SEC-020`, `INV-02`).
///
/// Request evaluation pins the returned `Arc<CompiledPolicySet>` once per request and holds
/// zero locks during authorization checks.
#[derive(Debug)]
pub struct PolicyStore {
    active: Mutex<Arc<CompiledPolicySet>>,
}

impl PolicyStore {
    /// Initializes a store from the compiled policy set.
    #[must_use]
    pub fn new(initial: CompiledPolicySet) -> Self {
        Self {
            active: Mutex::new(Arc::new(initial)),
        }
    }

    /// Initializes a store loaded with the built-in `policies/schema.cedarschema` and `policies/default.cedar`.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorCode::ValidationError`] if built-in policy compilation fails.
    pub fn default_builtin() -> GapResult<Self> {
        Ok(Self::new(CompiledPolicySet::default_builtin()?))
    }

    /// Pins the currently active immutable policy set (`INV-01`, `INV-02`).
    #[must_use]
    pub fn pin(&self) -> Arc<CompiledPolicySet> {
        match self.active.lock() {
            Ok(guard) => Arc::clone(&guard),
            Err(poisoned) => Arc::clone(&poisoned.into_inner()),
        }
    }

    /// Atomically validates and publishes a new schema + policy set (`SEC-020`).
    ///
    /// If validation or parsing fails, returns `Err` and preserves the previously active
    /// [`CompiledPolicySet`] unchanged.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorCode::ValidationError`] if compilation fails.
    pub fn reload(
        &self,
        schema_source: &str,
        policy_sources: &[&str],
    ) -> GapResult<Arc<CompiledPolicySet>> {
        let compiled = CompiledPolicySet::compile(schema_source, policy_sources)?;
        let next = Arc::new(compiled);
        match self.active.lock() {
            Ok(mut guard) => {
                *guard = Arc::clone(&next);
            }
            Err(mut poisoned) => {
                **poisoned.get_mut() = Arc::clone(&next);
            }
        }
        Ok(next)
    }
}

fn principal_groups(principal: &Principal) -> Vec<&str> {
    let mut groups = Vec::new();
    if let Some(val) = principal
        .attrs
        .get("groups")
        .or_else(|| principal.attrs.get("group"))
    {
        match val {
            AttrValue::Text(s) => groups.push(s.as_str()),
            AttrValue::List(items) => {
                for item in items {
                    groups.push(item.as_str());
                }
            }
            AttrValue::Bool(_) | AttrValue::Int(_) => {}
        }
    }
    groups
}

fn policy_matches(
    policy: &Policy,
    principal: &Principal,
    action: AuthzAction,
    resource: &ResourceAttributes,
    context: &AuthzContext,
) -> bool {
    if !match_principal(&policy.principal, principal) {
        return false;
    }
    if !match_action(&policy.action, action) {
        return false;
    }
    if !match_resource(&policy.resource, resource) {
        return false;
    }
    for (kind, expr) in &policy.clauses {
        let Some(EvalValue::Bool(val)) = eval_expr(expr, principal, resource, context) else {
            return false;
        };
        let satisfied = match kind {
            ClauseKind::When => val,
            ClauseKind::Unless => !val,
        };
        if !satisfied {
            return false;
        }
    }
    true
}

fn match_principal(constraint: &PrincipalConstraint, principal: &Principal) -> bool {
    match constraint {
        PrincipalConstraint::Any => true,
        PrincipalConstraint::Eq(uid) => uid.entity_type == "Principal" && uid.id == principal.id,
        PrincipalConstraint::In(uid) => {
            if uid.entity_type == "Group" {
                principal_groups(principal).contains(&uid.id.as_str())
            } else {
                uid.entity_type == "Principal" && uid.id == principal.id
            }
        }
    }
}

fn match_action(constraint: &ActionConstraint, action: AuthzAction) -> bool {
    match constraint {
        ActionConstraint::Any => true,
        ActionConstraint::Eq(expected) => *expected == action,
        ActionConstraint::In(set) => set.contains(&action),
    }
}

fn match_resource(constraint: &ResourceConstraint, resource: &ResourceAttributes) -> bool {
    match constraint {
        ResourceConstraint::Any => true,
        ResourceConstraint::Eq(uid) => {
            uid.entity_type == resource.kind.as_str() && uid.id == resource.id
        }
        ResourceConstraint::In(uid) => {
            if uid.entity_type == "Branch" {
                resource.branch == uid.id
            } else if uid.entity_type == "Tenant" {
                resource.tenant.as_str() == uid.id
            } else {
                uid.entity_type == resource.kind.as_str() && uid.id == resource.id
            }
        }
        ResourceConstraint::IsType(ty) => resource.kind.as_str() == ty,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum EvalValue {
    Bool(bool),
    Long(i64),
    String(String),
    Entity(EntityUid),
    Set(Vec<EvalValue>),
}

fn eval_expr(
    expr: &Expr,
    principal: &Principal,
    resource: &ResourceAttributes,
    context: &AuthzContext,
) -> Option<EvalValue> {
    match expr {
        Expr::Literal(lit) => Some(match lit {
            Literal::Bool(b) => EvalValue::Bool(*b),
            Literal::Long(n) => EvalValue::Long(*n),
            Literal::String(s) => EvalValue::String(s.clone()),
            Literal::Entity(e) => EvalValue::Entity(e.clone()),
        }),
        Expr::Set(items) => {
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                out.push(eval_expr(item, principal, resource, context)?);
            }
            Some(EvalValue::Set(out))
        }
        Expr::Attr { root, attr } => eval_attr(*root, attr, principal, resource, context),
        Expr::Not(inner) => match eval_expr(inner, principal, resource, context)? {
            EvalValue::Bool(b) => Some(EvalValue::Bool(!b)),
            _ => None,
        },
        Expr::And(lhs, rhs) => {
            let EvalValue::Bool(a) = eval_expr(lhs, principal, resource, context)? else {
                return None;
            };
            if !a {
                return Some(EvalValue::Bool(false));
            }
            let EvalValue::Bool(b) = eval_expr(rhs, principal, resource, context)? else {
                return None;
            };
            Some(EvalValue::Bool(b))
        }
        Expr::Or(lhs, rhs) => {
            let EvalValue::Bool(a) = eval_expr(lhs, principal, resource, context)? else {
                return None;
            };
            if a {
                return Some(EvalValue::Bool(true));
            }
            let EvalValue::Bool(b) = eval_expr(rhs, principal, resource, context)? else {
                return None;
            };
            Some(EvalValue::Bool(b))
        }
        Expr::Cmp { op, lhs, rhs } => {
            let l = eval_expr(lhs, principal, resource, context)?;
            let r = eval_expr(rhs, principal, resource, context)?;
            eval_cmp(*op, &l, &r).map(EvalValue::Bool)
        }
        Expr::In { lhs, rhs } => {
            let l = eval_expr(lhs, principal, resource, context)?;
            let r = eval_expr(rhs, principal, resource, context)?;
            match (l, r) {
                (item, EvalValue::Set(set)) => Some(EvalValue::Bool(set.contains(&item))),
                _ => Some(EvalValue::Bool(false)),
            }
        }
        Expr::Contains { target, item } => {
            let t = eval_expr(target, principal, resource, context)?;
            let i = eval_expr(item, principal, resource, context)?;
            match t {
                EvalValue::Set(set) => Some(EvalValue::Bool(set.contains(&i))),
                _ => None,
            }
        }
        Expr::StartsWith { target, prefix } => {
            let EvalValue::String(s) = eval_expr(target, principal, resource, context)? else {
                return None;
            };
            let EvalValue::String(p) = eval_expr(prefix, principal, resource, context)? else {
                return None;
            };
            Some(EvalValue::Bool(s.starts_with(&p)))
        }
    }
}

fn eval_attr(
    root: AttrRoot,
    attr: &str,
    principal: &Principal,
    resource: &ResourceAttributes,
    context: &AuthzContext,
) -> Option<EvalValue> {
    match root {
        AttrRoot::Principal => match attr {
            "id" => Some(EvalValue::String(principal.id.clone())),
            "kind" => Some(EvalValue::String(principal.kind.as_str().to_owned())),
            "tenant" => Some(EvalValue::String(principal.tenant.as_str().to_owned())),
            "scopes" => Some(EvalValue::Set(
                principal
                    .scopes
                    .iter()
                    .map(|s| EvalValue::String(s.clone()))
                    .collect(),
            )),
            other => principal.attrs.get(other).map(attr_value_to_eval),
        },
        AttrRoot::Resource => match attr {
            "id" => Some(EvalValue::String(resource.id.clone())),
            "tenant" => Some(EvalValue::String(resource.tenant.as_str().to_owned())),
            "trust" => Some(EvalValue::String(resource.trust_str().to_owned())),
            "sideEffect" => Some(EvalValue::String(resource.side_effect_str().to_owned())),
            "group" => Some(EvalValue::String(resource.group.clone())),
            "branch" => Some(EvalValue::String(resource.branch.clone())),
            "classification" => Some(EvalValue::String(resource.classification.clone())),
            "status" => Some(EvalValue::String(resource.status_str().to_owned())),
            "requester" => Some(EvalValue::String(resource.requester.clone())),
            _ => None,
        },
        AttrRoot::Context => match attr {
            "tenant" => Some(EvalValue::String(context.tenant.as_str().to_owned())),
            "time" => Some(EvalValue::Long(context.time)),
            "hop" => Some(EvalValue::Long(context.hop)),
            "riskScore" => Some(EvalValue::Long(context.risk_score)),
            "approved" => Some(EvalValue::Bool(context.approved)),
            "approverIsDifferent" => Some(EvalValue::Bool(context.approver_is_different)),
            "scopes" => Some(EvalValue::Set(
                context
                    .scopes
                    .iter()
                    .map(|s| EvalValue::String(s.clone()))
                    .collect(),
            )),
            _ => None,
        },
    }
}

fn attr_value_to_eval(v: &AttrValue) -> EvalValue {
    match v {
        AttrValue::Bool(b) => EvalValue::Bool(*b),
        AttrValue::Int(n) => EvalValue::Long(*n),
        AttrValue::Text(s) => EvalValue::String(s.clone()),
        AttrValue::List(items) => {
            EvalValue::Set(items.iter().map(|s| EvalValue::String(s.clone())).collect())
        }
    }
}

fn eval_cmp(op: CmpOp, lhs: &EvalValue, rhs: &EvalValue) -> Option<bool> {
    match op {
        CmpOp::Eq => Some(lhs == rhs),
        CmpOp::Ne => Some(lhs != rhs),
        CmpOp::Lt | CmpOp::Le | CmpOp::Gt | CmpOp::Ge => {
            let (EvalValue::Long(a), EvalValue::Long(b)) = (lhs, rhs) else {
                return None;
            };
            Some(match op {
                CmpOp::Lt => a < b,
                CmpOp::Le => a <= b,
                CmpOp::Gt => a > b,
                CmpOp::Ge => a >= b,
                CmpOp::Eq | CmpOp::Ne => false,
            })
        }
    }
}

/// Validates Cedar error conversion helper when policy set fails reload.
#[must_use]
pub fn config_policy_error(msg: impl Into<String>) -> GapError {
    GapError::new(ErrorCode::ValidationError, msg)
}
