//! Remote predicate, sort, projection, and limit pushdown planning (`WP-M3-02`).
//!
//! Spec references: `RUN-010`, `RUN-012`, `RUN-013`, `RUN-023`, `NGC-023`.

use std::collections::BTreeSet;

use gap_core::error::{ErrorCode, GapError, GapResult};
use gap_core::snapshot::Capabilities;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::ir::Pushdown;

/// Default maximum row budget allowed when pushdown falls back to local execution.
pub const DEFAULT_FALLBACK_ROWS: u64 = 10_000;

/// Default maximum number of distinct join keys encoded into a single remote predicate batch.
pub const DEFAULT_MAX_PUSHDOWN_KEYS: usize = 5_000;

/// Structured filter predicate that can be negotiated for remote pushdown.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PredicateExpr {
    /// Equality predicate (`field == value`).
    Eq { field: String, value: Value },
    /// Set membership predicate (`field IN values`).
    In { field: String, values: Vec<Value> },
    /// Binary comparison predicate (`GT`, `GTE`, `LT`, `LTE`, `NE`).
    Comparison {
        op: String,
        field: String,
        value: Value,
    },
    /// Logical conjunction (`AND`).
    And(Vec<PredicateExpr>),
    /// Logical disjunction (`OR`).
    Or(Vec<PredicateExpr>),
    /// Logical negation (`NOT`).
    Not(Box<PredicateExpr>),
}

impl PredicateExpr {
    /// Returns the normalized uppercase predicate operator names referenced by this expression.
    #[must_use]
    pub fn op_names(&self) -> BTreeSet<String> {
        let mut ops = BTreeSet::new();
        self.collect_op_names(&mut ops);
        ops
    }

    fn collect_op_names(&self, ops: &mut BTreeSet<String>) {
        match self {
            Self::Eq { .. } => {
                ops.insert("EQ".to_owned());
            }
            Self::In { .. } => {
                ops.insert("IN".to_owned());
            }
            Self::Comparison { op, .. } => {
                ops.insert(normalize_op_name(op));
            }
            Self::And(items) => {
                ops.insert("AND".to_owned());
                for item in items {
                    item.collect_op_names(ops);
                }
            }
            Self::Or(items) => {
                ops.insert("OR".to_owned());
                for item in items {
                    item.collect_op_names(ops);
                }
            }
            Self::Not(inner) => {
                ops.insert("NOT".to_owned());
                inner.collect_op_names(ops);
            }
        }
    }

    /// Serializes the predicate into a deterministic canonical JSON representation (`RUN-010`).
    #[must_use]
    pub fn to_canonical_json(&self) -> Value {
        let mut obj = Map::new();
        match self {
            Self::Eq { field, value } => {
                obj.insert("field".to_owned(), Value::String(field.clone()));
                obj.insert("op".to_owned(), Value::String("EQ".to_owned()));
                obj.insert("value".to_owned(), canonicalize_json(value));
            }
            Self::In { field, values } => {
                let canonical_values = dedup_and_sort_json_values(values);
                obj.insert("field".to_owned(), Value::String(field.clone()));
                obj.insert("op".to_owned(), Value::String("IN".to_owned()));
                obj.insert("values".to_owned(), Value::Array(canonical_values));
            }
            Self::Comparison { op, field, value } => {
                obj.insert("field".to_owned(), Value::String(field.clone()));
                obj.insert("op".to_owned(), Value::String(normalize_op_name(op)));
                obj.insert("value".to_owned(), canonicalize_json(value));
            }
            Self::And(items) => {
                let mut children: Vec<(String, Value)> = items
                    .iter()
                    .map(|item| {
                        let v = item.to_canonical_json();
                        (canonical_json_key(&v), v)
                    })
                    .collect();
                children.sort_by(|a, b| a.0.cmp(&b.0));
                obj.insert(
                    "args".to_owned(),
                    Value::Array(children.into_iter().map(|(_, v)| v).collect()),
                );
                obj.insert("op".to_owned(), Value::String("AND".to_owned()));
            }
            Self::Or(items) => {
                let mut children: Vec<(String, Value)> = items
                    .iter()
                    .map(|item| {
                        let v = item.to_canonical_json();
                        (canonical_json_key(&v), v)
                    })
                    .collect();
                children.sort_by(|a, b| a.0.cmp(&b.0));
                obj.insert(
                    "args".to_owned(),
                    Value::Array(children.into_iter().map(|(_, v)| v).collect()),
                );
                obj.insert("op".to_owned(), Value::String("OR".to_owned()));
            }
            Self::Not(inner) => {
                obj.insert("arg".to_owned(), inner.to_canonical_json());
                obj.insert("op".to_owned(), Value::String("NOT".to_owned()));
            }
        }
        Value::Object(obj)
    }
}

/// Pushdown capabilities declared or derived for a target branch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PushdownCapabilities {
    pub supports_limit: bool,
    pub supports_order_by: bool,
    pub supports_projection: bool,
    pub predicate_ops: BTreeSet<String>,
    pub max_fallback_rows: u64,
}

impl Default for PushdownCapabilities {
    fn default() -> Self {
        Self {
            supports_limit: false,
            supports_order_by: false,
            supports_projection: false,
            predicate_ops: BTreeSet::new(),
            max_fallback_rows: DEFAULT_FALLBACK_ROWS,
        }
    }
}

impl PushdownCapabilities {
    /// Derives pushdown capabilities from a snapshot `BranchDescriptor.capabilities`.
    #[must_use]
    pub fn from_snapshot_capabilities(caps: &Capabilities) -> Self {
        let mut predicate_ops = BTreeSet::new();
        for op in &caps.predicate_ops {
            predicate_ops.insert(normalize_op_name(op));
        }
        let supports_limit = predicate_ops.contains("LIMIT");
        let supports_order_by =
            predicate_ops.contains("ORDER_BY") || predicate_ops.contains("SORT");
        let supports_projection =
            predicate_ops.contains("PROJECTION") || predicate_ops.contains("SELECT");

        Self {
            supports_limit,
            supports_order_by,
            supports_projection,
            predicate_ops,
            max_fallback_rows: DEFAULT_FALLBACK_ROWS,
        }
    }

    /// Enables or disables remote `LIMIT` pushdown support.
    #[must_use]
    pub const fn with_limit(mut self, enabled: bool) -> Self {
        self.supports_limit = enabled;
        self
    }

    /// Enables or disables remote `ORDER BY` pushdown support.
    #[must_use]
    pub const fn with_order_by(mut self, enabled: bool) -> Self {
        self.supports_order_by = enabled;
        self
    }

    /// Enables or disables remote field-projection pushdown support.
    #[must_use]
    pub const fn with_projection(mut self, enabled: bool) -> Self {
        self.supports_projection = enabled;
        self
    }

    /// Overrides the bounded fallback row cap.
    #[must_use]
    pub const fn with_max_fallback_rows(mut self, max_rows: u64) -> Self {
        self.max_fallback_rows = max_rows;
        self
    }

    /// Inserts an allowed predicate operator name (normalized to uppercase).
    #[must_use]
    pub fn with_predicate_op(mut self, op: impl AsRef<str>) -> Self {
        self.predicate_ops.insert(normalize_op_name(op.as_ref()));
        self
    }

    /// Returns true if all operators used by `pred` are supported by the remote branch.
    #[must_use]
    pub fn supports_predicate(&self, pred: &PredicateExpr) -> bool {
        pred.op_names()
            .into_iter()
            .all(|op| self.predicate_ops.contains(&op))
    }
}

/// Caller's desired pushdown operations on a remote fetch.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequestedPushdown {
    pub limit: Option<u64>,
    pub order_by: Vec<String>,
    pub predicate: Option<PredicateExpr>,
    pub projection: BTreeSet<String>,
}

/// Diagnostic recorded when a requested pushdown cannot execute natively on the remote branch
/// and must fall back to bounded local execution (`GAP_PUSHDOWN_FALLBACK`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PushdownWarning {
    pub code: ErrorCode,
    pub aspect: String,
    pub detail: String,
    pub bounded_rows: u64,
}

/// Output of pushdown negotiation splitting remote pushdown from local fallback steps.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NegotiatedPushdown {
    pub remote: Pushdown,
    pub local_limit: Option<u64>,
    pub local_order_by: Vec<String>,
    pub local_predicate: Option<PredicateExpr>,
    pub local_projection: BTreeSet<String>,
    pub remote_fetch_cap: Option<u64>,
    pub warnings: Vec<PushdownWarning>,
}

/// Negotiates which parts of `requested` execute on the remote branch vs bounded local fallback.
///
/// Spec (`RUN-012`, `NGC-023`):
/// - Predicate, sort, projection, and limit are pushed down only when the remote branch declares
///   support for them.
/// - If a predicate or `order_by` falls back locally, `limit` must NOT run prematurely on the
///   remote branch (that would drop matching/top rows before local filtering/sorting); instead,
///   `limit` runs locally and remote fetching is bounded by `caps.max_fallback_rows`.
/// - If any fallback is needed and `caps.max_fallback_rows == 0` (or an explicit `requested.limit`
///   exceeds `caps.max_fallback_rows` during fallback), `GAP_PUSHDOWN_FALLBACK` is returned as a
///   typed error rather than performing an unbounded fetch.
pub fn negotiate_pushdown(
    requested: &RequestedPushdown,
    caps: &PushdownCapabilities,
) -> GapResult<NegotiatedPushdown> {
    let mut warnings = Vec::new();

    // 1. Predicate pushdown
    let (remote_predicate, local_predicate) = match &requested.predicate {
        Some(pred) if caps.supports_predicate(pred) => (Some(pred.to_canonical_json()), None),
        Some(pred) => {
            warnings.push(PushdownWarning {
                code: ErrorCode::PushdownFallback,
                aspect: "predicate".to_owned(),
                detail: format!(
                    "remote branch lacks predicate ops {:?}; evaluating predicate locally within {} rows",
                    pred.op_names(),
                    caps.max_fallback_rows
                ),
                bounded_rows: caps.max_fallback_rows,
            });
            (None, Some(pred.clone()))
        }
        None => (None, None),
    };

    // 2. Order-by pushdown
    let (remote_order_by, local_order_by) = if requested.order_by.is_empty() {
        (Vec::new(), Vec::new())
    } else if caps.supports_order_by {
        (requested.order_by.clone(), Vec::new())
    } else {
        warnings.push(PushdownWarning {
            code: ErrorCode::PushdownFallback,
            aspect: "order_by".to_owned(),
            detail: format!(
                "remote branch does not support order_by {:?}; sorting locally within {} rows",
                requested.order_by, caps.max_fallback_rows
            ),
            bounded_rows: caps.max_fallback_rows,
        });
        (Vec::new(), requested.order_by.clone())
    };

    // 3. Projection pushdown (local selection always trims extraneous fields deterministically;
    // emit warning only when projection was requested without remote support).
    let local_projection = if requested.projection.is_empty() || caps.supports_projection {
        BTreeSet::new()
    } else {
        warnings.push(PushdownWarning {
            code: ErrorCode::PushdownFallback,
            aspect: "projection".to_owned(),
            detail: format!(
                "remote branch does not support field projection; projecting {} fields locally",
                requested.projection.len()
            ),
            bounded_rows: caps.max_fallback_rows,
        });
        requested.projection.clone()
    };

    // 4. Limit pushdown (depends on whether filter or sort fell back locally)
    let blocks_remote_limit = local_predicate.is_some() || !local_order_by.is_empty();
    let mut needs_bounded_cap = blocks_remote_limit;

    let (remote_limit, local_limit) = match requested.limit {
        Some(limit) if !blocks_remote_limit && caps.supports_limit => (Some(limit), None),
        Some(limit) if blocks_remote_limit => {
            // Do not push the caller's small limit before local filter/sort; fetch bounded rows instead.
            warnings.push(PushdownWarning {
                code: ErrorCode::PushdownFallback,
                aspect: "limit".to_owned(),
                detail: format!(
                    "deferring limit({limit}) to local execution because filter/order_by runs locally"
                ),
                bounded_rows: caps.max_fallback_rows,
            });
            let cap = caps.supports_limit.then_some(caps.max_fallback_rows);
            (cap, Some(limit))
        }
        Some(limit) => {
            // Remote branch does not support LIMIT at all -> local slice bounded by max_fallback_rows.
            needs_bounded_cap = true;
            warnings.push(PushdownWarning {
                code: ErrorCode::PushdownFallback,
                aspect: "limit".to_owned(),
                detail: format!(
                    "remote branch does not support limit({limit}); enforcing local limit within {} rows",
                    caps.max_fallback_rows
                ),
                bounded_rows: caps.max_fallback_rows,
            });
            (None, Some(limit))
        }
        None => (None, None),
    };

    if needs_bounded_cap {
        if caps.max_fallback_rows == 0 {
            return Err(GapError::new(
                ErrorCode::PushdownFallback,
                "pushdown fallback required, but max_fallback_rows is 0 (unbounded fetch forbidden)",
            ));
        }
        if let Some(limit) = requested.limit
            && limit > caps.max_fallback_rows
        {
            return Err(GapError::new(
                ErrorCode::PushdownFallback,
                format!(
                    "requested limit {limit} exceeds bounded fallback cap {}",
                    caps.max_fallback_rows
                ),
            ));
        }
    }

    let remote_fetch_cap = if local_predicate.is_some()
        || !local_order_by.is_empty()
        || (requested.limit.is_some() && !caps.supports_limit)
    {
        Some(caps.max_fallback_rows)
    } else {
        None
    };

    Ok(NegotiatedPushdown {
        remote: Pushdown {
            limit: remote_limit,
            order_by: remote_order_by,
            predicate: remote_predicate,
        },
        local_limit,
        local_order_by,
        local_predicate,
        local_projection,
        remote_fetch_cap,
        warnings,
    })
}

/// Plans chunked remote `IN` (or `EQ` for singletons when `IN` is absent) predicates for a set of
/// extracted join keys (`RUN-023`).
///
/// Keys are deduplicated and canonically sorted so plans and their hashes stay deterministic
/// regardless of input row order (`RUN-010`).
pub fn plan_remote_predicate_keys(
    target_field: &str,
    keys: &[Value],
    max_keys_per_batch: usize,
    caps: &PushdownCapabilities,
) -> GapResult<Vec<Pushdown>> {
    if keys.is_empty() {
        return Ok(Vec::new());
    }
    if max_keys_per_batch == 0 {
        return Err(GapError::new(
            ErrorCode::ValidationError,
            "max_keys_per_batch must be greater than zero",
        ));
    }

    let canonical_keys = dedup_and_sort_json_values(keys);
    let supports_in = caps.predicate_ops.contains("IN");
    let supports_eq = caps.predicate_ops.contains("EQ");

    if !supports_in && !(supports_eq && canonical_keys.len() == 1) {
        return Err(GapError::new(
            ErrorCode::PushdownFallback,
            format!(
                "remote branch cannot accept predicate pushdown for field '{target_field}' (predicate_ops={:?})",
                caps.predicate_ops
            ),
        ));
    }

    let mut batches = Vec::new();
    for chunk in canonical_keys.chunks(max_keys_per_batch) {
        let expr = if !supports_in && supports_eq && chunk.len() == 1 {
            PredicateExpr::Eq {
                field: target_field.to_owned(),
                value: chunk[0].clone(),
            }
        } else {
            PredicateExpr::In {
                field: target_field.to_owned(),
                values: chunk.to_vec(),
            }
        };
        batches.push(Pushdown {
            limit: None,
            order_by: Vec::new(),
            predicate: Some(expr.to_canonical_json()),
        });
    }

    Ok(batches)
}

fn normalize_op_name(raw: &str) -> String {
    match raw.trim().to_ascii_uppercase().as_str() {
        "=" | "==" | "EQ" => "EQ".to_owned(),
        "!=" | "<>" | "NE" | "NEQ" => "NE".to_owned(),
        ">" | "GT" => "GT".to_owned(),
        ">=" | "GTE" => "GTE".to_owned(),
        "<" | "LT" => "LT".to_owned(),
        "<=" | "LTE" => "LTE".to_owned(),
        other => other.to_owned(),
    }
}

fn canonicalize_json(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut sorted = Map::new();
            let mut entries: Vec<(&String, &Value)> = map.iter().collect();
            entries.sort_by(|a, b| a.0.cmp(b.0));
            for (k, v) in entries {
                sorted.insert(k.clone(), canonicalize_json(v));
            }
            Value::Object(sorted)
        }
        Value::Array(arr) => Value::Array(arr.iter().map(canonicalize_json).collect()),
        other => other.clone(),
    }
}

fn canonical_json_key(value: &Value) -> String {
    serde_json::to_string(&canonicalize_json(value)).unwrap_or_default()
}

fn dedup_and_sort_json_values(values: &[Value]) -> Vec<Value> {
    let mut keyed: Vec<(String, Value)> = values
        .iter()
        .map(|v| {
            let canon = canonicalize_json(v);
            (serde_json::to_string(&canon).unwrap_or_default(), canon)
        })
        .collect();
    keyed.sort_by(|a, b| a.0.cmp(&b.0));
    keyed.dedup_by(|a, b| a.0 == b.0);
    keyed.into_iter().map(|(_, v)| v).collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use serde_json::json;

    // spec: RUN-012, NGC-023
    #[test]
    fn pushdown_negotiation_offloads_when_all_capabilities_present() {
        let caps = PushdownCapabilities::default()
            .with_limit(true)
            .with_order_by(true)
            .with_projection(true)
            .with_predicate_op("EQ")
            .with_predicate_op("AND");
        let req = RequestedPushdown {
            limit: Some(25),
            order_by: vec!["created_at DESC".to_owned()],
            predicate: Some(PredicateExpr::And(vec![PredicateExpr::Eq {
                field: "status".to_owned(),
                value: json!("paid"),
            }])),
            projection: ["id".to_owned(), "total".to_owned()].into_iter().collect(),
        };

        let negotiated = negotiate_pushdown(&req, &caps).expect("pushdown should succeed");
        assert!(negotiated.warnings.is_empty());
        assert_eq!(negotiated.remote.limit, Some(25));
        assert_eq!(negotiated.remote.order_by, vec!["created_at DESC"]);
        assert!(negotiated.remote.predicate.is_some());
        assert_eq!(negotiated.local_limit, None);
        assert!(negotiated.local_order_by.is_empty());
        assert!(negotiated.local_predicate.is_none());
        assert_eq!(negotiated.remote_fetch_cap, None);
    }

    // spec: RUN-012, NGC-023
    #[test]
    fn pushdown_fallback_defers_limit_when_predicate_not_supported_remotely() {
        let caps = PushdownCapabilities::default()
            .with_limit(true)
            .with_order_by(true)
            .with_max_fallback_rows(500);
        let req = RequestedPushdown {
            limit: Some(20),
            order_by: vec!["id ASC".to_owned()],
            predicate: Some(PredicateExpr::Comparison {
                op: ">".to_owned(),
                field: "amount".to_owned(),
                value: json!(100),
            }),
            projection: BTreeSet::new(),
        };

        let negotiated = negotiate_pushdown(&req, &caps).expect("bounded fallback should succeed");
        // Limit MUST NOT drop rows before local filtering; remote fetch uses bounded fallback cap.
        assert_eq!(negotiated.remote.limit, Some(500));
        assert_eq!(negotiated.local_limit, Some(20));
        assert!(negotiated.local_predicate.is_some());
        assert_eq!(negotiated.remote_fetch_cap, Some(500));
        assert_eq!(negotiated.warnings.len(), 2);
        assert!(
            negotiated
                .warnings
                .iter()
                .all(|w| w.code == ErrorCode::PushdownFallback)
        );
    }

    // spec: RUN-012
    #[test]
    fn pushdown_fallback_rejects_limit_above_bounded_cap() {
        let caps = PushdownCapabilities::default()
            .with_limit(false)
            .with_max_fallback_rows(100);
        let req = RequestedPushdown {
            limit: Some(250),
            ..Default::default()
        };

        let err = negotiate_pushdown(&req, &caps)
            .expect_err("requesting more than bounded fallback cap must fail");
        assert_eq!(err.code(), ErrorCode::PushdownFallback);
    }

    // spec: RUN-023, RUN-010
    #[test]
    fn pushdown_remote_predicate_keys_are_canonical_deduped_and_chunked() {
        let caps = PushdownCapabilities::default().with_predicate_op("IN");
        let keys_a = vec![json!("u3"), json!("u1"), json!("u2"), json!("u1")];
        let keys_b = vec![json!("u2"), json!("u3"), json!("u1")];

        let plan_a = plan_remote_predicate_keys("id", &keys_a, 2, &caps)
            .expect("chunked IN predicate should succeed");
        let plan_b = plan_remote_predicate_keys("id", &keys_b, 2, &caps)
            .expect("chunked IN predicate should succeed");

        assert_eq!(
            plan_a, plan_b,
            "pushdown batches must be input-order independent"
        );
        assert_eq!(plan_a.len(), 2);
        assert_eq!(
            plan_a[0].predicate,
            Some(json!({"field": "id", "op": "IN", "values": ["u1", "u2"]}))
        );
        assert_eq!(
            plan_a[1].predicate,
            Some(json!({"field": "id", "op": "IN", "values": ["u3"]}))
        );
    }

    // spec: RUN-012
    #[test]
    fn pushdown_derives_flags_from_snapshot_capabilities() {
        let snap_caps = Capabilities {
            foreach_batch: true,
            predicate_ops: vec!["eq".to_owned(), "LIMIT".to_owned(), "sort".to_owned()],
            ..Capabilities::default()
        };
        let derived = PushdownCapabilities::from_snapshot_capabilities(&snap_caps);
        assert!(derived.supports_limit);
        assert!(derived.supports_order_by);
        assert!(!derived.supports_projection);
        assert!(derived.predicate_ops.contains("EQ"));
    }
}
