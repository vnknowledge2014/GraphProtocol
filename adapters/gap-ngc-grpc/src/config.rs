//! Configuration, trust-tier classification policy, deadlines, and credential handling for `gap-ngc-grpc` (`spec/09-compat.md` §5.3, `NGC-025`, `SEC-011`, `INV-15`).

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::PathBuf;
use std::time::Duration;

use gap_core::error::{NgcErrorCode, NgcErrorInfo};
use gap_core::secret::{REDACTED_MARKER, Secret};
use serde::{Deserialize, Serialize};
use tonic::metadata::{Ascii, Binary, MetadataKey, MetadataMap, MetadataValue};
use url::{Host, Url};

/// Trust tier controlling whether `MethodOptions.idempotency_level` may classify unary RPCs as `QUERY` (`ADR-015`, `spec/09-compat.md` §5.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrustTier {
    /// Operator-managed internal connector.
    T0,
    /// Admin-reviewed connector.
    T1,
    /// Untrusted connector (default): hints cannot elevate RPCs to read-only `QUERY`.
    #[default]
    T2,
}

impl TrustTier {
    /// Returns `true` for `T0` and `T1` branches where admin policy may accept `idempotency_level`.
    #[must_use]
    pub const fn is_admin_trusted(self) -> bool {
        matches!(self, Self::T0 | Self::T1)
    }
}

/// Admin policy governing how unary gRPC RPCs are classified (`spec/09-compat.md` §5.3, `ADR-015`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AdminClassificationPolicy {
    /// When `true` on a `T0`/`T1` branch, unary RPCs with protobuf
    /// `idempotency_level = NO_SIDE_EFFECTS` or `IDEMPOTENT` map to `OpKind::Query`.
    pub accept_idempotency_options: bool,
    /// Explicit allow-list of operation names or full gRPC paths (`"pkg.Service/Method"`)
    /// declared read-only (`OpKind::Query`) by a T0/T1 admin.
    pub approved_query_methods: BTreeSet<String>,
    /// Admin-declared side-effect hints overriding untrusted proto defaults per operation.
    pub approved_side_effects: BTreeMap<String, DeclaredSideEffect>,
}

/// Admin-approved side-effect classification for a gRPC operation (`NGC-012`, `ADR-015`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeclaredSideEffect {
    None,
    Reversible,
    Irreversible,
}

/// Source of Protobuf descriptors driving `Describe` and dynamic invocation (`spec/09-compat.md` §5.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DescriptorSource {
    /// Pre-compiled binary `google.protobuf.FileDescriptorSet` protobuf bytes.
    FileDescriptorSet(Vec<u8>),
    /// `.proto` file paths resolved against `include_dirs` via `protox`.
    ProtoFiles {
        files: Vec<PathBuf>,
        include_dirs: Vec<PathBuf>,
    },
    /// In-memory `.proto` definitions `(virtual_file_name, proto_source)` compiled via `protox`.
    InlineProtos(Vec<(String, String)>),
    /// Live gRPC Server Reflection (`grpc.reflection.v1.ServerReflection` with `v1alpha` fallback).
    ServerReflection {
        /// Optional service filter; empty means discover all advertised non-reflection services.
        services: Vec<String>,
    },
}

/// Outbound gRPC metadata credential attached ONLY from connector configuration (`SEC-011`, `INV-15`).
///
/// `Debug` permanently masks secret material with [`REDACTED_MARKER`]. Caller bearer tokens
/// carried in incoming [`gap_ngc_sdk::connector::CallScope::metadata`] are never forwarded.
#[derive(Clone)]
pub enum GrpcCredential {
    /// Sends `authorization: Bearer <token>`.
    Bearer { token: Secret<String> },
    /// Sends a custom ASCII metadata entry (`<key>: <value>`).
    AsciiHeader { key: String, value: Secret<String> },
    /// Sends a custom binary metadata entry (`<key>-bin: <bytes>`).
    BinaryHeader { key: String, value: Secret<Vec<u8>> },
}

impl fmt::Debug for GrpcCredential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Bearer { .. } => f
                .debug_struct("Bearer")
                .field("token", &REDACTED_MARKER)
                .finish(),
            Self::AsciiHeader { key, .. } => f
                .debug_struct("AsciiHeader")
                .field("key", key)
                .field("value", &REDACTED_MARKER)
                .finish(),
            Self::BinaryHeader { key, .. } => f
                .debug_struct("BinaryHeader")
                .field("key", key)
                .field("value", &REDACTED_MARKER)
                .finish(),
        }
    }
}

/// Validates and attaches configured credentials onto an outbound tonic [`MetadataMap`] (`INV-15`).
///
/// # Errors
///
/// Returns `INVALID_ARGUMENT` if a configured key or secret bytes are malformed for gRPC metadata
/// without echoing the secret material in the error message.
pub fn attach_credentials(
    credentials: &[GrpcCredential],
    endpoint: &str,
    allow_insecure_non_loopback: bool,
    meta: &mut MetadataMap,
) -> Result<(), NgcErrorInfo> {
    if !credentials.is_empty() {
        ensure_credential_transport_safe(endpoint, allow_insecure_non_loopback)?;
    }

    for cred in credentials {
        match cred {
            GrpcCredential::Bearer { token } => {
                let header_val = format!("Bearer {}", token.expose_secret());
                let parsed =
                    MetadataValue::<Ascii>::try_from(header_val.as_str()).map_err(|_| {
                        NgcErrorInfo::new(
                            NgcErrorCode::InvalidArgument,
                            "configured bearer token contains non-ASCII metadata bytes",
                        )
                    })?;
                meta.insert("authorization", parsed);
            }
            GrpcCredential::AsciiHeader { key, value } => {
                let lower = key.to_ascii_lowercase();
                if lower.ends_with("-bin") {
                    return Err(NgcErrorInfo::new(
                        NgcErrorCode::InvalidArgument,
                        format!("ASCII credential metadata key `{lower}` must not end with `-bin`"),
                    ));
                }
                let parsed_key =
                    MetadataKey::<Ascii>::from_bytes(lower.as_bytes()).map_err(|_| {
                        NgcErrorInfo::new(
                            NgcErrorCode::InvalidArgument,
                            format!("invalid ASCII metadata key `{lower}`"),
                        )
                    })?;
                let parsed_val = MetadataValue::<Ascii>::try_from(value.expose_secret().as_str())
                    .map_err(|_| {
                    NgcErrorInfo::new(
                        NgcErrorCode::InvalidArgument,
                        format!(
                            "secret value for metadata key `{lower}` contains invalid ASCII bytes"
                        ),
                    )
                })?;
                meta.insert(parsed_key, parsed_val);
            }
            GrpcCredential::BinaryHeader { key, value } => {
                let lower = key.to_ascii_lowercase();
                if !lower.ends_with("-bin") {
                    return Err(NgcErrorInfo::new(
                        NgcErrorCode::InvalidArgument,
                        format!("binary credential metadata key `{lower}` must end with `-bin`"),
                    ));
                }
                let parsed_key =
                    MetadataKey::<Binary>::from_bytes(lower.as_bytes()).map_err(|_| {
                        NgcErrorInfo::new(
                            NgcErrorCode::InvalidArgument,
                            format!("invalid binary metadata key `{lower}`"),
                        )
                    })?;
                let parsed_val = MetadataValue::from_bytes(value.expose_secret());
                meta.insert_bin(parsed_key, parsed_val);
            }
        }
    }
    Ok(())
}

/// Validates that configured endpoint is well-formed and refuses sending credentials over
/// cleartext `http://` to non-loopback targets unless `allow_insecure_non_loopback` is set (`SEC-011`).
///
/// # Errors
///
/// Returns `INVALID_ARGUMENT` / `FAILED_PRECONDITION` when endpoint is invalid or insecure.
pub fn ensure_credential_transport_safe(
    endpoint: &str,
    allow_insecure_non_loopback: bool,
) -> Result<(), NgcErrorInfo> {
    let url = Url::parse(endpoint).map_err(|err| {
        NgcErrorInfo::new(
            NgcErrorCode::InvalidArgument,
            format!("invalid gRPC endpoint URL: {err}"),
        )
    })?;
    match url.scheme() {
        "https" => Ok(()),
        "http" => {
            if allow_insecure_non_loopback || is_loopback_host(url.host()) {
                Ok(())
            } else {
                Err(NgcErrorInfo::new(
                    NgcErrorCode::FailedPrecondition,
                    "refusing to send gRPC credentials over cleartext http to non-loopback host without allow_insecure_non_loopback",
                ))
            }
        }
        other => Err(NgcErrorInfo::new(
            NgcErrorCode::InvalidArgument,
            format!("unsupported gRPC endpoint scheme `{other}`"),
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

/// Complete configuration for [`crate::GrpcConnector`] (`spec/09-compat.md` §5.3).
#[derive(Debug, Clone)]
pub struct GrpcConfig {
    /// Connector identifier advertised in `ConnectorDescription.connector_id`.
    pub connector_id: String,
    /// Connector version advertised in `ConnectorDescription.connector_version`.
    pub connector_version: String,
    /// Target gRPC endpoint URL (e.g., `http://127.0.0.1:50051`).
    pub endpoint: String,
    /// Source of Protobuf descriptors (`FileDescriptorSet`, `.proto` files, inline `.proto`, or Server Reflection).
    pub source: DescriptorSource,
    /// Branch trust tier (defaults to [`TrustTier::T2`]).
    pub trust_tier: TrustTier,
    /// Admin policy for classifying unary RPCs as `QUERY` (`spec/09-compat.md` §5.3).
    pub admin_policy: AdminClassificationPolicy,
    /// Static metadata credentials configured on this connector (never bearer passthrough).
    pub credentials: Vec<GrpcCredential>,
    /// Permit sending credentials over non-loopback `http://` (tests/dev only).
    pub allow_insecure_non_loopback: bool,
    /// Default per-RPC timeout in milliseconds (`NGC-025`).
    pub default_timeout_ms: u32,
    /// Optional per-method deadline overrides keyed by operation name or `"pkg.Service/Method"`.
    pub method_deadlines: BTreeMap<String, Duration>,
    /// Maximum distinct `VariableSet`s per emulated `foreach` wave (`NGC-020`).
    pub max_batch_size: u32,
    /// Maximum concurrent upstream gRPC calls (`NGC-071`, `NGC-096`).
    pub max_in_flight: u32,
    /// Maximum page size advertised in `Limits`.
    pub max_page_size: u32,
    /// Maximum encoded response payload bytes (`Limits.max_response_bytes`).
    pub max_response_bytes: u64,
    /// Allow `MutateRequest.dry_run = true` simulation without invoking upstream mutating RPCs.
    pub supports_dry_run: bool,
}

impl Default for GrpcConfig {
    fn default() -> Self {
        Self {
            connector_id: "gap-ngc-grpc".to_owned(),
            connector_version: env!("CARGO_PKG_VERSION").to_owned(),
            endpoint: "http://127.0.0.1:50051".to_owned(),
            source: DescriptorSource::InlineProtos(Vec::new()),
            trust_tier: TrustTier::T2,
            admin_policy: AdminClassificationPolicy::default(),
            credentials: Vec::new(),
            allow_insecure_non_loopback: false,
            default_timeout_ms: 10_000,
            method_deadlines: BTreeMap::new(),
            max_batch_size: 64,
            max_in_flight: 8,
            max_page_size: 256,
            max_response_bytes: 4 * 1024 * 1024,
            supports_dry_run: true,
        }
    }
}
