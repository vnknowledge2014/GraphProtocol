//! Check identities, outcomes and the machine-readable report (`NGC-091`, `VER-011` `--report`).

use serde_json::{Value, json};

/// Stable check ids (they appear in reports and in the flaw matrix).
pub mod id {
    /// `Describe` is valid: SDL parses, names, schema refs, limits are coherent.
    pub const DESCRIBE_VALID: &str = "describe.valid";
    /// `Describe` is side-effect free and idempotent; `not_modified` is honest.
    pub const DESCRIBE_STABLE: &str = "describe.stable";
    /// `Health` answers `SERVING` within 500 ms.
    pub const HEALTH: &str = "health";
    /// `foreach`: one closing chunk per index, independence, partial failure.
    pub const FOREACH_CORRECTNESS: &str = "foreach.correctness";
    /// `foreach`: upstream calls are batched, never one per key.
    pub const FOREACH_N_PLUS_ONE: &str = "foreach.n_plus_one";
    /// Row order is deterministic and follows `order_by`.
    pub const QUERY_ORDER: &str = "query.order";
    /// A passed deadline ends the call and frees capacity.
    pub const DEADLINE: &str = "deadline";
    /// A client cancel stops the work and frees capacity.
    pub const CANCEL: &str = "cancel";
    /// Idempotency keys replay instead of re-applying.
    pub const MUTATE_IDEMPOTENCY: &str = "mutate.idempotency";
    /// Dry-run changes nothing.
    pub const MUTATE_DRY_RUN: &str = "mutate.dry_run";
    /// Pagination is stable and cursors are bound.
    pub const PAGE_STABILITY: &str = "page.stability";
    /// Errors carry the right code and leak nothing.
    pub const ERRORS_MAPPING: &str = "errors.mapping";
    /// Oversized answers are truncated and flagged.
    pub const LIMITS_RESPONSE_SIZE: &str = "limits.response_size";
}

/// Static description of a check.
#[derive(Debug, Clone, Copy)]
pub struct CheckInfo {
    /// Stable id (see [`id`]).
    pub id: &'static str,
    /// One-line description.
    pub title: &'static str,
    /// Spec requirements the check enforces.
    pub spec: &'static [&'static str],
}

/// Every check, in execution order.
pub const CHECKS: [CheckInfo; 13] = [
    CheckInfo {
        id: id::DESCRIBE_VALID,
        title: "Describe is valid (SDL parses, names, local schema refs, coherent limits)",
        spec: &["NGC-010", "NGC-012", "NGC-013", "NGC-015"],
    },
    CheckInfo {
        id: id::DESCRIBE_STABLE,
        title: "Describe is idempotent and not_modified is honest",
        spec: &["NGC-010"],
    },
    CheckInfo {
        id: id::HEALTH,
        title: "Health answers SERVING within 500 ms",
        spec: &["NGC-070"],
    },
    CheckInfo {
        id: id::FOREACH_CORRECTNESS,
        title: "foreach: one closing chunk per index, independent, partial failure isolated",
        spec: &["NGC-020"],
    },
    CheckInfo {
        id: id::FOREACH_N_PLUS_ONE,
        title: "foreach: upstream_calls <= ceil(distinct keys / upstream batch)",
        spec: &["NGC-026", "INV-18"],
    },
    CheckInfo {
        id: id::QUERY_ORDER,
        title: "row order is deterministic and follows order_by",
        spec: &["NGC-021"],
    },
    CheckInfo {
        id: id::DEADLINE,
        title: "a passed deadline ends the call with DEADLINE_EXCEEDED and frees capacity",
        spec: &["NGC-025", "INV-12"],
    },
    CheckInfo {
        id: id::CANCEL,
        title: "a client cancel stops the work and frees capacity",
        spec: &["NGC-025", "INV-12"],
    },
    CheckInfo {
        id: id::MUTATE_IDEMPOTENCY,
        title: "an idempotency key applies the effect once and replays the original result",
        spec: &["NGC-030"],
    },
    CheckInfo {
        id: id::MUTATE_DRY_RUN,
        title: "dry_run changes nothing and reports DRY_RUN (or fails UNIMPLEMENTED)",
        spec: &["NGC-031"],
    },
    CheckInfo {
        id: id::PAGE_STABILITY,
        title: "pagination is stable, repeatable and cursors are bound to their arguments",
        spec: &["NGC-021", "NGC-022"],
    },
    CheckInfo {
        id: id::ERRORS_MAPPING,
        title: "errors use the right code, reject unadvertised predicates and leak no credentials",
        spec: &["NGC-023", "NGC-060", "NGC-061", "INV-15"],
    },
    CheckInfo {
        id: id::LIMITS_RESPONSE_SIZE,
        title: "responses over max_response_bytes are truncated and flagged",
        spec: &["NGC-027"],
    },
];

/// Why a check did not run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkipReason {
    /// The connector does not advertise the capability: nothing to check.
    NotApplicable(String),
    /// The capability is advertised but the profile (or `Describe`) gives the kit nothing to probe it with.
    NotCovered(String),
}

/// Result of one check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The connector behaved.
    Pass,
    /// The connector broke a requirement; the text says how.
    Fail(String),
    /// The check did not run.
    Skip(SkipReason),
}

/// One check's result with its timing and notes.
#[derive(Debug, Clone)]
pub struct CheckResult {
    /// Static description.
    pub info: CheckInfo,
    /// Verdict.
    pub outcome: Outcome,
    /// Observations that are not failures (for example "effect count not verified").
    pub notes: Vec<String>,
    /// Wall time of the check.
    pub elapsed_ms: u64,
}

/// Everything a run produced.
#[derive(Debug, Clone)]
pub struct KitReport {
    /// Profile name.
    pub profile: String,
    /// `connector_id` from `Describe` (empty if `Describe` failed).
    pub connector_id: String,
    /// One entry per check, in [`CHECKS`] order.
    pub results: Vec<CheckResult>,
}

impl KitReport {
    /// Result of the check `check_id`.
    #[must_use]
    pub fn result(&self, check_id: &str) -> Option<&CheckResult> {
        self.results.iter().find(|r| r.info.id == check_id)
    }

    /// Ids of the checks that failed.
    #[must_use]
    pub fn failed(&self) -> Vec<&'static str> {
        self.results
            .iter()
            .filter(|r| matches!(r.outcome, Outcome::Fail(_)))
            .map(|r| r.info.id)
            .collect()
    }

    /// Ids of the checks the profile left uncovered although the capability is advertised.
    #[must_use]
    pub fn uncovered(&self) -> Vec<&'static str> {
        self.results
            .iter()
            .filter(|r| matches!(r.outcome, Outcome::Skip(SkipReason::NotCovered(_))))
            .map(|r| r.info.id)
            .collect()
    }

    /// `NGC-091`: a connector may claim "NGC-compatible" only if every applicable check passes.
    #[must_use]
    pub fn compatible(&self) -> bool {
        self.failed().is_empty() && self.uncovered().is_empty()
    }

    /// Spec ids enforced by checks that passed (feeds `--report` spec coverage).
    #[must_use]
    pub fn passed_spec_ids(&self) -> Vec<&'static str> {
        let mut ids: Vec<&'static str> = self
            .results
            .iter()
            .filter(|r| r.outcome == Outcome::Pass)
            .flat_map(|r| r.info.spec.iter().copied())
            .collect();
        ids.sort_unstable();
        ids.dedup();
        ids
    }

    /// Machine-readable report.
    #[must_use]
    pub fn to_json(&self) -> Value {
        let checks: Vec<Value> = self
            .results
            .iter()
            .map(|r| {
                let (status, reason, kind) = match &r.outcome {
                    Outcome::Pass => ("pass", None, None),
                    Outcome::Fail(why) => ("fail", Some(why.as_str()), None),
                    Outcome::Skip(SkipReason::NotApplicable(why)) => {
                        ("skip", Some(why.as_str()), Some("not_applicable"))
                    }
                    Outcome::Skip(SkipReason::NotCovered(why)) => {
                        ("skip", Some(why.as_str()), Some("not_covered"))
                    }
                };
                json!({
                    "id": r.info.id,
                    "title": r.info.title,
                    "spec": r.info.spec,
                    "status": status,
                    "reason": reason,
                    "kind": kind,
                    "notes": r.notes,
                    "elapsed_ms": r.elapsed_ms,
                })
            })
            .collect();
        json!({
            "profile": self.profile,
            "connector_id": self.connector_id,
            "compatible": self.compatible(),
            "failed": self.failed(),
            "uncovered": self.uncovered(),
            "passed_spec_ids": self.passed_spec_ids(),
            "checks": checks,
        })
    }

    /// Human-readable summary, one line per check.
    #[must_use]
    pub fn render_text(&self) -> String {
        let mut out = format!(
            "NGC connector kit: profile `{}`, connector `{}`\n",
            self.profile, self.connector_id
        );
        for r in &self.results {
            let line = match &r.outcome {
                Outcome::Pass => format!("PASS  {}", r.info.id),
                Outcome::Fail(why) => format!("FAIL  {} - {why}", r.info.id),
                Outcome::Skip(SkipReason::NotApplicable(why)) => {
                    format!("SKIP  {} - not applicable: {why}", r.info.id)
                }
                Outcome::Skip(SkipReason::NotCovered(why)) => {
                    format!("SKIP  {} - NOT COVERED: {why}", r.info.id)
                }
            };
            out.push_str(&line);
            out.push('\n');
            for note in &r.notes {
                out.push_str("      note: ");
                out.push_str(note);
                out.push('\n');
            }
        }
        out.push_str(if self.compatible() {
            "RESULT: NGC-compatible (all applicable checks passed)\n"
        } else {
            "RESULT: NOT NGC-compatible\n"
        });
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn result(info: CheckInfo, outcome: Outcome) -> CheckResult {
        CheckResult {
            info,
            outcome,
            notes: vec![],
            elapsed_ms: 0,
        }
    }

    // spec: NGC-091
    #[test]
    fn ngc_kit_check_table_has_unique_ids_and_known_spec_ids() {
        let mut ids: Vec<&str> = CHECKS.iter().map(|c| c.id).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), CHECKS.len(), "duplicate check id");
        for c in &CHECKS {
            assert!(!c.spec.is_empty(), "{} names no spec id", c.id);
            for s in c.spec {
                assert!(
                    s.starts_with("NGC-") || s.starts_with("INV-"),
                    "{} cites {s}",
                    c.id
                );
            }
        }
    }

    // spec: NGC-091
    #[test]
    fn ngc_kit_compatible_needs_no_fail_and_no_uncovered_capability() {
        let ok = KitReport {
            profile: "p".into(),
            connector_id: "c".into(),
            results: vec![
                result(CHECKS[0], Outcome::Pass),
                result(
                    CHECKS[1],
                    Outcome::Skip(SkipReason::NotApplicable("no".into())),
                ),
            ],
        };
        assert!(ok.compatible());
        assert_eq!(
            ok.passed_spec_ids(),
            vec!["NGC-010", "NGC-012", "NGC-013", "NGC-015"]
        );

        let failing = KitReport {
            results: vec![result(CHECKS[0], Outcome::Fail("bad".into()))],
            ..ok.clone()
        };
        assert!(!failing.compatible());
        assert_eq!(failing.failed(), vec![id::DESCRIBE_VALID]);

        let uncovered = KitReport {
            results: vec![result(
                CHECKS[0],
                Outcome::Skip(SkipReason::NotCovered("profile".into())),
            )],
            ..ok
        };
        assert!(!uncovered.compatible());
        assert_eq!(uncovered.uncovered(), vec![id::DESCRIBE_VALID]);
    }

    // spec: NGC-091
    #[test]
    fn ngc_kit_report_json_names_status_kind_and_spec_coverage() {
        let report = KitReport {
            profile: "p".into(),
            connector_id: "c".into(),
            results: vec![
                result(CHECKS[0], Outcome::Pass),
                result(CHECKS[1], Outcome::Fail("why".into())),
                result(
                    CHECKS[2],
                    Outcome::Skip(SkipReason::NotCovered("profile".into())),
                ),
            ],
        };
        let v = report.to_json();
        assert_eq!(v["compatible"], false);
        assert_eq!(v["checks"][0]["status"], "pass");
        assert_eq!(v["checks"][1]["status"], "fail");
        assert_eq!(v["checks"][1]["reason"], "why");
        assert_eq!(v["checks"][2]["kind"], "not_covered");
        assert_eq!(
            v["passed_spec_ids"],
            json!(["NGC-010", "NGC-012", "NGC-013", "NGC-015"])
        );
        assert!(report.render_text().contains("NOT NGC-compatible"));
    }
}
