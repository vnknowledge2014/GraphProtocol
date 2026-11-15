//! OAuth 2.0 opaque-token introspection (`RFC 7662`) with bounded caching (`SEC-001`, `SEC-010`).

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Mutex;

use gap_core::context::{ActorRef, AttrValue, PrincipalKind};
use gap_core::error::{ErrorCode, GapError, GapResult};
use gap_core::ids::TenantId;

use super::crypto::sha256;
use super::json::JsonValue;
use super::jwt::MAX_ALLOWED_CLOCK_SKEW_SECS;
use super::principal::{
    CredentialClaims, DerivedPrincipalOutput, UntrustedRequestHints,
    derive_authenticated_principal, parse_principal_kind, parse_rfc8693_act_chain,
};

/// Parsed RFC 7662 token introspection payload (`SEC-001`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IntrospectionResponse {
    /// RFC 7662 active status (`false` -> unauthenticated).
    pub active: bool,
    /// Subject (`sub`).
    pub sub: Option<String>,
    /// Issuer (`iss`).
    pub iss: Option<String>,
    /// Expected audiences (`aud`).
    pub aud: Vec<String>,
    /// Optional RFC 8707 resource target (`resource`).
    pub resource: Option<String>,
    /// Expiration unix timestamp (seconds).
    pub exp: Option<i64>,
    /// Not-before unix timestamp (seconds).
    pub nbf: Option<i64>,
    /// Granted scope list.
    pub scopes: BTreeSet<String>,
    /// Authenticated tenant ID string.
    pub tenant: Option<String>,
    /// Optional principal classification (`HUMAN` / `AGENT` / `SERVICE`).
    pub kind: Option<PrincipalKind>,
    /// Optional delegation chain parsed from `act`.
    pub delegation: Vec<ActorRef>,
    /// Extra verified attributes.
    pub attrs: BTreeMap<String, AttrValue>,
}

impl IntrospectionResponse {
    /// Parses an RFC 7662 JSON response object (`application/json`).
    ///
    /// # Errors
    ///
    /// Returns [`ErrorCode::ValidationError`] if JSON syntax or nested `act` claim is malformed.
    pub fn from_json_str(raw: &str) -> GapResult<Self> {
        let doc = JsonValue::parse(raw)?;
        let active = doc.get_bool("active").unwrap_or(false);
        let sub = doc.get_str("sub").map(str::to_owned);
        let iss = doc.get_str("iss").map(str::to_owned);
        let aud = match doc.get("aud") {
            Some(JsonValue::String(s)) => vec![s.clone()],
            Some(JsonValue::Array(items)) => items
                .iter()
                .filter_map(JsonValue::as_str)
                .map(str::to_owned)
                .collect(),
            _ => Vec::new(),
        };
        let resource = doc.get_str("resource").map(str::to_owned);
        let exp = doc.get_i64("exp");
        let nbf = doc.get_i64("nbf");
        let mut scopes = BTreeSet::new();
        if let Some(s) = doc.get_str("scope") {
            for item in s.split_whitespace() {
                if !item.is_empty() {
                    scopes.insert(item.to_owned());
                }
            }
        }
        let tenant = doc
            .get_str("tenant")
            .or_else(|| doc.get_str("tenant_id"))
            .map(str::to_owned);
        let kind = match doc.get_str("kind") {
            None => None,
            Some(k) => Some(parse_principal_kind(Some(k))?),
        };
        let delegation = parse_rfc8693_act_chain(doc.get("act"))?;
        Ok(Self {
            active,
            sub,
            iss,
            aud,
            resource,
            exp,
            nbf,
            scopes,
            tenant,
            kind,
            delegation,
            attrs: super::principal::extract_safe_attrs(&doc),
        })
    }
}

/// Transport hook invoking an OAuth 2.0 RFC 7662 introspection endpoint.
pub trait IntrospectionTransport: Send + Sync {
    /// Queries the authorization server's introspection endpoint for `token`.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorCode::BranchUnavailable`] or [`ErrorCode::Unauthenticated`] if lookup fails.
    fn introspect(&self, token: &str) -> GapResult<IntrospectionResponse>;
}

/// Configuration for [`TokenIntrospector`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IntrospectionConfig {
    /// Positive cache TTL cap in seconds (`min(positive_ttl_secs, exp - now)`).
    pub positive_ttl_secs: i64,
    /// Negative cache TTL in seconds for `active: false` responses.
    pub negative_ttl_secs: i64,
    /// Expected audience/resource string.
    pub expected_audience: String,
    /// Optional explicit RFC 8707 expected resource URI.
    pub expected_resource: Option<String>,
    /// Allowed issuers (empty skips issuer pinning ONLY if explicitly left empty for opaque AS).
    pub trusted_issuers: BTreeSet<String>,
    /// Maximum cached entries (SHA-256 hashes of opaque tokens; raw tokens never cached).
    pub max_cache_entries: usize,
    /// Allowed clock skew in seconds (`<= 60`).
    pub max_clock_skew_secs: i64,
}

impl IntrospectionConfig {
    /// Creates an introspection config pinned to `expected_audience`.
    #[must_use]
    pub fn new(expected_audience: impl Into<String>) -> Self {
        Self {
            positive_ttl_secs: 60,
            negative_ttl_secs: 15,
            expected_audience: expected_audience.into(),
            expected_resource: None,
            trusted_issuers: BTreeSet::new(),
            max_cache_entries: 4096,
            max_clock_skew_secs: MAX_ALLOWED_CLOCK_SKEW_SECS,
        }
    }
}

#[derive(Clone)]
struct CachedIntrospection {
    expires_at: i64,
    outcome: Option<CredentialClaims>,
}

/// Caching RFC 7662 opaque-token authenticator (`SEC-001`, `SEC-010`, `SEC-011`).
pub struct TokenIntrospector<'a> {
    config: IntrospectionConfig,
    transport: &'a dyn IntrospectionTransport,
    cache: Mutex<HashMap<[u8; 32], CachedIntrospection>>,
}

impl<'a> TokenIntrospector<'a> {
    /// Creates an introspector backed by `transport`.
    #[must_use]
    pub fn new(config: IntrospectionConfig, transport: &'a dyn IntrospectionTransport) -> Self {
        Self {
            config,
            transport,
            cache: Mutex::new(HashMap::new()),
        }
    }

    /// Validates an opaque bearer token, caching SHA-256 digests (`INV-15` zero raw-token retention).
    ///
    /// # Errors
    ///
    /// Returns [`ErrorCode::Unauthenticated`] if inactive, expired, wrong audience, or missing tenant.
    pub fn validate(
        &self,
        token: &str,
        hints: Option<&UntrustedRequestHints>,
        now_unix_secs: i64,
    ) -> GapResult<DerivedPrincipalOutput> {
        if token.is_empty() {
            return Err(GapError::new(
                ErrorCode::Unauthenticated,
                "empty opaque bearer token",
            ));
        }
        let digest = sha256(token.as_bytes());
        {
            let mut guard = self.cache.lock().map_err(|_| {
                GapError::new(ErrorCode::Internal, "introspection cache lock poisoned")
            })?;
            if let Some(entry) = guard.get(&digest).cloned() {
                if entry.expires_at > now_unix_secs {
                    return match entry.outcome {
                        Some(claims) => Ok(derive_authenticated_principal(claims, hints)),
                        None => Err(GapError::new(
                            ErrorCode::Unauthenticated,
                            "cached inactive bearer token",
                        )),
                    };
                }
                guard.remove(&digest);
            }
        }

        let response = self.transport.introspect(token)?;
        let validated = self.verify_response(&response, now_unix_secs);
        let cache_entry = match &validated {
            Ok(claims) => {
                let token_exp = response.exp.unwrap_or(i64::MAX);
                let ttl_until = now_unix_secs.saturating_add(self.config.positive_ttl_secs);
                CachedIntrospection {
                    expires_at: ttl_until.min(token_exp),
                    outcome: Some(claims.clone()),
                }
            }
            Err(_) => CachedIntrospection {
                expires_at: now_unix_secs.saturating_add(self.config.negative_ttl_secs),
                outcome: None,
            },
        };

        {
            let mut guard = self.cache.lock().map_err(|_| {
                GapError::new(ErrorCode::Internal, "introspection cache lock poisoned")
            })?;
            if guard.len() >= self.config.max_cache_entries {
                guard.retain(|_, v| v.expires_at > now_unix_secs);
                if guard.len() >= self.config.max_cache_entries
                    && let Some(k) = guard.keys().next().copied()
                {
                    guard.remove(&k);
                }
            }
            guard.insert(digest, cache_entry);
        }

        validated.map(|claims| derive_authenticated_principal(claims, hints))
    }

    fn verify_response(
        &self,
        resp: &IntrospectionResponse,
        now_unix_secs: i64,
    ) -> GapResult<CredentialClaims> {
        if !resp.active {
            return Err(GapError::new(
                ErrorCode::Unauthenticated,
                "token is not active",
            ));
        }
        let skew = self
            .config
            .max_clock_skew_secs
            .clamp(0, MAX_ALLOWED_CLOCK_SKEW_SECS);
        if let Some(exp) = resp.exp
            && now_unix_secs.saturating_sub(skew) >= exp
        {
            return Err(GapError::new(
                ErrorCode::Unauthenticated,
                "introspected token expired",
            ));
        }
        if let Some(nbf) = resp.nbf
            && now_unix_secs.saturating_add(skew) < nbf
        {
            return Err(GapError::new(
                ErrorCode::Unauthenticated,
                "introspected token not yet valid",
            ));
        }
        if !self.config.trusted_issuers.is_empty() {
            let iss = resp.iss.as_deref().ok_or_else(|| {
                GapError::new(ErrorCode::Unauthenticated, "missing introspected iss")
            })?;
            if !self.config.trusted_issuers.contains(iss) {
                return Err(GapError::new(
                    ErrorCode::Unauthenticated,
                    "untrusted introspected iss",
                ));
            }
        }
        if !resp.aud.iter().any(|a| a == &self.config.expected_audience) {
            return Err(GapError::new(
                ErrorCode::Unauthenticated,
                "introspected audience mismatch",
            ));
        }
        if let Some(expected_res) = &self.config.expected_resource {
            let matches_res = resp.resource.as_deref() == Some(expected_res.as_str())
                || resp.aud.iter().any(|a| a == expected_res);
            if !matches_res {
                return Err(GapError::new(
                    ErrorCode::Unauthenticated,
                    "introspected resource mismatch",
                ));
            }
        }
        let subject = resp
            .sub
            .as_deref()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| GapError::new(ErrorCode::Unauthenticated, "missing sub"))?
            .to_owned();
        let tenant_raw = resp
            .tenant
            .as_deref()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| GapError::new(ErrorCode::Unauthenticated, "missing tenant"))?;
        let tenant = TenantId::new(tenant_raw)
            .map_err(|_| GapError::new(ErrorCode::Unauthenticated, "invalid tenant identifier"))?;

        Ok(CredentialClaims {
            subject,
            kind: resp.kind.unwrap_or(PrincipalKind::Agent),
            tenant,
            scopes: resp.scopes.clone(),
            attrs: resp.attrs.clone(),
            delegation: resp.delegation.clone(),
        })
    }
}
