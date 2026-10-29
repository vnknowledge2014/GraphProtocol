//! HTTP/JSON, NDJSON (`Query`), and SSE (`Subscribe`, `WatchTask`) server adapter (`NGC-002`, `NGC-093`, `NGC-094`).

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::response::Response;
use bytes::Bytes;
use futures_util::StreamExt as _;
use gap_core::error::{NgcErrorCode, NgcErrorInfo};
use gap_proto::ngc::v1::{
    CancelTaskRequest, DescribeRequest, GetTaskRequest, HealthRequest, MutateRequest, QueryRequest,
    SketchRequest, StartTaskRequest, SubscribeRequest, WatchTaskRequest,
};
use http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use tokio::time::{Instant, sleep_until};
use tokio_util::sync::CancellationToken;

use crate::connector::Connector;
use crate::core::{CoreStream, ServerCore, headers_to_metadata};
use crate::error::{
    http_status_for_code, ngc_error_to_proto, retry_after_seconds, sanitize_error_info,
};
use crate::json::{ProtoMessage, from_json_slice, to_json_string, to_json_value, to_json_vec};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HttpRpc {
    Describe,
    Query,
    Mutate,
    Subscribe,
    StartTask,
    GetTask,
    CancelTask,
    WatchTask,
    Sketch,
    Health,
    Explain,
}

impl HttpRpc {
    fn from_path(path: &str) -> Option<Self> {
        match path {
            "/gap/ngc/v1/Describe" => Some(Self::Describe),
            "/gap/ngc/v1/Query" => Some(Self::Query),
            "/gap/ngc/v1/Mutate" => Some(Self::Mutate),
            "/gap/ngc/v1/Subscribe" => Some(Self::Subscribe),
            "/gap/ngc/v1/StartTask" => Some(Self::StartTask),
            "/gap/ngc/v1/GetTask" => Some(Self::GetTask),
            "/gap/ngc/v1/CancelTask" => Some(Self::CancelTask),
            "/gap/ngc/v1/WatchTask" => Some(Self::WatchTask),
            "/gap/ngc/v1/Sketch" => Some(Self::Sketch),
            "/gap/ngc/v1/Health" => Some(Self::Health),
            "/gap/ngc/v1/Explain" => Some(Self::Explain),
            _ => None,
        }
    }

    const fn is_health(self) -> bool {
        matches!(self, Self::Health)
    }
}

/// Builds an [`axum::Router`] implementing `NGC-002` and `NGC-094` over `core`.
pub fn router<C: Connector>(core: Arc<ServerCore<C>>) -> Router {
    Router::new().fallback(dispatch::<C>).with_state(core)
}

fn is_application_json(headers: &HeaderMap) -> bool {
    let Some(raw) = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
    else {
        return false;
    };
    let media_type = raw.split(';').next().unwrap_or("").trim();
    media_type.eq_ignore_ascii_case("application/json")
}

fn json_response(
    status: StatusCode,
    extra_headers: &[(header::HeaderName, HeaderValue)],
    body: Vec<u8>,
) -> Response {
    let mut builder = Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json");
    for (k, v) in extra_headers {
        builder = builder.header(k, v);
    }
    builder
        .body(Body::from(body))
        .unwrap_or_else(|_| Response::new(Body::empty()))
}

fn ngc_error_response(
    override_status: Option<StatusCode>,
    extra_headers: &[(header::HeaderName, HeaderValue)],
    err: &NgcErrorInfo,
    max_error_details_bytes: usize,
) -> Response {
    let sanitized = sanitize_error_info(err.clone(), max_error_details_bytes);
    let status = override_status.unwrap_or_else(|| http_status_for_code(sanitized.code));
    let proto = ngc_error_to_proto(&sanitized);
    let body = to_json_vec(&proto).unwrap_or_else(|_| {
        b"{\"code\":\"INTERNAL\",\"message\":\"error serialization failed\"}".to_vec()
    });

    let mut headers_vec = extra_headers.to_vec();
    if let Some(secs) = retry_after_seconds(&sanitized)
        && let Ok(hv) = HeaderValue::from_str(&secs.to_string())
    {
        headers_vec.push((header::RETRY_AFTER, hv));
    }

    json_response(status, &headers_vec, body)
}

async fn read_bounded_body(
    headers: &HeaderMap,
    body: Body,
    max_bytes: usize,
    timeout_dur: Duration,
    max_details_bytes: usize,
) -> Result<Vec<u8>, Box<Response>> {
    if let Some(cl) = headers
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<usize>().ok())
        && cl > max_bytes
    {
        let err = NgcErrorInfo::new(
            NgcErrorCode::InvalidArgument,
            "request body exceeds max_request_bytes",
        );
        return Err(Box::new(ngc_error_response(
            Some(StatusCode::PAYLOAD_TOO_LARGE),
            &[],
            &err,
            max_details_bytes,
        )));
    }

    let deadline = Instant::now()
        .checked_add(timeout_dur)
        .unwrap_or_else(|| Instant::now() + Duration::from_secs(60));
    let mut sleep = Box::pin(sleep_until(deadline));
    let mut stream = body.into_data_stream();
    let mut out = Vec::new();

    loop {
        tokio::select! {
            biased;
            () = &mut sleep => {
                let err = NgcErrorInfo::new(
                    NgcErrorCode::DeadlineExceeded,
                    "timed out reading HTTP request body",
                );
                return Err(Box::new(ngc_error_response(
                    Some(StatusCode::GATEWAY_TIMEOUT),
                    &[],
                    &err,
                    max_details_bytes,
                )));
            }
            next = stream.next() => {
                match next {
                    None => return Ok(out),
                    Some(Err(io_err)) => {
                        let err = NgcErrorInfo::new(
                            NgcErrorCode::InvalidArgument,
                            format!("failed reading HTTP request body: {io_err}"),
                        );
                        return Err(Box::new(ngc_error_response(
                            None,
                            &[],
                            &err,
                            max_details_bytes,
                        )));
                    }
                    Some(Ok(chunk)) => {
                        if out.len().saturating_add(chunk.len()) > max_bytes {
                            let err = NgcErrorInfo::new(
                                NgcErrorCode::InvalidArgument,
                                "request body exceeds max_request_bytes",
                            );
                            return Err(Box::new(ngc_error_response(
                                Some(StatusCode::PAYLOAD_TOO_LARGE),
                                &[],
                                &err,
                                max_details_bytes,
                            )));
                        }
                        out.extend_from_slice(&chunk);
                    }
                }
            }
        }
    }
}

fn unary_ok_or_err<Resp: ProtoMessage>(
    res: Result<Resp, NgcErrorInfo>,
    max_details_bytes: usize,
) -> Response {
    match res {
        Ok(resp) => match to_json_vec(&resp) {
            Ok(bytes) => json_response(StatusCode::OK, &[], bytes),
            Err(err) => ngc_error_response(None, &[], &err, max_details_bytes),
        },
        Err(err) => ngc_error_response(None, &[], &err, max_details_bytes),
    }
}

fn ndjson_stream_response<Item: ProtoMessage + Send + 'static>(
    stream: CoreStream<Item>,
) -> Response {
    let byte_stream = stream.map(|res| -> Result<Bytes, std::convert::Infallible> {
        match res {
            Ok(item) => {
                let mut line = to_json_vec(&item).unwrap_or_else(|_| b"{}".to_vec());
                line.push(b'\n');
                Ok(Bytes::from(line))
            }
            Err(err) => {
                let proto = ngc_error_to_proto(&err);
                let err_val = to_json_value(&proto).unwrap_or_else(
                    |_| serde_json::json!({"code": "INTERNAL", "message": "serialization error"}),
                );
                let wrapper = serde_json::json!({ "error": err_val });
                let mut line = serde_json::to_vec(&wrapper)
                    .unwrap_or_else(|_| b"{\"error\":{\"code\":\"INTERNAL\"}}".to_vec());
                line.push(b'\n');
                Ok(Bytes::from(line))
            }
        }
    });

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/x-ndjson")
        .body(Body::from_stream(byte_stream))
        .unwrap_or_else(|_| Response::new(Body::empty()))
}

fn sse_stream_response<Item: ProtoMessage + Send + 'static>(stream: CoreStream<Item>) -> Response {
    let byte_stream = stream.map(|res| -> Result<Bytes, std::convert::Infallible> {
        match res {
            Ok(item) => {
                let json = to_json_string(&item).unwrap_or_else(|_| "{}".to_owned());
                Ok(Bytes::from(format!("data: {json}\n\n")))
            }
            Err(err) => {
                let proto = ngc_error_to_proto(&err);
                let json = to_json_string(&proto).unwrap_or_else(|_| {
                    "{\"code\":\"INTERNAL\",\"message\":\"serialization error\"}".to_owned()
                });
                Ok(Bytes::from(format!("event: error\ndata: {json}\n\n")))
            }
        }
    });

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .body(Body::from_stream(byte_stream))
        .unwrap_or_else(|_| Response::new(Body::empty()))
}

async fn dispatch<C: Connector>(State(core): State<Arc<ServerCore<C>>>, req: Request) -> Response {
    let max_details = core.config().max_error_details_bytes;
    let Some(rpc) = HttpRpc::from_path(req.uri().path()) else {
        let err = NgcErrorInfo::new(NgcErrorCode::NotFound, "unknown NGC HTTP endpoint");
        return ngc_error_response(Some(StatusCode::NOT_FOUND), &[], &err, max_details);
    };

    if req.method() != Method::POST {
        let err = NgcErrorInfo::new(NgcErrorCode::InvalidArgument, "only POST is supported");
        let allow = [(header::ALLOW, HeaderValue::from_static("POST"))];
        return ngc_error_response(
            Some(StatusCode::METHOD_NOT_ALLOWED),
            &allow,
            &err,
            max_details,
        );
    }

    if !is_application_json(req.headers()) {
        let err = NgcErrorInfo::new(
            NgcErrorCode::InvalidArgument,
            "Content-Type must be application/json",
        );
        return ngc_error_response(
            Some(StatusCode::UNSUPPORTED_MEDIA_TYPE),
            &[],
            &err,
            max_details,
        );
    }

    let (parts, body) = req.into_parts();
    let body_timeout = if rpc.is_health() {
        core.config().health_timeout
    } else {
        core.config().default_timeout
    };
    let raw_bytes = match read_bounded_body(
        &parts.headers,
        body,
        core.config().max_request_bytes,
        body_timeout,
        max_details,
    )
    .await
    {
        Ok(b) => b,
        Err(resp) => return *resp,
    };

    let metadata = headers_to_metadata(&parts.headers);
    let cancel = CancellationToken::new();

    match rpc {
        HttpRpc::Describe => match from_json_slice::<DescribeRequest>(&raw_bytes) {
            Ok(msg) => unary_ok_or_err(core.describe(msg, metadata, cancel).await, max_details),
            Err(err) => ngc_error_response(None, &[], &err, max_details),
        },
        HttpRpc::Query => match from_json_slice::<QueryRequest>(&raw_bytes) {
            Ok(msg) => match core.query(msg, metadata, cancel).await {
                Ok(stream) => ndjson_stream_response(stream),
                Err(err) => ngc_error_response(None, &[], &err, max_details),
            },
            Err(err) => ngc_error_response(None, &[], &err, max_details),
        },
        HttpRpc::Mutate => match from_json_slice::<MutateRequest>(&raw_bytes) {
            Ok(msg) => unary_ok_or_err(core.mutate(msg, metadata, cancel).await, max_details),
            Err(err) => ngc_error_response(None, &[], &err, max_details),
        },
        HttpRpc::Subscribe => match from_json_slice::<SubscribeRequest>(&raw_bytes) {
            Ok(msg) => match core.subscribe(msg, metadata, cancel).await {
                Ok(stream) => sse_stream_response(stream),
                Err(err) => ngc_error_response(None, &[], &err, max_details),
            },
            Err(err) => ngc_error_response(None, &[], &err, max_details),
        },
        HttpRpc::StartTask => match from_json_slice::<StartTaskRequest>(&raw_bytes) {
            Ok(msg) => unary_ok_or_err(core.start_task(msg, metadata, cancel).await, max_details),
            Err(err) => ngc_error_response(None, &[], &err, max_details),
        },
        HttpRpc::GetTask => match from_json_slice::<GetTaskRequest>(&raw_bytes) {
            Ok(msg) => unary_ok_or_err(core.get_task(msg, metadata, cancel).await, max_details),
            Err(err) => ngc_error_response(None, &[], &err, max_details),
        },
        HttpRpc::CancelTask => match from_json_slice::<CancelTaskRequest>(&raw_bytes) {
            Ok(msg) => unary_ok_or_err(core.cancel_task(msg, metadata, cancel).await, max_details),
            Err(err) => ngc_error_response(None, &[], &err, max_details),
        },
        HttpRpc::WatchTask => match from_json_slice::<WatchTaskRequest>(&raw_bytes) {
            Ok(msg) => match core.watch_task(msg, metadata, cancel).await {
                Ok(stream) => sse_stream_response(stream),
                Err(err) => ngc_error_response(None, &[], &err, max_details),
            },
            Err(err) => ngc_error_response(None, &[], &err, max_details),
        },
        HttpRpc::Sketch => match from_json_slice::<SketchRequest>(&raw_bytes) {
            Ok(msg) => unary_ok_or_err(core.sketch(msg, metadata, cancel).await, max_details),
            Err(err) => ngc_error_response(None, &[], &err, max_details),
        },
        HttpRpc::Health => match from_json_slice::<HealthRequest>(&raw_bytes) {
            Ok(msg) => unary_ok_or_err(core.health(msg, metadata, cancel).await, max_details),
            Err(err) => ngc_error_response(None, &[], &err, max_details),
        },
        HttpRpc::Explain => match from_json_slice::<QueryRequest>(&raw_bytes) {
            Ok(msg) => unary_ok_or_err(core.explain(msg, metadata, cancel).await, max_details),
            Err(err) => ngc_error_response(None, &[], &err, max_details),
        },
    }
}
