//! `CallToolResult` envelope construction, `isError` rules, and `GQL-022` UTF-8-safe budget truncation (`CMP-004`, `GQL-022`).

use std::collections::HashMap;

use gap_core::error::GapError;
use gap_core::snapshot::Snapshot;
use gap_core::traits::GraphqlResponse;
use serde_json::{Map, Value, json};

use super::FacadeConfig;
use super::spotlight::apply_spotlighting;
use super::wire::{WireEra, server_info_meta};

/// Appended to truncated `content[0].text` when response text exceeds `budget.tokens` (`CMP-004`).
pub(crate) const TRUNCATION_SUFFIX: &str =
    "\n{\"truncated\":true,\"hint\":\"use first/after or returnFields\"}";

/// Deterministic `GQL-022` token estimator: `ceil(utf8_bytes / 3.6) = ceil(bytes * 5 / 18)`.
#[must_use]
pub(crate) const fn estimate_tokens_gql022(text: &str) -> usize {
    let bytes = text.len();
    if bytes == 0 {
        0
    } else {
        (bytes.saturating_mul(5)).div_ceil(18)
    }
}

/// Largest byte offset `<= max_bytes` that lies on a UTF-8 scalar boundary of `text`.
#[must_use]
pub(crate) fn floor_utf8_boundary(text: &str, mut max_bytes: usize) -> usize {
    if max_bytes >= text.len() {
        return text.len();
    }
    while max_bytes > 0 && !text.is_char_boundary(max_bytes) {
        max_bytes -= 1;
    }
    max_bytes
}

/// Truncates `text` to `budget_tokens` under `GQL-022`, clamping on a UTF-8 scalar boundary and
/// appending [`TRUNCATION_SUFFIX`] when truncated (`CMP-004`, `GQL-022`).
#[must_use]
pub(crate) fn truncate_text_to_budget(text: String, budget_tokens: Option<usize>) -> String {
    let Some(max_tokens) = budget_tokens else {
        return text;
    };
    if estimate_tokens_gql022(&text) <= max_tokens {
        return text;
    }
    let max_bytes = (max_tokens.saturating_mul(18)) / 5;
    let cut = floor_utf8_boundary(&text, max_bytes);
    let mut out = String::with_capacity(cut + TRUNCATION_SUFFIX.len());
    out.push_str(&text[..cut]);
    out.push_str(TRUNCATION_SUFFIX);
    out
}

/// Returns `true` iff `data` contains at least one non-null value (`CMP-004`).
#[must_use]
pub(crate) fn has_usable_data(data: Option<&Value>) -> bool {
    match data {
        None | Some(Value::Null) => false,
        Some(Value::Object(map)) => !map.is_empty() && map.values().any(|v| !v.is_null()),
        Some(_) => true,
    }
}

fn extract_input_required_state(extensions: &Map<String, Value>) -> Option<String> {
    let ir = extensions
        .get("gap")
        .and_then(Value::as_object)
        .and_then(|g| g.get("inputRequired"))?;
    if ir.is_null() {
        return None;
    }
    if let Some(tid) = ir.get("taskId").and_then(Value::as_str) {
        Some(tid.to_owned())
    } else if let Some(s) = ir.as_str() {
        Some(s.to_owned())
    } else {
        serde_json::to_string(ir).ok()
    }
}

/// Maps a [`GraphqlResponse`] into an MCP `CallToolResult` JSON object (`CMP-004`, `CMP-007`).
pub(crate) fn build_graphql_tool_result(
    config: &FacadeConfig,
    era: WireEra,
    snapshot: &Snapshot,
    root_type_name: Option<&str>,
    alias_to_field: &HashMap<String, String>,
    resp: GraphqlResponse,
    budget_tokens: Option<usize>,
) -> Value {
    let is_error = !resp.errors.is_empty() && !has_usable_data(resp.data.as_ref());
    let input_required_state = if !is_error && resp.errors.is_empty() {
        extract_input_required_state(&resp.extensions)
    } else {
        None
    };

    let mut structured = Map::new();
    if let Some(d) = resp.data {
        structured.insert("data".to_owned(), d);
    }
    if !resp.errors.is_empty() {
        let errs: Vec<Value> = resp
            .errors
            .iter()
            .map(|e| {
                serde_json::to_value(e).unwrap_or_else(|_| json!({ "message": e.to_string() }))
            })
            .collect();
        structured.insert("errors".to_owned(), Value::Array(errs));
    }
    if !resp.extensions.is_empty() {
        structured.insert("extensions".to_owned(), Value::Object(resp.extensions));
    }

    let spotlight_text = root_type_name
        .and_then(|rt| apply_spotlighting(config, snapshot, rt, alias_to_field, &mut structured));

    let raw_text = spotlight_text.unwrap_or_else(|| {
        serde_json::to_string(&Value::Object(structured.clone()))
            .unwrap_or_else(|_| "{}".to_owned())
    });
    let final_text = truncate_text_to_budget(raw_text, budget_tokens);

    let mut result = Map::new();
    if era == WireEra::Modern {
        if let Some(req_state) = input_required_state {
            result.insert("resultType".to_owned(), json!("input_required"));
            result.insert("requestState".to_owned(), json!(req_state));
        } else {
            result.insert("resultType".to_owned(), json!("complete"));
        }
    }
    result.insert(
        "content".to_owned(),
        json!([{
            "type": "text",
            "text": final_text,
        }]),
    );
    result.insert("structuredContent".to_owned(), Value::Object(structured));
    result.insert("isError".to_owned(), json!(is_error));
    if era == WireEra::Modern {
        result.insert("_meta".to_owned(), server_info_meta(config));
    }
    Value::Object(result)
}

/// Wraps a single [`GapError`] into a `CallToolResult` with `isError: true` (`CMP-004`).
pub(crate) fn build_gap_error_tool_result(
    config: &FacadeConfig,
    era: WireEra,
    snapshot: &Snapshot,
    err: GapError,
    budget_tokens: Option<usize>,
) -> Value {
    let resp = GraphqlResponse {
        data: None,
        errors: vec![err],
        extensions: Map::new(),
    };
    build_graphql_tool_result(
        config,
        era,
        snapshot,
        None,
        &HashMap::new(),
        resp,
        budget_tokens,
    )
}

/// Wraps a pure `data` (and optional `extensions`) payload into a `CallToolResult` (`isError: false`).
pub(crate) fn build_data_tool_result(
    config: &FacadeConfig,
    era: WireEra,
    snapshot: &Snapshot,
    data: Value,
    extensions: Map<String, Value>,
    budget_tokens: Option<usize>,
) -> Value {
    let resp = GraphqlResponse {
        data: Some(data),
        errors: Vec::new(),
        extensions,
    };
    build_graphql_tool_result(
        config,
        era,
        snapshot,
        None,
        &HashMap::new(),
        resp,
        budget_tokens,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    // spec: GQL-022, CMP-004
    #[test]
    fn token_estimate_is_ceil_of_utf8_bytes_over_3_6() {
        // ceil(bytes / 3.6): the boundary values distinguish ceil from floor and from rounding.
        for (bytes, expected) in [
            (0_usize, 0_usize),
            (1, 1),
            (3, 1),
            (4, 2),
            (18, 5),
            (19, 6),
            (36, 10),
            (37, 11),
            (360, 100),
            (361, 101),
        ] {
            let text = "x".repeat(bytes);
            assert_eq!(estimate_tokens_gql022(&text), expected, "{bytes} bytes");
        }
        // Multi-byte scalars count by UTF-8 bytes, not characters.
        assert_eq!(estimate_tokens_gql022("é"), 1);
        assert_eq!(estimate_tokens_gql022(&"é".repeat(10)), 6);
    }

    // spec: GQL-022
    #[test]
    fn truncation_keeps_text_within_the_budget_and_marks_the_cut() {
        let text = "a".repeat(100);
        assert_eq!(
            truncate_text_to_budget(text.clone(), Some(100)),
            text,
            "fits: untouched"
        );
        let cut = truncate_text_to_budget(text.clone(), Some(10));
        assert!(cut.ends_with(TRUNCATION_SUFFIX), "{cut}");
        let kept = &cut[..cut.len() - TRUNCATION_SUFFIX.len()];
        assert_eq!(kept.len(), 36, "10 tokens = 36 bytes");
        assert!(estimate_tokens_gql022(kept) <= 10);
        assert_eq!(truncate_text_to_budget(text.clone(), None), text);
    }
}
