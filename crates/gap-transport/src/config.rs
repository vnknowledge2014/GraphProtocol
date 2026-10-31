//! Transport configuration, redacted API-key records, and constant-time key verification
//! (`spec/00-conventions.md` §8, `spec/02-gap-ql.md` §8, `SEC-001`, `SEC-002`, `SEC-010`, `INV-15`).

use std::collections::BTreeSet;
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

pub use gap_core::config::{LimitsConfig, OperationMode, Profile};
use gap_core::ids::TenantId;
use gap_core::secret::REDACTED_MARKER;
use gap_exec::ExecLimits;
use thiserror::Error;

/// Default hard ceiling on per-request execution deadline (`60 s`, `Brief §3.1`, `RUN-030`).
pub const DEFAULT_MAX_DEADLINE: Duration = Duration::from_secs(60);

/// Default per-request deadline when client omits `extensions.gap.deadlineMs` (`30 s`, `Brief §3.1`).
pub const DEFAULT_DEADLINE: Duration = Duration::from_secs(30);

/// Default bounded `mpsc` channel capacity for `graphql-sse` event streams (`16`, `Brief §3.7`).
pub const DEFAULT_SSE_CHANNEL_CAPACITY: usize = 16;

/// Default maximum JSON nesting depth accepted on HTTP request bodies (`64`, `Brief §3.4`).
pub const DEFAULT_MAX_JSON_DEPTH: usize = 64;

/// Configuration validation error returned when constructing [`TransportConfig`].
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("invalid transport configuration for `{field}`: {reason}")]
pub struct ConfigValidationError {
    /// Configuration field that failed validation.
    pub field: &'static str,
    /// Redacted structural validation reason (`INV-15`: never echoes secret values).
    pub reason: String,
}

/// Configured API-key principal binding (`Brief §3.1`, `SEC-001`, `SEC-010`, `INV-15`).
///
/// Key material (`blake3_hex`) is always redacted in [`fmt::Debug`] (`INV-15`).
#[derive(Clone, PartialEq, Eq)]
pub struct ApiKeyRecord {
    /// Stable non-secret key identifier mapped to `Principal.id`.
    pub id: String,
    /// Lowercase/uppercase 64-character hex `blake3-256` digest of the raw API key.
    ///
    /// Note: `WP-M1-08` uses `blake3` + constant-time byte comparison because `argon2` is not
    /// vendored in M1; migrating stored verifiers to `Argon2id` (`SEC-001`) is tracked as a
    /// follow-up work item.
    pub blake3_hex: String,
    /// Authenticated tenant bound to this API key (`SEC-010`, `INV-09`).
    pub tenant: TenantId,
    /// Granted authorization scopes bound to this API key.
    pub scopes: BTreeSet<String>,
}

impl ApiKeyRecord {
    /// Constructs an [`ApiKeyRecord`] from a precomputed 64-char `blake3` hex digest.
    #[must_use]
    pub fn new(
        id: impl Into<String>,
        blake3_hex: impl Into<String>,
        tenant: TenantId,
        scopes: BTreeSet<String>,
    ) -> Self {
        Self {
            id: id.into(),
            blake3_hex: blake3_hex.into(),
            tenant,
            scopes,
        }
    }

    /// Computes `blake3::hash(raw_key.as_bytes())` and constructs an [`ApiKeyRecord`].
    #[must_use]
    pub fn from_raw_key(
        id: impl Into<String>,
        raw_key: &str,
        tenant: TenantId,
        scopes: BTreeSet<String>,
    ) -> Self {
        let hex = blake3::hash(raw_key.as_bytes()).to_hex().to_string();
        Self::new(id, hex, tenant, scopes)
    }
}

impl fmt::Debug for ApiKeyRecord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ApiKeyRecord")
            .field("id", &self.id)
            .field("blake3_hex", &REDACTED_MARKER)
            .field("tenant", &self.tenant)
            .field("scopes", &self.scopes)
            .finish()
    }
}

/// Structural work counters emitted by [`Blake3ApiKeyVerifier::verify_with_stats`] to prove
/// non-early-exiting constant-time comparison deterministically in offline tests (`Brief §4`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct VerificationStats {
    /// Number of [`ApiKeyRecord`] entries evaluated during verification.
    pub records_inspected: usize,
    /// Total digest bytes compared across all evaluated records (`32 * records_inspected`).
    pub bytes_compared: usize,
}

/// Pluggable API-key verifier trait (`Brief §1`, `§3.4`, `SEC-001`).
pub trait ApiKeyVerifier: Send + Sync + fmt::Debug {
    /// Verifies `candidate_key` against `records`, returning the matching [`ApiKeyRecord`] on
    /// success without leaking timing or early-exit side channels.
    fn verify<'a>(
        &'a self,
        candidate_key: &str,
        records: &'a [ApiKeyRecord],
    ) -> Option<&'a ApiKeyRecord>;
}

/// `blake3`-256 + constant-time byte-comparison [`ApiKeyVerifier`] (`Brief §1`, `§3.4`).
#[derive(Debug, Default, Clone, Copy)]
pub struct Blake3ApiKeyVerifier;

impl Blake3ApiKeyVerifier {
    /// Verifies `candidate_key` against `records` while recording exact structural comparison
    /// counts (`records_inspected`, `bytes_compared`).
    #[must_use]
    pub fn verify_with_stats<'a>(
        &self,
        _candidate_key: &str,
        _records: &'a [ApiKeyRecord],
    ) -> (Option<&'a ApiKeyRecord>, VerificationStats) {
        (None, VerificationStats::default())
    }
}

impl ApiKeyVerifier for Blake3ApiKeyVerifier {
    fn verify<'a>(
        &'a self,
        candidate_key: &str,
        records: &'a [ApiKeyRecord],
    ) -> Option<&'a ApiKeyRecord> {
        self.verify_with_stats(candidate_key, records).0
    }
}

/// HTTP transport, gateway authentication, deadline, and SSE configuration (`Brief §3.1`).
#[derive(Clone)]
pub struct TransportConfig {
    /// Active deployment profile (`Dev` allows anonymous read-scoped principal; `Prod` requires credentials).
    pub profile: Profile,
    /// Query size, variable, depth, alias, root-field, and complexity limits (`spec/02-gap-ql.md` §8).
    pub limits: LimitsConfig,
    /// Hard cap on per-request execution deadline (`default 60 s`, `RUN-030`).
    pub max_deadline: Duration,
    /// Default per-request deadline when client omits `extensions.gap.deadlineMs` (`default 30 s`, `RUN-030`).
    pub default_deadline: Duration,
    /// Execution concurrency, response size, and connector margin limits (`gap_exec::ExecLimits`).
    pub exec: ExecLimits,
    /// Configured API keys (`Debug`-redacted, `INV-15`).
    pub api_keys: Vec<ApiKeyRecord>,
    /// Shared atomic drain flag toggled during graceful shutdown (`/readyz` returns `503` when `true`).
    pub draining: Arc<AtomicBool>,
    /// Default tenant assigned to anonymous callers in `Profile::Dev` (`ADR-018`).
    pub default_tenant: TenantId,
    /// Optional explicit [`OperationMode`] override (`None` -> `Open` in `Dev`, `Hybrid` in `Prod`).
    pub operation_mode: Option<OperationMode>,
    /// Bounded `mpsc` channel capacity per SSE connection (`default 16`, `Brief §3.7`).
    pub sse_channel_capacity: usize,
    /// Maximum JSON nesting depth accepted on HTTP request bodies (`default 64`, `Brief §3.4`).
    pub max_json_depth: usize,
}

impl fmt::Debug for TransportConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TransportConfig")
            .field("profile", &self.profile)
            .field("limits", &self.limits)
            .field("max_deadline", &self.max_deadline)
            .field("default_deadline", &self.default_deadline)
            .field("exec", &self.exec)
            .field("api_keys", &self.api_keys)
            .field("draining", &self.draining)
            .field("default_tenant", &self.default_tenant)
            .field("operation_mode", &self.operation_mode)
            .field("sse_channel_capacity", &self.sse_channel_capacity)
            .field("max_json_depth", &self.max_json_depth)
            .finish()
    }
}

impl Default for TransportConfig {
    fn default() -> Self {
        Self::for_profile(Profile::Dev)
    }
}

impl TransportConfig {
    /// Creates a [`TransportConfig`] with spec defaults for `profile`.
    #[must_use]
    pub fn for_profile(profile: Profile) -> Self {
        Self {
            profile,
            limits: LimitsConfig::default(),
            max_deadline: DEFAULT_MAX_DEADLINE,
            default_deadline: DEFAULT_DEADLINE,
            exec: ExecLimits::default(),
            api_keys: Vec::new(),
            draining: Arc::new(AtomicBool::new(false)),
            default_tenant: TenantId::default_tenant(),
            operation_mode: None,
            sse_channel_capacity: DEFAULT_SSE_CHANNEL_CAPACITY,
            max_json_depth: DEFAULT_MAX_JSON_DEPTH,
        }
    }

    /// Validates `self` and returns it on success (`Brief §3.1`).
    ///
    /// # Errors
    ///
    /// Returns [`ConfigValidationError`] if any limit, deadline, channel capacity, or API-key
    /// record invariant is violated.
    pub fn validated(self) -> Result<Self, ConfigValidationError> {
        self.validate()?;
        Ok(self)
    }

    /// Validates all configuration invariants (`Brief §3.1`, `spec/02-gap-ql.md` §8, `INV-15`).
    ///
    /// # Errors
    ///
    /// Returns [`ConfigValidationError`] on the first out-of-range or malformed field.
    pub fn validate(&self) -> Result<(), ConfigValidationError> {
        Err(ConfigValidationError {
            field: "stub",
            reason: "not yet implemented".to_owned(),
        })
    }

    /// Resolves the effective [`OperationMode`] (`Open` in `Dev`, `Hybrid` in `Prod` when unset).
    #[must_use]
    pub const fn effective_operation_mode(&self) -> OperationMode {
        match self.operation_mode {
            Some(mode) => mode,
            None => match self.profile {
                Profile::Dev => OperationMode::Open,
                Profile::Prod => OperationMode::Hybrid,
            },
        }
    }
}
