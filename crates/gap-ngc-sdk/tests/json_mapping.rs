//! Canonical Protobuf JSON mapping tests and full-descriptor proptests (`NGC-001`, `NGC-002`, `NGC-003`).
#![forbid(unsafe_code)]

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, HashMap};

use gap_core::error::NgcErrorCode;
use gap_ngc_sdk::json::{
    ProtoMessage, descriptor_pool, dynamic_from_json_value, dynamic_to_json_value, from_json_slice,
    from_json_str, from_json_value, to_json_string, to_json_value, to_json_vec,
};
use gap_proto::ngc::v1::{
    ApprovalProof, CallContext, Cmp, Complete, DescribeRequest, Expr, ExprList, HealthResponse,
    MutateResponse, MutationOutcome, OpKind, OperationDescriptor, PredicateOp, Principal,
    PrincipalKind, QueryRequest, QueryResponse, RowSetChunk, SketchRequest, SketchResponse, expr,
    query_response,
};
use proptest::prelude::*;
use proptest::strategy::Union;
use proptest::test_runner::{Config as ProptestConfig, TestRunner};
use prost::Message as _;
use prost::bytes::Bytes;
use prost_reflect::{
    DynamicMessage, FieldDescriptor, Kind, MapKey, MessageDescriptor, OneofDescriptor, Value,
};
use prost_types::FileDescriptorSet;

const CASES_PER_MESSAGE: u32 = 64;
const MAX_RECURSION_DEPTH: usize = 3;

const _: () = assert!(CASES_PER_MESSAGE >= 64);
const _: () = assert!(MAX_RECURSION_DEPTH >= 3);

const TIMESTAMP_MIN_SECONDS: i64 = -62_135_596_800; // 0001-01-01T00:00:00Z
const TIMESTAMP_MAX_SECONDS: i64 = 253_402_300_799; // 9999-12-31T23:59:59Z
const TIMESTAMP_MAX_NANOS: i32 = 999_999_999;

// spec: NGC-002, NGC-003
#[test]
fn canonical_json_encodes_int64_strings_base64_bytes_enum_names_wkt_oneof_map_and_omits_defaults() {
    // 1. 64-bit integers (`int64`, `uint64`, `fixed64`) as JSON strings + default omission.
    let ctx = CallContext {
        tenant_id: "acme".to_owned(),
        request_id: String::new(), // default omitted
        traceparent: "00-abc".to_owned(),
        deadline_unix_ms: -9_007_199_254_740_993,
        remaining_cost_units: 0, // default omitted
        principal: Some(Principal {
            id: "u-1".to_owned(),
            kind: PrincipalKind::Agent as i32,
            delegation_chain: vec!["svc-edge".to_owned()],
            scopes: vec!["orders:read".to_owned()],
            attributes: HashMap::from([("region".to_owned(), "ap-southeast-1".to_owned())]),
        }),
        hop_count: 2,
        trace_path: vec!["node-a".to_owned()],
        idempotency_key: String::new(),
        snapshot_generation: 18_446_744_073_709_551_614,
    };
    let ctx_json = to_json_value(&ctx).expect("serialize CallContext");
    assert_eq!(ctx_json["tenantId"], "acme");
    assert!(ctx_json.get("requestId").is_none());
    assert!(ctx_json.get("remainingCostUnits").is_none());
    assert_eq!(ctx_json["deadlineUnixMs"], "-9007199254740993");
    assert_eq!(ctx_json["snapshotGeneration"], "18446744073709551614");
    assert_eq!(ctx_json["principal"]["kind"], "AGENT");
    assert_eq!(
        ctx_json["principal"]["attributes"]["region"],
        "ap-southeast-1"
    );
    assert_eq!(
        from_json_value::<CallContext>(ctx_json).expect("roundtrip CallContext"),
        ctx
    );

    // 2. `bytes` as standard base64 + `fixed64` list as strings.
    let sketch_req = SketchRequest {
        ctx: None,
        operation: "listInvoices".to_owned(),
        field_path: "customer_id".to_owned(),
        arguments: None,
        sample_size: 1_000,
        salt: vec![0xde, 0xad, 0xbe, 0xef],
        bottom_k: 64,
    };
    let sketch_req_json = to_json_value(&sketch_req).expect("serialize SketchRequest");
    assert_eq!(sketch_req_json["fieldPath"], "customer_id");
    assert_eq!(sketch_req_json["salt"], "3q2+7w==");
    assert_eq!(
        from_json_value::<SketchRequest>(sketch_req_json).expect("roundtrip SketchRequest"),
        sketch_req
    );

    let sketch_resp = SketchResponse {
        distinct_estimate: 9_007_199_254_740_999,
        null_ratio: 0.125,
        uniqueness_ratio: 0.875,
        bottom_k_hashes: vec![1, u64::MAX],
        sample_count: 500,
        value_kind: "uuid".to_owned(),
        exact: true,
    };
    let sketch_resp_json = to_json_value(&sketch_resp).expect("serialize SketchResponse");
    assert_eq!(sketch_resp_json["distinctEstimate"], "9007199254740999");
    assert_eq!(
        sketch_resp_json["bottomKHashes"],
        serde_json::json!(["1", "18446744073709551615"])
    );
    assert_eq!(
        from_json_value::<SketchResponse>(sketch_resp_json).expect("roundtrip SketchResponse"),
        sketch_resp
    );

    // 3. `Timestamp`, `Struct`, `Value`, `ListValue`, and `oneof`.
    let approval = ApprovalProof {
        approval_id: "appr-1".to_owned(),
        decided_by: "sec-ops".to_owned(),
        decided_at: Some(prost_types::Timestamp {
            seconds: 1_700_000_000,
            nanos: 123_000_000,
        }),
        signature: "ed25519-sig".to_owned(),
    };
    let approval_json = to_json_value(&approval).expect("serialize ApprovalProof");
    assert_eq!(approval_json["decidedAt"], "2023-11-14T22:13:20.123Z");
    assert_eq!(
        from_json_value::<ApprovalProof>(approval_json).expect("roundtrip ApprovalProof"),
        approval
    );

    let expr_msg = Expr {
        node: Some(expr::Node::And(ExprList {
            exprs: vec![
                Expr {
                    node: Some(expr::Node::Cmp(Cmp {
                        path: "amount".to_owned(),
                        op: PredicateOp::PGte as i32,
                        value: Some(prost_types::Value {
                            kind: Some(prost_types::value::Kind::NumberValue(42.5)),
                        }),
                    })),
                },
                Expr {
                    node: Some(expr::Node::Exists("deletedAt".to_owned())),
                },
            ],
        })),
    };
    let expr_json = to_json_value(&expr_msg).expect("serialize Expr");
    assert_eq!(expr_json["and"]["exprs"][0]["cmp"]["op"], "P_GTE");
    assert_eq!(expr_json["and"]["exprs"][0]["cmp"]["value"], 42.5);
    assert_eq!(expr_json["and"]["exprs"][1]["exists"], "deletedAt");
    assert_eq!(
        from_json_value::<Expr>(expr_json).expect("roundtrip Expr"),
        expr_msg
    );

    // 4. `QueryResponse` oneof (`rows` and `complete`) + `MutateResponse` + `OperationDescriptor`.
    let qr = QueryResponse {
        chunk: Some(query_response::Chunk::Complete(Complete {
            next_cursor: "cur-9".to_owned(),
            truncated: true,
            rows_total: 123_456_789_012_345,
            upstream_calls: 3,
            latency_us: 980,
        })),
    };
    let qr_str = to_json_string(&qr).expect("to_json_string");
    assert_eq!(
        from_json_str::<QueryResponse>(&qr_str).expect("from_json_str"),
        qr
    );

    let mr = MutateResponse {
        result: Some(prost_types::Struct {
            fields: BTreeMap::from([(
                "status".to_owned(),
                prost_types::Value {
                    kind: Some(prost_types::value::Kind::StringValue("ok".to_owned())),
                },
            )]),
        }),
        outcome: MutationOutcome::OutcomeApplied as i32,
        was_replayed: true,
        compensation_token: "comp-1".to_owned(),
        error: None,
        dry_run_report: None,
    };
    let mr_bytes = to_json_vec(&mr).expect("to_json_vec");
    assert_eq!(
        from_json_slice::<MutateResponse>(&mr_bytes).expect("from_json_slice"),
        mr
    );

    let op = OperationDescriptor {
        name: "createOrder".to_owned(),
        kind: OpKind::Mutation as i32,
        annotations: HashMap::from([("owner".to_owned(), "commerce".to_owned())]),
        ..OperationDescriptor::default()
    };
    assert_eq!(
        from_json_value::<OperationDescriptor>(to_json_value(&op).expect("op json"))
            .expect("op rt"),
        op
    );
}

// spec: NGC-002
#[test]
fn parser_accepts_snake_and_camel_case_ignores_unknown_fields_and_rejects_bad_enums_or_types() {
    // Both snake_case and camelCase + unknown top-level & nested fields ignored, while arbitrary
    // keys inside `google.protobuf.Struct` (`arguments`) are preserved verbatim!
    let raw = serde_json::json!({
        "ctx": {
            "tenant_id": "tenant-snake",
            "requestId": "req-camel",
            "deadline_unix_ms": "1700000005000",
            "unknownCtxField": { "deeply": [1, 2, 3] }
        },
        "operation": "getCustomer",
        "arguments": {
            "customer_id": "cust-7",
            "arbitraryUserStructKey": { "keepMe": true }
        },
        "foreach": [
            {
                "index": 0,
                "values": { "k": "v" },
                "futureForeachExtension": "ignored"
            }
        ],
        "unknownTopLevel": "ignored-just-like-binary-decoder"
    });

    let parsed: QueryRequest =
        from_json_value(raw).expect("valid snake+camel+unknown QueryRequest");
    let ctx = parsed.ctx.expect("ctx present");
    assert_eq!(ctx.tenant_id, "tenant-snake");
    assert_eq!(ctx.request_id, "req-camel");
    assert_eq!(ctx.deadline_unix_ms, 1_700_000_005_000);
    assert_eq!(parsed.operation, "getCustomer");
    let args = parsed.arguments.expect("arguments Struct preserved");
    assert!(args.fields.contains_key("customer_id"));
    assert!(args.fields.contains_key("arbitraryUserStructKey"));
    assert_eq!(parsed.foreach.len(), 1);

    // DescribeRequest snake_case vs camelCase equivalence
    let d_snake: DescribeRequest =
        from_json_str(r#"{"known_spec_hash":"h1","extra":123}"#).expect("snake DescribeRequest");
    let d_camel: DescribeRequest =
        from_json_str(r#"{"knownSpecHash":"h1"}"#).expect("camel DescribeRequest");
    assert_eq!(d_snake, d_camel);

    // Unknown enum name MUST be rejected with INVALID_ARGUMENT (even when unknown fields are ignored!).
    let bad_enum = from_json_str::<HealthResponse>(
        r#"{"status":"TOTALLY_UNKNOWN_STATUS","futureField":true}"#,
    )
    .expect_err("unknown enum string must fail");
    assert_eq!(bad_enum.code, NgcErrorCode::InvalidArgument);

    // Nested unknown enum name MUST also be rejected with INVALID_ARGUMENT.
    let bad_nested_enum = from_json_value::<QueryRequest>(serde_json::json!({
        "operation": "q",
        "predicate": {
            "cmp": {
                "path": "x",
                "op": "NOT_A_VALID_PREDICATE_OP",
                "value": 1
            }
        }
    }))
    .expect_err("nested unknown enum string must fail");
    assert_eq!(bad_nested_enum.code, NgcErrorCode::InvalidArgument);

    // Wrong JSON types MUST be rejected with INVALID_ARGUMENT.
    for invalid_json in [
        r#"{"foreachIndex": "not-a-uint32"}"#,
        r#"{"foreachIndex": -5}"#,
        r#"{"rows": "not-an-array"}"#,
        r#"{"lastForIndex": "not-a-bool"}"#,
        r#"not-json-at-all"#,
        r#"[1, 2, 3]"#,
    ] {
        let err = from_json_str::<RowSetChunk>(invalid_json)
            .expect_err("wrong JSON type or syntax must fail");
        assert_eq!(
            err.code,
            NgcErrorCode::InvalidArgument,
            "expected INVALID_ARGUMENT for `{invalid_json}`"
        );
    }
}

#[derive(Clone, Copy)]
struct StaticJsonOps {
    full_name: &'static str,
    static_json_roundtrip: fn(&[u8]) -> Result<Vec<u8>, String>,
}

fn static_json_roundtrip_impl<T>(wire_bytes: &[u8]) -> Result<Vec<u8>, String>
where
    T: ProtoMessage + PartialEq + std::fmt::Debug,
{
    let static_orig = T::decode(wire_bytes)
        .map_err(|e| format!("prost decode failed for {}: {e}", T::FULL_NAME))?;
    let json_bytes = to_json_vec(&static_orig)
        .map_err(|e| format!("to_json_vec failed for {}: {e:?}", T::FULL_NAME))?;
    let static_rt: T = from_json_slice(&json_bytes)
        .map_err(|e| format!("from_json_slice failed for {}: {e:?}", T::FULL_NAME))?;
    if static_orig != static_rt {
        return Err(format!(
            "static -> JSON -> static mismatch for {}:\n  orig={static_orig:?}\n  rt={static_rt:?}\n  json={}",
            T::FULL_NAME,
            String::from_utf8_lossy(&json_bytes)
        ));
    }

    // Also verify unknown fields injected into the serialized JSON object are ignored cleanly.
    let mut json_val = serde_json::from_slice::<serde_json::Value>(&json_bytes)
        .map_err(|e| format!("serde_json parse failed for {}: {e}", T::FULL_NAME))?;
    if let serde_json::Value::Object(ref mut obj) = json_val {
        obj.insert(
            "__injectedUnknownField".to_owned(),
            serde_json::json!({"nested": [1, "two", true]}),
        );
        let with_unknown: T = from_json_value(json_val).map_err(|e| {
            format!(
                "from_json_value with unknown field failed for {}: {e:?}",
                T::FULL_NAME
            )
        })?;
        if static_orig != with_unknown {
            return Err(format!(
                "injected unknown JSON field altered decoded value for {}",
                T::FULL_NAME
            ));
        }
    }

    Ok(static_rt.encode_to_vec())
}

macro_rules! register_json_messages {
    ($($rust_ty:ty),* $(,)?) => {
        vec![
            $(
                StaticJsonOps {
                    full_name: <$rust_ty as ProtoMessage>::FULL_NAME,
                    static_json_roundtrip: static_json_roundtrip_impl::<$rust_ty>,
                },
            )*
        ]
    };
}

fn all_json_messages() -> Vec<StaticJsonOps> {
    register_json_messages! {
        // gap.gateway.v1 (2)
        gap_proto::gateway::v1::ExecuteRequest,
        gap_proto::gateway::v1::ExecuteResponse,

        // gap.ngc.v1 (40)
        gap_proto::ngc::v1::ApprovalProof,
        gap_proto::ngc::v1::Artifact,
        gap_proto::ngc::v1::AuthRequirements,
        gap_proto::ngc::v1::CallContext,
        gap_proto::ngc::v1::CancelTaskRequest,
        gap_proto::ngc::v1::Capabilities,
        gap_proto::ngc::v1::Cmp,
        gap_proto::ngc::v1::Complete,
        gap_proto::ngc::v1::ConnectorDescription,
        gap_proto::ngc::v1::DescribeRequest,
        gap_proto::ngc::v1::DescribeResponse,
        gap_proto::ngc::v1::Event,
        gap_proto::ngc::v1::Example,
        gap_proto::ngc::v1::ExplainResponse,
        gap_proto::ngc::v1::Expr,
        gap_proto::ngc::v1::ExprList,
        gap_proto::ngc::v1::GetTaskRequest,
        gap_proto::ngc::v1::HealthRequest,
        gap_proto::ngc::v1::HealthResponse,
        gap_proto::ngc::v1::Limits,
        gap_proto::ngc::v1::MetaSummary,
        gap_proto::ngc::v1::MutateRequest,
        gap_proto::ngc::v1::MutateResponse,
        gap_proto::ngc::v1::NgcError,
        gap_proto::ngc::v1::OperationDescriptor,
        gap_proto::ngc::v1::OrderBy,
        gap_proto::ngc::v1::Page,
        gap_proto::ngc::v1::Principal,
        gap_proto::ngc::v1::QueryRequest,
        gap_proto::ngc::v1::QueryResponse,
        gap_proto::ngc::v1::RowSetChunk,
        gap_proto::ngc::v1::Selection,
        gap_proto::ngc::v1::SelectionField,
        gap_proto::ngc::v1::SketchRequest,
        gap_proto::ngc::v1::SketchResponse,
        gap_proto::ngc::v1::StartTaskRequest,
        gap_proto::ngc::v1::SubscribeRequest,
        gap_proto::ngc::v1::TaskStatus,
        gap_proto::ngc::v1::VariableSet,
        gap_proto::ngc::v1::WatchTaskRequest,
    }
}

fn collect_descriptor_messages() -> BTreeMap<String, MessageDescriptor> {
    let fds = FileDescriptorSet::decode(gap_proto::DESCRIPTOR_SET)
        .expect("decode gap_proto::DESCRIPTOR_SET");
    let pool = descriptor_pool();
    let mut out = BTreeMap::new();
    for f in &fds.file {
        let file = pool
            .get_file_by_name(f.name())
            .unwrap_or_else(|| panic!("missing file {} in descriptor_pool", f.name()));
        for msg in file.messages() {
            collect_msg_rec(&msg, &mut out);
        }
    }
    out
}

fn collect_msg_rec(msg: &MessageDescriptor, out: &mut BTreeMap<String, MessageDescriptor>) {
    if !msg.is_map_entry() {
        out.insert(msg.full_name().to_owned(), msg.clone());
    }
    for child in msg.child_messages() {
        collect_msg_rec(&child, out);
    }
}

fn set_canonical(msg: &mut DynamicMessage, field: &FieldDescriptor, value: Value) {
    msg.set_field(field, value);
    if !msg.has_field(field) {
        msg.clear_field(field);
    }
}

fn canonical_json_f32(v: f32) -> f32 {
    if !v.is_finite() || v == 0.0 {
        return 0.0;
    }
    if let Ok(bytes) = serde_json::to_vec(&v)
        && let Ok(rt) = serde_json::from_slice::<f32>(&bytes)
        && let Ok(bytes2) = serde_json::to_vec(&rt)
        && let Ok(rt2) = serde_json::from_slice::<f32>(&bytes2)
        && rt.to_bits() == rt2.to_bits()
    {
        return rt;
    }
    0.0
}

fn canonical_json_f64(v: f64) -> f64 {
    if !v.is_finite() || v == 0.0 {
        return 0.0;
    }
    if let Ok(bytes) = serde_json::to_vec(&v)
        && let Ok(rt) = serde_json::from_slice::<f64>(&bytes)
        && let Ok(bytes2) = serde_json::to_vec(&rt)
        && let Ok(rt2) = serde_json::from_slice::<f64>(&bytes2)
        && rt.to_bits() == rt2.to_bits()
    {
        return rt;
    }
    0.0
}

fn finite_f32_strategy() -> impl Strategy<Value = f32> {
    let mask = proptest::num::f32::POSITIVE
        | proptest::num::f32::NEGATIVE
        | proptest::num::f32::NORMAL
        | proptest::num::f32::SUBNORMAL
        | proptest::num::f32::ZERO;
    mask.prop_map(canonical_json_f32)
}

fn finite_f64_strategy() -> impl Strategy<Value = f64> {
    let f64_mask = proptest::num::f64::POSITIVE
        | proptest::num::f64::NEGATIVE
        | proptest::num::f64::NORMAL
        | proptest::num::f64::SUBNORMAL
        | proptest::num::f64::ZERO;
    f64_mask.prop_map(canonical_json_f64)
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
        other => panic!("unsupported map key kind: {other:?}"),
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
        Kind::Message(msg_desc) => panic!("unexpected message {}", msg_desc.full_name()),
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
    let null_field = desc.get_field_by_name("null_value").expect("null_value");
    let num_field = desc
        .get_field_by_name("number_value")
        .expect("number_value");
    let str_field = desc
        .get_field_by_name("string_value")
        .expect("string_value");
    let bool_field = desc.get_field_by_name("bool_value").expect("bool_value");

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
            .expect("struct_value");
        let list_field = desc.get_field_by_name("list_value").expect("list_value");
        let struct_desc = struct_field.kind().as_message().expect("Struct").clone();
        let list_desc = list_field.kind().as_message().expect("ListValue").clone();

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
    let entry_desc = fields_desc.kind().as_message().expect("entry").clone();
    let val_desc = entry_desc
        .map_entry_value_field()
        .kind()
        .as_message()
        .expect("Value")
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
    let val_desc = values_desc.kind().as_message().expect("Value").clone();
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
        let entry_desc = field.kind().as_message().expect("map entry").clone();
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
        let field = oneof.fields().next().expect("synthetic oneof field");
        let val_strat = element_value_strategy(&field.kind(), depth)?;
        return Some(
            proptest::option::of(val_strat)
                .prop_map(move |opt| opt.map(|v| (field.clone(), v)))
                .boxed(),
        );
    }

    let mut arms: Vec<BoxedStrategy<Option<(FieldDescriptor, Value)>>> = Vec::new();
    for arm_field in oneof.fields() {
        if let Some(val_strat) = element_value_strategy(&arm_field.kind(), depth) {
            let f = arm_field.clone();
            arms.push(val_strat.prop_map(move |v| Some((f.clone(), v))).boxed());
        }
    }

    if arms.is_empty() {
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

// spec: NGC-001, NGC-002, NGC-003
#[test]
fn proptest_json_roundtrip_covers_every_message_in_descriptor_set() {
    let descriptors = collect_descriptor_messages();
    let registry_vec = all_json_messages();
    let mut registry = BTreeMap::new();
    for item in registry_vec {
        let prev = registry.insert(item.full_name.to_owned(), item);
        assert!(prev.is_none(), "duplicate registration {}", item.full_name);
    }

    let reg_names: BTreeSet<&str> = registry.keys().map(String::as_str).collect();
    let desc_names: BTreeSet<&str> = descriptors.keys().map(String::as_str).collect();
    assert_eq!(
        reg_names, desc_names,
        "ProtoMessage registry must match DESCRIPTOR_SET messages 1-for-1"
    );

    let config = ProptestConfig {
        cases: CASES_PER_MESSAGE,
        max_shrink_iters: 64,
        failure_persistence: None,
        ..ProptestConfig::default()
    };

    for (full_name, desc) in &descriptors {
        let ops = registry
            .get(full_name)
            .unwrap_or_else(|| panic!("missing ops for {full_name}"));
        let strategy = dynamic_message_strategy(desc.clone(), MAX_RECURSION_DEPTH);
        let mut runner = TestRunner::new(config.clone());
        let populated = RefCell::new(BTreeSet::<String>::new());

        let result = runner.run(&strategy, |dyn_orig| {
            for field in desc.fields() {
                if dyn_orig.has_field(&field) {
                    populated.borrow_mut().insert(field.name().to_owned());
                }
            }

            // Dynamic -> canonical JSON Value -> Dynamic
            let json_val = dynamic_to_json_value(&dyn_orig)
                .map_err(|e| TestCaseError::fail(format!("{full_name} to_json: {e:?}")))?;
            let dyn_from_json = dynamic_from_json_value(desc, json_val)
                .map_err(|e| TestCaseError::fail(format!("{full_name} from_json: {e:?}")))?;
            prop_assert_eq!(
                &dyn_from_json,
                &dyn_orig,
                "dynamic -> JSON -> dynamic mismatch for {}",
                full_name
            );

            // Dynamic wire -> Static -> JSON -> Static -> Dynamic wire
            let wire = dyn_orig.encode_to_vec();
            let rt_wire = (ops.static_json_roundtrip)(&wire)
                .map_err(|msg| TestCaseError::fail(format!("{full_name}: {msg}")))?;
            let dyn_after_static = DynamicMessage::decode(desc.clone(), rt_wire.as_slice())
                .map_err(|e| TestCaseError::fail(format!("{full_name} wire decode: {e}")))?;
            prop_assert_eq!(
                &dyn_after_static,
                &dyn_orig,
                "dynamic -> static -> JSON -> static -> dynamic mismatch for {}",
                full_name
            );
            Ok(())
        });

        if let Err(err) = result {
            panic!("JSON proptest failed for {full_name}: {err}");
        }

        let populated = populated.into_inner();
        let never_populated: Vec<String> = desc
            .fields()
            .map(|f| f.name().to_owned())
            .filter(|name| !populated.contains(name))
            .collect();
        assert!(
            never_populated.is_empty(),
            "{full_name}: generator never populated fields {never_populated:?}"
        );
    }
}
