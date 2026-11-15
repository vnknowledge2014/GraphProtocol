//! RFC 8693 OAuth 2.0 Token Exchange client (`SEC-011`, `NGC-081`, `INV-09`, `INV-15`).
//!
//! Ensures caller bearer tokens are **never** stored on `Principal` or forwarded directly to
//! downstream connectors. Downstream calls obtain a target-scoped token via RFC 8693 exchange
//! held solely inside `Secret<String>` and cached under `(TenantId, principal.id, ...)`.

use std::collections::{BTreeSet, HashMap};
use std::sync::Mutex;

use gap_core::context::Principal;
use gap_core::error::{ErrorCode, GapError, GapResult};
use gap_core::ids::TenantId;
use gap_core::secret::Secret;

use super::crypto::{blake3_hash, hex_encode};

/// RFC 8693 token-exchange grant type URI.
pub const RFC8693_GRANT_TYPE: &str = "urn:ietf:params:oauth:grant-type:token-exchange";

/// RFC 8693 access-token type URI.
pub const RFC8693_ACCESS_TOKEN_TYPE: &str = "urn:ietf:params:oauth:token-type:access_token";

/// Structured RFC 8693 token exchange request submitted to [`TokenExchangeTransport`].
#[derive(Clone)]
pub struct TokenExchangeRequest {
    /// Constant [`RFC8693_GRANT_TYPE`].
    pub grant_type: String,
    /// Subject assertion / credential held strictly inside [`Secret`] (never raw caller bearer JWT).
    pub subject_token: Secret<String>,
    /// RFC 8693 `subject_token_type`.
    pub subject_token_type: String,
    /// Target downstream resource (`RFC 8707`).
    pub resource: String,
    /// Optional target audience (`audience`).
    pub audience: Option<String>,
    /// Requested down-scoped OAuth scope string.
    pub scope: Option<String>,
    /// Optional RFC 8693 `requested_token_type`.
    pub requested_token_type: String,
    /// Optional serialized actor / delegation descriptor (never raw secrets).
    pub actor_subject: Option<String>,
}

impl std::fmt::Debug for TokenExchangeRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TokenExchangeRequest")
            .field("grant_type", &self.grant_type)
            .field("subject_token", &self.subject_token)
            .field("resource", &self.resource)
            .field("audience", &self.audience)
            .field("scope", &self.scope)
            .field("requested_token_type", &self.requested_token_type)
            .finish()
    }
}

/// Down-scoped token returned by RFC 8693 exchange (`SEC-011`, `INV-15`).
#[derive(Clone)]
pub struct ExchangedToken {
    /// Opaque or JWT downstream token redacted by [`Secret`] on Debug/Display/Serialize.
    pub access_token: Secret<String>,
    /// RFC 8693 `issued_token_type`.
    pub issued_token_type: String,
    /// Token type (`Bearer`).
    pub token_type: String,
    /// Effective granted scopes for the downstream target.
    pub scopes: BTreeSet<String>,
    /// Unix timestamp (seconds) when this exchanged credential expires.
    pub expires_at_unix_secs: i64,
}

impl std::fmt::Debug for ExchangedToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExchangedToken")
            .field("access_token", &self.access_token)
            .field("issued_token_type", &self.issued_token_type)
            .field("token_type", &self.token_type)
            .field("scopes", &self.scopes)
            .field("expires_at_unix_secs", &self.expires_at_unix_secs)
            .finish()
    }
}

/// Transport hook performing the HTTP POST `/oauth/token` RFC 8693 exchange call.
pub trait TokenExchangeTransport: Send + Sync {
    /// Executes an RFC 8693 token exchange against the trusted authorization server.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorCode::AuthRequired`] or [`ErrorCode::Unauthenticated`] if exchange fails.
    fn exchange(&self, req: &TokenExchangeRequest) -> GapResult<ExchangedToken>;
}

/// Tenant-scoped cache key (`INV-09`: `TenantId` MUST participate in every cache key).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ExchangeCacheKey {
    tenant: TenantId,
    subject_id: String,
    delegation_digest: String,
    resource: String,
    scope_key: String,
}

/// Outbound RFC 8693 token exchange client with tenant-isolated credential cache (`SEC-011`, `NGC-081`).
pub struct TokenExchangeClient<'a> {
    transport: &'a dyn TokenExchangeTransport,
    expiry_margin_secs: i64,
    max_cache_entries: usize,
    cache: Mutex<HashMap<ExchangeCacheKey, ExchangedToken>>,
}

impl<'a> TokenExchangeClient<'a> {
    /// Creates an RFC 8693 token exchange client with `expiry_margin_secs` early-refresh window.
    #[must_use]
    pub fn new(transport: &'a dyn TokenExchangeTransport) -> Self {
        Self {
            transport,
            expiry_margin_secs: 30,
            max_cache_entries: 4096,
            cache: Mutex::new(HashMap::new()),
        }
    }

    /// Exchanges `gateway_subject_assertion` on behalf of authenticated `principal` for a scoped
    /// credential bound to `target_resource` and `requested_scopes` (`SEC-011`, `NGC-081`, `INV-09`).
    ///
    /// # Errors
    ///
    /// Returns [`ErrorCode::ValidationError`] if `target_resource` is empty or
    /// [`ErrorCode::AuthRequired`] / [`ErrorCode::Unauthenticated`] on exchange failure.
    pub fn exchange_for_target(
        &self,
        principal: &Principal,
        gateway_subject_assertion: &Secret<String>,
        target_resource: &str,
        requested_scopes: &BTreeSet<String>,
        now_unix_secs: i64,
    ) -> GapResult<ExchangedToken> {
        if target_resource.trim().is_empty() {
            return Err(GapError::new(
                ErrorCode::ValidationError,
                "RFC 8693 target_resource must not be empty",
            ));
        }

        // Enforce tenant & delegation inclusion in key (INV-09).
        let mut del_buf = String::new();
        for actor in &principal.delegation {
            del_buf.push_str(&actor.id);
            del_buf.push(':');
            del_buf.push_str(actor.kind.as_str());
            del_buf.push(';');
        }
        let delegation_digest = hex_encode(&blake3_hash(del_buf.as_bytes()));
        let scope_vec: Vec<&str> = requested_scopes.iter().map(String::as_str).collect();
        let scope_key = scope_vec.join(" ");

        let key = ExchangeCacheKey {
            tenant: principal.tenant.clone(),
            subject_id: principal.id.clone(),
            delegation_digest,
            resource: target_resource.to_owned(),
            scope_key: scope_key.clone(),
        };

        {
            let mut guard = self.cache.lock().map_err(|_| {
                GapError::new(ErrorCode::Internal, "token exchange cache lock poisoned")
            })?;
            if let Some(cached) = guard.get(&key)
                && cached
                    .expires_at_unix_secs
                    .saturating_sub(self.expiry_margin_secs)
                    > now_unix_secs
            {
                return Ok(cached.clone());
            }
            guard.remove(&key);
        }

        let actor_summary = if principal.delegation.is_empty() {
            None
        } else {
            Some(
                principal
                    .delegation
                    .iter()
                    .map(|a| format!("{}:{}", a.kind.as_str(), a.id))
                    .collect::<Vec<_>>()
                    .join("->"),
            )
        };

        let request = TokenExchangeRequest {
            grant_type: RFC8693_GRANT_TYPE.to_owned(),
            subject_token: gateway_subject_assertion.clone(),
            subject_token_type: RFC8693_ACCESS_TOKEN_TYPE.to_owned(),
            resource: target_resource.to_owned(),
            audience: Some(target_resource.to_owned()),
            scope: if scope_key.is_empty() {
                None
            } else {
                Some(scope_key)
            },
            requested_token_type: RFC8693_ACCESS_TOKEN_TYPE.to_owned(),
            actor_subject: actor_summary,
        };

        let issued = self.transport.exchange(&request)?;
        {
            let mut guard = self.cache.lock().map_err(|_| {
                GapError::new(ErrorCode::Internal, "token exchange cache lock poisoned")
            })?;
            if guard.len() >= self.max_cache_entries {
                guard.retain(|_, v| {
                    v.expires_at_unix_secs
                        .saturating_sub(self.expiry_margin_secs)
                        > now_unix_secs
                });
                if guard.len() >= self.max_cache_entries
                    && let Some(evict_key) = guard.keys().next().cloned()
                {
                    guard.remove(&evict_key);
                }
            }
            guard.insert(key, issued.clone());
        }

        Ok(issued)
    }
}
