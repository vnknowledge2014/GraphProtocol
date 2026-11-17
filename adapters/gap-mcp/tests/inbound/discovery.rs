//! Discovery (`gap_describe`, `gap_search`) and MCP resource tests (`CMP-002`, `CMP-005`, `GQL-011`, `GQL-020`..`024`, `GQL-061`, `GQL-062`, `INV-05`).

use std::sync::Arc;

use gap_core::context::RequestContext;
use gap_core::traits::{ElementKind, GraphqlEngine, HttpModule, SearchHit};
use gap_mcp::inbound::{
    ElementRef, FacadeConfig, HideT2, McpFacade, OperationCatalog, OperationInfo, Visibility,
};
use serde_json::json;

use super::support::{
    FakeEngine, FakeOperationCatalog, FakeTaskBackend, PopulatedElementSearch,
    billing_branch_input, compose_n_branch_snapshot, compose_snapshot, compose_two_branch_snapshot,
    default_facade, extnotes_t2_branch_input, make_ctx, make_op_id, modern_http_post,
    send_http_json,
};

// spec: CMP-002, GQL-023
#[tokio::test]
async fn gap_describe_overview_orders_by_op_count_caps_branches_and_clamps_synopsis_90_bytes() {
    let (facade, _) = default_facade();
    let snap = compose_two_branch_snapshot(true, 5);
    let ctx = make_ctx(snap, "agent:1", "acme-corp", &["read:all"]);

    let req = modern_http_post(
        json!(1),
        "tools/call",
        Some("gap_describe"),
        json!({ "name": "gap_describe", "arguments": { "overview": true } }),
    );
    let (resp, body) = send_http_json(&facade, &ctx, req).await;
    assert_eq!(resp.status, 200);
    let overview = &body["result"]["structuredContent"]["data"]["overview"];
    assert_eq!(overview["generation"], 5);
    let branches = overview["branches"].as_array().expect("branches array");
    assert_eq!(branches.len(), 2);
    // billing has 4 ops, extnotes has 2 ops -> billing must come first
    assert_eq!(branches[0]["id"], "billing");
    assert_eq!(branches[0]["operationCount"], 4);
    assert_eq!(branches[1]["id"], "extnotes");
    assert_eq!(branches[1]["operationCount"], 2);

    // Cap metagraph_branches = 3 on a 10-branch snapshot and check <= 90 byte synopsis
    let capped_facade = McpFacade::builder(FakeEngine::two_branch() as Arc<dyn GraphqlEngine>)
        .config(FacadeConfig {
            metagraph_branches: 3,
            ..FacadeConfig::default()
        })
        .build()
        .expect("build");
    let mut snap10_raw = (*compose_n_branch_snapshot(10)).clone();
    // Inject an un-clamped >90-byte synopsis containing a 4-byte emoji straddling byte 90
    // so facade-side UTF-8 boundary clamping (`<= 90` bytes) is directly exercised.
    let uids: Vec<_> = snap10_raw.branches.keys().copied().collect();
    for uid in uids {
        let mut mutated = (**snap10_raw
            .branches
            .get(&uid)
            .expect("uid from keys must exist"))
        .clone();
        mutated.synopsis = format!("{}🦀trailing-text-past-90-bytes", "a".repeat(88));
        snap10_raw.branches.insert(uid, Arc::new(mutated));
    }
    let ctx10 = make_ctx(Arc::new(snap10_raw), "agent:1", "acme-corp", &[]);
    let req10 = modern_http_post(
        json!(2),
        "tools/call",
        Some("gap_describe"),
        json!({ "name": "gap_describe", "arguments": { "overview": true } }),
    );
    let (_, body10) = send_http_json(&capped_facade, &ctx10, req10).await;
    let listed = body10["result"]["structuredContent"]["data"]["overview"]["branches"]
        .as_array()
        .expect("branches");
    assert_eq!(listed.len(), 3);
    for b in listed {
        let syn = b["synopsis"].as_str().expect("synopsis");
        assert!(syn.len() <= 90, "synopsis exceeded 90 bytes: {}", syn.len());
        assert_eq!(syn, "a".repeat(88));
    }
}

// spec: CMP-002, GQL-024, GQL-062, GQL-070
#[tokio::test]
async fn gap_describe_types_expands_depth_1_to_3_cycle_safe_and_suggests_visible_did_you_mean() {
    let (facade, _) = default_facade();
    let snap = compose_two_branch_snapshot(true, 1);
    let ctx = make_ctx(snap, "agent:1", "acme-corp", &["read:all"]);

    // Cyclic type billing_Node (parent: billing_Node, children: [billing_Node!]!, invoice: billing_Invoice)
    let call_depth = |depth: i64| {
        modern_http_post(
            json!(depth),
            "tools/call",
            Some("gap_describe"),
            json!({
                "name": "gap_describe",
                "arguments": { "types": ["billing_Node"], "depth": depth }
            }),
        )
    };

    let (_, d1) = send_http_json(&facade, &ctx, call_depth(1)).await;
    let sdl1 = d1["result"]["structuredContent"]["data"]["sdl"]
        .as_str()
        .expect("sdl d1");
    assert!(sdl1.contains("type billing_Node"));
    assert!(!sdl1.contains("type billing_Invoice"));

    let (_, d2) = send_http_json(&facade, &ctx, call_depth(2)).await;
    let sdl2 = d2["result"]["structuredContent"]["data"]["sdl"]
        .as_str()
        .expect("sdl d2");
    assert!(sdl2.contains("type billing_Node"));
    assert!(sdl2.contains("type billing_Invoice"));
    assert!(!sdl2.contains("type billing_LineItem"));

    let (_, d3) = send_http_json(&facade, &ctx, call_depth(3)).await;
    let sdl3 = d3["result"]["structuredContent"]["data"]["sdl"]
        .as_str()
        .expect("sdl d3");
    assert!(sdl3.contains("type billing_Node"));
    assert!(sdl3.contains("type billing_Invoice"));
    assert!(sdl3.contains("type billing_LineItem"));

    // Unknown type with edit-distance <= 2 typo -> GAP_UNKNOWN_FIELD + didYouMean
    let typo_req = modern_http_post(
        json!(99),
        "tools/call",
        Some("gap_describe"),
        json!({
            "name": "gap_describe",
            "arguments": { "types": ["billing_Invoic"] }
        }),
    );
    let (_, typo_body) = send_http_json(&facade, &ctx, typo_req).await;
    assert_eq!(typo_body["result"]["isError"], true);
    let err = &typo_body["result"]["structuredContent"]["errors"][0];
    assert_eq!(err["extensions"]["code"], "GAP_UNKNOWN_FIELD");
    let dym = err["extensions"]["suggestion"]["didYouMean"]
        .as_array()
        .expect("didYouMean array");
    assert!(dym.iter().any(|v| v == "billing_Invoice"));
}

// spec: CMP-002, GQL-061, GQL-062
#[tokio::test]
async fn gap_describe_operations_lists_catalog_through_visibility() {
    #[derive(Debug)]
    struct HideAdminOps;
    impl Visibility for HideAdminOps {
        fn visible(&self, _ctx: &RequestContext, element: ElementRef<'_>) -> bool {
            match element {
                ElementRef::Operation(op) => !op.name.starts_with("Admin"),
                _ => true,
            }
        }
    }

    let catalog = Arc::new(FakeOperationCatalog {
        ops: vec![
            OperationInfo {
                id: "sha256:1".to_owned(),
                name: "GetInvoice".to_owned(),
                description: "Public op".to_owned(),
                side_effect: gap_core::snapshot::SideEffectLevel::None,
            },
            OperationInfo {
                id: "sha256:2".to_owned(),
                name: "AdminPurge".to_owned(),
                description: "Hidden admin op".to_owned(),
                side_effect: gap_core::snapshot::SideEffectLevel::Irreversible,
            },
        ],
    });
    let facade = McpFacade::builder(FakeEngine::two_branch() as Arc<dyn GraphqlEngine>)
        .operations(catalog as Arc<dyn OperationCatalog>)
        .visibility(Arc::new(HideAdminOps))
        .build()
        .expect("build");

    let snap = compose_two_branch_snapshot(false, 1);
    let ctx = make_ctx(snap, "agent:1", "acme-corp", &[]);
    let req = modern_http_post(
        json!(1),
        "tools/call",
        Some("gap_describe"),
        json!({ "name": "gap_describe", "arguments": { "operations": true } }),
    );
    let (_, body) = send_http_json(&facade, &ctx, req).await;
    let ops = body["result"]["structuredContent"]["data"]["operations"]
        .as_array()
        .expect("ops");
    assert_eq!(ops.len(), 1);
    assert_eq!(ops[0]["name"], "GetInvoice");
}

// spec: INV-05, GQL-062, GQL-011
#[tokio::test]
async fn inv_05_hide_t2_makes_describe_and_search_byte_identical_to_snapshot_without_t2_branch() {
    let engine = FakeEngine::panicking() as Arc<dyn GraphqlEngine>;
    let facade_with_hide_t2 = McpFacade::builder(Arc::clone(&engine))
        .visibility(Arc::new(HideT2))
        .build()
        .expect("hide_t2 facade");
    let facade_clean = McpFacade::builder(engine).build().expect("clean facade");

    let snap_with_t2 = compose_snapshot(
        &[billing_branch_input(), extnotes_t2_branch_input()],
        1,
        "acme-corp",
    );
    let snap_without_t2 = compose_snapshot(&[billing_branch_input()], 1, "acme-corp");

    let ctx_with_t2 = make_ctx(snap_with_t2, "agent:1", "acme-corp", &["read:all"]);
    let ctx_without_t2 = make_ctx(snap_without_t2, "agent:1", "acme-corp", &["read:all"]);

    let probes = vec![
        (
            "gap_describe",
            json!({ "name": "gap_describe", "arguments": { "overview": true } }),
        ),
        (
            "gap_describe",
            json!({ "name": "gap_describe", "arguments": { "types": ["billing_Invoice"], "depth": 2 } }),
        ),
        // Typo close to hidden T2 type `extnotes_SecretNote`: must NOT leak `extnotes_SecretNote` in didYouMean!
        (
            "gap_describe",
            json!({ "name": "gap_describe", "arguments": { "types": ["extnotes_SecretNot"] } }),
        ),
        (
            "gap_search",
            json!({ "name": "gap_search", "arguments": { "query": "note secret memo" } }),
        ),
        (
            "gap_search",
            json!({ "name": "gap_search", "arguments": { "query": "invoice" } }),
        ),
    ];

    for (idx, (tool, params)) in probes.into_iter().enumerate() {
        let r1 = modern_http_post(json!(idx + 1), "tools/call", Some(tool), params.clone());
        let r2 = modern_http_post(json!(idx + 1), "tools/call", Some(tool), params);
        let resp_hidden = facade_with_hide_t2
            .handle(&ctx_with_t2, r1)
            .await
            .expect("handle hidden");
        let resp_clean = facade_clean
            .handle(&ctx_without_t2, r2)
            .await
            .expect("handle clean");

        let s_hidden = String::from_utf8(resp_hidden.body).expect("utf8");
        let s_clean = String::from_utf8(resp_clean.body).expect("utf8");
        assert!(
            !s_hidden.contains("\"extnotes\"") && !s_hidden.contains("extnotes_note"),
            "HideT2 leaked T2 branch or operation in probe {idx}: {s_hidden}"
        );
        assert!(
            !s_hidden.contains("SecretNote"),
            "HideT2 leaked T2 type name in probe {idx}: {s_hidden}"
        );
        assert_eq!(
            s_hidden, s_clean,
            "HideT2 output on [T0, T2] must be byte-identical to [T0] on probe {idx}"
        );
    }
}

// spec: CMP-002, GQL-011, GQL-020, GQL-021, GQL-022
#[tokio::test]
async fn gap_search_uses_element_search_when_populated_and_lexical_baseline_with_degraded_warning_when_empty()
 {
    let panicking = FakeEngine::panicking() as Arc<dyn GraphqlEngine>;
    let facade = McpFacade::builder(panicking).build().expect("facade");

    // 1. Default M1 snapshot (`EmptyElementSearch`, element_count() == 0) -> lexical baseline + GAP_DISCOVERY_DEGRADED warning
    let empty_idx_snap = compose_two_branch_snapshot(true, 1);
    let ctx_empty = make_ctx(Arc::clone(&empty_idx_snap), "agent:1", "acme-corp", &[]);
    let req = modern_http_post(
        json!(1),
        "tools/call",
        Some("gap_search"),
        json!({ "name": "gap_search", "arguments": { "query": "invoice" } }),
    );
    let (_, body_deg) = send_http_json(&facade, &ctx_empty, req).await;
    assert_eq!(body_deg["result"]["isError"], false);
    let sr = &body_deg["result"]["structuredContent"]["data"]["__search"];
    assert_eq!(sr["abstained"], false);
    assert!(!sr["hits"].as_array().expect("hits").is_empty());
    let warnings = body_deg["result"]["structuredContent"]["extensions"]["gap"]["warnings"]
        .as_array()
        .expect("degraded warning");
    assert!(
        warnings
            .iter()
            .any(|w| w["code"] == "GAP_DISCOVERY_DEGRADED")
    );

    // 2. Populated `ElementSearch` (`element_count() > 0`) -> uses `snapshot.index.search`, no degraded warning!
    let billing_uid = *empty_idx_snap
        .branches
        .keys()
        .find(|uid| {
            empty_idx_snap
                .branches
                .get(*uid)
                .is_some_and(|b| b.id.as_str() == "billing")
        })
        .expect("billing uid");
    let mut populated_snap = (*empty_idx_snap).clone();
    populated_snap.index = Arc::new(PopulatedElementSearch {
        hits: vec![SearchHit {
            element_id: "Query.billing_invoice".to_owned(),
            kind: ElementKind::Operation,
            branch_uid: Some(billing_uid),
            operation_id: Some(make_op_id("billing.billing_invoice")),
            lexical_score: 0.9,
            dense_score: Some(0.85),
            fused_score: 0.88,
        }],
    });
    let ctx_pop = make_ctx(Arc::new(populated_snap), "agent:1", "acme-corp", &[]);
    let req_pop = modern_http_post(
        json!(2),
        "tools/call",
        Some("gap_search"),
        json!({ "name": "gap_search", "arguments": { "query": "invoice" } }),
    );
    let (_, body_pop) = send_http_json(&facade, &ctx_pop, req_pop).await;
    let sr_pop = &body_pop["result"]["structuredContent"]["data"]["__search"];
    assert_eq!(sr_pop["abstained"], false);
    assert_eq!(sr_pop["hits"][0]["name"], "billing_invoice");
    assert_eq!(sr_pop["hits"][0]["branch"], "billing");
    let pop_warnings = &body_pop["result"]["structuredContent"]["extensions"]["gap"]["warnings"];
    assert!(
        pop_warnings.is_null() || pop_warnings.as_array().is_some_and(|a| a.is_empty()),
        "populated ElementSearch must not emit GAP_DISCOVERY_DEGRADED"
    );
}

// spec: GQL-020, GQL-021, GQL-022
#[tokio::test]
async fn gap_search_abstains_below_0_15_and_truncates_sdl_with_type_comment() {
    let (facade, _) = default_facade();
    let snap = compose_two_branch_snapshot(false, 1);
    let ctx = make_ctx(snap, "agent:1", "acme-corp", &[]);

    // Unrelated query -> no hit >= 0.15 -> abstained: true
    let abstain_req = modern_http_post(
        json!(1),
        "tools/call",
        Some("gap_search"),
        json!({
            "name": "gap_search",
            "arguments": { "query": "quantum flux capacitor" }
        }),
    );
    let (_, abstain_body) = send_http_json(&facade, &ctx, abstain_req).await;
    assert_eq!(
        abstain_body["result"]["structuredContent"]["data"]["__search"]["abstained"],
        true
    );

    // Tight budget_tokens on gap_search -> SDL fragment carries trailing `# truncated: use __type(name: "...")`
    let trunc_req = modern_http_post(
        json!(2),
        "tools/call",
        Some("gap_search"),
        json!({
            "name": "gap_search",
            "arguments": {
                "query": "invoice",
                "budget_tokens": 12
            }
        }),
    );
    let (_, trunc_body) = send_http_json(&facade, &ctx, trunc_req).await;
    let hits = trunc_body["result"]["structuredContent"]["data"]["__search"]["hits"]
        .as_array()
        .expect("hits");
    assert!(!hits.is_empty());
    let sdl = hits[0]["sdl"].as_str().expect("hit sdl");
    assert!(
        sdl.contains("# truncated: use __type(name: \""),
        "expected truncated SDL comment, got: {sdl}"
    );
}

// spec: CMP-005, GQL-062
#[tokio::test]
async fn resources_list_templates_read_and_subscribe_rejected() {
    #[derive(Debug)]
    struct PrincipalScopedVis;
    impl Visibility for PrincipalScopedVis {
        fn visible(&self, ctx: &RequestContext, element: ElementRef<'_>) -> bool {
            match element {
                ElementRef::Branch(b) if b.trust == gap_core::snapshot::TrustTier::T2 => {
                    ctx.principal.scopes.contains("see:t2")
                }
                ElementRef::Type {
                    branch: Some(b), ..
                }
                | ElementRef::Field {
                    branch: Some(b), ..
                } if b.trust == gap_core::snapshot::TrustTier::T2 => {
                    ctx.principal.scopes.contains("see:t2")
                }
                _ => true,
            }
        }
    }

    let facade = McpFacade::builder(FakeEngine::two_branch() as Arc<dyn GraphqlEngine>)
        .operations(FakeOperationCatalog::default_catalog())
        .tasks(FakeTaskBackend::seeded())
        .visibility(Arc::new(PrincipalScopedVis))
        .build()
        .expect("build");

    let snap = compose_two_branch_snapshot(true, 1);
    let ctx = make_ctx(snap, "agent:1", "acme-corp", &["read:all"]);

    // 1. resources/list
    let (_, list_body) = send_http_json(
        &facade,
        &ctx,
        modern_http_post(json!(1), "resources/list", None, json!({})),
    )
    .await;
    assert_eq!(list_body["result"]["resultType"], "complete");
    assert!(list_body["result"].get("ttlMs").is_some());
    assert!(list_body["result"].get("cacheScope").is_some());
    let res_uris: Vec<&str> = list_body["result"]["resources"]
        .as_array()
        .expect("resources")
        .iter()
        .filter_map(|r| r["uri"].as_str())
        .collect();
    assert_eq!(res_uris, vec!["gap://meta", "gap://operations"]);

    // 2. resources/templates/list
    let (_, tmpl_body) = send_http_json(
        &facade,
        &ctx,
        modern_http_post(json!(2), "resources/templates/list", None, json!({})),
    )
    .await;
    assert_eq!(tmpl_body["result"]["resultType"], "complete");
    let tmpl_uris: Vec<&str> = tmpl_body["result"]["resourceTemplates"]
        .as_array()
        .expect("templates")
        .iter()
        .filter_map(|t| t["uriTemplate"].as_str())
        .collect();
    assert_eq!(tmpl_uris, vec!["gap://schema/{type}", "gap://task/{id}"]);

    // 3. resources/read gap://meta (principal-filtered -> cacheScope: "private", ttlMs: 0, T2 hidden)
    let (_, meta_body) = send_http_json(
        &facade,
        &ctx,
        modern_http_post(
            json!(3),
            "resources/read",
            Some("gap://meta"),
            json!({ "uri": "gap://meta" }),
        ),
    )
    .await;
    assert_eq!(meta_body["result"]["resultType"], "complete");
    assert_eq!(meta_body["result"]["cacheScope"], "private");
    assert_eq!(meta_body["result"]["ttlMs"], 0);
    let meta_text = meta_body["result"]["contents"][0]["text"]
        .as_str()
        .expect("meta text");
    assert!(meta_text.contains("billing"));
    assert!(!meta_text.contains("extnotes"));

    // 4. resources/read gap://operations, gap://schema/billing_Invoice, gap://task/task-1
    for uri in [
        "gap://operations",
        "gap://schema/billing_Invoice",
        "gap://task/task-1",
    ] {
        let (r, b) = send_http_json(
            &facade,
            &ctx,
            modern_http_post(json!(4), "resources/read", Some(uri), json!({ "uri": uri })),
        )
        .await;
        assert_eq!(r.status, 200, "URI {uri} should succeed");
        assert_eq!(b["result"]["resultType"], "complete");
        assert_eq!(b["result"]["contents"][0]["uri"], uri);
    }

    // 5. resources/read on hidden T2 schema type -> -32602 (behaves as unknown)
    let (r_hid, b_hid) = send_http_json(
        &facade,
        &ctx,
        modern_http_post(
            json!(5),
            "resources/read",
            Some("gap://schema/extnotes_SecretNote"),
            json!({ "uri": "gap://schema/extnotes_SecretNote" }),
        ),
    )
    .await;
    assert_eq!(r_hid.status, 400);
    assert_eq!(b_hid["error"]["code"], -32602);

    // 6. resources/subscribe -> 404 + -32601
    let (r_sub, b_sub) = send_http_json(
        &facade,
        &ctx,
        modern_http_post(
            json!(6),
            "resources/subscribe",
            None,
            json!({ "uri": "gap://meta" }),
        ),
    )
    .await;
    assert_eq!(r_sub.status, 404);
    assert_eq!(b_sub["error"]["code"], -32601);
}
