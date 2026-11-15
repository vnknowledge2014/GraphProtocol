//! Unit tests for `S1RouterAdapter`, `BoundedSemaphore`, canonical `RTE-042` formatting,
//! TTL caching, circuit breaker fallback, and `ADR-006` replica invariance (`WP-M5-03`).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Wake, Waker};
use std::time::{Duration, Instant};

use gap_core::context::{Budget, Principal, PrincipalKind, RequestContext, TraceCtx};
use gap_core::error::{ErrorCode, GapError, GapResult};
use gap_core::ids::{BranchId, BranchUid, GroupId, OperationId, TenantId};
use gap_core::snapshot::{
    BranchDescriptor, BranchLimits, BranchStatus, Capabilities, CapabilityGroup, CostHint,
    Endpoint, Example, OpKind, OperationDescriptor, Protocol, SideEffect, SideEffectLevel,
    Snapshot, TrustTier,
};
use gap_core::traits::{BoxFuture, DiscoveryOutcome, ElementKind};
use gap_index::{ElementDoc, HybridElementIndex, HybridIndexConfig};
use gap_ql::with_gap_builtins;

use super::{
    DecisionBackend, OPTION_FORMAT_VERSION, RankCtx, RoutingModel, S1_CHOICE_QUESTION,
    S1AdapterConfig, S1RouterAdapter, format_s1_options, format_s1_state_jcs,
};
use crate::{
    CalibrationMode, DiscoveryConfig, GroupOption, ResolveConstraints, RouteDiscoveryService,
    SINK_OPTION_KEY, SINK_OPTION_TEXT, SanitizedQuery, sanitize_default,
};

fn poll_ready<F: Future>(fut: F) -> F::Output {
    let mut boxed: Pin<Box<F>> = Box::pin(fut);
    let waker = Waker::noop();
    let mut cx = Context::from_waker(waker);
    match boxed.as_mut().poll(&mut cx) {
        Poll::Ready(out) => out,
        Poll::Pending => panic!("expected test future to resolve synchronously"),
    }
}

#[derive(Default)]
struct CountingWaker {
    wakes: AtomicUsize,
}

impl Wake for CountingWaker {
    fn wake(self: Arc<Self>) {
        self.wakes.fetch_add(1, Ordering::AcqRel);
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.wakes.fetch_add(1, Ordering::AcqRel);
    }
}

#[derive(Debug, Clone)]
struct RecordedCall {
    state_jcs: String,
    question: String,
    options: Vec<GroupOption>,
    timeout_passed: Duration,
}

#[derive(Debug, Default)]
struct FixtureDecisionBackend {
    model: String,
    head_max_len: usize,
    calls: Mutex<Vec<RecordedCall>>,
    fail_next: Mutex<Option<GapError>>,
    artificial_delay: Duration,
    swap_logit_on_key_g0: f32,
}

impl FixtureDecisionBackend {
    fn new(model: &str) -> Self {
        Self {
            model: model.to_owned(),
            head_max_len: 512,
            calls: Mutex::new(Vec::new()),
            fail_next: Mutex::new(None),
            artificial_delay: Duration::ZERO,
            swap_logit_on_key_g0: 3.25,
        }
    }

    fn call_count(&self) -> usize {
        self.calls.lock().unwrap().len()
    }

    fn recorded_calls(&self) -> Vec<RecordedCall> {
        self.calls.lock().unwrap().clone()
    }
}

impl DecisionBackend for FixtureDecisionBackend {
    fn model_id(&self) -> &str {
        &self.model
    }

    fn head_max_len(&self) -> usize {
        self.head_max_len
    }

    fn score_choice<'a>(
        &'a self,
        state_jcs: &'a str,
        question: &'a str,
        options: &'a [GroupOption],
    ) -> BoxFuture<'a, GapResult<Vec<f32>>> {
        self.score_choice_with_timeout(state_jcs, question, options, Duration::from_secs(10))
    }

    fn score_choice_with_timeout<'a>(
        &'a self,
        state_jcs: &'a str,
        question: &'a str,
        options: &'a [GroupOption],
        timeout: Duration,
    ) -> BoxFuture<'a, GapResult<Vec<f32>>> {
        Box::pin(async move {
            self.calls.lock().unwrap().push(RecordedCall {
                state_jcs: state_jcs.to_owned(),
                question: question.to_owned(),
                options: options.to_vec(),
                timeout_passed: timeout,
            });
            if !self.artificial_delay.is_zero() {
                std::thread::sleep(self.artificial_delay);
            }
            if let Some(err) = self.fail_next.lock().unwrap().take() {
                return Err(err);
            }
            let logits = options
                .iter()
                .map(|opt| {
                    if opt.key == SINK_OPTION_KEY {
                        0.40
                    } else if opt.text.to_ascii_lowercase().contains("invoice") {
                        self.swap_logit_on_key_g0
                    } else {
                        -0.25
                    }
                })
                .collect();
            Ok(logits)
        })
    }
}

fn make_group_id(tag: &str) -> GroupId {
    let mut bytes = [0u8; 16];
    for (idx, b) in tag.bytes().take(16).enumerate() {
        bytes[idx] = b;
    }
    GroupId::from_bytes(bytes)
}

fn make_op(
    branch: &BranchId,
    name: &str,
    group_tag: &str,
    sdl: &str,
    intent: &str,
) -> Arc<OperationDescriptor> {
    let id = OperationId::from_parts(branch, name).expect("valid operation id");
    Arc::new(OperationDescriptor {
        id,
        group: make_group_id(group_tag),
        kind: OpKind::Query,
        side_effect: SideEffect {
            level: SideEffectLevel::None,
            verified: true,
            requires_approval: false,
        },
        idempotent: true,
        cost: CostHint::default(),
        desc_hash: [1u8; 32],
        sdl: Arc::from(sdl),
        examples: vec![Example {
            intent: intent.to_owned(),
            positive: true,
            query_snippet: None,
        }],
    })
}

fn make_branch(slug: &str, synopsis: &str, ops: Vec<Arc<OperationDescriptor>>) -> BranchDescriptor {
    BranchDescriptor {
        uid: BranchUid::new_v7(),
        id: BranchId::new(slug).expect("valid branch id"),
        protocol: Protocol::Rest,
        trust: TrustTier::T0,
        endpoint: Endpoint {
            uri: format!("https://{slug}.internal"),
            transport: "http".to_owned(),
        },
        spec_hash: [3u8; 32],
        pinned_desc_hash: [3u8; 32],
        capabilities: Capabilities::default(),
        limits: BranchLimits::default(),
        synopsis: synopsis.to_owned(),
        operations: ops,
        status: BranchStatus::Active,
    }
}

fn make_ctx(snapshot: Arc<Snapshot>) -> RequestContext {
    let tenant = TenantId::default_tenant();
    RequestContext::try_new(
        BranchUid::new_v7().as_uuid(),
        tenant.clone(),
        Principal::new("agent-test", PrincipalKind::Agent, tenant),
        Instant::now() + Duration::from_secs(30),
        Budget::new(10_000),
        TraceCtx::generate_root(false),
        0,
        None,
        snapshot,
    )
    .expect("valid RequestContext")
}

// spec: RTE-042, RTE-030
#[test]
fn s1_adapter_formats_canonical_jcs_state_g0_keys_sink_text_and_token_budget() {
    assert_eq!(OPTION_FORMAT_VERSION, 1);

    let noisy = format!(
        "invoice [MASK] {}",
        (0..200)
            .map(|i| format!("t{i}"))
            .collect::<Vec<_>>()
            .join(" ")
    );
    let raw_opts = vec![
        GroupOption {
            key: SINK_OPTION_KEY.to_owned(),
            text: "Caller attempted custom sink override".to_owned(),
            prior: 0.0,
        },
        GroupOption {
            key: "legacy-low".to_owned(),
            text: "inventory warehouse stock".to_owned(),
            prior: 0.25,
        },
        GroupOption {
            key: "legacy-high".to_owned(),
            text: noisy,
            prior: 0.90,
        },
    ];

    let formatted = format_s1_options(&raw_opts, 120);
    // Non-sink sorted descending by prior and canonically re-keyed g0, g1, ..., with sink last.
    assert_eq!(formatted.len(), 3);
    assert_eq!(formatted[0].key, "g0");
    assert_eq!(formatted[1].key, "g1");
    assert_eq!(formatted[2].key, SINK_OPTION_KEY);
    assert_eq!(formatted[2].text, SINK_OPTION_TEXT);
    assert!(!formatted[0].text.to_ascii_lowercase().contains("[mask]"));

    let expected_cap = ((0.8 * 120.0_f64) / 3.0_f64).floor().max(8.0) as usize;
    assert!(
        formatted[0].text.split_whitespace().count() <= expected_cap,
        "expected <= {expected_cap} tokens, got {}",
        formatted[0].text.split_whitespace().count()
    );

    let backend = Arc::new(FixtureDecisionBackend::new("laya-candle-fixture"));
    let adapter = S1RouterAdapter::new(Arc::clone(&backend), S1AdapterConfig::default());
    let q = SanitizedQuery::new("check\t\"recent\" [MASK] invoice");
    let ctx = RankCtx {
        max_side_effect: SideEffectLevel::Reversible,
        request_id: "req-format".to_owned(),
        timeout_ms: 400,
    };

    let out = poll_ready(adapter.rank(&q, &raw_opts, &ctx)).expect("rank succeeds");
    // Logits aligned back to caller input order: [sink, legacy-low, legacy-high].
    assert_eq!(out.logits.len(), 3);
    assert!(
        (out.logits[2] - 3.25).abs() < 1e-6,
        "legacy-high at caller idx 2 got {}",
        out.logits[2]
    );
    assert!(
        (out.logits[1] - (-0.25)).abs() < 1e-6,
        "legacy-low at caller idx 1 got {}",
        out.logits[1]
    );
    assert!(
        (out.logits[0] - 0.40).abs() < 1e-6,
        "sink at caller idx 0 got {}",
        out.logits[0]
    );

    let calls = backend.recorded_calls();
    assert_eq!(calls.len(), 1);
    assert!(calls[0].timeout_passed <= Duration::from_millis(400));
    assert_eq!(
        calls[0].state_jcs,
        format_s1_state_jcs(
            &SanitizedQuery::new(&sanitize_default(q.as_str())),
            SideEffectLevel::Reversible
        )
    );
    assert_eq!(calls[0].question, S1_CHOICE_QUESTION);
    assert_eq!(calls[0].options[0].key, "g0");
    assert_eq!(calls[0].options[1].key, "g1");
    assert_eq!(calls[0].options[2].key, SINK_OPTION_KEY);
    assert_eq!(calls[0].options[2].text, SINK_OPTION_TEXT);
}

fn build_snapshot_for_replicas(replica_slugs: &[&str]) -> Snapshot {
    let sdl = r#"
        type Invoice @key(fields: "id") {
            id: ID!
            totalCents: Int!
            status: String!
        }
        type Stock @key(fields: "sku") {
            sku: ID!
            available: Int!
        }
        type Query {
            getInvoice(id: ID!): Invoice
            checkStock(sku: ID!): Stock
        }
    "#;
    let schema = with_gap_builtins(sdl).expect("schema builds");
    let tenant = TenantId::default_tenant();

    let mut snap = Snapshot::empty(10);
    snap.schema = Arc::new(schema.into_inner());
    let mut docs = Vec::new();
    let billing_gid = make_group_id("grp-billing");
    let mut billing_members = Vec::new();

    for (idx, slug) in replica_slugs.iter().enumerate() {
        let bid = BranchId::new(*slug).expect("valid branch id");
        let op = make_op(
            &bid,
            "getInvoice",
            "grp-billing",
            "getInvoice(id: ID!): Invoice",
            "Lookup customer invoice status and charges",
        );
        let br = make_branch(
            slug,
            "Customer invoice and billing status lookup",
            vec![Arc::clone(&op)],
        );
        let uid = br.uid;
        billing_members.push((uid, op.id.clone()));
        snap.branches.insert(uid, Arc::new(br));

        // Only index the initial primary or mirror identical docs for all replicas.
        if idx == 0 {
            docs.push(
                ElementDoc::new(op.id.as_str(), ElementKind::Operation, "getInvoice")
                    .with_tenant(tenant.clone())
                    .with_branch_uid(uid)
                    .with_operation_id(op.id.clone())
                    .with_path("Query.getInvoice")
                    .with_text("getInvoice: customer invoice status and billing charges")
                    .with_examples(vec!["lookup customer invoice status".to_owned()]),
            );
        }
    }
    snap.groups.insert(
        billing_gid.clone(),
        Arc::new(CapabilityGroup {
            id: billing_gid,
            signature_sdl: Arc::from("Query.getInvoice(id: ID!): Invoice"),
            members: billing_members,
        }),
    );

    let stock_bid = BranchId::new("warehouse-primary").expect("stock branch");
    let stock_op = make_op(
        &stock_bid,
        "checkStock",
        "grp-warehouse",
        "checkStock(sku: ID!): Stock",
        "Check warehouse stock availability",
    );
    let stock_br = make_branch(
        "warehouse-primary",
        "Warehouse inventory and SKU stock lookup",
        vec![Arc::clone(&stock_op)],
    );
    let stock_uid = stock_br.uid;
    docs.push(
        ElementDoc::new(stock_op.id.as_str(), ElementKind::Operation, "checkStock")
            .with_tenant(tenant)
            .with_branch_uid(stock_uid)
            .with_operation_id(stock_op.id.clone())
            .with_path("Query.checkStock")
            .with_text("checkStock: warehouse SKU inventory stock levels")
            .with_examples(vec!["check warehouse stock".to_owned()]),
    );
    snap.groups.insert(
        stock_op.group.clone(),
        Arc::new(CapabilityGroup {
            id: stock_op.group.clone(),
            signature_sdl: Arc::from("Query.checkStock(sku: ID!): Stock"),
            members: vec![(stock_uid, stock_op.id.clone())],
        }),
    );
    snap.branches.insert(stock_uid, Arc::new(stock_br));

    snap.index = Arc::new(
        HybridElementIndex::build(docs, HybridIndexConfig::default()).expect("hybrid index"),
    );
    snap
}

// spec: RTE-030, RTE-042
// Design ref: ADR-006
#[test]
fn s1_adapter_replica_addition_invariance_with_fixture_logits() {
    let snap_1 = Arc::new(build_snapshot_for_replicas(&["billing-01"]));
    let snap_3 = Arc::new(build_snapshot_for_replicas(&[
        "billing-01",
        "billing-02",
        "billing-03",
    ]));

    let backend_1 = Arc::new(FixtureDecisionBackend::new("laya-fixture-rep1"));
    let backend_3 = Arc::new(FixtureDecisionBackend::new("laya-fixture-rep1"));

    let cfg = DiscoveryConfig {
        enforce_calibration_for_s1: false,
        ..DiscoveryConfig::default()
    };
    let svc_1 = RouteDiscoveryService::new(
        S1RouterAdapter::new(Arc::clone(&backend_1), S1AdapterConfig::default()),
        None,
        cfg.clone(),
    );
    let svc_3 = RouteDiscoveryService::new(
        S1RouterAdapter::new(Arc::clone(&backend_3), S1AdapterConfig::default()),
        None,
        cfg,
    );

    let ctx_1 = make_ctx(snap_1);
    let ctx_3 = make_ctx(snap_3);

    let res_1 = poll_ready(svc_1.resolve_detailed(
        &ctx_1,
        ResolveConstraints::default(),
        "read customer invoice status",
    ))
    .expect("resolve 1 replica");
    let res_3 = poll_ready(svc_3.resolve_detailed(
        &ctx_3,
        ResolveConstraints::default(),
        "read customer invoice status",
    ))
    .expect("resolve 3 replicas");

    assert_eq!(res_1.router, "s1:laya-fixture-rep1");
    assert_eq!(res_3.router, "s1:laya-fixture-rep1");
    assert_eq!(res_1.calibration_mode, CalibrationMode::Fallback);
    assert_eq!(res_3.calibration_mode, CalibrationMode::Fallback);

    let calls_1 = backend_1.recorded_calls();
    let calls_3 = backend_3.recorded_calls();
    assert_eq!(calls_1.len(), 1);
    assert_eq!(calls_3.len(), 1);
    assert_eq!(calls_1[0].state_jcs, calls_3[0].state_jcs);
    assert_eq!(calls_1[0].question, calls_3[0].question);
    assert_eq!(
        calls_1[0].options, calls_3[0].options,
        "S1 input options must be invariant under replica addition (ADR-006)"
    );

    assert_eq!(res_1.candidates.len(), res_3.candidates.len());
    for (a, b) in res_1.candidates.iter().zip(res_3.candidates.iter()) {
        assert_eq!(a.group_id, b.group_id);
        assert_eq!(a.skeleton, b.skeleton);
        assert!(
            (a.score - b.score).abs() < 1e-6,
            "score drifted under replica addition: {} vs {}",
            a.score,
            b.score
        );
        assert_eq!(a.probability, b.probability);
        assert_eq!(a.branch, b.branch);
        assert_eq!(a.operation_id, b.operation_id);
    }
}

// spec: RTE-046
#[test]
fn s1_adapter_decision_cache_hits_within_ttl_and_expires() {
    let opts = vec![
        GroupOption {
            key: "g0".to_owned(),
            text: "billing invoice status".to_owned(),
            prior: 0.9,
        },
        GroupOption {
            key: SINK_OPTION_KEY.to_owned(),
            text: SINK_OPTION_TEXT.to_owned(),
            prior: 0.0,
        },
    ];
    let q = SanitizedQuery::new("check invoice");
    let ctx = RankCtx::default();

    // 1. Non-expired TTL hits cache without re-invoking backend.
    let backend_cached = Arc::new(FixtureDecisionBackend::new("laya-cache-hit"));
    let adapter_cached = S1RouterAdapter::new(
        Arc::clone(&backend_cached),
        S1AdapterConfig {
            cache_ttl_secs: 600,
            cache_max_entries: 16,
            ..S1AdapterConfig::default()
        },
    );
    let r1 = poll_ready(adapter_cached.rank(&q, &opts, &ctx)).expect("first call");
    let r2 = poll_ready(adapter_cached.rank(&q, &opts, &ctx)).expect("second cached call");
    assert_eq!(r1.logits, r2.logits);
    assert_eq!(
        backend_cached.call_count(),
        1,
        "second request within TTL must hit decision cache"
    );

    // 2. Zero TTL forces expiry and re-evaluation on every request.
    let backend_expired = Arc::new(FixtureDecisionBackend::new("laya-cache-exp"));
    let adapter_expired = S1RouterAdapter::new(
        Arc::clone(&backend_expired),
        S1AdapterConfig {
            cache_ttl_secs: 0,
            cache_max_entries: 16,
            ..S1AdapterConfig::default()
        },
    );
    let _ = poll_ready(adapter_expired.rank(&q, &opts, &ctx)).expect("first call");
    let _ = poll_ready(adapter_expired.rank(&q, &opts, &ctx)).expect("expired call");
    assert_eq!(backend_expired.call_count(), 2);
}

// spec: RTE-045
#[test]
fn s1_adapter_bounded_semaphore_queues_up_to_2x_and_sheds_overflow() {
    let backend = Arc::new(FixtureDecisionBackend::new("laya-sem"));
    let adapter = S1RouterAdapter::new(
        backend,
        S1AdapterConfig {
            max_concurrency: 2,
            max_queue_depth: None, // defaults to 2 * max_concurrency = 4
            ..S1AdapterConfig::default()
        },
    );
    let sem = adapter.semaphore();
    assert_eq!(sem.max_permits(), 2);
    assert_eq!(sem.max_waiters(), 4);

    let tracker = Arc::new(CountingWaker::default());
    let waker = Waker::from(Arc::clone(&tracker));
    let mut cx = Context::from_waker(&waker);

    // Acquire both active permits (2 / 2).
    let p1 = match Pin::new(&mut sem.acquire()).poll(&mut cx) {
        Poll::Ready(Ok(p)) => p,
        other => panic!("expected immediate p1 permit, got {other:?}"),
    };
    let p2 = match Pin::new(&mut sem.acquire()).poll(&mut cx) {
        Poll::Ready(Ok(p)) => p,
        other => panic!("expected immediate p2 permit, got {other:?}"),
    };
    assert_eq!(sem.in_flight(), 2);

    // Queue up to 4 waiting futures (2 * max_concurrency).
    let mut queued: Vec<Pin<Box<super::semaphore::AcquireFuture<'_>>>> =
        (0..4).map(|_| Box::pin(sem.acquire())).collect();
    for (idx, fut) in queued.iter_mut().enumerate() {
        match fut.as_mut().poll(&mut cx) {
            Poll::Pending => {}
            other => panic!("waiter {idx} should queue cleanly, got {other:?}"),
        }
    }
    assert_eq!(sem.queued_waiters(), 4);

    // 7th concurrent request exceeds bounded queue depth and sheds immediately with DiscoveryDegraded.
    let overflow = poll_ready(sem.acquire()).expect_err("7th concurrent request must shed");
    assert_eq!(overflow.code(), ErrorCode::DiscoveryDegraded);

    // Drop one active permit -> wakes front waiter cleanly.
    drop(p1);
    assert_eq!(tracker.wakes.load(Ordering::Acquire), 1);

    // Cancel that woken (already-granted) future before polling -> transfers slot to next waiter!
    let woken_first = queued.remove(0);
    drop(woken_first);
    assert_eq!(tracker.wakes.load(Ordering::Acquire), 2);

    // Poll the transferred second waiter -> acquires permit cleanly.
    let mut next_fut = queued.remove(0);
    let p3 = match next_fut.as_mut().poll(&mut cx) {
        Poll::Ready(Ok(p)) => p,
        other => panic!("transferred waiter must receive permit, got {other:?}"),
    };

    drop(p2);
    drop(p3);
    queued.clear();
    assert_eq!(sem.in_flight(), 0);
    assert_eq!(sem.queued_waiters(), 0);
}

// spec: RTE-044, INV-17
#[test]
fn s1_adapter_timeout_and_rolling_failure_rate_trip_cooldown_and_fallback_inv_17() {
    let snap = Arc::new(build_snapshot_for_replicas(&["billing-01"]));
    let ctx = make_ctx(snap);

    let backend = Arc::new(FixtureDecisionBackend {
        model: "laya-flaky".to_owned(),
        head_max_len: 512,
        calls: Mutex::new(Vec::new()),
        fail_next: Mutex::new(None),
        artificial_delay: Duration::from_millis(25),
        swap_logit_on_key_g0: 3.25,
    });

    let adapter = Arc::new(S1RouterAdapter::new(
        Arc::clone(&backend),
        S1AdapterConfig {
            cache_ttl_secs: 0,
            failure_rate_threshold: 0.20,
            circuit_cooldown_secs: 60,
            ..S1AdapterConfig::default()
        },
    ));
    let svc = RouteDiscoveryService::new(
        Arc::clone(&adapter),
        None,
        DiscoveryConfig {
            enforce_calibration_for_s1: false,
            ..DiscoveryConfig::default()
        },
    );

    let q = SanitizedQuery::new("check invoice");
    let opts = vec![
        GroupOption {
            key: "g0".to_owned(),
            text: "billing invoice".to_owned(),
            prior: 0.9,
        },
        GroupOption {
            key: SINK_OPTION_KEY.to_owned(),
            text: SINK_OPTION_TEXT.to_owned(),
            prior: 0.0,
        },
    ];
    let tight_ctx = RankCtx {
        max_side_effect: SideEffectLevel::None,
        request_id: "req-timeout".to_owned(),
        timeout_ms: 5,
    };

    // 5 timed-out calls exceed >=5 window and >20% failure rate, opening the 60s circuit.
    for _ in 0..5 {
        let err = poll_ready(adapter.rank(&q, &opts, &tight_ctx))
            .expect_err("overrun call must fail with timeout degradation");
        assert_eq!(err.code(), ErrorCode::DiscoveryDegraded);
    }
    let calls_before_cooldown = backend.call_count();
    assert_eq!(calls_before_cooldown, 5);

    // RouteDiscoveryService degrades cleanly to BaselineRouter while circuit is open without touching backend.
    let degraded = poll_ready(svc.resolve_detailed(
        &ctx,
        ResolveConstraints::default(),
        "lookup customer invoice status",
    ))
    .expect("INV-17 fallback succeeds under open S1 cooldown");

    assert_eq!(backend.call_count(), calls_before_cooldown);
    assert_eq!(degraded.router, "baseline");
    assert!(
        degraded
            .warnings
            .iter()
            .any(|w| w == "GAP_DISCOVERY_DEGRADED"),
        "expected GAP_DISCOVERY_DEGRADED warning, got {:?}",
        degraded.warnings
    );
    let core_out = degraded.into_core_resolution();
    assert_eq!(core_out.outcome, DiscoveryOutcome::Degraded);
    assert!(!core_out.candidates.is_empty());
}
