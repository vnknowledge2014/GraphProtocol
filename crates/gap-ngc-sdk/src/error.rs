//! Error conversions between [`NgcErrorInfo`], wire [`NgcError`], [`tonic::Status`], and HTTP status codes (`NGC-060`..`063`, `NGC-093`, `NGC-094`).

use std::collections::BTreeMap;

use gap_core::error::{NgcErrorCode, NgcErrorInfo, clamp_error_message};
use gap_proto::ngc::v1::NgcError;
use http::StatusCode;
use prost::Message as _;
use tonic::metadata::MetadataValue;

/// Binary gRPC metadata key carrying prost-encoded [`NgcError`] payloads (`NGC-062`, `NGC-063`).
pub const NGC_ERROR_METADATA_KEY: &str = "ngc-error-bin";

/// Default maximum serialized JSON byte size for `NgcError.details` before dropping (`NGC-093` (e)).
pub const DEFAULT_MAX_ERROR_DETAILS_BYTES: usize = 4 * 1024;

/// Fixed sanitized message emitted when a connector panics (`NGC-061`, `NGC-093` (d)).
pub const CONNECTOR_PANIC_MESSAGE: &str = "connector panicked";

/// Converts a [`prost_types::Value`] into a [`serde_json::Value`].
#[must_use]
pub fn prost_value_to_json(val: &prost_types::Value) -> serde_json::Value {
    match &val.kind {
        None | Some(prost_types::value::Kind::NullValue(_)) => serde_json::Value::Null,
        Some(prost_types::value::Kind::NumberValue(n)) => serde_json::Number::from_f64(*n)
            .map_or(serde_json::Value::Null, serde_json::Value::Number),
        Some(prost_types::value::Kind::StringValue(s)) => serde_json::Value::String(s.clone()),
        Some(prost_types::value::Kind::BoolValue(b)) => serde_json::Value::Bool(*b),
        Some(prost_types::value::Kind::StructValue(st)) => {
            serde_json::Value::Object(prost_struct_to_json_map(st))
        }
        Some(prost_types::value::Kind::ListValue(list)) => {
            serde_json::Value::Array(list.values.iter().map(prost_value_to_json).collect())
        }
    }
}

/// Converts a [`serde_json::Value`] into a [`prost_types::Value`].
#[must_use]
pub fn json_to_prost_value(val: &serde_json::Value) -> prost_types::Value {
    let kind = match val {
        serde_json::Value::Null => prost_types::value::Kind::NullValue(0),
        serde_json::Value::Bool(b) => prost_types::value::Kind::BoolValue(*b),
        serde_json::Value::Number(n) => {
            prost_types::value::Kind::NumberValue(n.as_f64().unwrap_or(0.0))
        }
        serde_json::Value::String(s) => prost_types::value::Kind::StringValue(s.clone()),
        serde_json::Value::Array(items) => {
            prost_types::value::Kind::ListValue(prost_types::ListValue {
                values: items.iter().map(json_to_prost_value).collect(),
            })
        }
        serde_json::Value::Object(map) => {
            prost_types::value::Kind::StructValue(json_map_to_prost_struct(map))
        }
    };
    prost_types::Value { kind: Some(kind) }
}

/// Converts a [`prost_types::Struct`] into a [`serde_json::Map`].
#[must_use]
pub fn prost_struct_to_json_map(
    st: &prost_types::Struct,
) -> serde_json::Map<String, serde_json::Value> {
    let mut map = serde_json::Map::with_capacity(st.fields.len());
    for (k, v) in &st.fields {
        map.insert(k.clone(), prost_value_to_json(v));
    }
    map
}

/// Converts a [`serde_json::Map`] into a [`prost_types::Struct`].
#[must_use]
pub fn json_map_to_prost_struct(
    map: &serde_json::Map<String, serde_json::Value>,
) -> prost_types::Struct {
    let mut fields = BTreeMap::new();
    for (k, v) in map {
        fields.insert(k.clone(), json_to_prost_value(v));
    }
    prost_types::Struct { fields }
}

/// Converts [`NgcErrorInfo`] into the wire [`NgcError`] protobuf message (`NGC-063`).
#[must_use]
pub fn ngc_error_to_proto(err: &NgcErrorInfo) -> NgcError {
    NgcError {
        code: i32::from(err.code),
        message: clamp_error_message(err.message()),
        retryable: err.retryable,
        retry_after_ms: err.retry_after_ms,
        details: err.details.as_ref().map(json_map_to_prost_struct),
    }
}

/// Converts a wire [`NgcError`] protobuf message into [`NgcErrorInfo`] (`NGC-063`).
///
/// Unrecognized wire `code` tags map to [`NgcErrorCode::Unspecified`] without panicking.
#[must_use]
pub fn ngc_error_from_proto(proto: &NgcError) -> NgcErrorInfo {
    let code = NgcErrorCode::try_from(proto.code).unwrap_or(NgcErrorCode::Unspecified);
    let mut info =
        NgcErrorInfo::new(code, &proto.message).with_retry(proto.retryable, proto.retry_after_ms);
    if let Some(ref st) = proto.details {
        info = info.with_details(prost_struct_to_json_map(st));
    }
    info
}

/// Extension trait providing ergonomic wire/status conversions on [`NgcErrorInfo`] (`NGC-063`).
pub trait NgcErrorExt {
    /// Converts `self` into a wire [`NgcError`] protobuf message.
    fn to_proto(&self) -> NgcError;

    /// Constructs [`NgcErrorInfo`] from a wire [`NgcError`] protobuf message.
    fn from_proto(proto: &NgcError) -> NgcErrorInfo;

    /// Converts `self` into a [`tonic::Status`] carrying `ngc-error-bin`.
    fn to_status(&self) -> tonic::Status;

    /// Extracts [`NgcErrorInfo`] from a [`tonic::Status`].
    fn from_status(status: &tonic::Status) -> NgcErrorInfo;
}

impl NgcErrorExt for NgcErrorInfo {
    fn to_proto(&self) -> NgcError {
        ngc_error_to_proto(self)
    }

    fn from_proto(proto: &NgcError) -> NgcErrorInfo {
        ngc_error_from_proto(proto)
    }

    fn to_status(&self) -> tonic::Status {
        ngc_error_to_status(self)
    }

    fn from_status(status: &tonic::Status) -> NgcErrorInfo {
        status_to_ngc_error(status)
    }
}

/// Maps an [`NgcErrorCode`] to its gRPC [`tonic::Code`] (`NGC-062`, `NGC-063`).
#[must_use]
pub const fn ngc_error_code_to_tonic(code: NgcErrorCode) -> tonic::Code {
    match code {
        NgcErrorCode::InvalidArgument => tonic::Code::InvalidArgument,
        NgcErrorCode::NotFound => tonic::Code::NotFound,
        NgcErrorCode::PermissionDenied => tonic::Code::PermissionDenied,
        NgcErrorCode::Unauthenticated => tonic::Code::Unauthenticated,
        NgcErrorCode::ResourceExhausted => tonic::Code::ResourceExhausted,
        NgcErrorCode::FailedPrecondition => tonic::Code::FailedPrecondition,
        NgcErrorCode::DeadlineExceeded => tonic::Code::DeadlineExceeded,
        NgcErrorCode::Cancelled => tonic::Code::Cancelled,
        NgcErrorCode::Unavailable | NgcErrorCode::UpstreamError => tonic::Code::Unavailable,
        NgcErrorCode::Unimplemented => tonic::Code::Unimplemented,
        NgcErrorCode::Internal | NgcErrorCode::Unspecified => tonic::Code::Internal,
        NgcErrorCode::Conflict => tonic::Code::Aborted,
    }
}

/// Maps a gRPC [`tonic::Code`] to [`NgcErrorCode`] when `ngc-error-bin` metadata is absent (`NGC-063`).
#[must_use]
pub const fn tonic_code_to_ngc(code: tonic::Code) -> NgcErrorCode {
    match code {
        tonic::Code::InvalidArgument | tonic::Code::OutOfRange => NgcErrorCode::InvalidArgument,
        tonic::Code::NotFound => NgcErrorCode::NotFound,
        tonic::Code::PermissionDenied => NgcErrorCode::PermissionDenied,
        tonic::Code::Unauthenticated => NgcErrorCode::Unauthenticated,
        tonic::Code::ResourceExhausted => NgcErrorCode::ResourceExhausted,
        tonic::Code::FailedPrecondition => NgcErrorCode::FailedPrecondition,
        tonic::Code::DeadlineExceeded => NgcErrorCode::DeadlineExceeded,
        tonic::Code::Cancelled => NgcErrorCode::Cancelled,
        tonic::Code::Unavailable => NgcErrorCode::Unavailable,
        tonic::Code::Unimplemented => NgcErrorCode::Unimplemented,
        tonic::Code::Aborted | tonic::Code::AlreadyExists => NgcErrorCode::Conflict,
        tonic::Code::Internal | tonic::Code::Unknown | tonic::Code::DataLoss | tonic::Code::Ok => {
            NgcErrorCode::Internal
        }
    }
}

/// Converts [`NgcErrorInfo`] into a [`tonic::Status`] carrying prost-encoded [`NgcError`] in `ngc-error-bin` (`NGC-062`, `NGC-063`).
#[must_use]
pub fn ngc_error_to_status(err: &NgcErrorInfo) -> tonic::Status {
    let proto = ngc_error_to_proto(err);
    let encoded = proto.encode_to_vec();
    let mut status = tonic::Status::new(ngc_error_code_to_tonic(err.code), err.message());
    status
        .metadata_mut()
        .insert_bin(NGC_ERROR_METADATA_KEY, MetadataValue::from_bytes(&encoded));
    status
}

/// Extracts [`NgcErrorInfo`] from a [`tonic::Status`], preferring `ngc-error-bin` and falling back to [`tonic_code_to_ngc`] (`NGC-063`).
#[must_use]
pub fn status_to_ngc_error(status: &tonic::Status) -> NgcErrorInfo {
    if let Some(bin_val) = status.metadata().get_bin(NGC_ERROR_METADATA_KEY)
        && let Ok(bytes) = bin_val.to_bytes()
        && let Ok(proto) = NgcError::decode(bytes)
    {
        return ngc_error_from_proto(&proto);
    }

    let code = tonic_code_to_ngc(status.code());
    let retryable = code == NgcErrorCode::ResourceExhausted;
    NgcErrorInfo::new(code, status.message()).with_retry(retryable, 0)
}

/// Maps an [`NgcErrorCode`] to its HTTP status code per `NGC-094`.
#[must_use]
pub fn http_status_for_code(code: NgcErrorCode) -> StatusCode {
    match code {
        NgcErrorCode::InvalidArgument => StatusCode::BAD_REQUEST,
        NgcErrorCode::Unauthenticated => StatusCode::UNAUTHORIZED,
        NgcErrorCode::PermissionDenied => StatusCode::FORBIDDEN,
        NgcErrorCode::NotFound => StatusCode::NOT_FOUND,
        NgcErrorCode::Conflict => StatusCode::CONFLICT,
        NgcErrorCode::FailedPrecondition => StatusCode::PRECONDITION_FAILED,
        NgcErrorCode::ResourceExhausted => StatusCode::TOO_MANY_REQUESTS,
        NgcErrorCode::Cancelled => {
            StatusCode::from_u16(499).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR)
        }
        NgcErrorCode::Internal | NgcErrorCode::Unspecified => StatusCode::INTERNAL_SERVER_ERROR,
        NgcErrorCode::Unimplemented => StatusCode::NOT_IMPLEMENTED,
        NgcErrorCode::UpstreamError => StatusCode::BAD_GATEWAY,
        NgcErrorCode::Unavailable => StatusCode::SERVICE_UNAVAILABLE,
        NgcErrorCode::DeadlineExceeded => StatusCode::GATEWAY_TIMEOUT,
    }
}

/// Computes the `Retry-After` HTTP header value in whole seconds (rounded up) when `code == RESOURCE_EXHAUSTED` and `retry_after_ms > 0` (`NGC-094`).
#[must_use]
pub const fn retry_after_seconds(err: &NgcErrorInfo) -> Option<u64> {
    if matches!(err.code, NgcErrorCode::ResourceExhausted) && err.retry_after_ms > 0 {
        Some((err.retry_after_ms as u64).div_ceil(1_000))
    } else {
        None
    }
}

fn details_exceed_limit(
    details: &serde_json::Map<String, serde_json::Value>,
    max_bytes: usize,
) -> bool {
    match serde_json::to_vec(details) {
        Ok(encoded) => encoded.len() > max_bytes,
        Err(_) => true,
    }
}

/// Clamps [`NgcErrorInfo`] message to `<= 300` Unicode scalar values and drops `details` when its JSON encoding exceeds `max_error_details_bytes` (`NGC-061`, `NGC-093` (e), `INV-15`).
#[must_use]
pub fn sanitize_error_info(mut err: NgcErrorInfo, max_error_details_bytes: usize) -> NgcErrorInfo {
    if let Some(ref details) = err.details
        && details_exceed_limit(details, max_error_details_bytes)
    {
        err.details = None;
    }
    let clamped = clamp_error_message(err.message());
    let mut out =
        NgcErrorInfo::new(err.code, clamped).with_retry(err.retryable, err.retry_after_ms);
    if let Some(details) = err.details {
        out = out.with_details(details);
    }
    out
}

/// Clamps an embedded wire [`NgcError`] message to `<= 300` Unicode scalar values and drops `details` exceeding `max_error_details_bytes` (`NGC-061`, `NGC-093` (e), `INV-15`).
pub fn sanitize_proto_error(proto: &mut NgcError, max_error_details_bytes: usize) {
    if NgcErrorCode::try_from(proto.code).is_err() {
        proto.code = i32::from(NgcErrorCode::Unspecified);
    }
    proto.message = clamp_error_message(&proto.message);
    if let Some(ref st) = proto.details {
        let json_map = prost_struct_to_json_map(st);
        if details_exceed_limit(&json_map, max_error_details_bytes) {
            proto.details = None;
        }
    }
}
