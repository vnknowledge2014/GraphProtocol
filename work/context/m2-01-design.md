# WP-M2-01 design notes (reviewer decisions)

Written 2026-10-27 by the reviewer (Claude Code) before dispatching WP-M2-01 (`gap-store`).
Status: **promoted on 2026-10-28.** The rules that survived the WP-M2-01 review are now normative in `spec/05-ingestion.md` §11 (ING-080..ING-088, with tests); where this file and the spec differ the spec wins (known differences: §7 FTS is one table per tenant since review round 1 / R-4, the format version is 4, and ING-088 bounds the work of every hot-path statement). Original status: **non-normative until promoted.** The spec (ING-060…067, INV-09, INV-19, ADR-020, ADR-025) fixes the
behaviour of scopes and limits but not the store API. This file resolves what the spec leaves open so that the
implementing agent does not have to guess. After the WP is reviewed, the rules that survive become spec bullets
(new ING bullets) in the same change set as their tests. A deviation from this file is allowed only if it is listed
under Deviations in the handoff with the reason.

## 1. Scope, layering, dependencies

- `gap-store` is layer L1 (`tools/layers.toml`): it may depend on `gap-core` only. It must **not** depend on `gap-ql`,
  so the GAP-QL `Predicate` is not available here. `where` filtering is done by upper layers over returned slices;
  the store filters by edge type, direction, depth, strategy, `asOf` and scopes only.
- Allowed crates are exactly those of the WP-M2-01 row in `work/context/offline-vendoring.md`. Nothing else.
- The API is **synchronous** (rusqlite is blocking). Async callers use `spawn_blocking`. No background threads or
  timers inside the crate (a purge scheduler belongs to the gateway); tokio is allowed but not required.
- All wall-clock reads go through an injected `gap_core::clock::Clock` (INV-02; `clippy.toml` forbids direct calls).
- Keep the header of `lib.rs` (`forbid(unsafe_code)`, `deny(clippy::unwrap_used, expect_used, panic, todo,
  unimplemented)`). Do not touch `src/feedback/` and `src/surreal/` (later WPs); feature `store-surreal` stays empty.

## 2. Public types (names are binding, fields may be refined)

- Reuse `gap_core::ids::{TenantId, TenantScope, TaskId}` and `gap_core::clock::{Clock, Timestamp}`. Do **not** define a
  second tenant scope type; the WP deliverable "TenantScope token type" is met by re-exporting the gap-core type
  and by the rule in section 3.
- `NodeId`, `EdgeId`: validated string newtypes, 1..=512 bytes, no control characters, no NUL. `NodeType`, `EdgeType`:
  `[A-Za-z][A-Za-z0-9_]{0,63}`. Provenance `source` 1..=256 bytes.
- `Scope { Private, Task, Group, Global, Ephemeral }` (what is reported) and `ScopeSpec` (what a writer states):
  `Private`, `Task { task_id: TaskId }`, `Group { group: String }`, `Global`,
  `Ephemeral { base: BaseScope, expires_at: Timestamp }` with `BaseScope { Private, Task{..}, Group{..}, Global }`.
  `Ephemeral` is visible exactly like its base (ING-065). `group` is a non-empty string of at most 128 bytes (the
  principal's group claim; it is not the capability `GroupId` of gap-core).
- `Caller { principal: String, groups: BTreeSet<String>, task_chain: Vec<TaskId> }`, where `task_chain` lists the
  caller's current task followed by its ancestors (ADR-016). Built by the gateway from `RequestContext`.
- `Node { id, node_type, label, summary: Option<String>, props: serde_json::Map, scope: Scope, owner_principal,
  task_id: Option<TaskId>, group: Option<String>, expires_at: Option<Timestamp>, provenance, valid_from,
  valid_to: Option<Timestamp> }`. `Edge { id, edge_type, from, to, props, confidence: Option<f32> in [0,1],
  resolution: Exact|Heuristic, provenance, valid_from, valid_to }`. `NewNode` / `NewEdge` are the write inputs.
- Limits enforced on write (typed `StoreError::Invalid { field, reason }`): label ≤ 512 bytes, summary ≤ 1,024 bytes,
  props JSON object ≤ 64 KiB serialized and nesting depth ≤ 16, group ≤ 128 bytes.
- `StoreError` (thiserror): `Invalid`, `NotFound`, `Conflict`, `QuotaExceeded`, `FormatTooNew`, `Corrupt`, `Busy`,
  `Backend` (opaque). `Display` never contains SQL text, file paths, props or ids of nodes the caller cannot see.
  Provide `StoreError::into_gap_error(self, correlation_id)`: `Invalid` becomes `GAP_VALIDATION_ERROR`, everything
  else becomes the opaque `GapError::internal(correlation_id)`; the detail goes to `tracing` at `warn`, never props
  or secrets (INV-15).

## 3. Tenant isolation (INV-09, INV-19)

- Every trait method takes `&TenantScope` as its first argument; there is no method that reads or writes without it.
- Physical rule: `tenant` is a column of every table (nodes, edges, FTS rows, stats) and the **leading column of every
  index**; every statement binds `tenant = ?`. SQL text is built only from constants; all values are bound
  parameters (hostile ids and labels such as `'; DROP TABLE nodes;--` must be stored and returned verbatim).
- FTS5 rows carry the tenant; a search joins back to `nodes` and filters by tenant and visibility. A term that exists
  only in tenant A must return nothing for tenant B (property test).
- Cross-tenant property tests (proptest, 3 tenants): random operation sequences are applied to the store and to a
  per-tenant in-memory model; after every sequence each tenant's observable state (get, search, traverse, counts)
  equals its own model, i.e. no operation of tenant X is observable from tenant Y.

## 4. Temporal model (ADR-020, ING-062)

- Timestamps are stored as INTEGER microseconds UTC (`Timestamp::unix_micros`), never as text.
- A node id has at most one **current** version (`valid_to IS NULL`; enforce with a partial unique index per tenant).
  `put_node` on an existing current version: identical content (blake3 of the canonical JSON of type, label, summary,
  props, scope, provenance) is a no-op (no new version, so incremental indexing stays idempotent, ING-062); changed
  content closes the old version (`valid_to = now`) and inserts a new one (`valid_from = now`).
- Same for edges, unique current edge per `(tenant, edge_type, from, to)`.
- Writes take the same `Caller` as reads (the writer). `put_batch(tenant, writer, nodes, edges)` applies the nodes and then
  the edges in **one transaction** and is atomic: if any item is invalid, nothing of the batch is persisted. `put_node` and
  `put_edge` are single-item wrappers. A batch is capped at 10,000 items. This exists so that ingesting a large repository
  is not one commit per row; a test inserts 50,000 nodes and 100,000 edges through batches and checks the counts.
- `tombstone_node` sets `valid_to = now` on the node and on all its current edges in one transaction.
  `delete_node` is a hard delete (used by retention) and removes its edges.
- `as_of` selects rows with `valid_from <= t < COALESCE(valid_to, +inf)`. Default is `clock.now()` resolved once per
  call and reported back in the result (`TraverseResult.as_of`) so that a page cursor can pin it (WP-M2-03).
- `SUPERSEDES` is an ordinary edge type the writer creates; the store adds no special semantics.

## 5. Scopes and visibility (ING-065, ING-066)

- Visibility is decided inside the store, inside the tenant, before and independently of Cedar. A node is visible to
  a `Caller` iff: `GLOBAL`; or `PRIVATE` and `owner_principal == caller.principal`; or `GROUP` and
  `group ∈ caller.groups`; or `TASK` and `task_id ∈ caller.task_chain`; or `EPHEMERAL`, not expired
  (`expires_at > now`), and its base scope rule holds. Store the base scope in its own column.
- An **edge is visible iff both endpoints are visible** (this realises "most restrictive scope of its two endpoints").
- Indistinguishability: a node the caller cannot see behaves as if absent in every API. `get_node` returns `Ok(None)`;
  traversal skips it silently and does not walk through it; `visited`, `returned`, `truncated` and `depth_reached` count
  visible nodes only; search never returns it and never counts it; a write that targets it returns `NotFound`
  (identical to a missing id).
- No id oracle: only `GLOBAL` nodes accept a caller-chosen id. Every other scope gets a store-minted UUIDv7 id (returned
  from `put_node`). Updating a scoped node needs that id and the node must be visible to the writer. `GLOBAL` writes
  by ingestion use deterministic ids.
- `PRIVATE` nodes take `owner_principal` from the writer; every node stores its writer in `owner_principal`.
- Lifecycle (ING-066): `EPHEMERAL` requires `expires_at <= now + 24 h` (default when absent: now + 1 h) and is never
  returned once expired, even before it is purged. `ContextMaintenance::tombstone_task_nodes(tenant, task_id)` closes
  the TASK nodes of a finished task. `ContextMaintenance::purge_expired(now_policy)` hard-deletes expired ephemerals and
  TASK nodes tombstoned longer ago than the retention (default 7 days, configurable) across all tenants and returns
  only counts (`PurgeReport`), no tenant data. Both are plain methods; the 60 s purge cadence is the gateway's job and
  is recorded as a hand-over, not tested here.

## 6. Traversal (ING-064, ING-067)

`traverse(&self, tenant, caller, &TraverseQuery) -> Result<TraverseResult>`.

- Query: `from: NodeId`, `edge_types: Option<Vec<EdgeType>>` (empty list is invalid), `direction: Outbound|Inbound|Both`,
  `max_depth: 1..=16`, `strategy: Bfs|Dfs|Shortest|Weighted`, `as_of: Option<Timestamp>`,
  `scopes: Option<Vec<Scope>>` (narrows only; an empty list is `Invalid`), `first: 1..=1000`,
  `visited_cap: Option<u32>` (default 100,000, never above 1,000,000), `after: Option<TraverseResume>`.
- Result: `nodes`, `edges`, `resume: Option<TraverseResume>`, `truncated: bool` (the visited cap was reached, not a page
  boundary), `stats { visited, returned, depth_reached }`, `warnings: Vec<TraverseWarning>`
  (`SkippedInvalidWeights { count }`, ING-067), `as_of`.
- A start node that does not exist or is invisible gives an empty result, not an error.
- Deterministic order, so cursors are stable: **BFS** by (depth, node id); **DFS** preorder, neighbours in ascending
  (neighbour id, edge id) order; **SHORTEST** the BFS shortest-path tree (every reached node appears once with exactly
  the one edge to its smallest-id predecessor at minimal depth); **WEIGHTED** cost-ordered, cost = sum of
  `props["weight"]` (absent = 1.0; negative, NaN or infinite weights are skipped and counted in the warning; ties by smaller
  node id), each node settled at its minimum cost over paths of at most `max_depth` edges.
- Edges returned: every traversed edge whose two endpoints are in the returned node set, ordered by (depth of the source
  node, edge id).
- Pagination is stateless: `TraverseResume` is the order key of the last returned node; the next call re-runs the
  traversal and skips keys up to it. The visited cap counts skipped nodes too. Signing and encoding of cursors is
  WP-M2-03, not here. `TraverseResume` is `Serialize + Deserialize`.
- BFS and SHORTEST use a recursive CTE bounded by depth and by a `LIMIT` on visited rows. DFS and WEIGHTED are driven
  from Rust with an explicit stack or heap (no recursion) over a prepared neighbour query. Cycles, self-loops and
  parallel edges must terminate and must not duplicate nodes.
- Test oracle: a small in-memory reference model (test-only) that implements the semantics above independently of SQL.
  A proptest compares SQLite against the model for random graphs of up to 40 nodes with random scopes, callers,
  `as_of` instants and all four strategies.

## 7. Search

`search_nodes(tenant, caller, query: &str, limit: 1..=100, as_of)` returns visible nodes ranked by FTS5 `bm25()` over
`label` and `summary`, ties by node id. The user string is never interpreted as FTS5 syntax: split into tokens, quote each
token, and treat an empty token list as an empty result. Hostile queries (`"`, `*`, `NEAR(`, `-`, `:`, `OR`) must not
raise errors or change semantics.

## 8. Storage, migrations, limits, backup

- Format version in `PRAGMA user_version` (constant `FORMAT_VERSION: u32`). A `Migration` trait with ordered steps applied
  in one transaction each; opening a database whose version is greater than `FORMAT_VERSION` fails with `FormatTooNew`
  without touching it. The migration test creates a version-0 and a previous-version database by raw SQL.
- Connection setup: `journal_mode=WAL`, `foreign_keys=ON`, `busy_timeout=5000`, `trusted_schema=OFF`, `cell_size_check=ON`,
  `synchronous` from `SqliteConfig.durability` (default `Full`). Set rusqlite limits (`limits` feature) for length, SQL length
  and variable count. One writer connection behind a mutex plus a small pool of reader connections; readers never get
  `Busy` while a writer is active (WAL), proven with a thread test.
- Quotas (ING-064): `SqliteConfig.max_nodes_per_tenant` (default 5,000,000) and `max_edges_per_tenant` (default 20,000,000),
  maintained by a `tenant_stats` table updated in the same transaction; exceeding gives `QuotaExceeded`. Tests use tiny values.
- Backup/restore helpers: `backup_to(path)` uses the online backup API and is safe while writers run; `restore_from(path, target)`
  refuses an existing target, copies, runs `PRAGMA integrity_check` and the format-version check, and returns `Corrupt` on a
  damaged file. Round-trip test compares all rows.

## 9. Other traits (declarations only)

`TaskStore` (RUN-080/081), `AuditLog` (SEC-090…092) and `IdempotencyStore` (RUN-072) are declared, object-safe and documented
with their spec ids in `src/tasks`, `src/audit`, `src/idempotency`; every method takes `&TenantScope` (the audit log also an
`AuditChain { Tenant(TenantScope), System }`). Keep them minimal and mark them provisional: the implementing work packages
(M6-02, M6-04, M6-11) may extend them. No implementation, no tests beyond a compile-time object-safety check.

## 10. Reusable conformance suite

`pub mod conformance` (compiled under `cfg(any(test, feature = "conformance"))`, feature `conformance` non-default) exposes
`run_context_store_suite(factory) -> SuiteReport` where the factory builds a store from a shared `ManualClock`.
The suite uses `Result`, not panics (library code); a test asserts `report.failed.is_empty()`. It covers sections 3 to 7 through the
trait only, so a future backend (SurrealDB, Postgres) runs the same checks. The SQLite backend runs it in an integration test.

## 11. Out of scope (hand-over)

Signed cursors, `gap.traverse` / `gap.node` resolvers and `Predicate` evaluation (WP-M2-03); code ingestion writing nodes
(WP-M2-02); `TaskStore`/`AuditLog`/`IdempotencyStore` implementations (M6); SurrealDB (WP-M7-11); vector index (WP-M2-04);
the purge scheduler and the lifecycle hook that calls `tombstone_task_nodes` (gateway / task engine).
