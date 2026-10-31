//! Size gate `G-F1` (`CMP-003`, `CMP-008`): `tools/list` <= 1,200 tokens and each description <= 60 tokens under both conservative counters.

use gap_core::traits::HttpModule;
use serde_json::{Value, json};

use super::support::{compose_n_branch_snapshot, default_facade, make_ctx, modern_http_post};

/// Conservative Counter A: `ceil(utf8_bytes / 3)`.
pub fn counter_a(text: &str) -> usize {
    text.len().div_ceil(3)
}

/// Conservative Counter B: every maximal run of alphanumeric chars split into pieces of at most 4
/// chars, plus 1 token per other non-whitespace character.
pub fn counter_b(text: &str) -> usize {
    let mut tokens = 0_usize;
    let mut alnum_run = 0_usize;
    for ch in text.chars() {
        if ch.is_alphanumeric() {
            alnum_run += 1;
        } else {
            if alnum_run > 0 {
                tokens += alnum_run.div_ceil(4);
                alnum_run = 0;
            }
            if !ch.is_whitespace() {
                tokens += 1;
            }
        }
    }
    if alnum_run > 0 {
        tokens += alnum_run.div_ceil(4);
    }
    tokens
}

// spec: CMP-003, CMP-008
#[tokio::test]
async fn facade_tools_list_size_within_1200_tokens_across_5_50_200_1000_branches() {
    let (facade, _) = default_facade();
    let mut baseline_result_json: Option<String> = None;

    for n in [5_usize, 50, 200, 1_000] {
        let snap = compose_n_branch_snapshot(n);
        assert_eq!(snap.branches.len(), n);
        let ctx = make_ctx(snap, "agent:bench", "acme-corp", &["read:all"]);
        let req = modern_http_post(json!(1), "tools/list", None, json!({}));
        let resp = facade.handle(&ctx, req).await.expect("handle tools/list");
        assert_eq!(resp.status, 200);

        let rpc: Value = serde_json::from_slice(&resp.body).expect("valid JSON-RPC response");
        let result_serialized =
            serde_json::to_string(&rpc["result"]).expect("serialize ListToolsResult");

        let count_a = counter_a(&result_serialized);
        let count_b = counter_b(&result_serialized);
        assert!(
            count_a <= 1_200,
            "G-F1 Counter A exceeded for N={n}: {count_a} > 1200"
        );
        assert!(
            count_b <= 1_200,
            "G-F1 Counter B exceeded for N={n}: {count_b} > 1200"
        );

        if let Some(prev) = &baseline_result_json {
            assert_eq!(
                &result_serialized, prev,
                "tools/list result must be byte-identical across N={n} branches"
            );
        } else {
            baseline_result_json = Some(result_serialized);
        }
    }
}

// spec: CMP-003, CMP-008
#[tokio::test]
async fn facade_tools_list_size_each_description_at_most_60_tokens() {
    let (facade, _) = default_facade();
    let snap = compose_n_branch_snapshot(5);
    let ctx = make_ctx(snap, "agent:bench", "acme-corp", &[]);
    let req = modern_http_post(json!(1), "tools/list", None, json!({}));
    let resp = facade.handle(&ctx, req).await.expect("handle tools/list");
    let rpc: Value = serde_json::from_slice(&resp.body).expect("json");
    let tools = rpc["result"]["tools"].as_array().expect("tools array");
    assert_eq!(tools.len(), 5);

    for tool in tools {
        let name = tool["name"].as_str().expect("tool name");
        let desc = tool["description"].as_str().expect("tool description");
        let a = counter_a(desc);
        let b = counter_b(desc);
        assert!(
            a <= 60 && b <= 60,
            "tool `{name}` description exceeds 60 tokens (A={a}, B={b}): `{desc}`"
        );
    }
}
