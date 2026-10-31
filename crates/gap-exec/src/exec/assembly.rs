//! Incremental GraphQL response assembly (`RUN-060`, `GQL-051`) and `RUN-061` policy/PII redaction seam.

use std::fmt;
use std::sync::atomic::{AtomicUsize, Ordering};

use apollo_compiler::Schema;
use apollo_compiler::ast::Type;
use apollo_compiler::schema::{ExtendedType, FieldDefinition};
use futures_util::StreamExt as _;
use gap_core::error::{ErrorCode, NgcErrorInfo, PathSegment};
use gap_core::snapshot::Snapshot;
use gap_ngc_sdk::connector::BoxStream;
use gap_ngc_sdk::error::ngc_error_from_proto;
use gap_plan::{ArgValue, Fetch, SelectionDirective, SelectionField};
use gap_proto::ngc::v1::{QueryResponse, query_response};
use tokio_util::sync::CancellationToken;

use super::error::{GraphqlError, map_ngc_error};

/// Documented `RUN-061` seam for applying AuthzLayer field/value redaction (`@pii` or Cedar policy
/// masks) during response assembly before bytes leave the process (`spec/06-runtime.md` §6).
///
/// Full policy-plan wiring lands with `AuthzLayer` (`M6`); `WP-M1-07` invokes this seam on every
/// projected leaf and object field so downstream redaction plugs in without altering assembly flow.
pub trait ResponseRedactor: Send + Sync + fmt::Debug {
    /// Inspects or masks `value` projected for `(parent_type, field_name)` at `path` under `snapshot`.
    ///
    /// Returning `None` omits/removes the field when policy dictates removal; returning `Some(v)`
    /// emits `v` (for example `"***"` when masked).
    fn redact_field(
        &self,
        snapshot: &Snapshot,
        parent_type: &str,
        field_name: &str,
        path: &[PathSegment],
        value: serde_json::Value,
    ) -> Option<serde_json::Value>;
}

/// Default pass-through [`ResponseRedactor`] used in `WP-M1-07` (`RUN-061` seam).
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopResponseRedactor;

impl ResponseRedactor for NoopResponseRedactor {
    fn redact_field(
        &self,
        _snapshot: &Snapshot,
        _parent_type: &str,
        _field_name: &str,
        _path: &[PathSegment],
        value: serde_json::Value,
    ) -> Option<serde_json::Value> {
        Some(value)
    }
}

/// Exact compact UTF-8 JSON byte length of `s` when serialized as a JSON string (`"..."`),
/// matching `serde_json::to_vec` byte-for-byte.
#[must_use]
pub(crate) fn json_escaped_str_len(s: &str) -> usize {
    let mut len: usize = 2; // opening + closing `"`
    for &b in s.as_bytes() {
        let char_len = match b {
            b'"' | b'\\' | b'\x08' | b'\x0c' | b'\n' | b'\r' | b'\t' => 2,
            0x00..=0x1f => 6, // `\u00XX`
            _ => 1,
        };
        len = len.saturating_add(char_len);
    }
    len
}

/// Exact compact UTF-8 JSON byte length of `val` (`serde_json::to_vec(val).len()`).
#[must_use]
pub(crate) fn json_value_compact_len(val: &serde_json::Value) -> usize {
    match val {
        serde_json::Value::Null | serde_json::Value::Bool(true) => 4,
        serde_json::Value::Bool(false) => 5,
        serde_json::Value::Number(n) => n.to_string().len(),
        serde_json::Value::String(s) => json_escaped_str_len(s),
        serde_json::Value::Array(items) => {
            let mut sum = 2_usize;
            for (idx, item) in items.iter().enumerate() {
                if idx > 0 {
                    sum = sum.saturating_add(1);
                }
                sum = sum.saturating_add(json_value_compact_len(item));
            }
            sum
        }
        serde_json::Value::Object(map) => {
            let mut sum = 2_usize;
            for (idx, (k, v)) in map.iter().enumerate() {
                if idx > 0 {
                    sum = sum.saturating_add(1);
                }
                sum = sum
                    .saturating_add(json_escaped_str_len(k))
                    .saturating_add(1)
                    .saturating_add(json_value_compact_len(v));
            }
            sum
        }
    }
}

/// Lock-free incremental response byte budget shared across all fetches of a single `ExecRequest` (`RUN-060`, `INV-02`).
pub(crate) struct ResponseSizeBudget {
    used: AtomicUsize,
    cap: usize,
    abort_cancel: CancellationToken,
}

impl ResponseSizeBudget {
    #[must_use]
    pub(crate) fn new(cap: usize, abort_cancel: CancellationToken) -> Self {
        Self {
            used: AtomicUsize::new(0),
            cap,
            abort_cancel,
        }
    }

    #[must_use]
    pub(crate) const fn cap(&self) -> usize {
        self.cap
    }

    pub(crate) fn try_reserve_global(&self, delta: usize) -> bool {
        if delta == 0 {
            return self.used.load(Ordering::Acquire) <= self.cap;
        }
        let mut prev = self.used.load(Ordering::Acquire);
        loop {
            let Some(next) = prev.checked_add(delta) else {
                self.abort_cancel.cancel();
                return false;
            };
            if next > self.cap {
                self.abort_cancel.cancel();
                return false;
            }
            match self
                .used
                .compare_exchange_weak(prev, next, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => return true,
                Err(actual) => prev = actual,
            }
        }
    }

    pub(crate) fn release_global(&self, refund: usize) {
        if refund > 0 {
            self.used.fetch_sub(refund, Ordering::AcqRel);
        }
    }
}

/// Per-fetch byte tracker that supports exact local checkpoints when nullable ancestors absorb
/// a non-null violation without disturbing concurrent sibling fetches.
pub(crate) struct LocalSizeTracker<'a> {
    budget: &'a ResponseSizeBudget,
    local_used: usize,
}

impl<'a> LocalSizeTracker<'a> {
    #[must_use]
    pub(crate) const fn new(budget: &'a ResponseSizeBudget) -> Self {
        Self {
            budget,
            local_used: 0,
        }
    }

    #[must_use]
    pub(crate) const fn checkpoint(&self) -> usize {
        self.local_used
    }

    pub(crate) fn try_reserve(
        &mut self,
        delta: usize,
        path: &[PathSegment],
    ) -> Result<(), GraphqlError> {
        if self.budget.try_reserve_global(delta) {
            self.local_used = self.local_used.saturating_add(delta);
            Ok(())
        } else {
            Err(too_large_error(self.budget.cap, path))
        }
    }

    pub(crate) fn rollback_to(&mut self, checkpoint: usize) {
        if self.local_used > checkpoint {
            let refund = self.local_used - checkpoint;
            self.local_used = checkpoint;
            self.budget.release_global(refund);
        }
    }
}

#[must_use]
pub(crate) fn too_large_error(cap: usize, path: &[PathSegment]) -> GraphqlError {
    GraphqlError::new(
        ErrorCode::ResponseTooLarge,
        format!("assembled response exceeded max_response_bytes ({cap} bytes)"),
    )
    .with_path(path.to_vec())
}

/// Evaluates `@skip(if: ...)` and `@include(if: ...)` directives (`Boolean!` required).
///
/// Returns `Ok(true)` if the field/fetch is included, `Ok(false)` if skipped, or
/// `Err(GraphqlError)` (`GAP_VALIDATION_ERROR`) if a directive variable is missing, null, or non-boolean.
pub(crate) fn eval_directives_included(
    directives: &[SelectionDirective],
    variables: &serde_json::Map<String, serde_json::Value>,
) -> Result<bool, GraphqlError> {
    let mut included = true;
    for dir in directives {
        let cond = match &dir.if_arg {
            ArgValue::Literal(serde_json::Value::Bool(b)) => *b,
            ArgValue::Literal(_) => {
                return Err(GraphqlError::new(
                    ErrorCode::ValidationError,
                    format!("directive `@{}` argument `if` must be Boolean", dir.name),
                ));
            }
            ArgValue::Variable(var_name) => match variables.get(var_name.as_str()) {
                Some(serde_json::Value::Bool(b)) => *b,
                Some(serde_json::Value::Null) | None => {
                    return Err(GraphqlError::new(
                        ErrorCode::ValidationError,
                        format!(
                            "missing or null required Boolean! variable `${var_name}` for `@{}`",
                            dir.name
                        ),
                    ));
                }
                Some(_) => {
                    return Err(GraphqlError::new(
                        ErrorCode::ValidationError,
                        format!("variable `${var_name}` for `@{}` must be Boolean", dir.name),
                    ));
                }
            },
            ArgValue::List(_) | ArgValue::Object(_) => {
                return Err(GraphqlError::new(
                    ErrorCode::ValidationError,
                    format!("directive `@{}` argument `if` must be Boolean", dir.name),
                ));
            }
        };

        match dir.name.as_str() {
            "skip" if cond => included = false,
            "include" if !cond => included = false,
            _ => {}
        }
    }
    Ok(included)
}

#[derive(Debug)]
enum ProjErr {
    /// Non-null violation occurred inside a non-null position; error already appended to `errors`,
    /// and `tracker` already rolled back to the callee's entry checkpoint.
    NullBubble,
    /// `max_response_bytes` exceeded at `GraphqlError.path`.
    TooLarge(GraphqlError),
    /// Invalid runtime directive variable (pre-validated, defensive).
    Validation(GraphqlError),
}

/// Outcome of streaming and assembling a single root `Fetch`.
#[derive(Debug)]
pub(crate) enum FetchAssemblyOutcome {
    /// Fetch produced a valid root field value (`Value::Null` when nullable root absorbed an error).
    Field {
        value: serde_json::Value,
        errors: Vec<GraphqlError>,
        rows_observed: u64,
    },
    /// Non-null root field failed or bubbled a non-null violation, nulling top-level `data` (`GQL-051`).
    PropagateTopNull {
        errors: Vec<GraphqlError>,
        rows_observed: u64,
    },
    /// Assembled `data` exceeded `max_response_bytes` (`RUN-060`).
    TooLarge {
        error: GraphqlError,
        rows_observed: u64,
    },
}

fn is_composite_schema_type(schema: &Schema, type_name: &str) -> bool {
    matches!(
        schema.types.get(type_name),
        Some(ExtendedType::Object(_) | ExtendedType::Interface(_) | ExtendedType::Union(_))
    )
}

fn lookup_field_def<'a>(
    schema: &'a Schema,
    parent_type: &str,
    field_name: &str,
) -> Option<&'a FieldDefinition> {
    match schema.types.get(parent_type)? {
        ExtendedType::Object(obj) => obj.fields.get(field_name).map(|c| &*c.node),
        ExtendedType::Interface(iface) => iface.fields.get(field_name).map(|c| &*c.node),
        _ => None,
    }
}

fn resolve_concrete_typename<'a>(
    schema: &'a Schema,
    declared_type: &'a str,
    st: &prost_types::Struct,
) -> &'a str {
    if matches!(
        schema.types.get(declared_type),
        Some(ExtendedType::Interface(_) | ExtendedType::Union(_))
    ) && let Some(prost_types::Value {
        kind: Some(prost_types::value::Kind::StringValue(hint)),
    }) = st.fields.get("__typename")
        && let Some((key, ExtendedType::Object(_))) = schema.types.get_key_value(hint.as_str())
    {
        return key.as_str();
    }
    declared_type
}

fn prost_is_null(val: &prost_types::Value) -> bool {
    matches!(
        val.kind,
        None | Some(prost_types::value::Kind::NullValue(_))
    )
}

fn f64_to_json_int_or_float(n: f64, prefer_int: bool) -> Option<serde_json::Number> {
    if !n.is_finite() {
        return None;
    }
    #[allow(clippy::cast_precision_loss)]
    let fits_safe_i64 = n.fract() == 0.0
        && (prefer_int
            || (-9_007_199_254_740_991.0_f64..=9_007_199_254_740_991.0_f64).contains(&n))
        && n >= (i64::MIN as f64)
        && n < 9_223_372_036_854_775_808.0_f64;

    if fits_safe_i64 {
        #[allow(clippy::cast_possible_truncation)]
        let as_i64 = n as i64;
        Some(serde_json::Number::from(as_i64))
    } else {
        serde_json::Number::from_f64(n)
    }
}

struct MergedSelection<'a> {
    response_key: &'a str,
    name: &'a str,
    children: Vec<&'a SelectionField>,
}

fn collect_merged_selections<'a>(
    selections: &'a [SelectionField],
    variables: &serde_json::Map<String, serde_json::Value>,
) -> Result<Vec<MergedSelection<'a>>, GraphqlError> {
    let mut merged: Vec<MergedSelection<'a>> = Vec::with_capacity(selections.len());
    for sel in selections {
        if !eval_directives_included(&sel.directives, variables)? {
            continue;
        }
        let rkey = sel.response_key();
        if let Some(existing) = merged.iter_mut().find(|m| m.response_key == rkey) {
            existing.children.extend(sel.children.iter());
        } else {
            merged.push(MergedSelection {
                response_key: rkey,
                name: sel.name.as_str(),
                children: sel.children.iter().collect(),
            });
        }
    }
    Ok(merged)
}

struct AssemblyCtx<'a> {
    snapshot: &'a Snapshot,
    variables: &'a serde_json::Map<String, serde_json::Value>,
    redactor: &'a dyn ResponseRedactor,
    tracker: LocalSizeTracker<'a>,
    errors: Vec<GraphqlError>,
}

impl AssemblyCtx<'_> {
    /// Projects an arbitrary `prost_types::Value` as untyped JSON (for the `JSON` scalar),
    /// checking and reserving exact compact JSON bytes BEFORE cloning string payloads (`RUN-060`).
    fn project_raw_json_value(
        &mut self,
        val: &prost_types::Value,
        path: &[PathSegment],
    ) -> Result<serde_json::Value, ProjErr> {
        match &val.kind {
            None | Some(prost_types::value::Kind::NullValue(_)) => {
                self.tracker
                    .try_reserve(4, path)
                    .map_err(ProjErr::TooLarge)?;
                Ok(serde_json::Value::Null)
            }
            Some(prost_types::value::Kind::BoolValue(b)) => {
                let len = if *b { 4 } else { 5 };
                self.tracker
                    .try_reserve(len, path)
                    .map_err(ProjErr::TooLarge)?;
                Ok(serde_json::Value::Bool(*b))
            }
            Some(prost_types::value::Kind::NumberValue(n)) => {
                let Some(num) = f64_to_json_int_or_float(*n, false) else {
                    self.tracker
                        .try_reserve(4, path)
                        .map_err(ProjErr::TooLarge)?;
                    return Ok(serde_json::Value::Null);
                };
                let len = num.to_string().len();
                self.tracker
                    .try_reserve(len, path)
                    .map_err(ProjErr::TooLarge)?;
                Ok(serde_json::Value::Number(num))
            }
            Some(prost_types::value::Kind::StringValue(s)) => {
                let len = json_escaped_str_len(s);
                self.tracker
                    .try_reserve(len, path)
                    .map_err(ProjErr::TooLarge)?;
                Ok(serde_json::Value::String(s.clone()))
            }
            Some(prost_types::value::Kind::ListValue(list)) => {
                self.tracker
                    .try_reserve(2, path)
                    .map_err(ProjErr::TooLarge)?;
                let mut out = Vec::with_capacity(list.values.len());
                for (idx, item) in list.values.iter().enumerate() {
                    if idx > 0 {
                        self.tracker
                            .try_reserve(1, path)
                            .map_err(ProjErr::TooLarge)?;
                    }
                    out.push(self.project_raw_json_value(item, path)?);
                }
                Ok(serde_json::Value::Array(out))
            }
            Some(prost_types::value::Kind::StructValue(st)) => {
                self.project_raw_json_struct(st, path)
            }
        }
    }

    fn project_raw_json_struct(
        &mut self,
        st: &prost_types::Struct,
        path: &[PathSegment],
    ) -> Result<serde_json::Value, ProjErr> {
        self.tracker
            .try_reserve(2, path)
            .map_err(ProjErr::TooLarge)?;
        let mut map = serde_json::Map::with_capacity(st.fields.len());
        for (idx, (k, v)) in st.fields.iter().enumerate() {
            let key_bytes = usize::from(idx > 0)
                .saturating_add(json_escaped_str_len(k))
                .saturating_add(1);
            self.tracker
                .try_reserve(key_bytes, path)
                .map_err(ProjErr::TooLarge)?;
            let projected = self.project_raw_json_value(v, path)?;
            map.insert(k.clone(), projected);
        }
        Ok(serde_json::Value::Object(map))
    }

    /// Projects a non-null `prost_types::Value` as a named GraphQL scalar or enum, reserving exact
    /// compact JSON bytes before cloning strings.
    fn project_leaf_scalar(
        &mut self,
        val: &prost_types::Value,
        scalar_name: &str,
        is_non_null: bool,
        path: &[PathSegment],
    ) -> Result<serde_json::Value, ProjErr> {
        if scalar_name == "JSON" {
            return self.project_raw_json_value(val, path);
        }

        let cp = self.tracker.checkpoint();
        let converted: Option<serde_json::Value> = match (scalar_name, &val.kind) {
            ("Int", Some(prost_types::value::Kind::NumberValue(n))) => {
                if n.is_finite()
                    && n.fract() == 0.0
                    && (*n >= f64::from(i32::MIN))
                    && (*n <= f64::from(i32::MAX))
                {
                    #[allow(clippy::cast_possible_truncation)]
                    let i = *n as i64;
                    let num = serde_json::Number::from(i);
                    self.tracker
                        .try_reserve(num.to_string().len(), path)
                        .map_err(ProjErr::TooLarge)?;
                    Some(serde_json::Value::Number(num))
                } else {
                    None
                }
            }
            ("Float", Some(prost_types::value::Kind::NumberValue(n))) => {
                if let Some(num) = serde_json::Number::from_f64(*n) {
                    self.tracker
                        .try_reserve(num.to_string().len(), path)
                        .map_err(ProjErr::TooLarge)?;
                    Some(serde_json::Value::Number(num))
                } else {
                    None
                }
            }
            ("Boolean", Some(prost_types::value::Kind::BoolValue(b))) => {
                self.tracker
                    .try_reserve(if *b { 4 } else { 5 }, path)
                    .map_err(ProjErr::TooLarge)?;
                Some(serde_json::Value::Bool(*b))
            }
            ("String", Some(prost_types::value::Kind::StringValue(s))) => {
                self.tracker
                    .try_reserve(json_escaped_str_len(s), path)
                    .map_err(ProjErr::TooLarge)?;
                Some(serde_json::Value::String(s.clone()))
            }
            ("ID", Some(prost_types::value::Kind::StringValue(s))) => {
                self.tracker
                    .try_reserve(json_escaped_str_len(s), path)
                    .map_err(ProjErr::TooLarge)?;
                Some(serde_json::Value::String(s.clone()))
            }
            ("ID", Some(prost_types::value::Kind::NumberValue(n)))
                if n.is_finite() && n.fract() == 0.0 =>
            {
                #[allow(clippy::cast_possible_truncation)]
                let s = (*n as i64).to_string();
                self.tracker
                    .try_reserve(json_escaped_str_len(&s), path)
                    .map_err(ProjErr::TooLarge)?;
                Some(serde_json::Value::String(s))
            }
            // Enum or custom scalar (e.g. DateTime)
            (_, Some(prost_types::value::Kind::StringValue(s))) => {
                if let Some(ExtendedType::Enum(en)) = self.snapshot.schema.types.get(scalar_name)
                    && !en.values.contains_key(s.as_str())
                {
                    None
                } else {
                    self.tracker
                        .try_reserve(json_escaped_str_len(s), path)
                        .map_err(ProjErr::TooLarge)?;
                    Some(serde_json::Value::String(s.clone()))
                }
            }
            (_, _) if !matches!(scalar_name, "Int" | "Float" | "Boolean" | "String" | "ID") => {
                // Custom scalar fallback (e.g. numeric timestamp scalar)
                if matches!(
                    self.snapshot.schema.types.get(scalar_name),
                    Some(ExtendedType::Enum(_))
                ) {
                    None
                } else {
                    return self.project_raw_json_value(val, path);
                }
            }
            _ => None,
        };

        if let Some(ok_val) = converted {
            Ok(ok_val)
        } else {
            self.tracker.rollback_to(cp);
            self.errors.push(
                GraphqlError::new(
                    ErrorCode::TypeMismatch,
                    format!("connector returned incompatible scalar value for `{scalar_name}`"),
                )
                .with_path(path.to_vec()),
            );
            if is_non_null {
                Err(ProjErr::NullBubble)
            } else {
                self.tracker
                    .try_reserve(4, path)
                    .map_err(ProjErr::TooLarge)?;
                Ok(serde_json::Value::Null)
            }
        }
    }

    /// Projects `raw_val` according to GraphQL `expected_ty`.
    ///
    /// Checkpoint invariant:
    /// - `Ok(v)`: `tracker.checkpoint() - entry_cp == json_value_compact_len(&v)`.
    /// - `Err(ProjErr::NullBubble)`: `tracker.checkpoint() == entry_cp`, error already pushed.
    fn project_typed_value(
        &mut self,
        raw_val: Option<&prost_types::Value>,
        expected_ty: &Type,
        selections: &[&SelectionField],
        path: &mut Vec<PathSegment>,
    ) -> Result<serde_json::Value, ProjErr> {
        let entry_cp = self.tracker.checkpoint();
        let is_non_null = expected_ty.is_non_null();

        let Some(val) = raw_val.filter(|v| !prost_is_null(v)) else {
            if is_non_null {
                let label = match path.last() {
                    Some(PathSegment::Field(s)) => s.clone(),
                    Some(PathSegment::Index(i)) => format!("[{i}]"),
                    None => "<root>".to_owned(),
                };
                self.errors.push(
                    GraphqlError::new(
                        ErrorCode::BranchError,
                        format!("non-null field `{label}` resolved to null"),
                    )
                    .with_path(path.clone()),
                );
                return Err(ProjErr::NullBubble);
            }
            self.tracker
                .try_reserve(4, path)
                .map_err(ProjErr::TooLarge)?;
            return Ok(serde_json::Value::Null);
        };

        let inner_res = match expected_ty {
            Type::List(elem_ty) | Type::NonNullList(elem_ty) => {
                self.project_list_value(val, elem_ty, selections, path)
            }
            Type::Named(type_name) | Type::NonNullNamed(type_name) => {
                let tname = type_name.as_str();
                if is_composite_schema_type(&self.snapshot.schema, tname) {
                    let Some(prost_types::value::Kind::StructValue(st)) = &val.kind else {
                        self.errors.push(
                            GraphqlError::new(
                                ErrorCode::TypeMismatch,
                                format!("expected object row for `{tname}`"),
                            )
                            .with_path(path.clone()),
                        );
                        return if is_non_null {
                            Err(ProjErr::NullBubble)
                        } else {
                            self.tracker
                                .try_reserve(4, path)
                                .map_err(ProjErr::TooLarge)?;
                            Ok(serde_json::Value::Null)
                        };
                    };
                    self.project_object_struct(st, tname, selections, path)
                } else {
                    self.project_leaf_scalar(val, tname, is_non_null, path)
                }
            }
        };

        match inner_res {
            Ok(v) => Ok(v),
            Err(ProjErr::NullBubble) => {
                self.tracker.rollback_to(entry_cp);
                if is_non_null {
                    Err(ProjErr::NullBubble)
                } else {
                    self.tracker
                        .try_reserve(4, path)
                        .map_err(ProjErr::TooLarge)?;
                    Ok(serde_json::Value::Null)
                }
            }
            Err(other) => Err(other),
        }
    }

    fn project_list_value(
        &mut self,
        val: &prost_types::Value,
        elem_ty: &Type,
        selections: &[&SelectionField],
        path: &mut Vec<PathSegment>,
    ) -> Result<serde_json::Value, ProjErr> {
        let list_cp = self.tracker.checkpoint();
        let Some(prost_types::value::Kind::ListValue(list)) = &val.kind else {
            self.errors.push(
                GraphqlError::new(ErrorCode::TypeMismatch, "expected list value from branch")
                    .with_path(path.clone()),
            );
            return Err(ProjErr::NullBubble);
        };

        self.tracker
            .try_reserve(2, path)
            .map_err(ProjErr::TooLarge)?;
        let mut out = Vec::with_capacity(list.values.len());
        for (idx, item) in list.values.iter().enumerate() {
            path.push(PathSegment::Index(idx));
            if idx > 0
                && let Err(e) = self.tracker.try_reserve(1, path)
            {
                path.pop();
                return Err(ProjErr::TooLarge(e));
            }
            let item_res = self.project_typed_value(Some(item), elem_ty, selections, path);
            path.pop();
            match item_res {
                Ok(v) => out.push(v),
                Err(ProjErr::NullBubble) => {
                    self.tracker.rollback_to(list_cp);
                    return Err(ProjErr::NullBubble);
                }
                Err(other) => return Err(other),
            }
        }
        Ok(serde_json::Value::Array(out))
    }

    fn project_object_struct(
        &mut self,
        st: &prost_types::Struct,
        declared_type: &str,
        selections: &[&SelectionField],
        path: &mut Vec<PathSegment>,
    ) -> Result<serde_json::Value, ProjErr> {
        let obj_cp = self.tracker.checkpoint();
        let concrete_type = resolve_concrete_typename(&self.snapshot.schema, declared_type, st);

        // Flatten slice of `&SelectionField` to evaluate `@skip`/`@include` and merge duplicate response keys.
        let mut merged: Vec<MergedSelection<'_>> = Vec::with_capacity(selections.len());
        for &sel in selections {
            let included = eval_directives_included(&sel.directives, self.variables)
                .map_err(ProjErr::Validation)?;
            if !included {
                continue;
            }
            let rkey = sel.response_key();
            if let Some(existing) = merged.iter_mut().find(|m| m.response_key == rkey) {
                existing.children.extend(sel.children.iter());
            } else {
                merged.push(MergedSelection {
                    response_key: rkey,
                    name: sel.name.as_str(),
                    children: sel.children.iter().collect(),
                });
            }
        }

        self.tracker
            .try_reserve(2, path)
            .map_err(ProjErr::TooLarge)?;

        let mut out_map = serde_json::Map::with_capacity(merged.len());
        for ms in merged {
            if out_map.contains_key(ms.response_key) {
                continue;
            }
            let field_entry_cp = self.tracker.checkpoint();
            path.push(PathSegment::Field(ms.response_key.to_owned()));

            let comma = usize::from(!out_map.is_empty());
            let key_header = comma
                .saturating_add(json_escaped_str_len(ms.response_key))
                .saturating_add(1);
            if let Err(e) = self.tracker.try_reserve(key_header, path) {
                path.pop();
                return Err(ProjErr::TooLarge(e));
            }

            if ms.name == "__typename" {
                let tname_str = concrete_type.to_owned();
                if let Err(e) = self
                    .tracker
                    .try_reserve(json_escaped_str_len(&tname_str), path)
                {
                    path.pop();
                    return Err(ProjErr::TooLarge(e));
                }
                out_map.insert(
                    ms.response_key.to_owned(),
                    serde_json::Value::String(tname_str),
                );
                path.pop();
                continue;
            }

            let Some(field_def) = lookup_field_def(&self.snapshot.schema, concrete_type, ms.name)
                .or_else(|| lookup_field_def(&self.snapshot.schema, declared_type, ms.name))
            else {
                self.errors.push(
                    GraphqlError::new(
                        ErrorCode::UnknownField,
                        format!("field `{concrete_type}.{}` missing from schema", ms.name),
                    )
                    .with_path(path.clone()),
                );
                path.pop();
                self.tracker.rollback_to(obj_cp);
                return Err(ProjErr::NullBubble);
            };

            let val_cp = self.tracker.checkpoint();
            let raw_field = st.fields.get(ms.name);
            let field_res = self.project_typed_value(raw_field, &field_def.ty, &ms.children, path);

            let projected_val = match field_res {
                Ok(v) => v,
                Err(ProjErr::NullBubble) => {
                    path.pop();
                    self.tracker.rollback_to(obj_cp);
                    return Err(ProjErr::NullBubble);
                }
                Err(other) => {
                    path.pop();
                    return Err(other);
                }
            };

            // Invoke `RUN-061` authorization/PII redaction seam and reconcile any byte delta.
            match self.redactor.redact_field(
                self.snapshot,
                concrete_type,
                ms.name,
                path,
                projected_val,
            ) {
                Some(redacted_val) => {
                    self.tracker.rollback_to(val_cp);
                    let redacted_len = json_value_compact_len(&redacted_val);
                    if let Err(e) = self.tracker.try_reserve(redacted_len, path) {
                        path.pop();
                        return Err(ProjErr::TooLarge(e));
                    }
                    out_map.insert(ms.response_key.to_owned(), redacted_val);
                }
                None => {
                    self.tracker.rollback_to(field_entry_cp);
                    if field_def.ty.is_non_null() {
                        self.errors.push(
                            GraphqlError::new(
                                ErrorCode::Forbidden,
                                format!("non-null field `{concrete_type}.{}` redacted", ms.name),
                            )
                            .with_path(path.clone()),
                        );
                        path.pop();
                        self.tracker.rollback_to(obj_cp);
                        return Err(ProjErr::NullBubble);
                    }
                }
            }

            path.pop();
        }

        Ok(serde_json::Value::Object(out_map))
    }

    fn extract_root_scalar_from_row(
        &mut self,
        st: &prost_types::Struct,
        fetch: &Fetch,
        scalar_name: &str,
        is_non_null: bool,
        path: &[PathSegment],
    ) -> Result<serde_json::Value, ProjErr> {
        if scalar_name == "JSON" {
            if st.fields.len() == 1
                && let Some(unwrapped) = st
                    .fields
                    .get(&fetch.operation)
                    .or_else(|| st.fields.get(&fetch.response_key))
            {
                return self.project_raw_json_value(unwrapped, path);
            }
            return self.project_raw_json_struct(st, path);
        }

        let candidate = st
            .fields
            .get(&fetch.response_key)
            .or_else(|| st.fields.get(&fetch.operation))
            .or_else(|| st.fields.get("value"))
            .or_else(|| {
                if st.fields.len() == 1 {
                    st.fields.values().next()
                } else {
                    None
                }
            });

        let Some(val) = candidate.filter(|v| !prost_is_null(v)) else {
            if is_non_null {
                self.errors.push(
                    GraphqlError::new(
                        ErrorCode::BranchError,
                        format!(
                            "non-null scalar root field `{}` resolved to null",
                            fetch.response_key
                        ),
                    )
                    .with_path(path.to_vec()),
                );
                return Err(ProjErr::NullBubble);
            }
            self.tracker
                .try_reserve(4, path)
                .map_err(ProjErr::TooLarge)?;
            return Ok(serde_json::Value::Null);
        };

        self.project_leaf_scalar(val, scalar_name, is_non_null, path)
    }
}

fn finish_failed_root(
    root_is_non_null: bool,
    ctx: &mut AssemblyCtx<'_>,
    root_cp: usize,
    root_path: &[PathSegment],
    rows_observed: u64,
) -> FetchAssemblyOutcome {
    ctx.tracker.rollback_to(root_cp);
    if root_is_non_null {
        FetchAssemblyOutcome::PropagateTopNull {
            errors: std::mem::take(&mut ctx.errors),
            rows_observed,
        }
    } else {
        match ctx.tracker.try_reserve(4, root_path) {
            Ok(()) => FetchAssemblyOutcome::Field {
                value: serde_json::Value::Null,
                errors: std::mem::take(&mut ctx.errors),
                rows_observed,
            },
            Err(error) => FetchAssemblyOutcome::TooLarge {
                error,
                rows_observed,
            },
        }
    }
}

fn record_ngc_root_failure(
    ngc_err: &NgcErrorInfo,
    root_is_non_null: bool,
    sensitive_secrets: &[&str],
    ctx: &mut AssemblyCtx<'_>,
    root_cp: usize,
    root_path: &[PathSegment],
    rows_observed: u64,
) -> FetchAssemblyOutcome {
    ctx.errors.push(map_ngc_error(
        ngc_err,
        Some(root_path.to_vec()),
        sensitive_secrets,
    ));
    finish_failed_root(root_is_non_null, ctx, root_cp, root_path, rows_observed)
}

/// Borrowed execution context passed into [`consume_and_project_fetch`].
pub(crate) struct FetchAssemblyInput<'a> {
    pub(crate) snapshot: &'a Snapshot,
    pub(crate) fetch: &'a Fetch,
    pub(crate) root_field_ty: &'a Type,
    pub(crate) variables: &'a serde_json::Map<String, serde_json::Value>,
    pub(crate) sensitive_secrets: &'a [&'a str],
    pub(crate) redactor: &'a dyn ResponseRedactor,
    pub(crate) budget: &'a ResponseSizeBudget,
}

/// Incrementally consumes a branch `QueryResponse` stream for `fetch`, projecting rows according
/// to `snapshot.schema` while enforcing `max_response_bytes` on the fly (`RUN-060`, `GQL-051`).
pub(crate) async fn consume_and_project_fetch(
    input: FetchAssemblyInput<'_>,
    mut stream: BoxStream<QueryResponse>,
) -> FetchAssemblyOutcome {
    let FetchAssemblyInput {
        snapshot,
        fetch,
        root_field_ty,
        variables,
        sensitive_secrets,
        redactor,
        budget,
    } = input;
    let mut ctx = AssemblyCtx {
        snapshot,
        variables,
        redactor,
        tracker: LocalSizeTracker::new(budget),
        errors: Vec::new(),
    };
    let root_cp = ctx.tracker.checkpoint();
    let root_is_non_null = root_field_ty.is_non_null();
    let root_path = vec![PathSegment::Field(fetch.response_key.clone())];
    let sel_refs: Vec<&SelectionField> = fetch.selection.iter().collect();
    let mut rows_observed: u64 = 0;

    let query_type_name = snapshot
        .schema
        .root_operation(apollo_compiler::ast::OperationType::Query)
        .map_or("Query", |n| n.as_str());

    match root_field_ty {
        Type::List(elem_ty) | Type::NonNullList(elem_ty) => {
            if let Err(error) = ctx.tracker.try_reserve(2, &root_path) {
                return FetchAssemblyOutcome::TooLarge {
                    error,
                    rows_observed,
                };
            }
            let mut items: Vec<serde_json::Value> = Vec::new();
            let mut path = root_path.clone();
            let mut chunks_polled: usize = 0;

            while let Some(chunk_res) = stream.next().await {
                chunks_polled = chunks_polled.saturating_add(1);
                if chunks_polled.is_multiple_of(32) {
                    tokio::task::yield_now().await;
                }
                let resp = match chunk_res {
                    Ok(r) => r,
                    Err(ngc_err) => {
                        drop(stream);
                        return record_ngc_root_failure(
                            &ngc_err,
                            root_is_non_null,
                            sensitive_secrets,
                            &mut ctx,
                            root_cp,
                            &root_path,
                            rows_observed,
                        );
                    }
                };

                match resp.chunk {
                    Some(query_response::Chunk::Rows(row_chunk)) => {
                        if let Some(ref proto_err) = row_chunk.error {
                            let info = ngc_error_from_proto(proto_err);
                            drop(stream);
                            return record_ngc_root_failure(
                                &info,
                                root_is_non_null,
                                sensitive_secrets,
                                &mut ctx,
                                root_cp,
                                &root_path,
                                rows_observed,
                            );
                        }

                        for row_st in &row_chunk.rows {
                            rows_observed = rows_observed.saturating_add(1);
                            let idx = items.len();
                            path.push(PathSegment::Index(idx));
                            if idx > 0
                                && let Err(error) = ctx.tracker.try_reserve(1, &path)
                            {
                                drop(stream);
                                return FetchAssemblyOutcome::TooLarge {
                                    error,
                                    rows_observed,
                                };
                            }

                            let item_res = match &**elem_ty {
                                Type::Named(tname) | Type::NonNullNamed(tname)
                                    if !is_composite_schema_type(
                                        &snapshot.schema,
                                        tname.as_str(),
                                    ) =>
                                {
                                    let elem_cp = ctx.tracker.checkpoint();
                                    let elem_non_null = elem_ty.is_non_null();
                                    match ctx.extract_root_scalar_from_row(
                                        row_st,
                                        fetch,
                                        tname.as_str(),
                                        elem_non_null,
                                        &path,
                                    ) {
                                        Ok(v) => Ok(v),
                                        Err(ProjErr::NullBubble) if !elem_non_null => {
                                            ctx.tracker.rollback_to(elem_cp);
                                            ctx.tracker
                                                .try_reserve(4, &path)
                                                .map(|()| serde_json::Value::Null)
                                                .map_err(ProjErr::TooLarge)
                                        }
                                        Err(e) => Err(e),
                                    }
                                }
                                _ => {
                                    let wrapped = prost_types::Value {
                                        kind: Some(prost_types::value::Kind::StructValue(
                                            row_st.clone(),
                                        )),
                                    };
                                    ctx.project_typed_value(
                                        Some(&wrapped),
                                        elem_ty,
                                        &sel_refs,
                                        &mut path,
                                    )
                                }
                            };
                            path.pop();

                            match item_res {
                                Ok(v) => items.push(v),
                                Err(ProjErr::NullBubble) => {
                                    drop(stream);
                                    return finish_failed_root(
                                        root_is_non_null,
                                        &mut ctx,
                                        root_cp,
                                        &root_path,
                                        rows_observed,
                                    );
                                }
                                Err(ProjErr::TooLarge(error)) => {
                                    drop(stream);
                                    return FetchAssemblyOutcome::TooLarge {
                                        error,
                                        rows_observed,
                                    };
                                }
                                Err(ProjErr::Validation(err)) => {
                                    drop(stream);
                                    ctx.errors.push(err);
                                    return FetchAssemblyOutcome::PropagateTopNull {
                                        errors: ctx.errors,
                                        rows_observed,
                                    };
                                }
                            }
                        }
                    }
                    Some(query_response::Chunk::Complete(_)) => break,
                    None => {}
                }
            }
            drop(stream);

            let arr_val = serde_json::Value::Array(items);
            let final_val = apply_root_redaction(
                &mut ctx,
                query_type_name,
                fetch,
                root_is_non_null,
                root_cp,
                &root_path,
                arr_val,
            );
            match final_val {
                Ok(value) => FetchAssemblyOutcome::Field {
                    value,
                    errors: ctx.errors,
                    rows_observed,
                },
                Err(ProjErr::NullBubble) => FetchAssemblyOutcome::PropagateTopNull {
                    errors: ctx.errors,
                    rows_observed,
                },
                Err(ProjErr::TooLarge(error) | ProjErr::Validation(error)) => {
                    FetchAssemblyOutcome::TooLarge {
                        error,
                        rows_observed,
                    }
                }
            }
        }
        Type::Named(type_name) | Type::NonNullNamed(type_name) => {
            let tname = type_name.as_str();
            let mut first_projected: Option<serde_json::Value> = None;
            let mut chunks_polled: usize = 0;

            while let Some(chunk_res) = stream.next().await {
                chunks_polled = chunks_polled.saturating_add(1);
                if chunks_polled.is_multiple_of(32) {
                    tokio::task::yield_now().await;
                }
                let resp = match chunk_res {
                    Ok(r) => r,
                    Err(ngc_err) => {
                        drop(stream);
                        return record_ngc_root_failure(
                            &ngc_err,
                            root_is_non_null,
                            sensitive_secrets,
                            &mut ctx,
                            root_cp,
                            &root_path,
                            rows_observed,
                        );
                    }
                };

                match resp.chunk {
                    Some(query_response::Chunk::Rows(row_chunk)) => {
                        if let Some(ref proto_err) = row_chunk.error {
                            let info = ngc_error_from_proto(proto_err);
                            drop(stream);
                            return record_ngc_root_failure(
                                &info,
                                root_is_non_null,
                                sensitive_secrets,
                                &mut ctx,
                                root_cp,
                                &root_path,
                                rows_observed,
                            );
                        }

                        for row_st in &row_chunk.rows {
                            rows_observed = rows_observed.saturating_add(1);
                            if first_projected.is_some() {
                                continue;
                            }

                            let mut path = root_path.clone();
                            let proj_res = if is_composite_schema_type(&snapshot.schema, tname) {
                                ctx.project_object_struct(row_st, tname, &sel_refs, &mut path)
                            } else {
                                ctx.extract_root_scalar_from_row(
                                    row_st,
                                    fetch,
                                    tname,
                                    root_is_non_null,
                                    &path,
                                )
                            };

                            match proj_res {
                                Ok(v) => first_projected = Some(v),
                                Err(ProjErr::NullBubble) => {
                                    drop(stream);
                                    return finish_failed_root(
                                        root_is_non_null,
                                        &mut ctx,
                                        root_cp,
                                        &root_path,
                                        rows_observed,
                                    );
                                }
                                Err(ProjErr::TooLarge(error)) => {
                                    drop(stream);
                                    return FetchAssemblyOutcome::TooLarge {
                                        error,
                                        rows_observed,
                                    };
                                }
                                Err(ProjErr::Validation(err)) => {
                                    drop(stream);
                                    ctx.errors.push(err);
                                    return FetchAssemblyOutcome::PropagateTopNull {
                                        errors: ctx.errors,
                                        rows_observed,
                                    };
                                }
                            }
                        }
                    }
                    Some(query_response::Chunk::Complete(_)) => break,
                    None => {}
                }
            }
            drop(stream);

            let resolved_val = if let Some(val) = first_projected {
                val
            } else if root_is_non_null {
                ctx.errors.push(
                    GraphqlError::new(
                        ErrorCode::BranchError,
                        format!(
                            "non-null root field `{}` resolved to null",
                            fetch.response_key
                        ),
                    )
                    .with_path(root_path.clone()),
                );
                return finish_failed_root(
                    root_is_non_null,
                    &mut ctx,
                    root_cp,
                    &root_path,
                    rows_observed,
                );
            } else {
                if let Err(error) = ctx.tracker.try_reserve(4, &root_path) {
                    return FetchAssemblyOutcome::TooLarge {
                        error,
                        rows_observed,
                    };
                }
                serde_json::Value::Null
            };

            let final_val = apply_root_redaction(
                &mut ctx,
                query_type_name,
                fetch,
                root_is_non_null,
                root_cp,
                &root_path,
                resolved_val,
            );
            match final_val {
                Ok(value) => FetchAssemblyOutcome::Field {
                    value,
                    errors: ctx.errors,
                    rows_observed,
                },
                Err(ProjErr::NullBubble) => FetchAssemblyOutcome::PropagateTopNull {
                    errors: ctx.errors,
                    rows_observed,
                },
                Err(ProjErr::TooLarge(error) | ProjErr::Validation(error)) => {
                    FetchAssemblyOutcome::TooLarge {
                        error,
                        rows_observed,
                    }
                }
            }
        }
    }
}

fn apply_root_redaction(
    ctx: &mut AssemblyCtx<'_>,
    query_type_name: &str,
    fetch: &Fetch,
    root_is_non_null: bool,
    root_cp: usize,
    root_path: &[PathSegment],
    value: serde_json::Value,
) -> Result<serde_json::Value, ProjErr> {
    let _ = collect_merged_selections;
    match ctx.redactor.redact_field(
        ctx.snapshot,
        query_type_name,
        &fetch.operation,
        root_path,
        value,
    ) {
        Some(redacted) => {
            ctx.tracker.rollback_to(root_cp);
            let new_len = json_value_compact_len(&redacted);
            ctx.tracker
                .try_reserve(new_len, root_path)
                .map_err(ProjErr::TooLarge)?;
            Ok(redacted)
        }
        None => {
            ctx.tracker.rollback_to(root_cp);
            if root_is_non_null {
                ctx.errors.push(
                    GraphqlError::new(
                        ErrorCode::Forbidden,
                        format!("non-null root field `{}` redacted", fetch.response_key),
                    )
                    .with_path(root_path.to_vec()),
                );
                Err(ProjErr::NullBubble)
            } else {
                ctx.tracker
                    .try_reserve(4, root_path)
                    .map_err(ProjErr::TooLarge)?;
                Ok(serde_json::Value::Null)
            }
        }
    }
}

/// Reserves a failed root fetch's outcome when the fetch failed before opening a stream (e.g.
/// expired deadline or saturated bulkhead).
pub(crate) fn pre_stream_failed_fetch_outcome(
    error: GraphqlError,
    root_is_non_null: bool,
    root_path: &[PathSegment],
    budget: &ResponseSizeBudget,
) -> FetchAssemblyOutcome {
    if root_is_non_null {
        FetchAssemblyOutcome::PropagateTopNull {
            errors: vec![error],
            rows_observed: 0,
        }
    } else if budget.try_reserve_global(4) {
        FetchAssemblyOutcome::Field {
            value: serde_json::Value::Null,
            errors: vec![error],
            rows_observed: 0,
        }
    } else {
        FetchAssemblyOutcome::TooLarge {
            error: too_large_error(budget.cap, root_path),
            rows_observed: 0,
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use serde_json::json;

    use super::*;

    // spec: RUN-060
    #[test]
    fn compact_json_byte_counter_matches_serde_json_to_vec_exactly() {
        let samples = vec![
            json!(null),
            json!(true),
            json!(false),
            json!(0),
            json!(-42),
            json!(15_000),
            json!(3.25),
            json!(""),
            json!("ascii"),
            json!("quotes \" backslash \\ newline \n tab \t bell \u{0007} nul \u{0000}"),
            json!("Tiếng Việt & λ & 🚀"),
            json!([1, "two", null, false, {"k": "v"}]),
            json!({
                "orders": [
                    {"id": "o-1", "sku": "ABC", "totalCents": 42},
                    {"id": "o-2", "sku": "XYZ", "totalCents": 99}
                ],
                "absentCustomer": null
            }),
        ];

        for sample in samples {
            let expected = serde_json::to_vec(&sample).expect("serialize").len();
            assert_eq!(
                json_value_compact_len(&sample),
                expected,
                "byte mismatch for {sample:?}"
            );
        }
    }
}
