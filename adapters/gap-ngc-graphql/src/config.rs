//! Configuration, credentials, and schema source options for `gap-ngc-graphql` (`spec/09-compat.md` §5.4, `SEC-011`, `INV-15`, `ADR-023`).

use std::collections::BTreeMap;
use std::fmt;

use gap_core::error::{NgcErrorCode, NgcErrorInfo};
use gap_core::secret::{REDACTED_MARKER, Secret};
use http::{HeaderName, HeaderValue};
use serde_json::Value;
use url::{Host, Url};

/// Default hard cap on upstream GraphQL HTTP response body bytes (`16 MiB`).
pub const DEFAULT_MAX_UPSTREAM_BODY_BYTES: usize = 16 * 1024 * 1024;

/// Forbidden HTTP headers that static credential config must never override.
const FORBIDDEN_AUTH_HEADERS: &[&str] =
    &["host", "content-length", "transfer-encoding", "connection"];

/// Source of the upstream GraphQL schema (`spec/09-compat.md` §5.4).
#[derive(Debug, Clone)]
pub enum UpstreamSchemaSource {
    /// Upstream GraphQL SDL document.
    Sdl(String),
    /// Upstream GraphQL introspection JSON (`{ "__schema": ... }` or `{ "data": { "__schema": ... } }`).
    IntrospectionJson(Value),
    /// Query the live upstream endpoint with standard GraphQL `__schema` introspection.
    LiveIntrospection,
}

/// Outbound HTTP authentication configured on the connector (`SEC-011`, `INV-15`).
///
/// Caller bearer tokens carried in incoming `CallScope::metadata` are NEVER forwarded.
#[derive(Clone)]
pub enum GraphqlCredential {
    /// Sends `authorization: Bearer <token>`.
    Bearer { token: Secret<String> },
    /// Sends a custom header (`<name>: <value>`).
    Header { name: String, value: Secret<String> },
}

impl fmt::Debug for GraphqlCredential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Bearer { .. } => f
                .debug_struct("Bearer")
                .field("token", &REDACTED_MARKER)
                .finish(),
            Self::Header { name, .. } => f
                .debug_struct("Header")
                .field("name", name)
                .field("value", &REDACTED_MARKER)
                .finish(),
        }
    }
}

/// Admin side-effect classification override for upstream GraphQL mutations (`ADR-015`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MutationSideEffectOverride {
    Reversible,
    Irreversible,
}

/// Entity key mapping enabling `_entities(representations: $reps)` batching for an operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntityBatchMapping {
    /// Upstream GraphQL `__typename` (e.g., `"Product"`).
    pub upstream_typename: String,
    /// Ordered argument/key field names mapped from operation arguments/`foreach` variables into representation objects.
    pub key_fields: Vec<String>,
}

/// Configuration for [`crate::GraphqlConnector`] (`spec/09-compat.md` §5.4, `ADR-023`).
#[derive(Debug, Clone)]
pub struct GraphqlConfig {
    /// Connector identifier advertised in `ConnectorDescription.connector_id`.
    pub connector_id: String,
    /// Connector version advertised in `ConnectorDescription.connector_version`.
    pub connector_version: String,
    /// Branch namespace prefix prepended as `<ns>_<Type>` in generated `schema_sdl` (`spec/09-compat.md` §5.4, `GQL-004`).
    pub branch_namespace: String,
    /// Upstream GraphQL HTTP endpoint URL (`POST`, `application/graphql-response+json`).
    pub endpoint: String,
    /// Schema source driving operation extraction and SDL normalization.
    pub schema_source: UpstreamSchemaSource,
    /// Static connector credentials (never caller bearer passthrough).
    pub credentials: Vec<GraphqlCredential>,
    /// Explicit dev/test override permitting credentials over non-loopback `http://`.
    pub allow_insecure_non_loopback: bool,
    /// Reject mutation execution (`read_only = true` hides/blocks `Mutate`).
    pub read_only: bool,
    /// Prefer Apollo Federation `_entities(representations: $reps)` for `foreach` when upstream advertises it and mapping matches.
    pub prefer_federation_entities: bool,
    /// Optional explicit entity representation mappings keyed by operation name.
    pub entity_mappings: BTreeMap<String, EntityBatchMapping>,
    /// Optional admin overrides for mutation side-effect hints (`ADR-015`).
    pub mutation_side_effects: BTreeMap<String, MutationSideEffectOverride>,
    /// Default request timeout in milliseconds (`NGC-025`).
    pub default_timeout_ms: u32,
    /// Maximum distinct `foreach` keys grouped into one upstream GraphQL request (`NGC-020`).
    pub max_batch_size: u32,
    /// Maximum concurrent upstream HTTP calls (`NGC-071`, `NGC-096`).
    pub max_in_flight: u32,
    /// Maximum Relay / offset page size advertised in `Limits`.
    pub max_page_size: u32,
    /// Maximum encoded NGC response bytes (`Limits.max_response_bytes`).
    pub max_response_bytes: u64,
    /// Maximum raw HTTP response bytes buffered from upstream (`16 MiB` default).
    pub max_upstream_body_bytes: usize,
    /// Allow `MutateRequest.dry_run = true` simulation without issuing upstream GraphQL mutations.
    pub supports_dry_run: bool,
}

impl Default for GraphqlConfig {
    fn default() -> Self {
        Self {
            connector_id: "gap-ngc-graphql".to_owned(),
            connector_version: env!("CARGO_PKG_VERSION").to_owned(),
            branch_namespace: "upstream".to_owned(),
            endpoint: "http://127.0.0.1:4000/graphql".to_owned(),
            schema_source: UpstreamSchemaSource::LiveIntrospection,
            credentials: Vec::new(),
            allow_insecure_non_loopback: false,
            read_only: false,
            prefer_federation_entities: true,
            entity_mappings: BTreeMap::new(),
            mutation_side_effects: BTreeMap::new(),
            default_timeout_ms: 10_000,
            max_batch_size: 64,
            max_in_flight: 8,
            max_page_size: 100,
            max_response_bytes: 4 * 1024 * 1024,
            max_upstream_body_bytes: DEFAULT_MAX_UPSTREAM_BODY_BYTES,
            supports_dry_run: true,
        }
    }
}

/// Validates and parses the configured endpoint URL (`SEC-011`, `INV-15`).
///
/// # Errors
///
/// Returns `INVALID_ARGUMENT` for invalid URL syntax or unsupported scheme, and `FAILED_PRECONDITION`
/// if credentials would travel over cleartext `http://` to a non-loopback host without
/// `allow_insecure_non_loopback`.
pub fn validate_endpoint(
    endpoint: &str,
    has_credentials: bool,
    allow_insecure_non_loopback: bool,
) -> Result<Url, NgcErrorInfo> {
    let url = Url::parse(endpoint).map_err(|err| {
        NgcErrorInfo::new(
            NgcErrorCode::InvalidArgument,
            format!("invalid upstream GraphQL URL: {err}"),
        )
    })?;
    if !url.username().is_empty() || url.password().is_some() || url.fragment().is_some() {
        return Err(NgcErrorInfo::new(
            NgcErrorCode::InvalidArgument,
            "upstream GraphQL endpoint URL must not carry userinfo or fragments",
        ));
    }
    match url.scheme() {
        "https" => Ok(url),
        "http" => {
            if !has_credentials || allow_insecure_non_loopback || is_loopback_host(url.host()) {
                Ok(url)
            } else {
                Err(NgcErrorInfo::new(
                    NgcErrorCode::FailedPrecondition,
                    "refusing to send GraphQL credentials over cleartext http to non-loopback host without allow_insecure_non_loopback",
                ))
            }
        }
        other => Err(NgcErrorInfo::new(
            NgcErrorCode::InvalidArgument,
            format!("unsupported upstream GraphQL URL scheme `{other}`"),
        )),
    }
}

fn is_loopback_host(host: Option<Host<&str>>) -> bool {
    match host {
        Some(Host::Domain(d)) => d.eq_ignore_ascii_case("localhost"),
        Some(Host::Ipv4(ip)) => ip.is_loopback(),
        Some(Host::Ipv6(ip)) => ip.is_loopback(),
        None => false,
    }
}

/// Resolves static [`GraphqlCredential`] entries into validated HTTP header pairs (`INV-15`).
///
/// Error messages never include secret values.
///
/// # Errors
///
/// Returns `INVALID_ARGUMENT` if header names are forbidden/invalid or secret bytes cannot form HTTP header values.
pub fn resolve_auth_headers(
    credentials: &[GraphqlCredential],
    endpoint: &str,
    allow_insecure_non_loopback: bool,
) -> Result<Vec<(HeaderName, HeaderValue)>, NgcErrorInfo> {
    validate_endpoint(
        endpoint,
        !credentials.is_empty(),
        allow_insecure_non_loopback,
    )?;

    let mut headers = Vec::with_capacity(credentials.len());
    for cred in credentials {
        match cred {
            GraphqlCredential::Bearer { token } => {
                let raw = format!("Bearer {}", token.expose_secret());
                let mut val = HeaderValue::from_str(&raw).map_err(|_| {
                    NgcErrorInfo::new(
                        NgcErrorCode::InvalidArgument,
                        "configured GraphQL bearer token contains invalid HTTP header bytes",
                    )
                })?;
                val.set_sensitive(true);
                headers.push((http::header::AUTHORIZATION, val));
            }
            GraphqlCredential::Header { name, value } => {
                let lower = name.to_ascii_lowercase();
                if FORBIDDEN_AUTH_HEADERS.contains(&lower.as_str()) {
                    return Err(NgcErrorInfo::new(
                        NgcErrorCode::InvalidArgument,
                        format!("auth header `{lower}` cannot override transport framing header"),
                    ));
                }
                let hdr_name = HeaderName::from_bytes(lower.as_bytes()).map_err(|_| {
                    NgcErrorInfo::new(
                        NgcErrorCode::InvalidArgument,
                        format!("invalid HTTP header name `{lower}`"),
                    )
                })?;
                let mut hdr_val = HeaderValue::from_str(value.expose_secret()).map_err(|_| {
                    NgcErrorInfo::new(
                        NgcErrorCode::InvalidArgument,
                        format!(
                            "secret value for HTTP header `{lower}` contains invalid header bytes"
                        ),
                    )
                })?;
                hdr_val.set_sensitive(true);
                headers.push((hdr_name, hdr_val));
            }
        }
    }
    Ok(headers)
}
