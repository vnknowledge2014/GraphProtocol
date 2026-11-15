//! Two-GAP-node mutual delegation loop detection and A2A v1.0 SSE wire integration tests (`CMP-021`, `CMP-022`, `CMP-023`, `INV-20`).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};

use gap_a2a::{
    A2A_PROTOCOL_VERSION, A2A_VERSION_HEADER, A2aFacade, A2aFacadeConfig, A2aOutboundEnvelope,
    A2aPeerConfig, A2aPeerConnector, A2aPeerTransport, A2aStreamEvent, A2aTask, A2aTaskState,
    AgentCard, AgentCardConfig, AgentInterface, AgentProtocolBinding, GAP_TRACE_EXT_URI,
    InMemoryA2aBackend, SCOPE_HOP_HEADER, SCOPE_TRACE_PATH_HEADER, TraceExtension,
    build_agent_card,
};
use gap_core::clock::ManualClock;
use gap_core::context::{Budget, Principal, PrincipalKind, RequestContext, TraceCtx};
use gap_core::error::{ErrorCode, GapResult, NgcErrorCode, NgcErrorInfo};
use gap_core::ids::{BranchUid, TenantId};
use gap_core::snapshot::{BranchStatus, Protocol, Snapshot, TrustTier};
use gap_core::traits::{
    BoxFuture, DiscoveryCandidate, DiscoveryOutcome, DiscoveryRequest, DiscoveryResolution,
    DiscoveryService, ElementKind, HttpExchangeRequest, HttpModule,
};
use gap_ngc_sdk::connector::{CallScope, Connector, NgcResult};
use gap_proto::ngc::v1::{
    CallContext, CancelTaskRequest, StartTaskRequest, TaskState as ProtoTaskState,
};
use gap_sec::json::JsonValue;
use gap_testkit::snapshot::{BranchDescriptorBuilder, SnapshotBuilder};

fn block_on<F: Future>(fut: F) -> F::Output {
    let mut cx = Context::from_waker(Waker::noop());
    let mut pinned = std::pin::pin!(fut);
    loop {
        match Pin::as_mut(&mut pinned).poll(&mut cx) {
            Poll::Ready(val) => return val,
            Poll::Pending => std::thread::yield_now(),
        }
    }
}

#[derive(Debug)]
struct StaticDiscovery;

impl DiscoveryService for StaticDiscovery {
    fn resolve<'a>(
        &'a self,
        _ctx: &'a RequestContext,
        req: DiscoveryRequest,
    ) -> BoxFuture<'a, GapResult<DiscoveryResolution>> {
        Box::pin(async move {
            Ok(DiscoveryResolution {
                outcome: DiscoveryOutcome::Resolved,
                candidates: vec![DiscoveryCandidate {
                    element_id: "orders.get".to_owned(),
                    kind: ElementKind::Operation,
                    score: 0.99,
                    synopsis: req.intent,
                    sdl: None,
                }],
                tokens_estimate: 16,
                router: "baseline".to_owned(),
            })
        })
    }
}

fn peer_snapshot(tenant_slug: &str) -> Arc<Snapshot> {
    let branch = BranchDescriptorBuilder::new("catalog")
        .protocol(Protocol::Rest)
        .trust(TrustTier::T0)
        .synopsis("Product catalog")
        .status(BranchStatus::Active);
    Arc::new(
        SnapshotBuilder::new()
            .generation(3)
            .tenant_slug(tenant_slug)
            .with_branch(branch)
            .build()
            .expect("valid snapshot"),
    )
}

fn peer_ctx(
    snap: Arc<Snapshot>,
    tenant_slug: &str,
    hop: u8,
    trace_path: Vec<String>,
) -> RequestContext {
    let tenant = TenantId::new(tenant_slug).expect("tenant");
    let principal = Principal::new("peer-agent", PrincipalKind::Agent, tenant.clone());
    let mut trace = TraceCtx::generate_root(false);
    trace.path = trace_path;
    RequestContext::try_new(
        BranchUid::from_unix_ms(1_700_000_000_001, [7; 10]).as_uuid(),
        tenant,
        principal,
        Instant::now() + Duration::from_secs(30),
        Budget::new(10_000),
        trace,
        hop,
        None,
        snap,
    )
    .expect("RequestContext")
}

/// Bridges an outbound `A2aPeerConnector` call directly onto an inbound `A2aFacade::handle`.
#[derive(Debug)]
struct FacadeLoopTransport {
    peer_card: AgentCard,
    target_facade: Arc<A2aFacade<InMemoryA2aBackend, StaticDiscovery>>,
    target_snap: Arc<Snapshot>,
    recorded_hops: Mutex<Vec<TraceExtension>>,
}

impl A2aPeerTransport for FacadeLoopTransport {
    fn fetch_agent_card<'a>(&'a self, _url: &'a str) -> BoxFuture<'a, NgcResult<AgentCard>> {
        let card = self.peer_card.clone();
        Box::pin(async move { Ok(card) })
    }

    fn send_message<'a>(
        &'a self,
        envelope: &'a A2aOutboundEnvelope,
    ) -> BoxFuture<'a, NgcResult<A2aTask>> {
        Box::pin(async move {
            let mut headers: Vec<(String, String)> = envelope
                .headers
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            headers.push(("content-type".to_owned(), "application/a2a+json".to_owned()));

            let ctx = peer_ctx(self.target_snap.clone(), "acme", 0, Vec::new());
            let resp = self
                .target_facade
                .handle(
                    &ctx,
                    HttpExchangeRequest {
                        method: "POST".to_owned(),
                        path: "/message:send".to_owned(),
                        query: None,
                        headers,
                        body: envelope.body.to_jcs_string().into_bytes(),
                    },
                )
                .await
                .map_err(|e| NgcErrorInfo::new(NgcErrorCode::Internal, e.message()))?;

            let json_text = String::from_utf8(resp.body)
                .map_err(|_| NgcErrorInfo::new(NgcErrorCode::Internal, "non-utf8 A2A response"))?;
            let val = JsonValue::parse(&json_text)
                .map_err(|e| NgcErrorInfo::new(NgcErrorCode::Internal, e.message()))?;

            if resp.status != 200 {
                let reason = val
                    .get("details")
                    .and_then(|d| d.as_array())
                    .and_then(|arr| arr.first())
                    .and_then(|item| item.get_str("reason"))
                    .unwrap_or("GAP_BRANCH_ERROR");
                let msg = val.get_str("message").unwrap_or("A2A peer error");
                let code = if reason == ErrorCode::LoopDetected.to_string()
                    || reason == ErrorCode::HopLimit.to_string()
                {
                    NgcErrorCode::FailedPrecondition
                } else {
                    NgcErrorCode::UpstreamError
                };
                return Err(NgcErrorInfo::new(code, format!("{reason}: {msg}")));
            }

            let task_val = val.get("task").unwrap_or(&val);
            let task = A2aTask::from_json_value(task_val)
                .map_err(|e| NgcErrorInfo::new(NgcErrorCode::Internal, e.message()))?;
            if let Ok(Some(trace)) = TraceExtension::from_metadata(&task.metadata)
                && let Ok(mut g) = self.recorded_hops.lock()
            {
                g.push(trace);
            }
            Ok(task)
        })
    }

    fn get_task<'a>(
        &'a self,
        _target_url: &'a str,
        _interface_tenant: Option<&'a str>,
        headers: &'a BTreeMap<String, String>,
        task_id: &'a str,
    ) -> BoxFuture<'a, NgcResult<A2aTask>> {
        Box::pin(async move {
            let ctx = peer_ctx(self.target_snap.clone(), "acme", 0, Vec::new());
            let hdrs = headers
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            let resp = self
                .target_facade
                .handle(
                    &ctx,
                    HttpExchangeRequest {
                        method: "GET".to_owned(),
                        path: format!("/tasks/{task_id}"),
                        query: None,
                        headers: hdrs,
                        body: Vec::new(),
                    },
                )
                .await
                .map_err(|e| NgcErrorInfo::new(NgcErrorCode::Internal, e.message()))?;
            let val = JsonValue::parse(&String::from_utf8_lossy(&resp.body))
                .map_err(|e| NgcErrorInfo::new(NgcErrorCode::Internal, e.message()))?;
            A2aTask::from_json_value(&val)
                .map_err(|e| NgcErrorInfo::new(NgcErrorCode::Internal, e.message()))
        })
    }

    fn cancel_task<'a>(
        &'a self,
        _target_url: &'a str,
        _interface_tenant: Option<&'a str>,
        headers: &'a BTreeMap<String, String>,
        task_id: &'a str,
    ) -> BoxFuture<'a, NgcResult<A2aTask>> {
        Box::pin(async move {
            let ctx = peer_ctx(self.target_snap.clone(), "acme", 0, Vec::new());
            let hdrs = headers
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            let resp = self
                .target_facade
                .handle(
                    &ctx,
                    HttpExchangeRequest {
                        method: "POST".to_owned(),
                        path: format!("/tasks/{task_id}:cancel"),
                        query: None,
                        headers: hdrs,
                        body: Vec::new(),
                    },
                )
                .await
                .map_err(|e| NgcErrorInfo::new(NgcErrorCode::Internal, e.message()))?;
            let val = JsonValue::parse(&String::from_utf8_lossy(&resp.body))
                .map_err(|e| NgcErrorInfo::new(NgcErrorCode::Internal, e.message()))?;
            A2aTask::from_json_value(&val)
                .map_err(|e| NgcErrorInfo::new(NgcErrorCode::Internal, e.message()))
        })
    }
}

fn build_facade(
    node_id: &str,
    snap: Arc<Snapshot>,
) -> (
    Arc<A2aFacade<InMemoryA2aBackend, StaticDiscovery>>,
    AgentCard,
) {
    let clock = Arc::new(ManualClock::new(1_700_000_000_000));
    let backend = Arc::new(InMemoryA2aBackend::new(clock.clone()));
    let cfg = A2aFacadeConfig {
        node_id: node_id.to_owned(),
        card: AgentCardConfig {
            name: node_id.to_owned(),
            supported_interfaces: vec![AgentInterface {
                url: format!("https://{node_id}.example.com/a2a"),
                protocol_binding: AgentProtocolBinding::HttpJson,
                protocol_version: A2A_PROTOCOL_VERSION.to_owned(),
                tenant: None,
            }],
            ..AgentCardConfig::default()
        },
        ..A2aFacadeConfig::default()
    };
    let card = build_agent_card(&snap, &cfg.card, None).expect("agent card");
    let facade = Arc::new(A2aFacade::new(
        cfg,
        backend,
        Arc::new(StaticDiscovery),
        clock,
    ));
    (facade, card)
}

// spec: CMP-022, CMP-023, INV-20, NGC-042
#[test]
fn inv_20_two_gap_nodes_mutual_delegation_terminates_with_loop_detected() {
    let snap = peer_snapshot("acme");
    let (facade_a, card_a) = build_facade("node-a", snap.clone());
    let (facade_b, card_b) = build_facade("node-b", snap.clone());

    let connector_a_to_b = A2aPeerConnector::new(
        A2aPeerConfig::new("peer-b", "node-a", card_b.clone()),
        Arc::new(FacadeLoopTransport {
            peer_card: card_b,
            target_facade: Arc::clone(&facade_b),
            target_snap: snap.clone(),
            recorded_hops: Mutex::new(Vec::new()),
        }),
    );

    let transport_b_to_a = Arc::new(FacadeLoopTransport {
        peer_card: card_a.clone(),
        target_facade: Arc::clone(&facade_a),
        target_snap: snap,
        recorded_hops: Mutex::new(Vec::new()),
    });
    let connector_b_to_a = A2aPeerConnector::new(
        A2aPeerConfig::new("peer-a", "node-b", card_a),
        Arc::clone(&transport_b_to_a),
    );

    // Step 1: Node A delegates a task to Node B (`hop: 0 -> 1 (node-a) -> 2 (node-a, node-b)`).
    let scope_a = CallScope::for_test((Instant::now() + Duration::from_secs(30)).into());
    let mut req_a: StartTaskRequest = gap_ngc_sdk::json::from_json_slice(
        br#"{"operation":"gap_gapql","contextId":"ctx-loop","message":{"gapql":"{ orders { id } }"}}"#,
    )
    .expect("start req");
    req_a.ctx = Some(CallContext {
        tenant_id: "acme".to_owned(),
        request_id: "req-loop-1".to_owned(),
        hop_count: 0,
        trace_path: Vec::new(),
        ..CallContext::default()
    });
    let mut req_b = req_a.clone();
    let b_status =
        block_on(connector_a_to_b.start_task(req_a, scope_a)).expect("A -> B delegation succeeds");
    assert_eq!(b_status.state, ProtoTaskState::Completed as i32);

    // Step 2: Now Node B tries to delegate BACK into Node A carrying the accumulated trace path `[node-a, node-b]`.
    let mut loop_scope = CallScope::for_test((Instant::now() + Duration::from_secs(30)).into());
    loop_scope
        .metadata
        .insert(SCOPE_HOP_HEADER.to_owned(), "2".to_owned());
    loop_scope
        .metadata
        .insert(SCOPE_TRACE_PATH_HEADER.to_owned(), "node-a".to_owned());

    req_b.ctx = Some(CallContext {
        tenant_id: "acme".to_owned(),
        request_id: "req-loop-2".to_owned(),
        hop_count: 2,
        trace_path: vec!["node-a".to_owned(), "node-b".to_owned()],
        ..CallContext::default()
    });
    let err = block_on(connector_b_to_a.start_task(req_b, loop_scope))
        .expect_err("B -> A delegation back into visited node MUST fail with GAP_LOOP_DETECTED");
    assert_eq!(err.code, NgcErrorCode::FailedPrecondition);
    assert!(
        err.message().contains("GAP_LOOP_DETECTED"),
        "expected GAP_LOOP_DETECTED, got `{}`",
        err.message()
    );

    // Also verify inbound HTTP facade itself rejects cyclic metadata trace payloads with HTTP 508 + GAP_LOOP_DETECTED.
    let cyclic_http = format!(
        r#"{{"message":{{"messageId":"m-cyc","role":"ROLE_USER","parts":[{{"text":"loop check"}}],"metadata":{{"{GAP_TRACE_EXT_URI}":{{"hop":1,"path":["node-a"]}}}}}}}}"#
    );
    let resp = block_on(facade_a.handle(
        &peer_ctx(peer_snapshot("acme"), "acme", 0, Vec::new()),
        HttpExchangeRequest {
            method: "POST".to_owned(),
            path: "/message:send".to_owned(),
            query: None,
            headers: vec![(A2A_VERSION_HEADER.to_owned(), "1.0".to_owned())],
            body: cyclic_http.into_bytes(),
        },
    ))
    .expect("http loop response");
    assert_eq!(resp.status, 508);
    assert!(String::from_utf8_lossy(&resp.body).contains("GAP_LOOP_DETECTED"));
}

// spec: CMP-021, CMP-022, NGC-041
#[test]
fn streaming_send_and_subscribe_and_cancel_roundtrip() {
    let snap = peer_snapshot("acme");
    let (facade, _card) = build_facade("node-stream", snap.clone());
    let ctx = peer_ctx(snap, "acme", 0, Vec::new());

    // Stream text resolution over `/message:stream` (returns SSE `StreamResponse` frames).
    let stream_req = br#"{"message":{"messageId":"m-stream-1","role":"ROLE_USER","parts":[{"text":"find orders"}]}}"#;
    let stream_resp = block_on(facade.handle(
        &ctx,
        HttpExchangeRequest {
            method: "POST".to_owned(),
            path: "/message:stream".to_owned(),
            query: None,
            headers: vec![(A2A_VERSION_HEADER.to_owned(), "1.0".to_owned())],
            body: stream_req.to_vec(),
        },
    ))
    .expect("stream response");
    assert_eq!(stream_resp.status, 200);
    let sse_text = String::from_utf8_lossy(&stream_resp.body);
    assert!(sse_text.contains("data: "));
    assert!(sse_text.contains("TASK_STATE_COMPLETED"));

    // Submit async (`returnImmediately: true`) then subscribe & cancel over HTTP+JSON.
    let async_req = br#"{"configuration":{"returnImmediately":true},"message":{"messageId":"m-async-1","contextId":"ctx-sub","role":"ROLE_USER","parts":[{"data":{"gapql":"{ catalog_items { id } }"},"mediaType":"application/json"}]}}"#;
    let async_resp = block_on(facade.handle(
        &ctx,
        HttpExchangeRequest {
            method: "POST".to_owned(),
            path: "/message:send".to_owned(),
            query: None,
            headers: vec![(A2A_VERSION_HEADER.to_owned(), "1.0".to_owned())],
            body: async_req.to_vec(),
        },
    ))
    .expect("async send");
    assert_eq!(async_resp.status, 200);
    let async_json = JsonValue::parse(&String::from_utf8_lossy(&async_resp.body)).expect("json");
    let task_id = async_json
        .get("task")
        .and_then(|t| t.get_str("id"))
        .expect("task id")
        .to_owned();

    let sub_resp = block_on(facade.handle(
        &ctx,
        HttpExchangeRequest {
            method: "GET".to_owned(),
            path: format!("/tasks/{task_id}:subscribe"),
            query: None,
            headers: vec![(A2A_VERSION_HEADER.to_owned(), "1.0".to_owned())],
            body: Vec::new(),
        },
    ))
    .expect("subscribe");
    assert_eq!(sub_resp.status, 200);
    let sub_body = String::from_utf8_lossy(&sub_resp.body);
    assert!(sub_body.contains("statusUpdate"));

    let cancel_resp = block_on(facade.handle(
        &ctx,
        HttpExchangeRequest {
            method: "POST".to_owned(),
            path: format!("/tasks/{task_id}:cancel"),
            query: None,
            headers: vec![(A2A_VERSION_HEADER.to_owned(), "1.0".to_owned())],
            body: Vec::new(),
        },
    ))
    .expect("cancel");
    assert_eq!(cancel_resp.status, 200);
    assert!(String::from_utf8_lossy(&cancel_resp.body).contains("TASK_STATE_CANCELED"));

    // Verify JSON-RPC SubscribeToTask + decode_stream_events interoperability.
    let rpc_sub = format!(
        r#"{{"jsonrpc":"2.0","id":9,"method":"SubscribeToTask","params":{{"id":"{task_id}"}}}}"#
    );
    let rpc_resp = block_on(facade.handle(
        &ctx,
        HttpExchangeRequest {
            method: "POST".to_owned(),
            path: "/a2a".to_owned(),
            query: None,
            headers: vec![(A2A_VERSION_HEADER.to_owned(), "1.0".to_owned())],
            body: rpc_sub.into_bytes(),
        },
    ))
    .expect("rpc sub");
    assert_eq!(rpc_resp.status, 200);
    let rpc_body = String::from_utf8_lossy(&rpc_resp.body);
    assert!(rpc_body.contains("TASK_STATE_CANCELED"));

    // Sanity check decoding synthetic status stream events.
    let decoded = A2aPeerConnector::<FacadeLoopTransport>::decode_stream_events(&[
        A2aStreamEvent::StatusUpdate {
            task_id: task_id.clone(),
            context_id: "ctx-sub".to_owned(),
            status: gap_a2a::A2aTaskStatus {
                state: A2aTaskState::Canceled,
                message: None,
                timestamp: None,
            },
            metadata: BTreeMap::new(),
        },
    ])
    .expect("decode");
    assert_eq!(decoded[0].state, ProtoTaskState::Canceled as i32);
    let _ = CancelTaskRequest::default();
}
