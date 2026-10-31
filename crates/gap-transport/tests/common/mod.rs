//! Shared two-branch `gap_snapshot::compose` supergraph, `MockConnector` over `in_memory_grpc`,
//! and zero-socket `tower::ServiceExt::oneshot` helpers for `gap-transport` integration tests.

#![allow(dead_code, clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Request, Response, StatusCode};
use bytes::Bytes;
use futures_util::stream;
use gap_core::clock::{Clock, ManualClock, Timestamp};
use gap_core::ids::{BranchId, BranchUid, TenantId};
use gap_core::snapshot::{BranchStatus, Endpoint, Protocol, Snapshot, TrustTier};
use gap_exec::ConnectorRegistry;
use gap_ngc_sdk::connector::BoxStream;
use gap_ngc_sdk::core::{ServerConfig, ServerCore};
use gap_ngc_sdk::error::{json_map_to_prost_struct, prost_struct_to_json_map};
use gap_ngc_sdk::grpc::GrpcClient;
use gap_ngc_sdk::mock::MockConnector;
use gap_ngc_sdk::testing::{InMemoryGrpcHandle, in_memory_grpc};
use gap_plan::PlanCache;
use gap_proto::ngc::v1::{
    Capabilities as ProtoCapabilities, Complete, ConnectorDescription, Limits as ProtoLimits,
    MetaSummary, OpKind as ProtoOpKind, OperationDescriptor as ProtoOp, QueryResponse, RowSetChunk,
    SideEffectHint, query_response,
};
use gap_ql::{InMemoryPersistedStore, PersistedOperationStore};
use gap_snapshot::{BranchInput, SnapshotBuilder, SnapshotHandle, compose};
use gap_testkit::clock::test_clock;
use gap_transport::{
    AdmissionPolicy, ApiKeyRecord, AppState, TransportConfig, TransportMetrics, router,
};
use http_body_util::BodyExt as _;
use serde_json::{Value, json};
use tower::ServiceExt as _;

pub const SHOP_UID_STR: &str = "018bcfe5-6c01-7010-8101-010101010101";
pub const CRM_UID_STR: &str = "018bcfe5-6c02-7020-8202-020202020202";

pub fn shop_uid() -> BranchUid {
    BranchUid::parse(SHOP_UID_STR).expect("valid shop UUIDv7")
}

pub fn crm_uid() -> BranchUid {
    BranchUid::parse(CRM_UID_STR).expect("valid crm UUIDv7")
}

pub fn ts(ms: i64) -> Timestamp {
    Timestamp::from_unix_millis(ms).expect("valid timestamp")
}

pub fn shop_branch_input(rev_label: &str) -> BranchInput {
    let sdl = r#"
type Order {
    id: ID!
    sku: String!
    totalCents: Int! @cost(weight: 2)
}

type Query {
    shopVersion: String! @cost(weight: 1)
    order(id: ID!): Order @cost(weight: 2)
    strictOrder(id: ID!): Order! @cost(weight: 2)
    orders(first: Int): [Order!]! @cost(weight: 2, listSize: 10)
}

type Mutation {
    createOrder(sku: String!): Order! @cost(weight: 5)
}
"#;

    let make_op = |name: &str, kind: ProtoOpKind, weight: u32, se: SideEffectHint| ProtoOp {
        name: name.to_owned(),
        kind: kind as i32,
        description: format!("shop.{name} ({rev_label})"),
        input_schema: None,
        output_schema: None,
        side_effect_hint: se as i32,
        idempotent_hint: kind == ProtoOpKind::Query,
        supports_dry_run: false,
        compensation_operation: String::new(),
        examples: Vec::new(),
        cost_weight: weight,
        pagination: 0,
        key_fields: Vec::new(),
        annotations: HashMap::new(),
    };

    BranchInput {
        id: BranchId::new("shop").expect("shop branch id"),
        uid: shop_uid(),
        protocol: Protocol::Grpc,
        trust: TrustTier::T0,
        endpoint: Endpoint {
            uri: "https://shop.internal:8443".to_owned(),
            transport: "grpc".to_owned(),
        },
        description: ConnectorDescription {
            connector_id: "shop".to_owned(),
            connector_version: format!("1.0.0-{rev_label}"),
            ngc_version: "gap/0.1".to_owned(),
            protocol: gap_proto::ngc::v1::Protocol::Grpc as i32,
            spec_hash: "11".repeat(32),
            capabilities: Some(ProtoCapabilities {
                foreach_batch: true,
                idempotency: true,
                ..ProtoCapabilities::default()
            }),
            limits: Some(ProtoLimits {
                max_batch_size: 64,
                max_in_flight: 64,
                max_page_size: 200,
                default_timeout_ms: 120_000,
                max_response_bytes: 4_194_304,
            }),
            schema_sdl: sdl.to_owned(),
            operations: vec![
                make_op("shopVersion", ProtoOpKind::Query, 1, SideEffectHint::SeNone),
                make_op("order", ProtoOpKind::Query, 2, SideEffectHint::SeNone),
                make_op("strictOrder", ProtoOpKind::Query, 2, SideEffectHint::SeNone),
                make_op("orders", ProtoOpKind::Query, 2, SideEffectHint::SeNone),
                make_op(
                    "createOrder",
                    ProtoOpKind::Mutation,
                    5,
                    SideEffectHint::SeReversible,
                ),
            ],
            meta: Some(MetaSummary {
                node_categories: Vec::new(),
                edge_predicates: Vec::new(),
                synopsis_hint: format!("shop-{rev_label}"),
            }),
            auth: None,
        },
        status: BranchStatus::Active,
    }
}

pub fn crm_branch_input(extra_field: bool) -> BranchInput {
    let extra = if extra_field {
        "\n    crmGen2Banner: String! @cost(weight: 1)"
    } else {
        ""
    };
    let sdl = format!(
        r#"
type Lead {{
    id: ID!
    email: String!
    region: String
}}

type Query {{
    lead(id: ID!, region: String = "US"): Lead @cost(weight: 3)
    leads(first: Int): [Lead!]! @cost(weight: 2, listSize: 10){extra}
}}
"#
    );

    let make_op = |name: &str, weight: u32| ProtoOp {
        name: name.to_owned(),
        kind: ProtoOpKind::Query as i32,
        description: format!("crm.{name}"),
        input_schema: None,
        output_schema: None,
        side_effect_hint: SideEffectHint::SeNone as i32,
        idempotent_hint: true,
        supports_dry_run: false,
        compensation_operation: String::new(),
        examples: Vec::new(),
        cost_weight: weight,
        pagination: 0,
        key_fields: Vec::new(),
        annotations: HashMap::new(),
    };

    let mut operations = vec![make_op("lead", 3), make_op("leads", 2)];
    if extra_field {
        operations.push(make_op("crmGen2Banner", 1));
    }

    BranchInput {
        id: BranchId::new("crm").expect("crm branch id"),
        uid: crm_uid(),
        protocol: Protocol::Rest,
        trust: TrustTier::T1,
        endpoint: Endpoint {
            uri: "https://crm.internal:8443".to_owned(),
            transport: "grpc".to_owned(),
        },
        description: ConnectorDescription {
            connector_id: "crm".to_owned(),
            connector_version: if extra_field { "2.0.0" } else { "1.0.0" }.to_owned(),
            ngc_version: "gap/0.1".to_owned(),
            protocol: gap_proto::ngc::v1::Protocol::Rest as i32,
            spec_hash: if extra_field {
                "22".repeat(32)
            } else {
                "21".repeat(32)
            },
            capabilities: Some(ProtoCapabilities::default()),
            limits: Some(ProtoLimits {
                max_batch_size: 64,
                max_in_flight: 64,
                max_page_size: 200,
                default_timeout_ms: 120_000,
                max_response_bytes: 4_194_304,
            }),
            schema_sdl: sdl,
            operations,
            meta: Some(MetaSummary {
                node_categories: Vec::new(),
                edge_predicates: Vec::new(),
                synopsis_hint: "crm-leads".to_owned(),
            }),
            auth: None,
        },
        status: BranchStatus::Active,
    }
}

pub fn compose_two_branch_snapshot(base: &Snapshot, gen2_extra: bool) -> Snapshot {
    let rev = if gen2_extra { "v2" } else { "v1" };
    let comp = compose(&[shop_branch_input(rev), crm_branch_input(gen2_extra)])
        .expect("two-branch supergraph must compose cleanly");
    assert!(
        comp.warnings.is_empty(),
        "clean branch inputs must produce zero composition warnings: {:?}",
        comp.warnings
    );
    SnapshotBuilder::new(base, comp)
        .built_at(ts(
            1_761_900_001_000 + i64::from(base.generation as u32) * 1_000
        ))
        .builder("transport-test")
        .build()
}

pub fn build_gen1_snapshot_for_tenant(tenant: TenantId) -> Snapshot {
    let base = Snapshot::empty_for_tenant(0, tenant);
    compose_two_branch_snapshot(&base, false)
}

pub fn build_gen1_snapshot() -> Snapshot {
    build_gen1_snapshot_for_tenant(TenantId::default_tenant())
}

pub fn rows_response(rows: Vec<Value>) -> QueryResponse {
    QueryResponse {
        chunk: Some(query_response::Chunk::Rows(RowSetChunk {
            foreach_index: 0,
            rows: rows
                .iter()
                .map(|val| {
                    let map = val.as_object().expect("row fixture must be a JSON object");
                    json_map_to_prost_struct(map)
                })
                .collect(),
            last_for_index: true,
            error: None,
        })),
    }
}

pub fn complete_response() -> QueryResponse {
    QueryResponse {
        chunk: Some(query_response::Chunk::Complete(Complete {
            upstream_calls: 1,
            ..Complete::default()
        })),
    }
}

pub fn stream_rows(rows: Vec<Value>) -> BoxStream<QueryResponse> {
    Box::pin(stream::iter(vec![
        Ok(rows_response(rows)),
        Ok(complete_response()),
    ]))
}

pub fn default_shop_mock() -> MockConnector {
    MockConnector::new().on_query(|req, _scope| match req.operation.as_str() {
        "shopVersion" => Ok(stream_rows(vec![json!({ "value": "shop-gen1" })])),
        "order" | "strictOrder" => {
            let args = req.arguments.as_ref().map(prost_struct_to_json_map);
            let id = args
                .as_ref()
                .and_then(|m| m.get("id"))
                .and_then(Value::as_str)
                .unwrap_or("ord-1")
                .to_owned();
            Ok(stream_rows(vec![json!({
                "id": id,
                "sku": "SKU-ALPHA",
                "totalCents": 2500
            })]))
        }
        "orders" => Ok(stream_rows(vec![
            json!({ "id": "ord-1", "sku": "SKU-1", "totalCents": 1000 }),
            json!({ "id": "ord-2", "sku": "SKU-2", "totalCents": 2000 }),
        ])),
        other => panic!("unexpected shop operation `{other}`"),
    })
}

pub fn default_crm_mock() -> MockConnector {
    MockConnector::new().on_query(|req, _scope| match req.operation.as_str() {
        "lead" => {
            let args = req.arguments.as_ref().map(prost_struct_to_json_map);
            let id = args
                .as_ref()
                .and_then(|m| m.get("id"))
                .and_then(Value::as_str)
                .unwrap_or("lead-1")
                .to_owned();
            let region = args
                .as_ref()
                .and_then(|m| m.get("region"))
                .and_then(Value::as_str)
                .unwrap_or("US")
                .to_owned();
            Ok(stream_rows(vec![json!({
                "id": id,
                "email": "ada@example.test",
                "region": region
            })]))
        }
        "leads" => Ok(stream_rows(vec![json!({
            "id": "lead-1",
            "email": "ada@example.test",
            "region": "EU"
        })])),
        "crmGen2Banner" => Ok(stream_rows(vec![json!({ "value": "gen2-only" })])),
        other => panic!("unexpected crm operation `{other}`"),
    })
}

pub fn spawn_grpc_branch(
    mock: MockConnector,
    clock: Arc<dyn Clock>,
) -> (Arc<GrpcClient>, InMemoryGrpcHandle) {
    let cfg = ServerConfig::default()
        .with_clock(clock)
        .with_max_in_flight(256);
    let core = Arc::new(ServerCore::new(mock, cfg));
    let (client, handle) = in_memory_grpc(core);
    (Arc::new(client), handle)
}

pub struct TestHarness {
    pub state: Arc<AppState>,
    pub router: Router,
    pub clock: Arc<ManualClock>,
    pub snapshot_handle: Arc<SnapshotHandle>,
    pub metrics: Arc<TransportMetrics>,
    pub shop_mock: MockConnector,
    pub crm_mock: MockConnector,
    pub _shop_grpc: InMemoryGrpcHandle,
    pub _crm_grpc: InMemoryGrpcHandle,
}

impl TestHarness {
    pub fn new(config: TransportConfig) -> Self {
        Self::with_mocks(config, default_shop_mock(), default_crm_mock(), None, None)
    }

    pub fn with_mocks(
        config: TransportConfig,
        shop_mock: MockConnector,
        crm_mock: MockConnector,
        persisted: Option<Arc<dyn PersistedOperationStore>>,
        admission: Option<Arc<dyn AdmissionPolicy>>,
    ) -> Self {
        let clock = test_clock();
        let clock_dyn: Arc<dyn Clock> = clock.clone();
        let snap = build_gen1_snapshot_for_tenant(config.default_tenant.clone());
        let snapshot_handle = Arc::new(SnapshotHandle::new(snap));

        let (shop_client, shop_grpc) = spawn_grpc_branch(shop_mock.clone(), Arc::clone(&clock_dyn));
        let (crm_client, crm_grpc) = spawn_grpc_branch(crm_mock.clone(), Arc::clone(&clock_dyn));

        let registry = Arc::new(
            ConnectorRegistry::builder()
                .register(shop_uid(), shop_client, 64)
                .register(crm_uid(), crm_client, 64)
                .build(),
        );
        let plan_cache = Arc::new(PlanCache::default());
        let metrics = Arc::new(TransportMetrics::new());

        let mut app_state = AppState::new(
            Arc::clone(&snapshot_handle),
            registry,
            plan_cache,
            config,
            Arc::clone(&metrics),
        )
        .with_clock(clock_dyn);

        if let Some(store) = persisted {
            app_state = app_state.with_persisted_store(store);
        }
        if let Some(adm) = admission {
            app_state = app_state.with_admission_policy(adm);
        }

        let state = Arc::new(app_state);
        let app_router = router(Arc::clone(&state));

        Self {
            state,
            router: app_router,
            clock,
            snapshot_handle,
            metrics,
            shop_mock,
            crm_mock,
            _shop_grpc: shop_grpc,
            _crm_grpc: crm_grpc,
        }
    }
}

pub struct ObservedHttpResponse {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub raw_body: Bytes,
}

impl ObservedHttpResponse {
    pub fn content_type(&self) -> Option<&str> {
        self.headers
            .get("content-type")
            .and_then(|v| v.to_str().ok())
    }

    pub fn json(&self) -> Value {
        serde_json::from_slice(&self.raw_body).unwrap_or_else(|e| {
            panic!(
                "expected JSON response body (status {}), error: {e}, raw: {:?}",
                self.status,
                String::from_utf8_lossy(&self.raw_body)
            )
        })
    }

    pub fn text(&self) -> String {
        String::from_utf8(self.raw_body.to_vec()).expect("valid UTF-8 body")
    }
}

pub async fn send_oneshot(app: &Router, request: Request<Body>) -> ObservedHttpResponse {
    let response: Response<Body> = app
        .clone()
        .oneshot(request)
        .await
        .expect("router service is infallible");
    let status = response.status();
    let headers = response.headers().clone();
    let raw_body = response
        .into_body()
        .collect()
        .await
        .expect("collect response body")
        .to_bytes();
    ObservedHttpResponse {
        status,
        headers,
        raw_body,
    }
}

pub fn make_api_key(id: &str, raw_key: &str, tenant_slug: &str, scopes: &[&str]) -> ApiKeyRecord {
    let scope_set: BTreeSet<String> = scopes.iter().map(|s| (*s).to_owned()).collect();
    ApiKeyRecord::from_raw_key(
        id,
        raw_key,
        TenantId::new(tenant_slug).expect("valid tenant"),
        scope_set,
    )
}

pub fn make_persisted_store(docs: &[&str]) -> (Arc<InMemoryPersistedStore>, Vec<String>) {
    let mut store = InMemoryPersistedStore::new();
    let mut hashes = Vec::with_capacity(docs.len());
    for &doc in docs {
        let id = store.register(doc);
        hashes.push(id.as_str().to_owned());
    }
    (Arc::new(store), hashes)
}
