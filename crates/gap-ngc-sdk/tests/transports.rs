//! Per-transport conformance tests for HTTP (`NGC-002`, `NGC-094`), stdio (`NGC-003`, `NGC-095`), and gRPC (`NGC-001`, `NGC-062`, `NGC-063`, `NGC-093`).
#![forbid(unsafe_code)]

use std::collections::BTreeMap;
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
use gap_ngc_sdk::grpc::{GrpcClient, ServeError, serve_tcp};
use gap_ngc_sdk::http as ngc_http;
use gap_ngc_sdk::mock::MockConnector;
use gap_ngc_sdk::stdio::serve_stdio;
use gap_ngc_sdk::testing::{in_memory_grpc, in_memory_raw_stdio};
use gap_proto::ngc::v1::{
    DescribeRequest, DescribeResponse, Event, HealthRequest, HealthResponse, QueryResponse,
    RowSetChunk, SubscribeRequest, TaskState, TaskStatus, WatchTaskRequest,
    connector_server::{Connector as TonicConnector, ConnectorServer},
    query_response,
};
use http::{Method, Request, StatusCode};
use http_body_util::BodyExt as _;
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
    mock.wait_for_drops(1).await;
    assert_eq!(mock.in_flight(), 0);

    // 7. EOF cancels everything in flight and `serve_stdio` returns cleanly!
    stdin_tx
        .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":\"hang-eof\",\"method\":\"gap.ngc.Query\",\"params\":{\"operation\":\"hang\"}}\n")
        .await
        .expect("write");
    cancel_entered.notified().await;
    assert_eq!(mock.in_flight(), 1);

    drop(stdin_tx); // trigger EOF on stdin
    mock.wait_for_drops(2).await;
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

    let fatal_line = out_lines
        .next_line()
        .await
        .expect("io")
        .expect("fatal slow-loris line");
    let fatal_val: serde_json::Value = serde_json::from_str(&fatal_line).expect("json");
    assert_eq!(fatal_val["error"]["code"], -32600);
    assert_eq!(fatal_val["error"]["data"]["code"], "DEADLINE_EXCEEDED");
    server_join
        .await
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
        let err = serve_tcp(Arc::clone(&strict_core), addr)
            .await
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
