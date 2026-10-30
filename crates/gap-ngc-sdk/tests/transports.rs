//! Per-transport conformance tests for HTTP (`NGC-002`, `NGC-094`), stdio (`NGC-003`, `NGC-095`), and gRPC (`NGC-001`, `NGC-062`, `NGC-063`, `NGC-093`).
#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll};
use std::time::Duration;

use axum::body::Body;
use futures_util::{StreamExt, stream};
use gap_core::error::{NgcErrorCode, NgcErrorInfo};
use gap_ngc_sdk::core::{ServerConfig, ServerCore};
use gap_ngc_sdk::error::NGC_ERROR_METADATA_KEY;
use gap_ngc_sdk::grpc::{DEFAULT_CLIENT_MAX_DECODING_BYTES, GrpcClient, ServeError, serve_tcp};
use gap_ngc_sdk::http as ngc_http;
use gap_ngc_sdk::mock::{MockConnector, RpcMethod};
use gap_ngc_sdk::stdio::serve_stdio;
use gap_ngc_sdk::testing::{in_memory_grpc, in_memory_raw_stdio};
use gap_proto::ngc::v1::{
    ConnectorDescription, DescribeRequest, DescribeResponse, Event, HealthRequest, HealthResponse,
    QueryRequest, QueryResponse, RowSetChunk, SubscribeRequest, TaskState, TaskStatus,
    WatchTaskRequest,
    connector_server::{Connector as TonicConnector, ConnectorServer},
    query_response,
};
use http::{Method, Request, StatusCode};
use http_body_util::BodyExt as _;
use prost::Message as _;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWriteExt, BufReader, ReadBuf};
use tokio::sync::Notify;
use tower::ServiceExt as _;

// spec: NGC-002, NGC-093, NGC-094, NGC-080, INV-15
#[tokio::test(start_paused = true)]
async fn http_enforces_405_404_415_413_slow_loris_ndjson_sse_retry_after_and_metadata() {
    let seen_auth = Arc::new(std::sync::Mutex::new(None::<(String, String)>));
    let seen_auth_c = Arc::clone(&seen_auth);

    let mock = MockConnector::new()
        .on_describe(move |req, scope| {
            if req.known_spec_hash == "rate-limit" {
                return Err(
                    NgcErrorInfo::new(NgcErrorCode::ResourceExhausted, "quota reached")
                        .with_retry(true, 1_200),
                );
            }
            let auth = scope
                .metadata
                .get("authorization")
                .cloned()
                .unwrap_or_default();
            let dbg = format!("{scope:?}");
            *seen_auth_c.lock().expect("lock") = Some((auth, dbg));
            Ok(DescribeResponse {
                not_modified: true,
                description: None,
            })
        })
        .on_query(|req, _scope| {
            if req.operation == "mid-fail" {
                let items = vec![
                    Ok(QueryResponse {
                        chunk: Some(query_response::Chunk::Rows(RowSetChunk {
                            foreach_index: 0,
                            rows: Vec::new(),
                            last_for_index: false,
                            error: None,
                        })),
                    }),
                    Err(NgcErrorInfo::new(
                        NgcErrorCode::UpstreamError,
                        "db link lost",
                    )),
                ];
                return Ok(Box::pin(stream::iter(items)));
            }
            let items = vec![Ok(QueryResponse {
                chunk: Some(query_response::Chunk::Rows(RowSetChunk {
                    foreach_index: 0,
                    rows: Vec::new(),
                    last_for_index: true,
                    error: None,
                })),
            })];
            Ok(Box::pin(stream::iter(items)))
        })
        .on_subscribe(|req, _scope| {
            if req.operation == "sse-fail" {
                let items = vec![
                    Ok(Event {
                        id: "ev-1".to_owned(),
                        at: None,
                        payload: None,
                        resume_token: "t-1".to_owned(),
                    }),
                    Err(NgcErrorInfo::new(NgcErrorCode::Unavailable, "broker lost")),
                ];
                return Ok(Box::pin(stream::iter(items)));
            }
            Ok(Box::pin(stream::iter(vec![Ok(Event {
                id: "ev-ok".to_owned(),
                at: None,
                payload: None,
                resume_token: "t-ok".to_owned(),
            })])))
        })
        .on_watch_task(|_req, _scope| {
            Ok(Box::pin(stream::iter(vec![Ok(TaskStatus {
                task_id: "task-1".to_owned(),
                state: TaskState::Completed as i32,
                seq: 1,
                ..TaskStatus::default()
            })])))
        });

    let core = Arc::new(ServerCore::new(
        mock.clone(),
        ServerConfig::default().with_max_request_bytes(256),
    ));
    let app = ngc_http::router(core);

    // 1. Non-POST -> 405 Method Not Allowed with `Allow: POST`.
    for bad_method in [Method::GET, Method::PUT, Method::DELETE] {
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(bad_method)
                    .uri("/gap/ngc/v1/Describe")
                    .body(Body::empty())
                    .expect("req"),
            )
            .await
            .expect("oneshot");
        assert_eq!(resp.status(), StatusCode::METHOD_NOT_ALLOWED);
        assert_eq!(
            resp.headers()
                .get(http::header::ALLOW)
                .and_then(|v| v.to_str().ok()),
            Some("POST")
        );
    }

    // 2. Unknown path -> 404 Not Found.
    let resp_404 = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/gap/ngc/v1/DoesNotExist")
                .header(http::header::CONTENT_TYPE, "application/json")
                .body(Body::from("{}"))
                .expect("req"),
        )
        .await
        .expect("oneshot");
    assert_eq!(resp_404.status(), StatusCode::NOT_FOUND);

    // 3. Missing / wrong Content-Type -> 415 Unsupported Media Type.
    for ct in [None, Some("text/plain"), Some("application/x-ndjson")] {
        let mut builder = Request::builder()
            .method(Method::POST)
            .uri("/gap/ngc/v1/Describe");
        if let Some(c) = ct {
            builder = builder.header(http::header::CONTENT_TYPE, c);
        }
        let resp_415 = app
            .clone()
            .oneshot(builder.body(Body::from("{}")).expect("req"))
            .await
            .expect("oneshot");
        assert_eq!(resp_415.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
    }

    // 4. Oversized streaming body (`> 256` bytes) -> 413 WITHOUT polling trailing chunks!
    let polled_third_chunk = Arc::new(AtomicBool::new(false));
    let polled_c = Arc::clone(&polled_third_chunk);
    let body_stream = stream::unfold(0_u8, move |step| {
        let polled = Arc::clone(&polled_c);
        async move {
            match step {
                0 => Some((
                    Ok::<_, std::io::Error>(bytes::Bytes::from(vec![b' '; 180])),
                    1,
                )),
                1 => Some((Ok(bytes::Bytes::from(vec![b' '; 180])), 2)), // 360 > 256 -> stop!
                _ => {
                    polled.store(true, Ordering::SeqCst);
                    None
                }
            }
        }
    });
    let resp_413 = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/gap/ngc/v1/Describe")
                .header(http::header::CONTENT_TYPE, "application/json")
                .body(Body::from_stream(body_stream))
                .expect("req"),
        )
        .await
        .expect("oneshot");
    assert_eq!(resp_413.status(), StatusCode::PAYLOAD_TOO_LARGE);
    assert!(
        !polled_third_chunk.load(Ordering::SeqCst),
        "HTTP body reader buffered past max_request_bytes instead of stopping immediately"
    );

    // 5. Slow-loris body stall on `/gap/ngc/v1/Health` (`500 ms` health_timeout) -> 504 Gateway Timeout.
    let stalled_body =
        stream::once(async { Ok::<_, std::io::Error>(bytes::Bytes::from_static(b"{")) })
            .chain(stream::pending());
    let resp_504 = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/gap/ngc/v1/Health")
                .header(http::header::CONTENT_TYPE, "application/json")
                .body(Body::from_stream(stalled_body))
                .expect("req"),
        )
        .await
        .expect("oneshot");
    assert_eq!(resp_504.status(), StatusCode::GATEWAY_TIMEOUT);

    // 6. Retry-After header on 429 (`1_200 ms` -> `"2"` seconds) + credential forwarding without leaking in Debug.
    let resp_429 = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/gap/ngc/v1/Describe")
                .header(http::header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"knownSpecHash":"rate-limit"}"#))
                .expect("req"),
        )
        .await
        .expect("oneshot");
    assert_eq!(resp_429.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(
        resp_429
            .headers()
            .get(http::header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok()),
        Some("2")
    );

    let resp_auth = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/gap/ngc/v1/Describe")
                .header(http::header::CONTENT_TYPE, "application/json")
                .header(http::header::AUTHORIZATION, "Bearer top-secret-vault-cred")
                .body(Body::from("{}"))
                .expect("req"),
        )
        .await
        .expect("oneshot");
    assert_eq!(resp_auth.status(), StatusCode::OK);
    let (auth_val, scope_dbg) = seen_auth.lock().expect("lock").clone().expect("recorded");
    assert_eq!(auth_val, "Bearer top-secret-vault-cred");
    assert!(!scope_dbg.contains("top-secret-vault-cred"));

    // 7. NDJSON newline framing (`\n` required on EVERY item) and mid-stream `{"error": ...}` line.
    let resp_ndjson = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/gap/ngc/v1/Query")
                .header(http::header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"operation":"mid-fail"}"#))
                .expect("req"),
        )
        .await
        .expect("oneshot");
    assert_eq!(resp_ndjson.status(), StatusCode::OK);
    assert_eq!(
        resp_ndjson
            .headers()
            .get(http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok()),
        Some("application/x-ndjson")
    );
    let ndjson_bytes = resp_ndjson
        .into_body()
        .collect()
        .await
        .expect("collect")
        .to_bytes();
    assert!(
        ndjson_bytes.ends_with(b"\n"),
        "NDJSON payload must end with '\\n'"
    );
    let ndjson_str = std::str::from_utf8(&ndjson_bytes).expect("utf8");
    let lines: Vec<&str> = ndjson_str.split('\n').filter(|l| !l.is_empty()).collect();
    assert_eq!(
        lines.len(),
        2,
        "expected 2 newline-delimited NDJSON lines, got `{ndjson_str}`"
    );
    let second_line: serde_json::Value = serde_json::from_str(lines[1]).expect("line 2 json");
    assert_eq!(second_line["error"]["code"], "UPSTREAM_ERROR");

    // 8. SSE framing (`Subscribe` + `WatchTask`) and mid-stream `event: error`.
    let resp_sse = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/gap/ngc/v1/Subscribe")
                .header(http::header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"operation":"sse-fail"}"#))
                .expect("req"),
        )
        .await
        .expect("oneshot");
    assert_eq!(resp_sse.status(), StatusCode::OK);
    assert_eq!(
        resp_sse
            .headers()
            .get(http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok()),
        Some("text/event-stream")
    );
    assert_eq!(
        resp_sse
            .headers()
            .get(http::header::CACHE_CONTROL)
            .and_then(|v| v.to_str().ok()),
        Some("no-cache")
    );
    let sse_text = String::from_utf8(
        resp_sse
            .into_body()
            .collect()
            .await
            .expect("collect")
            .to_bytes()
            .to_vec(),
    )
    .expect("utf8");
    assert!(sse_text.contains("data: {\"id\":\"ev-1\""));
    assert!(sse_text.contains("event: error\ndata: {\"code\":\"UNAVAILABLE\""));

    let resp_watch = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/gap/ngc/v1/WatchTask")
                .header(http::header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"taskId":"task-1"}"#))
                .expect("req"),
        )
        .await
        .expect("oneshot");
    assert_eq!(resp_watch.status(), StatusCode::OK);
    assert_eq!(
        resp_watch
            .headers()
            .get(http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok()),
        Some("text/event-stream")
    );
    assert_eq!(
        resp_watch
            .headers()
            .get(http::header::CACHE_CONTROL)
            .and_then(|v| v.to_str().ok()),
        Some("no-cache")
    );
}

// spec: NGC-003, NGC-093, NGC-095
#[tokio::test(start_paused = true)]
async fn stdio_jsonrpc_error_codes_batch_rejection_interleaving_cancel_and_eof() {
    let cancel_entered = Arc::new(Notify::new());
    let cancel_entered_c = Arc::clone(&cancel_entered);

    let mock = MockConnector::new()
        .on_describe_async(|req, _scope| async move {
            if req.known_spec_hash == "slow" {
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
            Ok(DescribeResponse {
                not_modified: req.known_spec_hash == "fast",
                description: None,
            })
        })
        .on_query(move |req, _scope| {
            if req.operation == "hang" {
                cancel_entered_c.notify_one();
                return Ok(Box::pin(stream::pending()));
            }
            Ok(Box::pin(stream::iter(vec![Ok(QueryResponse {
                chunk: Some(query_response::Chunk::Rows(RowSetChunk {
                    foreach_index: 0,
                    rows: Vec::new(),
                    last_for_index: true,
                    error: None,
                })),
            })])))
        });

    let core = Arc::new(ServerCore::new(mock.clone(), ServerConfig::default()));
    let (mut stdin_tx, stdout_rx, server_join) = in_memory_raw_stdio(core);
    let mut lines = BufReader::new(stdout_rx).lines();

    let mut next_json = async || -> serde_json::Value {
        let line = lines
            .next_line()
            .await
            .expect("io")
            .expect("expected stdout JSON-RPC line");
        let val: serde_json::Value =
            serde_json::from_str(&line).expect("stdout must carry protocol JSON lines only");
        assert_eq!(val["jsonrpc"], "2.0");
        val
    };

    // 1. `-32700` Parse error (`id: null`).
    stdin_tx
        .write_all(b"{malformed-json\n")
        .await
        .expect("write");
    let e_parse = next_json().await;
    assert!(e_parse["id"].is_null());
    assert_eq!(e_parse["error"]["code"], -32700);

    // 2. `-32600` Invalid Request (batch array `[]` rejected + missing `jsonrpc`).
    stdin_tx
        .write_all(
            b"[{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"gap.ngc.Health\",\"params\":{}}]\n",
        )
        .await
        .expect("write");
    let e_batch = next_json().await;
    assert_eq!(e_batch["error"]["code"], -32600);

    stdin_tx
        .write_all(b"{\"id\":1,\"method\":\"gap.ngc.Health\",\"params\":{}}\n")
        .await
        .expect("write");
    let e_ver = next_json().await;
    assert_eq!(e_ver["error"]["code"], -32600);

    // 3. `-32601` Method not found.
    stdin_tx
        .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":9,\"method\":\"gap.ngc.Unknown\",\"params\":{}}\n")
        .await
        .expect("write");
    let e_method = next_json().await;
    assert_eq!(e_method["id"], 9);
    assert_eq!(e_method["error"]["code"], -32601);

    // 4. `-32602` Invalid params (wrong type / unknown enum).
    stdin_tx
        .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":\"p1\",\"method\":\"gap.ngc.Describe\",\"params\":{\"knownSpecHash\":12345}}\n")
        .await
        .expect("write");
    let e_params = next_json().await;
    assert_eq!(e_params["id"], "p1");
    assert_eq!(e_params["error"]["code"], -32602);

    // 5. Concurrent interleaved requests: `id: 100` (slow 200ms) sent before `id: 101` (fast 0ms).
    stdin_tx
        .write_all(
            b"{\"jsonrpc\":\"2.0\",\"id\":100,\"method\":\"gap.ngc.Describe\",\"params\":{\"knownSpecHash\":\"slow\"}}\n\
              {\"jsonrpc\":\"2.0\",\"id\":101,\"method\":\"gap.ngc.Describe\",\"params\":{\"knownSpecHash\":\"fast\"}}\n",
        )
        .await
        .expect("write");
    let first_done = next_json().await;
    let second_done = next_json().await;
    assert_eq!(
        first_done["id"], 101,
        "fast concurrent request must complete before slow request"
    );
    assert_eq!(first_done["result"]["notModified"], true);
    assert_eq!(second_done["id"], 100);

    // 6. `gap.ngc.cancel` notification cancels in-flight streaming call `"hang-1"`.
    stdin_tx
        .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":\"hang-1\",\"method\":\"gap.ngc.Query\",\"params\":{\"operation\":\"hang\"}}\n")
        .await
        .expect("write");
    cancel_entered.notified().await;
    assert_eq!(mock.in_flight(), 1);

    stdin_tx
        .write_all(
            b"{\"jsonrpc\":\"2.0\",\"method\":\"gap.ngc.cancel\",\"params\":{\"id\":\"hang-1\"}}\n",
        )
        .await
        .expect("write");
    let cancelled_resp = next_json().await;
    assert_eq!(cancelled_resp["id"], "hang-1");
    assert_eq!(cancelled_resp["error"]["code"], -32001);
    assert_eq!(cancelled_resp["error"]["data"]["code"], "CANCELLED");
    tokio::time::timeout(Duration::from_secs(5), mock.wait_for_drops(1))
        .await
        .expect("stdio cancel must drop connector stream");
    assert_eq!(mock.in_flight(), 0);

    // 7. EOF cancels everything in flight and `serve_stdio` returns cleanly!
    stdin_tx
        .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":\"hang-eof\",\"method\":\"gap.ngc.Query\",\"params\":{\"operation\":\"hang\"}}\n")
        .await
        .expect("write");
    cancel_entered.notified().await;
    assert_eq!(mock.in_flight(), 1);

    drop(stdin_tx); // trigger EOF on stdin
    tokio::time::timeout(Duration::from_secs(5), mock.wait_for_drops(2))
        .await
        .expect("stdio EOF must drop in-flight connector stream");
    server_join
        .await
        .expect("join")
        .expect("serve_stdio clean exit");
    assert_eq!(mock.in_flight(), 0);
}

struct OversizedWithoutBufferingReader {
    emitted_bytes: usize,
    limit_plus_margin: usize,
    read_past_rejection: Arc<AtomicBool>,
}

impl AsyncRead for OversizedWithoutBufferingReader {
    fn poll_read(
        mut self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        if self.emitted_bytes >= self.limit_plus_margin {
            self.read_past_rejection.store(true, Ordering::SeqCst);
            return Poll::Ready(Ok(()));
        }
        let step = 128.min(buf.remaining());
        let chunk = vec![b'a'; step];
        buf.put_slice(&chunk);
        self.emitted_bytes += step;
        Poll::Ready(Ok(()))
    }
}

// spec: NGC-093, NGC-095
#[tokio::test(start_paused = true)]
async fn stdio_oversized_line_emits_fatal_32600_and_ends_session_without_buffering_excess() {
    let read_past_rejection = Arc::new(AtomicBool::new(false));
    let reader = BufReader::with_capacity(
        128,
        OversizedWithoutBufferingReader {
            emitted_bytes: 0,
            limit_plus_margin: 512,
            read_past_rejection: Arc::clone(&read_past_rejection),
        },
    );
    let (writer, client_read) = tokio::io::duplex(4_096);
    let core = Arc::new(ServerCore::new(
        MockConnector::new(),
        ServerConfig::default().with_max_request_bytes(256),
    ));

    serve_stdio(core, reader, writer)
        .await
        .expect("oversized line terminates session cleanly");

    assert!(
        !read_past_rejection.load(Ordering::SeqCst),
        "stdio line reader kept reading excess bytes after exceeding max_request_bytes"
    );

    let mut out_lines = BufReader::new(client_read).lines();
    let fatal_line = out_lines
        .next_line()
        .await
        .expect("io")
        .expect("fatal -32600 line");
    let val: serde_json::Value = serde_json::from_str(&fatal_line).expect("json");
    assert_eq!(val["error"]["code"], -32600);
    assert!(out_lines.next_line().await.expect("io").is_none());
}

// spec: NGC-093, NGC-095
#[tokio::test(start_paused = true)]
async fn stdio_slow_loris_partial_line_times_out_while_idle_session_survives() {
    let core = Arc::new(ServerCore::new(
        MockConnector::new(),
        ServerConfig::default().with_default_timeout(Duration::from_secs(2)),
    ));
    let (mut stdin_tx, stdout_rx, server_join) = in_memory_raw_stdio(core);
    let mut out_lines = BufReader::new(stdout_rx).lines();

    // Idle session for 10s (> 2s default_timeout) BEFORE any byte of a line arrives MUST survive!
    tokio::time::sleep(Duration::from_secs(10)).await;
    stdin_tx
        .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"gap.ngc.Health\",\"params\":{}}\n")
        .await
        .expect("write health");
    let ok_line = out_lines
        .next_line()
        .await
        .expect("io")
        .expect("health response");
    let ok_val: serde_json::Value = serde_json::from_str(&ok_line).expect("json");
    assert_eq!(ok_val["id"], 1);
    assert_eq!(ok_val["result"]["status"], "SERVING");

    // Now start a slow-loris partial line WITHOUT `\n` and stall past 2s -> fatal -32600 & clean exit!
    stdin_tx
        .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":2")
        .await
        .expect("write partial line");
    stdin_tx.flush().await.expect("flush partial");

    let fatal_line = tokio::time::timeout(Duration::from_secs(5), out_lines.next_line())
        .await
        .expect("partial-line timer must fire within default_timeout (2s)")
        .expect("io")
        .expect("fatal slow-loris line");
    let fatal_val: serde_json::Value = serde_json::from_str(&fatal_line).expect("json");
    assert_eq!(fatal_val["error"]["code"], -32600);
    assert_eq!(fatal_val["error"]["data"]["code"], "DEADLINE_EXCEEDED");
    tokio::time::timeout(Duration::from_secs(5), server_join)
        .await
        .expect("stdio server must exit promptly on partial-line timeout")
        .expect("join")
        .expect("clean slow-loris termination");
}

struct BareStatusService;

#[tonic::async_trait]
impl TonicConnector for BareStatusService {
    async fn describe(
        &self,
        _request: tonic::Request<DescribeRequest>,
    ) -> Result<tonic::Response<DescribeResponse>, tonic::Status> {
        // Deliberately omit `ngc-error-bin` metadata to test GrpcClient code-table fallback.
        Err(tonic::Status::permission_denied(
            "bare status without metadata",
        ))
    }

    type QueryStream =
        Pin<Box<dyn futures_util::Stream<Item = Result<QueryResponse, tonic::Status>> + Send>>;

    async fn query(
        &self,
        _request: tonic::Request<gap_proto::ngc::v1::QueryRequest>,
    ) -> Result<tonic::Response<Self::QueryStream>, tonic::Status> {
        Err(tonic::Status::unimplemented("query bare"))
    }

    async fn mutate(
        &self,
        _request: tonic::Request<gap_proto::ngc::v1::MutateRequest>,
    ) -> Result<tonic::Response<gap_proto::ngc::v1::MutateResponse>, tonic::Status> {
        Err(tonic::Status::unimplemented("mutate bare"))
    }

    type SubscribeStream =
        Pin<Box<dyn futures_util::Stream<Item = Result<Event, tonic::Status>> + Send>>;

    async fn subscribe(
        &self,
        _request: tonic::Request<SubscribeRequest>,
    ) -> Result<tonic::Response<Self::SubscribeStream>, tonic::Status> {
        Err(tonic::Status::unimplemented("subscribe bare"))
    }

    async fn start_task(
        &self,
        _request: tonic::Request<gap_proto::ngc::v1::StartTaskRequest>,
    ) -> Result<tonic::Response<TaskStatus>, tonic::Status> {
        Err(tonic::Status::unimplemented("start_task bare"))
    }

    async fn get_task(
        &self,
        _request: tonic::Request<gap_proto::ngc::v1::GetTaskRequest>,
    ) -> Result<tonic::Response<TaskStatus>, tonic::Status> {
        Err(tonic::Status::unimplemented("get_task bare"))
    }

    async fn cancel_task(
        &self,
        _request: tonic::Request<gap_proto::ngc::v1::CancelTaskRequest>,
    ) -> Result<tonic::Response<TaskStatus>, tonic::Status> {
        Err(tonic::Status::unimplemented("cancel_task bare"))
    }

    type WatchTaskStream =
        Pin<Box<dyn futures_util::Stream<Item = Result<TaskStatus, tonic::Status>> + Send>>;

    async fn watch_task(
        &self,
        _request: tonic::Request<WatchTaskRequest>,
    ) -> Result<tonic::Response<Self::WatchTaskStream>, tonic::Status> {
        Err(tonic::Status::unimplemented("watch_task bare"))
    }

    async fn sketch(
        &self,
        _request: tonic::Request<gap_proto::ngc::v1::SketchRequest>,
    ) -> Result<tonic::Response<gap_proto::ngc::v1::SketchResponse>, tonic::Status> {
        Err(tonic::Status::unimplemented("sketch bare"))
    }

    async fn health(
        &self,
        _request: tonic::Request<HealthRequest>,
    ) -> Result<tonic::Response<HealthResponse>, tonic::Status> {
        Err(tonic::Status::unavailable("bare unavailable"))
    }

    async fn explain(
        &self,
        _request: tonic::Request<gap_proto::ngc::v1::QueryRequest>,
    ) -> Result<tonic::Response<gap_proto::ngc::v1::ExplainResponse>, tonic::Status> {
        Err(tonic::Status::unimplemented("explain bare"))
    }
}

// spec: NGC-001, NGC-062, NGC-063, NGC-070, NGC-080, NGC-093, INV-15
#[tokio::test(start_paused = true)]
async fn grpc_metadata_forwarding_health_500ms_bound_bare_status_fallback_and_non_loopback_refusal()
{
    // 1. Plaintext non-loopback `serve_tcp` refused before touching any socket (`NGC-093` (g)).
    let strict_core = Arc::new(ServerCore::new(
        MockConnector::new(),
        ServerConfig::default(),
    ));
    for non_loopback in ["0.0.0.0:50051", "[::]:50051", "192.0.2.10:50051"] {
        let addr: std::net::SocketAddr = non_loopback.parse().expect("valid SocketAddr");
        let err = tokio::time::timeout(
            Duration::from_millis(250),
            serve_tcp(Arc::clone(&strict_core), addr),
        )
        .await
        .expect("serve_tcp non-loopback check must return immediately without binding/hanging")
        .expect_err("non-loopback plaintext bind must fail closed");
        assert!(
            matches!(err, ServeError::InsecureNonLoopbackAddress(a) if a == addr),
            "unexpected error for {addr}: {err:?}"
        );
    }

    // 2. Metadata forwarding (`authorization` redacted in `CallScope` Debug) + `Health` 500ms timeout (`NGC-070`, `NGC-093` (h)).
    let recorded_meta = Arc::new(std::sync::Mutex::new(None::<(String, String)>));
    let rec_c = Arc::clone(&recorded_meta);
    let mock = MockConnector::new()
        .on_describe(move |_req, scope| {
            let auth = scope
                .metadata
                .get("authorization")
                .cloned()
                .unwrap_or_default();
            let dbg = format!("{scope:?}");
            *rec_c.lock().expect("lock") = Some((auth, dbg));
            Ok(DescribeResponse {
                not_modified: true,
                description: None,
            })
        })
        .on_health_async(|_req, _scope| async move {
            // Default timeout is 60s, but Health MUST be bounded by 500ms (`NGC-093` (h)).
            tokio::time::sleep(Duration::from_secs(2)).await;
            Ok(HealthResponse::default())
        });

    let core = Arc::new(ServerCore::new(mock, ServerConfig::default()));
    let (client, _handle) = in_memory_grpc(core);

    let mut meta = BTreeMap::new();
    meta.insert(
        "authorization".to_owned(),
        "Bearer grpc-vault-secret".to_owned(),
    );
    let resp = client
        .clone()
        .describe_with_metadata(DescribeRequest::default(), &meta)
        .await
        .expect("describe with metadata");
    assert!(resp.not_modified);
    let (auth_seen, dbg_seen) = recorded_meta
        .lock()
        .expect("lock")
        .clone()
        .expect("recorded");
    assert_eq!(auth_seen, "Bearer grpc-vault-secret");
    assert!(!dbg_seen.contains("grpc-vault-secret"));

    let t0 = tokio::time::Instant::now();
    let health_err = client
        .clone()
        .health(HealthRequest {})
        .await
        .expect_err("slow health must fail at 500ms");
    assert_eq!(health_err.code, NgcErrorCode::DeadlineExceeded);
    assert!(tokio::time::Instant::now().duration_since(t0) <= Duration::from_secs(1));

    // 3. Bare `tonic::Status` (without `ngc-error-bin`) over duplex falls back to code table in `GrpcClient`.
    let (conn_tx, conn_rx) = tokio::sync::mpsc::channel::<tokio::io::DuplexStream>(4);
    let server_task = tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(ConnectorServer::new(BareStatusService))
            .serve_with_incoming(
                tokio_stream::wrappers::ReceiverStream::new(conn_rx).map(Ok::<_, std::io::Error>),
            )
            .await
    });
    let channel = tonic::transport::Endpoint::from_static("http://[::]:50051")
        .connect_with_connector_lazy(tower::service_fn(move |_: http::Uri| {
            let tx = conn_tx.clone();
            async move {
                let (c, s) = tokio::io::duplex(64 * 1024);
                tx.send(s)
                    .await
                    .map_err(|_| std::io::Error::new(std::io::ErrorKind::BrokenPipe, "closed"))?;
                Ok::<_, std::io::Error>(hyper_util::rt::TokioIo::new(c))
            }
        }));
    let bare_client = GrpcClient::from_channel(channel);
    let bare_err = bare_client
        .describe(DescribeRequest::default())
        .await
        .expect_err("bare status error");
    let _ = NGC_ERROR_METADATA_KEY;
    assert_eq!(bare_err.code, NgcErrorCode::PermissionDenied);
    assert_eq!(bare_err.message(), "bare status without metadata");
    server_task.abort();
}

// spec: NGC-003, NGC-025, NGC-093, NGC-095, INV-15
#[tokio::test(start_paused = true)]
async fn stdio_rejects_duplicate_in_flight_id_preserves_running_call_allows_reuse_and_separates_num_vs_str()
 {
    let entered = Arc::new(Notify::new());
    let entered_c = Arc::clone(&entered);

    let mock = MockConnector::new()
        .on_query(move |req, _scope| {
            if req.operation.starts_with("hang") {
                entered_c.notify_one();
                return Ok(Box::pin(stream::pending()));
            }
            Ok(Box::pin(stream::iter(vec![Ok(QueryResponse {
                chunk: Some(query_response::Chunk::Rows(RowSetChunk {
                    foreach_index: 0,
                    rows: Vec::new(),
                    last_for_index: true,
                    error: None,
                })),
            })])))
        })
        .on_describe(|req, _scope| {
            Ok(DescribeResponse {
                not_modified: req.known_spec_hash == "reused-ok",
                description: None,
            })
        });

    let core = Arc::new(ServerCore::new(mock.clone(), ServerConfig::default()));
    let (mut stdin_tx, stdout_rx, server_join) = in_memory_raw_stdio(core);
    let mut lines = BufReader::new(stdout_rx).lines();

    let mut next_json = async || -> serde_json::Value {
        let line = tokio::time::timeout(Duration::from_secs(5), lines.next_line())
            .await
            .expect("stdout timed out waiting for JSON-RPC line")
            .expect("io")
            .expect("expected JSON-RPC line before EOF");
        let val: serde_json::Value = serde_json::from_str(&line).expect("valid JSON-RPC line");
        assert_eq!(val["jsonrpc"], "2.0");
        val
    };

    // 1. Start call `id: 91` on a stream that never ends.
    stdin_tx
        .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":91,\"method\":\"gap.ngc.Query\",\"params\":{\"operation\":\"hang-first\"}}\n")
        .await
        .expect("write first 91");
    entered.notified().await;
    assert_eq!(mock.in_flight(), 1);
    assert_eq!(mock.calls_for(RpcMethod::Query), 1);

    // 2. Duplicate `id: 91` while first is running -> rejected with `-32600` (message without payload),
    //    connector NOT invoked a second time, and running call `91` stays untouched!
    let canary_payload = "canary-dup-payload-secret-999";
    let dup_req = format!(
        "{{\"jsonrpc\":\"2.0\",\"id\":91,\"method\":\"gap.ngc.Query\",\"params\":{{\"operation\":\"hang-{canary_payload}\"}}}}\n"
    );
    stdin_tx
        .write_all(dup_req.as_bytes())
        .await
        .expect("write duplicate 91");

    let dup_resp = next_json().await;
    assert_eq!(dup_resp["id"], 91);
    assert_eq!(dup_resp["error"]["code"], -32600);
    assert_eq!(dup_resp["error"]["data"]["code"], "INVALID_ARGUMENT");
    let err_msg = dup_resp["error"]["message"].as_str().expect("err msg str");
    assert!(
        !err_msg.contains(canary_payload) && !err_msg.contains("91"),
        "duplicate-id error message must not echo request payload/id, got: `{err_msg}`"
    );
    assert_eq!(
        mock.calls_for(RpcMethod::Query),
        1,
        "duplicate id must not invoke connector"
    );
    assert_eq!(
        mock.in_flight(),
        1,
        "running call must stay untouched in flight"
    );

    // 3. Cancel `id: 91` -> first call answers with exact 1 `CANCELLED` response and drops its stream.
    stdin_tx
        .write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"gap.ngc.cancel\",\"params\":{\"id\":91}}\n")
        .await
        .expect("write cancel 91");
    let cancel_resp = next_json().await;
    assert_eq!(cancel_resp["id"], 91);
    assert_eq!(cancel_resp["error"]["code"], -32001);
    assert_eq!(cancel_resp["error"]["data"]["code"], "CANCELLED");
    tokio::time::timeout(Duration::from_secs(5), mock.wait_for_drops(1))
        .await
        .expect("original call 91 stream dropped");
    assert_eq!(mock.in_flight(), 0);

    // 4. Reuse `id: 91` after first call finished -> succeeds cleanly (and proves no late 2nd response for 91 arrived)!
    stdin_tx
        .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":91,\"method\":\"gap.ngc.Describe\",\"params\":{\"knownSpecHash\":\"reused-ok\"}}\n")
        .await
        .expect("write reused 91");
    let reused_resp = next_json().await;
    assert_eq!(reused_resp["id"], 91);
    assert_eq!(reused_resp["result"]["notModified"], true);

    // 5. Numeric `id: 1` and string `id: "1"` are distinct ids and can run & cancel independently.
    stdin_tx
        .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"gap.ngc.Query\",\"params\":{\"operation\":\"hang-num\"}}\n")
        .await
        .expect("write num 1");
    entered.notified().await;

    stdin_tx
        .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":\"1\",\"method\":\"gap.ngc.Query\",\"params\":{\"operation\":\"hang-str\"}}\n")
        .await
        .expect("write str 1");
    entered.notified().await;
    assert_eq!(mock.in_flight(), 2);

    // Cancel string `"1"` only -> numeric `1` remains in flight!
    stdin_tx
        .write_all(
            b"{\"jsonrpc\":\"2.0\",\"method\":\"gap.ngc.cancel\",\"params\":{\"id\":\"1\"}}\n",
        )
        .await
        .expect("cancel str 1");
    let c_str = next_json().await;
    assert_eq!(c_str["id"], "1");
    assert_eq!(c_str["error"]["data"]["code"], "CANCELLED");
    tokio::time::timeout(Duration::from_secs(5), mock.wait_for_drops(2))
        .await
        .expect("str 1 stream dropped");
    assert_eq!(
        mock.in_flight(),
        1,
        "numeric id 1 must still be in flight after cancelling string id \"1\""
    );

    // Cancel numeric `1` -> finishes cleanly.
    stdin_tx
        .write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"gap.ngc.cancel\",\"params\":{\"id\":1}}\n")
        .await
        .expect("cancel num 1");
    let c_num = next_json().await;
    assert_eq!(c_num["id"], 1);
    assert_eq!(c_num["error"]["data"]["code"], "CANCELLED");
    tokio::time::timeout(Duration::from_secs(5), mock.wait_for_drops(3))
        .await
        .expect("num 1 stream dropped");
    assert_eq!(mock.in_flight(), 0);

    drop(stdin_tx);
    server_join
        .await
        .expect("join")
        .expect("clean stdio shutdown");
}

// spec: NGC-001, NGC-062, NGC-063, NGC-093
#[tokio::test(start_paused = true)]
async fn grpc_client_bounds_decoded_message_size_and_maps_overflow_to_resource_exhausted() {
    assert_eq!(DEFAULT_CLIENT_MAX_DECODING_BYTES, 16 * 1024 * 1024);

    let make_stream_item = |payload_len: usize| -> QueryResponse {
        let mut fields = BTreeMap::new();
        fields.insert(
            "blob".to_owned(),
            prost_types::Value {
                kind: Some(prost_types::value::Kind::StringValue(
                    "x".repeat(payload_len),
                )),
            },
        );
        QueryResponse {
            chunk: Some(query_response::Chunk::Rows(RowSetChunk {
                foreach_index: 0,
                rows: vec![prost_types::Struct { fields }],
                last_for_index: true,
                error: None,
            })),
        }
    };

    let under_item = make_stream_item(980);
    let under_len = under_item.encoded_len();
    assert!(
        under_len > 980 && under_len <= 1_024,
        "expected under-bound encoded_len in (980, 1024], got {under_len}"
    );

    let over_item = make_stream_item(1_100);
    let over_len = over_item.encoded_len();
    assert!(
        over_len > 1_024,
        "expected over-bound encoded_len > 1024, got {over_len}"
    );

    let under_item_c = under_item.clone();
    let over_item_c = over_item.clone();
    let mock = MockConnector::new()
        .on_query(move |req, _scope| {
            let item = if req.operation == "over" {
                over_item_c.clone()
            } else {
                under_item_c.clone()
            };
            Ok(Box::pin(stream::iter(vec![Ok(item)])))
        })
        .on_describe(|req, _scope| {
            let sdl_len = if req.known_spec_hash == "over" {
                1_100
            } else {
                960
            };
            Ok(DescribeResponse {
                not_modified: false,
                description: Some(ConnectorDescription {
                    schema_sdl: "s".repeat(sdl_len),
                    ..ConnectorDescription::default()
                }),
            })
        });

    let core = Arc::new(ServerCore::new(mock, ServerConfig::default()));
    let (default_client, _handle) = in_memory_grpc(core);
    assert_eq!(
        default_client.max_decoding_message_size(),
        DEFAULT_CLIENT_MAX_DECODING_BYTES
    );

    let bounded_client = default_client.clone().with_max_decoding_message_size(1_024);
    assert_eq!(bounded_client.max_decoding_message_size(), 1_024);

    // 1. Streaming message larger than configured client bound -> RESOURCE_EXHAUSTED (not InvalidArgument/Internal).
    let mut over_stream = bounded_client
        .query(QueryRequest {
            operation: "over".to_owned(),
            ..QueryRequest::default()
        })
        .await
        .expect("stream headers open");
    let stream_err = over_stream
        .next()
        .await
        .expect("stream item")
        .expect_err("oversized decoded stream item must fail");
    assert_eq!(stream_err.code, NgcErrorCode::ResourceExhausted);

    // 2. Streaming message just under the configured client bound -> passes!
    let mut under_stream = bounded_client
        .query(QueryRequest {
            operation: "under".to_owned(),
            ..QueryRequest::default()
        })
        .await
        .expect("stream headers open");
    let got_item = under_stream
        .next()
        .await
        .expect("stream item")
        .expect("under-bound stream item must pass");
    assert_eq!(got_item, under_item);
    assert!(under_stream.next().await.is_none());

    // 3. Unary response larger than configured client bound -> RESOURCE_EXHAUSTED; just under -> passes!
    let unary_err = bounded_client
        .describe(DescribeRequest {
            known_spec_hash: "over".to_owned(),
        })
        .await
        .expect_err("oversized unary response must fail");
    assert_eq!(unary_err.code, NgcErrorCode::ResourceExhausted);

    let unary_ok = bounded_client
        .describe(DescribeRequest {
            known_spec_hash: "under".to_owned(),
        })
        .await
        .expect("under-bound unary response must pass");
    assert_eq!(unary_ok.description.expect("desc").schema_sdl.len(), 960);

    // 4. Default client (16 MiB bound) decodes the 1,100-byte message cleanly.
    let mut default_ok_stream = default_client
        .query(QueryRequest {
            operation: "over".to_owned(),
            ..QueryRequest::default()
        })
        .await
        .expect("default client stream open");
    let default_got = default_ok_stream
        .next()
        .await
        .expect("item")
        .expect("1.1 KiB well within 16 MiB default");
    assert_eq!(default_got, over_item);
}

// spec: NGC-003, NGC-025, NGC-095
#[tokio::test(start_paused = true)]
async fn stdio_releases_in_flight_id_before_final_send_completes_for_unary_and_stream() {
    let health_target = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let health_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let health_reached = Arc::new(Notify::new());
    let unary_done = Arc::new(Notify::new());
    let stream_done = Arc::new(Notify::new());

    let ht_c = Arc::clone(&health_target);
    let hc_c = Arc::clone(&health_count);
    let hr_c = Arc::clone(&health_reached);
    let ud_c = Arc::clone(&unary_done);
    let sd_c = Arc::clone(&stream_done);

    let mock = MockConnector::new()
        .on_health(move |_req, _scope| {
            let cur = hc_c.fetch_add(1, Ordering::SeqCst) + 1;
            if cur == ht_c.load(Ordering::SeqCst) {
                hr_c.notify_one();
            }
            Ok(HealthResponse {
                status: 1,
                ..HealthResponse::default()
            })
        })
        .on_describe(move |req, _scope| {
            if req.known_spec_hash == "u1" {
                ud_c.notify_one();
            }
            Ok(DescribeResponse {
                not_modified: true,
                description: None,
            })
        })
        .on_query(move |req, _scope| {
            if req.operation == "s-ok-1" || req.operation == "s-err-1" {
                sd_c.notify_one();
            }
            if req.operation == "s-err-1" {
                Ok(Box::pin(stream::iter(vec![Err(NgcErrorInfo::new(
                    NgcErrorCode::Unavailable,
                    "upstream closed",
                ))])))
            } else {
                Ok(Box::pin(stream::empty()))
            }
        });

    let core = Arc::new(ServerCore::new(
        mock.clone(),
        ServerConfig::default().with_stream_buffer(64),
    ));

    // Tiny 16-byte stdout duplex so response #0 blocks inside writer.write_all and responses #1..=#64
    // fill the 64-slot `out_tx` channel completely, causing the 66th `out_tx.send` to block!
    let (mut stdin_tx, stdin_rx) = tokio::io::duplex(64 * 1024);
    let (writer, stdout_rx) = tokio::io::duplex(16);
    let server_join =
        tokio::spawn(async move { serve_stdio(core, BufReader::new(stdin_rx), writer).await });
    let mut out_lines = BufReader::new(stdout_rx).lines();

    let mut batch_round = 0_usize;
    let mut saturate_out_channel = async |stdin: &mut tokio::io::DuplexStream| {
        let goal = health_count.load(Ordering::SeqCst) + 65;
        health_target.store(goal, Ordering::SeqCst);
        for i in 0..65 {
            let line = format!(
                "{{\"jsonrpc\":\"2.0\",\"id\":\"fill-{batch_round}-{i}\",\"method\":\"gap.ngc.Health\",\"params\":{{}}}}\n"
            );
            stdin.write_all(line.as_bytes()).await.expect("write fill");
        }
        stdin.flush().await.expect("flush fill");
        batch_round += 1;
        tokio::time::timeout(Duration::from_secs(5), health_reached.notified())
            .await
            .expect("all 65 filler Health calls completed");
        for _ in 0..16 {
            tokio::task::yield_now().await;
        }
    };

    // 1. Deterministic Unary check (`id: 777`): `u1` blocks on `out_tx.send`; `u2` reuses `777` while `u1` is blocked!
    saturate_out_channel(&mut stdin_tx).await;
    stdin_tx
        .write_all(
            b"{\"jsonrpc\":\"2.0\",\"id\":777,\"method\":\"gap.ngc.Describe\",\"params\":{\"knownSpecHash\":\"u1\"}}\n",
        )
        .await
        .expect("write u1");
    stdin_tx.flush().await.expect("flush u1");
    tokio::time::timeout(Duration::from_secs(5), unary_done.notified())
        .await
        .expect("u1 executed");
    for _ in 0..16 {
        tokio::task::yield_now().await;
    }

    // At this exact instant, `u1`'s final `out_tx.send` is blocked because `stdout_rx` has not been read.
    // Reusing `id: 777` MUST already be accepted (absent from in-flight map)!
    stdin_tx
        .write_all(
            b"{\"jsonrpc\":\"2.0\",\"id\":777,\"method\":\"gap.ngc.Describe\",\"params\":{\"knownSpecHash\":\"u2\"}}\n",
        )
        .await
        .expect("write u2");
    stdin_tx.flush().await.expect("flush u2");

    let mut u_resps = Vec::new();
    for _ in 0..67 {
        let line = tokio::time::timeout(Duration::from_secs(5), out_lines.next_line())
            .await
            .expect("no hang")
            .expect("io")
            .expect("line");
        let val: serde_json::Value = serde_json::from_str(&line).expect("json");
        if val["id"] == 777 {
            u_resps.push(val);
        }
    }
    assert_eq!(u_resps.len(), 2);
    for r in &u_resps {
        assert!(
            r.get("error").is_none(),
            "unary id 777 must already be released before final send unblocks, got: {r}"
        );
        assert_eq!(r["result"]["notModified"], true);
    }
    assert_eq!(mock.calls_for(RpcMethod::Describe), 2);

    // 2. Deterministic Stream normal-end check (`id: 888`): `s-ok-1` blocks on `out_tx.send(end_line)`.
    saturate_out_channel(&mut stdin_tx).await;
    stdin_tx
        .write_all(
            b"{\"jsonrpc\":\"2.0\",\"id\":888,\"method\":\"gap.ngc.Query\",\"params\":{\"operation\":\"s-ok-1\"}}\n",
        )
        .await
        .expect("write s-ok-1");
    stdin_tx.flush().await.expect("flush s-ok-1");
    tokio::time::timeout(Duration::from_secs(5), stream_done.notified())
        .await
        .expect("s-ok-1 executed");
    for _ in 0..16 {
        tokio::task::yield_now().await;
    }

    stdin_tx
        .write_all(
            b"{\"jsonrpc\":\"2.0\",\"id\":888,\"method\":\"gap.ngc.Query\",\"params\":{\"operation\":\"s-ok-2\"}}\n",
        )
        .await
        .expect("write s-ok-2");
    stdin_tx.flush().await.expect("flush s-ok-2");

    let mut s_resps = Vec::new();
    for _ in 0..67 {
        let line = tokio::time::timeout(Duration::from_secs(5), out_lines.next_line())
            .await
            .expect("no hang")
            .expect("io")
            .expect("line");
        let val: serde_json::Value = serde_json::from_str(&line).expect("json");
        if val["id"] == 888 {
            s_resps.push(val);
        }
    }
    assert_eq!(s_resps.len(), 2);
    for r in &s_resps {
        assert!(
            r.get("error").is_none(),
            "stream id 888 must already be released before final end send unblocks, got: {r}"
        );
        assert_eq!(r["result"]["end"], true);
    }

    // 3. Deterministic Stream error-end check (`id: 889`): `s-err-1` blocks on `out_tx.send(err_line)`.
    saturate_out_channel(&mut stdin_tx).await;
    stdin_tx
        .write_all(
            b"{\"jsonrpc\":\"2.0\",\"id\":889,\"method\":\"gap.ngc.Query\",\"params\":{\"operation\":\"s-err-1\"}}\n",
        )
        .await
        .expect("write s-err-1");
    stdin_tx.flush().await.expect("flush s-err-1");
    tokio::time::timeout(Duration::from_secs(5), stream_done.notified())
        .await
        .expect("s-err-1 executed");
    for _ in 0..16 {
        tokio::task::yield_now().await;
    }

    stdin_tx
        .write_all(
            b"{\"jsonrpc\":\"2.0\",\"id\":889,\"method\":\"gap.ngc.Query\",\"params\":{\"operation\":\"s-ok-3\"}}\n",
        )
        .await
        .expect("write s-ok-3");
    stdin_tx.flush().await.expect("flush s-ok-3");

    let mut e_resps = Vec::new();
    for _ in 0..67 {
        let line = tokio::time::timeout(Duration::from_secs(5), out_lines.next_line())
            .await
            .expect("no hang")
            .expect("io")
            .expect("line");
        let val: serde_json::Value = serde_json::from_str(&line).expect("json");
        if val["id"] == 889 {
            e_resps.push(val);
        }
    }
    assert_eq!(e_resps.len(), 2);
    assert_eq!(e_resps[0]["error"]["code"], -32001);
    assert_eq!(e_resps[0]["error"]["data"]["code"], "UNAVAILABLE");
    assert!(
        e_resps[1].get("error").is_none(),
        "stream error id 889 must already be released before final error send unblocks, got: {}",
        e_resps[1]
    );
    assert_eq!(e_resps[1]["result"]["end"], true);

    drop(stdin_tx);
    server_join
        .await
        .expect("join")
        .expect("clean stdio shutdown");
}

// spec: NGC-003, NGC-025, NGC-095
#[tokio::test(start_paused = true)]
async fn stdio_releases_in_flight_id_before_final_send_when_stream_open_fails_or_item_is_unrenderable()
 {
    // Reviewer hardening: the two spawned stream exits that the test above does not reach. Same technique: a
    // 16-byte stdout keeps the writer stuck, 65 `Health` replies fill the 64-slot channel, so the final line of the
    // call under test cannot be queued; the id must nevertheless be free for a second request.
    let health_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let health_target = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let health_reached = Arc::new(Notify::new());
    let connector_reached = Arc::new(Notify::new());

    let hc = Arc::clone(&health_count);
    let ht = Arc::clone(&health_target);
    let hr = Arc::clone(&health_reached);
    let cr = Arc::clone(&connector_reached);
    let mock = MockConnector::new()
        .on_health(move |_req, _scope| {
            if hc.fetch_add(1, Ordering::SeqCst) + 1 == ht.load(Ordering::SeqCst) {
                hr.notify_one();
            }
            Ok(HealthResponse {
                status: 1,
                ..HealthResponse::default()
            })
        })
        .on_query(move |req, _scope| match req.operation.as_str() {
            "open-fail" => {
                cr.notify_one();
                Err(NgcErrorInfo::new(
                    NgcErrorCode::Unavailable,
                    "cannot open the stream",
                ))
            }
            "nan" => {
                cr.notify_one();
                let mut fields = BTreeMap::new();
                fields.insert(
                    "metric".to_owned(),
                    prost_types::Value {
                        kind: Some(prost_types::value::Kind::NumberValue(f64::NAN)),
                    },
                );
                let row = QueryResponse {
                    chunk: Some(query_response::Chunk::Rows(RowSetChunk {
                        foreach_index: 0,
                        rows: vec![prost_types::Struct { fields }],
                        last_for_index: true,
                        error: None,
                    })),
                };
                Ok(Box::pin(stream::iter(vec![Ok(row)])))
            }
            _ => Ok(Box::pin(stream::empty())),
        });

    let core = Arc::new(ServerCore::new(
        mock,
        ServerConfig::default().with_stream_buffer(64),
    ));
    let (mut stdin_tx, stdin_rx) = tokio::io::duplex(64 * 1024);
    let (writer, stdout_rx) = tokio::io::duplex(16);
    let server_join =
        tokio::spawn(async move { serve_stdio(core, BufReader::new(stdin_rx), writer).await });
    let mut out_lines = BufReader::new(stdout_rx).lines();

    for (round, (operation, id, expected_code)) in [
        ("open-fail", 901_u32, "UNAVAILABLE"),
        ("nan", 902_u32, "INTERNAL"),
    ]
    .into_iter()
    .enumerate()
    {
        health_target.store(health_count.load(Ordering::SeqCst) + 65, Ordering::SeqCst);
        for i in 0..65 {
            let line = format!(
                "{{\"jsonrpc\":\"2.0\",\"id\":\"fill-{round}-{i}\",\"method\":\"gap.ngc.Health\",\"params\":{{}}}}\n"
            );
            stdin_tx.write_all(line.as_bytes()).await.expect("fill");
        }
        stdin_tx.flush().await.expect("flush fill");
        tokio::time::timeout(Duration::from_secs(5), health_reached.notified())
            .await
            .expect("all 65 filler Health calls completed");
        for _ in 0..32 {
            tokio::task::yield_now().await;
        }

        let first = format!(
            "{{\"jsonrpc\":\"2.0\",\"id\":{id},\"method\":\"gap.ngc.Query\",\"params\":{{\"operation\":\"{operation}\"}}}}\n"
        );
        stdin_tx.write_all(first.as_bytes()).await.expect("first");
        stdin_tx.flush().await.expect("flush first");
        tokio::time::timeout(Duration::from_secs(5), connector_reached.notified())
            .await
            .expect("connector reached");
        for _ in 0..32 {
            tokio::task::yield_now().await;
        }

        // The final line of the first call is blocked on the full channel right now; its id must already be free.
        let second = format!(
            "{{\"jsonrpc\":\"2.0\",\"id\":{id},\"method\":\"gap.ngc.Query\",\"params\":{{\"operation\":\"ok\"}}}}\n"
        );
        stdin_tx.write_all(second.as_bytes()).await.expect("second");
        stdin_tx.flush().await.expect("flush second");

        let mut resps = Vec::new();
        for _ in 0..67 {
            let line = tokio::time::timeout(Duration::from_secs(5), out_lines.next_line())
                .await
                .expect("no hang")
                .expect("io")
                .expect("line");
            let val: serde_json::Value = serde_json::from_str(&line).expect("json");
            if val["id"] == id {
                resps.push(val);
            }
        }
        assert_eq!(resps.len(), 2, "{operation}: expected exactly two replies");
        assert_eq!(
            resps[0]["error"]["code"], -32001,
            "{operation}: {}",
            resps[0]
        );
        assert_eq!(
            resps[0]["error"]["data"]["code"], expected_code,
            "{operation}: {}",
            resps[0]
        );
        assert!(
            resps[1].get("error").is_none(),
            "{operation}: id {id} must already be released before the final error line is queued, got: {}",
            resps[1]
        );
        assert_eq!(resps[1]["result"]["end"], true, "{operation}: {}", resps[1]);
    }

    drop(stdin_tx);
    server_join
        .await
        .expect("join")
        .expect("clean stdio shutdown");
}

// spec: NGC-003, NGC-095
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stdio_immediate_id_reuse_multithread_stress() {
    let mock = MockConnector::new()
        .on_describe(|_req, _scope| {
            Ok(DescribeResponse {
                not_modified: true,
                description: None,
            })
        })
        .on_query(|_req, _scope| {
            let chunk = QueryResponse {
                chunk: Some(query_response::Chunk::Rows(RowSetChunk {
                    foreach_index: 0,
                    rows: Vec::new(),
                    last_for_index: true,
                    error: None,
                })),
            };
            Ok(Box::pin(stream::iter(vec![Ok(chunk)])))
        });

    let core = Arc::new(ServerCore::new(mock, ServerConfig::default()));
    let (mut stdin_tx, stdout_rx, server_join) = in_memory_raw_stdio(core);
    let mut out_lines = BufReader::new(stdout_rx).lines();

    for i in 0..2_000_u32 {
        stdin_tx
            .write_all(
                b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"gap.ngc.Query\",\"params\":{}}\n",
            )
            .await
            .expect("write query");
        stdin_tx.flush().await.expect("flush query");

        let item_line = out_lines.next_line().await.expect("io").expect("item line");
        let item_val: serde_json::Value = serde_json::from_str(&item_line).expect("item json");
        assert_eq!(
            item_val["method"], "gap.ngc.item",
            "iteration {i}: unexpected line instead of item: {item_val}"
        );

        let end_line = out_lines.next_line().await.expect("io").expect("end line");
        let end_val: serde_json::Value = serde_json::from_str(&end_line).expect("end json");
        assert_eq!(
            end_val["result"]["end"], true,
            "iteration {i}: unexpected response: {end_val}"
        );
    }

    for i in 0..500_u32 {
        stdin_tx
            .write_all(
                b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"gap.ngc.Describe\",\"params\":{}}\n",
            )
            .await
            .expect("write describe");
        stdin_tx.flush().await.expect("flush describe");

        let line = out_lines
            .next_line()
            .await
            .expect("io")
            .expect("describe line");
        let val: serde_json::Value = serde_json::from_str(&line).expect("describe json");
        assert_eq!(
            val["result"]["notModified"], true,
            "unary iteration {i}: unexpected response: {val}"
        );
    }

    drop(stdin_tx);
    server_join.await.expect("join").expect("clean shutdown");
}

// spec: NGC-001, NGC-062, NGC-063, NGC-093
#[tokio::test(start_paused = true)]
async fn grpc_client_fuses_streams_after_first_error_and_drops_body() {
    let make_row = |len: usize| -> QueryResponse {
        let mut fields = BTreeMap::new();
        fields.insert(
            "payload".to_owned(),
            prost_types::Value {
                kind: Some(prost_types::value::Kind::StringValue("r".repeat(len))),
            },
        );
        QueryResponse {
            chunk: Some(query_response::Chunk::Rows(RowSetChunk {
                foreach_index: 0,
                rows: vec![prost_types::Struct { fields }],
                last_for_index: false,
                error: None,
            })),
        }
    };

    let mock = MockConnector::new()
        .on_query(move |req, _scope| {
            if req.operation == "eight-kib" {
                // One 8 KiB row followed by a pending tail so we also prove the underlying body is dropped!
                let first = stream::iter(vec![Ok(make_row(8 * 1024))]);
                Ok(Box::pin(first.chain(stream::pending())))
            } else {
                // Two good items + one error + trailing item that must never be seen.
                let items = vec![
                    Ok(make_row(16)),
                    Ok(make_row(32)),
                    Err(NgcErrorInfo::new(
                        NgcErrorCode::Unavailable,
                        "midstream fail",
                    )),
                    Ok(make_row(64)),
                ];
                Ok(Box::pin(stream::iter(items)))
            }
        })
        .on_subscribe(|_req, _scope| {
            let mut fields = BTreeMap::new();
            fields.insert(
                "payload".to_owned(),
                prost_types::Value {
                    kind: Some(prost_types::value::Kind::StringValue("e".repeat(8 * 1024))),
                },
            );
            let ev = Event {
                id: "big-ev".to_owned(),
                at: None,
                payload: Some(prost_types::Struct { fields }),
                resume_token: String::new(),
            };
            Ok(Box::pin(
                stream::iter(vec![Ok(ev)]).chain(stream::pending()),
            ))
        })
        .on_watch_task(|_req, _scope| {
            let ts = TaskStatus {
                task_id: "t".repeat(8 * 1024),
                state: TaskState::Completed as i32,
                ..TaskStatus::default()
            };
            Ok(Box::pin(
                stream::iter(vec![Ok(ts)]).chain(stream::pending()),
            ))
        });

    let core = Arc::new(ServerCore::new(mock.clone(), ServerConfig::default()));
    let (base_client, _handle) = in_memory_grpc(core);
    let client = base_client.with_max_decoding_message_size(4_096);

    // 1. `query`: 8 KiB message > 4 KiB client bound -> exact 1 `RESOURCE_EXHAUSTED` item, then `None`!
    let mut qs = client
        .query(QueryRequest {
            operation: "eight-kib".to_owned(),
            ..QueryRequest::default()
        })
        .await
        .expect("open query");
    let first_q = qs
        .next()
        .await
        .expect("first item")
        .expect_err("must fail decode bound");
    assert_eq!(first_q.code, NgcErrorCode::ResourceExhausted);
    assert!(
        qs.next().await.is_none(),
        "query stream must be fused to None immediately after first Err"
    );
    assert!(qs.next().await.is_none());
    tokio::time::timeout(Duration::from_secs(5), mock.wait_for_drops(1))
        .await
        .expect("underlying query body dropped on first decode error");

    // 2. `subscribe`: 8 KiB event > 4 KiB client bound -> exact 1 `RESOURCE_EXHAUSTED` item, then `None`!
    let mut ss = client
        .subscribe(SubscribeRequest::default())
        .await
        .expect("open subscribe");
    let first_s = ss
        .next()
        .await
        .expect("first item")
        .expect_err("must fail decode bound");
    assert_eq!(first_s.code, NgcErrorCode::ResourceExhausted);
    assert!(
        ss.next().await.is_none(),
        "subscribe stream must be fused to None immediately after first Err"
    );
    tokio::time::timeout(Duration::from_secs(5), mock.wait_for_drops(2))
        .await
        .expect("underlying subscribe body dropped on first decode error");

    // 3. `watch_task`: 8 KiB status > 4 KiB client bound -> exact 1 `RESOURCE_EXHAUSTED` item, then `None`!
    let mut ws = client
        .watch_task(WatchTaskRequest::default())
        .await
        .expect("open watch_task");
    let first_w = ws
        .next()
        .await
        .expect("first item")
        .expect_err("must fail decode bound");
    assert_eq!(first_w.code, NgcErrorCode::ResourceExhausted);
    assert!(
        ws.next().await.is_none(),
        "watch_task stream must be fused to None immediately after first Err"
    );
    tokio::time::timeout(Duration::from_secs(5), mock.wait_for_drops(3))
        .await
        .expect("underlying watch_task body dropped on first decode error");

    // 4. Server failing after 2 good items yields exact 2 Ok items + 1 Err + None!
    let mut two_good = client
        .query(QueryRequest {
            operation: "two-good-then-err".to_owned(),
            ..QueryRequest::default()
        })
        .await
        .expect("open two-good");
    assert!(two_good.next().await.expect("item 0").is_ok());
    assert!(two_good.next().await.expect("item 1").is_ok());
    let third = two_good
        .next()
        .await
        .expect("item 2")
        .expect_err("3rd item must be error");
    assert_eq!(third.code, NgcErrorCode::Unavailable);
    assert!(two_good.next().await.is_none());
    assert!(two_good.next().await.is_none());
}

// spec: NGC-001, NGC-062, NGC-063, NGC-093
#[test]
fn proptest_grpc_client_stream_never_yields_after_first_error() {
    use proptest::prelude::*;
    use proptest::test_runner::{Config as ProptestConfig, TestRunner};

    #[derive(Debug, Clone, Copy)]
    enum ScriptStep {
        GoodSmall(u16),
        GoodOversized,
        ServerErr(NgcErrorCode),
    }

    let step_strategy = prop_oneof![
        (1_u16..200).prop_map(ScriptStep::GoodSmall),
        Just(ScriptStep::GoodOversized),
        prop_oneof![
            Just(NgcErrorCode::InvalidArgument),
            Just(NgcErrorCode::Unavailable),
            Just(NgcErrorCode::Internal),
            Just(NgcErrorCode::ResourceExhausted),
        ]
        .prop_map(ScriptStep::ServerErr),
    ];

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio rt");

    let script_slot: Arc<std::sync::Mutex<Vec<ScriptStep>>> =
        Arc::new(std::sync::Mutex::new(Vec::new()));
    let slot_c = Arc::clone(&script_slot);

    let mock = MockConnector::new().on_query(move |_req, _scope| {
        let steps = slot_c.lock().expect("lock").clone();
        let items: Vec<Result<QueryResponse, NgcErrorInfo>> = steps
            .into_iter()
            .map(|st| match st {
                ScriptStep::GoodSmall(n) => {
                    let mut fields = BTreeMap::new();
                    fields.insert(
                        "s".to_owned(),
                        prost_types::Value {
                            kind: Some(prost_types::value::Kind::StringValue(
                                "a".repeat(usize::from(n)),
                            )),
                        },
                    );
                    Ok(QueryResponse {
                        chunk: Some(query_response::Chunk::Rows(RowSetChunk {
                            foreach_index: 0,
                            rows: vec![prost_types::Struct { fields }],
                            last_for_index: false,
                            error: None,
                        })),
                    })
                }
                ScriptStep::GoodOversized => {
                    let mut fields = BTreeMap::new();
                    fields.insert(
                        "s".to_owned(),
                        prost_types::Value {
                            kind: Some(prost_types::value::Kind::StringValue("Z".repeat(4_096))),
                        },
                    );
                    Ok(QueryResponse {
                        chunk: Some(query_response::Chunk::Rows(RowSetChunk {
                            foreach_index: 0,
                            rows: vec![prost_types::Struct { fields }],
                            last_for_index: false,
                            error: None,
                        })),
                    })
                }
                ScriptStep::ServerErr(code) => Err(NgcErrorInfo::new(code, "scripted error")),
            })
            .collect();
        Ok(Box::pin(stream::iter(items)))
    });

    let (client, _handle) = rt.block_on(async {
        let core = Arc::new(ServerCore::new(mock, ServerConfig::default()));
        let (c, h) = in_memory_grpc(core);
        (c.with_max_decoding_message_size(1_024), h)
    });

    let mut runner = TestRunner::new(ProptestConfig {
        cases: 64,
        failure_persistence: None,
        ..ProptestConfig::default()
    });

    runner
        .run(&proptest::collection::vec(step_strategy, 0..16), |steps| {
            let expected_ok_prefix = steps
                .iter()
                .take_while(|s| matches!(s, ScriptStep::GoodSmall(_)))
                .count();
            let expected_err_code =
                steps
                    .get(expected_ok_prefix)
                    .map(|first_bad| match *first_bad {
                        ScriptStep::GoodOversized => NgcErrorCode::ResourceExhausted,
                        ScriptStep::ServerErr(c) => c,
                        ScriptStep::GoodSmall(_) => unreachable!(),
                    });

            *script_slot.lock().expect("lock") = steps;

            rt.block_on(async {
                let mut s = client
                    .query(QueryRequest::default())
                    .await
                    .expect("open query");
                let mut seen = Vec::new();
                while let Some(res) = s.next().await {
                    seen.push(res);
                    if seen.len() > 32 {
                        break;
                    }
                }
                // Extra polls after None must continue returning None.
                for _ in 0..3 {
                    assert!(s.next().await.is_none());
                }

                match expected_err_code {
                    None => {
                        prop_assert_eq!(seen.len(), expected_ok_prefix);
                        prop_assert!(seen.iter().all(Result::is_ok));
                    }
                    Some(code) => {
                        prop_assert_eq!(
                            seen.len(),
                            expected_ok_prefix + 1,
                            "stream yielded items after first error: {:?}",
                            seen.len()
                        );
                        prop_assert!(seen[..expected_ok_prefix].iter().all(Result::is_ok));
                        let err = seen.pop().expect("err item").expect_err("must be Err");
                        prop_assert_eq!(err.code, code);
                    }
                }
                Ok(())
            })
        })
        .expect("GrpcClient stream fuse proptest");
}

// spec: NGC-002, NGC-070, NGC-093, NGC-094
#[tokio::test(start_paused = true)]
async fn http_body_limit_exact_boundary_content_types_sse_headers_and_health_body_timeout() {
    let mock = MockConnector::new()
        .on_describe(|req, _scope| {
            Ok(DescribeResponse {
                not_modified: !req.known_spec_hash.is_empty(),
                description: None,
            })
        })
        .on_subscribe(|_req, _scope| {
            Ok(Box::pin(stream::iter(vec![Ok(Event {
                id: "ev-1".to_owned(),
                ..Event::default()
            })])))
        })
        .on_watch_task(|_req, _scope| {
            Ok(Box::pin(stream::iter(vec![Ok(TaskStatus {
                task_id: "t-1".to_owned(),
                state: TaskState::Completed as i32,
                ..TaskStatus::default()
            })])))
        });

    let max_bytes = 64_usize;
    let core = Arc::new(ServerCore::new(
        mock,
        ServerConfig::default()
            .with_max_request_bytes(max_bytes)
            .with_health_timeout(Duration::from_millis(500))
            .with_default_timeout(Duration::from_secs(60)),
    ));
    let app = ngc_http::router(core);

    let prefix = r#"{"knownSpecHash":""#;
    let suffix = r#""}"#;
    let fill_exact = "a".repeat(max_bytes - prefix.len() - suffix.len());
    let body_64 = format!("{prefix}{fill_exact}{suffix}").into_bytes();
    assert_eq!(body_64.len(), 64);

    let fill_over = "a".repeat(max_bytes + 1 - prefix.len() - suffix.len());
    let body_65 = format!("{prefix}{fill_over}{suffix}").into_bytes();
    assert_eq!(body_65.len(), 65);

    // 1. T-8 (H04): `Content-Length` == `max_request_bytes` (`64`) -> 200 OK; `65` -> 413 Payload Too Large.
    let cl_ok = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/gap/ngc/v1/Describe")
                .header(http::header::CONTENT_TYPE, "application/json")
                .header(http::header::CONTENT_LENGTH, "64")
                .body(Body::from(body_64.clone()))
                .expect("req"),
        )
        .await
        .expect("oneshot");
    assert_eq!(
        cl_ok.status(),
        StatusCode::OK,
        "Content-Length == max_request_bytes (64) must be accepted"
    );

    let unpolled_flag = Arc::new(AtomicBool::new(false));
    let unpolled_c = Arc::clone(&unpolled_flag);
    let cl_65_stream = stream::once(async move {
        unpolled_c.store(true, Ordering::SeqCst);
        Ok::<_, std::io::Error>(bytes::Bytes::from(vec![b'{', b'}']))
    });
    let cl_413 = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/gap/ngc/v1/Describe")
                .header(http::header::CONTENT_TYPE, "application/json")
                .header(http::header::CONTENT_LENGTH, "65")
                .body(Body::from_stream(cl_65_stream))
                .expect("req"),
        )
        .await
        .expect("oneshot");
    assert_eq!(
        cl_413.status(),
        StatusCode::PAYLOAD_TOO_LARGE,
        "Content-Length == max_request_bytes + 1 (65) must be 413"
    );
    assert!(
        !unpolled_flag.load(Ordering::SeqCst),
        "Content-Length > max_request_bytes must reject before reading body stream"
    );

    // 2. T-8 (H05): Chunked / streaming body without `Content-Length`: exact `64` (`32 + 32`) -> 200 OK; `65` (`32 + 33`) -> 413.
    let c0_ok = bytes::Bytes::copy_from_slice(&body_64[..32]);
    let c1_ok = bytes::Bytes::copy_from_slice(&body_64[32..]);
    let chunked_ok = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/gap/ngc/v1/Describe")
                .header(http::header::CONTENT_TYPE, "application/json")
                .body(Body::from_stream(stream::iter(vec![
                    Ok::<_, std::io::Error>(c0_ok),
                    Ok(c1_ok),
                ])))
                .expect("req"),
        )
        .await
        .expect("oneshot");
    assert_eq!(
        chunked_ok.status(),
        StatusCode::OK,
        "chunked body of exact max_request_bytes (32 + 32 = 64) must be accepted"
    );

    let c0_over = bytes::Bytes::copy_from_slice(&body_65[..32]);
    let c1_over = bytes::Bytes::copy_from_slice(&body_65[32..]);
    let chunked_413 = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/gap/ngc/v1/Describe")
                .header(http::header::CONTENT_TYPE, "application/json")
                .body(Body::from_stream(stream::iter(vec![
                    Ok::<_, std::io::Error>(c0_over),
                    Ok(c1_over),
                ])))
                .expect("req"),
        )
        .await
        .expect("oneshot");
    assert_eq!(
        chunked_413.status(),
        StatusCode::PAYLOAD_TOO_LARGE,
        "chunked body of max_request_bytes + 1 (32 + 33 = 65) must be 413"
    );

    // 3. T-9 (H09): `Content-Type` media-type parsing: parameters & case-insensitivity accepted, prefix-mismatch/other/missing 415.
    for good_ct in [
        "application/json",
        "application/json; charset=utf-8",
        "Application/JSON",
        "APPLICATION/JSON ; charset=UTF-8",
    ] {
        let ok_ct = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/gap/ngc/v1/Describe")
                    .header(http::header::CONTENT_TYPE, good_ct)
                    .body(Body::from("{}"))
                    .expect("req"),
            )
            .await
            .expect("oneshot");
        assert_eq!(
            ok_ct.status(),
            StatusCode::OK,
            "Content-Type `{good_ct}` must be accepted"
        );
    }

    for bad_ct in [
        Some("application/jsonx"),
        Some("application/jsonx; charset=utf-8"),
        Some("text/json"),
        Some(""),
        None,
    ] {
        let mut req_b = Request::builder()
            .method(Method::POST)
            .uri("/gap/ngc/v1/Describe");
        if let Some(ct) = bad_ct {
            req_b = req_b.header(http::header::CONTENT_TYPE, ct);
        }
        let bad_resp = app
            .clone()
            .oneshot(req_b.body(Body::from("{}")).expect("req"))
            .await
            .expect("oneshot");
        assert_eq!(
            bad_resp.status(),
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "Content-Type `{bad_ct:?}` must be rejected with 415"
        );
    }

    // 4. T-10 (H08): SSE endpoints (`Subscribe` & `WatchTask`) carry `Cache-Control: no-cache` and `Content-Type: text/event-stream`.
    for sse_uri in ["/gap/ngc/v1/Subscribe", "/gap/ngc/v1/WatchTask"] {
        let sse_resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri(sse_uri)
                    .header(http::header::CONTENT_TYPE, "application/json")
                    .body(Body::from("{}"))
                    .expect("req"),
            )
            .await
            .expect("oneshot");
        assert_eq!(sse_resp.status(), StatusCode::OK);
        assert_eq!(
            sse_resp
                .headers()
                .get(http::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok()),
            Some("text/event-stream"),
            "{sse_uri} missing Content-Type: text/event-stream"
        );
        assert_eq!(
            sse_resp
                .headers()
                .get(http::header::CACHE_CONTROL)
                .and_then(|v| v.to_str().ok()),
            Some("no-cache"),
            "{sse_uri} missing Cache-Control: no-cache"
        );
    }

    // 5. T-11 (H11): Stalled `Health` request body fails 504 `DEADLINE_EXCEEDED` at `health_timeout` (500ms), NOT `default_timeout` (60s).
    let t0 = tokio::time::Instant::now();
    let stalled_health_body =
        stream::once(async { Ok::<_, std::io::Error>(bytes::Bytes::from_static(b"{")) })
            .chain(stream::pending());
    let health_504 = tokio::time::timeout(
        Duration::from_secs(1),
        app.oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/gap/ngc/v1/Health")
                .header(http::header::CONTENT_TYPE, "application/json")
                .body(Body::from_stream(stalled_health_body))
                .expect("req"),
        ),
    )
    .await
    .expect("Health body timeout must fire at health_timeout (500ms), not default_timeout (60s)")
    .expect("oneshot");

    assert_eq!(
        tokio::time::Instant::now().duration_since(t0),
        Duration::from_millis(500),
        "Health body read timeout must equal health_timeout (500ms)"
    );
    assert_eq!(health_504.status(), StatusCode::GATEWAY_TIMEOUT);
    let err_bytes = health_504
        .into_body()
        .collect()
        .await
        .expect("collect")
        .to_bytes();
    let err_json: serde_json::Value = serde_json::from_slice(&err_bytes).expect("json");
    assert_eq!(err_json["code"], "DEADLINE_EXCEEDED");
}

struct ScriptedChunksReader {
    chunks: Vec<Vec<u8>>,
    pos: usize,
    polled_past_script: Option<Arc<AtomicBool>>,
    eof_fut: Pin<Box<tokio_util::sync::WaitForCancellationFutureOwned>>,
}

impl AsyncRead for ScriptedChunksReader {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        if self.pos >= self.chunks.len() {
            if let Some(ref flag) = self.polled_past_script {
                flag.store(true, Ordering::SeqCst);
            }
            return match self.eof_fut.as_mut().poll(cx) {
                Poll::Ready(()) => Poll::Ready(Ok(())),
                Poll::Pending => Poll::Pending,
            };
        }
        let idx = self.pos;
        let slice = &self.chunks[idx];
        assert!(
            buf.remaining() >= slice.len(),
            "BufReader capacity must fit scripted chunk"
        );
        buf.put_slice(slice);
        self.pos += 1;
        Poll::Ready(Ok(()))
    }
}

// spec: NGC-003, NGC-093, NGC-095
#[tokio::test(start_paused = true)]
async fn stdio_line_limit_exact_boundary_single_read_and_partial_reads() {
    let max_bytes = 128_usize;
    let prefix =
        r#"{"jsonrpc":"2.0","id":1,"method":"gap.ngc.Describe","params":{"knownSpecHash":""#;
    let suffix = r#""}}"#;
    let fill_128 = "x".repeat(max_bytes - prefix.len() - suffix.len());
    let line_128 = format!("{prefix}{fill_128}{suffix}").into_bytes();
    assert_eq!(line_128.len(), 128);

    let fill_129 = "x".repeat(max_bytes + 1 - prefix.len() - suffix.len());
    let line_129 = format!("{prefix}{fill_129}{suffix}").into_bytes();
    assert_eq!(line_129.len(), 129);

    let make_core = || {
        let mock = MockConnector::new().on_describe(|req, _scope| {
            Ok(DescribeResponse {
                not_modified: !req.known_spec_hash.is_empty(),
                description: None,
            })
        });
        Arc::new(ServerCore::new(
            mock,
            ServerConfig::default().with_max_request_bytes(max_bytes),
        ))
    };

    // 1. S01 exact boundary (single read): `128` bytes + `\n` (and `128` bytes + `\r\n`) accepted!
    for line_ending in [&b"\n"[..], &b"\r\n"[..]] {
        let mut wire = line_128.clone();
        wire.extend_from_slice(line_ending);
        let eof_gate = tokio_util::sync::CancellationToken::new();
        let reader = BufReader::with_capacity(
            512,
            ScriptedChunksReader {
                chunks: vec![wire],
                pos: 0,
                polled_past_script: None,
                eof_fut: Box::pin(eof_gate.clone().cancelled_owned()),
            },
        );
        let (writer, client_read) = tokio::io::duplex(4_096);
        let core = make_core();
        let server_join = tokio::spawn(async move { serve_stdio(core, reader, writer).await });

        let mut out = BufReader::new(client_read).lines();
        let resp_line = out.next_line().await.expect("io").expect("response line");
        let val: serde_json::Value = serde_json::from_str(&resp_line).expect("json");
        assert_eq!(
            val["result"]["notModified"], true,
            "single-read exact max_request_bytes (128) must succeed, got: {val}"
        );
        eof_gate.cancel();
        server_join.await.expect("join").expect("clean EOF");
        assert!(out.next_line().await.expect("io").is_none());
    }

    // 2. S01 +1 byte (single read): `129` bytes + `\n` rejected with fatal `-32600` (without waiting for EOF)!
    {
        let mut wire_129 = line_129.clone();
        wire_129.push(b'\n');
        let never_eof = tokio_util::sync::CancellationToken::new();
        let reader = BufReader::with_capacity(
            512,
            ScriptedChunksReader {
                chunks: vec![wire_129],
                pos: 0,
                polled_past_script: None,
                eof_fut: Box::pin(never_eof.cancelled_owned()),
            },
        );
        let (writer, client_read) = tokio::io::duplex(4_096);
        tokio::time::timeout(
            Duration::from_secs(1),
            serve_stdio(make_core(), reader, writer),
        )
        .await
        .expect("single-read 129-byte line must terminate immediately without waiting for EOF")
        .expect("single-read 129-byte line ends session cleanly");
        let mut out = BufReader::new(client_read).lines();
        let resp_line = out
            .next_line()
            .await
            .expect("io")
            .expect("fatal -32600 line");
        let val: serde_json::Value = serde_json::from_str(&resp_line).expect("json");
        assert_eq!(val["error"]["code"], -32600);
        assert_eq!(val["error"]["data"]["code"], "INVALID_ARGUMENT");
        assert!(out.next_line().await.expect("io").is_none());
    }

    // 3. S02 exact boundary (several partial reads): `[64 bytes, 64 bytes, "\n"]` (`64 + 64 == 128` before `\n` arrives) accepted!
    {
        let eof_gate = tokio_util::sync::CancellationToken::new();
        let reader = BufReader::with_capacity(
            256,
            ScriptedChunksReader {
                chunks: vec![
                    line_128[..64].to_vec(),
                    line_128[64..128].to_vec(),
                    b"\n".to_vec(),
                ],
                pos: 0,
                polled_past_script: None,
                eof_fut: Box::pin(eof_gate.clone().cancelled_owned()),
            },
        );
        let (writer, client_read) = tokio::io::duplex(4_096);
        let core = make_core();
        let server_join = tokio::spawn(async move { serve_stdio(core, reader, writer).await });

        let mut out = BufReader::new(client_read).lines();
        let resp_line = out.next_line().await.expect("io").expect("response line");
        let val: serde_json::Value = serde_json::from_str(&resp_line).expect("json");
        assert_eq!(
            val["result"]["notModified"], true,
            "partial-read exact max_request_bytes (64 + 64 = 128) must succeed, got: {val}"
        );
        eof_gate.cancel();
        server_join.await.expect("join").expect("clean EOF");
        assert!(out.next_line().await.expect("io").is_none());
    }

    // 4. S02 +1 byte (several partial reads): `[64 bytes, 65 bytes]` (`64 + 65 == 129` without `\n`) rejected immediately with fatal `-32600`!
    {
        let read_third = Arc::new(AtomicBool::new(false));
        let never_eof = tokio_util::sync::CancellationToken::new();
        let reader = BufReader::with_capacity(
            256,
            ScriptedChunksReader {
                chunks: vec![line_129[..64].to_vec(), line_129[64..129].to_vec()],
                pos: 0,
                polled_past_script: Some(Arc::clone(&read_third)),
                eof_fut: Box::pin(never_eof.cancelled_owned()),
            },
        );
        let (writer, client_read) = tokio::io::duplex(4_096);
        tokio::time::timeout(
            Duration::from_secs(1),
            serve_stdio(make_core(), reader, writer),
        )
        .await
        .expect("partial-read 129-byte line must terminate immediately without waiting for EOF")
        .expect("partial-read 129-byte line ends session cleanly");
        assert!(
            !read_third.load(Ordering::SeqCst),
            "partial-read 129-byte line must fail immediately without reading further chunks"
        );
        let mut out = BufReader::new(client_read).lines();
        let resp_line = out
            .next_line()
            .await
            .expect("io")
            .expect("fatal -32600 line");
        let val: serde_json::Value = serde_json::from_str(&resp_line).expect("json");
        assert_eq!(val["error"]["code"], -32600);
        assert_eq!(val["error"]["data"]["code"], "INVALID_ARGUMENT");
        assert!(out.next_line().await.expect("io").is_none());
    }
}
