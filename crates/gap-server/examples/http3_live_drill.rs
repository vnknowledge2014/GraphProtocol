//! Release live dual-stack HTTP/3 (QUIC) mixed-load benchmark drill (`OPS-005`, `ADR-021`).
//!
//! Starts an assembled dual-stack `gap-server` (`TCP` HTTP/1.1+HTTP/2 + `UDP` HTTP/3 QUIC)
//! backed by the walking-skeleton supergraph, verifies `Alt-Svc` upgrade advertisement on TCP,
//! runs `800` mixed requests across `8` concurrent QUIC/HTTP/3 client workers (`60%` single-branch
//! `/graphql`, `25%` cross-branch join, `10%` `/mcp` progressive discovery, `5%` `/readyz` /
//! `/healthz`), and prints machine-readable JSON to `stdout`.

#![forbid(unsafe_code)]
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::todo,
    clippy::unimplemented
)]

use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::http::header::CONTENT_TYPE;
use axum::http::{HeaderMap, HeaderValue, Method, Request, StatusCode};
use bytes::Bytes;
use gap_core::config::{BranchProtocol, BranchSpec};
use gap_core::ids::BranchUid;
use gap_core::traits::BoxFuture;
use gap_mcp::outbound::fake::{FakeCall, FakeMcpServer, FakeTool, ServerEra};
use gap_mcp::outbound::transport::http::HttpTransport;
use gap_mcp::outbound::{McpConfig, McpConnector};
use gap_ngc_openapi::{
    AuthConfig, OpenApiConfig, OpenApiConnector, StaticCredentialSource, Upstream, UpstreamError,
    UpstreamRequest, UpstreamResponse,
};
use gap_server::assemble::{
    BranchBuildError, BranchFactory, BranchRuntime, ServerFile, assemble, wrap_connector,
};
use gap_server::{Shutdown, serve_dual_stack_with_shutdown};
use gap_transport::Http3Client;
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::net::TcpListener;
use tokio::sync::mpsc;

const DEV_KEY: &str = "gap_dev_skeleton_key_01";
const KEY_HASH: &str = "8a7d1134387be2f4aeb799122161c9868d94bdbec857230c917947a746ac8ba3";
const TOTAL_REQUESTS: usize = 800;
const CONCURRENCY: usize = 8;

#[derive(Debug, Clone, Default)]
struct DrillShopUpstream;

impl Upstream for DrillShopUpstream {
    async fn send(&self, request: UpstreamRequest) -> Result<UpstreamResponse, UpstreamError> {
        let pq = request.path_and_query.as_str();
        let (path, _) = pq.split_once('?').unwrap_or((pq, ""));
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        let body = match path {
            "/customers/1" => br#"{"id":1,"name":"Ada Lovelace","tier":"vip"}"#.to_vec(),
            "/orders" => {
                br#"[{"id":101,"customer_id":1,"total_cents":4200,"status":"paid"}]"#.to_vec()
            }
            _ => {
                return Ok(UpstreamResponse {
                    status: StatusCode::NOT_FOUND,
                    headers,
                    body: br#"{"error":"not_found"}"#.to_vec(),
                });
            }
        };
        Ok(UpstreamResponse {
            status: StatusCode::OK,
            headers,
            body,
        })
    }
}

fn build_drill_mcp_server() -> FakeMcpServer {
    let server = FakeMcpServer::new(ServerEra::Modern);
    let mut tool = FakeTool::new(
        "customer_notes",
        json!({
            "type": "object",
            "required": ["customer_id"],
            "additionalProperties": false,
            "properties": { "customer_id": { "type": "integer", "minimum": 1 } }
        }),
    );
    tool.description = "Fetch account notes for a shop customer id.".to_owned();
    server.set_tools(vec![tool]);
    server.set_call_handler(|name, args| {
        if name == "customer_notes" {
            let cid = args.get("customer_id").and_then(Value::as_i64).unwrap_or(1);
            FakeCall::Reply(json!({
                "content": [{"type": "text", "text": format!("notes for {cid}")}],
                "structuredContent": { "customer_id": cid, "note": "VIP note" },
                "isError": false
            }))
        } else {
            FakeCall::Error(-32602, "unknown tool".to_owned())
        }
    });
    server
}

#[derive(Debug, Default)]
struct DrillBranchFactory;

impl BranchFactory for DrillBranchFactory {
    fn build_branch<'a>(
        &'a self,
        server_file: &'a ServerFile,
        _branch_index: usize,
        branch: &'a BranchSpec,
        _uid: BranchUid,
    ) -> BoxFuture<'a, Result<BranchRuntime, BranchBuildError>> {
        Box::pin(async move {
            match branch.protocol {
                BranchProtocol::Openapi => {
                    let Some(doc_bytes) = server_file.openapi_document(&branch.id) else {
                        return Err(BranchBuildError::ConnectorInit {
                            branch_id: branch.id.clone(),
                            reason: "missing OpenAPI document bytes".to_owned(),
                        });
                    };
                    let Some(spec) = branch.openapi.as_ref() else {
                        return Err(BranchBuildError::ConnectorInit {
                            branch_id: branch.id.clone(),
                            reason: "missing OpenAPI spec".to_owned(),
                        });
                    };
                    let connector = OpenApiConnector::from_document_bytes(
                        doc_bytes,
                        OpenApiConfig {
                            connector_id: branch.id.to_string(),
                            base_url: spec.base_url.clone(),
                            allow_insecure_upstream: spec.allow_insecure_upstream,
                            auth: AuthConfig::None,
                            ..OpenApiConfig::default()
                        },
                        &StaticCredentialSource::new(),
                        DrillShopUpstream,
                    )
                    .map_err(|err| BranchBuildError::ConnectorInit {
                        branch_id: branch.id.clone(),
                        reason: err.to_string(),
                    })?;
                    wrap_connector(&branch.id, connector).await
                }
                BranchProtocol::Mcp => {
                    let fake_mcp = build_drill_mcp_server();
                    let transport = Arc::new(HttpTransport::new(fake_mcp.http_client(), 1 << 20));
                    let connector = McpConnector::new(
                        transport,
                        McpConfig {
                            connector_id: branch.id.to_string(),
                            ..McpConfig::default()
                        },
                    )
                    .map_err(|err| BranchBuildError::ConnectorInit {
                        branch_id: branch.id.clone(),
                        reason: err.to_string(),
                    })?;
                    let mut runtime = wrap_connector(&branch.id, connector).await?;
                    runtime.handle = Box::new((runtime.handle, fake_mcp));
                    Ok(runtime)
                }
            }
        })
    }
}

fn percentile(sorted: &[f64], pct: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let scaled = (pct / 100.0) * ((sorted.len() - 1) as f64);
    let idx = (scaled.round() as usize).min(sorted.len() - 1);
    (sorted[idx] * 1000.0).round() / 1000.0
}

fn build_mixed_request(idx: usize) -> Result<Request<Bytes>, String> {
    let slot = idx % 20;
    let bearer = format!("Bearer {DEV_KEY}");
    if slot < 12 {
        let body = Bytes::from_static(br#"{"query":"{ customer(id: 1) { id name tier } }"}"#);
        Request::builder()
            .method(Method::POST)
            .uri("https://localhost/graphql")
            .header("content-type", "application/json")
            .header("accept", "application/graphql-response+json")
            .header("authorization", bearer)
            .body(body)
            .map_err(|e| e.to_string())
    } else if slot < 17 {
        let body = Bytes::from_static(
            br#"{"query":"{ customer(id: 1) { id name tier } orders(customer_id: 1) { id total_cents status } customer_notes(customer_id: 1) }"}"#,
        );
        Request::builder()
            .method(Method::POST)
            .uri("https://localhost/graphql")
            .header("content-type", "application/json")
            .header("accept", "application/graphql-response+json")
            .header("authorization", bearer)
            .body(body)
            .map_err(|e| e.to_string())
    } else if slot < 19 {
        let body = Bytes::from(
            serde_json::to_vec(&json!({
                "jsonrpc": "2.0",
                "id": idx + 1,
                "method": "tools/call",
                "params": {
                    "_meta": {
                        "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                        "io.modelcontextprotocol/clientCapabilities": {}
                    },
                    "name": "gap_describe",
                    "arguments": { "overview": true }
                }
            }))
            .map_err(|e| e.to_string())?,
        );
        Request::builder()
            .method(Method::POST)
            .uri("https://localhost/mcp")
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream")
            .header("mcp-protocol-version", "2026-07-28")
            .header("mcp-method", "tools/call")
            .header("mcp-name", "gap_describe")
            .header("authorization", bearer)
            .body(body)
            .map_err(|e| e.to_string())
    } else {
        let uri = if idx.is_multiple_of(2) {
            "https://localhost/readyz"
        } else {
            "https://localhost/healthz"
        };
        Request::builder()
            .method(Method::GET)
            .uri(uri)
            .body(Bytes::new())
            .map_err(|e| e.to_string())
    }
}

async fn run() -> Result<(), String> {
    let tmp = TempDir::new().map_err(|e| e.to_string())?;
    let openapi_src = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples/walking-skeleton/shop.openapi.json");
    let openapi_dst = tmp.path().join("shop.openapi.json");
    fs::copy(&openapi_src, &openapi_dst).map_err(|e| e.to_string())?;

    let cfg_path = tmp.path().join("gap.toml");
    let toml_body = format!(
        r#"profile = "dev"

[server]
listen = "127.0.0.1:0"
http3_listen = "127.0.0.1:0"
shutdown_grace_ms = 2000

[gateway]
default_tenant = "default"
operation_mode = "open"

[[api_keys]]
id = "dev-key"
key_hash = "{KEY_HASH}"
tenant = "default"
scopes = ["read", "write"]

[[branches]]
id = "shop"
protocol = "openapi"
trust = "T1"
[branches.openapi]
spec = "shop.openapi.json"
base_url = "http://shop.local"
allow_insecure_upstream = true

[[branches]]
id = "notes"
protocol = "mcp"
trust = "T1"
[branches.mcp]
transport = "http"
url = "http://notes.local/mcp"
"#,
    );
    fs::write(&cfg_path, toml_body).map_err(|e| e.to_string())?;

    let sf = ServerFile::load(&cfg_path).map_err(|e| e.to_string())?;
    let assembled = assemble(&sf, &DrillBranchFactory)
        .await
        .map_err(|e| e.to_string())?;

    let tls = assembled
        .http3_tls()
        .cloned()
        .ok_or_else(|| "missing HTTP/3 TLS config".to_owned())?;
    let cert_der = assembled
        .http3_dev_cert_der()
        .ok_or_else(|| "missing HTTP/3 dev cert DER".to_owned())?
        .to_vec();

    let tcp_listener = TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|e| e.to_string())?;
    let tcp_addr = tcp_listener.local_addr().map_err(|e| e.to_string())?;
    let udp_socket = std::net::UdpSocket::bind("127.0.0.1:0").map_err(|e| e.to_string())?;
    let udp_addr = udp_socket.local_addr().map_err(|e| e.to_string())?;

    let draining = Arc::clone(&assembled.draining);
    let (sig_tx, sig_rx) = mpsc::channel(4);
    let shutdown = Shutdown::with_signals(draining, sig_rx, Arc::new(|_| {}));

    let srv_handle = tokio::spawn(async move {
        serve_dual_stack_with_shutdown(assembled, tcp_listener, Some((udp_socket, tls)), shutdown)
            .await
    });

    // Verify TCP `Alt-Svc` advertises the bound UDP HTTP/3 port.
    let tcp_client: Client<_, Body> = Client::builder(TokioExecutor::new()).build_http();
    let ready_req = Request::builder()
        .method(Method::GET)
        .uri(format!("http://{tcp_addr}/readyz"))
        .body(Body::empty())
        .map_err(|e| e.to_string())?;
    let tcp_resp = tcp_client
        .request(ready_req)
        .await
        .map_err(|e| e.to_string())?;
    let alt_svc_header = tcp_resp
        .headers()
        .get("alt-svc")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    let expected_alt_svc = format!("h3=\":{}\"; ma=86400", udp_addr.port());
    if tcp_resp.status() != StatusCode::OK || alt_svc_header != expected_alt_svc {
        return Err(format!(
            "unexpected TCP Alt-Svc response: status={}, alt-svc={alt_svc_header}",
            tcp_resp.status()
        ));
    }

    let next_idx = Arc::new(AtomicUsize::new(0));
    let mut workers = Vec::with_capacity(CONCURRENCY);
    for _ in 0..CONCURRENCY {
        let client = Http3Client::connect_pinned_der(udp_addr, "localhost", &cert_der)
            .await
            .map_err(|e| e.to_string())?;
        workers.push(client);
    }

    // Warmup one request per worker so handshake time is not mixed into steady-state RPS.
    for client in &mut workers {
        let req = build_mixed_request(1)?;
        let _ = client.request(req).await.map_err(|e| e.to_string())?;
    }

    let t_start = Instant::now();
    let mut handles = Vec::with_capacity(CONCURRENCY);
    for mut client in workers {
        let counter = Arc::clone(&next_idx);
        handles.push(tokio::spawn(async move {
            let mut lats_ms = Vec::new();
            let mut errs = 0_usize;
            loop {
                let idx = counter.fetch_add(1, Ordering::Relaxed);
                if idx >= TOTAL_REQUESTS {
                    break;
                }
                let Ok(req) = build_mixed_request(idx) else {
                    errs += 1;
                    continue;
                };
                let t0 = Instant::now();
                match tokio::time::timeout(Duration::from_secs(5), client.request(req)).await {
                    Ok(Ok((head, body))) if head.status() == StatusCode::OK => {
                        if idx % 20 < 17
                            && serde_json::from_slice::<Value>(&body)
                                .ok()
                                .and_then(|v| v.get("data").cloned())
                                .is_none()
                        {
                            errs += 1;
                        }
                        lats_ms.push(t0.elapsed().as_secs_f64() * 1000.0);
                    }
                    _ => errs += 1,
                }
            }
            (lats_ms, errs)
        }));
    }

    let mut all_lats = Vec::with_capacity(TOTAL_REQUESTS);
    let mut total_errors = 0_usize;
    for h in handles {
        let (lats, errs) = h.await.map_err(|e| e.to_string())?;
        all_lats.extend(lats);
        total_errors += errs;
    }
    let elapsed_s = t_start.elapsed().as_secs_f64();

    let _ = sig_tx.send(()).await;
    srv_handle
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())?;

    all_lats.sort_by(f64::total_cmp);
    let report = json!({
        "protocol": "HTTP/3 (QUIC RFC 9114)",
        "tls": "TLS 1.3 ALPN h3",
        "zero_rtt_disabled": true,
        "alt_svc_advertised": alt_svc_header,
        "mixed_load_requests": TOTAL_REQUESTS,
        "concurrency": CONCURRENCY,
        "errors": total_errors,
        "duration_s": (elapsed_s * 1000.0).round() / 1000.0,
        "measured_rps": ((TOTAL_REQUESTS as f64 / elapsed_s) * 10.0).round() / 10.0,
        "p50_ms": percentile(&all_lats, 50.0),
        "p95_ms": percentile(&all_lats, 95.0),
        "p99_ms": percentile(&all_lats, 99.0),
    });
    println!("{report}");
    if total_errors > 0 {
        return Err(format!("HTTP/3 drill recorded {total_errors} errors"));
    }
    Ok(())
}

#[tokio::main]
async fn main() {
    if let Err(err) = run().await {
        eprintln!("http3_live_drill failed: {err}");
        std::process::exit(1);
    }
}
