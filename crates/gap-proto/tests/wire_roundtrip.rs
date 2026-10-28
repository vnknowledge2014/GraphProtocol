//! Dynamic-to-static-to-dynamic wire compatibility, registry completeness, and negative decode tests.
#![forbid(unsafe_code)]

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::panic::{AssertUnwindSafe, catch_unwind};

use proptest::prelude::*;
use proptest::strategy::Union;
use proptest::test_runner::{Config as ProptestConfig, TestRunner};
use prost::Message as _;
use prost::bytes::Bytes;
use prost_reflect::{
    DescriptorPool, DynamicMessage, FieldDescriptor, Kind, MapKey, MessageDescriptor,
    OneofDescriptor, Value,
};
use prost_types::FileDescriptorSet;

const CASES_PER_MESSAGE: u32 = 256;
const MAX_RECURSION_DEPTH: usize = 3;

// The WP-M0-06 brief (section 3.5) fixes these floors; lowering either silently weakens the suite.
const _: () = assert!(CASES_PER_MESSAGE >= 256);
const _: () = assert!(MAX_RECURSION_DEPTH >= 3);

const TIMESTAMP_MIN_SECONDS: i64 = -62_135_596_800; // 0001-01-01T00:00:00Z
const TIMESTAMP_MAX_SECONDS: i64 = 253_402_300_799; // 9999-12-31T23:59:59Z
const TIMESTAMP_MAX_NANOS: i32 = 999_999_999;

const UNKNOWN_VARINT_FIELD_NUMBER: u32 = 29_999;
const UNKNOWN_BYTES_FIELD_NUMBER: u32 = 29_998;
const UNKNOWN_FIXED64_FIELD_NUMBER: u32 = 29_997;

#[derive(Clone, Copy)]
struct StaticMessageOps {
    full_name: &'static str,
    roundtrip: fn(&[u8]) -> Result<Vec<u8>, String>,
    assert_unknown_ignored: fn(&[u8], &[u8]) -> Result<(), String>,
    decode_truncated: fn(&[u8]) -> Result<bool, String>,
}

fn static_roundtrip<T>(dyn_encoded: &[u8]) -> Result<Vec<u8>, String>
where
    T: prost::Message + Default + PartialEq + std::fmt::Debug,
{
    let static_val = T::decode(dyn_encoded).map_err(|err| {
        format!(
            "static decode failed for {}: {err}",
            std::any::type_name::<T>()
        )
    })?;
    let static_encoded = static_val.encode_to_vec();
    let static_redecoded = T::decode(static_encoded.as_slice()).map_err(|err| {
        format!(
            "static re-decode failed for {}: {err}",
            std::any::type_name::<T>()
        )
    })?;
    if static_val != static_redecoded {
        return Err(format!(
            "static encode_to_vec -> decode PartialEq mismatch for {}: decoded={static_val:?}, redecoded={static_redecoded:?}",
            std::any::type_name::<T>()
        ));
    }
    Ok(static_encoded)
}

fn static_assert_unknown_ignored<T>(
    base_encoded: &[u8],
    with_unknown_encoded: &[u8],
) -> Result<(), String>
where
    T: prost::Message + Default + PartialEq + std::fmt::Debug,
{
    let base_val = T::decode(base_encoded).map_err(|err| {
        format!(
            "base decode failed for {}: {err}",
            std::any::type_name::<T>()
        )
    })?;
    let with_unknown_val = T::decode(with_unknown_encoded).map_err(|err| {
        format!(
            "decode with appended unknown field failed for {}: {err}",
            std::any::type_name::<T>()
        )
    })?;
    if base_val != with_unknown_val {
        return Err(format!(
            "appended unknown field changed static value for {}: base={base_val:?}, with_unknown={with_unknown_val:?}",
            std::any::type_name::<T>()
        ));
    }
    Ok(())
}

fn static_decode_truncated<T>(truncated: &[u8]) -> Result<bool, String>
where
    T: prost::Message + Default,
{
    match catch_unwind(AssertUnwindSafe(|| T::decode(truncated))) {
        Ok(Err(_)) => Ok(true),
        Ok(Ok(_)) => Ok(false),
        Err(_) => Err(format!(
            "static decode panicked for {} on {}-byte truncated buffer",
            std::any::type_name::<T>(),
            truncated.len()
        )),
    }
}

macro_rules! register_messages {
    ($($full_name:literal => $rust_ty:path),* $(,)?) => {
        vec![
            $(
                StaticMessageOps {
                    full_name: $full_name,
                    roundtrip: static_roundtrip::<$rust_ty>,
                    assert_unknown_ignored: static_assert_unknown_ignored::<$rust_ty>,
                    decode_truncated: static_decode_truncated::<$rust_ty>,
                },
            )*
        ]
    };
}

fn own_message_registry() -> Vec<StaticMessageOps> {
    register_messages! {
        // gap.gateway.v1 (2 messages)
        "gap.gateway.v1.ExecuteRequest" => gap_proto::gateway::v1::ExecuteRequest,
        "gap.gateway.v1.ExecuteResponse" => gap_proto::gateway::v1::ExecuteResponse,

        // gap.ngc.v1 (40 messages)
        "gap.ngc.v1.ApprovalProof" => gap_proto::ngc::v1::ApprovalProof,
        "gap.ngc.v1.Artifact" => gap_proto::ngc::v1::Artifact,
        "gap.ngc.v1.AuthRequirements" => gap_proto::ngc::v1::AuthRequirements,
        "gap.ngc.v1.CallContext" => gap_proto::ngc::v1::CallContext,
        "gap.ngc.v1.CancelTaskRequest" => gap_proto::ngc::v1::CancelTaskRequest,
        "gap.ngc.v1.Capabilities" => gap_proto::ngc::v1::Capabilities,
        "gap.ngc.v1.Cmp" => gap_proto::ngc::v1::Cmp,
        "gap.ngc.v1.Complete" => gap_proto::ngc::v1::Complete,
        "gap.ngc.v1.ConnectorDescription" => gap_proto::ngc::v1::ConnectorDescription,
        "gap.ngc.v1.DescribeRequest" => gap_proto::ngc::v1::DescribeRequest,
        "gap.ngc.v1.DescribeResponse" => gap_proto::ngc::v1::DescribeResponse,
        "gap.ngc.v1.Event" => gap_proto::ngc::v1::Event,
        "gap.ngc.v1.Example" => gap_proto::ngc::v1::Example,
        "gap.ngc.v1.ExplainResponse" => gap_proto::ngc::v1::ExplainResponse,
        "gap.ngc.v1.Expr" => gap_proto::ngc::v1::Expr,
        "gap.ngc.v1.ExprList" => gap_proto::ngc::v1::ExprList,
        "gap.ngc.v1.GetTaskRequest" => gap_proto::ngc::v1::GetTaskRequest,
        "gap.ngc.v1.HealthRequest" => gap_proto::ngc::v1::HealthRequest,
        "gap.ngc.v1.HealthResponse" => gap_proto::ngc::v1::HealthResponse,
        "gap.ngc.v1.Limits" => gap_proto::ngc::v1::Limits,
        "gap.ngc.v1.MetaSummary" => gap_proto::ngc::v1::MetaSummary,
        "gap.ngc.v1.MutateRequest" => gap_proto::ngc::v1::MutateRequest,
        "gap.ngc.v1.MutateResponse" => gap_proto::ngc::v1::MutateResponse,
        "gap.ngc.v1.NgcError" => gap_proto::ngc::v1::NgcError,
        "gap.ngc.v1.OperationDescriptor" => gap_proto::ngc::v1::OperationDescriptor,
        "gap.ngc.v1.OrderBy" => gap_proto::ngc::v1::OrderBy,
        "gap.ngc.v1.Page" => gap_proto::ngc::v1::Page,
        "gap.ngc.v1.Principal" => gap_proto::ngc::v1::Principal,
        "gap.ngc.v1.QueryRequest" => gap_proto::ngc::v1::QueryRequest,
        "gap.ngc.v1.QueryResponse" => gap_proto::ngc::v1::QueryResponse,
        "gap.ngc.v1.RowSetChunk" => gap_proto::ngc::v1::RowSetChunk,
        "gap.ngc.v1.Selection" => gap_proto::ngc::v1::Selection,
        "gap.ngc.v1.SelectionField" => gap_proto::ngc::v1::SelectionField,
        "gap.ngc.v1.SketchRequest" => gap_proto::ngc::v1::SketchRequest,
        "gap.ngc.v1.SketchResponse" => gap_proto::ngc::v1::SketchResponse,
        "gap.ngc.v1.StartTaskRequest" => gap_proto::ngc::v1::StartTaskRequest,
        "gap.ngc.v1.SubscribeRequest" => gap_proto::ngc::v1::SubscribeRequest,
        "gap.ngc.v1.TaskStatus" => gap_proto::ngc::v1::TaskStatus,
        "gap.ngc.v1.VariableSet" => gap_proto::ngc::v1::VariableSet,
        "gap.ngc.v1.WatchTaskRequest" => gap_proto::ngc::v1::WatchTaskRequest,
    }
}

#[cfg(feature = "a2a")]
#[allow(deprecated)]
fn a2a_message_registry() -> Vec<StaticMessageOps> {
    register_messages! {
        // lf.a2a.v1 (44 messages)
        "lf.a2a.v1.APIKeySecurityScheme" => gap_proto::a2a::v1::ApiKeySecurityScheme,
        "lf.a2a.v1.AgentCapabilities" => gap_proto::a2a::v1::AgentCapabilities,
        "lf.a2a.v1.AgentCard" => gap_proto::a2a::v1::AgentCard,
        "lf.a2a.v1.AgentCardSignature" => gap_proto::a2a::v1::AgentCardSignature,
        "lf.a2a.v1.AgentExtension" => gap_proto::a2a::v1::AgentExtension,
        "lf.a2a.v1.AgentInterface" => gap_proto::a2a::v1::AgentInterface,
        "lf.a2a.v1.AgentProvider" => gap_proto::a2a::v1::AgentProvider,
        "lf.a2a.v1.AgentSkill" => gap_proto::a2a::v1::AgentSkill,
        "lf.a2a.v1.Artifact" => gap_proto::a2a::v1::Artifact,
        "lf.a2a.v1.AuthenticationInfo" => gap_proto::a2a::v1::AuthenticationInfo,
        "lf.a2a.v1.AuthorizationCodeOAuthFlow" => gap_proto::a2a::v1::AuthorizationCodeOAuthFlow,
        "lf.a2a.v1.CancelTaskRequest" => gap_proto::a2a::v1::CancelTaskRequest,
        "lf.a2a.v1.ClientCredentialsOAuthFlow" => gap_proto::a2a::v1::ClientCredentialsOAuthFlow,
        "lf.a2a.v1.DeleteTaskPushNotificationConfigRequest" => gap_proto::a2a::v1::DeleteTaskPushNotificationConfigRequest,
        "lf.a2a.v1.DeviceCodeOAuthFlow" => gap_proto::a2a::v1::DeviceCodeOAuthFlow,
        "lf.a2a.v1.GetExtendedAgentCardRequest" => gap_proto::a2a::v1::GetExtendedAgentCardRequest,
        "lf.a2a.v1.GetTaskPushNotificationConfigRequest" => gap_proto::a2a::v1::GetTaskPushNotificationConfigRequest,
        "lf.a2a.v1.GetTaskRequest" => gap_proto::a2a::v1::GetTaskRequest,
        "lf.a2a.v1.HTTPAuthSecurityScheme" => gap_proto::a2a::v1::HttpAuthSecurityScheme,
        "lf.a2a.v1.ImplicitOAuthFlow" => gap_proto::a2a::v1::ImplicitOAuthFlow,
        "lf.a2a.v1.ListTaskPushNotificationConfigsRequest" => gap_proto::a2a::v1::ListTaskPushNotificationConfigsRequest,
        "lf.a2a.v1.ListTaskPushNotificationConfigsResponse" => gap_proto::a2a::v1::ListTaskPushNotificationConfigsResponse,
        "lf.a2a.v1.ListTasksRequest" => gap_proto::a2a::v1::ListTasksRequest,
        "lf.a2a.v1.ListTasksResponse" => gap_proto::a2a::v1::ListTasksResponse,
        "lf.a2a.v1.Message" => gap_proto::a2a::v1::Message,
        "lf.a2a.v1.MutualTlsSecurityScheme" => gap_proto::a2a::v1::MutualTlsSecurityScheme,
        "lf.a2a.v1.OAuth2SecurityScheme" => gap_proto::a2a::v1::OAuth2SecurityScheme,
        "lf.a2a.v1.OAuthFlows" => gap_proto::a2a::v1::OAuthFlows,
        "lf.a2a.v1.OpenIdConnectSecurityScheme" => gap_proto::a2a::v1::OpenIdConnectSecurityScheme,
        "lf.a2a.v1.Part" => gap_proto::a2a::v1::Part,
        "lf.a2a.v1.PasswordOAuthFlow" => gap_proto::a2a::v1::PasswordOAuthFlow,
        "lf.a2a.v1.SecurityRequirement" => gap_proto::a2a::v1::SecurityRequirement,
        "lf.a2a.v1.SecurityScheme" => gap_proto::a2a::v1::SecurityScheme,
        "lf.a2a.v1.SendMessageConfiguration" => gap_proto::a2a::v1::SendMessageConfiguration,
        "lf.a2a.v1.SendMessageRequest" => gap_proto::a2a::v1::SendMessageRequest,
        "lf.a2a.v1.SendMessageResponse" => gap_proto::a2a::v1::SendMessageResponse,
        "lf.a2a.v1.StreamResponse" => gap_proto::a2a::v1::StreamResponse,
        "lf.a2a.v1.StringList" => gap_proto::a2a::v1::StringList,
        "lf.a2a.v1.SubscribeToTaskRequest" => gap_proto::a2a::v1::SubscribeToTaskRequest,
        "lf.a2a.v1.Task" => gap_proto::a2a::v1::Task,
        "lf.a2a.v1.TaskArtifactUpdateEvent" => gap_proto::a2a::v1::TaskArtifactUpdateEvent,
        "lf.a2a.v1.TaskPushNotificationConfig" => gap_proto::a2a::v1::TaskPushNotificationConfig,
        "lf.a2a.v1.TaskStatus" => gap_proto::a2a::v1::TaskStatus,
        "lf.a2a.v1.TaskStatusUpdateEvent" => gap_proto::a2a::v1::TaskStatusUpdateEvent,
    }
}

fn load_own_descriptors() -> (DescriptorPool, BTreeMap<String, MessageDescriptor>) {
    let fds = FileDescriptorSet::decode(gap_proto::DESCRIPTOR_SET)
        .expect("decode DESCRIPTOR_SET as FileDescriptorSet");
    let file_names: Vec<String> = fds.file.iter().map(|f| f.name().to_owned()).collect();
    let mut pool = DescriptorPool::global();
    pool.decode_file_descriptor_set(gap_proto::DESCRIPTOR_SET)
        .expect("load DESCRIPTOR_SET into DescriptorPool");
    let messages = collect_non_map_messages(&pool, &file_names);
    (pool, messages)
}

#[cfg(feature = "a2a")]
fn load_a2a_descriptors() -> (DescriptorPool, BTreeMap<String, MessageDescriptor>) {
    const A2A_IMPORTS_FDS: &[u8] = include_bytes!(env!("GAP_PROTO_A2A_IMPORTS_FDS"));
    let fds = FileDescriptorSet::decode(gap_proto::A2A_DESCRIPTOR_SET)
        .expect("decode A2A_DESCRIPTOR_SET as FileDescriptorSet");
    let file_names: Vec<String> = fds.file.iter().map(|f| f.name().to_owned()).collect();
    let mut pool = DescriptorPool::global();
    pool.decode_file_descriptor_set(A2A_IMPORTS_FDS)
        .expect("load google/api descriptor prerequisites into DescriptorPool");
    pool.decode_file_descriptor_set(gap_proto::A2A_DESCRIPTOR_SET)
        .expect("load A2A_DESCRIPTOR_SET into DescriptorPool");
    let messages = collect_non_map_messages(&pool, &file_names);
    (pool, messages)
}

fn collect_non_map_messages(
    pool: &DescriptorPool,
    file_names: &[String],
) -> BTreeMap<String, MessageDescriptor> {
    let mut out = BTreeMap::new();
    for file_name in file_names {
        let file = pool
            .get_file_by_name(file_name)
            .unwrap_or_else(|| panic!("missing file {file_name} in DescriptorPool"));
        for msg in file.messages() {
            collect_message_recursive(&msg, &mut out);
        }
    }
    out
}

fn collect_message_recursive(
    msg: &MessageDescriptor,
    out: &mut BTreeMap<String, MessageDescriptor>,
) {
    if !msg.is_map_entry() {
        out.insert(msg.full_name().to_owned(), msg.clone());
    }
    for child in msg.child_messages() {
        collect_message_recursive(&child, out);
    }
}

fn build_registry_map(entries: &[StaticMessageOps]) -> BTreeMap<String, StaticMessageOps> {
    let mut map = BTreeMap::new();
    for entry in entries {
        let prev = map.insert(entry.full_name.to_owned(), *entry);
        assert!(
            prev.is_none(),
            "duplicate message registration in wire_roundtrip registry: {}",
            entry.full_name
        );
    }
    map
}

fn assert_registry_matches_descriptor(
    registry: &BTreeMap<String, StaticMessageOps>,
    descriptors: &BTreeMap<String, MessageDescriptor>,
    label: &str,
) {
    let reg_names: BTreeSet<&str> = registry.keys().map(String::as_str).collect();
    let desc_names: BTreeSet<&str> = descriptors.keys().map(String::as_str).collect();

    let missing_in_registry: Vec<&&str> = desc_names.difference(&reg_names).collect();
    let stale_in_registry: Vec<&&str> = reg_names.difference(&desc_names).collect();

    assert!(
        missing_in_registry.is_empty() && stale_in_registry.is_empty(),
        "{label} wire_roundtrip registry does not match descriptor message set!\n\
         Missing registrations (in descriptor, absent from registry): {missing_in_registry:?}\n\
         Stale registrations (in registry, absent from descriptor): {stale_in_registry:?}"
    );
}

fn set_canonical(msg: &mut DynamicMessage, field: &FieldDescriptor, value: Value) {
    msg.set_field(field, value);
    if !msg.has_field(field) {
        msg.clear_field(field);
    }
}

fn finite_f32_strategy() -> impl Strategy<Value = f32> {
    let mask = proptest::num::f32::POSITIVE
        | proptest::num::f32::NEGATIVE
        | proptest::num::f32::NORMAL
        | proptest::num::f32::SUBNORMAL
        | proptest::num::f32::ZERO;
    mask.prop_map(|v| if v.is_finite() { v } else { 0.0 })
}

fn finite_f64_strategy() -> impl Strategy<Value = f64> {
    let mask = proptest::num::f64::POSITIVE
        | proptest::num::f64::NEGATIVE
        | proptest::num::f64::NORMAL
        | proptest::num::f64::SUBNORMAL
        | proptest::num::f64::ZERO;
    mask.prop_map(|v| if v.is_finite() { v } else { 0.0 })
}

fn unicode_string_strategy() -> impl Strategy<Value = String> {
    proptest::collection::vec(any::<char>(), 0..=24)
        .prop_map(|chars| chars.into_iter().collect::<String>())
}

fn bytes_strategy() -> impl Strategy<Value = Bytes> {
    proptest::collection::vec(any::<u8>(), 0..=32).prop_map(Bytes::from)
}

fn map_key_strategy(kind: &Kind) -> BoxedStrategy<MapKey> {
    match kind {
        Kind::Bool => any::<bool>().prop_map(MapKey::Bool).boxed(),
        Kind::Int32 | Kind::Sint32 | Kind::Sfixed32 => any::<i32>().prop_map(MapKey::I32).boxed(),
        Kind::Int64 | Kind::Sint64 | Kind::Sfixed64 => any::<i64>().prop_map(MapKey::I64).boxed(),
        Kind::Uint32 | Kind::Fixed32 => any::<u32>().prop_map(MapKey::U32).boxed(),
        Kind::Uint64 | Kind::Fixed64 => any::<u64>().prop_map(MapKey::U64).boxed(),
        Kind::String => unicode_string_strategy().prop_map(MapKey::String).boxed(),
        other => panic!("unsupported protobuf map key kind: {other:?}"),
    }
}

fn scalar_or_enum_strategy(kind: &Kind) -> BoxedStrategy<Value> {
    match kind {
        Kind::Double => finite_f64_strategy().prop_map(Value::F64).boxed(),
        Kind::Float => finite_f32_strategy().prop_map(Value::F32).boxed(),
        Kind::Int32 | Kind::Sint32 | Kind::Sfixed32 => any::<i32>().prop_map(Value::I32).boxed(),
        Kind::Int64 | Kind::Sint64 | Kind::Sfixed64 => any::<i64>().prop_map(Value::I64).boxed(),
        Kind::Uint32 | Kind::Fixed32 => any::<u32>().prop_map(Value::U32).boxed(),
        Kind::Uint64 | Kind::Fixed64 => any::<u64>().prop_map(Value::U64).boxed(),
        Kind::Bool => any::<bool>().prop_map(Value::Bool).boxed(),
        Kind::String => unicode_string_strategy().prop_map(Value::String).boxed(),
        Kind::Bytes => bytes_strategy().prop_map(Value::Bytes).boxed(),
        Kind::Enum(enum_desc) => {
            let declared_numbers: Vec<i32> = enum_desc.values().map(|v| v.number()).collect();
            proptest::sample::select(declared_numbers)
                .prop_map(Value::EnumNumber)
                .boxed()
        }
        Kind::Message(msg_desc) => {
            panic!(
                "scalar_or_enum_strategy called on message {}",
                msg_desc.full_name()
            )
        }
    }
}

fn timestamp_strategy(desc: MessageDescriptor) -> BoxedStrategy<DynamicMessage> {
    let sec_field = desc
        .get_field_by_name("seconds")
        .expect("Timestamp.seconds");
    let nanos_field = desc.get_field_by_name("nanos").expect("Timestamp.nanos");

    (
        TIMESTAMP_MIN_SECONDS..=TIMESTAMP_MAX_SECONDS,
        0_i32..=TIMESTAMP_MAX_NANOS,
    )
        .prop_map(move |(secs, nanos)| {
            let mut msg = DynamicMessage::new(desc.clone());
            set_canonical(&mut msg, &sec_field, Value::I64(secs));
            set_canonical(&mut msg, &nanos_field, Value::I32(nanos));
            msg
        })
        .boxed()
}

fn google_value_strategy(desc: MessageDescriptor, depth: usize) -> BoxedStrategy<DynamicMessage> {
    let null_field = desc
        .get_field_by_name("null_value")
        .expect("Value.null_value");
    let num_field = desc
        .get_field_by_name("number_value")
        .expect("Value.number_value");
    let str_field = desc
        .get_field_by_name("string_value")
        .expect("Value.string_value");
    let bool_field = desc
        .get_field_by_name("bool_value")
        .expect("Value.bool_value");

    let mut arms: Vec<BoxedStrategy<(FieldDescriptor, Value)>> = vec![
        Just((null_field, Value::EnumNumber(0))).boxed(),
        finite_f64_strategy()
            .prop_map(move |n| (num_field.clone(), Value::F64(n)))
            .boxed(),
        unicode_string_strategy()
            .prop_map(move |s| (str_field.clone(), Value::String(s)))
            .boxed(),
        any::<bool>()
            .prop_map(move |b| (bool_field.clone(), Value::Bool(b)))
            .boxed(),
    ];

    if depth > 0 {
        let struct_field = desc
            .get_field_by_name("struct_value")
            .expect("Value.struct_value");
        let list_field = desc
            .get_field_by_name("list_value")
            .expect("Value.list_value");
        let struct_desc = struct_field
            .kind()
            .as_message()
            .expect("Value.struct_value message")
            .clone();
        let list_desc = list_field
            .kind()
            .as_message()
            .expect("Value.list_value message")
            .clone();

        arms.push(
            google_struct_strategy(struct_desc, depth - 1)
                .prop_map(move |m| (struct_field.clone(), Value::Message(m)))
                .boxed(),
        );
        arms.push(
            google_list_value_strategy(list_desc, depth - 1)
                .prop_map(move |m| (list_field.clone(), Value::Message(m)))
                .boxed(),
        );
    }

    Union::new(arms)
        .prop_map(move |(field, val)| {
            let mut msg = DynamicMessage::new(desc.clone());
            set_canonical(&mut msg, &field, val);
            msg
        })
        .boxed()
}

fn google_struct_strategy(desc: MessageDescriptor, depth: usize) -> BoxedStrategy<DynamicMessage> {
    let fields_desc = desc.get_field_by_name("fields").expect("Struct.fields");
    let entry_desc = fields_desc
        .kind()
        .as_message()
        .expect("Struct.fields map entry")
        .clone();
    let val_desc = entry_desc
        .map_entry_value_field()
        .kind()
        .as_message()
        .expect("Struct.fields value is google.protobuf.Value")
        .clone();

    let max_entries = if depth == 0 { 2 } else { 3 };
    let child_depth = depth.saturating_sub(1);

    proptest::collection::hash_map(
        unicode_string_strategy().prop_map(MapKey::String),
        google_value_strategy(val_desc, child_depth).prop_map(Value::Message),
        0..=max_entries,
    )
    .prop_map(move |map| {
        let mut msg = DynamicMessage::new(desc.clone());
        set_canonical(&mut msg, &fields_desc, Value::Map(map));
        msg
    })
    .boxed()
}

fn google_list_value_strategy(
    desc: MessageDescriptor,
    depth: usize,
) -> BoxedStrategy<DynamicMessage> {
    let values_desc = desc.get_field_by_name("values").expect("ListValue.values");
    let val_desc = values_desc
        .kind()
        .as_message()
        .expect("ListValue.values item is google.protobuf.Value")
        .clone();

    let max_items = if depth == 0 { 2 } else { 3 };
    let child_depth = depth.saturating_sub(1);

    proptest::collection::vec(
        google_value_strategy(val_desc, child_depth).prop_map(Value::Message),
        0..=max_items,
    )
    .prop_map(move |items| {
        let mut msg = DynamicMessage::new(desc.clone());
        set_canonical(&mut msg, &values_desc, Value::List(items));
        msg
    })
    .boxed()
}

fn leaf_message_at_depth_zero(desc: MessageDescriptor) -> BoxedStrategy<DynamicMessage> {
    match desc.full_name() {
        "google.protobuf.Timestamp" => timestamp_strategy(desc),
        "google.protobuf.Value" => google_value_strategy(desc, 0),
        "google.protobuf.Struct" => google_struct_strategy(desc, 0),
        "google.protobuf.ListValue" => google_list_value_strategy(desc, 0),
        _ => Just(DynamicMessage::new(desc)).boxed(),
    }
}

fn element_value_strategy(kind: &Kind, depth: usize) -> Option<BoxedStrategy<Value>> {
    match kind {
        Kind::Message(child_desc) => {
            if depth == 0 {
                match child_desc.full_name() {
                    "google.protobuf.Timestamp"
                    | "google.protobuf.Value"
                    | "google.protobuf.Struct"
                    | "google.protobuf.ListValue" => Some(
                        leaf_message_at_depth_zero(child_desc.clone())
                            .prop_map(Value::Message)
                            .boxed(),
                    ),
                    _ => None,
                }
            } else {
                Some(
                    dynamic_message_strategy(child_desc.clone(), depth - 1)
                        .prop_map(Value::Message)
                        .boxed(),
                )
            }
        }
        other => Some(scalar_or_enum_strategy(other)),
    }
}

fn non_oneof_field_strategy(
    field: FieldDescriptor,
    depth: usize,
) -> Option<BoxedStrategy<Option<(FieldDescriptor, Value)>>> {
    let max_coll = if depth >= 2 { 3 } else { 2 };

    if field.is_map() {
        let entry_desc = field
            .kind()
            .as_message()
            .expect("map field must have entry message")
            .clone();
        let key_strat = map_key_strategy(&entry_desc.map_entry_key_field().kind());
        let val_kind = entry_desc.map_entry_value_field().kind();
        let val_strat = element_value_strategy(&val_kind, depth)?;

        return Some(
            proptest::collection::hash_map(key_strat, val_strat, 0..=max_coll)
                .prop_map(move |map| {
                    if map.is_empty() {
                        None
                    } else {
                        Some((field.clone(), Value::Map(map)))
                    }
                })
                .boxed(),
        );
    }

    if field.is_list() {
        let elem_strat = element_value_strategy(&field.kind(), depth)?;
        return Some(
            proptest::collection::vec(elem_strat, 0..=max_coll)
                .prop_map(move |items| {
                    if items.is_empty() {
                        None
                    } else {
                        Some((field.clone(), Value::List(items)))
                    }
                })
                .boxed(),
        );
    }

    let val_strat = element_value_strategy(&field.kind(), depth)?;
    Some(
        proptest::option::of(val_strat)
            .prop_map(move |opt| opt.map(|v| (field.clone(), v)))
            .boxed(),
    )
}

fn oneof_field_strategy(
    oneof: &OneofDescriptor,
    depth: usize,
) -> Option<BoxedStrategy<Option<(FieldDescriptor, Value)>>> {
    if oneof.is_synthetic() {
        let field = oneof
            .fields()
            .next()
            .expect("synthetic proto3_optional oneof must have 1 field");
        let val_strat = element_value_strategy(&field.kind(), depth)?;
        return Some(
            proptest::option::of(val_strat)
                .prop_map(move |opt| opt.map(|v| (field.clone(), v)))
                .boxed(),
        );
    }

    // Real oneof: generate one arm of each oneof (m0-06-brief.md §3.5).
    let mut arms: Vec<BoxedStrategy<Option<(FieldDescriptor, Value)>>> = Vec::new();
    for arm_field in oneof.fields() {
        if let Some(val_strat) = element_value_strategy(&arm_field.kind(), depth) {
            let f = arm_field.clone();
            arms.push(val_strat.prop_map(move |v| Some((f.clone(), v))).boxed());
        }
    }

    if arms.is_empty() {
        // At depth == 0 when every arm of the oneof is a non-WKT message, populate one arm
        // with a leaf/empty DynamicMessage so oneof arm tags are still exercised without
        // recursing deeper than MAX_RECURSION_DEPTH.
        for arm_field in oneof.fields() {
            if let Kind::Message(child_desc) = arm_field.kind() {
                let f = arm_field.clone();
                arms.push(
                    leaf_message_at_depth_zero(child_desc)
                        .prop_map(move |m| Some((f.clone(), Value::Message(m))))
                        .boxed(),
                );
            }
        }
    }

    if arms.is_empty() {
        None
    } else {
        Some(Union::new(arms).boxed())
    }
}

fn dynamic_message_strategy(
    desc: MessageDescriptor,
    depth: usize,
) -> BoxedStrategy<DynamicMessage> {
    match desc.full_name() {
        "google.protobuf.Timestamp" => return timestamp_strategy(desc),
        "google.protobuf.Value" => return google_value_strategy(desc, depth),
        "google.protobuf.Struct" => return google_struct_strategy(desc, depth),
        "google.protobuf.ListValue" => return google_list_value_strategy(desc, depth),
        _ => {}
    }

    let mut mutations: Vec<BoxedStrategy<Option<(FieldDescriptor, Value)>>> = Vec::new();

    for field in desc.fields() {
        if field.containing_oneof().is_none()
            && let Some(strat) = non_oneof_field_strategy(field, depth)
        {
            mutations.push(strat);
        }
    }

    for oneof in desc.oneofs() {
        if let Some(strat) = oneof_field_strategy(&oneof, depth) {
            mutations.push(strat);
        }
    }

    if mutations.is_empty() {
        return Just(DynamicMessage::new(desc)).boxed();
    }

    mutations
        .prop_map(move |entries| {
            let mut msg = DynamicMessage::new(desc.clone());
            for (field, val) in entries.into_iter().flatten() {
                set_canonical(&mut msg, &field, val);
            }
            msg
        })
        .boxed()
}

fn encode_varint(mut value: u64, out: &mut Vec<u8>) {
    while value >= 0x80 {
        out.push(((value as u8) & 0x7f) | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

fn append_unknown_fields(base: &[u8]) -> Vec<u8> {
    let mut buf = base.to_vec();
    // Wire type 0 (varint) on unused high field number.
    encode_varint(u64::from(UNKNOWN_VARINT_FIELD_NUMBER) << 3, &mut buf);
    encode_varint(0x1234_5678, &mut buf);
    // Wire type 2 (length-delimited) on unused high field number.
    encode_varint((u64::from(UNKNOWN_BYTES_FIELD_NUMBER) << 3) | 2, &mut buf);
    let payload = b"untrusted-unknown-extension";
    encode_varint(payload.len() as u64, &mut buf);
    buf.extend_from_slice(payload);
    // Wire type 1 (64-bit) on unused high field number.
    encode_varint((u64::from(UNKNOWN_FIXED64_FIELD_NUMBER) << 3) | 1, &mut buf);
    buf.extend_from_slice(&0xdead_beef_cafe_babe_u64.to_le_bytes());
    buf
}

fn run_roundtrip_suite(
    registry: &BTreeMap<String, StaticMessageOps>,
    descriptors: &BTreeMap<String, MessageDescriptor>,
) {
    let config = ProptestConfig {
        cases: CASES_PER_MESSAGE,
        max_shrink_iters: 64,
        failure_persistence: None,
        ..ProptestConfig::default()
    };

    for (full_name, desc) in descriptors {
        let ops = registry
            .get(full_name)
            .unwrap_or_else(|| panic!("missing static registration for {full_name}"));
        let strategy = dynamic_message_strategy(desc.clone(), MAX_RECURSION_DEPTH);
        let mut runner = TestRunner::new(config.clone());
        let populated = RefCell::new(BTreeSet::<String>::new());

        let result = runner.run(&strategy, |dyn_original| {
            for field in desc.fields() {
                if dyn_original.has_field(&field) {
                    populated.borrow_mut().insert(field.name().to_owned());
                }
            }
            let dyn_encoded = dyn_original.encode_to_vec();

            // (a) Decode with static generated type and re-encode; dynamic decode of static
            // re-encoding must equal the original DynamicMessage; and
            // (b) static encode_to_vec -> decode must satisfy PartialEq.
            let static_encoded = (ops.roundtrip)(dyn_encoded.as_slice())
                .map_err(|msg| TestCaseError::fail(format!("{full_name}: {msg}")))?;

            let dyn_redecoded = DynamicMessage::decode(desc.clone(), static_encoded.as_slice())
                .map_err(|err| {
                    TestCaseError::fail(format!(
                        "{full_name}: DynamicMessage::decode of static re-encoding failed: {err}"
                    ))
                })?;

            prop_assert_eq!(
                &dyn_redecoded,
                &dyn_original,
                "dynamic -> static -> dynamic mismatch for {}",
                full_name
            );
            Ok(())
        });

        if let Err(err) = result {
            panic!("proptest wire round-trip failed for {full_name}: {err}");
        }

        // A field the generator never sets is never round-tripped: fail instead of passing vacuously.
        let populated = populated.into_inner();
        let never_populated: Vec<String> = desc
            .fields()
            .map(|field| field.name().to_owned())
            .filter(|name| !populated.contains(name))
            .collect();
        assert!(
            never_populated.is_empty(),
            "{full_name}: the generator never populated field(s) {never_populated:?} in {CASES_PER_MESSAGE} cases, \
             so the wire round-trip does not exercise them"
        );
    }
}

fn run_unknown_field_suite(
    registry: &BTreeMap<String, StaticMessageOps>,
    descriptors: &BTreeMap<String, MessageDescriptor>,
) {
    let config = ProptestConfig {
        cases: 8,
        max_shrink_iters: 16,
        failure_persistence: None,
        ..ProptestConfig::default()
    };

    for (full_name, desc) in descriptors {
        for reserved_unknown in [
            UNKNOWN_VARINT_FIELD_NUMBER,
            UNKNOWN_BYTES_FIELD_NUMBER,
            UNKNOWN_FIXED64_FIELD_NUMBER,
        ] {
            assert!(
                desc.get_field(reserved_unknown).is_none(),
                "{full_name} unexpectedly uses field number {reserved_unknown}"
            );
        }

        let ops = registry
            .get(full_name)
            .unwrap_or_else(|| panic!("missing static registration for {full_name}"));

        // Empty message + unknown fields.
        let empty_with_unknown = append_unknown_fields(&[]);
        (ops.assert_unknown_ignored)(&[], &empty_with_unknown)
            .unwrap_or_else(|msg| panic!("{full_name} (empty): {msg}"));

        // Populated dynamic messages + unknown fields.
        let strategy = dynamic_message_strategy(desc.clone(), 2);
        let mut runner = TestRunner::new(config.clone());
        runner
            .run(&strategy, |dyn_msg| {
                let base_bytes = dyn_msg.encode_to_vec();
                let extended_bytes = append_unknown_fields(&base_bytes);
                (ops.assert_unknown_ignored)(&base_bytes, &extended_bytes)
                    .map_err(|msg| TestCaseError::fail(format!("{full_name}: {msg}")))?;
                Ok(())
            })
            .unwrap_or_else(|err| panic!("unknown-field test failed for {full_name}: {err}"));
    }
}

fn run_truncated_buffer_suite(
    registry: &BTreeMap<String, StaticMessageOps>,
    descriptors: &BTreeMap<String, MessageDescriptor>,
) {
    let config = ProptestConfig {
        cases: 8,
        max_shrink_iters: 16,
        failure_persistence: None,
        ..ProptestConfig::default()
    };

    // Malformed / mid-frame truncated buffers that MUST return Err on every message type.
    let truncated_varint_tag = vec![0x80_u8];
    let mut truncated_len_delimited = Vec::new();
    encode_varint(
        (u64::from(UNKNOWN_BYTES_FIELD_NUMBER) << 3) | 2,
        &mut truncated_len_delimited,
    );
    encode_varint(16, &mut truncated_len_delimited);
    truncated_len_delimited.extend_from_slice(b"short");

    let mut truncated_fixed64 = Vec::new();
    encode_varint(
        (u64::from(UNKNOWN_FIXED64_FIELD_NUMBER) << 3) | 1,
        &mut truncated_fixed64,
    );
    truncated_fixed64.extend_from_slice(&[1, 2, 3]);

    for (full_name, desc) in descriptors {
        let ops = registry
            .get(full_name)
            .unwrap_or_else(|| panic!("missing static registration for {full_name}"));

        for (case_name, bad_buf) in [
            ("truncated_varint_tag", truncated_varint_tag.as_slice()),
            (
                "truncated_length_delimited",
                truncated_len_delimited.as_slice(),
            ),
            ("truncated_fixed64", truncated_fixed64.as_slice()),
        ] {
            let returned_err = (ops.decode_truncated)(bad_buf)
                .unwrap_or_else(|panic_msg| panic!("{full_name} ({case_name}): {panic_msg}"));
            assert!(
                returned_err,
                "{full_name} ({case_name}): expected Err when decoding truncated buffer, got Ok"
            );
        }

        // Also slice every prefix of generated non-empty messages + mid-frame tail:
        // arbitrary prefix cuts must never panic, and mid-frame cuts must return Err.
        let strategy = dynamic_message_strategy(desc.clone(), 2);
        let mut runner = TestRunner::new(config.clone());
        runner
            .run(&strategy, |dyn_msg| {
                let encoded = dyn_msg.encode_to_vec();
                for cut in 0..encoded.len() {
                    let _ = (ops.decode_truncated)(&encoded[..cut]).map_err(|panic_msg| {
                        TestCaseError::fail(format!("{full_name} prefix cut {cut}: {panic_msg}"))
                    })?;
                }

                // Append a length-delimited field and truncate 1 byte from the end: must return Err.
                let mut with_tail = encoded;
                encode_varint(
                    (u64::from(UNKNOWN_BYTES_FIELD_NUMBER) << 3) | 2,
                    &mut with_tail,
                );
                encode_varint(8, &mut with_tail);
                with_tail.extend_from_slice(b"1234567"); // 7 bytes instead of declared 8
                let returned_err = (ops.decode_truncated)(&with_tail).map_err(|panic_msg| {
                    TestCaseError::fail(format!("{full_name}: {panic_msg}"))
                })?;
                prop_assert!(
                    returned_err,
                    "{}: expected Err on mid-payload truncated buffer",
                    full_name
                );
                Ok(())
            })
            .unwrap_or_else(|err| panic!("truncated-buffer test failed for {full_name}: {err}"));
    }
}

// spec: NGC-001
#[test]
fn own_registry_covers_exact_descriptor_message_set() {
    let (_, descriptors) = load_own_descriptors();
    let registry = build_registry_map(&own_message_registry());
    assert_registry_matches_descriptor(&registry, &descriptors, "GAP/NGC");
    assert_eq!(descriptors.len(), 42, "expected 42 GAP/NGC messages");
}

// spec: NGC-001
#[test]
fn own_messages_roundtrip_dynamic_and_static() {
    let (_, descriptors) = load_own_descriptors();
    let registry = build_registry_map(&own_message_registry());
    assert_registry_matches_descriptor(&registry, &descriptors, "GAP/NGC");
    run_roundtrip_suite(&registry, &descriptors);
}

// spec: NGC-001
#[test]
fn own_messages_ignore_appended_unknown_fields() {
    let (_, descriptors) = load_own_descriptors();
    let registry = build_registry_map(&own_message_registry());
    run_unknown_field_suite(&registry, &descriptors);
}

// spec: NGC-001
#[test]
fn own_messages_reject_truncated_buffers_without_panicking() {
    let (_, descriptors) = load_own_descriptors();
    let registry = build_registry_map(&own_message_registry());
    run_truncated_buffer_suite(&registry, &descriptors);
}

// spec: NGC-001
#[test]
fn generator_covers_integer_map_keys_and_all_scalar_wire_kinds() {
    use prost_types::field_descriptor_proto::{Label, Type};
    use prost_types::{DescriptorProto, FieldDescriptorProto, FileDescriptorProto, MessageOptions};

    fn scalar_field(name: &str, number: i32, ty: Type) -> FieldDescriptorProto {
        FieldDescriptorProto {
            name: Some(name.to_owned()),
            number: Some(number),
            label: Some(Label::Optional as i32),
            r#type: Some(ty as i32),
            ..FieldDescriptorProto::default()
        }
    }

    fn map_entry_msg(name: &str, key_ty: Type, val_ty: Type) -> DescriptorProto {
        DescriptorProto {
            name: Some(name.to_owned()),
            field: vec![
                scalar_field("key", 1, key_ty),
                scalar_field("value", 2, val_ty),
            ],
            options: Some(MessageOptions {
                map_entry: Some(true),
                ..MessageOptions::default()
            }),
            ..DescriptorProto::default()
        }
    }

    fn map_field(name: &str, number: i32, entry_type_name: &str) -> FieldDescriptorProto {
        FieldDescriptorProto {
            name: Some(name.to_owned()),
            number: Some(number),
            label: Some(Label::Repeated as i32),
            r#type: Some(Type::Message as i32),
            type_name: Some(entry_type_name.to_owned()),
            ..FieldDescriptorProto::default()
        }
    }

    let synthetic = FileDescriptorProto {
        name: Some("synthetic_scalars_and_maps.proto".to_owned()),
        package: Some("gap.test.v1".to_owned()),
        syntax: Some("proto3".to_owned()),
        message_type: vec![DescriptorProto {
            name: Some("AllScalarsAndMaps".to_owned()),
            field: vec![
                scalar_field("f_float", 1, Type::Float),
                scalar_field("f_sint32", 2, Type::Sint32),
                scalar_field("f_sint64", 3, Type::Sint64),
                scalar_field("f_fixed32", 4, Type::Fixed32),
                scalar_field("f_sfixed32", 5, Type::Sfixed32),
                scalar_field("f_sfixed64", 6, Type::Sfixed64),
                map_field("m_i32", 7, ".gap.test.v1.AllScalarsAndMaps.MI32Entry"),
                map_field("m_i64", 8, ".gap.test.v1.AllScalarsAndMaps.MI64Entry"),
                map_field("m_u32", 9, ".gap.test.v1.AllScalarsAndMaps.MU32Entry"),
                map_field("m_u64", 10, ".gap.test.v1.AllScalarsAndMaps.MU64Entry"),
                map_field("m_bool", 11, ".gap.test.v1.AllScalarsAndMaps.MBoolEntry"),
            ],
            nested_type: vec![
                map_entry_msg("MI32Entry", Type::Int32, Type::String),
                map_entry_msg("MI64Entry", Type::Sint64, Type::Bytes),
                map_entry_msg("MU32Entry", Type::Fixed32, Type::Float),
                map_entry_msg("MU64Entry", Type::Uint64, Type::Double),
                map_entry_msg("MBoolEntry", Type::Bool, Type::Sfixed32),
            ],
            ..DescriptorProto::default()
        }],
        ..FileDescriptorProto::default()
    };

    let mut pool = DescriptorPool::new();
    pool.add_file_descriptor_proto(synthetic)
        .expect("synthetic descriptor must be valid");
    let desc = pool
        .get_message_by_name("gap.test.v1.AllScalarsAndMaps")
        .expect("synthetic message must exist");

    let strat = dynamic_message_strategy(desc.clone(), MAX_RECURSION_DEPTH);
    let mut runner = TestRunner::new(ProptestConfig {
        cases: CASES_PER_MESSAGE,
        max_shrink_iters: 32,
        failure_persistence: None,
        ..ProptestConfig::default()
    });
    runner
        .run(&strat, |msg| {
            let bytes = msg.encode_to_vec();
            let decoded = DynamicMessage::decode(desc.clone(), bytes.as_slice())
                .map_err(|e| TestCaseError::fail(format!("decode failed: {e}")))?;
            prop_assert_eq!(decoded, msg);
            Ok(())
        })
        .expect("all scalar kinds and integer/bool map keys must round-trip cleanly");
}

// spec: CMP-020
// spec: CMP-021
// spec: CMP-022
#[cfg(feature = "a2a")]
#[test]
fn a2a_registry_covers_exact_descriptor_message_set() {
    let (_, descriptors) = load_a2a_descriptors();
    let registry = build_registry_map(&a2a_message_registry());
    assert_registry_matches_descriptor(&registry, &descriptors, "A2A");
    assert_eq!(descriptors.len(), 44, "expected 44 A2A messages");
}

// spec: CMP-020
// spec: CMP-021
// spec: CMP-022
#[cfg(feature = "a2a")]
#[test]
fn a2a_messages_roundtrip_dynamic_and_static() {
    let (_, descriptors) = load_a2a_descriptors();
    let registry = build_registry_map(&a2a_message_registry());
    assert_registry_matches_descriptor(&registry, &descriptors, "A2A");
    run_roundtrip_suite(&registry, &descriptors);
}

// spec: CMP-020
#[cfg(feature = "a2a")]
#[test]
fn a2a_messages_ignore_appended_unknown_fields() {
    let (_, descriptors) = load_a2a_descriptors();
    let registry = build_registry_map(&a2a_message_registry());
    run_unknown_field_suite(&registry, &descriptors);
}

// spec: CMP-020
#[cfg(feature = "a2a")]
#[test]
fn a2a_messages_reject_truncated_buffers_without_panicking() {
    let (_, descriptors) = load_a2a_descriptors();
    let registry = build_registry_map(&a2a_message_registry());
    run_truncated_buffer_suite(&registry, &descriptors);
}
