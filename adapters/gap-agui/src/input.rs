//! Inbound `RunAgentInput` parser and deterministic query/intent extractors (`CMP-025`, `CMP-027`).

use gap_core::traits::GraphqlRequest;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Single message inside `RunAgentInput.messages`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgUiMessage {
    /// Optional client message ID.
    #[serde(default)]
    pub id: Option<String>,
    /// Message role (`"user"`, `"assistant"`, `"tool"`, `"system"`).
    pub role: String,
    /// Message text or JSON content.
    #[serde(default)]
    pub content: Value,
}

/// Client interrupt resolution item sent in `RunAgentInput.resume[]` (`CMP-027`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InterruptResume {
    /// Interrupt ID echoed from the prior `RUN_FINISHED { outcome: { type: "interrupt" } }`.
    pub interrupt_id: String,
    /// Approval / input decision (`"approve"`, `"deny"`, `"input"`).
    pub status: String,
    /// Optional structured payload (for example approver note or parameters).
    #[serde(default)]
    pub payload: Value,
}

/// Primary inbound payload accepted by `POST /ag-ui` (`ag-ui-protocol/ag-ui` `1.0`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunAgentInput {
    /// Conversation thread identifier.
    pub thread_id: String,
    /// Current run identifier (generated if empty).
    #[serde(default)]
    pub run_id: String,
    /// Optional parent run ID for conversation branching / time-travel (`CMP-026`).
    #[serde(default)]
    pub parent_run_id: Option<String>,
    /// Frontend application state JSON.
    #[serde(default)]
    pub state: Value,
    /// Conversation history messages.
    #[serde(default)]
    pub messages: Vec<AgUiMessage>,
    /// Frontend-declared client tools.
    #[serde(default)]
    pub tools: Vec<Value>,
    /// Ambient UI context items.
    #[serde(default)]
    pub context: Vec<Value>,
    /// Structured GAP properties (`forwardedProps.gapql`, `variables`, `operationName`, `approval`).
    #[serde(default)]
    pub forwarded_props: Value,
    /// Human-in-the-loop interrupt resolutions (`CMP-027`).
    #[serde(default)]
    pub resume: Vec<InterruptResume>,
}

/// Extracts an explicit GAP-QL [`GraphqlRequest`] from `input.forwarded_props`, if provided.
#[must_use]
pub fn extract_gapql(input: &RunAgentInput) -> Option<GraphqlRequest> {
    let props = input.forwarded_props.as_object()?;
    let gapql_val = props.get("gapql").or_else(|| props.get("graphql"))?;

    let mut req = GraphqlRequest::default();
    match gapql_val {
        Value::String(doc) if !doc.trim().is_empty() => {
            req.query = Some(doc.clone());
        }
        Value::Object(obj) => {
            if let Some(q) = obj.get("query").and_then(Value::as_str)
                && !q.trim().is_empty()
            {
                req.query = Some(q.to_owned());
            }
            if let Some(op) = obj.get("operationName").and_then(Value::as_str) {
                req.operation_name = Some(op.to_owned());
            }
            if let Some(vars) = obj.get("variables").and_then(Value::as_object) {
                req.variables = vars.clone();
            }
            if let Some(exts) = obj.get("extensions").and_then(Value::as_object) {
                req.extensions = exts.clone();
            }
        }
        _ => return None,
    }

    if let Some(vars) = props.get("variables").and_then(Value::as_object) {
        for (k, v) in vars {
            req.variables.insert(k.clone(), v.clone());
        }
    }
    if req.operation_name.is_none()
        && let Some(op) = props.get("operationName").and_then(Value::as_str)
    {
        req.operation_name = Some(op.to_owned());
    }
    if req.query.is_none() && req.extensions.is_empty() {
        return None;
    }
    Some(req)
}

/// Extracts the latest non-empty user message text for advisory discovery (`ARC-002`).
#[must_use]
pub fn extract_user_intent(input: &RunAgentInput) -> Option<String> {
    input
        .messages
        .iter()
        .rev()
        .find(|m| m.role.eq_ignore_ascii_case("user"))
        .and_then(|m| match &m.content {
            Value::String(s) => {
                let trimmed = s.trim();
                if trimmed.is_empty() {
                    None
                } else {
                    Some(trimmed.to_owned())
                }
            }
            other if !other.is_null() => Some(other.to_string()),
            _ => None,
        })
}
