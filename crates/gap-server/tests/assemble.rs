//! Gateway assembly, deterministic `BranchUid`, composition/describe failures, `HideT2` visibility,
//! and socket-free `DefaultBranchFactory` guard tests (`Brief §4`).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::http::header::CONTENT_TYPE;
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use gap_core::config::{BranchProtocol, BranchSpec};
use gap_core::context::{Budget, Principal, PrincipalKind, RequestContext, TraceCtx};
use gap_core::error::{ErrorCode, NgcErrorCode, NgcErrorInfo};
use gap_core::ids::{BranchId, BranchUid, TenantId};
use gap_core::snapshot::Snapshot;
use gap_core::traits::{BoxFuture, HttpExchangeRequest, HttpModule};
use gap_mcp::outbound::fake::{FakeCall, FakeMcpServer, FakeTool, ServerEra};
use gap_mcp::outbound::transport::http::HttpTransport;
use gap_mcp::outbound::{McpConfig, McpConnector};
use gap_ngc_openapi::{
    AuthConfig, OpenApiConfig, OpenApiConnector, StaticCredentialSource, Upstream, UpstreamError,
    UpstreamRequest, UpstreamResponse,
};
use gap_ngc_sdk::connector::{BoxStream, CallScope, Connector, NgcResult};
use gap_proto::ngc::v1::{
    DescribeRequest, DescribeResponse, HealthRequest, HealthResponse, QueryRequest, QueryResponse,
};
use gap_server::assemble::{
    AssembleError, BranchBuildError, BranchFactory, BranchRuntime, DefaultBranchFactory,
    ServerFile, assemble, wrap_connector,
};
use serde_json::{Value, json};
use tempfile::TempDir;
use uuid::Uuid;

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("canonical repo root")
}

fn find_branch<'a>(
    snapshot: &'a Snapshot,
    id: &BranchId,
) -> Option<&'a gap_core::snapshot::BranchDescriptor> {
    snapshot
        .branches
        .values()
        .map(Arc::as_ref)
        .find(|b| &b.id == id)
}

/// Deterministic in-memory OpenAPI upstream serving the walking-skeleton shop dataset.
#[derive(Debug, Clone, Default)]
struct FakeShopUpstream;

impl Upstream for FakeShopUpstream {
    async fn send(&self, request: UpstreamRequest) -> Result<UpstreamResponse, UpstreamError> {
        let pq = request.path_and_query.as_str();
        let (path, _query) = pq.split_once('?').unwrap_or((pq, ""));
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));

        let body_val = match path {
            "/customers" => json!([
                {"id": 1, "name": "Ada Lovelace", "tier": "vip"},
                {"id": 2, "name": "Alan Turing", "tier": "standard"}
            ]),
            "/customers/1" => json!({"id": 1, "name": "Ada Lovelace", "tier": "vip"}),
            "/customers/2" => json!({"id": 2, "name": "Alan Turing", "tier": "standard"}),
            "/orders" => json!([
                {"id": 101, "customer_id": 1, "total_cents": 4200, "status": "paid"},
                {"id": 103, "customer_id": 1, "total_cents": 8500, "status": "pending"}
            ]),
            "/orders/101" => {
                json!({"id": 101, "customer_id": 1, "total_cents": 4200, "status": "paid"})
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
    server.set_resources(vec![json!({
        "uri": "note://handbook/onboarding",
        "name": "Customer Onboarding Handbook",
        "mimeType": "text/plain"
    })]);
    server.set_call_handler(|name, args| {
        if name == "customer_notes" {
            let cid = args
                .get("customer_id")
                .and_then(Value::as_i64)
                .unwrap_or_default();
            FakeCall::Reply(json!({
                "content": [{"type": "text", "text": format!("notes for {cid}")}],
                "structuredContent": {
                    "customer_id": cid,
                    "notes": [{"note_id": "n-1", "customer_id": cid, "summary": "VIP note"}]
                },
                "isError": false
            }))
        } else {
            FakeCall::Error(-32602, "unknown tool".to_owned())
        }
    });
    server
}

/// Socket-free [`BranchFactory`] wiring `FakeShopUpstream` and `FakeMcpServer`.
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
                    let doc_bytes = server_file.openapi_document(&branch.id).ok_or_else(|| {
                        BranchBuildError::ConnectorInit {
                            branch_id: branch.id.clone(),
                            reason: "missing preloaded OpenAPI bytes".to_owned(),
                        }
                    })?;
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
                    .map_err(|err| BranchBuildError::ConnectorInit {
                        branch_id: branch.id.clone(),
                        reason: err.to_string(),
                    })?;
                    wrap_connector(&branch.id, connector).await
                }
                BranchProtocol::Mcp => {
                    let fake_mcp = build_fake_mcp_server();
                    let transport = Arc::new(HttpTransport::new(fake_mcp.http_client(), 1 << 20));
                    let mcp_cfg = McpConfig {
                        connector_id: branch.id.to_string(),
                        ..McpConfig::default()
                    };
                    let connector = McpConnector::new(transport, mcp_cfg).map_err(|err| {
                        BranchBuildError::ConnectorInit {
                            branch_id: branch.id.clone(),
                            reason: err.to_string(),
                        }
                    })?;
                    let mut runtime = wrap_connector(&branch.id, connector).await?;
                    runtime.handle = Box::new((runtime.handle, fake_mcp));
                    Ok(runtime)
                }
            }
        })
    }
}

// spec: ARC-002, ARC-007, OPS-001, INV-01
#[tokio::test]
async fn assemble_walking_skeleton_with_fake_upstreams_composes_generation_one() {
    let skeleton_toml = repo_root().join("examples/walking-skeleton/gap.toml");
    let sf = ServerFile::load(&skeleton_toml).unwrap();

    let assembled = assemble(&sf, &FakeBranchFactory)
        .await
        .expect("walking-skeleton must assemble");

    assert_eq!(assembled.snapshot_handle.generation(), 1);
    let pinned = assembled.snapshot_handle.pin();
    assert_eq!(pinned.generation, 1);
    assert_eq!(pinned.branches.len(), 2);

    let shop_id = BranchId::new("shop").unwrap();
    let notes_id = BranchId::new("notes").unwrap();
    let shop_desc = find_branch(&pinned, &shop_id).expect("shop branch");
    let notes_desc = find_branch(&pinned, &notes_id).expect("notes branch");

    assert!(assembled.app_state.registry.get(&shop_desc.uid).is_some());
    assert!(assembled.app_state.registry.get(&notes_desc.uid).is_some());
    assert_eq!(assembled.app_state.modules.len(), 1);
    assert_eq!(assembled.app_state.modules[0].module_name(), "mcp");
}

// spec: OPS-001, INV-01
#[tokio::test]
async fn branch_uid_is_deterministic_across_repeated_assemblies_and_honours_explicit_uid() {
    let skeleton_toml = repo_root().join("examples/walking-skeleton/gap.toml");
    let sf = ServerFile::load(&skeleton_toml).unwrap();

    let first = assemble(&sf, &FakeBranchFactory).await.unwrap();
    let second = assemble(&sf, &FakeBranchFactory).await.unwrap();

    let shop_id = BranchId::new("shop").unwrap();
    let notes_id = BranchId::new("notes").unwrap();

    let first_pinned = first.snapshot_handle.pin();
    let second_pinned = second.snapshot_handle.pin();
    assert_eq!(
        find_branch(&first_pinned, &shop_id).unwrap().uid,
        find_branch(&second_pinned, &shop_id).unwrap().uid,
        "derived BranchUid must be identical across repeated runs"
    );
    assert_eq!(
        find_branch(&first_pinned, &notes_id).unwrap().uid,
        find_branch(&second_pinned, &notes_id).unwrap().uid
    );

    // Explicit uid in gap.toml is honoured verbatim.
    let tmp = TempDir::new().unwrap();
    let explicit_uid_str = "018f0000-0000-7000-8000-000000000042";
    let gap_toml = tmp.path().join("gap.toml");
    fs::write(
        &gap_toml,
        format!(
            r#"
profile = "dev"

[[branches]]
id = "notes"
uid = "{explicit_uid_str}"
protocol = "mcp"
[branches.mcp]
transport = "http"
url = "http://127.0.0.1:9102/mcp"
"#
        ),
    )
    .unwrap();
    let sf_explicit = ServerFile::load(&gap_toml).unwrap();
    let assembled_explicit = assemble(&sf_explicit, &FakeBranchFactory).await.unwrap();
    let expected_uid: BranchUid = explicit_uid_str.parse().unwrap();
    let explicit_pinned = assembled_explicit.snapshot_handle.pin();
    assert_eq!(
        find_branch(&explicit_pinned, &notes_id).unwrap().uid,
        expected_uid
    );
}

// spec: OPS-001
#[tokio::test]
async fn assemble_composition_error_names_branch_and_is_not_swallowed() {
    let tmp = TempDir::new().unwrap();
    let spec_file = tmp.path().join("dup.openapi.json");
    fs::copy(
        repo_root().join("examples/walking-skeleton/shop.openapi.json"),
        &spec_file,
    )
    .unwrap();

    let gap_toml = tmp.path().join("gap.toml");
    fs::write(
        &gap_toml,
        r#"
profile = "dev"

[[branches]]
id = "shop_alpha"
protocol = "openapi"
[branches.openapi]
spec = "dup.openapi.json"
base_url = "http://127.0.0.1:9101"

[[branches]]
id = "shop_bravo"
protocol = "openapi"
[branches.openapi]
spec = "dup.openapi.json"
base_url = "http://127.0.0.1:9102"
"#,
    )
    .unwrap();

    let sf = ServerFile::load(&gap_toml).unwrap();
    let err = assemble(&sf, &FakeBranchFactory)
        .await
        .expect_err("colliding root fields across two branches must fail composition");

    let rendered = err.to_string();
    match err {
        AssembleError::Composition {
            branch,
            code,
            message,
        } => {
            assert_eq!(code, ErrorCode::AmbiguousField);
            assert!(
                branch.contains("shop_bravo") || message.contains("shop_bravo"),
                "composition error must name colliding branch, got branch={branch}, msg={message}"
            );
        }
        other => panic!("expected AssembleError::Composition, got: {other:?}"),
    }
    assert!(
        rendered.contains("shop_bravo"),
        "rendered error must name offending branch: {rendered}"
    );
}

#[derive(Debug)]
struct FailingDescribeConnector;

impl Connector for FailingDescribeConnector {
    async fn describe(
        &self,
        _req: DescribeRequest,
        _scope: CallScope,
    ) -> NgcResult<DescribeResponse> {
        Err(NgcErrorInfo::new(
            NgcErrorCode::Unavailable,
            "simulated startup describe failure",
        ))
    }

    async fn health(&self, _req: HealthRequest, _scope: CallScope) -> NgcResult<HealthResponse> {
        Ok(HealthResponse::default())
    }

    async fn query(
        &self,
        _req: QueryRequest,
        _scope: CallScope,
    ) -> NgcResult<BoxStream<QueryResponse>> {
        Err(NgcErrorInfo::new(
            NgcErrorCode::Unimplemented,
            "unused in test",
        ))
    }
}

#[derive(Debug)]
struct FailingDescribeFactory;

impl BranchFactory for FailingDescribeFactory {
    fn build_branch<'a>(
        &'a self,
        _server_file: &'a ServerFile,
        _branch_index: usize,
        branch: &'a BranchSpec,
        _uid: BranchUid,
    ) -> BoxFuture<'a, Result<BranchRuntime, BranchBuildError>> {
        Box::pin(async move { wrap_connector(&branch.id, FailingDescribeConnector).await })
    }
}

// spec: OPS-001
#[tokio::test]
async fn assemble_failing_describe_returns_error_naming_branch() {
    let tmp = TempDir::new().unwrap();
    let gap_toml = tmp.path().join("gap.toml");
    fs::write(
        &gap_toml,
        r#"
profile = "dev"

[[branches]]
id = "broken_notes"
protocol = "mcp"
[branches.mcp]
transport = "http"
url = "http://127.0.0.1:9102/mcp"
"#,
    )
    .unwrap();

    let sf = ServerFile::load(&gap_toml).unwrap();
    let err = assemble(&sf, &FailingDescribeFactory)
        .await
        .expect_err("failing Describe must abort startup");
    let rendered = err.to_string();
    assert!(
        rendered.contains("broken_notes"),
        "Describe error must name `broken_notes`, got: {rendered}"
    );
    match err {
        AssembleError::Branch(BranchBuildError::Describe { branch_id, .. }) => {
            assert_eq!(branch_id.as_str(), "broken_notes");
        }
        other => panic!("expected BranchBuildError::Describe, got: {other:?}"),
    }
}

fn make_ctx(snapshot: Arc<Snapshot>) -> RequestContext {
    let tenant = TenantId::new("default").unwrap();
    let mut principal = Principal::new("tester", PrincipalKind::Service, tenant.clone());
    principal.scopes.insert("read".to_owned());
    RequestContext::try_new(
        Uuid::now_v7(),
        tenant,
        principal,
        Instant::now() + Duration::from_secs(30),
        Budget::new(10_000),
        TraceCtx::generate_root(true),
        0,
        None,
        snapshot,
    )
    .unwrap()
}

async fn call_facade_overview(facade: &gap_mcp::inbound::McpFacade, ctx: &RequestContext) -> Value {
    let body = serde_json::to_vec(&json!({
        "jsonrpc": "2.0",
        "id": 1,
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
    .unwrap();
    let req = HttpExchangeRequest {
        method: "POST".to_owned(),
        path: "/mcp".to_owned(),
        query: None,
        headers: vec![
            ("content-type".to_owned(), "application/json".to_owned()),
            (
                "accept".to_owned(),
                "application/json, text/event-stream".to_owned(),
            ),
            ("mcp-protocol-version".to_owned(), "2026-07-28".to_owned()),
            ("mcp-method".to_owned(), "tools/call".to_owned()),
            ("mcp-name".to_owned(), "gap_describe".to_owned()),
        ],
        body,
    };
    let resp = facade.handle(ctx, req).await.unwrap();
    assert_eq!(resp.status, 200);
    serde_json::from_slice(&resp.body).unwrap()
}

// spec: OPS-001, ARC-007
#[tokio::test]
async fn assemble_visibility_hides_t2_in_prod_exposes_in_dev_and_propagates_spotlight() {
    let tmp = TempDir::new().unwrap();
    fs::copy(
        repo_root().join("examples/walking-skeleton/shop.openapi.json"),
        tmp.path().join("shop.openapi.json"),
    )
    .unwrap();

    let key_hash = blake3::hash(b"prod-test-key").to_hex().to_string();

    let dev_toml = tmp.path().join("dev.toml");
    fs::write(
        &dev_toml,
        r#"
profile = "dev"

[gateway]
facade_spotlight = false

[[branches]]
id = "shop"
protocol = "openapi"
trust = "T1"
[branches.openapi]
spec = "shop.openapi.json"
base_url = "http://127.0.0.1:9101"

[[branches]]
id = "untrusted_notes"
protocol = "mcp"
trust = "T2"
[branches.mcp]
transport = "http"
url = "http://127.0.0.1:9102/mcp"
"#,
    )
    .unwrap();

    let prod_toml = tmp.path().join("prod.toml");
    fs::write(
        &prod_toml,
        format!(
            r#"
profile = "prod"

[gateway]
facade_spotlight = true

[[api_keys]]
id = "prod-key"
key_hash = "{key_hash}"
tenant = "default"
scopes = ["read"]

[[branches]]
id = "shop"
protocol = "openapi"
trust = "T1"
[branches.openapi]
spec = "shop.openapi.json"
base_url = "http://127.0.0.1:9101"

[[branches]]
id = "untrusted_notes"
protocol = "mcp"
trust = "T2"
[branches.mcp]
transport = "http"
url = "http://127.0.0.1:9102/mcp"
"#
        ),
    )
    .unwrap();

    let sf_dev = ServerFile::load(&dev_toml).unwrap();
    let asm_dev = assemble(&sf_dev, &FakeBranchFactory).await.unwrap();
    assert!(!asm_dev.facade.config().spotlight);

    let dev_ctx = make_ctx(asm_dev.snapshot_handle.pin());
    let dev_json = call_facade_overview(&asm_dev.facade, &dev_ctx).await;
    let dev_branches = dev_json["result"]["structuredContent"]["data"]["overview"]["branches"]
        .as_array()
        .unwrap();
    let dev_ids: Vec<&str> = dev_branches
        .iter()
        .filter_map(|b| b["id"].as_str())
        .collect();
    assert!(dev_ids.contains(&"shop"));
    assert!(
        dev_ids.contains(&"untrusted_notes"),
        "AllVisible in dev must expose T2 branch, got: {dev_ids:?}"
    );

    let sf_prod = ServerFile::load(&prod_toml).unwrap();
    let asm_prod = assemble(&sf_prod, &FakeBranchFactory).await.unwrap();
    assert!(asm_prod.facade.config().spotlight);

    let prod_ctx = make_ctx(asm_prod.snapshot_handle.pin());
    let prod_json = call_facade_overview(&asm_prod.facade, &prod_ctx).await;
    let prod_branches = prod_json["result"]["structuredContent"]["data"]["overview"]["branches"]
        .as_array()
        .unwrap();
    let prod_ids: Vec<&str> = prod_branches
        .iter()
        .filter_map(|b| b["id"].as_str())
        .collect();
    assert_eq!(
        prod_ids,
        vec!["shop"],
        "HideT2 in prod must hide T2 branch, got: {prod_ids:?}"
    );
}

// spec: OPS-001, INV-15
#[tokio::test]
async fn default_branch_factory_rejects_https_cleartext_remote_credentials_and_prod_stdio_without_leaking_canary()
 {
    let canary = "sk-canary-branch-99887766-do-not-leak";
    let tmp = TempDir::new().unwrap();
    fs::write(
        tmp.path().join("shop.openapi.json"),
        r#"{"openapi":"3.0.3","info":{"title":"ok","version":"1"},"paths":{"/ping":{"get":{"operationId":"ping","responses":{"200":{"description":"ok","content":{"application/json":{"schema":{"type":"object","properties":{"ok":{"type":"boolean"}}}}}}}}}}}"#,
    )
    .unwrap();

    // 1. OpenAPI cleartext credentials to non-loopback host rejected without leaking canary.
    let oa_toml = tmp.path().join("oa_cleartext.toml");
    fs::write(
        &oa_toml,
        r#"
profile = "dev"

[[branches]]
id = "shop"
protocol = "openapi"
[branches.openapi]
spec = "shop.openapi.json"
base_url = "http://10.0.0.5:9101"
[branches.openapi.auth]
kind = "bearer"
secret = "env://UPSTREAM_SECRET"
"#,
    )
    .unwrap();
    let sf_oa = ServerFile::load_with_env(&oa_toml, [("UPSTREAM_SECRET", canary)]).unwrap();
    let uid = gap_server::assemble::derive_branch_uid(&sf_oa.config().branches[0].id);
    let err_oa = DefaultBranchFactory
        .build_branch(&sf_oa, 0, &sf_oa.config().branches[0], uid)
        .await
        .expect_err("cleartext remote OpenAPI credentials must be refused");
    assert!(!err_oa.to_string().contains(canary));
    assert!(!format!("{err_oa:?}").contains(canary));

    // 2. MCP HTTP api_key credentials to non-loopback host rejected without leaking canary.
    let mcp_toml = tmp.path().join("mcp_cleartext.toml");
    fs::write(
        &mcp_toml,
        r#"
profile = "dev"

[[branches]]
id = "notes"
protocol = "mcp"
[branches.mcp]
transport = "http"
url = "http://10.0.0.5:9102/mcp"
[branches.mcp.auth]
kind = "api_key"
name = "X-Api-Key"
secret = "env://UPSTREAM_SECRET"
"#,
    )
    .unwrap();
    let sf_mcp = ServerFile::load_with_env(&mcp_toml, [("UPSTREAM_SECRET", canary)]).unwrap();
    let err_mcp = DefaultBranchFactory
        .build_branch(&sf_mcp, 0, &sf_mcp.config().branches[0], uid)
        .await
        .expect_err("cleartext remote MCP credentials must be refused");
    assert!(!err_mcp.to_string().contains(canary));
    assert!(!format!("{err_mcp:?}").contains(canary));

    // 3. MCP stdio in prod refused by LaunchPolicy::Refuse before spawning any process.
    let prod_stdio_toml = tmp.path().join("prod_stdio.toml");
    let key_hash = blake3::hash(b"prod-test-key").to_hex().to_string();
    fs::write(
        &prod_stdio_toml,
        format!(
            r#"
profile = "prod"

[[api_keys]]
id = "prod-key"
key_hash = "{key_hash}"
tenant = "default"
scopes = ["read"]

[[branches]]
id = "local_mcp"
protocol = "mcp"
[branches.mcp]
transport = "stdio"
command = "/bin/echo"
sha256 = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
"#
        ),
    )
    .unwrap();
    let sf_stdio = ServerFile::load(&prod_stdio_toml).unwrap();
    let err_stdio = DefaultBranchFactory
        .build_branch(&sf_stdio, 0, &sf_stdio.config().branches[0], uid)
        .await
        .expect_err("stdio MCP must be refused in prod");
    assert!(err_stdio.to_string().contains("local_mcp"));
}
