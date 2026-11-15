//! Authentication subsystem (`SEC-001`..`SEC-012`, `NGC-081`): JWT/JWKS, RFC 7662 introspection,
//! Argon2id API keys, mTLS SPIFFE, stdio OS-user, RFC 9728 PRM, and RFC 8693 token exchange.

pub mod api_key;
pub mod crypto;
pub mod exchange;
pub mod introspect;
pub mod json;
pub mod jwt;
pub mod mtls;
pub mod principal;
pub mod prm;

#[cfg(test)]
mod tests;

use std::net::IpAddr;

pub use api_key::{AnonymousModePolicy, ApiKeyAuthenticator, ApiKeyIssueSpec, ApiKeyRecord};
pub use exchange::{
    ExchangedToken, RFC8693_ACCESS_TOKEN_TYPE, RFC8693_GRANT_TYPE, TokenExchangeClient,
    TokenExchangeRequest, TokenExchangeTransport,
};
pub use introspect::{
    IntrospectionConfig, IntrospectionResponse, IntrospectionTransport, TokenIntrospector,
};
pub use jwt::{
    AllowedJwtAlg, Jwk, JwkKeyMaterial, JwksCache, JwksCacheConfig, JwksFetcher,
    JwtValidationConfig, JwtValidator, MAX_ALLOWED_CLOCK_SKEW_SECS,
};
pub use mtls::{MtlsAuthenticator, SpiffeBinding, StdioAuthenticator, VerifiedClientCert};
pub use principal::{
    CredentialClaims, DerivedPrincipalOutput, MAX_DELEGATION_DEPTH, UntrustedRequestHints,
    derive_authenticated_principal, extract_safe_attrs, extract_scopes_from_claims,
    intersect_delegated_scopes, parse_principal_kind, parse_rfc8693_act_chain,
};
pub use prm::{
    AuthChallenge, CONSTANT_AUTH_FAILURE_MESSAGE, IpAuthRateLimitConfig, IpAuthRateLimiter,
    PRM_MCP_WELL_KNOWN_PATH, PRM_WELL_KNOWN_PATH, ProtectedResourceMetadata,
};

/// Inbound authentication credential presented at the GAP gateway edge (`SEC-001`).
#[derive(Clone)]
pub enum AuthCredentials<'a> {
    /// Asymmetric OIDC/OAuth JWT bearer token.
    BearerJwt {
        /// Compact JWS token (`header.payload.signature`).
        token: &'a str,
        /// Optional live JWKS refresh callback.
        fetcher: Option<&'a dyn JwksFetcher>,
    },
    /// Opaque bearer token verified via RFC 7662 introspection.
    OpaqueToken(&'a str),
    /// Structured API key (`<key_id>.<secret>`).
    ApiKey(&'a str),
    /// Mutual-TLS client certificate verified by the TLS terminator.
    Mtls(&'a VerifiedClientCert),
    /// Local OS user session on trusted `stdio` MCP transport.
    StdioOsUser(&'a str),
    /// Unauthenticated attempt (rejected fail-closed outside local dev loopback).
    Anonymous,
}

impl std::fmt::Debug for AuthCredentials<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BearerJwt { fetcher, .. } => f
                .debug_struct("BearerJwt")
                .field("token", &"[REDACTED]")
                .field("has_fetcher", &fetcher.is_some())
                .finish(),
            Self::OpaqueToken(_) => f.write_str("OpaqueToken([REDACTED])"),
            Self::ApiKey(_) => f.write_str("ApiKey([REDACTED])"),
            Self::Mtls(cert) => f.debug_tuple("Mtls").field(cert).finish(),
            Self::StdioOsUser(user) => f.debug_tuple("StdioOsUser").field(user).finish(),
            Self::Anonymous => f.write_str("Anonymous"),
        }
    }
}

/// Request context metadata needed at authentication time (`SEC-002`, `SEC-003`, `SEC-010`).
#[derive(Debug, Clone, Default)]
pub struct AuthRequestMeta<'a> {
    /// Client peer IP address used for per-IP failure throttling and loopback dev checks.
    pub peer_ip: Option<IpAddr>,
    /// Untrusted `X-Tenant`/body tenant hints strictly ignored during principal derivation.
    pub hints: Option<&'a UntrustedRequestHints>,
}

/// Unified edge authenticator orchestrating configured credential validators with constant-shape
/// HTTP `401`/`429` challenges (`SEC-001`..`SEC-004`, `SEC-010`..`SEC-012`).
pub struct Authenticator<'a> {
    /// Optional JWT validator.
    pub jwt: Option<&'a JwtValidator<'a>>,
    /// Optional opaque token introspector.
    pub introspect: Option<&'a TokenIntrospector<'a>>,
    /// Optional API-key authenticator.
    pub api_key: Option<&'a ApiKeyAuthenticator>,
    /// Optional mTLS SPIFFE authenticator.
    pub mtls: Option<&'a MtlsAuthenticator>,
    /// Optional local `stdio` OS-user authenticator.
    pub stdio: Option<&'a StdioAuthenticator>,
    /// Fail-closed anonymous mode policy (`SEC-002`).
    pub anonymous_policy: AnonymousModePolicy,
    /// Optional RFC 9728 Protected Resource Metadata included in `WWW-Authenticate` (`SEC-004`).
    pub prm: Option<&'a ProtectedResourceMetadata>,
    /// Optional sliding-window per-IP authentication failure rate limiter (`SEC-003`).
    pub rate_limiter: Option<&'a IpAuthRateLimiter>,
}

impl<'a> Authenticator<'a> {
    /// Authenticates `creds` at `now_unix_secs`, returning a verified [`DerivedPrincipalOutput`] or
    /// a boxed constant-shape [`AuthChallenge`] (`SEC-003`, `SEC-004`).
    pub fn authenticate(
        &self,
        creds: AuthCredentials<'_>,
        meta: AuthRequestMeta<'_>,
        now_unix_secs: i64,
    ) -> Result<DerivedPrincipalOutput, Box<AuthChallenge>> {
        if let (Some(limiter), Some(ip)) = (self.rate_limiter, meta.peer_ip)
            && let Ok(Some(retry_after)) = limiter.check_blocked(ip, now_unix_secs)
        {
            return Err(Box::new(AuthChallenge::rate_limited(
                retry_after,
                self.prm,
                "per-IP auth failure rate limit active",
            )));
        }

        let outcome = match creds {
            AuthCredentials::BearerJwt { token, fetcher } => match self.jwt {
                Some(v) => v.validate(token, meta.hints, fetcher, now_unix_secs),
                None => Err(gap_core::error::GapError::new(
                    gap_core::error::ErrorCode::Unauthenticated,
                    "JWT authentication not configured",
                )),
            },
            AuthCredentials::OpaqueToken(token) => match self.introspect {
                Some(i) => i.validate(token, meta.hints, now_unix_secs),
                None => Err(gap_core::error::GapError::new(
                    gap_core::error::ErrorCode::Unauthenticated,
                    "introspection authentication not configured",
                )),
            },
            AuthCredentials::ApiKey(key) => match self.api_key {
                Some(a) => a.authenticate(key, meta.hints, now_unix_secs),
                None => Err(gap_core::error::GapError::new(
                    gap_core::error::ErrorCode::Unauthenticated,
                    "API key authentication not configured",
                )),
            },
            AuthCredentials::Mtls(cert) => match self.mtls {
                Some(m) => m.authenticate(cert, meta.hints),
                None => Err(gap_core::error::GapError::new(
                    gap_core::error::ErrorCode::Unauthenticated,
                    "mTLS authentication not configured",
                )),
            },
            AuthCredentials::StdioOsUser(user) => match self.stdio {
                Some(s) => s.authenticate_os_user(user, meta.hints),
                None => Err(gap_core::error::GapError::new(
                    gap_core::error::ErrorCode::Unauthenticated,
                    "stdio authentication not configured",
                )),
            },
            AuthCredentials::Anonymous => self
                .anonymous_policy
                .try_authenticate_anonymous(meta.peer_ip, meta.hints),
        };

        match outcome {
            Ok(out) => Ok(out),
            Err(err) => {
                if let (Some(limiter), Some(ip)) = (self.rate_limiter, meta.peer_ip)
                    && let Ok(Some(retry_after)) = limiter.record_failure(ip, now_unix_secs)
                {
                    return Err(Box::new(AuthChallenge::rate_limited(
                        retry_after,
                        self.prm,
                        err.message().to_owned(),
                    )));
                }
                Err(Box::new(AuthChallenge::unauthenticated(
                    self.prm,
                    err.message().to_owned(),
                )))
            }
        }
    }
}
