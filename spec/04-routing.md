# GAP Specification — 04 Discovery, Routing, Calibration, Balancing

Normative IDs `RTE-*`. Rationale: ADR-003, 005, 006, 007, 008, 010, 011. Scope: the **discovery plane** (`__search`, `gap.resolve`, `__metaGraph`) and the **replica balancer / circuit breaker** used by the execution plane. Typed GAP-QL execution never calls a model (ARC-002).

## 1. Component map
```text
query ─► sanitize ─► [cache] ─► embed ─► retrieve(dense ⊕ BM25, visibility-filtered)
      ─► group (≤15 capability groups + sink) ─► RoutingModel (baseline | S1)
      ─► calibrate (temperature) ─► conformal decision ─► candidates / abstention
```
Two trait levels: `RoutingModel` (high level, `gap-route`; implemented by `BaselineRouter` — always available — and by `S1Router`, the adapter that formats options per RTE-042) and `DecisionBackend` (low level, declared in `gap-route`, implemented in `gap-s1` by `LayaCandle` / `LayaOrt` / `HttpSidecar`, ADR-005). Embedding goes through `gap_core::Embedder` (implemented in `gap-models`). The *replica balancer* (§8, crate `gap-balance`) is separate, deterministic, ML-free, and used by the execution plane.

## 2. ElementIndex
- **RTE-001** Every searchable element becomes an `ElementDoc { element_id, kind, branch_uid, group_id?, name, path, text, signature, examples[], trust, side_effect, required_scopes[], tokens_est }` where `text` = sanitized description (≤ 600 chars), `signature` = SDL signature of the element, `examples` = positive examples (negatives are used only by RouteBench).
- **RTE-002** *Dense channel:* embedding model id + sha256 pinned in the snapshot (`index.embedding_model`); default `bge-small-en-v1.5` (384-d, L2-normalized, cosine). Query and docs use the model's documented prefix conventions. Changing the model ⇒ new snapshot generation with a full re-embed; mixing models in one snapshot is forbidden.
- **RTE-003** *Lexical channel:* BM25 (k1 = 1.2, b = 0.75) over fields `name` (boost 3.0), `path` (2.0), `text` (1.0), `examples` (1.5); tokenization: Unicode word boundaries + identifier splitting (`camelCase`, `snake_case`, digits), ASCII-folding, no stemming in 1.0.
- **RTE-004** *Fusion:* reciprocal rank fusion `rrf(d) = Σ_c 1/(60 + rank_c(d))` over the top-64 of each channel; reported `score = rrf / (2/61)` ∈ [0, 1].
- **RTE-005** Index search is exact (flat SIMD) for ≤ 20,000 vectors; above that HNSW (M = 16, efConstruction = 200, efSearch = 64) with recall@10 ≥ 0.99 against exact on the bench corpus (gate, WP-M2-04).
- **RTE-006** Index construction is deterministic for a given input set and model (stable ordering by `element_id`), so equal inputs yield equal `index_hash`.

## 3. Visibility filtering (before ranking)
- **RTE-010** For each request, the set of visible elements is computed from policy action `Discover` on `(principal, element)`; implementations MAY cache bitsets per `(tenant, policy_version, principal_class)` where `principal_class` is a hash of the principal's scopes/attributes relevant to the policy. Both channels receive the bitset as a filter *during* search (not post-filtering after truncation), so recall is not reduced by hidden items (INV-05).
- **RTE-011** Candidate counts, `tokensEstimate`, set sizes and abstention decisions are computed over visible elements only. If the visible set is empty: `ABSTAINED(EMPTY_VISIBLE_SET)`.

## 4. Input hygiene
- **RTE-020** Before any embedding or model input, every string (query, option text, state values) MUST pass `sanitize()`: Unicode NFKC; delete C0/C1 controls except `\n\t`; delete bidi controls (U+202A–202E, U+2066–2069) and zero-width characters (U+200B–200D, U+2060, U+FEFF); replace with a single space every occurrence of tokenizer special tokens and markers `[MASK] [CLS] [SEP] [PAD] [UNK] <mask> <s> </s> <pad> <unk>` (case-insensitive) and any `<|…|>` span; collapse whitespace; truncate at `max_query_chars` (default 2,000) on a code-point boundary.
- **RTE-021** `sanitize()` is idempotent; property test: for random Unicode input, `sanitize(sanitize(x)) == sanitize(x)` and the output contains none of the listed markers (even when constructed via normalization, e.g. fullwidth `［ＭＡＳＫ］`).
- **RTE-022** Strings originating from branches additionally pass the injection linter (SEC-081): instruction-like imperatives addressed to the model ("ignore previous…", "you must…"), URLs with credentials, hidden-text patterns. Flagged elements are excluded from S1 inputs and marked `lint=flagged` in admin views; they remain searchable by lexical channel only after admin review.

## 5. `gap.resolve` pipeline
| # | Step | Budget (p95, CPU ref env) | Rule |
|---|---|---|---|
| 1 | normalize + sanitize | 0.2 ms | RTE-020 |
| 2 | response cache lookup | 0.1 ms | key `(tenant, principal_class, generation, calibration_id, router_id, blake3(sanitized_query), constraints_hash)`, TTL 60 s, moka |
| 3 | embed query | 15 ms | embedding cache by sanitized text (TTL 10 min) |
| 4 | retrieve + fuse | 5 ms (≤ 20k vectors) | RTE-002..006, visibility filter RTE-010 |
| 5 | constraints filter | 0.1 ms | drop elements with `side_effect > constraints.maxSideEffect` (default `NONE`) *before* ranking (RTE-032) |
| 6 | group + select | 0.2 ms | RTE-030 |
| 7 | rank | baseline 0.1 ms · S1 100–150 ms CPU / 30–40 ms GPU (vendor figures, to be re-measured at M5) | §6 |
| 8 | calibrate + decide | 0.1 ms | §7 |
| 9 | build candidates | 1 ms | RTE-031 |

- **RTE-030** Group retrieved elements by `group_id`, keep the top **K = 15** groups by best element score, and append the sink option `insufficient_evidence`. Options passed to any `RoutingModel` are exactly these ≤ 16 entries. (`K` is a Limit: range 4–15.)
- **RTE-031** `skeleton` generation is deterministic: pick the group's primary operation (highest-scoring healthy, visible, policy-permitted member); required arguments become variables `$argName` with GraphQL types from the operation signature; selection = declared key fields + up to 3 fields flagged `primary` by ingestion (else the first 3 scalar fields); `variablesSchema` is the JSON Schema of the variables. The skeleton MUST validate (GQL-012).
- **RTE-032** Mutating suggestions require `constraints.maxSideEffect ≥ level`; `UNKNOWN` is treated as `IRREVERSIBLE` for this comparison.
- **RTE-033** Result order is by calibrated probability desc, ties by `group_id`. The response never contains more than `constraints.maxCandidates` (default 5, max 10) candidates.
- **RTE-034** `__search` uses steps 1, 3–4 only (no grouping, no model) and returns per-element hits.

## 6. RoutingModel contract and S1 input format
```rust,ignore
#[async_trait] pub trait RoutingModel: Send + Sync {
    fn id(&self) -> &ModelId;                          // includes weights sha256
    async fn rank(&self, q: &SanitizedQuery, opts: &[GroupOption], ctx: &RankCtx) -> Result<RankOutput, RouteError>;
}
pub struct GroupOption { pub key: String /* "g0".."g14" | "insufficient_evidence" */, pub text: String, pub prior: f32 /* retrieval score */ }
pub struct RankOutput { pub logits: Vec<f32>, pub latency: Duration }   // calibration is applied by gap-route, not by backends
```
- **RTE-040 (abstention emission)** When the outcome is `ABSTAINED` with reason `NO_MATCH` or `LOW_CONFIDENCE`, emit one `UnmappedDomainEvent` (§10).
- **RTE-041** Backends MUST return raw logits aligned to `opts`; probabilities are produced by `softmax(logits / T_b)` in `gap-route` with `T_b = clamp(T̂_b, 0.5, 5.0)` from the active `CalibrationTable`.
- **RTE-042** *S1 question format* (backend `LayaCandle/LayaOrt/HttpSidecar`): one `choice` question with `state` = JCS-canonical JSON `{"query": <sanitized>, "constraints": {"max_side_effect": "<level>"}}` and options keyed `g0…`, each described by `text` truncated to `⌊0.8·head_max_len / n_options⌋` model tokens (≥ 8). The sink option text is fixed: "None of the other options can fulfil the request." Option order = descending retrieval score (deterministic). Branch/operation **ids and trust labels never enter model text**.
- **RTE-043** *Debias (optional, `s1.debias = true`)*: run a second pass with reversed option order and average logits per option; adopt by default only if RouteBench shows a gain ≥ 1 point top-1 at ≤ 2× latency.
- **RTE-044** Backend failure/timeout (`s1.timeout_ms`, default 400) ⇒ fall back to `BaselineRouter` for this request and add warning `GAP_DISCOVERY_DEGRADED`; a rolling failure rate > 20% over 1 min flips the router to baseline for 60 s (INV-17).
- **RTE-045** ML concurrency: at most `s1.max_concurrency` (default = physical cores / 2) concurrent forward passes, queue depth ≤ 2× that; overflow degrades to baseline (RTE-044) rather than queueing unboundedly.
- **RTE-046** Identical `(model_id, canonical state, options)` results are cached (moka, TTL 10 min) — the application-level realization of "encode state once" (ADR-004).

## 7. Calibration and abstention
### 7.1 Data (ADR-007)
- **RTE-060** `bench/routebench` builds examples `(query, visible_options, gold_group | insufficient_evidence)` from: positive/negative `examples` in `OperationDescriptor`; LLM-synthesized paraphrases and sibling-operation hard negatives; out-of-domain negatives (random intents from other corpora and adversarial near-misses). Splits are by `operation_id` (no leakage): 60% fit / 20% calibration / 20% test. Held-out *branches* (10%) measure cold-start behaviour.
- **RTE-061** Metrics (all reported with 95% bootstrap CI): top-1, top-3, MRR, ECE₁₅, Brier, NLL, selective accuracy at coverage {50, 70, 90}%, abstention AUROC, conformal empirical coverage, mean set size, ambiguity rate, abstention precision on OOD negatives.

### 7.2 Temperature scaling
- **RTE-062** For each bucket-family `b = (qtype="choice", n_options_bucket ∈ {2, 3–5, 6–10, 11–16})` fit `T̂_b = argmin_T NLL(softmax(z/T), y)` on the fit split with `T ∈ [0.5, 5.0]` (golden-section on log T). Store `{b, T̂_b, n, ECE_before, ECE_after}`.
- **RTE-063** Baseline router scores are converted to logits by `z = score / τ` with `τ` fit by the same procedure (so the baseline is also calibrated and conformal-ready).

### 7.3 Split-conformal decision (ADR-008)
- **RTE-064** On the calibration split compute, per bucket-family, nonconformity `s_i = 1 − p_i(y_i)` and `q̂_b = s_{(⌈(n+1)(1−α)⌉)}` (the ⌈(n+1)(1−α)⌉-th smallest). Requires `n ≥ 500`; else `calibration = "fallback"` and thresholds `{2: 0.75, 3–5: 0.60, 6–10: 0.45, 11–16: 0.35}` on `max p` apply (vendor-style; flagged in responses and metrics).
- **RTE-065** At inference: `C(x) = { o : 1 − p_o ≤ q̂_b }`. Outcome: `C = {sink}` or `C = ∅` ⇒ ABSTAINED(LOW_CONFIDENCE if `C=∅` else NO_MATCH); `|C ∖ {sink}| = 1` and `sink ∉ C` ⇒ CONFIDENT; otherwise AMBIGUOUS with candidates = `C ∖ {sink}` ordered by `p` (max 5; truncated sets are flagged).
- **RTE-066** `α` per request = 0.05 if any option in play has `side_effect ≠ NONE` (after RTE-032 filtering this only occurs when the caller allowed it) else 0.10; configurable per policy. Separate `q̂` tables are stored for each α in use.
- **RTE-067** `CalibrationTable` is versioned (`calibration_id = blake3(canonical table)`), tied to `(router model id, embedding model id, option format version)`, stored in the snapshot. If any of those change, calibration is **invalid** and the router MUST use the baseline until a new table is installed (never reuse stale tables).
- **RTE-068** Recalibration is an offline job (`gap calibrate --dataset … --out …`) plus a scheduled online job that refits from the feedback store (§11); installation goes through the normal snapshot publish path (ADR-009).

### 7.4 Gates
- **RTE-069 (Gate G-S1)** per ADR-007: S1 is default only if on the test split it satisfies (i) top-1 ≥ baseline + 3 pts or selective accuracy @ equal coverage ≥ baseline + 3 pts; (ii) ECE₁₅ ≤ 0.05; (iii) empirical coverage ≥ 1 − α − 0.02; (iv) latency SLOs (§9). A failing S1 never blocks release; it ships disabled.

## 8. Telemetry, replica selection, circuit breaker (execution plane; no ML)
### 8.1 Slot
```rust,ignore
pub struct TelemetrySlot {                       // one per BranchUid; lives in TelemetryRegistry, NOT in Snapshot
    pub latency_us: CachePadded<DecayedEwma>,     // half-life 10 s
    pub error_rate: CachePadded<DecayedEwma>,     // half-life 30 s, samples 0.0 / 1.0
    pub in_flight:  CachePadded<AtomicU32>,
    pub capacity:   AtomicU32,                    // from limits.max_in_flight × capacity_weight
    pub breaker:    Breaker,
    pub retry_budget: RetryBudget,
}
/// (f32 value, u32 timestamp in 4 ms units) packed in one AtomicU64 => decay + update is one CAS. UNSET = u64::MAX.
impl DecayedEwma {
    pub fn update(&self, sample: f32, now_q: u32, half_life_ms: f32) {
        let mut cur = self.0.load(Relaxed);
        loop {
            let next = if cur == UNSET { pack(sample, now_q) } else {
                let (v, t) = unpack(cur);
                let dt_ms = now_q.wrapping_sub(t) as f32 * 4.0;
                let alpha = 1.0 - (-dt_ms * core::f32::consts::LN_2 / half_life_ms).exp();   // weight of the new sample
                pack(v + alpha * (sample - v), now_q)
            };
            match self.0.compare_exchange_weak(cur, next, Relaxed, Relaxed) { Ok(_) => return, Err(x) => cur = x }
        }
    }
}
```
- **RTE-070** EWMA updates MUST be lock-free, allocation-free, lost-update-free (CAS loop) and time-decayed; a slot not updated for > 5 half-lives reads as *unknown* (cost uses the group median, giving idle replicas a chance — exploration).
- **RTE-071** `InFlightGuard::acquire(&slot)` increments `in_flight` and its `Drop` decrements it exactly once, including on future cancellation and panic unwinding; a test cancels 10,000 futures mid-call and asserts `in_flight == 0`.
- **RTE-072** Slots are keyed by `BranchUid`. A branch registered by a tenant is tenant-scoped by construction (its `BranchUid` exists only in that tenant's snapshot); a branch explicitly declared `shared` has *aggregate* telemetry that carries no tenant-attributable data (numbers only), while per-tenant limits are enforced separately (SEC-050, INV-09). Registry is a dense slab (`Vec<Arc<TelemetrySlot>>` behind `ArcSwap`) indexed by `BranchSlot(u32)` held in `BranchDescriptor`; no `DashMap` on the hot path.

- **RTE-077** *EWMA time rules.* `now_q` is `⌊elapsed_ms / 4⌋ mod 2³²`, where `elapsed_ms` is measured from the registry's epoch `Instant` of the injected `Clock` (INV-02). `DecayedEwma::update` MUST (a) ignore a sample that is not finite or is negative and report that it did; (b) treat a `now_q` that is behind the stored timestamp by less than 2¹⁶ ticks (≈ 4.4 min — concurrent writers read the clock before their CAS) as `dt = 0` and keep the later timestamp, instead of letting `wrapping_sub` turn it into a ~198-day `dt` that overwrites the average with a stale sample and rewinds the clock; (c) re-seed the cell with the sample when the stored value is older than 5 half-lives, the same condition under which a read returns *unknown* (RTE-070). `dt = 0` gives the new sample weight 0 (a time-weighted, not an event-weighted, average; intended). A read MUST also return *unknown* for a timestamp that is ahead of `now_q` by 2¹⁶ ticks or more. `TelemetryRegistry::maintain(now)` resets every cell older than 5 half-lives to *unknown* (the gateway calls it with each Health poll, RTE-083), so a cell idle for more than 2³¹ ticks (≈ 99 days) cannot come back to life through wrap-around.
- **RTE-078** *Registry.* `TelemetryRegistry::register(key, params)` is idempotent per `TelemetrySlotKey` (same key ⇒ same `BranchSlot`; the first registration's `params` win), hands out dense `u32` indexes, and never reuses an index within a process, so a stale `BranchSlot` held by an old snapshot can never address another tenant's slot (INV-09). It fails with `RegistryError::Full` beyond `max_slots` (default 65,536). Lookup by `BranchSlot` is lock-free and allocation-free (one `ArcSwap` load plus an index); `register` is the only path that takes a lock and clones the slab (copy-on-write, cold path). The metric label of a slot is supplied by the registrant (the gateway chooses it so that it never names a tenant, OPS-020). A shared branch (RTE-072) is registered under a reserved key chosen by the gateway; this crate does not interpret the tenant.

### 8.2 Choosing a replica (power of two choices)
- **RTE-073** Eligible members of a group: branch `ACTIVE`, breaker not `Open` (HalfOpen members are eligible only through probe tokens), policy-permitted, `trust` permitted for the principal, capability supports the needed features (foreach, predicate ops, as_of).
- **RTE-074** If 1 eligible ⇒ it. If ≥ 2 ⇒ sample two distinct members uniformly (thread-local xorshift) and pick the lower `cost = lat · (1 + in_flight/cap)² / max(0.05, 1 − err)` where `lat` is the EWMA (or the group median if unknown). Ties ⇒ lower `in_flight`, then lower `BranchUid`. This avoids herding (no global argmax).
- **RTE-075** The old utility `U = P·(1−E) − λ…` is **not** used: model probabilities never influence replica selection (ADR-006, ADR-011).
- **RTE-076** Sticky routing: for operations tagged `session_affine` (e.g. read-your-writes after a mutation in the same task) choose the replica that served the last mutation for `affinity_ttl` (default 30 s) if eligible.

- **RTE-079** *Selection details.* (a) `cap` in the cost is `max(1, capacity)`. (b) The group median is the lower median `sorted[(n−1)/2]` of the *known* `lat` values of the candidates; if no candidate has a known `lat`, every candidate uses `lat = 1.0` (cost then ranks by load and error). (c) `cost` is an `f64` computed identically for both candidates and compared exactly; tie-break order is lower `in_flight`, then lower `BranchUid` (byte order). (d) The two distinct members are drawn without modulo bias from an explicitly seeded `XorShift64` (tests, simulations) or, in production, a thread-local one whose seed differs per thread (a process-wide counter passed through SplitMix64, never a constant: identical per-thread streams would make every worker draw the same pair sequence and herd); the same `(seed, candidate list, slot states)` always yields the same choice. When more than 64 members are eligible the pair is still drawn from one consistent view: a member whose eligibility changes between two scans of the group MUST NOT turn into an index outside the eligible, non-excluded set (the call returns another eligible member, or `None`). (e) Selecting MUST NOT consume a probe token: a `HalfOpen` member whose probe slots are not exhausted is a candidate, and the caller takes the token with `Breaker::admit` afterwards; if `admit` refuses, the caller selects again with that member excluded. (f) Selection allocates nothing.

### 8.3 Circuit breaker
```text
Closed ── window has ≥20 calls AND (fail_rate ≥ 0.5 OR slow_rate ≥ 0.8) ──► Open(k)
Open(k) ── after min(5 s · 2^k, 60 s) ──► HalfOpen
HalfOpen ── at most 3 probes per episode, at most 3 concurrently; 2 probe successes ──► Closed
HalfOpen ── 2 probe failures (2 of 3 can no longer succeed) ──► Open(k+1)
k := 0 when the breaker has been Closed for 5 min; Closed ──► Open within 5 min of the last Closed entry uses k+1
any ── Health = NOT_SERVING ──► Open ;  Health = DEGRADED ──► capacity × 0.5 until SERVING
```
- **RTE-080** The failure window is a 30 s rolling window of 10 buckets × 3 s, each bucket `(ok, fail, slow)` atomics reset lazily on epoch change (CAS on `bucket_epoch`). *Failure* = connector error with `retryable=true` or timeout; client errors (`INVALID_ARGUMENT`, `PERMISSION_DENIED`, `NOT_FOUND`) are **not** failures. *Slow* = latency > 0.8 × `default_timeout_ms`.
- **RTE-081** `Open` rejects instantly with `GAP_BRANCH_UNAVAILABLE` (`retry.afterMs` = remaining open time) unless another group member can serve.
- **RTE-082** Breaker state transitions emit a structured event and `gap_breaker_state{branch}` gauge; flapping (> 6 transitions / 10 min) raises an alert.
- **RTE-083** `Health` polling every 5 s (jittered) per branch; `NOT_SERVING` ⇒ immediate `Open`.

- **RTE-084** *Call accounting.* Each completed call is recorded exactly once with an `Outcome` (`Ok`, `Failure` = retryable connector error or timeout, `ClientError` = RTE-080 client errors) and its latency. `Ok` and `ClientError` increment the bucket's `ok`, `Failure` increments `fail`; a call whose latency exceeds 0.8 × `default_timeout_ms` additionally increments `slow` (a timeout is therefore both `fail` and `slow`). `calls = ok + fail`; the window opens the breaker when `calls ≥ 20` and (`fail/calls ≥ 0.5` or `slow/calls ≥ 0.8`), evaluated with integer arithmetic. The error-rate EWMA takes 1.0 for `Failure` and 0.0 otherwise; the latency EWMA takes `Ok` and `Failure` latencies and ignores `ClientError`. A bucket is one atomic word holding its epoch tag and the three counters, updated by a single CAS loop that replaces the word when the epoch tag differs (so the lazy reset and the first increment of a new epoch are one atomic step: no lost increment, no stale count leaking into a new epoch); on counter saturation all three counters are halved (ratio-preserving). Only buckets whose epoch lies in the last 10 epochs (`epoch = ⌊ms / 3000⌋`) are summed; `maintain` clears buckets older than the window. *Slow* is decided on the exact duration (`latency > 0.8 × default_timeout_ms`; rounding the latency down to whole milliseconds would miss latencies between 0.8 × timeout and 0.8 × timeout + 1 ms). The epoch tag has a fixed width, so it MUST be compared with the current epoch modulo that width (or be wide enough for any `u64` millisecond clock): the breaker keeps tripping at every uptime, tested at `Now::from_ms(2²² · 30 000 + x)` (a tag compared as an absolute number stops counting after 2²² cycles of 30 s ≈ 4 years).
- **RTE-085** *FSM resolution.* The `HalfOpen` episode is decided by majority of 3 probes (this replaces the earlier "any probe failure reopens", which contradicted "≥ 2 of 3 succeed"). A probe is an RAII `ProbePermit`; dropping it without recording an outcome releases the probe slot and counts as neither success nor failure, so cancelled callers cannot stall an episode. Entering `Closed` clears the whole failure window (otherwise the failures that opened the breaker, still inside the 30 s window, would re-open it at the first call); while the state is not `Closed`, completions of ordinary calls (admitted before the breaker opened) update the EWMAs only and never the window or the probe tally. `HalfOpen` with every probe slot taken rejects with `retry.afterMs` = 1,000; `Open` rejects with the remaining open time (RTE-081); while the `NOT_SERVING` hold keeps the breaker `Open` after its timer elapsed the remaining time is 0 and the rejection carries `retry.afterMs` = 1,000 instead (never 0, which would invite an immediate retry loop). `Open(k)` lasts `min(5 s · 2^k, 60 s)`; `k` saturates at 6. A probe that ends in `ClientError` counts as a success (RTE-080: not a failure), and `Health = NOT_SERVING` during a `HalfOpen` episode ends it and opens with `k + 1`. `Health = NOT_SERVING` forces `Open` from any state (k unchanged when already `Open`) and, while the last Health is `NOT_SERVING`, the `Open → HalfOpen` transition is withheld even after the timer elapsed; a later `SERVING` does not close the breaker, probes do. Every transition emits a `tracing` event with `{branch, from, to, k, reason}` (RTE-082) and sets the gauge `gap_breaker_state{branch}` (0 Closed, 1 HalfOpen, 2 Open); `Breaker::flapping(now)` is true when more than 6 transitions happened in the last 10 min (the last 8 transition times are kept). State transitions use Acquire/Release or stronger ordering and are covered by `shuttle` schedules.
- **RTE-086** *Capacity.* `capacity = max(1, ⌊limits.max_in_flight × capacity_weight × degraded_factor⌋)`, with `capacity_weight ∈ (0, 1]` from NGC-072 (absent or out of range ⇒ 1.0) and `degraded_factor = 0.5` while the last Health was `DEGRADED`, else 1.0. `TelemetrySlot::apply_health` is the only writer of `capacity` after registration.

### 8.4 Retries and hedging
- **RTE-090** Retries apply only to operations with `kind = QUERY` or (`MUTATION` with `idempotent = true` and an idempotency key), only for `retryable` errors, max 2 retries, exponential backoff `min(50 ms · 2^n, 1 s)` with full jitter, never beyond the request deadline.
- **RTE-091** `RetryBudget`: deposit 0.1 token per first attempt, withdraw 1 per retry, balance ≤ 100, initial 10; an empty budget disables retries for that branch (prevents retry storms).
- **RTE-092** Hedged requests are off by default; if enabled for a read-only group (`hedge.after_ms = p95`), at most 1 extra attempt, counted against the retry budget.
- **RTE-093** *RetryBudget representation.* Fixed point in milli-tokens: initial 10,000, deposit 100 per first attempt, withdrawal 1,000 per retry, cap 100,000. Deposits saturate at the cap; a withdrawal succeeds iff the balance is ≥ 1,000 and is one CAS, so concurrent callers can never overdraw (11 concurrent withdrawals against 10 tokens succeed exactly 10 times) and deposits are never lost. No floating point is involved.

## 9. Budgets and SLOs
| Item | Kind | Value |
|---|---|---|
| `__search` end-to-end (excl. network) p95 | SLO | ≤ 40 ms (≤ 20k vectors, CPU) |
| `gap.resolve`, baseline router p95 | SLO | ≤ 40 ms |
| `gap.resolve`, S1 p95 | SLO | ≤ 250 ms CPU · ≤ 80 ms GPU/Metal (re-measure; vendor T4 = 33–40 ms/question) |
| Replica selection overhead p99 | SLO | ≤ 20 µs |
| Telemetry update p99 | SLO | ≤ 1 µs |
| Memory with S1 (f16 421M ≈ 0.85 GB) + embedder + index (20k × 384 f32 ≈ 31 MB) | Gate (M5) | ≤ 2.0 GB RSS |
| Model cold load (mmap) | Target→Gate (M5) | ≤ 5 s |

## 10. Abstention → proposal flow
- **RTE-100** `UnmappedDomainEvent { event_id, tenant, principal_class, at, query_redacted, query_hash, top_groups[(group_id, score)], reason, generation }`. `query_redacted` removes emails, phone numbers, runs of ≥ 8 digits, tokens matching secret patterns (entropy + vendor prefixes); stored ≤ 7 days, or only `query_hash` if `privacy.store_queries = false` (default `true` only in dev).
- **RTE-101** Singleflight: events with equal `(tenant, query_hash)` within 10 min coalesce (count++). Rate limit: 10 distinct events / min / tenant; excess dropped with metric `gap_unmapped_events_dropped_total`.
- **RTE-102** The control plane's `DiscoveryWorker` MAY look up allow-listed registries (config `ingest.registries[]`) for matching sources and create `Proposal`s with status `PROPOSED`. It MUST NOT extract, execute or activate anything without the proposal being approved (ADR-013) — except T0 sources under `auto_activate_t0 = true`.
- **RTE-103** The client sees `GAP_INSUFFICIENT_EVIDENCE` immediately; there is no blocking wait-and-replay in 1.0 (the earlier "await 200–800 ms then replay" design is withdrawn: it hid approval latency and encouraged auto-activation).

## 11. Feedback and online monitoring
- **RTE-110** `routing_feedback` rows: `(request_id, tenant, query_hash, router_id, calibration_id, options_hash, predicted_group, p_pred, set_size, outcome)`, where `outcome` is derived within 5 min on the same trace/session: `EXECUTED_OK`, `EXECUTED_ERROR`, `EXECUTED_OTHER` (agent ran a different operation than suggested → implicit negative), `NONE`, `USER_LABEL` (explicit via admin tool).
- **RTE-111** Online metrics: `gap_discovery_decisions_total{outcome}`, `gap_s1_online_ece` (computed over labelled rows, ≥ 500 per window), `gap_discovery_set_size` histogram, `gap_router_fallback_total{reason}`. Alert when `online_ece > 0.08` for 2 consecutive windows or abstention rate shifts > 2× baseline week.
- **RTE-112** No online weight updates in 1.0; learning happens through scheduled recalibration (RTE-068) and offline fine-tuning tracks.

## 12. Failure modes
| Failure | Behaviour |
|---|---|
| Embedding model missing/corrupt | Lexical-only discovery + `GAP_DISCOVERY_DEGRADED`; readiness stays green, alert fires |
| S1 weights missing | `router=baseline`; `gap models verify` reports; no crash |
| Calibration invalid (RTE-067) | baseline router |
| Index/snapshot mismatch | snapshot build fails; previous generation continues |
| Visibility bitset cache stale after policy change | keyed by `policy_version`; policy publish bumps version |
| All group members `Open` | `GAP_BRANCH_UNAVAILABLE` with soonest `afterMs` |

## 13. Required tests (tag with `spec:` IDs)
Sanitizer properties (RTE-021) · retrieval recall on bench (RTE-005) · visibility: hidden item never appears in hits/counts (RTE-010/011, INV-05) · replica-addition invariance (ADR-006) · conformal coverage on synthetic data with known distribution (RTE-064/065) · calibration invalidation on model change (RTE-067) · EWMA lost-update test with 64 threads (RTE-070) · cancel-safety of `InFlightGuard` (RTE-071) · herding simulation (no share swing > 20%, RTE-074) · breaker FSM table-driven + `shuttle` interleavings (RTE-080) · retry-budget exhaustion (RTE-091) · S1 timeout fallback (RTE-044) · golden parity candle vs PyTorch (ADR-005) · RouteBench gates (RTE-069).
