# 01 — Architecture Decision Records (ADR)

Status legend: **Accepted** = binding for agents · **Default** = binding unless the human overrides (see `docs/00-REVIEW.md` §5) · **Gate** = decision is conditional on a measured gate.
Changing an Accepted ADR requires: new ADR that supersedes it + spec diff + conformance/test diff, in one change set (ADR-022).
Normative keywords (MUST, SHOULD, MAY) follow RFC 2119 / RFC 8174.

Index: 001 GraphQL profile · 002 toolchain/deps · 003 three planes · 004 open-jev · 005 System-1 backend · 006 capability groups · 007 RouteBench & adoption gate · 008 split-conformal · 009 snapshot lifecycle · 010 operation-level index · 011 telemetry/LB/breaker · 012 join verification · 013 trust tiers · 014 poisoning/sandbox/SSRF · 015 mutations · 016 tasks · 017 MCP/A2A facade · 018 tenant · 019 deferred scope · 020 ContextStore · 021 measured claims · 022 agent execution protocol · 023 error model · 024 naming/versioning · 025 scope/capacity/deferral clarifications.

---

## ADR-001 — GAP-QL is a profile of GraphQL, not a new language
**Status:** Accepted
**Context:** The previous EBNF introduced keywords (`traverse {…}`, `federate {…}`) outside GraphQL, left 21 nonterminals undefined (review F-09), and would require a hand-written parser (Pest) for the full GraphQL grammar. LLMs are heavily trained on GraphQL; standard tooling (GraphiQL, codegen, linters, `apollo-compiler`) is valuable.
**Decision:**
1. Every GAP-QL request document MUST be a valid GraphQL executable document; every GAP schema MUST be valid GraphQL SDL (plus GAP directives). Parsing/validation uses `apollo-compiler` (pinned), never a bespoke grammar.
2. GAP built-ins live under a reserved namespace field `gap` on `Query` and `Mutation` (e.g. `{ gap { traverse(...) {…} } }`) plus `Subscription.gapTaskEvents`, so they can never collide with branch fields. Introspection is extended with `__search` and `__metaGraph` (reserved `__` names are legal for introspection extensions).
3. `traverse` → `gap.traverse(...)` typed field; `federate` → Task API (`gap.submitTask`, ADR-016); `resolve(intent)` → `gap.resolve` (ADR-003).
**Consequences:** Existing GraphQL clients work unmodified; custom syntax disappears; the grammar chapter of the spec shrinks to "delta from GraphQL" (`spec/02-gap-ql.md`). Cost: expressing traversal arguments as input objects is slightly more verbose than a bespoke DSL.
**Verification:** differential test vs `graphql-js` reference on a shared corpus (WP-M1-03); fuzz target `fuzz_gapql_parse`.

## ADR-002 — Toolchain and dependency policy
**Status:** Accepted
**Decision:** Rust edition 2024, `rust-toolchain.toml` pinned to `1.98.1`; `[workspace.dependencies]` as the only place versions appear; versions from `docs/evidence/crate-versions.md` (verified 2026-10-26). Pre-release crates (`ort`) are exact-pinned (`=`) and feature-gated. `tonic 0.14` + `axum 0.8` + `prost 0.14` form one coherent set; `cargo tree -d` MUST show a single `axum`, `hyper`, `prost`, `tower`. `cargo-deny` enforces licenses (allow: Apache-2.0, MIT, BSD-*, ISC, Unicode-3.0, Zlib, MPL-2.0; deny: GPL/AGPL; BSL only behind explicit feature `store-surreal`), advisories (RUSTSEC) and duplicate-version bans. `unsafe` is forbidden (`#![forbid(unsafe_code)]`) in every crate except `gap-s1` (mmap of safetensors), where each `unsafe` block carries a `// SAFETY:` justification and is listed in `docs/UNSAFE.md`.
**Consequences:** Build is reproducible (`Cargo.lock` committed). Dependency refresh is a scheduled WP, not ad hoc.

## ADR-003 — Three planes; System-1 is off the typed execution path
**Status:** Accepted
**Context:** The old pipeline ran System-1 on every request (review F-12). A typed GAP-QL operation is fully determined by the supergraph (as in Apollo's query planner); ML adds latency (≈100–140 ms CPU), non-determinism and puts authorization-adjacent decisions in a probabilistic model.
**Decision:** Three planes with strict contracts:
- **Execution plane (deterministic, no ML):** transport → gateway (authn, limits, persisted-operation lookup) → parse/validate → authorize → plan → execute → connectors. Same input + same snapshot + same policy ⇒ same plan.
- **Discovery plane (probabilistic, advisory):** `__metaGraph`, `__search`, `gap.resolve(intent)`. Output is *suggestions* (operation skeletons, ranked elements, or an explicit abstention). It NEVER executes, NEVER widens permissions, and results are filtered by the caller's visibility before ranking.
- **Control plane (async):** ingestion → verification → shadow snapshot → promotion (ADR-009, ADR-013).
Authorization is always evaluated by the deterministic policy engine (ADR-014) after planning; no model output participates in an allow/deny decision.
**Consequences:** Typed-path latency overhead is bounded by planner cost (target p99 ≤ 5 ms over branch time). System-1 failures degrade discovery only.
**Verification:** architecture test `no_ml_on_execution_path` (`tools/check_layers.py`: `gap-exec`, `gap-plan`, `gap-transport`, `gap-snapshot`, `gap-balance` MUST NOT reach `gap-s1`, `gap-route`, `gap-models` or ML runtime crates, transitively); determinism property test (WP-M3-02).

## ADR-004 — `open-jev` is out of the critical path
**Status:** Accepted
**Context:** `kyegomez/open-jev` is an "unofficial research implementation with random weights"; the `openjev` crate is a reservation (review F-02). A StateCache fusion would require training a model (RLCD, distilled labels).
**Decision:** No `open-jev` code or weights are dependencies. Borrowed *ideas that need no training*: (a) canonical JSON state serialization (keys sorted, stable path order) so identical states produce identical model inputs; (b) separating "encode state once" from "answer many questions" is realized at the *application* level via result caching keyed by `(state_hash, question_hash, model_id)`. A custom model trained with an RLCD-style objective is **Research Track R1** (WP-R1-*), started only if the ADR-007 gate fails on fine-tuned Laya, and never blocks a milestone.
**Verification:** `cargo deny` bans crate `openjev`; R1 has its own go/no-go memo.

## ADR-005 — System-1 backends and default checkpoint
**Status:** Default (D-002)
**Context:** `laya-multilingual` is near chance on typed decisions (0.342) and macro-ECE 0.387; `laya-typed-decisions` reaches 0.766 (EN); all ship uncalibrated; official guidance is to recalibrate per workload (review F-03, F-05). `laya-onnx` does not exist; `laya` (third-party) pins candle 0.9 (review F-07).
**Decision:**
1. Two trait levels. `gap-route` defines `trait RoutingModel` (rerank candidate groups + abstain; implementations `BaselineRouter` — hybrid retrieval score, always compiled — and the `S1Router` adapter that formats options per RTE-042) and the low-level `trait DecisionBackend` (choice over options → logits). `gap-s1` implements `DecisionBackend`: `LayaCandle` (owned code), `LayaOrt` (feature `s1-ort`, exact-pinned `ort`), `HttpSidecar` (speaks `/v1/systemone`; used for dev and as a *golden oracle* — compatible with `omni decide --endpoint`). Embeddings go through `gap_core::Embedder`, implemented in `gap-models`.
2. Default checkpoint: `laya-typed-decisions` (421M). `laya-multilingual` MUST NOT be a default. Vietnamese/other-language intents: the MCP facade tool description instructs the calling agent to pass `intent` in English; server-side translation is out of scope for 1.0. A multilingual fine-tune is an optional WP (WP-M5-09).
3. `LayaCandle` is validated against the PyTorch reference by golden tests (≥ 200 fixed (state, question, options) cases; max |Δlogit| ≤ 5e-2 for f16, ≤ 1e-3 for f32; identical argmax on ≥ 99.5%).
4. Model artifacts are not in the binary: `gap models pull` downloads by pinned URL + sha256 into the model cache; offline mode fails closed with a clear error; the manifest lives in `models/manifest.toml`.
5. Input hygiene (MUST): NFKC-normalize; strip control/bidi/zero-width characters; remove (replace by space) every tokenizer special token and marker (`[MASK] [CLS] [SEP] [PAD] [UNK] <mask> <s> </s> <pad> <unk>` and `<|…|>` patterns) from *all* strings (query, option text, state); truncate by tokens, never silently drop options; clamp temperatures to `[0.5, 5.0]`; gate on `answer_confidence = max(p)`, never on `act_probability`.
**Consequences:** 0.8–1.7 GB of weights are an operational artifact; memory budget documented in `spec/04-routing.md` §9.
**Verification:** golden tests, sanitizer property tests/fuzz, `gap-s1` cold-start test, license/provenance review WP (WP-M5-10: training-data provenance of Laya is *unstated* by the authors).

## ADR-006 — Capability groups: System-1 chooses among groups, the balancer chooses among replicas
**Status:** Accepted
**Context:** Replicas split probability mass (two equal replicas ≈ 0.5 each), which breaks `max(p)` abstention thresholds and biases load balancing (review F-13).
**Decision:** Every operation in the snapshot has a `capability_group_id`. A group is a set of operations that are semantically and contractually interchangeable (same input/output signature after normalization, declared by ingestion with `replica_of` evidence or by admin). Discovery ranks **groups**; execution picks a **replica within the group** (ADR-011). A group's text for retrieval/S1 is the union (deduped) of its members' descriptions. Divergent semantics MUST NOT share a group; unverified candidates live in separate groups (ambiguity surfaces as set size > 1, ADR-008).
**Verification:** property test "adding a healthy replica never lowers group confidence or triggers abstention" (WP-M5-03).

## ADR-007 — GAP-RouteBench, calibration pipeline, System-1 adoption gate
**Status:** Accepted (Gate)
**Context:** Vendor accuracy/ECE are from other workloads (review F-05). Top-1 ≈ 0.78 at 20 options is not an unguarded hard gate.
**Decision:** `bench/routebench/` defines a dataset and harness: (a) synthetic queries generated per operation from schema/docs (paraphrase, hard negatives from sibling operations, out-of-domain negatives labelled `insufficient_evidence`), (b) replay of real sessions with success labels once available, (c) fixed splits: train(fit temperature)/calibration(conformal)/test(report), no leakage by operation id. Metrics: top-1, top-3 recall, MRR, ECE (15 bins), Brier, selective accuracy @ coverage, abstention AUROC, conformal coverage and mean set size, per-bucket reliability diagrams.
**Adoption gate G-S1** (checked at M5, re-checked on every model/embedding change): S1 becomes the default reranker only if, on the held-out test split, versus `BaselineRouter`: (i) top-1 ≥ baseline + 3 points **or** selective accuracy at equal coverage ≥ baseline + 3 points, (ii) ECE ≤ 0.05 after calibration, (iii) empirical conformal coverage ≥ 1 − α − 0.02, (iv) discovery p95 within SLO (ADR-003/spec 04 §9). Otherwise S1 ships disabled (`routing.model = "baseline"`) and Track R1 / fine-tune WPs are triggered. The product MUST be fully functional with S1 disabled.
**Verification:** CI job `routebench-smoke` (small split, every PR touching `gap-route`/`gap-s1`/`gap-index`), nightly full run publishing `bench/results/`.

## ADR-008 — Abstention is split-conformal prediction
**Status:** Accepted
**Context:** "Conformal gate" in the old plan was a per-bucket threshold on `max(p)` (review F-15).
**Decision:** Use split conformal prediction with the LAC score `s = 1 − p_true` (APS optional): from the calibration split compute `q̂ = ⌈(n+1)(1−α)⌉/n` empirical quantile; prediction set `C(x) = {g : 1 − p_g ≤ q̂}` over capability groups ∪ {`insufficient_evidence`}. Outcomes: `|C|=1` and ≠ abstain → CONFIDENT; `|C|>1` → AMBIGUOUS (return ≤ 5 alternatives with disambiguating descriptions); `C = ∅` or `C = {insufficient_evidence}` → ABSTAINED. α defaults: 0.10 for read-only candidates, 0.05 if any candidate is `@sideEffect(level != NONE)`. Calibration is per bucket-family `(qtype, option_count_bucket)` with a minimum `n_cal = 500`; below that, fall back to the vendor-style bucket threshold map and flag `calibration="fallback"` in telemetry. Recalibration is a scheduled job that never blocks serving and is versioned (`calibration_id` stored in snapshot).
**Verification:** statistical test of coverage on held-out data (WP-M5-04); drift monitor `gap_s1_online_ece`.

## ADR-009 — Snapshot lifecycle, ArcSwap SLOs, reclamation, rollback, cache keys
**Status:** Accepted
**Context:** Measured: `load()` ≈ 15 ns; `store()` 0.16–23 µs (p99 59 µs @256 busy readers); dropping a 400 MB snapshot on the swapping thread took 28.9 ms (docs/evidence/arcswap-bench).
**Decision:**
1. Readers use `ArcSwap::load_full()` once per request and pin that `Arc<Snapshot>` for the whole request (and for a Task's lifetime if it needs schema stability).
2. Publish via a single `Publisher` actor: build → verify → prewarm → `store()`. The previous generation is handed to a **reclaimer thread** (drop off the swap path); `store()` caller blocking budget ≤ 1 ms.
3. SLOs: swap p99 ≤ 250 µs @256 busy readers (baseline 59 µs); zero failed/torn requests across 100 swaps with 1,000 concurrent requests; reader p99 regression ≤ 5% during swaps.
4. Ring of the last 5 generations kept via structural sharing (`im`/`Arc` per branch) so memory growth is bounded (gate: ≤ +15% RSS vs 1 generation for a 1-branch change).
5. Auto-rollback: after promotion, a 60 s watch window compares **snapshot-attributable** errors (planning/validation errors, errors from branches whose `spec_hash` changed) with the previous generation's baseline; roll back once iff rate > max(2× baseline, baseline + 2 pp) with ≥ 200 samples; the bad generation is marked and cannot be re-promoted without `--force`. Unrelated infrastructure errors do not trigger rollback.
6. Plan-cache key = `(tenant_id, policy_version, snapshot.branch_set_hash, normalized_query_hash, variable_shape_hash)`. Prewarm replays top-100 plans; promotion is **blocked** if < 95% re-plan successfully unless `breaking_change_ack` is set.
7. Telemetry slots are keyed by stable `BranchUid` in a registry independent of snapshot generations; removing a branch tombstones its slot after a grace period.
**Verification:** `swap_stress` (WP-M4-08), `shuttle` model of publisher/reclaimer state machine, rollback attribution unit tests.

## ADR-010 — Operation-level hybrid index and progressive disclosure
**Status:** Accepted
**Context:** Branch-level 20-token synopses cannot represent large APIs; the embedding model was unspecified (review F-14).
**Decision:** The snapshot carries an `ElementIndex` over **operations, types, fields, nodes' schema docs** with: dense vectors (default `bge-small-en-v1.5`, 384-d, via candle; model id + sha256 pinned in the snapshot), BM25 over names/descriptions/examples, fused by reciprocal rank fusion (k = 60). Flat SIMD exact search for ≤ 20,000 vectors; HNSW (`usearch`) above. Disclosure levels: **L0** `__metaGraph` (branch list, ≤ 24-token synopsis each, node/edge category counts; target ≤ 500 tokens for ≤ 50 branches — *a design target verified by GAP-Bench*, not a claim), **L1** `__search` (top-k element hits + minimal SDL fragments), **L2** `__type(name)`, **L3** execute. Responses carry `tokensEstimate`. Query embeddings are cached (moka) by normalized text.
**Verification:** GAP-Bench (ADR-021); retrieval recall@10 ≥ 0.95 on RouteBench synthetic split.

## ADR-011 — Telemetry, load balancing, circuit breaker
**Status:** Accepted
**Decision:** Normative algorithm in `spec/04-routing.md` §6–8. Summary: per-branch `TelemetrySlot` (`CachePadded` atomics) with **time-decayed EWMA** latency (half-life 10 s) and error rate (half-life 30 s) updated by CAS loops; `InFlightGuard` (RAII) increments/decrements in-flight so cancellation/panic cannot leak; replica choice by **power-of-two-choices** over eligible members with cost `lat_ewma · (1 + inflight/cap)² / max(0.05, 1 − err_ewma)`; circuit breaker FSM Closed→Open→HalfOpen (30 s rolling window, min 20 requests, open at ≥ 50% failures or ≥ 80% slow-calls, exponential open time 5 s→60 s, 3 probes); retries only for idempotent reads, max 2, retry budget 10% of recent requests; hedging off by default. The old utility formula `U = P·(1−E) − λ…` is removed.
**Verification:** `shuttle` tests for counters; simulation test for herding (no oscillation > 20% share swing); chaos tests (WP-M3-07).

## ADR-012 — Cross-branch `@join` verification is data-driven
**Status:** Accepted
**Context:** A wrong join silently yields wrong data (review F-16); S1 was never validated for schema matching.
**Decision:** Join lifecycle `PROPOSED → VERIFIED → ACTIVE → DEPRECATED`. A candidate (from deterministic name/type heuristics, declared keys, or LLM proposal) becomes VERIFIED only if all hold: type-coercible; RHS key unique (declared PK/unique, or uniqueness ratio ≥ 0.999 on a ≥ 1,000-value sample); **containment** |LHS ∩ RHS| / |LHS| ≥ 0.95 estimated from tenant-salted blake3 bottom-k sketches computed *inside the connector boundary* (raw values never leave); null ratio ≤ 0.5; cardinality estimate consistent with declared `ManyToOne/OneToMany`; and (cross-trust-domain joins, or any join touching T2) human approval. Where a connector cannot supply samples, the join stays PROPOSED. The S1/semantic score is recorded as advisory metadata only. ACTIVE joins carry provenance (`@provenance`) and are re-verified on `spec_hash` change and on a schedule; drift moves them to DEPRECATED + alert. Canary: new joins first serve only `shadow` comparisons (planned and executed on sampled traffic, results diffed, not returned).
**Verification:** adversarial fixtures (same-type-different-entity keys must be rejected) in `tests/fixtures/joins/` (WP-M4-06).

## ADR-013 — Trust tiers; ingestion proposes, humans (or signed policy) activate
**Status:** Accepted
**Context:** Auto-discovery from `$PATH`/registries/MCP servers is a supply-chain and RCE risk (review F-17).
**Decision:** Tiers — **T0** first-party, manifest signed with an org Ed25519 key; **T1** admin-approved with pinned content hashes; **T2** discovered/untrusted. Rules: discovery workers MAY only *propose* (create `Proposal` records); activation requires an approval event unless the source is T0 and policy `auto_activate_t0 = true`. T2 branches are quarantined: read-only (all `@sideEffect` forced to `UNKNOWN`→denied), executed in a sandbox, invisible to agents unless policy `expose_t2 = true`, no cross-trust joins. Registry sources are an allow-list in config; the CLI extractor only runs binaries explicitly listed with absolute path + sha256 and never from `$PATH` search.
**Verification:** policy tests; E2E "malicious MCP server" scenario (WP-M6-10).

## ADR-014 — Description pinning, sandboxing, SSRF, injection posture
**Status:** Accepted
**Decision:** (1) Every tool/operation description and schema is content-hashed (`desc_hash`) and pinned at approval; any change ⇒ branch enters `DRIFT` (descriptions frozen at the pinned version for agents; new version queued for re-approval) — defeats rug pulls. (2) Text from branches is *data*: sanitized (ADR-005 §5), length-capped, scanned for instruction-like patterns and invisible characters before entering any LLM/S1 input; MCP-facade responses mark untrusted fields with provenance and (optionally) spotlight delimiters. (3) Egress: all outbound HTTP from connectors/ingestion goes through an SSRF guard (deny loopback/link-local/private/metadata IPs after DNS resolution, DNS pinning, redirect re-check, allow-list overrides). (4) Process connectors (CLI) and any code execution run in a sandbox (macOS `sandbox-exec` profile / Linux `bwrap`), no network by default, CPU/memory/time/output limits. (5) "Rule of Two" session policy: if a session combines untrusted content + sensitive data access + external state change, side-effecting calls require human approval. (6) Secrets via vault abstraction; never stored in snapshots, logs, traces; redaction layer on all outputs.
**Verification:** threat-model test matrix `spec/07-security.md` §10.

## ADR-015 — Mutations and side effects
**Status:** Accepted
**Decision:** `@sideEffect(level: NONE | REVERSIBLE | IRREVERSIBLE | UNKNOWN, idempotent: Boolean)` on every mutation field. Derivation is deterministic and conservative: OpenAPI `GET/HEAD/OPTIONS` ⇒ NONE only if the branch tier is T0/T1; other verbs ⇒ UNKNOWN until an admin classifies; MCP annotations (`readOnlyHint`, …) are untrusted hints (ignored for T2, advisory for T1); CLI ⇒ UNKNOWN. Policy default: UNKNOWN and IRREVERSIBLE require approval (`input-required` Task state, ADR-016); REVERSIBLE requires a declared `@compensate`. Every mutation carries `idempotency_key` (server-generated if absent), supports `dryRun: true` where the connector declares it, and is written to the audit chain. No cross-branch atomicity is promised; multi-branch writes use the optional saga mode with explicit compensations; partial failure is reported per branch.
**Verification:** `spec/06-runtime.md` §9 test list; WP-M6-04/05.

## ADR-016 — Task API compatible with A2A v1.0 semantics
**Status:** Accepted
**Decision:** `gap.submitTask / gap.task / gap.tasks / gap.cancelTask / gap.decideApproval` + `Subscription.gapTaskEvents`. States: `SUBMITTED, WORKING, INPUT_REQUIRED, AUTH_REQUIRED, COMPLETED, FAILED, CANCELED, REJECTED` (A2A v1.0 mapping table verified in WP-M6-02). Tasks are durable (`gap-store`), have a lease/heartbeat, resume after process restart, carry `context_id`, a deadline budget that decreases along hops, cancellation that propagates to branches, and a `hop_count`/`trace` list to reject cycles when GAP nodes peer (a GAP node is an NGC/A2A branch of another node). `federate` modes are task `strategy`: `PARALLEL | SEQUENTIAL | DAG` (TOURNAMENT deferred).
**Verification:** crash-recovery test, cancellation propagation test, A2A TCK-style fixtures.

## ADR-017 — MCP and A2A facades first; "replace" is the long-term outcome
**Status:** Accepted (user goal preserved)
**Context:** Hosts speak MCP; zero day-1 adoption without a facade (review F-23). MCP `2026-07-28` is stateless (no `initialize`, no session id), A2A v1.0 stable.
**Decision:** M1 ships an **inbound MCP server** exposing five static meta-tools — `gap_search`, `gap_describe`, `gap_query` (read-only, so hosts may auto-approve it), `gap_mutate` (destructive hint, approval pipeline), `gap_task` — plus resources for L0, and an **inbound A2A v1.0 binding** (`/.well-known/agent-card.json`, `SendMessage → submitTask`). Outbound adapters turn MCP servers and A2A agents into branches. Native GAP transport adoption is measured (GAP-Bench) and is the criterion for "replacement"; nothing in the design requires hosts to change on day 1.
**Verification:** `spec/09-compat.md` conformance suite (MCP and A2A fixtures), E2E with a real MCP client (WP-M1-11).

## ADR-018 — Tenant dimension in every key from day one
**Status:** Accepted
**Decision:** `TenantId` is a mandatory field in `RequestContext`, plan-cache key, telemetry slot key, task/audit/store rows, policy evaluation and metrics labels (bounded cardinality). Single-tenant deployments use a constant tenant. Cross-tenant access is impossible by construction (store APIs require a `TenantScope` token).
**Verification:** cross-tenant property tests (WP-M6-07).

## ADR-019 — Deferred scope (explicit non-goals for 1.0)
**Status:** Accepted
**Decision:** Not in 1.0: QUIC transport, Rhai/coprocessor plugin tiers (a compile-time `Plugin` trait with `tower::Layer` hooks is allowed), TOURNAMENT federation mode, field-level `@rateLimit` (global/per-subject limits via `tower`/`governor` are in), multi-region replication, training a custom System-1 (Track R1), SDK codegen beyond thin TS/Python clients. Each has a tracking entry in `docs/02-PLAN.md` §8.

## ADR-020 — `ContextStore` trait; SQLite default; SurrealDB optional
**Status:** Default (D-003)
**Decision:** `gap-store` defines `ContextStore` (nodes, typed edges, temporal validity, provenance, scoped memory, traversal) plus `TaskStore`, `AuditLog`, `IdempotencyStore`. Default implementation: embedded SQLite via `rusqlite` (WAL, FTS5, recursive CTE traversal, bounded depth). `store-surreal` feature implements the same trait on SurrealDB 3.x (BSL: gated and documented). Conformance tests for the trait run against every backend. Scopes: `Private | Task | Group | Global | Ephemeral`; temporal edges use `valid_from/valid_to` and `SUPERSEDES`.

## ADR-021 — No unmeasured claims in normative documents
**Status:** Accepted
**Decision:** The spec states *targets* and *gates*, never marketing figures. The "45,000 → 500 tokens / 90× / $ saved" statements are removed (review F-06). `bench/gapbench/` measures, for N ∈ {10, 50, 200, 1000} tools and 3 task suites: prompt tokens consumed before the first useful call, tokens per successful task, task success rate, number of round-trips, latency, with and without provider prompt caching, GAP-via-MCP-facade vs. direct MCP baseline. Results are published in `bench/results/` with raw data and commit hash.

## ADR-022 — Agent execution protocol and isolation
**Status:** Accepted
**Context:** `write_scopes` is not an omni DAG field (review F-10); user rules require bite-sized, spec-driven work.
**Decision:** The unit of work is a **Work Package** (`work/work-packages.toml`): id, milestone, dependencies, `owns` (path globs), deliverables, acceptance tests, commands, size. One WP = one branch/worktree (GitButler virtual branch per the user's rules), Conventional Commits, merge only when acceptance commands pass. `tools/wp.py validate` checks: DAG acyclic, ids unique, `owns` globs of *concurrently runnable* WPs are disjoint, every WP has ≥ 1 executable acceptance command. `tools/check_layers.py` enforces crate layering. Agents expand a WP into 2–5-minute steps (exact paths, code, commands) *before* coding (`docs/AGENT-GUIDE.md` §4). Spec changes go through ADR + spec + test in one change set. Stop-and-ask conditions are enumerated in the guide.

## ADR-023 — Error model and HTTP mapping
**Status:** Accepted
**Decision:** Operation-level failures return HTTP 200 with GraphQL `errors[]` (GraphQL-over-HTTP `application/graphql-response+json`); malformed requests/auth failures use 4xx/5xx. Every error has `extensions.code` (stable string enum in `spec/02-gap-ql.md` §10), `extensions.retry {retryable, afterMs}`, optional `extensions.suggestion` (self-repair hint for LLMs, e.g. `didYouMean`, alternative fields) and `extensions.path`. `GAP_INSUFFICIENT_EVIDENCE` is a normal 200-level discovery outcome, not a 5xx. Numeric codes (402/504) are removed.

## ADR-024 — Naming and versioning
**Status:** Default (D-001)
**Decision:** Working name **GAP**; spec version `0.1.0-draft`, wire string `gap/0.1` (the old "v2" was a plan iteration, not a wire version). SemVer for spec and crates; breaking wire changes only on minor bumps before 1.0 with a migration note; after 1.0, N-1 compatibility window. Snapshot on-disk/format version `snapshot_format_version: u32` with an explicit migration trait. Binary `gap`; crates `gap-*` (never `gap-protocol`, `gap`).

## ADR-025 — Scope, capacity-hint and deferral clarifications
**Status:** Default (D-007, D-008)
**Context:** The context-bridge review of the original design session (`work/context/session-bridge.md` §E) confirmed six real gaps between ADR-019/ADR-020 and the spec; a seventh finding (`@defer/@stream`) was rejected because RUN-062 already covers it. This ADR supplements ADR-019 and ADR-020; it changes none of their decisions.
**Decision:**
1. *Context scopes (D-007).* All five scopes of ADR-020 exist in `ContextStore` with the visibility and lifecycle rules ING-065/ING-066. GAP-QL exposes `GapNode.scope` (read-only) and a `TraverseInput.scopes` filter that can only narrow visibility (GQL-043). There is no built-in `remember` mutation in 1.0: scoped writes use the store API and connector `Mutate` (ADR-015).
2. *Capacity weight.* No change to `ngc.proto`. `HealthResponse.details["capacity_weight"]` is the canonical per-connector capacity signal (NGC-072); in-process adapters derive it from rate-limit headers (CMP-033), out-of-process connectors report it themselves.
3. *`WEIGHTED` traversal.* Edge cost, tie-break and the unsupported-branch error are fixed by ING-067.
4. *Deferred past 1.0 (D-008), supplementing ADR-019:* interval/history temporal walks (`@between`, `[from, to]` over superseded nodes) — only `asOf` ships; multi-pass System-1 tournament — RTE-030's single-pass shortlist is the 1.0 behaviour and review item K-7 records Laya's technique, not a requirement; listener-socket handoff for single-node binary upgrades — zero-downtime upgrades use the active/standby topology of OPS-003. Tracking entries are in `docs/02-PLAN.md` §8.
**Consequences:** WP-M2-01 and WP-M2-03 implement scopes and `WEIGHTED` without guessing; the `ngc.proto` golden of WP-M0-06 is unaffected; conformance tests for ING-065 (visibility is indistinguishable from absence) and ING-067 (determinism) are required in the `ContextStore` trait suite.
