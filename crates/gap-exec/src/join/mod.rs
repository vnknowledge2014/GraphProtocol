//! Batched `foreach` & remote-predicate join executor, stitching, limits, partial-failure
//! isolation, cursor verification, and `asOf` consistency (`WP-M3-03`).
//!
//! Spec references: `RUN-020`, `RUN-021`, `RUN-022`, `RUN-023`, `RUN-024`, `RUN-025`, `RUN-026`,
//! `INV-18`.

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use gap_core::error::{ErrorCode, GapError, GapResult, PathSegment, RetryInfo};
use gap_core::ids::TenantId;
use gap_plan::ir::{JoinCardinality, JoinKeyPair};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

use crate::bulkhead::BranchBulkhead;
use crate::exec::error::{GraphqlError, GraphqlErrorExtensions};

/// Default planner batch cap used when sizing `foreach` chunks (`RUN-020`).
pub const DEFAULT_PLAN_MAX_BATCH: usize = 1_000;

/// Default maximum total RHS rows stitched per join node (`RUN-022`).
pub const DEFAULT_MAX_JOIN_ROWS: usize = 100_000;

/// Default maximum unique keys permitted in a single join node (`RUN-022`).
pub const DEFAULT_MAX_JOIN_KEYS: usize = 50_000;

/// Default maximum keys encoded directly into a remote `IN` predicate (`RUN-023`).
pub const DEFAULT_MAX_PUSHDOWN_KEYS: usize = 5_000;

/// Behaviour when a `ONE_TO_ONE` or `MANY_TO_ONE` join receives multiple RHS rows for one key
/// (`RUN-021`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum DuplicateKeyPolicy {
    /// Fail that key with `GAP_JOIN_KEY_NOT_UNIQUE` (default, `RUN-021`).
    #[default]
    Error,
    /// Keep the first RHS row encountered for that key (`join.on_duplicate = FIRST`).
    First,
}

/// Failure handling strategy when an upstream batch chunk errors (`RUN-024`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum JoinErrorMode {
    /// Null only the joined field for LHS rows in failed chunks and mark `partial = true` (`RUN-024`).
    #[default]
    Partial,
    /// Abort the join immediately on the first chunk error (`@budget(failFast: true)`).
    FailFast,
}

/// Execution parameters controlling a single join node execution (`RUN-020..026`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JoinExecConfig {
    /// Target branch name (used for diagnostics and `extensions.gap.asOf`).
    pub target_branch: String,
    /// LHS field name where stitched results are written on each LHS object row.
    pub response_key: String,
    /// Declared join cardinality (`OneToOne`, `ManyToOne`, `OneToMany`).
    pub cardinality: JoinCardinality,
    /// Key bindings from LHS rows (`from`) to RHS keys/arguments.
    pub keys: Vec<JoinKeyPair>,
    /// Branch-configured `max_batch_size`.
    pub branch_max_batch_size: usize,
    /// Plan-level max batch cap (`DEFAULT_PLAN_MAX_BATCH = 1_000`, `RUN-020`).
    pub plan_max_batch: usize,
    /// Branch `max_in_flight` limit used to cap concurrent chunk fanout (`max(1, max_in_flight/2)`).
    pub branch_max_in_flight: usize,
    /// Maximum stitched RHS rows across the join (`DEFAULT_MAX_JOIN_ROWS = 100_000`, `RUN-022`).
    pub max_join_rows: usize,
    /// Maximum unique LHS keys (`DEFAULT_MAX_JOIN_KEYS = 50_000`, `RUN-022`).
    pub max_join_keys: usize,
    /// Maximum keys pushed via remote `IN` predicate (`DEFAULT_MAX_PUSHDOWN_KEYS = 5_000`, `RUN-023`).
    pub max_pushdown_keys: usize,
    /// Cardinality duplicate handling (`Error` default vs `First`, `RUN-021`).
    pub on_duplicate: DuplicateKeyPolicy,
    /// Partial failure vs fail-fast mode (`RUN-024`).
    pub on_error: JoinErrorMode,
    /// Optional caller-requested `asOf` consistency timestamp/token (`RUN-026`).
    pub requested_as_of: Option<String>,
    /// Whether the target branch declares `capabilities.as_of` (`RUN-026`).
    pub branch_supports_as_of: bool,
    /// Whether missing `asOf` support aborts instead of warning (`@budget(strictAsOf: true)`).
    pub strict_as_of: bool,
}

impl JoinExecConfig {
    /// Builds a configuration with spec defaults for a given branch, response key, cardinality,
    /// and key mapping.
    #[must_use]
    pub fn with_defaults(
        target_branch: impl Into<String>,
        response_key: impl Into<String>,
        cardinality: JoinCardinality,
        keys: Vec<JoinKeyPair>,
    ) -> Self {
        Self {
            target_branch: target_branch.into(),
            response_key: response_key.into(),
            cardinality,
            keys,
            branch_max_batch_size: DEFAULT_PLAN_MAX_BATCH,
            plan_max_batch: DEFAULT_PLAN_MAX_BATCH,
            branch_max_in_flight: 16,
            max_join_rows: DEFAULT_MAX_JOIN_ROWS,
            max_join_keys: DEFAULT_MAX_JOIN_KEYS,
            max_pushdown_keys: DEFAULT_MAX_PUSHDOWN_KEYS,
            on_duplicate: DuplicateKeyPolicy::Error,
            on_error: JoinErrorMode::Partial,
            requested_as_of: None,
            branch_supports_as_of: true,
            strict_as_of: false,
        }
    }

    /// Effective chunk size `min(branch_max_batch_size, plan_max_batch)` (`RUN-020`).
    #[must_use]
    pub fn effective_chunk_size(&self) -> usize {
        self.branch_max_batch_size.min(self.plan_max_batch)
    }

    /// Chunk concurrency limit `max(1, branch_max_in_flight / 2)` (`RUN-020`).
    #[must_use]
    pub fn chunk_concurrency(&self) -> usize {
        (self.branch_max_in_flight / 2).max(1)
    }
}

/// Canonical, type-preserving key representation distinguishing integers from strings (`RUN-020`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CanonicalJoinKey {
    encoded: String,
    tuple: Vec<Value>,
}

impl Ord for CanonicalJoinKey {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.encoded.cmp(&other.encoded)
    }
}

impl PartialOrd for CanonicalJoinKey {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl std::hash::Hash for CanonicalJoinKey {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.encoded.hash(state);
    }
}

impl CanonicalJoinKey {
    /// Returns the raw tuple of extracted key JSON values.
    #[must_use]
    pub fn values(&self) -> &[Value] {
        &self.tuple
    }

    /// Returns the canonical typed encoding string.
    #[must_use]
    pub fn as_encoded(&self) -> &str {
        &self.encoded
    }

    /// Encodes a slice of key JSON values canonically, preserving number/string type distinctions.
    #[must_use]
    pub fn from_tuple(values: Vec<Value>) -> Self {
        let mut out = String::from("k:[");
        for (idx, val) in values.iter().enumerate() {
            if idx > 0 {
                out.push(',');
            }
            write_canonical_value(val, &mut out);
        }
        out.push(']');
        Self {
            encoded: out,
            tuple: values,
        }
    }
}

/// Encodes a single typed JSON value canonically so `42` and `"42"` never collide (`RUN-020`).
#[must_use]
pub fn encode_canonical_key_value(value: &Value) -> String {
    let mut out = String::new();
    write_canonical_value(value, &mut out);
    out
}

fn write_canonical_value(value: &Value, out: &mut String) {
    match value {
        Value::Null => out.push('n'),
        Value::Bool(b) => {
            out.push_str(if *b { "b:1" } else { "b:0" });
        }
        Value::Number(num) => {
            if let Some(i) = num.as_i64() {
                out.push_str("i:");
                out.push_str(&i.to_string());
            } else if let Some(u) = num.as_u64() {
                out.push_str("i:");
                out.push_str(&u.to_string());
            } else if let Some(f) = num.as_f64() {
                let normalized = if f == 0.0 { 0.0 } else { f };
                out.push_str("f:");
                out.push_str(&normalized.to_bits().to_string());
            } else {
                out.push_str("f:");
                out.push_str(&num.to_string());
            }
        }
        Value::String(s) => {
            out.push_str("s:");
            out.push_str(&serde_json::to_string(s).unwrap_or_default());
        }
        Value::Array(arr) => {
            out.push_str("a:[");
            for (idx, item) in arr.iter().enumerate() {
                if idx > 0 {
                    out.push(',');
                }
                write_canonical_value(item, out);
            }
            out.push(']');
        }
        Value::Object(map) => {
            let mut entries: Vec<(&String, &Value)> = map.iter().collect();
            entries.sort_by(|a, b| a.0.cmp(b.0));
            out.push_str("o:{");
            for (idx, (k, v)) in entries.iter().enumerate() {
                if idx > 0 {
                    out.push(',');
                }
                out.push_str(&serde_json::to_string(k).unwrap_or_default());
                out.push(':');
                write_canonical_value(v, out);
            }
            out.push('}');
        }
    }
}

/// Result returned by a single batched `Query{foreach}` upstream call (`RUN-020`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ForeachBatchRows {
    /// Rows already paired with their lookup key tuple `(key_tuple, rhs_row)`.
    Keyed(Vec<(Vec<Value>, Value)>),
    /// Flat RHS object rows indexed by `rhs_key_fields`.
    Flat {
        rows: Vec<Value>,
        rhs_key_fields: Vec<String>,
    },
}

/// Output of one chunk call against the target branch connector.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForeachChunkOutput {
    pub data: ForeachBatchRows,
    /// Branch consistency marker (`extensions.gap.asOf`), if reported (`RUN-026`).
    pub as_of: Option<String>,
}

impl ForeachChunkOutput {
    /// Constructs a keyed chunk output.
    #[must_use]
    pub const fn keyed(entries: Vec<(Vec<Value>, Value)>, as_of: Option<String>) -> Self {
        Self {
            data: ForeachBatchRows::Keyed(entries),
            as_of,
        }
    }

    /// Constructs a flat chunk output matched by `rhs_key_fields`.
    #[must_use]
    pub fn flat(
        rows: Vec<Value>,
        rhs_key_fields: impl IntoIterator<Item = impl Into<String>>,
        as_of: Option<String>,
    ) -> Self {
        Self {
            data: ForeachBatchRows::Flat {
                rows,
                rhs_key_fields: rhs_key_fields.into_iter().map(Into::into).collect(),
            },
            as_of,
        }
    }
}

/// Object-safe async interface issuing batched `foreach` chunk fetches (`RUN-020`).
pub trait ForeachBatchFetcher: Send + Sync {
    /// Executes one batched query for `keys` (all non-null, deduplicated within this chunk).
    fn fetch_batch(
        &self,
        keys: Vec<Vec<Value>>,
    ) -> Pin<Box<dyn Future<Output = GapResult<ForeachChunkOutput>> + Send + '_>>;
}

/// Async interface for remote-predicate `IN (...)` execution + bounded fallback (`RUN-023`).
pub trait RemotePredicateFetcher: Send + Sync {
    /// Fetches RHS rows matching the pushed-down `IN` key tuples.
    fn fetch_with_in_keys(
        &self,
        keys: Vec<Vec<Value>>,
    ) -> Pin<Box<dyn Future<Output = GapResult<ForeachChunkOutput>> + Send + '_>>;

    /// Fallback fetch returning bounded candidate rows when keys exceed `max_pushdown_keys` (`RUN-023`).
    fn fetch_bounded_fallback(
        &self,
        max_rows: usize,
    ) -> Pin<Box<dyn Future<Output = GapResult<ForeachChunkOutput>> + Send + '_>>;
}

/// Collapsed partial-failure error entry covering all LHS row indexes in a failed chunk (`RUN-024`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JoinFieldError {
    pub code: ErrorCode,
    pub message: String,
    pub field_path: String,
    pub affected_indexes: Vec<usize>,
    pub retry: RetryInfo,
}

impl JoinFieldError {
    /// Converts this join error to a [`GraphqlError`] with collapsed list path (`RUN-024`).
    #[must_use]
    pub fn into_graphql_error(self) -> GraphqlError {
        let detail = if self.affected_indexes.is_empty() {
            self.message
        } else {
            format!(
                "{} (affectedIndexes={:?})",
                self.message, self.affected_indexes
            )
        };
        GraphqlError {
            message: detail,
            path: Some(vec![PathSegment::Field(self.field_path)]),
            extensions: GraphqlErrorExtensions {
                code: self.code,
                retry: Some(self.retry),
            },
        }
    }
}

/// Non-fatal execution warning emitted into `extensions.warnings` (`RUN-023`, `RUN-026`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JoinWarning {
    pub code: ErrorCode,
    pub message: String,
}

/// Executed join output with stitched LHS rows, telemetry counters, errors, warnings, and `asOf`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JoinExecResult {
    pub rows: Vec<Value>,
    pub errors: Vec<JoinFieldError>,
    pub warnings: Vec<JoinWarning>,
    pub partial: bool,
    pub upstream_calls: usize,
    pub unique_keys: usize,
    pub stitched_rhs_rows: usize,
    pub as_of: BTreeMap<String, String>,
}

/// Executes a `FOREACH_BATCH` cross-branch join strictly following `RUN-020..026` and `INV-18`:
/// 1. Extracts key tuples from LHS rows in order, skipping `null`/missing keys with `Value::Null`.
/// 2. Deduplicates keys via type-preserving canonical encoding and checks `max_join_keys`.
/// 3. Splits unique keys into `ceil(unique_keys / min(branch_max_batch, plan_max_batch))` chunks.
/// 4. Dispatches chunks concurrently capped at `max(1, branch_max_in_flight / 2)` (and optional
///    per-branch [`BranchBulkhead`]), counting actual `upstream_calls`.
/// 5. Enforces `max_join_rows`, `ONE_TO_ONE`/`MANY_TO_ONE` uniqueness, partial-failure chunk
///    isolation vs `@budget(failFast: true)`, and `asOf` consistency checks.
/// 6. Stitches RHS records onto LHS rows in deterministic LHS order (`RUN-020`).
pub async fn execute_foreach_join(
    lhs_rows: Vec<Value>,
    config: &JoinExecConfig,
    fetcher: &dyn ForeachBatchFetcher,
    bulkhead: Option<&BranchBulkhead>,
    cancel: &CancellationToken,
) -> GapResult<JoinExecResult> {
    if config.keys.is_empty() {
        return Err(GapError::new(
            ErrorCode::ValidationError,
            "join config declares zero key pairs",
        ));
    }
    let chunk_size = config.effective_chunk_size();
    if chunk_size == 0 {
        return Err(GapError::new(
            ErrorCode::JoinNotBatchable,
            format!(
                "branch '{}' effective max_batch_size is 0; foreach join cannot batch",
                config.target_branch
            ),
        ));
    }

    let mut warnings = check_as_of_consistency(config)?;
    if cancel.is_cancelled() {
        return Err(GapError::new(ErrorCode::Cancelled, "join cancelled"));
    }

    let extracted = extract_and_dedup_keys(&lhs_rows, &config.keys, config.max_join_keys)?;
    let mut stitched_rows = lhs_rows;

    // Rows with null/missing keys resolve directly to null without an upstream call (`RUN-020` (2)).
    for &null_idx in &extracted.null_row_indexes {
        set_joined_field(
            &mut stitched_rows[null_idx],
            &config.response_key,
            Value::Null,
        );
    }

    if extracted.unique_keys.is_empty() {
        return Ok(JoinExecResult {
            rows: stitched_rows,
            errors: Vec::new(),
            warnings,
            partial: false,
            upstream_calls: 0,
            unique_keys: 0,
            stitched_rhs_rows: 0,
            as_of: BTreeMap::new(),
        });
    }

    let unique_count = extracted.unique_keys.len();
    let chunk_specs: Vec<Vec<usize>> = (0..unique_count)
        .collect::<Vec<_>>()
        .chunks(chunk_size)
        .map(<[usize]>::to_vec)
        .collect();

    let upstream_calls = Arc::new(AtomicUsize::new(0));
    let total_rhs_rows = Arc::new(AtomicUsize::new(0));
    let sem = Arc::new(Semaphore::new(config.chunk_concurrency()));
    let chunk_cancel = cancel.child_token();

    let mut chunk_futs = Vec::with_capacity(chunk_specs.len());
    for key_indexes in &chunk_specs {
        let key_tuples: Vec<Vec<Value>> = key_indexes
            .iter()
            .map(|&idx| extracted.unique_keys[idx].key.values().to_vec())
            .collect();
        let sem = Arc::clone(&sem);
        let calls = Arc::clone(&upstream_calls);
        let rhs_counter = Arc::clone(&total_rhs_rows);
        let cancel_tok = chunk_cancel.clone();
        let max_join_rows = config.max_join_rows;
        let branch_name = config.target_branch.clone();
        let on_error = config.on_error;

        chunk_futs.push(async move {
            let _conc_permit = tokio::select! {
                biased;
                () = cancel_tok.cancelled() => {
                    return Err(GapError::new(ErrorCode::Cancelled, "join chunk cancelled"));
                }
                res = sem.acquire() => {
                    res.map_err(|_| GapError::new(ErrorCode::Cancelled, "join semaphore closed"))?
                }
            };

            let _bh_permit = if let Some(bh) = bulkhead {
                Some(bh.acquire(&branch_name).await?)
            } else {
                None
            };

            if cancel_tok.is_cancelled() {
                return Err(GapError::new(ErrorCode::Cancelled, "join chunk cancelled"));
            }

            calls.fetch_add(1, Ordering::AcqRel);
            let output = match fetcher.fetch_batch(key_tuples).await {
                Ok(out) => out,
                Err(err) => {
                    if on_error == JoinErrorMode::FailFast {
                        cancel_tok.cancel();
                    }
                    return Err(err);
                }
            };

            let row_count = chunk_row_len(&output.data);
            let new_total = rhs_counter.fetch_add(row_count, Ordering::AcqRel) + row_count;
            if new_total > max_join_rows {
                cancel_tok.cancel();
                return Err(GapError::new(
                    ErrorCode::JoinLimitExceeded,
                    format!("stitched RHS rows {new_total} exceeded max_join_rows {max_join_rows}"),
                ));
            }

            Ok(output)
        });
    }

    let chunk_results = futures_util::future::join_all(chunk_futs).await;

    let mut rhs_by_key: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    let mut errors = Vec::new();
    let mut partial = false;
    let mut failed_lhs_indexes: BTreeSet<usize> = BTreeSet::new();
    let mut as_of = BTreeMap::new();
    let mut stitched_rhs_rows = 0usize;

    for (chunk_idx, result) in chunk_results.into_iter().enumerate() {
        let affected_lhs = collect_affected_lhs_indexes(&chunk_specs[chunk_idx], &extracted);
        match result {
            Ok(output) => {
                if let Some(ts) = output.as_of {
                    as_of.insert(config.target_branch.clone(), ts);
                }
                stitched_rhs_rows += chunk_row_len(&output.data);
                index_chunk_rows(output.data, &mut rhs_by_key);
            }
            Err(err) => {
                if err.code() == ErrorCode::JoinLimitExceeded {
                    return Err(err);
                }
                if config.on_error == JoinErrorMode::FailFast {
                    return Err(err);
                }
                partial = true;
                for &idx in &affected_lhs {
                    failed_lhs_indexes.insert(idx);
                    set_joined_field(&mut stitched_rows[idx], &config.response_key, Value::Null);
                }
                errors.push(JoinFieldError {
                    code: err.code(),
                    message: err.message().to_owned(),
                    field_path: config.response_key.clone(),
                    affected_indexes: affected_lhs,
                    retry: err.extensions.retry,
                });
            }
        }
    }

    if stitched_rhs_rows > config.max_join_rows {
        return Err(GapError::new(
            ErrorCode::JoinLimitExceeded,
            format!(
                "stitched RHS rows {stitched_rhs_rows} exceeded max_join_rows {}",
                config.max_join_rows
            ),
        ));
    }

    stitch_indexed_rows(
        &mut stitched_rows,
        &extracted,
        &rhs_by_key,
        &failed_lhs_indexes,
        config,
        &mut errors,
        &mut partial,
    )?;

    if config.requested_as_of.is_some() && !config.branch_supports_as_of {
        // Explicit deterministic warning order.
        let _ = &mut warnings;
    }

    Ok(JoinExecResult {
        rows: stitched_rows,
        errors,
        warnings,
        partial,
        upstream_calls: upstream_calls.load(Ordering::Acquire),
        unique_keys: unique_count,
        stitched_rhs_rows,
        as_of,
    })
}

/// Executes a `REMOTE_PREDICATE` join (`RUN-023`), pushing `IN (keys)` when `unique_keys <=
/// max_pushdown_keys` and falling back to bounded local filtering (`GAP_PUSHDOWN_FALLBACK`) beyond
/// that threshold.
pub async fn execute_remote_predicate_join(
    lhs_rows: Vec<Value>,
    config: &JoinExecConfig,
    fetcher: &dyn RemotePredicateFetcher,
    fallback_fetch_cap: usize,
    bulkhead: Option<&BranchBulkhead>,
    cancel: &CancellationToken,
) -> GapResult<JoinExecResult> {
    if config.keys.is_empty() {
        return Err(GapError::new(
            ErrorCode::ValidationError,
            "remote predicate join config declares zero key pairs",
        ));
    }

    let mut warnings = check_as_of_consistency(config)?;
    if cancel.is_cancelled() {
        return Err(GapError::new(ErrorCode::Cancelled, "join cancelled"));
    }

    let extracted = extract_and_dedup_keys(&lhs_rows, &config.keys, config.max_join_keys)?;
    let mut stitched_rows = lhs_rows;
    for &null_idx in &extracted.null_row_indexes {
        set_joined_field(
            &mut stitched_rows[null_idx],
            &config.response_key,
            Value::Null,
        );
    }

    if extracted.unique_keys.is_empty() {
        return Ok(JoinExecResult {
            rows: stitched_rows,
            errors: Vec::new(),
            warnings,
            partial: false,
            upstream_calls: 0,
            unique_keys: 0,
            stitched_rhs_rows: 0,
            as_of: BTreeMap::new(),
        });
    }

    let _bh_permit = if let Some(bh) = bulkhead {
        Some(bh.acquire(&config.target_branch).await?)
    } else {
        None
    };

    let unique_count = extracted.unique_keys.len();
    let mut as_of = BTreeMap::new();
    let chunk_out = if unique_count <= config.max_pushdown_keys {
        let key_tuples: Vec<Vec<Value>> = extracted
            .unique_keys
            .iter()
            .map(|entry| entry.key.values().to_vec())
            .collect();
        fetcher.fetch_with_in_keys(key_tuples).await?
    } else {
        let cap = fallback_fetch_cap.min(config.max_join_rows).max(1);
        warnings.push(JoinWarning {
            code: ErrorCode::PushdownFallback,
            message: format!(
                "unique keys {unique_count} exceeded max_pushdown_keys {}; falling back to bounded local filter (cap={cap})",
                config.max_pushdown_keys
            ),
        });
        fetcher.fetch_bounded_fallback(cap).await?
    };

    if let Some(ts) = chunk_out.as_of {
        as_of.insert(config.target_branch.clone(), ts);
    }

    let raw_count = chunk_row_len(&chunk_out.data);
    if raw_count > config.max_join_rows {
        return Err(GapError::new(
            ErrorCode::JoinLimitExceeded,
            format!(
                "remote predicate RHS rows {raw_count} exceeded max_join_rows {}",
                config.max_join_rows
            ),
        ));
    }

    let mut rhs_by_key: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    index_chunk_rows(chunk_out.data, &mut rhs_by_key);

    // Filter out rows whose key is not in `extracted.key_index` when fallback ran.
    let valid_keys: BTreeSet<&str> = extracted
        .unique_keys
        .iter()
        .map(|entry| entry.key.as_encoded())
        .collect();
    rhs_by_key.retain(|k, _| valid_keys.contains(k.as_str()));

    let stitched_rhs_rows: usize = rhs_by_key.values().map(Vec::len).sum();
    let mut errors = Vec::new();
    let mut partial = false;
    let failed = BTreeSet::new();

    stitch_indexed_rows(
        &mut stitched_rows,
        &extracted,
        &rhs_by_key,
        &failed,
        config,
        &mut errors,
        &mut partial,
    )?;

    Ok(JoinExecResult {
        rows: stitched_rows,
        errors,
        warnings,
        partial,
        upstream_calls: 1,
        unique_keys: unique_count,
        stitched_rhs_rows,
        as_of,
    })
}

struct KeyBucket {
    key: CanonicalJoinKey,
    lhs_indexes: Vec<usize>,
}

struct ExtractedKeys {
    unique_keys: Vec<KeyBucket>,
    null_row_indexes: Vec<usize>,
}

fn extract_and_dedup_keys(
    lhs_rows: &[Value],
    key_pairs: &[JoinKeyPair],
    max_join_keys: usize,
) -> GapResult<ExtractedKeys> {
    let mut unique_keys: Vec<KeyBucket> = Vec::new();
    let mut key_to_slot: BTreeMap<String, usize> = BTreeMap::new();
    let mut null_row_indexes = Vec::new();

    for (row_idx, row) in lhs_rows.iter().enumerate() {
        let Some(obj) = row.as_object() else {
            null_row_indexes.push(row_idx);
            continue;
        };

        let mut tuple = Vec::with_capacity(key_pairs.len());
        let mut has_null = false;
        for kp in key_pairs {
            match obj.get(&kp.from) {
                Some(v) if !v.is_null() => tuple.push(v.clone()),
                _ => {
                    has_null = true;
                    break;
                }
            }
        }
        if has_null {
            null_row_indexes.push(row_idx);
            continue;
        }

        let canon = CanonicalJoinKey::from_tuple(tuple);
        if let Some(&slot) = key_to_slot.get(canon.as_encoded()) {
            unique_keys[slot].lhs_indexes.push(row_idx);
        } else {
            if unique_keys.len() >= max_join_keys {
                let observed = unique_keys.len() + 1;
                return Err(GapError::new(
                    ErrorCode::JoinLimitExceeded,
                    format!("unique join keys {observed} exceeded max_join_keys {max_join_keys}"),
                ));
            }
            let slot = unique_keys.len();
            key_to_slot.insert(canon.as_encoded().to_owned(), slot);
            unique_keys.push(KeyBucket {
                key: canon,
                lhs_indexes: vec![row_idx],
            });
        }
    }

    Ok(ExtractedKeys {
        unique_keys,
        null_row_indexes,
    })
}

fn collect_affected_lhs_indexes(key_indexes: &[usize], extracted: &ExtractedKeys) -> Vec<usize> {
    let mut affected = Vec::new();
    for &key_idx in key_indexes {
        affected.extend_from_slice(&extracted.unique_keys[key_idx].lhs_indexes);
    }
    affected.sort_unstable();
    affected.dedup();
    affected
}

fn chunk_row_len(data: &ForeachBatchRows) -> usize {
    match data {
        ForeachBatchRows::Keyed(entries) => entries.len(),
        ForeachBatchRows::Flat { rows, .. } => rows.len(),
    }
}

fn index_chunk_rows(data: ForeachBatchRows, rhs_by_key: &mut BTreeMap<String, Vec<Value>>) {
    match data {
        ForeachBatchRows::Keyed(entries) => {
            for (tuple, rhs_row) in entries {
                if tuple.iter().any(Value::is_null) {
                    continue;
                }
                let key = CanonicalJoinKey::from_tuple(tuple);
                rhs_by_key
                    .entry(key.as_encoded().to_owned())
                    .or_default()
                    .push(rhs_row);
            }
        }
        ForeachBatchRows::Flat {
            rows,
            rhs_key_fields,
        } => {
            for rhs_row in rows {
                let Some(obj) = rhs_row.as_object() else {
                    continue;
                };
                let mut tuple = Vec::with_capacity(rhs_key_fields.len());
                let mut missing = false;
                for field in &rhs_key_fields {
                    match obj.get(field) {
                        Some(v) if !v.is_null() => tuple.push(v.clone()),
                        _ => {
                            missing = true;
                            break;
                        }
                    }
                }
                if missing {
                    continue;
                }
                let key = CanonicalJoinKey::from_tuple(tuple);
                rhs_by_key
                    .entry(key.as_encoded().to_owned())
                    .or_default()
                    .push(rhs_row);
            }
        }
    }
}

fn stitch_indexed_rows(
    stitched_rows: &mut [Value],
    extracted: &ExtractedKeys,
    rhs_by_key: &BTreeMap<String, Vec<Value>>,
    failed_lhs_indexes: &BTreeSet<usize>,
    config: &JoinExecConfig,
    errors: &mut Vec<JoinFieldError>,
    partial: &mut bool,
) -> GapResult<()> {
    for bucket in &extracted.unique_keys {
        let active_indexes: Vec<usize> = bucket
            .lhs_indexes
            .iter()
            .copied()
            .filter(|idx| !failed_lhs_indexes.contains(idx))
            .collect();
        if active_indexes.is_empty() {
            continue;
        }

        let matched = rhs_by_key.get(bucket.key.as_encoded());
        match config.cardinality {
            JoinCardinality::OneToOne | JoinCardinality::ManyToOne => match matched {
                None => {
                    for idx in active_indexes {
                        set_joined_field(
                            &mut stitched_rows[idx],
                            &config.response_key,
                            Value::Null,
                        );
                    }
                }
                Some(items) if items.is_empty() => {
                    for idx in active_indexes {
                        set_joined_field(
                            &mut stitched_rows[idx],
                            &config.response_key,
                            Value::Null,
                        );
                    }
                }
                Some(items)
                    if items.len() == 1 || config.on_duplicate == DuplicateKeyPolicy::First =>
                {
                    let chosen = items[0].clone();
                    for idx in active_indexes {
                        set_joined_field(
                            &mut stitched_rows[idx],
                            &config.response_key,
                            chosen.clone(),
                        );
                    }
                }
                Some(items) => {
                    let msg = format!(
                        "join '{}' ({:?}) expected <= 1 RHS row for key {}, observed {}",
                        config.response_key,
                        config.cardinality,
                        bucket.key.as_encoded(),
                        items.len()
                    );
                    if config.on_error == JoinErrorMode::FailFast {
                        return Err(GapError::new(ErrorCode::JoinKeyNotUnique, msg));
                    }
                    *partial = true;
                    for &idx in &active_indexes {
                        set_joined_field(
                            &mut stitched_rows[idx],
                            &config.response_key,
                            Value::Null,
                        );
                    }
                    errors.push(JoinFieldError {
                        code: ErrorCode::JoinKeyNotUnique,
                        message: msg,
                        field_path: config.response_key.clone(),
                        affected_indexes: active_indexes,
                        retry: RetryInfo {
                            retryable: false,
                            after_ms: None,
                        },
                    });
                }
            },
            JoinCardinality::OneToMany => {
                let list_val = Value::Array(matched.cloned().unwrap_or_default());
                for idx in active_indexes {
                    set_joined_field(
                        &mut stitched_rows[idx],
                        &config.response_key,
                        list_val.clone(),
                    );
                }
            }
        }
    }
    Ok(())
}

fn set_joined_field(row: &mut Value, field: &str, value: Value) {
    if let Some(obj) = row.as_object_mut() {
        obj.insert(field.to_owned(), value);
    } else {
        let mut map = Map::new();
        map.insert(field.to_owned(), value);
        *row = Value::Object(map);
    }
}

fn check_as_of_consistency(config: &JoinExecConfig) -> GapResult<Vec<JoinWarning>> {
    let mut warnings = Vec::new();
    if config.requested_as_of.is_some() && !config.branch_supports_as_of {
        let msg = format!(
            "branch '{}' lacks capabilities.as_of for requested temporal snapshot",
            config.target_branch
        );
        if config.strict_as_of {
            return Err(GapError::new(ErrorCode::UnsupportedFeature, msg));
        }
        warnings.push(JoinWarning {
            code: ErrorCode::UnsupportedFeature,
            message: msg,
        });
    }
    Ok(warnings)
}

/// Signed cursor payload bound to `(op_hash, generation, tenant, schema_hash)` (`RUN-025`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectionCursorPayload {
    pub op_hash: String,
    pub generation: u64,
    pub tenant: String,
    pub schema_hash: String,
    pub offset: u64,
}

impl ConnectionCursorPayload {
    /// Convenience constructor from typed [`TenantId`].
    #[must_use]
    pub fn new(
        op_hash: impl Into<String>,
        generation: u64,
        tenant: &TenantId,
        schema_hash: impl Into<String>,
        offset: u64,
    ) -> Self {
        Self {
            op_hash: op_hash.into(),
            generation,
            tenant: tenant.as_str().to_owned(),
            schema_hash: schema_hash.into(),
            offset,
        }
    }
}

/// Page slice returned by [`paginate_connection`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectionPage<T> {
    pub items: Vec<T>,
    pub has_next_page: bool,
    pub end_cursor: Option<String>,
    pub start_offset: u64,
}

/// Encodes and HMAC-SHA256-signs a pagination cursor (`RUN-025`).
pub fn encode_signed_cursor(payload: &ConnectionCursorPayload, secret: &[u8]) -> GapResult<String> {
    if secret.is_empty() {
        return Err(GapError::new(
            ErrorCode::ValidationError,
            "cursor signing secret must not be empty",
        ));
    }
    let json_bytes = serde_json::to_vec(payload).map_err(|err| {
        GapError::new(
            ErrorCode::ValidationError,
            format!("failed to encode cursor payload: {err}"),
        )
    })?;
    let tag = hmac_sha256::hmac_sha256(secret, &json_bytes);
    Ok(format!(
        "{}.{}",
        hmac_sha256::hex_encode(&json_bytes),
        hmac_sha256::hex_encode(&tag)
    ))
}

/// Verifies a signed pagination cursor against HMAC signature, `(op_hash, tenant)`, and
/// cross-generation `schema_hash` stability (`RUN-025`).
pub fn decode_and_verify_cursor(
    cursor: &str,
    secret: &[u8],
    expected_op_hash: &str,
    current_generation: u64,
    expected_tenant: &TenantId,
    current_schema_hash: &str,
) -> GapResult<ConnectionCursorPayload> {
    if secret.is_empty() {
        return Err(GapError::new(
            ErrorCode::ValidationError,
            "cursor verification secret must not be empty",
        ));
    }
    let (payload_hex, tag_hex) = cursor
        .split_once('.')
        .ok_or_else(|| GapError::new(ErrorCode::ValidationError, "malformed connection cursor"))?;
    let payload_bytes = hmac_sha256::hex_decode(payload_hex).ok_or_else(|| {
        GapError::new(
            ErrorCode::ValidationError,
            "invalid connection cursor encoding",
        )
    })?;
    let provided_tag = hmac_sha256::hex_decode(tag_hex).ok_or_else(|| {
        GapError::new(ErrorCode::ValidationError, "invalid connection cursor tag")
    })?;

    let expected_tag = hmac_sha256::hmac_sha256(secret, &payload_bytes);
    if !hmac_sha256::constant_time_eq(&expected_tag, &provided_tag) {
        return Err(GapError::new(
            ErrorCode::ValidationError,
            "connection cursor signature verification failed",
        ));
    }

    let payload: ConnectionCursorPayload =
        serde_json::from_slice(&payload_bytes).map_err(|_| {
            GapError::new(
                ErrorCode::ValidationError,
                "corrupt connection cursor payload",
            )
        })?;

    if payload.op_hash != expected_op_hash {
        return Err(GapError::new(
            ErrorCode::ValidationError,
            "connection cursor bound to a different operation",
        ));
    }
    if payload.tenant != expected_tenant.as_str() {
        return Err(GapError::new(
            ErrorCode::ValidationError,
            "connection cursor bound to a different tenant",
        ));
    }
    if payload.generation != current_generation && payload.schema_hash != current_schema_hash {
        return Err(GapError::new(
            ErrorCode::ValidationError,
            "connection cursor invalidated across snapshot generations due to schema change",
        ));
    }

    Ok(payload)
}

/// Binding parameters used to sign and verify connection pagination cursors (`RUN-025`).
#[derive(Debug, Clone, Copy)]
pub struct CursorBinding<'a> {
    pub secret: &'a [u8],
    pub op_hash: &'a str,
    pub generation: u64,
    pub tenant: &'a TenantId,
    pub schema_hash: &'a str,
}

/// Paginates LHS connection items independently of joined RHS fields using verified cursors
/// (`RUN-025`).
pub fn paginate_connection<T: Clone>(
    items: &[T],
    after_cursor: Option<&str>,
    first: Option<usize>,
    binding: &CursorBinding<'_>,
) -> GapResult<ConnectionPage<T>> {
    let start_offset = match after_cursor {
        Some(token) => {
            let decoded = decode_and_verify_cursor(
                token,
                binding.secret,
                binding.op_hash,
                binding.generation,
                binding.tenant,
                binding.schema_hash,
            )?;
            usize::try_from(decoded.offset).unwrap_or(usize::MAX)
        }
        None => 0,
    };

    if start_offset >= items.len() {
        return Ok(ConnectionPage {
            items: Vec::new(),
            has_next_page: false,
            end_cursor: None,
            start_offset: u64::try_from(start_offset).unwrap_or(u64::MAX),
        });
    }

    let limit = first.unwrap_or(items.len().saturating_sub(start_offset));
    let end_offset = start_offset.saturating_add(limit).min(items.len());
    let slice = items[start_offset..end_offset].to_vec();
    let has_next_page = end_offset < items.len();
    let end_cursor = if slice.is_empty() {
        None
    } else {
        let next_payload = ConnectionCursorPayload::new(
            binding.op_hash,
            binding.generation,
            binding.tenant,
            binding.schema_hash,
            u64::try_from(end_offset).unwrap_or(u64::MAX),
        );
        Some(encode_signed_cursor(&next_payload, binding.secret)?)
    };

    Ok(ConnectionPage {
        items: slice,
        has_next_page,
        end_cursor,
        start_offset: u64::try_from(start_offset).unwrap_or(u64::MAX),
    })
}

/// Pure safe Rust RFC 6234 SHA-256 and RFC 2104 HMAC-SHA256 for signed connection cursors.
mod hmac_sha256 {
    const K: [u32; 64] = [
        0x428a_2f98,
        0x7137_4491,
        0xb5c0_fbcf,
        0xe9b5_dba5,
        0x3956_c25b,
        0x59f1_11f1,
        0x923f_82a4,
        0xab1c_5ed5,
        0xd807_aa98,
        0x1283_5b01,
        0x2431_85be,
        0x550c_7dc3,
        0x72be_5d74,
        0x80de_b1fe,
        0x9bdc_06a7,
        0xc19b_f174,
        0xe49b_69c1,
        0xefbe_4786,
        0x0fc1_9dc6,
        0x240c_a1cc,
        0x2de9_2c6f,
        0x4a74_84aa,
        0x5cb0_a9dc,
        0x76f9_88da,
        0x983e_5152,
        0xa831_c66d,
        0xb003_27c8,
        0xbf59_7fc7,
        0xc6e0_0bf3,
        0xd5a7_9147,
        0x06ca_6351,
        0x1429_2967,
        0x27b7_0a85,
        0x2e1b_2138,
        0x4d2c_6dfc,
        0x5338_0d13,
        0x650a_7354,
        0x766a_0abb,
        0x81c2_c92e,
        0x9272_2c85,
        0xa2bf_e8a1,
        0xa81a_664b,
        0xc24b_8b70,
        0xc76c_51a3,
        0xd192_e819,
        0xd699_0624,
        0xf40e_3585,
        0x106a_a070,
        0x19a4_c116,
        0x1e37_6c08,
        0x2748_774c,
        0x34b0_bcb5,
        0x391c_0cb3,
        0x4ed8_aa4a,
        0x5b9c_ca4f,
        0x682e_6ff3,
        0x748f_82ee,
        0x78a5_636f,
        0x84c8_7814,
        0x8cc7_0208,
        0x90be_fffa,
        0xa450_6ceb,
        0xbef9_a3f7,
        0xc671_78f2,
    ];

    const IV: [u32; 8] = [
        0x6a09_e667,
        0xbb67_ae85,
        0x3c6e_f372,
        0xa54f_f53a,
        0x510e_527f,
        0x9b05_688c,
        0x1f83_d9ab,
        0x5be0_cd19,
    ];

    pub(super) fn sha256(data: &[u8]) -> [u8; 32] {
        let bit_len = (data.len() as u64).wrapping_mul(8);
        let mut padded = Vec::with_capacity(data.len() + 72);
        padded.extend_from_slice(data);
        padded.push(0x80);
        while (padded.len() % 64) != 56 {
            padded.push(0);
        }
        padded.extend_from_slice(&bit_len.to_be_bytes());

        let mut h = IV;
        for chunk in padded.as_chunks::<64>().0 {
            let mut w = [0_u32; 64];
            for (idx, word) in chunk.as_chunks::<4>().0.iter().enumerate() {
                w[idx] = u32::from_be_bytes(*word);
            }
            for i in 16..64 {
                let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
                let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
                w[i] = w[i - 16]
                    .wrapping_add(s0)
                    .wrapping_add(w[i - 7])
                    .wrapping_add(s1);
            }

            let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh] = h;
            for i in 0..64 {
                let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
                let ch = (e & f) ^ ((!e) & g);
                let temp1 = hh
                    .wrapping_add(s1)
                    .wrapping_add(ch)
                    .wrapping_add(K[i])
                    .wrapping_add(w[i]);
                let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
                let maj = (a & b) ^ (a & c) ^ (b & c);
                let temp2 = s0.wrapping_add(maj);

                hh = g;
                g = f;
                f = e;
                e = d.wrapping_add(temp1);
                d = c;
                c = b;
                b = a;
                a = temp1.wrapping_add(temp2);
            }

            let delta = [a, b, c, d, e, f, g, hh];
            for (slot, add) in h.iter_mut().zip(delta) {
                *slot = slot.wrapping_add(add);
            }
        }

        let mut out = [0_u8; 32];
        for (idx, word) in h.iter().enumerate() {
            let bytes = word.to_be_bytes();
            let base = idx * 4;
            out[base..base + 4].copy_from_slice(&bytes);
        }
        out
    }

    pub(super) fn hmac_sha256(key: &[u8], msg: &[u8]) -> [u8; 32] {
        let mut block = [0_u8; 64];
        if key.len() > 64 {
            let hashed = sha256(key);
            block[..32].copy_from_slice(&hashed);
        } else {
            block[..key.len()].copy_from_slice(key);
        }

        let mut o_key_pad = [0x5c_u8; 64];
        let mut i_key_pad = [0x36_u8; 64];
        for i in 0..64 {
            o_key_pad[i] ^= block[i];
            i_key_pad[i] ^= block[i];
        }

        let mut inner = Vec::with_capacity(64 + msg.len());
        inner.extend_from_slice(&i_key_pad);
        inner.extend_from_slice(msg);
        let inner_hash = sha256(&inner);

        let mut outer = Vec::with_capacity(64 + 32);
        outer.extend_from_slice(&o_key_pad);
        outer.extend_from_slice(&inner_hash);
        sha256(&outer)
    }

    pub(super) fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
        if a.len() != b.len() {
            return false;
        }
        let mut diff = 0_u8;
        for (&x, &y) in a.iter().zip(b.iter()) {
            diff |= x ^ y;
        }
        diff == 0
    }

    pub(super) fn hex_encode(bytes: &[u8]) -> String {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut out = String::with_capacity(bytes.len() * 2);
        for &b in bytes {
            out.push(char::from(HEX[(b >> 4) as usize]));
            out.push(char::from(HEX[(b & 0x0f) as usize]));
        }
        out
    }

    pub(super) fn hex_decode(input: &str) -> Option<Vec<u8>> {
        if !input.len().is_multiple_of(2) {
            return None;
        }
        let bytes = input.as_bytes();
        let mut out = Vec::with_capacity(bytes.len() / 2);
        for &[hi_b, lo_b] in bytes.as_chunks::<2>().0 {
            let hi = hex_nibble(hi_b)?;
            let lo = hex_nibble(lo_b)?;
            out.push((hi << 4) | lo);
        }
        Some(out)
    }

    const fn hex_nibble(b: u8) -> Option<u8> {
        match b {
            b'0'..=b'9' => Some(b - b'0'),
            b'a'..=b'f' => Some(b - b'a' + 10),
            b'A'..=b'F' => Some(b - b'A' + 10),
            _ => None,
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use std::time::Duration;

    use proptest::prelude::*;
    use serde_json::json;

    struct CountingUsersFetcher {
        calls: AtomicUsize,
        fail_batch_containing: Option<Value>,
        multi_row_for: Option<Value>,
    }

    impl CountingUsersFetcher {
        fn new() -> Self {
            Self {
                calls: AtomicUsize::new(0),
                fail_batch_containing: None,
                multi_row_for: None,
            }
        }
    }

    impl ForeachBatchFetcher for CountingUsersFetcher {
        fn fetch_batch(
            &self,
            keys: Vec<Vec<Value>>,
        ) -> Pin<Box<dyn Future<Output = GapResult<ForeachChunkOutput>> + Send + '_>> {
            Box::pin(async move {
                self.calls.fetch_add(1, Ordering::AcqRel);
                if let Some(ref target) = self.fail_batch_containing
                    && keys.iter().any(|k| &k[0] == target)
                {
                    return Err(GapError::new(
                        ErrorCode::BranchUnavailable,
                        "simulated chunk outage",
                    )
                    .with_retry(true, Some(15)));
                }

                let mut rows = Vec::new();
                for k in keys {
                    rows.push(json!({ "id": k[0], "name": format!("user-{}", k[0]) }));
                    if self.multi_row_for.as_ref() == Some(&k[0]) {
                        rows.push(json!({ "id": k[0], "name": format!("user-{}-dup", k[0]) }));
                    }
                }
                Ok(ForeachChunkOutput::flat(
                    rows,
                    ["id"],
                    Some("2026-11-14T00:00:00Z".to_owned()),
                ))
            })
        }
    }

    // spec: RUN-020, INV-18
    #[tokio::test]
    async fn inv_18_join_batches_1000_lhs_rows_with_nulls_and_duplicates_within_ceil_bound() {
        // 1,000 LHS rows with 100 null customer_ids and heavy duplication over 120 distinct IDs
        // plus integer 42 vs string "42" type distinction test.
        let mut lhs = Vec::with_capacity(1_000);
        for i in 0..1_000usize {
            let cid = if i % 10 == 0 {
                Value::Null
            } else if i % 15 == 1 {
                json!(42)
            } else if i % 15 == 2 {
                json!("42")
            } else {
                json!(format!("u{}", i % 118))
            };
            lhs.push(json!({ "order_id": i, "customer_id": cid }));
        }

        let mut cfg = JoinExecConfig::with_defaults(
            "users",
            "customer",
            JoinCardinality::ManyToOne,
            vec![JoinKeyPair {
                from: "customer_id".to_owned(),
                to: "ids".to_owned(),
            }],
        );
        cfg.branch_max_batch_size = 30;
        cfg.plan_max_batch = 1_000;
        cfg.branch_max_in_flight = 8;

        let fetcher = CountingUsersFetcher::new();
        let bulkhead = BranchBulkhead::new(8, Duration::from_millis(50));
        let cancel = CancellationToken::new();

        let result = execute_foreach_join(lhs, &cfg, &fetcher, Some(&bulkhead), &cancel)
            .await
            .expect("batched join on 1,000 LHS rows must succeed");

        let expected_max_calls = result.unique_keys.div_ceil(cfg.effective_chunk_size());
        assert_eq!(result.upstream_calls, fetcher.calls.load(Ordering::Acquire));
        assert!(
            result.upstream_calls <= expected_max_calls,
            "INV-18 violated: calls {} > ceil({}/{})={}",
            result.upstream_calls,
            result.unique_keys,
            cfg.effective_chunk_size(),
            expected_max_calls
        );
        assert!(!result.partial);
        assert!(result.errors.is_empty());
        assert_eq!(result.rows.len(), 1_000);

        // Verify integer 42 vs string "42" stayed distinct and null rows stayed Null!
        assert_eq!(result.rows[0]["customer"], Value::Null);
        assert_eq!(result.rows[1]["customer"]["id"], json!(42));
        assert_eq!(result.rows[2]["customer"]["id"], json!("42"));
        assert_eq!(
            result.as_of.get("users").map(String::as_str),
            Some("2026-11-14T00:00:00Z")
        );
    }

    // spec: RUN-021
    #[tokio::test]
    async fn join_cardinality_one_to_one_many_to_one_and_one_to_many_rules() {
        let lhs = vec![
            json!({ "id": 1, "k": "a" }),
            json!({ "id": 2, "k": "dup" }),
            json!({ "id": 3, "k": "dup" }),
        ];

        let mut fetcher = CountingUsersFetcher::new();
        fetcher.multi_row_for = Some(json!("dup"));

        let mut cfg = JoinExecConfig::with_defaults(
            "users",
            "profile",
            JoinCardinality::OneToOne,
            vec![JoinKeyPair {
                from: "k".to_owned(),
                to: "ids".to_owned(),
            }],
        );
        cfg.on_duplicate = DuplicateKeyPolicy::Error;
        cfg.on_error = JoinErrorMode::Partial;

        let cancel = CancellationToken::new();
        let res = execute_foreach_join(lhs.clone(), &cfg, &fetcher, None, &cancel)
            .await
            .expect("partial duplicate error returns partial report");
        assert!(res.partial);
        assert_eq!(res.errors.len(), 1);
        assert_eq!(res.errors[0].code, ErrorCode::JoinKeyNotUnique);
        assert_eq!(res.errors[0].affected_indexes, vec![1, 2]);
        assert_eq!(res.rows[0]["profile"]["id"], json!("a"));
        assert_eq!(res.rows[1]["profile"], Value::Null);
        assert_eq!(res.rows[2]["profile"], Value::Null);

        // With DuplicateKeyPolicy::First -> takes first row without error
        cfg.on_duplicate = DuplicateKeyPolicy::First;
        let res_first = execute_foreach_join(lhs.clone(), &cfg, &fetcher, None, &cancel)
            .await
            .expect("FIRST policy keeps first duplicate row");
        assert!(!res_first.partial);
        assert!(res_first.errors.is_empty());
        assert_eq!(res_first.rows[1]["profile"]["name"], json!("user-\"dup\""));

        // With OneToMany -> preserves all RHS rows in order inside Array
        cfg.cardinality = JoinCardinality::OneToMany;
        cfg.on_duplicate = DuplicateKeyPolicy::Error;
        let res_many = execute_foreach_join(lhs, &cfg, &fetcher, None, &cancel)
            .await
            .expect("OneToMany preserves list");
        let list = res_many.rows[1]["profile"].as_array().unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0]["name"], json!("user-\"dup\""));
        assert_eq!(list[1]["name"], json!("user-\"dup\"-dup"));
    }

    // spec: RUN-022
    #[tokio::test]
    async fn join_limits_enforce_max_join_keys_and_max_join_rows() {
        let cancel = CancellationToken::new();
        let lhs: Vec<Value> = (0..10).map(|i| json!({ "k": format!("k{i}") })).collect();
        let fetcher = CountingUsersFetcher::new();

        let mut cfg = JoinExecConfig::with_defaults(
            "users",
            "user",
            JoinCardinality::ManyToOne,
            vec![JoinKeyPair {
                from: "k".to_owned(),
                to: "ids".to_owned(),
            }],
        );
        cfg.max_join_keys = 5;
        let err_keys = execute_foreach_join(lhs.clone(), &cfg, &fetcher, None, &cancel)
            .await
            .expect_err("exceeding max_join_keys must fail GAP_JOIN_LIMIT_EXCEEDED");
        assert_eq!(err_keys.code(), ErrorCode::JoinLimitExceeded);

        cfg.max_join_keys = 100;
        cfg.max_join_rows = 4;
        let err_rows = execute_foreach_join(lhs, &cfg, &fetcher, None, &cancel)
            .await
            .expect_err("exceeding max_join_rows must fail GAP_JOIN_LIMIT_EXCEEDED");
        assert_eq!(err_rows.code(), ErrorCode::JoinLimitExceeded);
    }

    // spec: RUN-023
    #[tokio::test]
    async fn join_remote_predicate_falls_back_when_keys_exceed_max_pushdown_keys() {
        struct MockRemoteFetcher {
            used_fallback: AtomicUsize,
        }
        impl RemotePredicateFetcher for MockRemoteFetcher {
            fn fetch_with_in_keys(
                &self,
                keys: Vec<Vec<Value>>,
            ) -> Pin<Box<dyn Future<Output = GapResult<ForeachChunkOutput>> + Send + '_>>
            {
                Box::pin(async move {
                    let rows = keys
                        .into_iter()
                        .map(|k| json!({ "id": k[0], "tier": "direct" }))
                        .collect();
                    Ok(ForeachChunkOutput::flat(rows, ["id"], None))
                })
            }

            fn fetch_bounded_fallback(
                &self,
                max_rows: usize,
            ) -> Pin<Box<dyn Future<Output = GapResult<ForeachChunkOutput>> + Send + '_>>
            {
                Box::pin(async move {
                    self.used_fallback.fetch_add(1, Ordering::AcqRel);
                    let rows = (0..max_rows.min(10))
                        .map(|i| json!({ "id": i, "tier": "fallback" }))
                        .collect();
                    Ok(ForeachChunkOutput::flat(rows, ["id"], None))
                })
            }
        }

        let cancel = CancellationToken::new();
        let lhs: Vec<Value> = (0..6).map(|i| json!({ "uid": i })).collect();
        let mut cfg = JoinExecConfig::with_defaults(
            "users",
            "user",
            JoinCardinality::ManyToOne,
            vec![JoinKeyPair {
                from: "uid".to_owned(),
                to: "id".to_owned(),
            }],
        );
        cfg.max_pushdown_keys = 10;

        let fetcher = MockRemoteFetcher {
            used_fallback: AtomicUsize::new(0),
        };
        let direct = execute_remote_predicate_join(lhs.clone(), &cfg, &fetcher, 100, None, &cancel)
            .await
            .expect("direct IN pushdown succeeds");
        assert!(direct.warnings.is_empty());
        assert_eq!(fetcher.used_fallback.load(Ordering::Acquire), 0);
        assert_eq!(direct.rows[3]["user"]["tier"], json!("direct"));

        // Lower max_pushdown_keys below 6 -> triggers GAP_PUSHDOWN_FALLBACK + bounded local filter
        cfg.max_pushdown_keys = 3;
        let fallback = execute_remote_predicate_join(lhs, &cfg, &fetcher, 100, None, &cancel)
            .await
            .expect("bounded fallback succeeds with warning");
        assert_eq!(fetcher.used_fallback.load(Ordering::Acquire), 1);
        assert_eq!(fallback.warnings.len(), 1);
        assert_eq!(fallback.warnings[0].code, ErrorCode::PushdownFallback);
        assert_eq!(fallback.rows[3]["user"]["tier"], json!("fallback"));
    }

    // spec: RUN-024
    #[tokio::test]
    async fn join_partial_failure_nulls_only_failed_chunk_rows_and_fail_fast_aborts() {
        let lhs = vec![
            json!({ "id": 0, "k": "a" }),
            json!({ "id": 1, "k": "b" }),
            json!({ "id": 2, "k": "bad" }),
            json!({ "id": 3, "k": "d" }),
        ];

        let mut fetcher = CountingUsersFetcher::new();
        fetcher.fail_batch_containing = Some(json!("bad"));

        let mut cfg = JoinExecConfig::with_defaults(
            "users",
            "user",
            JoinCardinality::ManyToOne,
            vec![JoinKeyPair {
                from: "k".to_owned(),
                to: "ids".to_owned(),
            }],
        );
        cfg.branch_max_batch_size = 2; // chunk 0: [a, b], chunk 1: [bad, d]
        cfg.on_error = JoinErrorMode::Partial;

        let cancel = CancellationToken::new();
        let res = execute_foreach_join(lhs.clone(), &cfg, &fetcher, None, &cancel)
            .await
            .expect("PARTIAL mode retains healthy chunk 0 rows");
        assert!(res.partial);
        assert_eq!(res.errors.len(), 1);
        assert_eq!(res.errors[0].affected_indexes, vec![2, 3]);
        assert_eq!(res.rows[0]["user"]["id"], json!("a"));
        assert_eq!(res.rows[1]["user"]["id"], json!("b"));
        assert_eq!(res.rows[2]["user"], Value::Null);
        assert_eq!(res.rows[3]["user"], Value::Null);

        cfg.on_error = JoinErrorMode::FailFast;
        let err = execute_foreach_join(lhs, &cfg, &fetcher, None, &cancel)
            .await
            .expect_err("@budget(failFast: true) must abort");
        assert_eq!(err.code(), ErrorCode::BranchUnavailable);
    }

    // spec: RUN-025, RUN-026
    #[tokio::test]
    async fn join_pagination_cursor_binds_op_generation_and_tenant_and_as_of_consistency() {
        // Verify SHA-256 & HMAC-SHA256 KATs first.
        assert_eq!(
            hmac_sha256::hex_encode(&hmac_sha256::sha256(b"abc")),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            hmac_sha256::hex_encode(&hmac_sha256::hmac_sha256(&[0x0b; 20], b"Hi There")),
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
        );

        let tenant_a = TenantId::new("acme").unwrap();
        let tenant_b = TenantId::new("globex").unwrap();
        let secret = b"cursor-signing-key";

        let rows = vec![10_i32, 20, 30, 40, 50];
        let bind_gen1 = CursorBinding {
            secret,
            op_hash: "op-hash-1",
            generation: 1,
            tenant: &tenant_a,
            schema_hash: "schema-v1",
        };
        let page1 = paginate_connection(&rows, None, Some(2), &bind_gen1).unwrap();
        assert_eq!(page1.items, vec![10, 20]);
        assert!(page1.has_next_page);
        let cursor1 = page1.end_cursor.unwrap();

        // Cross-generation with unchanged schema works (`RUN-025`).
        let bind_gen2 = CursorBinding {
            generation: 2,
            ..bind_gen1
        };
        let page2 = paginate_connection(&rows, Some(&cursor1), Some(2), &bind_gen2).unwrap();
        assert_eq!(page2.items, vec![30, 40]);

        // Cross-generation with changed schema fails (`RUN-025`).
        let err_schema =
            decode_and_verify_cursor(&cursor1, secret, "op-hash-1", 2, &tenant_a, "schema-v2")
                .expect_err("schema change across generations must invalidate cursor");
        assert_eq!(err_schema.code(), ErrorCode::ValidationError);

        // Cross-tenant cursor replay fails (`INV-09`, `RUN-025`).
        let err_tenant =
            decode_and_verify_cursor(&cursor1, secret, "op-hash-1", 1, &tenant_b, "schema-v1")
                .expect_err("wrong tenant must fail");
        assert_eq!(err_tenant.code(), ErrorCode::ValidationError);

        // `RUN-026` asOf check: warning unless strictAsOf = true.
        let mut cfg = JoinExecConfig::with_defaults(
            "legacy",
            "item",
            JoinCardinality::ManyToOne,
            vec![JoinKeyPair {
                from: "k".to_owned(),
                to: "id".to_owned(),
            }],
        );
        cfg.requested_as_of = Some("2026-11-14T00:00:00Z".to_owned());
        cfg.branch_supports_as_of = false;
        cfg.strict_as_of = false;
        let warns = check_as_of_consistency(&cfg).expect("non-strict asOf only warns");
        assert_eq!(warns.len(), 1);
        assert_eq!(warns[0].code, ErrorCode::UnsupportedFeature);

        cfg.strict_as_of = true;
        let strict_err = check_as_of_consistency(&cfg)
            .expect_err("@budget(strictAsOf: true) must reject unsupported asOf");
        assert_eq!(strict_err.code(), ErrorCode::UnsupportedFeature);
    }

    // spec: RUN-020, INV-18
    proptest! {
        #[test]
        fn join_stitching_preserves_lhs_order_and_null_keys(
            keys in prop::collection::vec(prop::option::of(0i64..25i64), 0..80),
            batch_size in 1usize..12usize,
        ) {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();

            let lhs: Vec<Value> = keys
                .iter()
                .enumerate()
                .map(|(i, opt_k)| match opt_k {
                    Some(k) => json!({ "seq": i, "k": k }),
                    None => json!({ "seq": i, "k": Value::Null }),
                })
                .collect();

            let mut cfg = JoinExecConfig::with_defaults(
                "users",
                "user",
                JoinCardinality::ManyToOne,
                vec![JoinKeyPair {
                    from: "k".to_owned(),
                    to: "id".to_owned(),
                }],
            );
            cfg.branch_max_batch_size = batch_size;
            let fetcher = CountingUsersFetcher::new();
            let cancel = CancellationToken::new();

            let res = rt
                .block_on(execute_foreach_join(lhs, &cfg, &fetcher, None, &cancel))
                .unwrap();

            prop_assert_eq!(res.rows.len(), keys.len());
            let ceil_bound = if res.unique_keys == 0 {
                0
            } else {
                res.unique_keys.div_ceil(batch_size)
            };
            prop_assert!(res.upstream_calls <= ceil_bound);

            for (idx, (row, opt_k)) in res.rows.iter().zip(keys.iter()).enumerate() {
                prop_assert_eq!(row["seq"].as_u64(), Some(idx as u64));
                match opt_k {
                    Some(k) => prop_assert_eq!(row["user"]["id"].as_i64(), Some(*k)),
                    None => prop_assert_eq!(&row["user"], &Value::Null),
                }
            }
        }
    }
}
