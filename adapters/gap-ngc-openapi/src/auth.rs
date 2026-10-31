//! Credential resolution, redaction, and upstream origin transport guards (`CMP-031`, `INV-15`).

use std::collections::BTreeMap;
use std::fmt;

use axum::http::{HeaderName, HeaderValue};
use gap_core::secret::{REDACTED_MARKER, SecretString};
use thiserror::Error;
use url::{Host, Url};

use crate::extract::is_rfc7230_token;
use crate::request::{BuiltRequest, append_encoded_query_pair};

/// Standard RFC 4648 Base64 alphabet.
const BASE64_ALPHABET: &[u8; 64] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Forbidden HTTP headers that an `ApiKey` header configuration must never override (`CMP-030`, `CMP-031`).
const FORBIDDEN_AUTH_HEADERS: &[&str] =
    &["host", "content-length", "transfer-encoding", "connection"];

/// Errors produced when validating upstream `base_url` or resolving connector credentials (`CMP-031`, `INV-15`).
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum AuthError {
    /// `base_url` could not be parsed as a valid URL.
    #[error("invalid upstream base_url")]
    InvalidBaseUrl,
    /// `https://` is refused in v0 because no TLS backend is vendored (`CMP-031`).
    #[error("https base_url is unsupported in gap-ngc-openapi v0 (no TLS backend configured)")]
    TlsUnsupported,
    /// Scheme was neither `http` nor `https`.
    #[error("unsupported base_url scheme `{0}` (only `http` is supported in v0)")]
    UnsupportedScheme(String),
    /// `base_url` carried embedded userinfo, query parameters, or a fragment.
    #[error("base_url must not contain userinfo, query parameters, or fragments")]
    MalformedBaseUrlComponents,
    /// Credentials were configured against a non-loopback `http://` host without `allow_insecure_upstream`.
    #[error(
        "refusing to send credentials over cleartext http to a non-loopback host without allow_insecure_upstream"
    )]
    InsecureCleartextCredentials,
    /// Referenced credential name was not found in [`CredentialSource`].
    #[error("credential reference `{0}` was not found in CredentialSource")]
    MissingCredential(String),
    /// Configured `Basic` username or `ApiKey` header/query name was invalid, or secret bytes were invalid for HTTP headers.
    #[error("invalid authentication configuration: {0}")]
    InvalidAuthParameter(String),
}

/// Where an API key is attached on outbound upstream requests (`CMP-031`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApiKeyLocation {
    /// Sent as an HTTP header (`<name>: <secret>`).
    Header,
    /// Appended as a percent-encoded query parameter (`?<name>=<secret>`).
    Query,
}

/// Declarative authentication configuration referencing secrets by name (`CMP-031`, `INV-15`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum AuthConfig {
    /// No upstream authentication.
    #[default]
    None,
    /// `Authorization: Bearer <secret>`.
    Bearer {
        /// Key looked up in [`CredentialSource`].
        credential: String,
    },
    /// API key in a header or query parameter.
    ApiKey {
        /// Target slot (`Header` or `Query`).
        location: ApiKeyLocation,
        /// Header or query parameter name.
        name: String,
        /// Key looked up in [`CredentialSource`].
        credential: String,
    },
    /// `Authorization: Basic base64(<user>:<secret>)`.
    Basic {
        /// Non-secret username (must not contain `':'`).
        user: String,
        /// Key looked up in [`CredentialSource`].
        credential: String,
    },
}

impl AuthConfig {
    /// Returns `true` if this configuration attaches any secret material.
    #[must_use]
    pub const fn has_credentials(&self) -> bool {
        !matches!(self, Self::None)
    }
}

/// Source resolving symbolic credential references into [`SecretString`]s (`CMP-031`, `INV-15`).
///
/// `gap_sec::vault` is a stub until `WP-M6-07`; connectors resolve credentials through this trait.
pub trait CredentialSource: Send + Sync + 'static {
    /// Resolves `credential_ref` into secret material.
    fn resolve(&self, credential_ref: &str) -> Result<SecretString, AuthError>;
}

/// In-memory map-backed [`CredentialSource`] for static configuration and tests (`INV-15`).
#[derive(Clone, Default)]
pub struct StaticCredentialSource {
    entries: BTreeMap<String, SecretString>,
}

impl StaticCredentialSource {
    /// Creates an empty [`StaticCredentialSource`].
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Inserts a secret under `credential_ref`.
    pub fn insert(&mut self, credential_ref: impl Into<String>, secret: impl Into<String>) {
        self.entries
            .insert(credential_ref.into(), SecretString::new(secret.into()));
    }

    /// Builder helper returning `self` with `(credential_ref, secret)` added.
    #[must_use]
    pub fn with_secret(
        mut self,
        credential_ref: impl Into<String>,
        secret: impl Into<String>,
    ) -> Self {
        self.insert(credential_ref, secret);
        self
    }
}

impl fmt::Debug for StaticCredentialSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let keys: Vec<&str> = self.entries.keys().map(String::as_str).collect();
        f.debug_struct("StaticCredentialSource")
            .field("credential_refs", &keys)
            .field("secrets", &REDACTED_MARKER)
            .finish()
    }
}

impl CredentialSource for StaticCredentialSource {
    fn resolve(&self, credential_ref: &str) -> Result<SecretString, AuthError> {
        self.entries
            .get(credential_ref)
            .cloned()
            .ok_or_else(|| AuthError::MissingCredential(credential_ref.to_owned()))
    }
}

fn is_loopback_host(host: Option<Host<&str>>) -> bool {
    match host {
        Some(Host::Ipv4(ip)) => ip.is_loopback(),
        Some(Host::Ipv6(ip)) => ip.is_loopback(),
        Some(Host::Domain(domain)) => domain.eq_ignore_ascii_case("localhost"),
        None => false,
    }
}

/// Validated upstream base URL whose scheme, host, port, and base path prefix are locked at startup (`CMP-030`, `CMP-031`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedBaseUrl {
    url: Url,
    base_path: String,
}

impl ValidatedBaseUrl {
    /// Validates `raw` under `has_credentials` and `allow_insecure_upstream`.
    pub fn parse(
        raw: &str,
        has_credentials: bool,
        allow_insecure_upstream: bool,
    ) -> Result<Self, AuthError> {
        let parsed = Url::parse(raw).map_err(|_| AuthError::InvalidBaseUrl)?;
        match parsed.scheme() {
            "http" => {}
            "https" => return Err(AuthError::TlsUnsupported),
            other => return Err(AuthError::UnsupportedScheme(other.to_owned())),
        }
        if parsed.host().is_none() {
            return Err(AuthError::InvalidBaseUrl);
        }
        if !parsed.username().is_empty()
            || parsed.password().is_some()
            || parsed.query().is_some()
            || parsed.fragment().is_some()
        {
            return Err(AuthError::MalformedBaseUrlComponents);
        }
        if has_credentials && !is_loopback_host(parsed.host()) && !allow_insecure_upstream {
            return Err(AuthError::InsecureCleartextCredentials);
        }
        let base_path = parsed.path().trim_end_matches('/').to_owned();
        Ok(Self {
            url: parsed,
            base_path,
        })
    }

    /// Borrows the validated [`Url`].
    #[must_use]
    pub const fn as_url(&self) -> &Url {
        &self.url
    }

    /// Joins a validated relative `path_and_query` onto this base URL while guaranteeing the
    /// scheme, host, and port remain identical (`CMP-030`).
    pub fn join_relative(&self, path_and_query: &str) -> Result<Url, AuthError> {
        if !path_and_query.starts_with('/')
            || path_and_query.starts_with("//")
            || path_and_query.contains('#')
        {
            return Err(AuthError::MalformedBaseUrlComponents);
        }
        let (rel_path, opt_query) = match path_and_query.split_once('?') {
            Some((p, q)) => (p, Some(q)),
            None => (path_and_query, None),
        };
        let combined_path = if self.base_path.is_empty() {
            rel_path.to_owned()
        } else if rel_path == "/" {
            self.base_path.clone()
        } else {
            format!("{}{rel_path}", self.base_path)
        };
        let mut target = self.url.clone();
        target.set_path(&combined_path);
        target.set_query(opt_query);

        if target.scheme() != self.url.scheme()
            || target.host() != self.url.host()
            || target.port() != self.url.port()
            || !target.username().is_empty()
            || target.password().is_some()
            || target.fragment().is_some()
        {
            return Err(AuthError::MalformedBaseUrlComponents);
        }
        Ok(target)
    }
}

#[derive(Clone)]
enum ResolvedAuthInner {
    None,
    Header {
        name: HeaderName,
        header_secret: SecretString,
    },
    QueryApiKey {
        name: String,
        secret: SecretString,
    },
}

/// Resolved, redacted authentication material ready to apply to outbound requests (`CMP-031`, `INV-15`).
#[derive(Clone)]
pub struct ResolvedAuth {
    inner: ResolvedAuthInner,
}

impl fmt::Debug for ResolvedAuth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("ResolvedAuth")
            .field(&REDACTED_MARKER)
            .finish()
    }
}

fn validate_secret_header_value(raw: &str) -> Result<HeaderValue, AuthError> {
    if raw
        .bytes()
        .any(|b| b == b'\r' || b == b'\n' || b == 0 || (b < 0x20 && b != b'\t') || b == 0x7f)
    {
        return Err(AuthError::InvalidAuthParameter(
            "resolved credential contains control characters".to_owned(),
        ));
    }
    let mut hv = HeaderValue::from_str(raw).map_err(|_| {
        AuthError::InvalidAuthParameter(
            "resolved credential is not valid visible ASCII for an HTTP header".to_owned(),
        )
    })?;
    hv.set_sensitive(true);
    Ok(hv)
}

impl ResolvedAuth {
    /// Resolves `config` against `source` (`CMP-031`).
    pub fn resolve(config: &AuthConfig, source: &dyn CredentialSource) -> Result<Self, AuthError> {
        let inner = match config {
            AuthConfig::None => ResolvedAuthInner::None,
            AuthConfig::Bearer { credential } => {
                let secret = source.resolve(credential)?;
                let formatted = format!("Bearer {}", secret.expose_secret());
                // Validate at construction so invalid header bytes fail closed early.
                let _ = validate_secret_header_value(&formatted)?;
                ResolvedAuthInner::Header {
                    name: axum::http::header::AUTHORIZATION,
                    header_secret: SecretString::new(formatted),
                }
            }
            AuthConfig::ApiKey {
                location,
                name,
                credential,
            } => {
                if name.is_empty() {
                    return Err(AuthError::InvalidAuthParameter(
                        "ApiKey `name` must not be empty".to_owned(),
                    ));
                }
                let secret = source.resolve(credential)?;
                match location {
                    ApiKeyLocation::Header => {
                        let lower = name.to_ascii_lowercase();
                        if FORBIDDEN_AUTH_HEADERS.contains(&lower.as_str())
                            || !is_rfc7230_token(name)
                        {
                            return Err(AuthError::InvalidAuthParameter(format!(
                                "invalid ApiKey header name `{name}`"
                            )));
                        }
                        let header_name =
                            HeaderName::from_bytes(name.as_bytes()).map_err(|_| {
                                AuthError::InvalidAuthParameter(format!(
                                    "invalid ApiKey header name `{name}`"
                                ))
                            })?;
                        let _ = validate_secret_header_value(secret.expose_secret())?;
                        ResolvedAuthInner::Header {
                            name: header_name,
                            header_secret: secret,
                        }
                    }
                    ApiKeyLocation::Query => ResolvedAuthInner::QueryApiKey {
                        name: name.clone(),
                        secret,
                    },
                }
            }
            AuthConfig::Basic { user, credential } => {
                if user.is_empty()
                    || user.contains(':')
                    || user.bytes().any(|b| b < 0x20 || b == 0x7f)
                {
                    return Err(AuthError::InvalidAuthParameter(
                        "Basic auth `user` must be non-empty and must not contain `:` or control characters"
                            .to_owned(),
                    ));
                }
                let secret = source.resolve(credential)?;
                let combined = SecretString::new(format!("{user}:{}", secret.expose_secret()));
                let b64 = encode_base64(combined.expose_secret().as_bytes());
                // Explicit drop of `combined` zeroizes the intermediate `user:secret` buffer.
                drop(combined);
                let formatted = format!("Basic {b64}");
                let _ = validate_secret_header_value(&formatted)?;
                ResolvedAuthInner::Header {
                    name: axum::http::header::AUTHORIZATION,
                    header_secret: SecretString::new(formatted),
                }
            }
        };
        Ok(Self { inner })
    }

    /// Applies resolved credentials onto `req` (`CMP-031`, `INV-15`).
    pub fn apply(&self, req: &mut BuiltRequest) -> Result<(), AuthError> {
        match &self.inner {
            ResolvedAuthInner::None => Ok(()),
            ResolvedAuthInner::Header {
                name,
                header_secret,
            } => {
                let hv = validate_secret_header_value(header_secret.expose_secret())?;
                req.headers.retain(|(existing, _)| existing != name);
                req.headers.push((name.clone(), hv));
                Ok(())
            }
            ResolvedAuthInner::QueryApiKey { name, secret } => {
                append_encoded_query_pair(&mut req.path_and_query, name, secret.expose_secret());
                Ok(())
            }
        }
    }
}

/// Standard RFC 4648 Base64 encoder (`CMP-031`, `CMP-033`).
#[must_use]
pub(crate) fn encode_base64(bytes: &[u8]) -> String {
    if bytes.is_empty() {
        return String::new();
    }
    let out_len = bytes.len().div_ceil(3) * 4;
    let mut out = String::with_capacity(out_len);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0];
        let b1 = chunk.get(1).copied().unwrap_or(0);
        let b2 = chunk.get(2).copied().unwrap_or(0);

        out.push(char::from(BASE64_ALPHABET[usize::from(b0 >> 2)]));
        out.push(char::from(
            BASE64_ALPHABET[usize::from(((b0 & 0x03) << 4) | (b1 >> 4))],
        ));
        if chunk.len() > 1 {
            out.push(char::from(
                BASE64_ALPHABET[usize::from(((b1 & 0x0f) << 2) | (b2 >> 6))],
            ));
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(char::from(BASE64_ALPHABET[usize::from(b2 & 0x3f)]));
        } else {
            out.push('=');
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // spec: CMP-031, INV-15
    #[test]
    fn inv_15_auth_modes_inject_expected_wire_credentials_and_redact_all_debug_and_errors() {
        let canary = "sk-live-super-secret-canary-4242";
        let store = StaticCredentialSource::new().with_secret("shop_tok", canary);

        assert!(
            !format!("{store:?}").contains(canary),
            "StaticCredentialSource Debug leaked secret"
        );

        // 1. Bearer
        let bearer = ResolvedAuth::resolve(
            &AuthConfig::Bearer {
                credential: "shop_tok".to_owned(),
            },
            &store,
        )
        .expect("resolve bearer");
        assert!(!format!("{bearer:?}").contains(canary));
        let mut req = BuiltRequest {
            path_and_query: "/customers/1".to_owned(),
            headers: Vec::new(),
        };
        bearer.apply(&mut req).expect("apply bearer");
        assert_eq!(req.headers.len(), 1);
        assert_eq!(req.headers[0].0.as_str(), "authorization");
        assert_eq!(
            req.headers[0].1.to_str().expect("str"),
            format!("Bearer {canary}")
        );
        // HeaderValue must be marked sensitive so its own Debug never prints the secret
        assert!(
            !format!("{:?}", req.headers[0].1).contains(canary),
            "HeaderValue Debug leaked secret"
        );

        // 2. ApiKey in Header
        let api_hdr = ResolvedAuth::resolve(
            &AuthConfig::ApiKey {
                location: ApiKeyLocation::Header,
                name: "X-API-Key".to_owned(),
                credential: "shop_tok".to_owned(),
            },
            &store,
        )
        .expect("resolve api header");
        let mut req = BuiltRequest {
            path_and_query: "/customers".to_owned(),
            headers: Vec::new(),
        };
        api_hdr.apply(&mut req).expect("apply api header");
        assert_eq!(req.headers[0].0.as_str(), "x-api-key");
        assert_eq!(req.headers[0].1.to_str().expect("str"), canary);
        assert!(!format!("{:?}", req.headers[0].1).contains(canary));

        // 3. ApiKey in Query (percent-encodes special chars)
        let special_secret = "tok?evil=1&x=2#frag";
        let store2 = StaticCredentialSource::new().with_secret("q_tok", special_secret);
        let api_q = ResolvedAuth::resolve(
            &AuthConfig::ApiKey {
                location: ApiKeyLocation::Query,
                name: "api_key".to_owned(),
                credential: "q_tok".to_owned(),
            },
            &store2,
        )
        .expect("resolve api query");
        let mut req = BuiltRequest {
            path_and_query: "/customers?limit=5".to_owned(),
            headers: Vec::new(),
        };
        api_q.apply(&mut req).expect("apply api query");
        assert_eq!(
            req.path_and_query,
            "/customers?limit=5&api_key=tok%3Fevil%3D1%26x%3D2%23frag"
        );

        // 4. Basic auth
        let basic = ResolvedAuth::resolve(
            &AuthConfig::Basic {
                user: "svc-user".to_owned(),
                credential: "shop_tok".to_owned(),
            },
            &store,
        )
        .expect("resolve basic");
        let mut req = BuiltRequest {
            path_and_query: "/customers".to_owned(),
            headers: Vec::new(),
        };
        basic.apply(&mut req).expect("apply basic");
        let expected_b64 = encode_base64(format!("svc-user:{canary}").as_bytes());
        assert_eq!(
            req.headers[0].1.to_str().expect("str"),
            format!("Basic {expected_b64}")
        );
        assert!(!format!("{basic:?}").contains(canary));
        assert!(!format!("{:?}", req.headers[0].1).contains(&expected_b64));
    }

    // spec: CMP-031, CMP-030
    #[test]
    fn base_url_refuses_https_non_loopback_cleartext_credentials_and_preserves_origin() {
        // https is always refused in v0 (fail closed)
        assert_eq!(
            ValidatedBaseUrl::parse("https://127.0.0.1:8443", false, false),
            Err(AuthError::TlsUnsupported)
        );
        assert_eq!(
            ValidatedBaseUrl::parse("https://api.example.com/v1", true, true),
            Err(AuthError::TlsUnsupported)
        );

        // Non-loopback http WITH credentials is refused unless allow_insecure_upstream = true
        assert_eq!(
            ValidatedBaseUrl::parse("http://10.0.0.5:8080/v1", true, false),
            Err(AuthError::InsecureCleartextCredentials)
        );
        assert!(ValidatedBaseUrl::parse("http://10.0.0.5:8080/v1", true, true).is_ok());
        assert!(ValidatedBaseUrl::parse("http://10.0.0.5:8080/v1", false, false).is_ok());

        // Loopback hosts (127.0.0.1, [::1], localhost) allow credentials over http
        for loopback in [
            "http://127.0.0.1:8080/v1",
            "http://[::1]:8080",
            "http://localhost:8080/shop/",
        ] {
            let base = ValidatedBaseUrl::parse(loopback, true, false).expect(loopback);
            let joined = base
                .join_relative("/customers/42?region=eu")
                .expect("join relative");
            assert_eq!(joined.scheme(), "http");
            assert_eq!(joined.host(), base.as_url().host());
            assert_eq!(joined.port(), base.as_url().port());
        }

        // Embedded userinfo, query, or fragment in base_url is rejected
        for bad in [
            "http://user:pass@127.0.0.1:8080",
            "http://127.0.0.1:8080?foo=bar",
            "http://127.0.0.1:8080#frag",
            "ftp://127.0.0.1:21",
        ] {
            assert!(ValidatedBaseUrl::parse(bad, false, false).is_err(), "{bad}");
        }

        // RFC 4648 base64 vectors
        assert_eq!(encode_base64(b""), "");
        assert_eq!(encode_base64(b"f"), "Zg==");
        assert_eq!(encode_base64(b"fo"), "Zm8=");
        assert_eq!(encode_base64(b"foo"), "Zm9v");
        assert_eq!(encode_base64(b"foob"), "Zm9vYg==");
        assert_eq!(encode_base64(b"fooba"), "Zm9vYmE=");
        assert_eq!(encode_base64(b"foobar"), "Zm9vYmFy");
    }
}
