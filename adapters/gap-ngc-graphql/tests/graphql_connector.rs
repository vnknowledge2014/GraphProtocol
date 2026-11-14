//! Integration tests for `gap-ngc-graphql` (`WP-M3-06`: `spec/09-compat.md` §5.4, `CMP-050`, `NGC-020`, `NGC-025`, `NGC-026`, `ADR-023`, `SEC-011`, `INV-15`, `INV-18`).

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, Response, StatusCode};
use axum::routing::post;
use futures_util::StreamExt as _;
use gap_core::error::NgcErrorCode;
use gap_core::secret::Secret;
use gap_ngc_graphql::transport::GRAPHQL_RESPONSE_JSON;
use gap_ngc_graphql::{
    EntityBatchMapping, GraphqlConfig, GraphqlConnector, GraphqlCredential, GraphqlWireResponse,
    UpstreamSchemaSource, interpret_wire_response, rewrite_executable_document_types,
    validate_endpoint,
};
use gap_ngc_sdk::connector::{CallScope, Connector as _};
use gap_ngc_sdk::error::{json_map_to_prost_struct, prost_struct_to_json_map};
use gap_proto::ngc::v1::{
    DescribeRequest, HealthRequest, MutateRequest, MutationOutcome, OpKind, Page, PaginationKind,
    QueryRequest, Selection, SelectionField, ServingStatus, VariableSet, query_response,
};
use serde_json::{Map, Value, json};
use tokio::net::TcpListener;
use tokio::sync::Mutex;
use tokio::time::Instant;

fn fixture_sdl_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/graphql/inventory.graphql")
}

fn load_fixture_sdl() -> String {
    std::fs::read_to_string(fixture_sdl_path()).expect("read inventory.graphql fixture")
}

fn struct_from_json(val: Value) -> prost_types::Struct {
    let map = val.as_object().cloned().unwrap_or_else(Map::new);
    json_map_to_prost_struct(&map)
}

#[derive(Clone, Debug)]
struct RecordedHttpCall {
    operation_name: Option<String>,
    query: String,
    variables: Map<String, Value>,
    authorization: Option<String>,
    api_key: Option<String>,
}

#[derive(Clone, Debug)]
enum MockResponseMode {
    Normal,
    Delay(Duration),
    Custom {
        status: StatusCode,
        content_type: String,
        body: String,
    },
}

struct MockGraphqlState {
    calls: Mutex<Vec<RecordedHttpCall>>,
    http_count: AtomicUsize,
    stock: Mutex<HashMap<String, i64>>,
    mode: Mutex<MockResponseMode>,
    reservation_seq: AtomicUsize,
}

impl MockGraphqlState {
    fn new() -> Self {
        let mut stock = HashMap::new();
        stock.insert("SKU-1".to_owned(), 20);
        stock.insert("SKU-2".to_owned(), 8);
        Self {
            calls: Mutex::new(Vec::new()),
            http_count: AtomicUsize::new(0),
            stock: Mutex::new(stock),
            mode: Mutex::new(MockResponseMode::Normal),
            reservation_seq: AtomicUsize::new(0),
        }
    }
}

fn sample_introspection_json() -> Value {
    let t_ref = |kind: &str, name: Option<&str>, of_type: Option<Value>| -> Value {
        json!({
            "kind": kind,
            "name": name,
            "ofType": of_type
        })
    };
    let non_null = |inner: Value| t_ref("NON_NULL", None, Some(inner));
    let list_of = |inner: Value| t_ref("LIST", None, Some(inner));
    let named = |kind: &str, n: &str| t_ref(kind, Some(n), None);

    json!({
        "__schema": {
            "queryType": { "name": "Query" },
            "mutationType": { "name": "Mutation" },
            "subscriptionType": null,
            "types": [
                { "kind": "SCALAR", "name": "_Any" },
                { "kind": "SCALAR", "name": "JSON" },
                {
                    "kind": "UNION",
                    "name": "_Entity",
                    "possibleTypes": [
                        { "kind": "OBJECT", "name": "Product" },
                        { "kind": "OBJECT", "name": "Warehouse" }
                    ]
                },
                {
                    "kind": "OBJECT",
                    "name": "PageInfo",
                    "fields": [
                        { "name": "hasNextPage", "args": [], "type": non_null(named("SCALAR", "Boolean")) },
                        { "name": "hasPreviousPage", "args": [], "type": non_null(named("SCALAR", "Boolean")) },
                        { "name": "startCursor", "args": [], "type": named("SCALAR", "String") },
                        { "name": "endCursor", "args": [], "type": named("SCALAR", "String") }
                    ]
                },
                {
                    "kind": "OBJECT",
                    "name": "Warehouse",
                    "fields": [
                        { "name": "id", "args": [], "type": non_null(named("SCALAR", "ID")) },
                        { "name": "code", "args": [], "type": non_null(named("SCALAR", "String")) },
                        { "name": "region", "args": [], "type": non_null(named("SCALAR", "String")) }
                    ]
                },
                {
                    "kind": "OBJECT",
                    "name": "Product",
                    "fields": [
                        { "name": "id", "args": [], "type": non_null(named("SCALAR", "ID")) },
                        { "name": "sku", "args": [], "type": non_null(named("SCALAR", "String")) },
                        { "name": "name", "args": [], "type": non_null(named("SCALAR", "String")) },
                        { "name": "priceCents", "args": [], "type": non_null(named("SCALAR", "Int")) },
                        { "name": "warehouse", "args": [], "type": named("OBJECT", "Warehouse") }
                    ]
                },
                {
                    "kind": "OBJECT",
                    "name": "ProductEdge",
                    "fields": [
                        { "name": "node", "args": [], "type": non_null(named("OBJECT", "Product")) },
                        { "name": "cursor", "args": [], "type": non_null(named("SCALAR", "String")) }
                    ]
                },
                {
                    "kind": "OBJECT",
                    "name": "ProductConnection",
                    "fields": [
                        { "name": "edges", "args": [], "type": non_null(list_of(non_null(named("OBJECT", "ProductEdge")))) },
                        { "name": "pageInfo", "args": [], "type": non_null(named("OBJECT", "PageInfo")) },
                        { "name": "totalCount", "args": [], "type": named("SCALAR", "Int") }
                    ]
                },
                {
                    "kind": "INPUT_OBJECT",
                    "name": "ReserveStockInput",
                    "inputFields": [
                        { "name": "sku", "type": non_null(named("SCALAR", "String")) },
                        { "name": "quantity", "type": non_null(named("SCALAR", "Int")) }
                    ]
                },
                {
                    "kind": "OBJECT",
                    "name": "ReservationResult",
                    "fields": [
                        { "name": "reservationId", "args": [], "type": non_null(named("SCALAR", "ID")) },
                        { "name": "remaining", "args": [], "type": non_null(named("SCALAR", "Int")) }
                    ]
                },
                {
                    "kind": "OBJECT",
                    "name": "Query",
                    "fields": [
                        {
                            "name": "product",
                            "args": [{ "name": "id", "type": non_null(named("SCALAR", "ID")) }],
                            "type": named("OBJECT", "Product")
                        },
                        {
                            "name": "products",
                            "args": [
                                { "name": "first", "type": named("SCALAR", "Int") },
                                { "name": "after", "type": named("SCALAR", "String") },
                                { "name": "category", "type": named("SCALAR", "String") }
                            ],
                            "type": non_null(named("OBJECT", "ProductConnection"))
                        },
                        {
                            "name": "warehouse",
                            "args": [{ "name": "id", "type": non_null(named("SCALAR", "ID")) }],
                            "type": named("OBJECT", "Warehouse")
                        },
                        {
                            "name": "_entities",
                            "args": [{ "name": "representations", "type": non_null(list_of(non_null(named("SCALAR", "_Any")))) }],
                            "type": non_null(list_of(named("UNION", "_Entity")))
                        }
                    ]
                },
                {
                    "kind": "OBJECT",
                    "name": "Mutation",
                    "fields": [
                        {
                            "name": "reserveStock",
                            "args": [{ "name": "input", "type": non_null(named("INPUT_OBJECT", "ReserveStockInput")) }],
                            "type": non_null(named("OBJECT", "ReservationResult"))
                        }
                    ]
                }
            ]
        }
    })
}

fn product_json(id: &str) -> Value {
    json!({
        "id": id,
        "sku": format!("SKU-{id}"),
        "name": format!("Item {id}"),
        "priceCents": 1990
    })
}

fn warehouse_json(id: &str) -> Value {
    json!({
        "id": id,
        "code": format!("WH-{id}"),
        "region": "us-west"
    })
}

async fn mock_graphql_handler(
    State(state): State<Arc<MockGraphqlState>>,
    headers: HeaderMap,
    axum::Json(payload): axum::Json<Value>,
) -> Response<Body> {
    let query = payload
        .get("query")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let op_name = payload
        .get("operationName")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);
    let vars = payload
        .get("variables")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();

    let authorization = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .map(ToOwned::to_owned);
    let api_key = headers
        .get("x-api-key")
        .and_then(|v| v.to_str().ok())
        .map(ToOwned::to_owned);

    state.http_count.fetch_add(1, Ordering::SeqCst);
    state.calls.lock().await.push(RecordedHttpCall {
        operation_name: op_name.clone(),
        query: query.clone(),
        variables: vars.clone(),
        authorization,
        api_key,
    });

    let mode = state.mode.lock().await.clone();
    match mode {
        MockResponseMode::Delay(d) => tokio::time::sleep(d).await,
        MockResponseMode::Custom {
            status,
            content_type,
            body,
        } => {
            return Response::builder()
                .status(status)
                .header("content-type", content_type)
                .body(Body::from(body))
                .expect("build custom mock response");
        }
        MockResponseMode::Normal => {}
    }

    // Validate received query parses cleanly as GraphQL executable syntax.
    let mut parser = apollo_compiler::parser::Parser::new();
    let parsed_doc = parser
        .parse_ast(&query, "req.graphql")
        .expect("mock received valid GraphQL query");

    let body = if query.contains("__schema") {
        json!({ "data": sample_introspection_json() })
    } else if op_name.as_deref() == Some("GapEntitiesBatch") {
        let reps = vars
            .get("reps")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let entities: Vec<Value> = reps
            .iter()
            .map(|r| {
                let id = r.get("id").and_then(Value::as_str).unwrap_or("unknown");
                match r.get("__typename").and_then(Value::as_str) {
                    Some("Warehouse") => warehouse_json(id),
                    _ => product_json(id),
                }
            })
            .collect();
        json!({ "data": { "_entities": entities } })
    } else if op_name.as_deref() == Some("GapAliasedBatch") {
        // Parse root aliased selections (`_b0`, `_b1`, ...) from AST.
        let mut data_map = Map::new();
        for def in &parsed_doc.definitions {
            if let apollo_compiler::ast::Definition::OperationDefinition(op_def) = def {
                for sel in &op_def.selection_set {
                    if let apollo_compiler::ast::Selection::Field(f) = sel {
                        let alias = f
                            .alias
                            .as_ref()
                            .map(|a| a.as_str())
                            .unwrap_or(f.name.as_str());
                        let prefix = alias.strip_prefix('_').unwrap_or(alias);
                        let id_var = format!("{prefix}_id");
                        let id_val = vars
                            .get(&id_var)
                            .and_then(Value::as_str)
                            .unwrap_or("unknown");
                        let item = if f.name.as_str() == "warehouse" {
                            warehouse_json(id_val)
                        } else {
                            product_json(id_val)
                        };
                        data_map.insert(alias.to_owned(), item);
                    }
                }
            }
        }
        json!({ "data": Value::Object(data_map) })
    } else if query.contains("reserveStock") {
        let sku = vars
            .get("input")
            .and_then(|v| v.get("sku"))
            .and_then(Value::as_str)
            .unwrap_or("SKU-1")
            .to_owned();
        let qty = vars
            .get("input")
            .and_then(|v| v.get("quantity"))
            .and_then(Value::as_i64)
            .unwrap_or(1);
        let mut guard = state.stock.lock().await;
        let cur = guard.get(&sku).copied().unwrap_or(0);
        let remaining = cur - qty;
        guard.insert(sku, remaining);
        let seq = state.reservation_seq.fetch_add(1, Ordering::SeqCst) + 1;
        json!({
            "data": {
                "reserveStock": {
                    "reservationId": format!("GQL-RES-{seq}"),
                    "remaining": remaining
                }
            }
        })
    } else if query.contains("products(") || query.contains("products {") {
        let after = vars.get("after").and_then(Value::as_str);
        let edges = if after == Some("cursor-page-2") {
            vec![json!({ "cursor": "c-3", "node": product_json("3") })]
        } else {
            vec![
                json!({ "cursor": "c-1", "node": product_json("1") }),
                json!({ "cursor": "c-2", "node": product_json("2") }),
            ]
        };
        let has_next = after != Some("cursor-page-2");
        let end_cursor = if has_next { "cursor-page-2" } else { "c-3" };
        json!({
            "data": {
                "products": {
                    "edges": edges,
                    "pageInfo": {
                        "hasNextPage": has_next,
                        "endCursor": end_cursor
                    },
                    "totalCount": 3
                }
            }
        })
    } else if query.contains("warehouse") {
        let id = vars.get("id").and_then(Value::as_str).unwrap_or("W1");
        json!({ "data": { "warehouse": warehouse_json(id) } })
    } else {
        let id = vars.get("id").and_then(Value::as_str).unwrap_or("1");
        json!({ "data": { "product": product_json(id) } })
    };

    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", GRAPHQL_RESPONSE_JSON)
        .body(Body::from(body.to_string()))
        .expect("build normal GraphQL response")
}

async fn spawn_mock_graphql_server() -> (String, Arc<MockGraphqlState>, tokio::task::JoinHandle<()>)
{
    let state = Arc::new(MockGraphqlState::new());
    let app = Router::new()
        .route("/graphql", post(mock_graphql_handler))
        .with_state(Arc::clone(&state));
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind loopback GraphQL port");
    let addr = listener.local_addr().expect("local addr");
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://{addr}/graphql"), state, handle)
}

// spec: CMP-050, GQL-004
#[tokio::test]
async fn graphql_describe_sdl_introspection_and_branch_namespacing() {
    let sdl = load_fixture_sdl();
    let scope = CallScope::for_test(Instant::now() + Duration::from_secs(5));

    // 1) UpstreamSchemaSource::Sdl normalization with `<ns>_<Type>` prefixing.
    let conn_sdl = GraphqlConnector::connect(GraphqlConfig {
        connector_id: "inv-gql-sdl".to_owned(),
        branch_namespace: "inv".to_owned(),
        endpoint: "http://127.0.0.1:4000/graphql".to_owned(),
        schema_source: UpstreamSchemaSource::Sdl(sdl),
        ..GraphqlConfig::default()
    })
    .await
    .expect("connect from fixture SDL");

    let bundle = conn_sdl.schema_bundle();
    assert!(bundle.supports_federation_entities);
    assert!(bundle.federation_entity_types.contains("Product"));
    assert!(bundle.federation_entity_types.contains("Warehouse"));
    assert_eq!(
        bundle
            .branch_to_upstream_type
            .get("inv_Product")
            .map(String::as_str),
        Some("Product")
    );
    assert_eq!(
        bundle
            .branch_to_upstream_type
            .get("inv_Warehouse")
            .map(String::as_str),
        Some("Warehouse")
    );
    assert!(bundle.schema_sdl.contains("type inv_Product"));
    assert!(bundle.schema_sdl.contains("type inv_Warehouse"));
    assert!(bundle.schema_sdl.contains("inv_ReserveStockInput"));

    let desc_resp = conn_sdl
        .describe(DescribeRequest::default(), scope.clone())
        .await
        .expect("describe sdl");
    let description = desc_resp.description.expect("description present");
    let ops: BTreeMap<&str, _> = description
        .operations
        .iter()
        .map(|o| (o.name.as_str(), o))
        .collect();
    assert_eq!(ops["product"].kind(), OpKind::Query);
    assert_eq!(ops["products"].kind(), OpKind::Query);
    assert_eq!(
        ops["products"].pagination(),
        PaginationKind::PaginationCursor
    );
    assert_eq!(ops["reserveStock"].kind(), OpKind::Mutation);
    // Federation internal `_entities` field is exposed for batching support, not cluttered as a regular user query unless modeled, or verify non-internal queries present.
    assert!(ops.contains_key("warehouse"));

    // Conditional hash caching (`known_spec_hash`).
    let cached = conn_sdl
        .describe(
            DescribeRequest {
                known_spec_hash: description.spec_hash.clone(),
            },
            scope.clone(),
        )
        .await
        .expect("cached describe");
    assert!(cached.not_modified);
    assert!(cached.description.is_none());

    // 2) UpstreamSchemaSource::IntrospectionJson produces equivalent operations & SDL.
    let conn_intro = GraphqlConnector::connect(GraphqlConfig {
        connector_id: "inv-gql-intro".to_owned(),
        branch_namespace: "inv".to_owned(),
        schema_source: UpstreamSchemaSource::IntrospectionJson(sample_introspection_json()),
        ..GraphqlConfig::default()
    })
    .await
    .expect("connect from IntrospectionJson");
    assert_eq!(
        conn_intro.schema_bundle().operations.len(),
        conn_sdl.schema_bundle().operations.len()
    );
    assert_eq!(
        conn_intro.schema_bundle().operations["products"]
            .relay
            .as_ref()
            .map(|r| r.node_type.as_str()),
        Some("Product")
    );

    // 3) UpstreamSchemaSource::LiveIntrospection fetches `__schema` from live endpoint.
    let (endpoint, state, handle) = spawn_mock_graphql_server().await;
    let conn_live = GraphqlConnector::connect(GraphqlConfig {
        connector_id: "inv-gql-live-intro".to_owned(),
        branch_namespace: "inv".to_owned(),
        endpoint,
        schema_source: UpstreamSchemaSource::LiveIntrospection,
        ..GraphqlConfig::default()
    })
    .await
    .expect("connect via LiveIntrospection");
    assert_eq!(state.http_count.load(Ordering::SeqCst), 1);
    assert!(conn_live.schema_bundle().supports_federation_entities);
    assert!(
        conn_live
            .schema_bundle()
            .operations
            .contains_key("products")
    );
    handle.abort();
}

// spec: CMP-050
#[tokio::test]
async fn graphql_rewrite_executable_document_types() {
    let sdl = load_fixture_sdl();
    let conn = GraphqlConnector::connect(GraphqlConfig {
        branch_namespace: "inv".to_owned(),
        schema_source: UpstreamSchemaSource::Sdl(sdl),
        ..GraphqlConfig::default()
    })
    .await
    .expect("build connector");
    let map = &conn.schema_bundle().branch_to_upstream_type;

    let branch_doc = r#"
        query InspectProduct($input: inv_ReserveStockInput!) {
          product(id: "1") {
            ... on inv_Product {
              sku
              ...ProdFields
            }
          }
        }
        fragment ProdFields on inv_Product {
          name
          priceCents
        }
    "#;
    let rewritten = rewrite_executable_document_types(branch_doc, map)
        .expect("rewrite executable document types");

    assert!(!rewritten.contains("inv_ReserveStockInput"));
    assert!(!rewritten.contains("inv_Product"));
    assert!(rewritten.contains("$input: ReserveStockInput!"));
    assert!(rewritten.contains("... on Product"));
    assert!(rewritten.contains("fragment ProdFields on Product"));

    // Rewritten document must parse cleanly with upstream type names.
    let mut parser = apollo_compiler::parser::Parser::new();
    parser
        .parse_ast(&rewritten, "rewritten.graphql")
        .expect("rewritten executable query parses cleanly");
}

// spec: CMP-050, NGC-020, NGC-026, INV-18
#[tokio::test]
async fn inv_18_graphql_foreach_aliased_and_federation_entities_batching() {
    let (endpoint, state, handle) = spawn_mock_graphql_server().await;
    let sdl = load_fixture_sdl();
    let scope = CallScope::for_test(Instant::now() + Duration::from_secs(10));

    // 1) Aliased root-field batching (`N = 10` distinct keys + 2 duplicates, `max_batch_size = 4` => `3` HTTP requests).
    let conn_aliased = GraphqlConnector::connect(GraphqlConfig {
        connector_id: "gql-alias".to_owned(),
        branch_namespace: "inv".to_owned(),
        endpoint: endpoint.clone(),
        schema_source: UpstreamSchemaSource::Sdl(sdl.clone()),
        prefer_federation_entities: false,
        max_batch_size: 4,
        max_in_flight: 4,
        ..GraphqlConfig::default()
    })
    .await
    .expect("connect aliased connector");

    let keys_aliased = [
        "W1", "W2", "W1", "W3", "W4", "W5", "W2", "W6", "W7", "W8", "W9", "W10",
    ];
    let foreach_aliased: Vec<VariableSet> = keys_aliased
        .iter()
        .enumerate()
        .map(|(i, id)| VariableSet {
            index: u32::try_from(i).expect("idx"),
            values: Some(struct_from_json(json!({ "id": id }))),
        })
        .collect();

    let mut stream = conn_aliased
        .query(
            QueryRequest {
                operation: "warehouse".to_owned(),
                foreach: foreach_aliased,
                ..QueryRequest::default()
            },
            scope.clone(),
        )
        .await
        .expect("aliased foreach query");

    let mut by_idx = BTreeMap::new();
    let mut comp = None;
    while let Some(item) = stream.next().await {
        match item.expect("chunk ok").chunk {
            Some(query_response::Chunk::Rows(c)) => {
                if let Some(first) = c.rows.first() {
                    let obj = prost_struct_to_json_map(first);
                    by_idx.insert(
                        c.foreach_index,
                        obj.get("code")
                            .and_then(|v| v.as_str())
                            .unwrap_or_default()
                            .to_owned(),
                    );
                }
            }
            Some(query_response::Chunk::Complete(c)) => comp = Some(c),
            None => {}
        }
    }
    assert_eq!(by_idx.len(), 12);
    assert_eq!(by_idx[&0], "WH-W1");
    assert_eq!(by_idx[&2], "WH-W1");
    assert_eq!(by_idx[&11], "WH-W10");
    assert_eq!(state.http_count.load(Ordering::SeqCst), 3);
    assert_eq!(comp.expect("complete").upstream_calls, 3);
    {
        let recorded = state.calls.lock().await;
        assert!(
            recorded
                .iter()
                .all(|c| c.operation_name.as_deref() == Some("GapAliasedBatch"))
        );
        assert!(recorded[0].query.contains("_b0: warehouse"));
    }

    // 2) Apollo Federation `_entities(representations: $reps)` batching (`N = 7` distinct + 1 dup, `max_batch_size = 3` => `3` HTTP requests).
    state.http_count.store(0, Ordering::SeqCst);
    state.calls.lock().await.clear();

    let mut entity_mappings = BTreeMap::new();
    entity_mappings.insert(
        "product".to_owned(),
        EntityBatchMapping {
            upstream_typename: "Product".to_owned(),
            key_fields: vec!["id".to_owned()],
        },
    );
    let conn_fed = GraphqlConnector::connect(GraphqlConfig {
        connector_id: "gql-fed".to_owned(),
        branch_namespace: "inv".to_owned(),
        endpoint,
        schema_source: UpstreamSchemaSource::Sdl(sdl),
        prefer_federation_entities: true,
        entity_mappings,
        max_batch_size: 3,
        max_in_flight: 4,
        ..GraphqlConfig::default()
    })
    .await
    .expect("connect federation connector");

    let fed_keys = ["P1", "P2", "P3", "P1", "P4", "P5", "P6", "P7"];
    let foreach_fed: Vec<VariableSet> = fed_keys
        .iter()
        .enumerate()
        .map(|(i, id)| VariableSet {
            index: u32::try_from(i).expect("idx"),
            values: Some(struct_from_json(json!({ "id": id }))),
        })
        .collect();

    let mut fed_stream = conn_fed
        .query(
            QueryRequest {
                operation: "product".to_owned(),
                foreach: foreach_fed,
                ..QueryRequest::default()
            },
            scope,
        )
        .await
        .expect("federation _entities foreach query");

    let mut fed_by_idx = BTreeMap::new();
    let mut fed_comp = None;
    while let Some(item) = fed_stream.next().await {
        match item.expect("chunk ok").chunk {
            Some(query_response::Chunk::Rows(c)) => {
                if let Some(first) = c.rows.first() {
                    let obj = prost_struct_to_json_map(first);
                    fed_by_idx.insert(
                        c.foreach_index,
                        obj.get("sku")
                            .and_then(|v| v.as_str())
                            .unwrap_or_default()
                            .to_owned(),
                    );
                }
            }
            Some(query_response::Chunk::Complete(c)) => fed_comp = Some(c),
            None => {}
        }
    }
    assert_eq!(fed_by_idx.len(), 8);
    assert_eq!(fed_by_idx[&0], "SKU-P1");
    assert_eq!(fed_by_idx[&3], "SKU-P1");
    assert_eq!(fed_by_idx[&7], "SKU-P7");
    assert_eq!(state.http_count.load(Ordering::SeqCst), 3);
    assert_eq!(fed_comp.expect("complete").upstream_calls, 3);
    let calls = state.calls.lock().await;
    assert!(
        calls
            .iter()
            .all(|c| c.operation_name.as_deref() == Some("GapEntitiesBatch"))
    );
    assert!(calls[0].query.contains("_entities(representations: $reps)"));
    assert!(calls[0].variables.contains_key("reps"));

    handle.abort();
}

// spec: CMP-050, NGC-020
#[tokio::test]
async fn graphql_relay_cursor_pagination_and_mutation_dry_run() {
    let (endpoint, state, handle) = spawn_mock_graphql_server().await;
    let sdl = load_fixture_sdl();
    let scope = CallScope::for_test(Instant::now() + Duration::from_secs(10));

    let connector = GraphqlConnector::connect(GraphqlConfig {
        connector_id: "gql-relay".to_owned(),
        branch_namespace: "inv".to_owned(),
        endpoint: endpoint.clone(),
        schema_source: UpstreamSchemaSource::Sdl(sdl.clone()),
        supports_dry_run: true,
        ..GraphqlConfig::default()
    })
    .await
    .expect("connect relay connector");

    // 1) Health check passes.
    let health = connector
        .health(HealthRequest {}, scope.clone())
        .await
        .expect("health ok");
    assert_eq!(health.status(), ServingStatus::Serving);

    // 2) Relay auto-unwrap returns unwrapped Product nodes + next_cursor from pageInfo.endCursor.
    let mut page1 = connector
        .query(
            QueryRequest {
                operation: "products".to_owned(),
                page: Some(Page {
                    limit: 2,
                    cursor: String::new(),
                    offset: 0,
                }),
                selection: Some(Selection {
                    fields: vec![
                        SelectionField {
                            name: "id".to_owned(),
                            ..SelectionField::default()
                        },
                        SelectionField {
                            name: "sku".to_owned(),
                            ..SelectionField::default()
                        },
                    ],
                }),
                ..QueryRequest::default()
            },
            scope.clone(),
        )
        .await
        .expect("relay page 1");

    let mut page1_rows = Vec::new();
    let mut page1_comp = None;
    while let Some(item) = page1.next().await {
        match item.expect("chunk ok").chunk {
            Some(query_response::Chunk::Rows(c)) => {
                for r in c.rows {
                    page1_rows.push(prost_struct_to_json_map(&r));
                }
            }
            Some(query_response::Chunk::Complete(c)) => page1_comp = Some(c),
            None => {}
        }
    }
    assert_eq!(page1_rows.len(), 2);
    assert_eq!(page1_rows[0].get("sku"), Some(&json!("SKU-1")));
    assert_eq!(page1_rows[1].get("sku"), Some(&json!("SKU-2")));
    assert_eq!(page1_comp.expect("complete").next_cursor, "cursor-page-2");

    // 3) Mutation dry_run performs 0 upstream calls; real mutate executes 1 HTTP POST.
    let calls_before = state.http_count.load(Ordering::SeqCst);
    let dry = connector
        .mutate(
            MutateRequest {
                operation: "reserveStock".to_owned(),
                arguments: Some(struct_from_json(json!({
                    "input": { "sku": "SKU-1", "quantity": 4 }
                }))),
                dry_run: true,
                ..MutateRequest::default()
            },
            scope.clone(),
        )
        .await
        .expect("dry run");
    assert_eq!(dry.outcome(), MutationOutcome::OutcomeDryRun);
    assert_eq!(state.http_count.load(Ordering::SeqCst), calls_before);

    let applied = connector
        .mutate(
            MutateRequest {
                operation: "reserveStock".to_owned(),
                arguments: Some(struct_from_json(json!({
                    "input": { "sku": "SKU-1", "quantity": 4 }
                }))),
                dry_run: false,
                ..MutateRequest::default()
            },
            scope.clone(),
        )
        .await
        .expect("real mutation");
    assert_eq!(applied.outcome(), MutationOutcome::OutcomeApplied);
    assert_eq!(state.http_count.load(Ordering::SeqCst), calls_before + 1);
    let m_obj = prost_struct_to_json_map(&applied.result.expect("mutate result"));
    assert_eq!(m_obj.get("reservationId"), Some(&json!("GQL-RES-1")));
    assert_eq!(m_obj.get("remaining").and_then(Value::as_f64), Some(16.0));

    // 4) read_only = true rejects Mutate with PermissionDenied and 0 upstream calls.
    let ro_conn = GraphqlConnector::connect(GraphqlConfig {
        connector_id: "gql-ro".to_owned(),
        branch_namespace: "inv".to_owned(),
        endpoint,
        schema_source: UpstreamSchemaSource::Sdl(sdl),
        read_only: true,
        ..GraphqlConfig::default()
    })
    .await
    .expect("read_only connect");
    let ro_err = ro_conn
        .mutate(
            MutateRequest {
                operation: "reserveStock".to_owned(),
                arguments: Some(struct_from_json(json!({
                    "input": { "sku": "SKU-1", "quantity": 1 }
                }))),
                dry_run: false,
                ..MutateRequest::default()
            },
            scope,
        )
        .await
        .expect_err("read_only must reject mutation");
    assert_eq!(ro_err.code, NgcErrorCode::PermissionDenied);

    handle.abort();
}

// See ADR-023.
// spec: CMP-050, NGC-025, GQL-003
#[tokio::test]
async fn graphql_wire_adr_023_status_codes_and_graphql_response_json() {
    // 1) Unit checks on `interpret_wire_response` across 200, 294, 400/422+graphql-response+json, and 500 text.
    let ok_294 = interpret_wire_response(GraphqlWireResponse {
        status: StatusCode::from_u16(294).expect("294 status"),
        is_graphql_body: true,
        data: Some(json!({"product": {"id": "1"}})),
        errors: vec![json!({"message": "partial field fail"})],
    })
    .expect("294 partial success accepted per ADR-023");
    assert_eq!(ok_294.status.as_u16(), 294);
    assert!(ok_294.data.is_some());
    assert_eq!(ok_294.errors.len(), 1);

    let err_400_gql = interpret_wire_response(GraphqlWireResponse {
        status: StatusCode::BAD_REQUEST,
        is_graphql_body: true,
        data: None,
        errors: vec![
            json!({"message": "invalid filter expression", "extensions": {"code": "BAD_USER_INPUT"}}),
        ],
    })
    .expect_err("400 application/graphql-response+json must surface GraphQL errors per ADR-023");
    assert_eq!(err_400_gql.code, NgcErrorCode::InvalidArgument);
    assert!(err_400_gql.message().contains("invalid filter expression"));

    let err_422_gql = interpret_wire_response(GraphqlWireResponse {
        status: StatusCode::UNPROCESSABLE_ENTITY,
        is_graphql_body: true,
        data: None,
        errors: vec![json!({"message": "validation error"})],
    })
    .expect_err("422 application/graphql-response+json must decode GraphQL errors per ADR-023");
    assert_eq!(err_422_gql.code, NgcErrorCode::InvalidArgument);
    assert!(err_422_gql.message().contains("validation error"));

    let err_500 = interpret_wire_response(GraphqlWireResponse {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        is_graphql_body: false,
        data: None,
        errors: Vec::new(),
    })
    .expect_err("non-GraphQL 500 body must return NgcErrorInfo");
    assert_eq!(err_500.code, NgcErrorCode::UpstreamError);

    // 2) Live HTTP 294 & HTTP 400 `application/graphql-response+json` + per-request timeout via connector.
    let (endpoint, state, handle) = spawn_mock_graphql_server().await;
    let sdl = load_fixture_sdl();
    let connector = GraphqlConnector::connect(GraphqlConfig {
        connector_id: "gql-adr023".to_owned(),
        branch_namespace: "inv".to_owned(),
        endpoint,
        schema_source: UpstreamSchemaSource::Sdl(sdl),
        default_timeout_ms: 50,
        ..GraphqlConfig::default()
    })
    .await
    .expect("connect ADR-023 test connector");

    let scope = CallScope::for_test(Instant::now() + Duration::from_secs(10));

    // HTTP 294 custom 2xx status with `data` succeeds through full connector query pipeline.
    *state.mode.lock().await = MockResponseMode::Custom {
        status: StatusCode::from_u16(294).expect("294"),
        content_type: "application/json".to_owned(),
        body: r#"{"data":{"product":{"id":"9","sku":"SKU-9","name":"Partial","priceCents":100}}}"#
            .to_owned(),
    };
    let mut s294 = connector
        .query(
            QueryRequest {
                operation: "product".to_owned(),
                arguments: Some(struct_from_json(json!({ "id": "9" }))),
                ..QueryRequest::default()
            },
            scope.clone(),
        )
        .await
        .expect("HTTP 294 GraphQL response accepted");
    let first = s294.next().await.expect("row chunk").expect("ok");
    assert!(matches!(first.chunk, Some(query_response::Chunk::Rows(_))));

    // HTTP 422 `application/graphql-response+json` surfaces upstream GraphQL error message cleanly.
    *state.mode.lock().await = MockResponseMode::Custom {
        status: StatusCode::UNPROCESSABLE_ENTITY,
        content_type: GRAPHQL_RESPONSE_JSON.to_owned(),
        body: r#"{"errors":[{"message":"product id cannot be negative"}]}"#.to_owned(),
    };
    let g_err = connector
        .query(
            QueryRequest {
                operation: "product".to_owned(),
                arguments: Some(struct_from_json(json!({ "id": "-1" }))),
                ..QueryRequest::default()
            },
            scope.clone(),
        )
        .await
        .err()
        .expect("422 GraphQL errors must fail query");
    assert_eq!(g_err.code, NgcErrorCode::InvalidArgument);
    assert!(g_err.message().contains("product id cannot be negative"));

    // Slow upstream hits `default_timeout_ms = 50ms` -> `DEADLINE_EXCEEDED` (`NGC-025`).
    *state.mode.lock().await = MockResponseMode::Delay(Duration::from_millis(250));
    let t_err = connector
        .query(
            QueryRequest {
                operation: "product".to_owned(),
                arguments: Some(struct_from_json(json!({ "id": "1" }))),
                ..QueryRequest::default()
            },
            scope,
        )
        .await
        .err()
        .expect("slow upstream must time out");
    assert_eq!(t_err.code, NgcErrorCode::DeadlineExceeded);

    handle.abort();
}

// spec: SEC-011, INV-15
#[tokio::test]
async fn inv_15_graphql_credentials_redaction_and_no_bearer_passthrough() {
    let bearer = GraphqlCredential::Bearer {
        token: Secret::new("gql-static-secret-jwt".to_owned()),
    };
    let header = GraphqlCredential::Header {
        name: "x-api-key".to_owned(),
        value: Secret::new("gql-api-key-secret".to_owned()),
    };
    let dbg_b = format!("{bearer:?}");
    let dbg_h = format!("{header:?}");
    assert!(dbg_b.contains("***") && !dbg_b.contains("gql-static-secret-jwt"));
    assert!(dbg_h.contains("***") && !dbg_h.contains("gql-api-key-secret"));

    // Non-loopback cleartext HTTP credential refusal (`SEC-011`).
    let err = validate_endpoint("http://subgraph.internal/graphql", true, false)
        .expect_err("cleartext non-loopback credentials must fail");
    assert_eq!(err.code, NgcErrorCode::FailedPrecondition);
    validate_endpoint("https://subgraph.internal/graphql", true, false)
        .expect("HTTPS remote allowed");

    let (endpoint, state, handle) = spawn_mock_graphql_server().await;
    let sdl = load_fixture_sdl();
    let connector = GraphqlConnector::connect(GraphqlConfig {
        connector_id: "gql-sec".to_owned(),
        branch_namespace: "inv".to_owned(),
        endpoint,
        schema_source: UpstreamSchemaSource::Sdl(sdl),
        credentials: vec![bearer, header],
        ..GraphqlConfig::default()
    })
    .await
    .expect("connect with static HTTP credentials");

    let mut scope = CallScope::for_test(Instant::now() + Duration::from_secs(10));
    scope.metadata.insert(
        "authorization".to_owned(),
        "Bearer end-user-token-must-never-leak".to_owned(),
    );

    let mut stream = connector
        .query(
            QueryRequest {
                operation: "product".to_owned(),
                arguments: Some(struct_from_json(json!({ "id": "1" }))),
                ..QueryRequest::default()
            },
            scope,
        )
        .await
        .expect("query succeeds");
    while let Some(item) = stream.next().await {
        item.expect("chunk ok");
    }

    let calls = state.calls.lock().await;
    assert_eq!(calls.len(), 1);
    assert_eq!(
        calls[0].authorization.as_deref(),
        Some("Bearer gql-static-secret-jwt")
    );
    assert_eq!(calls[0].api_key.as_deref(), Some("gql-api-key-secret"));

    handle.abort();
}
