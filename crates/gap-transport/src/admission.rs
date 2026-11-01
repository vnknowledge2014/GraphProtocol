//! The production [`AdmissionPolicy`]: per-subject rate limit, then adaptive concurrency limit with priority
//! shedding, then (after planning) the per-minute cost budget (`RUN-040`, `RUN-050`, `SEC-050`).
//!
//! Order matters: the per-principal rate limit runs **first** (an abusive principal is refused before it can
//! occupy shared concurrency), the concurrency permit second (a refused request has not consumed a permit),
//! and the cost budget is charged by the pipeline once the plan's cost is known.

use std::sync::Arc;

use axum::http::HeaderMap;
use gap_core::clock::Clock;
use gap_core::context::Principal;
use gap_core::error::GapError;

use crate::gateway::{AdmissionGuard, AdmissionOutcome, AdmissionPolicy};
use crate::ratelimit::{RateLimitConfig, RateLimitConfigError, Subject, SubjectRateLimiter};
use crate::shed::{AdaptiveLimiter, LimiterConfig, LimiterConfigError, Priority};

/// Invalid admission configuration.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AdmissionConfigError {
    /// The concurrency limiter configuration is invalid.
    #[error("{0}")]
    Limiter(LimiterConfigError),
    /// The rate limiter configuration is invalid.
    #[error("{0}")]
    RateLimit(RateLimitConfigError),
}

/// Rate limit + adaptive concurrency limit + cost budget as one [`AdmissionPolicy`].
#[derive(Debug)]
pub struct GatewayAdmission {
    limiter: Arc<AdaptiveLimiter>,
    rate: SubjectRateLimiter,
}

impl GatewayAdmission {
    /// Builds the policy from validated configurations.
    ///
    /// # Errors
    ///
    /// Returns [`AdmissionConfigError`] if either configuration is invalid.
    pub fn new(
        limiter: LimiterConfig,
        rate: RateLimitConfig,
        clock: Arc<dyn Clock>,
    ) -> Result<Self, AdmissionConfigError> {
        let limiter = AdaptiveLimiter::new(limiter, Arc::clone(&clock))
            .map_err(AdmissionConfigError::Limiter)?;
        let rate = SubjectRateLimiter::new(rate, clock).map_err(AdmissionConfigError::RateLimit)?;
        Ok(Self { limiter, rate })
    }

    /// The concurrency limiter (limit, in-flight and shed counters for metrics and tests).
    #[must_use]
    pub fn limiter(&self) -> &Arc<AdaptiveLimiter> {
        &self.limiter
    }

    /// The per-subject rate limiter.
    #[must_use]
    pub fn rate_limiter(&self) -> &SubjectRateLimiter {
        &self.rate
    }
}

impl AdmissionPolicy for GatewayAdmission {
    fn check(&self, principal: &Principal, headers: &HeaderMap) -> Result<(), GapError> {
        self.admit(principal, headers).map(drop)
    }

    fn admit(
        &self,
        principal: &Principal,
        headers: &HeaderMap,
    ) -> Result<AdmissionGuard, GapError> {
        self.rate.check_request(&Subject::of(principal))?;
        let permit = self.limiter.try_acquire(Priority::from_headers(headers))?;
        Ok(AdmissionGuard::new(permit))
    }

    fn charge_cost(&self, principal: &Principal, units: u64) -> Result<(), GapError> {
        self.rate.charge_cost(&Subject::of(principal), units)
    }
}

impl From<AdmissionOutcome> for crate::shed::Outcome {
    fn from(outcome: AdmissionOutcome) -> Self {
        match outcome {
            AdmissionOutcome::Success => Self::Success,
            AdmissionOutcome::Dropped => Self::Dropped,
            AdmissionOutcome::Ignore => Self::Ignore,
        }
    }
}
