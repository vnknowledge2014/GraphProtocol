//! Black-box NGC connector conformance kit run against `gap-mcp`'s outbound connector (`WP-M1-09`, `NGC-091`, `CMP-010..015`).

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use futures_util::stream;
use gap_conformance::ngc_kit::{CHECKS, Outcome, Profile, SkipReason, id, run_kit};
use gap_mcp::outbound::fake::{FakeCall, FakeMcpServer, FakeTool, ServerEra};
use gap_mcp::outbound::{HttpTransport, McpConfig, McpConnector, ReqwestPost};
use gap_ngc_sdk::core::{ServerConfig, ServerCore};
use gap_ngc_sdk::grpc::serve_incoming_with_shutdown;
use gap_ngc_sdk::testing::in_memory_grpc;
use serde_json::{Value, json};

const MAX_IN_FLIGHT: u32 = 8;
const MAX_RESPONSE_BYTES: u64 = 8_192;

fn profile() -> Profile {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../conformance/ngc/mcp.kit.toml");
    Profile::from_path(&path).expect("parse conformance/ngc/mcp.kit.toml")
}

/// The fake MCP server the profile describes: a keyed read, a tool that never answers and a tool with a big answer.
fn kit_server(era: ServerEra) -> FakeMcpServer {
    let server = FakeMcpServer::new(era);
    server.set_tools(vec![
        FakeTool::new("customer", json!({"type": "object", "properties": {"id": {"type": "integer"}}, "required": ["id"], "additionalProperties": false})),
        FakeTool::new("slow", json!({"type": "object"})),
        FakeTool::new("blobs", json!({"type": "object"})),
    ]);
    server.set_call_handler(|name, arguments| match name {
        "customer" => match arguments.get("id").and_then(Value::as_i64) {
            Some(id) if (1..=12).contains(&id) => FakeCall::Reply(json!({"structuredContent": {"id": id, "name": format!("customer-{id}")}})),
            _ => FakeCall::Reply(json!({"structuredContent": []})),
        },
        "slow" => FakeCall::Hang,
        _ => FakeCall::Reply(json!({"structuredContent": (0..40).map(|n| json!({"n": n, "pad": "x".repeat(900)})).collect::<Vec<_>>()})),
    });
    server
}

fn config() -> McpConfig {
    McpConfig {
        connector_id: "gap-mcp-kit".to_owned(),
        max_in_flight: MAX_IN_FLIGHT,
        max_response_bytes: MAX_RESPONSE_BYTES,
        ..McpConfig::default()
    }
}

fn assert_compatible(report: &gap_conformance::ngc_kit::KitReport) {
    assert!(
        report.compatible(),
        "gap-mcp failed the NGC kit:\n{}",
        report.render_text()
    );
    assert!(
        report.uncovered().is_empty(),
        "uncovered checks:\n{}",
        report.render_text()
    );
    assert_eq!(report.results.len(), CHECKS.len());
    for res in &report.results {
        // pagination, ordering and idempotency are not advertised by an MCP connector: not applicable, everything else passes
        let not_applicable =
            [id::MUTATE_IDEMPOTENCY, id::PAGE_STABILITY, id::QUERY_ORDER].contains(&res.info.id);
        if not_applicable {
            assert!(
                matches!(res.outcome, Outcome::Skip(SkipReason::NotApplicable(_))),
                "{}: {:?}",
                res.info.id,
                res.outcome
            );
        } else {
            assert_eq!(
                res.outcome,
                Outcome::Pass,
                "{}: {:?} (notes: {:?})",
                res.info.id,
                res.outcome,
                res.notes
            );
        }
    }
}

// spec: NGC-091, CMP-010, CMP-011, CMP-012, CMP-015, NGC-010, NGC-012, NGC-020, NGC-025, NGC-026, NGC-027, NGC-060, NGC-061, NGC-070, INV-12, INV-15, INV-18
#[tokio::test(start_paused = true)]
async fn ngc_kit_mcp_connector_is_compatible_over_both_eras_and_both_transports() {
    for era in [ServerEra::Modern, ServerEra::Legacy] {
        let server = kit_server(era);
        let http =
            McpConnector::new(Arc::new(server.http_transport()), config()).expect("connector");
        let (client, handle) = in_memory_grpc(Arc::new(ServerCore::new(
            http,
            ServerConfig::default().with_max_in_flight(MAX_IN_FLIGHT as usize),
        )));
        assert_compatible(&run_kit(&client, &profile()).await);
        handle.shutdown().await.expect("clean shutdown");

        let server = kit_server(era);
        let stdio =
            McpConnector::new(Arc::new(server.stdio_transport()), config()).expect("connector");
        let (client, handle) = in_memory_grpc(Arc::new(ServerCore::new(
            stdio,
            ServerConfig::default().with_max_in_flight(MAX_IN_FLIGHT as usize),
        )));
        assert_compatible(&run_kit(&client, &profile()).await);
        handle.shutdown().await.expect("clean shutdown");
    }
}

// spec: NGC-091, CMP-010
#[tokio::test]
#[ignore = "needs loopback sockets"]
async fn ngc_kit_mcp_http_over_loopback_sockets() {
    use axum::body::Body;
    use axum::http::{Request, Response, StatusCode};
    use futures_util::StreamExt;
    use gap_mcp::outbound::transport::http::{HttpPost, HttpRequest};

    let server = kit_server(ServerEra::Modern);
    let backend = Arc::new(server.http_client());
    let app = axum::Router::new().route(
        "/mcp",
        axum::routing::post(move |req: Request<Body>| {
            let backend = Arc::clone(&backend);
            async move {
                let headers = req
                    .headers()
                    .iter()
                    .filter_map(|(k, v)| {
                        v.to_str()
                            .ok()
                            .map(|v| (k.as_str().to_owned(), v.to_owned()))
                    })
                    .collect();
                let body = axum::body::to_bytes(req.into_body(), 8 << 20)
                    .await
                    .unwrap_or_default()
                    .to_vec();
                match backend
                    .post(HttpRequest {
                        headers,
                        body,
                        timeout: Duration::from_secs(30),
                    })
                    .await
                {
                    Ok(mut reply) => {
                        let mut bytes = Vec::new();
                        while let Some(Ok(chunk)) = reply.body.next().await {
                            bytes.extend_from_slice(&chunk);
                        }
                        let mut builder = Response::builder().status(
                            StatusCode::from_u16(reply.status).unwrap_or(StatusCode::BAD_GATEWAY),
                        );
                        for (k, v) in reply.headers {
                            builder = builder.header(k, v);
                        }
                        builder.body(Body::from(bytes)).unwrap_or_default()
                    }
                    Err(_) => Response::builder()
                        .status(StatusCode::BAD_GATEWAY)
                        .body(Body::empty())
                        .unwrap_or_default(),
                }
            }
        }),
    );
    let rest = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind REST");
    let rest_addr = rest.local_addr().expect("addr");
    let rest_task = tokio::spawn(async move {
        let _ = axum::serve(rest, app).await;
    });

    let post =
        ReqwestPost::new(&format!("http://{rest_addr}/mcp"), None, false).expect("reqwest client");
    let connector = McpConnector::new(Arc::new(HttpTransport::new(post, 4 << 20)), config())
        .expect("connector");
    let grpc = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind gRPC");
    let grpc_addr = grpc.local_addr().expect("addr");
    let incoming = stream::unfold(grpc, |listener| async move {
        let res = listener.accept().await.map(|(s, _)| s);
        Some((res, listener))
    });
    let core = Arc::new(ServerCore::new(
        connector,
        ServerConfig::default().with_max_in_flight(MAX_IN_FLIGHT as usize),
    ));
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let grpc_task = tokio::spawn(async move {
        serve_incoming_with_shutdown(core, Box::pin(incoming), async move {
            let _ = stop_rx.await;
        })
        .await
    });
    let client = gap_conformance::ngc_kit::connect(&grpc_addr.to_string()).expect("connect");
    let report = run_kit(&client, &profile()).await;
    assert_compatible(&report);
    let _ = stop_tx.send(());
    let _ = grpc_task.await;
    rest_task.abort();
}
