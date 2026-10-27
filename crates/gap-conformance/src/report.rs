//! Deterministic JSON conformance report and spec-ID coverage computation
//! (`VER-010`, `VER-011`, `spec/02-gap-ql.md` §11).

use std::collections::{BTreeMap, BTreeSet};

use gap_core::clock::{Clock, Timestamp};
use serde::{Deserialize, Serialize};

use crate::fixture::ConformanceClass;

/// Per-fixture execution outcome (`spec/02-gap-ql.md` §11).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Verdict {
    /// Fixture (or all of its `steps`) matched expected response subsets.
    Pass,
    /// Observed response did not subset-match `expect`.
    Fail,
    /// Execution could not complete (unmapped principal, transport error, malformed response).
    Error,
}

/// Runner binary identity recorded at the top of [`ConformanceReport`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunnerMetadata {
    /// Runner crate/binary name (`"gap-conformance"`).
    pub name: String,
    /// Runner crate version (`CARGO_PKG_VERSION`).
    pub version: String,
}

impl Default for RunnerMetadata {
    fn default() -> Self {
        Self {
            name: env!("CARGO_PKG_NAME").to_owned(),
            version: env!("CARGO_PKG_VERSION").to_owned(),
        }
    }
}

/// Per-fixture outcome entry inside [`ConformanceReport`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FixtureReport {
    /// Human-readable fixture name (`"name"`).
    pub name: String,
    /// Portable relative path under the fixtures directory (`"file"`).
    pub file: String,
    /// Conformance classes declared by the fixture (`"class"`).
    pub class: Vec<ConformanceClass>,
    /// Spec requirement IDs declared by the fixture (`"spec"`).
    pub spec: Vec<String>,
    /// Verdict (`"pass"`, `"fail"`, or `"error"`).
    pub verdict: Verdict,
    /// Failure/error diagnostic (`null` when `verdict == "pass"`).
    pub detail: Option<String>,
}

/// Aggregate fixture and spec-coverage counts in [`ConformanceReport`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReportSummary {
    /// Total fixtures executed.
    pub total: usize,
    /// Count of fixtures with verdict `"pass"`.
    pub passed: usize,
    /// Count of fixtures with verdict `"fail"`.
    pub failed: usize,
    /// Count of fixtures with verdict `"error"`.
    pub errors: usize,
    /// Distinct spec IDs named across executed fixtures.
    pub spec_ids_total: usize,
    /// Spec IDs covered by at least one passing fixture (`spec/02-gap-ql.md` §11).
    pub spec_ids_covered: usize,
}

/// Reference to a fixture naming a given spec ID inside [`SpecCoverageEntry`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpecFixtureRef {
    /// Fixture `"name"`.
    pub name: String,
    /// Relative fixture `"file"`.
    pub file: String,
    /// Verdict of that fixture.
    pub verdict: Verdict,
}

/// Coverage record for a single normative requirement ID (`spec/02-gap-ql.md` §11).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpecCoverageEntry {
    /// `true` iff `passed >= 1` (`spec/02-gap-ql.md` §11).
    pub covered: bool,
    /// Number of passing fixtures naming this spec ID.
    pub passed: usize,
    /// Fixtures naming this spec ID and their verdicts.
    pub fixtures: Vec<SpecFixtureRef>,
}

/// Deterministic JSON conformance report (`VER-010`, `VER-011`).
///
/// Uses [`BTreeMap`] ordering for `coverage` and omits `generated_at` unless a [`Clock`] is
/// explicitly injected (`INV-02`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConformanceReport {
    /// Runner name and version.
    pub runner: RunnerMetadata,
    /// Target endpoint description.
    pub target: String,
    /// Optional timestamp injected exclusively via `gap_core::clock::Clock` (`None` by default).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generated_at: Option<Timestamp>,
    /// Per-fixture results in deterministic execution order.
    pub fixtures: Vec<FixtureReport>,
    /// Summary counts.
    pub summary: ReportSummary,
    /// Deterministic `BTreeMap` of spec ID -> coverage status.
    pub coverage: BTreeMap<String, SpecCoverageEntry>,
}

impl ConformanceReport {
    /// Builds a deterministic [`ConformanceReport`] from `target`, `fixtures`, and optional
    /// injected `clock`.
    #[must_use]
    pub fn from_results(
        target: impl Into<String>,
        fixtures: Vec<FixtureReport>,
        clock: Option<&dyn Clock>,
    ) -> Self {
        let mut passed_count = 0_usize;
        let mut failed_count = 0_usize;
        let mut error_count = 0_usize;
        let mut coverage: BTreeMap<String, SpecCoverageEntry> = BTreeMap::new();

        for fixture in &fixtures {
            match fixture.verdict {
                Verdict::Pass => passed_count += 1,
                Verdict::Fail => failed_count += 1,
                Verdict::Error => error_count += 1,
            }

            let mut seen_in_fixture = BTreeSet::<&str>::new();
            for spec_id in &fixture.spec {
                if !seen_in_fixture.insert(spec_id.as_str()) {
                    continue;
                }
                let entry = coverage
                    .entry(spec_id.clone())
                    .or_insert_with(|| SpecCoverageEntry {
                        covered: false,
                        passed: 0,
                        fixtures: Vec::new(),
                    });
                if fixture.verdict == Verdict::Pass {
                    entry.passed += 1;
                    entry.covered = true;
                }
                entry.fixtures.push(SpecFixtureRef {
                    name: fixture.name.clone(),
                    file: fixture.file.clone(),
                    verdict: fixture.verdict,
                });
            }
        }

        let spec_ids_total = coverage.len();
        let spec_ids_covered = coverage.values().filter(|entry| entry.covered).count();

        Self {
            runner: RunnerMetadata::default(),
            target: target.into(),
            generated_at: clock.map(Clock::now_timestamp),
            summary: ReportSummary {
                total: fixtures.len(),
                passed: passed_count,
                failed: failed_count,
                errors: error_count,
                spec_ids_total,
                spec_ids_covered,
            },
            fixtures,
            coverage,
        }
    }

    /// Returns `true` iff zero fixtures failed and zero fixtures errored (`exit code 0`).
    #[must_use]
    pub const fn all_passed(&self) -> bool {
        self.summary.failed == 0 && self.summary.errors == 0
    }

    /// CLI exit code (`0` if all fixtures passed, `1` if at least one failed or errored).
    #[must_use]
    pub const fn exit_code(&self) -> u8 {
        if self.all_passed() { 0 } else { 1 }
    }

    /// Serializes this report as pretty-printed deterministic JSON with trailing newline.
    ///
    /// # Errors
    ///
    /// Returns [`serde_json::Error`] if serialization fails.
    pub fn to_json_pretty(&self) -> Result<String, serde_json::Error> {
        let mut s = serde_json::to_string_pretty(self)?;
        s.push('\n');
        Ok(s)
    }
}

#[cfg(test)]
mod tests {
    use gap_core::clock::ManualClock;

    use super::*;

    // spec: GQL-002 (VER-010, VER-011)
    #[test]
    fn coverage_counts_spec_id_covered_only_when_at_least_one_fixture_passed() {
        let fixtures = vec![
            FixtureReport {
                name: "f1 pass".to_owned(),
                file: "a/01.json".to_owned(),
                class: vec![ConformanceClass::Core],
                // Duplicate ID inside one fixture must not double-count `passed`.
                spec: vec![
                    "GQL-070".to_owned(),
                    "GQL-002".to_owned(),
                    "GQL-070".to_owned(),
                ],
                verdict: Verdict::Pass,
                detail: None,
            },
            FixtureReport {
                name: "f2 fail".to_owned(),
                file: "a/02.json".to_owned(),
                class: vec![ConformanceClass::Core],
                spec: vec!["GQL-070".to_owned(), "GQL-062".to_owned()],
                verdict: Verdict::Fail,
                detail: Some("mismatch at `$.status`".to_owned()),
            },
            FixtureReport {
                name: "f3 error".to_owned(),
                file: "a/03.json".to_owned(),
                class: vec![ConformanceClass::Secure],
                spec: vec!["INV-15".to_owned()],
                verdict: Verdict::Error,
                detail: Some("unmapped principal".to_owned()),
            },
        ];

        let report = ConformanceReport::from_results("http://127.0.0.1:8080", fixtures, None);
        assert_eq!(report.runner.name, "gap-conformance");
        assert_eq!(report.target, "http://127.0.0.1:8080");
        assert!(report.generated_at.is_none());
        assert_eq!(
            report.summary,
            ReportSummary {
                total: 3,
                passed: 1,
                failed: 1,
                errors: 1,
                spec_ids_total: 4,
                spec_ids_covered: 2,
            }
        );
        assert!(!report.all_passed());
        assert_eq!(report.exit_code(), 1);

        // `BTreeMap` key ordering must be sorted lexicographically.
        let keys: Vec<&str> = report.coverage.keys().map(String::as_str).collect();
        assert_eq!(keys, vec!["GQL-002", "GQL-062", "GQL-070", "INV-15"]);

        // GQL-070: 1 pass + 1 fail => covered = true, passed = 1, 2 fixture refs.
        let gql_070 = &report.coverage["GQL-070"];
        assert!(gql_070.covered);
        assert_eq!(gql_070.passed, 1);
        assert_eq!(gql_070.fixtures.len(), 2);

        // GQL-062: 0 pass + 1 fail => covered = false, passed = 0.
        let gql_062 = &report.coverage["GQL-062"];
        assert!(!gql_062.covered);
        assert_eq!(gql_062.passed, 0);

        // INV-15: 0 pass + 1 error => covered = false, passed = 0.
        let inv_15 = &report.coverage["INV-15"];
        assert!(!inv_15.covered);
        assert_eq!(inv_15.passed, 0);
    }

    // spec: INV-02, GQL-002 (VER-011)
    #[test]
    fn inv_02_report_omits_wall_clock_by_default_and_uses_injected_clock_when_provided() {
        let pass_fixture = FixtureReport {
            name: "ok".to_owned(),
            file: "core/ok.json".to_owned(),
            class: vec![ConformanceClass::Core],
            spec: vec!["GQL-002".to_owned()],
            verdict: Verdict::Pass,
            detail: None,
        };

        let r1 = ConformanceReport::from_results(
            "http://127.0.0.1:8080",
            vec![pass_fixture.clone()],
            None,
        );
        let r2 = ConformanceReport::from_results(
            "http://127.0.0.1:8080",
            vec![pass_fixture.clone()],
            None,
        );
        assert_eq!(r1.exit_code(), 0);
        let j1 = r1.to_json_pretty().expect("json 1");
        let j2 = r2.to_json_pretty().expect("json 2");
        assert_eq!(j1, j2, "reports without clock must be byte-identical");
        assert!(!j1.contains("generated_at"));

        let clock = ManualClock::new(1_700_000_000_000);
        let with_clock = ConformanceReport::from_results(
            "http://127.0.0.1:8080",
            vec![pass_fixture],
            Some(&clock),
        );
        let j_clock = with_clock.to_json_pretty().expect("json with clock");
        assert!(j_clock.contains("\"generated_at\": \"2023-11-14T22:13:20Z\""));
    }
}
