//! Reference connector with switchable flaws: the kit's own test subject and a worked example of a connector that passes it.
//!
//! [`ReferenceConnector::new`]`(`[`Flaw::None`]`)` is a small in-memory connector built on the SDK's `MockConnector`: a keyed
//! `customers` read with `foreach` batching, a paged `customers_list`, an `orders` effect probe, a resumable `blobs` answer
//! larger than the response cap, a never-ending `slow` operation guarded by a connector-owned gate, and an idempotent,
//! dry-run-capable `create_order` mutation. Every other [`Flaw`] breaks exactly one requirement so that the kit can be shown to
//! catch it ("a deliberately broken mock fails each check", `WP-M1-04`).
//!
//! The matching profile is `conformance/ngc/sdk-reference.kit.toml`.

use std::collections::{BTreeMap, BTreeSet};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Context, Poll};

use futures_util::Stream;
use futures_util::stream;
use gap_core::error::{NgcErrorCode, NgcErrorInfo};
use gap_ngc_sdk::connector::{BoxStream, CallScope, NgcResult};
use gap_ngc_sdk::core::{ServerConfig, ServerCore};
use gap_ngc_sdk::error::{prost_struct_to_json_map, prost_value_to_json};
use gap_ngc_sdk::foreach::{ForeachConfig, foreach};
use gap_ngc_sdk::grpc::GrpcClient;
use gap_ngc_sdk::mock::MockConnector;
use gap_ngc_sdk::testing::{InMemoryGrpcHandle, in_memory_grpc};
use gap_proto::ngc::v1::{
    Capabilities, Complete, ConnectorDescription, DescribeResponse, HealthResponse, Limits,
    MutateRequest, MutateResponse, MutationOutcome, OpKind, OperationDescriptor, PaginationKind,
    Protocol, QueryRequest, QueryResponse, RowSetChunk, ServingStatus, query_response,
};
use prost::Message;
use prost_types::{Struct, Value, value::Kind};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use super::outcome::id;

/// Customers `1..=CUSTOMER_COUNT`.
const CUSTOMER_COUNT: i64 = 12;
/// `limits.max_batch_size` of the reference connector.
pub const MAX_BATCH_SIZE: u32 = 4;
/// `limits.max_in_flight`; also the size of the connector-owned gate of the `slow` operation.
pub const MAX_IN_FLIGHT: u32 = 8;
/// `limits.max_response_bytes`.
pub const MAX_RESPONSE_BYTES: u64 = 8_192;
/// Rows of the `blobs` answer (more than fit in [`MAX_RESPONSE_BYTES`]).
pub const BLOB_ROWS: i64 = 40;
/// Size of one `blobs` payload.
const BLOB_PAYLOAD_CHARS: usize = 400;
/// `PredicateOp::P_EQ`, the only operator the reference connector advertises.
const PREDICATE_EQ: i32 = 1;

/// One deliberate defect (or none). Each variant breaks one requirement of `spec/03-ngc.md`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flaw {
    /// A correct connector.
    None,
    /// `schema_sdl` does not parse (`NGC-012`).
    DescribeBadSdl,
    /// An operation schema has a remote `$ref` (`NGC-013`).
    DescribeRemoteRef,
    /// `spec_hash` changes on every call (`NGC-010`).
    DescribeNondeterministic,
    /// `not_modified` for any hash, even one never issued (`NGC-010`).
    DescribeIgnoresKnownHash,
    /// `Health` reports `NOT_SERVING` (`NGC-070`).
    HealthNotServing,
    /// The first index never receives its closing chunk (`NGC-020`).
    ForeachMissingClose,
    /// Rows of an index depend on which other indices share the batch (`NGC-020`).
    ForeachDependent,
    /// One failing key fails every index of its batch (`NGC-020`).
    ForeachLeaksFailure,
    /// One upstream call per key (`NGC-026`, `INV-18`).
    ForeachNPlusOne,
    /// `order_by` is ignored (`NGC-021`).
    OrderIgnored,
    /// Row order rotates from call to call (`NGC-021`).
    OrderNondeterministic,
    /// Work started for a call whose deadline passed keeps its slot forever (`NGC-025`).
    LeaksOnDeadline,
    /// Work started for a call the client cancelled keeps its slot forever (`NGC-025`).
    LeaksOnCancel,
    /// The effect is applied again on a repeat of the idempotency key (`NGC-030`).
    IdempotencyBroken,
    /// The repeat returns the original result but is not flagged `was_replayed` (`NGC-030`).
    ReplayNotFlagged,
    /// Idempotency keys are shared across tenants (`NGC-030`).
    IdempotencyIgnoresTenant,
    /// A dry run applies the effect (`NGC-031`).
    DryRunMutates,
    /// A dry run reports `APPLIED` (`NGC-031`).
    DryRunWrongOutcome,
    /// Each page after the first repeats the last row of the previous one (`NGC-021`).
    PageOverlap,
    /// The page size changes the order of the rows (`NGC-021`).
    PageUnstable,
    /// A cursor works with other arguments and for other tenants (`NGC-022`).
    CursorUnbound,
    /// A cursor nobody issued restarts the traversal (`NGC-022`).
    CursorGarbageAccepted,
    /// An unadvertised predicate operator is silently ignored (`NGC-023`).
    PredicateIgnored,
    /// Error messages echo the caller's `authorization` (`NGC-061`, `INV-15`).
    ErrorLeaksSecret,
    /// Invalid arguments fail `INTERNAL` (`NGC-060`).
    WrongErrorCode,
    /// The response cap is ignored (`NGC-027`).
    NoTruncation,
    /// The answer is cut at the cap but not flagged `truncated` (`NGC-027`).
    TruncatedUnflagged,
}

impl Flaw {
    /// Every flaw except [`Flaw::None`].
    pub const ALL: [Self; 27] = [
        Self::DescribeBadSdl,
        Self::DescribeRemoteRef,
        Self::DescribeNondeterministic,
        Self::DescribeIgnoresKnownHash,
        Self::HealthNotServing,
        Self::ForeachMissingClose,
        Self::ForeachDependent,
        Self::ForeachLeaksFailure,
        Self::ForeachNPlusOne,
        Self::OrderIgnored,
        Self::OrderNondeterministic,
        Self::LeaksOnDeadline,
        Self::LeaksOnCancel,
        Self::IdempotencyBroken,
        Self::ReplayNotFlagged,
        Self::IdempotencyIgnoresTenant,
        Self::DryRunMutates,
        Self::DryRunWrongOutcome,
        Self::PageOverlap,
        Self::PageUnstable,
        Self::CursorUnbound,
        Self::CursorGarbageAccepted,
        Self::PredicateIgnored,
        Self::ErrorLeaksSecret,
        Self::WrongErrorCode,
        Self::NoTruncation,
        Self::TruncatedUnflagged,
    ];

    /// The checks that must fail, and no others, when this flaw is present.
    ///
    /// `LeaksOnDeadline` also fails `cancel`: a slot that is never given back leaves the connector without capacity for the
    /// next slow call, which is exactly what `cancel` tries first.
    #[must_use]
    pub fn targets(self) -> &'static [&'static str] {
        match self {
            Self::None => &[],
            Self::DescribeBadSdl | Self::DescribeRemoteRef => &[id::DESCRIBE_VALID],
            Self::DescribeNondeterministic | Self::DescribeIgnoresKnownHash => {
                &[id::DESCRIBE_STABLE]
            }
            Self::HealthNotServing => &[id::HEALTH],
            Self::ForeachMissingClose | Self::ForeachDependent | Self::ForeachLeaksFailure => {
                &[id::FOREACH_CORRECTNESS]
            }
            Self::ForeachNPlusOne => &[id::FOREACH_N_PLUS_ONE],
            Self::OrderIgnored => &[id::QUERY_ORDER],
            Self::OrderNondeterministic => &[id::QUERY_ORDER, id::PAGE_STABILITY],
            Self::LeaksOnDeadline => &[id::DEADLINE, id::CANCEL],
            Self::LeaksOnCancel => &[id::CANCEL],
            Self::IdempotencyBroken | Self::ReplayNotFlagged | Self::IdempotencyIgnoresTenant => {
                &[id::MUTATE_IDEMPOTENCY]
            }
            Self::DryRunMutates | Self::DryRunWrongOutcome => &[id::MUTATE_DRY_RUN],
            Self::PageOverlap
            | Self::PageUnstable
            | Self::CursorUnbound
            | Self::CursorGarbageAccepted => &[id::PAGE_STABILITY],
            Self::PredicateIgnored | Self::ErrorLeaksSecret | Self::WrongErrorCode => {
                &[id::ERRORS_MAPPING]
            }
            Self::NoTruncation | Self::TruncatedUnflagged => &[id::LIMITS_RESPONSE_SIZE],
        }
    }
}

// ---------------------------------------------------------------------------------------------------------------------
// data

fn num(n: f64) -> Value {
    Value {
        kind: Some(Kind::NumberValue(n)),
    }
}

fn text(s: impl Into<String>) -> Value {
    Value {
        kind: Some(Kind::StringValue(s.into())),
    }
}

fn object(pairs: Vec<(&str, Value)>) -> Struct {
    Struct {
        fields: pairs
            .into_iter()
            .map(|(k, v)| (k.to_owned(), v))
            .collect::<BTreeMap<_, _>>(),
    }
}

#[allow(clippy::cast_precision_loss)]
fn customer(id: i64) -> Struct {
    object(vec![
        ("id", num(id as f64)),
        ("name", text(format!("customer-{id}"))),
        ("region", text(if id % 2 == 0 { "eu" } else { "us" })),
        ("score", num(((id * 37) % 11) as f64)),
    ])
}

#[allow(clippy::cast_precision_loss)]
fn blob(n: i64) -> Struct {
    object(vec![
        ("n", num(n as f64)),
        ("payload", text("x".repeat(BLOB_PAYLOAD_CHARS))),
    ])
}

fn number_field(row: &Struct, name: &str) -> Option<f64> {
    match row.fields.get(name).and_then(|v| v.kind.as_ref()) {
        Some(Kind::NumberValue(n)) => Some(*n),
        _ => None,
    }
}

fn string_field(row: &Struct, name: &str) -> Option<String> {
    match row.fields.get(name).and_then(|v| v.kind.as_ref()) {
        Some(Kind::StringValue(s)) => Some(s.clone()),
        _ => None,
    }
}

// ---------------------------------------------------------------------------------------------------------------------
// state

struct State {
    orders: Mutex<Vec<Struct>>,
    /// `(scope tenant, operation, idempotency key)` -> first response.
    idempotency: Mutex<BTreeMap<(String, String, String), MutateResponse>>,
    /// Connector-owned gate of the `slow` operation (the SDK's own cap is separate).
    slow_gate: Arc<Semaphore>,
    describe_calls: AtomicU64,
    list_calls: AtomicU64,
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The `slow` operation: open until dropped. A leaky connector keeps the gate slot when it is dropped.
struct SlowStream {
    permit: Option<OwnedSemaphorePermit>,
    flaw: Flaw,
    deadline: tokio::time::Instant,
}

impl Stream for SlowStream {
    type Item = NgcResult<QueryResponse>;

    fn poll_next(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Poll::Pending
    }
}

impl Drop for SlowStream {
    fn drop(&mut self) {
        let Some(permit) = self.permit.take() else {
            return;
        };
        let past_deadline = tokio::time::Instant::now() >= self.deadline;
        let leak = match self.flaw {
            Flaw::LeaksOnDeadline => past_deadline,
            Flaw::LeaksOnCancel => !past_deadline,
            _ => false,
        };
        if leak {
            permit.forget();
        }
    }
}

// ---------------------------------------------------------------------------------------------------------------------
// description

fn description(flaw: Flaw, minimal: bool, describe_call: u64) -> ConnectorDescription {
    let sdl = if flaw == Flaw::DescribeBadSdl {
        "type Customer { id: Int! name: String!".to_owned()
    } else {
        concat!(
            "type Customer { id: Int! name: String! region: String! score: Int! }\n",
            "type Order { n: Int! item: String! }\n",
            "type Blob { n: Int! payload: String! }\n",
            "type Query {\n",
            "  customers(id: Int): [Customer!]!\n",
            "  customersList(region: String): [Customer!]!\n",
            "  orders: [Order!]!\n",
            "  blobs: [Blob!]!\n",
            "}\n",
            "type Mutation { createOrder(item: String!): Order! }\n",
        )
        .to_owned()
    };
    let input_schema = if flaw == Flaw::DescribeRemoteRef {
        object(vec![(
            "$ref",
            text("https://example.invalid/customer.json"),
        )])
    } else {
        object(vec![
            ("type", text("object")),
            (
                "properties",
                Value {
                    kind: Some(Kind::StructValue(object(vec![(
                        "id",
                        Value {
                            kind: Some(Kind::StructValue(object(vec![("type", text("integer"))]))),
                        },
                    )]))),
                },
            ),
        ])
    };
    let op = |name: &str, kind: OpKind| OperationDescriptor {
        name: name.to_owned(),
        kind: kind as i32,
        description: format!("reference operation {name}"),
        input_schema: Some(input_schema.clone()),
        ..OperationDescriptor::default()
    };
    let mut mutation = op("create_order", OpKind::Mutation);
    mutation.supports_dry_run = !minimal;
    mutation.idempotent_hint = !minimal;
    ConnectorDescription {
        connector_id: "gap-ngc-reference".to_owned(),
        connector_version: "0.1.0".to_owned(),
        ngc_version: "gap/0.1".to_owned(),
        protocol: Protocol::Custom as i32,
        spec_hash: if flaw == Flaw::DescribeNondeterministic {
            format!("ref-{describe_call}")
        } else {
            "ref-0".to_owned()
        },
        capabilities: Some(Capabilities {
            foreach_batch: !minimal,
            predicate_ops: vec![PREDICATE_EQ],
            order_by: !minimal,
            pagination: if minimal {
                PaginationKind::PaginationNone as i32
            } else {
                PaginationKind::PaginationCursor as i32
            },
            dry_run: !minimal,
            idempotency: !minimal,
            ..Capabilities::default()
        }),
        limits: Some(Limits {
            max_batch_size: if minimal { 0 } else { MAX_BATCH_SIZE },
            max_in_flight: MAX_IN_FLIGHT,
            max_page_size: 100,
            default_timeout_ms: 60_000,
            max_response_bytes: if minimal { 0 } else { MAX_RESPONSE_BYTES },
        }),
        schema_sdl: sdl,
        operations: vec![
            op("customers", OpKind::Query),
            op("customers_list", OpKind::Query),
            op("orders", OpKind::Query),
            op("blobs", OpKind::Query),
            op("slow", OpKind::Query),
            mutation,
        ],
        ..ConnectorDescription::default()
    }
}

// ---------------------------------------------------------------------------------------------------------------------
// query

fn invalid(flaw: Flaw, scope: &CallScope, message: &str) -> NgcErrorInfo {
    let code = if flaw == Flaw::WrongErrorCode {
        NgcErrorCode::Internal
    } else {
        NgcErrorCode::InvalidArgument
    };
    if flaw == Flaw::ErrorLeaksSecret {
        let auth = scope
            .metadata
            .get("authorization")
            .cloned()
            .unwrap_or_default();
        return NgcErrorInfo::new(code, format!("{message} (credential presented: {auth})"));
    }
    NgcErrorInfo::new(code, message)
}

fn rows_stream(rows: Vec<Struct>, complete: Complete) -> BoxStream<QueryResponse> {
    let items = vec![
        Ok(QueryResponse {
            chunk: Some(query_response::Chunk::Rows(RowSetChunk {
                foreach_index: 0,
                rows,
                last_for_index: true,
                error: None,
            })),
        }),
        Ok(QueryResponse {
            chunk: Some(query_response::Chunk::Complete(complete)),
        }),
    ];
    Box::pin(stream::iter(items))
}

/// Applies the single `P_EQ` predicate the connector understands.
fn apply_predicate(
    flaw: Flaw,
    scope: &CallScope,
    req: &QueryRequest,
    rows: &mut Vec<Struct>,
) -> NgcResult<()> {
    let Some(expr) = &req.predicate else {
        return Ok(());
    };
    let Some(gap_proto::ngc::v1::expr::Node::Cmp(cmp)) = &expr.node else {
        return Err(invalid(
            flaw,
            scope,
            "only simple comparisons are supported",
        ));
    };
    if cmp.op != PREDICATE_EQ {
        if flaw == Flaw::PredicateIgnored {
            return Ok(());
        }
        return Err(invalid(flaw, scope, "predicate operator is not supported"));
    }
    let want = cmp.value.as_ref().map(prost_value_to_json);
    rows.retain(|row| {
        let have = row.fields.get(&cmp.path).map(prost_value_to_json);
        match (&have, &want) {
            (Some(a), Some(b)) => match (a.as_f64(), b.as_f64()) {
                (Some(x), Some(y)) => (x - y).abs() < f64::EPSILON,
                _ => a == b,
            },
            _ => false,
        }
    });
    Ok(())
}

fn lookup_customers(values: &Struct) -> NgcResult<Vec<Struct>> {
    let Some(Kind::NumberValue(wanted)) = values.fields.get("id").and_then(|v| v.kind.as_ref())
    else {
        return Err(NgcErrorInfo::new(
            NgcErrorCode::InvalidArgument,
            "id must be a number",
        ));
    };
    Ok((1..=CUSTOMER_COUNT)
        .map(customer)
        .filter(|row| number_field(row, "id").is_some_and(|id| (id - wanted).abs() < f64::EPSILON))
        .collect())
}

fn customers_op(
    flaw: Flaw,
    scope: &CallScope,
    req: QueryRequest,
) -> NgcResult<BoxStream<QueryResponse>> {
    if !req.foreach.is_empty() {
        let sets = req.foreach.clone();
        let batch_size = if flaw == Flaw::ForeachNPlusOne {
            1
        } else {
            usize::try_from(MAX_BATCH_SIZE).unwrap_or(1)
        };
        let run = foreach(
            sets,
            ForeachConfig::new(batch_size, 2),
            scope,
            move |batch: Vec<Struct>| async move {
                if flaw == Flaw::ForeachLeaksFailure
                    && batch.iter().any(|v| lookup_customers(v).is_err())
                {
                    return Err(NgcErrorInfo::new(
                        NgcErrorCode::InvalidArgument,
                        "a key in the batch is invalid",
                    ));
                }
                let together = batch.len() >= 2;
                Ok(batch
                    .iter()
                    .map(|values| {
                        lookup_customers(values).map(|rows| {
                            if flaw == Flaw::ForeachDependent && together {
                                rows.into_iter()
                                    .map(|mut row| {
                                        let score = number_field(&row, "score").unwrap_or(0.0);
                                        row.fields.insert("score".to_owned(), num(score + 1.0));
                                        row
                                    })
                                    .collect()
                            } else {
                                rows
                            }
                        })
                    })
                    .collect())
            },
        )?;
        let first_index = req.foreach.first().map(|s| s.index);
        let inner = run.into_query_stream();
        return Ok(Box::pin(futures_util::StreamExt::map(inner, move |item| {
            item.map(|mut resp| {
                if flaw == Flaw::ForeachMissingClose
                    && let Some(query_response::Chunk::Rows(chunk)) = &mut resp.chunk
                    && Some(chunk.foreach_index) == first_index
                {
                    chunk.last_for_index = false;
                }
                resp
            })
        })));
    }

    // Single execution.
    let mut rows: Vec<Struct> = match req.arguments.as_ref().and_then(|a| a.fields.get("id")) {
        None => (1..=CUSTOMER_COUNT).map(customer).collect(),
        Some(Value {
            kind: Some(Kind::NumberValue(_)),
        }) => {
            let values = req.arguments.clone().unwrap_or_default();
            lookup_customers(&values)?
        }
        Some(_) => return Err(invalid(flaw, scope, "id must be an integer")),
    };
    apply_predicate(flaw, scope, &req, &mut rows)?;
    let total = rows.len() as u64;
    Ok(rows_stream(
        rows,
        Complete {
            rows_total: total,
            upstream_calls: 1,
            ..Complete::default()
        },
    ))
}

fn args_digest(operation: &str, req: &QueryRequest, tenant: &str, bound: bool) -> u64 {
    let mut hasher = DefaultHasher::new();
    operation.hash(&mut hasher);
    if bound {
        if let Some(args) = &req.arguments {
            serde_json::Value::Object(prost_struct_to_json_map(args))
                .to_string()
                .hash(&mut hasher);
        }
        tenant.hash(&mut hasher);
    }
    hasher.finish()
}

fn list_op(
    flaw: Flaw,
    state: &State,
    scope: &CallScope,
    req: QueryRequest,
) -> NgcResult<BoxStream<QueryResponse>> {
    let call_no = state.list_calls.fetch_add(1, Ordering::Relaxed);
    let region = req
        .arguments
        .as_ref()
        .and_then(|a| string_field(a, "region"));
    let mut rows: Vec<Struct> = (1..=CUSTOMER_COUNT)
        .map(customer)
        .filter(|row| {
            region
                .as_ref()
                .is_none_or(|r| string_field(row, "region").as_ref() == Some(r))
        })
        .collect();
    apply_predicate(flaw, scope, &req, &mut rows)?;

    // Default order: ascending id. `order_by` is honored unless the flaw ignores it.
    let page = req.page.clone();
    let limit = page.as_ref().map_or(0, |p| p.limit);
    let mut descending = flaw == Flaw::OrderIgnored;
    if flaw == Flaw::PageUnstable && limit % 2 == 1 {
        descending = true;
    }
    if let Some(order) = req.order_by.first() {
        if order.path != "id" {
            return Err(NgcErrorInfo::new(
                NgcErrorCode::InvalidArgument,
                "order_by supports only `id`",
            ));
        }
        if flaw != Flaw::OrderIgnored {
            descending = order.descending;
        }
    }
    rows.sort_by(|a, b| {
        number_field(a, "id")
            .partial_cmp(&number_field(b, "id"))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    if descending {
        rows.reverse();
    }
    if flaw == Flaw::OrderNondeterministic && limit > 0 && !rows.is_empty() {
        let shift = usize::try_from(call_no).unwrap_or(0) % rows.len();
        rows.rotate_left(shift);
    }

    let tenant = req.ctx.as_ref().map_or("", |c| c.tenant_id.as_str());
    let bound = flaw != Flaw::CursorUnbound;
    let digest = args_digest("customers_list", &req, tenant, bound);
    let mut offset = 0_usize;
    if let Some(p) = &page
        && !p.cursor.is_empty()
    {
        let parsed = p.cursor.strip_prefix("v1:").and_then(|rest| {
            let (hash, off) = rest.split_once(':')?;
            Some((
                u64::from_str_radix(hash, 16).ok()?,
                off.parse::<usize>().ok()?,
            ))
        });
        match parsed {
            Some((hash, off)) if hash == digest => offset = off,
            Some(_) => {
                return Err(NgcErrorInfo::new(
                    NgcErrorCode::InvalidArgument,
                    "cursor does not belong to this request",
                ));
            }
            None if flaw == Flaw::CursorGarbageAccepted => {}
            None => {
                return Err(NgcErrorInfo::new(
                    NgcErrorCode::InvalidArgument,
                    "malformed cursor",
                ));
            }
        }
    }
    let total = rows.len();
    let mut start = offset.min(total);
    if flaw == Flaw::PageOverlap && start > 0 {
        start -= 1;
    }
    let end = if limit == 0 {
        total
    } else {
        (offset.min(total) + usize::try_from(limit).unwrap_or(usize::MAX)).min(total)
    };
    let next_cursor = if limit > 0 && end < total {
        format!("v1:{digest:016x}:{end}")
    } else {
        String::new()
    };
    let page_rows: Vec<Struct> = rows[start..end].to_vec();
    let count = page_rows.len() as u64;
    Ok(rows_stream(
        page_rows,
        Complete {
            next_cursor,
            rows_total: count,
            upstream_calls: 1,
            ..Complete::default()
        },
    ))
}

fn blobs_op(flaw: Flaw, req: &QueryRequest) -> BoxStream<QueryResponse> {
    let offset = req
        .page
        .as_ref()
        .and_then(|p| p.cursor.strip_prefix("blob:"))
        .and_then(|n| n.parse::<i64>().ok())
        .unwrap_or(0);
    let mut rows = Vec::new();
    let mut bytes = 0_u64;
    let mut next = offset;
    while next < BLOB_ROWS {
        let row = blob(next);
        let size = row.encoded_len() as u64;
        if flaw != Flaw::NoTruncation && bytes + size > MAX_RESPONSE_BYTES {
            break;
        }
        bytes += size;
        rows.push(row);
        next += 1;
    }
    let cut = next < BLOB_ROWS;
    let count = rows.len() as u64;
    rows_stream(
        rows,
        Complete {
            next_cursor: if cut {
                format!("blob:{next}")
            } else {
                String::new()
            },
            truncated: cut && flaw != Flaw::TruncatedUnflagged,
            rows_total: count,
            upstream_calls: 1,
            ..Complete::default()
        },
    )
}

fn slow_op(flaw: Flaw, state: &State, scope: &CallScope) -> NgcResult<BoxStream<QueryResponse>> {
    match Arc::clone(&state.slow_gate).try_acquire_owned() {
        Ok(permit) => Ok(Box::pin(SlowStream {
            permit: Some(permit),
            flaw,
            deadline: scope.deadline,
        })),
        Err(_) => Err(NgcErrorInfo::new(
            NgcErrorCode::ResourceExhausted,
            "the slow operation has no free slot",
        )
        .with_retry(true, 50)),
    }
}

// ---------------------------------------------------------------------------------------------------------------------
// mutate

fn create_order(
    flaw: Flaw,
    minimal: bool,
    state: &State,
    req: &MutateRequest,
) -> NgcResult<MutateResponse> {
    if req.operation != "create_order" {
        return Err(NgcErrorInfo::new(
            NgcErrorCode::NotFound,
            "unknown mutation",
        ));
    }
    let item = req
        .arguments
        .as_ref()
        .and_then(|a| string_field(a, "item"))
        .unwrap_or_else(|| "unspecified".to_owned());
    let ctx = req.ctx.clone().unwrap_or_default();

    let apply = |item: &str| -> Struct {
        let mut orders = lock(&state.orders);
        #[allow(clippy::cast_precision_loss)]
        let n = (orders.len() + 1) as f64;
        let order = object(vec![("n", num(n)), ("item", text(item))]);
        orders.push(order.clone());
        order
    };

    if req.dry_run {
        if minimal {
            return Err(NgcErrorInfo::new(
                NgcErrorCode::Unimplemented,
                "dry_run is not supported by this connector",
            ));
        }
        if flaw == Flaw::DryRunMutates {
            apply(&item);
        }
        return Ok(MutateResponse {
            outcome: if flaw == Flaw::DryRunWrongOutcome {
                MutationOutcome::OutcomeApplied as i32
            } else {
                MutationOutcome::OutcomeDryRun as i32
            },
            dry_run_report: Some(object(vec![("would_create", text(&item))])),
            ..MutateResponse::default()
        });
    }

    let key = ctx.idempotency_key.clone();
    let scope_tenant = if flaw == Flaw::IdempotencyIgnoresTenant {
        String::new()
    } else {
        ctx.tenant_id.clone()
    };
    let slot = (scope_tenant, req.operation.clone(), key.clone());
    if !key.is_empty() {
        let previous = lock(&state.idempotency).get(&slot).cloned();
        if let Some(mut original) = previous {
            if flaw == Flaw::IdempotencyBroken {
                let fresh = apply(&item);
                return Ok(MutateResponse {
                    result: Some(fresh),
                    outcome: MutationOutcome::OutcomeApplied as i32,
                    ..MutateResponse::default()
                });
            }
            original.was_replayed = flaw != Flaw::ReplayNotFlagged;
            return Ok(original);
        }
    }
    let order = apply(&item);
    let response = MutateResponse {
        result: Some(order),
        outcome: MutationOutcome::OutcomeApplied as i32,
        ..MutateResponse::default()
    };
    if !key.is_empty() {
        lock(&state.idempotency).insert(slot, response.clone());
    }
    Ok(response)
}

// ---------------------------------------------------------------------------------------------------------------------
// public API

/// The reference connector.
pub struct ReferenceConnector {
    flaw: Flaw,
    minimal: bool,
    state: Arc<State>,
}

impl ReferenceConnector {
    /// A fresh connector with `flaw` (or none).
    #[must_use]
    pub fn new(flaw: Flaw) -> Self {
        Self {
            flaw,
            minimal: false,
            state: Arc::new(State {
                orders: Mutex::new(Vec::new()),
                idempotency: Mutex::new(BTreeMap::new()),
                slow_gate: Arc::new(Semaphore::new(MAX_IN_FLIGHT as usize)),
                describe_calls: AtomicU64::new(0),
                list_calls: AtomicU64::new(0),
            }),
        }
    }

    /// A connector that advertises none of foreach, `order_by`, pagination, idempotency, dry run or a response cap, so the
    /// kit has to skip those checks as not applicable (capability gating, `NGC-091`).
    #[must_use]
    pub fn minimal(mut self) -> Self {
        self.minimal = true;
        self
    }

    /// Builds the SDK mock that serves this connector.
    #[must_use]
    pub fn into_mock(self) -> MockConnector {
        let Self {
            flaw,
            minimal,
            state,
        } = self;
        let (s_describe, s_query, s_mutate) =
            (Arc::clone(&state), Arc::clone(&state), Arc::clone(&state));
        MockConnector::new()
            .on_describe(move |req, _scope| {
                let call_no = s_describe.describe_calls.fetch_add(1, Ordering::Relaxed);
                if flaw == Flaw::DescribeIgnoresKnownHash && !req.known_spec_hash.is_empty() {
                    return Ok(DescribeResponse {
                        not_modified: true,
                        description: None,
                    });
                }
                let desc = description(flaw, minimal, call_no);
                if !req.known_spec_hash.is_empty() && req.known_spec_hash == desc.spec_hash {
                    return Ok(DescribeResponse {
                        not_modified: true,
                        description: None,
                    });
                }
                Ok(DescribeResponse {
                    not_modified: false,
                    description: Some(desc),
                })
            })
            .on_health(move |_req, _scope| {
                Ok(HealthResponse {
                    status: if flaw == Flaw::HealthNotServing {
                        ServingStatus::NotServing as i32
                    } else {
                        ServingStatus::Serving as i32
                    },
                    version: "0.1.0".to_owned(),
                    ..HealthResponse::default()
                })
            })
            .on_query(move |req, scope| match req.operation.as_str() {
                "customers" => customers_op(flaw, &scope, req),
                "customers_list" => list_op(flaw, &s_query, &scope, req),
                "orders" => {
                    let rows = lock(&s_query.orders).clone();
                    let total = rows.len() as u64;
                    Ok(rows_stream(
                        rows,
                        Complete {
                            rows_total: total,
                            upstream_calls: 1,
                            ..Complete::default()
                        },
                    ))
                }
                "blobs" => Ok(blobs_op(flaw, &req)),
                "slow" => slow_op(flaw, &s_query, &scope),
                other => Err(NgcErrorInfo::new(
                    NgcErrorCode::NotFound,
                    format!("unknown operation `{other}`"),
                )),
            })
            .on_mutate(move |req, _scope| create_order(flaw, minimal, &s_mutate, &req))
    }

    /// Serves this connector over an in-memory gRPC transport (no OS socket) and returns a client for it.
    #[must_use]
    pub fn in_memory(self) -> (GrpcClient, InMemoryGrpcHandle) {
        let config = ServerConfig::default().with_max_in_flight(MAX_IN_FLIGHT as usize);
        let core = Arc::new(ServerCore::new(self.into_mock(), config));
        in_memory_grpc(core)
    }
}

/// Names of the leaf predicate operators the connector advertises (for documentation and tests).
#[must_use]
pub fn advertised_predicate_ops() -> BTreeSet<i32> {
    BTreeSet::from([PREDICATE_EQ])
}
