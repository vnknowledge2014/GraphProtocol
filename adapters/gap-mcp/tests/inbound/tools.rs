//! Static five-tool schema contract, strict validation, read-only/mutation guards, `gap_task`, truncation, and `isError` tests (`CMP-002`..`004`, `GQL-022`, `SEC-080`).

use std::sync::Arc;

use gap_core::error::{ErrorCode, GapError, Suggestion};
use gap_core::traits::{GraphqlEngine, GraphqlResponse, HttpModule};
use gap_mcp::inbound::McpFacade;
use serde_json::{Value, json};

use super::support::{
    FakeEngine, compose_two_branch_snapshot, default_facade, make_ctx, modern_http_post,
    send_http_json, workspace_root,
};

// spec: CMP-002, CMP-003, SEC-080
#[tokio::test]
async fn tools_list_matches_spec_09_contract_fixture() {
    let expected_raw = std::fs::read_to_string(
        workspace_root().join("conformance/mcp/tools.expected.json"),
    )
    .expect("read tools.expected.json");
    let expected: Value = serde_json::from_str(&expected_raw).expect("parse tools.expected.json");

    let (facade, _) = default_facade();
    let snap = compose_two_branch_snapshot(true, 1);
    let ctx = make_ctx(snap, "agent:1", "acme-corp", &[]);

    let req = modern_http_post(json!(1), "tools/list", None, json!({}));
    let (resp, body) = send_http_json(&facade, &ctx, req).await;
    assert_eq!(resp.status, 200);
    assert_eq!(body["result"]["tools"], expected["tools"]);
    assert_eq!(body["result"]["resultType"], "complete");
    assert_eq!(body["result"]["cacheScope"], "public");
    assert!(body["result"]["ttlMs"].as_u64().unwrap_or(0) > 0);
}

// spec: CMP-001, CMP-002, CMP-004
#[tokio::test]
async fn tools_list_identical_across_principals_tenants_and_connections() {
    let (facade, _) = default_facade();
    let snap_a = compose_two_branch_snapshot(true, 1);
    let snap_b = compose_two_branch_snapshot(false, 9);
    let ctx_a = make_ctx(snap_a, "user:alice", "acme-corp", &["read:all", "tasks:decide"]);
    let ctx_b = make_ctx(snap_b, "service:minimal", "acme-corp", &[]);

    let req1 = modern_http_post(json!(1), "tools/list", None, json!({}));
    let req2 = modern_http_post(json!(1), "tools/list", None, json!({}));

    let resp_a = facade.handle(&ctx_a, req1).await.expect("handle a");
    let resp_b = facade.handle(&ctx_b, req2).await.expect("handle b");
    assert_eq!(resp_a.status, 200);
    assert_eq!(
        resp_a.body, resp_b.body,
        "tools/list bytes must be identical across principals and snapshots"
    );
}

// spec: CMP-002
#[tokio::test]
async fn unknown_tool_name_and_strict_argument_validation_return_minus_32602_with_path() {
    let (facade, _) = default_facade();
    let snap = compose_two_branch_snapshot(true, 1);
    let ctx = make_ctx(snap, "agent:1", "acme-corp", &[]);

    // Unknown tool -> -32602
    let unknown = modern_http_post(
        json!(1),
        "tools/call",
        Some("gap_nonexistent"),
        json!({ "name": "gap_nonexistent", "arguments": {} }),
    );
    let (r_unk, b_unk) = send_http_json(&facade, &ctx, unknown).await;
    assert_eq!(r_unk.status, 400);
    assert_eq!(b_unk["error"]["code"], -32602);

    let bad_cases: Vec<(&str, Value, &str)> = vec![
        (
            "gap_search",
            json!({ "query": "invoice", "limit": 0 }),
            "arguments.limit",
        ),
        (
            "gap_search",
            json!({ "query": "invoice", "limit": 11 }),
            "arguments.limit",
        ),
        (
            "gap_search",
            json!({ "query": "invoice", "kinds": ["bogus_kind"] }),
            "arguments.kinds[0]",
        ),
        (
            "gap_search",
            json!({ "query": "invoice", "unknown_flag": true }),
            "arguments.unknown_flag",
        ),
        (
            "gap_describe",
            json!({ "depth": 4 }),
            "arguments.depth",
        ),
        (
            "gap_query",
            json!({ "variables": {} }),
            "arguments.query",
        ),
        (
            "gap_query",
            json!({ "query": "{ __typename }", "budget": { "tokens": 0 } }),
            "arguments.budget.tokens",
        ),
        (
            "gap_query",
            json!({ "query": "{ __typename }", "budget": { "extra": 1 } }),
            "arguments.budget.extra",
        ),
        (
            "gap_task",
            json!({ "action": "explode" }),
            "arguments.action",
        ),
    ];

    for (idx, (tool, args, expected_path)) in bad_cases.into_iter().enumerate() {
        let req = modern_http_post(
            json!(idx + 10),
            "tools/call",
            Some(tool),
            json!({ "name": tool, "arguments": args }),
        );
        let (resp, body) = send_http_json(&facade, &ctx, req).await;
        assert_eq!(resp.status, 400, "case {tool} {expected_path} must return 400");
        assert_eq!(body["error"]["code"], -32602);
        let msg = body["error"]["message"].as_str().unwrap_or("");
        assert!(
            msg.contains(expected_path),
            "expected `-32602` message `{msg}` to mention offending path `{expected_path}`"
        );
    }
}

// spec: CMP-002, CMP-004
#[tokio::test]
async fn gap_query_rejects_mutation_subscription_and_submit_task_before_calling_engine() {
    let panicking = FakeEngine::panicking();
    let facade = McpFacade::builder(Arc::clone(&panicking) as Arc<dyn GraphqlEngine>)
        .build()
        .expect("build");
    let snap = compose_two_branch_snapshot(true, 1);
    let ctx = make_ctx(snap, "agent:1", "acme-corp", &["read:all"]);

    for forbidden_doc in [
        "mutation { billing_createInvoice(amount: 10) { id } }",
        "subscription { gapTaskEvents(taskId: \"t1\") { seq } }",
        "query { gap { submitTask(input: { strategy: SINGLE }) { id } } }",
    ] {
        let req = modern_http_post(
            json!(1),
            "tools/call",
            Some("gap_query"),
            json!({
                "name": "gap_query",
                "arguments": { "query": forbidden_doc }
            }),
        );
        let (resp, body) = send_http_json(&facade, &ctx, req).await;
        assert_eq!(resp.status, 200);
        assert_eq!(body["result"]["isError"], true);
        let err = &body["result"]["structuredContent"]["errors"][0];
        assert_eq!(err["extensions"]["code"], "GAP_OPERATION_NOT_ALLOWED");
        let combined = body["result"].to_string();
        assert!(
            combined.contains("gap_mutate"),
            "rejection must guide caller to gap_mutate: {combined}"
        );
    }
    assert!(panicking.recorded().is_empty());
}

// spec: CMP-002, CMP-004
#[tokio::test]
async fn gap_query_forwards_variables_operation_name_and_budget_extensions() {
    let (facade, engine) = default_facade();
    let snap = compose_two_branch_snapshot(true, 1);
    let ctx = make_ctx(snap, "agent:1", "acme-corp", &["read:all"]);

    let req = modern_http_post(
        json!(1),
        "tools/call",
        Some("gap_query"),
        json!({
            "name": "gap_query",
            "arguments": {
                "query": "query GetInv($id: ID!) { billing_invoice(id: $id) { id total } }",
                "operation_name": "GetInv",
                "variables": { "id": "inv-1" },
                "budget": { "tokens": 500, "ms": 2500 }
            }
        }),
    );
    let (resp, body) = send_http_json(&facade, &ctx, req).await;
    assert_eq!(resp.status, 200);
    assert_eq!(body["result"]["isError"], false);

    let calls = engine.recorded();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].generation, 1);
    assert_eq!(calls[0].tenant, "acme-corp");
    assert_eq!(calls[0].principal_id, "agent:1");
    assert_eq!(calls[0].request.operation_name.as_deref(), Some("GetInv"));
    assert_eq!(calls[0].request.variables.get("id"), Some(&json!("inv-1")));
    assert_eq!(
        calls[0].request.extensions["gap"]["deadlineMs"],
        json!(2500)
    );
    assert_eq!(
        calls[0].request.extensions["gap"]["budgetTokens"],
        json!(500)
    );
}

// spec: CMP-002, CMP-004
#[tokio::test]
async fn gap_mutate_rejects_query_before_engine_and_forwards_dry_run_and_cloned_idempotency_key() {
    let panicking = FakeEngine::panicking();
    let panic_facade = McpFacade::builder(panicking as Arc<dyn GraphqlEngine>)
        .build()
        .expect("build");
    let snap = compose_two_branch_snapshot(true, 1);
    let ctx = make_ctx(snap, "agent:1", "acme-corp", &["write:billing"]);

    // Query passed to gap_mutate -> refused before engine with GAP_OPERATION_NOT_ALLOWED naming gap_query
    let bad = modern_http_post(
        json!(1),
        "tools/call",
        Some("gap_mutate"),
        json!({
            "name": "gap_mutate",
            "arguments": { "query": "{ billing_invoice(id: \"1\") { id } }" }
        }),
    );
    let (_, bad_body) = send_http_json(&panic_facade, &ctx, bad).await;
    assert_eq!(bad_body["result"]["isError"], true);
    assert_eq!(
        bad_body["result"]["structuredContent"]["errors"][0]["extensions"]["code"],
        "GAP_OPERATION_NOT_ALLOWED"
    );
    assert!(bad_body["result"].to_string().contains("gap_query"));

    // Valid mutation forwards dry_run + idempotency_key on a cloned ctx without mutating original ctx
    let (facade, engine) = default_facade();
    assert!(ctx.idempotency_key.is_none());
    let ok_req = modern_http_post(
        json!(2),
        "tools/call",
        Some("gap_mutate"),
        json!({
            "name": "gap_mutate",
            "arguments": {
                "query": "mutation { billing_createInvoice(amount: 500) { id total } }",
                "dry_run": true,
                "idempotency_key": "idem-xyz-99"
            }
        }),
    );
    let (resp, body) = send_http_json(&facade, &ctx, ok_req).await;
    assert_eq!(resp.status, 200);
    assert_eq!(body["result"]["isError"], false);
    assert!(ctx.idempotency_key.is_none(), "caller ctx must stay untouched");

    let recorded = engine.recorded();
    assert_eq!(recorded.len(), 1);
    assert_eq!(recorded[0].idempotency_key.as_deref(), Some("idem-xyz-99"));
    assert_eq!(recorded[0].request.extensions["gap"]["dryRun"], json!(true));
}

// spec: CMP-002, CMP-004
#[tokio::test]
async fn gap_mutate_maps_input_required_extension_to_input_required_result_type() {
    let (facade, _) = default_facade();
    let snap = compose_two_branch_snapshot(true, 1);
    let ctx = make_ctx(snap, "agent:1", "acme-corp", &["write:billing"]);

    let req = modern_http_post(
        json!(1),
        "tools/call",
        Some("gap_mutate"),
        json!({
            "name": "gap_mutate",
            "arguments": {
                "query": "mutation { billing_closeAccount(id: \"acc-1\") }"
            }
        }),
    );
    let (resp, body) = send_http_json(&facade, &ctx, req).await;
    assert_eq!(resp.status, 200);
    assert_eq!(body["result"]["resultType"], "input_required");
    assert_eq!(body["result"]["isError"], false);
    let text = body["result"]["content"][0]["text"].as_str().unwrap_or("");
    assert!(text.contains("task-approve-1"));
    assert!(text.contains("Approve closing account acc-1"));
}

// spec: CMP-002, CMP-004
#[tokio::test]
async fn gap_task_dispatches_all_actions_and_enforces_tasks_decide_scope() {
    let (facade, _) = default_facade();
    let snap = compose_two_branch_snapshot(true, 1);
    let no_scope_ctx = make_ctx(Arc::clone(&snap), "agent:1", "acme-corp", &["read:all"]);
    let approver_ctx = make_ctx(snap, "human:approver", "acme-corp", &["tasks:decide"]);

    // 1. submit
    let submit_req = modern_http_post(
        json!(1),
        "tools/call",
        Some("gap_task"),
        json!({
            "name": "gap_task",
            "arguments": {
                "action": "submit",
                "input": { "goal": "sync-invoices" }
            }
        }),
    );
    let (_, submit_body) = send_http_json(&facade, &no_scope_ctx, submit_req).await;
    assert_eq!(submit_body["result"]["isError"], false);
    assert_eq!(
        submit_body["result"]["structuredContent"]["data"]["task"]["state"],
        "SUBMITTED"
    );

    // 2. list
    let list_req = modern_http_post(
        json!(2),
        "tools/call",
        Some("gap_task"),
        json!({ "name": "gap_task", "arguments": { "action": "list" } }),
    );
    let (_, list_body) = send_http_json(&facade, &no_scope_ctx, list_req).await;
    assert_eq!(list_body["result"]["isError"], false);
    assert!(
        list_body["result"]["structuredContent"]["data"]["tasks"]["nodes"]
            .as_array()
            .is_some_and(|a| a.len() >= 2)
    );

    // 3. decide WITHOUT tasks:decide -> GAP_FORBIDDEN (redacted message, never naming internal scope rules)
    let decide_denied = modern_http_post(
        json!(3),
        "tools/call",
        Some("gap_task"),
        json!({
            "name": "gap_task",
            "arguments": {
                "action": "decide",
                "id": "task-1",
                "decision": "approve"
            }
        }),
    );
    let (_, denied_body) = send_http_json(&facade, &no_scope_ctx, decide_denied).await;
    assert_eq!(denied_body["result"]["isError"], true);
    assert_eq!(
        denied_body["result"]["structuredContent"]["errors"][0]["extensions"]["code"],
        "GAP_FORBIDDEN"
    );

    // 4. decide WITH tasks:decide -> COMPLETED
    let decide_ok = modern_http_post(
        json!(4),
        "tools/call",
        Some("gap_task"),
        json!({
            "name": "gap_task",
            "arguments": {
                "action": "decide",
                "id": "task-1",
                "decision": "approve",
                "note": "approved"
            }
        }),
    );
    let (_, ok_body) = send_http_json(&facade, &approver_ctx, decide_ok).await;
    assert_eq!(ok_body["result"]["isError"], false);
    assert_eq!(
        ok_body["result"]["structuredContent"]["data"]["task"]["state"],
        "COMPLETED"
    );

    // 5. cancel
    let cancel_req = modern_http_post(
        json!(5),
        "tools/call",
        Some("gap_task"),
        json!({
            "name": "gap_task",
            "arguments": {
                "action": "cancel",
                "id": "task-1"
            }
        }),
    );
    let (_, cancel_body) = send_http_json(&facade, &no_scope_ctx, cancel_req).await;
    assert_eq!(cancel_body["result"]["isError"], false);
    assert_eq!(
        cancel_body["result"]["structuredContent"]["data"]["task"]["state"],
        "CANCELLED"
    );
}

// spec: CMP-004, GQL-022
#[tokio::test]
async fn budget_truncation_respects_utf8_multibyte_boundaries_and_preserves_structured_content() {
    // Multi-byte UTF-8 payload (4-byte emoji + 3-byte Vietnamese/CJK scalars)
    let multibyte = "🦀🚀✨Xin chào thế giới — こんにちは世界 — ".repeat(8);
    let mb_clone = multibyte.clone();
    let engine = FakeEngine::with_handler(move |_ctx, _req| GraphqlResponse {
        data: Some(json!({
            "billing_invoice": {
                "id": mb_clone,
                "total": 99
            }
        })),
        errors: Vec::new(),
        extensions: serde_json::Map::new(),
    });
    let facade = McpFacade::builder(engine as Arc<dyn GraphqlEngine>)
        .build()
        .expect("build");
    let snap = compose_two_branch_snapshot(false, 1);
    let ctx = make_ctx(snap, "agent:1", "acme-corp", &["read:all"]);

    // Test across multiple tight token budgets to hit every possible byte offset mod 4 inside multi-byte scalars
    for tokens in [5_i64, 6, 7, 8, 9, 10, 11, 12, 15, 20] {
        let req = modern_http_post(
            json!(tokens),
            "tools/call",
            Some("gap_query"),
            json!({
                "name": "gap_query",
                "arguments": {
                    "query": "{ billing_invoice(id: \"1\") { id total } }",
                    "budget": { "tokens": tokens }
                }
            }),
        );
        let (resp, body) = send_http_json(&facade, &ctx, req).await;
        assert_eq!(resp.status, 200);
        let text = body["result"]["content"][0]["text"]
            .as_str()
            .expect("valid UTF-8 text string");
        assert!(
            text.contains(r#""truncated":true"#) && text.contains("use first/after or returnFields"),
            "budget {tokens} must append truncation notice, got: {text}"
        );
        // Full structuredContent is NEVER truncated
        assert_eq!(
            body["result"]["structuredContent"]["data"]["billing_invoice"]["id"],
            json!(multibyte)
        );
    }
}

// spec: CMP-004
#[tokio::test]
async fn is_error_true_only_when_errors_present_and_no_usable_data_preserving_error_extensions() {
    let snap = compose_two_branch_snapshot(true, 1);
    let ctx = make_ctx(snap, "agent:1", "acme-corp", &["read:all"]);

    // Partial success: usable data + error -> isError MUST be false!
    let partial_engine = FakeEngine::with_handler(|_ctx, _req| {
        let err = GapError::new(ErrorCode::BranchTimeout, "extnotes timed out")
            .with_retry(true, Some(250))
            .with_correlation_id("corr-partial-1")
            .with_suggestion(Suggestion {
                alternative: Some("billing_invoice".to_owned()),
                ..Suggestion::default()
            });
        GraphqlResponse {
            data: Some(json!({ "billing_invoice": { "id": "inv-1", "total": 10 }, "extnotes_note": null })),
            errors: vec![err],
            extensions: serde_json::Map::new(),
        }
    });
    let partial_facade = McpFacade::builder(partial_engine as Arc<dyn GraphqlEngine>)
        .build()
        .expect("build");
    let req1 = modern_http_post(
        json!(1),
        "tools/call",
        Some("gap_query"),
        json!({ "name": "gap_query", "arguments": { "query": "{ billing_invoice(id: \"1\") { id } }" } }),
    );
    let (_, body1) = send_http_json(&partial_facade, &ctx, req1).await;
    assert_eq!(
        body1["result"]["isError"], false,
        "partial data with errors must have isError = false"
    );
    let e0 = &body1["result"]["structuredContent"]["errors"][0];
    assert_eq!(e0["extensions"]["code"], "GAP_BRANCH_TIMEOUT");
    assert_eq!(e0["extensions"]["retry"]["retryable"], true);
    assert_eq!(e0["extensions"]["retry"]["afterMs"], 250);
    assert_eq!(e0["extensions"]["correlationId"], "corr-partial-1");

    // Total failure: errors + data == None -> isError MUST be true!
    let fail_engine = FakeEngine::with_handler(|_ctx, _req| GraphqlResponse {
        data: None,
        errors: vec![
            GapError::new(ErrorCode::RateLimited, "rate limited")
                .with_retry(true, Some(1000))
                .with_correlation_id("corr-fail-2"),
        ],
        extensions: serde_json::Map::new(),
    });
    let fail_facade = McpFacade::builder(fail_engine as Arc<dyn GraphqlEngine>)
        .build()
        .expect("build");
    let req2 = modern_http_post(
        json!(2),
        "tools/call",
        Some("gap_query"),
        json!({ "name": "gap_query", "arguments": { "query": "{ billing_invoice(id: \"1\") { id } }" } }),
    );
    let (_, body2) = send_http_json(&fail_facade, &ctx, req2).await;
    assert_eq!(body2["result"]["isError"], true);
    assert_eq!(
        body2["result"]["structuredContent"]["errors"][0]["extensions"]["code"],
        "GAP_RATE_LIMITED"
    );
    assert_eq!(
        body2["result"]["structuredContent"]["errors"][0]["extensions"]["correlationId"],
        "corr-fail-2"
    );
}
