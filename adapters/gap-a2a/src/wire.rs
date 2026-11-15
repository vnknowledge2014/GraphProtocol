//! `A2A v1.0` wire types, `TASK_STATE_*` / `ROLE_*` enums, `StreamResponse` SSE framing, and trace-loop extension (`CMP-021`, `CMP-022`, `INV-20`, `SPEC-NOTES.md` `Q-11`..`Q-15`).

use std::collections::BTreeMap;

use gap_core::clock::Timestamp;
use gap_core::context::DEFAULT_MAX_HOPS;
use gap_core::error::{ErrorCode, GapError, GapResult};
use gap_proto::ngc::v1::TaskState as ProtoTaskState;
use gap_sec::json::JsonValue;

/// Canonical A2A protocol version enforced by `A2A-Version` headers (`SPEC-NOTES.md` `Q-13`).
pub const A2A_PROTOCOL_VERSION: &str = "1.0";

/// HTTP header selecting A2A specification version (`SPEC-NOTES.md` `Q-13`).
pub const A2A_VERSION_HEADER: &str = "a2a-version";

/// HTTP header listing active A2A extension URIs (`SPEC-NOTES.md` `Q-13`).
pub const A2A_EXTENSIONS_HEADER: &str = "a2a-extensions";

/// Canonical HTTP+JSON media type for A2A `v1.0.1` (`SPEC-NOTES.md` `Q-15`).
pub const A2A_CONTENT_TYPE: &str = "application/a2a+json";

/// Versioned GAP peer trace/loop-detection extension URI (`CMP-022`, `SPEC-NOTES.md` `Q-13`).
pub const GAP_TRACE_EXT_URI: &str = "https://gap-protocol.org/ext/trace/v1";

/// Default maximum peer delegation hops matching [`gap_core::context::DEFAULT_MAX_HOPS`].
pub const DEFAULT_A2A_MAX_HOPS: u8 = DEFAULT_MAX_HOPS;

/// A2A v1.0 `TaskState` enum (`a2a.proto` §5.5 `TASK_STATE_*` wire strings, `ADR-016`, `Q-11`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum A2aTaskState {
    /// Queued and accepted (`TASK_STATE_SUBMITTED`, `1`).
    Submitted,
    /// Actively running (`TASK_STATE_WORKING`, `2`).
    Working,
    /// Finished successfully (`TASK_STATE_COMPLETED`, `3`, terminal).
    Completed,
    /// Finished with error (`TASK_STATE_FAILED`, `4`, terminal).
    Failed,
    /// Canceled by caller (`TASK_STATE_CANCELED`, `5`, terminal).
    Canceled,
    /// Paused awaiting caller continuation (`TASK_STATE_INPUT_REQUIRED`, `6`, interrupted).
    InputRequired,
    /// Rejected prior to execution (`TASK_STATE_REJECTED`, `7`, terminal).
    Rejected,
    /// Paused awaiting caller authorization (`TASK_STATE_AUTH_REQUIRED`, `8`, interrupted).
    AuthRequired,
}

impl A2aTaskState {
    /// Canonical `ProtoJSON` wire string with mandatory `TASK_STATE_` prefix (`Q-11`).
    #[must_use]
    pub const fn as_wire_str(self) -> &'static str {
        match self {
            Self::Submitted => "TASK_STATE_SUBMITTED",
            Self::Working => "TASK_STATE_WORKING",
            Self::Completed => "TASK_STATE_COMPLETED",
            Self::Failed => "TASK_STATE_FAILED",
            Self::Canceled => "TASK_STATE_CANCELED",
            Self::InputRequired => "TASK_STATE_INPUT_REQUIRED",
            Self::Rejected => "TASK_STATE_REJECTED",
            Self::AuthRequired => "TASK_STATE_AUTH_REQUIRED",
        }
    }

    /// Parses an A2A v1.0 wire `TaskState` string, rejecting `TASK_STATE_UNSPECIFIED` (`Q-11`).
    ///
    /// # Errors
    ///
    /// Returns [`ErrorCode::ValidationError`] on `TASK_STATE_UNSPECIFIED` or unrecognized values.
    pub fn from_wire_str(raw: &str) -> GapResult<Self> {
        match raw {
            "TASK_STATE_SUBMITTED" => Ok(Self::Submitted),
            "TASK_STATE_WORKING" => Ok(Self::Working),
            "TASK_STATE_COMPLETED" => Ok(Self::Completed),
            "TASK_STATE_FAILED" => Ok(Self::Failed),
            "TASK_STATE_CANCELED" => Ok(Self::Canceled),
            "TASK_STATE_INPUT_REQUIRED" => Ok(Self::InputRequired),
            "TASK_STATE_REJECTED" => Ok(Self::Rejected),
            "TASK_STATE_AUTH_REQUIRED" => Ok(Self::AuthRequired),
            "TASK_STATE_UNSPECIFIED" => Err(GapError::new(
                ErrorCode::ValidationError,
                "A2A TASK_STATE_UNSPECIFIED (0) is forbidden on the wire",
            )),
            other => Err(GapError::new(
                ErrorCode::ValidationError,
                format!("unknown A2A TaskState `{other}`"),
            )),
        }
    }

    /// Returns `true` if this state is terminal (`COMPLETED | FAILED | CANCELED | REJECTED`).
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Failed | Self::Canceled | Self::Rejected
        )
    }

    /// Returns `true` if this state is interrupted (`INPUT_REQUIRED | AUTH_REQUIRED`).
    #[must_use]
    pub const fn is_interrupted(self) -> bool {
        matches!(self, Self::InputRequired | Self::AuthRequired)
    }

    /// Maps into [`gap_proto::ngc::v1::TaskState`].
    #[must_use]
    pub const fn to_proto(self) -> ProtoTaskState {
        match self {
            Self::Submitted => ProtoTaskState::Submitted,
            Self::Working => ProtoTaskState::Working,
            Self::Completed => ProtoTaskState::Completed,
            Self::Failed => ProtoTaskState::Failed,
            Self::Canceled => ProtoTaskState::Canceled,
            Self::InputRequired => ProtoTaskState::InputRequired,
            Self::Rejected => ProtoTaskState::Rejected,
            Self::AuthRequired => ProtoTaskState::AuthRequired,
        }
    }

    /// Maps from [`gap_proto::ngc::v1::TaskState`], rejecting `TaskStateUnspecified`.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorCode::ValidationError`] when `proto` is `TaskStateUnspecified`.
    pub fn from_proto(proto: ProtoTaskState) -> GapResult<Self> {
        match proto {
            ProtoTaskState::Submitted => Ok(Self::Submitted),
            ProtoTaskState::Working => Ok(Self::Working),
            ProtoTaskState::Completed => Ok(Self::Completed),
            ProtoTaskState::Failed => Ok(Self::Failed),
            ProtoTaskState::Canceled => Ok(Self::Canceled),
            ProtoTaskState::InputRequired => Ok(Self::InputRequired),
            ProtoTaskState::Rejected => Ok(Self::Rejected),
            ProtoTaskState::AuthRequired => Ok(Self::AuthRequired),
            ProtoTaskState::Unspecified => Err(GapError::new(
                ErrorCode::ValidationError,
                "cannot encode NGC TASK_STATE_UNSPECIFIED into A2A v1.0 wire TaskState",
            )),
        }
    }
}

/// A2A v1.0 message sender role (`ROLE_USER` / `ROLE_AGENT`, `Q-13`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum A2aRole {
    /// Caller / user agent (`ROLE_USER`).
    User,
    /// Responding agent (`ROLE_AGENT`).
    Agent,
}

impl A2aRole {
    /// Canonical `ProtoJSON` wire string.
    #[must_use]
    pub const fn as_wire_str(self) -> &'static str {
        match self {
            Self::User => "ROLE_USER",
            Self::Agent => "ROLE_AGENT",
        }
    }

    /// Parses `ROLE_USER` or `ROLE_AGENT`.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorCode::ValidationError`] for unrecognized or unspecified roles.
    pub fn from_wire_str(raw: &str) -> GapResult<Self> {
        match raw {
            "ROLE_USER" => Ok(Self::User),
            "ROLE_AGENT" => Ok(Self::Agent),
            other => Err(GapError::new(
                ErrorCode::ValidationError,
                format!("invalid A2A Role `{other}`"),
            )),
        }
    }
}

/// Tagless A2A v1.0 `Part` union (`{ text }` or `{ data, mediaType }`, `Q-13`).
#[derive(Debug, Clone, PartialEq)]
pub enum A2aPart {
    /// Plain-text content part.
    Text {
        /// UTF-8 message text.
        text: String,
        /// Optional extension metadata attached to this part.
        metadata: BTreeMap<String, JsonValue>,
    },
    /// Structured JSON data part.
    Data {
        /// Structured JSON payload.
        data: JsonValue,
        /// IANA media type (for example `application/json`).
        media_type: String,
        /// Optional extension metadata attached to this part.
        metadata: BTreeMap<String, JsonValue>,
    },
}

impl A2aPart {
    /// Encodes this part without a deprecated v0.x `kind` discriminator (`Q-13`).
    #[must_use]
    pub fn to_json_value(&self) -> JsonValue {
        let mut obj = BTreeMap::new();
        match self {
            Self::Text { text, metadata } => {
                obj.insert("text".to_owned(), JsonValue::String(text.clone()));
                if !metadata.is_empty() {
                    obj.insert("metadata".to_owned(), JsonValue::Object(metadata.clone()));
                }
            }
            Self::Data {
                data,
                media_type,
                metadata,
            } => {
                obj.insert("data".to_owned(), data.clone());
                obj.insert(
                    "mediaType".to_owned(),
                    JsonValue::String(media_type.clone()),
                );
                if !metadata.is_empty() {
                    obj.insert("metadata".to_owned(), JsonValue::Object(metadata.clone()));
                }
            }
        }
        JsonValue::Object(obj)
    }

    /// Decodes a v1.0 `Part` JSON object, rejecting ambiguous or `kind`-tagged malformed parts.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorCode::ValidationError`] when neither `text` nor `data` is valid.
    pub fn from_json_value(val: &JsonValue) -> GapResult<Self> {
        let obj = val
            .as_object()
            .ok_or_else(|| GapError::new(ErrorCode::ValidationError, "A2A Part must be object"))?;
        let metadata = parse_optional_map(obj.get("metadata"))?;
        let text = obj.get("text");
        let data = obj.get("data");
        match (text, data) {
            (Some(t), None) => {
                let s = t.as_str().ok_or_else(|| {
                    GapError::new(ErrorCode::ValidationError, "Part.text must be string")
                })?;
                Ok(Self::Text {
                    text: s.to_owned(),
                    metadata,
                })
            }
            (None, Some(d)) => {
                let media_type = obj
                    .get("mediaType")
                    .and_then(JsonValue::as_str)
                    .unwrap_or("application/json")
                    .to_owned();
                Ok(Self::Data {
                    data: d.clone(),
                    media_type,
                    metadata,
                })
            }
            (Some(_), Some(_)) => Err(GapError::new(
                ErrorCode::ValidationError,
                "A2A Part must not define both `text` and `data`",
            )),
            (None, None) => Err(GapError::new(
                ErrorCode::ValidationError,
                "A2A Part must define either `text` or `data`",
            )),
        }
    }
}

/// Versioned GAP loop-detection / hop-count trace extension (`CMP-022`, `INV-20`, `Q-13`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TraceExtension {
    /// Zero-based peer hop counter (`<= max_hops`).
    pub hop: u8,
    /// Ordered list of visited GAP/A2A node identifiers (`INV-20`).
    pub path: Vec<String>,
}

impl TraceExtension {
    /// Extracts [`TraceExtension`] from a `metadata` object if [`GAP_TRACE_EXT_URI`] is present.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorCode::ValidationError`] if the extension block has an invalid schema.
    pub fn from_metadata(metadata: &BTreeMap<String, JsonValue>) -> GapResult<Option<Self>> {
        let Some(raw) = metadata.get(GAP_TRACE_EXT_URI) else {
            return Ok(None);
        };
        Self::from_json_value(raw).map(Some)
    }

    /// Parses a trace extension JSON object `{ "hop": u8, "path": string[] }`.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorCode::ValidationError`] when types or ranges are invalid.
    pub fn from_json_value(val: &JsonValue) -> GapResult<Self> {
        let obj = val.as_object().ok_or_else(|| {
            GapError::new(
                ErrorCode::ValidationError,
                "trace extension value must be a JSON object",
            )
        })?;
        let hop_raw = obj.get("hop").and_then(JsonValue::as_i64).ok_or_else(|| {
            GapError::new(ErrorCode::ValidationError, "trace.hop must be integer")
        })?;
        let hop = u8::try_from(hop_raw)
            .map_err(|_| GapError::new(ErrorCode::ValidationError, "trace.hop out of u8 range"))?;
        let path_arr = obj
            .get("path")
            .and_then(JsonValue::as_array)
            .ok_or_else(|| {
                GapError::new(
                    ErrorCode::ValidationError,
                    "trace.path must be string array",
                )
            })?;
        let mut path = Vec::with_capacity(path_arr.len());
        for entry in path_arr {
            let node = entry.as_str().ok_or_else(|| {
                GapError::new(
                    ErrorCode::ValidationError,
                    "trace.path entries must be strings",
                )
            })?;
            path.push(node.to_owned());
        }
        Ok(Self { hop, path })
    }

    /// Serializes `{ "hop": ..., "path": [...] }`.
    #[must_use]
    pub fn to_json_value(&self) -> JsonValue {
        let mut obj = BTreeMap::new();
        obj.insert("hop".to_owned(), JsonValue::Int(i64::from(self.hop)));
        obj.insert(
            "path".to_owned(),
            JsonValue::Array(self.path.iter().cloned().map(JsonValue::String).collect()),
        );
        JsonValue::Object(obj)
    }

    /// Writes this trace extension into `metadata` under [`GAP_TRACE_EXT_URI`].
    pub fn insert_into(&self, metadata: &mut BTreeMap<String, JsonValue>) {
        metadata.insert(GAP_TRACE_EXT_URI.to_owned(), self.to_json_value());
    }

    /// Validates no cycle exists for `node_id` and advances `hop` up to `max_hops` (`INV-20`).
    ///
    /// # Errors
    ///
    /// Returns [`ErrorCode::LoopDetected`] if `node_id` already appears in `path`, or
    /// [`ErrorCode::HopLimit`] if advancing exceeds `max_hops`.
    pub fn advance(&self, node_id: &str, max_hops: u8) -> GapResult<Self> {
        if self.path.iter().any(|visited| visited == node_id) {
            return Err(GapError::new(
                ErrorCode::LoopDetected,
                format!("A2A peer loop detected at node `{node_id}`"),
            ));
        }
        let next_hop = self
            .hop
            .checked_add(1)
            .ok_or_else(|| GapError::new(ErrorCode::HopLimit, "A2A hop counter overflow"))?;
        if next_hop > max_hops {
            return Err(GapError::new(
                ErrorCode::HopLimit,
                format!("A2A hop count {next_hop} exceeds max_hops {max_hops}"),
            ));
        }
        let mut next_path = self.path.clone();
        next_path.push(node_id.to_owned());
        Ok(Self {
            hop: next_hop,
            path: next_path,
        })
    }
}

/// A2A v1.0 `Message` (`a2a.proto` §3, `Q-13`).
#[derive(Debug, Clone, PartialEq)]
pub struct A2aMessage {
    /// Required message identifier.
    pub message_id: String,
    /// Optional multi-turn conversation context ID.
    pub context_id: Option<String>,
    /// Optional task ID when resuming an interrupted task (`TASK_STATE_INPUT_REQUIRED`).
    pub task_id: Option<String>,
    /// Sender role (`ROLE_USER` / `ROLE_AGENT`).
    pub role: A2aRole,
    /// Non-empty list of message parts.
    pub parts: Vec<A2aPart>,
    /// Extension-namespaced metadata map.
    pub metadata: BTreeMap<String, JsonValue>,
    /// Declared extension URIs carried by this message.
    pub extensions: Vec<String>,
    /// Optional referenced related task IDs.
    pub reference_task_ids: Vec<String>,
}

impl A2aMessage {
    /// Encodes to JSON DOM.
    #[must_use]
    pub fn to_json_value(&self) -> JsonValue {
        let mut obj = BTreeMap::new();
        obj.insert(
            "messageId".to_owned(),
            JsonValue::String(self.message_id.clone()),
        );
        if let Some(ref cid) = self.context_id {
            obj.insert("contextId".to_owned(), JsonValue::String(cid.clone()));
        }
        if let Some(ref tid) = self.task_id {
            obj.insert("taskId".to_owned(), JsonValue::String(tid.clone()));
        }
        obj.insert(
            "role".to_owned(),
            JsonValue::String(self.role.as_wire_str().to_owned()),
        );
        obj.insert(
            "parts".to_owned(),
            JsonValue::Array(self.parts.iter().map(A2aPart::to_json_value).collect()),
        );
        if !self.metadata.is_empty() {
            obj.insert(
                "metadata".to_owned(),
                JsonValue::Object(self.metadata.clone()),
            );
        }
        if !self.extensions.is_empty() {
            obj.insert(
                "extensions".to_owned(),
                JsonValue::Array(
                    self.extensions
                        .iter()
                        .cloned()
                        .map(JsonValue::String)
                        .collect(),
                ),
            );
        }
        if !self.reference_task_ids.is_empty() {
            obj.insert(
                "referenceTaskIds".to_owned(),
                JsonValue::Array(
                    self.reference_task_ids
                        .iter()
                        .cloned()
                        .map(JsonValue::String)
                        .collect(),
                ),
            );
        }
        JsonValue::Object(obj)
    }

    /// Decodes an A2A v1.0 `Message` from JSON DOM.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorCode::ValidationError`] if required fields are missing or invalid.
    pub fn from_json_value(val: &JsonValue) -> GapResult<Self> {
        let obj = val.as_object().ok_or_else(|| {
            GapError::new(ErrorCode::ValidationError, "A2A Message must be object")
        })?;
        let message_id = obj
            .get("messageId")
            .and_then(JsonValue::as_str)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                GapError::new(ErrorCode::ValidationError, "Message.messageId is required")
            })?
            .to_owned();
        let role_raw = obj
            .get("role")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| GapError::new(ErrorCode::ValidationError, "Message.role is required"))?;
        let role = A2aRole::from_wire_str(role_raw)?;
        let parts_arr = obj
            .get("parts")
            .and_then(JsonValue::as_array)
            .filter(|a| !a.is_empty())
            .ok_or_else(|| {
                GapError::new(
                    ErrorCode::ValidationError,
                    "Message.parts must be a non-empty array",
                )
            })?;
        let mut parts = Vec::with_capacity(parts_arr.len());
        for p in parts_arr {
            parts.push(A2aPart::from_json_value(p)?);
        }
        Ok(Self {
            message_id,
            context_id: opt_string(obj.get("contextId"))?,
            task_id: opt_string(obj.get("taskId"))?,
            role,
            parts,
            metadata: parse_optional_map(obj.get("metadata"))?,
            extensions: parse_optional_string_list(obj.get("extensions"))?,
            reference_task_ids: parse_optional_string_list(obj.get("referenceTaskIds"))?,
        })
    }
}

/// A2A v1.0 `Artifact` carrying output parts.
#[derive(Debug, Clone, PartialEq)]
pub struct A2aArtifact {
    /// Required artifact identifier.
    pub artifact_id: String,
    /// Optional human-readable artifact name.
    pub name: Option<String>,
    /// Optional description.
    pub description: Option<String>,
    /// Non-empty artifact parts.
    pub parts: Vec<A2aPart>,
    /// Optional extension metadata.
    pub metadata: BTreeMap<String, JsonValue>,
    /// Optional extension URIs.
    pub extensions: Vec<String>,
}

impl A2aArtifact {
    /// Encodes to JSON DOM.
    #[must_use]
    pub fn to_json_value(&self) -> JsonValue {
        let mut obj = BTreeMap::new();
        obj.insert(
            "artifactId".to_owned(),
            JsonValue::String(self.artifact_id.clone()),
        );
        if let Some(ref name) = self.name {
            obj.insert("name".to_owned(), JsonValue::String(name.clone()));
        }
        if let Some(ref desc) = self.description {
            obj.insert("description".to_owned(), JsonValue::String(desc.clone()));
        }
        obj.insert(
            "parts".to_owned(),
            JsonValue::Array(self.parts.iter().map(A2aPart::to_json_value).collect()),
        );
        if !self.metadata.is_empty() {
            obj.insert(
                "metadata".to_owned(),
                JsonValue::Object(self.metadata.clone()),
            );
        }
        if !self.extensions.is_empty() {
            obj.insert(
                "extensions".to_owned(),
                JsonValue::Array(
                    self.extensions
                        .iter()
                        .cloned()
                        .map(JsonValue::String)
                        .collect(),
                ),
            );
        }
        JsonValue::Object(obj)
    }

    /// Decodes from JSON DOM.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorCode::ValidationError`] if fields are malformed.
    pub fn from_json_value(val: &JsonValue) -> GapResult<Self> {
        let obj = val.as_object().ok_or_else(|| {
            GapError::new(ErrorCode::ValidationError, "A2A Artifact must be object")
        })?;
        let artifact_id = obj
            .get("artifactId")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| {
                GapError::new(
                    ErrorCode::ValidationError,
                    "Artifact.artifactId is required",
                )
            })?
            .to_owned();
        let parts_arr = obj
            .get("parts")
            .and_then(JsonValue::as_array)
            .ok_or_else(|| {
                GapError::new(ErrorCode::ValidationError, "Artifact.parts is required")
            })?;
        let mut parts = Vec::with_capacity(parts_arr.len());
        for p in parts_arr {
            parts.push(A2aPart::from_json_value(p)?);
        }
        Ok(Self {
            artifact_id,
            name: opt_string(obj.get("name"))?,
            description: opt_string(obj.get("description"))?,
            parts,
            metadata: parse_optional_map(obj.get("metadata"))?,
            extensions: parse_optional_string_list(obj.get("extensions"))?,
        })
    }
}

/// A2A v1.0 `TaskStatus` (`{ state, message?, timestamp? }`).
#[derive(Debug, Clone, PartialEq)]
pub struct A2aTaskStatus {
    /// Current lifecycle state (`TASK_STATE_*`).
    pub state: A2aTaskState,
    /// Optional status message (`ROLE_AGENT`).
    pub message: Option<A2aMessage>,
    /// Optional RFC 3339 timestamp string.
    pub timestamp: Option<Timestamp>,
}

impl A2aTaskStatus {
    /// Encodes to JSON DOM.
    #[must_use]
    pub fn to_json_value(&self) -> JsonValue {
        let mut obj = BTreeMap::new();
        obj.insert(
            "state".to_owned(),
            JsonValue::String(self.state.as_wire_str().to_owned()),
        );
        if let Some(ref msg) = self.message {
            obj.insert("message".to_owned(), msg.to_json_value());
        }
        if let Some(ts) = self.timestamp {
            obj.insert("timestamp".to_owned(), JsonValue::String(ts.to_string()));
        }
        JsonValue::Object(obj)
    }

    /// Decodes from JSON DOM.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorCode::ValidationError`] if `state` or optional fields are invalid.
    pub fn from_json_value(val: &JsonValue) -> GapResult<Self> {
        let obj = val.as_object().ok_or_else(|| {
            GapError::new(ErrorCode::ValidationError, "A2A TaskStatus must be object")
        })?;
        let state_raw = obj
            .get("state")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| {
                GapError::new(ErrorCode::ValidationError, "TaskStatus.state is required")
            })?;
        let state = A2aTaskState::from_wire_str(state_raw)?;
        let message = match obj.get("message") {
            Some(m) => Some(A2aMessage::from_json_value(m)?),
            None => None,
        };
        let timestamp = match opt_string(obj.get("timestamp"))? {
            Some(s) => Some(s.parse::<Timestamp>().map_err(|e| {
                GapError::new(
                    ErrorCode::ValidationError,
                    format!("invalid RFC 3339 TaskStatus.timestamp: {e}"),
                )
            })?),
            None => None,
        };
        Ok(Self {
            state,
            message,
            timestamp,
        })
    }
}

/// Full A2A v1.0 `Task` representation.
#[derive(Debug, Clone, PartialEq)]
pub struct A2aTask {
    /// Required task identifier.
    pub id: String,
    /// Multi-turn context identifier (`CMP-021`).
    pub context_id: String,
    /// Current status.
    pub status: A2aTaskStatus,
    /// Output artifacts produced by the task.
    pub artifacts: Vec<A2aArtifact>,
    /// Message history.
    pub history: Vec<A2aMessage>,
    /// Extension-namespaced metadata map.
    pub metadata: BTreeMap<String, JsonValue>,
}

impl A2aTask {
    /// Encodes to JSON DOM.
    #[must_use]
    pub fn to_json_value(&self) -> JsonValue {
        let mut obj = BTreeMap::new();
        obj.insert("id".to_owned(), JsonValue::String(self.id.clone()));
        obj.insert(
            "contextId".to_owned(),
            JsonValue::String(self.context_id.clone()),
        );
        obj.insert("status".to_owned(), self.status.to_json_value());
        if !self.artifacts.is_empty() {
            obj.insert(
                "artifacts".to_owned(),
                JsonValue::Array(
                    self.artifacts
                        .iter()
                        .map(A2aArtifact::to_json_value)
                        .collect(),
                ),
            );
        }
        if !self.history.is_empty() {
            obj.insert(
                "history".to_owned(),
                JsonValue::Array(self.history.iter().map(A2aMessage::to_json_value).collect()),
            );
        }
        if !self.metadata.is_empty() {
            obj.insert(
                "metadata".to_owned(),
                JsonValue::Object(self.metadata.clone()),
            );
        }
        JsonValue::Object(obj)
    }

    /// Decodes an `A2aTask` from JSON DOM.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorCode::ValidationError`] if required fields are missing or invalid.
    pub fn from_json_value(val: &JsonValue) -> GapResult<Self> {
        let obj = val
            .as_object()
            .ok_or_else(|| GapError::new(ErrorCode::ValidationError, "A2A Task must be object"))?;
        let id = obj
            .get("id")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| GapError::new(ErrorCode::ValidationError, "Task.id is required"))?
            .to_owned();
        let context_id = obj
            .get("contextId")
            .and_then(JsonValue::as_str)
            .unwrap_or_default()
            .to_owned();
        let status_val = obj
            .get("status")
            .ok_or_else(|| GapError::new(ErrorCode::ValidationError, "Task.status is required"))?;
        let status = A2aTaskStatus::from_json_value(status_val)?;
        let artifacts = match obj.get("artifacts") {
            Some(a) => {
                let items = a.as_array().ok_or_else(|| {
                    GapError::new(ErrorCode::ValidationError, "Task.artifacts must be array")
                })?;
                let mut out = Vec::with_capacity(items.len());
                for item in items {
                    out.push(A2aArtifact::from_json_value(item)?);
                }
                out
            }
            None => Vec::new(),
        };
        let history = match obj.get("history") {
            Some(h) => {
                let items = h.as_array().ok_or_else(|| {
                    GapError::new(ErrorCode::ValidationError, "Task.history must be array")
                })?;
                let mut out = Vec::with_capacity(items.len());
                for item in items {
                    out.push(A2aMessage::from_json_value(item)?);
                }
                out
            }
            None => Vec::new(),
        };
        Ok(Self {
            id,
            context_id,
            status,
            artifacts,
            history,
            metadata: parse_optional_map(obj.get("metadata"))?,
        })
    }
}

/// A2A v1.0 `StreamResponse` oneof wrapper (`Q-14`: no `final` flag, no `event:` SSE line).
#[derive(Debug, Clone, PartialEq)]
pub enum A2aStreamEvent {
    /// Initial or updated task snapshot (`{ "task": ... }`).
    Task(A2aTask),
    /// Direct message event (`{ "message": ... }`).
    Message(A2aMessage),
    /// Incremental status transition (`{ "statusUpdate": ... }`).
    StatusUpdate {
        /// Task identifier.
        task_id: String,
        /// Context identifier.
        context_id: String,
        /// Updated task status.
        status: A2aTaskStatus,
        /// Optional extension metadata.
        metadata: BTreeMap<String, JsonValue>,
    },
    /// Incremental artifact chunk (`{ "artifactUpdate": ... }`).
    ArtifactUpdate {
        /// Task identifier.
        task_id: String,
        /// Context identifier.
        context_id: String,
        /// Output artifact.
        artifact: A2aArtifact,
        /// Append mode indicator.
        append: bool,
        /// Final chunk indicator.
        last_chunk: bool,
        /// Optional extension metadata.
        metadata: BTreeMap<String, JsonValue>,
    },
}

impl A2aStreamEvent {
    /// Encodes as an A2A `StreamResponse` JSON DOM object (`Q-14`).
    #[must_use]
    pub fn to_json_value(&self) -> JsonValue {
        let mut wrapper = BTreeMap::new();
        match self {
            Self::Task(task) => {
                wrapper.insert("task".to_owned(), task.to_json_value());
            }
            Self::Message(msg) => {
                wrapper.insert("message".to_owned(), msg.to_json_value());
            }
            Self::StatusUpdate {
                task_id,
                context_id,
                status,
                metadata,
            } => {
                let mut ev = BTreeMap::new();
                ev.insert("taskId".to_owned(), JsonValue::String(task_id.clone()));
                ev.insert(
                    "contextId".to_owned(),
                    JsonValue::String(context_id.clone()),
                );
                ev.insert("status".to_owned(), status.to_json_value());
                if !metadata.is_empty() {
                    ev.insert("metadata".to_owned(), JsonValue::Object(metadata.clone()));
                }
                wrapper.insert("statusUpdate".to_owned(), JsonValue::Object(ev));
            }
            Self::ArtifactUpdate {
                task_id,
                context_id,
                artifact,
                append,
                last_chunk,
                metadata,
            } => {
                let mut ev = BTreeMap::new();
                ev.insert("taskId".to_owned(), JsonValue::String(task_id.clone()));
                ev.insert(
                    "contextId".to_owned(),
                    JsonValue::String(context_id.clone()),
                );
                ev.insert("artifact".to_owned(), artifact.to_json_value());
                if *append {
                    ev.insert("append".to_owned(), JsonValue::Bool(true));
                }
                if *last_chunk {
                    ev.insert("lastChunk".to_owned(), JsonValue::Bool(true));
                }
                if !metadata.is_empty() {
                    ev.insert("metadata".to_owned(), JsonValue::Object(metadata.clone()));
                }
                wrapper.insert("artifactUpdate".to_owned(), JsonValue::Object(ev));
            }
        }
        JsonValue::Object(wrapper)
    }

    /// Formats this event as a single SSE frame (`data: <json>\n\n` with no `event:` line, `Q-14`).
    #[must_use]
    pub fn to_http_sse_frame(&self) -> String {
        format!("data: {}\n\n", self.to_json_value().to_jcs_string())
    }

    /// Formats this event wrapped in a JSON-RPC 2.0 streaming result frame (`Q-14`).
    #[must_use]
    pub fn to_jsonrpc_sse_frame(&self, id: &JsonValue) -> String {
        let mut envelope = BTreeMap::new();
        envelope.insert("jsonrpc".to_owned(), JsonValue::String("2.0".to_owned()));
        envelope.insert("id".to_owned(), id.clone());
        envelope.insert("result".to_owned(), self.to_json_value());
        format!("data: {}\n\n", JsonValue::Object(envelope).to_jcs_string())
    }
}

/// Converts a slice of [`A2aStreamEvent`]s into an HTTP+JSON SSE response body (`Q-14`).
#[must_use]
pub fn encode_http_sse_stream(events: &[A2aStreamEvent]) -> String {
    let mut out = String::new();
    for ev in events {
        out.push_str(&ev.to_http_sse_frame());
    }
    out
}

/// Converts a slice of [`A2aStreamEvent`]s into a JSON-RPC 2.0 SSE response body (`Q-14`).
#[must_use]
pub fn encode_jsonrpc_sse_stream(id: &JsonValue, events: &[A2aStreamEvent]) -> String {
    let mut out = String::new();
    for ev in events {
        out.push_str(&ev.to_jsonrpc_sse_frame(id));
    }
    out
}

pub(crate) fn parse_optional_map(
    val: Option<&JsonValue>,
) -> GapResult<BTreeMap<String, JsonValue>> {
    match val {
        None | Some(JsonValue::Null) => Ok(BTreeMap::new()),
        Some(other) => other.as_object().cloned().ok_or_else(|| {
            GapError::new(
                ErrorCode::ValidationError,
                "expected JSON object for metadata",
            )
        }),
    }
}

pub(crate) fn parse_optional_string_list(val: Option<&JsonValue>) -> GapResult<Vec<String>> {
    match val {
        None | Some(JsonValue::Null) => Ok(Vec::new()),
        Some(other) => {
            let arr = other.as_array().ok_or_else(|| {
                GapError::new(ErrorCode::ValidationError, "expected JSON string array")
            })?;
            let mut out = Vec::with_capacity(arr.len());
            for item in arr {
                let s = item.as_str().ok_or_else(|| {
                    GapError::new(
                        ErrorCode::ValidationError,
                        "array items must be JSON strings",
                    )
                })?;
                out.push(s.to_owned());
            }
            Ok(out)
        }
    }
}

pub(crate) fn opt_string(val: Option<&JsonValue>) -> GapResult<Option<String>> {
    match val {
        None | Some(JsonValue::Null) => Ok(None),
        Some(other) => other
            .as_str()
            .map(|s| Some(s.to_owned()))
            .ok_or_else(|| GapError::new(ErrorCode::ValidationError, "expected JSON string")),
    }
}
