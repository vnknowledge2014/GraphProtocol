//! `ExecLayer` (`tower::Service<ExecRequest>`) executing `Fetch` / `Parallel` / `Sequence` plans
//! with `RUN-030` deadline propagation, `RUN-031` cancellation, `RUN-060` incremental response-size
//! enforcement, and `GQL-050`/`GQL-051` response envelopes.

pub mod assembly;
pub mod error;

use std::collections::{BTreeMap, BTreeSet};
use std::convert::Infallible;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use apollo_compiler::Schema;
use apollo_compiler::ast::{OperationType, Type, Value as AstValue};
use apollo_compiler::schema::{ExtendedType, FieldDefinition};
use futures_util::future::join_all;
use gap_core::clock::{Clock, SystemClock};
use gap_core::error::{ErrorCode, PathSegment};
use gap_core::snapshot::{BranchLimits, Snapshot};
use gap_ngc_sdk::connector::CallScope;
use gap_ngc_sdk::error::json_map_to_prost_struct;
use gap_plan::{ArgValue, Fetch, Plan, PlanNode, SelectionField};
use gap_proto::ngc::v1::{
    CallContext, OrderBy, Page, QueryRequest, Selection as ProtoSelection,
    SelectionField as ProtoSelectionField,
};
use serde::{Deserialize, Serialize};
use tokio::sync::Semaphore;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tower::Service;

use crate::connector_layer::{BranchQueryError, ConnectorRegistry};
use assembly::{
    FetchAssemblyInput, FetchAssemblyOutcome, ResponseSizeBudget, consume_and_project_fetch,
    eval_directives_included, json_escaped_str_len, pre_stream_failed_fetch_outcome,
    too_large_error,
};
pub use assembly::{NoopResponseRedactor, ResponseRedactor};
pub use error::{GraphqlError, GraphqlErrorExtensions, map_ngc_error, sanitize_connector_message};

/// Default maximum concurrent `Fetch` nodes per request (`RUN-015`).
pub const DEFAULT_MAX_PARALLEL_FETCHES: usize = 8;

/// Default maximum serialized response `data` size in bytes (`4 MiB`, `spec/02-gap-ql.md` §8, `RUN-060`).
pub const DEFAULT_MAX_RESPONSE_BYTES: usize = 4 * 1024 * 1024;

/// Default minimum safety margin subtracted from request deadline per connector call (`50 ms`, `RUN-030`).
pub const DEFAULT_DEADLINE_MARGIN_FLOOR_MS: u64 = 50;

/// Default fractional safety margin subtracted from remaining request budget (`5%`, `RUN-030`).
pub const DEFAULT_DEADLINE_MARGIN_FRACTION: f64 = 0.05;

/// Per-request execution limits (`RUN-015`, `RUN-030`, `RUN-060`, `Brief §3.2`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ExecLimits {
    /// Maximum concurrent `Fetch` calls inside `PlanNode::Parallel` (`RUN-015`, default `8`).
    pub max_parallel_fetches: usize,
    /// Incremental cap on assembled response `data` UTF-8 JSON bytes (`RUN-060`, default `4 MiB`).
    pub max_response_bytes: usize,
    /// Minimum deadline margin reserved for response assembly (`RUN-030`, default `50 ms`).
    pub margin_floor: Duration,
    /// Fractional deadline margin reserved from remaining request time (`RUN-030`, default `0.05`).
    pub margin_fraction: f64,
}

impl Default for ExecLimits {
    fn default() -> Self {
        Self {
            max_parallel_fetches: DEFAULT_MAX_PARALLEL_FETCHES,
            max_response_bytes: DEFAULT_MAX_RESPONSE_BYTES,
            margin_floor: Duration::from_millis(DEFAULT_DEADLINE_MARGIN_FLOOR_MS),
            margin_fraction: DEFAULT_DEADLINE_MARGIN_FRACTION,
        }
    }
}

/// Typed input to [`ExecLayer`] (`RUN-001`, `INV-01`, `Brief §3.2`).
#[derive(Clone)]
pub struct ExecRequest {
    /// Pinned supergraph snapshot for this request (`INV-01`: never re-read from a handle).
    pub snapshot: Arc<Snapshot>,
    /// Compiled, variable-independent execution plan (`RUN-010`).
    pub plan: Arc<Plan>,
    /// Caller-supplied GraphQL variables bound at execution time.
    pub variables: serde_json::Map<String, serde_json::Value>,
    /// Overall monotonic request deadline (`RUN-030`).
    pub deadline: Instant,
    /// Request cancellation token (`RUN-031`, `INV-12`).
    pub cancel: CancellationToken,
    /// Execution concurrency, size, and deadline-margin limits.
    pub limits: ExecLimits,
}

impl fmt::Debug for ExecRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ExecRequest")
            .field("generation", &self.snapshot.generation)
            .field("tenant_id", &self.snapshot.tenant_id)
            .field("operation_name", &self.plan.operation_name)
            .field("variable_count", &self.variables.len())
            .field("deadline", &self.deadline)
            .field("cancelled", &self.cancel.is_cancelled())
            .field("limits", &self.limits)
            .finish()
    }
}

impl ExecRequest {
    /// Creates an [`ExecRequest`] with default [`ExecLimits`], empty `variables`, and a fresh [`CancellationToken`].
    #[must_use]
    pub fn new(snapshot: Arc<Snapshot>, plan: Arc<Plan>, deadline: Instant) -> Self {
        Self {
            snapshot,
            plan,
            variables: serde_json::Map::new(),
            deadline,
            cancel: CancellationToken::new(),
            limits: ExecLimits::default(),
        }
    }

    /// Sets `variables`.
    #[must_use]
    pub fn with_variables(mut self, variables: serde_json::Map<String, serde_json::Value>) -> Self {
        self.variables = variables;
        self
    }

    /// Sets `cancel`.
    #[must_use]
    pub fn with_cancel(mut self, cancel: CancellationToken) -> Self {
        self.cancel = cancel;
        self
    }

    /// Sets `limits`.
    #[must_use]
    pub const fn with_limits(mut self, limits: ExecLimits) -> Self {
        self.limits = limits;
        self
    }
}

/// Planned and actual execution cost reported inside [`GapExtensions`] (`GQL-050`, `spec/02-gap-ql.md` §6).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CostSummary {
    /// Static cost computed by the planner (`GQL-041`, `RUN-005`).
    pub planned: u64,
    /// Actual cost accumulated over executed fetches and returned rows.
    pub actual: u64,
}

/// GAP response metadata serialized under `extensions` (`GQL-050`, `GQL-051`, `Brief §3.2`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GapExtensions {
    /// Snapshot generation that served the request (`GQL-050`, `INV-01`).
    pub generation: u64,
    /// Planned vs. actual cost summary (`GQL-050`).
    pub cost: CostSummary,
    /// `true` iff `data` is present (`Some`) and at least one error was recorded (`GQL-051`).
    pub partial: bool,
}

/// Typed GraphQL execution result returned by [`ExecLayer`] (`RUN-001`, `GQL-050`, `GQL-051`, `Brief §3.2`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecResponse {
    /// Assembled GraphQL `data` object, or `None` (`"data": null`) when top-level non-null propagation,
    /// pre-execution variable validation, or `max_response_bytes` aborts `data`.
    pub data: Option<serde_json::Value>,
    /// Execution or validation errors in deterministic order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub errors: Vec<GraphqlError>,
    /// GAP response extensions (`generation`, `cost`, `partial`).
    pub extensions: GapExtensions,
}

fn ast_default_to_json(val: &AstValue) -> Result<serde_json::Value, GraphqlError> {
    match val {
        AstValue::Null => Ok(serde_json::Value::Null),
        AstValue::Boolean(b) => Ok(serde_json::Value::Bool(*b)),
        AstValue::String(s) => Ok(serde_json::Value::String(s.clone())),
        AstValue::Enum(name) => Ok(serde_json::Value::String(name.as_str().to_owned())),
        AstValue::Int(int_val) => {
            let raw = int_val.as_str();
            if let Ok(i) = raw.parse::<i64>() {
                Ok(serde_json::Value::Number(i.into()))
            } else if let Ok(u) = raw.parse::<u64>() {
                Ok(serde_json::Value::Number(u.into()))
            } else {
                Err(GraphqlError::new(
                    ErrorCode::ValidationError,
                    format!("default integer `{raw}` overflows 64-bit representation"),
                ))
            }
        }
        AstValue::Float(float_val) => {
            let f = float_val.try_to_f64().map_err(|_| {
                GraphqlError::new(
                    ErrorCode::ValidationError,
                    format!("default float `{}` is invalid", float_val.as_str()),
                )
            })?;
            serde_json::Number::from_f64(f)
                .map(serde_json::Value::Number)
                .ok_or_else(|| {
                    GraphqlError::new(
                        ErrorCode::ValidationError,
                        "default float is not finite JSON",
                    )
                })
        }
        AstValue::List(items) => {
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                out.push(ast_default_to_json(item)?);
            }
            Ok(serde_json::Value::Array(out))
        }
        AstValue::Object(entries) => {
            let mut map = serde_json::Map::with_capacity(entries.len());
            for (k, v) in entries {
                map.insert(k.as_str().to_owned(), ast_default_to_json(v)?);
            }
            Ok(serde_json::Value::Object(map))
        }
        AstValue::Variable(var) => Err(GraphqlError::new(
            ErrorCode::ValidationError,
            format!("unexpected variable `${var}` inside schema default value"),
        )),
    }
}

fn validate_bound_json_against_type(
    val: &serde_json::Value,
    expected_ty: &Type,
    schema: &Schema,
    arg_label: &str,
) -> Result<(), GraphqlError> {
    if val.is_null() {
        if expected_ty.is_non_null() {
            return Err(GraphqlError::new(
                ErrorCode::ValidationError,
                format!("argument `{arg_label}` is non-null but received null"),
            ));
        }
        return Ok(());
    }

    match expected_ty {
        Type::List(elem_ty) | Type::NonNullList(elem_ty) => {
            let Some(arr) = val.as_array() else {
                return Err(GraphqlError::new(
                    ErrorCode::ValidationError,
                    format!("argument `{arg_label}` expected a list value"),
                ));
            };
            for item in arr {
                validate_bound_json_against_type(item, elem_ty, schema, arg_label)?;
            }
            Ok(())
        }
        Type::Named(tname) | Type::NonNullNamed(tname) => {
            if let Some(ExtendedType::InputObject(in_obj)) = schema.types.get(tname.as_str()) {
                let Some(obj_map) = val.as_object() else {
                    return Err(GraphqlError::new(
                        ErrorCode::ValidationError,
                        format!("argument `{arg_label}` expected input object `{tname}`"),
                    ));
                };
                for (fname, fdef) in &in_obj.fields {
                    match obj_map.get(fname.as_str()) {
                        Some(fval) => {
                            validate_bound_json_against_type(
                                fval,
                                &fdef.ty,
                                schema,
                                fname.as_str(),
                            )?;
                        }
                        None if fdef.ty.is_non_null() && fdef.default_value.is_none() => {
                            return Err(GraphqlError::new(
                                ErrorCode::ValidationError,
                                format!(
                                    "missing required field `{tname}.{fname}` in `{arg_label}`"
                                ),
                            ));
                        }
                        None => {}
                    }
                }
            }
            Ok(())
        }
    }
}

fn bind_arg_value(
    arg_val: &ArgValue,
    expected_ty: Option<&Type>,
    default_val: Option<&AstValue>,
    schema: &Schema,
    variables: &serde_json::Map<String, serde_json::Value>,
    arg_label: &str,
) -> Result<Option<serde_json::Value>, GraphqlError> {
    match arg_val {
        ArgValue::Literal(lit) => {
            if let Some(ty) = expected_ty {
                validate_bound_json_against_type(lit, ty, schema, arg_label)?;
            }
            Ok(Some(lit.clone()))
        }
        ArgValue::Variable(var_name) => match variables.get(var_name.as_str()) {
            Some(val) => {
                if let Some(ty) = expected_ty {
                    validate_bound_json_against_type(val, ty, schema, arg_label)?;
                }
                Ok(Some(val.clone()))
            }
            None => {
                if let Some(def_ast) = default_val {
                    return ast_default_to_json(def_ast).map(Some);
                }
                if expected_ty.is_some_and(Type::is_non_null) {
                    return Err(GraphqlError::new(
                        ErrorCode::ValidationError,
                        format!(
                            "missing required variable `${var_name}` for argument `{arg_label}`"
                        ),
                    ));
                }
                Ok(None)
            }
        },
        ArgValue::List(items) => {
            let elem_expected = match expected_ty {
                Some(Type::List(inner) | Type::NonNullList(inner)) => Some(&**inner),
                _ => None,
            };
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                let bound =
                    bind_arg_value(item, elem_expected, None, schema, variables, arg_label)?
                        .unwrap_or(serde_json::Value::Null);
                if let Some(ety) = elem_expected {
                    validate_bound_json_against_type(&bound, ety, schema, arg_label)?;
                }
                out.push(bound);
            }
            Ok(Some(serde_json::Value::Array(out)))
        }
        ArgValue::Object(entries) => {
            let input_obj_def =
                expected_ty.and_then(
                    |ty| match schema.types.get(ty.inner_named_type().as_str()) {
                        Some(ExtendedType::InputObject(in_obj)) => Some(in_obj),
                        _ => None,
                    },
                );

            let mut out = serde_json::Map::with_capacity(entries.len());
            for (k, v) in entries {
                let (field_ty, field_def_val) = match input_obj_def {
                    Some(in_obj) => match in_obj.fields.get(k.as_str()) {
                        Some(fdef) => (Some(&*fdef.ty), fdef.default_value.as_deref()),
                        None => (None, None),
                    },
                    None => (None, None),
                };
                if let Some(bound) =
                    bind_arg_value(v, field_ty, field_def_val, schema, variables, k)?
                {
                    out.insert(k.clone(), bound);
                }
            }

            if let Some(in_obj) = input_obj_def {
                for (fname, fdef) in &in_obj.fields {
                    if !out.contains_key(fname.as_str()) {
                        if let Some(def_ast) = fdef.default_value.as_deref() {
                            out.insert(fname.as_str().to_owned(), ast_default_to_json(def_ast)?);
                        } else if fdef.ty.is_non_null() {
                            return Err(GraphqlError::new(
                                ErrorCode::ValidationError,
                                format!("missing required input field `{fname}` in `{arg_label}`"),
                            ));
                        }
                    }
                }
            }

            Ok(Some(serde_json::Value::Object(out)))
        }
    }
}

fn validate_selection_directives(
    selections: &[SelectionField],
    variables: &serde_json::Map<String, serde_json::Value>,
) -> Result<(), GraphqlError> {
    for sel in selections {
        let _ = eval_directives_included(&sel.directives, variables)?;
        validate_selection_directives(&sel.children, variables)?;
    }
    Ok(())
}

fn lookup_root_query_field<'a>(
    schema: &'a Schema,
    operation_name: &str,
) -> Option<&'a FieldDefinition> {
    let query_root = schema
        .root_operation(OperationType::Query)
        .map_or("Query", |n| n.as_str());
    match schema.types.get(query_root)? {
        ExtendedType::Object(obj) => obj.fields.get(operation_name).map(|c| &*c.node),
        ExtendedType::Interface(iface) => iface.fields.get(operation_name).map(|c| &*c.node),
        _ => None,
    }
}

fn bind_fetch_arguments(
    fetch: &Fetch,
    field_def: &FieldDefinition,
    schema: &Schema,
    variables: &serde_json::Map<String, serde_json::Value>,
) -> Result<serde_json::Map<String, serde_json::Value>, GraphqlError> {
    let mut bound = serde_json::Map::new();

    for arg_def in &field_def.arguments {
        let arg_name = arg_def.name.as_str();
        if let Some(plan_arg) = fetch.arguments.get(arg_name) {
            if let Some(v) = bind_arg_value(
                plan_arg,
                Some(&arg_def.ty),
                arg_def.default_value.as_deref(),
                schema,
                variables,
                arg_name,
            )? {
                bound.insert(arg_name.to_owned(), v);
            }
        } else if let Some(def_ast) = arg_def.default_value.as_deref() {
            bound.insert(arg_name.to_owned(), ast_default_to_json(def_ast)?);
        } else if arg_def.ty.is_non_null() {
            return Err(GraphqlError::new(
                ErrorCode::ValidationError,
                format!(
                    "missing required argument `{arg_name}` on `Query.{}`",
                    fetch.operation
                ),
            ));
        }
    }

    // Bind any additional arguments carried on hand-built plans not declared on `field_def`.
    for (k, v) in &fetch.arguments {
        if !bound.contains_key(k)
            && !field_def.arguments.iter().any(|a| a.name.as_str() == k)
            && let Some(val) = bind_arg_value(v, None, None, schema, variables, k)?
        {
            bound.insert(k.clone(), val);
        }
    }

    Ok(bound)
}

fn build_proto_selection(
    selections: &[SelectionField],
    variables: &serde_json::Map<String, serde_json::Value>,
) -> Vec<ProtoSelectionField> {
    let mut out = Vec::new();
    for sel in selections {
        let included = eval_directives_included(&sel.directives, variables).unwrap_or(false);
        if !included || sel.name == "__typename" {
            continue;
        }
        let child_fields = build_proto_selection(&sel.children, variables);
        out.push(ProtoSelectionField {
            name: sel.name.clone(),
            alias: sel.alias.clone().unwrap_or_default(),
            args: None,
            children: (!child_fields.is_empty()).then_some(ProtoSelection {
                fields: child_fields,
            }),
        });
    }
    out
}

#[derive(Debug)]
struct PreparedFetch {
    included: bool,
    bound_args: serde_json::Map<String, serde_json::Value>,
    proto_selection: Vec<ProtoSelectionField>,
}

fn prepare_plan_fetches<'a>(
    node: &'a PlanNode,
    snapshot: &'a Snapshot,
    variables: &serde_json::Map<String, serde_json::Value>,
    prepared: &mut BTreeMap<usize, PreparedFetch>,
    active_root_keys: &mut BTreeSet<&'a str>,
) -> Result<(), GraphqlError> {
    match node {
        PlanNode::Fetch(fetch) => {
            let included = eval_directives_included(&fetch.directives, variables)?;
            validate_selection_directives(&fetch.selection, variables)?;

            let Some(field_def) = lookup_root_query_field(&snapshot.schema, &fetch.operation)
            else {
                return Err(GraphqlError::new(
                    ErrorCode::UnknownField,
                    format!(
                        "root field `Query.{}` not found in snapshot generation {}",
                        fetch.operation, snapshot.generation
                    ),
                ));
            };

            let bound_args = bind_fetch_arguments(fetch, field_def, &snapshot.schema, variables)?;
            let proto_selection = if included {
                active_root_keys.insert(fetch.response_key.as_str());
                build_proto_selection(&fetch.selection, variables)
            } else {
                Vec::new()
            };

            let key = std::ptr::from_ref::<Fetch>(fetch) as usize;
            prepared.insert(
                key,
                PreparedFetch {
                    included,
                    bound_args,
                    proto_selection,
                },
            );
            Ok(())
        }
        PlanNode::Parallel { nodes } | PlanNode::Sequence { nodes } => {
            for child in nodes {
                prepare_plan_fetches(child, snapshot, variables, prepared, active_root_keys)?;
            }
            Ok(())
        }
        PlanNode::Flatten { .. } => Err(GraphqlError::new(
            ErrorCode::UnsupportedFeature,
            "`PlanNode::Flatten` is not supported in gap-exec v0",
        )),
        PlanNode::Join(_) => Err(GraphqlError::new(
            ErrorCode::UnsupportedFeature,
            "`PlanNode::Join` is not supported in gap-exec v0",
        )),
        PlanNode::Traverse(_) => Err(GraphqlError::new(
            ErrorCode::UnsupportedFeature,
            "`PlanNode::Traverse` is not supported in gap-exec v0",
        )),
        PlanNode::Mutate(_) => Err(GraphqlError::new(
            ErrorCode::UnsupportedFeature,
            "`PlanNode::Mutate` is not supported in gap-exec v0",
        )),
        PlanNode::Gate(_) => Err(GraphqlError::new(
            ErrorCode::UnsupportedFeature,
            "`PlanNode::Gate` is not supported in gap-exec v0",
        )),
    }
}

/// Computes `(call_deadline_instant, deadline_unix_ms)` per `RUN-030` (`Brief §3.3`):
///
/// `margin = max(margin_floor, margin_fraction * remaining)`
/// `call_deadline = min(request.deadline - margin, now + branch.limits.timeout_ms)`
///
/// Returns `Err(GraphqlError(GAP_DEADLINE_EXCEEDED))` when `request.deadline - margin <= now` or
/// `branch.limits.timeout_ms == 0`.
fn compute_per_call_deadline(
    request_deadline: Instant,
    limits: &ExecLimits,
    branch_timeout_ms: u32,
    clock: &dyn Clock,
    response_key: &str,
) -> Result<(Instant, i64), GraphqlError> {
    let now = Instant::now();
    let Some(remaining) = request_deadline.checked_duration_since(now) else {
        return Err(
            GraphqlError::new(ErrorCode::DeadlineExceeded, "request deadline exceeded")
                .with_path(vec![PathSegment::Field(response_key.to_owned())]),
        );
    };
    if remaining.is_zero() || branch_timeout_ms == 0 {
        return Err(
            GraphqlError::new(ErrorCode::DeadlineExceeded, "request deadline exceeded")
                .with_path(vec![PathSegment::Field(response_key.to_owned())]),
        );
    }

    let frac = limits.margin_fraction.clamp(0.0, 1.0);
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    let frac_nanos = ((remaining.as_nanos() as f64) * frac).round() as u128;
    let frac_margin = Duration::from_nanos(u64::try_from(frac_nanos).unwrap_or(u64::MAX));
    let margin = limits.margin_floor.max(frac_margin);

    if remaining <= margin {
        return Err(GraphqlError::new(
            ErrorCode::DeadlineExceeded,
            "remaining request budget does not cover deadline safety margin",
        )
        .with_path(vec![PathSegment::Field(response_key.to_owned())]));
    }

    let net_remaining = remaining - margin;
    let branch_cap = Duration::from_millis(u64::from(branch_timeout_ms));
    let call_budget = net_remaining.min(branch_cap);

    let call_deadline = now + call_budget;
    let budget_ms = i64::try_from(call_budget.as_millis()).unwrap_or(i64::MAX);
    let deadline_unix_ms = clock.unix_millis().saturating_add(budget_ms);

    Ok((call_deadline, deadline_unix_ms))
}

fn operation_base_weight(snapshot: &Snapshot, fetch: &Fetch) -> u64 {
    snapshot
        .branches
        .get(&fetch.branch_uid)
        .and_then(|b| {
            b.operations
                .iter()
                .find(|op| op.id.name() == fetch.operation)
        })
        .map_or(1, |op| u64::from(op.cost.weight.max(1)))
}

struct ExecRunCtx<'a> {
    request: &'a ExecRequest,
    prepared: BTreeMap<usize, PreparedFetch>,
    parallel_sem: Semaphore,
    exec_cancel: CancellationToken,
    budget: ResponseSizeBudget,
}

#[derive(Default)]
struct NodeExecOutcome {
    fields: serde_json::Map<String, serde_json::Value>,
    errors: Vec<GraphqlError>,
    top_level_nulled: bool,
    too_large: Option<GraphqlError>,
    actual_cost: u64,
}

impl NodeExecOutcome {
    fn merge(&mut self, other: Self, budget: &ResponseSizeBudget) {
        self.actual_cost = self.actual_cost.saturating_add(other.actual_cost);
        if self.too_large.is_none() && other.too_large.is_some() {
            self.too_large = other.too_large;
        }
        if other.top_level_nulled {
            self.top_level_nulled = true;
        }
        self.errors.extend(other.errors);

        for (k, v) in other.fields {
            if let Some(serde_json::Value::Object(existing_obj)) = self.fields.get_mut(&k)
                && let serde_json::Value::Object(new_obj) = v
            {
                // Reconcile duplicate `{}` braces when two fetches merge into the same root object key.
                let was_non_empty = !existing_obj.is_empty();
                let new_non_empty = !new_obj.is_empty();
                budget.release_global(2);
                if was_non_empty && new_non_empty && !budget.try_reserve_global(1) {
                    if self.too_large.is_none() {
                        self.too_large = Some(too_large_error(
                            budget.cap(),
                            &[PathSegment::Field(k.clone())],
                        ));
                    }
                    continue;
                }
                for (nk, nv) in new_obj {
                    existing_obj.insert(nk, nv);
                }
            } else {
                self.fields.insert(k, v);
            }
        }
    }
}

/// Deterministic `tower::Service` executing compiled [`Plan`]s against [`ConnectorRegistry`] (`RUN-001`).
#[derive(Debug, Clone)]
pub struct ExecLayer {
    registry: Arc<ConnectorRegistry>,
    clock: Arc<dyn Clock>,
    redactor: Arc<dyn ResponseRedactor>,
}

impl ExecLayer {
    /// Creates an [`ExecLayer`] backed by `registry`, [`SystemClock`], and [`NoopResponseRedactor`].
    #[must_use]
    pub fn new(registry: Arc<ConnectorRegistry>) -> Self {
        Self {
            registry,
            clock: Arc::new(SystemClock),
            redactor: Arc::new(NoopResponseRedactor),
        }
    }

    /// Injects a custom [`Clock`] (e.g. [`gap_core::clock::ManualClock`]) used to derive
    /// `CallContext.deadline_unix_ms` (`RUN-030`, `Brief §3.3`).
    #[must_use]
    pub fn with_clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.clock = clock;
        self
    }

    /// Injects a [`ResponseRedactor`] (`RUN-061` authorization/PII redaction seam).
    #[must_use]
    pub fn with_redactor(mut self, redactor: Arc<dyn ResponseRedactor>) -> Self {
        self.redactor = redactor;
        self
    }

    /// Borrows the underlying [`ConnectorRegistry`].
    #[must_use]
    pub fn registry(&self) -> &Arc<ConnectorRegistry> {
        &self.registry
    }

    /// Executes `request` to completion (`RUN-001`, `RUN-015`, `RUN-030`, `RUN-031`, `RUN-060`).
    pub async fn execute(&self, request: ExecRequest) -> ExecResponse {
        let generation = request.snapshot.generation;
        let planned_cost = request.plan.cost.planned_cost;

        if request.cancel.is_cancelled() {
            return Self::abort_response(
                generation,
                planned_cost,
                0,
                vec![GraphqlError::new(ErrorCode::Cancelled, "request cancelled")],
            );
        }

        let mut prepared = BTreeMap::new();
        let mut active_root_keys = BTreeSet::new();
        if let Err(validation_err) = prepare_plan_fetches(
            &request.plan.root,
            &request.snapshot,
            &request.variables,
            &mut prepared,
            &mut active_root_keys,
        ) {
            return Self::abort_response(generation, planned_cost, 0, vec![validation_err]);
        }

        let exec_cancel = request.cancel.child_token();
        let budget =
            ResponseSizeBudget::new(request.limits.max_response_bytes, exec_cancel.clone());

        // Reserve top-level `data` `{}` (`2` bytes) and fixed root-key headers upfront (`RUN-060`).
        if !budget.try_reserve_global(2) {
            return Self::abort_response(
                generation,
                planned_cost,
                0,
                vec![too_large_error(request.limits.max_response_bytes, &[])],
            );
        }
        for (idx, &root_key) in active_root_keys.iter().enumerate() {
            let header_bytes = usize::from(idx > 0)
                .saturating_add(json_escaped_str_len(root_key))
                .saturating_add(1);
            if !budget.try_reserve_global(header_bytes) {
                let path = vec![PathSegment::Field(root_key.to_owned())];
                return Self::abort_response(
                    generation,
                    planned_cost,
                    0,
                    vec![too_large_error(request.limits.max_response_bytes, &path)],
                );
            }
        }

        let run_ctx = ExecRunCtx {
            request: &request,
            prepared,
            parallel_sem: Semaphore::new(request.limits.max_parallel_fetches.max(1)),
            exec_cancel,
            budget,
        };

        let outcome = tokio::select! {
            biased;
            () = request.cancel.cancelled() => {
                return Self::abort_response(
                    generation,
                    planned_cost,
                    0,
                    vec![GraphqlError::new(ErrorCode::Cancelled, "request cancelled")],
                );
            }
            res = self.execute_node(&request.plan.root, &run_ctx) => res,
        };

        if request.cancel.is_cancelled() {
            return Self::abort_response(
                generation,
                planned_cost,
                outcome.actual_cost,
                vec![GraphqlError::new(ErrorCode::Cancelled, "request cancelled")],
            );
        }

        if let Some(too_large_err) = outcome.too_large {
            return Self::abort_response(
                generation,
                planned_cost,
                outcome.actual_cost,
                vec![too_large_err],
            );
        }

        let data = if outcome.top_level_nulled {
            None
        } else {
            Some(serde_json::Value::Object(outcome.fields))
        };
        let partial = data.is_some() && !outcome.errors.is_empty();

        ExecResponse {
            data,
            errors: outcome.errors,
            extensions: GapExtensions {
                generation,
                cost: CostSummary {
                    planned: planned_cost,
                    actual: outcome.actual_cost,
                },
                partial,
            },
        }
    }

    fn abort_response(
        generation: u64,
        planned: u64,
        actual: u64,
        errors: Vec<GraphqlError>,
    ) -> ExecResponse {
        ExecResponse {
            data: None,
            errors,
            extensions: GapExtensions {
                generation,
                cost: CostSummary { planned, actual },
                partial: false,
            },
        }
    }

    fn execute_node<'a>(
        &'a self,
        node: &'a PlanNode,
        ctx: &'a ExecRunCtx<'a>,
    ) -> Pin<Box<dyn Future<Output = NodeExecOutcome> + Send + 'a>> {
        Box::pin(async move {
            match node {
                PlanNode::Fetch(fetch) => self.execute_fetch(fetch, ctx).await,
                PlanNode::Parallel { nodes } => {
                    let futures = nodes.iter().map(|child| self.execute_node(child, ctx));
                    let child_outcomes = join_all(futures).await;
                    let mut combined = NodeExecOutcome::default();
                    for child_out in child_outcomes {
                        combined.merge(child_out, &ctx.budget);
                    }
                    combined
                }
                PlanNode::Sequence { nodes } => {
                    let mut combined = NodeExecOutcome::default();
                    for child in nodes {
                        if ctx.exec_cancel.is_cancelled() || combined.too_large.is_some() {
                            break;
                        }
                        let child_out = self.execute_node(child, ctx).await;
                        combined.merge(child_out, &ctx.budget);
                    }
                    combined
                }
                PlanNode::Flatten { .. }
                | PlanNode::Join(_)
                | PlanNode::Traverse(_)
                | PlanNode::Mutate(_)
                | PlanNode::Gate(_) => NodeExecOutcome {
                    top_level_nulled: true,
                    errors: vec![GraphqlError::new(
                        ErrorCode::UnsupportedFeature,
                        "unsupported plan node in gap-exec v0",
                    )],
                    ..NodeExecOutcome::default()
                },
            }
        })
    }

    async fn execute_fetch<'a>(
        &'a self,
        fetch: &'a Fetch,
        ctx: &'a ExecRunCtx<'a>,
    ) -> NodeExecOutcome {
        let key = std::ptr::from_ref::<Fetch>(fetch) as usize;
        let Some(prep) = ctx.prepared.get(&key) else {
            return NodeExecOutcome::default();
        };
        if !prep.included {
            return NodeExecOutcome::default();
        }

        // Bound request-wide parallel fetches (`RUN-015`).
        let Ok(_parallel_permit) = ctx.parallel_sem.acquire().await else {
            return NodeExecOutcome::default();
        };

        if ctx.exec_cancel.is_cancelled() {
            return NodeExecOutcome::default();
        }

        let Some(field_def) =
            lookup_root_query_field(&ctx.request.snapshot.schema, &fetch.operation)
        else {
            return NodeExecOutcome {
                top_level_nulled: true,
                errors: vec![GraphqlError::new(
                    ErrorCode::UnknownField,
                    format!("unknown root field `Query.{}`", fetch.operation),
                )],
                ..NodeExecOutcome::default()
            };
        };

        let root_is_non_null = field_def.ty.is_non_null();
        let root_path = vec![PathSegment::Field(fetch.response_key.clone())];

        let branch_timeout_ms = ctx
            .request
            .snapshot
            .branches
            .get(&fetch.branch_uid)
            .map_or_else(
                || BranchLimits::default().timeout_ms,
                |b| b.limits.timeout_ms,
            );

        let (call_deadline, deadline_unix_ms) = match compute_per_call_deadline(
            ctx.request.deadline,
            &ctx.request.limits,
            branch_timeout_ms,
            &*self.clock,
            &fetch.response_key,
        ) {
            Ok(pair) => pair,
            Err(deadline_err) => {
                let fail_out = pre_stream_failed_fetch_outcome(
                    deadline_err,
                    root_is_non_null,
                    &root_path,
                    &ctx.budget,
                );
                return Self::assembly_to_node_outcome(fetch, fail_out, 0);
            }
        };

        let remaining_cost_units = u32::try_from(
            ctx.request
                .plan
                .cost
                .effective_budget
                .unwrap_or(ctx.request.plan.cost.planned_cost),
        )
        .unwrap_or(u32::MAX);

        let order_by: Vec<OrderBy> = fetch
            .pushdown
            .order_by
            .iter()
            .map(|clause| {
                if let Some(col) = clause.strip_suffix(" DESC") {
                    OrderBy {
                        path: col.trim().to_owned(),
                        descending: true,
                    }
                } else {
                    let col = clause.strip_suffix(" ASC").unwrap_or(clause);
                    OrderBy {
                        path: col.trim().to_owned(),
                        descending: false,
                    }
                }
            })
            .collect();

        let page = fetch.pushdown.limit.map(|limit| Page {
            limit: u32::try_from(limit).unwrap_or(u32::MAX),
            cursor: String::new(),
            offset: 0,
        });

        let query_req = QueryRequest {
            ctx: Some(CallContext {
                tenant_id: ctx.request.snapshot.tenant_id.as_str().to_owned(),
                request_id: String::new(),
                traceparent: String::new(),
                deadline_unix_ms,
                remaining_cost_units,
                principal: None,
                hop_count: 0,
                trace_path: Vec::new(),
                idempotency_key: String::new(),
                snapshot_generation: ctx.request.snapshot.generation,
            }),
            operation: fetch.operation.clone(),
            arguments: Some(json_map_to_prost_struct(&prep.bound_args)),
            selection: (!prep.proto_selection.is_empty()).then(|| ProtoSelection {
                fields: prep.proto_selection.clone(),
            }),
            predicate: None,
            order_by,
            page,
            foreach: Vec::new(),
            as_of: String::new(),
        };

        let call_scope = CallScope {
            deadline: call_deadline,
            cancel: ctx.exec_cancel.child_token(),
            request_id: String::new(),
            tenant_id: ctx.request.snapshot.tenant_id.as_str().to_owned(),
            traceparent: String::new(),
            metadata: BTreeMap::new(),
        };

        let sensitive_secrets: Vec<&str> = self
            .registry
            .get(&fetch.branch_uid)
            .map_or_else(Vec::new, |b| b.sensitive_values());

        let base_weight = operation_base_weight(&ctx.request.snapshot, fetch);

        let stream = match self
            .registry
            .open_query(&fetch.branch_uid, query_req, call_scope)
            .await
        {
            Ok(s) => s,
            Err(BranchQueryError::Registry(gap_err)) => {
                let gql_err = GraphqlError::from(gap_err).with_path(root_path.clone());
                let fail_out = pre_stream_failed_fetch_outcome(
                    gql_err,
                    root_is_non_null,
                    &root_path,
                    &ctx.budget,
                );
                return Self::assembly_to_node_outcome(fetch, fail_out, 0);
            }
            Err(BranchQueryError::Connector(ngc_err)) => {
                let gql_err = map_ngc_error(&ngc_err, Some(root_path.clone()), &sensitive_secrets);
                let fail_out = pre_stream_failed_fetch_outcome(
                    gql_err,
                    root_is_non_null,
                    &root_path,
                    &ctx.budget,
                );
                return Self::assembly_to_node_outcome(fetch, fail_out, base_weight);
            }
        };

        let assembly_out = consume_and_project_fetch(
            FetchAssemblyInput {
                snapshot: &ctx.request.snapshot,
                fetch,
                root_field_ty: &field_def.ty,
                variables: &ctx.request.variables,
                sensitive_secrets: &sensitive_secrets,
                redactor: &*self.redactor,
                budget: &ctx.budget,
            },
            stream,
        )
        .await;

        Self::assembly_to_node_outcome(fetch, assembly_out, base_weight)
    }

    fn assembly_to_node_outcome(
        fetch: &Fetch,
        assembly_out: FetchAssemblyOutcome,
        base_weight: u64,
    ) -> NodeExecOutcome {
        match assembly_out {
            FetchAssemblyOutcome::Field {
                value,
                errors,
                rows_observed,
            } => {
                let mut fields = serde_json::Map::new();
                fields.insert(fetch.response_key.clone(), value);
                NodeExecOutcome {
                    fields,
                    errors,
                    top_level_nulled: false,
                    too_large: None,
                    actual_cost: base_weight.saturating_add(rows_observed),
                }
            }
            FetchAssemblyOutcome::PropagateTopNull {
                errors,
                rows_observed,
            } => NodeExecOutcome {
                fields: serde_json::Map::new(),
                errors,
                top_level_nulled: true,
                too_large: None,
                actual_cost: base_weight.saturating_add(rows_observed),
            },
            FetchAssemblyOutcome::TooLarge {
                error,
                rows_observed,
            } => NodeExecOutcome {
                fields: serde_json::Map::new(),
                errors: Vec::new(),
                top_level_nulled: true,
                too_large: Some(error),
                actual_cost: base_weight.saturating_add(rows_observed),
            },
        }
    }
}

impl Service<ExecRequest> for ExecLayer {
    type Response = ExecResponse;
    type Error = Infallible;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: ExecRequest) -> Self::Future {
        let this = self.clone();
        Box::pin(async move { Ok(this.execute(req).await) })
    }
}
