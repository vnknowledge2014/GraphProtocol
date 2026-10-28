# WP-M2-01 review findings, round 2 (reviewer, 2026-10-28): test gaps found by mutation testing

Reviewed commit `30022e9` (code unchanged since your `f89ad6d`). Gates: all green with the real tools (nextest workspace 235/235,
`cargo deny` bans/licenses/sources ok, fmt, clippy, doc, layers, validate, refs, trace, tools unit tests). The independent oracle finds
no mismatch in 7,000 random programs. Mutation testing (142 one-line mutants of `gap-store`): 99 killed, 43 survived. Of the survivors,
13 are equivalent (defence in depth that is redundant on purpose, listed at the end). The other 31 are real gaps: the code is right, but
no test fails when the behaviour is broken. Several are in tenant isolation (INV-09, INV-19), so they matter most.

## What to do

Add tests only. Do not change production code unless a new test shows a real bug: then fix it in its own commit and say so in the handoff.
For each mutant below, apply the replacement to the named file in your working tree, run your new test, confirm that it FAILS, then
restore the file with `git checkout -- <that explicit path>` before continuing; never commit a mutated file. List in the handoff, per
mutant number, the name of the test that kills it. A test that passes under the mutation does not count.

Rules for the tests: deterministic (no sleeps, no wall clock, ManualClock only), literal expected values, tagged `// spec: <ID>` with ids
from `spec/05-ingestion.md` (ING-080..087 were just added there; use them, plus INV-09 and INV-19), each under 10 seconds in debug.

### T-1 same ids in two tenants (cross-tenant writes, reads, maintenance, traversal)
- `[10]` tenant: edge as-of lookup unscoped. File `src/sqlite/crud.rs`, occurrence 2. Replace `WHERE tenant = ?1
      AND id = ?2
      AND valid_from <= ?3` with `WHERE ?1 = ?1
      AND id = ?2
      AND valid_from <= ?3`.
- `[12]` tenant: hard delete of incident edges unscoped. File `src/sqlite/crud.rs`. Replace `WHERE tenant = ?1 AND (from_id = ?2 OR to_id = ?2)` with `WHERE ?1 = ?1 AND (from_id = ?2 OR to_id = ?2)`.
- `[90]` tenant: tombstone_task_nodes unscoped. File `src/sqlite/maintenance.rs`. Replace `WHERE tenant = ?1
      AND base_scope = 'TASK'` with `WHERE ?1 = ?1
      AND base_scope = 'TASK'`.
- `[94]` purge: version guard ignores tenant. File `src/sqlite/maintenance.rs`. Replace `WHERE other.tenant = n.tenant
            AND other.id = n.id` with `WHERE other.id = n.id`.
- `[96]` tenant: purge incident edges unscoped. File `src/sqlite/maintenance.rs`. Replace `WHERE tenant = ?1
      AND (from_id = ?2 OR to_id = ?2)` with `WHERE ?1 = ?1
      AND (from_id = ?2 OR to_id = ?2)`.
- `[97]` purge: edges deleted although versions remain. File `src/sqlite/maintenance.rs`. Replace `if !still_exists {` with `if true {`.
- `[57]` tenant: bfs neighbour nodes unscoped. File `src/sqlite/traverse.rs`, occurrence 1. Replace `nv.tenant = ?1` with `?1 = ?1`.
- `[58]` tenant: step neighbour nodes unscoped. File `src/sqlite/traverse.rs`, occurrence 2. Replace `nv.tenant = ?1` with `?1 = ?1`.
- `[59]` tenant: bfs edges unscoped. File `src/sqlite/traverse.rs`. Replace `ON e.tenant = ?1` with `ON ?1 = ?1`.
- `[60]` tenant: step edges unscoped. File `src/sqlite/traverse.rs`. Replace `WHERE e.tenant = ?1` with `WHERE ?1 = ?1`.

### T-2 content hash and re-put semantics
- `[24]` hash: base scope not hashed. File `src/sqlite/crud.rs`. Replace `hash_str(&mut hasher, resolved.base_scope.as_str());` with `(nothing)`.
- `[25]` hash: group not hashed. File `src/sqlite/crud.rs`. Replace `hash_opt_str(&mut hasher, resolved.group.as_deref());` with `(nothing)`.
- `[26]` hash: expiry not hashed. File `src/sqlite/crud.rs`. Replace `hash_opt_i64(&mut hasher, resolved.expires_at_us);` with `(nothing)`.
- `[28]` hash: node props not hashed. File `src/sqlite/crud.rs`, occurrence 1. Replace `hash_str(&mut hasher, props_canonical_json);` with `(nothing)`.
- `[29]` hash: edge props not hashed. File `src/sqlite/crud.rs`, occurrence 2. Replace `hash_str(&mut hasher, props_canonical_json);` with `(nothing)`.
- `[31]` hash: edge resolution not hashed. File `src/sqlite/crud.rs`. Replace `hash_str(&mut hasher, edge.resolution.as_str());` with `(nothing)`.
- `[23]` scoped update: owner change not versioned. File `src/sqlite/crud.rs`. Replace `if existing.content_hash.as_slice() != hash.as_slice() || owner_changed {` with `if existing.content_hash.as_slice() != hash.as_slice() {`.

### T-3 write rules and quotas
- `[32]` write: global put over invisible node allowed. File `src/sqlite/crud.rs`. Replace `if !is_meta_visible(&existing, writer, now_us) {
                                return Err(StoreError::Conflict);
                            }` with `(nothing)`.
- `[37]` quota: minted node off by one. File `src/sqlite/crud.rs`, occurrence 1. Replace `if active_nodes >= self.config.max_nodes_per_tenant {` with `if active_nodes > self.config.max_nodes_per_tenant {`.

### T-4 temporal and traversal edge cases
- `[5]` vis: ephemeral ignores as_of. File `src/sqlite/crud.rs`. Replace `if exp_us <= now_us || exp_us <= as_of_us {` with `if exp_us <= now_us {`.
- `[66]` temporal: bfs edge boundary. File `src/sqlite/traverse.rs`, occurrence 1. Replace `AND (e.valid_to IS NULL OR ?3 < e.valid_to)` with `AND (e.valid_to IS NULL OR ?3 <= e.valid_to)`.
- `[67]` temporal: step edge boundary. File `src/sqlite/traverse.rs`, occurrence 2. Replace `AND (e.valid_to IS NULL OR ?3 < e.valid_to)` with `AND (e.valid_to IS NULL OR ?3 <= e.valid_to)`.
- `[73]` weighted: depth bound off by one (settle). File `src/sqlite/traverse.rs`, occurrence 1. Replace `if u32::from(hops) < env.query.max_depth {` with `if u32::from(hops) <= env.query.max_depth {`.
- `[77]` bfs: truncated expansion rule. File `src/sqlite/traverse.rs`. Replace `(!truncated || *u_depth < cutoff_depth)` with `true`.
- `[87]` search: ephemeral ignores now. File `src/sqlite/search.rs`. Replace `OR (n.expires_at IS NOT NULL AND n.expires_at > ?4 AND n.expires_at > ?3)` with `OR (n.expires_at IS NOT NULL AND n.expires_at > ?3)`.

### T-5 FTS shards, migration and backup integrity
- `[119]` restore: integrity check ignored. File `src/sqlite/backup.rs`. Replace `if integrity_rows.len() != 1 || !integrity_rows[0].eq_ignore_ascii_case("ok") {` with `if false {`.
- `[136]` migration: every tenant table receives all rows. File `src/sqlite/schema.rs`. Replace `WHERE tenant = ?1
                 ORDER BY fts_rowid ASC` with `WHERE ?1 = ?1
                 ORDER BY fts_rowid ASC`.
- `[137]` verify: leftover shared FTS table accepted. File `src/sqlite/schema.rs`. Replace `if fts_table_exists(conn, "nodes_fts")? {` with `if false {`.
- `[141]` delete_node: FTS rows left behind. File `src/sqlite/crud.rs`. Replace `del_fts.execute(params![rowid]).map_err(map_sqlite_err)?;` with `(nothing)`.

### T-6 connection configuration
- `[124]` durability: FULL is NORMAL. File `src/sqlite/mod.rs`. Replace `Self::Full => "PRAGMA synchronous = FULL;",` with `Self::Full => "PRAGMA synchronous = NORMAL;",`.
- `[126]` pragma: reader cell_size_check off. File `src/sqlite/mod.rs`. Replace `PRAGMA cell_size_check = ON;
         PRAGMA foreign_keys = ON;
         PRAGMA query_only = ON;` with `PRAGMA foreign_keys = ON;
         PRAGMA query_only = ON;`.

## Required scenarios per group

**T-1.** Build two tenants `ta` and `tb` that reuse the same strings: GLOBAL node ids `n1`, `n2`, `n3`, the same edge id `e1`, and the same
`TaskId` on TASK nodes. Then, for each public method that touches edges, nodes or maintenance, show that an operation in `ta` leaves `tb` byte
for byte unchanged (read `tb` through the API before and after, and raw SQL row counts of `nodes`, `edges` and the tenant FTS table):
`get_edge` for an id that exists only in `tb` returns `None` for `ta` even when `ta` has nodes with the same endpoint ids; `delete_node` of
`n1` in `ta` keeps `tb`'s `n1`, its edges and its search hits; `tombstone_task_nodes(ta, T)` leaves `tb`'s TASK nodes with the same `T` active;
`purge_expired` purges an expired ephemeral node and a TASK node tombstoned beyond retention in `ta` while `tb` has live nodes and edges with the same
ids (they stay), and a `ta` TASK node is still purged when `tb` has an active node with the same id; the purge keeps an edge whose endpoint id still has
another version. Traversal: `tb` has edge `n1 -> n2`, `ta` has nodes `n1` and `n2` but no edge: BFS, DFS, SHORTEST and WEIGHTED from `n1` in `ta`, all
three directions, return only `n1` and no edge. Also `ta` has `n2` visible only to another principal while `tb`'s `n2` is GLOBAL and `ta` has edge
`n1 -> n2`: the caller of `ta` must not see `n2` (node, edge, stats), for all four strategies.

**T-2.** For every field that is part of the content hash, a re-put that changes only that field MUST create a new version and take effect:
node label, summary, props, scope kind, group (a member of the old group loses access, a member of the new group gains it), ephemeral base scope
(for example PRIVATE to GLOBAL), ephemeral expiry, TASK id; edge props, resolution and confidence. Check with `get_node` / `get_edge` at `as_of` before
and after the change (two versions). A re-put of identical content still creates no version (already covered). A group member who can see a GROUP node
and updates it becomes its `owner_principal`: assert the owner and that a version was created even if only the writer changed.

**T-3.** A GLOBAL put with a caller-chosen id that collides with a node the writer cannot see fails with `Conflict` and writes nothing from the batch
(ING-081). Quota: with `max_nodes_per_tenant = 3`, the third minted node succeeds and the fourth fails with `QuotaExceeded` (also when the nodes are minted,
not only GLOBAL with id); same exactness for edges already covered, keep it.

**T-4.** Edge `valid_to` boundary in `traverse` for BFS and for the step queries of DFS and WEIGHTED: at `as_of` equal to the edge's `valid_to` the edge is
not traversed, one microsecond earlier it is. WEIGHTED never returns a node that needs more than `max_depth` edges (a chain longer than `max_depth`
with `max_depth = 2`, literal expected nodes). BFS cut by `visited_cap` follows ING-084: nodes at the cut depth are not expanded, so an edge between two
returned nodes at the cut depth is absent while the same graph without the cap returns it (literal expected edges). `search_nodes` and `get_node` with
`as_of` earlier than now, for an ephemeral node that expired after `as_of` but before now: both agree it is not returned (ING-066 "never returned once
expired"), and an ephemeral node that has not expired at now but expires before a future `as_of` is not returned at that `as_of`.

**T-5.** After `delete_node`, the tenant's FTS table has no row for any version of the node (raw SQL row count equals the remaining versions of the
tenant), and the text can no longer be found even by raw `MATCH`. Migration v2 to v3 with two tenants: each shard table holds exactly its own tenant's
rows (raw SQL counts per shard equal the number of node versions of that tenant), no cross rows. `verify_required_tables` / restore reject a v3
database that still has the shared `nodes_fts` table (`Corrupt`). `restore_from` rejects a backup whose header and page 1 are intact but where
`PRAGMA integrity_check` fails (corrupt one byte inside a data page of a populated table, not the header) with `Corrupt`, and does not create the target.

**T-6.** A store opened with the default `SqliteConfig` has `PRAGMA synchronous = 2` (FULL) on the writer; every pooled reader connection has
`cell_size_check = 1`, `trusted_schema = 0` and `query_only = 1`.

## Equivalent mutants (no test needed, listed so that you do not chase them)
- `[55]` bfs: start node PRIVATE not checked. File `src/sqlite/traverse.rs`. Replace `OR (n0.base_scope = 'PRIVATE' AND n0.owner_principal = ?5)` with `OR (n0.base_scope = 'PRIVATE')`.
- `[56]` tenant: bfs start unscoped. File `src/sqlite/traverse.rs`. Replace `n0.tenant = ?1` with `?1 = ?1`.
- `[63]` scopes: bfs start filter ignored. File `src/sqlite/traverse.rs`. Replace `(?8 = 0 OR n0.scope IN (SELECT val FROM allowed_scopes))` with `(1 = 1)`.
- `[71]` dfs: re-expansion bound. File `src/sqlite/traverse.rs`. Replace `if d >= prev_best {` with `if d > prev_best {`.
- `[80]` weighted: dominance pruning removed. File `src/sqlite/traverse.rs`. Replace `(cost > u_table[h] || (0..h).any(|hp| u_table[hp] <= cost))` with `(cost > u_table[h])`.
- `[81]` weighted: relaxation ignores fewer-hop states (settle). File `src/sqlite/traverse.rs`, occurrence 1. Replace `if (0..=nh).all(|hp| next_cost < v_table[hp]) {` with `if next_cost < v_table[nh] {`.
- `[82]` tenant: search n.tenant filter removed (expected equivalent: per-tenant table). File `src/sqlite/search.rs`. Replace `AND n.tenant = ?2` with `(nothing)`.
- `[118]` restore: version 0 accepted. File `src/sqlite/backup.rs`. Replace `if version_u32 == 0 {` with `if false {`.
- `[120]` restore: existing target not refused. File `src/sqlite/backup.rs`. Replace `if path_or_wal_exists(target_path) {` with `if false {`.
- `[122]` migrate: skipped step accepted. File `src/sqlite/schema.rs`. Replace `if version + 1 != target {` with `if false {`.
- `[127]` open: WAL failure ignored. File `src/sqlite/mod.rs`. Replace `if !mode.eq_ignore_ascii_case("wal") {` with `if false {`.
- `[129]` weighted: relaxation ignores fewer-hop states (probe). File `src/sqlite/traverse.rs`, occurrence 2. Replace `if (0..=nh).all(|hp| next_cost < v_table[hp]) {` with `if next_cost < v_table[nh] {`.
- `[30]` hash: edge target not hashed. File `src/sqlite/crud.rs`. Replace `hash_str(&mut hasher, edge.to.as_str());` with `(nothing)`.

## Delivery

Same branch `wp/WP-M2-01`, one Conventional Commit per group T-1 to T-6 (message `test(gap-store): ...`; a commit that also fixes a bug is `fix(gap-store): ...`).
Commit this file in the first commit. Re-run all gates and report counts: `cargo test -p gap-store` three times, `--features conformance`, clippy
(workspace), doc, fmt, the tenant proptest binary 100 times, and the layers, validate, refs, trace and unittest tools. Append `## Follow-up 2` to
`work/handoffs/WP-M2-01.md` with the mutant-to-test table. Same restrictions as before: edit only `crates/gap-store/**`, `Cargo.lock` (cargo only),
`work/steps/`, `work/handoffs/`, `work/status.toml`; do not merge; do not mark the WP done. Finish with exactly one line:
`WP-M2-01 follow-up 2: READY`, `WP-M2-01 follow-up 2: READY-WITH-NOT-RUN` or `WP-M2-01 follow-up 2: BLOCKED - <reason>`, then a short summary in Vietnamese.
