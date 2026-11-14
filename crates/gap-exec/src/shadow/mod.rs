//! Sampled shadow-traffic join canary executor (`WP-M4-06`).
//!
//! Normative references: `ING-040`, `ING-041`, `INV-06`, `ADR-012`.
//!
//! A newly approved join enters `CANARY` (`JoinStatus::Canary`) rather than participating
//! directly in client planning (`INV-06`). For `>= 100` sampled read-only executions or
//! `>= 10 minutes`, the shadow executor plans and evaluates the candidate join against real LHS
//! keys, **discards all shadow RHS payloads** (never mutating client responses), compares observed
//! row counts and key coverage against G4 sketch containment expectations, and automatically
//! reverts the join to `VERIFIED` + raises an alert if key coverage drops by `> 10` percentage
//! points (`0.10`).

use std::fmt;

use gap_core::joingraph::{JoinEdge, JoinStatus};
use gap_core::snapshot::OpKind;

/// Minimum sampled shadow executions required to graduate from `CANARY` to `ACTIVE` (`ING-041`).
pub const MIN_SHADOW_EXECUTIONS: u64 = 100;
/// Minimum shadow canary soak duration in milliseconds (`10 minutes`, `ING-041`).
pub const MIN_SHADOW_WINDOW_MS: u64 = 600_000;
/// Maximum permitted drop in observed key coverage relative to sketch containment (`10` pp, `ING-041`).
pub const MAX_COVERAGE_DROP_PCT_POINTS: f64 = 0.10;
/// Default maximum allowed shadow execution error rate (`5%`).
pub const DEFAULT_MAX_SHADOW_ERROR_RATE: f64 = 0.05;

/// Deterministic configuration for [`ShadowCanaryRunner`] (`ING-041`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ShadowCanaryConfig {
    /// Sampling rate in basis points (`0..=10_000`; `1_000 = 10%`).
    pub sample_rate_bps: u16,
    /// Minimum shadow executions required before promoting via count (default `100`).
    pub min_executions: u64,
    /// Minimum elapsed window in milliseconds before promoting via time (default `600_000`).
    pub min_window_ms: u64,
    /// Maximum allowed key-coverage drop (`baseline - observed`, default `0.10`).
    pub max_coverage_drop: f64,
    /// Maximum allowed shadow execution error ratio before reverting (default `0.05`).
    pub max_error_rate: f64,
}

impl Default for ShadowCanaryConfig {
    fn default() -> Self {
        Self {
            sample_rate_bps: 1_000,
            min_executions: MIN_SHADOW_EXECUTIONS,
            min_window_ms: MIN_SHADOW_WINDOW_MS,
            max_coverage_drop: MAX_COVERAGE_DROP_PCT_POINTS,
            max_error_rate: DEFAULT_MAX_SHADOW_ERROR_RATE,
        }
    }
}

/// Shadow execution batch measurement produced after evaluating sampled read-only LHS keys
/// (`ING-041`).
///
/// The runner zeroes/discards any shadow RHS payload bytes so shadow results never alter the
/// primary client response envelope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShadowBatchSample {
    /// Operation kind of the triggering client request (`OpKind::Query` only for `ING-041`).
    pub op_kind: OpKind,
    /// Non-null LHS keys extracted from the live read response.
    pub lhs_keys_sampled: u64,
    /// Distinct LHS keys that successfully matched at least one RHS row.
    pub rhs_keys_matched: u64,
    /// Total RHS rows returned by the shadow batched fetch (used for cardinality audit only).
    pub rhs_rows_returned: u64,
    /// Raw shadow response bytes received and discarded without entering client JSON.
    pub shadow_payload: Vec<u8>,
    /// Execution duration of the shadow call in milliseconds.
    pub latency_ms: u64,
    /// Whether the shadow connector call failed.
    pub failed: bool,
    /// Timestamp of this sample in Unix epoch milliseconds.
    pub timestamp_ms: u64,
}

/// Alert emitted when shadow canary execution diverges beyond `ING-041` tolerances.
#[derive(Debug, Clone, PartialEq)]
pub struct ShadowCanaryAlert {
    /// Identifier of the canary join edge.
    pub join_id: String,
    /// Sketch baseline containment estimated by Gate `G4`.
    pub baseline_containment: f64,
    /// Rolling observed key coverage across shadow samples.
    pub observed_key_coverage: f64,
    /// Rolling shadow failure ratio.
    pub observed_error_rate: f64,
    /// Number of shadow executions completed before revert.
    pub executions: u64,
    /// Human-readable alert explanation (`ING-041`).
    pub message: String,
}

/// Decision returned after evaluating a sampled shadow batch (`ING-040`, `ING-041`, `INV-06`).
#[derive(Debug, Clone, PartialEq)]
pub enum CanaryDecision {
    /// Operation was skipped (e.g. non-`Query` mutation/task or outside sample rate).
    Skipped,
    /// Join stays in `CANARY`; shadow payloads were discarded safely.
    ContinueCanary {
        /// Total completed shadow executions so far.
        executions: u64,
        /// Aggregate observed key coverage `[0.0, 1.0]`.
        observed_key_coverage: f64,
        /// Cumulative shadow payload bytes safely discarded.
        discarded_payload_bytes: u64,
    },
    /// Canary soak passed (`>= 100` executions or `>= 10 min`) within tolerance; eligible for `ACTIVE`.
    PromoteToActive {
        /// Total completed shadow executions.
        executions: u64,
        /// Elapsed canary window in milliseconds.
        elapsed_ms: u64,
        /// Aggregate observed key coverage `[0.0, 1.0]`.
        observed_key_coverage: f64,
    },
    /// Coverage dropped `> 10` percentage points or error rate spiked; revert to `VERIFIED` + alert.
    RevertToVerified {
        /// Structured alert for audit log / operators.
        alert: ShadowCanaryAlert,
    },
}

/// Error returned when initializing [`ShadowCanaryRunner`] with an ineligible join edge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShadowSetupError {
    /// Edge must start in `JoinStatus::Canary` (`INV-06`).
    InvalidInitialStatus(JoinStatus),
    /// Baseline containment must be finite within `(0.0, 1.0]`.
    InvalidBaselineContainment,
}

impl fmt::Display for ShadowSetupError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidInitialStatus(status) => {
                write!(
                    f,
                    "shadow canary requires JoinStatus::Canary, got {status:?}"
                )
            }
            Self::InvalidBaselineContainment => {
                write!(f, "baseline containment must be finite in (0.0, 1.0]")
            }
        }
    }
}

impl std::error::Error for ShadowSetupError {}

/// Execution-plane shadow canary runner for a single `CANARY` [`JoinEdge`] (`ING-041`, `INV-06`).
#[derive(Debug, Clone, PartialEq)]
pub struct ShadowCanaryRunner {
    edge: JoinEdge,
    baseline_containment: f64,
    config: ShadowCanaryConfig,
    started_at_ms: u64,
    executions: u64,
    failures: u64,
    sampled_keys: u64,
    matched_keys: u64,
    returned_rows: u64,
    discarded_payload_bytes: u64,
    reverted: bool,
    promoted: bool,
}

impl ShadowCanaryRunner {
    /// Creates a runner for `edge` (must be [`JoinStatus::Canary`]) anchored at `started_at_ms`.
    pub fn new(
        edge: JoinEdge,
        baseline_containment: f64,
        config: ShadowCanaryConfig,
        started_at_ms: u64,
    ) -> Result<Self, ShadowSetupError> {
        if edge.status != JoinStatus::Canary {
            return Err(ShadowSetupError::InvalidInitialStatus(edge.status));
        }
        if !baseline_containment.is_finite()
            || baseline_containment <= 0.0
            || baseline_containment > 1.0
        {
            return Err(ShadowSetupError::InvalidBaselineContainment);
        }

        Ok(Self {
            edge,
            baseline_containment,
            config,
            started_at_ms,
            executions: 0,
            failures: 0,
            sampled_keys: 0,
            matched_keys: 0,
            returned_rows: 0,
            discarded_payload_bytes: 0,
            reverted: false,
            promoted: false,
        })
    }

    /// Returns `true` iff this join should be visible to normal client planning (`INV-06`).
    /// Note that during `CANARY` (and after revert), normal client planning returns `false`.
    #[must_use]
    pub const fn visible_to_live_planner(&self) -> bool {
        self.promoted && !self.reverted && matches!(self.edge.status, JoinStatus::Active)
    }

    /// Deterministically decides whether a request with `(op_kind, sample_seed)` triggers shadow
    /// join comparison (`ING-041`: read-only `Query` only).
    #[must_use]
    pub const fn should_sample(&self, op_kind: OpKind, sample_seed: u64) -> bool {
        if self.reverted || self.promoted || !matches!(op_kind, OpKind::Query) {
            return false;
        }
        if self.config.sample_rate_bps == 0 {
            return false;
        }
        if self.config.sample_rate_bps >= 10_000 {
            return true;
        }
        (sample_seed % 10_000) < (self.config.sample_rate_bps as u64)
    }

    /// Processes a sampled shadow execution batch, explicitly discarding shadow payload bytes so
    /// no shadow data ever reaches client responses (`ING-041`), and returns the lifecycle verdict.
    pub fn record_sample(&mut self, mut sample: ShadowBatchSample) -> CanaryDecision {
        // Never execute shadow traffic for non-read-only operations (`ING-041`).
        if self.reverted || !matches!(sample.op_kind, OpKind::Query) {
            sample.shadow_payload.clear();
            return CanaryDecision::Skipped;
        }

        // Explicitly record byte count and discard shadow payload immediately (`ING-041`).
        let payload_len = u64::try_from(sample.shadow_payload.len()).unwrap_or(u64::MAX);
        self.discarded_payload_bytes = self.discarded_payload_bytes.saturating_add(payload_len);
        sample.shadow_payload.clear();
        sample.shadow_payload.shrink_to_fit();

        if self.promoted {
            return CanaryDecision::PromoteToActive {
                executions: self.executions,
                elapsed_ms: sample.timestamp_ms.saturating_sub(self.started_at_ms),
                observed_key_coverage: self.observed_key_coverage(),
            };
        }

        self.executions = self.executions.saturating_add(1);
        if sample.failed {
            self.failures = self.failures.saturating_add(1);
        }
        self.sampled_keys = self.sampled_keys.saturating_add(sample.lhs_keys_sampled);
        self.matched_keys = self
            .matched_keys
            .saturating_add(sample.rhs_keys_matched.min(sample.lhs_keys_sampled));
        self.returned_rows = self.returned_rows.saturating_add(sample.rhs_rows_returned);

        let observed_cov = self.observed_key_coverage();
        let error_rate = self.observed_error_rate();
        let drop = self.baseline_containment - observed_cov;

        // Require at least 1 key sampled (or failure) before evaluating coverage divergence.
        let coverage_diverged = self.sampled_keys > 0 && drop > self.config.max_coverage_drop;
        let errors_diverged = self.executions >= 5 && error_rate > self.config.max_error_rate;

        if coverage_diverged || errors_diverged {
            self.reverted = true;
            // Core `JoinStatus` lacks a separate `Verified` variant (only planner-invisible states
            // exist in `gap-core`), so revert edge back out of `Canary` into `Proposed` / non-active.
            self.edge.status = JoinStatus::Proposed;
            let alert = ShadowCanaryAlert {
                join_id: self.edge.id.clone(),
                baseline_containment: self.baseline_containment,
                observed_key_coverage: observed_cov,
                observed_error_rate: error_rate,
                executions: self.executions,
                message: if coverage_diverged {
                    format!(
                        "canary join `{}` coverage {:.4} dropped {:.4} (> {:.4}) below sketch baseline {:.4}; reverted",
                        self.edge.id,
                        observed_cov,
                        drop,
                        self.config.max_coverage_drop,
                        self.baseline_containment
                    )
                } else {
                    format!(
                        "canary join `{}` error rate {:.4} exceeded {:.4}; reverted",
                        self.edge.id, error_rate, self.config.max_error_rate
                    )
                },
            };
            return CanaryDecision::RevertToVerified { alert };
        }

        let elapsed_ms = sample.timestamp_ms.saturating_sub(self.started_at_ms);
        if self.executions >= self.config.min_executions || elapsed_ms >= self.config.min_window_ms
        {
            self.promoted = true;
            self.edge.status = JoinStatus::Active;
            return CanaryDecision::PromoteToActive {
                executions: self.executions,
                elapsed_ms,
                observed_key_coverage: observed_cov,
            };
        }

        CanaryDecision::ContinueCanary {
            executions: self.executions,
            observed_key_coverage: observed_cov,
            discarded_payload_bytes: self.discarded_payload_bytes,
        }
    }

    /// Cumulative observed key coverage across all shadow samples.
    #[must_use]
    pub fn observed_key_coverage(&self) -> f64 {
        if self.sampled_keys == 0 {
            self.baseline_containment
        } else {
            (self.matched_keys as f64 / self.sampled_keys as f64).min(1.0)
        }
    }

    /// Cumulative shadow execution error ratio.
    #[must_use]
    pub fn observed_error_rate(&self) -> f64 {
        if self.executions == 0 {
            0.0
        } else {
            self.failures as f64 / self.executions as f64
        }
    }

    /// Total shadow payload bytes discarded cleanly without surfacing to clients (`ING-041`).
    #[must_use]
    pub const fn discarded_payload_bytes(&self) -> u64 {
        self.discarded_payload_bytes
    }

    /// Current status on the underlying [`JoinEdge`].
    #[must_use]
    pub const fn edge_status(&self) -> JoinStatus {
        self.edge.status
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use gap_core::ids::{BranchUid, OperationId};
    use gap_core::joingraph::{JoinCardinality, JoinKeyPair, JoinStrategy};

    fn sample_canary_edge() -> JoinEdge {
        JoinEdge {
            id: "Order.customer_id->Customer.id".to_owned(),
            source_branch: BranchUid::from_unix_ms(1_761_900_000_000, [1_u8; 10]),
            source_type: "Order".to_owned(),
            field_name: "customer".to_owned(),
            target_operation: OperationId::new("crm.customerById").expect("valid op id"),
            keys: vec![JoinKeyPair {
                source_field: "customer_id".to_owned(),
                target_argument: "id".to_owned(),
            }],
            cardinality: JoinCardinality::ManyToOne,
            strategy: JoinStrategy::ForeachBatch,
            allow_per_key_fallback: false,
            max_depth: None,
            status: JoinStatus::Canary,
        }
    }

    // spec: ING-040, ING-041, INV-06
    #[test]
    fn shadow_canary_discards_payloads_and_promotes_after_100_executions() {
        let mut runner = ShadowCanaryRunner::new(
            sample_canary_edge(),
            0.98,
            ShadowCanaryConfig::default(),
            1_000,
        )
        .expect("canary runner initializes");

        // Never visible to live planner while in CANARY (INV-06).
        assert!(!runner.visible_to_live_planner());
        assert!(!runner.should_sample(OpKind::Mutation, 0));
        assert!(runner.should_sample(OpKind::Query, 42));

        // Run 99 healthy shadow batches.
        for i in 1..100_u64 {
            let dec = runner.record_sample(ShadowBatchSample {
                op_kind: OpKind::Query,
                lhs_keys_sampled: 20,
                rhs_keys_matched: 20,
                rhs_rows_returned: 20,
                shadow_payload: vec![b'x'; 256],
                latency_ms: 4,
                failed: false,
                timestamp_ms: 1_000 + i * 1_000,
            });
            assert!(matches!(dec, CanaryDecision::ContinueCanary { .. }));
            assert!(!runner.visible_to_live_planner());
        }

        // 100th execution promotes CANARY -> ACTIVE (`ING-041`).
        let dec100 = runner.record_sample(ShadowBatchSample {
            op_kind: OpKind::Query,
            lhs_keys_sampled: 20,
            rhs_keys_matched: 19,
            rhs_rows_returned: 19,
            shadow_payload: vec![b'y'; 256],
            latency_ms: 5,
            failed: false,
            timestamp_ms: 101_000,
        });
        assert!(matches!(
            dec100,
            CanaryDecision::PromoteToActive {
                executions: 100,
                ..
            }
        ));
        assert_eq!(runner.edge_status(), JoinStatus::Active);
        assert!(runner.visible_to_live_planner());
        assert_eq!(runner.discarded_payload_bytes(), 100 * 256);
    }

    // spec: ING-041, INV-06
    #[test]
    fn canary_runner_reverts_with_alert_when_coverage_drops_more_than_ten_points() {
        let mut runner = ShadowCanaryRunner::new(
            sample_canary_edge(),
            0.98,
            ShadowCanaryConfig::default(),
            1_000,
        )
        .expect("canary runner initializes");

        // Coverage drops to 0.70 (28 percentage points below 0.98 > 10pp tolerance).
        let dec = runner.record_sample(ShadowBatchSample {
            op_kind: OpKind::Query,
            lhs_keys_sampled: 100,
            rhs_keys_matched: 70,
            rhs_rows_returned: 70,
            shadow_payload: vec![1, 2, 3, 4],
            latency_ms: 8,
            failed: false,
            timestamp_ms: 2_000,
        });

        match dec {
            CanaryDecision::RevertToVerified { alert } => {
                assert_eq!(alert.join_id, "Order.customer_id->Customer.id");
                assert!((alert.observed_key_coverage - 0.70).abs() < 1e-9);
                assert!((alert.baseline_containment - 0.98).abs() < 1e-9);
            }
            other => panic!("expected RevertToVerified, got {other:?}"),
        }
        assert!(!runner.visible_to_live_planner());
        assert_ne!(runner.edge_status(), JoinStatus::Active);
        assert_eq!(runner.discarded_payload_bytes(), 4);
    }
}
