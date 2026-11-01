//! `TransportEngine` (`GraphqlEngine` over the pipeline) and mounted protocol facades (`HttpModule`):
//! authentication, snapshot pinning, credential stripping, body caps and error mapping (`ARC-007`, `SEC-010`,
//! `SEC-011`, `INV-01`, `RUN-030`).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::{TestHarness, build_gen1_snapshot_for_tenant, make_api_key, send_oneshot};
use gap_core::context::{Budget, Principal, PrincipalKind, RequestContext, TraceCtx};
use gap_core::error::{ErrorCode, GapError, GapResult};
use gap_core::ids::TenantId;
use gap_core::traits::{
    BoxFuture, GraphqlEngine, GraphqlRequest, HttpExchangeRequest, HttpExchangeResponse, HttpModule,
};
use gap_ngc_sdk::mock::RpcMethod;
use gap_transport::{AdmissionPolicy, Profile, TransportConfig, TransportEngine};
use serde_json::{Value, json};
use uuid::Uuid;

fn context(harness: &TestHarness, deadline_in: Duration) -> RequestContext {
    let tenant = TenantId::default_tenant();
    RequestContext::try_new(
        Uuid::now_v7(),
        tenant.clone(),
        Principal::new("agent-ctx", PrincipalKind::Agent, tenant),
        Instant::now() + deadline_in,
        Budget::new(10_000),
        TraceCtx::generate_root(true),
        0,
        None,
        harness.snapshot_handle.pin(),
    )
    .expect("context")
}

fn query(text: &str) -> GraphqlRequest {
    GraphqlRequest {
        query: Some(text.to_owned()),
        ..GraphqlRequest::default()
    }
}

// spec: ARC-007, INV-03
#[tokio::test(start_paused = true)]
async fn engine_executes_a_document_against_the_context_snapshot() {
    let harness = TestHarness::new(TransportConfig::for_profile(Profile::Dev));
    let engine = TransportEngine::new(Arc::clone(&harness.state));
    let ctx = context(&harness, Duration::from_secs(30));

    let response = engine
        .execute(&ctx, query("{ order(id: \"ord-9\") { id sku } }"))
        .await;
    assert!(response.errors.is_empty(), "{:?}", response.errors);
    assert_eq!(
        response.data.unwrap()["order"],
        json!({"id": "ord-9", "sku": "SKU-ALPHA"})
    );
    assert_eq!(response.extensions["gap"]["generation"], json!(1));
    assert_eq!(response.extensions["gap"]["partial"], json!(false));
}

// spec: ARC-007, GQL-070
#[tokio::test(start_paused = true)]
async fn engine_reports_request_errors_without_data() {
    let harness = TestHarness::new(TransportConfig::for_profile(Profile::Dev));
    let engine = TransportEngine::new(Arc::clone(&harness.state));
    let ctx = context(&harness, Duration::from_secs(30));

    let parse = engine.execute(&ctx, query("{ order(")).await;
    assert!(parse.data.is_none());
    assert_eq!(parse.errors[0].code(), ErrorCode::ParseError);

    let unknown = engine
        .execute(&ctx, query("{ ordr(id: \"1\") { id } }"))
        .await;
    assert!(unknown.data.is_none());
    assert_eq!(unknown.errors[0].code(), ErrorCode::UnknownField);
    assert!(
        unknown.errors[0].extensions.suggestion.is_some(),
        "self-repair hints survive the round trip: {:?}",
        unknown.errors[0]
    );
}

// spec: INV-01
#[tokio::test(start_paused = true)]
async fn engine_uses_the_pinned_snapshot_not_the_current_one() {
    let harness = TestHarness::new(TransportConfig::for_profile(Profile::Dev));
    let engine = TransportEngine::new(Arc::clone(&harness.state));
    let ctx = context(&harness, Duration::from_secs(30));

    // A newer generation is published after the context pinned generation 1.
    let next = common::compose_two_branch_snapshot(&harness.snapshot_handle.pin(), true);
    harness
        .snapshot_handle
        .publish(next)
        .expect("publish generation 2");
    assert_eq!(harness.snapshot_handle.pin().generation, 2);

    let response = engine.execute(&ctx, query("{ shopVersion }")).await;
    assert!(response.errors.is_empty(), "{:?}", response.errors);
    assert_eq!(
        response.extensions["gap"]["generation"],
        json!(1),
        "the context's snapshot wins"
    );
}

// spec: RUN-030, INV-12
#[tokio::test(start_paused = true)]
async fn engine_never_outlives_the_context_deadline() {
    let harness = TestHarness::new(TransportConfig::for_profile(Profile::Dev));
    let engine = TransportEngine::new(Arc::clone(&harness.state));
    let ctx = context(&harness, Duration::ZERO);

    let mut request = query("{ shopVersion }");
    request
        .extensions
        .insert("gap".to_owned(), json!({ "deadlineMs": 60_000 }));
    let response = engine.execute(&ctx, request).await;
    assert!(
        response
            .errors
            .iter()
            .any(|e| e.code() == ErrorCode::DeadlineExceeded),
        "an expired context must not run with the client's larger deadline: {response:?}"
    );
    assert_eq!(
        harness.shop_mock.calls_for(RpcMethod::Query),
        0,
        "no connector call after the deadline"
    );
}

// spec: RUN-031, INV-12
#[tokio::test(start_paused = true)]
async fn dropping_the_engine_future_cancels_the_connector_call() {
    use futures_util::stream;
    use gap_ngc_sdk::connector::BoxStream;
    use gap_proto::ngc::v1::QueryResponse;
    use tokio::sync::Notify;
    use tokio_util::sync::CancellationToken;

    let started = Arc::new(Notify::new());
    let observed: Arc<Mutex<Option<CancellationToken>>> = Arc::new(Mutex::new(None));
    let (started_in, observed_in) = (Arc::clone(&started), Arc::clone(&observed));
    let slow_shop = gap_ngc_sdk::mock::MockConnector::new().on_query(move |_req, scope| {
        *observed_in.lock().unwrap() = Some(scope.cancel.clone());
        started_in.notify_one();
        let pending: BoxStream<QueryResponse> = Box::pin(stream::pending());
        Ok(pending)
    });
    let harness = TestHarness::with_mocks(
        TransportConfig::for_profile(Profile::Dev),
        slow_shop.clone(),
        common::default_crm_mock(),
        None,
        None,
    );
    let engine = TransportEngine::new(Arc::clone(&harness.state));
    let ctx = context(&harness, Duration::from_secs(30));

    let mut fut = Box::pin(engine.execute(&ctx, query("{ shopVersion }")));
    tokio::select! {
        _ = &mut fut => panic!("a pending connector stream cannot finish"),
        () = started.notified() => {}
    }
    assert_eq!(slow_shop.in_flight(), 1);

    drop(fut);
    tokio::time::timeout(Duration::from_millis(50), slow_shop.wait_for_drops(1))
        .await
        .expect("the connector stream must be dropped within 50 ms of dropping the engine future");
    assert_eq!(slow_shop.in_flight(), 0);
    assert!(
        observed
            .lock()
            .unwrap()
            .clone()
            .expect("scope recorded")
            .is_cancelled(),
        "the connector's CallScope must be cancelled"
    );
}

#[derive(Debug, Clone)]
struct Seen {
    principal_id: String,
    generation: u64,
    method: String,
    path: String,
    query: Option<String>,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

#[derive(Debug)]
struct EchoModule {
    seen: Arc<Mutex<Vec<Seen>>>,
    behaviour: Behaviour,
}

#[derive(Debug, Clone, Copy)]
enum Behaviour {
    Ok,
    RateLimited,
    SleepFor(Duration),
}

impl HttpModule for EchoModule {
    fn module_name(&self) -> &'static str {
        "echo"
    }

    fn mount_prefixes(&self) -> &'static [&'static str] {
        &["/echo", "/.well-known/echo.json"]
    }

    fn handle<'a>(
        &'a self,
        ctx: &'a RequestContext,
        req: HttpExchangeRequest,
    ) -> BoxFuture<'a, GapResult<HttpExchangeResponse>> {
        Box::pin(async move {
            self.seen.lock().unwrap().push(Seen {
                principal_id: ctx.principal.id.to_string(),
                generation: ctx.snapshot.generation,
                method: req.method.clone(),
                path: req.path.clone(),
                query: req.query.clone(),
                headers: req.headers.clone(),
                body: req.body.clone(),
            });
            match self.behaviour {
                Behaviour::Ok => Ok(HttpExchangeResponse {
                    status: 202,
                    headers: vec![("x-echo".to_owned(), "yes".to_owned())],
                    body: br#"{"ok":true}"#.to_vec(),
                }),
                Behaviour::RateLimited => Err(GapError::new(ErrorCode::RateLimited, "slow down")
                    .with_retry(true, Some(2_000))),
                Behaviour::SleepFor(d) => {
                    tokio::time::sleep(d).await;
                    Ok(HttpExchangeResponse {
                        status: 200,
                        headers: Vec::new(),
                        body: Vec::new(),
                    })
                }
            }
        })
    }
}

fn echo_harness(
    behaviour: Behaviour,
    mut config: TransportConfig,
) -> (TestHarness, Arc<Mutex<Vec<Seen>>>) {
    config
        .api_keys
        .push(make_api_key("agent-7", "key-7", "default", &["read"]));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let module: Arc<dyn HttpModule> = Arc::new(EchoModule {
        seen: Arc::clone(&seen),
        behaviour,
    });
    (TestHarness::with_modules(config, vec![module]), seen)
}

async fn call(harness: &TestHarness, request: Request<Body>) -> common::ObservedHttpResponse {
    send_oneshot(&harness.router, request).await
}

// spec: ARC-007, SEC-010, SEC-011, INV-01
#[tokio::test(start_paused = true)]
async fn a_module_gets_an_authenticated_context_and_no_client_credentials() {
    let (harness, seen) = echo_harness(Behaviour::Ok, TransportConfig::for_profile(Profile::Dev));
    let response = call(
        &harness,
        Request::builder()
            .method("POST")
            .uri("/echo/x?a=1")
            .header("authorization", "Bearer key-7")
            .header("mcp-method", "tools/list")
            .header("cookie", "session=secret")
            .header("x-request-id", "req-abc-1")
            .body(Body::from("payload"))
            .unwrap(),
    )
    .await;
    assert_eq!(response.status, StatusCode::ACCEPTED, "{}", response.text());
    assert_eq!(response.json(), json!({"ok": true}));
    assert_eq!(
        response.headers.get("x-echo").and_then(|v| v.to_str().ok()),
        Some("yes")
    );
    assert_eq!(
        response
            .headers
            .get("x-request-id")
            .and_then(|v| v.to_str().ok()),
        Some("req-abc-1")
    );

    let seen = seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 1);
    let s = &seen[0];
    assert_eq!(
        s.principal_id, "agent-7",
        "tenant and principal come only from authentication"
    );
    assert_eq!(s.generation, 1);
    assert_eq!(
        (s.method.as_str(), s.path.as_str(), s.query.as_deref()),
        ("POST", "/echo/x", Some("a=1"))
    );
    assert_eq!(s.body, b"payload");
    assert!(
        s.headers
            .iter()
            .any(|(k, v)| k == "mcp-method" && v == "tools/list"),
        "{:?}",
        s.headers
    );
    assert!(
        !s.headers.iter().any(|(k, v)| k == "authorization"
            || k == "cookie"
            || v.contains("secret")
            || v.contains("key-7")),
        "credentials must be stripped: {:?}",
        s.headers
    );
}

// spec: ARC-007
#[tokio::test(start_paused = true)]
async fn only_declared_prefixes_reach_a_module() {
    let (harness, seen) = echo_harness(Behaviour::Ok, TransportConfig::for_profile(Profile::Dev));
    for (path, expected) in [
        ("/echo", StatusCode::ACCEPTED),
        ("/echo/deep/path", StatusCode::ACCEPTED),
        ("/.well-known/echo.json", StatusCode::ACCEPTED),
        ("/echoes", StatusCode::NOT_FOUND),
        ("/ech", StatusCode::NOT_FOUND),
        ("/.well-known/other.json", StatusCode::NOT_FOUND),
        ("/", StatusCode::NOT_FOUND),
    ] {
        let response = call(
            &harness,
            Request::builder().uri(path).body(Body::empty()).unwrap(),
        )
        .await;
        assert_eq!(response.status, expected, "{path}");
    }
    assert_eq!(seen.lock().unwrap().len(), 3);
}

// spec: SEC-010
#[tokio::test(start_paused = true)]
async fn a_module_is_unreachable_without_valid_credentials_in_prod_and_with_a_wrong_key_in_dev() {
    let (prod, seen_prod) =
        echo_harness(Behaviour::Ok, TransportConfig::for_profile(Profile::Prod));
    let anonymous = call(
        &prod,
        Request::builder().uri("/echo").body(Body::empty()).unwrap(),
    )
    .await;
    assert_eq!(anonymous.status, StatusCode::UNAUTHORIZED);
    assert_eq!(
        anonymous
            .headers
            .get("www-authenticate")
            .and_then(|v| v.to_str().ok()),
        Some("Bearer")
    );
    assert!(seen_prod.lock().unwrap().is_empty());

    let (dev, seen_dev) = echo_harness(Behaviour::Ok, TransportConfig::for_profile(Profile::Dev));
    let wrong = call(
        &dev,
        Request::builder()
            .uri("/echo")
            .header("authorization", "Bearer not-a-key")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(wrong.status, StatusCode::UNAUTHORIZED);
    assert!(seen_dev.lock().unwrap().is_empty());
}

// spec: GQL-003, INV-02
#[tokio::test(start_paused = true)]
async fn a_module_body_is_bounded_before_the_module_sees_it() {
    let mut config = TransportConfig::for_profile(Profile::Dev);
    config.limits.max_request_body_bytes = 16_384;
    let (harness, seen) = echo_harness(Behaviour::Ok, config);

    let declared = call(
        &harness,
        Request::builder()
            .method("POST")
            .uri("/echo")
            .header("content-length", "16385")
            .body(Body::from("tiny"))
            .unwrap(),
    )
    .await;
    assert_eq!(declared.status, StatusCode::PAYLOAD_TOO_LARGE);

    let actual = call(
        &harness,
        Request::builder()
            .method("POST")
            .uri("/echo")
            .body(Body::from(vec![b'x'; 16_385]))
            .unwrap(),
    )
    .await;
    assert_eq!(actual.status, StatusCode::PAYLOAD_TOO_LARGE);

    let at_cap = call(
        &harness,
        Request::builder()
            .method("POST")
            .uri("/echo")
            .body(Body::from(vec![b'x'; 16_384]))
            .unwrap(),
    )
    .await;
    assert_eq!(at_cap.status, StatusCode::ACCEPTED);
    assert_eq!(
        seen.lock().unwrap().len(),
        1,
        "only the exact-fit body reached the module"
    );
}

#[derive(Debug)]
struct RefuseAll;

impl AdmissionPolicy for RefuseAll {
    fn check(
        &self,
        _principal: &Principal,
        _headers: &axum::http::HeaderMap,
    ) -> Result<(), GapError> {
        Err(GapError::new(ErrorCode::Overloaded, "shed").with_retry(true, Some(1_000)))
    }
}

// spec: INV-02, RUN-040
#[tokio::test(start_paused = true)]
async fn admission_control_applies_to_modules_before_they_run() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let module: Arc<dyn HttpModule> = Arc::new(EchoModule {
        seen: Arc::clone(&seen),
        behaviour: Behaviour::Ok,
    });
    let harness = TestHarness::with_modules_and_admission(
        TransportConfig::for_profile(Profile::Dev),
        vec![module],
        Arc::new(RefuseAll),
    );
    let response = call(
        &harness,
        Request::builder().uri("/echo").body(Body::empty()).unwrap(),
    )
    .await;
    assert_eq!(
        response.status,
        StatusCode::SERVICE_UNAVAILABLE,
        "{}",
        response.text()
    );
    assert_eq!(
        response.json()["errors"][0]["extensions"]["code"],
        "GAP_OVERLOADED"
    );
    assert!(
        seen.lock().unwrap().is_empty(),
        "a shed request never reaches the module"
    );
}

// spec: GQL-003 (spec/02 section 10)
#[tokio::test(start_paused = true)]
async fn a_module_error_becomes_the_matching_http_status() {
    let (harness, _) = echo_harness(
        Behaviour::RateLimited,
        TransportConfig::for_profile(Profile::Dev),
    );
    let response = call(
        &harness,
        Request::builder().uri("/echo").body(Body::empty()).unwrap(),
    )
    .await;
    assert_eq!(
        response.status,
        StatusCode::TOO_MANY_REQUESTS,
        "{}",
        response.text()
    );
    let body: Value = response.json();
    assert_eq!(body["errors"][0]["extensions"]["code"], "GAP_RATE_LIMITED");
    assert!(body.get("data").is_none());
}

// spec: RUN-030
#[tokio::test(start_paused = true)]
async fn a_module_that_outlives_the_request_deadline_is_cut_off() {
    let mut config = TransportConfig::for_profile(Profile::Dev);
    config.default_deadline = Duration::from_secs(2);
    config.max_deadline = Duration::from_secs(2);
    let (harness, _) = echo_harness(Behaviour::SleepFor(Duration::from_secs(60)), config);
    let response = call(
        &harness,
        Request::builder().uri("/echo").body(Body::empty()).unwrap(),
    )
    .await;
    assert_eq!(
        response.status,
        StatusCode::GATEWAY_TIMEOUT,
        "{}",
        response.text()
    );
    assert_eq!(
        response.json()["errors"][0]["extensions"]["code"],
        "GAP_DEADLINE_EXCEEDED"
    );
}

// spec: OPS-020, INV-15
#[tokio::test(start_paused = true)]
async fn module_requests_are_counted_under_one_bounded_route_label() {
    let (harness, _) = echo_harness(Behaviour::Ok, TransportConfig::for_profile(Profile::Dev));
    for _ in 0..2 {
        call(
            &harness,
            Request::builder()
                .uri("/echo/anything/at/all")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    }
    let metrics = call(
        &harness,
        Request::builder()
            .uri("/metrics")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    let text = metrics.text();
    // `202` is outside the closed status set of the counter and is bucketed as `other`.
    assert!(
        text.contains(r#"gap_http_requests_total{route="module",status="other"} 2"#),
        "{text}"
    );
    assert!(
        !text.contains("/echo/anything"),
        "no user-controlled label values"
    );
    let _ = build_gen1_snapshot_for_tenant;
}
