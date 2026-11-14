//! Integration tests for `gap-ngc-grpc` (`WP-M3-06`: `spec/09-compat.md` §5.3, `NGC-020`, `NGC-025`, `NGC-026`, `SEC-011`, `INV-15`, `ADR-015`).

use std::collections::{BTreeMap, HashMap};
use std::convert::Infallible;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll};
use std::time::Duration;

use bytes::Bytes;
use futures_util::{Stream, StreamExt as _};
use gap_core::error::NgcErrorCode;
use gap_core::secret::Secret;
use gap_ngc_grpc::codec::RawBytesCodec;
use gap_ngc_grpc::reflect::{
    ErrorResponse, FileDescriptorResponse, ListServiceResponse, ReflectionRequest,
    ReflectionResponse, ServiceResponse, reflection_request, reflection_response,
};
use gap_ngc_grpc::{
    AdminClassificationPolicy, DescriptorSource, GrpcConfig, GrpcConnector, GrpcCredential,
    TrustTier, ensure_credential_transport_safe,
};
use gap_ngc_sdk::connector::{CallScope, Connector as _};
use gap_ngc_sdk::error::{json_map_to_prost_struct, prost_struct_to_json_map};
use gap_proto::ngc::v1::{
    DescribeRequest, HealthRequest, MutateRequest, MutationOutcome, OpKind, QueryRequest,
    Selection, SelectionField, ServingStatus, SideEffectHint, SubscribeRequest, VariableSet,
    query_response,
};
use http::{Request, Response};
use prost::Message as _;
use prost_reflect::{DescriptorPool, DeserializeOptions, DynamicMessage};
use prost_types::FileDescriptorSet;
use serde_json::{Map, json};
use tokio::net::TcpListener;
use tokio::sync::Mutex;
use tokio::time::Instant;
use tokio_stream::wrappers::TcpListenerStream;
use tonic::body::Body;
use tonic::server::{Grpc, NamedService, ServerStreamingService, StreamingService, UnaryService};
use tonic::transport::Server;
use tonic::{Code, Request as TonicRequest, Response as TonicResponse, Status, Streaming};
use tower::Service;

fn fixture_proto_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/grpc/inventory.proto")
}

fn fixture_proto_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/grpc")
}

fn compile_fixture_fds() -> FileDescriptorSet {
    let proto_path = fixture_proto_path();
    let proto_dir = fixture_proto_dir();
    protox::compile([proto_path], [proto_dir]).expect("fixture inventory.proto should compile")
}

fn struct_from_json(val: serde_json::Value) -> prost_types::Struct {
    let map = val.as_object().cloned().unwrap_or_else(Map::new);
    json_map_to_prost_struct(&map)
}

#[derive(Clone)]
struct RecordedRpc {
    path: String,
    authorization: Option<String>,
    custom_ascii: Option<String>,
}

struct MockGrpcState {
    pool: DescriptorPool,
    fds: FileDescriptorSet,
    stock_by_sku: Mutex<HashMap<String, i32>>,
    recorded_calls: Mutex<Vec<RecordedRpc>>,
    get_item_calls: AtomicUsize,
    reserve_calls: AtomicUsize,
    artificial_delay: Mutex<Duration>,
    reservation_seq: AtomicUsize,
}

impl MockGrpcState {
    fn new(fds: FileDescriptorSet) -> Self {
        let pool = DescriptorPool::from_file_descriptor_set(fds.clone())
            .expect("fixture FDS should decode into DescriptorPool");
        let mut stock = HashMap::new();
        stock.insert("SKU-1".to_owned(), 25);
        stock.insert("SKU-2".to_owned(), 10);
        stock.insert("SKU-3".to_owned(), 4);
        stock.insert("SKU-4".to_owned(), 99);
        stock.insert("SKU-5".to_owned(), 0);
        Self {
            pool,
            fds,
            stock_by_sku: Mutex::new(stock),
            recorded_calls: Mutex::new(Vec::new()),
            get_item_calls: AtomicUsize::new(0),
            reserve_calls: AtomicUsize::new(0),
            artificial_delay: Mutex::new(Duration::ZERO),
            reservation_seq: AtomicUsize::new(0),
        }
    }

    async fn record(&self, path: &str, meta: &tonic::metadata::MetadataMap) {
        let auth = meta
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .map(ToOwned::to_owned);
        let custom_ascii = meta
            .get("x-connector-key")
            .and_then(|v| v.to_str().ok())
            .map(ToOwned::to_owned);
        self.recorded_calls.lock().await.push(RecordedRpc {
            path: path.to_owned(),
            authorization: auth,
            custom_ascii,
        });
    }

    fn encode_json_msg(
        &self,
        full_name: &str,
        json_val: &serde_json::Value,
    ) -> Result<Bytes, Status> {
        let desc = self
            .pool
            .get_message_by_name(full_name)
            .ok_or_else(|| Status::internal(format!("missing descriptor `{full_name}`")))?;
        let encoded_json = json_val.to_string();
        let mut de = serde_json::Deserializer::from_str(&encoded_json);
        let opts = DeserializeOptions::new().deny_unknown_fields(false);
        let dyn_msg = DynamicMessage::deserialize_with_options(desc, &mut de, &opts)
            .map_err(|e| Status::internal(format!("dynamic deserialize failed: {e}")))?;
        Ok(Bytes::from(dyn_msg.encode_to_vec()))
    }

    fn decode_msg_json(&self, full_name: &str, raw: &[u8]) -> Result<serde_json::Value, Status> {
        let desc = self
            .pool
            .get_message_by_name(full_name)
            .ok_or_else(|| Status::internal(format!("missing descriptor `{full_name}`")))?;
        let dyn_msg = DynamicMessage::decode(desc, raw)
            .map_err(|e| Status::invalid_argument(format!("bad protobuf request: {e}")))?;
        let mut buf = Vec::new();
        let mut ser = serde_json::Serializer::new(&mut buf);
        let opts = prost_reflect::SerializeOptions::new()
            .use_proto_field_name(true)
            .skip_default_fields(false)
            .use_enum_numbers(false);
        dyn_msg
            .serialize_with_options(&mut ser, &opts)
            .map_err(|e| Status::internal(format!("dynamic serialize failed: {e}")))?;
        serde_json::from_slice(&buf).map_err(|e| Status::internal(e.to_string()))
    }
}

#[derive(Clone)]
struct MockInventoryService {
    state: Arc<MockGrpcState>,
}

impl NamedService for MockInventoryService {
    const NAME: &'static str = "acme.inventory.v1.InventoryService";
}

impl Service<Request<Body>> for MockInventoryService {
    type Response = Response<Body>;
    type Error = Infallible;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: Request<Body>) -> Self::Future {
        let state = Arc::clone(&self.state);
        Box::pin(async move {
            let path = req.uri().path().to_owned();
            let mut grpc = Grpc::new(RawBytesCodec);
            let resp = match path.as_str() {
                "/acme.inventory.v1.InventoryService/GetItem" => {
                    struct Handler(Arc<MockGrpcState>);
                    impl UnaryService<Bytes> for Handler {
                        type Response = Bytes;
                        type Future = Pin<
                            Box<dyn Future<Output = Result<TonicResponse<Bytes>, Status>> + Send>,
                        >;
                        fn call(&mut self, req: TonicRequest<Bytes>) -> Self::Future {
                            let st = Arc::clone(&self.0);
                            Box::pin(async move {
                                st.record(
                                    "/acme.inventory.v1.InventoryService/GetItem",
                                    req.metadata(),
                                )
                                .await;
                                st.get_item_calls.fetch_add(1, Ordering::SeqCst);
                                let delay = *st.artificial_delay.lock().await;
                                if !delay.is_zero() {
                                    tokio::time::sleep(delay).await;
                                }
                                let body = st.decode_msg_json(
                                    "acme.inventory.v1.GetItemRequest",
                                    req.get_ref(),
                                )?;
                                let sku = body["sku"].as_str().unwrap_or_default().to_owned();
                                let include_dims =
                                    body["include_dimensions"].as_bool().unwrap_or(false);
                                let stock =
                                    st.stock_by_sku.lock().await.get(&sku).copied().unwrap_or(0);
                                let status_str = if stock > 0 {
                                    "ITEM_STATUS_AVAILABLE"
                                } else {
                                    "ITEM_STATUS_BACKORDERED"
                                };
                                let dims = if include_dims {
                                    json!({ "width_mm": 120, "height_mm": 45 })
                                } else {
                                    serde_json::Value::Null
                                };
                                let mut payload = json!({
                                    "sku": sku,
                                    "name": format!("Widget {sku}"),
                                    "stock": stock,
                                    "status": status_str,
                                    "tags": ["warehouse-a", "fast-ship"],
                                    "labels": { "aisle": "B4", "tier": "2" }
                                });
                                if !dims.is_null() {
                                    payload["dimensions"] = dims;
                                }
                                let bytes = st.encode_json_msg(
                                    "acme.inventory.v1.GetItemResponse",
                                    &payload,
                                )?;
                                Ok(TonicResponse::new(bytes))
                            })
                        }
                    }
                    grpc.unary(Handler(state), req).await
                }
                "/acme.inventory.v1.InventoryService/CheckAvailability" => {
                    struct Handler(Arc<MockGrpcState>);
                    impl UnaryService<Bytes> for Handler {
                        type Response = Bytes;
                        type Future = Pin<
                            Box<dyn Future<Output = Result<TonicResponse<Bytes>, Status>> + Send>,
                        >;
                        fn call(&mut self, req: TonicRequest<Bytes>) -> Self::Future {
                            let st = Arc::clone(&self.0);
                            Box::pin(async move {
                                st.record(
                                    "/acme.inventory.v1.InventoryService/CheckAvailability",
                                    req.metadata(),
                                )
                                .await;
                                let body = st.decode_msg_json(
                                    "acme.inventory.v1.CheckAvailabilityRequest",
                                    req.get_ref(),
                                )?;
                                let sku = body["sku"].as_str().unwrap_or_default();
                                let stock =
                                    st.stock_by_sku.lock().await.get(sku).copied().unwrap_or(0);
                                let bytes = st.encode_json_msg(
                                    "acme.inventory.v1.CheckAvailabilityResponse",
                                    &json!({ "sku": sku, "in_stock": stock > 0 }),
                                )?;
                                Ok(TonicResponse::new(bytes))
                            })
                        }
                    }
                    grpc.unary(Handler(state), req).await
                }
                "/acme.inventory.v1.InventoryService/ReserveItem" => {
                    struct Handler(Arc<MockGrpcState>);
                    impl UnaryService<Bytes> for Handler {
                        type Response = Bytes;
                        type Future = Pin<
                            Box<dyn Future<Output = Result<TonicResponse<Bytes>, Status>> + Send>,
                        >;
                        fn call(&mut self, req: TonicRequest<Bytes>) -> Self::Future {
                            let st = Arc::clone(&self.0);
                            Box::pin(async move {
                                st.record(
                                    "/acme.inventory.v1.InventoryService/ReserveItem",
                                    req.metadata(),
                                )
                                .await;
                                st.reserve_calls.fetch_add(1, Ordering::SeqCst);
                                let body = st.decode_msg_json(
                                    "acme.inventory.v1.ReserveItemRequest",
                                    req.get_ref(),
                                )?;
                                let sku = body["sku"].as_str().unwrap_or_default().to_owned();
                                let qty = i32::try_from(body["quantity"].as_i64().unwrap_or(0))
                                    .unwrap_or(0);
                                let mut map = st.stock_by_sku.lock().await;
                                let current = map.get(&sku).copied().unwrap_or(0);
                                if qty <= 0 || current < qty {
                                    return Err(Status::failed_precondition(
                                        "insufficient stock for reservation",
                                    ));
                                }
                                let remaining = current - qty;
                                map.insert(sku, remaining);
                                let seq = st.reservation_seq.fetch_add(1, Ordering::SeqCst) + 1;
                                let bytes = st.encode_json_msg(
                                    "acme.inventory.v1.ReserveItemResponse",
                                    &json!({
                                        "reservation_id": format!("RES-{seq}"),
                                        "remaining_stock": remaining
                                    }),
                                )?;
                                Ok(TonicResponse::new(bytes))
                            })
                        }
                    }
                    grpc.unary(Handler(state), req).await
                }
                "/acme.inventory.v1.InventoryService/WatchStock" => {
                    struct Handler(Arc<MockGrpcState>);
                    impl ServerStreamingService<Bytes> for Handler {
                        type Response = Bytes;
                        type ResponseStream =
                            Pin<Box<dyn Stream<Item = Result<Bytes, Status>> + Send>>;
                        type Future = Pin<
                            Box<
                                dyn Future<
                                        Output = Result<
                                            TonicResponse<Self::ResponseStream>,
                                            Status,
                                        >,
                                    > + Send,
                            >,
                        >;
                        fn call(&mut self, req: TonicRequest<Bytes>) -> Self::Future {
                            let st = Arc::clone(&self.0);
                            Box::pin(async move {
                                st.record(
                                    "/acme.inventory.v1.InventoryService/WatchStock",
                                    req.metadata(),
                                )
                                .await;
                                let ev1 = st.encode_json_msg(
                                    "acme.inventory.v1.StockEvent",
                                    &json!({ "event_id": "ev-1", "sku": "SKU-1", "delta": -2 }),
                                )?;
                                let ev2 = st.encode_json_msg(
                                    "acme.inventory.v1.StockEvent",
                                    &json!({ "event_id": "ev-2", "sku": "SKU-2", "delta": 5 }),
                                )?;
                                let s: Self::ResponseStream =
                                    Box::pin(tokio_stream::iter([Ok(ev1), Ok(ev2)]));
                                Ok(TonicResponse::new(s))
                            })
                        }
                    }
                    grpc.server_streaming(Handler(state), req).await
                }
                other => {
                    let status = Status::unimplemented(format!("unknown RPC `{other}`"));
                    status.into_http()
                }
            };
            Ok(resp)
        })
    }
}

fn handle_reflection_request(state: &MockGrpcState, req: ReflectionRequest) -> ReflectionResponse {
    let msg = match req.message_request {
        Some(reflection_request::MessageRequest::ListServices(_)) => Some(
            reflection_response::MessageResponse::ListServicesResponse(ListServiceResponse {
                service: vec![ServiceResponse {
                    name: "acme.inventory.v1.InventoryService".to_owned(),
                }],
            }),
        ),
        Some(reflection_request::MessageRequest::FileContainingSymbol(sym)) => {
            if state.pool.get_service_by_name(&sym).is_some()
                || state.pool.get_message_by_name(&sym).is_some()
            {
                let encoded: Vec<Vec<u8>> =
                    state.fds.file.iter().map(|f| f.encode_to_vec()).collect();
                Some(
                    reflection_response::MessageResponse::FileDescriptorResponse(
                        FileDescriptorResponse {
                            file_descriptor_proto: encoded,
                        },
                    ),
                )
            } else {
                Some(reflection_response::MessageResponse::ErrorResponse(
                    ErrorResponse {
                        error_code: Code::NotFound as i32,
                        error_message: format!("symbol not found: {sym}"),
                    },
                ))
            }
        }
        Some(reflection_request::MessageRequest::FileByFilename(name)) => {
            let matches: Vec<Vec<u8>> = state
                .fds
                .file
                .iter()
                .filter(|f| f.name() == name)
                .map(|f| f.encode_to_vec())
                .collect();
            if matches.is_empty() {
                Some(reflection_response::MessageResponse::ErrorResponse(
                    ErrorResponse {
                        error_code: Code::NotFound as i32,
                        error_message: format!("file not found: {name}"),
                    },
                ))
            } else {
                Some(
                    reflection_response::MessageResponse::FileDescriptorResponse(
                        FileDescriptorResponse {
                            file_descriptor_proto: matches,
                        },
                    ),
                )
            }
        }
        None => Some(reflection_response::MessageResponse::ErrorResponse(
            ErrorResponse {
                error_code: Code::InvalidArgument as i32,
                error_message: "empty reflection request".to_owned(),
            },
        )),
    };
    ReflectionResponse {
        valid_host: String::new(),
        message_response: msg,
    }
}

#[derive(Clone)]
struct MockReflectionV1Service {
    state: Arc<MockGrpcState>,
}

impl NamedService for MockReflectionV1Service {
    const NAME: &'static str = "grpc.reflection.v1.ServerReflection";
}

impl Service<Request<Body>> for MockReflectionV1Service {
    type Response = Response<Body>;
    type Error = Infallible;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: Request<Body>) -> Self::Future {
        let state = Arc::clone(&self.state);
        Box::pin(async move {
            let mut grpc = Grpc::new(RawBytesCodec);
            struct Handler(Arc<MockGrpcState>);
            impl StreamingService<Bytes> for Handler {
                type Response = Bytes;
                type ResponseStream = Pin<Box<dyn Stream<Item = Result<Bytes, Status>> + Send>>;
                type Future = Pin<
                    Box<
                        dyn Future<Output = Result<TonicResponse<Self::ResponseStream>, Status>>
                            + Send,
                    >,
                >;
                fn call(&mut self, req: TonicRequest<Streaming<Bytes>>) -> Self::Future {
                    let st = Arc::clone(&self.0);
                    Box::pin(async move {
                        let inbound = req.into_inner();
                        let s = futures_util::stream::unfold(
                            (inbound, st),
                            |(mut inbound, st)| async move {
                                match inbound.message().await {
                                    Ok(Some(frame)) => {
                                        let res = match ReflectionRequest::decode(frame) {
                                            Ok(decoded) => {
                                                let reply = handle_reflection_request(&st, decoded);
                                                Ok(Bytes::from(reply.encode_to_vec()))
                                            }
                                            Err(e) => Err(Status::invalid_argument(e.to_string())),
                                        };
                                        Some((res, (inbound, st)))
                                    }
                                    Ok(None) => None,
                                    Err(status) => Some((Err(status), (inbound, st))),
                                }
                            },
                        );
                        let boxed: Self::ResponseStream = Box::pin(s);
                        Ok(TonicResponse::new(boxed))
                    })
                }
            }
            Ok(grpc.streaming(Handler(state), req).await)
        })
    }
}

#[derive(Clone)]
struct MockReflectionV1AlphaService {
    state: Arc<MockGrpcState>,
}

impl NamedService for MockReflectionV1AlphaService {
    const NAME: &'static str = "grpc.reflection.v1alpha.ServerReflection";
}

impl Service<Request<Body>> for MockReflectionV1AlphaService {
    type Response = Response<Body>;
    type Error = Infallible;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: Request<Body>) -> Self::Future {
        let state = Arc::clone(&self.state);
        Box::pin(async move {
            let mut grpc = Grpc::new(RawBytesCodec);
            struct Handler(Arc<MockGrpcState>);
            impl StreamingService<Bytes> for Handler {
                type Response = Bytes;
                type ResponseStream = Pin<Box<dyn Stream<Item = Result<Bytes, Status>> + Send>>;
                type Future = Pin<
                    Box<
                        dyn Future<Output = Result<TonicResponse<Self::ResponseStream>, Status>>
                            + Send,
                    >,
                >;
                fn call(&mut self, req: TonicRequest<Streaming<Bytes>>) -> Self::Future {
                    let st = Arc::clone(&self.0);
                    Box::pin(async move {
                        let inbound = req.into_inner();
                        let s = futures_util::stream::unfold(
                            (inbound, st),
                            |(mut inbound, st)| async move {
                                match inbound.message().await {
                                    Ok(Some(frame)) => {
                                        let res = match ReflectionRequest::decode(frame) {
                                            Ok(decoded) => {
                                                let reply = handle_reflection_request(&st, decoded);
                                                Ok(Bytes::from(reply.encode_to_vec()))
                                            }
                                            Err(e) => Err(Status::invalid_argument(e.to_string())),
                                        };
                                        Some((res, (inbound, st)))
                                    }
                                    Ok(None) => None,
                                    Err(status) => Some((Err(status), (inbound, st))),
                                }
                            },
                        );
                        let boxed: Self::ResponseStream = Box::pin(s);
                        Ok(TonicResponse::new(boxed))
                    })
                }
            }
            Ok(grpc.streaming(Handler(state), req).await)
        })
    }
}

enum ReflectionMode {
    V1,
    V1AlphaOnly,
}

async fn spawn_mock_grpc_server(
    mode: ReflectionMode,
) -> (String, Arc<MockGrpcState>, tokio::task::JoinHandle<()>) {
    let fds = compile_fixture_fds();
    let state = Arc::new(MockGrpcState::new(fds));
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind loopback gRPC port");
    let addr = listener.local_addr().expect("local addr");
    let endpoint = format!("http://{addr}");

    let inv_svc = MockInventoryService {
        state: Arc::clone(&state),
    };
    let incoming = TcpListenerStream::new(listener);

    let handle = match mode {
        ReflectionMode::V1 => {
            let refl = MockReflectionV1Service {
                state: Arc::clone(&state),
            };
            tokio::spawn(async move {
                let _ = Server::builder()
                    .add_service(inv_svc)
                    .add_service(refl)
                    .serve_with_incoming(incoming)
                    .await;
            })
        }
        ReflectionMode::V1AlphaOnly => {
            let refl = MockReflectionV1AlphaService {
                state: Arc::clone(&state),
            };
            tokio::spawn(async move {
                let _ = Server::builder()
                    .add_service(inv_svc)
                    .add_service(refl)
                    .serve_with_incoming(incoming)
                    .await;
            })
        }
    };

    (endpoint, state, handle)
}

// See ADR-015.
// spec: CMP-050, NGC-014
#[tokio::test]
async fn grpc_descriptor_sources_and_t2_vs_t1_classification() {
    let fds = compile_fixture_fds();
    let fds_bytes = fds.encode_to_vec();
    let proto_text = std::fs::read_to_string(fixture_proto_path()).expect("read fixture proto");

    // 1) TrustTier::T2 ignores idempotency_level even when accept_idempotency_options is true.
    let cfg_t2 = GrpcConfig {
        connector_id: "inv-t2".to_owned(),
        source: DescriptorSource::FileDescriptorSet(fds_bytes),
        trust_tier: TrustTier::T2,
        admin_policy: AdminClassificationPolicy {
            accept_idempotency_options: true,
            ..AdminClassificationPolicy::default()
        },
        ..GrpcConfig::default()
    };
    let conn_t2 = GrpcConnector::connect(cfg_t2)
        .await
        .expect("connect FDS descriptor source");
    let scope = CallScope::for_test(Instant::now() + Duration::from_secs(5));
    let desc_t2 = conn_t2
        .describe(DescribeRequest::default(), scope.clone())
        .await
        .expect("describe T2")
        .description
        .expect("description present");

    let ops_t2: BTreeMap<&str, _> = desc_t2
        .operations
        .iter()
        .map(|o| (o.name.as_str(), o))
        .collect();
    assert_eq!(ops_t2.len(), 4);
    assert_eq!(ops_t2["InventoryService_GetItem"].kind(), OpKind::Mutation);
    assert_eq!(
        ops_t2["InventoryService_GetItem"].side_effect_hint(),
        SideEffectHint::SideEffectUnspecified
    );
    assert_eq!(
        ops_t2["InventoryService_CheckAvailability"].kind(),
        OpKind::Mutation
    );
    assert_eq!(
        ops_t2["InventoryService_ReserveItem"].kind(),
        OpKind::Mutation
    );
    assert_eq!(
        ops_t2["InventoryService_WatchStock"].kind(),
        OpKind::Subscription
    );

    // Conditional hash caching (`known_spec_hash`).
    let cached = conn_t2
        .describe(
            DescribeRequest {
                known_spec_hash: desc_t2.spec_hash.clone(),
            },
            scope.clone(),
        )
        .await
        .expect("cached describe");
    assert!(cached.not_modified);
    assert!(cached.description.is_none());

    // 2) TrustTier::T1 with accept_idempotency_options = true promotes NO_SIDE_EFFECTS & IDEMPOTENT to Query.
    let cfg_t1 = GrpcConfig {
        connector_id: "inv-t1".to_owned(),
        source: DescriptorSource::ProtoFiles {
            files: vec![fixture_proto_path()],
            include_dirs: vec![fixture_proto_dir()],
        },
        trust_tier: TrustTier::T1,
        admin_policy: AdminClassificationPolicy {
            accept_idempotency_options: true,
            ..AdminClassificationPolicy::default()
        },
        ..GrpcConfig::default()
    };
    let conn_t1 = GrpcConnector::connect(cfg_t1)
        .await
        .expect("connect ProtoFiles source");
    let desc_t1 = conn_t1
        .describe(DescribeRequest::default(), scope.clone())
        .await
        .expect("describe T1")
        .description
        .expect("description");
    let ops_t1: BTreeMap<&str, _> = desc_t1
        .operations
        .iter()
        .map(|o| (o.name.as_str(), o))
        .collect();
    assert_eq!(ops_t1["InventoryService_GetItem"].kind(), OpKind::Query);
    assert_eq!(
        ops_t1["InventoryService_GetItem"].side_effect_hint(),
        SideEffectHint::SeNone
    );
    assert_eq!(
        ops_t1["InventoryService_CheckAvailability"].kind(),
        OpKind::Query
    );
    assert_eq!(
        ops_t1["InventoryService_ReserveItem"].kind(),
        OpKind::Mutation
    );
    assert_eq!(
        ops_t1["InventoryService_WatchStock"].kind(),
        OpKind::Subscription
    );

    // Generated GAP-QL SDL exposes extracted operations and nested types.
    assert!(desc_t1.schema_sdl.contains("InventoryService_GetItem"));
    assert!(desc_t1.schema_sdl.contains("Dimensions"));

    // 3) InlineProtos + explicit approved_query_methods allow-list selectively elevates GetItem only.
    let mut approved = std::collections::BTreeSet::new();
    approved.insert("acme.inventory.v1.InventoryService/GetItem".to_owned());
    let cfg_allow = GrpcConfig {
        connector_id: "inv-allow".to_owned(),
        source: DescriptorSource::InlineProtos(vec![(
            "acme/inventory/v1/inventory.proto".to_owned(),
            proto_text,
        )]),
        trust_tier: TrustTier::T1,
        admin_policy: AdminClassificationPolicy {
            accept_idempotency_options: false,
            approved_query_methods: approved,
            ..AdminClassificationPolicy::default()
        },
        ..GrpcConfig::default()
    };
    let conn_allow = GrpcConnector::connect(cfg_allow)
        .await
        .expect("connect InlineProtos source");
    let desc_allow = conn_allow
        .describe(DescribeRequest::default(), scope)
        .await
        .expect("describe allowlist")
        .description
        .expect("description");
    let ops_allow: BTreeMap<&str, _> = desc_allow
        .operations
        .iter()
        .map(|o| (o.name.as_str(), o))
        .collect();
    assert_eq!(ops_allow["InventoryService_GetItem"].kind(), OpKind::Query);
    assert_eq!(
        ops_allow["InventoryService_CheckAvailability"].kind(),
        OpKind::Mutation
    );
}

// spec: CMP-050
#[tokio::test]
async fn grpc_live_server_reflection_v1_and_v1alpha_fallback() {
    let (ep_v1, _st_v1, h_v1) = spawn_mock_grpc_server(ReflectionMode::V1).await;
    let conn_v1 = GrpcConnector::connect(GrpcConfig {
        connector_id: "refl-v1".to_owned(),
        endpoint: ep_v1,
        source: DescriptorSource::ServerReflection {
            services: Vec::new(),
        },
        trust_tier: TrustTier::T1,
        admin_policy: AdminClassificationPolicy {
            accept_idempotency_options: true,
            ..AdminClassificationPolicy::default()
        },
        ..GrpcConfig::default()
    })
    .await
    .expect("v1 reflection connect should succeed");
    assert_eq!(conn_v1.schema_bundle().methods.len(), 4);
    h_v1.abort();

    let (ep_alpha, _st_alpha, h_alpha) = spawn_mock_grpc_server(ReflectionMode::V1AlphaOnly).await;
    let conn_alpha = GrpcConnector::connect(GrpcConfig {
        connector_id: "refl-v1alpha".to_owned(),
        endpoint: ep_alpha,
        source: DescriptorSource::ServerReflection {
            services: vec!["acme.inventory.v1.InventoryService".to_owned()],
        },
        trust_tier: TrustTier::T1,
        admin_policy: AdminClassificationPolicy {
            accept_idempotency_options: true,
            ..AdminClassificationPolicy::default()
        },
        ..GrpcConfig::default()
    })
    .await
    .expect("v1alpha fallback reflection should succeed");
    assert_eq!(conn_alpha.schema_bundle().methods.len(), 4);
    h_alpha.abort();
}

// spec: CMP-050, NGC-020, NGC-026
#[tokio::test]
async fn grpc_query_foreach_dedup_mutate_and_subscribe() {
    let (endpoint, state, handle) = spawn_mock_grpc_server(ReflectionMode::V1).await;
    let connector = GrpcConnector::connect(GrpcConfig {
        connector_id: "inv-live".to_owned(),
        endpoint,
        source: DescriptorSource::ServerReflection {
            services: Vec::new(),
        },
        trust_tier: TrustTier::T1,
        admin_policy: AdminClassificationPolicy {
            accept_idempotency_options: true,
            ..AdminClassificationPolicy::default()
        },
        max_batch_size: 32,
        max_in_flight: 4,
        supports_dry_run: true,
        ..GrpcConfig::default()
    })
    .await
    .expect("live gRPC connector should connect");

    let scope = CallScope::for_test(Instant::now() + Duration::from_secs(10));

    // 1) Health check passes.
    let health = connector
        .health(HealthRequest {}, scope.clone())
        .await
        .expect("health should succeed");
    assert_eq!(health.status(), ServingStatus::Serving);

    // 2) Single unary Query + Selection projection + complex protobuf field types.
    let mut stream = connector
        .query(
            QueryRequest {
                operation: "InventoryService_GetItem".to_owned(),
                arguments: Some(struct_from_json(json!({
                    "sku": "SKU-1",
                    "include_dimensions": true
                }))),
                selection: Some(Selection {
                    fields: vec![
                        SelectionField {
                            name: "sku".to_owned(),
                            ..SelectionField::default()
                        },
                        SelectionField {
                            name: "status".to_owned(),
                            ..SelectionField::default()
                        },
                        SelectionField {
                            name: "dimensions".to_owned(),
                            children: Some(Selection {
                                fields: vec![SelectionField {
                                    name: "width_mm".to_owned(),
                                    ..SelectionField::default()
                                }],
                            }),
                            ..SelectionField::default()
                        },
                        SelectionField {
                            name: "tags".to_owned(),
                            ..SelectionField::default()
                        },
                        SelectionField {
                            name: "labels".to_owned(),
                            ..SelectionField::default()
                        },
                    ],
                }),
                ..QueryRequest::default()
            },
            scope.clone(),
        )
        .await
        .expect("single query");

    let mut rows = Vec::new();
    let mut complete = None;
    while let Some(item) = stream.next().await {
        let resp = item.expect("query chunk ok");
        match resp.chunk {
            Some(query_response::Chunk::Rows(c)) => {
                for r in c.rows {
                    rows.push(prost_struct_to_json_map(&r));
                }
            }
            Some(query_response::Chunk::Complete(c)) => complete = Some(c),
            None => {}
        }
    }
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get("sku"), Some(&json!("SKU-1")));
    assert_eq!(rows[0].get("status"), Some(&json!("ITEM_STATUS_AVAILABLE")));
    assert_eq!(
        rows[0].get("dimensions"),
        Some(&json!({ "width_mm": 120.0 }))
    );
    assert_eq!(
        rows[0].get("tags"),
        Some(&json!(["warehouse-a", "fast-ship"]))
    );
    assert_eq!(
        rows[0].get("labels"),
        Some(&json!({ "aisle": "B4", "tier": "2" }))
    );
    let comp = complete.expect("complete chunk");
    assert_eq!(comp.upstream_calls, 1);

    // 3) Emulated `foreach` deduplication and honest `Complete.upstream_calls == N_distinct` (`NGC-020`, `NGC-026`).
    state.get_item_calls.store(0, Ordering::SeqCst);
    let foreach_vars: Vec<VariableSet> = [
        "SKU-1", "SKU-2", "SKU-1", "SKU-3", "SKU-2", "SKU-4", "SKU-5",
    ]
    .into_iter()
    .enumerate()
    .map(|(i, sku)| VariableSet {
        index: u32::try_from(i).expect("index fits u32"),
        values: Some(struct_from_json(json!({ "sku": sku }))),
    })
    .collect();

    let mut fe_stream = connector
        .query(
            QueryRequest {
                operation: "InventoryService_GetItem".to_owned(),
                foreach: foreach_vars,
                ..QueryRequest::default()
            },
            scope.clone(),
        )
        .await
        .expect("foreach query");

    let mut by_idx: BTreeMap<u32, String> = BTreeMap::new();
    let mut fe_complete = None;
    while let Some(item) = fe_stream.next().await {
        let resp = item.expect("foreach chunk ok");
        match resp.chunk {
            Some(query_response::Chunk::Rows(c)) => {
                let idx = c.foreach_index;
                if let Some(first_row) = c.rows.first() {
                    let obj = prost_struct_to_json_map(first_row);
                    if let Some(sku) = obj.get("sku").and_then(|v| v.as_str()) {
                        by_idx.insert(idx, sku.to_owned());
                    }
                }
            }
            Some(query_response::Chunk::Complete(c)) => fe_complete = Some(c),
            None => {}
        }
    }
    assert_eq!(by_idx.len(), 7);
    assert_eq!(by_idx[&0], "SKU-1");
    assert_eq!(by_idx[&2], "SKU-1");
    assert_eq!(by_idx[&4], "SKU-2");
    assert_eq!(by_idx[&6], "SKU-5");
    assert_eq!(state.get_item_calls.load(Ordering::SeqCst), 5);
    assert_eq!(fe_complete.expect("complete").upstream_calls, 5);

    // 4) Mutate with `dry_run = true` performs zero upstream calls; `dry_run = false` mutates upstream.
    let dry_resp = connector
        .mutate(
            MutateRequest {
                operation: "InventoryService_ReserveItem".to_owned(),
                arguments: Some(struct_from_json(json!({ "sku": "SKU-1", "quantity": 3 }))),
                dry_run: true,
                ..MutateRequest::default()
            },
            scope.clone(),
        )
        .await
        .expect("dry_run mutate");
    assert_eq!(dry_resp.outcome(), MutationOutcome::OutcomeDryRun);
    assert_eq!(state.reserve_calls.load(Ordering::SeqCst), 0);

    let applied_resp = connector
        .mutate(
            MutateRequest {
                operation: "InventoryService_ReserveItem".to_owned(),
                arguments: Some(struct_from_json(json!({ "sku": "SKU-1", "quantity": 3 }))),
                dry_run: false,
                ..MutateRequest::default()
            },
            scope.clone(),
        )
        .await
        .expect("applied mutate");
    assert_eq!(applied_resp.outcome(), MutationOutcome::OutcomeApplied);
    assert_eq!(state.reserve_calls.load(Ordering::SeqCst), 1);
    let res_obj = prost_struct_to_json_map(&applied_resp.result.expect("mutate result"));
    assert_eq!(res_obj.get("reservation_id"), Some(&json!("RES-1")));
    assert_eq!(res_obj.get("remaining_stock"), Some(&json!(22.0)));

    // 5) Server-streaming Subscribe yields streamed events.
    let mut ev_stream = connector
        .subscribe(
            SubscribeRequest {
                operation: "InventoryService_WatchStock".to_owned(),
                arguments: Some(struct_from_json(json!({ "warehouse_id": "WH-1" }))),
                ..SubscribeRequest::default()
            },
            scope,
        )
        .await
        .expect("subscribe stream");
    let mut events = Vec::new();
    while let Some(ev) = ev_stream.next().await {
        events.push(ev.expect("stream event ok"));
    }
    assert_eq!(events.len(), 2);
    let ev1_map = prost_struct_to_json_map(&events[0].payload.clone().expect("ev1 payload"));
    assert_eq!(ev1_map.get("event_id"), Some(&json!("ev-1")));
    assert_eq!(events[1].resume_token, "2");

    handle.abort();
}

// spec: NGC-025, SEC-011, INV-15
#[tokio::test]
async fn inv_15_grpc_per_method_deadlines_credentials_redaction_and_no_bearer_passthrough() {
    // 1) Credential Debug redacts secret tokens across all variants (`INV-15`).
    let bearer = GrpcCredential::Bearer {
        token: Secret::new("top-secret-bearer-token-999".to_owned()),
    };
    let ascii = GrpcCredential::AsciiHeader {
        key: "x-connector-key".to_owned(),
        value: Secret::new("super-secret-ascii-key".to_owned()),
    };
    let binary = GrpcCredential::BinaryHeader {
        key: "x-connector-bin".to_owned(),
        value: Secret::new(vec![0xde, 0xad, 0xbe, 0xef]),
    };
    let dbg_bearer = format!("{bearer:?}");
    let dbg_ascii = format!("{ascii:?}");
    let dbg_binary = format!("{binary:?}");
    assert!(dbg_bearer.contains("***") && !dbg_bearer.contains("top-secret"));
    assert!(dbg_ascii.contains("***") && !dbg_ascii.contains("super-secret"));
    assert!(dbg_binary.contains("***") && !dbg_binary.contains("222"));

    // 2) Cleartext non-loopback HTTP credential refusal (`SEC-011`).
    let remote_err = ensure_credential_transport_safe("http://inventory.internal:50051", false)
        .expect_err("non-loopback cleartext HTTP with credentials must be rejected");
    assert_eq!(remote_err.code, NgcErrorCode::FailedPrecondition);
    ensure_credential_transport_safe("https://inventory.internal:50051", false)
        .expect("HTTPS remote allowed");
    ensure_credential_transport_safe("http://127.0.0.1:50051", false)
        .expect("HTTP loopback allowed");

    // 3) No caller bearer passthrough + static configured credential attachment + per-method deadline.
    let (endpoint, state, handle) = spawn_mock_grpc_server(ReflectionMode::V1).await;
    let mut deadlines = BTreeMap::new();
    deadlines.insert(
        "InventoryService_GetItem".to_owned(),
        Duration::from_millis(50),
    );
    let connector = GrpcConnector::connect(GrpcConfig {
        connector_id: "inv-sec".to_owned(),
        endpoint,
        source: DescriptorSource::ServerReflection {
            services: Vec::new(),
        },
        trust_tier: TrustTier::T1,
        admin_policy: AdminClassificationPolicy {
            accept_idempotency_options: true,
            ..AdminClassificationPolicy::default()
        },
        credentials: vec![bearer, ascii],
        method_deadlines: deadlines,
        ..GrpcConfig::default()
    })
    .await
    .expect("connect with static credentials");

    let mut scope = CallScope::for_test(Instant::now() + Duration::from_secs(10));
    scope.metadata.insert(
        "authorization".to_owned(),
        "Bearer caller-enduser-secret-never-forward".to_owned(),
    );

    let mut stream = connector
        .query(
            QueryRequest {
                operation: "InventoryService_GetItem".to_owned(),
                arguments: Some(struct_from_json(json!({ "sku": "SKU-1" }))),
                ..QueryRequest::default()
            },
            scope.clone(),
        )
        .await
        .expect("fast call with static auth");
    while let Some(item) = stream.next().await {
        item.expect("chunk ok");
    }

    let recorded = state.recorded_calls.lock().await;
    let get_calls: Vec<_> = recorded
        .iter()
        .filter(|r| r.path == "/acme.inventory.v1.InventoryService/GetItem")
        .collect();
    assert_eq!(get_calls.len(), 1);
    assert_eq!(
        get_calls[0].authorization.as_deref(),
        Some("Bearer top-secret-bearer-token-999")
    );
    assert_eq!(
        get_calls[0].custom_ascii.as_deref(),
        Some("super-secret-ascii-key")
    );
    drop(recorded);

    // 4) Per-method deadline triggers DEADLINE_EXCEEDED (`NGC-025`).
    *state.artificial_delay.lock().await = Duration::from_millis(250);
    let err = connector
        .query(
            QueryRequest {
                operation: "InventoryService_GetItem".to_owned(),
                arguments: Some(struct_from_json(json!({ "sku": "SKU-1" }))),
                ..QueryRequest::default()
            },
            scope,
        )
        .await
        .err()
        .expect("slow call should hit per-method 50ms deadline");
    assert_eq!(err.code, NgcErrorCode::DeadlineExceeded);

    handle.abort();
}
