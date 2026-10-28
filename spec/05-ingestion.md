# GAP Specification — 05 Ingestion, Synthesis and Verification (control plane)

Normative IDs `ING-*`. Rationale: ADR-012, 013, 014. "System-1.5" of the earlier draft is realized as **Enricher** (optional, constrained LLM) inside a pipeline whose *trusted* parts are deterministic extraction and symbolic/data-driven verification.

## 1. Pipeline and Proposal lifecycle
```mermaid
stateDiagram-v2
  [*] --> PROPOSED: proposeSource / DiscoveryWorker / drift detector
  PROPOSED --> EXTRACTING: approved to inspect (policy)
  EXTRACTING --> ENRICHING: CISG built (deterministic)
  ENRICHING --> VERIFYING: enrichment done (or skipped)
  VERIFYING --> STAGED: gates G1-G9 pass => shadow Snapshot built
  VERIFYING --> FAILED: gate fails (report attached)
  STAGED --> AWAITING_APPROVAL: policy requires approval
  STAGED --> PROMOTING: auto-activation allowed (T0 + policy)
  AWAITING_APPROVAL --> PROMOTING: decideProposal(APPROVE)
  AWAITING_APPROVAL --> REJECTED: DENY / TTL(14d)
  PROMOTING --> ACTIVE: prewarm + swap succeeded
  ACTIVE --> WATCHING: 60 s watch window
  WATCHING --> STABLE: no attributable regression
  WATCHING --> ROLLED_BACK: rollback rule (ADR-009 s5)
  STABLE --> PROPOSED: drift detected / scheduled re-verify
```
- **ING-001** Every transition is persisted, audited (INV-14) and emits `gap_ingest_transitions_total{from,to}`. The *report* (JSON) lists inputs (source URL/path, `spec_hash`), tool versions, model ids, each gate result with evidence, and diffs vs. the previous generation.
- **ING-002** A Proposal is idempotent on `(tenant, location_canonical, kind)`: re-proposing returns the open Proposal.
- **ING-003** *Propose-not-activate* (ADR-013): moving from `PROPOSED` to `EXTRACTING` for any T2 source, and from `STAGED` to `PROMOTING` for any T1/T2 source, requires an explicit approval event by a principal with `admin:ingest`. T0 sources MAY auto-activate when `auto_activate_t0 = true` and the signed manifest verifies (SEC-060).

## 2. Source extractors (deterministic stage A)
All extractors implement `trait Extractor { fn kind() -> Protocol; fn extract(&self, src: &SourceHandle, limits: &ExtractLimits) -> Result<Cisg, ExtractError>; }`, are pure functions of their input bytes (+ pinned tool versions), have `ExtractLimits { max_bytes, max_depth, max_items, timeout }`, and each has a fuzz target (`fuzz_extract_<kind>`).

| Kind | Input | Output specifics | Security notes |
|---|---|---|---|
| OpenAPI 3.0/3.1 (+ Swagger 2 via conversion) | URL (SSRF-guarded) or file | operations = path×method; params→args; request/response schemas→types; `operationId` or synthesized name; pagination detection (`cursor/next/limit/offset` patterns); side effect from verb (ADR-015); `servers[]` URLs recorded but **never auto-followed** | `$ref` resolved locally only; external refs rejected; size ≤ 50 MiB |
| gRPC | `.proto` files or server reflection | services/rpcs→operations; messages→types; streaming→subscription/stream | reflection only against allow-listed hosts |
| GraphQL | SDL or introspection result | root fields→operations; keep native types; detect Relay connections | depth-limited introspection |
| SQL | DDL files or `information_schema` snapshot | tables→node types; FKs→candidate joins (`evidence=FK`); unique indexes→key fields | **no data access** at extraction; connector later read-only |
| MCP server | `tools/list`, `resources/list`, `prompts/list` over spec `2026-07-28` (fallback `2025-11-25`) | tool→operation (QUERY unless classified), JSON Schema→args; resources→node categories | descriptions untrusted (ADR-014); annotations are hints; server launched only per ADR-013/SEC-070 |
| A2A agent | AgentCard (`/.well-known/agent-card.json`) | skills→TASK operations; modes/capabilities recorded | card signature verified if present |
| CLI | explicitly allow-listed binary (absolute path + sha256) with declared help subcommands | subcommands/flags→operations (UNKNOWN side effect) | runs sandboxed, no network, time/output caps; never from `$PATH` search (ADR-013) |
| Code & docs | repository path(s) | tree-sitter AST → nodes/edges (§8) | file-size and file-count caps; no code execution |

- **ING-010** Extraction MUST be reproducible: same input bytes + same extractor version ⇒ byte-identical CISG (canonical JSON, sorted). The CISG records `extractor_id`, `extractor_version`, `source_hash`.
- **ING-011** Extractors MUST NOT perform network or filesystem access beyond the `SourceHandle` (enforced by running them in a restricted worker process: no network namespace unless the kind requires fetch, in which case via the SSRF-guarded fetcher).
- **ING-012** A malformed or hostile input MUST yield `ExtractError` within `timeout`, never a panic, hang or unbounded allocation (fuzzed for ≥ 1 h per target in CI nightly; 24 h before each release).

## 3. CISG (canonical intermediate schema graph)
```rust
pub struct Cisg {
    pub source: SourceRef,                 // kind, canonical location, source_hash
    pub extractor: ExtractorInfo,          // id, version
    pub types: Vec<TypeDef>,               // objects, enums, scalars (with JSON Schema where known)
    pub operations: Vec<OperationDef>,     // see below
    pub node_categories: Vec<String>, pub edge_predicates: Vec<String>,   // for graph-ish sources
    pub keys: Vec<KeyDef>,                 // PK/unique/identifier fields
    pub candidate_joins: Vec<CandidateJoin>,  // from FKs / naming heuristics / declared relations (evidence tagged)
    pub docs: Vec<DocBlob>,                // free text, each with provenance + hash
}
pub struct OperationDef {
    pub name: String, pub kind: OpKind, pub args: Vec<ArgDef>, pub result: TypeRef,
    pub description: String,               // UNTRUSTED
    pub side_effect: SideEffect,           // derived deterministically (ADR-015), never from description
    pub idempotent: Option<bool>, pub pagination: Pagination, pub key_fields: Vec<String>,
    pub examples: Vec<Example>, pub native_ref: serde_json::Value,   // e.g. {method:"GET", path:"/v1/invoices/{id}"}
}
pub struct CandidateJoin { pub from: FieldRef, pub to: FieldRef, pub cardinality: Cardinality, pub evidence: JoinEvidence /* FK | DECLARED | NAME_TYPE | LLM */ }
```
- **ING-020** `description`/`docs` text is stored as `UntrustedText { text, sanitized, lint: Clean|Flagged(reasons), hash }`; only `sanitized` flows downstream.
- **ING-021** `candidate_joins` with `evidence = LLM` are second-class: they can only be proposed, never auto-promoted beyond VERIFIED (ADR-012).

## 4. Enricher (optional, constrained generation)
- **ING-030** `trait Enricher { async fn enrich(&self, cisg: &Cisg, ctx: &EnrichCtx) -> Result<Enrichment, EnrichError>; }`. Implementations: `NullEnricher` (deterministic heuristics only; always available), `LlmEnricher` (provider-agnostic: local Ollama/llama.cpp, any OpenAI-compatible endpoint, Anthropic; selected by `ingest.enricher.provider`).
- **ING-031** Output is constrained by grammar/JSON-Schema (`llguidance`) to: per-operation `description_rewrite` (≤ 240 chars), `examples` (≥ 3 positive, ≥ 2 negative intents), `primary_fields[]` (≤ 3), `capability_group_hint`, branch `synopsis` (≤ 24 model tokens), and `candidate_joins[]` with a rationale. The enricher can **only** add descriptive metadata and *candidates*; it cannot set trust tier, side-effect level, auth, scopes, endpoints or hashes (those fields are not in the output schema).
- **ING-032** Inputs to the LLM are sanitized, size-capped, and wrapped as data (delimited fields, never concatenated into instructions); the prompt template and its version are part of the cache key `(source_hash, enricher_id, prompt_version, model_id)`.
- **ING-033** Privacy default: `allow_cloud_enrichment = false` — only locally hosted or explicitly configured endpoints receive source content (D-006). With cloud enabled, a `redact` pass removes values/secrets/PII from docs and examples.
- **ING-034** Enrichment quality is measured, not assumed: `bench/enrichment/` contains golden specs (≥ 30 public OpenAPI/proto/SQL samples with hand-checked joins and synopses). Gate G-E1 (M4): join proposal precision ≥ 0.9 at recall ≥ 0.6 on the golden set *after* G4 verification, ≥ 0.98 precision for the VERIFIED set; synopsis human-rated usefulness ≥ 4/5 on a 20-item sample; no hallucinated operation names (0 tolerance, checked mechanically against CISG).
- **ING-035** Failure of the enricher (timeout, invalid output after 2 constrained retries, provider down) MUST NOT fail the pipeline: fall back to `NullEnricher` output and mark `enrichment=degraded` in the report; no auto-activation when degraded.

## 5. Verification gates
Gates run in order; a failure stops the pipeline with `FAILED` and a precise report. "Hard" gates cannot be overridden; "soft" gates can be acknowledged by an admin with a reason (audited).

| Gate | Check | Kind | Fail action |
|---|---|---|---|
| **G1 Schema validity** | composed supergraph SDL validates (apollo-compiler); no reserved names (GQL-004); names unique; type refs resolve | hard | FAILED |
| **G2 Type coercion** | for each join key pair: scalar types coercible (`ID/String/UUID/Int` rules in table below), nullability compatible | hard | candidate dropped |
| **G3 Key lookup** | RHS key is declared unique/PK or `uniqueness_ratio ≥ 0.999` (sketch); RHS supports batched lookup (`foreach` or `IN` predicate) | hard | candidate dropped |
| **G4 Data-driven join verification** (ADR-012) | containment ≥ 0.95, null ratio ≤ 0.5, cardinality consistent, sample ≥ 1,000 distinct values (or entire domain if smaller); sketches via NGC `Sketch` with tenant salt | hard for ACTIVE | stays PROPOSED/VERIFIED |
| **G5 Semantic plausibility** | S1/embedding similarity of field names/docs; **advisory only**, recorded in evidence | soft | none (annotation) |
| **G6 Boundedness** | every cycle in the join graph includes a `traverse`-bounded or depth-limited entry; max join fan-out per hop ≤ declared cap | hard | `GAP_CYCLE_UNBOUNDED` |
| **G7 Security lint** | description injection linter (SEC-081); no reserved/hidden Unicode; `desc_hash` computed; SSRF re-check of any URL in spec; side-effect classification complete (no op left unclassified for T0/T1 without admin ack) | hard | quarantine T2 / FAILED |
| **G8 Regression replay** | re-plan the top-N (default 200) cached/persisted operations and the conformance fixtures on the shadow snapshot; diff results of read-only ones on sampled traffic (shadow execution, results discarded) | hard if breaking, soft otherwise | FAILED or `breaking_change_ack` |
| **G9 Resource budget** | snapshot size/RSS delta within limits; build time within budget | soft | admin ack |

Coercion table (G2): `UUID↔String(format uuid)`, `Int↔Int64`, `Int↔String(numeric)` only with explicit `@join(on: {from,to, coerce: …})`, `ID↔String` allowed, `Int↔Boolean` forbidden, `Float↔Int` forbidden for keys.
- **ING-040** A join becomes `ACTIVE` only after G1–G4, G6–G8 pass and (for cross-trust or T2) approval; before that it is invisible to planning (INV-06).
- **ING-041** *Shadow canary:* a newly ACTIVE join first runs in `shadow` mode on sampled real traffic for ≥ 100 executions or 10 minutes: plans and executions run, results are compared (row counts, key coverage) against expectations from sketches; divergence beyond tolerance (coverage drop > 10 pts) reverts the join to VERIFIED and alerts. The join serves clients only after the canary passes.

## 6. Staging, approval, promotion, drift
- **ING-050** A *shadow Snapshot* is built from `current + proposal` (structural sharing, ADR-009). It is never visible to clients until promotion; admin tooling can query it via `X-Gap-Snapshot: shadow:<proposal_id>` (requires `admin:schema`).
- **ING-051** Approval policy (Cedar) examples: `T0 && signed && !breaking → auto`; `T1 → one admin`; `T2 or cross-trust join or mutation-capable → two-person rule` (configurable).
- **ING-052 Drift detection:** every `drift.interval` (default 15 min, jittered) call `Describe(known_spec_hash)`; on change compute `desc_hash` per operation and classify: **COSMETIC** (hash unchanged, e.g., ordering) → ignore; **SCHEMA_COMPATIBLE** (added optional fields/operations) → new Proposal (auto-verify, approval per policy); **SCHEMA_BREAKING** (removed/renamed field or operation, type change, new required argument, side-effect-relevant change) → Proposal with `breaking=true` and the branch enters `DRIFT`; **DESCRIPTION_CHANGE** (descriptions changed while schema same) → branch enters `DRIFT`.
- **ING-053** While a branch is in `DRIFT`, agents see the **pinned** descriptions (INV-08) and execution continues against the pinned schema only if the connector still accepts the pinned operation signatures (verified by a cheap probe); otherwise affected operations return `GAP_BRANCH_DRIFT`. Re-approval of the new version clears `DRIFT`.
- **ING-054** *Rug-pull test* (WP-M6-10): a mock MCP server that changes a tool description after approval must (a) not alter any text served to agents, (b) raise an alert, (c) create a Proposal, (d) leave audit records.
- **ING-055** Removing a branch: operations become `DEPRECATED` for one `grace` period (default 24 h) returning `GAP_BRANCH_UNAVAILABLE` with `suggestion.alternative` (if a group sibling exists), then disappear.

## 7. Discovery-triggered proposals
Handled by the `DiscoveryWorker` (RTE-100..103): coalesced `UnmappedDomainEvent`s → registry lookup (allow-list) → `Proposal(PROPOSED)` with `notes` = evidence (top groups, query hash). No execution, no network fetch of unapproved code. Admin UI/CLI: `gap proposals list|show|approve|deny`.

## 8. Code and document graph (the original motivating use case)
- **ING-060** `gap-ngc-code` indexes repositories into a `ContextStore` graph. Node types: `Repo, File, Module, Symbol(kind: function|method|struct|enum|trait|class|interface|const|type), Doc(kind: readme|adr|api|comment|markdown_section), Test, Concept`. Edge types: `DEFINES, CONTAINS, IMPORTS, CALLS, IMPLEMENTS, EXTENDS, REFERENCES, DOCUMENTS, TESTS, DEPENDS_ON, SUPERSEDES`. Each edge has `resolution: EXACT|HEURISTIC` and `confidence ∈ [0,1]`; tree-sitter-derived call edges are `HEURISTIC` unless confirmed by an importer of a precise index (SCIP/LSIF import is an optional extractor: `ingest.code.scip_paths[]`).
- **ING-061** Languages in 1.0: Rust, TypeScript/JavaScript, Python, Go, Java, Markdown. Grammar versions pinned; a parse failure of one file never aborts the run.
- **ING-062** Incremental indexing: file content hash → re-extract only changed files; deleted files tombstoned with `valid_to` (temporal graph, ADR-020); full re-index reproducible.
- **ING-063** Node summaries: each Symbol/Doc has `summary` (≤ 40 tokens; deterministic first-sentence/signature extraction; LLM summary optional via Enricher with the same constraints). `traverse(returnFields:[...])` returns slices; target (Gate G-C1 at M2): answer the benchmark question set "what calls X / what breaks if I change X / where is Y documented" with ≤ 15% of the tokens of a file-scanning baseline at ≥ equal answer accuracy.
- **ING-064** Graph store limits: ≤ 5 M nodes / 20 M edges per tenant in the default SQLite backend (documented ceiling; larger needs another backend). `traverse` is bounded by depth ≤ 16, `first` ≤ 1,000, and a per-query visited cap (default 100,000).
- **ING-065** *Context scopes (ADR-025, D-007).* Every `ContextStore` node carries `scope ∈ {PRIVATE, TASK, GROUP, GLOBAL, EPHEMERAL}` plus `owner_principal` and, where the scope needs them, `task_id`, `group` and `expires_at`. The store evaluates visibility **inside the caller's tenant** (INV-09), before and independently of Cedar: `PRIVATE` → `owner_principal == caller.principal`; `TASK` → `task_id` is the caller's task or one of its ancestors in the A2A task chain (ADR-016); `GROUP` → `group ∈ caller.groups` (principal claim); `GLOBAL` → every principal of the tenant; `EPHEMERAL` → visible exactly like the scope it was created under, until `expires_at`. A node the caller may not see MUST be indistinguishable from a node that does not exist (no error, no count, no `truncated` hint). Nodes written by ingestion (ING-060…) are `GLOBAL`; any other writer MUST state the scope explicitly. An edge takes the most restrictive scope of its two endpoints.
- **ING-066** *Scope lifecycle.* `EPHEMERAL` nodes MUST carry `expires_at` ≤ 24 h after creation (default 1 h); the store never returns an expired node and purges it within 60 s. `TASK` nodes are tombstoned when their task reaches a terminal state and hard-deleted after a retention window (default 7 days, configurable). `PRIVATE`, `GROUP` and `GLOBAL` nodes persist until superseded (`valid_to`) or deleted. Scoped memory is written only through the store API and connector `Mutate` under the ADR-015 rules; 1.0 has no built-in `remember` mutation.
- **ING-067** *`WEIGHTED` traversal (ADR-025).* `TraverseStrategy.WEIGHTED` is Dijkstra over the edge cost `GapEdge.props["weight"]` (finite, ≥ 0; absent ⇒ 1.0). Edges whose weight is negative, NaN or infinite are skipped and reported as a `GAP_PARTIAL_RESULT` warning; equal-cost ties are broken by the smaller node `id`, so cursors stay deterministic. A native-traverse branch that cannot honour `WEIGHTED` MUST answer `GAP_UNSUPPORTED_FEATURE`. Interval or history walks over superseded nodes (`@between`) are not part of 1.0: only point-in-time `asOf` is (ADR-025).

## 9. Isolation and resource limits
- **ING-070** Extraction/enrichment run in worker processes (`gap-ingest-worker`) with rlimits (CPU time, address space, open files), `PR_SET_NO_NEW_PRIVS` / seatbelt profile where available, no network except through the SSRF-guarded fetcher; worker crash ⇒ Proposal `FAILED` with report, never server crash.
- **ING-071** Concurrency: at most `ingest.max_concurrent` (default 2) pipelines per tenant; per-pipeline wall-clock cap 30 min (configurable); backlog metric `gap_ingest_backlog`.

## 10. Required tests
Extractor determinism (ING-010) · extractor fuzz (ING-012) · hostile-spec corpus (zip-bomb-like JSON, deep nesting, huge `$ref` chains, external refs) · enricher cannot set forbidden fields (ING-031, schema-level) · degraded fallback (ING-035) · G2 coercion table · G3/G4 on adversarial fixtures (same-type-different-entity keys, partial overlap, skewed duplicates) · canary revert (ING-041) · drift classification table (ING-052) · rug-pull scenario (ING-054) · incremental code indexing idempotence (ING-062) · rollback attribution (ADR-009) · golden-set gate G-E1 (ING-034) · token-efficiency gate G-C1 (ING-063).

## 11. ContextStore API contract (WP-M2-01)
Rules of the `gap-store` crate (layer L1, ARC-002). It does not depend on `gap-ql`, so `where` predicates and `returnFields` are applied by the upper layers over the returned slices. Promoted from the reviewed design notes of WP-M2-01; each rule is pinned by tests of that WP.
- **ING-080** *API shape.* `ContextStore` is synchronous (async callers use `spawn_blocking`). Every method takes a `TenantScope` first (INV-19) and the caller or writer identity (`principal`, `groups`, the current task followed by its ancestors). All time comes from an injected `Clock` and nothing runs in the background. `put_batch(nodes, edges)` applies the nodes and then the edges in one transaction, is all-or-nothing, and takes at most 10,000 items. A batch cannot reference by id a non-`GLOBAL` node created in the same batch, because the store mints that id.
- **ING-081** *Identifiers.* Only `GLOBAL` nodes accept a caller-chosen id on creation. Every other scope gets a store-minted UUIDv7 (injected clock plus CSPRNG entropy), so ids of scoped nodes cannot be probed. Updating a scoped node needs its minted id and the node MUST be visible to the writer; a write that targets a node the writer cannot see MUST fail with `NotFound` exactly like a missing id. A `GLOBAL` write that collides with a node the writer cannot see fails with `Conflict`. A `GLOBAL` node MUST NOT be turned into a non-`GLOBAL` node.
- **ING-082** *Versions.* A node id has at most one current version and edges are unique per `(tenant, type, from, to)`. Timestamps are stored as microseconds UTC. Re-putting identical content (blake3 of the canonical JSON of type, label, summary, props, scope and provenance) MUST NOT create a version; changed content closes the current version (`valid_to = now`) and opens a new one. `tombstone_node` closes the node and all its current edges in one transaction. `delete_node` is a hard delete of every version and incident edge and is also allowed on a tombstoned node. `asOf(t)` selects `valid_from <= t < valid_to`.
- **ING-083** *Visibility.* A node the caller cannot see MUST behave as absent in `get`, `traverse`, `counts` and `search`, and in every statistic (`visited`, `returned`, `truncated` and `depth_reached` count visible nodes only). An edge is visible iff both of its endpoints are visible. `ContextMaintenance::purge_expired` returns counts only, never tenant data.
- **ING-084** *Traversal order.* The order of the returned nodes is total and deterministic. `BFS`: by (depth, id). `SHORTEST`: the BFS shortest-path tree, each reached node with exactly one edge, to its smallest-id predecessor at minimal depth and then the smallest edge id. `DFS`: preorder over neighbours by (id, edge id); a node met again at a smaller depth is expanded again, so every node within `maxDepth` is reached. `WEIGHTED`: by (cost, id), where the cost of a node is the minimum over paths of at most `maxDepth` edges and its depth is the smallest hop count among its minimum-cost paths. When `visited_cap` is reached, the whole equal-cost group at the cap MUST be completed before the order is applied and cut, because zero-weight edges can add nodes of the same cost. The returned edges are the traversed edges whose two endpoints are both in the returned page, ordered by (depth of the edge's `from` node, edge id); a BFS that was cut by the cap does not expand the nodes at the cut depth.
- **ING-085** *Paging.* `TraverseResume` is stateless: it is the order key of the last returned node. The next call re-runs the walk with the same `asOf` and skips the keys up to it; the visited cap counts the skipped nodes. Following `resume` MUST terminate and concatenate to the unpaged result without duplicates or gaps. `truncated` means that the visited cap stopped the walk while more visible nodes were reachable and is independent of paging. Cursor signing belongs to the resolver (WP-M2-03).
- **ING-086** *Search.* `search_nodes` treats the user string as literals: alphanumeric tokens, each quoted, all required. FTS5 operators have no effect and an empty token list returns nothing. Both the matches and their order MUST depend only on the caller's tenant, so there is one FTS5 table per tenant named `nodes_fts_<16 hex digits of blake3(tenant id)[..8]>`, created lazily. Only that hex string is ever interpolated into SQL, never the tenant string.
- **ING-087** *Storage and limits.* The format version lives in `PRAGMA user_version`, with ordered migrations of one transaction each. Opening a database with a higher version MUST fail with `FormatTooNew` without modifying it. `open` and `restore_from` MUST read only the file header, never the whole file. Writes enforce: ids 512 bytes, label 512, summary 1,024, props 64 KiB with nesting depth 16, group claim 128, batch 10,000 items; reads enforce traverse depth 16, `first` 1,000, `visited_cap` 1,000,000 and search limit 100; the per-tenant ceilings of ING-064 are rejected with `QuotaExceeded`. Backup is online; `restore_from` refuses an existing target and rejects a file that fails `integrity_check` or has an unsupported version.
- **ING-088** *Bounded work.* The cost of every `ContextStore` operation MUST be bounded by the rows it touches, never by the size of the tenant: no statement of `get`, `put_batch`, `traverse` (every strategy and direction), `search_nodes`, `tombstone_node` or `delete_node` may scan the `nodes` or `edges` rows of a tenant, and the query plans MUST NOT depend on `ANALYZE` statistics, because a fresh database has none. Only `counts` may be proportional to the size of the tenant. The per-tenant ceilings of ING-064 (5 M nodes, 20 M edges) are only meaningful under this rule.
