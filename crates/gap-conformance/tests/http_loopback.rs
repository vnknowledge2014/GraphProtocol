//! In-process `axum` `127.0.0.1:0` loopback integration tests for `HttpDriver` (`GQL-003`,
//! `INV-15`, `VER-011`).
//!
//! Note: inside environments where macOS Seatbelt blocks loopback `connect()` with
//! `Operation not permitted (os error 1)`, these tests intentionally remain active (not skipped or
//! `#[ignore]`d) per `WP-M0-09` protocol so the reviewer runs them outside the sandbox.

use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Bytes;
use axum::extract::OriginalUri;
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode};
use axum::response::IntoResponse;
use axum::routing::any;
use gap_conformance::driver::{ConformanceDriver, GRAPHQL_ACCEPT_HEADER, HttpDriver};
use gap_conformance::fixture::FixtureRequest;
use gap_core::secret::SecretString;
use serde_json::{Value, json};

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

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind 127.0.0.1:0");
    let addr = listener.local_addr().expect("local_addr");
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
