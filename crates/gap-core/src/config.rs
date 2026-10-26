//! `gap.toml` configuration model, `GAP_*__*` environment overrides, `SecretRef`, and JSON Schema
//! generation (`spec/00-conventions.md` §8, `spec/02-gap-ql.md` §§7–8, `spec/06-runtime.md` §§9–10,
//! `INV-15`).

use std::borrow::Cow;
use std::fmt;
use std::path::Path;
use std::str::FromStr;

use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema, schema_for};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use thiserror::Error;
use toml::Table;

use crate::ids::TenantId;

/// Prefix for environment variable overrides (`GAP_SERVER__LISTEN=...`).
pub const ENV_OVERRIDE_PREFIX: &str = "GAP_";

/// Configuration parse, override, or range-validation failure.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ConfigError {
    /// Failed to read `gap.toml` from disk.
    #[error("failed to read config file `{path}`: {reason}")]
    Io {
        /// File path that failed to load.
        path: String,
        /// Sanitized I/O failure message.
        reason: String,
    },
    /// Invalid TOML syntax or unknown/mistyped key (`#[serde(deny_unknown_fields)]`).
    #[error("invalid configuration document: {0}")]
    Parse(String),
    /// Malformed `GAP_*` environment variable name.
    #[error("invalid environment override variable `{0}`: empty path segment")]
    InvalidEnvKey(String),
    /// Environment override collided with a scalar where a nested table was expected.
    #[error("environment override `{0}` conflicts with a non-table configuration value")]
    EnvPathConflict(String),
    /// Out-of-range limit or security posture violation.
    #[error("configuration validation failed for `{field}`: {reason}")]
    Validation {
        /// Dotted configuration field path.
        field: &'static str,
        /// Violation detail.
        reason: String,
    },
    /// JSON Schema serialization failed.
    #[error("failed to serialize JSON schema: {0}")]
    Schema(String),
}

/// Error returned when parsing a [`SecretRef`] (`INV-15`, `spec/00-conventions.md` §8).
///
/// Never echoes raw unrecognized input strings so accidentally inlined secrets cannot leak into
/// logs or error messages (`INV-15`).
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error(
    "invalid secret reference ({reason}); expected `secret://<vault>/<name>`, `env://<NAME>`, or `file:///<abs-path>` (inline secrets are forbidden by INV-15)"
)]
pub struct SecretRefError {
    /// Redacted structural failure reason.
    pub reason: &'static str,
}

/// External secret reference (`spec/00-conventions.md` §8, `SEC-040`, `INV-15`).
///
/// Secrets in `gap.toml` are NEVER inlined; they are referenced exclusively via URI schemes:
/// - `secret://<provider>/<name>`
/// - `env://<VAR_NAME>`
/// - `file:///<absolute/path>`
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SecretRef {
    /// Secret stored in a named vault provider (`secret://<provider>/<name>`).
    Vault {
        /// Vault backend slug (e.g. `"vault"`, `"keychain"`, `"kms"`).
        provider: String,
        /// Secret path/name within the provider.
        name: String,
    },
    /// Secret read from an environment variable (`env://<VAR_NAME>`).
    Env {
        /// Environment variable identifier (`[A-Z_][A-Z0-9_]*`).
        var: String,
    },
    /// Secret read from an `0600`-permissioned local file (`file:///<abs-path>`).
    File {
        /// Absolute filesystem path starting with `/`.
        path: String,
    },
}

impl fmt::Display for SecretRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Vault { provider, name } => write!(f, "secret://{provider}/{name}"),
            Self::Env { var } => write!(f, "env://{var}"),
            Self::File { path } => write!(f, "file://{path}"),
        }
    }
}

impl FromStr for SecretRef {
    type Err = SecretRefError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.is_empty()
            || s.bytes()
                .any(|b| b.is_ascii_control() || b.is_ascii_whitespace())
        {
            return Err(SecretRefError {
                reason: "empty or contains whitespace/control characters",
            });
        }
        if let Some(rest) = s.strip_prefix("secret://") {
            let Some((provider, name)) = rest.split_once('/') else {
                return Err(SecretRefError {
                    reason: "secret:// URI missing `<provider>/<name>` path",
                });
            };
            if provider.is_empty()
                || !provider
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
            {
                return Err(SecretRefError {
                    reason: "invalid secret:// provider identifier",
                });
            }
            if name.is_empty() || name.starts_with('/') || name.contains("..") {
                return Err(SecretRefError {
                    reason: "invalid secret:// secret name",
                });
            }
            return Ok(Self::Vault {
                provider: provider.to_owned(),
                name: name.to_owned(),
            });
        }
        if let Some(var) = s.strip_prefix("env://") {
            let mut bytes = var.bytes();
            let Some(first) = bytes.next() else {
                return Err(SecretRefError {
                    reason: "env:// URI missing variable name",
                });
            };
            if !(first.is_ascii_uppercase() || first == b'_')
                || !bytes.all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
            {
                return Err(SecretRefError {
                    reason: "env:// variable must match `[A-Z_][A-Z0-9_]*`",
                });
            }
            return Ok(Self::Env {
                var: var.to_owned(),
            });
        }
        if let Some(abs_tail) = s.strip_prefix("file:///") {
            if abs_tail.is_empty() || abs_tail.contains("..") {
                return Err(SecretRefError {
                    reason: "file:/// URI requires a non-empty normalized absolute path",
                });
            }
            return Ok(Self::File {
                path: format!("/{abs_tail}"),
            });
        }
        Err(SecretRefError {
            reason: "unrecognized scheme or inline secret value",
        })
    }
}

impl Serialize for SecretRef {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for SecretRef {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = Cow::<'de, str>::deserialize(deserializer)?;
        raw.parse().map_err(serde::de::Error::custom)
    }
}

impl JsonSchema for SecretRef {
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("SecretRef")
    }

    fn json_schema(_generator: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "type": "string",
            "pattern": "^(secret://[A-Za-z0-9._-]+/[^\\s]+|env://[A-Z_][A-Z0-9_]*|file:///[^\\s]+)$",
            "description": "External secret reference (`secret://<vault>/<name>`, `env://<VAR>`, or `file:///<abs-path>`). Inline secrets are forbidden (INV-15)."
        })
    }
}

/// Deployment profile (`OPS-001`).
#[derive(
    Debug,
    Default,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize,
    JsonSchema,
)]
#[serde(rename_all = "lowercase")]
pub enum Profile {
    /// Local development profile (`operation_mode` defaults to `open`, anonymous allowed if enabled).
    Dev,
    /// Production profile (`operation_mode` defaults to `hybrid`, anonymous forbidden, `OPS-001`).
    #[default]
    Prod,
}

/// Gateway document allow-listing mode (`GQL-060`).
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "lowercase")]
pub enum OperationMode {
    /// Arbitrary queries and mutations permitted (default in `dev`).
    Open,
    /// Arbitrary read queries permitted; mutations/tasks require persisted operations (default in `prod`).
    Hybrid,
    /// Only registered persisted operations permitted.
    Safelist,
}

/// `[server]` listener and graceful-shutdown settings (`OPS-002`, `RUN-090`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct ServerConfig {
    /// Primary HTTP/2 GraphQL, SSE, MCP, and A2A bind address (`OPS-002`).
    pub listen: String,
    /// Separate authenticated `/admin/*` listener (`OPS-002`).
    pub admin_listen: String,
    /// Dedicated gRPC Gateway listener (`OPS-002`).
    pub grpc_listen: String,
    /// Graceful drain window on `SIGTERM` in milliseconds (`RUN-090`, default `30_000`).
    pub shutdown_grace_ms: u64,
    /// Optional TLS certificate reference (`file:///...` or `secret://...`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tls_cert_secret: Option<SecretRef>,
    /// Optional TLS private key reference (`file:///...` or `secret://...`, `INV-15`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tls_key_secret: Option<SecretRef>,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            listen: "127.0.0.1:8080".to_owned(),
            admin_listen: "127.0.0.1:9090".to_owned(),
            grpc_listen: "127.0.0.1:8081".to_owned(),
            shutdown_grace_ms: 30_000,
            tls_cert_secret: None,
            tls_key_secret: None,
        }
    }
}

/// `[gateway]` tenant, operation-mode, and facade settings (`ADR-018`, `GQL-060`, `INV-20`, `SEC-083`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct GatewayConfig {
    /// Default tenant slug when running single-tenant (`ADR-018`).
    pub default_tenant: TenantId,
    /// Explicit operation mode override; defaults to `open` (`dev`) or `hybrid` (`prod`) (`GQL-060`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub operation_mode: Option<OperationMode>,
    /// Maximum peer-to-peer forwarding hops (`INV-20`, default `4`, range `1..=16`).
    pub max_hops: u8,
    /// Wrap `T2` branch results in `<gap:untrusted>` spotlighting tags (`SEC-083`, default `true`).
    pub facade_spotlight: bool,
}

impl Default for GatewayConfig {
    fn default() -> Self {
        Self {
            default_tenant: TenantId::default_tenant(),
            operation_mode: None,
            max_hops: 4,
            facade_spotlight: true,
        }
    }
}

/// `[limits]` query complexity, depth, size, and deadline caps (`spec/02-gap-ql.md` §8, `OPS-020`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct LimitsConfig {
    /// Max selection depth (`1..=32`, default `12`).
    pub max_selection_depth: u32,
    /// Max aliases per document (`1..=500`, default `50`).
    pub max_aliases: u32,
    /// Max root fields (`1..=100`, default `20`).
    pub max_root_fields: u32,
    /// Max planned cost units (`100..=1_000_000`, default `10_000`).
    pub max_cost_units: u32,
    /// Max HTTP/gRPC request body in bytes (`16_384..=16_777_216`, default `1_048_576`).
    pub max_request_body_bytes: u64,
    /// Max serialized response body in bytes (`65_536..=67_108_864`, default `4_194_304`).
    pub max_response_body_bytes: u64,
    /// Default list multiplier when caller omits `first`/`limit` (`1..=1_000`, default `100`).
    pub default_list_size: u32,
    /// Hard cap on `traverse.maxDepth` (`1..=16`, default `16`, `GQL-040`).
    pub traverse_max_depth: u8,
    /// Max variables per document (`1..=1_000`, default `100`).
    pub max_variables: u32,
    /// Max request deadline in milliseconds (`100..=600_000`, default `30_000`).
    pub request_deadline_ms: u64,
    /// Max registered branches for bounded metric cardinality (`1..=10_000`, default `1_000`, `OPS-020`).
    pub max_branches: u32,
}

impl Default for LimitsConfig {
    fn default() -> Self {
        Self {
            max_selection_depth: 12,
            max_aliases: 50,
            max_root_fields: 20,
            max_cost_units: 10_000,
            max_request_body_bytes: 1_048_576,
            max_response_body_bytes: 4_194_304,
            default_list_size: 100,
            traverse_max_depth: 16,
            max_variables: 100,
            request_deadline_ms: 30_000,
            max_branches: 1_000,
        }
    }
}

/// `[auth]` OAuth 2.1 / mTLS / API-key settings (`SEC-001`, `SEC-002`, `INV-15`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct AuthConfig {
    /// Allow unauthenticated callers (`SEC-002`: permitted ONLY when `profile = "dev"`).
    pub allow_anonymous: bool,
    /// Expected OAuth 2.1 token issuer (`iss`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub issuer: Option<String>,
    /// Expected resource indicator / audience (`aud`, RFC 8707).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub audience: Option<String>,
    /// Remote JWKS endpoint URL.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub jwks_uri: Option<String>,
    /// Optional RFC 7662 introspection client secret reference (`INV-15`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub introspection_client_secret: Option<SecretRef>,
    /// Allowed JWT clock skew in seconds (`0..=60`, default `60`, `SEC-001`).
    pub clock_skew_secs: u32,
}

impl Default for AuthConfig {
    fn default() -> Self {
        Self {
            allow_anonymous: false,
            issuer: None,
            audience: None,
            jwks_uri: None,
            introspection_client_secret: None,
            clock_skew_secs: 60,
        }
    }
}

/// `[security]` Cedar policy, four-eyes approval, and process-sandbox settings (`SEC-020`, `SEC-071`, `RUN-071`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct SecurityConfig {
    /// Directory containing `*.cedar` policies and `schema.cedarschema` (`SEC-020`).
    pub policy_dir: String,
    /// Require approver principal != requester principal (`RUN-071`).
    pub four_eyes_approval: bool,
    /// Permit `T1` process connectors when OS sandbox is unavailable (`SEC-071`; `T2` always fails closed).
    pub allow_unsandboxed: bool,
    /// Gateway signing key reference for `ApprovalProof` (`NGC-032`, `INV-15`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub approval_signing_key_secret: Option<SecretRef>,
}

impl Default for SecurityConfig {
    fn default() -> Self {
        Self {
            policy_dir: "policies".to_owned(),
            four_eyes_approval: true,
            allow_unsandboxed: false,
            approval_signing_key_secret: None,
        }
    }
}

/// `[storage]` state store, blob store, and bounded blocking-pool settings (`OPS-003`, `ARC-010`, `ARC-011`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct StorageConfig {
    /// Local SQLite database path (`OPS-003`).
    pub sqlite_path: String,
    /// Local artifact blob directory (`RUN-086`).
    pub blob_dir: String,
    /// Worker thread count for dedicated CPU/blocking pool (`1..=256`, default `4`, `ARC-010`).
    pub blocking_pool_threads: usize,
    /// Bounded blocking-pool queue capacity (`1..=65_536`, default `128`, `ARC-010`, `ARC-011`).
    pub blocking_queue_capacity: usize,
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            sqlite_path: ".gap/state.db".to_owned(),
            blob_dir: ".gap/blobs".to_owned(),
            blocking_pool_threads: 4,
            blocking_queue_capacity: 128,
        }
    }
}

/// `[telemetry]` structured logging and OpenTelemetry settings (`OPS-010`, `spec/08-operations.md` §§2, 4).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct TelemetryConfig {
    /// `tracing-subscriber` `EnvFilter` directive string (default `"info"`).
    pub log_level: String,
    /// Emit structured JSON logs (`true` by default, `spec/08-operations.md` §4).
    pub log_json: bool,
    /// OpenTelemetry `service.name` resource attribute.
    pub service_name: String,
    /// Optional OTLP gRPC collector endpoint (e.g. `"http://127.0.0.1:4317"`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub otlp_endpoint: Option<String>,
    /// Optional OTLP authorization header secret reference (`INV-15`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub otlp_auth_header_secret: Option<SecretRef>,
    /// Parent-based head sampling ratio (`0.0..=1.0`, default `0.10`, `OPS-010`).
    pub head_sample_ratio: f64,
    /// Slow-request threshold in milliseconds (`OPS-010`, default `500`).
    pub slow_ms: u64,
}

impl Default for TelemetryConfig {
    fn default() -> Self {
        Self {
            log_level: "info".to_owned(),
            log_json: true,
            service_name: "gap-server".to_owned(),
            otlp_endpoint: None,
            otlp_auth_header_secret: None,
            head_sample_ratio: 0.10,
            slow_ms: 500,
        }
    }
}

/// Root `gap.toml` configuration (`spec/00-conventions.md` §8).
#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct GapConfig {
    /// Active deployment profile (`dev` or `prod`).
    pub profile: Profile,
    /// Listener and shutdown configuration.
    pub server: ServerConfig,
    /// Gateway routing, tenant, and facade posture.
    pub gateway: GatewayConfig,
    /// Complexity, depth, payload, and deadline limits.
    pub limits: LimitsConfig,
    /// Authentication settings.
    pub auth: AuthConfig,
    /// Policy, approval, and sandbox settings.
    pub security: SecurityConfig,
    /// Persistence and bounded blocking pool settings.
    pub storage: StorageConfig,
    /// Structured logging and OpenTelemetry settings.
    pub telemetry: TelemetryConfig,
}

fn is_bool_leaf(path: &[String]) -> bool {
    matches!(
        (
            path.first().map(String::as_str),
            path.get(1).map(String::as_str)
        ),
        (Some("gateway"), Some("facade_spotlight"))
            | (Some("auth"), Some("allow_anonymous"))
            | (
                Some("security"),
                Some("four_eyes_approval" | "allow_unsandboxed")
            )
            | (Some("telemetry"), Some("log_json"))
    ) && path.len() == 2
}

fn is_int_leaf(path: &[String]) -> bool {
    matches!(
        (
            path.first().map(String::as_str),
            path.get(1).map(String::as_str)
        ),
        (Some("server"), Some("shutdown_grace_ms"))
            | (Some("gateway"), Some("max_hops"))
            | (
                Some("limits"),
                Some(
                    "max_selection_depth"
                        | "max_aliases"
                        | "max_root_fields"
                        | "max_cost_units"
                        | "max_request_body_bytes"
                        | "max_response_body_bytes"
                        | "default_list_size"
                        | "traverse_max_depth"
                        | "max_variables"
                        | "request_deadline_ms"
                        | "max_branches"
                )
            )
            | (Some("auth"), Some("clock_skew_secs"))
            | (
                Some("storage"),
                Some("blocking_pool_threads" | "blocking_queue_capacity")
            )
            | (Some("telemetry"), Some("slow_ms"))
    ) && path.len() == 2
}

fn is_float_leaf(path: &[String]) -> bool {
    path.len() == 2 && path[0] == "telemetry" && path[1] == "head_sample_ratio"
}

fn coerce_env_value(path: &[String], raw: &str) -> toml::Value {
    if is_bool_leaf(path) {
        if let Ok(b) = raw.parse::<bool>() {
            return toml::Value::Boolean(b);
        }
    } else if is_int_leaf(path) {
        if let Ok(n) = raw.parse::<i64>() {
            return toml::Value::Integer(n);
        }
    } else if is_float_leaf(path)
        && let Ok(f) = raw.parse::<f64>()
    {
        return toml::Value::Float(f);
    }
    toml::Value::String(raw.to_owned())
}

fn insert_env_override(
    root: &mut Table,
    env_key: &str,
    raw_value: &str,
) -> Result<(), ConfigError> {
    let Some(stripped) = env_key.strip_prefix(ENV_OVERRIDE_PREFIX) else {
        return Ok(());
    };
    if stripped.is_empty() {
        return Err(ConfigError::InvalidEnvKey(env_key.to_owned()));
    }
    let segments: Vec<String> = stripped
        .split("__")
        .map(|seg| seg.to_ascii_lowercase())
        .collect();
    if segments.iter().any(String::is_empty) {
        return Err(ConfigError::InvalidEnvKey(env_key.to_owned()));
    }

    let mut current = root;
    for seg in &segments[..segments.len() - 1] {
        let entry = current
            .entry(seg.clone())
            .or_insert_with(|| toml::Value::Table(Table::new()));
        let Some(sub_table) = entry.as_table_mut() else {
            return Err(ConfigError::EnvPathConflict(env_key.to_owned()));
        };
        current = sub_table;
    }

    let leaf_value = coerce_env_value(&segments, raw_value);
    if let Some(last) = segments.last() {
        current.insert(last.clone(), leaf_value);
    }
    Ok(())
}

impl GapConfig {
    /// Effective [`OperationMode`] (`GQL-060`: `open` in `dev`, `hybrid` in `prod` when unset).
    #[must_use]
    pub const fn effective_operation_mode(&self) -> OperationMode {
        match self.gateway.operation_mode {
            Some(mode) => mode,
            None => match self.profile {
                Profile::Dev => OperationMode::Open,
                Profile::Prod => OperationMode::Hybrid,
            },
        }
    }

    /// Parses `gap.toml` text without reading process environment variables.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError`] on unknown keys, invalid secret references, or out-of-range limits.
    pub fn from_toml_str(toml_str: &str) -> Result<Self, ConfigError> {
        Self::from_toml_and_env(toml_str, std::iter::empty::<(&str, &str)>())
    }

    /// Parses `gap.toml` text and applies `GAP_*__*` environment variable overrides from `env_vars`.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError`] on unknown keys in either TOML or `GAP_*__*`, invalid `SecretRef`
    /// values (`INV-15`), or range violations (`spec/02-gap-ql.md` §8).
    pub fn from_toml_and_env<I, K, V>(toml_str: &str, env_vars: I) -> Result<Self, ConfigError>
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<str>,
        V: AsRef<str>,
    {
        let mut table: Table = toml_str
            .parse()
            .map_err(|e: toml::de::Error| ConfigError::Parse(e.message().to_owned()))?;
        for (k, v) in env_vars {
            insert_env_override(&mut table, k.as_ref(), v.as_ref())?;
        }
        let cfg = Self::deserialize(table)
            .map_err(|e: toml::de::Error| ConfigError::Parse(e.message().to_owned()))?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// Reads `path` from disk and applies `GAP_*__*` overrides from [`std::env::vars`].
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError`] if reading, parsing, overriding, or validating fails.
    pub fn load_from_path_with_process_env(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let p = path.as_ref();
        let text = std::fs::read_to_string(p).map_err(|e| ConfigError::Io {
            path: p.display().to_string(),
            reason: e.to_string(),
        })?;
        Self::from_toml_and_env(&text, std::env::vars())
    }

    /// Validates cross-field invariants and configurable limit ranges (`spec/02-gap-ql.md` §8,
    /// `SEC-001`, `SEC-002`, `INV-20`, `ARC-010`, `OPS-010`).
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Validation`] on the first out-of-range or unsafe setting.
    pub fn validate(&self) -> Result<(), ConfigError> {
        fn check_range<T: PartialOrd + fmt::Display>(
            field: &'static str,
            val: T,
            min: T,
            max: T,
        ) -> Result<(), ConfigError> {
            if val < min || val > max {
                return Err(ConfigError::Validation {
                    field,
                    reason: format!("value {val} outside allowed range {min}..={max}"),
                });
            }
            Ok(())
        }

        if self.profile == Profile::Prod && self.auth.allow_anonymous {
            return Err(ConfigError::Validation {
                field: "auth.allow_anonymous",
                reason: "anonymous access is forbidden when profile = \"prod\" (SEC-002)"
                    .to_owned(),
            });
        }

        check_range("gateway.max_hops", self.gateway.max_hops, 1, 16)?;
        check_range(
            "limits.max_selection_depth",
            self.limits.max_selection_depth,
            1,
            32,
        )?;
        check_range("limits.max_aliases", self.limits.max_aliases, 1, 500)?;
        check_range(
            "limits.max_root_fields",
            self.limits.max_root_fields,
            1,
            100,
        )?;
        check_range(
            "limits.max_cost_units",
            self.limits.max_cost_units,
            100,
            1_000_000,
        )?;
        check_range(
            "limits.max_request_body_bytes",
            self.limits.max_request_body_bytes,
            16 * 1024,
            16 * 1024 * 1024,
        )?;
        check_range(
            "limits.max_response_body_bytes",
            self.limits.max_response_body_bytes,
            64 * 1024,
            64 * 1024 * 1024,
        )?;
        check_range(
            "limits.default_list_size",
            self.limits.default_list_size,
            1,
            1_000,
        )?;
        check_range(
            "limits.traverse_max_depth",
            self.limits.traverse_max_depth,
            1,
            16,
        )?;
        check_range("limits.max_variables", self.limits.max_variables, 1, 1_000)?;
        check_range(
            "limits.request_deadline_ms",
            self.limits.request_deadline_ms,
            100,
            600_000,
        )?;
        check_range("limits.max_branches", self.limits.max_branches, 1, 10_000)?;
        check_range("auth.clock_skew_secs", self.auth.clock_skew_secs, 0, 60)?;
        check_range(
            "storage.blocking_pool_threads",
            self.storage.blocking_pool_threads,
            1,
            256,
        )?;
        check_range(
            "storage.blocking_queue_capacity",
            self.storage.blocking_queue_capacity,
            1,
            65_536,
        )?;
        if !(0.0..=1.0).contains(&self.telemetry.head_sample_ratio) {
            return Err(ConfigError::Validation {
                field: "telemetry.head_sample_ratio",
                reason: "must be finite and within 0.0..=1.0 (OPS-010)".to_owned(),
            });
        }
        Ok(())
    }

    /// Generates the `schemars` [`Schema`] for `gap.toml`.
    #[must_use]
    pub fn json_schema() -> Schema {
        schema_for!(Self)
    }

    /// Renders `docs/config.schema.json` with deterministic 2-space pretty-printing and a trailing
    /// newline.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Schema`] if JSON serialization fails.
    pub fn json_schema_pretty() -> Result<String, ConfigError> {
        let mut rendered = serde_json::to_string_pretty(&Self::json_schema())
            .map_err(|e| ConfigError::Schema(e.to_string()))?;
        rendered.push('\n');
        Ok(rendered)
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use super::*;

    // spec: INV-15, SEC-040
    #[test]
    fn inv_15_secret_ref_accepts_uri_schemes_and_redacts_inline_secrets() {
        let vault: SecretRef = "secret://vault/billing-api-key"
            .parse()
            .expect("valid secret://");
        assert_eq!(vault.to_string(), "secret://vault/billing-api-key");

        let env_ref: SecretRef = "env://GAP_BILLING_TOKEN".parse().expect("valid env://");
        assert_eq!(env_ref.to_string(), "env://GAP_BILLING_TOKEN");

        let file_ref: SecretRef = "file:///etc/gap/tls.key".parse().expect("valid file:///");
        assert_eq!(file_ref.to_string(), "file:///etc/gap/tls.key");

        let canary = "sk-live-super-sensitive-inline-secret";
        let err = canary
            .parse::<SecretRef>()
            .expect_err("inline secret must fail");
        assert!(
            !err.to_string().contains(canary),
            "SecretRefError must never echo inline secret values (INV-15)"
        );

        let toml_inline = format!("[auth]\nintrospection_client_secret = \"{canary}\"\n");
        let cfg_err = GapConfig::from_toml_str(&toml_inline)
            .expect_err("inline secret in gap.toml must fail");
        assert!(
            !cfg_err.to_string().contains(canary),
            "ConfigError must never echo inline secret values (INV-15)"
        );
    }

    // spec: GQL-040, GQL-060, SEC-001, SEC-002, OPS-001
    #[test]
    fn gap_toml_defaults_deny_unknown_fields_and_enforce_spec_ranges() {
        let cfg = GapConfig::from_toml_str("").expect("empty gap.toml uses spec defaults");
        assert_eq!(cfg.profile, Profile::Prod);
        assert_eq!(cfg.effective_operation_mode(), OperationMode::Hybrid);
        assert_eq!(cfg.limits.traverse_max_depth, 16);

        let unknown_top = GapConfig::from_toml_str("unknown_top_key = true\n");
        assert!(unknown_top.is_err());

        let unknown_nested = GapConfig::from_toml_str("[server]\nextra_port = 9999\n");
        assert!(unknown_nested.is_err());

        let prod_anon =
            GapConfig::from_toml_str("profile = \"prod\"\n[auth]\nallow_anonymous = true\n");
        assert!(prod_anon.is_err());

        let dev_anon =
            GapConfig::from_toml_str("profile = \"dev\"\n[auth]\nallow_anonymous = true\n")
                .expect("anonymous allowed in dev profile");
        assert_eq!(dev_anon.effective_operation_mode(), OperationMode::Open);

        let depth_too_high = GapConfig::from_toml_str("[limits]\ntraverse_max_depth = 17\n");
        assert!(depth_too_high.is_err());
    }

    // spec: INV-15, OPS-010 (ADR-018)
    #[test]
    fn gap_env_double_underscore_overrides_apply_and_reject_unknown_keys() {
        let base_toml = r#"
profile = "dev"

[server]
listen = "127.0.0.1:8080"

[limits]
max_cost_units = 10000
"#;
        let cfg = GapConfig::from_toml_and_env(
            base_toml,
            [
                ("GAP_SERVER__LISTEN", "0.0.0.0:9443"),
                ("GAP_GATEWAY__DEFAULT_TENANT", "acme-prod"),
                ("GAP_LIMITS__MAX_COST_UNITS", "50000"),
                ("GAP_TELEMETRY__LOG_JSON", "false"),
                ("GAP_TELEMETRY__HEAD_SAMPLE_RATIO", "0.25"),
                (
                    "GAP_AUTH__INTROSPECTION_CLIENT_SECRET",
                    "secret://vault/oidc",
                ),
                ("UNRELATED_ENV_VAR", "ignored"),
            ],
        )
        .expect("valid GAP_*__* overrides succeed");

        assert_eq!(cfg.server.listen, "0.0.0.0:9443");
        assert_eq!(cfg.gateway.default_tenant.as_str(), "acme-prod");
        assert_eq!(cfg.limits.max_cost_units, 50_000);
        assert!(!cfg.telemetry.log_json);
        assert!((cfg.telemetry.head_sample_ratio - 0.25).abs() < f64::EPSILON);
        assert_eq!(
            cfg.auth
                .introspection_client_secret
                .as_ref()
                .map(ToString::to_string)
                .as_deref(),
            Some("secret://vault/oidc")
        );

        let bad_env = GapConfig::from_toml_and_env("", [("GAP_SERVER__NO_SUCH_FIELD", "123")]);
        assert!(bad_env.is_err(), "unknown GAP_*__* key must fail");

        let empty_seg = GapConfig::from_toml_and_env("", [("GAP_SERVER__", "123")]);
        assert!(empty_seg.is_err());
    }

    // spec: OPS-001
    #[test]
    fn config_schema_file_is_up_to_date() {
        let schema_path =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/config.schema.json");
        let expected = GapConfig::json_schema_pretty().expect("generate JSON schema");
        let committed = fs::read_to_string(&schema_path)
            .expect("docs/config.schema.json must exist in repository");
        assert_eq!(
            committed, expected,
            "docs/config.schema.json is out of date with GapConfig::json_schema_pretty()"
        );
    }
}
