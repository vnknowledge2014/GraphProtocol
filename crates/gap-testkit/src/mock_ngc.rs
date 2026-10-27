//! Programmable in-process `MockNgcConnector` over `serde_json::Value` and `gap-core` types
//! (`NGC-020`, `NGC-026`, `INV-02`).
//!
//! `WP-M1-03` owns the `gap-ngc-sdk` `Connector` trait, so this mock intentionally avoids
//! inventing that trait and instead provides deterministic per-operation scripting, paused-clock
//! latency via `tokio::time::sleep`, call recording without holding locks across `.await`, and
//! `NGC-020` `foreach` streaming semantics.

use std::collections::{BTreeMap, VecDeque};
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::Poll;
use std::time::Duration;

use gap_core::error::{NgcErrorCode, NgcErrorInfo};
use serde_json::Value;

/// Deterministic interleaving policy for `MockNgcConnector::call_foreach` (`NGC-020`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ForeachInterleaving {
    /// Emits chunks ordered by ascending `foreach_index` (`0..n`).
    #[default]
    InOrder,
    /// Emits indices in descending order (`(0..n).rev()`), keeping intra-index chunk order intact.
    Reversed,
    /// Deterministically interleaves chunks across indices using `seed` while preserving
    /// intra-index order so every index's `last_for_index = true` chunk closes its stream.
    SeededShuffle(u64),
}

/// Scripted behavior for a single operation invocation (or a single `foreach` variable set).
#[derive(Debug, Clone, PartialEq)]
pub struct ScriptStep {
    /// Simulated upstream execution latency awaited via `tokio::time::sleep`.
    pub latency: Duration,
    /// Final outcome (`Ok(Value)` or `Err(NgcErrorInfo)`).
    pub outcome: Result<Value, NgcErrorInfo>,
    /// Optional non-closing chunks emitted for this index before the final closing chunk in
    /// `call_foreach` (`last_for_index = false`).
    pub pre_chunks: Vec<Value>,
}

impl ScriptStep {
    /// Creates an immediate `Ok(value)` step (`Duration::ZERO` latency).
    #[must_use]
    pub const fn ok(value: Value) -> Self {
        Self {
            latency: Duration::ZERO,
            outcome: Ok(value),
            pre_chunks: Vec::new(),
        }
    }

    /// Creates an immediate `Err(error)` step (`Duration::ZERO` latency).
    #[must_use]
    pub const fn err(error: NgcErrorInfo) -> Self {
        Self {
            latency: Duration::ZERO,
            outcome: Err(error),
            pre_chunks: Vec::new(),
        }
    }

    /// Attaches simulated latency (awaited through `tokio::time::sleep`).
    #[must_use]
    pub const fn with_latency(mut self, latency: Duration) -> Self {
        self.latency = latency;
        self
    }

    /// Attaches intermediate non-closing chunks emitted before the closing `last_for_index = true`
    /// chunk during `call_foreach`.
    #[must_use]
    pub fn with_pre_chunks(mut self, pre_chunks: Vec<Value>) -> Self {
        self.pre_chunks = pre_chunks;
        self
    }
}

/// Category of recorded call on [`MockNgcConnector`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallKind {
    /// Single unary call (`MockNgcConnector::call`).
    Unary,
    /// Batched `foreach` call (`MockNgcConnector::call_foreach`).
    Foreach,
}

/// Audit record of an invocation against [`MockNgcConnector`].
#[derive(Debug, Clone, PartialEq)]
pub struct RecordedCall {
    /// Target operation identifier/name.
    pub operation: String,
    /// Unary vs `foreach` call kind.
    pub kind: CallKind,
    /// Base arguments passed by the caller.
    pub arguments: Value,
    /// Variable sets supplied to `call_foreach` (empty for `Unary`).
    pub foreach_sets: Vec<Value>,
}

/// Streamed chunk emitted by `MockNgcConnector::call_foreach` (`NGC-020`).
#[derive(Debug, Clone, PartialEq)]
pub struct MockForeachChunk {
    /// Zero-based index into the request's `foreach` variable sets (`NGC-020`).
    pub foreach_index: u32,
    /// Payload for this chunk (`None` when `error` is `Some`).
    pub data: Option<Value>,
    /// Per-index error reported on the closing chunk when this index failed (`NGC-020`, `NGC-062`).
    pub error: Option<NgcErrorInfo>,
    /// `true` iff this is the single closing chunk for `foreach_index` (`NGC-020`).
    pub last_for_index: bool,
}

/// Complete response of a `MockNgcConnector::call_foreach` invocation (`NGC-020`, `NGC-026`).
#[derive(Debug, Clone, PartialEq)]
pub struct ForeachResult {
    /// Deterministically interleaved chunks across all `foreach` indices.
    pub chunks: Vec<MockForeachChunk>,
    /// Honest upstream batch call count (`ceil(n / upstream_batch_size)`, `NGC-026`).
    pub upstream_calls: u32,
}

type DynamicResponder =
    Arc<dyn Fn(u32, &Value, Option<&Value>) -> ScriptStep + Send + Sync + 'static>;

#[derive(Default)]
struct OperationScript {
    queue: VecDeque<ScriptStep>,
    responder: Option<DynamicResponder>,
}

struct MockState {
    scripts: BTreeMap<String, OperationScript>,
    calls: Vec<RecordedCall>,
    interleaving: ForeachInterleaving,
    upstream_batch_size: u32,
}

/// Programmable in-process mock NGC connector (`NGC-020`, `NGC-026`).
#[derive(Clone)]
pub struct MockNgcConnector {
    state: Arc<Mutex<MockState>>,
}

impl fmt::Debug for MockNgcConnector {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (scripted_ops, call_count, interleaving, batch_size) = match self.state.lock() {
            Ok(guard) => (
                guard.scripts.len(),
                guard.calls.len(),
                guard.interleaving,
                guard.upstream_batch_size,
            ),
            Err(poisoned) => {
                let guard = poisoned.into_inner();
                (
                    guard.scripts.len(),
                    guard.calls.len(),
                    guard.interleaving,
                    guard.upstream_batch_size,
                )
            }
        };
        f.debug_struct("MockNgcConnector")
            .field("scripted_operations", &scripted_ops)
            .field("recorded_calls", &call_count)
            .field("interleaving", &interleaving)
            .field("upstream_batch_size", &batch_size)
            .finish()
    }
}

impl Default for MockNgcConnector {
    fn default() -> Self {
        Self::new()
    }
}

impl MockNgcConnector {
    /// Creates an empty [`MockNgcConnector`] with `ForeachInterleaving::InOrder` and
    /// `upstream_batch_size = 64`.
    #[must_use]
    pub fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(MockState {
                scripts: BTreeMap::new(),
                calls: Vec::new(),
                interleaving: ForeachInterleaving::InOrder,
                upstream_batch_size: 64,
            })),
        }
    }

    /// Builder-style configuration of [`ForeachInterleaving`].
    #[must_use]
    pub fn with_interleaving(self, interleaving: ForeachInterleaving) -> Self {
        self.set_interleaving(interleaving);
        self
    }

    /// Builder-style configuration of `upstream_batch_size` (`>= 1`, used for `NGC-026`
    /// `upstream_calls` accounting).
    #[must_use]
    pub fn with_upstream_batch_size(self, batch_size: u32) -> Self {
        self.set_upstream_batch_size(batch_size);
        self
    }

    /// Updates [`ForeachInterleaving`] for subsequent `call_foreach` executions.
    pub fn set_interleaving(&self, interleaving: ForeachInterleaving) {
        let mut guard = self.lock_state();
        guard.interleaving = interleaving;
    }

    /// Updates `upstream_batch_size` (clamped to `>= 1`).
    pub fn set_upstream_batch_size(&self, batch_size: u32) {
        let mut guard = self.lock_state();
        guard.upstream_batch_size = batch_size.max(1);
    }

    /// Appends a single [`ScriptStep`] to the FIFO script queue for `operation`.
    pub fn push_step(&self, operation: impl Into<String>, step: ScriptStep) {
        let mut guard = self.lock_state();
        guard
            .scripts
            .entry(operation.into())
            .or_default()
            .queue
            .push_back(step);
    }

    /// Appends a sequence of [`ScriptStep`]s to the FIFO script queue for `operation`.
    pub fn script_operation<I>(&self, operation: impl Into<String>, steps: I)
    where
        I: IntoIterator<Item = ScriptStep>,
    {
        let mut guard = self.lock_state();
        let entry = guard.scripts.entry(operation.into()).or_default();
        entry.queue.extend(steps);
    }

    /// Installs a dynamic fallback responder for `operation` invoked whenever its FIFO queue is
    /// empty. Receives `(foreach_index, base_arguments, optional_foreach_var_set)`.
    pub fn on_operation<F>(&self, operation: impl Into<String>, responder: F)
    where
        F: Fn(u32, &Value, Option<&Value>) -> ScriptStep + Send + Sync + 'static,
    {
        let mut guard = self.lock_state();
        guard.scripts.entry(operation.into()).or_default().responder = Some(Arc::new(responder));
    }

    /// Returns a snapshot of all recorded calls in invocation order.
    #[must_use]
    pub fn recorded_calls(&self) -> Vec<RecordedCall> {
        self.lock_state().calls.clone()
    }

    /// Clears recorded calls without altering scripted operation queues.
    pub fn clear_recorded_calls(&self) {
        self.lock_state().calls.clear();
    }

    /// Executes a unary operation call, awaiting scripted latency via `tokio::time::sleep` without
    /// holding any lock across `.await`.
    ///
    /// # Errors
    ///
    /// Returns [`NgcErrorInfo`] if the scripted step failed or no script/responder was registered.
    pub async fn call(&self, operation: &str, arguments: Value) -> Result<Value, NgcErrorInfo> {
        let step = self.prepare_unary(operation, arguments);
        if !step.latency.is_zero() {
            tokio::time::sleep(step.latency).await;
        }
        step.outcome
    }

    /// Executes a batched `foreach` query per `NGC-020`:
    ///
    /// - Evaluates each variable set independently (`0..foreach_sets.len()`).
    /// - Awaits scripted per-index latencies concurrently via `tokio::time::sleep` (paused-clock
    ///   compatible, zero locks across `.await`).
    /// - Tags every emitted chunk with `foreach_index`.
    /// - Emits **exact one** closing chunk (`last_for_index = true`) per index as the final chunk
    ///   for that index.
    /// - Isolates any failing index's `NgcErrorInfo` to that index's closing chunk without
    ///   affecting other indices.
    /// - Interleaves chunks deterministically according to [`ForeachInterleaving`].
    pub async fn call_foreach(
        &self,
        operation: &str,
        base_arguments: Value,
        foreach_sets: Vec<Value>,
    ) -> ForeachResult {
        let (steps, interleaving, upstream_calls) =
            self.prepare_foreach(operation, base_arguments, foreach_sets);
        if steps.is_empty() {
            return ForeachResult {
                chunks: Vec::new(),
                upstream_calls: 0,
            };
        }

        let latencies: Vec<Duration> = steps.iter().map(|s| s.latency).collect();
        await_concurrent_sleeps(&latencies).await;

        let mut per_index_queues: Vec<VecDeque<MockForeachChunk>> = Vec::with_capacity(steps.len());
        for (idx, step) in steps.into_iter().enumerate() {
            let foreach_index = u32::try_from(idx).unwrap_or(u32::MAX);
            let mut q = VecDeque::with_capacity(step.pre_chunks.len() + 1);
            for pre in step.pre_chunks {
                q.push_back(MockForeachChunk {
                    foreach_index,
                    data: Some(pre),
                    error: None,
                    last_for_index: false,
                });
            }
            match step.outcome {
                Ok(val) => q.push_back(MockForeachChunk {
                    foreach_index,
                    data: Some(val),
                    error: None,
                    last_for_index: true,
                }),
                Err(err) => q.push_back(MockForeachChunk {
                    foreach_index,
                    data: None,
                    error: Some(err),
                    last_for_index: true,
                }),
            }
            per_index_queues.push(q);
        }

        let chunks = interleave_queues(per_index_queues, interleaving);
        ForeachResult {
            chunks,
            upstream_calls,
        }
    }

    fn prepare_unary(&self, operation: &str, arguments: Value) -> ScriptStep {
        let mut guard = self.lock_state();
        guard.calls.push(RecordedCall {
            operation: operation.to_owned(),
            kind: CallKind::Unary,
            arguments: arguments.clone(),
            foreach_sets: Vec::new(),
        });
        next_step_from_state(&mut guard, operation, 0, &arguments, None)
    }

    fn prepare_foreach(
        &self,
        operation: &str,
        base_arguments: Value,
        foreach_sets: Vec<Value>,
    ) -> (Vec<ScriptStep>, ForeachInterleaving, u32) {
        let mut guard = self.lock_state();
        let interleaving = guard.interleaving;
        let batch_size = guard.upstream_batch_size.max(1);
        let n_u32 = u32::try_from(foreach_sets.len()).unwrap_or(u32::MAX);
        let upstream_calls = if n_u32 == 0 {
            0
        } else {
            n_u32.div_ceil(batch_size)
        };

        let mut steps = Vec::with_capacity(foreach_sets.len());
        for (idx, var_set) in foreach_sets.iter().enumerate() {
            let idx_u32 = u32::try_from(idx).unwrap_or(u32::MAX);
            steps.push(next_step_from_state(
                &mut guard,
                operation,
                idx_u32,
                &base_arguments,
                Some(var_set),
            ));
        }

        guard.calls.push(RecordedCall {
            operation: operation.to_owned(),
            kind: CallKind::Foreach,
            arguments: base_arguments,
            foreach_sets,
        });

        (steps, interleaving, upstream_calls)
    }

    fn lock_state(&self) -> std::sync::MutexGuard<'_, MockState> {
        match self.state.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }
}

fn next_step_from_state(
    state: &mut MockState,
    operation: &str,
    foreach_index: u32,
    base_args: &Value,
    var_set: Option<&Value>,
) -> ScriptStep {
    if let Some(script) = state.scripts.get_mut(operation) {
        if let Some(step) = script.queue.pop_front() {
            return step;
        }
        if let Some(ref responder) = script.responder {
            return responder(foreach_index, base_args, var_set);
        }
    }
    ScriptStep::err(NgcErrorInfo::new(
        NgcErrorCode::Unimplemented,
        format!("no mock script configured for operation `{operation}`"),
    ))
}

async fn await_concurrent_sleeps(latencies: &[Duration]) {
    let mut timers: Vec<Option<Pin<Box<tokio::time::Sleep>>>> = latencies
        .iter()
        .filter(|d| !d.is_zero())
        .map(|&d| Some(Box::pin(tokio::time::sleep(d))))
        .collect();

    if timers.is_empty() {
        return;
    }

    std::future::poll_fn(|cx| {
        let mut any_pending = false;
        for slot in &mut timers {
            if let Some(sleep_fut) = slot {
                if sleep_fut.as_mut().poll(cx).is_ready() {
                    *slot = None;
                } else {
                    any_pending = true;
                }
            }
        }
        if any_pending {
            Poll::Pending
        } else {
            Poll::Ready(())
        }
    })
    .await;
}

fn interleave_queues(
    mut queues: Vec<VecDeque<MockForeachChunk>>,
    mode: ForeachInterleaving,
) -> Vec<MockForeachChunk> {
    let total_chunks: usize = queues.iter().map(VecDeque::len).sum();
    let mut out = Vec::with_capacity(total_chunks);

    match mode {
        ForeachInterleaving::InOrder => {
            for mut q in queues {
                out.extend(q.drain(..));
            }
        }
        ForeachInterleaving::Reversed => {
            for mut q in queues.into_iter().rev() {
                out.extend(q.drain(..));
            }
        }
        ForeachInterleaving::SeededShuffle(seed) => {
            queues.retain(|q| !q.is_empty());
            let mut rng_state = seed ^ 0x9e37_79b9_7f4a_7c15;
            while !queues.is_empty() {
                rng_state = splitmix64_next(&mut rng_state);
                let pick = (rng_state as usize) % queues.len();
                if let Some(chunk) = queues[pick].pop_front() {
                    out.push(chunk);
                }
                if queues[pick].is_empty() {
                    queues.remove(pick);
                }
            }
        }
    }

    out
}

fn splitmix64_next(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    // spec: NGC-020, INV-02
    #[tokio::test(start_paused = true)]
    async fn unary_script_awaits_paused_clock_latency_outcomes_and_records_calls() {
        let mock = MockNgcConnector::new();
        mock.script_operation(
            "billing.getInvoice",
            [
                ScriptStep::ok(json!({"id": "inv-1", "total": 100}))
                    .with_latency(Duration::from_millis(150)),
                ScriptStep::err(
                    NgcErrorInfo::new(NgcErrorCode::DeadlineExceeded, "upstream timed out")
                        .with_retry(true, 200),
                )
                .with_latency(Duration::from_millis(40)),
            ],
        );

        let t0 = tokio::time::Instant::now();
        let res1 = mock
            .call("billing.getInvoice", json!({"id": "inv-1"}))
            .await
            .expect("first scripted call should succeed");
        assert_eq!(res1, json!({"id": "inv-1", "total": 100}));
        assert_eq!(t0.elapsed(), Duration::from_millis(150));

        let err2 = mock
            .call("billing.getInvoice", json!({"id": "inv-2"}))
            .await
            .expect_err("second scripted call should fail");
        assert_eq!(err2.code, NgcErrorCode::DeadlineExceeded);
        assert!(err2.retryable);
        assert_eq!(err2.retry_after_ms, 200);
        assert_eq!(t0.elapsed(), Duration::from_millis(190));

        // Exhausted queue without responder returns typed `Unimplemented`.
        let err3 = mock
            .call("billing.getInvoice", json!({"id": "inv-3"}))
            .await
            .expect_err("unscripted call should return Unimplemented");
        assert_eq!(err3.code, NgcErrorCode::Unimplemented);

        let calls = mock.recorded_calls();
        assert_eq!(calls.len(), 3);
        assert_eq!(calls[0].operation, "billing.getInvoice");
        assert_eq!(calls[0].kind, CallKind::Unary);
        assert_eq!(calls[0].arguments, json!({"id": "inv-1"}));
    }

    // spec: NGC-020, NGC-026
    #[tokio::test(start_paused = true)]
    async fn ngc_020_foreach_tags_indices_emits_single_closing_chunk_and_isolates_failures() {
        let mock = MockNgcConnector::new().with_upstream_batch_size(2);
        mock.script_operation(
            "billing.listItems",
            [
                ScriptStep::ok(json!({"rows": ["a2"]}))
                    .with_pre_chunks(vec![json!({"rows": ["a1"]})])
                    .with_latency(Duration::from_millis(50)),
                ScriptStep::err(NgcErrorInfo::new(
                    NgcErrorCode::PermissionDenied,
                    "row forbidden",
                ))
                .with_latency(Duration::from_millis(120)),
                ScriptStep::ok(json!({"rows": ["c1"]})).with_latency(Duration::from_millis(30)),
            ],
        );

        let t0 = tokio::time::Instant::now();
        let out = mock
            .call_foreach(
                "billing.listItems",
                json!({"currency": "USD"}),
                vec![json!({"id": "a"}), json!({"id": "b"}), json!({"id": "c"})],
            )
            .await;

        // Concurrent per-index timers complete in max(50, 120, 30) = 120ms, not 200ms!
        assert_eq!(t0.elapsed(), Duration::from_millis(120));
        // Honest batch count: ceil(3 / 2) == 2 (`NGC-026`).
        assert_eq!(out.upstream_calls, 2);

        // Total chunks: 2 for index 0 + 1 for index 1 + 1 for index 2 = 4.
        assert_eq!(out.chunks.len(), 4);

        // Check `NGC-020` invariants per index:
        for expected_idx in 0_u32..3 {
            let idx_chunks: Vec<&MockForeachChunk> = out
                .chunks
                .iter()
                .filter(|c| c.foreach_index == expected_idx)
                .collect();
            assert!(!idx_chunks.is_empty());
            let closing_count = idx_chunks.iter().filter(|c| c.last_for_index).count();
            assert_eq!(
                closing_count, 1,
                "index {expected_idx} must have exact one closing chunk"
            );
            assert!(
                idx_chunks.last().expect("non-empty").last_for_index,
                "closing chunk must be the final chunk for index {expected_idx}"
            );
        }

        // Index 1 failed with `PermissionDenied` without affecting indices 0 or 2.
        let idx1_closing = out
            .chunks
            .iter()
            .find(|c| c.foreach_index == 1 && c.last_for_index)
            .expect("index 1 closing");
        assert!(idx1_closing.data.is_none());
        assert_eq!(
            idx1_closing.error.as_ref().map(|e| e.code),
            Some(NgcErrorCode::PermissionDenied)
        );

        let idx0_closing = out
            .chunks
            .iter()
            .find(|c| c.foreach_index == 0 && c.last_for_index)
            .expect("index 0 closing");
        assert_eq!(idx0_closing.data, Some(json!({"rows": ["a2"]})));
        assert!(idx0_closing.error.is_none());

        let idx2_closing = out
            .chunks
            .iter()
            .find(|c| c.foreach_index == 2 && c.last_for_index)
            .expect("index 2 closing");
        assert_eq!(idx2_closing.data, Some(json!({"rows": ["c1"]})));
        assert!(idx2_closing.error.is_none());

        let recorded = mock.recorded_calls();
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0].kind, CallKind::Foreach);
        assert_eq!(recorded[0].foreach_sets.len(), 3);
    }

    // spec: NGC-020
    #[tokio::test(start_paused = true)]
    async fn ngc_020_foreach_interleaving_in_order_reversed_and_seeded_shuffle_are_deterministic() {
        let make_mock = |mode: ForeachInterleaving| {
            let m = MockNgcConnector::new().with_interleaving(mode);
            m.on_operation("crm.lookup", |idx, _base, var_set| {
                let key = var_set
                    .and_then(|v| v.get("k"))
                    .cloned()
                    .unwrap_or(Value::Null);
                ScriptStep::ok(json!({"idx": idx, "k": key, "phase": "final"}))
                    .with_pre_chunks(vec![json!({"idx": idx, "k": key, "phase": "part1"})])
            });
            m
        };

        let sets = vec![
            json!({"k": 0}),
            json!({"k": 1}),
            json!({"k": 2}),
            json!({"k": 3}),
        ];

        // 1. InOrder
        let in_order = make_mock(ForeachInterleaving::InOrder)
            .call_foreach("crm.lookup", json!({}), sets.clone())
            .await;
        let in_order_indices: Vec<(u32, bool)> = in_order
            .chunks
            .iter()
            .map(|c| (c.foreach_index, c.last_for_index))
            .collect();
        assert_eq!(
            in_order_indices,
            vec![
                (0, false),
                (0, true),
                (1, false),
                (1, true),
                (2, false),
                (2, true),
                (3, false),
                (3, true),
            ]
        );

        // 2. Reversed
        let reversed = make_mock(ForeachInterleaving::Reversed)
            .call_foreach("crm.lookup", json!({}), sets.clone())
            .await;
        let reversed_indices: Vec<(u32, bool)> = reversed
            .chunks
            .iter()
            .map(|c| (c.foreach_index, c.last_for_index))
            .collect();
        assert_eq!(
            reversed_indices,
            vec![
                (3, false),
                (3, true),
                (2, false),
                (2, true),
                (1, false),
                (1, true),
                (0, false),
                (0, true),
            ]
        );

        // 3. SeededShuffle: deterministic across runs, distinct from InOrder, and never emits a
        // chunk for index `k` after `last_for_index = true` for `k`!
        let s1 = make_mock(ForeachInterleaving::SeededShuffle(0xdead_beef_cafe_babe))
            .call_foreach("crm.lookup", json!({}), sets.clone())
            .await;
        let s2 = make_mock(ForeachInterleaving::SeededShuffle(0xdead_beef_cafe_babe))
            .call_foreach("crm.lookup", json!({}), sets.clone())
            .await;
        assert_eq!(s1, s2, "same seed must produce identical chunk sequence");

        let shuffled_indices: Vec<(u32, bool)> = s1
            .chunks
            .iter()
            .map(|c| (c.foreach_index, c.last_for_index))
            .collect();
        assert_ne!(shuffled_indices, in_order_indices);
        assert_ne!(shuffled_indices, reversed_indices);

        let mut closed = [false; 4];
        let mut close_counts = [0_usize; 4];
        for chunk in &s1.chunks {
            let idx = chunk.foreach_index as usize;
            assert!(
                !closed[idx],
                "no chunk for index {idx} may appear after last_for_index=true"
            );
            if chunk.last_for_index {
                closed[idx] = true;
                close_counts[idx] += 1;
            }
        }
        assert_eq!(close_counts, [1, 1, 1, 1]);
    }

    // spec: NGC-020, NGC-026
    #[tokio::test(start_paused = true)]
    async fn f1_foreach_empty_reports_zero_upstream_calls_and_three_items_report_one_batched_call()
    {
        let mock = MockNgcConnector::new();
        mock.on_operation("crm.batch", |idx, _base, _var| {
            ScriptStep::ok(json!({"idx": idx}))
        });

        let empty_out = mock.call_foreach("crm.batch", json!({}), Vec::new()).await;
        assert_eq!(empty_out.upstream_calls, 0);
        assert!(empty_out.chunks.is_empty());

        let three_out = mock
            .call_foreach(
                "crm.batch",
                json!({}),
                vec![json!({"id": 1}), json!({"id": 2}), json!({"id": 3})],
            )
            .await;
        assert_eq!(three_out.upstream_calls, 1);
        assert_eq!(three_out.chunks.len(), 3);
    }

    // spec: NGC-020
    #[tokio::test(start_paused = true)]
    async fn f2_seeded_shuffle_matches_golden_permutations_for_eight_items_and_varies_by_seed() {
        let run_for_seed = |seed: u64| async move {
            let mock =
                MockNgcConnector::new().with_interleaving(ForeachInterleaving::SeededShuffle(seed));
            mock.on_operation("crm.shuffle8", |idx, _base, _var| {
                ScriptStep::ok(json!({"idx": idx}))
            });
            let sets: Vec<Value> = (0_u32..8).map(|i| json!({"i": i})).collect();
            let res = mock.call_foreach("crm.shuffle8", json!({}), sets).await;
            res.chunks
                .into_iter()
                .map(|c| c.foreach_index)
                .collect::<Vec<u32>>()
        };

        let order_42 = run_for_seed(42).await;
        assert_eq!(order_42, vec![3, 0, 2, 6, 7, 5, 4, 1]);

        let order_99 = run_for_seed(99).await;
        assert_eq!(order_99, vec![2, 0, 5, 7, 6, 1, 4, 3]);
        assert_ne!(order_42, order_99);

        for seed in [0_u64, 1, 42, 99, 12_345] {
            let mut sorted = run_for_seed(seed).await;
            sorted.sort_unstable();
            assert_eq!(sorted, vec![0, 1, 2, 3, 4, 5, 6, 7]);
        }
    }
}
