# Agent Guide — how to build GAP from first commit to 1.0

Audience: any AI coding agent (Antigravity, Claude Code, Cursor, omni swarm, …) and the humans supervising them. English on purpose: it is the working language of the spec and of upstream tooling.
Source of truth order when documents disagree: **ADR (latest) > spec/ > work/work-packages.toml > this guide > comments in code.** Fix the lower one in the same change.

## 1. Read order (≈ 25 minutes, do not skip)
1. `AGENTS.md` and `.agent/rules/*.md` (project rules; the host may also inject global rules).
2. This guide.
3. `spec/00-conventions.md`, `spec/01-architecture.md` (invariants INV-01…20).
4. Your work package in `work/work-packages.toml` and **only** the spec sections listed in its `spec_refs` + the ADRs they cite (`docs/01-DECISIONS.md`).
5. `docs/00-REVIEW.md` §3 for the *why* behind constraints (e.g. why System-1 is off the hot path) when a rule looks odd. Do not "simplify" a constraint before reading its finding.

## 2. Operating principles
1. **Spec-driven.** No code without a work package (WP). If you find a gap in the spec, record it (§9) — do not improvise silently.
2. **Measure, never assume.** Numbers in docs are targets/gates (spec/00 §3). Do not copy performance claims into code comments or docs without a measurement artifact. Do not invent crate versions or APIs: check `docs/evidence/crate-versions.md`, `cargo add --dry-run`, docs.rs, the vendored specs in `third_party/`.
3. **Untrusted text stays untrusted.** Descriptions, tool results, docs and error strings from branches never enter an instruction channel (SEC-080). Never execute anything discovered from the network.
4. **Deterministic core, advisory ML.** Nothing on the execution path may depend on a model (ARC-002, INV-03). The system must be fully functional with S1 disabled (INV-17).
5. **Fail closed, fail loud, fail small.** Return `Result` with typed errors; no `unwrap/expect/panic/todo/unimplemented` outside tests; partial failure is explicit (`partial=true`); defaults are deny.
6. **Smallest correct change.** Respect the WP boundary (`owns`). Prefer boring, explicit code; no magic numbers (name constants and cite the spec id).
7. **Challenge assumptions** (user rule): before implementing, write down what you assume, where it could break, and the simpler alternative you rejected — put it at the top of your step file (§4).

## 3. The work cycle
```text
python3 tools/wp.py ready                      # pick a ready WP (dependencies done)
python3 tools/wp.py set WP-Mx-yy doing         # claim it (commit this line first)
but branch new wp/WP-Mx-yy                     # GitButler virtual branch per user rules (or git worktree)
… expand into steps (§4) → TDD loop (§5) → acceptance → handoff (§10) …
python3 tools/wp.py set WP-Mx-yy done
```
- One WP = one branch = one reviewable change set. Conventional Commits with scope = crate: `feat(gap-ql): add cost model`; footer `Refs: WP-M1-01; spec: GQL-041, GQL-042`.
- If an acceptance command cannot be run in your environment (no GPU, no network), say so in the handoff and run the nearest substitute; never mark `done` on an unrun gate.
- Gate WPs (role `reviewer`, titled "… gate …") produce a file under `docs/gates/` with *evidence links*; a gate that fails stops the milestone — you propose the remedy, you do not lower the threshold.

## 4. Step-expansion protocol (user rule: "Bite-Sized Plans")
Before editing code, create `work/steps/<WP-ID>.md` (commit it — it is the audit trail). Each step is 2–5 minutes of work and contains **exact file paths, exact code or diff, exact command, expected output**:
````markdown
# WP-M1-01 steps
Assumptions: … ; Rejected simpler alternative: … ; Risks: …
## Step 1 — failing test for depth limit (GQL-040)
File: crates/gap-ql/tests/limits.rs
```rust
#[test] fn depth_over_limit_is_rejected() { /* spec: GQL-040 */ … }
```
Run: cargo nextest run -p gap-ql depth_over_limit   → expect FAIL (no impl)
## Step 2 — implement depth check
File: crates/gap-ql/src/limits.rs  (new) …
Run: same command → expect PASS
````
Rules: ≤ 12 steps per WP (if more, the WP is too big: split it in `work-packages.toml` and re-validate); every step ends with a command; the last steps run the WP's acceptance commands. Large steps are a smell — split.
For omni swarm runs: give each task one S/M WP (or a sub-range of steps for an L WP). omni's per-task token budget is small (≈100k), so L-size WPs should be driven by the host agent, with omni used for parallel, well-bounded subtasks.

## 5. TDD loop and test rules
1. Write the failing test first; mark it `spec: <ID>` (`#[doc = "spec: GQL-040"]` or a `// spec: GQL-040` line directly above the `#[test]`). Name invariant tests `inv_<nn>_<what>`.
2. Make it pass with the least code; refactor; keep functions small, pure core + thin I/O shell.
3. Parsers/decoders/sanitizers: add property tests (`proptest`) and a fuzz target if one is listed in the WP.
4. Concurrency primitives: `shuttle` for interleavings (preferred), `loom` only for tiny hand-written lock-free structures; add a stress test with ≥ 64 threads; never use `sleep` > 50 ms in tests — use the manual clock (`gap-testkit`).
5. Time/randomness are injected (Clock trait, seeded RNG). A test that fails 1 in 100 runs is a bug in the test or code: fix or quarantine with a tracking note, never retry-until-green.
6. Coverage: ≥ 80% lines for L0–L3 crates (`cargo llvm-cov`), mutation testing (`cargo-mutants`) nightly on `gap-balance`, `gap-ql` cost model, `gap-route` calibration math (target ≥ 60% score).
7. Benchmarks: `criterion` with `--quick` in PR CI; full runs nightly; store results as JSON under `bench/results/` only through the WPs that own them.

## 6. Parallel-safety rules (many agents, one repo)
- Touch only paths in your WP's `owns`. `tools/wp.py validate` guarantees concurrent WPs have disjoint ownership. Need a change elsewhere? Create `work/requests/<from>-to-<to>.md` (what, why, exact patch) and set your WP `blocked` if you cannot proceed; do not edit the other WP's files.
- **Module stubs.** `python3 tools/wp.py modules` lists, per crate, the top-level modules implied by all WPs' `owns` globs. WP-M0-02 creates those modules as empty stubs and declares them in each `lib.rs`/`main.rs`, so later WPs fill only their own module directories and never edit `lib.rs`. If you need a module that is not planned: add it to your WP's `owns`, re-run `wp.py validate`, and add the `mod` line in a tiny preliminary commit merged first.
- **`Cargo.toml` of a crate:** keep `[dependencies]` alphabetically sorted so concurrent additions merge by union. Versions live only in the root `[workspace.dependencies]` (owned by WP-M0-02); to add a new third-party crate, add it there in a tiny preliminary commit on its own branch (`chore(deps): …`) and merge it first.
- **`Cargo.lock`:** never hand-merge. On conflict: take `main`'s version, run `cargo build --workspace` to regenerate, commit.
- Generated files (descriptor goldens, SDL snapshots, JSON schemas) have a generator command in their header; regenerate, never edit by hand.
- Never rewrite published history; no force-push; no `--no-verify`; no disabling hooks, lints or tests to get green.

## 7. Coding standards (condensed; enforced by clippy/deny/CI)
- Rust 2024, toolchain pinned. `#![forbid(unsafe_code)]` everywhere except `gap-s1` (each `unsafe` has `// SAFETY:` and an entry in `docs/UNSAFE.md`).
- Errors: `thiserror` in libraries with `GapError`-compatible codes; `anyhow` only in binaries/tests. Never stringly-type error codes.
- Async: no lock guard across `.await` (INV-02); bounded channels; cancel-safe futures; RAII guards for counters/permits (INV…, RTE-071).
- Logging via `tracing` with structured fields; never log secrets, raw queries, bearer tokens (INV-15); use `Secret<T>`.
- Public APIs documented; `cargo doc` with `-D warnings`; doc examples compile.
- Data flows down, actions flow up; immutability by default; pure functions for planning/calibration/cost; side effects at the edges.
- Naming: crates `gap-*`; modules by responsibility; constants `SCREAMING_SNAKE` with a doc line citing the spec id and unit (`_MS`, `_US`, `_BYTES`).
- Dependencies: justify every new crate in the PR (maintenance, license, unsafe usage, alternatives). `cargo deny` must pass.

## 8. Definition of Done (per WP)
- [ ] Acceptance commands in the WP all pass locally (paste the tail of each output in the handoff).
- [ ] New/changed behavior is covered by tests carrying `spec:` ids; `python3 tools/spec_trace.py` reports no new uncovered MUST.
- [ ] `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo nextest run --workspace`, `cargo deny check`, `python3 tools/check_layers.py`, `python3 tools/wp.py validate` are green.
- [ ] Docs updated where behavior is user-visible (spec if semantics changed — via §9).
- [ ] No TODO/FIXME without a linked WP id; no commented-out code.
- [ ] Handoff note written (§10); `work/status.toml` updated.
- [ ] If an index is used by the host (omni): re-index after file changes (`omni index`) per the user's rule.

## 9. Changing the spec (deviation protocol)
You may *not* silently deviate. If implementation reveals a spec error or a better design:
1. Stop coding that part. 2. Write `docs/decisions/<date>-<slug>.md` (context, options, decision, impact on spec/ADR/tests). 3. If the change touches an Accepted ADR or any `MUST`, propose a new ADR superseding it (`docs/01-DECISIONS.md`) and the spec diff and test diff **in one change set**; mark the WP `blocked` until a human or the `reviewer` agent accepts it. 4. Editorial fixes (typos, clarifications that do not change behavior) may go in directly with a `docs:` commit.

## 10. Handoff format (end of every WP; paste in the PR/commit body and `work/handoffs/<WP-ID>.md`)
```text
WP: WP-M1-01   Branch: wp/WP-M1-01   Commits: <n>
Modified crates / public signatures: gap-ql: parse_validate(&Schema,&str)->Result<Doc,GapError> …
Tests: <added> added, <passed>/<total> passing; spec ids covered: GQL-001, GQL-002 …
Acceptance output: (tails)
Benchmarks / latency deltas: …    Deviations & ADR addenda: none | <links>
Risks / follow-ups: …            Stop-and-ask items raised: none | …
```

## 11. Stop-and-ask list (escalate to the human, then wait)
- Needs a paid API key, cloud account, GPU, or any credential not already in the environment; or would send repository/spec content to a third-party service (D-006).
- Any change to `SEC-*` semantics, trust-tier rules, policy defaults, audit format, sandbox profiles.
- License questions (new dependency with non-allow-listed license, model weights, vendored spec text).
- A gate fails or a SLO cannot be met by reasonable engineering — propose options; never relax thresholds yourself.
- Two specs contradict each other, or a WP exceeds size L after honest estimation.
- Destructive or irreversible operations (deleting data/branches, rewriting history, installing system-wide tools, editing files outside the repo such as `~/.gemini`, `~/.config`, `~/.claude`, git remote operations).
- Anything that requires disabling a hook, lint, sandbox or test.
- Security-relevant finding in existing code (report privately per `SECURITY.md`; do not open public issues with exploit details).

## 12. Tooling cheat-sheet
| Task | Command |
|---|---|
| Everything CI runs | `just verify` |
| One crate | `cargo nextest run -p <crate>` · `cargo clippy -p <crate> --all-targets -- -D warnings` |
| Layering / WP / spec checks | `python3 tools/check_layers.py` · `python3 tools/wp.py validate` · `python3 tools/spec_trace.py` |
| Conformance | `cargo run -p gap-conformance -- --class Core --spawn examples/walking-skeleton` |
| Fuzz (smoke) | `cd fuzz && cargo fuzz run <target> -- -max_total_time=60` |
| Benches | `cargo bench -p <crate> -- --quick` |
| omni: preview cost of a milestone DAG | `omni run --estimate-dag work/dag/m1.json` |
| omni: run a milestone DAG | `omni run --dag work/dag/m1.json` (prefer for S/M WPs; see §4) |
| omni: System-1 oracle for experiments | `omni decide --state '<json>' --question … --choice a,b,c --abstain --json` (note: observed to be a lightweight local engine; do not treat as Laya) |
| Regenerate DAGs | `for m in M0 … M7; do python3 tools/wp.py dag --milestone $m --out work/dag/${m,,}.json; done` |

## 13. Anti-patterns (each one cost the earlier draft a finding)
- Putting a model on the typed execution path or letting model output influence authorization (F-12).
- Treating replicas as competing options for a probability-based gate (F-13).
- Trusting any text/hint from a branch: descriptions, `readOnlyHint`, `servers[]`, `spec_hash` (F-17, F-22).
- Verifying a join with a language model instead of data (F-16).
- Pinning dependency versions from memory; using pre-release crates without exact pins (F-08).
- Quoting latency from a vendor's GPU as a CPU target (F-04); quoting `ArcSwap` costs without measuring (F-01).
- Using `DashMap`/`RwLock` on the request path; unbounded queues; `sleep`-based tests.
- Adding endpoints/features outside ADR-019's non-goals "because it is easy".
- Writing docs that promise what no test or benchmark verifies (ADR-021).
