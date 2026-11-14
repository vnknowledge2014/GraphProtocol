//! Gate `G8` regression replay on cached/persisted operations and conformance fixtures
//! (`WP-M4-06`).
//!
//! Normative references: `ING-040`, `ING-041`, `INV-06`, `spec/05-ingestion.md` §5.

use std::collections::BTreeMap;

use gap_core::snapshot::OpKind;

use crate::cisg::{Cisg, CisgValue, OperationDef, TypeRef};
use crate::gates::join_verify::MAX_CANARY_COVERAGE_DROP;
use crate::gates::{AdminAck, GateFailAction, GateKind};

/// Default top-N cached/persisted operations replayed by Gate `G8` (`spec/05-ingestion.md` §5).
pub const DEFAULT_REPLAY_TOP_N: usize = 200;

/// Minimum fraction of replayed operations that must re-plan without errors unless
/// `breaking_change_ack` is present (soft non-breaking floor / `ADR-009`).
pub const DEFAULT_MIN_REPLAY_PASS_RATE: f64 = 0.95;

/// Synthetic gate identifier label for Gate `G8` regression replay reports.
pub const GATE_G8_LABEL: &str = "G8";

/// Cached operation, persisted query, or conformance fixture replayed against a candidate `CISG`
/// (`spec/05-ingestion.md` §5, `ING-040`).
#[derive(Debug, Clone, PartialEq)]
pub struct ReplayOperation {
    /// Stable query / fixture identifier.
    pub id: String,
    /// Target operation name in the branch `CISG`.
    pub operation_name: String,
    /// Expected operation kind (`Query`, `Mutation`, `Subscription`, `Task`).
    pub expected_kind: OpKind,
    /// Expected leaf return type name (if asserted by the operation).
    pub expected_return_type: Option<String>,
    /// Required argument names and their expected leaf scalar/type names.
    pub required_args: Vec<(String, String)>,
    /// Selected fields `(type_name, field_name, leaf_type_name)` that must remain compatible.
    pub selected_fields: Vec<(String, String, String)>,
    /// Whether this fixture is part of the mandatory conformance suite (`true`) or cached query.
    pub is_conformance_fixture: bool,
    /// Expected row count baseline (from live / sketch baselines) for read-only shadow diffing.
    pub expected_row_count: usize,
    /// Expected key coverage `[0.0, 1.0]` from Gate `G4` sketches.
    pub expected_key_coverage: f64,
}

/// Read-only shadow execution diff observed while replaying `operation_id` on sampled traffic
/// (`ING-041`, `spec/05-ingestion.md` §5).
#[derive(Debug, Clone, PartialEq)]
pub struct ShadowReplayObservation {
    /// Identifier matching [`ReplayOperation::id`].
    pub operation_id: String,
    /// Whether the operation was strictly read-only (`OpKind::Query`).
    pub read_only: bool,
    /// Whether shadow response bytes were discarded immediately after diffing (`ING-041`).
    pub response_discarded: bool,
    /// Observed row count returned in shadow execution.
    pub observed_row_count: usize,
    /// Observed join key coverage `[0.0, 1.0]` in shadow execution.
    pub observed_key_coverage: f64,
    /// Optional structured error code returned during shadow execution.
    pub error_code: Option<String>,
}

/// Configuration for Gate `G8` regression replay (`spec/05-ingestion.md` §5).
#[derive(Debug, Clone, PartialEq)]
pub struct ReplayConfig {
    /// Maximum cached/persisted operations replayed (default `200`).
    pub max_operations: usize,
    /// Minimum re-plan success ratio (default `0.95`).
    pub min_pass_rate: f64,
    /// Maximum tolerated drop in key coverage vs sketch expectations (`0.10`, `ING-041`).
    pub max_coverage_drop: f64,
    /// Audited admin acknowledgment permitting controlled breaking changes.
    pub breaking_change_ack: Option<AdminAck>,
}

impl Default for ReplayConfig {
    fn default() -> Self {
        Self {
            max_operations: DEFAULT_REPLAY_TOP_N,
            min_pass_rate: DEFAULT_MIN_REPLAY_PASS_RATE,
            max_coverage_drop: MAX_CANARY_COVERAGE_DROP,
            breaking_change_ack: None,
        }
    }
}

/// Classification of a replay difference discovered by Gate `G8` (`spec/05-ingestion.md` §5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ReplayDiffSeverity {
    /// Backward-compatible delta (e.g. non-breaking shadow row variance within bounds).
    NonBreaking,
    /// Removed operation, removed field, incompatible type change, or mutated response leaked.
    Breaking,
}

/// Detailed finding produced for a replayed operation or conformance fixture.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplayDiff {
    /// Replayed operation identifier.
    pub operation_id: String,
    /// Target operation name.
    pub operation_name: String,
    /// Breaking vs non-breaking classification.
    pub severity: ReplayDiffSeverity,
    /// Concise human-readable reason.
    pub message: String,
}

/// Verdict returned by Gate `G8` regression replay (`ING-040`, `ING-041`).
#[derive(Debug, Clone, PartialEq)]
pub struct ReplayReport {
    /// Number of operations & conformance fixtures evaluated (capped by `max_operations`).
    pub replayed_count: usize,
    /// Number of replayed items passing without diffs.
    pub passed_count: usize,
    /// Breaking schema/plan diffs encountered.
    pub breaking_diffs: Vec<ReplayDiff>,
    /// Non-breaking warnings/diffs encountered.
    pub non_breaking_diffs: Vec<ReplayDiff>,
    /// Joins or operations exhibiting shadow key-coverage divergence `> max_coverage_drop`.
    pub coverage_regressions: Vec<String>,
    /// Gate kind effective for this run (`Hard` when breaking diffs exist, `Soft` otherwise).
    pub effective_kind: GateKind,
    /// Whether `G8` permits promotion (`can_activate_join(..., g8_passed, ...)`).
    pub passed: bool,
    /// Optional prescribed failure action (`Failed` on blocking failure).
    pub fail_action: Option<GateFailAction>,
    /// Audited `breaking_change_ack` if applied.
    pub applied_ack: Option<AdminAck>,
    /// Canonical `CISG` evidence map for inclusion in verification reports.
    pub evidence: BTreeMap<String, CisgValue>,
}

/// Evaluates Gate `G8` regression replay across top-N cached operations and conformance fixtures
/// against candidate schema `candidate` and optional read-only `shadow_observations` (`ING-040`,
/// `ING-041`, `spec/05-ingestion.md` §5).
#[must_use]
pub fn evaluate_gate_g8_replay(
    candidate: &Cisg,
    operations: &[ReplayOperation],
    shadow_observations: &[ShadowReplayObservation],
    cfg: &ReplayConfig,
) -> ReplayReport {
    let op_index: BTreeMap<&str, &OperationDef> = candidate
        .operations
        .iter()
        .map(|op| (op.name.as_str(), op))
        .collect();

    let mut type_fields: BTreeMap<(&str, &str), String> = BTreeMap::new();
    for ty in &candidate.types {
        for field in &ty.fields {
            type_fields.insert(
                (ty.name.as_str(), field.name.as_str()),
                leaf_type_name(&field.field_type).to_owned(),
            );
        }
    }

    let obs_by_id: BTreeMap<&str, &ShadowReplayObservation> = shadow_observations
        .iter()
        .map(|obs| (obs.operation_id.as_str(), obs))
        .collect();

    let limit = cfg.max_operations.max(1);
    // Prioritize conformance fixtures first, then cached operations, capped deterministically.
    let mut ordered: Vec<&ReplayOperation> = operations
        .iter()
        .filter(|op| op.is_conformance_fixture)
        .chain(operations.iter().filter(|op| !op.is_conformance_fixture))
        .take(limit)
        .collect();
    ordered.sort_by(|a, b| a.id.cmp(&b.id));

    let replayed_count = ordered.len();
    let mut passed_count = 0_usize;
    let mut breaking_diffs = Vec::new();
    let mut non_breaking_diffs = Vec::new();
    let mut coverage_regressions = Vec::new();

    for op in ordered {
        let mut op_breaking = false;
        let mut op_warning = false;

        match op_index.get(op.operation_name.as_str()) {
            None => {
                op_breaking = true;
                breaking_diffs.push(ReplayDiff {
                    operation_id: op.id.clone(),
                    operation_name: op.operation_name.clone(),
                    severity: ReplayDiffSeverity::Breaking,
                    message: format!(
                        "operation `{}` removed from candidate CISG",
                        op.operation_name
                    ),
                });
            }
            Some(def) => {
                if def.kind != op.expected_kind {
                    op_breaking = true;
                    breaking_diffs.push(ReplayDiff {
                        operation_id: op.id.clone(),
                        operation_name: op.operation_name.clone(),
                        severity: ReplayDiffSeverity::Breaking,
                        message: format!(
                            "operation `{}` kind changed from {:?} to {:?}",
                            op.operation_name, op.expected_kind, def.kind
                        ),
                    });
                }
                if let Some(ref exp_ret) = op.expected_return_type {
                    let actual_ret = leaf_type_name(&def.result);
                    if !types_compatible(exp_ret, actual_ret) {
                        op_breaking = true;
                        breaking_diffs.push(ReplayDiff {
                            operation_id: op.id.clone(),
                            operation_name: op.operation_name.clone(),
                            severity: ReplayDiffSeverity::Breaking,
                            message: format!(
                                "operation `{}` return type `{exp_ret}` changed incompatibly to `{actual_ret}`",
                                op.operation_name
                            ),
                        });
                    }
                }
                for (arg_name, exp_arg_type) in &op.required_args {
                    match def.args.iter().find(|a| a.name == *arg_name) {
                        None => {
                            op_breaking = true;
                            breaking_diffs.push(ReplayDiff {
                                operation_id: op.id.clone(),
                                operation_name: op.operation_name.clone(),
                                severity: ReplayDiffSeverity::Breaking,
                                message: format!(
                                    "required argument `{}.{arg_name}` was removed",
                                    op.operation_name
                                ),
                            });
                        }
                        Some(arg) => {
                            let actual = leaf_type_name(&arg.arg_type);
                            if !types_compatible(exp_arg_type, actual) {
                                op_breaking = true;
                                breaking_diffs.push(ReplayDiff {
                                    operation_id: op.id.clone(),
                                    operation_name: op.operation_name.clone(),
                                    severity: ReplayDiffSeverity::Breaking,
                                    message: format!(
                                        "argument `{}.{arg_name}` type changed from `{exp_arg_type}` to `{actual}`",
                                        op.operation_name
                                    ),
                                });
                            }
                        }
                    }
                }
                for (ty_name, field_name, exp_field_type) in &op.selected_fields {
                    match type_fields.get(&(ty_name.as_str(), field_name.as_str())) {
                        None => {
                            op_breaking = true;
                            breaking_diffs.push(ReplayDiff {
                                operation_id: op.id.clone(),
                                operation_name: op.operation_name.clone(),
                                severity: ReplayDiffSeverity::Breaking,
                                message: format!(
                                    "selected field `{ty_name}.{field_name}` removed from schema"
                                ),
                            });
                        }
                        Some(actual) if !types_compatible(exp_field_type, actual) => {
                            op_breaking = true;
                            breaking_diffs.push(ReplayDiff {
                                operation_id: op.id.clone(),
                                operation_name: op.operation_name.clone(),
                                severity: ReplayDiffSeverity::Breaking,
                                message: format!(
                                    "field `{ty_name}.{field_name}` type changed from `{exp_field_type}` to `{actual}`"
                                ),
                            });
                        }
                        Some(_) => {}
                    }
                }
            }
        }

        if let Some(obs) = obs_by_id.get(op.id.as_str()) {
            // `ING-041` / spec §5: shadow execution runs read-only operations and discards results.
            if !obs.read_only {
                op_breaking = true;
                breaking_diffs.push(ReplayDiff {
                    operation_id: op.id.clone(),
                    operation_name: op.operation_name.clone(),
                    severity: ReplayDiffSeverity::Breaking,
                    message: "shadow replay attempted non-read-only operation".to_owned(),
                });
            }
            if !obs.response_discarded {
                op_breaking = true;
                breaking_diffs.push(ReplayDiff {
                    operation_id: op.id.clone(),
                    operation_name: op.operation_name.clone(),
                    severity: ReplayDiffSeverity::Breaking,
                    message: "shadow execution response payload was not discarded (ING-041)"
                        .to_owned(),
                });
            }
            if let Some(ref code) = obs.error_code {
                op_breaking = true;
                breaking_diffs.push(ReplayDiff {
                    operation_id: op.id.clone(),
                    operation_name: op.operation_name.clone(),
                    severity: ReplayDiffSeverity::Breaking,
                    message: format!("shadow execution returned error `{code}`"),
                });
            }

            let coverage_drop = op.expected_key_coverage - obs.observed_key_coverage;
            if coverage_drop > cfg.max_coverage_drop {
                op_warning = true;
                let msg = format!(
                    "op `{}` key coverage {:.4} dropped {:.4} below expectation {:.4}",
                    op.id, obs.observed_key_coverage, coverage_drop, op.expected_key_coverage
                );
                coverage_regressions.push(op.id.clone());
                non_breaking_diffs.push(ReplayDiff {
                    operation_id: op.id.clone(),
                    operation_name: op.operation_name.clone(),
                    severity: ReplayDiffSeverity::NonBreaking,
                    message: msg,
                });
            } else if op.expected_row_count > 0 && obs.observed_row_count != op.expected_row_count {
                op_warning = true;
                non_breaking_diffs.push(ReplayDiff {
                    operation_id: op.id.clone(),
                    operation_name: op.operation_name.clone(),
                    severity: ReplayDiffSeverity::NonBreaking,
                    message: format!(
                        "row count shifted from {} to {} during shadow diff",
                        op.expected_row_count, obs.observed_row_count
                    ),
                });
            }
        }

        if !op_breaking && !op_warning {
            passed_count += 1;
        }
    }

    let valid_ack = cfg
        .breaking_change_ack
        .as_ref()
        .filter(|a| a.is_valid())
        .cloned();
    let has_breaking = !breaking_diffs.is_empty();
    let pass_ratio = if replayed_count == 0 {
        1.0
    } else {
        passed_count as f64 / replayed_count as f64
    };
    let below_pass_floor = pass_ratio < cfg.min_pass_rate;

    // `spec/05-ingestion.md` §5: hard if breaking, soft otherwise (`breaking_change_ack`).
    // Coverage regressions > 10pp also prevent G8 from auto-passing.
    let (passed, effective_kind, fail_action, applied_ack) = if has_breaking {
        if let Some(ack) = valid_ack {
            (true, GateKind::Soft, None, Some(ack))
        } else {
            (false, GateKind::Hard, Some(GateFailAction::Failed), None)
        }
    } else if !coverage_regressions.is_empty() || below_pass_floor {
        if let Some(ack) = valid_ack {
            (true, GateKind::Soft, None, Some(ack))
        } else {
            (
                false,
                GateKind::Soft,
                Some(GateFailAction::AdminAckRequired),
                None,
            )
        }
    } else {
        (true, GateKind::Soft, None, None)
    };

    let mut evidence = BTreeMap::new();
    evidence.insert(
        "gate".to_owned(),
        CisgValue::String(GATE_G8_LABEL.to_owned()),
    );
    evidence.insert(
        "replayed_count".to_owned(),
        CisgValue::Int(i64::try_from(replayed_count).unwrap_or(i64::MAX)),
    );
    evidence.insert(
        "passed_count".to_owned(),
        CisgValue::Int(i64::try_from(passed_count).unwrap_or(i64::MAX)),
    );
    evidence.insert(
        "breaking_diff_count".to_owned(),
        CisgValue::Int(i64::try_from(breaking_diffs.len()).unwrap_or(i64::MAX)),
    );
    evidence.insert(
        "non_breaking_diff_count".to_owned(),
        CisgValue::Int(i64::try_from(non_breaking_diffs.len()).unwrap_or(i64::MAX)),
    );
    evidence.insert(
        "coverage_regression_count".to_owned(),
        CisgValue::Int(i64::try_from(coverage_regressions.len()).unwrap_or(i64::MAX)),
    );

    ReplayReport {
        replayed_count,
        passed_count,
        breaking_diffs,
        non_breaking_diffs,
        coverage_regressions,
        effective_kind,
        passed,
        fail_action,
        applied_ack,
        evidence,
    }
}

fn leaf_type_name(mut tr: &TypeRef) -> &str {
    loop {
        match tr {
            TypeRef::Named(name) => return name.as_str(),
            TypeRef::NonNull(inner) | TypeRef::List(inner) => tr = inner.as_ref(),
        }
    }
}

fn types_compatible(expected: &str, actual: &str) -> bool {
    if expected == actual {
        return true;
    }
    matches!(
        (expected, actual),
        ("ID", "String") | ("String", "ID") | ("UUID", "String") | ("String", "UUID")
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use gap_core::snapshot::{Protocol, SideEffectLevel};

    use crate::cisg::{
        ArgDef, Cisg, CisgValue, ExtractorInfo, FieldDef, OperationDef, Pagination, SourceRef,
        TypeDef, UntrustedText,
    };

    fn sample_cisg() -> Cisg {
        Cisg {
            source: SourceRef {
                kind: Protocol::Graphql,
                location_canonical: "mem://orders.graphql".to_owned(),
                source_hash: "a".repeat(64),
            },
            extractor: ExtractorInfo {
                id: "test".to_owned(),
                version: "0.1.0".to_owned(),
            },
            types: vec![TypeDef {
                name: "Order".to_owned(),
                kind: "OBJECT".to_owned(),
                fields: vec![
                    FieldDef {
                        name: "id".to_owned(),
                        field_type: TypeRef::NonNull(Box::new(TypeRef::Named("ID".to_owned()))),
                        is_pii: false,
                        description: UntrustedText::from_raw("Order ID"),
                    },
                    FieldDef {
                        name: "totalCents".to_owned(),
                        field_type: TypeRef::NonNull(Box::new(TypeRef::Named("Int".to_owned()))),
                        is_pii: false,
                        description: UntrustedText::from_raw("Total in cents"),
                    },
                ],
                enum_values: Vec::new(),
                json_schema: None,
                description: UntrustedText::from_raw("Order record"),
            }],
            operations: vec![OperationDef {
                name: "orderById".to_owned(),
                kind: OpKind::Query,
                args: vec![ArgDef {
                    name: "id".to_owned(),
                    arg_type: TypeRef::NonNull(Box::new(TypeRef::Named("ID".to_owned()))),
                    default_value: None,
                    description: UntrustedText::from_raw("Order identifier"),
                }],
                result: TypeRef::Named("Order".to_owned()),
                description: UntrustedText::from_raw("Look up an order"),
                side_effect: SideEffectLevel::None,
                idempotent: Some(true),
                pagination: Pagination::None,
                key_fields: vec!["id".to_owned()],
                examples: Vec::new(),
                native_ref: CisgValue::Null,
            }],
            node_categories: Vec::new(),
            edge_predicates: Vec::new(),
            keys: Vec::new(),
            candidate_joins: Vec::new(),
            docs: Vec::new(),
        }
    }

    fn base_replay_op() -> ReplayOperation {
        ReplayOperation {
            id: "op-order-by-id".to_owned(),
            operation_name: "orderById".to_owned(),
            expected_kind: OpKind::Query,
            expected_return_type: Some("Order".to_owned()),
            required_args: vec![("id".to_owned(), "ID".to_owned())],
            selected_fields: vec![
                ("Order".to_owned(), "id".to_owned(), "ID".to_owned()),
                (
                    "Order".to_owned(),
                    "totalCents".to_owned(),
                    "Int".to_owned(),
                ),
            ],
            is_conformance_fixture: true,
            expected_row_count: 1,
            expected_key_coverage: 0.98,
        }
    }

    // spec: ING-040, ING-041
    #[test]
    fn replay_g8_passes_when_top_n_operations_and_shadow_diffs_are_within_tolerance() {
        let cisg = sample_cisg();
        let ops = vec![base_replay_op()];
        let obs = vec![ShadowReplayObservation {
            operation_id: "op-order-by-id".to_owned(),
            read_only: true,
            response_discarded: true,
            observed_row_count: 1,
            observed_key_coverage: 0.97,
            error_code: None,
        }];

        let report = evaluate_gate_g8_replay(&cisg, &ops, &obs, &ReplayConfig::default());
        assert!(report.passed);
        assert!(report.breaking_diffs.is_empty());
        assert!(report.coverage_regressions.is_empty());
        assert_eq!(report.passed_count, 1);
    }

    // spec: ING-040
    #[test]
    fn replay_g8_blocks_breaking_field_removal_until_breaking_change_ack() {
        let mut cisg = sample_cisg();
        // Remove `totalCents` from Order -> breaking regression.
        cisg.types[0].fields.retain(|f| f.name != "totalCents");

        let ops = vec![base_replay_op()];
        let report = evaluate_gate_g8_replay(&cisg, &ops, &[], &ReplayConfig::default());
        assert!(!report.passed);
        assert_eq!(report.effective_kind, GateKind::Hard);
        assert_eq!(report.fail_action, Some(GateFailAction::Failed));
        assert_eq!(report.breaking_diffs.len(), 1);

        // With valid audited `breaking_change_ack`, admin may explicitly allow the breaking change.
        let ack_cfg = ReplayConfig {
            breaking_change_ack: Some(AdminAck {
                actor: "admin@acme.test".to_owned(),
                reason: "deprecated totalCents after v2 migration notice".to_owned(),
                timestamp_utc: "2026-11-14T12:30:00Z".to_owned(),
            }),
            ..ReplayConfig::default()
        };
        let ack_report = evaluate_gate_g8_replay(&cisg, &ops, &[], &ack_cfg);
        assert!(ack_report.passed);
        assert!(ack_report.applied_ack.is_some());
    }

    // spec: ING-041, INV-06
    #[test]
    fn canary_replay_flags_shadow_coverage_drop_and_undiscarded_payloads() {
        let cisg = sample_cisg();
        let ops = vec![base_replay_op()];

        // Coverage drops by > 10 percentage points (0.98 -> 0.80).
        let diverged_obs = vec![ShadowReplayObservation {
            operation_id: "op-order-by-id".to_owned(),
            read_only: true,
            response_discarded: true,
            observed_row_count: 1,
            observed_key_coverage: 0.80,
            error_code: None,
        }];
        let div_report =
            evaluate_gate_g8_replay(&cisg, &ops, &diverged_obs, &ReplayConfig::default());
        assert!(!div_report.passed);
        assert_eq!(
            div_report.coverage_regressions,
            vec!["op-order-by-id".to_owned()]
        );

        // Undiscarded shadow payload breaks the gate invariant.
        let leaked_obs = vec![ShadowReplayObservation {
            operation_id: "op-order-by-id".to_owned(),
            read_only: true,
            response_discarded: false,
            observed_row_count: 1,
            observed_key_coverage: 0.98,
            error_code: None,
        }];
        let leak_report =
            evaluate_gate_g8_replay(&cisg, &ops, &leaked_obs, &ReplayConfig::default());
        assert!(!leak_report.passed);
        assert_eq!(leak_report.breaking_diffs.len(), 1);
    }
}
