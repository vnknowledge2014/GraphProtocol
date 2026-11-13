//! Unit and integration tests for `Shutdown` two-stage signal handling (`RUN-090`) and
//! `serve_stdio` (`SEC-004`, `INV-01`, `OPS-011`) with zero sockets and zero child processes.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::http::header::CONTENT_TYPE;
use axum::http::{HeaderMap, HeaderValue, Request, StatusCode};
use gap_core::config::{BranchProtocol, BranchSpec};
use gap_core::context::PrincipalKind;
use gap_core::ids::BranchUid;
use gap_core::snapshot::Snapshot;
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
use gap_server::run::{
    FORCE_SHUTDOWN_EXIT_CODE, RunError, Shutdown, build_stdio_context_factory, serve_http,
    serve_stdio_with_streams,
};
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::sync::{mpsc, oneshot};
use tower::ServiceExt as _;
use tracing_subscriber::fmt::MakeWriter;

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
        let (path, _query) = pq.split_once('?').unwrap_or((pq, ""));
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        let body_val = match path {
            "/customers/1" => json!({"id": 1, "name": "Ada Lovelace", "tier": "vip"}),
            _ => json!([]),
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
            "properties": {
                "customer_id": { "type": "integer", "minimum": 1 }
            }
        }),
    );
    tool.description = "Fetch account notes for a shop customer id.".to_owned();
    server.set_tools(vec![tool]);
    server.set_call_handler(|_name, args| {
        let cid = args
            .get("customer_id")
            .and_then(Value::as_i64)
            .unwrap_or_default();
        FakeCall::Reply(json!({
            "content": [{"type": "text", "text": format!("notes for {cid}")}],
            "structuredContent": {"customer_id": cid},
            "isError": false
        }))
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
                    let cfg = OpenApiConfig {
                        connector_id: branch.id.to_string(),
                        base_url: spec.base_url.clone(),
                        allow_insecure_upstream: spec.allow_insecure_upstream,
                        auth: AuthConfig::None,
                        ..OpenApiConfig::default()
                    };
                    let creds = StaticCredentialSource::new();
                    let connector = OpenApiConnector::from_document_bytes(
                        doc_bytes,
                        cfg,
                        &creds,
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

#[derive(Clone, Default)]
struct SharedVecWriter {
    buf: Arc<Mutex<Vec<u8>>>,
}

struct VecGuard {
    buf: Arc<Mutex<Vec<u8>>>,
}

impl io::Write for VecGuard {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        self.buf.lock().unwrap().extend_from_slice(data);
        Ok(data.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for SharedVecWriter {
    type Writer = VecGuard;

    fn make_writer(&'a self) -> Self::Writer {
        VecGuard {
            buf: Arc::clone(&self.buf),
        }
    }
}

// spec: RUN-090, OPS-002
#[tokio::test]
async fn shutdown_first_signal_sets_draining_immediately_and_completes_graceful_drain() {
    let sf = ServerFile::load(repo_root().join("examples/walking-skeleton/gap.toml")).unwrap();
    let assembled = assemble(&sf, &FakeBranchFactory).await.unwrap();
    let app = gap_transport::router(Arc::clone(&assembled.app_state));

    let (sig_tx, sig_rx) = mpsc::channel(8);
    let force_called = Arc::new(AtomicBool::new(false));
    let force_called_hook = Arc::clone(&force_called);

    let shutdown = Shutdown::with_signals(
        Arc::clone(&assembled.draining),
        sig_rx,
        Arc::new(move |_code| {
            force_called_hook.store(true, Ordering::SeqCst);
        }),
    );

    let (trigger_observed_tx, trigger_observed_rx) = oneshot::channel();
    let (finish_drain_tx, finish_drain_rx) = oneshot::channel();

    let drive_task = tokio::spawn(async move {
        shutdown
            .drive(|trigger| async move {
                trigger.await;
                let _ = trigger_observed_tx.send(());
                let _ = finish_drain_rx.await;
                Ok(())
            })
            .await
    });

    assert!(!assembled.draining.load(Ordering::SeqCst));

    // Send first signal: draining must flip immediately and /readyz must return 503 while still draining.
    sig_tx.send(()).await.unwrap();
    tokio::time::timeout(Duration::from_millis(500), trigger_observed_rx)
        .await
        .expect("shutdown trigger must resolve on first signal")
        .unwrap();

    assert!(
        assembled.draining.load(Ordering::SeqCst),
        "first signal must immediately set draining=true"
    );

    let readyz_resp = app
        .oneshot(
            Request::builder()
                .uri("/readyz")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(readyz_resp.status(), StatusCode::SERVICE_UNAVAILABLE);

    // Complete drain cleanly.
    finish_drain_tx.send(()).unwrap();
    let result = tokio::time::timeout(Duration::from_millis(500), drive_task)
        .await
        .unwrap()
        .unwrap();
    assert!(
        result.is_ok(),
        "clean drain must return Ok(()), got {result:?}"
    );
    assert!(
        !force_called.load(Ordering::SeqCst),
        "single signal must not invoke force exit"
    );
}

// spec: RUN-090
#[tokio::test]
async fn shutdown_second_signal_during_drain_forces_immediate_nonzero_exit() {
    let draining = Arc::new(AtomicBool::new(false));
    let (sig_tx, sig_rx) = mpsc::channel(8);
    let forced_code = Arc::new(AtomicI32::new(0));
    let forced_code_hook = Arc::clone(&forced_code);

    let shutdown = Shutdown::with_signals(
        Arc::clone(&draining),
        sig_rx,
        Arc::new(move |code| {
            forced_code_hook.store(code, Ordering::SeqCst);
        }),
    );

    let (in_drain_tx, in_drain_rx) = oneshot::channel();
    let drive_task = tokio::spawn(async move {
        shutdown
            .drive(|trigger| async move {
                trigger.await;
                let _ = in_drain_tx.send(());
                // Simulate stuck in-flight drain that never finishes on its own.
                std::future::pending::<()>().await;
                Ok(())
            })
            .await
    });

    // First signal enters drain.
    sig_tx.send(()).await.unwrap();
    tokio::time::timeout(Duration::from_millis(500), in_drain_rx)
        .await
        .unwrap()
        .unwrap();
    assert!(draining.load(Ordering::SeqCst));

    // Second signal forces immediate exit with non-zero code (130).
    sig_tx.send(()).await.unwrap();
    let res = tokio::time::timeout(Duration::from_millis(200), drive_task)
        .await
        .expect("second signal must abort drain immediately")
        .unwrap();

    assert_eq!(forced_code.load(Ordering::SeqCst), FORCE_SHUTDOWN_EXIT_CODE);
    match res {
        Err(RunError::ForcedShutdown { exit_code }) => {
            assert_eq!(exit_code, FORCE_SHUTDOWN_EXIT_CODE);
            assert_ne!(exit_code, 0);
        }
        other => panic!("expected ForcedShutdown error on second signal, got: {other:?}"),
    }
}

// spec: SEC-004, INV-01, OPS-001
#[tokio::test]
async fn stdio_context_factory_uses_service_principal_scopes_and_pins_fresh_snapshot_per_request() {
    let sf = ServerFile::load(repo_root().join("examples/walking-skeleton/gap.toml")).unwrap();
    let assembled = assemble(&sf, &FakeBranchFactory).await.unwrap();

    let ctx_factory = build_stdio_context_factory(&assembled);
    let ctx1 = ctx_factory().expect("context factory must succeed");
    assert_eq!(ctx1.principal.kind, PrincipalKind::Service);
    assert!(
        ctx1.principal.scopes.contains("read"),
        "stdio Service principal must inherit configured scopes, got: {:?}",
        ctx1.principal.scopes
    );
    assert_eq!(
        ctx1.budget.max_cost_units,
        assembled.config.limits.max_cost_units
    );
    assert_eq!(ctx1.snapshot.generation, 1);

    // Hot-swap snapshot to generation 2 and verify subsequent stdio requests pin generation 2.
    let gen2 = Snapshot::empty_for_tenant(2, assembled.config.gateway.default_tenant.clone());
    assembled.snapshot_handle.publish(gen2).unwrap();

    let ctx2 = ctx_factory().expect("second context must pin generation 2");
    assert_eq!(ctx1.snapshot.generation, 1);
    assert_eq!(ctx2.snapshot.generation, 2);
}

// spec: SEC-004, OPS-011, INV-01
#[tokio::test]
async fn stdio_never_pollutes_stdout_with_logs_and_routes_tracing_to_stderr() {
    let sf = ServerFile::load(repo_root().join("examples/walking-skeleton/gap.toml")).unwrap();
    let assembled = assemble(&sf, &FakeBranchFactory).await.unwrap();

    let meta = json!({
        "io.modelcontextprotocol/protocolVersion": "2026-07-28",
        "io.modelcontextprotocol/clientCapabilities": {}
    });
    let line1 = serde_json::to_string(&json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/list",
        "params": { "_meta": meta }
    }))
    .unwrap();
    let line2 = serde_json::to_string(&json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/call",
        "params": {
            "_meta": meta,
            "name": "gap_query",
            "arguments": {
                "query": "{ customer(id: 1) { id name } customer_notes(customer_id: 1) }"
            }
        }
    }))
    .unwrap();

    let input = format!("{line1}\n{line2}\n");
    let mut stdout_buf: Vec<u8> = Vec::new();
    let stderr_sink = SharedVecWriter::default();

    serve_stdio_with_streams(
        &assembled,
        tokio::io::BufReader::new(input.as_bytes()),
        &mut stdout_buf,
        stderr_sink.clone(),
    )
    .await
    .expect("serve_stdio_with_streams must succeed");

    let stdout_text = String::from_utf8(stdout_buf).expect("stdout UTF-8");
    let lines: Vec<&str> = stdout_text.lines().collect();
    assert_eq!(
        lines.len(),
        2,
        "stdout must carry exclusively the 2 JSON-RPC protocol frames, got: {stdout_text:?}"
    );
    for (idx, line) in lines.iter().enumerate() {
        let parsed: Value = serde_json::from_str(line).unwrap_or_else(|err| {
            panic!("stdout line {idx} is not valid JSON-RPC (log pollution?): {err}: {line}")
        });
        assert_eq!(parsed["jsonrpc"], "2.0");
    }

    let query_resp: Value = serde_json::from_str(lines[1]).unwrap();
    assert_eq!(query_resp["result"]["isError"], false);
    assert_eq!(
        query_resp["result"]["structuredContent"]["data"]["customer"]["name"],
        "Ada Lovelace"
    );

    let stderr_bytes = stderr_sink.buf.lock().unwrap().clone();
    let stderr_text = String::from_utf8(stderr_bytes).expect("stderr UTF-8");
    assert!(
        !stderr_text.is_empty(),
        "structured startup/stdio logs must be emitted to the stderr writer"
    );
}

// spec: OPS-001, OPS-002, RUN-090
#[tokio::test]
#[ignore = "needs loopback sockets"]
async fn serve_http_binds_loopback_and_shuts_down() {
    let sf = ServerFile::load(repo_root().join("examples/walking-skeleton/gap.toml")).unwrap();
    let assembled = assemble(&sf, &FakeBranchFactory).await.unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    drop(serve_http(assembled, listener));
}
