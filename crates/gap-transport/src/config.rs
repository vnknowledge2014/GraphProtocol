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

/// Hard upper bound on `max_deadline` (`600 s`, matching `limits.request_deadline_ms` ceiling).
pub const HARD_MAX_DEADLINE: Duration = Duration::from_secs(600);

/// Default bounded `mpsc` channel capacity for `graphql-sse` event streams (`16`, `Brief §3.7`).
pub const DEFAULT_SSE_CHANNEL_CAPACITY: usize = 16;

/// Hard upper bound on `sse_channel_capacity` (`65_536`).
pub const MAX_SSE_CHANNEL_CAPACITY: usize = 65_536;

/// Default maximum JSON nesting depth accepted on HTTP request bodies (`64`, `Brief §3.4`).
pub const DEFAULT_MAX_JSON_DEPTH: usize = 64;

/// Hard upper bound on `max_json_depth` (`512`).
pub const HARD_MAX_JSON_DEPTH: usize = 512;

/// Configuration validation error returned when constructing or validating [`TransportConfig`].
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("invalid transport configuration for `{field}`: {reason}")]
pub struct ConfigValidationError {
    /// Configuration field that failed validation.
    pub field: &'static str,
    /// Redacted structural validation reason (`INV-15`: never echoes secret values).
    pub reason: String,
}

impl ConfigValidationError {
    fn new(field: &'static str, reason: impl Into<String>) -> Self {
        Self {
            field,
            reason: reason.into(),
        }
    }
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

    /// Validates `id` and `blake3_hex` at construction (`INV-15`: never echoes `blake3_hex`).
    ///
    /// # Errors
    ///
    /// Returns [`ConfigValidationError`] if `id` is empty/non-printable or `blake3_hex` is not a
    /// 64-character ASCII hexadecimal digest.
    pub fn try_new(
        id: impl Into<String>,
        blake3_hex: impl Into<String>,
        tenant: TenantId,
        scopes: BTreeSet<String>,
    ) -> Result<Self, ConfigValidationError> {
        let rec = Self::new(id, blake3_hex, tenant, scopes);
        rec.validate()?;
        Ok(rec)
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

    /// Validates structural invariants of this record without echoing `blake3_hex` (`INV-15`).
    ///
    /// # Errors
    ///
    /// Returns [`ConfigValidationError`] if `id` or `blake3_hex` is malformed.
    pub fn validate(&self) -> Result<(), ConfigValidationError> {
        if self.id.is_empty() || self.id.len() > 128 {
            return Err(ConfigValidationError::new(
                "api_keys.id",
                "key id length must lie within 1..=128 bytes",
            ));
        }
        if !self
            .id
            .bytes()
            .all(|b| b.is_ascii_graphic() && !b.is_ascii_control())
        {
            return Err(ConfigValidationError::new(
                "api_keys.id",
                "key id must contain only printable ASCII non-whitespace characters",
            ));
        }
        if decode_hex_32(&self.blake3_hex).is_none() {
            return Err(ConfigValidationError::new(
                "api_keys.blake3_hex",
                "digest must be a 64-character ASCII hexadecimal string (value redacted)",
            ));
        }
        Ok(())
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

fn hex_nibble(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn decode_hex_32(hex: &str) -> Option<[u8; 32]> {
    let raw = hex.as_bytes();
    if raw.len() != 64 {
        return None;
    }
    let mut out = [0_u8; 32];
    for (idx, pair) in raw.as_chunks::<2>().0.iter().enumerate() {
        let hi = hex_nibble(pair[0])?;
        let lo = hex_nibble(pair[1])?;
        out[idx] = (hi << 4) | lo;
    }
    Some(out)
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
    ///
    /// Every configured record and all 32 digest bytes per record are always inspected via
    /// bitwise XOR/OR accumulation without early exit.
    #[must_use]
    pub fn verify_with_stats<'a>(
        &self,
        candidate_key: &str,
        records: &'a [ApiKeyRecord],
    ) -> (Option<&'a ApiKeyRecord>, VerificationStats) {
        let candidate_digest = *blake3::hash(candidate_key.as_bytes()).as_bytes();
        let mut stats = VerificationStats::default();
        let mut matched_idx: Option<usize> = None;

        for (record_idx, record) in records.iter().enumerate() {
            let (expected_digest, valid_hex_mask) = match decode_hex_32(&record.blake3_hex) {
                Some(bytes) => (bytes, 0_u8),
                None => ([0_u8; 32], 0xFF_u8),
            };

            let mut diff: u8 = valid_hex_mask;
            for byte_idx in 0..32 {
                diff |=
                    std::hint::black_box(candidate_digest[byte_idx] ^ expected_digest[byte_idx]);
                stats.bytes_compared += 1;
            }
            stats.records_inspected += 1;

            if std::hint::black_box(diff) == 0 && matched_idx.is_none() {
                matched_idx = Some(record_idx);
            }
        }

        let matched = matched_idx.and_then(|idx| records.get(idx));
        (matched, stats)
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
    /// Whether a request without credentials becomes the anonymous read-scoped principal. Defaults to `true` in `Dev` and
    /// `false` in `Prod`; `gap-server` sets it from `[auth].allow_anonymous`, so a `dev` deployment that forbids anonymous
    /// access really does (`SEC-002`). A presented but wrong credential is refused whatever this says.
    pub allow_anonymous: bool,
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
            allow_anonymous: matches!(profile, Profile::Dev),
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

    /// Constructs and validates a [`TransportConfig`] (`Brief §3.1` validation at construction).
    ///
    /// # Errors
    ///
    /// Returns [`ConfigValidationError`] if any limit, deadline, channel capacity, or API-key
    /// invariant is out of range.
    #[allow(clippy::too_many_arguments)]
    pub fn try_new(
        profile: Profile,
        limits: LimitsConfig,
        max_deadline: Duration,
        default_deadline: Duration,
        exec: ExecLimits,
        api_keys: Vec<ApiKeyRecord>,
        draining: Arc<AtomicBool>,
    ) -> Result<Self, ConfigValidationError> {
        let cfg = Self {
            profile,
            allow_anonymous: matches!(profile, Profile::Dev),
            limits,
            max_deadline,
            default_deadline,
            exec,
            api_keys,
            draining,
            default_tenant: TenantId::default_tenant(),
            operation_mode: None,
            sse_channel_capacity: DEFAULT_SSE_CHANNEL_CAPACITY,
            max_json_depth: DEFAULT_MAX_JSON_DEPTH,
        };
        cfg.validated()
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
        validate_limits(&self.limits)?;

        if self.default_deadline.is_zero() {
            return Err(ConfigValidationError::new(
                "default_deadline",
                "default_deadline must be greater than zero",
            ));
        }
        if self.max_deadline < self.default_deadline {
            return Err(ConfigValidationError::new(
                "max_deadline",
                "max_deadline must be greater than or equal to default_deadline",
            ));
        }
        if self.max_deadline > HARD_MAX_DEADLINE {
            return Err(ConfigValidationError::new(
                "max_deadline",
                "max_deadline must not exceed 600s",
            ));
        }

        if self.exec.max_parallel_fetches == 0 {
            return Err(ConfigValidationError::new(
                "exec.max_parallel_fetches",
                "max_parallel_fetches must be >= 1",
            ));
        }
        if self.exec.max_response_bytes == 0 {
            return Err(ConfigValidationError::new(
                "exec.max_response_bytes",
                "max_response_bytes must be >= 1",
            ));
        }
        if !(0.0..1.0).contains(&self.exec.margin_fraction) {
            return Err(ConfigValidationError::new(
                "exec.margin_fraction",
                "margin_fraction must lie within 0.0..1.0",
            ));
        }

        if !(1..=MAX_SSE_CHANNEL_CAPACITY).contains(&self.sse_channel_capacity) {
            return Err(ConfigValidationError::new(
                "sse_channel_capacity",
                format!("sse_channel_capacity must lie within 1..={MAX_SSE_CHANNEL_CAPACITY}"),
            ));
        }
        if !(1..=HARD_MAX_JSON_DEPTH).contains(&self.max_json_depth) {
            return Err(ConfigValidationError::new(
                "max_json_depth",
                format!("max_json_depth must lie within 1..={HARD_MAX_JSON_DEPTH}"),
            ));
        }

        let mut seen_ids = BTreeSet::new();
        for record in &self.api_keys {
            record.validate()?;
            if !seen_ids.insert(record.id.as_str()) {
                return Err(ConfigValidationError::new(
                    "api_keys.id",
                    format!("duplicate API key id `{}`", record.id),
                ));
            }
        }

        Ok(())
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

fn validate_limits(limits: &LimitsConfig) -> Result<(), ConfigValidationError> {
    if !(1..=32).contains(&limits.max_selection_depth) {
        return Err(ConfigValidationError::new(
            "limits.max_selection_depth",
            "must lie within 1..=32",
        ));
    }
    if !(1..=500).contains(&limits.max_aliases) {
        return Err(ConfigValidationError::new(
            "limits.max_aliases",
            "must lie within 1..=500",
        ));
    }
    if !(1..=100).contains(&limits.max_root_fields) {
        return Err(ConfigValidationError::new(
            "limits.max_root_fields",
            "must lie within 1..=100",
        ));
    }
    if !(100..=1_000_000).contains(&limits.max_cost_units) {
        return Err(ConfigValidationError::new(
            "limits.max_cost_units",
            "must lie within 100..=1_000_000",
        ));
    }
    if !(16_384..=16_777_216).contains(&limits.max_request_body_bytes) {
        return Err(ConfigValidationError::new(
            "limits.max_request_body_bytes",
            "must lie within 16_384..=16_777_216",
        ));
    }
    if !(65_536..=67_108_864).contains(&limits.max_response_body_bytes) {
        return Err(ConfigValidationError::new(
            "limits.max_response_body_bytes",
            "must lie within 65_536..=67_108_864",
        ));
    }
    if !(1..=1_000).contains(&limits.default_list_size) {
        return Err(ConfigValidationError::new(
            "limits.default_list_size",
            "must lie within 1..=1_000",
        ));
    }
    if !(1..=16).contains(&limits.traverse_max_depth) {
        return Err(ConfigValidationError::new(
            "limits.traverse_max_depth",
            "must lie within 1..=16",
        ));
    }
    if !(1..=1_000).contains(&limits.max_variables) {
        return Err(ConfigValidationError::new(
            "limits.max_variables",
            "must lie within 1..=1_000",
        ));
    }
    if !(100..=600_000).contains(&limits.request_deadline_ms) {
        return Err(ConfigValidationError::new(
            "limits.request_deadline_ms",
            "must lie within 100..=600_000",
        ));
    }
    if !(1..=10_000).contains(&limits.max_branches) {
        return Err(ConfigValidationError::new(
            "limits.max_branches",
            "must lie within 1..=10_000",
        ));
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    // spec: SEC-001, SEC-010, INV-15
    #[test]
    fn inv_15_config_validation_and_debug_never_echo_key_material() {
        let mut cfg = TransportConfig::for_profile(Profile::Dev);
        assert!(cfg.validate().is_ok());

        let canary_bad_hex = "secret-inline-key-not-64-hex-canary-998877";
        let bad_rec = ApiKeyRecord::new(
            "k1",
            canary_bad_hex,
            TenantId::default_tenant(),
            BTreeSet::from(["read".to_owned()]),
        );
        let err = bad_rec.validate().expect_err("must reject non-hex digest");
        assert_eq!(err.field, "api_keys.blake3_hex");
        assert!(
            !err.to_string().contains(canary_bad_hex),
            "validation error must never echo secret material"
        );

        let good_rec = ApiKeyRecord::from_raw_key(
            "k-good",
            "raw-secret-api-key-value",
            TenantId::default_tenant(),
            BTreeSet::from(["read".to_owned()]),
        );
        let dbg = format!("{good_rec:?}");
        assert!(!dbg.contains(&good_rec.blake3_hex));
        assert!(dbg.contains(REDACTED_MARKER));

        cfg.max_deadline = Duration::from_secs(5);
        cfg.default_deadline = Duration::from_secs(10);
        assert_eq!(
            cfg.validate().expect_err("bad deadline").field,
            "max_deadline"
        );

        cfg.max_deadline = Duration::from_secs(60);
        cfg.sse_channel_capacity = 0;
        assert_eq!(
            cfg.validate().expect_err("zero sse cap").field,
            "sse_channel_capacity"
        );
    }

    // spec: SEC-001, SEC-010
    #[test]
    fn blake3_verifier_inspects_every_record_and_all_32_bytes() {
        let t = TenantId::default_tenant();
        let scopes = BTreeSet::from(["read".to_owned()]);
        let records = vec![
            ApiKeyRecord::from_raw_key("k1", "alpha-secret", t.clone(), scopes.clone()),
            ApiKeyRecord::from_raw_key("k2", "bravo-secret", t.clone(), scopes.clone()),
            ApiKeyRecord::from_raw_key("k3", "charlie-secret", t, scopes),
        ];

        let verifier = Blake3ApiKeyVerifier;
        for (probe, expected_id) in [
            ("alpha-secret", Some("k1")),
            ("bravo-secret", Some("k2")),
            ("charlie-secret", Some("k3")),
            ("delta-missing", None),
        ] {
            let (found, stats) = verifier.verify_with_stats(probe, &records);
            assert_eq!(found.map(|r| r.id.as_str()), expected_id);
            assert_eq!(stats.records_inspected, 3);
            assert_eq!(stats.bytes_compared, 96);
        }

        // Single-byte digest tampering at byte 0, byte 15, and byte 31 MUST all reject while
        // inspecting all 32 bytes (kills `diff =` overwrite and `0..31` / `1..32` off-by-one loops).
        let base_hex = records[0].blake3_hex.clone();
        for byte_pos in [0_usize, 15, 31] {
            let nibble_idx = byte_pos * 2 + 1;
            let mut hex_bytes = base_hex.as_bytes().to_vec();
            hex_bytes[nibble_idx] = if hex_bytes[nibble_idx] == b'0' {
                b'1'
            } else {
                b'0'
            };
            let tampered_hex = String::from_utf8(hex_bytes).expect("ascii hex");
            let tampered = vec![ApiKeyRecord::new(
                "tampered",
                tampered_hex,
                TenantId::default_tenant(),
                BTreeSet::from(["read".to_owned()]),
            )];
            let (found, stats) = verifier.verify_with_stats("alpha-secret", &tampered);
            assert!(
                found.is_none(),
                "single-byte mismatch at byte {byte_pos} must reject"
            );
            assert_eq!(stats.records_inspected, 1);
            assert_eq!(stats.bytes_compared, 32);
        }

        // Duplicate key IDs and zero deadlines must fail validation.
        let mut dup_cfg = TransportConfig::for_profile(Profile::Prod);
        dup_cfg.api_keys = vec![records[0].clone(), records[0].clone()];
        assert_eq!(
            dup_cfg.validate().expect_err("duplicate key id").field,
            "api_keys.id"
        );
        let mut zero_dl = TransportConfig::for_profile(Profile::Dev);
        zero_dl.default_deadline = Duration::ZERO;
        assert_eq!(
            zero_dl.validate().expect_err("zero deadline").field,
            "default_deadline"
        );
    }
}
