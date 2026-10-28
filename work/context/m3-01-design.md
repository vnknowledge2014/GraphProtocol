# WP-M3-01 binding design — gap-balance (telemetry slots, EWMA, breaker, retry budget, P2C)

Status: WP-M3-01 CLOSED (2026-10-28); the rules decided during review are now in `spec/04-routing.md` §8 (RTE-079 d, RTE-084, RTE-085) and the spec wins on any difference. Original status: binding for WP-M3-01 (reviewer, 2026-10-28). The normative text is `spec/04-routing.md` §8
(RTE-070..086, RTE-090..093; RTE-077..079, 084..086 and 093 were added by the reviewer on 2026-10-28 and
resolve defects of the first draft: out-of-order timestamps, the self-contradicting HalfOpen rule,
window clearing, registry growth). If this file and the spec disagree, the spec wins. Do not re-decide
anything here; if a rule looks wrong or impossible, implement the closest safe behaviour, list it under
Deviations and continue.

## 1. Scope

In: everything in `crates/gap-balance/**`.
Out (later WPs, do not build): the retry loop, backoff and jitter (RTE-090), hedging (RTE-092), sticky routing
(RTE-076), the eligibility filter of RTE-073 other than breaker state (branch ACTIVE, policy, trust, capability
belong to the caller), the Health polling task, the tower layer (all WP-M3-04), adding a `BranchSlot` field to
`gap-core`'s `BranchDescriptor` (`BranchSlot` is defined here; gap-core is not touched).
`gap-balance` is layer L1: it depends on `gap-core` only (plus the third-party crates below), and never on an ML
crate (`tools/check_layers.py`, ARC-002). No `unsafe` in the library (`#![forbid(unsafe_code)]` stays).

## 2. Cargo.toml (use exactly this; it was resolved, built, benched and clippy-checked offline in a scratch copy)

```toml
[lib]
bench = false                       # otherwise `cargo bench -- --quick` reaches libtest and fails

[dependencies]
arc-swap.workspace = true
crossbeam-utils.workspace = true
gap-core.workspace = true
metrics.workspace = true
thiserror.workspace = true
tracing.workspace = true

[target.'cfg(gap_shuttle)'.dependencies]
shuttle.workspace = true            # only when built with RUSTFLAGS="--cfg gap_shuttle"

[dev-dependencies]
criterion = { workspace = true, features = ["cargo_bench_support"] }
hdrhistogram.workspace = true
proptest.workspace = true
tokio = { workspace = true, features = ["macros", "rt-multi-thread", "test-util", "time"] }

[[bench]]
name = "update"
harness = false
[[bench]]
name = "selection"
harness = false
[[bench]]
name = "percentiles"
harness = false

[lints.rust]
unexpected_cfgs = { level = "warn", check-cfg = ["cfg(gap_shuttle)"] }
```
Keep `[package]`, sorted single-line `workspace = true` style (ARC-001). Declare a dependency only if you use it
(`tokio` is dev-only; drop `crossbeam-utils` only if you do not use `CachePadded`). `loom` is vendored but not
required: do not use it. Do not touch the root `Cargo.toml`. `Cargo.lock` is updated by cargo offline (about 57
packages are added: criterion, shuttle, hdrhistogram, metrics, arc-swap and their dependencies); commit it with the
first commit that needs it.

## 3. The `sync` shim (so that `shuttle` can explore the real code)

`src/sync.rs` re-exports `Arc`, `AtomicU32/U64/Bool`, `Ordering`, `Mutex` (cold paths only) and `thread` helpers from
`std` normally and from `shuttle` under `#[cfg(gap_shuttle)]`. Every atomic of the breaker, the buckets, the retry
budget and the in-flight counter MUST go through the shim. Shuttle tests live in `tests/shuttle_*.rs`, start with
`#![cfg(gap_shuttle)]`, and run as
`RUSTFLAGS="--cfg gap_shuttle" cargo test -p gap-balance --test 'shuttle_*' --locked --offline --target-dir target/shuttle`
(separate target dir; report the exact command and its result in the handoff, as an extra acceptance command). They
are not part of `cargo nextest run -p gap-balance`. `clippy` must also pass with the cfg set.

## 4. Public API (names are the contract for WP-M3-04; parameter lists may change if you list it in the handoff)

```rust
// time
pub struct Now { pub ms: u64, pub q: u32 }              // ms since registry epoch; q = (ms / 4) as u32 (wrapping)
// ewma
impl DecayedEwma { pub const fn new() -> Self;          // UNSET
    pub fn update(&self, sample: f32, now_q: u32, half_life_ms: f32) -> bool;  // false = rejected (RTE-077a)
    pub fn read(&self, now_q: u32, half_life_ms: f32) -> Option<f32>;         // None = unknown
    pub fn reset_if_stale(&self, now_q: u32, half_life_ms: f32) -> bool;
    #[doc(hidden)] pub fn update_traced(&self, ..) -> Option<(u64, u64)>;      // None = sample rejected; else (old_raw, new_raw) of the committing CAS (old == new for a no-op)
    #[doc(hidden)] pub fn raw(&self) -> u64; }
// slot
pub struct SlotParams { pub max_in_flight: u32, pub default_timeout_ms: u32, pub label: Arc<str> }
pub struct TelemetrySlot { /* RTE §8.1 fields: latency_us, error_rate, in_flight, capacity, breaker, retry_budget */ }
impl TelemetrySlot {
    pub fn record(&self, outcome: Outcome, latency: Duration, now: Now);   // EWMAs + breaker window (RTE-084)
    pub fn on_first_attempt(&self);                                        // retry budget deposit
    pub fn apply_health(&self, status: HealthStatus, capacity_weight: Option<f64>, now: Now);  // RTE-083/086
    pub fn breaker(&self) -> &Breaker;  pub fn retry_budget(&self) -> &RetryBudget;
    pub fn view(&self, now: Now) -> SlotView;       // latency, error rate (Option), in_flight, capacity, state
}
pub enum Outcome { Ok, Failure, ClientError }
pub enum HealthStatus { Serving, Degraded, NotServing }
// guard
impl InFlightGuard { pub fn acquire(slot: &Arc<TelemetrySlot>) -> InFlightGuard; }   // Drop decrements once
// breaker
pub enum BreakerState { Closed, HalfOpen, Open }
pub enum Admission { Allowed, Probe(ProbePermit), Rejected { retry_after: Duration } }
impl Breaker { pub fn admit(&self, now: Now) -> Admission; pub fn state(&self, now: Now) -> BreakerState;
    pub fn flapping(&self, now: Now) -> bool; /* + on_health, record_* used by TelemetrySlot */ }
impl ProbePermit { pub fn record(self, outcome: Outcome, latency: Duration, now: Now); }   // Drop = neither
// retry
impl RetryBudget { pub fn new() -> Self; pub fn deposit_first_attempt(&self); pub fn try_withdraw(&self) -> bool;
    pub fn balance_millis(&self) -> u32; }
// registry
pub struct BranchSlot(u32);   // accessor index(); never constructed from an arbitrary u32 outside the crate's tests
impl TelemetryRegistry { pub fn new(clock: Arc<dyn Clock>, max_slots: u32) -> Self;
    pub fn register(&self, key: TelemetrySlotKey, params: SlotParams) -> Result<BranchSlot, RegistryError>;
    pub fn get(&self, slot: BranchSlot) -> Option<Arc<TelemetrySlot>>;  pub fn find(&self, key: &TelemetrySlotKey) -> Option<BranchSlot>;
    pub fn now(&self) -> Now;  pub fn maintain(&self);  pub fn len(&self) -> usize; }
// p2c
pub struct XorShift64;   pub struct Candidate<'a> { pub uid: BranchUid, pub slot: &'a TelemetrySlot }
pub fn select(cands: &[Candidate<'_>], now: Now, rng: &mut XorShift64) -> Option<usize>;
pub fn select_excluding(cands: &[Candidate<'_>], excluded: &[usize], now: Now, rng: &mut XorShift64) -> Option<usize>;
```
Hot-path functions take `Now` (computed once per request with `registry.now()`); they never read a clock themselves.
The registry slab is **two-level copy-on-write** (fixed chunks of 256 slots, outer `Vec<Arc<Chunk>>` in the `ArcSwap`),
so one registration copies O(256 + len/256) pointers instead of the whole slab; `get` is one `ArcSwap` load plus two
indexings. `BranchSlot` indexes are dense and never reused (RTE-078).

## 5. Rules the spec leaves to you (decided here)

- `HalfOpen` with all probe slots taken: `Rejected { retry_after: 1 s }`. `Open`: remaining open time. `Closed`: `Allowed`.
- The breaker uses integer arithmetic for "slow": `latency_ms * 5 > default_timeout_ms * 4` (strictly greater, no floats).
- The window trips on `calls >= 20 && (fail * 2 >= calls || slow * 5 >= calls * 4)`.
- `Open(k)` duration: k = 0,1,2,3,4,5,6 -> 5, 10, 20, 40, 60, 60, 60 s.
- Metric/gauge/event label is `SlotParams.label` (RTE-078); never a tenant. Validate it at `register`: 1..=64 bytes, no control characters, else `RegistryError::InvalidLabel`.
- `view()` and `maintain()` are cold paths; they may take a lock but must never block `record`, `admit`, `select`, `acquire`.
- Memory ordering: `Relaxed` is allowed for statistics (EWMA words, counters read only for display); breaker state
  transitions and the probe tally need `AcqRel`/`Acquire`/`Release` or stronger. Justify each non-`SeqCst` choice
  of the breaker in a one-line comment.
- Accepted limit, write it in the handoff: a shared branch's retry budget and telemetry are aggregate across tenants
  (RTE-072), so one tenant's failures can drain the shared retry budget; per-tenant limits are enforced elsewhere (SEC-050).

## 6. Tests (every test carries `// spec: <ID>` with IDs defined in spec/*.md; ADR ids only in parentheses)

All tests are deterministic: explicit `Now`/ticks and `ManualClock`, seeded `XorShift64`, no sleeps, no wall clock,
no thread-timing assumptions, no iteration over the implementation's own constants to build an expectation (use literal
numbers; the reviewer mutates thresholds, constants and orderings and every mutation must fail some test). Each test
under 10 s in a debug build.

1. **EWMA literal tests** (RTE-070, RTE-077): first sample seeds; one half-life later a sample 100 over a value 0 gives
   50.0 (tolerance 1e-3); `dt = 0` leaves the value unchanged and `update` still returns true; non-finite and negative
   samples return false and change nothing; `now_q` behind the stored timestamp by 3 ticks acts as `dt = 0` and keeps
   the later timestamp; behind by 2^16 ticks or more is *unknown* on read; exactly 5 half-lives old is still known, 5
   half-lives plus one tick is unknown; an `update` on an older cell re-seeds (value == sample); `reset_if_stale` and
   `registry.maintain()` clear stale cells and keep fresh ones; the `u32` wrap (a cell written at `u32::MAX - 10` read at
   `5`) behaves as 16 ticks of age, not 2^32.
2. **Lost-update test, 64 threads** (RTE-070): 64 `std::thread`s released by a `Barrier`, 5,000 `update_traced` calls each,
   each with a unique logical tick and a literal per-thread sample pattern. After the joins build the multigraph of
   committed `(old_raw, new_raw)` transitions (a no-op update reports `old == new`, a self-loop) and assert it is a single trail from `UNSET` to `raw()`: exactly 320,000
   transitions, out-degree minus in-degree is +1 at `UNSET`, -1 at the final word, 0 elsewhere, and all edges are in one
   weakly connected component. A load-compute-store implementation must fail this test. Also assert the final value is
   finite and within the min/max of the samples used.
3. **InFlightGuard** (RTE-071): acquire/drop returns to 0; nested guards; a guard dropped during a panic unwinding
   (`catch_unwind`) decrements once; **cancel 10,000 futures** on a tokio current-thread runtime with paused time:
   10,000 tasks each hold a guard across `sleep(1 h).await`; wait until `in_flight == 10_000` (yield loop, no timers),
   abort all, await all, assert `in_flight == 0`; the same with futures dropped by `tokio::time::timeout` and futures
   dropped before their first poll (never acquired). A `mem::forget` of a guard is documented as a leak, not tested.
4. **Allocation-free hot paths** (RTE-070, RTE-079, ARC-006): one dedicated integration test file may contain a counting
   global allocator that delegates to `System` (`#![allow(unsafe_code)]` in that file only, with a comment why); the count
   is thread-local-scoped so other test threads do not pollute it. Assert 0 allocations in `DecayedEwma::update/read`,
   `TelemetrySlot::record`, `InFlightGuard::acquire` + drop, `Breaker::admit`, `RetryBudget` ops, `select`, `registry.get`.
5. **Breaker FSM, table-driven** (RTE-080..085): a table of at least 30 literal rows `(setup, events with explicit ms,
   expected state/admission/retry_after)`; must include: 19 calls never trip, 20 calls trip at exactly 10 failures and not
   at 9; slow trip at 16 of 20 and not at 15 of 20; client errors never count as failures; a timeout counts as `fail` and
   `slow`; events at 29,999 ms still inside the window and at 30,000 ms outside; epoch boundaries at 3,000 ms multiples;
   Open durations for k = 0..6 and the 60 s cap, with the exact-boundary millisecond; `Open -> HalfOpen` at the boundary;
   4th concurrent probe rejected; 2 successes close; 2 failures reopen with k+1; a dropped permit frees its slot and
   changes no tally; the window is cleared on Closed (the first call after closing does not re-trip); non-probe
   completions while Open/HalfOpen do not touch the window or tally; k resets after 5 minutes Closed and escalates when
   re-opening inside 5 minutes; `NOT_SERVING` from each of Closed/HalfOpen/Open, the hold on `Open -> HalfOpen` while
   the last Health is `NOT_SERVING`, and `SERVING` not closing; `flapping` false at 6 and true at 7 transitions inside 10
   min, false again after they age out; every transition emits one `tracing` event with the RTE-085 fields and sets the
   gauge (capture events with a hand-written minimal `tracing::Subscriber` via `with_default`, gauges with a hand-written
   `metrics::Recorder` via `with_local_recorder`).
6. **Breaker differential proptest** (RTE-080, RTE-084): random sequences of (advance ms, record Ok/Failure/ClientError with
   a latency, admit, resolve or drop permits, Health) are run against the real `Breaker` and against a brute-force
   reference model written in the test (a `Vec` of timestamped events, window = events with
   `epoch in [e-9, e]`, `epoch = ms / 3000`), comparing state, admission kind and `retry_after` after every step. At least
   2,000 cases, fixed seed in CI via `proptest` config `rng_seed`, plus a printed failing seed on failure.
7. **Shuttle** (`tests/shuttle_*.rs`, RTE-071, RTE-080, RTE-085, RTE-091): random schedules (at least 10,000 iterations) and a
   PCT run for: bucket conservation (N threads x M records in one epoch sum to N x M, none lost, none duplicated); a race
   across an epoch change (no count of epoch e survives into e+10, the first increment of a new epoch is never lost);
   at most 3 `ProbePermit`s alive at once and at most 3 issued per episode under any interleaving; exactly one thread
   performs `Open -> HalfOpen`; observed state sequences only use legal edges; `in_flight` returns to 0; 11 concurrent
   `try_withdraw` against 10 tokens succeed exactly 10 times.
8. **P2C** (RTE-073..075, RTE-079): single candidate returned; two candidates, each chosen under some seed; cost formula
   with literal numbers (lat 100, in_flight 2, cap 4, err 0.5 -> 450.0 and the comparison that follows from it); the
   `max(0.05, 1 - err)` floor (err 0.99 and err 1.0 give the same cost); `cap = 0` behaves as 1; unknown `lat` uses the
   lower median (4 known values -> index 1; 3 known -> index 1; none known -> 1.0); tie order in_flight then `BranchUid`;
   `Open` excluded, `HalfOpen` with free probe slots included, with none excluded; selection never changes breaker state or
   the probe tally; the two sampled indexes are always distinct (100,000 draws); uniformity over ordered pairs for n = 5
   with a fixed seed within +-10 % of the expected count (literal bounds); same seed same sequence, different seed
   different sequence; `select_excluding` skips excluded indexes and returns `None` when nothing is left.
9. **Herding simulation** (RTE-074, ADR-011): deterministic discrete-time simulation, 5 identical replicas, 100,000
   requests, feedback applied with a one-tick delay (the classic herding trigger), measured over windows of 1,000
   requests: assert the largest absolute change of any replica's share between consecutive windows is <= 0.20 for P2C,
   and a control: the same simulation with a global-argmin selector (written in the test) exceeds 0.20, proving the
   simulation can see herding. Repeat once with one replica 3x slower: shares move toward the faster replicas without
   any consecutive-window swing above 0.20, and the slow one never drops to exactly 0 share (exploration, RTE-070).
10. **RetryBudget** (RTE-091, RTE-093): initial balance 10,000 milli-tokens; 10 withdrawals succeed and the 11th fails;
    10 deposits give exactly +1,000; cap at 100,000 after 2,000 deposits; empty budget re-enables after 10 deposits; 64
    threads withdrawing concurrently from 10 tokens succeed exactly 10 times; deposits are never lost (64 x 1,000
    deposits from 0 saturate or sum exactly).
11. **Registry and tenant isolation** (RTE-072, RTE-078, INV-09, INV-10): idempotent `register`; dense indexes 0..n; no reuse;
    `Full` at a tiny `max_slots`; `InvalidLabel` cases; the same `BranchUid` under tenants A and B gives two slots and
    updating one never changes the other; `find` with another tenant returns `None`; 16 threads registering the same key get
    one slot; reader threads calling `get` on already issued slots never see `None` while a writer registers 5,000 keys;
    `maintain` leaves registered slots in place. INV-10 (telemetry never touches a snapshot) is enforced by the layers
    tool: `gap-balance` depends on `gap-core` only and never constructs a `Snapshot`.
12. **Health and capacity** (RTE-083, RTE-086, NGC-070, NGC-072): `max_in_flight` 10 x weight 0.3 -> 3; `Degraded` halves
    (floor, minimum 1); both multiply; weights 0, -1, NaN, 1.5, infinity and `None` act as 1.0; `Serving` restores;
    `NotServing` opens at once (state `Open`, `retry_after` = remaining open time).

## 7. Benchmarks (measure, do not claim; AGENT-GUIDE rule 2)

- `benches/update.rs` (criterion, `--quick` safe): `DecayedEwma::update`, `TelemetrySlot::record` (Ok and Failure),
  `InFlightGuard` acquire+drop, `RetryBudget` deposit and withdraw, single thread and 8 contending threads.
- `benches/selection.rs` (criterion): `select` for n = 2, 8, 64 candidates, with and without contention.
- `benches/percentiles.rs` (`harness = false`, plain `main`, `hdrhistogram`): 100,000 warm-up and 1,000,000 timed
  operations per scenario, prints one line per scenario `name n=<..> p50=<ns> p99=<ns> p99.9=<ns>` plus the measured
  timer overhead (an empty timed loop; `Instant::now()` is about 40 ns on the reviewer's Mac, so report both raw and
  overhead-subtracted numbers); never asserts timing; MUST ignore every command-line argument (`--quick`, `--bench`).
- Targets (RTE §9): selection p99 <= 20 us, update p99 <= 1 us. Report the numbers from a **release** build in the
  handoff and say which machine and whether other load was present. If a target is missed, say so; do not weaken it.

## 8. Process (same rules as WP-M2-01)

Branch `wp/WP-M3-01` from `main`; claim with `python3 tools/wp.py set WP-M3-01 doing`; write the step plan to
`work/steps/WP-M3-01.md` first; one Conventional Commit per deliverable group; handoff in
`work/handoffs/WP-M3-01.md` (AGENT-GUIDE §10) with Deviations, UNSPECIFIED choices, NOT RUN gates, the bench numbers and a
hand-over section for WP-M3-04. Edit only `crates/gap-balance/**`, `Cargo.lock` (cargo only), `work/steps/`,
`work/handoffs/`, `work/status.toml`. Do not merge; do not mark done. `cargo nextest` is not installed: run
`cargo test -p gap-balance --locked --offline` three times in a row and report acceptance command 1 as NOT RUN.
