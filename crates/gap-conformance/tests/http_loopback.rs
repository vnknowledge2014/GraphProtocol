//! In-process `axum` `127.0.0.1:0` loopback integration tests for `HttpDriver` (`GQL-003`,
//! `INV-15`, `VER-011`).
//!
//! Note: inside environments where macOS Seatbelt blocks loopback `connect()` with
//! `Operation not permitted (os error 1)`, these tests intentionally remain active (not skipped or
//! `#[ignore]`d) per `WP-M0-09` protocol so the reviewer runs them outside the sandbox.

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::body::Bytes;
use axum::extract::OriginalUri;
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::response::IntoResponse;
use axum::routing::{any, post};
use gap_conformance::driver::{
    ConformanceDriver, GRAPHQL_ACCEPT_HEADER, HttpDriver, PrincipalsMap,
};
use gap_conformance::fixture::{ConformanceClass, FixtureExecution, FixtureRequest, LoadedFixture};
use gap_conformance::report::Verdict;
use gap_conformance::runner::{RunnerConfig, run_suite};
use gap_core::secret::SecretString;
use gap_testkit::port::PortAllocator;
use serde_json::{Value, json};
use tokio::io::AsyncWriteExt;

#[derive(Debug, Clone)]
struct CapturedHttpCall {
    method: String,
    uri: String,
    accept: Option<String>,
    content_type: Option<String>,
    authorization: Option<String>,
    body: Value,
}

// spec: GQL-003, INV-15 (VER-011)
#[tokio::test]
async fn http_driver_posts_to_exact_axum_loopback_url_without_appending_path() {
    let captured: Arc<Mutex<Vec<CapturedHttpCall>>> = Arc::new(Mutex::new(Vec::new()));
    let captured_for_handler = Arc::clone(&captured);

    let app = Router::new().fallback(any(
        move |method: Method, OriginalUri(uri): OriginalUri, headers: HeaderMap, body: Bytes| {
            let captured_for_handler = Arc::clone(&captured_for_handler);
            async move {
                let parsed_body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
                let record = CapturedHttpCall {
                    method: method.as_str().to_owned(),
                    uri: uri.to_string(),
                    accept: headers
                        .get("accept")
                        .and_then(|v| v.to_str().ok())
                        .map(str::to_owned),
                    content_type: headers
                        .get("content-type")
                        .and_then(|v| v.to_str().ok())
                        .map(str::to_owned),
                    authorization: headers
                        .get("authorization")
                        .and_then(|v| v.to_str().ok())
                        .map(str::to_owned),
                    body: parsed_body,
                };
                if let Ok(mut guard) = captured_for_handler.lock() {
                    guard.push(record);
                }

                let mut resp_headers = HeaderMap::new();
                resp_headers.insert(
                    "content-type",
                    HeaderValue::from_static("application/graphql-response+json"),
                );
                resp_headers.insert("X-Gap-Generation", HeaderValue::from_static("9"));

                let payload = json!({
                    "data": {"billing": {"invoice": {"total": 125}}},
                    "dataNull": true
                });
                (StatusCode::OK, resp_headers, payload.to_string()).into_response()
            }
        },
    ));

    let reserved = PortAllocator::reserve().expect("reserve loopback listener");
    let addr = reserved.local_addr();
    let listener = tokio::net::TcpListener::from_std(
        reserved
            .into_nonblocking_std()
            .expect("nonblocking std listener"),
    )
    .expect("convert into tokio::net::TcpListener");
    let server_task = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    // Deliberately use a custom path + query string to prove `HttpDriver` posts to `--target`
    // verbatim without appending `/graphql` or stripping query parameters.
    let target_url = format!("http://{addr}/custom/gap-endpoint?tenant=acme");
    let driver = HttpDriver::new(&target_url).expect("construct HttpDriver");
    let bearer = SecretString::new("tok-loopback-secret-12345".to_owned());
    let req = FixtureRequest {
        query: "query Inv($id: ID!) { billing { invoice(id: $id) { total } } }".to_owned(),
        variables: Some(json!({"id": "inv-1"})),
        operation_name: Some("Inv".to_owned()),
        extensions: Some(json!({"gap": {"budget": {"maxCalls": 2}}})),
    };

    let obs = driver
        .execute(&req, Some(&bearer))
        .await
        .expect("execute against loopback axum server");

    server_task.abort();

    assert_eq!(obs.status, 200);
    assert!(
        !obs.data_null,
        "dataNull must be derived, ignoring server field"
    );
    assert_eq!(obs.data, json!({"billing": {"invoice": {"total": 125}}}));
    assert_eq!(obs.errors, json!([]));
    assert_eq!(obs.extensions, json!({}));
    assert_eq!(
        obs.headers.get("x-gap-generation").map(String::as_str),
        Some("9")
    );

    let calls = captured.lock().expect("lock captured calls");
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].method, "POST");
    assert_eq!(calls[0].uri, "/custom/gap-endpoint?tenant=acme");
    assert_eq!(calls[0].accept.as_deref(), Some(GRAPHQL_ACCEPT_HEADER));
    assert!(
        calls[0]
            .content_type
            .as_deref()
            .is_some_and(|ct| ct.starts_with("application/json"))
    );
    assert_eq!(
        calls[0].authorization.as_deref(),
        Some("Bearer tok-loopback-secret-12345")
    );
    assert_eq!(
        calls[0].body,
        json!({
            "query": "query Inv($id: ID!) { billing { invoice(id: $id) { total } } }",
            "variables": {"id": "inv-1"},
            "operationName": "Inv",
            "extensions": {"gap": {"budget": {"maxCalls": 2}}}
        })
    );
}

// spec: GQL-002, GQL-003, INV-15 (VER-010, VER-011)
#[tokio::test]
async fn cli_target_mode_runs_end_to_end_against_axum_and_writes_report() {
    let app = Router::new().fallback(any(|| async {
        let mut resp_headers = HeaderMap::new();
        resp_headers.insert(
            "content-type",
            HeaderValue::from_static("application/graphql-response+json"),
        );
        let payload = json!({"data": {"ok": true}});
        (StatusCode::OK, resp_headers, payload.to_string()).into_response()
    }));

    let reserved = PortAllocator::reserve().expect("reserve loopback listener");
    let addr = reserved.local_addr();
    let listener = tokio::net::TcpListener::from_std(
        reserved
            .into_nonblocking_std()
            .expect("nonblocking std listener"),
    )
    .expect("convert into tokio::net::TcpListener");
    let server_task = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let tmp = tempfile::TempDir::new().expect("tempdir");
    let fixtures_dir = tmp.path().join("fixtures");
    std::fs::create_dir_all(&fixtures_dir).expect("mkdir fixtures");
    std::fs::write(
        fixtures_dir.join("01_ok.json"),
        json!({
            "name": "live target ok",
            "class": ["Core"],
            "spec": ["GQL-002", "GQL-003"],
            "principal": "agent-e2e",
            "request": {"query": "{ ok }"},
            "expect": {"status": 200, "data": {"ok": true}, "dataNull": false}
        })
        .to_string(),
    )
    .expect("write fixture");

    let principals_path = tmp.path().join("principals.json");
    std::fs::write(
        &principals_path,
        json!({"agent-e2e": "tok-e2e-live-canary"}).to_string(),
    )
    .expect("write principals");

    let report_path = tmp.path().join("report.json");
    let target_url = format!("http://{addr}/graphql");

    let mut out = Vec::new();
    let mut err = Vec::new();
    let exit_code = gap_conformance::cli::run_cli_from_args(
        [
            "gap-conformance",
            "--target",
            &target_url,
            "--fixtures",
            fixtures_dir.to_str().expect("utf8"),
            "--class",
            "Core",
            "--principals",
            principals_path.to_str().expect("utf8"),
            "--report",
            report_path.to_str().expect("utf8"),
        ],
        &mut out,
        &mut err,
    )
    .await;

    server_task.abort();

    assert_eq!(
        exit_code,
        0,
        "expected exit code 0; stderr={}; report={:?}",
        String::from_utf8_lossy(&err),
        std::fs::read_to_string(&report_path).ok()
    );
    let report_raw = std::fs::read_to_string(&report_path).expect("read report.json");
    assert!(!report_raw.contains("tok-e2e-live-canary"));
    let report_val: Value = serde_json::from_str(&report_raw).expect("parse report.json");
    assert_eq!(report_val["summary"]["passed"], 1);
    assert_eq!(report_val["coverage"]["GQL-002"]["covered"], true);
}

// spec: GQL-003, INV-15 (VER-011)
#[tokio::test]
async fn http_driver_does_not_follow_redirects_or_forward_bearer_token() {
    let root_hits = Arc::new(AtomicUsize::new(0));
    let final_hits = Arc::new(AtomicUsize::new(0));
    let root_hits_handler = Arc::clone(&root_hits);
    let final_hits_handler = Arc::clone(&final_hits);

    let app = Router::new()
        .route(
            "/",
            post(move |headers: HeaderMap| {
                let root_hits_handler = Arc::clone(&root_hits_handler);
                async move {
                    if headers.get(header::AUTHORIZATION).is_some() {
                        root_hits_handler.fetch_add(1, Ordering::SeqCst);
                    }
                    let mut resp_headers = HeaderMap::new();
                    resp_headers.insert(header::LOCATION, HeaderValue::from_static("/final"));
                    (StatusCode::FOUND, resp_headers, "").into_response()
                }
            }),
        )
        .route(
            "/final",
            any(move || {
                let final_hits_handler = Arc::clone(&final_hits_handler);
                async move {
                    final_hits_handler.fetch_add(1, Ordering::SeqCst);
                    let mut resp_headers = HeaderMap::new();
                    resp_headers.insert(
                        header::CONTENT_TYPE,
                        HeaderValue::from_static("application/graphql-response+json"),
                    );
                    (
                        StatusCode::OK,
                        resp_headers,
                        json!({"data": {"followed": true}}).to_string(),
                    )
                        .into_response()
                }
            }),
        );

    let reserved = PortAllocator::reserve().expect("reserve loopback listener");
    let addr = reserved.local_addr();
    let listener = tokio::net::TcpListener::from_std(
        reserved
            .into_nonblocking_std()
            .expect("nonblocking std listener"),
    )
    .expect("convert into tokio::net::TcpListener");
    let server_task = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let target_url = format!("http://{addr}/");
    let driver = HttpDriver::new(&target_url).expect("construct HttpDriver");
    let bearer = SecretString::new("tok-redirect-canary-998877".to_owned());
    let req = FixtureRequest {
        query: "{ ok }".to_owned(),
        variables: None,
        operation_name: None,
        extensions: None,
    };

    let obs = driver
        .execute(&req, Some(&bearer))
        .await
        .expect("execute against 302 axum server");

    server_task.abort();

    assert_eq!(obs.status, 302);
    assert_eq!(
        obs.headers.get("location").map(String::as_str),
        Some("/final")
    );
    assert!(obs.data_null);
    assert_eq!(
        root_hits.load(Ordering::SeqCst),
        1,
        "POST / must receive the single authenticated request"
    );
    assert_eq!(
        final_hits.load(Ordering::SeqCst),
        0,
        "/final redirect target must never be requested"
    );
}

// spec: GQL-003, INV-15 (VER-011)
#[tokio::test]
async fn http_driver_with_timeouts_errors_when_server_accepts_and_never_replies() {
    let reserved = PortAllocator::reserve().expect("reserve loopback listener");
    let target_url = reserved.http_url();
    let listener = tokio::net::TcpListener::from_std(
        reserved
            .into_nonblocking_std()
            .expect("nonblocking std listener"),
    )
    .expect("convert into tokio::net::TcpListener");

    let accept_task = tokio::spawn(async move {
        let mut held_sockets = Vec::new();
        while let Ok((stream, _peer)) = listener.accept().await {
            held_sockets.push(stream);
        }
    });

    let canary = "tok-timeout-canary-5544332211";
    let driver = HttpDriver::with_timeouts(
        &target_url,
        Duration::from_secs(2),
        Duration::from_millis(200),
    )
    .expect("construct HttpDriver with 200ms total timeout");
    let bearer = SecretString::new(canary.to_owned());
    let req = FixtureRequest {
        query: "{ ok }".to_owned(),
        variables: None,
        operation_name: None,
        extensions: None,
    };

    let err = tokio::time::timeout(Duration::from_secs(5), driver.execute(&req, Some(&bearer)))
        .await
        .expect("HttpDriver::execute must fail within 5s instead of hanging")
        .expect_err("silent accepted socket must time out");

    let err_msg = err.to_string();
    assert!(
        err_msg.to_ascii_lowercase().contains("timeout"),
        "expected error message to contain `timeout`, got: {err_msg}"
    );
    assert!(
        !err_msg.contains(canary),
        "timeout error must never leak principal bearer token"
    );

    // Also verify runner maps the driver timeout to Verdict::Error with scrubbed `timeout` detail.
    let fixture = LoadedFixture {
        path: PathBuf::from("01_hang.json"),
        relative_path: "01_hang.json".to_owned(),
        name: "hanging target times out".to_owned(),
        class: vec![ConformanceClass::Core],
        spec: vec!["GQL-003".to_owned()],
        schema: None,
        principal: Some("alice".to_owned()),
        execution: FixtureExecution::Single {
            request: req,
            expect: json!({"status": 200}),
        },
    };
    let cfg = RunnerConfig {
        classes: vec![],
        principals: PrincipalsMap::from_entries([("alice", canary)]),
    };
    let report = tokio::time::timeout(
        Duration::from_secs(5),
        run_suite(&driver, &[fixture], &cfg, None),
    )
    .await
    .expect("run_suite must return within 5s");

    accept_task.abort();

    assert_eq!(report.summary.errors, 1);
    assert_eq!(report.fixtures[0].verdict, Verdict::Error);
    let detail = report.fixtures[0]
        .detail
        .as_deref()
        .expect("verdict=error must carry detail");
    assert!(
        detail.to_ascii_lowercase().contains("timeout"),
        "expected report detail to contain `timeout`, got: {detail}"
    );
    assert!(!detail.contains(canary));

    // Also verify `total_timeout` covers stalled response-body reads after HTTP 200 headers arrive.
    let reserved_body = PortAllocator::reserve().expect("reserve stalled-body listener");
    let body_url = reserved_body.http_url();
    let body_listener = tokio::net::TcpListener::from_std(
        reserved_body
            .into_nonblocking_std()
            .expect("nonblocking std listener"),
    )
    .expect("convert into tokio::net::TcpListener");

    let stalled_body_task = tokio::spawn(async move {
        if let Ok((mut stream, _)) = body_listener.accept().await {
            let partial = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 64\r\n\r\n{\"data\":";
            let _ = stream.write_all(partial).await;
            let _ = stream.flush().await;
            tokio::time::sleep(Duration::from_secs(60)).await;
        }
    });

    let body_driver = HttpDriver::with_timeouts(
        &body_url,
        Duration::from_secs(2),
        Duration::from_millis(200),
    )
    .expect("construct stalled-body HttpDriver");
    let body_req = FixtureRequest {
        query: "{ ok }".to_owned(),
        variables: None,
        operation_name: None,
        extensions: None,
    };
    let body_err = tokio::time::timeout(
        Duration::from_secs(5),
        body_driver.execute(&body_req, Some(&bearer)),
    )
    .await
    .expect("stalled body read must fail within 5s")
    .expect_err("stalled body read must time out");

    stalled_body_task.abort();

    assert!(
        body_err
            .to_string()
            .to_ascii_lowercase()
            .contains("timeout"),
        "expected stalled-body error to contain `timeout`, got: {body_err}"
    );
    assert!(!body_err.to_string().contains(canary));
}
