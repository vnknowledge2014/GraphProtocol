//! Principal derivation, tenant binding (`SEC-010`), and RFC 8693 `act` delegation (`SEC-012`).

use std::collections::{BTreeMap, BTreeSet};

use gap_core::context::{ActorRef, AttrValue, Principal, PrincipalKind};
use gap_core::error::{ErrorCode, GapError, GapResult};
use gap_core::ids::TenantId;

use super::json::JsonValue;

/// Maximum depth of nested RFC 8693 `act` objects parsed into `Principal.delegation`.
pub const MAX_DELEGATION_DEPTH: usize = 16;

/// Untrusted request hints that MUST NEVER determine tenant identity (`SEC-010`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UntrustedRequestHints {
    /// Unauthenticated `X-Tenant` HTTP header value, if supplied.
    pub x_tenant_header: Option<String>,
    /// Unauthenticated tenant field in JSON/GraphQL body, if supplied.
    pub body_tenant: Option<String>,
}

/// Verified identity claims produced by any GAP authenticator (`JWT`, `Introspect`, `ApiKey`, `mTLS`, `stdio`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CredentialClaims {
    /// Subject identifier (`sub`, SPIFFE URI, OS user, or service ID).
    pub subject: String,
    /// Caller classification (`HUMAN`, `AGENT`, `SERVICE`).
    pub kind: PrincipalKind,
    /// Authoritative authenticated tenant ID (`SEC-010`).
    pub tenant: TenantId,
    /// Granted scope set extracted from the verified credential.
    pub scopes: BTreeSet<String>,
    /// Verified non-secret attributes exposed to Cedar (`SEC-020`).
    pub attrs: BTreeMap<String, AttrValue>,
    /// Outermost-first actor delegation chain extracted from `act` claims (`SEC-012`).
    pub delegation: Vec<ActorRef>,
}

/// Result of deriving a [`Principal`] from verified credentials alongside untrusted hints (`SEC-010`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DerivedPrincipalOutput {
    /// Authenticated principal guaranteed to carry zero raw bearer tokens (`SEC-011`).
    pub principal: Principal,
    /// Recorded mismatch when an ignored untrusted header/body tenant differed from credential tenant.
    pub ignored_hint_mismatch: Option<String>,
}

/// Constructs an authenticated [`Principal`] solely from verified [`CredentialClaims`].
///
/// Any conflicting `X-Tenant` header or request body tenant field in `hints` is **strictly ignored**
/// (`SEC-010`) and recorded only as diagnostic metadata so downstream callers cannot spoof tenant isolation.
#[must_use]
pub fn derive_authenticated_principal(
    claims: CredentialClaims,
    hints: Option<&UntrustedRequestHints>,
) -> DerivedPrincipalOutput {
    let mut mismatch = None;
    if let Some(h) = hints {
        let cred_tenant = claims.tenant.as_str();
        let header_conflict = h
            .x_tenant_header
            .as_deref()
            .filter(|v| !v.is_empty() && *v != cred_tenant);
        let body_conflict = h
            .body_tenant
            .as_deref()
            .filter(|v| !v.is_empty() && *v != cred_tenant);
        if header_conflict.is_some() || body_conflict.is_some() {
            mismatch = Some(format!(
                "ignored untrusted tenant hint (credential tenant `{cred_tenant}` kept per SEC-010)"
            ));
        }
    }

    let principal = Principal {
        id: claims.subject,
        kind: claims.kind,
        tenant: claims.tenant,
        scopes: claims.scopes,
        attrs: strip_secret_claim_keys(claims.attrs),
        delegation: claims.delegation,
    };

    DerivedPrincipalOutput {
        principal,
        ignored_hint_mismatch: mismatch,
    }
}

/// Parses a JSON object's nested RFC 8693 `act` claims into an outermost-first [`Vec<ActorRef>`].
///
/// Expected claim structure:
/// `{"sub": "agent-1", "kind": "AGENT", "act": {"sub": "svc-proxy", "kind": "SERVICE"}}`.
///
/// # Errors
///
/// Returns [`ErrorCode::Unauthenticated`] if any `act` object omits a non-empty `sub` or exceeds
/// [`MAX_DELEGATION_DEPTH`].
pub fn parse_rfc8693_act_chain(act_claim: Option<&JsonValue>) -> GapResult<Vec<ActorRef>> {
    let mut chain = Vec::new();
    let mut cursor = act_claim;
    let mut depth = 0_usize;

    while let Some(node) = cursor {
        depth += 1;
        if depth > MAX_DELEGATION_DEPTH {
            return Err(GapError::new(
                ErrorCode::Unauthenticated,
                "delegation act chain exceeds maximum depth",
            ));
        }
        let Some(obj) = node.as_object() else {
            return Err(GapError::new(
                ErrorCode::Unauthenticated,
                "malformed RFC 8693 act claim (expected object)",
            ));
        };
        let sub = obj
            .get("sub")
            .and_then(JsonValue::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                GapError::new(
                    ErrorCode::Unauthenticated,
                    "RFC 8693 act claim missing non-empty sub",
                )
            })?;
        let kind = parse_principal_kind(obj.get("kind").and_then(JsonValue::as_str))?;
        chain.push(ActorRef {
            id: sub.to_owned(),
            kind,
        });
        cursor = obj.get("act");
    }

    Ok(chain)
}

/// Computes the effective delegated scope intersection across all actor scope sets (`SEC-012`).
///
/// An empty input slice yields an empty scope set (fail-closed default deny).
#[must_use]
pub fn intersect_delegated_scopes(layers: &[BTreeSet<String>]) -> BTreeSet<String> {
    let Some(first) = layers.first() else {
        return BTreeSet::new();
    };
    let mut acc = first.clone();
    for layer in &layers[1..] {
        acc.retain(|scope| layer.contains(scope));
    }
    acc
}

/// Parses a `kind` string claim into [`PrincipalKind`], defaulting to `Agent` when omitted.
///
/// # Errors
///
/// Returns [`ErrorCode::Unauthenticated`] if an explicit `kind` string is unrecognized.
pub fn parse_principal_kind(raw: Option<&str>) -> GapResult<PrincipalKind> {
    match raw {
        None => Ok(PrincipalKind::Agent),
        Some(v) => match v.to_ascii_uppercase().as_str() {
            "HUMAN" => Ok(PrincipalKind::Human),
            "AGENT" => Ok(PrincipalKind::Agent),
            "SERVICE" => Ok(PrincipalKind::Service),
            _ => Err(GapError::new(
                ErrorCode::Unauthenticated,
                "unrecognized principal kind claim",
            )),
        },
    }
}

/// Parses standard OAuth `scope` (space-delimited string) or `scp` (string array) claims.
#[must_use]
pub fn extract_scopes_from_claims(claims: &JsonValue) -> BTreeSet<String> {
    let mut scopes = BTreeSet::new();
    if let Some(scope_str) = claims.get_str("scope") {
        for item in scope_str.split_whitespace() {
            if !item.is_empty() {
                scopes.insert(item.to_owned());
            }
        }
    }
    if let Some(scp_arr) = claims.get("scp").and_then(JsonValue::as_array) {
        for item in scp_arr {
            if let Some(s) = item.as_str()
                && !s.is_empty()
            {
                scopes.insert(s.to_owned());
            }
        }
    }
    scopes
}

/// Extracts non-secret attribute claims from a verified claims object for policy evaluation.
#[must_use]
pub fn extract_safe_attrs(claims: &JsonValue) -> BTreeMap<String, AttrValue> {
    let mut out = BTreeMap::new();
    let Some(map) = claims.as_object() else {
        return out;
    };
    for (k, v) in map {
        if is_reserved_or_secret_claim(k) {
            continue;
        }
        if let Some(attr) = json_to_attr_value(v) {
            out.insert(k.clone(), attr);
        }
    }
    strip_secret_claim_keys(out)
}

fn json_to_attr_value(v: &JsonValue) -> Option<AttrValue> {
    match v {
        JsonValue::Bool(b) => Some(AttrValue::Bool(*b)),
        JsonValue::Int(i) => Some(AttrValue::Int(*i)),
        JsonValue::String(s) => Some(AttrValue::Text(s.clone())),
        JsonValue::Array(items) => {
            let mut strs = Vec::with_capacity(items.len());
            for item in items {
                strs.push(item.as_str()?.to_owned());
            }
            Some(AttrValue::List(strs))
        }
        JsonValue::Null | JsonValue::Float(_) | JsonValue::Object(_) => None,
    }
}

fn is_reserved_or_secret_claim(key: &str) -> bool {
    let lower = key.to_ascii_lowercase();
    matches!(
        lower.as_str(),
        "iss"
            | "sub"
            | "aud"
            | "exp"
            | "nbf"
            | "iat"
            | "jti"
            | "act"
            | "scope"
            | "scp"
            | "access_token"
            | "refresh_token"
            | "id_token"
            | "token"
            | "client_secret"
            | "authorization"
            | "secret"
            | "password"
            | "api_key"
    )
}

fn strip_secret_claim_keys(mut attrs: BTreeMap<String, AttrValue>) -> BTreeMap<String, AttrValue> {
    attrs.retain(|k, _| !is_reserved_or_secret_claim(k));
    attrs
}
