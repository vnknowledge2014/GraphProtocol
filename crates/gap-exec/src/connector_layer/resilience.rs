//! Resilience configuration, outcome classification, and retry/backoff helpers (`RTE-080`, `RTE-090`, `RTE-091`).

use std::time::Duration;

use gap_balance::slot::Outcome;
use gap_core::error::NgcErrorCode;

/// Defaults and validation for breaker/retry/health parameters.
#[derive(Debug, Clone)]
pub struct ResilienceConfig {
    /// Maximum retry attempts for idempotent reads (default 2, `RTE-090`).
    pub max_retries: u32,
    /// Base sleep duration for jittered backoff (default 50 ms, `RTE-090`).
    pub backoff_base: Duration,
    /// Cap on jittered sleep before deadline check applies (default 1 s, `RTE-090`).
    pub backoff_cap: Duration,
    /// Health poll interval (default 5 s, `NGC-070`).
    pub health_interval: Duration,
    /// Fraction of `health_interval` used for ± jitter (default 0.2, `NGC-070`).
    pub health_jitter: f64,
    /// Deadline margin subtracted before retry sleep check (default 100 ms).
    pub deadline_margin: Duration,
    /// Maximum duration for a single health RPC (default 500 ms, `NGC-070`).
    pub health_timeout: Duration,
}

impl Default for ResilienceConfig {
    fn default() -> Self {
        Self {
            max_retries: 2,
            backoff_base: Duration::from_millis(50),
            backoff_cap: Duration::from_secs(1),
            health_interval: Duration::from_secs(5),
            health_jitter: 0.2,
            deadline_margin: Duration::from_millis(100),
            health_timeout: Duration::from_millis(500),
        }
    }
}

impl ResilienceConfig {
    /// Returns `Err` if any field is out of its valid range.
    pub fn validate(&self) -> Result<(), String> {
        if self.max_retries > 10 {
            return Err(format!(
                "max_retries {} exceeds maximum 10",
                self.max_retries
            ));
        }
        if self.backoff_base.is_zero() {
            return Err("backoff_base must be > 0".into());
        }
        if self.backoff_cap < self.backoff_base {
            return Err("backoff_cap must be >= backoff_base".into());
        }
        if self.health_interval.is_zero() {
            return Err("health_interval must be > 0".into());
        }
        if !(0.0..=1.0).contains(&self.health_jitter) {
            return Err(format!(
                "health_jitter {} is not in [0.0, 1.0]",
                self.health_jitter
            ));
        }
        if self.health_timeout.is_zero() {
            return Err("health_timeout must be > 0".into());
        }
        Ok(())
    }
}

/// Classifies an NGC error code into the telemetry outcome taxonomy (`RTE-080`).
///
/// `INVALID_ARGUMENT`, `PERMISSION_DENIED`, and `NOT_FOUND` are deterministic client errors
/// (the branch is healthy; the caller is at fault). Everything else — including timeouts,
/// transport failures, and unknown codes — is a branch `Failure` and counts in the breaker window.
#[inline]
pub fn classify_error_code(code: NgcErrorCode) -> Outcome {
    match code {
        NgcErrorCode::InvalidArgument | NgcErrorCode::PermissionDenied | NgcErrorCode::NotFound => {
            Outcome::ClientError
        }
        _ => Outcome::Failure,
    }
}

/// Returns `true` iff `err.retryable` is set, indicating the error warrants a retry attempt.
#[inline]
pub fn is_retryable(err: &gap_core::error::NgcErrorInfo) -> bool {
    err.retryable
}

/// Computes the full-jitter backoff duration for `attempt` using `config` and `rng`.
///
/// `base_cap = min(base * 2^attempt, cap)`, then `jitter = rng.next_u64() % base_cap`.
/// Full-jitter spreads retries across the whole range to avoid thundering herd (`RTE-090`).
pub fn jittered_backoff(
    attempt: u32,
    config: &ResilienceConfig,
    rng: &mut gap_balance::XorShift64,
) -> Duration {
    let base_ms = config.backoff_base.as_millis() as u64;
    let cap_ms = config.backoff_cap.as_millis() as u64;
    let shifted = base_ms.saturating_mul(1_u64 << attempt.min(63));
    let ceiling_ms = shifted.min(cap_ms).max(1);
    let jitter_ms = rng.next_u64() % ceiling_ms;
    Duration::from_millis(jitter_ms)
}
