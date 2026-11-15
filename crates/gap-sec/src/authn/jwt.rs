//! JWT (`EdDSA` / `RS256`) validation and JWKS cache with key rotation (`SEC-001`, `SEC-004`, `SEC-010`, `SEC-012`).

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Mutex;

use gap_core::error::{ErrorCode, GapError, GapResult};
use gap_core::ids::TenantId;

use super::crypto::{Ed25519VerifyingKey, base64url_decode, rsa_rs256_verify};
use super::json::JsonValue;
use super::principal::{
    CredentialClaims, DerivedPrincipalOutput, UntrustedRequestHints,
    derive_authenticated_principal, extract_safe_attrs, extract_scopes_from_claims,
    parse_principal_kind, parse_rfc8693_act_chain,
};

/// Maximum permitted clock skew in seconds (`SEC-001` enforces `<= 60 s`).
pub const MAX_ALLOWED_CLOCK_SKEW_SECS: i64 = 60;

/// Explicitly allowed asymmetric JWS signing algorithms (`SEC-001`).
///
/// Symmetric (`HS*`) and unsigned (`none`) algorithms are intentionally absent so
/// token-confusion attacks against asymmetric JWKS keys are impossible by type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum AllowedJwtAlg {
    /// Ed25519 (`EdDSA`, `RFC 8037`).
    EdDsa,
    /// RSASSA-PKCS1-v1_5 with SHA-256 (`RS256`).
    Rs256,
}

impl AllowedJwtAlg {
    /// Parses a JWS `alg` header value, rejecting `none`, `HS256`, and all unallowed algorithms.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorCode::Unauthenticated`] if `raw` is not an allowed algorithm.
    pub fn parse(raw: &str) -> GapResult<Self> {
        match raw {
            "EdDSA" => Ok(Self::EdDsa),
            "RS256" => Ok(Self::Rs256),
            _ => Err(GapError::new(
                ErrorCode::Unauthenticated,
                "disallowed or unrecognized JWS algorithm",
            )),
        }
    }

    /// Canonical JWS header `alg` string.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::EdDsa => "EdDSA",
            Self::Rs256 => "RS256",
        }
    }
}

/// Cryptographic public key material inside a JWK (`RFC 7517`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JwkKeyMaterial {
    /// OKP `Ed25519` public key (`32` bytes).
    Ed25519([u8; 32]),
    /// RSA public key `(n, e)` in big-endian unsigned bytes.
    Rsa {
        /// Modulus `n` (`>= 2048` bits enforced at verification).
        n: Vec<u8>,
        /// Public exponent `e`.
        e: Vec<u8>,
    },
}

/// JSON Web Key entry (`RFC 7517`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Jwk {
    /// Key ID (`kid`).
    pub kid: String,
    /// Optional explicit key `alg` restriction (`EdDSA` / `RS256`).
    pub alg: Option<AllowedJwtAlg>,
    /// Public key material.
    pub material: JwkKeyMaterial,
}

impl Jwk {
    /// Creates an `Ed25519` verification JWK.
    #[must_use]
    pub fn ed25519(kid: impl Into<String>, public_key: [u8; 32]) -> Self {
        Self {
            kid: kid.into(),
            alg: Some(AllowedJwtAlg::EdDsa),
            material: JwkKeyMaterial::Ed25519(public_key),
        }
    }

    /// Creates an `RS256` verification JWK from big-endian `(n, e)` bytes.
    #[must_use]
    pub fn rsa_rs256(kid: impl Into<String>, n: Vec<u8>, e: Vec<u8>) -> Self {
        Self {
            kid: kid.into(),
            alg: Some(AllowedJwtAlg::Rs256),
            material: JwkKeyMaterial::Rsa { n, e },
        }
    }

    /// Parses a JWKS document `{"keys":[...]}` into validated [`Jwk`] entries.
    ///
    /// Unsupported keys (e.g. symmetric `oct` keys or encryption keys `use: "enc"`) are skipped.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorCode::ValidationError`] if the document is invalid JSON or lacks a `keys` array.
    pub fn parse_jwks_json(raw: &str) -> GapResult<Vec<Self>> {
        let doc = JsonValue::parse(raw)?;
        let keys = doc
            .get("keys")
            .and_then(JsonValue::as_array)
            .ok_or_else(|| GapError::new(ErrorCode::ValidationError, "JWKS missing keys array"))?;
        let mut parsed = Vec::new();
        for item in keys {
            if item.get_str("use") == Some("enc") {
                continue;
            }
            let Some(kid) = item.get_str("kid").filter(|s| !s.is_empty()) else {
                continue;
            };
            let explicit_alg = match item.get_str("alg") {
                None => None,
                Some(a) => match AllowedJwtAlg::parse(a) {
                    Ok(alg) => Some(alg),
                    Err(_) => continue,
                },
            };
            match item.get_str("kty") {
                Some("OKP") if item.get_str("crv") == Some("Ed25519") => {
                    let Some(x_b64) = item.get_str("x") else {
                        continue;
                    };
                    let Ok(raw_x) = base64url_decode(x_b64) else {
                        continue;
                    };
                    if let Ok(arr) = <[u8; 32]>::try_from(raw_x.as_slice())
                        && Ed25519VerifyingKey::from_bytes(&arr).is_ok()
                    {
                        parsed.push(Self {
                            kid: kid.to_owned(),
                            alg: explicit_alg.or(Some(AllowedJwtAlg::EdDsa)),
                            material: JwkKeyMaterial::Ed25519(arr),
                        });
                    }
                }
                Some("RSA") => {
                    let (Some(n_b64), Some(e_b64)) = (item.get_str("n"), item.get_str("e")) else {
                        continue;
                    };
                    let (Ok(n), Ok(e)) = (base64url_decode(n_b64), base64url_decode(e_b64)) else {
                        continue;
                    };
                    parsed.push(Self {
                        kid: kid.to_owned(),
                        alg: explicit_alg.or(Some(AllowedJwtAlg::Rs256)),
                        material: JwkKeyMaterial::Rsa { n, e },
                    });
                }
                _ => {}
            }
        }
        Ok(parsed)
    }

    fn verify_signature(
        &self,
        alg: AllowedJwtAlg,
        signing_input: &[u8],
        signature: &[u8],
    ) -> GapResult<()> {
        if let Some(bound_alg) = self.alg
            && bound_alg != alg
        {
            return Err(GapError::new(
                ErrorCode::Unauthenticated,
                "JWT alg mismatches key metadata",
            ));
        }
        match (alg, &self.material) {
            (AllowedJwtAlg::EdDsa, JwkKeyMaterial::Ed25519(pk)) => {
                let vk = Ed25519VerifyingKey::from_bytes(pk)?;
                vk.verify(signing_input, signature)
            }
            (AllowedJwtAlg::Rs256, JwkKeyMaterial::Rsa { n, e }) => {
                rsa_rs256_verify(n, e, signing_input, signature)
            }
            _ => Err(GapError::new(
                ErrorCode::Unauthenticated,
                "token algorithm incompatible with JWK key type",
            )),
        }
    }
}

/// Abstract JWKS refresh source invoked synchronously or via cached mock in tests.
pub trait JwksFetcher: Send + Sync {
    /// Fetches fresh JWKS keys on key miss/rotation.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorCode::Unauthenticated`] or [`ErrorCode::BranchUnavailable`] on fetch failure.
    fn fetch_keys(&self) -> GapResult<Vec<Jwk>>;
}

/// Configuration for [`JwksCache`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JwksCacheConfig {
    /// How long previously active keys remain valid during graceful key rotation.
    pub rotation_grace_secs: i64,
    /// Minimum cooldown between network refreshes triggered by unknown `kid`s (anti-DoS).
    pub min_refresh_interval_secs: i64,
    /// Maximum cached key TTL before background refresh is requested.
    pub cache_ttl_secs: i64,
}

impl Default for JwksCacheConfig {
    fn default() -> Self {
        Self {
            rotation_grace_secs: 300,
            min_refresh_interval_secs: 15,
            cache_ttl_secs: 3600,
        }
    }
}

struct RetiredKey {
    jwk: Jwk,
    valid_until: i64,
}

struct JwksState {
    active: BTreeMap<String, Jwk>,
    retired: Vec<RetiredKey>,
    last_refresh_unix: Option<i64>,
    refresh_count: u64,
}

/// Thread-safe JWKS cache supporting graceful key rotation and refresh-on-miss throttling (`SEC-001`).
pub struct JwksCache {
    config: JwksCacheConfig,
    state: Mutex<JwksState>,
}

impl std::fmt::Debug for JwksCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JwksCache")
            .field("config", &self.config)
            .finish()
    }
}

impl JwksCache {
    /// Creates an initialized JWKS cache pre-loaded with `initial_keys`.
    #[must_use]
    pub fn new(config: JwksCacheConfig, initial_keys: Vec<Jwk>, now_unix_secs: i64) -> Self {
        let mut active = BTreeMap::new();
        for k in initial_keys {
            active.insert(k.kid.clone(), k);
        }
        Self {
            config,
            state: Mutex::new(JwksState {
                active,
                retired: Vec::new(),
                last_refresh_unix: Some(now_unix_secs),
                refresh_count: 0,
            }),
        }
    }

    /// Replaces active keys while retaining displaced keys through `rotation_grace_secs`.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorCode::Internal`] if internal state lock is poisoned.
    pub fn rotate_keys(&self, new_keys: Vec<Jwk>, now_unix_secs: i64) -> GapResult<()> {
        let mut guard = self
            .state
            .lock()
            .map_err(|_| GapError::new(ErrorCode::Internal, "JWKS cache lock poisoned"))?;
        apply_rotation(&mut guard, self.config, new_keys, now_unix_secs);
        Ok(())
    }

    /// Returns total number of upstream fetches performed (observable in tests).
    ///
    /// # Errors
    ///
    /// Returns [`ErrorCode::Internal`] if internal state lock is poisoned.
    pub fn refresh_count(&self) -> GapResult<u64> {
        let guard = self
            .state
            .lock()
            .map_err(|_| GapError::new(ErrorCode::Internal, "JWKS cache lock poisoned"))?;
        Ok(guard.refresh_count)
    }

    fn resolve_key(
        &self,
        kid: &str,
        now_unix_secs: i64,
        fetcher: Option<&dyn JwksFetcher>,
    ) -> GapResult<Jwk> {
        let should_fetch = {
            let mut guard = self
                .state
                .lock()
                .map_err(|_| GapError::new(ErrorCode::Internal, "JWKS cache lock poisoned"))?;
            guard.retired.retain(|r| r.valid_until > now_unix_secs);

            let expired = guard.last_refresh_unix.is_none_or(|last| {
                now_unix_secs.saturating_sub(last) >= self.config.cache_ttl_secs
            });

            if !expired {
                if let Some(k) = guard.active.get(kid) {
                    return Ok(k.clone());
                }
                if let Some(r) = guard.retired.iter().find(|r| r.jwk.kid == kid) {
                    return Ok(r.jwk.clone());
                }
            }

            let cooldown_ok = guard.last_refresh_unix.is_none_or(|last| {
                now_unix_secs.saturating_sub(last) >= self.config.min_refresh_interval_secs
            });
            let can_fetch = fetcher.is_some() && (expired || cooldown_ok);
            if !can_fetch {
                if let Some(k) = guard.active.get(kid) {
                    return Ok(k.clone());
                }
                if let Some(r) = guard.retired.iter().find(|r| r.jwk.kid == kid) {
                    return Ok(r.jwk.clone());
                }
            }
            can_fetch
        };

        if should_fetch
            && let Some(f) = fetcher
            && let Ok(fresh) = f.fetch_keys()
        {
            let mut guard = self
                .state
                .lock()
                .map_err(|_| GapError::new(ErrorCode::Internal, "JWKS cache lock poisoned"))?;
            guard.refresh_count += 1;
            apply_rotation(&mut guard, self.config, fresh, now_unix_secs);
        }

        let mut guard = self
            .state
            .lock()
            .map_err(|_| GapError::new(ErrorCode::Internal, "JWKS cache lock poisoned"))?;
        guard.retired.retain(|r| r.valid_until > now_unix_secs);
        if let Some(k) = guard.active.get(kid) {
            return Ok(k.clone());
        }
        if let Some(r) = guard.retired.iter().find(|r| r.jwk.kid == kid) {
            return Ok(r.jwk.clone());
        }
        Err(GapError::new(
            ErrorCode::Unauthenticated,
            "signing key not found in JWKS",
        ))
    }
}

fn apply_rotation(
    state: &mut JwksState,
    config: JwksCacheConfig,
    new_keys: Vec<Jwk>,
    now_unix_secs: i64,
) {
    let mut next_active = BTreeMap::new();
    for k in new_keys {
        next_active.insert(k.kid.clone(), k);
    }
    let old_active = std::mem::replace(&mut state.active, next_active);
    let grace_until = now_unix_secs.saturating_add(config.rotation_grace_secs);
    state.retired.retain(|r| r.valid_until > now_unix_secs);
    for (kid, old_key) in old_active {
        if !state.active.contains_key(&kid) {
            state.retired.push(RetiredKey {
                jwk: old_key,
                valid_until: grace_until,
            });
        }
    }
    state.last_refresh_unix = Some(now_unix_secs);
}

/// Validation policy configuration for [`JwtValidator`] (`SEC-001`, `SEC-004`, `SEC-010`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JwtValidationConfig {
    /// Allowed JWS algorithms (defaults to `EdDSA` and `RS256`).
    pub allowed_algs: BTreeSet<AllowedJwtAlg>,
    /// Allowed token issuer (`iss`) values (must be non-empty; default-deny).
    pub trusted_issuers: BTreeSet<String>,
    /// Expected `aud` claim value (`SEC-001`).
    pub expected_audience: String,
    /// Optional RFC 8707 expected `resource` URI (`SEC-004`).
    pub expected_resource: Option<String>,
    /// Maximum tolerated `exp`/`nbf` clock skew in seconds (`<= 60` enforced).
    pub max_clock_skew_secs: i64,
    /// Claim key carrying authenticated tenant ID (`SEC-010`).
    pub tenant_claim_name: String,
    /// Optional trusted issuer -> fallback pinned tenant mapping when issuer represents single tenant.
    pub issuer_default_tenants: BTreeMap<String, TenantId>,
}

impl JwtValidationConfig {
    /// Creates a strict JWT validation configuration allowing `EdDSA` and `RS256`.
    ///
    /// Clock skew is clamped at [`MAX_ALLOWED_CLOCK_SKEW_SECS`] (`60 s`).
    #[must_use]
    pub fn new(
        trusted_issuers: impl IntoIterator<Item = String>,
        expected_audience: impl Into<String>,
    ) -> Self {
        let mut allowed_algs = BTreeSet::new();
        allowed_algs.insert(AllowedJwtAlg::EdDsa);
        allowed_algs.insert(AllowedJwtAlg::Rs256);
        Self {
            allowed_algs,
            trusted_issuers: trusted_issuers.into_iter().collect(),
            expected_audience: expected_audience.into(),
            expected_resource: None,
            max_clock_skew_secs: MAX_ALLOWED_CLOCK_SKEW_SECS,
            tenant_claim_name: "tenant".to_owned(),
            issuer_default_tenants: BTreeMap::new(),
        }
    }
}

/// Validates JWT bearer credentials against a [`JwksCache`] (`SEC-001`, `SEC-004`, `SEC-010`, `SEC-012`).
pub struct JwtValidator<'a> {
    config: JwtValidationConfig,
    jwks: &'a JwksCache,
}

impl<'a> JwtValidator<'a> {
    /// Creates a new validator backed by `config` and `jwks`.
    #[must_use]
    pub const fn new(config: JwtValidationConfig, jwks: &'a JwksCache) -> Self {
        Self { config, jwks }
    }

    /// Verifies compact JWS `token` at `now_unix_secs` and returns an authenticated [`DerivedPrincipalOutput`].
    ///
    /// # Errors
    ///
    /// Returns [`ErrorCode::Unauthenticated`] on malformed tokens, disallowed `alg`, signature mismatch,
    /// untrusted `iss`, missing/wrong `aud` or `resource`, expired/`nbf` claims, missing `tenant`,
    /// or malformed `act` delegation claims.
    pub fn validate(
        &self,
        token: &str,
        hints: Option<&UntrustedRequestHints>,
        fetcher: Option<&dyn JwksFetcher>,
        now_unix_secs: i64,
    ) -> GapResult<DerivedPrincipalOutput> {
        let mut segments = token.split('.');
        let (Some(header_b64), Some(claims_b64), Some(sig_b64), None) = (
            segments.next(),
            segments.next(),
            segments.next(),
            segments.next(),
        ) else {
            return Err(GapError::new(
                ErrorCode::Unauthenticated,
                "JWT must contain 3 dot-separated segments",
            ));
        };

        let header_bytes = base64url_decode(header_b64).map_err(|_| {
            GapError::new(ErrorCode::Unauthenticated, "invalid JWT header base64url")
        })?;
        let header_str = std::str::from_utf8(&header_bytes)
            .map_err(|_| GapError::new(ErrorCode::Unauthenticated, "non-UTF8 JWT header"))?;
        let header = JsonValue::parse(header_str)
            .map_err(|_| GapError::new(ErrorCode::Unauthenticated, "malformed JWT header JSON"))?;

        let alg_str = header
            .get_str("alg")
            .ok_or_else(|| GapError::new(ErrorCode::Unauthenticated, "JWT header missing alg"))?;
        let alg = AllowedJwtAlg::parse(alg_str)?;
        if !self.config.allowed_algs.contains(&alg) {
            return Err(GapError::new(
                ErrorCode::Unauthenticated,
                "JWT alg not enabled by policy",
            ));
        }

        let kid = header
            .get_str("kid")
            .filter(|s| !s.is_empty())
            .ok_or_else(|| GapError::new(ErrorCode::Unauthenticated, "JWT header missing kid"))?;

        let sig_bytes = base64url_decode(sig_b64).map_err(|_| {
            GapError::new(
                ErrorCode::Unauthenticated,
                "invalid JWT signature base64url",
            )
        })?;

        let jwk = self.jwks.resolve_key(kid, now_unix_secs, fetcher)?;
        let signing_input_len = header_b64.len() + 1 + claims_b64.len();
        let signing_input = token.as_bytes().get(..signing_input_len).ok_or_else(|| {
            GapError::new(ErrorCode::Unauthenticated, "invalid JWT signing input")
        })?;
        jwk.verify_signature(alg, signing_input, &sig_bytes)?;

        let claims_bytes = base64url_decode(claims_b64).map_err(|_| {
            GapError::new(ErrorCode::Unauthenticated, "invalid JWT claims base64url")
        })?;
        let claims_str = std::str::from_utf8(&claims_bytes)
            .map_err(|_| GapError::new(ErrorCode::Unauthenticated, "non-UTF8 JWT claims"))?;
        let claims = JsonValue::parse(claims_str)
            .map_err(|_| GapError::new(ErrorCode::Unauthenticated, "malformed JWT claims JSON"))?;

        self.validate_registered_claims(&claims, now_unix_secs)?;
        let cred = self.extract_credential_claims(&claims)?;
        Ok(derive_authenticated_principal(cred, hints))
    }

    fn validate_registered_claims(&self, claims: &JsonValue, now_unix_secs: i64) -> GapResult<()> {
        let skew = self
            .config
            .max_clock_skew_secs
            .clamp(0, MAX_ALLOWED_CLOCK_SKEW_SECS);

        let iss = claims
            .get_str("iss")
            .filter(|s| !s.is_empty())
            .ok_or_else(|| GapError::new(ErrorCode::Unauthenticated, "JWT claims missing iss"))?;
        if !self.config.trusted_issuers.contains(iss) {
            return Err(GapError::new(
                ErrorCode::Unauthenticated,
                "untrusted JWT issuer",
            ));
        }

        if !claim_matches_target(claims.get("aud"), &self.config.expected_audience) {
            return Err(GapError::new(
                ErrorCode::Unauthenticated,
                "JWT audience mismatch",
            ));
        }

        if let Some(expected_res) = &self.config.expected_resource {
            let resource_match = claim_matches_target(claims.get("resource"), expected_res)
                || claim_matches_target(claims.get("aud"), expected_res);
            if !resource_match {
                return Err(GapError::new(
                    ErrorCode::Unauthenticated,
                    "JWT RFC 8707 resource indicator mismatch",
                ));
            }
        }

        let exp = claims.get_i64("exp").ok_or_else(|| {
            GapError::new(ErrorCode::Unauthenticated, "JWT missing numeric exp claim")
        })?;
        if now_unix_secs.saturating_sub(skew) >= exp {
            return Err(GapError::new(ErrorCode::Unauthenticated, "JWT expired"));
        }

        if let Some(nbf) = claims.get_i64("nbf")
            && now_unix_secs.saturating_add(skew) < nbf
        {
            return Err(GapError::new(
                ErrorCode::Unauthenticated,
                "JWT not yet valid (nbf)",
            ));
        }

        let sub = claims.get_str("sub").filter(|s| !s.is_empty());
        if sub.is_none() {
            return Err(GapError::new(
                ErrorCode::Unauthenticated,
                "JWT missing non-empty sub",
            ));
        }
        Ok(())
    }

    fn extract_credential_claims(&self, claims: &JsonValue) -> GapResult<CredentialClaims> {
        let iss = claims.get_str("iss").unwrap_or("");
        let subject = claims
            .get_str("sub")
            .ok_or_else(|| GapError::new(ErrorCode::Unauthenticated, "JWT missing sub"))?
            .to_owned();

        let tenant_raw = claims
            .get_str(&self.config.tenant_claim_name)
            .or_else(|| claims.get_str("tenant_id"));
        let tenant = match tenant_raw {
            Some(raw) => TenantId::new(raw).map_err(|_| {
                GapError::new(
                    ErrorCode::Unauthenticated,
                    "JWT tenant claim invalid identifier",
                )
            })?,
            None => self
                .config
                .issuer_default_tenants
                .get(iss)
                .cloned()
                .ok_or_else(|| {
                    GapError::new(
                        ErrorCode::Unauthenticated,
                        "JWT missing tenant binding claim",
                    )
                })?,
        };

        let kind = parse_principal_kind(
            claims
                .get_str("kind")
                .or_else(|| claims.get_str("principal_type")),
        )?;
        let scopes = extract_scopes_from_claims(claims);
        let delegation = parse_rfc8693_act_chain(claims.get("act"))?;
        let attrs = extract_safe_attrs(claims);

        Ok(CredentialClaims {
            subject,
            kind,
            tenant,
            scopes,
            attrs,
            delegation,
        })
    }
}

fn claim_matches_target(claim: Option<&JsonValue>, target: &str) -> bool {
    match claim {
        Some(JsonValue::String(s)) => s == target,
        Some(JsonValue::Array(arr)) => arr.iter().any(|v| v.as_str() == Some(target)),
        _ => false,
    }
}
