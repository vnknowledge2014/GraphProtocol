//! Per-subject request-rate and cost-unit limits (`RUN-050`, `SEC-050`, `INV-02`, `INV-09`).
//!
//! One token bucket per `(tenant, principal)`: `rate_per_sec` tokens per second up to `burst` (default 50 rps,
//! burst 100), one token per request; a second bucket limits **planned cost units per minute** (default
//! 600,000), charged after planning and before execution. An exhausted bucket refuses with
//! `GAP_RATE_LIMITED` and the exact wait until the request could succeed (`retry.afterMs`).
//!
//! Buckets are integer arithmetic (nano-tokens: one token is `1e9`, refill is `elapsed_ns x rate`), so there is
//! no floating-point drift and a manual clock makes every test exact. The key space is bounded (`max_subjects`):
//! idle full buckets are dropped first and, at the cap, the least recently used subject is evicted, so a client
//! cannot grow memory by rotating identities. Keys include the tenant, so one tenant's traffic can never
//! consume another's budget.

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use gap_core::clock::Clock;
use gap_core::context::Principal;
use gap_core::error::{ErrorCode, GapError};

/// One token in nano-token units.
const TOKEN: u128 = 1_000_000_000;
/// Number of independently locked shards.
const SHARDS: usize = 16;

/// Invalid [`RateLimitConfig`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RateLimitConfigError(pub String);

impl fmt::Display for RateLimitConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid rate limit configuration: {}", self.0)
    }
}

impl std::error::Error for RateLimitConfigError {}

/// Limits per `(tenant, principal)` (`RUN-050` defaults).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RateLimitConfig {
    /// Sustained requests per second (default 50).
    pub rate_per_sec: u32,
    /// Bucket size: how many requests may arrive at once (default 100).
    pub burst: u32,
    /// Planned cost units per minute (default 600,000).
    pub cost_units_per_min: u64,
    /// Upper bound on tracked subjects (default 100,000).
    pub max_subjects: usize,
    /// A subject idle (bucket full) for this long is forgotten (default 10 minutes).
    pub idle_ttl: Duration,
}

impl Default for RateLimitConfig {
    fn default() -> Self {
        Self {
            rate_per_sec: 50,
            burst: 100,
            cost_units_per_min: 600_000,
            max_subjects: 100_000,
            idle_ttl: Duration::from_secs(600),
        }
    }
}

impl RateLimitConfig {
    /// Checks the invariants the buckets rely on.
    ///
    /// # Errors
    ///
    /// Returns [`RateLimitConfigError`] naming the first violated rule.
    pub fn validate(&self) -> Result<(), RateLimitConfigError> {
        let bad = |m: &str| Err(RateLimitConfigError(m.to_owned()));
        if self.rate_per_sec == 0 {
            return bad("rate_per_sec must be at least 1");
        }
        if self.burst == 0 {
            return bad("burst must be at least 1");
        }
        if self.cost_units_per_min == 0 {
            return bad("cost_units_per_min must be at least 1");
        }
        if self.max_subjects < SHARDS {
            return bad("max_subjects must be at least 16");
        }
        if self.idle_ttl.is_zero() {
            return bad("idle_ttl must be positive");
        }
        Ok(())
    }
}

/// The limited subject: tenant and principal id (`SEC-010`: both come only from authentication).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Subject {
    tenant: String,
    principal: String,
}

impl Subject {
    /// The subject of an authenticated principal.
    #[must_use]
    pub fn of(principal: &Principal) -> Self {
        Self {
            tenant: principal.tenant.to_string(),
            principal: principal.id.clone(),
        }
    }

    fn shard(&self) -> usize {
        // FNV-1a: stable, allocation-free; only used to spread subjects over shards.
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for b in self
            .tenant
            .bytes()
            .chain([0xff])
            .chain(self.principal.bytes())
        {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x0100_0000_01b3);
        }
        (h % SHARDS as u64) as usize
    }
}

#[derive(Debug, Clone, Copy)]
struct Bucket {
    /// Request tokens, in nano-tokens, at most `burst x TOKEN`.
    requests: u128,
    /// Cost tokens, in nano-units, at most `cost_units_per_min x TOKEN`.
    cost: u128,
    refilled_at: Instant,
    last_used: Instant,
}

/// Token-bucket limiter per `(tenant, principal)` (`RUN-050`).
pub struct SubjectRateLimiter {
    cfg: RateLimitConfig,
    clock: Arc<dyn Clock>,
    shards: Vec<Mutex<HashMap<Subject, Bucket>>>,
}

impl fmt::Debug for SubjectRateLimiter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SubjectRateLimiter")
            .field("cfg", &self.cfg)
            .field("subjects", &self.subjects())
            .finish_non_exhaustive()
    }
}

impl SubjectRateLimiter {
    /// Builds a limiter after validating `cfg`.
    ///
    /// # Errors
    ///
    /// Returns [`RateLimitConfigError`] if `cfg` is invalid.
    pub fn new(cfg: RateLimitConfig, clock: Arc<dyn Clock>) -> Result<Self, RateLimitConfigError> {
        cfg.validate()?;
        Ok(Self {
            shards: (0..SHARDS).map(|_| Mutex::new(HashMap::new())).collect(),
            cfg,
            clock,
        })
    }

    /// Number of subjects currently tracked.
    #[must_use]
    pub fn subjects(&self) -> usize {
        self.shards
            .iter()
            .map(|s| s.lock().map_or(0, |g| g.len()))
            .sum()
    }

    fn request_capacity(&self) -> u128 {
        u128::from(self.cfg.burst) * TOKEN
    }

    fn cost_capacity(&self) -> u128 {
        u128::from(self.cfg.cost_units_per_min) * TOKEN
    }

    /// Nano-tokens of request budget added per elapsed nanosecond: `rate x 1e9 / 1e9 = rate`.
    fn refill(&self, bucket: &mut Bucket, now: Instant) {
        let elapsed = now.saturating_duration_since(bucket.refilled_at).as_nanos();
        if elapsed == 0 {
            return;
        }
        bucket.requests = (bucket.requests + elapsed * u128::from(self.cfg.rate_per_sec))
            .min(self.request_capacity());
        // cost units per minute -> per nanosecond: units x 1e9 / 60e9 = units / 60 nano-units per ns.
        bucket.cost = (bucket.cost + elapsed * u128::from(self.cfg.cost_units_per_min) / 60)
            .min(self.cost_capacity());
        bucket.refilled_at = now;
    }

    fn with_bucket<T>(
        &self,
        subject: &Subject,
        f: impl FnOnce(&mut Bucket, Instant) -> T,
    ) -> Result<T, GapError> {
        let now = self.clock.now_instant();
        let mut shard = self.shards[subject.shard()]
            .lock()
            .map_err(|_| GapError::new(ErrorCode::Internal, "rate limiter state is unavailable"))?;
        let per_shard_cap = self.cfg.max_subjects / SHARDS;
        if !shard.contains_key(subject) && shard.len() >= per_shard_cap {
            self.make_room(&mut shard, now);
        }
        let bucket = shard.entry(subject.clone()).or_insert(Bucket {
            requests: self.request_capacity(),
            cost: self.cost_capacity(),
            refilled_at: now,
            last_used: now,
        });
        self.refill(bucket, now);
        bucket.last_used = now;
        Ok(f(bucket, now))
    }

    /// Drops idle full buckets; if the shard is still at its cap, evicts the least recently used subject.
    fn make_room(&self, shard: &mut HashMap<Subject, Bucket>, now: Instant) {
        let (rc, cc, ttl) = (
            self.request_capacity(),
            self.cost_capacity(),
            self.cfg.idle_ttl,
        );
        shard.retain(|_, b| {
            let full = b.requests >= rc && b.cost >= cc;
            !(full && now.saturating_duration_since(b.last_used) >= ttl)
        });
        let per_shard_cap = self.cfg.max_subjects / SHARDS;
        while shard.len() >= per_shard_cap {
            let Some(oldest) = shard
                .iter()
                .min_by_key(|(_, b)| b.last_used)
                .map(|(k, _)| k.clone())
            else {
                break;
            };
            shard.remove(&oldest);
        }
    }

    fn wait_for(&self, missing_nano: u128, per_ns_rate: u128) -> Duration {
        let ns = missing_nano.div_ceil(per_ns_rate.max(1));
        Duration::from_nanos(u64::try_from(ns).unwrap_or(u64::MAX))
    }

    fn limited(what: &str, wait: Duration) -> GapError {
        let after_ms = u32::try_from(wait.as_millis().max(1)).unwrap_or(u32::MAX);
        GapError::new(
            ErrorCode::RateLimited,
            format!("{what} limit exceeded for this principal; retry after {after_ms} ms"),
        )
        .with_retry(true, Some(after_ms))
    }

    /// Takes one request token for `subject`.
    ///
    /// # Errors
    ///
    /// Returns `GAP_RATE_LIMITED` with the exact wait until a token is available.
    pub fn check_request(&self, subject: &Subject) -> Result<(), GapError> {
        let outcome = self.with_bucket(subject, |bucket, _| {
            if bucket.requests >= TOKEN {
                bucket.requests -= TOKEN;
                Ok(())
            } else {
                Err(TOKEN - bucket.requests)
            }
        })?;
        outcome.map_err(|missing| {
            Self::limited(
                "request rate",
                self.wait_for(missing, u128::from(self.cfg.rate_per_sec)),
            )
        })
    }

    /// Charges `units` of planned cost against `subject`'s per-minute cost budget.
    ///
    /// A request whose cost exceeds the whole per-minute budget can never succeed: it is refused with the wait
    /// for a full bucket and will keep being refused (the planner's own cost cap should refuse it earlier).
    ///
    /// # Errors
    ///
    /// Returns `GAP_RATE_LIMITED` with the exact wait until the budget covers `units`.
    pub fn charge_cost(&self, subject: &Subject, units: u64) -> Result<(), GapError> {
        let need = u128::from(units) * TOKEN;
        let outcome = self.with_bucket(subject, |bucket, _| {
            if bucket.cost >= need {
                bucket.cost -= need;
                Ok(())
            } else {
                Err(need.min(self.cost_capacity())
                    - bucket.cost.min(need.min(self.cost_capacity())))
            }
        })?;
        outcome.map_err(|missing| {
            Self::limited(
                "planned cost",
                self.wait_for(missing * 60, u128::from(self.cfg.cost_units_per_min)),
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gap_core::clock::ManualClock;
    use gap_core::context::PrincipalKind;
    use gap_core::ids::TenantId;

    fn limiter(cfg: RateLimitConfig) -> (SubjectRateLimiter, Arc<ManualClock>) {
        let clock = Arc::new(ManualClock::new(1_700_000_000_000));
        (
            SubjectRateLimiter::new(cfg, Arc::clone(&clock) as Arc<dyn Clock>).expect("valid"),
            clock,
        )
    }

    fn subject(tenant: &str, id: &str) -> Subject {
        Subject::of(&Principal::new(
            id,
            PrincipalKind::Agent,
            TenantId::new(tenant).unwrap(),
        ))
    }

    // spec: RUN-050
    #[test]
    fn defaults_are_the_specified_ones() {
        let cfg = RateLimitConfig::default();
        assert_eq!(
            (cfg.rate_per_sec, cfg.burst, cfg.cost_units_per_min),
            (50, 100, 600_000)
        );
        assert!(cfg.validate().is_ok());
        for bad in [
            RateLimitConfig {
                rate_per_sec: 0,
                ..RateLimitConfig::default()
            },
            RateLimitConfig {
                burst: 0,
                ..RateLimitConfig::default()
            },
            RateLimitConfig {
                cost_units_per_min: 0,
                ..RateLimitConfig::default()
            },
            RateLimitConfig {
                max_subjects: 3,
                ..RateLimitConfig::default()
            },
            RateLimitConfig {
                idle_ttl: Duration::ZERO,
                ..RateLimitConfig::default()
            },
        ] {
            assert!(bad.validate().is_err(), "{bad:?}");
        }
    }

    // spec: RUN-050
    #[test]
    fn a_burst_is_allowed_then_refused_with_the_exact_wait() {
        let (l, clock) = limiter(RateLimitConfig {
            rate_per_sec: 10,
            burst: 5,
            ..RateLimitConfig::default()
        });
        let s = subject("acme", "agent-1");
        for _ in 0..5 {
            l.check_request(&s).expect("within the burst");
        }
        let err = l.check_request(&s).expect_err("bucket empty");
        assert_eq!(err.code(), ErrorCode::RateLimited);
        assert_eq!(
            err.extensions.retry.after_ms,
            Some(100),
            "one token at 10 rps = 100 ms"
        );
        // Exactly the advertised wait later, one request passes; one nanosecond earlier it would not.
        clock.advance(Duration::from_millis(99));
        assert!(l.check_request(&s).is_err());
        clock.advance(Duration::from_millis(1));
        l.check_request(&s).expect("a token has been refilled");
        assert!(l.check_request(&s).is_err());
    }

    // spec: RUN-050
    #[test]
    fn the_sustained_rate_is_the_configured_one() {
        let (l, clock) = limiter(RateLimitConfig {
            rate_per_sec: 50,
            burst: 100,
            ..RateLimitConfig::default()
        });
        let s = subject("acme", "agent-1");
        let mut allowed = 0_u32;
        // 10 seconds of 200 rps offered load: burst (100) + 10 s x 50 = 600 allowed.
        for _ in 0..(10 * 200) {
            if l.check_request(&s).is_ok() {
                allowed += 1;
            }
            clock.advance(Duration::from_millis(5));
        }
        assert!((595..=605).contains(&allowed), "allowed {allowed}");
    }

    // spec: SEC-050, INV-09
    #[test]
    fn tenants_and_principals_have_separate_budgets() {
        let (l, _clock) = limiter(RateLimitConfig {
            rate_per_sec: 1,
            burst: 2,
            ..RateLimitConfig::default()
        });
        let (a1, a2, b1) = (
            subject("acme", "agent-1"),
            subject("acme", "agent-2"),
            subject("beta", "agent-1"),
        );
        for s in [&a1, &a1] {
            l.check_request(s).unwrap();
        }
        assert!(l.check_request(&a1).is_err());
        l.check_request(&a2)
            .expect("another principal of the same tenant");
        l.check_request(&b1)
            .expect("the same principal id in another tenant");
    }

    // spec: RUN-050
    #[test]
    fn planned_cost_is_a_per_minute_budget_with_its_own_wait() {
        let (l, clock) = limiter(RateLimitConfig {
            cost_units_per_min: 6_000,
            ..RateLimitConfig::default()
        });
        let s = subject("acme", "agent-1");
        l.charge_cost(&s, 4_000).expect("within the budget");
        l.charge_cost(&s, 2_000).expect("exactly the rest");
        let err = l.charge_cost(&s, 600).expect_err("budget spent");
        assert_eq!(err.code(), ErrorCode::RateLimited);
        // 6,000 units/min = 100 units/s: 600 units need 6 s.
        assert_eq!(err.extensions.retry.after_ms, Some(6_000));
        clock.advance(Duration::from_secs(6));
        l.charge_cost(&s, 600).expect("refilled");
        // The budget never exceeds one minute's worth, however long the subject was idle.
        clock.advance(Duration::from_secs(3_600));
        l.charge_cost(&s, 6_000).expect("a full bucket");
        assert!(l.charge_cost(&s, 1).is_err());
        // A single request costing more than a whole minute can never pass.
        clock.advance(Duration::from_secs(3_600));
        assert!(l.charge_cost(&s, 6_001).is_err());
    }

    // spec: INV-02
    #[test]
    fn the_subject_table_is_bounded_and_evicts_the_least_recently_used() {
        let (l, clock) = limiter(RateLimitConfig {
            max_subjects: 64,
            ..RateLimitConfig::default()
        });
        for i in 0..10_000 {
            clock.advance(Duration::from_millis(1));
            l.check_request(&subject("acme", &format!("agent-{i}")))
                .unwrap();
        }
        assert!(l.subjects() <= 64, "{} subjects tracked", l.subjects());
        // A recently active subject keeps its (partly spent) bucket; rotating identities cannot reset it.
        let busy = subject("acme", "busy");
        for _ in 0..100 {
            l.check_request(&busy).unwrap();
        }
        assert!(l.check_request(&busy).is_err());
    }

    // spec: INV-02
    #[test]
    fn idle_full_buckets_are_forgotten_after_the_ttl() {
        let (l, clock) = limiter(RateLimitConfig {
            max_subjects: 16,
            idle_ttl: Duration::from_secs(60),
            ..RateLimitConfig::default()
        });
        for i in 0..200 {
            l.check_request(&subject("acme", &format!("a-{i}")))
                .unwrap();
        }
        clock.advance(Duration::from_secs(3_600));
        for i in 0..200 {
            l.check_request(&subject("acme", &format!("b-{i}")))
                .unwrap();
        }
        assert!(l.subjects() <= 16);
    }
}
