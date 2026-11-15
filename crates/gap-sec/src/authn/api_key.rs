//! Argon2id API-key authentication (`SEC-001`) and fail-closed anonymous guard (`SEC-002`).

use std::collections::{BTreeMap, BTreeSet};
use std::net::IpAddr;

use gap_core::context::{AttrValue, PrincipalKind};
use gap_core::error::{ErrorCode, GapError, GapResult};
use gap_core::ids::TenantId;
use gap_core::secret::Secret;

use super::crypto::{Argon2Params, argon2id_phc_encode, argon2id_phc_verify};
use super::principal::{
    CredentialClaims, DerivedPrincipalOutput, UntrustedRequestHints, derive_authenticated_principal,
};

/// Stored API-key credential record (`SEC-001`, `SEC-010`).
///
/// Stores only a PHC `$argon2id$...` password hash — never the raw API secret (`INV-15`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiKeyRecord {
    /// Public key identifier prefix (portion before `.`).
    pub key_id: String,
    /// PHC `$argon2id$v=19$...` encoded secret hash.
    pub phc_hash: String,
    /// Authenticated principal subject ID bound to this key.
    pub subject: String,
    /// Caller classification (`Service`, `Agent`, or `Human`).
    pub kind: PrincipalKind,
    /// Authoritative tenant ID bound at issuance (`SEC-010`).
    pub tenant: TenantId,
    /// Granted scope set.
    pub scopes: BTreeSet<String>,
    /// Verified non-secret attributes.
    pub attrs: BTreeMap<String, AttrValue>,
    /// Optional expiration unix timestamp (seconds).
    pub expires_at: Option<i64>,
    /// Explicit revocation switch.
    pub revoked: bool,
}

/// Parameters for issuing a new Argon2id [`ApiKeyRecord`] (`SEC-001`).
#[derive(Debug, Clone)]
pub struct ApiKeyIssueSpec<'a> {
    /// Public key identifier prefix (must not contain `.`).
    pub key_id: &'a str,
    /// Raw API secret bytes wrapped in [`Secret`].
    pub raw_secret: &'a Secret<String>,
    /// Unique salt bytes for Argon2id hashing.
    pub salt: &'a [u8],
    /// RFC 9106 Argon2id work parameters.
    pub params: Argon2Params,
    /// Principal subject identifier bound to the key.
    pub subject: &'a str,
    /// Principal kind (`Service`, `Agent`, or `Human`).
    pub kind: PrincipalKind,
    /// Authoritative tenant bound at issuance (`SEC-010`).
    pub tenant: TenantId,
    /// Granted scope set.
    pub scopes: BTreeSet<String>,
}

impl ApiKeyRecord {
    /// Creates an [`ApiKeyRecord`] by hashing `raw_secret` using RFC 9106 Argon2id (`SEC-001`).
    ///
    /// # Errors
    ///
    /// Returns [`ErrorCode::ValidationError`] if `key_id` contains `.` or parameters are invalid.
    pub fn issue(spec: ApiKeyIssueSpec<'_>) -> GapResult<Self> {
        let key_id = spec.key_id.to_owned();
        if key_id.is_empty() || key_id.contains('.') {
            return Err(GapError::new(
                ErrorCode::ValidationError,
                "API key_id must be non-empty and must not contain '.'",
            ));
        }
        let secret_bytes = spec.raw_secret.expose_secret().as_bytes();
        if secret_bytes.is_empty() {
            return Err(GapError::new(
                ErrorCode::ValidationError,
                "API key secret must not be empty",
            ));
        }
        let phc_hash = argon2id_phc_encode(secret_bytes, spec.salt, spec.params)?;
        Ok(Self {
            key_id,
            phc_hash,
            subject: spec.subject.to_owned(),
            kind: spec.kind,
            tenant: spec.tenant,
            scopes: spec.scopes,
            attrs: BTreeMap::new(),
            expires_at: None,
            revoked: false,
        })
    }
}

/// Verifies `<key_id>.<secret>` API keys against Argon2id records (`SEC-001`, `SEC-003`).
#[derive(Debug, Default)]
pub struct ApiKeyAuthenticator {
    records: BTreeMap<String, ApiKeyRecord>,
    dummy_phc: Option<String>,
}

impl ApiKeyAuthenticator {
    /// Creates an empty API-key authenticator initialized with a uniform-cost dummy Argon2id target.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorCode::ValidationError`] only if internal dummy hash setup fails.
    pub fn new(records: impl IntoIterator<Item = ApiKeyRecord>) -> GapResult<Self> {
        let mut map = BTreeMap::new();
        for rec in records {
            map.insert(rec.key_id.clone(), rec);
        }
        let dummy = argon2id_phc_encode(
            b"gap-dummy-key-mitigation",
            b"gap-dummy-salt-01",
            Argon2Params::testing(),
        )?;
        Ok(Self {
            records: map,
            dummy_phc: Some(dummy),
        })
    }

    /// Inserts or replaces an API key record.
    pub fn insert(&mut self, record: ApiKeyRecord) {
        self.records.insert(record.key_id.clone(), record);
    }

    /// Authenticates `presented_key` (`<key_id>.<secret>`) at `now_unix_secs` (`SEC-001`, `SEC-010`).
    ///
    /// Executes Argon2id verification even on unknown `key_id` to preserve constant-shape behavior
    /// against key-existence timing probes (`SEC-003`).
    ///
    /// # Errors
    ///
    /// Returns [`ErrorCode::Unauthenticated`] if malformed, unknown, wrong secret, revoked, or expired.
    pub fn authenticate(
        &self,
        presented_key: &str,
        hints: Option<&UntrustedRequestHints>,
        now_unix_secs: i64,
    ) -> GapResult<DerivedPrincipalOutput> {
        let Some((key_id, secret)) = presented_key.split_once('.') else {
            return Err(GapError::new(
                ErrorCode::Unauthenticated,
                "malformed API key token",
            ));
        };
        if key_id.is_empty() || secret.is_empty() {
            return Err(GapError::new(
                ErrorCode::Unauthenticated,
                "incomplete API key token",
            ));
        }

        let Some(rec) = self.records.get(key_id) else {
            if let Some(dummy) = &self.dummy_phc {
                let _ = argon2id_phc_verify(secret.as_bytes(), dummy);
            }
            return Err(GapError::new(
                ErrorCode::Unauthenticated,
                "invalid API key credentials",
            ));
        };

        let matched = argon2id_phc_verify(secret.as_bytes(), &rec.phc_hash).unwrap_or(false);
        if !matched {
            return Err(GapError::new(
                ErrorCode::Unauthenticated,
                "invalid API key credentials",
            ));
        }
        if rec.revoked {
            return Err(GapError::new(ErrorCode::Unauthenticated, "API key revoked"));
        }
        if let Some(exp) = rec.expires_at
            && now_unix_secs >= exp
        {
            return Err(GapError::new(ErrorCode::Unauthenticated, "API key expired"));
        }

        let claims = CredentialClaims {
            subject: rec.subject.clone(),
            kind: rec.kind,
            tenant: rec.tenant.clone(),
            scopes: rec.scopes.clone(),
            attrs: rec.attrs.clone(),
            delegation: Vec::new(),
        };
        Ok(derive_authenticated_principal(claims, hints))
    }
}

/// Fail-closed policy governing unauthenticated/anonymous development requests (`SEC-002`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AnonymousModePolicy {
    /// Whether anonymous access is requested (default `false`; fail closed).
    pub allow_anonymous: bool,
    /// Explicit development mode flag (`allow_anonymous` is rejected outside `dev_mode`).
    pub dev_mode: bool,
    /// Whether peer IP must be loopback (`127.0.0.1` / `::1`) whenever `allow_anonymous` is used.
    pub require_loopback: bool,
    /// Synthetic tenant assigned strictly in local loopback dev mode.
    pub dev_tenant: Option<TenantId>,
}

impl AnonymousModePolicy {
    /// Fail-closed production default (`allow_anonymous = false`).
    #[must_use]
    pub const fn fail_closed() -> Self {
        Self {
            allow_anonymous: false,
            dev_mode: false,
            require_loopback: true,
            dev_tenant: None,
        }
    }

    /// Validates startup configuration so production builds cannot accidentally expose anonymous access (`SEC-002`).
    ///
    /// # Errors
    ///
    /// Returns [`ErrorCode::ValidationError`] if `allow_anonymous` is true without `dev_mode`.
    pub fn validate_config(&self) -> GapResult<()> {
        if self.allow_anonymous && !self.dev_mode {
            return Err(GapError::new(
                ErrorCode::ValidationError,
                "SEC-002 violation: allow_anonymous requires explicit dev_mode=true",
            ));
        }
        if self.allow_anonymous && !self.require_loopback {
            return Err(GapError::new(
                ErrorCode::ValidationError,
                "SEC-002 violation: allow_anonymous requires require_loopback=true",
            ));
        }
        Ok(())
    }

    /// Authorizes an anonymous request **only** when `dev_mode`, `allow_anonymous`, and a loopback peer IP hold.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorCode::Unauthenticated`] in all non-dev or non-loopback paths (`SEC-002`).
    pub fn try_authenticate_anonymous(
        &self,
        peer_ip: Option<IpAddr>,
        hints: Option<&UntrustedRequestHints>,
    ) -> GapResult<DerivedPrincipalOutput> {
        if !self.allow_anonymous || !self.dev_mode || !self.require_loopback {
            return Err(GapError::new(
                ErrorCode::Unauthenticated,
                "anonymous requests are disabled",
            ));
        }
        let is_loopback = peer_ip.is_some_and(|ip| ip.is_loopback());
        if !is_loopback {
            return Err(GapError::new(
                ErrorCode::Unauthenticated,
                "anonymous development access permitted from loopback only",
            ));
        }

        let tenant = self
            .dev_tenant
            .clone()
            .unwrap_or_else(TenantId::default_tenant);
        let claims = CredentialClaims {
            subject: "dev:anonymous".to_owned(),
            kind: PrincipalKind::Human,
            tenant,
            scopes: BTreeSet::new(),
            attrs: BTreeMap::new(),
            delegation: Vec::new(),
        };
        Ok(derive_authenticated_principal(claims, hints))
    }
}
