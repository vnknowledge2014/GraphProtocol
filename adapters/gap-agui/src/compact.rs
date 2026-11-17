//! AG-UI v1.0 stream compaction (`ag-ui-protocol/ag-ui` Serialization, `CMP-026`).

use serde_json::{Map, Value};

use crate::events::{AgUiEvent, EventType};

fn merge_metadata(target: &mut Option<Map<String, Value>>, incoming: Option<&Map<String, Value>>) {
    let Some(inc) = incoming else {
        return;
    };
    let dst = target.get_or_insert_with(Map::new);
    for (k, v) in inc {
        dst.insert(k.clone(), v.clone());
    }
}

fn apply_state_delta_ops(snapshot: &mut Value, ops: &[Value]) {
    let Some(root) = snapshot.as_object_mut() else {
        return;
    };
    for op in ops {
        let Some(obj) = op.as_object() else {
            continue;
        };
        let action = obj.get("op").and_then(Value::as_str).unwrap_or("");
        let path = obj.get("path").and_then(Value::as_str).unwrap_or("");
        let key = path.trim_start_matches('/');
        if key.is_empty() || key.contains('/') {
            continue;
        }
        match action {
            "add" | "replace" => {
                if let Some(val) = obj.get("value") {
                    root.insert(key.to_owned(), val.clone());
                }
            }
            "remove" => {
                root.remove(key);
            }
            _ => {}
        }
    }
}

/// Compacts verbose AG-UI event streams while preserving semantics (`CMP-026`):
///
/// - Merges adjacent `TEXT_MESSAGE_CONTENT` chunks carrying the same `message_id`.
/// - Merges adjacent `TOOL_CALL_ARGS` chunks carrying the same `tool_call_id`.
/// - Collapses intermediate `STATE_DELTA` ops directly into the latest `STATE_SNAPSHOT`
///   and merges per-key metadata (`last write wins`).
#[must_use]
pub fn compact_events(events: &[AgUiEvent]) -> Vec<AgUiEvent> {
    let mut out: Vec<AgUiEvent> = Vec::with_capacity(events.len());
    let mut last_snapshot_idx: Option<usize> = None;

    for ev in events {
        match ev.event_type {
            EventType::TextMessageContent => {
                if let Some(prev) = out.last_mut()
                    && prev.event_type == EventType::TextMessageContent
                    && prev.message_id == ev.message_id
                    && prev.subagent_run_id == ev.subagent_run_id
                {
                    let buf = prev.delta.get_or_insert_with(String::new);
                    if let Some(chunk) = &ev.delta {
                        buf.push_str(chunk);
                    }
                    merge_metadata(&mut prev.metadata, ev.metadata.as_ref());
                    prev.timestamp = ev.timestamp.or(prev.timestamp);
                    continue;
                }
                out.push(ev.clone());
            }
            EventType::ToolCallArgs => {
                if let Some(prev) = out.last_mut()
                    && prev.event_type == EventType::ToolCallArgs
                    && prev.tool_call_id == ev.tool_call_id
                    && prev.subagent_run_id == ev.subagent_run_id
                {
                    let buf = prev.delta.get_or_insert_with(String::new);
                    if let Some(chunk) = &ev.delta {
                        buf.push_str(chunk);
                    }
                    merge_metadata(&mut prev.metadata, ev.metadata.as_ref());
                    prev.timestamp = ev.timestamp.or(prev.timestamp);
                    continue;
                }
                out.push(ev.clone());
            }
            EventType::StateSnapshot => {
                if let Some(prev_idx) = last_snapshot_idx
                    && let Some(prev) = out.get_mut(prev_idx)
                {
                    prev.snapshot = ev.snapshot.clone();
                    merge_metadata(&mut prev.metadata, ev.metadata.as_ref());
                    prev.timestamp = ev.timestamp.or(prev.timestamp);
                    continue;
                }
                last_snapshot_idx = Some(out.len());
                out.push(ev.clone());
            }
            EventType::StateDelta => {
                if let Some(prev_idx) = last_snapshot_idx
                    && let Some(prev) = out.get_mut(prev_idx)
                    && let Some(snap) = prev.snapshot.as_mut()
                    && let Some(ops) = ev.delta_ops.as_deref()
                {
                    apply_state_delta_ops(snap, ops);
                    merge_metadata(&mut prev.metadata, ev.metadata.as_ref());
                    prev.timestamp = ev.timestamp.or(prev.timestamp);
                    continue;
                }
                out.push(ev.clone());
            }
            _ => out.push(ev.clone()),
        }
    }
    out
}
