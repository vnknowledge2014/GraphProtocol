//! TDD tests for `gap_ngc_sdk::error`, `gap_ngc_sdk::connector`, and `gap_ngc_sdk::core` (`NGC-025`, `NGC-031`, `NGC-060`..`063`, `NGC-071`, `NGC-080`, `NGC-090`, `NGC-093`, `NGC-094`, `INV-12`, `INV-15`).
#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::future::{Future, ready};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll};
use std::time::Duration;

use futures_util::{Stream, StreamExt as _, stream};
use gap_core::clock::ManualClock;
use gap_core::error::{MAX_ERROR_MESSAGE_CHARS, NgcErrorCode, NgcErrorInfo};
use gap_ngc_sdk::connector::{BoxStream, CallScope, Connector, NgcResult};
use gap_ngc_sdk::core::{ServerConfig, ServerCore};
use gap_ngc_sdk::error::{
    NGC_ERROR_METADATA_KEY, http_status_for_code, json_map_to_prost_struct,
    ngc_error_code_to_tonic, ngc_error_from_proto, ngc_error_to_proto, ngc_error_to_status,
    prost_struct_to_json_map, retry_after_seconds, sanitize_error_info, sanitize_proto_error,
    status_to_ngc_error,
};
use gap_ngc_sdk::mock::MockConnector;
use gap_proto::ngc::v1::{
    CallContext, CancelTaskRequest, DescribeRequest, DescribeResponse, GetTaskRequest,
    HealthRequest, MutateRequest, MutateResponse, NgcError, QueryRequest, QueryResponse,
    RowSetChunk, ServingStatus, SketchRequest, StartTaskRequest, SubscribeRequest,
    WatchTaskRequest, query_response,
};
use tokio::sync::Notify;
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
    metadata.insert("cookie".to_owned(), "sid=cookie-lower-val".to_owned());
    metadata.insert("Cookie".to_owned(), "sid=cookie-mixed-val".to_owned());
    metadata.insert(
        "set-cookie".to_owned(),
        "sid=set-cookie-lower-val; HttpOnly".to_owned(),
    );
    metadata.insert(
        "Set-Cookie".to_owned(),
        "sid=set-cookie-mixed-val; Secure".to_owned(),
    );
    metadata.insert(
        "proxy-authorization".to_owned(),
        "Basic proxy-auth-lower-val".to_owned(),
    );
    metadata.insert(
        "Proxy-Authorization".to_owned(),
        "Bearer proxy-auth-mixed-val".to_owned(),
    );
    metadata.insert(
        "x-trace-context-bin".to_owned(),
        "raw-binary-secret".to_owned(),
    );
    metadata.insert("x-refresh-token".to_owned(), "refresh-tok-value".to_owned());
    metadata.insert("x-client-secret".to_owned(), "client-sec-value".to_owned());
    metadata.insert("x-api-key".to_owned(), "api-key-12345".to_owned());
    metadata.insert(
        "traceparent".to_owned(),
        "00-traceparent-metadata-visible".to_owned(),
    );
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
    assert!(dbg.contains("00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01"));
    assert!(dbg.contains("00-traceparent-metadata-visible"));
    assert!(dbg.contains("visible-header-value"));
    assert!(dbg.contains("[REDACTED]"));

    for forbidden in [
        "super-secret-jwt-value",
        "cookie-lower-val",
        "cookie-mixed-val",
        "set-cookie-lower-val",
        "set-cookie-mixed-val",
        "proxy-auth-lower-val",
        "proxy-auth-mixed-val",
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

// spec: NGC-025, NGC-093, INV-12
#[tokio::test(start_paused = true)]
async fn server_core_negative_deadline_unix_ms_fails_deadline_exceeded_without_invoking_connector()
{
    let clock = Arc::new(ManualClock::new(1_700_000_000_000));
    let mock = MockConnector::new()
        .on_mutate(|_req, _scope| Ok(MutateResponse::default()))
        .on_query(|_req, _scope| Ok(Box::pin(stream::empty())));
    let core = ServerCore::new(mock.clone(), ServerConfig::default().with_clock(clock));

    for neg_deadline in [-1_i64, -10_000_i64, i64::MIN] {
        let ctx = Some(CallContext {
            deadline_unix_ms: neg_deadline,
            ..CallContext::default()
        });
        let u_err = core
            .mutate(
                MutateRequest {
                    ctx: ctx.clone(),
                    ..MutateRequest::default()
                },
                BTreeMap::new(),
                CancellationToken::new(),
            )
            .await
            .expect_err("negative deadline_unix_ms must fail unary call");
        assert_eq!(u_err.code, NgcErrorCode::DeadlineExceeded);

        let s_err = core
            .query(
                QueryRequest {
                    ctx,
                    ..QueryRequest::default()
                },
                BTreeMap::new(),
                CancellationToken::new(),
            )
            .await
            .err()
            .expect("negative deadline_unix_ms must fail stream open");
        assert_eq!(s_err.code, NgcErrorCode::DeadlineExceeded);
    }

    assert_eq!(
        mock.total_calls(),
        0,
        "connector must never be invoked when deadline_unix_ms is negative"
    );
}

// spec: NGC-025, NGC-093, INV-12
#[tokio::test(start_paused = true)]
async fn server_core_pre_cancelled_token_fails_cancelled_without_invoking_connector() {
    let mock = MockConnector::new()
        .on_describe(|_req, _scope| Ok(DescribeResponse::default()))
        .on_mutate(|_req, _scope| Ok(MutateResponse::default()))
        .on_query(|_req, _scope| Ok(Box::pin(stream::empty())));
    let core = ServerCore::new(mock.clone(), ServerConfig::default());

    let pre_cancelled = CancellationToken::new();
    pre_cancelled.cancel();

    let d_err = core
        .describe(
            DescribeRequest::default(),
            BTreeMap::new(),
            pre_cancelled.clone(),
        )
        .await
        .expect_err("pre-cancelled unary describe must fail CANCELLED");
    assert_eq!(d_err.code, NgcErrorCode::Cancelled);

    let m_err = core
        .mutate(
            MutateRequest::default(),
            BTreeMap::new(),
            pre_cancelled.clone(),
        )
        .await
        .expect_err("pre-cancelled unary mutate must fail CANCELLED");
    assert_eq!(m_err.code, NgcErrorCode::Cancelled);

    let q_err = core
        .query(QueryRequest::default(), BTreeMap::new(), pre_cancelled)
        .await
        .err()
        .expect("pre-cancelled stream query must fail CANCELLED");
    assert_eq!(q_err.code, NgcErrorCode::Cancelled);

    assert_eq!(
        mock.total_calls(),
        0,
        "connector must never be invoked when transport token is already cancelled"
    );
}

// spec: NGC-025, NGC-093, INV-12
#[tokio::test(start_paused = true)]
async fn server_core_post_completion_wall_clock_deadline_rejects_unary_and_stream_open() {
    // Test both exact equality (`now_ms == deadline_unix_ms`, advance = 500ms) and strict past (`advance = 501ms`)
    // while paused tokio `Instant` remains frozen at `t0`.
    for advance_ms in [500_u64, 501_u64] {
        let base_ms: u64 = 1_700_000_000_000;
        let deadline_ms: i64 = 1_700_000_000_500;
        let clock = Arc::new(ManualClock::new(base_ms));
        let clock_u = Arc::clone(&clock);
        let clock_s = Arc::clone(&clock);
        let t0 = Instant::now();

        let mock = MockConnector::new()
            .on_mutate(move |_req, _scope| {
                clock_u.advance(Duration::from_millis(advance_ms));
                Ok(MutateResponse::default())
            })
            .on_query(move |_req, _scope| {
                clock_s.advance(Duration::from_millis(advance_ms));
                let item = QueryResponse {
                    chunk: Some(query_response::Chunk::Rows(RowSetChunk {
                        foreach_index: 0,
                        rows: Vec::new(),
                        last_for_index: true,
                        error: None,
                    })),
                };
                Ok(Box::pin(stream::iter(vec![Ok(item)])))
            });

        let core = ServerCore::new(
            mock.clone(),
            ServerConfig::default().with_clock(clock.clone()),
        );

        let u_err = core
            .mutate(
                MutateRequest {
                    ctx: Some(CallContext {
                        deadline_unix_ms: deadline_ms,
                        ..CallContext::default()
                    }),
                    ..MutateRequest::default()
                },
                BTreeMap::new(),
                CancellationToken::new(),
            )
            .await
            .expect_err("unary response must not be delivered after wall clock passed deadline");
        assert_eq!(u_err.code, NgcErrorCode::DeadlineExceeded);

        // Reset ManualClock to `base_ms` for the stream-open check.
        let reset_clock = Arc::new(ManualClock::new(base_ms));
        let reset_clock_s = Arc::clone(&reset_clock);
        let stream_mock = MockConnector::new().on_query(move |_req, _scope| {
            reset_clock_s.advance(Duration::from_millis(advance_ms));
            Ok(Box::pin(stream::pending()))
        });
        let stream_core = ServerCore::new(
            stream_mock.clone(),
            ServerConfig::default().with_clock(reset_clock),
        );

        let s_err = stream_core
            .query(
                QueryRequest {
                    ctx: Some(CallContext {
                        deadline_unix_ms: deadline_ms,
                        ..CallContext::default()
                    }),
                    ..QueryRequest::default()
                },
                BTreeMap::new(),
                CancellationToken::new(),
            )
            .await
            .err()
            .expect("stream open must fail DEADLINE_EXCEEDED when wall clock passed deadline");
        assert_eq!(s_err.code, NgcErrorCode::DeadlineExceeded);
        assert_eq!(
            Instant::now().duration_since(t0),
            Duration::ZERO,
            "tokio monotonic clock must not have advanced"
        );
        tokio::time::timeout(Duration::from_secs(1), stream_mock.wait_for_drops(1))
            .await
            .expect("rejected stream must be dropped immediately");
    }
}

struct CancelObservingFuture<T> {
    scope_cancel: CancellationToken,
    started: Arc<Notify>,
    cancelled_when_dropped: Arc<AtomicBool>,
    _marker: std::marker::PhantomData<fn() -> T>,
}

impl<T> Future for CancelObservingFuture<T> {
    type Output = NgcResult<T>;

    fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.started.notify_one();
        Poll::Pending
    }
}

impl<T> Drop for CancelObservingFuture<T> {
    fn drop(&mut self) {
        self.cancelled_when_dropped
            .store(self.scope_cancel.is_cancelled(), Ordering::SeqCst);
    }
}

struct CancelObservingStream {
    scope_cancel: CancellationToken,
    cancelled_when_dropped: Arc<AtomicBool>,
}

impl Stream for CancelObservingStream {
    type Item = NgcResult<QueryResponse>;

    fn poll_next(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Poll::Pending
    }
}

impl Drop for CancelObservingStream {
    fn drop(&mut self) {
        self.cancelled_when_dropped
            .store(self.scope_cancel.is_cancelled(), Ordering::SeqCst);
    }
}

#[derive(Clone)]
struct CancelObservingConnector {
    started: Arc<Notify>,
    unary_cancelled_at_drop: Arc<AtomicBool>,
    unary_cancelled_observed: Arc<Notify>,
    stream_open_hang: Arc<AtomicBool>,
    stream_open_cancelled_at_drop: Arc<AtomicBool>,
    stream_cancelled_at_drop: Arc<AtomicBool>,
    stream_cancelled_observed: Arc<Notify>,
}

impl Connector for CancelObservingConnector {
    fn describe(
        &self,
        _request: DescribeRequest,
        scope: CallScope,
    ) -> impl Future<Output = NgcResult<DescribeResponse>> + Send {
        let obs = Arc::clone(&self.unary_cancelled_observed);
        let cancel_for_task = scope.cancel.clone();
        tokio::spawn(async move {
            cancel_for_task.cancelled().await;
            obs.notify_one();
        });
        CancelObservingFuture {
            scope_cancel: scope.cancel,
            started: Arc::clone(&self.started),
            cancelled_when_dropped: Arc::clone(&self.unary_cancelled_at_drop),
            _marker: std::marker::PhantomData,
        }
    }

    fn query(
        &self,
        _request: QueryRequest,
        scope: CallScope,
    ) -> impl Future<Output = NgcResult<BoxStream<QueryResponse>>> + Send {
        let hang_open = self.stream_open_hang.load(Ordering::SeqCst);
        let started = Arc::clone(&self.started);
        let open_drop = Arc::clone(&self.stream_open_cancelled_at_drop);
        let stream_drop = Arc::clone(&self.stream_cancelled_at_drop);
        let obs = Arc::clone(&self.stream_cancelled_observed);

        async move {
            if hang_open {
                let fut: CancelObservingFuture<BoxStream<QueryResponse>> = CancelObservingFuture {
                    scope_cancel: scope.cancel,
                    started,
                    cancelled_when_dropped: open_drop,
                    _marker: std::marker::PhantomData,
                };
                return fut.await;
            }
            let cancel_for_task = scope.cancel.clone();
            tokio::spawn(async move {
                cancel_for_task.cancelled().await;
                obs.notify_one();
            });
            let s: BoxStream<QueryResponse> = Box::pin(CancelObservingStream {
                scope_cancel: scope.cancel,
                cancelled_when_dropped: stream_drop,
            });
            Ok(s)
        }
    }
}

// spec: NGC-025, NGC-093, INV-12
#[tokio::test(start_paused = true)]
async fn server_core_cooperative_cancellation_cancels_call_scope_on_deadline_and_client_cancel() {
    let conn = CancelObservingConnector {
        started: Arc::new(Notify::new()),
        unary_cancelled_at_drop: Arc::new(AtomicBool::new(false)),
        unary_cancelled_observed: Arc::new(Notify::new()),
        stream_open_hang: Arc::new(AtomicBool::new(false)),
        stream_open_cancelled_at_drop: Arc::new(AtomicBool::new(false)),
        stream_cancelled_at_drop: Arc::new(AtomicBool::new(false)),
        stream_cancelled_observed: Arc::new(Notify::new()),
    };
    let core = Arc::new(ServerCore::new(
        conn.clone(),
        ServerConfig::default().with_default_timeout(Duration::from_millis(200)),
    ));

    // 1. Unary client cancel: `scope.cancel` must be cancelled BEFORE `guarded_fut` drops.
    let cancel_tok = CancellationToken::new();
    let cancel_driver = {
        let started = Arc::clone(&conn.started);
        let tok = cancel_tok.clone();
        tokio::spawn(async move {
            started.notified().await;
            tok.cancel();
        })
    };
    let u_cancel = core
        .describe(DescribeRequest::default(), BTreeMap::new(), cancel_tok)
        .await
        .expect_err("unary cancelled");
    cancel_driver.await.expect("driver");
    assert_eq!(u_cancel.code, NgcErrorCode::Cancelled);
    assert!(
        conn.unary_cancelled_at_drop.swap(false, Ordering::SeqCst),
        "unary scope.cancel must be cancelled when client cancels (before future drop)"
    );
    tokio::time::timeout(
        Duration::from_millis(50),
        conn.unary_cancelled_observed.notified(),
    )
    .await
    .expect("unary scope.cancel.cancelled() notified");

    // 2. Unary deadline expiry: `scope.cancel` must be cancelled BEFORE `guarded_fut` drops.
    let u_deadline = core
        .describe(
            DescribeRequest::default(),
            BTreeMap::new(),
            CancellationToken::new(),
        )
        .await
        .expect_err("unary deadline");
    assert_eq!(u_deadline.code, NgcErrorCode::DeadlineExceeded);
    assert!(
        conn.unary_cancelled_at_drop.swap(false, Ordering::SeqCst),
        "unary scope.cancel must be cancelled when deadline fires (before future drop)"
    );
    tokio::time::timeout(
        Duration::from_millis(50),
        conn.unary_cancelled_observed.notified(),
    )
    .await
    .expect("unary scope.cancel.cancelled() notified on deadline");

    // 3. Stream-open client cancel & stream-open deadline expiry.
    conn.stream_open_hang.store(true, Ordering::SeqCst);
    let open_cancel_tok = CancellationToken::new();
    let open_cancel_driver = {
        let started = Arc::clone(&conn.started);
        let tok = open_cancel_tok.clone();
        tokio::spawn(async move {
            started.notified().await;
            tok.cancel();
        })
    };
    let so_cancel = core
        .query(QueryRequest::default(), BTreeMap::new(), open_cancel_tok)
        .await
        .err()
        .expect("stream-open cancelled");
    open_cancel_driver.await.expect("driver");
    assert_eq!(so_cancel.code, NgcErrorCode::Cancelled);
    assert!(
        conn.stream_open_cancelled_at_drop
            .swap(false, Ordering::SeqCst),
        "stream-open scope.cancel must be cancelled on client cancel"
    );

    let so_deadline = core
        .query(
            QueryRequest::default(),
            BTreeMap::new(),
            CancellationToken::new(),
        )
        .await
        .err()
        .expect("stream-open deadline");
    assert_eq!(so_deadline.code, NgcErrorCode::DeadlineExceeded);
    assert!(
        conn.stream_open_cancelled_at_drop
            .swap(false, Ordering::SeqCst),
        "stream-open scope.cancel must be cancelled on deadline"
    );
    conn.stream_open_hang.store(false, Ordering::SeqCst);

    // 4. Open `CoreStream` client cancel: keep `s1` alive after terminal `Err(Cancelled)` so
    // `CoreStream._cancel_guard` has NOT dropped yet!
    let s1_tok = CancellationToken::new();
    let mut s1 = core
        .query(QueryRequest::default(), BTreeMap::new(), s1_tok.clone())
        .await
        .expect("open s1");
    s1_tok.cancel();
    let s1_err = s1
        .next()
        .await
        .expect("terminal cancel item")
        .expect_err("must be Err(Cancelled)");
    assert_eq!(s1_err.code, NgcErrorCode::Cancelled);
    assert!(
        conn.stream_cancelled_at_drop.swap(false, Ordering::SeqCst),
        "CoreStream::terminate must cancel scope.cancel before dropping inner stream (while CoreStream is still alive)"
    );
    tokio::time::timeout(
        Duration::from_millis(50),
        conn.stream_cancelled_observed.notified(),
    )
    .await
    .expect("stream scope.cancel.cancelled() notified while CoreStream is still alive");
    drop(s1);

    // 5. Open `CoreStream` deadline expiry: keep `s2` alive after terminal `Err(DeadlineExceeded)`!
    let mut s2 = core
        .query(
            QueryRequest::default(),
            BTreeMap::new(),
            CancellationToken::new(),
        )
        .await
        .expect("open s2");
    let s2_err = s2
        .next()
        .await
        .expect("terminal deadline item")
        .expect_err("must be Err(DeadlineExceeded)");
    assert_eq!(s2_err.code, NgcErrorCode::DeadlineExceeded);
    assert!(
        conn.stream_cancelled_at_drop.swap(false, Ordering::SeqCst),
        "CoreStream::terminate must cancel scope.cancel on deadline before dropping inner stream"
    );
    tokio::time::timeout(
        Duration::from_millis(50),
        conn.stream_cancelled_observed.notified(),
    )
    .await
    .expect("stream scope.cancel.cancelled() notified on deadline while CoreStream is still alive");
    drop(s2);
}

// spec: NGC-071, NGC-093
#[tokio::test(start_paused = true)]
async fn server_core_open_stream_holds_max_in_flight_permit_until_stream_ends() {
    let finish_gate = Arc::new(Notify::new());
    let gate_c = Arc::clone(&finish_gate);

    let mock = MockConnector::new().on_query(move |req, _scope| {
        if req.operation == "hold" {
            let gate = Arc::clone(&gate_c);
            let first = QueryResponse {
                chunk: Some(query_response::Chunk::Rows(RowSetChunk {
                    foreach_index: 0,
                    rows: Vec::new(),
                    last_for_index: true,
                    error: None,
                })),
            };
            let s = stream::unfold(0_u8, move |step| {
                let gate = Arc::clone(&gate);
                let first_item = first.clone();
                async move {
                    match step {
                        0 => Some((Ok(first_item), 1)),
                        1 => {
                            gate.notified().await;
                            None
                        }
                        _ => None,
                    }
                }
            });
            Ok(Box::pin(s))
        } else {
            Ok(Box::pin(stream::empty()))
        }
    });

    let core = ServerCore::new(mock, ServerConfig::default().with_max_in_flight(1));

    // Open stream 1 (`hold`) and read its first item; the stream is still open (`Pending` before `None`).
    let mut s1 = core
        .query(
            QueryRequest {
                operation: "hold".to_owned(),
                ..QueryRequest::default()
            },
            BTreeMap::new(),
            CancellationToken::new(),
        )
        .await
        .expect("open s1");
    assert!(s1.next().await.expect("first item").is_ok());

    // While `s1` is open, `max_in_flight = 1` permit MUST still be held -> 2nd `Query` fails `RESOURCE_EXHAUSTED`.
    let rejected = core
        .query(
            QueryRequest::default(),
            BTreeMap::new(),
            CancellationToken::new(),
        )
        .await
        .err()
        .expect("second Query must fail RESOURCE_EXHAUSTED while first stream is open");
    assert_eq!(rejected.code, NgcErrorCode::ResourceExhausted);
    assert!(rejected.retryable);

    // Let `s1` finish (`None`), while STILL holding `s1` binding alive: permit is released as soon as the stream ends!
    finish_gate.notify_one();
    assert!(s1.next().await.is_none(), "s1 reaches clean EOF");

    let mut s2 = core
        .query(
            QueryRequest::default(),
            BTreeMap::new(),
            CancellationToken::new(),
        )
        .await
        .expect("second Query must succeed after first stream ended");
    assert!(s2.next().await.is_none());
    drop(s1);
    drop(s2);

    // Also verify dropping an open stream before EOF releases the permit immediately.
    let s3 = core
        .query(
            QueryRequest {
                operation: "hold".to_owned(),
                ..QueryRequest::default()
            },
            BTreeMap::new(),
            CancellationToken::new(),
        )
        .await
        .expect("open s3");
    assert!(
        core.query(
            QueryRequest::default(),
            BTreeMap::new(),
            CancellationToken::new(),
        )
        .await
        .is_err()
    );
    drop(s3);
    assert!(
        core.query(
            QueryRequest::default(),
            BTreeMap::new(),
            CancellationToken::new(),
        )
        .await
        .is_ok()
    );
}
