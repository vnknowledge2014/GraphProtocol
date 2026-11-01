//! Untrusted branch output spotlighting (`CMP-007`, `SEC-083`).

use std::collections::HashMap;

use gap_core::snapshot::{FieldCoordinate, Snapshot, TrustTier};
use serde_json::{Map, Value, json};

use super::FacadeConfig;

/// Provenance record emitted in `extensions.gap.provenance` for each spotlighted root field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SpotlightProvenance {
    pub(crate) field: String,
    pub(crate) branch: String,
    pub(crate) trust: TrustTier,
}

impl SpotlightProvenance {
    pub(crate) fn to_json(&self) -> Value {
        let trust_str = match self.trust {
            TrustTier::T0 => "T0",
            TrustTier::T1 => "T1",
            TrustTier::T2 => "T2",
        };
        json!({
            "field": self.field,
            "branch": self.branch,
            "trust": trust_str,
        })
    }
}

/// Escapes any case-insensitive `<gap:untrusted` or `</gap:untrusted` prefix inside `raw`
/// so untrusted payloads can never forge an opening or closing spotlight delimiter (`SEC-083`).
pub(crate) fn escape_spotlight_tags(raw: &str) -> String {
    const OPEN_PREFIX: &[u8] = b"<gap:untrusted";
    const CLOSE_PREFIX: &[u8] = b"</gap:untrusted";

    let bytes = raw.as_bytes();
    let mut out = String::with_capacity(raw.len());
    let mut cursor = 0_usize;
    let mut last_flushed = 0_usize;

    while cursor < bytes.len() {
        if bytes[cursor] == b'<' {
            let tail = &bytes[cursor..];
            let matches_open = tail.len() >= OPEN_PREFIX.len()
                && tail[..OPEN_PREFIX.len()].eq_ignore_ascii_case(OPEN_PREFIX);
            let matches_close = tail.len() >= CLOSE_PREFIX.len()
                && tail[..CLOSE_PREFIX.len()].eq_ignore_ascii_case(CLOSE_PREFIX);
            if matches_open || matches_close {
                out.push_str(&raw[last_flushed..cursor]);
                out.push_str("\\u003c");
                cursor += 1;
                last_flushed = cursor;
                continue;
            }
        }
        cursor += 1;
    }

    if last_flushed < raw.len() {
        out.push_str(&raw[last_flushed..]);
    }
    out
}

fn should_spotlight(config: &FacadeConfig, trust: TrustTier) -> bool {
    if !config.spotlight {
        return false;
    }
    match trust {
        TrustTier::T2 => true,
        TrustTier::T1 => config.spotlight_t1,
        TrustTier::T0 => false,
    }
}

/// Computes spotlighted provenance entries and, when at least one root field in `data` is
/// spotlighted, builds the `<gap:untrusted source="branch/op">…</gap:untrusted>` text rendering
/// while leaving `structured_content` pure JSON (`CMP-007`, `SEC-083`).
pub(crate) fn apply_spotlighting(
    config: &FacadeConfig,
    snapshot: &Snapshot,
    root_type_name: &str,
    alias_to_field: &HashMap<String, String>,
    structured_content: &mut Map<String, Value>,
) -> Option<String> {
    if !config.spotlight {
        return None;
    }

    let Some(Value::Object(data_map)) = structured_content.get("data") else {
        return None;
    };

    let mut wrapped_fields: Vec<(String, String, String, TrustTier)> = Vec::new();
    for response_key in data_map.keys() {
        let schema_field = alias_to_field
            .get(response_key)
            .map_or(response_key.as_str(), String::as_str);
        let coord = FieldCoordinate::new(root_type_name, schema_field);
        if let Some(uid) = snapshot.ownership.owner(&coord)
            && let Some(branch) = snapshot.branches.get(&uid)
            && should_spotlight(config, branch.trust)
        {
            wrapped_fields.push((
                response_key.clone(),
                branch.id.as_str().to_owned(),
                schema_field.to_owned(),
                branch.trust,
            ));
        }
    }

    if wrapped_fields.is_empty() {
        return None;
    }

    let provenance_json: Vec<Value> = wrapped_fields
        .iter()
        .map(|(field, branch, _, trust)| {
            SpotlightProvenance {
                field: field.clone(),
                branch: branch.clone(),
                trust: *trust,
            }
            .to_json()
        })
        .collect();

    // Attach `extensions.gap.provenance` onto `structured_content` BEFORE rendering text.
    let ext_entry = structured_content
        .entry("extensions".to_owned())
        .or_insert_with(|| Value::Object(Map::new()));
    if !ext_entry.is_object() {
        *ext_entry = Value::Object(Map::new());
    }
    if let Value::Object(ext_map) = ext_entry {
        let gap_entry = ext_map
            .entry("gap".to_owned())
            .or_insert_with(|| Value::Object(Map::new()));
        if !gap_entry.is_object() {
            *gap_entry = Value::Object(Map::new());
        }
        if let Value::Object(gap_map) = gap_entry {
            gap_map.insert("provenance".to_owned(), Value::Array(provenance_json));
        }
    }

    let Some(Value::Object(data_map)) = structured_content.get("data") else {
        return None;
    };

    let mut data_parts: Vec<String> = Vec::with_capacity(data_map.len());
    for (k, v) in data_map {
        let key_json = serde_json::to_string(k).unwrap_or_else(|_| "\"\"".to_owned());
        let val_json = serde_json::to_string(v).unwrap_or_else(|_| "null".to_owned());
        if let Some((_, branch_id, op_name, _)) =
            wrapped_fields.iter().find(|(rk, _, _, _)| rk == k)
        {
            let escaped = escape_spotlight_tags(&val_json);
            data_parts.push(format!(
                "{key_json}:<gap:untrusted source=\"{branch_id}/{op_name}\">{escaped}</gap:untrusted>"
            ));
        } else {
            data_parts.push(format!("{key_json}:{val_json}"));
        }
    }

    let mut top_parts: Vec<String> = Vec::with_capacity(structured_content.len());
    for (top_k, top_v) in &*structured_content {
        let top_key_json = serde_json::to_string(top_k).unwrap_or_else(|_| "\"\"".to_owned());
        if top_k == "data" {
            top_parts.push(format!("{top_key_json}:{{{}}}", data_parts.join(",")));
        } else {
            let encoded = serde_json::to_string(top_v).unwrap_or_else(|_| "null".to_owned());
            top_parts.push(format!("{top_key_json}:{encoded}"));
        }
    }

    Some(format!("{{{}}}", top_parts.join(",")))
}
