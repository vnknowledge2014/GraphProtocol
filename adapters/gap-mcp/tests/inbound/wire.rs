//! Wire-protocol tests covering `CMP-001`, `ARC-007`, `INV-01`, and `INV-15`.

use std::sync::Arc;
use std::time::Duration;

use futures_util::future::join_all;
use gap_core::error::{ErrorCode, GapError};
use gap_core::traits::{
    GraphqlEngine, GraphqlResponse, HttpExchangeRequest, HttpModule,
};
use gap_mcp::inbound::{FacadeConfig, FacadeConfigError, McpFacade, StdioServer};
use gap_snapshot::SnapshotHandle;
use proptest::prelude::*;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};

use super::support::{
    FakeEngine, compose_two_branch_snapshot, default_facade, make_ctx, modern_http_post,
    run_stdio_session, send_http_json, with_modern_meta,
};

// spec: ARC-007
#[test]
fn module_metadata_is_mcp_and_mounts_slash_mcp() {
    let (facade, _) = default_facade();
    assert_eq!(facade.module_name(), "mcp");
    assert_eq!(facade.mount_prefixes(), &["/mcp"]);
}

// spec: CMP-001
#[test]
fn facade_config_rejects_invalid_bounds_and_debug_is_clean() {
    let engine = FakeEngine::two_branch() as Arc<dyn GraphqlEngine>;

    let zero_bytes = FacadeConfig {
        max_request_bytes: 0,
        ..FacadeConfig::default()
    };
    assert_eq!(
        McpFacade::builder(Arc::clone(&engine))
            .config(zero_bytes)
            .build()
            .expect_err("zero max_request_bytes must fail"),
        FacadeConfigError::InvalidMaxRequestBytes(0)
    );

    let zero_sessions = FacadeConfig {
        max_sessions: 0,
        ..FacadeConfig::default()
    };
    assert_eq!(
        McpFacade::builder(Arc::clone(&engine))
            .config(zero_sessions)
            .build()
            .expect_err("zero max_sessions must fail"),
        FacadeConfigError::ZeroMaxSessions
    );

    let zero_ttl = FacadeConfig {
        session_idle_ttl: Duration::ZERO,
        ..FacadeConfig::default()
    };
    assert_eq!(
        McpFacade::builder(Arc::clone(&engine))
            .config(zero_ttl)
            .build()
            .expect_err("zero session_idle_ttl must fail"),
        FacadeConfigError::ZeroSessionIdleTtl
    );

    let zero_branches = FacadeConfig {
        metagraph_branches: 0,
        ..FacadeConfig::default()
    };
    assert_eq!(
        McpFacade::builder(Arc::clone(&engine))
            .config(zero_branches)
            .build()
            .expect_err("zero metagraph_branches must fail"),
        FacadeConfigError::ZeroMetagraphBranches
    );

    let dbg = format!("{:?}", FacadeConfig::default());
    assert!(dbg.contains("max_request_bytes"));
    assert!(dbg.contains("gap-mcp"));
}

// spec: CMP-001
#[tokio::test]
async fn discover_stateless_http_and_stdio() {
    let (facade, _) = default_facade();
    let snap = compose_two_branch_snapshot(true, 1);
    let ctx = make_ctx(snap, "agent:1", "acme-corp", &["read:all"]);

    let req = modern_http_post(json!(1), "server/discover", None, json!({}));
    let (resp, body) = send_http_json(&facade, &ctx, req).await;
    assert_eq!(resp.status, 200);
    assert_eq!(body["result"]["resultType"], "complete");
    assert_eq!(
        body["result"]["supportedVersions"],
        json!(["2026-07-28", "2025-11-25"])
    );
    assert_eq!(
        body["result"]["capabilities"],
        json!({
            "tools": { "listChanged": false },
            "resources": { "subscribe": false, "listChanged": false }
        })
    );
    assert_eq!(body["result"]["cacheScope"], "public");
    assert!(body["result"]["ttlMs"].as_u64().unwrap_or(0) > 0);
    assert_eq!(
        body["result"]["_meta"]["io.modelcontextprotocol/serverInfo"]["name"],
        "gap-mcp"
    );

    let stdio_lines = vec![
        json!({
            "jsonrpc": "2.0",
            "id": "d-stdio",
            "method": "server/discover",
            "params": with_modern_meta(json!({}))
        })
        .to_string(),
    ];
    let out = run_stdio_session(facade, ctx, &stdio_lines).await;
    assert_eq!(out.len(), 1);
    assert_eq!(out[0]["id"], "d-stdio");
    assert_eq!(out[0]["result"]["resultType"], "complete");
    assert_eq!(
        out[0]["result"]["supportedVersions"],
        json!(["2026-07-28", "2025-11-25"])
    );
}

// spec: CMP-001
#[tokio::test]
async fn missing_meta_protocol_version_or_capabilities_returns_minus_32602() {
    let (facade, _) = default_facade();
    let snap = compose_two_branch_snapshot(true, 1);
    let ctx = make_ctx(snap, "agent:1", "acme-corp", &[]);

    // Missing clientCapabilities in _meta over stdio -> -32602
    let lines = vec![
        json!({
            "jsonrpc": "2.0",
            "id": 10,
            "method": "tools/list",
            "params": {
                "_meta": {
                    "io.modelcontextprotocol/protocolVersion": "2026-07-28"
                }
            }
        })
        .to_string(),
        // Missing protocolVersion in _meta over stdio -> -32602
        json!({
            "jsonrpc": "2.0",
            "id": 11,
            "method": "tools/list",
            "params": {
                "_meta": {
                    "io.modelcontextprotocol/clientCapabilities": {}
                }
            }
        })
        .to_string(),
    ];
    let out = run_stdio_session(facade.clone(), ctx.clone(), &lines).await;
    assert_eq!(out.len(), 2);
    assert_eq!(out[0]["error"]["code"], -32602);
    assert_eq!(out[1]["error"]["code"], -32602);

    // HTTP with matching version header + _meta.protocolVersion, but missing clientCapabilities -> 400 + -32602
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
            (
                "mcp-protocol-version".to_owned(),
                "2026-07-28".to_owned(),
            ),
            ("mcp-method".to_owned(), "tools/list".to_owned()),
        ],
        body: serde_json::to_vec(&json!({
            "jsonrpc": "2.0",
            "id": 12,
            "method": "tools/list",
            "params": {
                "_meta": {
                    "io.modelcontextprotocol/protocolVersion": "2026-07-28"
                }
            }
        }))
        .expect("json"),
    };
    let (resp, body) = send_http_json(&facade, &ctx, req).await;
    assert_eq!(resp.status, 400);
    assert_eq!(body["error"]["code"], -32602);
}

// spec: CMP-001
#[tokio::test]
async fn unsupported_protocol_version_returns_minus_32022_with_supported_list() {
    let (facade, _) = default_facade();
    let snap = compose_two_branch_snapshot(true, 1);
    let ctx = make_ctx(snap, "agent:1", "acme-corp", &[]);

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
            (
                "mcp-protocol-version".to_owned(),
                "2099-01-01".to_owned(),
            ),
            ("mcp-method".to_owned(), "tools/list".to_owned()),
        ],
        body: serde_json::to_vec(&json!({
            "jsonrpc": "2.0",
            "id": 99,
            "method": "tools/list",
            "params": {
                "_meta": {
                    "io.modelcontextprotocol/protocolVersion": "2099-01-01",
                    "io.modelcontextprotocol/clientCapabilities": {}
                }
            }
        }))
        .expect("json"),
    };
    let (resp, body) = send_http_json(&facade, &ctx, req).await;
    assert_eq!(resp.status, 400);
    assert_eq!(body["error"]["code"], -32022);
    assert_eq!(
        body["error"]["data"]["supported"],
        json!(["2026-07-28", "2025-11-25"])
    );
    assert_eq!(body["error"]["data"]["requested"], "2099-01-01");
}

// spec: CMP-001
#[tokio::test]
async fn missing_or_mismatched_mcp_method_header_returns_400_minus_32020() {
    let (facade, _) = default_facade();
    let snap = compose_two_branch_snapshot(true, 1);
    let ctx = make_ctx(snap, "agent:1", "acme-corp", &[]);

    // Missing Mcp-Method
    let mut req = modern_http_post(json!(1), "tools/list", None, json!({}));
    req.headers.retain(|(k, _)| k != "mcp-method");
    let (resp, body) = send_http_json(&facade, &ctx, req).await;
    assert_eq!(resp.status, 400);
    assert_eq!(body["error"]["code"], -32020);

    // Mismatched MCP-Protocol-Version header vs _meta
    let mut req2 = modern_http_post(json!(2), "tools/list", None, json!({}));
    for (k, v) in &mut req2.headers {
        if k == "mcp-protocol-version" {
            *v = "2025-06-18".to_owned();
        }
    }
    let (resp2, body2) = send_http_json(&facade, &ctx, req2).await;
    assert_eq!(resp2.status, 400);
    assert_eq!(body2["error"]["code"], -32020);
}

// spec: CMP-001
#[tokio::test]
async fn hostile_mcp_method_header_never_overrides_body_method() {
    // Engine panics if called: if Mcp-Method ("tools/list") were trusted over body ("tools/call" gap_query)
    // OR if body ("nonexistent/method") were overridden by Mcp-Method ("tools/list"), this catches it!
    let engine = FakeEngine::panicking();
    let facade = McpFacade::builder(engine as Arc<dyn GraphqlEngine>)
        .build()
        .expect("facade");
    let snap = compose_two_branch_snapshot(true, 1);
    let ctx = make_ctx(snap, "agent:1", "acme-corp", &[]);

    // Case 1: Header claims benign "tools/list", body is "tools/call" -> MUST reject -32020, never return tools/list and never call engine!
    let mut req = modern_http_post(
        json!(1),
        "tools/call",
        Some("gap_query"),
        json!({
            "name": "gap_query",
            "arguments": { "query": "{ billing_invoice(id: \"1\") { id } }" }
        }),
    );
    for (k, v) in &mut req.headers {
        if k == "mcp-method" {
            *v = "tools/list".to_owned();
        }
    }
    let (resp, body) = send_http_json(&facade, &ctx, req).await;
    assert_eq!(resp.status, 400);
    assert_eq!(body["error"]["code"], -32020);
    assert!(body.get("result").is_none());

    // Case 2: Header claims "server/discover" AND body has method "unknown/rpc" with matching header -> must execute body ("unknown/rpc" -> -32601), proving body decides dispatch.
    let req2 = modern_http_post(json!(2), "unknown/rpc", None, json!({}));
    let (resp2, body2) = send_http_json(&facade, &ctx, req2).await;
    assert_eq!(resp2.status, 404);
    assert_eq!(body2["error"]["code"], -32601);
}

// spec: CMP-001
#[tokio::test]
async fn missing_or_mismatched_mcp_name_header_on_tools_call_and_resources_read_returns_minus_32020()
{
    let (facade, _) = default_facade();
    let snap = compose_two_branch_snapshot(true, 1);
    let ctx = make_ctx(snap, "agent:1", "acme-corp", &[]);

    // Missing Mcp-Name on tools/call
    let req1 = modern_http_post(
        json!(1),
        "tools/call",
        None,
        json!({ "name": "gap_describe", "arguments": { "overview": true } }),
    );
    let (resp1, body1) = send_http_json(&facade, &ctx, req1).await;
    assert_eq!(resp1.status, 400);
    assert_eq!(body1["error"]["code"], -32020);

    // Wrong Mcp-Name on tools/call
    let req2 = modern_http_post(
        json!(2),
        "tools/call",
        Some("gap_search"),
        json!({ "name": "gap_describe", "arguments": { "overview": true } }),
    );
    let (resp2, body2) = send_http_json(&facade, &ctx, req2).await;
    assert_eq!(resp2.status, 400);
    assert_eq!(body2["error"]["code"], -32020);

    // Missing Mcp-Name on resources/read
    let req3 = modern_http_post(
        json!(3),
        "resources/read",
        None,
        json!({ "uri": "gap://meta" }),
    );
    let (resp3, body3) = send_http_json(&facade, &ctx, req3).await;
    assert_eq!(resp3.status, 400);
    assert_eq!(body3["error"]["code"], -32020);

    // Wrong Mcp-Name on resources/read
    let req4 = modern_http_post(
        json!(4),
        "resources/read",
        Some("gap://operations"),
        json!({ "uri": "gap://meta" }),
    );
    let (resp4, body4) = send_http_json(&facade, &ctx, req4).await;
    assert_eq!(resp4.status, 400);
    assert_eq!(body4["error"]["code"], -32020);
}

// spec: CMP-001
#[tokio::test]
async fn base64_sentinel_mcp_name_decoded_and_verified() {
    let (facade, _) = default_facade();
    let snap = compose_two_branch_snapshot(true, 1);
    let ctx = make_ctx(snap, "agent:1", "acme-corp", &[]);

    // "gap://meta" in RFC 4648 Base64 is "Z2FwOi8vbWV0YQ=="
    let req = modern_http_post(
        json!(1),
        "resources/read",
        Some("=?base64?Z2FwOi8vbWV0YQ==?="),
        json!({ "uri": "gap://meta" }),
    );
    let (resp, body) = send_http_json(&facade, &ctx, req).await;
    assert_eq!(resp.status, 200);
    assert_eq!(body["result"]["resultType"], "complete");

    // Malformed base64 inside sentinel -> 400 -32020
    let bad = modern_http_post(
        json!(2),
        "resources/read",
        Some("=?base64?%%%not_base64%%%?="),
        json!({ "uri": "gap://meta" }),
    );
    let (resp_bad, body_bad) = send_http_json(&facade, &ctx, bad).await;
    assert_eq!(resp_bad.status, 400);
    assert_eq!(body_bad["error"]["code"], -32020);
}

// spec: CMP-001
#[tokio::test]
async fn wrong_accept_and_content_type_headers_rejected() {
    let (facade, _) = default_facade();
    let snap = compose_two_branch_snapshot(true, 1);
    let ctx = make_ctx(snap, "agent:1", "acme-corp", &[]);

    // Missing text/event-stream in Accept -> 406
    let mut req = modern_http_post(json!(1), "tools/list", None, json!({}));
    for (k, v) in &mut req.headers {
        if k == "accept" {
            *v = "application/json".to_owned();
        }
    }
    let (resp, _) = send_http_json(&facade, &ctx, req).await;
    assert_eq!(resp.status, 406);

    // Wrong Content-Type -> 415
    let mut req2 = modern_http_post(json!(2), "tools/list", None, json!({}));
    for (k, v) in &mut req2.headers {
        if k == "content-type" {
            *v = "text/plain".to_owned();
        }
    }
    let (resp2, _) = send_http_json(&facade, &ctx, req2).await;
    assert_eq!(resp2.status, 415);
}

// spec: CMP-001
#[tokio::test]
async fn get_and_unknown_delete_return_405_with_allow_post() {
    let (facade, _) = default_facade();
    let snap = compose_two_branch_snapshot(true, 1);
    let ctx = make_ctx(snap, "agent:1", "acme-corp", &[]);

    for method in ["GET", "DELETE", "PUT"] {
        let req = HttpExchangeRequest {
            method: method.to_owned(),
            path: "/mcp".to_owned(),
            query: None,
            headers: Vec::new(),
            body: Vec::new(),
        };
        let resp = facade.handle(&ctx, req).await.expect("handle");
        assert_eq!(resp.status, 405, "method {method} should return 405");
        let allow = resp
            .headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("allow"))
            .map(|(_, v)| v.as_str());
        assert_eq!(allow, Some("POST"));
    }
}

// spec: CMP-001
#[tokio::test]
async fn disallowed_origin_returns_403_while_loopback_and_configured_origins_pass() {
    let (facade, _) = default_facade();
    let snap = compose_two_branch_snapshot(true, 1);
    let ctx = make_ctx(snap, "agent:1", "acme-corp", &[]);

    for bad_origin in [
        "https://attacker.example",
        "http://localhost.attacker.example",
        "http://127.0.0.1.evil.com",
        "null",
    ] {
        let mut req = modern_http_post(json!(1), "tools/list", None, json!({}));
        req.headers
            .push(("origin".to_owned(), bad_origin.to_owned()));
        let resp = facade.handle(&ctx, req).await.expect("handle");
        assert_eq!(
            resp.status, 403,
            "untrusted origin `{bad_origin}` must return 403"
        );
    }

    for good_origin in [
        "http://localhost:8080",
        "http://127.0.0.1:3000",
        "http://[::1]:8080",
    ] {
        let mut req = modern_http_post(json!(2), "tools/list", None, json!({}));
        req.headers
            .push(("origin".to_owned(), good_origin.to_owned()));
        let resp = facade.handle(&ctx, req).await.expect("handle");
        assert_eq!(
            resp.status, 200,
            "loopback origin `{good_origin}` must succeed"
        );
    }

    // Explicit allowed_origins overrides loopback default
    let custom_facade = McpFacade::builder(FakeEngine::two_branch() as Arc<dyn GraphqlEngine>)
        .config(FacadeConfig {
            allowed_origins: vec!["https://console.acme.example".to_owned()],
            ..FacadeConfig::default()
        })
        .build()
        .expect("build");

    let mut allowed_req = modern_http_post(json!(3), "tools/list", None, json!({}));
    allowed_req.headers.push((
        "origin".to_owned(),
        "https://console.acme.example".to_owned(),
    ));
    assert_eq!(
        custom_facade
            .handle(&ctx, allowed_req)
            .await
            .expect("ok")
            .status,
        200
    );

    let mut blocked_loopback = modern_http_post(json!(4), "tools/list", None, json!({}));
    blocked_loopback
        .headers
        .push(("origin".to_owned(), "http://localhost:8080".to_owned()));
    assert_eq!(
        custom_facade
            .handle(&ctx, blocked_loopback)
            .await
            .expect("ok")
            .status,
        403
    );
}

// spec: CMP-001
#[tokio::test]
async fn oversized_http_body_returns_413() {
    let facade = McpFacade::builder(FakeEngine::two_branch() as Arc<dyn GraphqlEngine>)
        .config(FacadeConfig {
            max_request_bytes: 128,
            ..FacadeConfig::default()
        })
        .build()
        .expect("build");
    let snap = compose_two_branch_snapshot(true, 1);
    let ctx = make_ctx(snap, "agent:1", "acme-corp", &[]);

    let mut req = modern_http_post(
        json!(1),
        "tools/list",
        None,
        json!({ "padding": "x".repeat(256) }),
    );
    assert!(req.body.len() > 128);
    let (resp, body) = send_http_json(&facade, &ctx, req).await;
    assert_eq!(resp.status, 413);
    assert_eq!(body["error"]["code"], -32600);
    req = modern_http_post(json!(2), "tools/list", None, json!({}));
    let _ = req;
}

// spec: CMP-001
#[tokio::test]
async fn jsonrpc_batch_array_refused_with_minus_32600() {
    let (facade, _) = default_facade();
    let snap = compose_two_branch_snapshot(true, 1);
    let ctx = make_ctx(snap, "agent:1", "acme-corp", &[]);

    let batch_body = serde_json::to_vec(&json!([
        {
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/list",
            "params": with_modern_meta(json!({}))
        }
    ]))
    .expect("json");
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
            (
                "mcp-protocol-version".to_owned(),
                "2026-07-28".to_owned(),
            ),
            ("mcp-method".to_owned(), "tools/list".to_owned()),
        ],
        body: batch_body,
    };
    let (resp, body) = send_http_json(&facade, &ctx, req).await;
    assert_eq!(resp.status, 400);
    assert_eq!(body["error"]["code"], -32600);

    // Also over stdio
    let out = run_stdio_session(
        facade,
        ctx,
        &[r#"[{"jsonrpc":"2.0","id":1,"method":"tools/list"}]"#.to_owned()],
    )
    .await;
    assert_eq!(out.len(), 1);
    assert_eq!(out[0]["error"]["code"], -32600);
}

// spec: CMP-001
#[tokio::test]
async fn malformed_json_returns_minus_32700_and_unknown_method_returns_minus_32601() {
    let (facade, _) = default_facade();
    let snap = compose_two_branch_snapshot(true, 1);
    let ctx = make_ctx(snap, "agent:1", "acme-corp", &[]);

    let bad_req = HttpExchangeRequest {
        method: "POST".to_owned(),
        path: "/mcp".to_owned(),
        query: None,
        headers: vec![
            ("content-type".to_owned(), "application/json".to_owned()),
            (
                "accept".to_owned(),
                "application/json, text/event-stream".to_owned(),
            ),
        ],
        body: b"{not valid json".to_vec(),
    };
    let (resp, body) = send_http_json(&facade, &ctx, bad_req).await;
    assert_eq!(resp.status, 400);
    assert_eq!(body["error"]["code"], -32700);

    let unknown_req = modern_http_post(json!(7), "prompts/get", Some("p1"), json!({ "name": "p1" }));
    let (resp2, body2) = send_http_json(&facade, &ctx, unknown_req).await;
    assert_eq!(resp2.status, 404);
    assert_eq!(body2["error"]["code"], -32601);
}

// spec: CMP-001
#[tokio::test]
async fn legacy_session_evicts_oldest_at_max_sessions() {
    let facade = McpFacade::builder(FakeEngine::two_branch() as Arc<dyn GraphqlEngine>)
        .config(FacadeConfig {
            max_sessions: 2,
            ..FacadeConfig::default()
        })
        .build()
        .expect("build");
    let snap = compose_two_branch_snapshot(true, 1);
    let ctx = make_ctx(snap, "agent:1", "acme-corp", &[]);

    let init_req = |id: i64| HttpExchangeRequest {
        method: "POST".to_owned(),
        path: "/mcp".to_owned(),
        query: None,
        headers: vec![
            ("content-type".to_owned(), "application/json".to_owned()),
            (
                "accept".to_owned(),
                "application/json, text/event-stream".to_owned(),
            ),
        ],
        body: serde_json::to_vec(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-11-25",
                "capabilities": {},
                "clientInfo": { "name": "Legacy", "version": "1.0" }
            }
        }))
        .expect("json"),
    };

    let mut session_ids = Vec::new();
    for i in 1..=3 {
        let (resp, _) = send_http_json(&facade, &ctx, init_req(i)).await;
        assert_eq!(resp.status, 200);
        let sid = resp
            .headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("mcp-session-id"))
            .map(|(_, v)| v.clone())
            .expect("mcp-session-id header");
        session_ids.push(sid);
    }

    let list_with_sid = |sid: &str| HttpExchangeRequest {
        method: "POST".to_owned(),
        path: "/mcp".to_owned(),
        query: None,
        headers: vec![
            ("content-type".to_owned(), "application/json".to_owned()),
            (
                "accept".to_owned(),
                "application/json, text/event-stream".to_owned(),
            ),
            ("mcp-session-id".to_owned(), sid.to_owned()),
        ],
        body: serde_json::to_vec(&json!({
            "jsonrpc": "2.0",
            "id": 100,
            "method": "tools/list",
            "params": {}
        }))
        .expect("json"),
    };

    // Oldest session (session_ids[0]) must have been evicted when session 3 was created!
    let r0 = facade
        .handle(&ctx, list_with_sid(&session_ids[0]))
        .await
        .expect("handle");
    assert_eq!(r0.status, 404, "evicted oldest session must return 404");

    let r1 = facade
        .handle(&ctx, list_with_sid(&session_ids[1]))
        .await
        .expect("handle");
    assert_eq!(r1.status, 200);

    let r2 = facade
        .handle(&ctx, list_with_sid(&session_ids[2]))
        .await
        .expect("handle");
    assert_eq!(r2.status, 200);
}

// spec: CMP-001
#[tokio::test(start_paused = true)]
async fn legacy_session_expires_after_idle_ttl_with_paused_time() {
    let facade = McpFacade::builder(FakeEngine::two_branch() as Arc<dyn GraphqlEngine>)
        .config(FacadeConfig {
            session_idle_ttl: Duration::from_secs(60),
            ..FacadeConfig::default()
        })
        .build()
        .expect("build");
    let snap = compose_two_branch_snapshot(true, 1);
    let ctx = make_ctx(snap, "agent:1", "acme-corp", &[]);

    let init = HttpExchangeRequest {
        method: "POST".to_owned(),
        path: "/mcp".to_owned(),
        query: None,
        headers: vec![
            ("content-type".to_owned(), "application/json".to_owned()),
            (
                "accept".to_owned(),
                "application/json, text/event-stream".to_owned(),
            ),
        ],
        body: serde_json::to_vec(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-11-25",
                "capabilities": {},
                "clientInfo": { "name": "Legacy", "version": "1.0" }
            }
        }))
        .expect("json"),
    };
    let (resp, _) = send_http_json(&facade, &ctx, init).await;
    let sid = resp
        .headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("mcp-session-id"))
        .map(|(_, v)| v.clone())
        .expect("sid");

    tokio::time::advance(Duration::from_secs(61)).await;

    let follow_up = HttpExchangeRequest {
        method: "POST".to_owned(),
        path: "/mcp".to_owned(),
        query: None,
        headers: vec![
            ("content-type".to_owned(), "application/json".to_owned()),
            (
                "accept".to_owned(),
                "application/json, text/event-stream".to_owned(),
            ),
            ("mcp-session-id".to_owned(), sid),
        ],
        body: serde_json::to_vec(&json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/list",
            "params": {}
        }))
        .expect("json"),
    };
    let expired_resp = facade.handle(&ctx, follow_up).await.expect("handle");
    assert_eq!(expired_resp.status, 404, "expired session must return 404");
}

// spec: CMP-001
#[tokio::test]
async fn stdio_framing_partial_writes_multiple_lines_oversized_line_and_eof() {
    let facade = McpFacade::builder(FakeEngine::two_branch() as Arc<dyn GraphqlEngine>)
        .config(FacadeConfig {
            max_request_bytes: 512,
            ..FacadeConfig::default()
        })
        .build()
        .expect("build");
    let snap = compose_two_branch_snapshot(true, 1);
    let ctx = make_ctx(snap, "agent:1", "acme-corp", &[]);

    let (mut client_write, server_read) = tokio::io::duplex(16 * 1024);
    let (server_write, mut client_read) = tokio::io::duplex(16 * 1024);
    let server = StdioServer::new(facade, Arc::new(move || Ok(ctx.clone())));
    let task = tokio::spawn(async move {
        server
            .serve(BufReader::new(server_read), server_write)
            .await
    });

    let msg1 = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "server/discover",
        "params": with_modern_meta(json!({}))
    })
    .to_string();
    let msg2 = json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/list",
        "params": with_modern_meta(json!({}))
    })
    .to_string();

    // 1. Partial line split across two writes
    let mid = msg1.len() / 2;
    client_write
        .write_all(&msg1.as_bytes()[..mid])
        .await
        .expect("w1");
    client_write
        .write_all(&msg1.as_bytes()[mid..])
        .await
        .expect("w2");
    client_write.write_all(b"\n").await.expect("nl");

    // 2. Oversized line (> 512 bytes) followed by two valid requests in one write
    let oversized = format!("{{\"jsonrpc\":\"2.0\",\"id\":99,\"pad\":\"{}\"}}\n", "a".repeat(600));
    let combined = format!("{oversized}{msg2}\n{msg1}\n");
    client_write
        .write_all(combined.as_bytes())
        .await
        .expect("combined");
    drop(client_write);

    task.await.expect("join").expect("serve ok");

    let mut raw = String::new();
    client_read.read_to_string(&mut raw).await.expect("read");
    let responses: Vec<Value> = raw
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str::<Value>(l).expect("json line"))
        .collect();
    assert_eq!(responses.len(), 4);
    assert_eq!(responses[0]["id"], 1);
    assert_eq!(responses[0]["result"]["resultType"], "complete");
    assert_eq!(responses[1]["error"]["code"], -32600);
    assert_eq!(responses[2]["id"], 2);
    assert_eq!(responses[3]["id"], 1);
}

// spec: INV-15
#[tokio::test]
async fn inv_15_internal_failure_returns_opaque_minus_32603_with_correlation_id_and_no_leaks() {
    let snap = compose_two_branch_snapshot(true, 1);
    let (facade, _) = default_facade();

    // Stdio ctx_factory failure containing sensitive path/secret details -> must emit -32603 opaque message + correlationId without leaking secret!
    let failing_server = StdioServer::new(
        facade,
        Arc::new(|| {
            Err(GapError::new(
                ErrorCode::Internal,
                "db crash at /etc/secret/db.key token=Bearer super-secret-xyz",
            )
            .with_correlation_id("corr-secret-test-1"))
        }),
    );
    let (mut cw, sr) = tokio::io::duplex(4096);
    let (sw, mut cr) = tokio::io::duplex(4096);
    let handle = tokio::spawn(async move { failing_server.serve(BufReader::new(sr), sw).await });
    let line = format!(
        "{}\n",
        json!({
            "jsonrpc": "2.0",
            "id": 77,
            "method": "tools/list",
            "params": with_modern_meta(json!({}))
        })
    );
    cw.write_all(line.as_bytes()).await.expect("write");
    drop(cw);
    handle.await.expect("join").expect("serve");
    let mut out = String::new();
    cr.read_to_string(&mut out).await.expect("read");
    let parsed: Value = serde_json::from_str(out.trim()).expect("json");
    assert_eq!(parsed["id"], 77);
    assert_eq!(parsed["error"]["code"], -32603);
    assert_eq!(
        parsed["error"]["data"]["correlationId"],
        "corr-secret-test-1"
    );
    let serialized = parsed.to_string();
    assert!(!serialized.contains("/etc/secret"));
    assert!(!serialized.contains("super-secret-xyz"));
    let _ = snap;
}

// spec: INV-01, CMP-001
#[tokio::test(start_paused = true)]
async fn inv_01_200_concurrent_stateless_requests_across_published_generations() {
    let g1 = compose_two_branch_snapshot(true, 1);
    let g2 = compose_two_branch_snapshot(true, 2);
    let handle = Arc::new(SnapshotHandle::new((*g1).clone()));

    let engine = FakeEngine::with_handler(|ctx, _req| {
        let mut ext = serde_json::Map::new();
        ext.insert(
            "gap".to_owned(),
            json!({ "generation": ctx.snapshot.generation }),
        );
        GraphqlResponse {
            data: Some(json!({
                "billing_invoice": {
                    "id": format!("gen-{}", ctx.snapshot.generation),
                    "total": ctx.snapshot.generation
                }
            })),
            errors: Vec::new(),
            extensions: ext,
        }
    });
    let facade = McpFacade::builder(engine as Arc<dyn GraphqlEngine>)
        .build()
        .expect("facade");

    let mut futs = Vec::with_capacity(200);
    for idx in 0..200_u64 {
        if idx == 100 {
            handle.publish((*g2).clone()).expect("publish generation 2");
        }
        let pinned = handle.pin();
        let expected_gen = pinned.generation;
        let facade_cl = facade.clone();
        futs.push(async move {
            let ctx = make_ctx(pinned, &format!("agent:{idx}"), "acme-corp", &["read:all"]);
            let req = if idx % 2 == 0 {
                modern_http_post(
                    json!(idx),
                    "tools/call",
                    Some("gap_query"),
                    json!({
                        "name": "gap_query",
                        "arguments": {
                            "query": "{ billing_invoice(id: \"1\") { id total } }"
                        }
                    }),
                )
            } else {
                modern_http_post(
                    json!(idx),
                    "tools/call",
                    Some("gap_describe"),
                    json!({
                        "name": "gap_describe",
                        "arguments": { "overview": true }
                    }),
                )
            };
            let (resp, body) = send_http_json(&facade_cl, &ctx, req).await;
            assert_eq!(resp.status, 200);
            assert_eq!(body["id"], json!(idx));
            if idx % 2 == 0 {
                assert_eq!(
                    body["result"]["structuredContent"]["extensions"]["gap"]["generation"],
                    json!(expected_gen)
                );
            } else {
                assert_eq!(
                    body["result"]["structuredContent"]["data"]["overview"]["generation"],
                    json!(expected_gen)
                );
            }
        });
    }
    join_all(futs).await;
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 64,
        failure_persistence: None,
        ..ProptestConfig::default()
    })]

    // spec: CMP-001, INV-15
    #[test]
    fn prop_arbitrary_http_payloads_never_panic(
        method in "(POST|GET|DELETE|PUT)",
        mcp_method in "[ -~]{0,32}",
        raw_body in proptest::collection::vec(any::<u8>(), 0..512),
    ) {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("rt");
        let (facade, _) = default_facade();
        let snap = compose_two_branch_snapshot(false, 1);
        let ctx = make_ctx(snap, "prop:agent", "acme-corp", &[]);
        let req = HttpExchangeRequest {
            method,
            path: "/mcp".to_owned(),
            query: None,
            headers: vec![
                ("content-type".to_owned(), "application/json".to_owned()),
                ("accept".to_owned(), "application/json, text/event-stream".to_owned()),
                ("mcp-protocol-version".to_owned(), "2026-07-28".to_owned()),
                ("mcp-method".to_owned(), mcp_method),
            ],
            body: raw_body,
        };
        let resp = rt.block_on(facade.handle(&ctx, req)).expect("handle never errors");
        prop_assert!((200..600).contains(&resp.status));
    }
}
