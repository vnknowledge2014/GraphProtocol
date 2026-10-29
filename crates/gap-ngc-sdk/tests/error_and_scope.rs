//! TDD tests for `gap_ngc_sdk::error` and `gap_ngc_sdk::connector` (`NGC-031`, `NGC-060`..`063`, `NGC-080`, `NGC-090`, `NGC-094`, `INV-15`).
#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::future::{Future, ready};
use std::time::Duration;

use futures_util::stream;
use gap_core::error::{MAX_ERROR_MESSAGE_CHARS, NgcErrorCode, NgcErrorInfo};
use gap_ngc_sdk::connector::{BoxStream, CallScope, Connector, NgcResult};
use gap_ngc_sdk::error::{
    NGC_ERROR_METADATA_KEY, http_status_for_code, json_map_to_prost_struct,
    ngc_error_code_to_tonic, ngc_error_from_proto, ngc_error_to_proto, ngc_error_to_status,
    prost_struct_to_json_map, retry_after_seconds, sanitize_error_info, sanitize_proto_error,
    status_to_ngc_error,
};
use gap_proto::ngc::v1::{
    CancelTaskRequest, DescribeRequest, DescribeResponse, GetTaskRequest, HealthRequest,
    MutateRequest, NgcError, QueryRequest, QueryResponse, ServingStatus, SketchRequest,
    StartTaskRequest, SubscribeRequest, WatchTaskRequest,
};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

const ALL_NGC_CODES: [NgcErrorCode; 14] = [
    NgcErrorCode::Unspecified,
    NgcErrorCode::InvalidArgument,
    NgcErrorCode::NotFound,
    NgcErrorCode::PermissionDenied,
    NgcErrorCode::Unauthenticated,
    NgcErrorCode::ResourceExhausted,
    NgcErrorCode::FailedPrecondition,
    NgcErrorCode::DeadlineExceeded,
    NgcErrorCode::Cancelled,
    NgcErrorCode::Unavailable,
    NgcErrorCode::Unimplemented,
    NgcErrorCode::Internal,
    NgcErrorCode::Conflict,
    NgcErrorCode::UpstreamError,
];

struct MinimalConnector;

impl Connector for MinimalConnector {
    fn describe(
        &self,
        _request: DescribeRequest,
        _scope: CallScope,
    ) -> impl Future<Output = NgcResult<DescribeResponse>> + Send {
        ready(Ok(DescribeResponse {
            not_modified: true,
            description: None,
        }))
    }

    fn query(
        &self,
        _request: QueryRequest,
        _scope: CallScope,
    ) -> impl Future<Output = NgcResult<BoxStream<QueryResponse>>> + Send {
        let s: BoxStream<QueryResponse> = Box::pin(stream::empty());
        ready(Ok(s))
    }
}

// spec: NGC-060, NGC-061, NGC-063
#[test]
fn ngc_error_info_and_proto_ngc_error_roundtrip_all_codes_and_details() {
    let mut details = serde_json::Map::new();
    details.insert("field".to_owned(), serde_json::json!("customer_id"));
    details.insert("count".to_owned(), serde_json::json!(3.5));
    details.insert("active".to_owned(), serde_json::json!(true));
    details.insert("empty".to_owned(), serde_json::Value::Null);
    details.insert(
        "nested".to_owned(),
        serde_json::json!({"items": ["a", 2.0, false]}),
    );

    for code in ALL_NGC_CODES {
        let info = NgcErrorInfo::new(code, "validation or execution error")
            .with_retry(code == NgcErrorCode::ResourceExhausted, 1_250)
            .with_details(details.clone());

        let proto = ngc_error_to_proto(&info);
        assert_eq!(proto.code, i32::from(code));
        assert_eq!(proto.message, info.message());
        assert_eq!(proto.retryable, info.retryable);
        assert_eq!(proto.retry_after_ms, 1_250);
        assert!(proto.details.is_some());

        let roundtripped = ngc_error_from_proto(&proto);
        assert_eq!(roundtripped, info);
    }

    // Struct <-> JSON map direct round-trip
    let prost_struct = json_map_to_prost_struct(&details);
    let back_map = prost_struct_to_json_map(&prost_struct);
    assert_eq!(back_map, details);
}

// spec: NGC-063
#[test]
fn unknown_wire_error_codes_map_to_unspecified_without_panicking() {
    for unknown_tag in [-1, -999, 14, 99, i32::MAX, i32::MIN] {
        let proto = NgcError {
            code: unknown_tag,
            message: "future error variant".to_owned(),
            retryable: true,
            retry_after_ms: 500,
            details: None,
        };
        let decoded = ngc_error_from_proto(&proto);
        assert_eq!(decoded.code, NgcErrorCode::Unspecified);
        assert_eq!(decoded.message(), "future error variant");
        assert!(decoded.retryable);
        assert_eq!(decoded.retry_after_ms, 500);
    }
}

// spec: NGC-062, NGC-063
#[test]
fn tonic_status_conversion_carries_ngc_error_bin_and_falls_back_to_code_table() {
    let expected_tonic_table = [
        (NgcErrorCode::InvalidArgument, tonic::Code::InvalidArgument),
        (NgcErrorCode::NotFound, tonic::Code::NotFound),
        (
            NgcErrorCode::PermissionDenied,
            tonic::Code::PermissionDenied,
        ),
        (NgcErrorCode::Unauthenticated, tonic::Code::Unauthenticated),
        (
            NgcErrorCode::ResourceExhausted,
            tonic::Code::ResourceExhausted,
        ),
        (
            NgcErrorCode::FailedPrecondition,
            tonic::Code::FailedPrecondition,
        ),
        (
            NgcErrorCode::DeadlineExceeded,
            tonic::Code::DeadlineExceeded,
        ),
        (NgcErrorCode::Cancelled, tonic::Code::Cancelled),
        (NgcErrorCode::Unavailable, tonic::Code::Unavailable),
        (NgcErrorCode::UpstreamError, tonic::Code::Unavailable),
        (NgcErrorCode::Unimplemented, tonic::Code::Unimplemented),
        (NgcErrorCode::Internal, tonic::Code::Internal),
        (NgcErrorCode::Unspecified, tonic::Code::Internal),
        (NgcErrorCode::Conflict, tonic::Code::Aborted),
    ];

    let mut details = serde_json::Map::new();
    details.insert("hint".to_owned(), serde_json::json!("check quota"));

    for (ngc_code, expected_tonic) in expected_tonic_table {
        assert_eq!(ngc_error_code_to_tonic(ngc_code), expected_tonic);

        // Even codes that share a tonic::Code (e.g. UpstreamError vs Unavailable,
        // Unspecified vs Internal) preserve their exact NgcErrorCode via `ngc-error-bin`.
        let original = NgcErrorInfo::new(ngc_code, "detailed connector failure")
            .with_retry(true, 750)
            .with_details(details.clone());
        let status = ngc_error_to_status(&original);
        assert_eq!(status.code(), expected_tonic);
        assert!(status.metadata().get_bin(NGC_ERROR_METADATA_KEY).is_some());

        let recovered = status_to_ngc_error(&status);
        assert_eq!(recovered, original);
    }

    // Fallback when `ngc-error-bin` is absent or corrupt:
    let bare_aborted = tonic::Status::aborted("optimistic lock conflict");
    let from_bare = status_to_ngc_error(&bare_aborted);
    assert_eq!(from_bare.code, NgcErrorCode::Conflict);
    assert_eq!(from_bare.message(), "optimistic lock conflict");

    let bare_exhausted = tonic::Status::resource_exhausted("rate limited");
    let from_exhausted = status_to_ngc_error(&bare_exhausted);
    assert_eq!(from_exhausted.code, NgcErrorCode::ResourceExhausted);
    assert!(from_exhausted.retryable);

    let mut corrupt = tonic::Status::not_found("missing item");
    corrupt.metadata_mut().insert_bin(
        NGC_ERROR_METADATA_KEY,
        tonic::metadata::MetadataValue::from_bytes(&[0xff, 0xff, 0xff]),
    );
    let from_corrupt = status_to_ngc_error(&corrupt);
    assert_eq!(from_corrupt.code, NgcErrorCode::NotFound);
    assert_eq!(from_corrupt.message(), "missing item");
}

// spec: NGC-094
#[test]
fn http_status_table_and_retry_after_rounding_match_ngc_094() {
    let expected = [
        (NgcErrorCode::InvalidArgument, 400),
        (NgcErrorCode::Unauthenticated, 401),
        (NgcErrorCode::PermissionDenied, 403),
        (NgcErrorCode::NotFound, 404),
        (NgcErrorCode::Conflict, 409),
        (NgcErrorCode::FailedPrecondition, 412),
        (NgcErrorCode::ResourceExhausted, 429),
        (NgcErrorCode::Cancelled, 499),
        (NgcErrorCode::Internal, 500),
        (NgcErrorCode::Unspecified, 500),
        (NgcErrorCode::Unimplemented, 501),
        (NgcErrorCode::UpstreamError, 502),
        (NgcErrorCode::Unavailable, 503),
        (NgcErrorCode::DeadlineExceeded, 504),
    ];

    for (code, status_u16) in expected {
        assert_eq!(http_status_for_code(code).as_u16(), status_u16);
    }

    // Retry-After is emitted on RESOURCE_EXHAUSTED when retry_after_ms > 0, rounded up to whole seconds.
    let zero_ms = NgcErrorInfo::new(NgcErrorCode::ResourceExhausted, "busy").with_retry(true, 0);
    assert_eq!(retry_after_seconds(&zero_ms), None);

    let one_ms = NgcErrorInfo::new(NgcErrorCode::ResourceExhausted, "busy").with_retry(true, 1);
    assert_eq!(retry_after_seconds(&one_ms), Some(1));

    let exact_sec =
        NgcErrorInfo::new(NgcErrorCode::ResourceExhausted, "busy").with_retry(true, 1_000);
    assert_eq!(retry_after_seconds(&exact_sec), Some(1));

    let ceil_sec =
        NgcErrorInfo::new(NgcErrorCode::ResourceExhausted, "busy").with_retry(true, 1_001);
    assert_eq!(retry_after_seconds(&ceil_sec), Some(2));

    let non_exhausted =
        NgcErrorInfo::new(NgcErrorCode::Unavailable, "transient").with_retry(true, 2_500);
    assert_eq!(retry_after_seconds(&non_exhausted), None);
}

// spec: NGC-061, NGC-093, INV-15
#[test]
fn error_sanitization_clamps_long_messages_and_drops_oversized_details() {
    let mut big_details = serde_json::Map::new();
    big_details.insert(
        "blob".to_owned(),
        serde_json::Value::String("d".repeat(5_000)),
    );

    let info = NgcErrorInfo::new(NgcErrorCode::Internal, "m".repeat(10_000))
        .with_details(big_details.clone());
    let sanitized = sanitize_error_info(info, 4_096);
    assert_eq!(sanitized.message().chars().count(), MAX_ERROR_MESSAGE_CHARS);
    assert!(sanitized.details.is_none());

    let mut small_details = serde_json::Map::new();
    small_details.insert("ok".to_owned(), serde_json::json!(true));
    let kept = sanitize_error_info(
        NgcErrorInfo::new(NgcErrorCode::InvalidArgument, "bad arg")
            .with_details(small_details.clone()),
        4_096,
    );
    assert_eq!(kept.details, Some(small_details));

    let mut proto = NgcError {
        code: i32::from(NgcErrorCode::UpstreamError),
        message: "p".repeat(10_000),
        retryable: false,
        retry_after_ms: 0,
        details: Some(json_map_to_prost_struct(&big_details)),
    };
    sanitize_proto_error(&mut proto, 4_096);
    assert_eq!(proto.message.chars().count(), MAX_ERROR_MESSAGE_CHARS);
    assert!(proto.details.is_none());
}

// spec: INV-15, NGC-080
#[test]
fn inv_15_call_scope_debug_redacts_authorization_and_sensitive_metadata() {
    let mut metadata = BTreeMap::new();
    metadata.insert(
        "authorization".to_owned(),
        "Bearer super-secret-jwt-value".to_owned(),
    );
    metadata.insert(
        "x-trace-context-bin".to_owned(),
        "raw-binary-secret".to_owned(),
    );
    metadata.insert("x-refresh-token".to_owned(), "refresh-tok-value".to_owned());
    metadata.insert("x-client-secret".to_owned(), "client-sec-value".to_owned());
    metadata.insert("x-api-key".to_owned(), "api-key-12345".to_owned());
    metadata.insert(
        "x-Safe-Header".to_owned(),
        "visible-header-value".to_owned(),
    );

    let scope = CallScope {
        cancel: CancellationToken::new(),
        deadline: Instant::now() + Duration::from_secs(30),
        request_id: "req-001".to_owned(),
        tenant_id: "tenant-alpha".to_owned(),
        traceparent: "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01".to_owned(),
        metadata,
    };

    let dbg = format!("{scope:?}");
    assert!(dbg.contains("req-001"));
    assert!(dbg.contains("tenant-alpha"));
    assert!(dbg.contains("visible-header-value"));
    assert!(dbg.contains("[REDACTED]"));

    for forbidden in [
        "super-secret-jwt-value",
        "raw-binary-secret",
        "refresh-tok-value",
        "client-sec-value",
        "api-key-12345",
    ] {
        assert!(
            !dbg.contains(forbidden),
            "CallScope Debug leaked sensitive value `{forbidden}` in `{dbg}`"
        );
    }
}

// spec: NGC-031, NGC-090
#[tokio::test(start_paused = true)]
async fn minimal_connector_defaults_health_to_serving_and_optional_rpcs_to_unimplemented() {
    let conn = MinimalConnector;
    let mk_scope = || CallScope::for_test(Instant::now() + Duration::from_secs(5));

    let health = conn
        .health(HealthRequest {}, mk_scope())
        .await
        .expect("default health succeeds");
    assert_eq!(health.status, ServingStatus::Serving as i32);

    // Optional RPCs default to UNIMPLEMENTED (including Mutate with dry_run = true, NGC-031).
    let mutate_dry = conn
        .mutate(
            MutateRequest {
                dry_run: true,
                ..MutateRequest::default()
            },
            mk_scope(),
        )
        .await
        .expect_err("default mutate(dry_run=true) must return UNIMPLEMENTED");
    assert_eq!(mutate_dry.code, NgcErrorCode::Unimplemented);

    assert_eq!(
        conn.subscribe(SubscribeRequest::default(), mk_scope())
            .await
            .err()
            .expect("subscribe unimplemented")
            .code,
        NgcErrorCode::Unimplemented
    );
    assert_eq!(
        conn.start_task(StartTaskRequest::default(), mk_scope())
            .await
            .expect_err("start_task unimplemented")
            .code,
        NgcErrorCode::Unimplemented
    );
    assert_eq!(
        conn.get_task(GetTaskRequest::default(), mk_scope())
            .await
            .expect_err("get_task unimplemented")
            .code,
        NgcErrorCode::Unimplemented
    );
    assert_eq!(
        conn.cancel_task(CancelTaskRequest::default(), mk_scope())
            .await
            .expect_err("cancel_task unimplemented")
            .code,
        NgcErrorCode::Unimplemented
    );
    assert_eq!(
        conn.watch_task(WatchTaskRequest::default(), mk_scope())
            .await
            .err()
            .expect("watch_task unimplemented")
            .code,
        NgcErrorCode::Unimplemented
    );
    assert_eq!(
        conn.sketch(SketchRequest::default(), mk_scope())
            .await
            .expect_err("sketch unimplemented")
            .code,
        NgcErrorCode::Unimplemented
    );
    assert_eq!(
        conn.explain(QueryRequest::default(), mk_scope())
            .await
            .expect_err("explain unimplemented")
            .code,
        NgcErrorCode::Unimplemented
    );
}
