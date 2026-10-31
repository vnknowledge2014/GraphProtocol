//! `graphql-sse` v2.6.3 distinct-connections SSE framing and bounded `mpsc` event stream
//! (`Brief §3.7`, `GQL-003`, `OPS-002`, `ARC-011`, `ARC-012`, `RUN-031`).

use std::convert::Infallible;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use axum::body::Body;
use axum::http::{Response, StatusCode, header};
use bytes::Bytes;
use futures_util::Stream;
use gap_core::error::GapError;
use tokio::sync::mpsc;
use tokio_util::sync::{CancellationToken, DropGuard};

use crate::metrics::TransportMetrics;
use crate::operation::{CONTENT_TYPE_EVENT_STREAM, serialize_errors_envelope};

/// Formats a `graphql-sse` v2.6.3 `next` event frame (`event: next\ndata: <json>\n\n`).
#[must_use]
pub fn format_sse_next(json_bytes: &[u8]) -> Bytes {
    let text = std::str::from_utf8(json_bytes).unwrap_or(r#"{"data":null}"#);
    let mut buf = Vec::with_capacity(text.len() + 24);
    buf.extend_from_slice(b"event: next\n");
    let mut emitted_line = false;
    for line in text.lines() {
        buf.extend_from_slice(b"data: ");
        buf.extend_from_slice(line.as_bytes());
        buf.push(b'\n');
        emitted_line = true;
    }
    if !emitted_line {
        buf.extend_from_slice(b"data: {}\n");
    }
    buf.push(b'\n');
    Bytes::from(buf)
}

/// Formats the terminal `graphql-sse` v2.6.3 `complete` event frame (`event: complete\ndata: \n\n`,
/// `third_party/specs/graphql-sse-v2.6.3/PROTOCOL.md` L47–52).
#[must_use]
pub const fn format_sse_complete() -> Bytes {
    Bytes::from_static(b"event: complete\ndata: \n\n")
}

/// Bounded `mpsc` SSE receiver handle paired with its connection [`CancellationToken`].
#[derive(Debug)]
pub struct SseStreamHandle {
    /// Bounded SSE frame receiver (`ARC-011`).
    pub rx: mpsc::Receiver<Bytes>,
    /// Connection cancellation token cancelled when the HTTP stream drops (`RUN-031`).
    pub cancel: CancellationToken,
}

/// `Stream` adapter over a bounded `mpsc::Receiver<Bytes>` holding a [`DropGuard`] so dropping the
/// HTTP response body immediately cancels downstream execution (`RUN-031`, `ARC-012`).
pub struct SseBodyStream {
    rx: mpsc::Receiver<Bytes>,
    _cancel_drop_guard: DropGuard,
}

impl SseBodyStream {
    /// Wraps `rx` and `cancel_drop_guard` into an HTTP body [`Stream`].
    #[must_use]
    pub fn new(rx: mpsc::Receiver<Bytes>, cancel_drop_guard: DropGuard) -> Self {
        Self {
            rx,
            _cancel_drop_guard: cancel_drop_guard,
        }
    }
}

impl Stream for SseBodyStream {
    type Item = Result<Bytes, Infallible>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.rx.poll_recv(cx).map(|opt| opt.map(Ok))
    }
}

/// Builds a `200 OK` `text/event-stream` HTTP [`Response`] backed by [`SseBodyStream`].
#[must_use]
pub fn build_sse_response(handle: SseStreamHandle) -> Response<Body> {
    let drop_guard = handle.cancel.drop_guard();
    let stream = SseBodyStream::new(handle.rx, drop_guard);
    let mut resp = Response::new(Body::from_stream(stream));
    *resp.status_mut() = StatusCode::OK;
    let headers = resp.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static(CONTENT_TYPE_EVENT_STREAM),
    );
    headers.insert(
        header::CACHE_CONTROL,
        axum::http::HeaderValue::from_static("no-cache"),
    );
    headers.insert(
        header::CONNECTION,
        axum::http::HeaderValue::from_static("keep-alive"),
    );
    resp
}

/// Sends `next_frame` followed by `complete` over bounded `tx`, aborting cleanly on `cancel` or
/// receiver drop (`ARC-011`, `RUN-031`).
pub async fn send_next_and_complete(
    tx: mpsc::Sender<Bytes>,
    next_frame: Bytes,
    cancel: CancellationToken,
) {
    let sent_next = tokio::select! {
        biased;
        () = cancel.cancelled() => false,
        res = tx.send(next_frame) => res.is_ok(),
    };
    if !sent_next {
        return;
    }
    let _ = tokio::select! {
        biased;
        () = cancel.cancelled() => Err(()),
        res = tx.send(format_sse_complete()) => res.map_err(|_| ()),
    };
}

/// Streams pre-execution GraphQL parse/validation errors as `event: next` + `event: complete` on a
/// `200 OK` `text/event-stream` response (`Brief §3.7`, `graphql-sse` `PROTOCOL.md` L27–45).
#[must_use]
pub fn sse_request_error_response(
    errors: Vec<GapError>,
    request_id: &str,
    channel_capacity: usize,
    metrics: &Arc<TransportMetrics>,
    cancel: CancellationToken,
) -> Response<Body> {
    let cap = channel_capacity.max(1);
    let (tx, rx) = mpsc::channel::<Bytes>(cap);
    let error_json = serialize_errors_envelope(errors, request_id);
    let next_frame = format_sse_next(&error_json);
    let producer_guard = metrics.track_sse_producer();
    let producer_cancel = cancel.clone();

    tokio::spawn(async move {
        let _guard = producer_guard;
        send_next_and_complete(tx, next_frame, producer_cancel).await;
    });

    build_sse_response(SseStreamHandle { rx, cancel })
}
