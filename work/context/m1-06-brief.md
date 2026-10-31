# WP-M1-06 brief — `gap-plan` v0 (single-branch planner, plan cache, `plan_hash`, explain) and `sha2` in `gap-ql`

Status: binding for WP-M1-06 (reviewer, 2026-10-31). Deliverables and acceptance commands are those of `work/work-packages.toml`; this file makes them concrete. If the two disagree the work package wins; say so in the handoff.
Normative inputs: `spec/06-runtime.md` §3 (RUN-005, RUN-010, RUN-011, RUN-013, RUN-014; RUN-012 is only declared), `spec/02-gap-ql.md` (§4 `PlanExplanation`, GQL-040..042 cost, §10 error codes), `spec/01-architecture.md` §4/§6 (INV-01, INV-02, ARC-002), ADR-009 §6 (cache key) in `docs/01-DECISIONS.md`,
`crates/gap-core/src/snapshot/mod.rs` (`Snapshot`, `FieldOwnership`, `PlanCacheKey`, `BranchDescriptor`), `crates/gap-ql/src/{validate,cost,limits,persisted}.rs`, and `work/handoffs/WP-M1-05.md` (what composition produces: root fields live on `Query`/`Mutation` under their own names, branch types are prefixed `<ns>_`, `@branch` on every owned field).

## 1. Scope

In: `crates/gap-plan/**`, `crates/gap-ql/src/persisted.rs` (and nothing else of `gap-ql`), `crates/gap-ql/Cargo.toml` and `crates/gap-plan/Cargo.toml` (dependencies only, as in section 2), `Cargo.lock` (cargo only).
Out (stop and ask before touching): every other crate, root `Cargo.toml`, `spec/`, `docs/`, `third_party/`, `.local/`. `gap-plan` is layer L2: it may depend on `gap-core` (the `Snapshot` type lives there) and `gap-ql`, never on `gap-snapshot` (L3).
Not in this WP: joins, traversal, mutations, gates, remote predicates, pushdown beyond projection (the IR declares `Join`, `Traverse`, `Mutate`, `Gate`, `Pushdown{limit, order_by, predicate}` but the planner never produces them: `gap-core::Capabilities` carries no pagination/order flags yet, record this for WP-M3-02), execution (WP-M1-07), authorization, discovery fields (`gap { ... }` is refused with the existing error code you judge closest and a documented message).

## 2. Dependencies (pre-seeded and resolved offline by the reviewer; anything else is a stop-and-ask)

`gap-ql` gains `sha2.workspace = true`. `gap-plan` declares exactly: `apollo-compiler`, `blake3`, `gap-core`, `gap-ql`, `moka` (`features = ["sync"]`, no default features), `serde` (`derive`), `serde_json`, `thiserror`, `tracing`; dev-dependencies `proptest`, `tokio` (`macros`, `rt-multi-thread`, `time`).
`moka` and `sha2` are vendored. `CARGO_HOME=.git/tmp-cargo-home` for cargo; do **not** delete it yourself (leave it, say so in the handoff: the reviewer removes it); `cargo nextest` is not installed for you (report NOT RUN, run `cargo test ... --locked --offline` three times).

## 3. Design (binding shape; names inside a module are yours unless stated)

1. **`gap-ql` `sha2`**: replace the hand-written FIPS 180-4 code in `persisted.rs` by `sha2::Sha256`, keeping `sha256_bytes` and `sha256_hex` signatures and every NIST vector test; add a test that compares both against the crate for 1,000 random inputs of 0..4 KiB (proptest) and delete the old compression code.
2. **`ir`**: `Plan { root: PlanNode, operation_name: Option<String>, cost: CostEstimate-like struct, branches: Vec<BranchId> }` and `PlanNode::{Fetch, Parallel, Sequence, Flatten, Join, Traverse, Mutate, Gate}` (serde, `Debug`, `PartialEq`). `Fetch { branch: BranchId, branch_uid, operation: String, arguments: BTreeMap<String, ArgValue>, selection: Vec<SelectionField>, response_key: String, pushdown: Pushdown }`
   with `ArgValue::{Literal(serde_json::Value), Variable(String)}` (plans are **variable-independent**: values are bound at execution, so one cached plan serves every variable set) and `SelectionField { name, alias: Option<String>, children: Vec<SelectionField> }` (fragments and inline fragments flattened, `__typename` kept, field order = document order).
3. **`planner`**: `plan(snapshot: &Snapshot, document: &ValidatedOperation or the apollo `ExecutableDocument` + operation name, config: &PlanConfig) -> Result<Plan, GapError>`. Each root field of a **query** is resolved through `snapshot.ownership` (`FieldCoordinate{ type_name: "Query", field }`) to exactly one branch (RUN-011: no owner is `GAP_UNKNOWN_FIELD`, never a guess), mapped to that branch's operation of the same name; the root fields of one branch stay separate `Fetch` nodes (one call per root field);
   root fields of different branches go under `Parallel`, ordered by `(branch_id, operation, response_key)` (RUN-013), a single fetch is not wrapped. Mutations and subscriptions are refused (`Mutate` is declared only). The cost of the plan is computed **before** anything is returned and over-budget plans are refused (RUN-005): use `gap_ql::cost`.
4. **`explain`**: `PlanExplanation { plan: serde_json::Value, branches_touched: Vec<String>, estimated_cost: u64, pushdowns: Vec<String>, warnings: Vec<String> }` matching spec/02 §4, built from a `Plan` without executing anything, deterministic.
5. **`plan_hash`**: `blake3` hex of the compact `serde_json` text of the plan value (object keys sorted: `serde_json::Map` is a `BTreeMap` here; assert that in a test). Decision recorded for the handoff: RFC 8785 canonicalisation lives in `gap-snapshot` (L3) and cannot be used from L2; the compact sorted form is deterministic across processes for the types used (no floats in plans).
6. **Plan cache** (`moka::sync::Cache`, no default features): key = `gap_core::snapshot::PlanCacheKey` (tenant, generation, `branch_set_hash`, policy version, document hash, operation name: INV-09 tenant isolation, a policy bump or a new generation misses), capacity `plan_cache.max_entries` (default 10,000), TTL 1 h, weigher by the serialised size; values are `Arc<Plan>`.
   `PlanCache::get_or_plan(...)` counts hits and misses (plain atomics) and exposes `gap.plan_cache` = `hit|miss`. The read path takes no lock of its own (INV-02: no `Mutex`/`RwLock` guard in your code; moka's internals are its own).
7. Crate header as the other crates; existing placeholder modules (`join`, `pushdown`, `traverse`) stay.

## 4. Tests (binding)

* Determinism (RUN-013): the same document planned 1,000 times gives one `plan_hash`; the hash is pinned by a golden value (so a change is a conscious edit); two processes (spawn the test binary twice via an `#[ignore]`d test the reviewer runs, or a documented `cargo test` pair) agree; independent root fields given in any document order give the same plan.
* Ownership (RUN-011): a two-branch snapshot built with `gap_snapshot::compose` in a **dev-dependency-free** way is not possible (L3): build the `Snapshot` by hand in the tests (`gap_core::snapshot` types) or through `gap-testkit` if it offers it; assert unknown root field, a field owned by branch A fetched from A only, aliases, variables as `ArgValue::Variable`, fragments flattened, `__typename`, nested selections, directives `@skip/@include` kept or evaluated (state which and test it), a `gap` field refused, a mutation refused.
* Cost before execution (RUN-005): an over-budget document is refused and the planner's callee count stays 0; the cost equals `gap_ql::cost` for the same document.
* Cache: hit on the second call, miss after a generation change, after a policy-version change, for another tenant and for another operation name; a plan evicted by TTL (use moka's `run_pending_tasks` and a short TTL in a test cache) is replanned; concurrent `get_or_plan` of the same key plans at most a handful of times (document the exact guarantee: moka's `get_with` coalesces, use it).
* `explain` golden JSON for a two-branch plan; `sha2` tests of section 3.1; hostile documents (deep nesting at the limit, 10,000 root fields, 1 MiB alias) end bounded.
* Every test carries `// spec: <ID>` (RUN-005, RUN-010, RUN-011, RUN-013, RUN-014, INV-01, INV-02, INV-09, GQL-040, GQL-041); deterministic, 3 runs without flakiness.

## 5. Process rules

Follow `docs/AGENT-GUIDE.md`. Branch `wp/WP-M1-06` from `main`; claim with `python3 tools/wp.py set WP-M1-06 doing`; write `work/steps/WP-M1-06.md` (at most 12 steps, each ending with a command); small Conventional Commits.
Edit only the paths of section 1, `work/steps/`, `work/handoffs/`, `work/status.toml`. Run before the handoff: `cargo fmt --all --check`, `cargo clippy --workspace --all-targets --locked --offline -- -D warnings`, `RUSTDOCFLAGS='-D warnings' cargo doc --workspace --no-deps --locked --offline`, `cargo build --workspace --locked --offline`,
`cargo test -p gap-plan -p gap-ql --locked --offline` (3 runs), `python3 tools/check_layers.py`, `python3 tools/wp.py validate`, `python3 tools/check_refs.py`, `python3 tools/spec_trace.py --warn`, `python3 -B -m unittest discover -s tools -p 'test_*.py'`.
Mutation checks (required in the handoff; edit, show the named test failing, restore): resolve a root field to the first branch instead of its owner; drop the `(branch_id, operation, response_key)` sort; embed variable values in the plan; drop `tenant` from the cache key; ignore the policy version; skip the cost check; let a mutation plan; keep `gap` fields; flatten fragments wrongly (drop inline-fragment children);
use a `HashMap` iteration order in `plan_hash`; break one NIST vector in `sha256_hex`; never evict (TTL ignored); cache key without the generation.
Do not merge; do not mark the work package done. If a test exposes a spec or brief defect, stop, keep the failing test and finish with BLOCKED. Finish with exactly one line `WP-M1-06: READY`, `WP-M1-06: READY-WITH-NOT-RUN` or `WP-M1-06: BLOCKED - <reason>`, then a short summary in Vietnamese.

## 6. What the reviewer will check

Scope against `owns`; nextest, deny and the gate list; a clean-room offline rebuild; the `sha2` change against 100,000 random inputs through Python `hashlib`; a planning oracle (an independent Python walk of the same documents over the same snapshot fixtures: branch per root field and selection trees); determinism across two processes;
cache stress under busy loops; a mutation campaign over `planner`, `ir`, `cache`, `explain` and `persisted.rs`.
