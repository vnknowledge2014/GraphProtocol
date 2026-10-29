//! Canonical Protobuf JSON codec backed by a process-wide [`prost_reflect::DescriptorPool`] (`NGC-002`, `NGC-003`).

use std::sync::OnceLock;

use gap_core::error::{NgcErrorCode, NgcErrorInfo};
use prost_reflect::{
    DescriptorPool, DeserializeOptions, DynamicMessage, Kind, MessageDescriptor,
    ReflectMessage as _, SerializeOptions,
};

use crate::connector::NgcResult;

static DESCRIPTOR_POOL: OnceLock<DescriptorPool> = OnceLock::new();

/// Returns the process-wide [`DescriptorPool`] initialized once from [`gap_proto::DESCRIPTOR_SET`].
#[must_use]
pub fn descriptor_pool() -> &'static DescriptorPool {
    DESCRIPTOR_POOL.get_or_init(|| {
        let mut pool = DescriptorPool::global();
        let _ = pool.decode_file_descriptor_set(gap_proto::DESCRIPTOR_SET);
        pool
    })
}

/// Associates a static `prost` message type with its fully-qualified Protobuf descriptor name.
pub trait ProtoMessage: prost::Message + Default {
    /// Fully-qualified Protobuf message name (e.g. `"gap.ngc.v1.QueryRequest"`).
    const FULL_NAME: &'static str;
}

macro_rules! impl_proto_messages {
    ($($ty:ty => $full_name:literal),* $(,)?) => {
        $(
            impl ProtoMessage for $ty {
                const FULL_NAME: &'static str = $full_name;
            }
        )*
    };
}

impl_proto_messages! {
    // google.protobuf Well-Known Types used in GAP/NGC contracts
    prost_types::Struct => "google.protobuf.Struct",
    prost_types::Value => "google.protobuf.Value",
    prost_types::ListValue => "google.protobuf.ListValue",
    prost_types::Timestamp => "google.protobuf.Timestamp",

    // gap.gateway.v1 (2 messages)
    gap_proto::gateway::v1::ExecuteRequest => "gap.gateway.v1.ExecuteRequest",
    gap_proto::gateway::v1::ExecuteResponse => "gap.gateway.v1.ExecuteResponse",

    // gap.ngc.v1 (40 messages)
    gap_proto::ngc::v1::ApprovalProof => "gap.ngc.v1.ApprovalProof",
    gap_proto::ngc::v1::Artifact => "gap.ngc.v1.Artifact",
    gap_proto::ngc::v1::AuthRequirements => "gap.ngc.v1.AuthRequirements",
    gap_proto::ngc::v1::CallContext => "gap.ngc.v1.CallContext",
    gap_proto::ngc::v1::CancelTaskRequest => "gap.ngc.v1.CancelTaskRequest",
    gap_proto::ngc::v1::Capabilities => "gap.ngc.v1.Capabilities",
    gap_proto::ngc::v1::Cmp => "gap.ngc.v1.Cmp",
    gap_proto::ngc::v1::Complete => "gap.ngc.v1.Complete",
    gap_proto::ngc::v1::ConnectorDescription => "gap.ngc.v1.ConnectorDescription",
    gap_proto::ngc::v1::DescribeRequest => "gap.ngc.v1.DescribeRequest",
    gap_proto::ngc::v1::DescribeResponse => "gap.ngc.v1.DescribeResponse",
    gap_proto::ngc::v1::Event => "gap.ngc.v1.Event",
    gap_proto::ngc::v1::Example => "gap.ngc.v1.Example",
    gap_proto::ngc::v1::ExplainResponse => "gap.ngc.v1.ExplainResponse",
    gap_proto::ngc::v1::Expr => "gap.ngc.v1.Expr",
    gap_proto::ngc::v1::ExprList => "gap.ngc.v1.ExprList",
    gap_proto::ngc::v1::GetTaskRequest => "gap.ngc.v1.GetTaskRequest",
    gap_proto::ngc::v1::HealthRequest => "gap.ngc.v1.HealthRequest",
    gap_proto::ngc::v1::HealthResponse => "gap.ngc.v1.HealthResponse",
    gap_proto::ngc::v1::Limits => "gap.ngc.v1.Limits",
    gap_proto::ngc::v1::MetaSummary => "gap.ngc.v1.MetaSummary",
    gap_proto::ngc::v1::MutateRequest => "gap.ngc.v1.MutateRequest",
    gap_proto::ngc::v1::MutateResponse => "gap.ngc.v1.MutateResponse",
    gap_proto::ngc::v1::NgcError => "gap.ngc.v1.NgcError",
    gap_proto::ngc::v1::OperationDescriptor => "gap.ngc.v1.OperationDescriptor",
    gap_proto::ngc::v1::OrderBy => "gap.ngc.v1.OrderBy",
    gap_proto::ngc::v1::Page => "gap.ngc.v1.Page",
    gap_proto::ngc::v1::Principal => "gap.ngc.v1.Principal",
    gap_proto::ngc::v1::QueryRequest => "gap.ngc.v1.QueryRequest",
    gap_proto::ngc::v1::QueryResponse => "gap.ngc.v1.QueryResponse",
    gap_proto::ngc::v1::RowSetChunk => "gap.ngc.v1.RowSetChunk",
    gap_proto::ngc::v1::Selection => "gap.ngc.v1.Selection",
    gap_proto::ngc::v1::SelectionField => "gap.ngc.v1.SelectionField",
    gap_proto::ngc::v1::SketchRequest => "gap.ngc.v1.SketchRequest",
    gap_proto::ngc::v1::SketchResponse => "gap.ngc.v1.SketchResponse",
    gap_proto::ngc::v1::StartTaskRequest => "gap.ngc.v1.StartTaskRequest",
    gap_proto::ngc::v1::SubscribeRequest => "gap.ngc.v1.SubscribeRequest",
    gap_proto::ngc::v1::TaskStatus => "gap.ngc.v1.TaskStatus",
    gap_proto::ngc::v1::VariableSet => "gap.ngc.v1.VariableSet",
    gap_proto::ngc::v1::WatchTaskRequest => "gap.ngc.v1.WatchTaskRequest",
}

/// Resolves the [`MessageDescriptor`] for static message `M` from [`descriptor_pool`].
///
/// # Errors
///
/// Returns `INTERNAL` if the descriptor is missing from [`gap_proto::DESCRIPTOR_SET`].
pub fn message_descriptor<M: ProtoMessage>() -> NgcResult<MessageDescriptor> {
    descriptor_pool()
        .get_message_by_name(M::FULL_NAME)
        .ok_or_else(|| {
            NgcErrorInfo::new(
                NgcErrorCode::Internal,
                format!("missing message descriptor for {}", M::FULL_NAME),
            )
        })
}

fn canonical_serialize_options() -> SerializeOptions {
    SerializeOptions::new()
        .stringify_64_bit_integers(true)
        .use_enum_numbers(false)
        .use_proto_field_name(false)
        .skip_default_fields(true)
}

/// Serializes a [`DynamicMessage`] into canonical Protobuf JSON [`serde_json::Value`] (`NGC-002`).
///
/// # Errors
///
/// Returns `INTERNAL` if canonical JSON serialization fails (for example, an out-of-range `Timestamp`).
pub fn dynamic_to_json_value(msg: &DynamicMessage) -> NgcResult<serde_json::Value> {
    msg.serialize_with_options(
        serde_json::value::Serializer,
        &canonical_serialize_options(),
    )
    .map_err(|err| {
        NgcErrorInfo::new(
            NgcErrorCode::Internal,
            format!(
                "failed to serialize {} to JSON: {err}",
                msg.descriptor().full_name()
            ),
        )
    })
}

/// Serializes a [`DynamicMessage`] into canonical Protobuf JSON UTF-8 bytes (`NGC-002`).
///
/// # Errors
///
/// Returns `INTERNAL` if canonical JSON serialization fails.
pub fn dynamic_to_json_vec(msg: &DynamicMessage) -> NgcResult<Vec<u8>> {
    let mut out = Vec::new();
    let mut ser = serde_json::Serializer::new(&mut out);
    msg.serialize_with_options(&mut ser, &canonical_serialize_options())
        .map_err(|err| {
            NgcErrorInfo::new(
                NgcErrorCode::Internal,
                format!(
                    "failed to serialize {} to JSON bytes: {err}",
                    msg.descriptor().full_name()
                ),
            )
        })?;
    Ok(out)
}

/// Recursively strips unknown message keys while leaving `google.protobuf.*` Well-Known Types
/// (`Struct`, `Value`, `ListValue`, `Timestamp`, etc.) intact so arbitrary `Struct` user keys
/// survive and `prost_reflect` can run with `deny_unknown_fields(true)` — rejecting unknown enum
/// string names and wrong JSON types with `INVALID_ARGUMENT` (`NGC-002`).
fn prune_unknown_fields(desc: &MessageDescriptor, val: &mut serde_json::Value) {
    if desc.full_name().starts_with("google.protobuf.") {
        return;
    }

    let serde_json::Value::Object(map) = val else {
        return;
    };

    let unknown_keys: Vec<String> = map
        .keys()
        .filter(|k| {
            desc.get_field_by_json_name(k)
                .or_else(|| desc.get_field_by_name(k))
                .is_none()
        })
        .cloned()
        .collect();

    for key in unknown_keys {
        map.remove(&key);
    }

    for (key, field_val) in map.iter_mut() {
        let Some(field) = desc
            .get_field_by_json_name(key)
            .or_else(|| desc.get_field_by_name(key))
        else {
            continue;
        };

        if field.is_map() {
            if let Some(entry_desc) = field.kind().as_message()
                && let Kind::Message(val_msg_desc) = entry_desc.map_entry_value_field().kind()
                && let serde_json::Value::Object(entries) = field_val
            {
                for entry_val in entries.values_mut() {
                    prune_unknown_fields(&val_msg_desc, entry_val);
                }
            }
        } else if field.is_list() {
            if let Kind::Message(elem_msg_desc) = field.kind()
                && let serde_json::Value::Array(items) = field_val
            {
                for item in items.iter_mut() {
                    prune_unknown_fields(&elem_msg_desc, item);
                }
            }
        } else if let Kind::Message(child_msg_desc) = field.kind() {
            prune_unknown_fields(&child_msg_desc, field_val);
        }
    }
}

/// Deserializes a [`DynamicMessage`] from JSON, ignoring unknown message fields while rejecting
/// unknown enum names and wrong value types with `INVALID_ARGUMENT` (`NGC-002`).
///
/// # Errors
///
/// Returns `INVALID_ARGUMENT` if `value` violates the Protobuf JSON mapping for `desc`.
pub fn dynamic_from_json_value(
    desc: &MessageDescriptor,
    mut value: serde_json::Value,
) -> NgcResult<DynamicMessage> {
    prune_unknown_fields(desc, &mut value);
    let opts = DeserializeOptions::new().deny_unknown_fields(true);
    DynamicMessage::deserialize_with_options(desc.clone(), &value, &opts).map_err(|err| {
        NgcErrorInfo::new(
            NgcErrorCode::InvalidArgument,
            format!("invalid JSON for {}: {err}", desc.full_name()),
        )
    })
}

fn static_to_dynamic<M: ProtoMessage>(msg: &M) -> NgcResult<DynamicMessage> {
    let desc = message_descriptor::<M>()?;
    let wire = msg.encode_to_vec();
    DynamicMessage::decode(desc, wire.as_slice()).map_err(|err| {
        NgcErrorInfo::new(
            NgcErrorCode::Internal,
            format!(
                "failed to transcode {} to DynamicMessage: {err}",
                M::FULL_NAME
            ),
        )
    })
}

fn dynamic_to_static<M: ProtoMessage>(dyn_msg: &DynamicMessage) -> NgcResult<M> {
    dyn_msg.transcode_to::<M>().map_err(|err| {
        NgcErrorInfo::new(
            NgcErrorCode::InvalidArgument,
            format!(
                "failed to transcode DynamicMessage to {}: {err}",
                M::FULL_NAME
            ),
        )
    })
}

/// Converts a static `prost` message into canonical Protobuf JSON [`serde_json::Value`] (`NGC-002`).
///
/// # Errors
///
/// Returns an [`NgcErrorInfo`] if descriptor resolution or canonical serialization fails.
pub fn to_json_value<M: ProtoMessage>(msg: &M) -> NgcResult<serde_json::Value> {
    let dyn_msg = static_to_dynamic(msg)?;
    dynamic_to_json_value(&dyn_msg)
}

/// Converts a static `prost` message into canonical Protobuf JSON UTF-8 bytes (`NGC-002`).
///
/// # Errors
///
/// Returns an [`NgcErrorInfo`] if descriptor resolution or canonical serialization fails.
pub fn to_json_vec<M: ProtoMessage>(msg: &M) -> NgcResult<Vec<u8>> {
    let dyn_msg = static_to_dynamic(msg)?;
    dynamic_to_json_vec(&dyn_msg)
}

/// Converts a static `prost` message into a canonical Protobuf JSON [`String`] (`NGC-002`).
///
/// # Errors
///
/// Returns an [`NgcErrorInfo`] if descriptor resolution or canonical serialization fails.
pub fn to_json_string<M: ProtoMessage>(msg: &M) -> NgcResult<String> {
    let bytes = to_json_vec(msg)?;
    String::from_utf8(bytes).map_err(|err| {
        NgcErrorInfo::new(
            NgcErrorCode::Internal,
            format!("non-UTF-8 JSON output for {}: {err}", M::FULL_NAME),
        )
    })
}

/// Parses a static `prost` message from a [`serde_json::Value`] (`NGC-002`).
///
/// # Errors
///
/// Returns `INVALID_ARGUMENT` if `value` fails Protobuf JSON validation.
pub fn from_json_value<M: ProtoMessage>(value: serde_json::Value) -> NgcResult<M> {
    let desc = message_descriptor::<M>()?;
    let dyn_msg = dynamic_from_json_value(&desc, value)?;
    dynamic_to_static(&dyn_msg)
}

/// Parses a static `prost` message from UTF-8 JSON bytes (`NGC-002`).
///
/// # Errors
///
/// Returns `INVALID_ARGUMENT` if `bytes` is invalid JSON or fails Protobuf JSON validation.
pub fn from_json_slice<M: ProtoMessage>(bytes: &[u8]) -> NgcResult<M> {
    let value: serde_json::Value = serde_json::from_slice(bytes).map_err(|err| {
        NgcErrorInfo::new(
            NgcErrorCode::InvalidArgument,
            format!("malformed JSON for {}: {err}", M::FULL_NAME),
        )
    })?;
    from_json_value(value)
}

/// Parses a static `prost` message from a JSON string (`NGC-002`).
///
/// # Errors
///
/// Returns `INVALID_ARGUMENT` if `text` is invalid JSON or fails Protobuf JSON validation.
pub fn from_json_str<M: ProtoMessage>(text: &str) -> NgcResult<M> {
    from_json_slice(text.as_bytes())
}
