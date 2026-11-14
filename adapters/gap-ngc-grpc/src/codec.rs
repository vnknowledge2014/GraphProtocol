//! Dynamic Protobuf wire codec, JSON <-> `DynamicMessage` translation, and `Selection` projection (`NGC-002`, `NGC-024`, `spec/09-compat.md` §5.3).

use bytes::{Buf as _, BufMut as _, Bytes};
use gap_core::error::{NgcErrorCode, NgcErrorInfo};
use gap_ngc_sdk::connector::NgcResult;
use gap_ngc_sdk::error::{json_map_to_prost_struct, prost_struct_to_json_map};
use gap_proto::ngc::v1::Selection;
use prost::Message as _;
use prost_reflect::{DeserializeOptions, DynamicMessage, MessageDescriptor, SerializeOptions};
use serde_json::{Map, Value};
use tonic::Status;
use tonic::codec::{Codec, DecodeBuf, Decoder, EncodeBuf, Encoder};

/// Raw zero-transformation gRPC codec passing pre-encoded Protobuf message bytes (`Bytes`).
#[derive(Debug, Clone, Copy, Default)]
pub struct RawBytesCodec;

/// Encoder side of [`RawBytesCodec`].
#[derive(Debug, Clone, Copy, Default)]
pub struct RawBytesEncoder;

/// Decoder side of [`RawBytesCodec`].
#[derive(Debug, Clone, Copy, Default)]
pub struct RawBytesDecoder;

impl Codec for RawBytesCodec {
    type Encode = Bytes;
    type Decode = Bytes;
    type Encoder = RawBytesEncoder;
    type Decoder = RawBytesDecoder;

    fn encoder(&mut self) -> Self::Encoder {
        RawBytesEncoder
    }

    fn decoder(&mut self) -> Self::Decoder {
        RawBytesDecoder
    }
}

impl Encoder for RawBytesEncoder {
    type Item = Bytes;
    type Error = Status;

    fn encode(&mut self, item: Self::Item, dst: &mut EncodeBuf<'_>) -> Result<(), Self::Error> {
        dst.reserve(item.len());
        dst.put_slice(&item);
        Ok(())
    }
}

impl Decoder for RawBytesDecoder {
    type Item = Bytes;
    type Error = Status;

    fn decode(&mut self, src: &mut DecodeBuf<'_>) -> Result<Option<Self::Item>, Self::Error> {
        let len = src.remaining();
        Ok(Some(src.copy_to_bytes(len)))
    }
}

/// Encodes NGC JSON/Struct arguments into Protobuf wire bytes according to `input_desc`.
///
/// Unknown fields are rejected (`INVALID_ARGUMENT`) so mistyped arguments fail loudly before hitting upstream.
///
/// # Errors
///
/// Returns `INVALID_ARGUMENT` if arguments cannot be coerced into `input_desc`.
pub fn encode_request_bytes(
    input_desc: &MessageDescriptor,
    arguments: Option<&prost_types::Struct>,
) -> NgcResult<Bytes> {
    let map = arguments.map(prost_struct_to_json_map).unwrap_or_default();
    let json_val = Value::Object(map);
    let opts = DeserializeOptions::new().deny_unknown_fields(true);
    let dynamic = DynamicMessage::deserialize_with_options(input_desc.clone(), &json_val, &opts)
        .map_err(|err| {
            NgcErrorInfo::new(
                NgcErrorCode::InvalidArgument,
                format!(
                    "arguments failed protobuf coercion for `{}`: {err}",
                    input_desc.full_name()
                ),
            )
        })?;
    Ok(Bytes::from(dynamic.encode_to_vec()))
}

/// Decodes raw Protobuf response bytes using `output_desc`, serializes into JSON, and applies `Selection` projection.
///
/// Uses protobuf field names (`snake_case` matching generated GAP-QL field names) and includes
/// default scalar fields so queried fields are present in returned rows.
///
/// # Errors
///
/// Returns `UPSTREAM_ERROR` if upstream bytes fail Protobuf decoding or `INVALID_ARGUMENT` if
/// a requested selection alias/field is invalid.
pub fn decode_response_row(
    output_desc: &MessageDescriptor,
    raw_bytes: &[u8],
    selection: Option<&Selection>,
) -> NgcResult<prost_types::Struct> {
    let dynamic = DynamicMessage::decode(output_desc.clone(), raw_bytes).map_err(|err| {
        NgcErrorInfo::new(
            NgcErrorCode::UpstreamError,
            format!(
                "failed to decode upstream `{}` protobuf payload: {err}",
                output_desc.full_name()
            ),
        )
    })?;

    let ser_opts = SerializeOptions::new()
        .use_proto_field_name(true)
        .skip_default_fields(false)
        .use_enum_numbers(false)
        .stringify_64_bit_integers(true);

    let mut json_buf = Vec::with_capacity(raw_bytes.len().saturating_mul(2).max(64));
    let mut serializer = serde_json::Serializer::new(&mut json_buf);
    dynamic
        .serialize_with_options(&mut serializer, &ser_opts)
        .map_err(|err| {
            NgcErrorInfo::new(
                NgcErrorCode::Internal,
                format!(
                    "failed to convert decoded `{}` to JSON: {err}",
                    output_desc.full_name()
                ),
            )
        })?;

    let decoded_val: Value = serde_json::from_slice(&json_buf).map_err(|err| {
        NgcErrorInfo::new(
            NgcErrorCode::Internal,
            format!("failed to parse serialized protobuf JSON: {err}"),
        )
    })?;

    let projected = match selection {
        Some(sel) if !sel.fields.is_empty() => apply_selection(&decoded_val, sel),
        _ => decoded_val,
    };

    match projected {
        Value::Object(map) => Ok(json_map_to_prost_struct(&map)),
        other => {
            let mut wrap = Map::new();
            wrap.insert("value".to_owned(), other);
            Ok(json_map_to_prost_struct(&wrap))
        }
    }
}

/// Recursively projects JSON `value` onto an NGC `Selection` tree (`NGC-024`).
#[must_use]
pub fn apply_selection(value: &Value, selection: &Selection) -> Value {
    if selection.fields.is_empty() {
        return value.clone();
    }

    match value {
        Value::Array(items) => Value::Array(
            items
                .iter()
                .map(|item| apply_selection(item, selection))
                .collect(),
        ),
        Value::Object(obj) => {
            let mut out = Map::with_capacity(selection.fields.len());
            for field in &selection.fields {
                if field.name == "__typename" {
                    let out_key = if field.alias.is_empty() {
                        "__typename"
                    } else {
                        field.alias.as_str()
                    };
                    if let Some(tn) = obj.get("__typename") {
                        out.insert(out_key.to_owned(), tn.clone());
                    }
                    continue;
                }

                let out_key = if field.alias.is_empty() {
                    field.name.clone()
                } else {
                    field.alias.clone()
                };

                let selected_val = match obj.get(&field.name) {
                    Some(raw) => match &field.children {
                        Some(child_sel) if !child_sel.fields.is_empty() => {
                            apply_selection(raw, child_sel)
                        }
                        _ => raw.clone(),
                    },
                    None => Value::Null,
                };
                out.insert(out_key, selected_val);
            }
            Value::Object(out)
        }
        other => other.clone(),
    }
}
