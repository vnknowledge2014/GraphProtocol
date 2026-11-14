//! `SqlConnector<D>` implementing the NGC `Connector` trait (`CMP-050`, `NGC-020`, `NGC-050`).

use std::collections::BTreeSet;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::task::{Context, Poll};
use std::time::Duration;

use futures_util::stream::{self, Stream, StreamExt};
use gap_core::error::{NgcErrorCode, NgcErrorInfo};
use gap_ngc_sdk::connector::{BoxStream, CallScope, Connector, NgcResult};
use gap_ngc_sdk::error::json_map_to_prost_struct;
use gap_ngc_sdk::foreach::{ForeachConfig, foreach};
use gap_proto::NGC_WIRE_VERSION;
use gap_proto::ngc::v1::{
    Capabilities, Complete, ConnectorDescription, DescribeRequest, DescribeResponse, HealthRequest,
    HealthResponse, Limits, OpKind, OperationDescriptor, PaginationKind, PredicateOp, Protocol,
    QueryRequest, QueryResponse, RowSetChunk, Selection, ServingStatus, SideEffectHint,
    SketchRequest, SketchResponse, query_response,
};
use serde_json::{Map, Value, json};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::time::{Instant, sleep, sleep_until};

use crate::config::{Dialect, OperationConfig, OperationKind, SqlConfig};
use crate::driver::{Driver, SqlParam, SqlRow, SqlValue};
use crate::template::{
    BoundStatement, build_foreach_params, build_get_by_key, build_list_with_args,
    build_sketch_sample, render_simple_predicate, validate_foreach_key,
};
use crate::{cursor, sketch};

const SATURATED_RETRY_AFTER_MS: u32 = 100;

fn invalid(msg: impl Into<String>) -> NgcErrorInfo {
    NgcErrorInfo::new(NgcErrorCode::InvalidArgument, msg)
}

fn not_found(msg: impl Into<String>) -> NgcErrorInfo {
    NgcErrorInfo::new(NgcErrorCode::NotFound, msg)
}

fn check_scope(scope: &CallScope) -> NgcResult<()> {
    if scope.cancel.is_cancelled() {
        return Err(NgcErrorInfo::new(NgcErrorCode::Cancelled, "call cancelled"));
    }
    if Instant::now() >= scope.deadline {
        return Err(NgcErrorInfo::new(
            NgcErrorCode::DeadlineExceeded,
            "call deadline exceeded",
        ));
    }
    Ok(())
}

/// RAII stream wrapper holding an in-flight semaphore permit until dropped (`NGC-071`, `NGC-090`).
struct GuardedQueryStream {
    inner: BoxStream<QueryResponse>,
    _permit: OwnedSemaphorePermit,
}

impl Stream for GuardedQueryStream {
    type Item = NgcResult<QueryResponse>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.inner.as_mut().poll_next(cx)
    }
}

/// Exact protobuf varint byte length of a non-negative `u64`.
const fn varint_len_u64(mut v: u64) -> usize {
    let mut len = 1;
    while v >= 0x80 {
        v >>= 7;
        len += 1;
    }
    len
}

fn varint_len_usize(v: usize) -> usize {
    varint_len_u64(u64::try_from(v).unwrap_or(u64::MAX))
}

fn encoded_prost_value_len(val: &prost_types::Value) -> usize {
    use prost_types::value::Kind;
    match &val.kind {
        None => 0,
        Some(Kind::NullValue(v)) => {
            #[allow(clippy::cast_sign_loss)]
            let bits = *v as i64 as u64;
            1 + varint_len_u64(bits)
        }
        Some(Kind::NumberValue(_)) => 1 + 8,
        Some(Kind::StringValue(s)) => 1 + varint_len_usize(s.len()) + s.len(),
        Some(Kind::BoolValue(_)) => 1 + 1,
        Some(Kind::StructValue(st)) => {
            let inner = encoded_prost_struct_len(st);
            1 + varint_len_usize(inner) + inner
        }
        Some(Kind::ListValue(list)) => {
            let inner: usize = list
                .values
                .iter()
                .map(|elem| {
                    let elem_len = encoded_prost_value_len(elem);
                    1 + varint_len_usize(elem_len) + elem_len
                })
                .sum();
            1 + varint_len_usize(inner) + inner
        }
    }
}

/// Exact prost wire encoded byte length of a [`prost_types::Struct`] (`NGC-027`).
fn encoded_prost_struct_len(st: &prost_types::Struct) -> usize {
    st.fields
        .iter()
        .map(|(k, v)| {
            let key_len = if k.is_empty() {
                0
            } else {
                1 + varint_len_usize(k.len()) + k.len()
            };
            let v_inner = encoded_prost_value_len(v);
            let val_len = 1 + varint_len_usize(v_inner) + v_inner;
            let entry_len = key_len + val_len;
            1 + varint_len_usize(entry_len) + entry_len
        })
        .sum()
}

fn pascal_case(ident: &str) -> String {
    let mut out = String::with_capacity(ident.len());
    for part in ident.split('_') {
        let mut chars = part.chars();
        if let Some(first) = chars.next() {
            out.push(first.to_ascii_uppercase());
            out.extend(chars);
        }
    }
    if out.is_empty() {
        "Row".to_owned()
    } else {
        out
    }
}

fn build_operation_descriptor(op: &OperationConfig) -> OperationDescriptor {
    let mut out_props = Map::with_capacity(op.columns.len());
    let mut out_req = Vec::new();
    for col in &op.columns {
        out_props.insert(
            col.name.clone(),
            json!({ "type": col.sql_type.json_schema_type() }),
        );
        if !col.nullable {
            out_req.push(Value::String(col.name.clone()));
        }
    }
    let mut out_schema = Map::new();
    out_schema.insert(
        "$schema".to_owned(),
        Value::String("https://json-schema.org/draft/2020-12/schema".to_owned()),
    );
    out_schema.insert("type".to_owned(), Value::String("object".to_owned()));
    out_schema.insert("properties".to_owned(), Value::Object(out_props));
    if !out_req.is_empty() {
        out_schema.insert("required".to_owned(), Value::Array(out_req));
    }

    let mut in_props = Map::new();
    let mut in_req = Vec::new();
    match op.kind {
        OperationKind::GetByKey => {
            for k in &op.key_columns {
                if let Some(col) = op.column(k) {
                    in_props.insert(
                        col.name.clone(),
                        json!({ "type": col.sql_type.json_schema_type() }),
                    );
                    in_req.push(Value::String(col.name.clone()));
                }
            }
        }
        OperationKind::List => {
            for col in &op.columns {
                in_props.insert(
                    col.name.clone(),
                    json!({ "type": col.sql_type.json_schema_type() }),
                );
            }
        }
    }
    let mut in_schema = Map::new();
    in_schema.insert(
        "$schema".to_owned(),
        Value::String("https://json-schema.org/draft/2020-12/schema".to_owned()),
    );
    in_schema.insert("type".to_owned(), Value::String("object".to_owned()));
    in_schema.insert("properties".to_owned(), Value::Object(in_props));
    if !in_req.is_empty() {
        in_schema.insert("required".to_owned(), Value::Array(in_req));
    }
    in_schema.insert("additionalProperties".to_owned(), Value::Bool(false));

    OperationDescriptor {
        name: op.name.clone(),
        kind: OpKind::Query as i32,
        description: format!("Read from table {}", op.table),
        input_schema: Some(json_map_to_prost_struct(&in_schema)),
        output_schema: Some(json_map_to_prost_struct(&out_schema)),
        side_effect_hint: SideEffectHint::SeNone as i32,
        idempotent_hint: true,
        supports_dry_run: false,
        compensation_operation: String::new(),
        examples: Vec::new(),
        cost_weight: 1,
        pagination: match op.kind {
            OperationKind::GetByKey => PaginationKind::PaginationNone as i32,
            OperationKind::List => PaginationKind::PaginationCursor as i32,
        },
        key_fields: op.key_columns.clone(),
        annotations: std::collections::HashMap::new(),
    }
}

fn build_schema_sdl(config: &SqlConfig) -> String {
    let mut types = String::new();
    let mut query_fields = Vec::with_capacity(config.operations.len());

    for op in &config.operations {
        let row_type = format!("{}Row", pascal_case(&op.name));
        types.push_str(&format!("type {row_type} {{\n"));
        for col in &op.columns {
            let bang = if col.nullable { "" } else { "!" };
            types.push_str(&format!(
                "  {}: {}{}\n",
                col.name,
                col.sql_type.graphql_scalar(),
                bang
            ));
        }
        types.push_str("}\n\n");

        let args: Vec<String> = match op.kind {
            OperationKind::GetByKey => op
                .key_columns
                .iter()
                .filter_map(|k| {
                    op.column(k)
                        .map(|c| format!("{}: {}!", c.name, c.sql_type.graphql_scalar()))
                })
                .collect(),
            OperationKind::List => op
                .columns
                .iter()
                .map(|c| format!("{}: {}", c.name, c.sql_type.graphql_scalar()))
                .collect(),
        };
        let args_str = if args.is_empty() {
            String::new()
        } else {
            format!("({})", args.join(", "))
        };
        let ret_ty = match op.kind {
            OperationKind::GetByKey => row_type,
            OperationKind::List => format!("[{row_type}!]!"),
        };
        query_fields.push(format!("  {}{args_str}: {ret_ty}", op.name));
    }

    format!("{types}type Query {{\n{}\n}}\n", query_fields.join("\n"))
}

fn compute_spec_hash(config: &SqlConfig, schema_sdl: &str, max_batch_size: u32) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(config.connector_id.as_bytes());
    hasher.update(b"\x00");
    hasher.update(config.connector_version.as_bytes());
    hasher.update(b"\x00");
    hasher.update(match config.dialect {
        Dialect::Sqlite => b"sqlite" as &[u8],
        Dialect::Postgres => b"postgres" as &[u8],
    });
    hasher.update(b"\x00");
    hasher.update(&max_batch_size.to_le_bytes());
    hasher.update(&config.max_in_flight.to_le_bytes());
    hasher.update(&config.max_page_size.to_le_bytes());
    hasher.update(&config.max_response_bytes.to_le_bytes());
    hasher.update(schema_sdl.as_bytes());
    hasher.update(b"\x00");
    for op in &config.operations {
        hasher.update(op.name.as_bytes());
        hasher.update(&[match op.kind {
            OperationKind::GetByKey => 1,
            OperationKind::List => 2,
        }]);
        hasher.update(op.table.as_bytes());
        hasher.update(&op.row_cap.to_le_bytes());
        hasher.update(&op.statement_timeout_ms.to_le_bytes());
        hasher.update(&op.simulated_latency_ms.to_le_bytes());
        for col in &op.columns {
            hasher.update(col.name.as_bytes());
            hasher.update(col.sql_type.json_schema_type().as_bytes());
            hasher.update(&[u8::from(col.nullable)]);
        }
        for key in &op.key_columns {
            hasher.update(key.as_bytes());
        }
    }
    hasher.finalize().to_hex().to_string()
}

fn build_description(config: &SqlConfig) -> ConnectorDescription {
    let has_foreach = config
        .operations
        .iter()
        .any(|op| op.kind == OperationKind::GetByKey && op.key_columns.len() == 1);
    let max_batch_size = if has_foreach {
        config.max_batch_size
    } else {
        0
    };
    let schema_sdl = build_schema_sdl(config);
    let spec_hash = compute_spec_hash(config, &schema_sdl, max_batch_size);
    let default_timeout_ms = config
        .operations
        .iter()
        .map(|op| u32::try_from(op.statement_timeout_ms).unwrap_or(u32::MAX))
        .min()
        .unwrap_or(30_000);
    let ops = config
        .operations
        .iter()
        .map(build_operation_descriptor)
        .collect();

    ConnectorDescription {
        connector_id: config.connector_id.clone(),
        connector_version: config.connector_version.clone(),
        ngc_version: NGC_WIRE_VERSION.to_owned(),
        protocol: Protocol::Sql as i32,
        spec_hash,
        capabilities: Some(Capabilities {
            foreach_batch: has_foreach,
            predicate_ops: vec![
                PredicateOp::PEq as i32,
                PredicateOp::PNe as i32,
                PredicateOp::PGt as i32,
                PredicateOp::PGte as i32,
                PredicateOp::PLt as i32,
                PredicateOp::PLte as i32,
                PredicateOp::PIn as i32,
                PredicateOp::PExists as i32,
            ],
            order_by: true,
            pagination: PaginationKind::PaginationCursor as i32,
            native_traverse: false,
            subscribe: false,
            tasks: false,
            dry_run: false,
            idempotency: false,
            sketch: true,
            explain: false,
            as_of: false,
        }),
        limits: Some(Limits {
            max_batch_size,
            max_in_flight: config.max_in_flight,
            max_page_size: config.max_page_size,
            default_timeout_ms,
            max_response_bytes: config.max_response_bytes,
        }),
        schema_sdl,
        operations: ops,
        meta: None,
        auth: None,
    }
}

fn extract_selection<'a>(
    op: &'a OperationConfig,
    selection: Option<&Selection>,
) -> NgcResult<Vec<&'a str>> {
    let Some(sel) = selection else {
        return Ok(Vec::new());
    };
    if sel.fields.is_empty() {
        return Ok(Vec::new());
    }
    let allow = op.column_allow_list();
    let mut seen = BTreeSet::new();
    for field in &sel.fields {
        if !allow.contains(field.name.as_str()) {
            return Err(invalid(format!("unknown selected column '{}'", field.name)));
        }
        if !seen.insert(field.name.as_str()) {
            return Err(invalid(format!(
                "duplicate selected column '{}'",
                field.name
            )));
        }
    }
    Ok(op
        .columns
        .iter()
        .filter(|col| seen.contains(col.name.as_str()))
        .map(|col| col.name.as_str())
        .collect())
}

fn sql_param_matches_value(param: &SqlParam, value: &SqlValue) -> bool {
    match (param, value) {
        (SqlParam::Integer(a), SqlValue::Integer(b)) => a == b,
        (SqlParam::Integer(a), SqlValue::Real(b)) => (*a as f64) == *b,
        (SqlParam::Real(a), SqlValue::Real(b)) => {
            let na = if *a == 0.0 { 0.0 } else { *a };
            let nb = if *b == 0.0 { 0.0 } else { *b };
            na == nb
        }
        (SqlParam::Real(a), SqlValue::Integer(b)) => {
            let na = if *a == 0.0 { 0.0 } else { *a };
            na == (*b as f64)
        }
        (SqlParam::Text(a), SqlValue::Text(b)) => a == b,
        (SqlParam::Bool(a), SqlValue::Bool(b)) => a == b,
        (SqlParam::Bool(a), SqlValue::Integer(b)) => *a == (*b != 0),
        (SqlParam::Blob(a), SqlValue::Blob(b)) => a == b,
        _ => false,
    }
}

fn row_to_struct(op: &OperationConfig, row: SqlRow, selected: &[&str]) -> prost_types::Struct {
    let fields = if selected.is_empty() {
        row.into_iter()
            .map(|(k, v)| {
                let ty = op.column(&k).map(|c| &c.sql_type);
                (k, v.into_prost_typed(ty))
            })
            .collect()
    } else {
        row.into_iter()
            .filter(|(k, _)| selected.contains(&k.as_str()))
            .map(|(k, v)| {
                let ty = op.column(&k).map(|c| &c.sql_type);
                (k, v.into_prost_typed(ty))
            })
            .collect()
    };
    prost_types::Struct { fields }
}

struct SqlShared<D> {
    config: SqlConfig,
    description: ConnectorDescription,
    in_flight_gate: Arc<Semaphore>,
    driver: D,
}

impl<D: Driver> SqlShared<D> {
    async fn exec_bound(
        &self,
        stmt: BoundStatement,
        row_cap: usize,
        statement_timeout_ms: u64,
        simulated_latency_ms: u64,
        scope: &CallScope,
    ) -> NgcResult<Vec<SqlRow>> {
        check_scope(scope)?;
        let op_deadline = Instant::now() + Duration::from_millis(statement_timeout_ms.max(1));
        let effective_deadline = scope.deadline.min(op_deadline);

        if simulated_latency_ms > 0 {
            tokio::select! {
                biased;
                () = scope.cancel.cancelled() => {
                    return Err(NgcErrorInfo::new(NgcErrorCode::Cancelled, "call cancelled"));
                }
                () = sleep_until(effective_deadline) => {
                    return Err(NgcErrorInfo::new(NgcErrorCode::DeadlineExceeded, "query deadline exceeded"));
                }
                () = sleep(Duration::from_millis(simulated_latency_ms)) => {}
            }
        }

        let mut query_scope = scope.clone();
        query_scope.deadline = effective_deadline;

        tokio::select! {
            biased;
            () = scope.cancel.cancelled() => {
                Err(NgcErrorInfo::new(NgcErrorCode::Cancelled, "call cancelled"))
            }
            () = sleep_until(effective_deadline) => {
                Err(NgcErrorInfo::new(NgcErrorCode::DeadlineExceeded, "query deadline exceeded"))
            }
            res = self.driver.query(stmt.sql, stmt.params, row_cap, query_scope) => {
                res.map_err(|e| e.into_ngc())
            }
        }
    }
}

/// SQL Native Graph Connector generic over a [`Driver`] backend (`CMP-050`).
pub struct SqlConnector<D: Driver> {
    shared: Arc<SqlShared<D>>,
}

impl<D: Driver> Clone for SqlConnector<D> {
    fn clone(&self) -> Self {
        Self {
            shared: Arc::clone(&self.shared),
        }
    }
}

impl<D: Driver> std::fmt::Debug for SqlConnector<D> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SqlConnector")
            .field("connector_id", &self.shared.config.connector_id)
            .field("spec_hash", &self.shared.description.spec_hash)
            .finish_non_exhaustive()
    }
}

impl<D: Driver> SqlConnector<D> {
    /// Validates `config`, precomputes the descriptor, and constructs a [`SqlConnector`].
    pub fn new(config: SqlConfig, driver: D) -> Result<Self, String> {
        config.validate()?;
        let description = build_description(&config);
        let permits = usize::try_from(config.max_in_flight).unwrap_or(usize::MAX);
        Ok(Self {
            shared: Arc::new(SqlShared {
                config,
                description,
                in_flight_gate: Arc::new(Semaphore::new(permits)),
                driver,
            }),
        })
    }

    /// Borrows the precomputed [`ConnectorDescription`].
    #[must_use]
    pub fn description(&self) -> &ConnectorDescription {
        &self.shared.description
    }

    fn op(&self, name: &str) -> NgcResult<&OperationConfig> {
        self.shared
            .config
            .operation(name)
            .ok_or_else(|| not_found(format!("unknown operation '{name}'")))
    }

    fn acquire_permit(&self) -> NgcResult<OwnedSemaphorePermit> {
        Arc::clone(&self.shared.in_flight_gate)
            .try_acquire_owned()
            .map_err(|_| {
                NgcErrorInfo::new(
                    NgcErrorCode::ResourceExhausted,
                    "connector max_in_flight capacity exhausted",
                )
                .with_retry(true, SATURATED_RETRY_AFTER_MS)
            })
    }

    fn start_foreach(
        &self,
        op: &OperationConfig,
        request: QueryRequest,
        selected_names: Vec<String>,
        scope: &CallScope,
        permit: OwnedSemaphorePermit,
    ) -> NgcResult<BoxStream<QueryResponse>> {
        if request.page.is_some() {
            return Err(invalid("pagination cannot be combined with foreach"));
        }
        if !request.order_by.is_empty() {
            return Err(invalid("order_by cannot be combined with foreach"));
        }
        if op.kind != OperationKind::GetByKey || op.key_columns.len() != 1 {
            return Err(invalid(format!(
                "operation '{}' does not support foreach batching",
                op.name
            )));
        }

        let dialect = self.shared.config.dialect;
        if let Some(pred) = &request.predicate {
            let _ = render_simple_predicate(op, dialect, pred, 0)?;
            return Err(invalid("predicate is not supported with foreach"));
        }

        let op_cfg = op.clone();
        let static_args = request.arguments.clone().unwrap_or_default();
        let batch_cap = usize::try_from(self.shared.config.max_batch_size)
            .unwrap_or(1)
            .max(1);
        let in_flight_cap = usize::try_from(self.shared.config.max_in_flight)
            .unwrap_or(1)
            .max(1);
        let foreach_cfg = ForeachConfig::new(batch_cap, in_flight_cap);

        let shared = Arc::clone(&self.shared);
        let scope_owned = scope.clone();
        let upstream_calls = Arc::new(AtomicU32::new(0));
        let calls_for_complete = Arc::clone(&upstream_calls);

        let mut fetch_names = selected_names.clone();
        if !fetch_names.is_empty() {
            for key_col in &op_cfg.key_columns {
                if !fetch_names.contains(key_col) {
                    fetch_names.push(key_col.clone());
                }
            }
        }

        let run = foreach(request.foreach, foreach_cfg, scope, move |var_structs| {
            let shared = Arc::clone(&shared);
            let op_cfg = op_cfg.clone();
            let static_args = static_args.clone();
            let scope_owned = scope_owned.clone();
            let selected_names = selected_names.clone();
            let fetch_names = fetch_names.clone();
            let upstream_calls = Arc::clone(&upstream_calls);

            async move {
                let mut outcomes: Vec<Option<NgcResult<Vec<prost_types::Struct>>>> =
                    vec![None; var_structs.len()];
                let mut valid_positions: Vec<(usize, SqlParam)> =
                    Vec::with_capacity(var_structs.len());

                for (pos, vars) in var_structs.iter().enumerate() {
                    match validate_foreach_key(&op_cfg, &static_args, vars) {
                        Ok(param) => valid_positions.push((pos, param)),
                        Err(err) => outcomes[pos] = Some(Err(err)),
                    }
                }

                if !valid_positions.is_empty() {
                    let fetch_refs: Vec<&str> = fetch_names.iter().map(String::as_str).collect();
                    let key_params: Vec<SqlParam> =
                        valid_positions.iter().map(|(_, p)| p.clone()).collect();
                    let stmt = match build_foreach_params(&op_cfg, dialect, key_params, &fetch_refs)
                    {
                        Ok(s) => s,
                        Err(err) => {
                            for (pos, _) in valid_positions {
                                outcomes[pos] = Some(Err(err.clone()));
                            }
                            let finalized = outcomes
                                .into_iter()
                                .map(|o| {
                                    o.unwrap_or_else(|| {
                                        Err(NgcErrorInfo::new(
                                            NgcErrorCode::Internal,
                                            "unpopulated foreach slot",
                                        ))
                                    })
                                })
                                .collect();
                            return Ok(finalized);
                        }
                    };

                    upstream_calls.fetch_add(1, Ordering::AcqRel);
                    let row_cap = op_cfg.row_cap.max(1) as usize;
                    let rows = match shared
                        .exec_bound(
                            stmt,
                            row_cap,
                            op_cfg.statement_timeout_ms,
                            op_cfg.simulated_latency_ms,
                            &scope_owned,
                        )
                        .await
                    {
                        Ok(r) => r,
                        Err(err) => {
                            if matches!(
                                err.code,
                                NgcErrorCode::Cancelled | NgcErrorCode::DeadlineExceeded
                            ) {
                                return Err(err);
                            }
                            for (pos, _) in valid_positions {
                                outcomes[pos] = Some(Err(err.clone()));
                            }
                            let finalized = outcomes
                                .into_iter()
                                .map(|o| {
                                    o.unwrap_or_else(|| {
                                        Err(NgcErrorInfo::new(
                                            NgcErrorCode::Internal,
                                            "unpopulated foreach slot",
                                        ))
                                    })
                                })
                                .collect();
                            return Ok(finalized);
                        }
                    };

                    let key_col = &op_cfg.key_columns[0];
                    let selected_refs: Vec<&str> =
                        selected_names.iter().map(String::as_str).collect();
                    for (pos, param) in valid_positions {
                        let matched: Vec<prost_types::Struct> = rows
                            .iter()
                            .filter(|row| {
                                row.get(key_col.as_str())
                                    .is_some_and(|val| sql_param_matches_value(&param, val))
                            })
                            .cloned()
                            .map(|row| row_to_struct(&op_cfg, row, &selected_refs))
                            .collect();
                        outcomes[pos] = Some(Ok(matched));
                    }
                }

                let finalized = outcomes
                    .into_iter()
                    .map(|o| {
                        o.unwrap_or_else(|| {
                            Err(NgcErrorInfo::new(
                                NgcErrorCode::Internal,
                                "unpopulated foreach slot",
                            ))
                        })
                    })
                    .collect();
                Ok(finalized)
            }
        })?;

        let (chunk_stream, _) = run.into_parts();
        struct ForeachStreamState {
            chunks: BoxStream<RowSetChunk>,
            upstream_calls: Arc<AtomicU32>,
            rows_total: u64,
            done: bool,
        }

        let inner: BoxStream<QueryResponse> = Box::pin(stream::unfold(
            ForeachStreamState {
                chunks: chunk_stream,
                upstream_calls: calls_for_complete,
                rows_total: 0,
                done: false,
            },
            |mut st| async move {
                if st.done {
                    return None;
                }
                match st.chunks.next().await {
                    Some(Ok(chunk)) => {
                        st.rows_total = st.rows_total.saturating_add(chunk.rows.len() as u64);
                        Some((
                            Ok(QueryResponse {
                                chunk: Some(query_response::Chunk::Rows(chunk)),
                            }),
                            st,
                        ))
                    }
                    Some(Err(err)) => {
                        st.done = true;
                        Some((Err(err), st))
                    }
                    None => {
                        st.done = true;
                        Some((
                            Ok(QueryResponse {
                                chunk: Some(query_response::Chunk::Complete(Complete {
                                    next_cursor: String::new(),
                                    truncated: false,
                                    rows_total: st.rows_total,
                                    upstream_calls: st.upstream_calls.load(Ordering::Acquire),
                                    latency_us: 0,
                                })),
                            }),
                            st,
                        ))
                    }
                }
            },
        ));

        Ok(Box::pin(GuardedQueryStream {
            inner,
            _permit: permit,
        }))
    }
}

impl<D: Driver> Connector for SqlConnector<D> {
    async fn describe(
        &self,
        request: DescribeRequest,
        scope: CallScope,
    ) -> NgcResult<DescribeResponse> {
        check_scope(&scope)?;
        if !request.known_spec_hash.is_empty()
            && request.known_spec_hash == self.shared.description.spec_hash
        {
            return Ok(DescribeResponse {
                not_modified: true,
                description: None,
            });
        }
        Ok(DescribeResponse {
            not_modified: false,
            description: Some(self.shared.description.clone()),
        })
    }

    async fn query(
        &self,
        request: QueryRequest,
        scope: CallScope,
    ) -> NgcResult<BoxStream<QueryResponse>> {
        check_scope(&scope)?;

        if !request.as_of.is_empty() {
            return Err(invalid(
                "as_of time travel is not supported by this connector",
            ));
        }

        let op = self.op(&request.operation)?.clone();
        let dialect = self.shared.config.dialect;
        let selected_cols = extract_selection(&op, request.selection.as_ref())?;
        let selected_names: Vec<String> = selected_cols.iter().map(|s| (*s).to_owned()).collect();
        let selected_refs: Vec<&str> = selected_names.iter().map(String::as_str).collect();

        let permit = self.acquire_permit()?;

        if !request.foreach.is_empty() {
            return self.start_foreach(&op, request, selected_names, &scope, permit);
        }

        // Validate predicate operators upfront so unadvertised operators fail `INVALID_ARGUMENT` (`NGC-023`).
        if let Some(pred) = &request.predicate {
            let _ = render_simple_predicate(&op, dialect, pred, 0)?;
        }

        let (stmt, q_hash, start_offset, requested_limit, truncated_by_row_cap, is_list) = match op
            .kind
        {
            OperationKind::GetByKey => {
                if request.predicate.is_some() {
                    return Err(invalid(
                        "predicate is not supported on get_by_key operations",
                    ));
                }
                if !request.order_by.is_empty() {
                    return Err(invalid(
                        "order_by is not supported on get_by_key operations",
                    ));
                }
                if let Some(page) = &request.page
                    && (!page.cursor.is_empty() || page.limit > 0 || page.offset > 0)
                {
                    return Err(invalid(
                        "pagination is not supported on get_by_key operations",
                    ));
                }
                let args = request.arguments.as_ref().cloned().unwrap_or_default();
                let stmt = build_get_by_key(&op, dialect, &args, &selected_refs)?;
                (stmt, String::new(), 0_usize, 1_usize, false, false)
            }
            OperationKind::List => {
                let q_hash = cursor::query_hash(&request);
                let effective_max_page = self.shared.config.max_page_size.min(op.row_cap).max(1);

                let mut offset = 0_usize;
                let mut requested_limit = effective_max_page;
                let mut truncated_by_cap = false;

                if let Some(page) = &request.page {
                    if !page.cursor.is_empty() {
                        offset = cursor::decode(&op.name, &q_hash, &scope.tenant_id, &page.cursor)?;
                    } else if page.offset > 0 {
                        offset = usize::try_from(page.offset).unwrap_or(usize::MAX);
                    }
                    if page.limit > 0 {
                        if page.limit > op.row_cap {
                            truncated_by_cap = true;
                        }
                        requested_limit = page.limit.min(effective_max_page);
                    }
                }

                let order: Vec<(String, bool)> = request
                    .order_by
                    .iter()
                    .map(|o| (o.path.clone(), o.descending))
                    .collect();
                let fetch_limit = requested_limit.saturating_add(1);
                let stmt = build_list_with_args(
                    &op,
                    dialect,
                    &selected_refs,
                    request.arguments.as_ref(),
                    request.predicate.as_ref(),
                    &order,
                    fetch_limit,
                    u64::try_from(offset).unwrap_or(u64::MAX),
                )?;
                (
                    stmt,
                    q_hash,
                    offset,
                    requested_limit as usize,
                    truncated_by_cap,
                    true,
                )
            }
        };

        let shared = Arc::clone(&self.shared);
        let row_cap = requested_limit.saturating_add(1);
        let max_response_bytes = self.shared.config.max_response_bytes;

        struct SoloState<D> {
            shared: Arc<SqlShared<D>>,
            op: OperationConfig,
            stmt: Option<BoundStatement>,
            pending_complete: Option<Complete>,
            selected_names: Vec<String>,
            scope: CallScope,
            q_hash: String,
            start_offset: usize,
            requested_limit: usize,
            row_cap: usize,
            truncated_by_row_cap: bool,
            is_list: bool,
            max_response_bytes: u64,
            done: bool,
        }

        let init = SoloState {
            shared,
            op,
            stmt: Some(stmt),
            pending_complete: None,
            selected_names,
            scope,
            q_hash,
            start_offset,
            requested_limit,
            row_cap,
            truncated_by_row_cap,
            is_list,
            max_response_bytes,
            done: false,
        };

        let inner: BoxStream<QueryResponse> = Box::pin(stream::unfold(init, |mut st| async move {
            if st.done {
                return None;
            }
            if let Some(complete) = st.pending_complete.take() {
                st.done = true;
                return Some((
                    Ok(QueryResponse {
                        chunk: Some(query_response::Chunk::Complete(complete)),
                    }),
                    st,
                ));
            }

            let stmt = st.stmt.take()?;
            let started = Instant::now();
            match st
                .shared
                .exec_bound(
                    stmt,
                    st.row_cap,
                    st.op.statement_timeout_ms,
                    st.op.simulated_latency_ms,
                    &st.scope,
                )
                .await
            {
                Err(err) => {
                    st.done = true;
                    Some((Err(err), st))
                }
                Ok(mut rows) => {
                    let mut has_more_page = false;
                    if st.is_list && rows.len() > st.requested_limit {
                        rows.truncate(st.requested_limit);
                        has_more_page = true;
                    }

                    let selected_refs: Vec<&str> =
                        st.selected_names.iter().map(String::as_str).collect();
                    let mut structs: Vec<prost_types::Struct> = rows
                        .into_iter()
                        .map(|r| row_to_struct(&st.op, r, &selected_refs))
                        .collect();

                    let mut truncated_by_bytes = false;
                    if st.max_response_bytes > 0 {
                        let mut acc_bytes = 0_u64;
                        let mut keep_count = 0_usize;
                        for row in &structs {
                            let row_bytes =
                                u64::try_from(encoded_prost_struct_len(row)).unwrap_or(u64::MAX);
                            if acc_bytes.saturating_add(row_bytes) > st.max_response_bytes {
                                truncated_by_bytes = true;
                                break;
                            }
                            acc_bytes = acc_bytes.saturating_add(row_bytes);
                            keep_count += 1;
                        }
                        if truncated_by_bytes {
                            structs.truncate(keep_count);
                        }
                    }

                    let emitted = structs.len();
                    let emitted_u64 = u64::try_from(emitted).unwrap_or(u64::MAX);
                    let next_cursor =
                        if st.is_list && (has_more_page || (truncated_by_bytes && emitted > 0)) {
                            match cursor::encode(
                                &st.op.name,
                                &st.q_hash,
                                &st.scope.tenant_id,
                                st.start_offset.saturating_add(emitted),
                            ) {
                                Ok(tok) => tok,
                                Err(err) => {
                                    st.done = true;
                                    return Some((Err(err), st));
                                }
                            }
                        } else {
                            String::new()
                        };

                    let latency_us =
                        u32::try_from(started.elapsed().as_micros()).unwrap_or(u32::MAX);
                    st.pending_complete = Some(Complete {
                        next_cursor,
                        truncated: st.truncated_by_row_cap || truncated_by_bytes,
                        rows_total: emitted_u64,
                        upstream_calls: 1,
                        latency_us,
                    });

                    Some((
                        Ok(QueryResponse {
                            chunk: Some(query_response::Chunk::Rows(RowSetChunk {
                                foreach_index: 0,
                                rows: structs,
                                last_for_index: true,
                                error: None,
                            })),
                        }),
                        st,
                    ))
                }
            }
        }));

        Ok(Box::pin(GuardedQueryStream {
            inner,
            _permit: permit,
        }))
    }

    async fn sketch(&self, request: SketchRequest, scope: CallScope) -> NgcResult<SketchResponse> {
        check_scope(&scope)?;
        let op = self.op(&request.operation)?.clone();
        if request.field_path.is_empty() {
            return Err(invalid("field_path must not be empty"));
        }
        if !op.column_allow_list().contains(request.field_path.as_str()) {
            return Err(invalid(format!(
                "unknown field_path '{}'",
                request.field_path
            )));
        }
        let sample = if request.sample_size == 0 {
            sketch::MAX_SAMPLE
        } else {
            request.sample_size.min(sketch::MAX_SAMPLE)
        };
        let k = if request.bottom_k == 0 {
            sketch::DEFAULT_K
        } else {
            request.bottom_k
        };
        let _permit = self.acquire_permit()?;
        let fetch_cap = sample.saturating_add(1);
        let stmt = build_sketch_sample(
            &op,
            self.shared.config.dialect,
            &request.field_path,
            request.arguments.as_ref(),
            fetch_cap,
        )?;
        let rows = self
            .shared
            .exec_bound(
                stmt,
                fetch_cap as usize,
                op.statement_timeout_ms,
                op.simulated_latency_ms,
                &scope,
            )
            .await?;
        sketch::compute(&rows, &request.field_path, &request.salt, k, sample)
    }

    async fn health(&self, _request: HealthRequest, scope: CallScope) -> NgcResult<HealthResponse> {
        check_scope(&scope)?;
        let _permit = self.acquire_permit()?;
        let timeout_ms = self
            .shared
            .config
            .operations
            .iter()
            .map(|o| o.statement_timeout_ms)
            .min()
            .unwrap_or(5_000);
        let stmt = BoundStatement {
            sql: "SELECT 1 AS ok".to_owned(),
            params: Vec::new(),
        };
        self.shared
            .exec_bound(stmt, 1, timeout_ms, 0, &scope)
            .await?;
        Ok(HealthResponse {
            status: ServingStatus::Serving as i32,
            version: NGC_WIRE_VERSION.to_owned(),
            details: None,
        })
    }
}
