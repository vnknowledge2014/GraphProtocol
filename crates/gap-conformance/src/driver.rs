//! Conformance transport drivers (`HttpDriver`, `GrpcDriver`, `StdioDriver`) and redacted
//! principal bearer-token mapping (`VER-010`, `VER-011`, `GQL-003`, `INV-15`).

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

use gap_core::secret::{REDACTED_MARKER, SecretString};
use gap_core::traits::BoxFuture;
use reqwest::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE, HeaderValue};
use serde_json::Value;
use thiserror::Error;

use crate::fixture::FixtureRequest;
use crate::matcher::ObservedResponse;

/// Exact `Accept` header value mandated for `HttpDriver` (`GQL-003`, `WP-M0-09`).
pub const GRAPHQL_ACCEPT_HEADER: &str = "application/graphql-response+json, application/json;q=0.9";

/// Specific failure loading a `--principals <file.json>` mapping (`INV-15`).
#[derive(Debug, Error)]
pub enum PrincipalsErrorKind {
    /// Filesystem read error.
    #[error("I/O error: {0}")]
    Io(#[source] std::io::Error),
    /// Invalid JSON syntax (reports only line/column so file contents can never leak).
    #[error("invalid JSON syntax at line {line}, column {column}")]
    InvalidJson {
        /// 1-based line number.
        line: usize,
        /// 1-based column number.
        column: usize,
    },
    /// Root value was not a JSON object.
    #[error("principals file root must be a JSON object mapping principal name to bearer token")]
    InvalidRootType,
    /// Principal name was empty/whitespace or token value was not a non-empty string.
    #[error("invalid bearer token entry for principal `{principal}`: expected non-empty string")]
    InvalidEntry {
        /// Principal key (never the secret token value).
        principal: String,
    },
}

/// Error returned when loading `--principals <file.json>` (`INV-15`).
#[derive(Debug, Error)]
#[error("principals file `{}`: {kind}", path.display())]
pub struct PrincipalsLoadError {
    /// Path to the principals JSON file.
    pub path: PathBuf,
    /// Specific failure reason (never contains secret values).
    #[source]
    pub kind: PrincipalsErrorKind,
}

/// Mapping from logical fixture `principal` names to [`SecretString`] bearer tokens (`INV-15`).
#[derive(Clone, Default)]
pub struct PrincipalsMap {
    tokens: BTreeMap<String, SecretString>,
}

impl fmt::Debug for PrincipalsMap {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PrincipalsMap")
            .field("tokens", &self.tokens)
            .finish()
    }
}

impl PrincipalsMap {
    /// Creates an empty [`PrincipalsMap`].
    #[must_use]
    pub fn empty() -> Self {
        Self {
            tokens: BTreeMap::new(),
        }
    }

    /// Creates a [`PrincipalsMap`] from `(principal, token)` pairs, wrapping each token in
    /// [`SecretString`].
    #[must_use]
    pub fn from_entries<I, K, V>(entries: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<String>,
    {
        let mut tokens = BTreeMap::new();
        for (k, v) in entries {
            tokens.insert(k.into(), SecretString::new(v.into()));
        }
        Self { tokens }
    }

    /// Loads a [`PrincipalsMap`] from a JSON object file (`{ "<principal>": "<bearer-token>" }`).
    ///
    /// # Errors
    ///
    /// Returns [`PrincipalsLoadError`] naming `path` without ever echoing token contents (`INV-15`).
    pub fn load_file(path: &Path) -> Result<Self, PrincipalsLoadError> {
        let fail = |kind: PrincipalsErrorKind| PrincipalsLoadError {
            path: path.to_path_buf(),
            kind,
        };

        let raw =
            std::fs::read_to_string(path).map_err(|err| fail(PrincipalsErrorKind::Io(err)))?;
        let parsed: Value = serde_json::from_str(&raw).map_err(|err| {
            fail(PrincipalsErrorKind::InvalidJson {
                line: err.line(),
                column: err.column(),
            })
        })?;

        let Some(obj) = parsed.as_object() else {
            return Err(fail(PrincipalsErrorKind::InvalidRootType));
        };

        let mut tokens = BTreeMap::new();
        for (principal, token_val) in obj {
            if principal.trim().is_empty() {
                return Err(fail(PrincipalsErrorKind::InvalidEntry {
                    principal: principal.clone(),
                }));
            }
            let Some(token_str) = token_val.as_str() else {
                return Err(fail(PrincipalsErrorKind::InvalidEntry {
                    principal: principal.clone(),
                }));
            };
            if token_str.trim().is_empty() {
                return Err(fail(PrincipalsErrorKind::InvalidEntry {
                    principal: principal.clone(),
                }));
            }
            tokens.insert(principal.clone(), SecretString::new(token_str.to_owned()));
        }

        Ok(Self { tokens })
    }

    /// Looks up the redacted [`SecretString`] bearer token for `principal`.
    #[must_use]
    pub fn get(&self, principal: &str) -> Option<&SecretString> {
        self.tokens.get(principal)
    }

    /// Number of configured principals.
    #[must_use]
    pub fn len(&self) -> usize {
        self.tokens.len()
    }

    /// Returns `true` if no principals are configured.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.tokens.is_empty()
    }

    /// Replaces any accidental occurrence of a configured bearer token in `input` with
    /// [`REDACTED_MARKER`] (`"***"`, `INV-15`).
    #[must_use]
    pub fn scrub_text(&self, input: &str) -> String {
        let mut out = input.to_owned();
        for secret in self.tokens.values() {
            let raw = secret.expose_secret();
            if !raw.is_empty() && out.contains(raw) {
                out = out.replace(raw, REDACTED_MARKER);
            }
        }
        out
    }
}

/// Typed error returned by [`ConformanceDriver`] implementations.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum DriverError {
    /// Target URL uses `https://`, which is unsupported because `reqwest` has no TLS backend compiled in.
    #[error(
        "https:// target `{url}` is unsupported: no TLS backend is compiled into gap-conformance"
    )]
    HttpsUnsupported {
        /// Sanitized target URL.
        url: String,
    },
    /// Target URL uses an unsupported scheme (neither `http` nor `https`).
    #[error("target URL `{url}` uses unsupported scheme `{scheme}` (expected `http://`)")]
    UnsupportedScheme {
        /// Sanitized target URL.
        url: String,
        /// Parsed URL scheme.
        scheme: String,
    },
    /// Target URL could not be parsed.
    #[error("invalid target URL `{url}`: {reason}")]
    InvalidTargetUrl {
        /// Sanitized target URL.
        url: String,
        /// URL parse error detail.
        reason: String,
    },
    /// Driver transport is a stub scheduled for a later milestone work package.
    #[error("{transport} conformance driver is unsupported in WP-M0-09: {detail}")]
    UnsupportedTransport {
        /// Transport identifier (`"grpc"` or `"stdio"`).
        transport: &'static str,
        /// Owning work package / explanation.
        detail: &'static str,
    },
    /// Configured bearer token contained invalid HTTP header bytes (secret value is never included).
    #[error("principal bearer token contains invalid HTTP header bytes")]
    InvalidBearerHeader,
    /// HTTP client builder failed.
    #[error("failed to initialize HTTP client: {0}")]
    ClientInit(String),
    /// Network / HTTP transport failure while executing a request.
    #[error("HTTP transport error calling `{target}`: {reason}")]
    Transport {
        /// Sanitized target URL.
        target: String,
        /// Sanitized transport error reason.
        reason: String,
    },
    /// Server advertised a JSON media type (or returned non-object JSON) that violated the
    /// GraphQL-over-HTTP response envelope.
    #[error("malformed JSON response from `{target}` (HTTP {status}): {reason}")]
    InvalidResponseBody {
        /// Sanitized target URL.
        target: String,
        /// HTTP status code.
        status: u16,
        /// Parse/type error detail.
        reason: String,
    },
}

/// Object-safe async transport driver used by the conformance runner (`ARC-007`, `VER-011`).
pub trait ConformanceDriver: Send + Sync + fmt::Debug {
    /// Target endpoint description recorded in the conformance report.
    fn target(&self) -> &str;

    /// Executes `request` with an optional principal `bearer_token` (`INV-15`).
    fn execute<'a>(
        &'a self,
        request: &'a FixtureRequest,
        bearer_token: Option<&'a SecretString>,
    ) -> BoxFuture<'a, Result<ObservedResponse, DriverError>>;
}

/// Live GraphQL-over-HTTP conformance driver (`GQL-003`, `VER-011`).
#[derive(Debug, Clone)]
pub struct HttpDriver {
    raw_target: String,
    parsed_url: reqwest::Url,
    client: reqwest::Client,
}

impl HttpDriver {
    /// Constructs an [`HttpDriver`] targeting `target_url` **exactly as given** without appending
    /// any path segment.
    ///
    /// # Errors
    ///
    /// - Returns [`DriverError::HttpsUnsupported`] immediately (offline, without touching the
    ///   network) if `target_url` uses `https://` because no TLS backend is compiled in.
    /// - Returns [`DriverError::UnsupportedScheme`] for schemes other than `http://`.
    /// - Returns [`DriverError::InvalidTargetUrl`] if `target_url` is not a valid URL.
    pub fn new(target_url: impl Into<String>) -> Result<Self, DriverError> {
        let raw = target_url.into();
        let parsed = reqwest::Url::parse(&raw).map_err(|err| DriverError::InvalidTargetUrl {
            url: redact_raw_url_userinfo(&raw),
            reason: err.to_string(),
        })?;

        let sanitized_target = sanitize_parsed_url(&raw, &parsed);
        match parsed.scheme() {
            "http" => {}
            "https" => {
                return Err(DriverError::HttpsUnsupported {
                    url: sanitized_target,
                });
            }
            other => {
                return Err(DriverError::UnsupportedScheme {
                    url: sanitized_target,
                    scheme: other.to_owned(),
                });
            }
        }

        let client = reqwest::Client::builder()
            .no_proxy()
            .build()
            .map_err(|err| DriverError::ClientInit(err.to_string()))?;

        Ok(Self {
            raw_target: sanitized_target,
            parsed_url: parsed,
            client,
        })
    }

    /// Builds the outbound [`reqwest::Request`] for `fixture_req` and optional `bearer_token`.
    ///
    /// Exposed for deterministic offline verification of method, exact URL, headers, and JSON body.
    ///
    /// # Errors
    ///
    /// Returns [`DriverError`] if the bearer token cannot be encoded as a sensitive HTTP header or
    /// request serialization fails.
    pub fn build_http_request(
        &self,
        fixture_req: &FixtureRequest,
        bearer_token: Option<&SecretString>,
    ) -> Result<reqwest::Request, DriverError> {
        let mut builder = self
            .client
            .post(self.parsed_url.clone())
            .header(ACCEPT, GRAPHQL_ACCEPT_HEADER)
            .header(CONTENT_TYPE, "application/json")
            .json(fixture_req);

        if let Some(secret) = bearer_token {
            let mut auth_val = HeaderValue::from_str(&format!("Bearer {}", secret.expose_secret()))
                .map_err(|_| DriverError::InvalidBearerHeader)?;
            auth_val.set_sensitive(true);
            builder = builder.header(AUTHORIZATION, auth_val);
        }

        builder.build().map_err(|err| DriverError::Transport {
            target: self.raw_target.clone(),
            reason: scrub_optional_secret(&format_error_chain(&err), bearer_token),
        })
    }
}

impl ConformanceDriver for HttpDriver {
    fn target(&self) -> &str {
        &self.raw_target
    }

    fn execute<'a>(
        &'a self,
        request: &'a FixtureRequest,
        bearer_token: Option<&'a SecretString>,
    ) -> BoxFuture<'a, Result<ObservedResponse, DriverError>> {
        Box::pin(async move {
            let http_req = self.build_http_request(request, bearer_token)?;
            tracing::debug!(
                target_url = %self.raw_target,
                has_bearer = bearer_token.is_some(),
                "sending GAP-QL conformance HTTP request"
            );

            let resp =
                self.client
                    .execute(http_req)
                    .await
                    .map_err(|err| DriverError::Transport {
                        target: self.raw_target.clone(),
                        reason: scrub_optional_secret(&format_error_chain(&err), bearer_token),
                    })?;

            let status = resp.status().as_u16();
            let mut raw_headers: Vec<(String, String)> = Vec::with_capacity(resp.headers().len());
            for (name, val) in resp.headers() {
                let val_str = match val.to_str() {
                    Ok(s) => s.to_owned(),
                    Err(_) => String::from_utf8_lossy(val.as_bytes()).into_owned(),
                };
                raw_headers.push((name.as_str().to_owned(), val_str));
            }

            let body_bytes = resp.bytes().await.map_err(|err| DriverError::Transport {
                target: self.raw_target.clone(),
                reason: scrub_optional_secret(&format_error_chain(&err), bearer_token),
            })?;

            decode_http_response(&self.raw_target, status, &raw_headers, &body_bytes)
        })
    }
}

/// Decodes raw HTTP status, headers, and body bytes into an [`ObservedResponse`].
///
/// # Errors
///
/// Returns [`DriverError::InvalidResponseBody`] if the response body is non-empty and either parses
/// as non-object JSON or fails JSON parsing when `Content-Type` advertises JSON.
pub(crate) fn decode_http_response(
    target: &str,
    status: u16,
    raw_headers: &[(String, String)],
    body_bytes: &[u8],
) -> Result<ObservedResponse, DriverError> {
    if body_bytes.is_empty() {
        return Ok(ObservedResponse::from_parts(
            status,
            raw_headers.iter().map(|(k, v)| (k.as_str(), v.as_str())),
            None,
        ));
    }

    let advertises_json = raw_headers.iter().any(|(k, v)| {
        k.eq_ignore_ascii_case("content-type") && v.to_ascii_lowercase().contains("json")
    });

    match serde_json::from_slice::<Value>(body_bytes) {
        Ok(Value::Object(map)) => Ok(ObservedResponse::from_parts(
            status,
            raw_headers.iter().map(|(k, v)| (k.as_str(), v.as_str())),
            Some(&map),
        )),
        Ok(_) => Err(DriverError::InvalidResponseBody {
            target: target.to_owned(),
            status,
            reason: "GraphQL response body must be a JSON object".to_owned(),
        }),
        Err(err) if advertises_json => Err(DriverError::InvalidResponseBody {
            target: target.to_owned(),
            status,
            reason: err.to_string(),
        }),
        Err(_) => Ok(ObservedResponse::from_parts(
            status,
            raw_headers.iter().map(|(k, v)| (k.as_str(), v.as_str())),
            None,
        )),
    }
}

fn sanitize_parsed_url(raw: &str, parsed: &reqwest::Url) -> String {
    if parsed.username().is_empty() && parsed.password().is_none() {
        return raw.to_owned();
    }
    let mut cleaned = parsed.clone();
    let _ = cleaned.set_username("");
    let _ = cleaned.set_password(None);
    cleaned.to_string()
}

fn redact_raw_url_userinfo(raw: &str) -> String {
    if let Some((scheme, rest)) = raw.split_once("://")
        && let Some((_, host_and_path)) = rest.split_once('@')
    {
        return format!("{scheme}://{REDACTED_MARKER}@{host_and_path}");
    }
    raw.to_owned()
}

fn scrub_optional_secret(text: &str, secret: Option<&SecretString>) -> String {
    if let Some(s) = secret {
        let raw = s.expose_secret();
        if !raw.is_empty() && text.contains(raw) {
            return text.replace(raw, REDACTED_MARKER);
        }
    }
    text.to_owned()
}

fn format_error_chain(err: &dyn std::error::Error) -> String {
    let mut parts = vec![err.to_string()];
    let mut cursor = err.source();
    while let Some(cause) = cursor {
        parts.push(cause.to_string());
        cursor = cause.source();
    }
    parts.join(": ")
}

/// Stub gRPC conformance driver (`NGC-001`; implemented in later milestone work packages).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrpcDriver {
    target: String,
}

impl GrpcDriver {
    /// Creates a stub [`GrpcDriver`] for `target`.
    #[must_use]
    pub fn new(target: impl Into<String>) -> Self {
        Self {
            target: target.into(),
        }
    }
}

impl ConformanceDriver for GrpcDriver {
    fn target(&self) -> &str {
        &self.target
    }

    fn execute<'a>(
        &'a self,
        _request: &'a FixtureRequest,
        _bearer_token: Option<&'a SecretString>,
    ) -> BoxFuture<'a, Result<ObservedResponse, DriverError>> {
        Box::pin(async {
            Err(DriverError::UnsupportedTransport {
                transport: "grpc",
                detail: "gRPC gateway/connector conformance transport arrives in M1/M3",
            })
        })
    }
}

/// Stub stdio conformance driver (`NGC-003`; implemented in later milestone work packages).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StdioDriver {
    target: String,
}

impl StdioDriver {
    /// Creates a stub [`StdioDriver`] for `target`.
    #[must_use]
    pub fn new(target: impl Into<String>) -> Self {
        Self {
            target: target.into(),
        }
    }
}

impl ConformanceDriver for StdioDriver {
    fn target(&self) -> &str {
        &self.target
    }

    fn execute<'a>(
        &'a self,
        _request: &'a FixtureRequest,
        _bearer_token: Option<&'a SecretString>,
    ) -> BoxFuture<'a, Result<ObservedResponse, DriverError>> {
        Box::pin(async {
            Err(DriverError::UnsupportedTransport {
                transport: "stdio",
                detail: "stdio conformance transport arrives in M1",
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use serde_json::json;
    use tempfile::TempDir;

    use super::*;

    // spec: GQL-003, INV-15 (VER-011)
    #[test]
    fn https_target_fails_offline_with_clear_typed_no_tls_error() {
        let err = HttpDriver::new("https://api.example.test:8443/gap")
            .expect_err("https:// target must fail without TLS backend");
        assert!(
            matches!(&err, DriverError::HttpsUnsupported { url } if url == "https://api.example.test:8443/gap")
        );
        let msg = err.to_string();
        assert!(msg.contains("https://"));
        assert!(msg.contains("TLS"));

        // Uppercase scheme `HTTPS://` must also hit `HttpsUnsupported`.
        let upper_err = HttpDriver::new("HTTPS://api.example.test/gap")
            .expect_err("HTTPS:// target must fail without TLS backend");
        assert!(matches!(upper_err, DriverError::HttpsUnsupported { .. }));
    }

    // spec: GQL-003, INV-15 (VER-011)
    #[test]
    fn rejects_non_http_schemes_malformed_urls_and_redacts_url_userinfo() {
        let ws_err = HttpDriver::new("ws://127.0.0.1:8080/graphql")
            .expect_err("ws:// scheme must be rejected");
        assert!(matches!(
            ws_err,
            DriverError::UnsupportedScheme { ref scheme, .. } if scheme == "ws"
        ));

        let bad_err =
            HttpDriver::new("not a valid url").expect_err("malformed URL must be rejected");
        assert!(matches!(bad_err, DriverError::InvalidTargetUrl { .. }));

        // Userinfo credentials embedded in a rejected URL must never leak into the error string.
        let secret_pw = "pw-canary-url-999111";
        let leak_err = HttpDriver::new(format!("https://admin:{secret_pw}@api.example.test/gap"))
            .expect_err("https URL with userinfo must fail");
        assert!(!leak_err.to_string().contains(secret_pw));
        assert!(!format!("{leak_err:?}").contains(secret_pw));
    }

    // spec: GQL-003, INV-15 (VER-011)
    #[test]
    fn http_driver_builds_post_to_exact_target_url_with_accept_json_body_and_sensitive_bearer() {
        let driver =
            HttpDriver::new("http://127.0.0.1:19090/custom/v1?x=1").expect("valid http URL");
        assert_eq!(driver.target(), "http://127.0.0.1:19090/custom/v1?x=1");

        let fixture_req = FixtureRequest {
            query: "query Get($id: ID!) { item(id: $id) { id } }".to_owned(),
            variables: Some(json!({"id": "42"})),
            operation_name: Some("Get".to_owned()),
            extensions: Some(json!({"gap": {"explain": true}})),
        };
        let secret = SecretString::new("tok-canary-778899".to_owned());
        let http_req = driver
            .build_http_request(&fixture_req, Some(&secret))
            .expect("build reqwest::Request");

        assert_eq!(http_req.method(), reqwest::Method::POST);
        assert_eq!(
            http_req.url().as_str(),
            "http://127.0.0.1:19090/custom/v1?x=1"
        );
        assert_eq!(
            http_req.headers().get(ACCEPT).and_then(|v| v.to_str().ok()),
            Some("application/graphql-response+json, application/json;q=0.9")
        );
        assert_eq!(
            http_req
                .headers()
                .get(CONTENT_TYPE)
                .and_then(|v| v.to_str().ok()),
            Some("application/json")
        );

        let auth_hdr = http_req
            .headers()
            .get(AUTHORIZATION)
            .expect("Authorization header must be set");
        assert!(
            auth_hdr.is_sensitive(),
            "Authorization header must be marked sensitive"
        );
        assert_eq!(auth_hdr.to_str().ok(), Some("Bearer tok-canary-778899"));
        // Debug formatting of `http_req` must never reveal the sensitive bearer token!
        assert!(!format!("{http_req:?}").contains("tok-canary-778899"));

        let body_bytes = http_req
            .body()
            .and_then(reqwest::Body::as_bytes)
            .expect("in-memory JSON body");
        let parsed_body: Value = serde_json::from_slice(body_bytes).expect("valid JSON body");
        assert_eq!(
            parsed_body,
            json!({
                "query": "query Get($id: ID!) { item(id: $id) { id } }",
                "variables": {"id": "42"},
                "operationName": "Get",
                "extensions": {"gap": {"explain": true}}
            })
        );

        // Minimal request without optional fields omits `variables`, `operationName`, `extensions`
        // and omits `Authorization` when `bearer_token` is `None`.
        let minimal_req = FixtureRequest {
            query: "{ ok }".to_owned(),
            variables: None,
            operation_name: None,
            extensions: None,
        };
        let built_min = driver
            .build_http_request(&minimal_req, None)
            .expect("minimal request");
        assert!(built_min.headers().get(AUTHORIZATION).is_none());
        let min_body: Value = serde_json::from_slice(
            built_min
                .body()
                .and_then(reqwest::Body::as_bytes)
                .expect("body"),
        )
        .expect("json");
        assert_eq!(min_body, json!({"query": "{ ok }"}));
    }

    // spec: ARC-007, NGC-091 (VER-011)
    #[tokio::test]
    async fn grpc_and_stdio_drivers_are_dyn_compatible_and_return_typed_unsupported_error() {
        let drivers: Vec<Box<dyn ConformanceDriver>> = vec![
            Box::new(GrpcDriver::new("http://127.0.0.1:50051")),
            Box::new(StdioDriver::new("gap-ngc-cli")),
        ];
        let req = FixtureRequest {
            query: "{ ok }".to_owned(),
            variables: None,
            operation_name: None,
            extensions: None,
        };

        let grpc_err = drivers[0]
            .execute(&req, None)
            .await
            .expect_err("gRPC stub must return UnsupportedTransport");
        assert!(matches!(
            grpc_err,
            DriverError::UnsupportedTransport {
                transport: "grpc",
                ..
            }
        ));

        let stdio_err = drivers[1]
            .execute(&req, None)
            .await
            .expect_err("stdio stub must return UnsupportedTransport");
        assert!(matches!(
            stdio_err,
            DriverError::UnsupportedTransport {
                transport: "stdio",
                ..
            }
        ));
    }

    // spec: INV-15, SEC-040, SEC-041 (VER-011)
    #[test]
    fn inv_15_principals_map_loads_secrets_redacts_debug_and_never_leaks_on_error() {
        let tmp = TempDir::new().expect("tempdir");
        let canary = "tok-super-secret-canary-424242";
        let ok_path = tmp.path().join("principals.json");
        fs::write(
            &ok_path,
            json!({
                "agent-readonly": canary,
                "agent-admin": "tok-admin-999"
            })
            .to_string(),
        )
        .expect("write principals.json");

        let map = PrincipalsMap::load_file(&ok_path).expect("load valid principals");
        assert_eq!(map.len(), 2);
        assert_eq!(
            map.get("agent-readonly")
                .map(SecretString::expose_secret)
                .map(String::as_str),
            Some(canary)
        );
        assert!(map.get("missing-principal").is_none());

        let dbg = format!("{map:?}");
        assert!(!dbg.contains(canary));
        assert!(dbg.contains(REDACTED_MARKER));

        let scrubbed = map.scrub_text(&format!("upstream said Bearer {canary} failed"));
        assert!(!scrubbed.contains(canary));
        assert_eq!(scrubbed, "upstream said Bearer *** failed");

        // Malformed file containing a raw secret string must NEVER echo the secret in the error.
        let bad_json_path = tmp.path().join("bad_principals.json");
        fs::write(&bad_json_path, format!("{{\"agent\": \"{canary}\", broken"))
            .expect("write broken json");
        let err1 = PrincipalsMap::load_file(&bad_json_path).expect_err("broken json must fail");
        assert!(!err1.to_string().contains(canary));
        assert!(!format!("{err1:?}").contains(canary));

        // Non-object root containing the secret string must NEVER echo the secret in the error.
        let scalar_root_path = tmp.path().join("scalar_principals.json");
        fs::write(&scalar_root_path, format!("\"{canary}\"")).expect("write scalar json");
        let err2 = PrincipalsMap::load_file(&scalar_root_path).expect_err("scalar root must fail");
        assert!(!err2.to_string().contains(canary));
        assert!(!format!("{err2:?}").contains(canary));

        // Empty token value must be rejected naming only the principal key.
        let empty_tok_path = tmp.path().join("empty_tok.json");
        fs::write(&empty_tok_path, json!({"agent-empty": "  "}).to_string())
            .expect("write empty token json");
        let err3 = PrincipalsMap::load_file(&empty_tok_path).expect_err("empty token must fail");
        assert!(matches!(
            err3.kind,
            PrincipalsErrorKind::InvalidEntry { ref principal } if principal == "agent-empty"
        ));
    }

    // spec: GQL-003, INV-15 (VER-011)
    #[test]
    fn decode_http_response_and_invalid_bearer_header_handle_edge_cases_safely() {
        let ok_obs = decode_http_response(
            "http://127.0.0.1:8080",
            200,
            &[(
                "Content-Type".to_owned(),
                "application/graphql-response+json".to_owned(),
            )],
            br#"{"data":{"ok":true},"dataNull":true}"#,
        )
        .expect("valid GraphQL JSON object");
        assert_eq!(ok_obs.status, 200);
        assert!(!ok_obs.data_null);
        assert_eq!(ok_obs.data, json!({"ok": true}));

        // Empty 401 response produces default envelope (`data: null`, `dataNull: true`).
        let empty_401 = decode_http_response("http://127.0.0.1:8080", 401, &[], b"")
            .expect("empty 401 response");
        assert_eq!(empty_401.status, 401);
        assert!(empty_401.data_null);

        // Plain-text 503 transport refusal produces default envelope.
        let text_503 = decode_http_response(
            "http://127.0.0.1:8080",
            503,
            &[("Content-Type".to_owned(), "text/plain".to_owned())],
            b"Service Unavailable",
        )
        .expect("text/plain 503 response");
        assert_eq!(text_503.status, 503);
        assert!(text_503.data_null);

        // Malformed JSON when `Content-Type` advertises JSON fails with `InvalidResponseBody`.
        let bad_json = decode_http_response(
            "http://127.0.0.1:8080",
            200,
            &[(
                "Content-Type".to_owned(),
                "application/graphql-response+json".to_owned(),
            )],
            b"{broken",
        )
        .expect_err("corrupt JSON with JSON content-type must fail");
        assert!(matches!(
            bad_json,
            DriverError::InvalidResponseBody { status: 200, .. }
        ));

        // Non-object JSON array root fails with `InvalidResponseBody`.
        let arr_json = decode_http_response(
            "http://127.0.0.1:8080",
            200,
            &[("Content-Type".to_owned(), "application/json".to_owned())],
            b"[1,2,3]",
        )
        .expect_err("array root must fail");
        assert!(matches!(arr_json, DriverError::InvalidResponseBody { .. }));

        // Bearer token containing CR/LF fails with `InvalidBearerHeader` and never echoes the token.
        let driver = HttpDriver::new("http://127.0.0.1:8080").expect("driver");
        let bad_secret = SecretString::new("canary-crlf-token\r\ninjected: 1".to_owned());
        let req = FixtureRequest {
            query: "{ ok }".to_owned(),
            variables: None,
            operation_name: None,
            extensions: None,
        };
        let hdr_err = driver
            .build_http_request(&req, Some(&bad_secret))
            .expect_err("CRLF in bearer token must fail");
        assert_eq!(hdr_err, DriverError::InvalidBearerHeader);
        assert!(!hdr_err.to_string().contains("canary-crlf-token"));
    }
}
