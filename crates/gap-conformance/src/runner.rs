//! Conformance suite execution engine (`VER-010`, `VER-011`, `spec/02-gap-ql.md` §11, `INV-15`).

use gap_core::clock::Clock;

use crate::driver::{ConformanceDriver, PrincipalsMap};
use crate::fixture::{ConformanceClass, FixtureExecution, LoadedFixture};
use crate::report::{ConformanceReport, FixtureReport, Verdict};

/// Executes loaded conformance fixtures against a [`ConformanceDriver`].
#[derive(Debug, Clone, Default)]
pub struct RunnerConfig {
    /// Optional conformance class filter (`--class`, repeatable). Empty runs all classes.
    pub classes: Vec<ConformanceClass>,
    /// Principal-to-bearer-token map (`--principals`).
    pub principals: PrincipalsMap,
}

/// Runs `fixtures` filtered by `config.classes` against `driver` and produces a deterministic
/// [`ConformanceReport`].
pub async fn run_suite(
    driver: &dyn ConformanceDriver,
    fixtures: &[LoadedFixture],
    config: &RunnerConfig,
    clock: Option<&dyn Clock>,
) -> ConformanceReport {
    let mut results = Vec::new();

    for fixture in fixtures {
        if !matches_class_filter(&fixture.class, &config.classes) {
            continue;
        }
        let report_item = run_single_fixture(driver, fixture, &config.principals).await;
        results.push(report_item);
    }

    ConformanceReport::from_results(driver.target(), results, clock)
}

fn matches_class_filter(
    fixture_classes: &[ConformanceClass],
    filter_classes: &[ConformanceClass],
) -> bool {
    if filter_classes.is_empty() {
        return true;
    }
    fixture_classes
        .iter()
        .any(|cls| filter_classes.contains(cls))
}

async fn run_single_fixture(
    driver: &dyn ConformanceDriver,
    fixture: &LoadedFixture,
    principals: &PrincipalsMap,
) -> FixtureReport {
    let span = tracing::info_span!(
        "conformance_fixture",
        fixture_name = %fixture.name,
        fixture_file = %fixture.relative_path,
        principal = fixture.principal.as_deref().unwrap_or("<anonymous>"),
    );
    let _enter = span.enter();

    let bearer_token = if let Some(ref principal_name) = fixture.principal {
        let Some(secret) = principals.get(principal_name) else {
            let reason =
                format!("principal `{principal_name}` has no bearer token mapping in --principals");
            tracing::warn!(
                fixture_file = %fixture.relative_path,
                principal = %principal_name,
                "unmapped fixture principal; reporting verdict=error"
            );
            return FixtureReport {
                name: fixture.name.clone(),
                file: fixture.relative_path.clone(),
                class: fixture.class.clone(),
                spec: fixture.spec.clone(),
                verdict: Verdict::Error,
                detail: Some(principals.scrub_text(&reason)),
            };
        };
        Some(secret)
    } else {
        None
    };

    drop(_enter);

    let (verdict, detail) = match &fixture.execution {
        FixtureExecution::Single { request, expect } => {
            match driver.execute(request, bearer_token).await {
                Ok(observed) => match observed.matches(expect) {
                    Ok(()) => (Verdict::Pass, None),
                    Err(mismatch) => (
                        Verdict::Fail,
                        Some(principals.scrub_text(&mismatch.to_string())),
                    ),
                },
                Err(driver_err) => (
                    Verdict::Error,
                    Some(principals.scrub_text(&driver_err.to_string())),
                ),
            }
        }
        FixtureExecution::Steps(steps) => {
            let mut outcome = (Verdict::Pass, None);
            for (step_idx, step) in steps.iter().enumerate() {
                match driver.execute(&step.request, bearer_token).await {
                    Ok(observed) => {
                        if let Err(mismatch) = observed.matches(&step.expect) {
                            outcome = (
                                Verdict::Fail,
                                Some(
                                    principals.scrub_text(&format!("step {step_idx}: {mismatch}")),
                                ),
                            );
                            break;
                        }
                    }
                    Err(driver_err) => {
                        outcome = (
                            Verdict::Error,
                            Some(principals.scrub_text(&format!("step {step_idx}: {driver_err}"))),
                        );
                        break;
                    }
                }
            }
            outcome
        }
    };

    tracing::info!(
        fixture_name = %fixture.name,
        fixture_file = %fixture.relative_path,
        verdict = ?verdict,
        "completed conformance fixture"
    );

    FixtureReport {
        name: fixture.name.clone(),
        file: fixture.relative_path.clone(),
        class: fixture.class.clone(),
        spec: fixture.spec.clone(),
        verdict,
        detail,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::path::PathBuf;
    use std::sync::Mutex;

    use gap_core::secret::SecretString;
    use gap_core::traits::BoxFuture;
    use serde_json::json;

    use super::*;
    use crate::driver::DriverError;
    use crate::fixture::{FixtureExecution, FixtureRequest, FixtureStep};
    use crate::matcher::ObservedResponse;
    use crate::report::Verdict;

    #[derive(Debug)]
    struct ScriptedDriver {
        target: String,
        outcomes: Mutex<VecDeque<Result<ObservedResponse, DriverError>>>,
        calls: Mutex<Vec<(String, Option<String>)>>,
    }

    impl ScriptedDriver {
        fn new(outcomes: Vec<Result<ObservedResponse, DriverError>>) -> Self {
            Self {
                target: "http://127.0.0.1:18080/graphql".to_owned(),
                outcomes: Mutex::new(outcomes.into()),
                calls: Mutex::new(Vec::new()),
            }
        }

        fn recorded_calls(&self) -> Vec<(String, Option<String>)> {
            self.calls.lock().expect("lock calls").clone()
        }
    }

    impl ConformanceDriver for ScriptedDriver {
        fn target(&self) -> &str {
            &self.target
        }

        fn execute<'a>(
            &'a self,
            request: &'a FixtureRequest,
            bearer_token: Option<&'a SecretString>,
        ) -> BoxFuture<'a, Result<ObservedResponse, DriverError>> {
            Box::pin(async move {
                self.calls.lock().expect("lock calls").push((
                    request.query.clone(),
                    bearer_token.map(|s| s.expose_secret().clone()),
                ));
                self.outcomes
                    .lock()
                    .expect("lock outcomes")
                    .pop_front()
                    .unwrap_or_else(|| {
                        Err(DriverError::Transport {
                            target: self.target.clone(),
                            reason: "no scripted outcome remaining".to_owned(),
                        })
                    })
            })
        }
    }

    fn make_single_fixture(
        name: &str,
        file: &str,
        class: Vec<ConformanceClass>,
        spec: Vec<&str>,
        principal: Option<&str>,
        query: &str,
        expect: serde_json::Value,
    ) -> LoadedFixture {
        LoadedFixture {
            path: PathBuf::from(file),
            relative_path: file.to_owned(),
            name: name.to_owned(),
            class,
            spec: spec.into_iter().map(str::to_owned).collect(),
            schema: None,
            principal: principal.map(str::to_owned),
            execution: FixtureExecution::Single {
                request: FixtureRequest {
                    query: query.to_owned(),
                    variables: None,
                    operation_name: None,
                    extensions: None,
                },
                expect,
            },
        }
    }

    // spec: GQL-002, INV-15 (VER-010, VER-011)
    #[tokio::test]
    async fn filters_classes_and_reports_unmapped_principal_as_error_never_skipped() {
        let ok_obs = ObservedResponse::from_parts(
            200,
            std::iter::empty::<(&str, &str)>(),
            json!({"data": {"ok": true}}).as_object(),
        );
        let driver = ScriptedDriver::new(vec![Ok(ok_obs)]);

        let fixtures = vec![
            make_single_fixture(
                "core pass",
                "core/01.json",
                vec![ConformanceClass::Core],
                vec!["GQL-002"],
                Some("agent-a"),
                "{ ok }",
                json!({"status": 200, "data": {"ok": true}}),
            ),
            make_single_fixture(
                "tasks skipped-by-class-filter",
                "tasks/01.json",
                vec![ConformanceClass::Tasks],
                vec!["GQL-002"],
                None,
                "{ task }",
                json!({"status": 200}),
            ),
            make_single_fixture(
                "secure missing principal",
                "secure/01.json",
                vec![ConformanceClass::Secure],
                vec!["INV-15"],
                Some("unmapped-agent"),
                "{ secretOp }",
                json!({"status": 200}),
            ),
        ];

        let canary = "tok-agent-a-canary-555111";
        let config = RunnerConfig {
            classes: vec![ConformanceClass::Core, ConformanceClass::Secure],
            principals: PrincipalsMap::from_entries([("agent-a", canary)]),
        };

        let report = run_suite(&driver, &fixtures, &config, None).await;
        assert_eq!(
            report.summary.total, 2,
            "Tasks fixture filtered out; unmapped principal kept"
        );
        assert_eq!(report.summary.passed, 1);
        assert_eq!(report.summary.errors, 1);
        assert_eq!(report.fixtures[0].verdict, Verdict::Pass);
        assert_eq!(report.fixtures[1].verdict, Verdict::Error);
        assert!(
            report.fixtures[1]
                .detail
                .as_deref()
                .is_some_and(|d| d.contains("unmapped-agent"))
        );

        // Only `core pass` reached the driver; `unmapped-agent` must not be sent without its token.
        let calls = driver.recorded_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "{ ok }");
        assert_eq!(calls[0].1.as_deref(), Some(canary));

        // Report JSON must never contain `canary`.
        let report_json = report.to_json_pretty().expect("serialize report");
        assert!(!report_json.contains(canary));
    }

    // spec: GQL-002, INV-15 (VER-010)
    #[tokio::test]
    async fn steps_short_circuit_on_first_failure_and_scrub_any_secret_in_detail() {
        let step0_ok = ObservedResponse::from_parts(
            200,
            std::iter::empty::<(&str, &str)>(),
            json!({"data": {"step": 1}}).as_object(),
        );
        let step1_wrong = ObservedResponse::from_parts(
            200,
            std::iter::empty::<(&str, &str)>(),
            json!({"data": {"step": 999}}).as_object(),
        );
        let step2_would_ok = ObservedResponse::from_parts(
            200,
            std::iter::empty::<(&str, &str)>(),
            json!({"data": {"step": 3}}).as_object(),
        );

        let canary = "tok-step-canary-888000";
        let leak_err = DriverError::Transport {
            target: "http://127.0.0.1:18080/graphql".to_owned(),
            reason: format!("simulated upstream echo of {canary}"),
        };

        let driver = ScriptedDriver::new(vec![
            Ok(step0_ok),
            Ok(step1_wrong),
            Ok(step2_would_ok),
            Err(leak_err),
        ]);

        let multi_step = LoadedFixture {
            path: PathBuf::from("core/steps.json"),
            relative_path: "core/steps.json".to_owned(),
            name: "three step flow".to_owned(),
            class: vec![ConformanceClass::Core],
            spec: vec!["GQL-002".to_owned()],
            schema: None,
            principal: Some("agent-step".to_owned()),
            execution: FixtureExecution::Steps(vec![
                FixtureStep {
                    request: FixtureRequest {
                        query: "{ s1 }".to_owned(),
                        variables: None,
                        operation_name: None,
                        extensions: None,
                    },
                    expect: json!({"status": 200, "data": {"step": 1}}),
                },
                FixtureStep {
                    request: FixtureRequest {
                        query: "{ s2 }".to_owned(),
                        variables: None,
                        operation_name: None,
                        extensions: None,
                    },
                    expect: json!({"status": 200, "data": {"step": 2}}),
                },
                FixtureStep {
                    request: FixtureRequest {
                        query: "{ s3 }".to_owned(),
                        variables: None,
                        operation_name: None,
                        extensions: None,
                    },
                    expect: json!({"status": 200, "data": {"step": 3}}),
                },
            ]),
        };

        let driver_err_fixture = make_single_fixture(
            "transport leak scrub",
            "core/err.json",
            vec![ConformanceClass::Core],
            vec!["INV-15"],
            Some("agent-step"),
            "{ leak }",
            json!({"status": 200}),
        );

        // Wait: because `multi_step` stops at step 1 (consuming 2 outcomes), we only want the next
        // outcome in `driver` to be `Err(leak_err)`! Let's build a dedicated 3-outcome driver so
        // `step2_would_ok` is NOT sitting between step 1 and `driver_err_fixture`.
        let _ = driver;
        let step0_ok_b = ObservedResponse::from_parts(
            200,
            std::iter::empty::<(&str, &str)>(),
            json!({"data": {"step": 1}}).as_object(),
        );
        let step1_wrong_b = ObservedResponse::from_parts(
            200,
            std::iter::empty::<(&str, &str)>(),
            json!({"data": {"step": 999}}).as_object(),
        );
        let leak_err_b = DriverError::Transport {
            target: "http://127.0.0.1:18080/graphql".to_owned(),
            reason: format!("simulated upstream echo of {canary}"),
        };
        let exact_driver =
            ScriptedDriver::new(vec![Ok(step0_ok_b), Ok(step1_wrong_b), Err(leak_err_b)]);

        let config = RunnerConfig {
            classes: Vec::new(),
            principals: PrincipalsMap::from_entries([("agent-step", canary)]),
        };
        let report = run_suite(
            &exact_driver,
            &[multi_step, driver_err_fixture],
            &config,
            None,
        )
        .await;

        // `multi_step` ran `{ s1 }` and `{ s2 }`, stopped before `{ s3 }`, and then `driver_err_fixture` ran `{ leak }`.
        let calls: Vec<String> = exact_driver
            .recorded_calls()
            .into_iter()
            .map(|(q, _)| q)
            .collect();
        assert_eq!(calls, vec!["{ s1 }", "{ s2 }", "{ leak }"]);

        assert_eq!(report.fixtures[0].verdict, Verdict::Fail);
        assert!(
            report.fixtures[0]
                .detail
                .as_deref()
                .is_some_and(|d| d.contains("step 1") && d.contains("$.data.step"))
        );

        assert_eq!(report.fixtures[1].verdict, Verdict::Error);
        let err_detail = report.fixtures[1].detail.as_deref().expect("error detail");
        assert!(
            !err_detail.contains(canary),
            "secret must be scrubbed from detail"
        );
        assert!(err_detail.contains("***"));
    }
}
