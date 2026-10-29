//! Deduplicating, chunked, concurrency-bounded `foreach` execution helper (`NGC-015`, `NGC-020`, `NGC-025`, `NGC-026`, `NGC-096`, `INV-18`).

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::future::{Future, ready};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use futures_util::future::Either;
use futures_util::stream::{self, StreamExt};
use gap_core::error::{NgcErrorCode, NgcErrorInfo};
use gap_proto::ngc::v1::{Complete, QueryResponse, RowSetChunk, VariableSet, query_response};
use tokio::time::{Instant, Sleep, sleep_until};
use tokio_util::sync::CancellationToken;

use crate::connector::{BoxStream, CallScope, NgcResult};
use crate::error::ngc_error_to_proto;

/// Default maximum number of rows packed into a single [`RowSetChunk`] (`NGC-020`).
pub const DEFAULT_MAX_ROWS_PER_CHUNK: usize = 256;

/// Configuration for [`foreach`] (`NGC-096`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ForeachConfig {
    /// Maximum distinct variable sets per upstream batch (`>= 1`; `0` is rejected per `NGC-015` / `NGC-096` (c)).
    pub max_batch_size: usize,
    /// Maximum upstream batch calls executing concurrently (`>= 1`; `NGC-096` (c)).
    pub max_in_flight: usize,
    /// Maximum rows carried in a single emitted [`RowSetChunk`] (`>= 1`).
    pub max_rows_per_chunk: usize,
}

impl ForeachConfig {
    /// Creates a [`ForeachConfig`] with [`DEFAULT_MAX_ROWS_PER_CHUNK`].
    #[must_use]
    pub const fn new(max_batch_size: usize, max_in_flight: usize) -> Self {
        Self {
            max_batch_size,
            max_in_flight,
            max_rows_per_chunk: DEFAULT_MAX_ROWS_PER_CHUNK,
        }
    }

    /// Overrides `max_rows_per_chunk` (clamped to `>= 1`).
    #[must_use]
    pub const fn with_max_rows_per_chunk(mut self, max_rows_per_chunk: usize) -> Self {
        self.max_rows_per_chunk = if max_rows_per_chunk == 0 {
            1
        } else {
            max_rows_per_chunk
        };
        self
    }
}

/// Shared atomic reader for the number of upstream batch invocations made by a [`ForeachRun`] (`NGC-026`, `NGC-096` (f)).
#[derive(Debug, Clone)]
pub struct UpstreamCallsHandle {
    counter: Arc<AtomicU32>,
}

impl UpstreamCallsHandle {
    /// Returns the number of upstream calls actually started so far.
    #[must_use]
    pub fn get(&self) -> u32 {
        self.counter.load(Ordering::Acquire)
    }
}

/// Running `foreach` evaluation exposing its [`RowSetChunk`] stream and honest `upstream_calls` counter (`NGC-096`).
pub struct ForeachRun {
    stream: BoxStream<RowSetChunk>,
    upstream_calls: UpstreamCallsHandle,
}

impl std::fmt::Debug for ForeachRun {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ForeachRun")
            .field("upstream_calls", &self.upstream_calls())
            .finish_non_exhaustive()
    }
}

impl ForeachRun {
    /// Mutable borrow of the [`RowSetChunk`] stream.
    pub fn stream_mut(&mut self) -> &mut BoxStream<RowSetChunk> {
        &mut self.stream
    }

    /// Number of upstream batch calls actually made (`NGC-026`, `NGC-096` (f)).
    #[must_use]
    pub fn upstream_calls(&self) -> u32 {
        self.upstream_calls.get()
    }

    /// Splits [`ForeachRun`] into its [`RowSetChunk`] stream and an [`UpstreamCallsHandle`].
    #[must_use]
    pub fn into_parts(self) -> (BoxStream<RowSetChunk>, UpstreamCallsHandle) {
        (self.stream, self.upstream_calls)
    }

    /// Converts [`ForeachRun`] into a full [`QueryResponse`] stream ending with [`Complete`] (`NGC-020`, `NGC-026`).
    #[must_use]
    pub fn into_query_stream(self) -> BoxStream<QueryResponse> {
        let (chunk_stream, calls_handle) = self.into_parts();
        struct QueryStreamState {
            chunk_stream: BoxStream<RowSetChunk>,
            calls_handle: UpstreamCallsHandle,
            rows_total: u64,
            finished: bool,
        }

        let init = QueryStreamState {
            chunk_stream,
            calls_handle,
            rows_total: 0,
            finished: false,
        };

        Box::pin(stream::unfold(init, |mut st| async move {
            if st.finished {
                return None;
            }
            match st.chunk_stream.next().await {
                Some(Ok(row_chunk)) => {
                    st.rows_total = st.rows_total.saturating_add(row_chunk.rows.len() as u64);
                    let resp = QueryResponse {
                        chunk: Some(query_response::Chunk::Rows(row_chunk)),
                    };
                    Some((Ok(resp), st))
                }
                Some(Err(err)) => {
                    st.finished = true;
                    Some((Err(err), st))
                }
                None => {
                    st.finished = true;
                    let complete = QueryResponse {
                        chunk: Some(query_response::Chunk::Complete(Complete {
                            next_cursor: String::new(),
                            truncated: false,
                            rows_total: st.rows_total,
                            upstream_calls: st.calls_handle.get(),
                            latency_us: 0,
                        })),
                    };
                    Some((Ok(complete), st))
                }
            }
        }))
    }
}

/// Canonical JSON representation for `VariableSet.values` deduplication (`NGC-096` (b)):
/// object keys are sorted lexicographically and IEEE-754 `-0.0` normalizes to `+0.0`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum CanonicalValue {
    Null,
    Bool(bool),
    NumberBits(u64),
    String(String),
    Array(Vec<CanonicalValue>),
    Object(BTreeMap<String, CanonicalValue>),
}

fn canonicalize_prost_value(val: &prost_types::Value) -> CanonicalValue {
    match &val.kind {
        None | Some(prost_types::value::Kind::NullValue(_)) => CanonicalValue::Null,
        Some(prost_types::value::Kind::BoolValue(b)) => CanonicalValue::Bool(*b),
        Some(prost_types::value::Kind::NumberValue(n)) => {
            let bits = if *n == 0.0 {
                0.0_f64.to_bits()
            } else if n.is_nan() {
                f64::NAN.to_bits()
            } else {
                n.to_bits()
            };
            CanonicalValue::NumberBits(bits)
        }
        Some(prost_types::value::Kind::StringValue(s)) => CanonicalValue::String(s.clone()),
        Some(prost_types::value::Kind::ListValue(list)) => {
            CanonicalValue::Array(list.values.iter().map(canonicalize_prost_value).collect())
        }
        Some(prost_types::value::Kind::StructValue(st)) => canonicalize_prost_struct(Some(st)),
    }
}

fn canonicalize_prost_struct(st: Option<&prost_types::Struct>) -> CanonicalValue {
    let mut map = BTreeMap::new();
    if let Some(s) = st {
        for (k, v) in &s.fields {
            map.insert(k.clone(), canonicalize_prost_value(v));
        }
    }
    CanonicalValue::Object(map)
}

#[derive(Clone)]
struct DistinctGroup {
    values: prost_types::Struct,
    indices: Vec<u32>,
}

struct PlannedBatch {
    groups: Vec<DistinctGroup>,
}

fn emit_rows_for_index(
    index: u32,
    rows: &[prost_types::Struct],
    max_rows_per_chunk: usize,
    out: &mut Vec<RowSetChunk>,
) {
    let step = max_rows_per_chunk.max(1);
    if rows.is_empty() {
        out.push(RowSetChunk {
            foreach_index: index,
            rows: Vec::new(),
            last_for_index: true,
            error: None,
        });
        return;
    }

    let total_slices = rows.len().div_ceil(step);
    for (slice_idx, slice) in rows.chunks(step).enumerate() {
        let is_last = slice_idx + 1 == total_slices;
        out.push(RowSetChunk {
            foreach_index: index,
            rows: slice.to_vec(),
            last_for_index: is_last,
            error: None,
        });
    }
}

fn emit_error_for_index(index: u32, err: &NgcErrorInfo, out: &mut Vec<RowSetChunk>) {
    out.push(RowSetChunk {
        foreach_index: index,
        rows: Vec::new(),
        last_for_index: true,
        error: Some(ngc_error_to_proto(err)),
    });
}

fn build_batch_chunks(
    groups: &[DistinctGroup],
    batch_outcome: NgcResult<Vec<NgcResult<Vec<prost_types::Struct>>>>,
    max_rows_per_chunk: usize,
) -> Vec<RowSetChunk> {
    let mut chunks = Vec::new();
    match batch_outcome {
        Err(batch_err) => {
            for group in groups {
                for &idx in &group.indices {
                    emit_error_for_index(idx, &batch_err, &mut chunks);
                }
            }
        }
        Ok(elem_results) => {
            if elem_results.len() != groups.len() {
                let mismatch = NgcErrorInfo::new(
                    NgcErrorCode::Internal,
                    format!(
                        "foreach upstream returned {} results for batch of {} distinct variable sets",
                        elem_results.len(),
                        groups.len()
                    ),
                );
                for group in groups {
                    for &idx in &group.indices {
                        emit_error_for_index(idx, &mismatch, &mut chunks);
                    }
                }
            } else {
                for (group, elem_res) in groups.iter().zip(elem_results) {
                    match elem_res {
                        Ok(rows) => {
                            for &idx in &group.indices {
                                emit_rows_for_index(idx, &rows, max_rows_per_chunk, &mut chunks);
                            }
                        }
                        Err(elem_err) => {
                            for &idx in &group.indices {
                                emit_error_for_index(idx, &elem_err, &mut chunks);
                            }
                        }
                    }
                }
            }
        }
    }
    chunks
}

/// Evaluates `sets` in deduplicated, concurrency-bounded batches against `upstream` (`NGC-096`).
///
/// # Errors
///
/// Synchronously returns `INVALID_ARGUMENT` if `cfg.max_batch_size == 0`, `cfg.max_in_flight == 0`,
/// or `sets` contains duplicate `VariableSet.index` values (`NGC-096` (a), (c)).
pub fn foreach<F, Fut>(
    sets: Vec<VariableSet>,
    cfg: ForeachConfig,
    scope: &CallScope,
    mut upstream: F,
) -> NgcResult<ForeachRun>
where
    F: FnMut(Vec<prost_types::Struct>) -> Fut + Send + 'static,
    Fut: Future<Output = NgcResult<Vec<NgcResult<Vec<prost_types::Struct>>>>> + Send + 'static,
{
    if cfg.max_batch_size == 0 {
        return Err(NgcErrorInfo::new(
            NgcErrorCode::InvalidArgument,
            "foreach max_batch_size must be >= 1 (0 means foreach unsupported, NGC-015)",
        ));
    }
    if cfg.max_in_flight == 0 {
        return Err(NgcErrorInfo::new(
            NgcErrorCode::InvalidArgument,
            "foreach max_in_flight must be >= 1",
        ));
    }

    let mut seen_indices = BTreeSet::new();
    let mut distinct_by_key: BTreeMap<CanonicalValue, usize> = BTreeMap::new();
    let mut distinct_groups: Vec<DistinctGroup> = Vec::new();

    for vs in sets {
        if !seen_indices.insert(vs.index) {
            return Err(NgcErrorInfo::new(
                NgcErrorCode::InvalidArgument,
                format!("duplicate foreach VariableSet.index {}", vs.index),
            ));
        }

        let canon = canonicalize_prost_struct(vs.values.as_ref());
        if let Some(&group_pos) = distinct_by_key.get(&canon) {
            distinct_groups[group_pos].indices.push(vs.index);
        } else {
            let group_pos = distinct_groups.len();
            distinct_by_key.insert(canon, group_pos);
            distinct_groups.push(DistinctGroup {
                values: vs.values.unwrap_or_default(),
                indices: vec![vs.index],
            });
        }
    }

    let counter = Arc::new(AtomicU32::new(0));
    let handle = UpstreamCallsHandle {
        counter: Arc::clone(&counter),
    };

    if distinct_groups.is_empty() {
        return Ok(ForeachRun {
            stream: Box::pin(stream::empty()),
            upstream_calls: handle,
        });
    }

    let batches: Vec<PlannedBatch> = distinct_groups
        .chunks(cfg.max_batch_size)
        .map(|slice| PlannedBatch {
            groups: slice.to_vec(),
        })
        .collect();

    let cancel = scope.cancel.clone();
    let cancel_for_map = cancel.clone();
    let deadline = scope.deadline;
    let max_rows_per_chunk = cfg.max_rows_per_chunk.max(1);

    let batch_stream: Pin<Box<dyn futures_util::Stream<Item = Vec<RowSetChunk>> + Send>> = Box::pin(
        stream::iter(batches)
            .map(move |planned| {
                if cancel_for_map.is_cancelled() || Instant::now() >= deadline {
                    Either::Right(ready(Vec::new()))
                } else {
                    counter.fetch_add(1, Ordering::AcqRel);
                    let inputs: Vec<prost_types::Struct> =
                        planned.groups.iter().map(|g| g.values.clone()).collect();
                    let fut = upstream(inputs);
                    let groups = planned.groups;
                    Either::Left(async move {
                        let outcome = fut.await;
                        build_batch_chunks(&groups, outcome, max_rows_per_chunk)
                    })
                }
            })
            .buffer_unordered(cfg.max_in_flight),
    );

    struct UnfoldState {
        in_flight: Option<Pin<Box<dyn futures_util::Stream<Item = Vec<RowSetChunk>> + Send>>>,
        pending: VecDeque<NgcResult<RowSetChunk>>,
        cancel: CancellationToken,
        deadline: Instant,
        deadline_sleep: Pin<Box<Sleep>>,
        done: bool,
    }

    let state = UnfoldState {
        in_flight: Some(batch_stream),
        pending: VecDeque::new(),
        cancel,
        deadline,
        deadline_sleep: Box::pin(sleep_until(deadline)),
        done: false,
    };

    let chunk_stream: BoxStream<RowSetChunk> =
        Box::pin(stream::unfold(state, |mut st| async move {
            if st.done {
                return None;
            }
            if st.cancel.is_cancelled() {
                st.done = true;
                st.in_flight = None;
                st.pending.clear();
                return Some((
                    Err(NgcErrorInfo::new(
                        NgcErrorCode::Cancelled,
                        "foreach cancelled",
                    )),
                    st,
                ));
            }
            if Instant::now() >= st.deadline {
                st.done = true;
                st.in_flight = None;
                st.pending.clear();
                return Some((
                    Err(NgcErrorInfo::new(
                        NgcErrorCode::DeadlineExceeded,
                        "foreach deadline exceeded",
                    )),
                    st,
                ));
            }

            if let Some(item) = st.pending.pop_front() {
                return Some((item, st));
            }

            loop {
                let batch_s = st.in_flight.as_mut()?;

                let next_batch = tokio::select! {
                    biased;
                    () = st.cancel.cancelled() => {
                        st.done = true;
                        st.in_flight = None;
                        return Some((
                            Err(NgcErrorInfo::new(NgcErrorCode::Cancelled, "foreach cancelled")),
                            st,
                        ));
                    }
                    () = st.deadline_sleep.as_mut() => {
                        st.done = true;
                        st.in_flight = None;
                        return Some((
                            Err(NgcErrorInfo::new(
                                NgcErrorCode::DeadlineExceeded,
                                "foreach deadline exceeded",
                            )),
                            st,
                        ));
                    }
                    maybe_chunks = batch_s.next() => maybe_chunks,
                };

                match next_batch {
                    Some(chunks) => {
                        for ch in chunks {
                            st.pending.push_back(Ok(ch));
                        }
                        if let Some(first) = st.pending.pop_front() {
                            return Some((first, st));
                        }
                    }
                    None => {
                        return None;
                    }
                }
            }
        }));

    Ok(ForeachRun {
        stream: chunk_stream,
        upstream_calls: handle,
    })
}
