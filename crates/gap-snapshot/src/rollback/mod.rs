//! Attribution-aware 60 s error watch window, flap dampening, and automatic single-step
//! rollback (`WP-M4-08`).
//!
//! Normative references: `RUN-007`, `RUN-008`, `INV-16`, `ADR-009` §5.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use gap_core::ids::BranchUid;
use gap_core::snapshot::Snapshot;

/// Default post-promotion watch window duration (`60 s`, `ADR-009` §5).
pub const DEFAULT_WATCH_WINDOW_SECS: u64 = 60;
/// Minimum attributable call samples before triggering automatic rollback (`200`, `ADR-009` §5).
pub const DEFAULT_MIN_WATCH_SAMPLES: u64 = 200;
/// Baseline multiplier threshold (`2x baseline`, `ADR-009` §5).
pub const DEFAULT_ERROR_RATE_MULTIPLIER: f64 = 2.0;
/// Additive error-rate floor threshold (`+2 percentage points`, `ADR-009` §5).
pub const DEFAULT_ERROR_RATE_DELTA: f64 = 0.02;
/// Maximum automatic quarantines allowed inside `flap_window` before hard quarantine (`3` in `24h`).
pub const DEFAULT_FLAP_MAX_QUARANTINES: usize = 3;
/// Sliding window duration for flap dampening (`24 h`, `86_400 s`).
pub const DEFAULT_FLAP_WINDOW_SECS: u64 = 86_400;

/// Configuration governing automatic post-publish attribution watch and flap dampening
/// (`RUN-007`, `RUN-008`, `ADR-009` §5).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WatchConfig {
    /// Post-promotion watch window (default `60 s`).
    pub watch_window: Duration,
    /// Minimum attributable request/operation samples (default `200`).
    pub min_samples: u64,
    /// Threshold multiplier vs previous-generation baseline (`2.0`).
    pub multiplier: f64,
    /// Additive threshold vs previous-generation baseline (`0.02`).
    pub min_delta: f64,
    /// Auto-quarantines within `flap_window` before hard quarantine (`3`).
    pub max_quarantines_in_window: usize,
    /// Flap-dampening horizon (`24 h`).
    pub flap_window: Duration,
}

impl Default for WatchConfig {
    fn default() -> Self {
        Self {
            watch_window: Duration::from_secs(DEFAULT_WATCH_WINDOW_SECS),
            min_samples: DEFAULT_MIN_WATCH_SAMPLES,
            multiplier: DEFAULT_ERROR_RATE_MULTIPLIER,
            min_delta: DEFAULT_ERROR_RATE_DELTA,
            max_quarantines_in_window: DEFAULT_FLAP_MAX_QUARANTINES,
            flap_window: Duration::from_secs(DEFAULT_FLAP_WINDOW_SECS),
        }
    }
}

/// Classification of an execution/planning observation fed to [`RollbackWatcher`] (`ADR-009` §5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WatchSignal {
    /// Execution against a specific branch (`branch_uid`); attributable iff `branch_uid` was added
    /// or its `spec_hash` changed between parent generation and current generation.
    BranchCall {
        /// Branch identifier.
        branch_uid: BranchUid,
        /// `true` if this call failed due to a schema/connector validation/execution mismatch.
        failed: bool,
    },
    /// Snapshot planning/ownership/validation error (always snapshot-attributable).
    PlanOrValidation {
        /// `true` when planning or validation errored.
        failed: bool,
    },
    /// Execution against a newly promoted cross-branch join edge.
    JoinCall {
        /// Target join edge identifier.
        join_id: String,
        /// `true` if execution/key coverage failed.
        failed: bool,
    },
    /// Unrelated infrastructure failure (e.g. host network blip, power/LB transient) that MUST NOT
    /// trigger snapshot rollback (`ADR-009` §5).
    UnrelatedInfraFailure,
}

/// Action recommended or triggered by [`RollbackWatcher::evaluate`] (`RUN-007`, `ADR-009` §5).
#[derive(Debug, Clone, PartialEq)]
pub enum RollbackDecision {
    /// Stay on the current generation; error rates are within baseline tolerance or outside window.
    Healthy,
    /// Fault isolated to a specific changed branch (`spec_hash` changed or newly added in `G+1`):
    /// quarantine ONLY that branch while preserving healthy branches from the generation.
    QuarantineBranch {
        /// Current bad generation being replaced.
        bad_generation: u64,
        /// Clean prior generation to restore `bad_branch` state from / roll back against.
        clean_generation: u64,
        /// Guilty branch UID.
        branch_uid: BranchUid,
        /// Attributable error ratio observed for `branch_uid`.
        observed_rate: f64,
        /// `true` if flap dampening escalated this branch to `HardQuarantined` (>= 3 in 24h).
        hard_quarantined: bool,
    },
    /// Fault isolated to a newly activated join edge `join_id`: quarantine ONLY that join edge.
    QuarantineJoin {
        /// Current bad generation.
        bad_generation: u64,
        /// Clean prior generation.
        clean_generation: u64,
        /// Faulty join edge identifier.
        join_id: String,
        /// Attributable error ratio observed on `join_id`.
        observed_rate: f64,
    },
    /// Unattributed / global planning or composition regression: roll back the entire generation
    /// cleanly to `clean_generation` as a new higher monotonic generation (`INV-16`, `RUN-007`).
    FullRollback {
        /// Bad generation to mark `bad = true`.
        bad_generation: u64,
        /// Clean prior generation to restore.
        clean_generation: u64,
        /// Global snapshot-attributable error ratio.
        observed_rate: f64,
    },
}

/// Admin acknowledgment record required to clear a flap-locked (`HardQuarantined`) branch (`RUN-008`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClearQuarantineAck {
    /// Authenticated administrator principal ID.
    pub actor: String,
    /// Non-empty justification for re-enabling the flapping branch.
    pub reason: String,
}

impl ClearQuarantineAck {
    /// Returns `true` when both `actor` and `reason` are non-empty.
    #[must_use]
    pub fn is_valid(&self) -> bool {
        !self.actor.trim().is_empty() && !self.reason.trim().is_empty()
    }
}

#[derive(Debug, Clone, Default)]
struct BranchCounters {
    total: u64,
    failed: u64,
}

#[derive(Debug, Clone)]
struct ActiveWatch {
    generation: u64,
    clean_generation: u64,
    baseline_error_rate: f64,
    changed_branches: BTreeSet<BranchUid>,
    added_joins: BTreeSet<String>,
    total_attributable: u64,
    failed_attributable: u64,
    plan_total: u64,
    plan_failed: u64,
    per_branch: BTreeMap<BranchUid, BranchCounters>,
    per_join: BTreeMap<String, BranchCounters>,
    infra_ignored_count: u64,
    triggered: bool,
}

/// Stateful attribution monitor tracking one promoted snapshot over a 60 s watch window
/// (`RUN-007`, `RUN-008`, `INV-16`, `ADR-009` §5).
#[derive(Debug)]
pub struct RollbackWatcher {
    config: WatchConfig,
    watch: Option<ActiveWatch>,
    quarantine_history: BTreeMap<BranchUid, Vec<u64>>,
    hard_quarantined: BTreeMap<BranchUid, String>,
}

impl RollbackWatcher {
    /// Creates a [`RollbackWatcher`] with `config`.
    #[must_use]
    pub fn new(config: WatchConfig) -> Self {
        Self {
            config,
            watch: None,
            quarantine_history: BTreeMap::new(),
            hard_quarantined: BTreeMap::new(),
        }
    }

    /// Starts watching newly promoted `current` against parent `previous` using `baseline_error_rate`
    /// measured on `previous` (`ADR-009` §5).
    pub fn start_watch(
        &mut self,
        previous: &Snapshot,
        current: &Snapshot,
        baseline_error_rate: f64,
    ) {
        let mut changed_branches = BTreeSet::new();
        for (uid, desc) in &current.branches {
            match previous.branches.get(uid) {
                None => {
                    changed_branches.insert(*uid);
                }
                Some(prev_desc)
                    if prev_desc.spec_hash != desc.spec_hash
                        || prev_desc.pinned_desc_hash != desc.pinned_desc_hash =>
                {
                    changed_branches.insert(*uid);
                }
                Some(_) => {}
            }
        }

        let prev_joins: BTreeSet<&str> = previous
            .joins
            .active_edges()
            .map(|e| e.id.as_str())
            .collect();
        let added_joins: BTreeSet<String> = current
            .joins
            .active_edges()
            .filter(|e| !prev_joins.contains(e.id.as_str()))
            .map(|e| e.id.clone())
            .collect();

        self.watch = Some(ActiveWatch {
            generation: current.generation,
            clean_generation: previous.generation,
            baseline_error_rate: baseline_error_rate.clamp(0.0, 1.0),
            changed_branches,
            added_joins,
            total_attributable: 0,
            failed_attributable: 0,
            plan_total: 0,
            plan_failed: 0,
            per_branch: BTreeMap::new(),
            per_join: BTreeMap::new(),
            infra_ignored_count: 0,
            triggered: false,
        });
    }

    /// Records one execution/planning signal into the active watch window.
    pub fn record(&mut self, signal: WatchSignal) {
        let Some(ref mut w) = self.watch else {
            return;
        };
        if w.triggered {
            return;
        }

        match signal {
            WatchSignal::UnrelatedInfraFailure => {
                // `ADR-009` §5: unrelated infrastructure errors must NEVER count toward rollback.
                w.infra_ignored_count = w.infra_ignored_count.saturating_add(1);
            }
            WatchSignal::PlanOrValidation { failed } => {
                w.total_attributable = w.total_attributable.saturating_add(1);
                w.plan_total = w.plan_total.saturating_add(1);
                if failed {
                    w.failed_attributable = w.failed_attributable.saturating_add(1);
                    w.plan_failed = w.plan_failed.saturating_add(1);
                }
            }
            WatchSignal::BranchCall { branch_uid, failed } => {
                if w.changed_branches.contains(&branch_uid) {
                    w.total_attributable = w.total_attributable.saturating_add(1);
                    let entry = w.per_branch.entry(branch_uid).or_default();
                    entry.total = entry.total.saturating_add(1);
                    if failed {
                        w.failed_attributable = w.failed_attributable.saturating_add(1);
                        entry.failed = entry.failed.saturating_add(1);
                    }
                }
            }
            WatchSignal::JoinCall { join_id, failed } => {
                if w.added_joins.contains(&join_id) {
                    w.total_attributable = w.total_attributable.saturating_add(1);
                    let entry = w.per_join.entry(join_id).or_default();
                    entry.total = entry.total.saturating_add(1);
                    if failed {
                        w.failed_attributable = w.failed_attributable.saturating_add(1);
                        entry.failed = entry.failed.saturating_add(1);
                    }
                }
            }
        }
    }

    /// Returns count of unrelated infrastructure failures safely excluded from attribution.
    #[must_use]
    pub fn ignored_infra_count(&self) -> u64 {
        self.watch.as_ref().map_or(0, |w| w.infra_ignored_count)
    }

    /// Evaluates whether the active watch generation crossed `max(2 * baseline, baseline + 0.02)`
    /// with `>= 200` attributable samples inside the `60 s` watch window (`ADR-009` §5).
    ///
    /// Rolls back **at most once** per generation (`triggered = true`) and tracks per-branch flap
    /// history at `now_ms`.
    pub fn evaluate(&mut self, elapsed: Duration, now_ms: u64) -> RollbackDecision {
        let Some(ref mut w) = self.watch else {
            return RollbackDecision::Healthy;
        };
        if w.triggered
            || elapsed > self.config.watch_window
            || w.total_attributable < self.config.min_samples
        {
            return RollbackDecision::Healthy;
        }

        let threshold = f64::max(
            self.config.multiplier * w.baseline_error_rate,
            w.baseline_error_rate + self.config.min_delta,
        );
        let overall_rate = w.failed_attributable as f64 / w.total_attributable as f64;

        // First check if failure concentrates cleanly in a single changed branch with substantial calls.
        let bad_gen = w.generation;
        let clean_gen = w.clean_generation;
        let min_branch_samples = self.config.min_samples / 4;

        let mut offending_branches: Vec<(BranchUid, f64, u64)> = w
            .per_branch
            .iter()
            .filter_map(|(&uid, c)| {
                if c.total >= min_branch_samples.max(1) {
                    let rate = c.failed as f64 / c.total as f64;
                    if rate > threshold {
                        return Some((uid, rate, c.failed));
                    }
                }
                None
            })
            .collect();
        offending_branches.sort_by(|a, b| b.2.cmp(&a.2).then_with(|| a.0.cmp(&b.0)));

        let plan_rate = if w.plan_total == 0 {
            0.0
        } else {
            w.plan_failed as f64 / w.plan_total as f64
        };

        // Isolated changed branch fault (planning itself is healthy).
        if offending_branches.len() == 1 && plan_rate <= threshold && overall_rate > threshold {
            let (branch_uid, rate, _) = offending_branches[0];
            w.triggered = true;
            let hard = self.record_quarantine_event(branch_uid, now_ms);
            return RollbackDecision::QuarantineBranch {
                bad_generation: bad_gen,
                clean_generation: clean_gen,
                branch_uid,
                observed_rate: rate,
                hard_quarantined: hard,
            };
        }

        // Isolated newly added join fault.
        let mut offending_joins: Vec<(String, f64, u64)> = w
            .per_join
            .iter()
            .filter_map(|(jid, c)| {
                if c.total >= min_branch_samples.max(1) {
                    let rate = c.failed as f64 / c.total as f64;
                    if rate > threshold {
                        return Some((jid.clone(), rate, c.failed));
                    }
                }
                None
            })
            .collect();
        offending_joins.sort_by(|a, b| b.2.cmp(&a.2).then_with(|| a.0.cmp(&b.0)));

        if offending_joins.len() == 1
            && offending_branches.is_empty()
            && plan_rate <= threshold
            && overall_rate > threshold
        {
            let (join_id, rate, _) = offending_joins.swap_remove(0);
            w.triggered = true;
            return RollbackDecision::QuarantineJoin {
                bad_generation: bad_gen,
                clean_generation: clean_gen,
                join_id,
                observed_rate: rate,
            };
        }

        if overall_rate > threshold {
            w.triggered = true;
            return RollbackDecision::FullRollback {
                bad_generation: bad_gen,
                clean_generation: clean_gen,
                observed_rate: overall_rate,
            };
        }

        RollbackDecision::Healthy
    }

    fn record_quarantine_event(&mut self, branch_uid: BranchUid, now_ms: u64) -> bool {
        let horizon_ms = u64::try_from(self.config.flap_window.as_millis()).unwrap_or(u64::MAX);
        let cutoff = now_ms.saturating_sub(horizon_ms);

        let hist = self.quarantine_history.entry(branch_uid).or_default();
        hist.retain(|&ts| ts >= cutoff);
        hist.push(now_ms);

        if hist.len() >= self.config.max_quarantines_in_window {
            self.hard_quarantined.insert(
                branch_uid,
                format!(
                    "branch triggered >= {} automatic rollbacks within 24h",
                    self.config.max_quarantines_in_window
                ),
            );
            true
        } else {
            false
        }
    }

    /// Returns `true` if `branch_uid` is currently flap-locked (`HardQuarantined`) (`RUN-008`).
    #[must_use]
    pub fn is_hard_quarantined(&self, branch_uid: BranchUid) -> bool {
        self.hard_quarantined.contains_key(&branch_uid)
    }

    /// Clears a flap-dampened `HardQuarantined` lock only with a valid audited admin ack.
    pub fn clear_hard_quarantine(
        &mut self,
        branch_uid: BranchUid,
        ack: &ClearQuarantineAck,
    ) -> Result<(), &'static str> {
        if !ack.is_valid() {
            return Err("valid non-empty ClearQuarantineAck is required");
        }
        self.hard_quarantined.remove(&branch_uid);
        self.quarantine_history.remove(&branch_uid);
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    use gap_core::ids::BranchId;
    use gap_core::snapshot::{BranchStatus, Endpoint, Protocol, TrustTier};
    use gap_proto::ngc::v1::{
        Capabilities as ProtoCapabilities, ConnectorDescription, Limits as ProtoLimits,
        MetaSummary, OpKind as ProtoOpKind, OperationDescriptor as ProtoOp, SideEffectHint,
    };

    use crate::compose::{BranchInput, compose};
    use crate::handle::SnapshotBuilder;
    use crate::publisher::{PublishOptions, PublisherActor, PublisherConfig};

    fn uid(idx: u16) -> BranchUid {
        BranchUid::from_unix_ms(1_761_900_000_000 + u64::from(idx), [idx as u8; 10])
    }

    fn branch_input(idx: u16, rev: u32) -> BranchInput {
        let slug = format!("br{idx:02}");
        let field = format!("op{idx:02}");
        BranchInput {
            id: BranchId::new(&slug).unwrap(),
            uid: uid(idx),
            protocol: Protocol::Rest,
            trust: TrustTier::T1,
            endpoint: Endpoint {
                uri: format!("https://{slug}.internal/ngc"),
                transport: "grpc".to_owned(),
            },
            description: ConnectorDescription {
                connector_id: slug,
                connector_version: format!("0.{rev}.0"),
                ngc_version: "gap/0.1".to_owned(),
                protocol: gap_proto::ngc::v1::Protocol::Rest as i32,
                spec_hash: format!("{idx:062x}{rev:02x}"),
                capabilities: Some(ProtoCapabilities::default()),
                limits: Some(ProtoLimits::default()),
                schema_sdl: format!("type Query {{ {field}(id: ID!): String }}\n"),
                operations: vec![ProtoOp {
                    name: field,
                    kind: ProtoOpKind::Query as i32,
                    description: "read".to_owned(),
                    input_schema: None,
                    output_schema: None,
                    side_effect_hint: SideEffectHint::SeNone as i32,
                    idempotent_hint: true,
                    supports_dry_run: false,
                    compensation_operation: String::new(),
                    examples: Vec::new(),
                    cost_weight: 1,
                    pagination: 0,
                    key_fields: Vec::new(),
                    annotations: HashMap::new(),
                }],
                meta: Some(MetaSummary {
                    node_categories: Vec::new(),
                    edge_predicates: Vec::new(),
                    synopsis_hint: "service".to_owned(),
                }),
                auth: None,
            },
            status: BranchStatus::Active,
        }
    }

    // spec: RUN-007, RUN-008, INV-16
    #[test]
    fn rollback_watcher_isolates_single_bad_branch_ignores_infra_and_dampens_flapping() {
        let base = Snapshot::empty(0);
        let comp1 = compose(&[branch_input(1, 1), branch_input(2, 1)]).unwrap();
        let gen1 = SnapshotBuilder::new(&base, comp1).build();
        let comp2 = compose(&[branch_input(1, 2), branch_input(2, 1)]).unwrap();
        let gen2 = SnapshotBuilder::new(&gen1, comp2).build();

        let actor = PublisherActor::new(gen1.clone(), PublisherConfig::default());
        actor
            .publish(gen2.clone(), &PublishOptions::default(), Vec::new())
            .unwrap();

        let mut watcher = RollbackWatcher::new(WatchConfig::default());
        watcher.start_watch(&gen1, &gen2, 0.005);

        // Flood 500 unrelated infra failures -> MUST NEVER trigger snapshot rollback (`ADR-009` §5).
        for _ in 0..500 {
            watcher.record(WatchSignal::UnrelatedInfraFailure);
        }
        // Plus 200 healthy branch 2 (unchanged) + 190 healthy + 10 failing? No: 199 healthy branch 1 calls (< 2% delta).
        for _ in 0..200 {
            watcher.record(WatchSignal::BranchCall {
                branch_uid: uid(1),
                failed: false,
            });
            watcher.record(WatchSignal::BranchCall {
                branch_uid: uid(2),
                failed: true, // branch 2 did NOT change spec_hash -> not attributed to gen2 rollout
            });
        }
        assert_eq!(watcher.ignored_infra_count(), 500);
        assert_eq!(
            watcher.evaluate(Duration::from_secs(10), 1_000),
            RollbackDecision::Healthy
        );

        // Now branch 1 (whose spec_hash changed in gen2) emits 40 failing calls out of 200 new samples -> ~10% error rate > max(1%, 2.5%).
        let mut watcher2 = RollbackWatcher::new(WatchConfig::default());
        watcher2.start_watch(&gen1, &gen2, 0.005);
        for i in 0..200 {
            watcher2.record(WatchSignal::BranchCall {
                branch_uid: uid(1),
                failed: i < 20, // 10% failure rate
            });
        }
        let decision = watcher2.evaluate(Duration::from_secs(15), 10_000);
        match decision {
            RollbackDecision::QuarantineBranch {
                bad_generation,
                clean_generation,
                branch_uid,
                hard_quarantined,
                ..
            } => {
                assert_eq!(bad_generation, 2);
                assert_eq!(clean_generation, 1);
                assert_eq!(branch_uid, uid(1));
                assert!(!hard_quarantined);
                let rec = actor
                    .quarantine_branch_and_publish(
                        branch_uid,
                        clean_generation,
                        bad_generation,
                        "isolated branch 1 fault",
                    )
                    .expect("quarantine publish succeeds");
                assert_eq!(rec.generation, 3);
                assert_eq!(
                    actor.pin().branches.get(&uid(1)).unwrap().status,
                    BranchStatus::Quarantined
                );
                assert_eq!(
                    actor.pin().branches.get(&uid(2)).unwrap().status,
                    BranchStatus::Active
                );
            }
            other => panic!("expected QuarantineBranch, got {other:?}"),
        }

        // Subsequent evaluate on same generation rolls back at most once (`ADR-009` §5).
        assert_eq!(
            watcher2.evaluate(Duration::from_secs(20), 11_000),
            RollbackDecision::Healthy
        );

        // Flap dampening: 3 auto-quarantines within 24h lock branch 1 into HardQuarantined.
        for cycle in 2..=3_u64 {
            watcher2.start_watch(&gen1, &gen2, 0.0);
            for i in 0..200 {
                watcher2.record(WatchSignal::BranchCall {
                    branch_uid: uid(1),
                    failed: i < 15,
                });
            }
            let d = watcher2.evaluate(Duration::from_secs(5), 10_000 + cycle * 60_000);
            if cycle == 3 {
                assert!(matches!(
                    d,
                    RollbackDecision::QuarantineBranch {
                        hard_quarantined: true,
                        ..
                    }
                ));
            }
        }
        assert!(watcher2.is_hard_quarantined(uid(1)));

        watcher2
            .clear_hard_quarantine(
                uid(1),
                &ClearQuarantineAck {
                    actor: "sre@acme.test".to_owned(),
                    reason: "connector schema patched and verified".to_owned(),
                },
            )
            .expect("valid admin ack clears hard quarantine");
        assert!(!watcher2.is_hard_quarantined(uid(1)));
    }

    // spec: RUN-007, INV-16
    #[test]
    fn rollback_watcher_full_rollback_on_unattributed_planning_regression() {
        let base = Snapshot::empty(0);
        let comp1 = compose(&[branch_input(1, 1)]).unwrap();
        let gen1 = SnapshotBuilder::new(&base, comp1).build();
        let comp2 = compose(&[branch_input(1, 2)]).unwrap();
        let gen2 = SnapshotBuilder::new(&gen1, comp2).build();

        let actor = PublisherActor::new(gen1.clone(), PublisherConfig::default());
        actor
            .publish(gen2.clone(), &PublishOptions::default(), Vec::new())
            .unwrap();

        let mut watcher = RollbackWatcher::new(WatchConfig::default());
        watcher.start_watch(&gen1, &gen2, 0.001);
        for i in 0..250 {
            watcher.record(WatchSignal::PlanOrValidation { failed: i < 25 });
        }
        let d = watcher.evaluate(Duration::from_secs(30), 50_000);
        match d {
            RollbackDecision::FullRollback {
                bad_generation,
                clean_generation,
                ..
            } => {
                assert_eq!(bad_generation, 2);
                assert_eq!(clean_generation, 1);
                let rec = actor
                    .rollback_to_generation(
                        clean_generation,
                        bad_generation,
                        "global plan regression",
                        false,
                    )
                    .expect("rollback generates gen 3");
                assert_eq!(rec.generation, 3);
                assert_eq!(actor.pin().generation, 3);
            }
            other => panic!("expected FullRollback, got {other:?}"),
        }
    }
}
