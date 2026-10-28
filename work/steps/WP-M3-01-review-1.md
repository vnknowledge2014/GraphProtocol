# WP-M3-01 review findings, round 1 (reviewer, 2026-10-28)

Reviewed `wp/WP-M3-01` at `3548d66`; the spec clarifications this round depends on are in `ac1cf44` (RTE-079 d, RTE-084, RTE-085 -
read them first, they are already on your branch).

## What I verified (no action)

- Gates with the real tools: fmt, clippy (workspace, and `--cfg gap_shuttle`), doc, build, **nextest 290/290** (29 of them gap-balance),
  `cargo deny` bans/licenses/sources, layers, validate, refs, trace (40 MUST without test, 0 unknown ids), unittest, shuttle (8 tests,
  82,000 schedules) - all green. Scope is inside `owns`; `Cargo.lock` gained 57 packages and lost none.
- Benchmarks (release, my run): `ewma_update_1t` p99 97 ns, `slot_record_ok_1t` p99 187 ns, `select_n64_1t` p99 497 ns,
  `select_n64_contended_4w` p99 2.2 us; same as yours. Targets met (the 8-thread single-slot spin p99 of 7 us is reported honestly).
- Independent oracle (a second implementation of RTE-070/077/079/080/084/085/086/091/093 written from the spec only, driven through a
  harness): 6,000 EWMA scripts, 600 P2C scenarios (including > 64 members, ties, exclusions, tripped and NOT_SERVING members), 300 retry
  budget scripts and 4,000 breaker scripts (up to 400 operations each) agree with your code, with the findings F-1 to F-5 below as the only
  differences.
- Mutation run on the head (133 string mutants, 3 roots, plain tests + shuttle): 108 killed, 5 equivalent, **20 real gaps** (T-1 to T-9).

## Equivalent mutants (no action)

- `age < HALF_RING_TICKS` -> `<=` (both sides are "unknown" for every realistic half-life), removing the alpha clamp, removing the blended
  clamp, `-0.0` normalisation in `pack` (see F-6: it is harmless but its comment is wrong), `k` cap 5 vs 6 (same 60 s; only the event field
  differs), `clear_stale_buckets` (window sums already filter by epoch), cap 4095 vs 4094 (the check runs before the increment),
  `closed_gen` bump OR `clear_buckets_except_gen` (each alone is enough; removing both is killed), modulo-bias threshold.

## Defects (production code)

### F-1 [Medium] the *slow* threshold is decided on whole milliseconds (RTE-084)
`latency.as_millis().saturating_mul(5) > default_timeout_ms * 4` rounds the latency down, so with `default_timeout_ms = 1000` a call of
800.5 ms is not *slow* although 800.5 > 800. Compare the exact duration (`latency.as_nanos() * 5 > u128::from(default_timeout_ms) * 4 *
1_000_000`, or compare `Duration`s). Test through `window_totals` with timeouts 100, 1,000 and 30,000 ms: 0.8 x T exactly -> not slow,
0.8 x T + 1 us -> slow, 0.8 x T + 500 us -> slow, 0.8 x T + 1 ms -> slow, 0.8 x T - 1 us -> not slow.

### F-2 [Medium] the breaker stops tripping after 2^22 bucket cycles (about 4 years of uptime) (RTE-084)
The bucket tag keeps `cycle` in 22 bits and the code compares `cycle * 10 + idx` as an absolute epoch. At `Now::from_ms(2^22 * 30_000 + x)`
every bucket reads as "ancient", the window sums 0, and 40 straight failures leave the breaker `Closed` (I reproduced it: the same script
trips at ms 100,000 and at 2^22 * 30,000 - 60,000, and does not trip at 2^22 * 30,000 + 1,000). Fix it by comparing the tag modulo its width
(distance `(cur_cycle - tag_cycle) & MASK`, "recent" means 0 or 1 cycles) or by widening the tag; the state word and the other fields must
keep fitting. Tests: for `base` in `{0, 2^22 * 30_000 - 100, 2^22 * 30_000, 2^22 * 30_000 + 100, 2 * 2^22 * 30_000 + 5_000}` record 20 failures
spread over 10 s straddling `base` (e.g. ten at `base - 3_000` and ten at `base + 6_000`) and expect `Open`; also assert `window_totals` counts the
calls made just before and just after the wrap, and that a bucket older than 30 s no longer counts after the wrap.

### F-3 [Low] `retry_after` is 0 while a NOT_SERVING hold outlives the open timer (RTE-085, now specified)
`Admission::Rejected { retry_after: 0 }` makes callers retry at once. The spec sentence in RTE-085 now says 1,000 ms in that case. Test:
trip, `apply_health(NotServing)`, `admit` at `open_until + 10_000` -> `Rejected { retry_after: 1000 ms }` and state `Open`; then
`apply_health(Serving)` and `admit` -> `Probe`; before the timer elapsed the value is still the remaining time.

### F-4 [Medium] every thread's `XorShift64::with_thread_local` starts from the same constant state (RTE-079 d, now specified)
`TLS_RNG` is initialised with `NONZERO_FALLBACK_STATE` for every thread, so all workers draw the identical pair sequence: the correlated
picks defeat the point of P2C (herding). Seed each thread lazily from a process-wide counter passed through SplitMix64 (`XorShift64::new`),
without allocating and without clocks. Test: eight threads each draw their first 16 `next_u64` through `with_thread_local`; the eight
sequences are pairwise different; explicit `XorShift64::new(seed)` stays deterministic.

### F-5 [Low] `select_excluding` with more than 64 eligible members can return an excluded or ineligible index (RTE-079 d/f, now specified)
The second scan re-reads `is_selectable_readonly`; if a member changed eligibility between the scans, `found_a`/`found_b` keep their initial
`0` and index 0 is returned although it may be excluded. Use `Option` (or one consistent snapshot) and return another eligible member or
`None`. Test (stress, 2 s budget): 100 members all tripped at t0 (so selectable at `now >= open_until`), member 0 in `excluded`, a second thread
toggling members 50..100 with `apply_health(NotServing)` / `apply_health(Serving)`, 200,000 selections: the result is never `Some(0)`.

### F-6 [Low] wrong comment and an inaccurate alpha
- `ewma.rs` `pack`: the comment says normalising `-0.0` guarantees `pack(..) != UNSET`. It does not matter (`-0.0` is `0x8000_0000`, never
  `0xFFFF_FFFF`); keep the normalisation for a canonical value but fix the comment.
- `alpha = 1 - exp(-x)` in `f32` loses precision when `x` is small (one tick with a 500 s half-life gives an alpha that is wrong by up to
  about 1 %). Use `-(-x).exp_m1()`. Test: value 0 then sample 1000 one tick (4 ms) later with half-life 500,000 ms reads `1000 * (1 - 2^(-4/500000))`
  within a relative 1e-4; and the half-life examples you already have stay exact.

## Test gaps (the production code is right, nothing fails when it is broken)

Each item names the mutant that survives today; the new test MUST fail on it.

- **T-1 [High]** *Open member whose timer elapsed is selectable* (mutant: `BreakerState::Open => false` in `is_selectable_readonly`).
  Without it a tripped branch that nobody selects never gets its probe. Test: 3 members, member A tripped at t0; at `t0 + 4,999` A is never
  selected over 5,000 draws; at `t0 + 5,000` it is selected sometimes, `admit` then returns `Probe`; a member with the NOT_SERVING hold and an
  elapsed timer is never selected; a `HalfOpen` member with 3 probes taken is never selected.
- **T-2 [High]** *unknown latency uses the group median, also when only ONE of the two candidates is unknown* (mutant: `a_unknown || b_unknown`
  -> `&&`). Test with `select_excluding` and two eligible members (deterministic): A known with latency 0.5 us (`Duration::from_nanos(500)`),
  `in_flight` 0, `BranchUid` 9; B unknown, `in_flight` 0, `BranchUid` 3. Fallback = 0.5, costs tie, lower uid wins: B every time (the mutant gives
  A). Add the mirror with uids swapped (A wins).
- **T-3 [Medium]** `lower_median_known_latency` for more than 64 members and with exclusions (mutants: `le_count >= target_rank`, and the median
  ignoring `excluded`). Unit-test the function directly: 70 members, 35 known at 10 us and 35 known at 1,000 us -> median 10.0 (lower), then
  69 known 1..69 us -> 35.0; with indexes excluded and with `Open` members it ignores them; same checks for 64 and 65 eligible known values.
- **T-4 [Medium]** a stale `ProbePermit` must not touch a later episode (mutant: drop ignores the episode). Test: episode 1: permits A, B, C
  admitted, B and C record failures (reopen, `Open`), A still alive; advance past the timer, episode 2: admit D; drop A; now admit two more
  probes (E, F) and the next `admit` is `Rejected { retry_after: 1000 ms }` (3 live: D, E, F). Same with `A.record(..)` instead of drop.
- **T-5 [Medium]** `flapping` boundaries (mutants: window 5 min, future timestamps counted). Seven transitions at t, `flapping(t + 599_999)` true,
  `flapping(t + 600_000)` false; seven transitions 6 minutes ago still count; a transition timestamp that is later than `now` is not counted.
- **T-6 [Medium]** latency half-life 10 s (mutant: 5 s). Test through `record` + `view`: Ok 100 ms, then Ok 200 ms exactly 10 s later reads 150,000 us
  (the error-rate half-life test already exists).
- **T-7 [Medium]** counter saturation per counter (mutants: saturation ignores `slow`, halving skips `slow`). Slow Ok calls at 70 % slow and 30 %
  fast in one 3 s epoch (below both trip thresholds), 10,000 calls: no counter exceeds 4,095, the `slow/calls` ratio stays within 1 % of 0.7 after the
  halvings, and the breaker stays `Closed`.
- **T-8 [Medium]** two new `shuttle` tests in `tests/shuttle_balance.rs` (10,000 random + 250 PCT schedules like the others):
  (a) `TelemetryRegistry::register` of the SAME key from 3 threads: `len() == 1` and all three get the same `BranchSlot` (mutant: no re-check under
  the writer lock survives the plain-thread test); (b) generation race: a writer whose window just reached the threshold races the sequence
  Open -> HalfOpen -> Closed (two probe successes) on another thread; the breaker must never end `Open` because of a window from an earlier
  generation (mutants: the bucket write and the trip CAS ignore `closed_gen`). If you cannot build a schedule that kills them, say so in the handoff with
  the schedule you tried.
- **T-9 [Low]** the `tracing` event field `k` saturates at 6 and `Open(6)` lasts 60 s (mutant `MAX_K = 5` is equivalent for the duration but not for the event).

Tag every new test with `// spec: <ID>` (RTE-079, RTE-084, RTE-085, RTE-077, RTE-078, RTE-080 as appropriate).

## Handoff accuracy (fix in the handoff, not in code)

- Header says `Commits: 11`; the branch has 10 plus my spec commit and these follow-up commits: state the real number.
- The public-signature block lists `TelemetrySlot { pub latency_us, ... }` (the fields are `pub(crate)`) and `RegistryError::InvalidLabel { reason }`
  (it is a unit variant). Re-generate that block from the code (`cargo doc` or reading `lib.rs`).
- Hand-over section: add that `registry.maintain()` does not advance breaker timers (only `admit`/`state`/`flapping`/`record` do), and therefore the
  selector MUST include elapsed-timer `Open` members (T-1) - this is what lets the first request after the timer take the probe.

## Delivery

Same branch `wp/WP-M3-01`. Production code changes are limited to F-1..F-6; everything else is tests. Commit this file with your first commit
(`fix(gap-balance): ...` / `test(gap-balance): ...`, as many commits as you like). Re-run the gates (`cargo test -p gap-balance` three times,
shuttle command, clippy default / `--cfg gap_shuttle` / workspace, doc, fmt, benches `--quick` and the release `percentiles` run again: the F-1/F-4/F-5
changes must not move selection p99 above 20 us or update p99 above 1 us single-threaded, report both numbers) and append `## Follow-up 1` to
`work/handoffs/WP-M3-01.md` listing each new test with the mutant it kills and your own mutation check of the new tests (the string-mutation method
you used in earlier packages is fine; at least T-1, T-2, T-4 and F-2 must be shown to fail on the broken code). If a new test exposes another defect,
stop, keep the failing test, report it with the evidence and finish with `BLOCKED`; do not hide it. Restrictions as before: edit only
`crates/gap-balance/**`, `Cargo.lock` (cargo only), `work/steps/`, `work/handoffs/`, `work/status.toml`; do not touch `spec/`; do not merge; do not mark the WP done.
Finish with exactly one line: `WP-M3-01 follow-up 1: READY`, `WP-M3-01 follow-up 1: READY-WITH-NOT-RUN` or `WP-M3-01 follow-up 1: BLOCKED - <reason>`, then a short
summary in Vietnamese.
