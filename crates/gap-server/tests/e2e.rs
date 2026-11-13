//! In-process end-to-end gateway tests driving `gap_transport::router` with `tower::ServiceExt::oneshot`
//! over fake OpenAPI and MCP upstreams (`Brief §4`, zero sockets, zero child processes).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;

use axum::body::Body;
use axum::http::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE, WWW_AUTHENTICATE};
use axum::http::{HeaderMap, HeaderValue, Method, Request, StatusCode};
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
use serde_json::{Value, json};
use tempfile::TempDir;
use tower::ServiceExt as _;

/// Raw development key of `examples/walking-skeleton/gap.toml` (its config forbids anonymous access).
const DEV_KEY: &str = "gap_dev_skeleton_key_01";

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("canonical repo root")
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

async fn read_response_bytes(resp: axum::response::Response) -> (StatusCode, HeaderMap, Vec<u8>) {
    let status = resp.status();
    let headers = resp.headers().clone();
    let bytes = axum::body::to_bytes(resp.into_body(), 4 * 1024 * 1024)
        .await
        .unwrap()
        .to_vec();
    (status, headers, bytes)
}

fn mcp_post_request(
    method: &str,
    mcp_name: Option<&str>,
    auth_bearer: Option<&str>,
    body: &Value,
) -> Request<Body> {
    let mut builder = Request::builder()
        .method(Method::POST)
        .uri("/mcp")
        .header(CONTENT_TYPE, "application/json")
        .header(ACCEPT, "application/json, text/event-stream")
        .header("MCP-Protocol-Version", "2026-07-28")
        .header("Mcp-Method", method);
    if let Some(name) = mcp_name {
        builder = builder.header("Mcp-Name", name);
    }
    if let Some(token) = auth_bearer {
        builder = builder.header(AUTHORIZATION, format!("Bearer {token}"));
    }
    builder
        .body(Body::from(serde_json::to_vec(body).unwrap()))
        .unwrap()
}

// spec: ARC-002, ARC-007, OPS-002, INV-01
#[tokio::test]
async fn e2e_graphql_queries_across_both_openapi_and_mcp_branches_with_generation() {
    let sf = ServerFile::load(repo_root().join("examples/walking-skeleton/gap.toml")).unwrap();
    let assembled = assemble(&sf, &FakeBranchFactory).await.unwrap();
    let app = gap_transport::router(Arc::clone(&assembled.app_state));

    let gql_body = json!({
        "query": "{ customer(id: 1) { id name tier } orders(customer_id: 1) { id total_cents status } customer_notes(customer_id: 1) }"
    });
    let req = Request::builder()
        .method(Method::POST)
        .uri("/graphql")
        .header(CONTENT_TYPE, "application/json")
        .header(AUTHORIZATION, format!("Bearer {DEV_KEY}"))
        .header(ACCEPT, "application/graphql-response+json")
        .body(Body::from(serde_json::to_vec(&gql_body).unwrap()))
        .unwrap();

    let (status, _headers, body) = read_response_bytes(app.oneshot(req).await.unwrap()).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "body={}",
        String::from_utf8_lossy(&body)
    );

    let json_resp: Value = serde_json::from_slice(&body).unwrap();
    assert!(
        json_resp.get("errors").is_none(),
        "unexpected GraphQL errors: {json_resp}"
    );
    assert_eq!(json_resp["extensions"]["gap"]["generation"], 1);
    assert_eq!(
        json_resp["data"]["customer"],
        json!({"id": 1, "name": "Ada Lovelace", "tier": "vip"})
    );
    let orders = json_resp["data"]["orders"].as_array().unwrap();
    assert_eq!(orders.len(), 2);
    assert_eq!(orders[0]["id"], 101);
    assert_eq!(orders[0]["total_cents"], 4200);
    assert_eq!(json_resp["data"]["customer_notes"]["customer_id"], 1);
    assert_eq!(
        json_resp["data"]["customer_notes"]["notes"][0]["summary"],
        "VIP note"
    );
}

// spec: OPS-002, ARC-007, INV-01
#[tokio::test]
async fn e2e_mcp_http_discover_list_describe_search_query_and_refused_mutation() {
    let sf = ServerFile::load(repo_root().join("examples/walking-skeleton/gap.toml")).unwrap();
    let assembled = assemble(&sf, &FakeBranchFactory).await.unwrap();
    let app = gap_transport::router(Arc::clone(&assembled.app_state));

    let meta = json!({
        "io.modelcontextprotocol/protocolVersion": "2026-07-28",
        "io.modelcontextprotocol/clientCapabilities": {}
    });

    // 1. server/discover
    let discover_req = mcp_post_request(
        "server/discover",
        None,
        Some(DEV_KEY),
        &json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "server/discover",
            "params": { "_meta": meta }
        }),
    );
    let (st, _, b) = read_response_bytes(app.clone().oneshot(discover_req).await.unwrap()).await;
    assert_eq!(st, StatusCode::OK, "body={}", String::from_utf8_lossy(&b));
    let discover_val: Value = serde_json::from_slice(&b).unwrap();
    assert_eq!(discover_val["result"]["resultType"], "complete");
    assert!(
        discover_val["result"]["supportedVersions"]
            .as_array()
            .unwrap()
            .contains(&json!("2026-07-28"))
    );

    // 2. tools/list (static 5 tools)
    let list_req = mcp_post_request(
        "tools/list",
        None,
        Some(DEV_KEY),
        &json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/list",
            "params": { "_meta": meta }
        }),
    );
    let (st, _, b) = read_response_bytes(app.clone().oneshot(list_req).await.unwrap()).await;
    assert_eq!(st, StatusCode::OK);
    let list_val: Value = serde_json::from_slice(&b).unwrap();
    let tool_names: Vec<&str> = list_val["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|t| t["name"].as_str())
        .collect();
    assert_eq!(
        tool_names,
        vec![
            "gap_search",
            "gap_describe",
            "gap_query",
            "gap_mutate",
            "gap_task"
        ]
    );

    // 3. gap_describe { overview: true }
    let describe_req = mcp_post_request(
        "tools/call",
        Some("gap_describe"),
        Some(DEV_KEY),
        &json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": {
                "_meta": meta,
                "name": "gap_describe",
                "arguments": { "overview": true }
            }
        }),
    );
    let (st, _, b) = read_response_bytes(app.clone().oneshot(describe_req).await.unwrap()).await;
    assert_eq!(st, StatusCode::OK);
    let desc_val: Value = serde_json::from_slice(&b).unwrap();
    assert_eq!(desc_val["result"]["isError"], false);
    let branch_ids: Vec<&str> =
        desc_val["result"]["structuredContent"]["data"]["overview"]["branches"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|br| br["id"].as_str())
            .collect();
    assert!(branch_ids.contains(&"shop"));
    assert!(branch_ids.contains(&"notes"));

    // 4. gap_search
    let search_req = mcp_post_request(
        "tools/call",
        Some("gap_search"),
        Some(DEV_KEY),
        &json!({
            "jsonrpc": "2.0",
            "id": 4,
            "method": "tools/call",
            "params": {
                "_meta": meta,
                "name": "gap_search",
                "arguments": { "query": "customer" }
            }
        }),
    );
    let (st, _, b) = read_response_bytes(app.clone().oneshot(search_req).await.unwrap()).await;
    assert_eq!(st, StatusCode::OK);
    let search_val: Value = serde_json::from_slice(&b).unwrap();
    assert_eq!(search_val["result"]["isError"], false);
    let hits = search_val["result"]["structuredContent"]["data"]["__search"]["hits"]
        .as_array()
        .unwrap();
    assert!(
        !hits.is_empty(),
        "gap_search for `customer` must return hits"
    );

    // 5. gap_query across both branches
    let query_req = mcp_post_request(
        "tools/call",
        Some("gap_query"),
        Some(DEV_KEY),
        &json!({
            "jsonrpc": "2.0",
            "id": 5,
            "method": "tools/call",
            "params": {
                "_meta": meta,
                "name": "gap_query",
                "arguments": {
                    "query": "{ customer(id: 1) { id name } customer_notes(customer_id: 1) }"
                }
            }
        }),
    );
    let (st, _, b) = read_response_bytes(app.clone().oneshot(query_req).await.unwrap()).await;
    assert_eq!(st, StatusCode::OK);
    let query_val: Value = serde_json::from_slice(&b).unwrap();
    assert_eq!(query_val["result"]["isError"], false, "resp={query_val}");
    assert_eq!(
        query_val["result"]["structuredContent"]["data"]["customer"],
        json!({"id": 1, "name": "Ada Lovelace"})
    );
    assert_eq!(
        query_val["result"]["structuredContent"]["data"]["customer_notes"]["customer_id"],
        1
    );
    assert_eq!(
        query_val["result"]["structuredContent"]["extensions"]["gap"]["generation"],
        1
    );

    // 6. Mutation refused by gap_query
    let mut_req = mcp_post_request(
        "tools/call",
        Some("gap_query"),
        Some(DEV_KEY),
        &json!({
            "jsonrpc": "2.0",
            "id": 6,
            "method": "tools/call",
            "params": {
                "_meta": meta,
                "name": "gap_query",
                "arguments": {
                    "query": "mutation { deleteEverything }"
                }
            }
        }),
    );
    let (st, _, b) = read_response_bytes(app.oneshot(mut_req).await.unwrap()).await;
    assert_eq!(st, StatusCode::OK);
    let mut_val: Value = serde_json::from_slice(&b).unwrap();
    assert_eq!(mut_val["result"]["isError"], true);
    assert_eq!(
        mut_val["result"]["structuredContent"]["errors"][0]["extensions"]["code"],
        "GAP_OPERATION_NOT_ALLOWED"
    );
}

// spec: OPS-001, OPS-002, SEC-004, SEC-010, INV-15
#[tokio::test]
async fn e2e_prod_api_key_auth_enforced_on_both_graphql_and_mcp() {
    let tmp = TempDir::new().unwrap();
    fs::copy(
        repo_root().join("examples/walking-skeleton/shop.openapi.json"),
        tmp.path().join("shop.openapi.json"),
    )
    .unwrap();

    let raw_key = "gap_prod_secret_key_01";
    let key_hash = blake3::hash(raw_key.as_bytes()).to_hex().to_string();
    let prod_toml = tmp.path().join("gap.toml");
    fs::write(
        &prod_toml,
        format!(
            r#"
profile = "prod"

[[api_keys]]
id = "prod-agent"
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
id = "notes"
protocol = "mcp"
trust = "T2"
[branches.mcp]
transport = "http"
url = "http://127.0.0.1:9102/mcp"
"#
        ),
    )
    .unwrap();

    let sf = ServerFile::load(&prod_toml).unwrap();
    let assembled = assemble(&sf, &FakeBranchFactory).await.unwrap();
    let app = gap_transport::router(Arc::clone(&assembled.app_state));

    let meta = json!({
        "io.modelcontextprotocol/protocolVersion": "2026-07-28",
        "io.modelcontextprotocol/clientCapabilities": {}
    });

    // 1. Missing Authorization -> 401 on /graphql and /mcp
    let gql_no_auth = Request::builder()
        .method(Method::POST)
        .uri("/graphql")
        .header(CONTENT_TYPE, "application/json")
        .body(Body::from(r#"{"query":"{ customer(id: 1) { id } }"}"#))
        .unwrap();
    let (st, hdrs, _) = read_response_bytes(app.clone().oneshot(gql_no_auth).await.unwrap()).await;
    assert_eq!(st, StatusCode::UNAUTHORIZED);
    assert!(hdrs.contains_key(WWW_AUTHENTICATE));

    let mcp_no_auth = mcp_post_request(
        "tools/call",
        Some("gap_describe"),
        None,
        &json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": { "_meta": meta, "name": "gap_describe", "arguments": { "overview": true } }
        }),
    );
    let (st, hdrs, _) = read_response_bytes(app.clone().oneshot(mcp_no_auth).await.unwrap()).await;
    assert_eq!(st, StatusCode::UNAUTHORIZED);
    assert!(hdrs.contains_key(WWW_AUTHENTICATE));

    // 2. Invalid Authorization -> 401 on /graphql and /mcp
    let gql_bad_auth = Request::builder()
        .method(Method::POST)
        .uri("/graphql")
        .header(CONTENT_TYPE, "application/json")
        .header(AUTHORIZATION, "Bearer wrong_secret_key")
        .body(Body::from(r#"{"query":"{ customer(id: 1) { id } }"}"#))
        .unwrap();
    let (st, _, _) = read_response_bytes(app.clone().oneshot(gql_bad_auth).await.unwrap()).await;
    assert_eq!(st, StatusCode::UNAUTHORIZED);

    let mcp_bad_auth = mcp_post_request(
        "tools/call",
        Some("gap_describe"),
        Some("wrong_secret_key"),
        &json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": { "_meta": meta, "name": "gap_describe", "arguments": { "overview": true } }
        }),
    );
    let (st, _, _) = read_response_bytes(app.clone().oneshot(mcp_bad_auth).await.unwrap()).await;
    assert_eq!(st, StatusCode::UNAUTHORIZED);

    // 3. Valid Authorization -> /mcp succeeds (200) and HideT2 hides `notes`; /graphql passes authn (403 Cedar stub, not 401)
    let mcp_good_auth = mcp_post_request(
        "tools/call",
        Some("gap_describe"),
        Some(raw_key),
        &json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": { "_meta": meta, "name": "gap_describe", "arguments": { "overview": true } }
        }),
    );
    let (st, _, b) = read_response_bytes(app.clone().oneshot(mcp_good_auth).await.unwrap()).await;
    assert_eq!(st, StatusCode::OK);
    let val: Value = serde_json::from_slice(&b).unwrap();
    let visible_ids: Vec<&str> = val["result"]["structuredContent"]["data"]["overview"]["branches"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|br| br["id"].as_str())
        .collect();
    assert_eq!(visible_ids, vec!["shop"]);

    let gql_good_auth = Request::builder()
        .method(Method::POST)
        .uri("/graphql")
        .header(CONTENT_TYPE, "application/json")
        .header(AUTHORIZATION, format!("Bearer {raw_key}"))
        .body(Body::from(r#"{"query":"{ customer(id: 1) { id } }"}"#))
        .unwrap();
    let (st, _, b) = read_response_bytes(app.oneshot(gql_good_auth).await.unwrap()).await;
    assert_eq!(
        st,
        StatusCode::FORBIDDEN,
        "valid prod API key must pass authentication (403 Cedar default-deny, not 401), body={}",
        String::from_utf8_lossy(&b)
    );
}

// spec: OPS-002, RUN-090
#[tokio::test]
async fn e2e_healthz_readyz_draining_transition_and_metrics() {
    let sf = ServerFile::load(repo_root().join("examples/walking-skeleton/gap.toml")).unwrap();
    let assembled = assemble(&sf, &FakeBranchFactory).await.unwrap();
    let app = gap_transport::router(Arc::clone(&assembled.app_state));

    // /healthz -> 200
    let (st, _, _) = read_response_bytes(
        app.clone()
            .oneshot(
                Request::builder()
                    .uri("/healthz")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(st, StatusCode::OK);

    // /readyz -> 200 before draining
    let (st, _, _) = read_response_bytes(
        app.clone()
            .oneshot(
                Request::builder()
                    .uri("/readyz")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(st, StatusCode::OK);

    // Flip draining -> /readyz must immediately return 503 while /healthz stays 200
    assembled.draining.store(true, Ordering::SeqCst);
    let (st_ready, _, _) = read_response_bytes(
        app.clone()
            .oneshot(
                Request::builder()
                    .uri("/readyz")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(st_ready, StatusCode::SERVICE_UNAVAILABLE);

    let (st_health, _, _) = read_response_bytes(
        app.clone()
            .oneshot(
                Request::builder()
                    .uri("/healthz")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(st_health, StatusCode::OK);

    // /metrics -> 200 Prometheus text
    let (st_metrics, hdrs, metrics_bytes) = read_response_bytes(
        app.oneshot(
            Request::builder()
                .uri("/metrics")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap(),
    )
    .await;
    assert_eq!(st_metrics, StatusCode::OK);
    assert!(!metrics_bytes.is_empty());
    let ct = hdrs
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(
        ct.contains("text/plain"),
        "expected Prometheus text/plain content-type, got: {ct}"
    );
}

// spec: SEC-002, SEC-010, OPS-001
#[tokio::test]
async fn e2e_dev_profile_serves_anonymous_requests_only_when_allow_anonymous_is_true() {
    let meta = json!({
        "io.modelcontextprotocol/protocolVersion": "2026-07-28",
        "io.modelcontextprotocol/clientCapabilities": {}
    });
    for (allow, expected) in [
        ("false", StatusCode::UNAUTHORIZED),
        ("true", StatusCode::OK),
    ] {
        let tmp = TempDir::new().unwrap();
        fs::copy(
            repo_root().join("examples/walking-skeleton/shop.openapi.json"),
            tmp.path().join("shop.openapi.json"),
        )
        .unwrap();
        let toml = tmp.path().join("gap.toml");
        fs::write(
            &toml,
            format!(
                r#"
profile = "dev"

[auth]
allow_anonymous = {allow}

[[branches]]
id = "shop"
protocol = "openapi"
trust = "T1"
[branches.openapi]
spec = "shop.openapi.json"
base_url = "http://127.0.0.1:9101"

[[branches]]
id = "notes"
protocol = "mcp"
trust = "T1"
[branches.mcp]
transport = "http"
url = "http://127.0.0.1:9102/mcp"
"#
            ),
        )
        .unwrap();
        let sf = ServerFile::load(&toml).unwrap();
        let assembled = assemble(&sf, &FakeBranchFactory).await.unwrap();
        let app = gap_transport::router(Arc::clone(&assembled.app_state));

        let graphql = Request::builder()
            .method(Method::POST)
            .uri("/graphql")
            .header(CONTENT_TYPE, "application/json")
            .body(Body::from(r#"{"query":"{ customer(id: 1) { id } }"}"#))
            .unwrap();
        let (status, _, body) =
            read_response_bytes(app.clone().oneshot(graphql).await.unwrap()).await;
        assert_eq!(
            status,
            expected,
            "/graphql with allow_anonymous = {allow}: {}",
            String::from_utf8_lossy(&body)
        );

        let mcp = mcp_post_request(
            "tools/call",
            Some("gap_describe"),
            None,
            &json!({
                "jsonrpc": "2.0", "id": 1, "method": "tools/call",
                "params": { "_meta": meta, "name": "gap_describe", "arguments": { "overview": true } }
            }),
        );
        let (status, _, _) = read_response_bytes(app.clone().oneshot(mcp).await.unwrap()).await;
        assert_eq!(status, expected, "/mcp with allow_anonymous = {allow}");
    }
}

// spec: RUN-013, CMP-008
#[tokio::test]
async fn e2e_typed_path_overhead_p99_under_10ms_with_zero_io_mocks() {
    // Measures GAP typed-path overhead (parse + plan + exec) over zero-I/O mocks.
    // Gate: p99 of 200 iterations must be < 10 ms (RUN-013 ceiling at 1 ms upstream mocks;
    // zero-I/O mocks give the full 10 ms budget to the GAP layer itself).
    use std::time::Instant;

    let sf = ServerFile::load(repo_root().join("examples/walking-skeleton/gap.toml")).unwrap();
    let assembled = assemble(&sf, &FakeBranchFactory).await.unwrap();
    let app = gap_transport::router(Arc::clone(&assembled.app_state));

    const ITERATIONS: usize = 200;
    let gql_body = serde_json::to_vec(&json!({"query": "{ __typename }"})).unwrap();
    let mut durations_us: Vec<u128> = Vec::with_capacity(ITERATIONS);

    for _ in 0..ITERATIONS {
        let req = Request::builder()
            .method(Method::POST)
            .uri("/graphql")
            .header(CONTENT_TYPE, "application/json")
            .header(AUTHORIZATION, format!("Bearer {DEV_KEY}"))
            .body(Body::from(gql_body.clone()))
            .unwrap();
        let t0 = Instant::now();
        let resp = app.clone().oneshot(req).await.unwrap();
        let _ = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        durations_us.push(t0.elapsed().as_micros());
    }

    durations_us.sort_unstable();
    let p99_us = durations_us[ITERATIONS * 99 / 100];
    let p99_ms = p99_us as f64 / 1000.0;
    assert!(
        p99_ms < 10.0,
        "typed-path p99 {p99_ms:.2} ms exceeds 10 ms ceiling (RUN-013); \
         median={:.2} ms, max={:.2} ms",
        durations_us[ITERATIONS / 2] as f64 / 1000.0,
        durations_us[ITERATIONS - 1] as f64 / 1000.0
    );
}
