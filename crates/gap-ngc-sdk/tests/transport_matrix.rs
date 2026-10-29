//! Cross-transport equivalence matrix over socket-free gRPC, HTTP, and stdio (`NGC-001`..`003`, `NGC-020`, `NGC-025`, `NGC-026`, `NGC-031`, `NGC-060`..`063`, `NGC-070`, `NGC-071`, `NGC-090`, `NGC-093`..`096`, `INV-12`, `INV-15`).
#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use futures_util::{StreamExt, stream};
use gap_core::clock::ManualClock;
use gap_core::error::{ErrorCode, MAX_ERROR_MESSAGE_CHARS, NgcErrorCode, NgcErrorInfo};
use gap_ngc_sdk::core::{ServerConfig, ServerCore};
use gap_ngc_sdk::error::{CONNECTOR_PANIC_MESSAGE, ngc_error_from_proto, prost_struct_to_json_map};
use gap_ngc_sdk::foreach::{ForeachConfig, foreach};
use gap_ngc_sdk::grpc::GrpcClient;
use gap_ngc_sdk::http as ngc_http;
use gap_ngc_sdk::json::{from_json_slice, from_json_value, to_json_vec};
use gap_ngc_sdk::mock::{MockConnector, RpcMethod};
use gap_ngc_sdk::testing::{
    InMemoryGrpcHandle, InMemoryStdioHandle, StdioTestClient, in_memory_grpc, in_memory_stdio,
};
use gap_proto::NGC_WIRE_VERSION;
use gap_proto::ngc::v1::{
    CallContext, Complete, ConnectorDescription, DescribeRequest, DescribeResponse, Event,
    HealthRequest, HealthResponse, MutateRequest, MutateResponse, NgcError, Protocol, QueryRequest,
    QueryResponse, RowSetChunk, SketchRequest, SketchResponse, SubscribeRequest, VariableSet,
    query_response,
};
use http::{Method, Request, StatusCode};
use http_body_util::BodyExt as _;
use tokio::sync::Notify;
use tower::ServiceExt as _;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TransportKind {
    Grpc,
    Http,
    Stdio,
}

const ALL_TRANSPORTS: [TransportKind; 3] = [
    TransportKind::Grpc,
    TransportKind::Http,
    TransportKind::Stdio,
];

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

#[derive(Debug, Clone, PartialEq, Eq)]
struct NormalizedError {
    code: NgcErrorCode,
    message: String,
    retryable: bool,
    retry_after_ms: u32,
    has_details: bool,
}

impl From<NgcErrorInfo> for NormalizedError {
    fn from(e: NgcErrorInfo) -> Self {
        Self {
            code: e.code,
            message: e.message().to_owned(),
            retryable: e.retryable,
            retry_after_ms: e.retry_after_ms,
            has_details: e.details.is_some(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
struct NormalizedQueryOutcome {
    rows_by_index: BTreeMap<u32, Vec<serde_json::Map<String, serde_json::Value>>>,
    closing_counts: BTreeMap<u32, usize>,
    index_errors: BTreeMap<u32, NormalizedError>,
    upstream_calls: Option<u32>,
    rows_total: Option<u64>,
    terminal_error: Option<NormalizedError>,
}

impl NormalizedQueryOutcome {
    fn from_items_and_terminal(
        items: Vec<QueryResponse>,
        terminal_error: Option<NgcErrorInfo>,
    ) -> Self {
        let mut out = Self {
            rows_by_index: BTreeMap::new(),
            closing_counts: BTreeMap::new(),
            index_errors: BTreeMap::new(),
            upstream_calls: None,
            rows_total: None,
            terminal_error: terminal_error.map(NormalizedError::from),
        };
        for item in items {
            match item.chunk {
                Some(query_response::Chunk::Rows(rc)) => {
                    let entry = out.rows_by_index.entry(rc.foreach_index).or_default();
                    for row in &rc.rows {
                        entry.push(prost_struct_to_json_map(row));
                    }
                    if rc.last_for_index {
                        *out.closing_counts.entry(rc.foreach_index).or_default() += 1;
                    }
                    if let Some(proto_err) = rc.error {
                        out.index_errors.insert(
                            rc.foreach_index,
                            NormalizedError::from(ngc_error_from_proto(&proto_err)),
                        );
                    }
                }
                Some(query_response::Chunk::Complete(c)) => {
                    out.upstream_calls = Some(c.upstream_calls);
                    out.rows_total = Some(c.rows_total);
                }
                None => {}
            }
        }
        out
    }
}

enum Harness {
    Grpc {
        client: GrpcClient,
        _handle: InMemoryGrpcHandle,
    },
    Http {
        router: axum::Router,
    },
    Stdio {
        client: StdioTestClient,
        _handle: InMemoryStdioHandle,
    },
}

impl Harness {
    fn new(kind: TransportKind, core: Arc<ServerCore<MockConnector>>) -> Self {
        match kind {
            TransportKind::Grpc => {
                let (client, handle) = in_memory_grpc(core);
                Self::Grpc {
                    client,
                    _handle: handle,
                }
            }
            TransportKind::Http => Self::Http {
                router: ngc_http::router(core),
            },
            TransportKind::Stdio => {
                let (client, handle) = in_memory_stdio(core);
                Self::Stdio {
                    client,
                    _handle: handle,
                }
            }
        }
    }

    async fn http_unary<Req, Resp>(&self, method: &str, req: &Req) -> Result<Resp, NgcErrorInfo>
    where
        Req: gap_ngc_sdk::json::ProtoMessage,
        Resp: gap_ngc_sdk::json::ProtoMessage,
    {
        let Self::Http { router } = self else {
            unreachable!()
        };
        let body = to_json_vec(req).expect("encode req JSON");
        let http_req = Request::builder()
            .method(Method::POST)
            .uri(format!("/gap/ngc/v1/{method}"))
            .header(http::header::CONTENT_TYPE, "application/json")
            .body(Body::from(body))
            .expect("build HTTP req");
        let resp = router
            .clone()
            .oneshot(http_req)
            .await
            .expect("HTTP oneshot");
        let status = resp.status();
        let bytes = resp
            .into_body()
            .collect()
            .await
            .expect("collect HTTP body")
            .to_bytes();
        if status == StatusCode::OK {
            Ok(from_json_slice::<Resp>(&bytes).expect("decode HTTP OK JSON"))
        } else {
            let proto_err: NgcError = from_json_slice(&bytes).unwrap_or_else(|_| NgcError {
                code: i32::from(NgcErrorCode::InvalidArgument),
                message: "http error".to_owned(),
                retryable: false,
                retry_after_ms: 0,
                details: None,
            });
            Err(ngc_error_from_proto(&proto_err))
        }
    }

    async fn describe(&self, req: DescribeRequest) -> Result<DescribeResponse, NgcErrorInfo> {
        match self {
            Self::Grpc { client, .. } => client.clone().describe(req).await,
            Self::Http { .. } => self.http_unary("Describe", &req).await,
            Self::Stdio { client, .. } => client.describe(req).await,
        }
    }

    async fn mutate(&self, req: MutateRequest) -> Result<MutateResponse, NgcErrorInfo> {
        match self {
            Self::Grpc { client, .. } => client.clone().mutate(req).await,
            Self::Http { .. } => self.http_unary("Mutate", &req).await,
            Self::Stdio { client, .. } => client.mutate(req).await,
        }
    }

    async fn sketch(&self, req: SketchRequest) -> Result<SketchResponse, NgcErrorInfo> {
        match self {
            Self::Grpc { client, .. } => client.clone().sketch(req).await,
            Self::Http { .. } => self.http_unary("Sketch", &req).await,
            Self::Stdio { client, .. } => client.sketch(req).await,
        }
    }

    async fn health(&self, req: HealthRequest) -> Result<HealthResponse, NgcErrorInfo> {
        match self {
            Self::Grpc { client, .. } => client.clone().health(req).await,
            Self::Http { .. } => self.http_unary("Health", &req).await,
            Self::Stdio { client, .. } => client.health(req).await,
        }
    }

    async fn query_all(&self, req: QueryRequest) -> NormalizedQueryOutcome {
        match self {
            Self::Grpc { client, .. } => match client.clone().query(req).await {
                Err(err) => NormalizedQueryOutcome::from_items_and_terminal(Vec::new(), Some(err)),
                Ok(mut s) => {
                    let mut items = Vec::new();
                    let mut term = None;
                    while let Some(res) = s.next().await {
                        match res {
                            Ok(item) => items.push(item),
                            Err(err) => {
                                term = Some(err);
                                break;
                            }
                        }
                    }
                    NormalizedQueryOutcome::from_items_and_terminal(items, term)
                }
            },
            Self::Http { router } => {
                let body = to_json_vec(&req).expect("encode QueryRequest");
                let http_req = Request::builder()
                    .method(Method::POST)
                    .uri("/gap/ngc/v1/Query")
                    .header(http::header::CONTENT_TYPE, "application/json")
                    .body(Body::from(body))
                    .expect("build HTTP Query req");
                let resp = router.clone().oneshot(http_req).await.expect("HTTP Query");
                let status = resp.status();
                let bytes = resp
                    .into_body()
                    .collect()
                    .await
                    .expect("collect NDJSON")
                    .to_bytes();
                if status != StatusCode::OK {
                    let proto_err: NgcError =
                        from_json_slice(&bytes).unwrap_or_else(|_| NgcError {
                            code: i32::from(NgcErrorCode::InvalidArgument),
                            message: "http error".to_owned(),
                            retryable: false,
                            retry_after_ms: 0,
                            details: None,
                        });
                    return NormalizedQueryOutcome::from_items_and_terminal(
                        Vec::new(),
                        Some(ngc_error_from_proto(&proto_err)),
                    );
                }
                let text = std::str::from_utf8(&bytes).expect("UTF-8 NDJSON");
                let mut items = Vec::new();
                let mut term = None;
                for line in text.lines() {
                    if line.is_empty() {
                        continue;
                    }
                    assert!(
                        term.is_none(),
                        "HTTP NDJSON emitted trailing line `{line}` after terminal error"
                    );
                    let val: serde_json::Value =
                        serde_json::from_str(line).expect("NDJSON line JSON");
                    if let Some(err_obj) = val.get("error") {
                        let proto_err: NgcError =
                            from_json_value(err_obj.clone()).expect("decode NDJSON error");
                        term = Some(ngc_error_from_proto(&proto_err));
                        continue;
                    }
                    let qr: QueryResponse =
                        from_json_value(val).expect("decode NDJSON QueryResponse");
                    items.push(qr);
                }
                NormalizedQueryOutcome::from_items_and_terminal(items, term)
            }
            Self::Stdio { client, .. } => match client.query(req).await {
                Err(err) => NormalizedQueryOutcome::from_items_and_terminal(Vec::new(), Some(err)),
                Ok(mut s) => {
                    let mut items = Vec::new();
                    let mut term = None;
                    while let Some(res) = s.next().await {
                        match res {
                            Ok(item) => items.push(item),
                            Err(err) => {
                                term = Some(err);
                                break;
                            }
                        }
                    }
                    NormalizedQueryOutcome::from_items_and_terminal(items, term)
                }
            },
        }
    }
}

fn make_row(label: &str, seq: i32) -> prost_types::Struct {
    let mut fields = BTreeMap::new();
    fields.insert(
        "label".to_owned(),
        prost_types::Value {
            kind: Some(prost_types::value::Kind::StringValue(label.to_owned())),
        },
    );
    fields.insert(
        "seq".to_owned(),
        prost_types::Value {
            kind: Some(prost_types::value::Kind::NumberValue(f64::from(seq))),
        },
    );
    prost_types::Struct { fields }
}

// spec: NGC-001, NGC-002, NGC-003, NGC-020, NGC-026, NGC-031, NGC-090, NGC-093, NGC-094, NGC-095, NGC-096
#[tokio::test(start_paused = true)]
async fn matrix_describe_query_foreach_and_unimplemented_defaults_are_identical() {
    let mut describe_outcomes = Vec::new();
    let mut query_outcomes = Vec::new();
    let mut foreach_outcomes = Vec::new();
    let mut optional_outcomes = Vec::new();
    let mut dry_run_outcomes = Vec::new();

    for kind in ALL_TRANSPORTS {
        let mock = MockConnector::new()
            .on_describe(|req, _scope| {
                Ok(DescribeResponse {
                    not_modified: false,
                    description: Some(ConnectorDescription {
                        connector_id: format!("echo-{}", req.known_spec_hash),
                        connector_version: "0.1.0".to_owned(),
                        ngc_version: NGC_WIRE_VERSION.to_owned(),
                        protocol: Protocol::Grpc as i32,
                        ..ConnectorDescription::default()
                    }),
                })
            })
            .on_query(|req, scope| {
                if req.foreach.is_empty() {
                    let items = vec![
                        Ok(QueryResponse {
                            chunk: Some(query_response::Chunk::Rows(RowSetChunk {
                                foreach_index: 0,
                                rows: vec![make_row("alpha", 1), make_row("beta", 2)],
                                last_for_index: true,
                                error: None,
                            })),
                        }),
                        Ok(QueryResponse {
                            chunk: Some(query_response::Chunk::Complete(Complete {
                                next_cursor: String::new(),
                                truncated: false,
                                rows_total: 2,
                                upstream_calls: 1,
                                latency_us: 0,
                            })),
                        }),
                    ];
                    return Ok(Box::pin(stream::iter(items)));
                }

                // 4-index foreach scenario: duplicate key, empty index, failing index!
                let run = foreach(
                    req.foreach,
                    ForeachConfig::new(2, 2).with_max_rows_per_chunk(1),
                    &scope,
                    |batch| async move {
                        let out = batch
                            .into_iter()
                            .map(|st| {
                                let mode = st
                                    .fields
                                    .get("mode")
                                    .and_then(|v| match &v.kind {
                                        Some(prost_types::value::Kind::StringValue(s)) => {
                                            Some(s.as_str())
                                        }
                                        _ => None,
                                    })
                                    .unwrap_or("empty");
                                match mode {
                                    "ok" => Ok(vec![make_row("hit", 1), make_row("hit", 2)]),
                                    "fail" => {
                                        Err(NgcErrorInfo::new(NgcErrorCode::NotFound, "no record"))
                                    }
                                    _ => Ok(Vec::new()),
                                }
                            })
                            .collect();
                        Ok(out)
                    },
                )?;
                Ok(run.into_query_stream())
            });

        let core = Arc::new(ServerCore::new(mock, ServerConfig::default()));
        let harness = Harness::new(kind, core);

        let d = harness
            .describe(DescribeRequest {
                known_spec_hash: "v1".to_owned(),
            })
            .await
            .expect("describe ok");
        describe_outcomes.push(d);

        let q = harness
            .query_all(QueryRequest {
                operation: "listItems".to_owned(),
                ..QueryRequest::default()
            })
            .await;
        query_outcomes.push(q);

        // 4 indices: 0="ok", 1="fail", 2="empty", 3="ok" (duplicate of 0 -> 3 distinct keys -> ceil(3/2) == 2 upstream calls!)
        let mode_struct = |m: &str| prost_types::Struct {
            fields: BTreeMap::from([(
                "mode".to_owned(),
                prost_types::Value {
                    kind: Some(prost_types::value::Kind::StringValue(m.to_owned())),
                },
            )]),
        };
        let fq = harness
            .query_all(QueryRequest {
                operation: "batchLookup".to_owned(),
                foreach: vec![
                    VariableSet {
                        index: 0,
                        values: Some(mode_struct("ok")),
                    },
                    VariableSet {
                        index: 1,
                        values: Some(mode_struct("fail")),
                    },
                    VariableSet {
                        index: 2,
                        values: Some(mode_struct("empty")),
                    },
                    VariableSet {
                        index: 3,
                        values: Some(mode_struct("ok")),
                    },
                ],
                ..QueryRequest::default()
            })
            .await;
        assert_eq!(fq.upstream_calls, Some(2));
        assert_eq!(
            fq.closing_counts,
            BTreeMap::from([(0, 1), (1, 1), (2, 1), (3, 1)])
        );
        assert_eq!(fq.rows_by_index[&0].len(), 2);
        assert_eq!(fq.rows_by_index[&3].len(), 2);
        assert!(fq.rows_by_index[&2].is_empty());
        assert_eq!(fq.index_errors[&1].code, NgcErrorCode::NotFound);
        foreach_outcomes.push(fq);

        let opt_err = harness
            .sketch(SketchRequest::default())
            .await
            .expect_err("sketch unimplemented");
        optional_outcomes.push(NormalizedError::from(opt_err));

        let dry_err = harness
            .mutate(MutateRequest {
                dry_run: true,
                ..MutateRequest::default()
            })
            .await
            .expect_err("mutate(dry_run=true) unimplemented");
        assert_eq!(
            dry_err.clone().into_dry_run_gap_error().code(),
            ErrorCode::DryRunUnsupported
        );
        dry_run_outcomes.push(NormalizedError::from(dry_err));
    }

    assert_eq!(describe_outcomes[0], describe_outcomes[1]);
    assert_eq!(describe_outcomes[1], describe_outcomes[2]);
    assert_eq!(query_outcomes[0], query_outcomes[1]);
    assert_eq!(query_outcomes[1], query_outcomes[2]);
    assert_eq!(foreach_outcomes[0], foreach_outcomes[1]);
    assert_eq!(foreach_outcomes[1], foreach_outcomes[2]);
    assert_eq!(optional_outcomes[0], optional_outcomes[1]);
    assert_eq!(optional_outcomes[1], optional_outcomes[2]);
    assert_eq!(dry_run_outcomes[0], dry_run_outcomes[1]);
    assert_eq!(dry_run_outcomes[1], dry_run_outcomes[2]);
}

// spec: NGC-025, NGC-093, INV-12
#[tokio::test(start_paused = true)]
async fn inv_12_matrix_past_deadline_midstream_deadline_and_midstream_cancel() {
    let mut past_outcomes = Vec::new();
    let mut midstream_deadline_outcomes = Vec::new();

    for kind in ALL_TRANSPORTS {
        // 1. Deadline already in the past (`< now_ms` and `== now_ms`): connector MUST NOT be invoked (`total_calls() == 0`).
        let clock = Arc::new(ManualClock::new(1_700_000_000_000));
        let mock_past = MockConnector::new();
        let cfg = ServerConfig::default().with_clock(clock.clone());
        let core_past = Arc::new(ServerCore::new(mock_past.clone(), cfg));
        let harness_past = Harness::new(kind, core_past);

        for expired_deadline in [1_699_999_999_500_i64, 1_700_000_000_000_i64] {
            let past_out = harness_past
                .query_all(QueryRequest {
                    ctx: Some(CallContext {
                        deadline_unix_ms: expired_deadline,
                        ..CallContext::default()
                    }),
                    operation: "neverCalled".to_owned(),
                    ..QueryRequest::default()
                })
                .await;
            assert_eq!(
                mock_past.total_calls(),
                0,
                "connector must not be invoked on {kind:?} when deadline ({expired_deadline}) already passed"
            );
            assert_eq!(
                past_out.terminal_error.as_ref().expect("deadline err").code,
                NgcErrorCode::DeadlineExceeded
            );
            if expired_deadline == 1_700_000_000_000 {
                past_outcomes.push(past_out);
            }
        }

        // 2. Deadline passing mid-stream: emits 1 chunk, stalls, drops connector stream, ends DEADLINE_EXCEEDED.
        let clock_mid = Arc::new(ManualClock::new(1_700_000_000_000));
        let mock_mid = MockConnector::new().on_query(|_req, _scope| {
            let first = stream::iter(vec![Ok(QueryResponse {
                chunk: Some(query_response::Chunk::Rows(RowSetChunk {
                    foreach_index: 0,
                    rows: vec![make_row("before-deadline", 1)],
                    last_for_index: false,
                    error: None,
                })),
            })]);
            let rest = stream::pending::<Result<QueryResponse, NgcErrorInfo>>();
            Ok(Box::pin(first.chain(rest)))
        });
        let core_mid = Arc::new(ServerCore::new(
            mock_mid.clone(),
            ServerConfig::default().with_clock(clock_mid),
        ));
        let harness_mid = Harness::new(kind, core_mid);

        let mid_out = harness_mid
            .query_all(QueryRequest {
                ctx: Some(CallContext {
                    deadline_unix_ms: 1_700_000_000_250, // +250 ms
                    ..CallContext::default()
                }),
                operation: "slowStream".to_owned(),
                ..QueryRequest::default()
            })
            .await;

        assert_eq!(mid_out.rows_by_index.get(&0).map(Vec::len), Some(1));
        tokio::time::timeout(Duration::from_secs(5), mock_mid.wait_for_drops(1))
            .await
            .expect("mid-stream deadline must drop connector stream");
        assert_eq!(
            mid_out
                .terminal_error
                .as_ref()
                .expect("midstream deadline")
                .code,
            NgcErrorCode::DeadlineExceeded
        );
        midstream_deadline_outcomes.push(mid_out);

        // 3. Client cancel mid-stream: read first chunk, drop/cancel client stream, observe connector stream drop!
        let mock_cancel = MockConnector::new().on_query(|_req, _scope| {
            let first = stream::iter(vec![Ok(QueryResponse {
                chunk: Some(query_response::Chunk::Rows(RowSetChunk {
                    foreach_index: 0,
                    rows: vec![make_row("first", 1)],
                    last_for_index: false,
                    error: None,
                })),
            })]);
            Ok(Box::pin(first.chain(stream::pending())))
        });
        let core_cancel = Arc::new(ServerCore::new(
            mock_cancel.clone(),
            ServerConfig::default(),
        ));
        let harness_cancel = Harness::new(kind, core_cancel);

        match &harness_cancel {
            Harness::Grpc { client, .. } => {
                let mut s = client
                    .clone()
                    .query(QueryRequest::default())
                    .await
                    .expect("open grpc stream");
                let first = s.next().await.expect("item 1").expect("ok 1");
                assert!(first.chunk.is_some());
                drop(s);
            }
            Harness::Http { router } => {
                let req = Request::builder()
                    .method(Method::POST)
                    .uri("/gap/ngc/v1/Query")
                    .header(http::header::CONTENT_TYPE, "application/json")
                    .body(Body::from("{}"))
                    .expect("http req");
                let resp = router.clone().oneshot(req).await.expect("http resp");
                let mut body = resp.into_body().into_data_stream();
                let first_frame = body.next().await.expect("frame 1").expect("bytes 1");
                assert!(!first_frame.is_empty());
                drop(body);
            }
            Harness::Stdio { client, .. } => {
                let mut s = client
                    .query(QueryRequest::default())
                    .await
                    .expect("open stdio stream");
                let first = s.next().await.expect("item 1").expect("ok 1");
                assert!(first.chunk.is_some());
                drop(s); // StdioStream::drop sends gap.ngc.cancel notification!
            }
        }
        tokio::time::timeout(Duration::from_secs(5), mock_cancel.wait_for_drops(1))
            .await
            .expect("client cancel must drop connector stream");
        assert_eq!(mock_cancel.in_flight(), 0);
    }

    assert_eq!(
        past_outcomes[0].terminal_error,
        past_outcomes[1].terminal_error
    );
    assert_eq!(
        past_outcomes[1].terminal_error,
        past_outcomes[2].terminal_error
    );
    assert_eq!(
        midstream_deadline_outcomes[0],
        midstream_deadline_outcomes[1]
    );
    assert_eq!(
        midstream_deadline_outcomes[1],
        midstream_deadline_outcomes[2]
    );
}

// spec: NGC-060, NGC-061, NGC-062, NGC-063, NGC-071, NGC-093, NGC-094, NGC-095, INV-15
#[tokio::test(start_paused = true)]
async fn matrix_saturation_panic_oversized_all_14_codes_and_error_clamping() {
    for kind in ALL_TRANSPORTS {
        // 1. `max_in_flight` saturation (`max_in_flight = 1`): immediate retryable RESOURCE_EXHAUSTED, no queueing,
        //    and permit released once first call finishes!
        let entered = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let entered_c = Arc::clone(&entered);
        let release_c = Arc::clone(&release);

        let mock_sat = MockConnector::new().on_describe_async(move |req, _scope| {
            let entered = Arc::clone(&entered_c);
            let release = Arc::clone(&release_c);
            async move {
                if req.known_spec_hash != "after" {
                    entered.notify_one();
                    release.notified().await;
                }
                Ok(DescribeResponse {
                    not_modified: true,
                    description: None,
                })
            }
        });
        let core_sat = Arc::new(ServerCore::new(
            mock_sat.clone(),
            ServerConfig::default()
                .with_max_in_flight(1)
                .with_saturated_retry_after_ms(350),
        ));
        let harness_sat = Arc::new(Harness::new(kind, core_sat));

        let h_first = Arc::clone(&harness_sat);
        let first_task =
            tokio::spawn(async move { h_first.describe(DescribeRequest::default()).await });
        entered.notified().await;

        let t0 = tokio::time::Instant::now();
        let sat_err = harness_sat
            .describe(DescribeRequest::default())
            .await
            .expect_err("saturated call must fail immediately");
        assert_eq!(
            tokio::time::Instant::now().duration_since(t0),
            Duration::ZERO,
            "saturated call must never queue on {kind:?}"
        );
        assert_eq!(sat_err.code, NgcErrorCode::ResourceExhausted);
        assert!(sat_err.retryable);
        assert_eq!(sat_err.retry_after_ms, 350);

        // Release first call and verify permit returns to pool so next call succeeds!
        release.notify_one();
        assert!(
            first_task
                .await
                .expect("join first")
                .expect("first ok")
                .not_modified
        );
        let after_release = harness_sat
            .describe(DescribeRequest {
                known_spec_hash: "after".to_owned(),
            })
            .await
            .expect("permit must return to pool after first call completes");
        assert!(after_release.not_modified);
    }

    // Dedicated permit-release check (`max_in_flight = 1`, 3 sequential calls MUST all succeed!)
    for kind in ALL_TRANSPORTS {
        let mock_seq = MockConnector::new();
        let core_seq = Arc::new(ServerCore::new(
            mock_seq,
            ServerConfig::default().with_max_in_flight(1),
        ));
        let harness_seq = Harness::new(kind, core_seq);
        for _ in 0..3 {
            let d = harness_seq
                .describe(DescribeRequest::default())
                .await
                .expect("sequential call succeeds when permit is released");
            assert!(!d.not_modified);
        }
    }

    // 2. Connector panic (sync + async + mid-stream) -> INTERNAL with fixed message, no panic text, server keeps serving.
    let mut panic_outcomes = Vec::new();
    for kind in ALL_TRANSPORTS {
        let mock_panic = MockConnector::new()
            .on_describe_async(|req, _scope| {
                if req.known_spec_hash == "boom-sync" {
                    panic!("secret-sync-panic-payload-999");
                }
                async move {
                    if req.known_spec_hash == "boom-async" {
                        tokio::task::yield_now().await;
                        panic!("secret-async-panic-payload-888");
                    }
                    Ok(DescribeResponse {
                        not_modified: true,
                        description: None,
                    })
                }
            })
            .on_query(|req, _scope| {
                if req.operation == "boom-stream" {
                    let s = stream::unfold(0_u8, |step| async move {
                        if step == 0 {
                            Some((
                                Ok(QueryResponse {
                                    chunk: Some(query_response::Chunk::Rows(RowSetChunk {
                                        foreach_index: 0,
                                        rows: vec![make_row("before-panic", 1)],
                                        last_for_index: false,
                                        error: None,
                                    })),
                                }),
                                1,
                            ))
                        } else {
                            panic!("secret-stream-panic-payload-777");
                        }
                    });
                    return Ok(Box::pin(s));
                }
                Ok(Box::pin(stream::empty()))
            });
        let core_panic = Arc::new(ServerCore::new(mock_panic, ServerConfig::default()));
        let harness_panic = Harness::new(kind, core_panic);

        for trigger in ["boom-sync", "boom-async"] {
            let err = harness_panic
                .describe(DescribeRequest {
                    known_spec_hash: trigger.to_owned(),
                })
                .await
                .expect_err("panic caught");
            assert_eq!(err.code, NgcErrorCode::Internal);
            assert_eq!(err.message(), CONNECTOR_PANIC_MESSAGE);
            assert!(!err.message().contains("secret-"));
            if trigger == "boom-sync" {
                panic_outcomes.push(NormalizedError::from(err));
            }
        }

        let stream_panic_out = harness_panic
            .query_all(QueryRequest {
                operation: "boom-stream".to_owned(),
                ..QueryRequest::default()
            })
            .await;
        assert_eq!(stream_panic_out.rows_by_index[&0].len(), 1);
        let term = stream_panic_out
            .terminal_error
            .as_ref()
            .expect("mid-stream panic terminal error");
        assert_eq!(term.code, NgcErrorCode::Internal);
        assert_eq!(term.message, CONNECTOR_PANIC_MESSAGE);

        // Subsequent call on the exact same server succeeds!
        let ok = harness_panic
            .describe(DescribeRequest {
                known_spec_hash: "healthy".to_owned(),
            })
            .await
            .expect("server keeps serving after panic");
        assert!(ok.not_modified);
    }
    assert_eq!(panic_outcomes[0], panic_outcomes[1]);
    assert_eq!(panic_outcomes[1], panic_outcomes[2]);

    // 3. Oversized request (`max_request_bytes = 512`, payload = 4_096 bytes) rejected without invoking connector.
    for kind in ALL_TRANSPORTS {
        let mock_big = MockConnector::new();
        let core_big = Arc::new(ServerCore::new(
            mock_big.clone(),
            ServerConfig::default().with_max_request_bytes(512),
        ));
        let harness_big = Harness::new(kind, core_big);
        let big_err = harness_big
            .describe(DescribeRequest {
                known_spec_hash: "x".repeat(4_096),
            })
            .await
            .expect_err("oversized request must fail");
        assert_eq!(big_err.code, NgcErrorCode::InvalidArgument);
        assert_eq!(mock_big.total_calls(), 0);
    }

    // 4. All 14 ErrorCode values + 10,000-char message clamping + >4 KiB details dropping.
    let mut per_transport_14: Vec<Vec<NormalizedError>> = Vec::new();
    let mut clamped_outcomes: Vec<NormalizedError> = Vec::new();

    for kind in ALL_TRANSPORTS {
        let mock_codes = MockConnector::new()
            .on_describe(|req, _scope| {
                if req.known_spec_hash == "clamp-check" {
                    let mut huge = serde_json::Map::new();
                    huge.insert(
                        "secret_dump".to_owned(),
                        serde_json::json!("z".repeat(6_000)),
                    );
                    return Err(
                        NgcErrorInfo::new(NgcErrorCode::UpstreamError, "e".repeat(10_000))
                            .with_retry(true, 900)
                            .with_details(huge),
                    );
                }
                let tag: i32 = req.known_spec_hash.parse().unwrap_or(0);
                let code = NgcErrorCode::try_from(tag).unwrap_or(NgcErrorCode::Unspecified);
                let mut small = serde_json::Map::new();
                small.insert("tag".to_owned(), serde_json::json!(f64::from(tag)));
                Err(NgcErrorInfo::new(code, format!("err-{tag}"))
                    .with_retry(tag % 2 == 1, 200)
                    .with_details(small))
            })
            .on_query(|_req, _scope| {
                // Embedded RowSetChunk.error with 10,000-char message and >4 KiB details
                let mut huge_fields = BTreeMap::new();
                huge_fields.insert(
                    "oversized".to_owned(),
                    prost_types::Value {
                        kind: Some(prost_types::value::Kind::StringValue("y".repeat(6_000))),
                    },
                );
                let items = vec![Ok(QueryResponse {
                    chunk: Some(query_response::Chunk::Rows(RowSetChunk {
                        foreach_index: 0,
                        rows: Vec::new(),
                        last_for_index: true,
                        error: Some(NgcError {
                            code: i32::from(NgcErrorCode::FailedPrecondition),
                            message: "q".repeat(10_000),
                            retryable: false,
                            retry_after_ms: 0,
                            details: Some(prost_types::Struct {
                                fields: huge_fields,
                            }),
                        }),
                    })),
                })];
                Ok(Box::pin(stream::iter(items)))
            });

        let core_codes = Arc::new(ServerCore::new(mock_codes, ServerConfig::default()));
        let harness_codes = Harness::new(kind, core_codes);

        let mut mapped = Vec::new();
        for code in ALL_NGC_CODES {
            let tag = i32::from(code);
            let err = harness_codes
                .describe(DescribeRequest {
                    known_spec_hash: tag.to_string(),
                })
                .await
                .expect_err("expected error");
            assert_eq!(err.code, code);
            mapped.push(NormalizedError::from(err));
        }
        per_transport_14.push(mapped);

        let clamped = harness_codes
            .describe(DescribeRequest {
                known_spec_hash: "clamp-check".to_owned(),
            })
            .await
            .expect_err("expected clamped error");
        assert_eq!(clamped.message().chars().count(), MAX_ERROR_MESSAGE_CHARS);
        assert!(
            clamped.details.is_none(),
            "details > 4 KiB must be dropped on {kind:?}"
        );
        clamped_outcomes.push(NormalizedError::from(clamped));

        let embedded_q = harness_codes.query_all(QueryRequest::default()).await;
        let idx_err = &embedded_q.index_errors[&0];
        assert_eq!(idx_err.message.chars().count(), MAX_ERROR_MESSAGE_CHARS);
        assert!(
            !idx_err.has_details,
            "embedded RowSetChunk.error details > 4 KiB must be dropped"
        );
    }

    assert_eq!(per_transport_14[0], per_transport_14[1]);
    assert_eq!(per_transport_14[1], per_transport_14[2]);
    assert_eq!(clamped_outcomes[0], clamped_outcomes[1]);
    assert_eq!(clamped_outcomes[1], clamped_outcomes[2]);
}

// spec: NGC-001, NGC-002, NGC-003, NGC-020, NGC-093, NGC-094, NGC-095
#[tokio::test(start_paused = true)]
async fn matrix_nan_stream_element_fails_closed_as_internal_without_placeholder() {
    fn nan_struct() -> prost_types::Struct {
        let mut fields = BTreeMap::new();
        fields.insert(
            "metric".to_owned(),
            prost_types::Value {
                kind: Some(prost_types::value::Kind::NumberValue(f64::NAN)),
            },
        );
        prost_types::Struct { fields }
    }

    for kind in ALL_TRANSPORTS {
        let mock = MockConnector::new()
            .on_query(|_req, _scope| {
                let items = vec![
                    Ok(QueryResponse {
                        chunk: Some(query_response::Chunk::Rows(RowSetChunk {
                            foreach_index: 0,
                            rows: vec![nan_struct()],
                            last_for_index: true,
                            error: None,
                        })),
                    }),
                    Ok(QueryResponse {
                        chunk: Some(query_response::Chunk::Complete(Complete {
                            next_cursor: String::new(),
                            truncated: false,
                            rows_total: 1,
                            upstream_calls: 1,
                            latency_us: 5,
                        })),
                    }),
                ];
                Ok(Box::pin(stream::iter(items)))
            })
            .on_subscribe(|_req, _scope| {
                let items = vec![
                    Ok(Event {
                        id: "ev-nan".to_owned(),
                        at: None,
                        payload: Some(nan_struct()),
                        resume_token: "r1".to_owned(),
                    }),
                    Ok(Event {
                        id: "ev-after-nan".to_owned(),
                        at: None,
                        payload: Some(make_row("ok", 1)),
                        resume_token: "r2".to_owned(),
                    }),
                ];
                Ok(Box::pin(stream::iter(items)))
            });

        let core = Arc::new(ServerCore::new(mock.clone(), ServerConfig::default()));
        let harness = Harness::new(kind, core);

        match &harness {
            Harness::Grpc { client, .. } => {
                // Binary protobuf carries IEEE-754 NaN intact on both Query and Subscribe.
                let mut qs = client
                    .clone()
                    .query(QueryRequest::default())
                    .await
                    .expect("gRPC Query open");
                let first = qs.next().await.expect("chunk 0").expect("chunk 0 ok");
                let Some(query_response::Chunk::Rows(rc)) = first.chunk else {
                    panic!("expected Rows chunk on gRPC");
                };
                assert!(rc.last_for_index);
                let Some(prost_types::value::Kind::NumberValue(v)) =
                    rc.rows[0].fields["metric"].kind
                else {
                    panic!("expected NumberValue");
                };
                assert!(v.is_nan(), "gRPC must preserve NaN row value");
                let second = qs.next().await.expect("complete").expect("complete ok");
                assert!(matches!(
                    second.chunk,
                    Some(query_response::Chunk::Complete(_))
                ));
                assert!(qs.next().await.is_none());

                let mut ss = client
                    .clone()
                    .subscribe(SubscribeRequest::default())
                    .await
                    .expect("gRPC Subscribe open");
                let ev1 = ss.next().await.expect("ev1").expect("ev1 ok");
                assert_eq!(ev1.id, "ev-nan");
                let ev2 = ss.next().await.expect("ev2").expect("ev2 ok");
                assert_eq!(ev2.id, "ev-after-nan");
                assert!(ss.next().await.is_none());
            }
            Harness::Http { router } => {
                // 1. HTTP NDJSON (`Query`): exact 1 line `{"error": <NgcError INTERNAL>}`, no `{}`, nothing after!
                let q_resp = router
                    .clone()
                    .oneshot(
                        Request::builder()
                            .method(Method::POST)
                            .uri("/gap/ngc/v1/Query")
                            .header(http::header::CONTENT_TYPE, "application/json")
                            .body(Body::from("{}"))
                            .expect("req"),
                    )
                    .await
                    .expect("HTTP Query oneshot");
                assert_eq!(q_resp.status(), StatusCode::OK);
                let raw_ndjson = String::from_utf8(
                    q_resp
                        .into_body()
                        .collect()
                        .await
                        .expect("collect NDJSON")
                        .to_bytes()
                        .to_vec(),
                )
                .expect("utf8");
                assert!(
                    !raw_ndjson.contains("{}"),
                    "HTTP NDJSON must never emit `{{}}` placeholder, got: `{raw_ndjson}`"
                );
                let nd_lines: Vec<&str> = raw_ndjson.lines().collect();
                assert_eq!(
                    nd_lines.len(),
                    1,
                    "HTTP NDJSON must end immediately with 1 error line and nothing after, got: `{raw_ndjson}`"
                );
                let err_line: serde_json::Value =
                    serde_json::from_str(nd_lines[0]).expect("NDJSON error JSON");
                let proto_err: NgcError =
                    from_json_value(err_line["error"].clone()).expect("NgcError JSON");
                let info = ngc_error_from_proto(&proto_err);
                assert_eq!(info.code, NgcErrorCode::Internal);

                // 2. HTTP SSE (`Subscribe`): exact 1 `event: error` frame with INTERNAL and nothing after!
                let s_resp = router
                    .clone()
                    .oneshot(
                        Request::builder()
                            .method(Method::POST)
                            .uri("/gap/ngc/v1/Subscribe")
                            .header(http::header::CONTENT_TYPE, "application/json")
                            .body(Body::from("{}"))
                            .expect("req"),
                    )
                    .await
                    .expect("HTTP Subscribe oneshot");
                assert_eq!(s_resp.status(), StatusCode::OK);
                let raw_sse = String::from_utf8(
                    s_resp
                        .into_body()
                        .collect()
                        .await
                        .expect("collect SSE")
                        .to_bytes()
                        .to_vec(),
                )
                .expect("utf8");
                assert!(
                    !raw_sse.contains("{}"),
                    "HTTP SSE must never emit `{{}}` placeholder, got: `{raw_sse}`"
                );
                assert!(
                    !raw_sse.contains("ev-after-nan"),
                    "HTTP SSE must emit nothing after unrenderable element, got: `{raw_sse}`"
                );
                let blocks: Vec<&str> = raw_sse
                    .split("\n\n")
                    .filter(|b| !b.trim().is_empty())
                    .collect();
                assert_eq!(
                    blocks.len(),
                    1,
                    "HTTP SSE must contain exact 1 error event block, got: `{raw_sse}`"
                );
                let data_json = blocks[0]
                    .strip_prefix("event: error\ndata: ")
                    .expect("SSE block must start with `event: error\\ndata: `");
                let sse_proto_err: NgcError =
                    from_json_slice(data_json.as_bytes()).expect("SSE NgcError JSON");
                assert_eq!(
                    ngc_error_from_proto(&sse_proto_err).code,
                    NgcErrorCode::Internal
                );
            }
            Harness::Stdio { client, .. } => {
                // 1. stdio `Query`: ends with exact 1 INTERNAL error element and no rows/placeholders.
                let mut qs = client
                    .query(QueryRequest::default())
                    .await
                    .expect("stdio Query open");
                let q_err = qs
                    .next()
                    .await
                    .expect("stdio Query terminal item")
                    .expect_err("stdio Query must fail with INTERNAL on NaN");
                assert_eq!(q_err.code, NgcErrorCode::Internal);
                assert!(qs.next().await.is_none());

                // 2. stdio `Subscribe`: ends with exact 1 INTERNAL error element and nothing after.
                let mut ss = client
                    .subscribe(SubscribeRequest::default())
                    .await
                    .expect("stdio Subscribe open");
                let s_err = ss
                    .next()
                    .await
                    .expect("stdio Subscribe terminal item")
                    .expect_err("stdio Subscribe must fail with INTERNAL on NaN");
                assert_eq!(s_err.code, NgcErrorCode::Internal);
                assert!(ss.next().await.is_none());
            }
        }
        assert_eq!(mock.in_flight(), 0);
    }
}

// spec: NGC-070, NGC-071, NGC-093
#[tokio::test(start_paused = true)]
async fn matrix_health_bypasses_max_in_flight_saturation_across_all_transports() {
    for kind in ALL_TRANSPORTS {
        let stream_entered = Arc::new(Notify::new());
        let stream_release = Arc::new(Notify::new());
        let entered_c = Arc::clone(&stream_entered);
        let release_c = Arc::clone(&stream_release);

        let mock = MockConnector::new().on_query(move |_req, _scope| {
            let entered = Arc::clone(&entered_c);
            let release = Arc::clone(&release_c);
            let s = stream::unfold(0_u8, move |step| {
                let entered = Arc::clone(&entered);
                let release = Arc::clone(&release);
                async move {
                    match step {
                        0 => {
                            entered.notify_one();
                            Some((
                                Ok(QueryResponse {
                                    chunk: Some(query_response::Chunk::Rows(RowSetChunk {
                                        foreach_index: 0,
                                        rows: Vec::new(),
                                        last_for_index: false,
                                        error: None,
                                    })),
                                }),
                                1,
                            ))
                        }
                        1 => {
                            release.notified().await;
                            Some((
                                Ok(QueryResponse {
                                    chunk: Some(query_response::Chunk::Rows(RowSetChunk {
                                        foreach_index: 0,
                                        rows: Vec::new(),
                                        last_for_index: true,
                                        error: None,
                                    })),
                                }),
                                2,
                            ))
                        }
                        _ => None,
                    }
                }
            });
            Ok(Box::pin(s))
        });

        let core = Arc::new(ServerCore::new(
            mock.clone(),
            ServerConfig::default()
                .with_max_in_flight(2)
                .with_saturated_retry_after_ms(250),
        ));
        let harness = Harness::new(kind, Arc::clone(&core));

        match &harness {
            Harness::Grpc { client, .. } => {
                let mut s1 = client
                    .clone()
                    .query(QueryRequest::default())
                    .await
                    .expect("open gRPC stream 1");
                let mut s2 = client
                    .clone()
                    .query(QueryRequest::default())
                    .await
                    .expect("open gRPC stream 2");
                assert!(s1.next().await.expect("s1 item 0").is_ok());
                assert!(s2.next().await.expect("s2 item 0").is_ok());
                assert_eq!(mock.in_flight(), 2);
                assert_eq!(core.in_flight_count(), 2);

                // Saturated: Health MUST still answer SERVING while a 3rd Query gets RESOURCE_EXHAUSTED!
                let health = harness
                    .health(HealthRequest {})
                    .await
                    .expect("gRPC Health must bypass max_in_flight saturation");
                assert_eq!(health.status, 1);

                let t0 = tokio::time::Instant::now();
                let sat_out = harness.query_all(QueryRequest::default()).await;
                assert_eq!(
                    tokio::time::Instant::now().duration_since(t0),
                    Duration::ZERO
                );
                let sat_err = sat_out
                    .terminal_error
                    .expect("3rd gRPC Query must fail with RESOURCE_EXHAUSTED");
                assert_eq!(sat_err.code, NgcErrorCode::ResourceExhausted);
                assert!(sat_err.retryable);
                assert_eq!(sat_err.retry_after_ms, 250);
                assert_eq!(mock.calls_for(RpcMethod::Query), 2);

                stream_release.notify_waiters();
                while let Some(res) = s1.next().await {
                    res.expect("s1 finish");
                }
                while let Some(res) = s2.next().await {
                    res.expect("s2 finish");
                }
            }
            Harness::Http { router } => {
                let make_query_req = || {
                    Request::builder()
                        .method(Method::POST)
                        .uri("/gap/ngc/v1/Query")
                        .header(http::header::CONTENT_TYPE, "application/json")
                        .body(Body::from("{}"))
                        .expect("HTTP Query req")
                };
                let r1 = router
                    .clone()
                    .oneshot(make_query_req())
                    .await
                    .expect("open HTTP stream 1");
                let r2 = router
                    .clone()
                    .oneshot(make_query_req())
                    .await
                    .expect("open HTTP stream 2");
                assert_eq!(r1.status(), StatusCode::OK);
                assert_eq!(r2.status(), StatusCode::OK);
                let mut b1 = r1.into_body();
                let mut b2 = r2.into_body();
                assert!(b1.frame().await.expect("b1 frame 0").is_ok());
                assert!(b2.frame().await.expect("b2 frame 0").is_ok());
                assert_eq!(mock.in_flight(), 2);
                assert_eq!(core.in_flight_count(), 2);

                // Saturated: Health MUST still answer SERVING while a 3rd Query gets RESOURCE_EXHAUSTED!
                let health = harness
                    .health(HealthRequest {})
                    .await
                    .expect("HTTP Health must bypass max_in_flight saturation");
                assert_eq!(health.status, 1);

                let t0 = tokio::time::Instant::now();
                let sat_out = harness.query_all(QueryRequest::default()).await;
                assert_eq!(
                    tokio::time::Instant::now().duration_since(t0),
                    Duration::ZERO
                );
                let sat_err = sat_out
                    .terminal_error
                    .expect("3rd HTTP Query must fail with RESOURCE_EXHAUSTED");
                assert_eq!(sat_err.code, NgcErrorCode::ResourceExhausted);
                assert!(sat_err.retryable);
                assert_eq!(sat_err.retry_after_ms, 250);
                assert_eq!(mock.calls_for(RpcMethod::Query), 2);

                stream_release.notify_waiters();
                let _ = b1.collect().await.expect("b1 finish");
                let _ = b2.collect().await.expect("b2 finish");
            }
            Harness::Stdio { client, .. } => {
                let mut s1 = client
                    .query(QueryRequest::default())
                    .await
                    .expect("open stdio stream 1");
                let mut s2 = client
                    .query(QueryRequest::default())
                    .await
                    .expect("open stdio stream 2");
                assert!(s1.next().await.expect("s1 item 0").is_ok());
                assert!(s2.next().await.expect("s2 item 0").is_ok());
                assert_eq!(mock.in_flight(), 2);
                assert_eq!(core.in_flight_count(), 2);

                // Saturated: Health MUST still answer SERVING while a 3rd Query gets RESOURCE_EXHAUSTED!
                let health = harness
                    .health(HealthRequest {})
                    .await
                    .expect("stdio Health must bypass max_in_flight saturation");
                assert_eq!(health.status, 1);

                let t0 = tokio::time::Instant::now();
                let sat_out = harness.query_all(QueryRequest::default()).await;
                assert_eq!(
                    tokio::time::Instant::now().duration_since(t0),
                    Duration::ZERO
                );
                let sat_err = sat_out
                    .terminal_error
                    .expect("3rd stdio Query must fail with RESOURCE_EXHAUSTED");
                assert_eq!(sat_err.code, NgcErrorCode::ResourceExhausted);
                assert!(sat_err.retryable);
                assert_eq!(sat_err.retry_after_ms, 250);
                assert_eq!(mock.calls_for(RpcMethod::Query), 2);

                stream_release.notify_waiters();
                while let Some(res) = s1.next().await {
                    res.expect("s1 finish");
                }
                while let Some(res) = s2.next().await {
                    res.expect("s2 finish");
                }
            }
        }

        assert_eq!(
            mock.in_flight(),
            0,
            "connector in-flight count must return to zero on {kind:?}"
        );
        assert_eq!(
            core.in_flight_count(),
            0,
            "ServerCore in-flight permit count must return to zero on {kind:?}"
        );
        // Verify all permits returned to ServerCore by running a fresh Describe call.
        let desc = harness
            .describe(DescribeRequest::default())
            .await
            .expect("Describe after closing streams");
        assert!(!desc.not_modified);

        // Even with max_in_flight = 0 (permanently saturated), Health still enforces its 500 ms bound and panic isolation.
        let call_idx = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let idx_c = Arc::clone(&call_idx);
        let mock_guard = MockConnector::new().on_health_async(move |_req, _scope| {
            let n = idx_c.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            assert!(n != 0, "secret-health-panic-when-saturated");
            async move {
                tokio::time::sleep(Duration::from_secs(5)).await;
                Ok(HealthResponse {
                    status: 1,
                    ..HealthResponse::default()
                })
            }
        });
        let core_zero = Arc::new(ServerCore::new(
            mock_guard,
            ServerConfig::default().with_max_in_flight(0),
        ));
        let h_zero = Harness::new(kind, core_zero);
        let p_err = h_zero
            .health(HealthRequest {})
            .await
            .expect_err("panicking Health under saturation must return INTERNAL");
        assert_eq!(p_err.code, NgcErrorCode::Internal);
        assert_eq!(p_err.message(), CONNECTOR_PANIC_MESSAGE);

        let t_start = tokio::time::Instant::now();
        let d_err = h_zero
            .health(HealthRequest {})
            .await
            .expect_err("slow Health under saturation must still time out at 500 ms");
        assert_eq!(d_err.code, NgcErrorCode::DeadlineExceeded);
        assert_eq!(
            tokio::time::Instant::now().duration_since(t_start),
            Duration::from_millis(500)
        );
    }
}
