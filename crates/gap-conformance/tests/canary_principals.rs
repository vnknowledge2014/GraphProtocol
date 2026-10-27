//! End-to-end `INV-15` canary proof for `gap-conformance` principal bearer tokens (`VER-010`, `VER-011`).

use std::fs;

use gap_conformance::cli::{CliArgs, EXIT_CONFORMANCE_FAILURE, EXIT_OK, execute_target_mode};
use gap_conformance::driver::{
    ConformanceDriver, DriverError, GRAPHQL_ACCEPT_HEADER, HttpDriver, PrincipalsMap,
};
use gap_conformance::fixture::{ConformanceClass, FixtureRequest, load_fixtures_dir};
use gap_conformance::matcher::ObservedResponse;
use gap_conformance::runner::{RunnerConfig, run_suite};
use gap_core::secret::SecretString;
use gap_core::traits::BoxFuture;
use gap_testkit::canary::CanarySink;
use gap_testkit::port::PortAllocator;
use gap_testkit::tracing_capture::TracingCapture;
use serde_json::{Value, json};
use tempfile::TempDir;

const CANARY_TOKEN: &str = "gap-principal-canary-bearer-778899aabbccddeeff";

/// Driver that simulates both normal responses AND upstream/transport errors that accidentally
/// echo the bearer token, verifying `gap-conformance` scrubs and redacts it everywhere (`INV-15`).
#[derive(Debug)]
struct AdversarialEchoDriver;

impl ConformanceDriver for AdversarialEchoDriver {
    fn target(&self) -> &str {
        "http://127.0.0.1:18080/gap"
    }

    fn execute<'a>(
        &'a self,
        request: &'a FixtureRequest,
        bearer_token: Option<&'a SecretString>,
    ) -> BoxFuture<'a, Result<ObservedResponse, DriverError>> {
        let query = request.query.clone();
        Box::pin(async move {
            tracing::info!(
                has_token = bearer_token.is_some(),
                redacted_token = ?bearer_token,
                "executing adversarial driver call"
            );

            if query.contains("trigger_transport_echo") {
                let raw_tok = bearer_token
                    .map(|s| s.expose_secret().as_str())
                    .unwrap_or("none");
                return Err(DriverError::Transport {
                    target: "http://127.0.0.1:18080/gap".to_owned(),
                    reason: format!("proxy rejected Authorization: Bearer {raw_tok}"),
                });
            }

            if query.contains("trigger_mismatch_echo") {
                let raw_tok = bearer_token
                    .map(|s| s.expose_secret().as_str())
                    .unwrap_or("none");
                let body = json!({
                    "data": { "echo": format!("leaked:{raw_tok}") }
                });
                return Ok(ObservedResponse::from_parts(
                    200,
                    std::iter::empty::<(&str, &str)>(),
                    body.as_object(),
                ));
            }

            let body = json!({
                "data": { "viewer": "alice" }
            });
            Ok(ObservedResponse::from_parts(
                200,
                [("content-type", "application/graphql-response+json")],
                body.as_object(),
            ))
        })
    }
}

// spec: INV-15, SEC-040, SEC-041 (VER-010, VER-011)
#[tokio::test]
async fn inv_15_principal_bearer_tokens_never_appear_in_logs_errors_debug_or_report() {
    let sink = CanarySink::new(CANARY_TOKEN).expect("valid canary");
    let dir = TempDir::new().expect("tempdir");

    let principals_path = dir.path().join("principals.json");
    fs::write(
        &principals_path,
        serde_json::to_string_pretty(&json!({
            "alice": CANARY_TOKEN
        }))
        .expect("serialize principals fixture"),
    )
    .expect("write principals.json");

    let fixtures_dir = dir.path().join("fixtures");
    fs::create_dir_all(&fixtures_dir).expect("mkdir fixtures");

    // 1. Passing authenticated fixture
    fs::write(
        fixtures_dir.join("01-pass.json"),
        serde_json::to_string_pretty(&json!({
            "name": "pass with principal",
            "class": ["Secure"],
            "spec": ["INV-15", "SEC-040"],
            "principal": "alice",
            "request": { "query": "{ viewer }" },
            "expect": { "status": 200, "data": { "viewer": "alice" } }
        }))
        .expect("json"),
    )
    .expect("write 01-pass.json");

    // 2. Failing authenticated fixture whose upstream body echoes the token into mismatch output
    fs::write(
        fixtures_dir.join("02-fail-echo.json"),
        serde_json::to_string_pretty(&json!({
            "name": "fail with echoed token",
            "class": ["Secure"],
            "spec": ["INV-15"],
            "principal": "alice",
            "request": { "query": "{ trigger_mismatch_echo }" },
            "expect": { "status": 200, "data": { "echo": "expected_clean" } }
        }))
        .expect("json"),
    )
    .expect("write 02-fail-echo.json");

    // 3. Erroring authenticated fixture whose transport error echoes the token
    fs::write(
        fixtures_dir.join("03-error-echo.json"),
        serde_json::to_string_pretty(&json!({
            "name": "transport error with echoed token",
            "class": ["Secure"],
            "spec": ["INV-15"],
            "principal": "alice",
            "request": { "query": "{ trigger_transport_echo }" },
            "expect": { "status": 200 }
        }))
        .expect("json"),
    )
    .expect("write 03-error-echo.json");

    // 4. Unmapped principal fixture (must report verdict=error, never skipped)
    fs::write(
        fixtures_dir.join("04-unmapped.json"),
        serde_json::to_string_pretty(&json!({
            "name": "unmapped principal bob",
            "class": ["Secure"],
            "spec": ["SEC-041"],
            "principal": "bob",
            "request": { "query": "{ viewer }" },
            "expect": { "status": 200 }
        }))
        .expect("json"),
    )
    .expect("write 04-unmapped.json");

    let report_path = dir.path().join("report.json");

    let (
        (principals, http_req, http_exec_res, suite_report, cli_exit, stdout_bytes, stderr_bytes),
        capture,
    ) = TracingCapture::with_captured_async(async {
        let principals = PrincipalsMap::load_file(&principals_path).expect("load principals");
        let secret = principals.get("alice").expect("alice token");

        // Build real HttpDriver outbound request and verify Authorization header wire value vs Debug redaction
        let reserved = PortAllocator::reserve().expect("reserve loopback listener");
        let target_url = reserved.http_url();
        let tokio_listener = tokio::net::TcpListener::from_std(
            reserved
                .into_nonblocking_std()
                .expect("nonblocking std listener"),
        )
        .expect("convert into tokio::net::TcpListener");
        let http_driver = HttpDriver::new(target_url).expect("build HttpDriver");
        let sample_req = FixtureRequest {
            query: "{ viewer }".to_owned(),
            variables: None,
            operation_name: None,
            extensions: None,
        };
        let http_req = http_driver
            .build_http_request(&sample_req, Some(secret))
            .expect("build outbound reqwest::Request");
        assert_eq!(
            http_req
                .headers()
                .get("accept")
                .and_then(|v| v.to_str().ok()),
            Some(GRAPHQL_ACCEPT_HEADER)
        );
        assert_eq!(
            http_req
                .headers()
                .get("authorization")
                .and_then(|v| v.to_str().ok()),
            Some(format!("Bearer {CANARY_TOKEN}")).as_deref(),
            "wire Authorization header must carry Bearer token"
        );
        assert!(
            http_req
                .headers()
                .get("authorization")
                .expect("authorization header")
                .is_sensitive(),
            "Authorization HeaderValue must be marked sensitive"
        );

        // Also exercise real HttpDriver::execute (whether loopback succeeds or errors, neither logs nor errors may leak)
        drop(tokio_listener);
        let http_exec_res = http_driver.execute(&sample_req, Some(secret)).await;

        // Run suite against AdversarialEchoDriver
        let loaded = load_fixtures_dir(&fixtures_dir).expect("load fixtures");
        let cfg = RunnerConfig {
            classes: vec![ConformanceClass::Secure],
            principals: principals.clone(),
        };
        let suite_report = run_suite(&AdversarialEchoDriver, &loaded, &cfg, None).await;

        // Also run CLI target mode end-to-end writing report.json
        let cli = CliArgs {
            target: Some("http://127.0.0.1:18080/gap".to_owned()),
            fixtures: fixtures_dir.clone(),
            classes: vec![ConformanceClass::Secure],
            principals: Some(principals_path.clone()),
            report: Some(report_path.clone()),
            offline: None,
            spawn: None,
            connector_kit: false,
        };
        let mut stdout_buf = Vec::new();
        let mut stderr_buf = Vec::new();
        let cli_exit = execute_target_mode(
            &AdversarialEchoDriver,
            &cli,
            &mut stdout_buf,
            &mut stderr_buf,
        )
        .await;

        (
            principals,
            http_req,
            http_exec_res,
            suite_report,
            cli_exit,
            stdout_buf,
            stderr_buf,
        )
    })
    .await;

    assert_eq!(cli_exit, EXIT_CONFORMANCE_FAILURE);
    assert_eq!(suite_report.summary.total, 4);
    assert_eq!(suite_report.summary.passed, 1);
    assert_eq!(suite_report.summary.failed, 1);
    assert_eq!(suite_report.summary.errors, 2);

    // Verify pass-only CLI exit code is 0 too
    let pass_only_dir = dir.path().join("pass_only");
    fs::create_dir_all(&pass_only_dir).expect("mkdir pass_only");
    fs::copy(
        fixtures_dir.join("01-pass.json"),
        pass_only_dir.join("01-pass.json"),
    )
    .expect("copy 01-pass.json");
    let mut pass_out = Vec::new();
    let mut pass_err = Vec::new();
    let pass_exit = execute_target_mode(
        &AdversarialEchoDriver,
        &CliArgs {
            target: Some("http://127.0.0.1:18080/gap".to_owned()),
            fixtures: pass_only_dir,
            classes: vec![],
            principals: Some(principals_path.clone()),
            report: None,
            offline: None,
            spawn: None,
            connector_kit: false,
        },
        &mut pass_out,
        &mut pass_err,
    )
    .await;
    assert_eq!(pass_exit, EXIT_OK);

    // Assert zero canary leakage across EVERY surface!
    sink.check_tracing(&capture)
        .expect("captured tracing spans and events must not contain bearer token");
    sink.check_debug(&principals)
        .expect("PrincipalsMap Debug must redact bearer token");
    sink.check_debug(&http_req)
        .expect("reqwest::Request Debug must redact sensitive Authorization header");
    if let Err(ref transport_err) = http_exec_res {
        sink.check_debug(transport_err)
            .expect("HttpDriver error Debug must not contain bearer token");
        sink.check_display(transport_err)
            .expect("HttpDriver error Display must not contain bearer token");
    }
    sink.check_debug(&suite_report)
        .expect("ConformanceReport Debug must not contain bearer token");
    sink.check_serializable(&suite_report)
        .expect("ConformanceReport JSON must not contain bearer token");

    let report_text = fs::read_to_string(&report_path).expect("read report.json");
    sink.check_report_text(&report_text)
        .expect("written report.json text must not contain bearer token");
    let report_json: Value = serde_json::from_str(&report_text).expect("parse report.json");
    sink.check_json_value(&report_json)
        .expect("parsed report.json Value must not contain bearer token");

    sink.check_report_text(&String::from_utf8_lossy(&stdout_bytes))
        .expect("CLI stdout must not contain bearer token");
    sink.check_report_text(&String::from_utf8_lossy(&stderr_bytes))
        .expect("CLI stderr must not contain bearer token");
}
