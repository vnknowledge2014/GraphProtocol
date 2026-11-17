//! AG-UI v1.0 `EventType` discriminators, run outcomes, and SSE serialization (`CMP-026`, `CMP-027`).

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// Supported AG-UI specification version string.
pub const AG_UI_PROTOCOL_VERSION: &str = "1.0";

/// Standard AG-UI v1.0 streaming event types (`ag-ui-protocol/ag-ui`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum EventType {
    /// Signals start of an agent run (`threadId`, `runId`).
    RunStarted,
    /// Signals completion (or human-in-the-loop interrupt outcome) of an agent run.
    RunFinished,
    /// Signals run-level fatal error.
    RunError,
    /// Signals beginning of a discrete step inside a run.
    StepStarted,
    /// Signals end of a discrete step inside a run.
    StepFinished,
    /// Opens an assistant text message stream.
    TextMessageStart,
    /// Incremental text chunk within an open message.
    TextMessageContent,
    /// Closes an open assistant text message stream.
    TextMessageEnd,
    /// Announces tool invocation start.
    ToolCallStart,
    /// Incremental JSON argument fragment for an active tool call.
    ToolCallArgs,
    /// Completes argument transmission for a tool call.
    ToolCallEnd,
    /// Carries the structured/spotlighted result of an executed tool call.
    ToolCallResult,
    /// Synchronizes full state snapshot to the frontend UI.
    StateSnapshot,
    /// Carries JSON Patch (RFC 6902) delta operations on shared state.
    StateDelta,
    /// Emits ongoing background activity progress.
    ActivitySnapshot,
    /// Transformed raw event passthrough.
    Raw,
    /// Custom application event envelope.
    Custom,
}

/// Interactive human-in-the-loop interrupt descriptor (`CMP-027`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgUiInterrupt {
    /// Stable identifier used to correlate subsequent `RunAgentInput.resume[]`.
    pub id: String,
    /// Machine-readable reason (`"approval_required"`, `"input_required"`, ...).
    pub reason: String,
    /// Structured summary payload describing requested confirmation or inputs.
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub payload: Value,
}

/// Terminal outcome inside `RUN_FINISHED` (`CMP-027`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RunOutcome {
    /// Normal run completion.
    Success,
    /// Run paused waiting for client/human interrupt resolution (`INPUT_REQUIRED` / `GAP_APPROVAL_REQUIRED`).
    Interrupt {
        /// Pending interrupt descriptors to echo via `resume[]`.
        interrupts: Vec<AgUiInterrupt>,
    },
}

/// Base AG-UI v1.0 event envelope supporting all standard event discriminators (`CMP-026`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgUiEvent {
    /// Event discriminator (`RUN_STARTED`, `TEXT_MESSAGE_CONTENT`, ...).
    #[serde(rename = "type")]
    pub event_type: EventType,
    /// Optional Unix millisecond timestamp.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<u64>,
    /// Optional open-by-key metadata (generation, cost, trace info; never secrets).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Map<String, Value>>,
    /// Optional originating subagent run identifier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subagent_run_id: Option<String>,
    /// Optional original transformed event payload.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw_event: Option<Value>,
    /// Conversation thread identifier (`RUN_STARTED`, `RUN_FINISHED`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
    /// Run identifier (`RUN_STARTED`, `RUN_FINISHED`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    /// Parent run identifier for branching / time travel (`RUN_STARTED`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_run_id: Option<String>,
    /// Step name (`STEP_STARTED`, `STEP_FINISHED`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step_name: Option<String>,
    /// Assistant message identifier (`TEXT_MESSAGE_*`, `TOOL_CALL_RESULT`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_id: Option<String>,
    /// Message role (`TEXT_MESSAGE_START`, `TOOL_CALL_RESULT`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    /// Streaming delta string (`TEXT_MESSAGE_CONTENT`, `TOOL_CALL_ARGS`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delta: Option<String>,
    /// Tool call identifier (`TOOL_CALL_*`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// Invoked tool name (`TOOL_CALL_START`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_name: Option<String>,
    /// Serialized result content (`TOOL_CALL_RESULT`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    /// Full synchronized state snapshot (`STATE_SNAPSHOT`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<Value>,
    /// RFC 6902 JSON Patch operations (`STATE_DELTA`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delta_ops: Option<Vec<Value>>,
    /// Run result summary (`RUN_FINISHED`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    /// Structured completion / interrupt outcome (`RUN_FINISHED`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<RunOutcome>,
    /// Error message (`RUN_ERROR`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// Machine-readable error code (`RUN_ERROR`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
}

impl AgUiEvent {
    /// Creates an empty event envelope with `event_type` and `timestamp`.
    #[must_use]
    pub const fn new(event_type: EventType, timestamp: Option<u64>) -> Self {
        Self {
            event_type,
            timestamp,
            metadata: None,
            subagent_run_id: None,
            raw_event: None,
            thread_id: None,
            run_id: None,
            parent_run_id: None,
            step_name: None,
            message_id: None,
            role: None,
            delta: None,
            tool_call_id: None,
            tool_call_name: None,
            content: None,
            snapshot: None,
            delta_ops: None,
            result: None,
            outcome: None,
            message: None,
            code: None,
        }
    }

    /// Constructs `RUN_STARTED`.
    #[must_use]
    pub fn run_started(
        thread_id: impl Into<String>,
        run_id: impl Into<String>,
        parent_run_id: Option<String>,
        timestamp: Option<u64>,
    ) -> Self {
        let mut ev = Self::new(EventType::RunStarted, timestamp);
        ev.thread_id = Some(thread_id.into());
        ev.run_id = Some(run_id.into());
        ev.parent_run_id = parent_run_id;
        ev
    }

    /// Constructs `STEP_STARTED`.
    #[must_use]
    pub fn step_started(step_name: impl Into<String>, timestamp: Option<u64>) -> Self {
        let mut ev = Self::new(EventType::StepStarted, timestamp);
        ev.step_name = Some(step_name.into());
        ev
    }

    /// Constructs `STEP_FINISHED`.
    #[must_use]
    pub fn step_finished(step_name: impl Into<String>, timestamp: Option<u64>) -> Self {
        let mut ev = Self::new(EventType::StepFinished, timestamp);
        ev.step_name = Some(step_name.into());
        ev
    }

    /// Constructs `TOOL_CALL_START`.
    #[must_use]
    pub fn tool_call_start(
        tool_call_id: impl Into<String>,
        tool_call_name: impl Into<String>,
        parent_message_id: Option<String>,
        timestamp: Option<u64>,
    ) -> Self {
        let mut ev = Self::new(EventType::ToolCallStart, timestamp);
        ev.tool_call_id = Some(tool_call_id.into());
        ev.tool_call_name = Some(tool_call_name.into());
        ev.message_id = parent_message_id;
        ev
    }

    /// Constructs `TOOL_CALL_ARGS`.
    #[must_use]
    pub fn tool_call_args(
        tool_call_id: impl Into<String>,
        delta: impl Into<String>,
        timestamp: Option<u64>,
    ) -> Self {
        let mut ev = Self::new(EventType::ToolCallArgs, timestamp);
        ev.tool_call_id = Some(tool_call_id.into());
        ev.delta = Some(delta.into());
        ev
    }

    /// Constructs `TOOL_CALL_END`.
    #[must_use]
    pub fn tool_call_end(tool_call_id: impl Into<String>, timestamp: Option<u64>) -> Self {
        let mut ev = Self::new(EventType::ToolCallEnd, timestamp);
        ev.tool_call_id = Some(tool_call_id.into());
        ev
    }

    /// Constructs `TOOL_CALL_RESULT`.
    #[must_use]
    pub fn tool_call_result(
        message_id: impl Into<String>,
        tool_call_id: impl Into<String>,
        content: impl Into<String>,
        timestamp: Option<u64>,
    ) -> Self {
        let mut ev = Self::new(EventType::ToolCallResult, timestamp);
        ev.message_id = Some(message_id.into());
        ev.tool_call_id = Some(tool_call_id.into());
        ev.role = Some("tool".to_owned());
        ev.content = Some(content.into());
        ev
    }

    /// Constructs `TEXT_MESSAGE_START`.
    #[must_use]
    pub fn text_message_start(
        message_id: impl Into<String>,
        role: impl Into<String>,
        timestamp: Option<u64>,
    ) -> Self {
        let mut ev = Self::new(EventType::TextMessageStart, timestamp);
        ev.message_id = Some(message_id.into());
        ev.role = Some(role.into());
        ev
    }

    /// Constructs `TEXT_MESSAGE_CONTENT`.
    #[must_use]
    pub fn text_message_content(
        message_id: impl Into<String>,
        delta: impl Into<String>,
        timestamp: Option<u64>,
    ) -> Self {
        let mut ev = Self::new(EventType::TextMessageContent, timestamp);
        ev.message_id = Some(message_id.into());
        ev.delta = Some(delta.into());
        ev
    }

    /// Constructs `TEXT_MESSAGE_END`.
    #[must_use]
    pub fn text_message_end(message_id: impl Into<String>, timestamp: Option<u64>) -> Self {
        let mut ev = Self::new(EventType::TextMessageEnd, timestamp);
        ev.message_id = Some(message_id.into());
        ev
    }

    /// Constructs `STATE_SNAPSHOT`.
    #[must_use]
    pub fn state_snapshot(snapshot: Value, timestamp: Option<u64>) -> Self {
        let mut ev = Self::new(EventType::StateSnapshot, timestamp);
        ev.snapshot = Some(snapshot);
        ev
    }

    /// Constructs `STATE_DELTA`.
    #[must_use]
    pub fn state_delta(delta_ops: Vec<Value>, timestamp: Option<u64>) -> Self {
        let mut ev = Self::new(EventType::StateDelta, timestamp);
        ev.delta_ops = Some(delta_ops);
        ev
    }

    /// Constructs `RUN_FINISHED`.
    #[must_use]
    pub fn run_finished(
        thread_id: impl Into<String>,
        run_id: impl Into<String>,
        outcome: RunOutcome,
        result: Option<Value>,
        timestamp: Option<u64>,
    ) -> Self {
        let mut ev = Self::new(EventType::RunFinished, timestamp);
        ev.thread_id = Some(thread_id.into());
        ev.run_id = Some(run_id.into());
        ev.outcome = Some(outcome);
        ev.result = result;
        ev
    }

    /// Constructs `RUN_ERROR`.
    #[must_use]
    pub fn run_error(
        message: impl Into<String>,
        code: impl Into<String>,
        timestamp: Option<u64>,
    ) -> Self {
        let mut ev = Self::new(EventType::RunError, timestamp);
        ev.message = Some(message.into());
        ev.code = Some(code.into());
        ev
    }
}

/// Encodes `events` as standard Server-Sent Events (`data: <json>\n\n`) (`CMP-025`).
#[must_use]
pub fn encode_sse_stream(events: &[AgUiEvent]) -> Vec<u8> {
    let mut out = Vec::with_capacity(events.len().saturating_mul(160));
    for ev in events {
        if let Ok(line) = serde_json::to_string(ev) {
            out.extend_from_slice(b"data: ");
            out.extend_from_slice(line.as_bytes());
            out.extend_from_slice(b"\n\n");
        }
    }
    out
}
