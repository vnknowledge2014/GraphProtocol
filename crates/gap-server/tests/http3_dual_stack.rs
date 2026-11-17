//! Full dual-stack (`HTTP/1.1+HTTP/2` TCP + `HTTP/3 QUIC` UDP) `gap-server` integration tests
//! (`OPS-005`, `OPS-002`, `RUN-090`, `INV-15`).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;

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
    AssembleError, BranchBuildError, BranchFactory, BranchRuntime, ServerFile, assemble,
    assemble_default, wrap_connector,
};
use gap_server::{Shutdown, serve_dual_stack_with_shutdown};
use gap_transport::{Http3Client, Http3TlsConfig};
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::net::TcpListener;
use tokio::sync::mpsc;

const DEV_KEY: &str = "gap_dev_skeleton_key_01";
const KEY_HASH: &str = "8a7d1134387be2f4aeb799122161c9868d94bdbec857230c917947a746ac8ba3";

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("canonical repo root")
}

#[derive(Debug, Clone, Default)]
struct FakeShopUpstream;

impl Upstream for FakeShopUpstream {
    async fn send(&self, request: UpstreamRequest) -> Result<UpstreamResponse, UpstreamError> {
        let pq = request.path_and_query.as_str();
        let (path, _) = pq.split_once('?').unwrap_or((pq, ""));
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        let body_val = match path {
            "/customers/1" => json!({"id": 1, "name": "Ada Lovelace", "tier": "vip"}),
            "/orders" => json!([
                {"id": 101, "customer_id": 1, "total_cents": 4200, "status": "paid"}
            ]),
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
            body: serde_json::to_vec(&body_val).unwrap(),
        })
    }
}

fn build_fake_mcp_server() -> FakeMcpServer {
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
struct FakeBranchFactory;

impl BranchFactory for FakeBranchFactory {
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
                    let doc_bytes = server_file.openapi_document(&branch.id).unwrap();
                    let spec = branch.openapi.as_ref().unwrap();
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
                        FakeShopUpstream,
                    )
                    .unwrap();
                    wrap_connector(&branch.id, connector).await
                }
                BranchProtocol::Mcp => {
                    let fake_mcp = build_fake_mcp_server();
                    let transport = Arc::new(HttpTransport::new(fake_mcp.http_client(), 1 << 20));
                    let connector = McpConnector::new(
                        transport,
                        McpConfig {
                            connector_id: branch.id.to_string(),
                            ..McpConfig::default()
                        },
                    )
                    .unwrap();
                    let mut runtime = wrap_connector(&branch.id, connector).await?;
                    runtime.handle = Box::new((runtime.handle, fake_mcp));
                    Ok(runtime)
                }
            }
        })
    }
}

fn write_skeleton_temp_toml(dir: &Path, extra_server_lines: &str) -> PathBuf {
    let skeleton = repo_root().join("examples/walking-skeleton");
    let openapi_src = skeleton.join("shop.openapi.json");
    let openapi_dst = dir.join("shop.openapi.json");
    fs::copy(&openapi_src, &openapi_dst).expect("copy OpenAPI");

    let cfg_path = dir.join("gap.toml");
    let body = format!(
        r#"profile = "dev"

[server]
listen = "127.0.0.1:0"
http3_listen = "127.0.0.1:0"
shutdown_grace_ms = 2000
{extra_server_lines}

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
    fs::write(&cfg_path, body).expect("write gap.toml");
    cfg_path
}

// spec: OPS-005, OPS-002, RUN-090
#[tokio::test]
async fn gap_server_dual_stack_serves_alt_svc_graphql_mcp_and_drains_cleanly() {
    let tmp = TempDir::new().expect("tempdir");
    let cfg_path = write_skeleton_temp_toml(tmp.path(), "");
    let sf = ServerFile::load(&cfg_path).expect("load ServerFile");
    let assembled = assemble(&sf, &FakeBranchFactory)
        .await
        .expect("assemble dual-stack gateway");

    let tls = assembled.http3_tls().expect("HTTP/3 TLS config").clone();
    let cert_der = assembled
        .http3_dev_cert_der()
        .expect("dev self-signed DER")
        .to_vec();

    let tcp_listener = TcpListener::bind("127.0.0.1:0").await.expect("bind TCP");
    let tcp_addr = tcp_listener.local_addr().expect("TCP addr");
    let udp_socket = std::net::UdpSocket::bind("127.0.0.1:0").expect("bind UDP");
    let udp_addr = udp_socket.local_addr().expect("UDP addr");

    let draining = Arc::clone(&assembled.draining);
    let (sig_tx, sig_rx) = mpsc::channel(4);
    let shutdown = Shutdown::with_signals(Arc::clone(&draining), sig_rx, Arc::new(|_| {}));

    let srv_handle = tokio::spawn(async move {
        serve_dual_stack_with_shutdown(assembled, tcp_listener, Some((udp_socket, tls)), shutdown)
            .await
    });

    // 1. Probe TCP `/readyz` for `Alt-Svc: h3=":<udp_port>"; ma=86400`.
    let http_client: Client<_, Body> = Client::builder(TokioExecutor::new()).build_http();
    let tcp_resp = http_client
        .request(
            Request::builder()
                .method(Method::GET)
                .uri(format!("http://{tcp_addr}/readyz"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("TCP /readyz");
    assert_eq!(tcp_resp.status(), StatusCode::OK);
    let alt_svc = tcp_resp
        .headers()
        .get("alt-svc")
        .expect("Alt-Svc emitted on TCP response")
        .to_str()
        .unwrap();
    assert_eq!(alt_svc, format!("h3=\":{}\"; ma=86400", udp_addr.port()));

    // 2. Connect over QUIC/HTTP/3 using the discovered port.
    let mut h3_client = Http3Client::connect_pinned_der(udp_addr, "localhost", &cert_der)
        .await
        .expect("connect HTTP/3");

    // Cross-branch GraphQL join (shop REST + notes MCP) over HTTP/3.
    let gql_body = serde_json::to_vec(&json!({
        "query": "{ customer(id: 1) { id name tier } orders(customer_id: 1) { id status } customer_notes(customer_id: 1) }"
    }))
    .unwrap();
    let (gql_head, gql_bytes) = h3_client
        .request(
            Request::builder()
                .method(Method::POST)
                .uri("https://localhost/graphql")
                .header("content-type", "application/json")
                .header("accept", "application/graphql-response+json")
                .header("authorization", format!("Bearer {DEV_KEY}"))
                .body(Bytes::from(gql_body))
                .unwrap(),
        )
        .await
        .expect("HTTP/3 cross-branch /graphql");
    assert_eq!(gql_head.status(), StatusCode::OK);
    let gql_json: Value = serde_json::from_slice(&gql_bytes).expect("JSON");
    assert_eq!(gql_json["data"]["customer"]["name"], "Ada Lovelace");
    assert_eq!(gql_json["data"]["orders"][0]["status"], "paid");
    assert!(
        gql_json["data"]["customer_notes"]
            .to_string()
            .contains("VIP note")
    );

    // MCP `tools/list` (`5` static tools) over HTTP/3.
    let mcp_body = serde_json::to_vec(&json!({
        "jsonrpc": "2.0",
        "id": 42,
        "method": "tools/list",
        "params": {
            "_meta": {
                "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                "io.modelcontextprotocol/clientCapabilities": {}
            }
        }
    }))
    .unwrap();
    let (mcp_head, mcp_bytes) = h3_client
        .request(
            Request::builder()
                .method(Method::POST)
                .uri("https://localhost/mcp")
                .header("content-type", "application/json")
                .header("accept", "application/json, text/event-stream")
                .header("mcp-protocol-version", "2026-07-28")
                .header("mcp-method", "tools/list")
                .header("authorization", format!("Bearer {DEV_KEY}"))
                .body(Bytes::from(mcp_body))
                .unwrap(),
        )
        .await
        .expect("HTTP/3 /mcp tools/list");
    assert_eq!(mcp_head.status(), StatusCode::OK);
    let mcp_json: Value = serde_json::from_slice(&mcp_bytes).expect("MCP JSON");
    assert_eq!(mcp_json["result"]["tools"].as_array().unwrap().len(), 5);

    // AG-UI v1.0 `/ag-ui/capabilities` & SSE `POST /ag-ui` over HTTP/3 QUIC (`CMP-025`, `OPS-005`).
    let (agui_cap_head, agui_cap_bytes) = h3_client
        .request(
            Request::builder()
                .method(Method::GET)
                .uri("https://localhost/ag-ui/capabilities")
                .header("authorization", format!("Bearer {DEV_KEY}"))
                .body(Bytes::new())
                .unwrap(),
        )
        .await
        .expect("HTTP/3 GET /ag-ui/capabilities");
    assert_eq!(agui_cap_head.status(), StatusCode::OK);
    let agui_cap_json: Value = serde_json::from_slice(&agui_cap_bytes).expect("cap JSON");
    assert_eq!(agui_cap_json["protocol"], "ag-ui");
    assert_eq!(agui_cap_json["version"], "1.0");
    assert_eq!(agui_cap_json["interrupts"], true);

    let agui_run_body = serde_json::to_vec(&json!({
        "threadId": "th-h3-1",
        "runId": "run-h3-1",
        "messages": [{"id": "m1", "role": "user", "content": "fetch customer 1 over QUIC"}],
        "forwardedProps": {
            "gapql": "{ customer(id: 1) { id name } }"
        }
    }))
    .unwrap();
    let (agui_run_head, agui_run_bytes) = h3_client
        .request(
            Request::builder()
                .method(Method::POST)
                .uri("https://localhost/ag-ui")
                .header("content-type", "application/json")
                .header("accept", "text/event-stream")
                .header("authorization", format!("Bearer {DEV_KEY}"))
                .body(Bytes::from(agui_run_body))
                .unwrap(),
        )
        .await
        .expect("HTTP/3 POST /ag-ui");
    assert_eq!(agui_run_head.status(), StatusCode::OK);
    let agui_sse = std::str::from_utf8(&agui_run_bytes).expect("UTF-8 SSE");
    assert!(agui_sse.contains("RUN_STARTED"));
    assert!(agui_sse.contains("Ada Lovelace"));
    assert!(agui_sse.contains("RUN_FINISHED"));

    drop(h3_client);

    // 3. Trigger shutdown and verify draining flag flips cleanly.
    sig_tx.send(()).await.expect("send shutdown signal");
    srv_handle
        .await
        .unwrap()
        .expect("dual-stack server clean exit");
    assert!(draining.load(Ordering::Acquire));
}

// spec: OPS-005, INV-15
#[tokio::test]
async fn prod_http3_loads_explicit_pem_secrets_and_redacts_corrupt_key_errors() {
    let tmp = TempDir::new().expect("tempdir");
    let (cert_pem, key_pem) = Http3TlsConfig::self_signed_pem_for_dev(&["gap.internal".to_owned()])
        .expect("generate self-signed test PEM pair");
    let cert_path = tmp.path().join("tls.crt");
    let key_path = tmp.path().join("tls.key");
    fs::write(&cert_path, &cert_pem).expect("write cert");
    fs::write(&key_path, &key_pem).expect("write key");

    let cfg_path = tmp.path().join("prod.toml");
    fs::write(
        &cfg_path,
        format!(
            r#"profile = "prod"

[server]
listen = "127.0.0.1:0"
http3_listen = "127.0.0.1:0"
tls_cert_secret = "file://{}"
tls_key_secret = "file://{}"

[[api_keys]]
id = "prod-key"
key_hash = "{KEY_HASH}"
tenant = "default"
scopes = ["read"]
"#,
            cert_path.display(),
            key_path.display(),
        ),
    )
    .expect("write prod.toml");

    let sf = ServerFile::load(&cfg_path).expect("prod config with PEM paths loads");
    let assembled = assemble_default(&sf)
        .await
        .expect("assemble prod HTTP/3 TLS");
    assert!(assembled.http3_tls().is_some());
    assert!(assembled.http3_dev_cert_der().is_none());

    // Replace key with a corrupt canary value and ensure `INV-15` redacts secret bytes in errors.
    let canary = "SECRET_CANARY_KEY_MATERIAL_DO_NOT_LEAK";
    fs::write(&key_path, canary).expect("overwrite corrupt key");
    let _ = Http3TlsConfig::from_pem(cert_pem.as_bytes(), canary.as_bytes())
        .expect_err("corrupt PEM must fail");
    let err = assemble_default(&sf)
        .await
        .expect_err("assemble with corrupt PEM must fail");
    assert!(
        matches!(err, AssembleError::Http3Tls(_)),
        "expected Http3Tls error, got {err:?}"
    );
    assert!(
        !err.to_string().contains(canary),
        "error message must never echo secret key bytes (INV-15)"
    );
}
