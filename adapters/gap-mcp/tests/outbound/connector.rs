//! Connector behaviour against the in-memory fake server (`CMP-010..015`, `NGC-010`, `NGC-020`, `NGC-025`, `NGC-026`, `INV-12`, `INV-15`, `INV-18`).

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use gap_core::error::NgcErrorCode;
use gap_mcp::outbound::fake::{FakeCall, FakeMcpServer, FakeTool, ServerEra};
use gap_mcp::outbound::{McpConfig, OpClassification};
use gap_ngc_sdk::connector::Connector;
use gap_proto::ngc::v1::{DescribeRequest, HealthRequest, OpKind, Page, ServingStatus};
use serde_json::{Value, json};

use super::{collect, echo_server, foreach_query, http_connector, query, scope, stdio_connector};

const ERAS: [ServerEra; 2] = [ServerEra::Modern, ServerEra::Legacy];

// spec: CMP-010, CMP-011, CMP-012
#[tokio::test(start_paused = true)]
async fn outbound_solo_query_maps_results_over_both_eras_and_both_transports() {
    for era in ERAS {
        let server = echo_server(era);
        let http = http_connector(&server, McpConfig::default());
        let stdio = stdio_connector(&server, McpConfig::default());
        for (label, rows) in [
            (
                "http",
                collect(
                    http.query(query("echo", json!({"id": 7, "tag": "x"})), scope(30))
                        .await
                        .expect("query"),
                )
                .await
                .expect("rows"),
            ),
            (
                "stdio",
                collect(
                    stdio
                        .query(query("echo", json!({"id": 7, "tag": "x"})), scope(30))
                        .await
                        .expect("query"),
                )
                .await
                .expect("rows"),
            ),
        ] {
            assert_eq!(rows.rows[&0].len(), 1, "{era:?} {label}");
            assert_eq!(
                rows.rows[&0][0]["echo"]["id"].as_f64(),
                Some(7.0),
                "{era:?} {label}"
            );
            assert_eq!(
                rows.rows[&0][0]["echo"]["tag"],
                json!("x"),
                "{era:?} {label}"
            );
            // the server must have received an integer, not 7.0
            let sent = server.calls().last().expect("a call").1.clone();
            assert!(
                sent["id"].is_i64() || sent["id"].is_u64(),
                "{era:?} {label}: {sent}"
            );
            assert_eq!(
                rows.complete.expect("complete").upstream_calls,
                1,
                "{era:?} {label}"
            );
        }
        let list = collect(
            http.query(query("list", json!({})), scope(30))
                .await
                .expect("query"),
        )
        .await
        .expect("rows");
        assert_eq!(list.rows[&0].len(), 3);
    }
}

// spec: CMP-011, ADR-015, NGC-010, NGC-012
#[tokio::test(start_paused = true)]
async fn outbound_describe_is_stable_not_modified_and_honours_admin_classification_over_hints() {
    let server = FakeMcpServer::new(ServerEra::Modern);
    let mut dangerous = FakeTool::new(
        "wipe",
        json!({"type": "object", "properties": {"id": {"type": "string"}}}),
    );
    dangerous.annotations = json!({"readOnlyHint": true});
    let mut safe = FakeTool::new("peek", json!({"type": "object"}));
    safe.annotations = json!({"destructiveHint": true});
    server.set_tools(vec![
        dangerous,
        safe,
        FakeTool::new(
            "search",
            json!({"type": "object", "properties": {"q": {"type": "string"}}, "required": ["q"]}),
        ),
    ]);
    server.set_resources(vec![
        json!({"uri": "file:///a.txt", "name": "a", "mimeType": "text/plain"}),
    ]);
    let cfg = McpConfig {
        classification: OpClassification {
            mutations: ["wipe".to_owned()].into_iter().collect(),
        },
        ..McpConfig::default()
    };
    let connector = http_connector(&server, cfg);
    let first = connector
        .describe(DescribeRequest::default(), scope(30))
        .await
        .expect("describe");
    let second = connector
        .describe(DescribeRequest::default(), scope(30))
        .await
        .expect("describe");
    assert_eq!(first, second, "describe must be stable");
    let description = first.description.expect("description");
    let kind_of = |name: &str| {
        description
            .operations
            .iter()
            .find(|o| o.name == name)
            .map(|o| o.kind)
    };
    assert_eq!(
        kind_of("wipe"),
        Some(OpKind::Mutation as i32),
        "readOnlyHint must not relax the admin classification"
    );
    assert_eq!(
        kind_of("peek"),
        Some(OpKind::Query as i32),
        "destructiveHint must not tighten or loosen anything"
    );
    assert_eq!(kind_of("mcp_read_resource"), Some(OpKind::Query as i32));
    assert_eq!(
        description.meta.expect("meta").node_categories,
        ["McpResource"]
    );
    assert_eq!(description.limits.expect("limits").max_batch_size, 64);
    assert!(
        gap_ql::schema::with_gap_builtins(&description.schema_sdl).is_ok(),
        "{}",
        description.schema_sdl
    );
    let again = connector
        .describe(
            DescribeRequest {
                known_spec_hash: description.spec_hash.clone(),
            },
            scope(30),
        )
        .await
        .expect("not modified");
    assert!(again.not_modified && again.description.is_none());
    let stale = connector
        .describe(
            DescribeRequest {
                known_spec_hash: "stale".to_owned(),
            },
            scope(30),
        )
        .await
        .expect("full");
    assert!(!stale.not_modified);
    // the discovery happened once: one server/discover, one tools/list, one resources/list
    let methods: Vec<String> = server.requests().into_iter().map(|r| r.method).collect();
    assert_eq!(
        methods.iter().filter(|m| *m == "tools/list").count(),
        1,
        "{methods:?}"
    );
}

// spec: CMP-010
#[tokio::test(start_paused = true)]
async fn outbound_pagination_is_drained_and_hostile_servers_hit_the_caps() {
    let server = echo_server(ServerEra::Modern);
    server.set_tools(
        (0..7)
            .map(|i| FakeTool::new(&format!("t{i}"), json!({"type": "object"})))
            .collect(),
    );
    server.set_page_size(2);
    let connector = http_connector(&server, McpConfig::default());
    let described = connector
        .describe(DescribeRequest::default(), scope(30))
        .await
        .expect("describe")
        .description
        .expect("d");
    assert_eq!(described.operations.len(), 7);
    let pages = server
        .requests()
        .iter()
        .filter(|r| r.method == "tools/list")
        .count();
    assert_eq!(pages, 4, "7 tools in pages of 2");

    // page cap
    let capped = http_connector(
        &server,
        McpConfig {
            max_pages: 2,
            ..McpConfig::default()
        },
    );
    let err = capped
        .describe(DescribeRequest::default(), scope(30))
        .await
        .expect_err("page cap");
    assert_eq!(err.code, NgcErrorCode::UpstreamError);
    // item cap
    let items = http_connector(
        &server,
        McpConfig {
            max_items: 3,
            ..McpConfig::default()
        },
    );
    assert!(
        items
            .describe(DescribeRequest::default(), scope(30))
            .await
            .is_err()
    );
}

// spec: CMP-012, INV-15
#[tokio::test(start_paused = true)]
async fn outbound_tool_errors_and_input_required_results_are_upstream_errors_and_never_leak_secrets()
 {
    let canary = "sk-live-CANARY-777";
    let server = echo_server(ServerEra::Modern);
    server.set_call_handler(move |name, arguments| match (name, arguments.get("id").and_then(Value::as_i64)) {
        (_, Some(1)) => FakeCall::Reply(json!({"isError": true, "content": [{"type": "text", "text": format!("denied for token {canary}")}]})),
        (_, Some(2)) => FakeCall::Reply(json!({"resultType": "input_required", "requestState": "s", "inputRequests": {}})),
        (_, Some(3)) => FakeCall::Error(-32000, format!("server blew up with {canary}")),
        _ => FakeCall::Reply(json!({"structuredContent": {"ok": true}})),
    });
    let cfg = McpConfig {
        redact: vec![canary.to_owned()],
        ..McpConfig::default()
    };
    let connector = http_connector(&server, cfg.clone());
    for id in [1, 2, 3] {
        let err = match connector
            .query(query("echo", json!({"id": id})), scope(30))
            .await
        {
            Ok(stream) => collect(stream).await.expect_err("error expected"),
            Err(e) => e,
        };
        assert_eq!(err.code, NgcErrorCode::UpstreamError, "id {id}");
        assert!(
            !err.message().contains(canary),
            "secret leaked for id {id}: {}",
            err.message()
        );
    }
    assert!(!format!("{connector:?} {cfg:?}").contains(canary));
    let described = connector
        .describe(DescribeRequest::default(), scope(30))
        .await
        .expect("describe");
    assert!(!format!("{described:?}").contains(canary));
}

// spec: NGC-025, INV-12, CMP-012
#[tokio::test(start_paused = true)]
async fn outbound_deadline_already_past_never_sends_a_request_and_midcall_deadline_cancels_upstream()
 {
    let server = echo_server(ServerEra::Modern);
    server.set_call_handler(|_, _| FakeCall::Hang);
    let connector = http_connector(&server, McpConfig::default());
    let mut past = scope(30);
    past.deadline = tokio::time::Instant::now();
    let err = connector
        .query(query("echo", json!({"id": 1})), past)
        .await
        .err()
        .expect("past deadline");
    assert_eq!(err.code, NgcErrorCode::DeadlineExceeded);
    assert!(
        server.requests().is_empty(),
        "nothing may be sent for an expired call"
    );

    // mid-call deadline on the stateless HTTP era: the request is dropped (connection closes), no notifications/cancelled
    let stream = connector
        .query(query("echo", json!({"id": 1})), scope(2))
        .await
        .expect("query");
    let err = collect(stream).await.expect_err("deadline");
    assert_eq!(err.code, NgcErrorCode::DeadlineExceeded);
    assert_eq!(server.dropped_in_flight(), 1);
    assert!(
        server.cancelled_ids().is_empty(),
        "modern HTTP cancels by closing the stream"
    );
}

// spec: CMP-012, NGC-025
#[tokio::test(start_paused = true)]
async fn outbound_stdio_cancel_sends_notifications_cancelled_with_the_request_id() {
    let server = echo_server(ServerEra::Modern);
    server.set_call_handler(|_, _| FakeCall::Hang);
    let connector = stdio_connector(&server, McpConfig::default());
    // warm up the handshake so the cancelled id belongs to tools/call
    connector
        .describe(DescribeRequest::default(), scope(30))
        .await
        .expect("describe");
    let call_scope = scope(60);
    let cancel = call_scope.cancel.clone();
    let stream = connector
        .query(query("echo", json!({"id": 1})), call_scope)
        .await
        .expect("query");
    let task = tokio::spawn(collect(stream));
    tokio::time::sleep(Duration::from_millis(50)).await;
    cancel.cancel();
    let err = task.await.expect("join").expect_err("cancelled");
    assert_eq!(err.code, NgcErrorCode::Cancelled);
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        server.cancelled_ids().len(),
        1,
        "{:?}",
        server.cancelled_ids()
    );
}

// spec: NGC-071, INV-12
#[tokio::test(start_paused = true)]
async fn outbound_max_in_flight_saturation_is_an_immediate_retryable_error_and_capacity_comes_back()
{
    let server = echo_server(ServerEra::Modern);
    server.set_call_handler(|_, _| FakeCall::Hang);
    let connector = http_connector(
        &server,
        McpConfig {
            max_in_flight: 2,
            ..McpConfig::default()
        },
    );
    let a = connector
        .query(query("echo", json!({"id": 1})), scope(600))
        .await
        .expect("first");
    let b = connector
        .query(query("echo", json!({"id": 2})), scope(600))
        .await
        .expect("second");
    let err = connector
        .query(query("echo", json!({"id": 3})), scope(600))
        .await
        .err()
        .expect("saturated");
    assert_eq!(err.code, NgcErrorCode::ResourceExhausted);
    assert!(err.retryable && err.retry_after_ms == 100, "{err:?}");
    drop(a);
    let _fourth = connector
        .query(query("echo", json!({"id": 4})), scope(600))
        .await
        .expect("capacity returned after a stream was dropped");
    drop(b);
}

// spec: NGC-020, NGC-026, INV-18, CMP-015
#[tokio::test(start_paused = true)]
async fn outbound_foreach_is_emulated_with_honest_upstream_calls_dedup_isolation_and_bounded_concurrency()
 {
    let server = echo_server(ServerEra::Modern);
    server.set_call_handler(|_, arguments| {
        let id = arguments.get("id").and_then(Value::as_i64).unwrap_or(0);
        if id == 666 {
            return FakeCall::Reply(
                json!({"isError": true, "content": [{"type": "text", "text": "bad key"}]}),
            );
        }
        FakeCall::Delayed(
            Duration::from_millis(100),
            json!({"structuredContent": {"id": id}}),
        )
    });
    let connector = http_connector(
        &server,
        McpConfig {
            fanout_concurrency: 4,
            ..McpConfig::default()
        },
    );
    let sets: Vec<Value> = [1, 2, 3, 4, 5, 6, 7, 8, 2, 666]
        .iter()
        .map(|i| json!({"id": i}))
        .collect();
    let got = collect(
        connector
            .query(foreach_query("echo", &sets), scope(60))
            .await
            .expect("query"),
    )
    .await
    .expect("rows");
    let complete = got.complete.expect("complete");
    assert_eq!(
        complete.upstream_calls, 9,
        "9 distinct keys, the duplicate 2 is evaluated once"
    );
    assert_eq!(server.calls().len(), 9);
    assert_eq!(
        server.max_in_flight(),
        4,
        "fan-out must reach but never exceed the cap"
    );
    assert_eq!(got.closed.len(), 10, "exactly one closing chunk per index");
    assert_eq!(got.errors.len(), 1);
    assert_eq!(got.errors[&9].code, NgcErrorCode::UpstreamError);
    assert_eq!(got.rows[&0][0]["id"].as_f64(), Some(1.0));
    assert_eq!(
        got.rows[&8][0]["id"].as_f64(),
        Some(2.0),
        "the duplicate index gets the same rows"
    );
}

// spec: NGC-020, NGC-025
#[tokio::test(start_paused = true)]
async fn outbound_foreach_stops_issuing_calls_when_cancelled() {
    let server = echo_server(ServerEra::Modern);
    let started = Arc::new(AtomicUsize::new(0));
    let s2 = Arc::clone(&started);
    server.set_call_handler(move |_, _| {
        s2.fetch_add(1, Ordering::SeqCst);
        FakeCall::Delayed(
            Duration::from_millis(200),
            json!({"structuredContent": {"ok": true}}),
        )
    });
    let connector = http_connector(
        &server,
        McpConfig {
            fanout_concurrency: 2,
            ..McpConfig::default()
        },
    );
    let sets: Vec<Value> = (1..=20).map(|i| json!({"id": i})).collect();
    let call_scope = scope(600);
    let cancel = call_scope.cancel.clone();
    let task = tokio::spawn(collect(
        connector
            .query(foreach_query("echo", &sets), call_scope)
            .await
            .expect("query"),
    ));
    tokio::time::sleep(Duration::from_millis(50)).await;
    cancel.cancel();
    let _ = task.await.expect("join");
    tokio::time::sleep(Duration::from_secs(5)).await;
    assert!(
        started.load(Ordering::SeqCst) < 20,
        "calls must stop after cancel, saw {}",
        started.load(Ordering::SeqCst)
    );
}

// spec: NGC-027
#[tokio::test(start_paused = true)]
async fn outbound_oversized_answers_are_cut_and_flagged_truncated() {
    let server = echo_server(ServerEra::Modern);
    server.set_call_handler(|_, _| FakeCall::Reply(json!({"structuredContent": (0..100).map(|i| json!({"n": i, "pad": "x".repeat(50)})).collect::<Vec<_>>()})));
    let connector = http_connector(
        &server,
        McpConfig {
            max_response_bytes: 1_000,
            ..McpConfig::default()
        },
    );
    let got = collect(
        connector
            .query(query("list", json!({})), scope(30))
            .await
            .expect("query"),
    )
    .await
    .expect("rows");
    let complete = got.complete.expect("complete");
    assert!(complete.truncated && complete.next_cursor.is_empty());
    assert!(got.rows[&0].len() < 100 && !got.rows[&0].is_empty());
}

// spec: NGC-021, NGC-022, NGC-023, NGC-060
#[tokio::test(start_paused = true)]
async fn outbound_unsupported_query_features_are_invalid_argument_and_unknown_operations_not_found()
{
    let server = echo_server(ServerEra::Modern);
    server.set_tools(vec![
        FakeTool::new("echo", json!({"type": "object"})),
        FakeTool::new("wipe", json!({"type": "object"})),
    ]);
    let cfg = McpConfig {
        classification: OpClassification {
            mutations: ["wipe".to_owned()].into_iter().collect(),
        },
        ..McpConfig::default()
    };
    let connector = http_connector(&server, cfg);
    let base = query("echo", json!({}));
    let bad = [
        gap_proto::ngc::v1::QueryRequest {
            as_of: "2026-01-01T00:00:00Z".to_owned(),
            ..base.clone()
        },
        gap_proto::ngc::v1::QueryRequest {
            page: Some(Page {
                limit: 5,
                ..Page::default()
            }),
            ..base.clone()
        },
        gap_proto::ngc::v1::QueryRequest {
            order_by: vec![gap_proto::ngc::v1::OrderBy {
                path: "a".to_owned(),
                descending: false,
            }],
            ..base.clone()
        },
        query("wipe", json!({})),
    ];
    for req in bad {
        let err = connector
            .query(req, scope(30))
            .await
            .err()
            .expect("rejected");
        assert_eq!(err.code, NgcErrorCode::InvalidArgument);
    }
    let err = connector
        .query(query("nope", json!({})), scope(30))
        .await
        .err()
        .expect("unknown");
    assert_eq!(err.code, NgcErrorCode::NotFound);
    assert!(
        server.calls().is_empty(),
        "rejected requests never reach the upstream"
    );
}

// spec: CMP-011, CMP-012
#[tokio::test(start_paused = true)]
async fn outbound_resources_are_read_through_the_reserved_operation_with_the_uri_as_mcp_name() {
    let server = echo_server(ServerEra::Modern);
    server.set_resources(vec![
        json!({"uri": "file:///a.json", "name": "a", "mimeType": "application/json"}),
    ]);
    let connector = http_connector(&server, McpConfig::default());
    let got = collect(
        connector
            .query(
                query("mcp_read_resource", json!({"uri": "file:///a.json"})),
                scope(30),
            )
            .await
            .expect("query"),
    )
    .await
    .expect("rows");
    assert_eq!(got.rows[&0], [json!({"read": true})]);
    assert_eq!(server.resource_reads(), ["file:///a.json"]);
    let read = server
        .requests()
        .into_iter()
        .find(|r| r.method == "resources/read")
        .expect("read request");
    assert_eq!(
        read.headers.get("mcp-name").map(String::as_str),
        Some("file:///a.json")
    );
    let err = connector
        .query(query("mcp_read_resource", json!({})), scope(30))
        .await
        .err();
    let err = match err {
        Some(e) => e,
        None => collect(
            connector
                .query(query("mcp_read_resource", json!({})), scope(30))
                .await
                .expect("stream"),
        )
        .await
        .expect_err("uri required"),
    };
    assert_eq!(err.code, NgcErrorCode::InvalidArgument);
}

// spec: CMP-014
#[tokio::test(start_paused = true)]
async fn outbound_refresh_reports_whether_the_tool_list_changed() {
    let server = echo_server(ServerEra::Modern);
    let connector = http_connector(&server, McpConfig::default());
    connector
        .describe(DescribeRequest::default(), scope(30))
        .await
        .expect("describe");
    let token = tokio_util::sync::CancellationToken::new();
    assert!(
        !connector.refresh(&token).await.expect("refresh"),
        "nothing changed"
    );
    let mut tools = vec![
        FakeTool::new("echo", json!({"type": "object"})),
        FakeTool::new("list", json!({"type": "object"})),
    ];
    tools[0].description = "a rug-pulled description".to_owned();
    server.set_tools(tools);
    assert!(
        connector.refresh(&token).await.expect("refresh"),
        "a description change must be detected"
    );
}

// spec: NGC-070
#[tokio::test(start_paused = true)]
async fn outbound_health_is_serving_until_discovery_fails() {
    let server = echo_server(ServerEra::Modern);
    let connector = http_connector(&server, McpConfig::default());
    let health = connector
        .health(HealthRequest::default(), scope(5))
        .await
        .expect("health");
    assert_eq!(health.status, ServingStatus::Serving as i32);
    server.drop_result_type(true); // a broken envelope makes discovery fail
    assert!(
        connector
            .describe(DescribeRequest::default(), scope(30))
            .await
            .is_err()
    );
    let health = connector
        .health(HealthRequest::default(), scope(5))
        .await
        .expect("health");
    assert_eq!(health.status, ServingStatus::NotServing as i32);
}

// spec: CMP-010
#[tokio::test(start_paused = true)]
async fn outbound_a_server_repeating_a_pagination_cursor_is_cut_after_two_pages() {
    let server = echo_server(ServerEra::Modern);
    server.repeat_cursor(true);
    let connector = http_connector(&server, McpConfig::default());
    let err = connector
        .describe(DescribeRequest::default(), scope(30))
        .await
        .expect_err("endless cursor");
    assert_eq!(err.code, NgcErrorCode::UpstreamError);
    let pages = server
        .requests()
        .iter()
        .filter(|r| r.method == "tools/list")
        .count();
    assert_eq!(
        pages, 2,
        "the repeated cursor must be detected on the second page, not after the page cap"
    );
}

// spec: NGC-023, NGC-060
#[tokio::test(start_paused = true)]
async fn outbound_predicate_pushdown_is_invalid_argument() {
    let server = echo_server(ServerEra::Modern);
    let connector = http_connector(&server, McpConfig::default());
    let mut req = query("echo", json!({"id": 1}));
    req.predicate = Some(gap_proto::ngc::v1::Expr {
        node: Some(gap_proto::ngc::v1::expr::Node::Exists("id".to_owned())),
    });
    let err = connector
        .query(req, scope(30))
        .await
        .err()
        .expect("rejected");
    assert_eq!(err.code, NgcErrorCode::InvalidArgument);
    assert!(server.calls().is_empty());
}
