//! Integration and adversarial tests for `gap-ngc-sql` (`CMP-050`, `CMP-051`, `CMP-052`, `NGC-020..027`, `NGC-050`).

use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::StreamExt;
use gap_core::error::NgcErrorCode;
use gap_ngc_sdk::connector::{CallScope, Connector};
use gap_ngc_sdk::error::prost_struct_to_json_map;
use gap_ngc_sql::cursor::{self, CURSOR_TTL};
use gap_ngc_sql::{
    ColumnDef, Dialect, Driver, DriverError, OperationConfig, OperationKind, SqlConfig,
    SqlConnector, SqlParam, SqlRow, SqlType, SqlValue, SqliteDriver,
};
use gap_proto::ngc::v1::{
    Cmp, Complete, DescribeRequest, ErrorCode as ProtoErrorCode, Expr, ExprList, HealthRequest,
    OrderBy, Page, PredicateOp, QueryRequest, RowSetChunk, Selection, SelectionField,
    ServingStatus, SketchRequest, VariableSet, expr, query_response,
};
use tokio::time::Instant;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn shop_sql() -> String {
    std::fs::read_to_string(repo_root().join("tests/fixtures/sql/shop.sql")).expect("read shop.sql")
}

fn scope() -> CallScope {
    CallScope::for_test(Instant::now() + Duration::from_secs(5))
}

fn scope_for_tenant(tenant: &str) -> CallScope {
    let mut s = scope();
    s.tenant_id = tenant.to_owned();
    s
}

fn customer_columns() -> Vec<ColumnDef> {
    vec![
        ColumnDef {
            name: "id".to_owned(),
            sql_type: SqlType::Integer,
            nullable: false,
        },
        ColumnDef {
            name: "name".to_owned(),
            sql_type: SqlType::Text,
            nullable: false,
        },
        ColumnDef {
            name: "tier".to_owned(),
            sql_type: SqlType::Text,
            nullable: false,
        },
        ColumnDef {
            name: "credit_cents".to_owned(),
            sql_type: SqlType::Integer,
            nullable: false,
        },
        ColumnDef {
            name: "region".to_owned(),
            sql_type: SqlType::Text,
            nullable: false,
        },
        ColumnDef {
            name: "active".to_owned(),
            sql_type: SqlType::Boolean,
            nullable: false,
        },
        ColumnDef {
            name: "notes".to_owned(),
            sql_type: SqlType::Text,
            nullable: true,
        },
    ]
}

fn shop_config(dialect: Dialect) -> SqlConfig {
    let cols = customer_columns();
    SqlConfig {
        connector_id: "shop-sql".to_owned(),
        dialect,
        max_in_flight: 8,
        max_batch_size: 50,
        max_page_size: 100,
        max_response_bytes: 4 * 1024 * 1024,
        operations: vec![
            OperationConfig {
                name: "customer".to_owned(),
                kind: OperationKind::GetByKey,
                table: "customers".to_owned(),
                columns: cols.clone(),
                key_columns: vec!["id".to_owned()],
                row_cap: 100,
                statement_timeout_ms: 5_000,
                simulated_latency_ms: 0,
            },
            OperationConfig {
                name: "customers".to_owned(),
                kind: OperationKind::List,
                table: "customers".to_owned(),
                columns: cols,
                key_columns: vec!["id".to_owned()],
                row_cap: 100,
                statement_timeout_ms: 5_000,
                simulated_latency_ms: 0,
            },
        ],
        ..SqlConfig::default()
    }
}

fn sqlite_shop_connector(cfg: SqlConfig) -> SqlConnector<SqliteDriver> {
    let conn = rusqlite::Connection::open_in_memory().expect("open in-memory sqlite");
    conn.execute_batch(&shop_sql()).expect("seed shop.sql");
    let driver = SqliteDriver::from_connection(conn).expect("wrap read-only SqliteDriver");
    SqlConnector::new(cfg, driver).expect("construct SqlConnector")
}

fn make_struct(pairs: Vec<(&str, serde_json::Value)>) -> prost_types::Struct {
    let fields = pairs
        .into_iter()
        .map(|(k, v)| (k.to_owned(), gap_ngc_sdk::error::json_to_prost_value(&v)))
        .collect();
    prost_types::Struct { fields }
}

struct CollectedQuery {
    rows_by_index: HashMap<u32, Vec<serde_json::Map<String, serde_json::Value>>>,
    errors_by_index: HashMap<u32, i32>,
    complete: Complete,
}

async fn collect_stream(
    mut stream: gap_ngc_sdk::connector::BoxStream<gap_proto::ngc::v1::QueryResponse>,
) -> CollectedQuery {
    let mut rows_by_index: HashMap<u32, Vec<serde_json::Map<String, serde_json::Value>>> =
        HashMap::new();
    let mut errors_by_index: HashMap<u32, i32> = HashMap::new();
    let mut complete = None;

    while let Some(item) = stream.next().await {
        let resp = item.expect("stream item ok");
        match resp.chunk.expect("chunk present") {
            query_response::Chunk::Rows(RowSetChunk {
                foreach_index,
                rows,
                error,
                ..
            }) => {
                let entry = rows_by_index.entry(foreach_index).or_default();
                for r in rows {
                    entry.push(prost_struct_to_json_map(&r));
                }
                if let Some(err) = error {
                    errors_by_index.insert(foreach_index, err.code);
                }
            }
            query_response::Chunk::Complete(done) => {
                assert!(complete.is_none(), "expected single Complete chunk");
                complete = Some(done);
            }
        }
    }

    CollectedQuery {
        rows_by_index,
        errors_by_index,
        complete: complete.expect("stream ended with Complete"),
    }
}

type RecordedCall = (String, Vec<SqlParam>, usize);

#[derive(Clone, Default)]
struct RecordingDriver {
    recorded: Arc<Mutex<Vec<RecordedCall>>>,
    next_rows: Arc<Mutex<Vec<SqlRow>>>,
    calls: Arc<AtomicUsize>,
}

impl RecordingDriver {
    fn with_rows(rows: Vec<SqlRow>) -> Self {
        Self {
            recorded: Arc::new(Mutex::new(Vec::new())),
            next_rows: Arc::new(Mutex::new(rows)),
            calls: Arc::new(AtomicUsize::new(0)),
        }
    }

    fn calls(&self) -> Vec<RecordedCall> {
        self.recorded.lock().expect("lock recorded").clone()
    }
}

impl Driver for RecordingDriver {
    fn query(
        &self,
        sql: String,
        params: Vec<SqlParam>,
        row_cap: usize,
        _scope: CallScope,
    ) -> impl Future<Output = Result<Vec<SqlRow>, DriverError>> + Send {
        self.calls.fetch_add(1, Ordering::AcqRel);
        self.recorded
            .lock()
            .expect("lock recorded")
            .push((sql, params, row_cap));
        let rows = self.next_rows.lock().expect("lock rows").clone();
        std::future::ready(Ok(rows))
    }
}

// spec: CMP-050, NGC-010, NGC-012, NGC-013, NGC-070
#[tokio::test]
async fn describe_and_health_expose_sdl_schemas_and_blake3_hash() {
    let conn = sqlite_shop_connector(shop_config(Dialect::Sqlite));

    let desc = conn
        .describe(DescribeRequest::default(), scope())
        .await
        .expect("describe ok");
    assert!(!desc.not_modified);
    let inner = desc.description.expect("description present");
    assert_eq!(inner.connector_id, "shop-sql");
    assert_eq!(inner.operations.len(), 2);
    assert!(inner.schema_sdl.contains("type Query"));
    assert!(inner.schema_sdl.contains("type CustomerRow"));
    assert!(inner.schema_sdl.contains("type CustomersRow"));
    assert_eq!(inner.spec_hash.len(), 64, "expected 32-byte blake3 hex");

    for op in &inner.operations {
        assert!(op.input_schema.is_some());
        assert!(op.output_schema.is_some());
        assert_eq!(op.key_fields, vec!["id".to_owned()]);
    }

    let cached = conn
        .describe(
            DescribeRequest {
                known_spec_hash: inner.spec_hash.clone(),
            },
            scope(),
        )
        .await
        .expect("cached describe ok");
    assert!(cached.not_modified);
    assert!(cached.description.is_none());

    let health = conn
        .health(HealthRequest::default(), scope())
        .await
        .expect("health ok");
    assert_eq!(health.status, ServingStatus::Serving as i32);
}

// spec: CMP-050, CMP-052, NGC-020, NGC-026
#[tokio::test]
async fn get_by_key_and_foreach_deduplicate_and_isolate_partial_failures() {
    let conn = sqlite_shop_connector(shop_config(Dialect::Sqlite));

    let single = collect_stream(
        conn.query(
            QueryRequest {
                operation: "customer".to_owned(),
                arguments: Some(make_struct(vec![("id", serde_json::json!(1))])),
                selection: Some(Selection {
                    fields: vec![
                        SelectionField {
                            name: "id".to_owned(),
                            ..SelectionField::default()
                        },
                        SelectionField {
                            name: "name".to_owned(),
                            ..SelectionField::default()
                        },
                        SelectionField {
                            name: "active".to_owned(),
                            ..SelectionField::default()
                        },
                    ],
                }),
                ..QueryRequest::default()
            },
            scope(),
        )
        .await
        .expect("single get_by_key ok"),
    )
    .await;
    assert_eq!(single.complete.upstream_calls, 1);
    let rows = &single.rows_by_index[&0];
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["name"], "Ada Lovelace Analytics Labs");
    assert_eq!(rows[0]["active"], serde_json::json!(true));
    assert!(!rows[0].contains_key("credit_cents"), "projection enforced");

    let absent = collect_stream(
        conn.query(
            QueryRequest {
                operation: "customer".to_owned(),
                arguments: Some(make_struct(vec![("id", serde_json::json!(999))])),
                ..QueryRequest::default()
            },
            scope(),
        )
        .await
        .expect("absent key ok"),
    )
    .await;
    assert!(absent.rows_by_index.get(&0).is_none_or(Vec::is_empty));

    // foreach batch with duplicate keys (1, 1), valid key (2), absent key (999), and invalid key ("bad")
    let batched = collect_stream(
        conn.query(
            QueryRequest {
                operation: "customer".to_owned(),
                foreach: vec![
                    VariableSet {
                        index: 10,
                        values: Some(make_struct(vec![("id", serde_json::json!(1))])),
                    },
                    VariableSet {
                        index: 20,
                        values: Some(make_struct(vec![("id", serde_json::json!(1))])),
                    },
                    VariableSet {
                        index: 30,
                        values: Some(make_struct(vec![("id", serde_json::json!("not-an-id"))])),
                    },
                    VariableSet {
                        index: 40,
                        values: Some(make_struct(vec![("id", serde_json::json!(999))])),
                    },
                    VariableSet {
                        index: 50,
                        values: Some(make_struct(vec![("id", serde_json::json!(2))])),
                    },
                ],
                ..QueryRequest::default()
            },
            scope(),
        )
        .await
        .expect("foreach stream starts"),
    )
    .await;

    assert_eq!(batched.complete.upstream_calls, 1, "dedup + single SQL IN");
    assert_eq!(batched.rows_by_index[&10].len(), 1);
    assert_eq!(batched.rows_by_index[&20].len(), 1);
    assert_eq!(batched.rows_by_index[&10][0]["id"], serde_json::json!(1.0));
    assert_eq!(batched.rows_by_index[&20][0]["id"], serde_json::json!(1.0));
    assert_eq!(
        batched.errors_by_index.get(&30).copied(),
        Some(ProtoErrorCode::InvalidArgument as i32)
    );
    assert!(batched.rows_by_index.get(&40).is_none_or(Vec::is_empty));
    assert_eq!(batched.rows_by_index[&50][0]["id"], serde_json::json!(2.0));
}

// spec: CMP-052
#[tokio::test]
async fn postgres_dialect_uses_dollar_placeholders_and_any_array_foreach() {
    let mut row1 = BTreeMap::new();
    row1.insert("id".to_owned(), SqlValue::Integer(1));
    row1.insert("name".to_owned(), SqlValue::Text("Ada".to_owned()));
    row1.insert("tier".to_owned(), SqlValue::Text("gold".to_owned()));
    row1.insert("credit_cents".to_owned(), SqlValue::Integer(100));
    row1.insert("region".to_owned(), SqlValue::Text("eu".to_owned()));
    row1.insert("active".to_owned(), SqlValue::Bool(true));
    row1.insert("notes".to_owned(), SqlValue::Null);

    let driver = RecordingDriver::with_rows(vec![row1]);
    let conn = SqlConnector::new(shop_config(Dialect::Postgres), driver.clone())
        .expect("postgres connector");

    let collected = collect_stream(
        conn.query(
            QueryRequest {
                operation: "customer".to_owned(),
                foreach: vec![
                    VariableSet {
                        index: 1,
                        values: Some(make_struct(vec![("id", serde_json::json!(1))])),
                    },
                    VariableSet {
                        index: 2,
                        values: Some(make_struct(vec![("id", serde_json::json!(2))])),
                    },
                ],
                ..QueryRequest::default()
            },
            scope(),
        )
        .await
        .expect("pg foreach ok"),
    )
    .await;

    assert_eq!(collected.rows_by_index[&1].len(), 1);
    assert_eq!(
        collected.rows_by_index[&1][0]["active"],
        serde_json::json!(true)
    );
    let calls = driver.calls();
    assert_eq!(calls.len(), 1);
    assert!(calls[0].0.contains("= ANY($1)"), "got SQL: {}", calls[0].0);
    assert_eq!(
        calls[0].1,
        vec![SqlParam::Array(vec![
            SqlParam::Integer(1),
            SqlParam::Integer(2),
        ])]
    );
}

// spec: CMP-050, NGC-021, NGC-022, NGC-023, NGC-024
#[tokio::test]
async fn list_supports_predicates_projection_ordering_and_bound_cursors() {
    let conn = sqlite_shop_connector(shop_config(Dialect::Sqlite));

    // Compound predicate: region = 'eu' AND (tier = 'gold' OR credit_cents >= 200000) AND EXISTS(notes)
    let pred = Expr {
        node: Some(expr::Node::And(ExprList {
            exprs: vec![
                Expr {
                    node: Some(expr::Node::Cmp(Cmp {
                        path: "region".to_owned(),
                        op: PredicateOp::PEq as i32,
                        value: Some(gap_ngc_sdk::error::json_to_prost_value(&serde_json::json!(
                            "eu"
                        ))),
                    })),
                },
                Expr {
                    node: Some(expr::Node::Or(ExprList {
                        exprs: vec![
                            Expr {
                                node: Some(expr::Node::Cmp(Cmp {
                                    path: "tier".to_owned(),
                                    op: PredicateOp::PEq as i32,
                                    value: Some(gap_ngc_sdk::error::json_to_prost_value(
                                        &serde_json::json!("gold"),
                                    )),
                                })),
                            },
                            Expr {
                                node: Some(expr::Node::Cmp(Cmp {
                                    path: "credit_cents".to_owned(),
                                    op: PredicateOp::PGte as i32,
                                    value: Some(gap_ngc_sdk::error::json_to_prost_value(
                                        &serde_json::json!(200_000),
                                    )),
                                })),
                            },
                        ],
                    })),
                },
                Expr {
                    node: Some(expr::Node::Exists("notes".to_owned())),
                },
            ],
        })),
    };

    let res = collect_stream(
        conn.query(
            QueryRequest {
                operation: "customers".to_owned(),
                predicate: Some(pred),
                order_by: vec![OrderBy {
                    path: "credit_cents".to_owned(),
                    descending: true,
                }],
                ..QueryRequest::default()
            },
            scope_for_tenant("tenant-a"),
        )
        .await
        .expect("compound predicate list ok"),
    )
    .await;

    let ids: Vec<i64> = res.rows_by_index[&0]
        .iter()
        .map(|r| r["id"].as_f64().expect("numeric id") as i64)
        .collect();
    // id=4 (eu, gold, 152000, notes present), id=1 (eu, gold, 125000, notes present); id=2 has NULL notes
    assert_eq!(ids, vec![4, 1]);

    // Cursor pagination + binding checks (`NGC-022`)
    let page1_req = QueryRequest {
        operation: "customers".to_owned(),
        order_by: vec![OrderBy {
            path: "id".to_owned(),
            descending: false,
        }],
        page: Some(Page {
            limit: 5,
            cursor: String::new(),
            offset: 0,
        }),
        ..QueryRequest::default()
    };
    let page1 = collect_stream(
        conn.query(page1_req.clone(), scope_for_tenant("tenant-a"))
            .await
            .expect("page1 ok"),
    )
    .await;
    assert_eq!(page1.rows_by_index[&0].len(), 5);
    assert!(!page1.complete.next_cursor.is_empty());

    // Wrong tenant rejected
    let wrong_tenant = conn
        .query(
            QueryRequest {
                page: Some(Page {
                    limit: 5,
                    cursor: page1.complete.next_cursor.clone(),
                    offset: 0,
                }),
                ..page1_req.clone()
            },
            scope_for_tenant("tenant-b"),
        )
        .await;
    assert_eq!(
        wrong_tenant.err().map(|e| e.code),
        Some(NgcErrorCode::InvalidArgument)
    );

    // Wrong arguments rejected
    let wrong_args = conn
        .query(
            QueryRequest {
                arguments: Some(make_struct(vec![("region", serde_json::json!("eu"))])),
                page: Some(Page {
                    limit: 5,
                    cursor: page1.complete.next_cursor.clone(),
                    offset: 0,
                }),
                ..page1_req.clone()
            },
            scope_for_tenant("tenant-a"),
        )
        .await;
    assert_eq!(
        wrong_args.err().map(|e| e.code),
        Some(NgcErrorCode::InvalidArgument)
    );

    // Expired cursor rejected (spec allows >=10m; encode_with_ttl(ZERO) expires immediately)
    assert!(CURSOR_TTL >= Duration::from_secs(10 * 60));
    let mut expired_probe = page1_req.clone();
    expired_probe.page = None;
    let q_hash = cursor::query_hash(&expired_probe);
    let expired_tok = cursor::encode_with_ttl(
        "customers",
        &q_hash,
        "tenant-a",
        5,
        Duration::from_millis(1),
    )
    .expect("encode short ttl");
    std::thread::sleep(Duration::from_millis(5));
    let expired_res = conn
        .query(
            QueryRequest {
                page: Some(Page {
                    limit: 5,
                    cursor: expired_tok,
                    offset: 0,
                }),
                ..page1_req
            },
            scope_for_tenant("tenant-a"),
        )
        .await;
    assert_eq!(
        expired_res.err().map(|e| e.code),
        Some(NgcErrorCode::InvalidArgument)
    );

    // Unadvertised predicate operator (`P_CONTAINS`) rejected cleanly (`NGC-023`)
    let contains_res = conn
        .query(
            QueryRequest {
                operation: "customers".to_owned(),
                predicate: Some(Expr {
                    node: Some(expr::Node::Cmp(Cmp {
                        path: "name".to_owned(),
                        op: PredicateOp::PContains as i32,
                        value: Some(gap_ngc_sdk::error::json_to_prost_value(&serde_json::json!(
                            "Ada"
                        ))),
                    })),
                }),
                ..QueryRequest::default()
            },
            scope(),
        )
        .await;
    assert_eq!(
        contains_res.err().map(|e| e.code),
        Some(NgcErrorCode::InvalidArgument)
    );
}

// spec: CMP-050, CMP-051
#[tokio::test]
async fn adversarial_sql_injection_and_read_only_guards_hold() {
    // 1. Config rejects malicious table or column identifiers (`CMP-050`)
    let mut bad_cfg = shop_config(Dialect::Sqlite);
    bad_cfg.operations[0].table = "customers; DROP TABLE customers; --".to_owned();
    let driver = RecordingDriver::default();
    assert!(SqlConnector::new(bad_cfg, driver).is_err());

    let conn = sqlite_shop_connector(shop_config(Dialect::Sqlite));

    // 2. Hostile value payloads remain strictly bound as literals and cannot alter query structure
    for payload in [
        "' OR '1'='1",
        "eu'; DROP TABLE customers; --",
        "eu' UNION SELECT 1,2,3,4,5,6,7 --",
    ] {
        let out = collect_stream(
            conn.query(
                QueryRequest {
                    operation: "customers".to_owned(),
                    arguments: Some(make_struct(vec![("region", serde_json::json!(payload))])),
                    ..QueryRequest::default()
                },
                scope(),
            )
            .await
            .expect("parameterized query returns 0 rows safely"),
        )
        .await;
        assert!(out.rows_by_index.get(&0).is_none_or(Vec::is_empty));
    }

    // 3. Injection via identifier slots (arguments key, selection, order_by, predicate path) fails INVALID_ARGUMENT
    let bad_arg = conn
        .query(
            QueryRequest {
                operation: "customers".to_owned(),
                arguments: Some(make_struct(vec![(
                    "region = 'eu' OR 1=1 --",
                    serde_json::json!("eu"),
                )])),
                ..QueryRequest::default()
            },
            scope(),
        )
        .await;
    assert_eq!(
        bad_arg.err().map(|e| e.code),
        Some(NgcErrorCode::InvalidArgument)
    );

    let bad_order = conn
        .query(
            QueryRequest {
                operation: "customers".to_owned(),
                order_by: vec![OrderBy {
                    path: "id; DROP TABLE customers --".to_owned(),
                    descending: false,
                }],
                ..QueryRequest::default()
            },
            scope(),
        )
        .await;
    assert_eq!(
        bad_order.err().map(|e| e.code),
        Some(NgcErrorCode::InvalidArgument)
    );

    let bad_selection = conn
        .query(
            QueryRequest {
                operation: "customers".to_owned(),
                selection: Some(Selection {
                    fields: vec![SelectionField {
                        name: "id, (SELECT sqlite_version())".to_owned(),
                        ..SelectionField::default()
                    }],
                }),
                ..QueryRequest::default()
            },
            scope(),
        )
        .await;
    assert_eq!(
        bad_selection.err().map(|e| e.code),
        Some(NgcErrorCode::InvalidArgument)
    );

    // 4. Verify table was untouched and `PRAGMA query_only` + file `open()` block DML/DDL (`CMP-050`)
    let tmp_path = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!(
        "gap_ngc_sql_readonly_{}.sqlite3",
        std::process::id()
    ));
    {
        let setup_conn = rusqlite::Connection::open(&tmp_path).expect("create sqlite fixture");
        setup_conn.execute_batch(&shop_sql()).expect("seed fixture");
    }
    let ro_driver = SqliteDriver::open(tmp_path.to_str().expect("utf8 path")).expect("open RO");
    let dml_res = ro_driver
        .query(
            "DELETE FROM customers WHERE id = 1".to_owned(),
            Vec::new(),
            10,
            scope(),
        )
        .await;
    let _ = std::fs::remove_file(&tmp_path);
    assert!(dml_res.is_err(), "read-only connection must reject DELETE");
}

// spec: CMP-050, NGC-025, NGC-027
#[tokio::test]
async fn statement_timeout_interrupts_sqlite_and_row_cap_flags_truncated() {
    // 1. Real SQLite recursive CTE interrupted by statement deadline via InterruptWatchdog
    let mem = rusqlite::Connection::open_in_memory().expect("open in-memory");
    let driver = SqliteDriver::from_connection(mem).expect("ro driver");
    let slow_scope = CallScope::for_test(Instant::now() + Duration::from_millis(25));
    let slow_res = driver
        .query(
            "WITH RECURSIVE cnt(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM cnt WHERE x < 500000000) SELECT COUNT(*) AS total FROM cnt".to_owned(),
            Vec::new(),
            1,
            slow_scope,
        )
        .await;
    assert!(
        matches!(slow_res, Err(DriverError::Timeout(_))),
        "expected DriverError::Timeout from interrupted CTE, got: {slow_res:?}"
    );

    // 2. Requesting page.limit > op.row_cap marks response `truncated = true` (`NGC-027`)
    let mut cfg = shop_config(Dialect::Sqlite);
    cfg.operations[1].row_cap = 4;
    let conn = sqlite_shop_connector(cfg);
    let out = collect_stream(
        conn.query(
            QueryRequest {
                operation: "customers".to_owned(),
                page: Some(Page {
                    limit: 20,
                    cursor: String::new(),
                    offset: 0,
                }),
                ..QueryRequest::default()
            },
            scope(),
        )
        .await
        .expect("list ok"),
    )
    .await;
    assert_eq!(out.rows_by_index[&0].len(), 4);
    assert!(out.complete.truncated);
    assert!(!out.complete.next_cursor.is_empty());
}

// spec: NGC-050, CMP-050
#[tokio::test]
async fn sketch_computes_salted_blake3_bottom_k_hashes_and_ratios() {
    let conn = sqlite_shop_connector(shop_config(Dialect::Sqlite));

    // Whole domain fits within sample_size = 100 -> `exact = true`
    let full = conn
        .sketch(
            SketchRequest {
                operation: "customers".to_owned(),
                field_path: "tier".to_owned(),
                sample_size: 100,
                salt: b"tenant-salt".to_vec(),
                bottom_k: 3,
                ..SketchRequest::default()
            },
            scope(),
        )
        .await
        .expect("full tier sketch ok");
    assert!(full.exact);
    assert_eq!(full.sample_count, 12);
    assert_eq!(full.distinct_estimate, 4);
    assert_eq!(full.bottom_k_hashes.len(), 3);
    assert!(full.bottom_k_hashes.windows(2).all(|w| w[0] < w[1]));
    assert!((full.null_ratio - 0.0).abs() < 1e-6);
    assert!((full.uniqueness_ratio - (4.0 / 12.0)).abs() < 1e-6);
    assert_eq!(full.value_kind, "string");

    // Sample smaller than table -> `exact = false`
    let partial = conn
        .sketch(
            SketchRequest {
                operation: "customers".to_owned(),
                field_path: "notes".to_owned(),
                sample_size: 6,
                salt: b"tenant-salt".to_vec(),
                bottom_k: 16,
                ..SketchRequest::default()
            },
            scope(),
        )
        .await
        .expect("partial notes sketch ok");
    assert!(!partial.exact);
    assert_eq!(partial.sample_count, 6);
    assert!(partial.null_ratio > 0.0);
}
