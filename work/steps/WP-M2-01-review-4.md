# WP-M2-01 review findings, round 4 (reviewer, 2026-10-28): tests for the new ING-088 logic

Reviewed commit `36db473`. R-5 is verified:
- My benchmark (release, never-analysed DB, depth 4, `first` 100) on 20,000 nodes / 100,000 edges: BFS, SHORTEST, DFS and WEIGHTED take
  0.02 to 0.25 s in every direction and for `visited_cap` 10 and 100,000 (up to 10,362 visited nodes for BOTH). On 10,000 / 50,000 BFS
  (772 visited) is now below the noise of my harness (before: 18.7 s). The visited counts are identical to the pre-fix measurements.
- Differential oracle: 7,000 random programs against the new code, 0 mismatches.
- Gates (real tools): fmt, clippy, build, doc, nextest workspace 256/256, `cargo deny` bans/licenses/sources, layers, validate, refs,
  trace (43 MUST without test, ING-088 now has tests), unittest: all green.
- Plan assertions are strong: 21 of 22 plan mutants I made (wrong index, `CROSS JOIN` -> `JOIN`, dropped `INDEXED BY` in traversal,
  neighbours, start node, point lookups, close/delete of incident edges, expired-task selection, tenant enumeration, purge index
  columns) are killed by `plan_tests`.

What remains is a mutation round on the code that follow-up 3 added (`m7.py`, 31 mutants + control). 25 killed, 4 equivalent, **6 real test gaps**.
This round asks for tests only.

## Equivalent mutants (no action)

- `NEIGHBOURS_BOTH_SQL` without `AND e.from_id != ?2` (self-loop returned twice): every consumer keys edges by `EdgeId` or checks `visited`.
- `CTE_BOTH_SQL` without `AND e.from_id != bfs.node_id`: the recursive `UNION` removes the duplicate row.
- `ORDER BY 15 ASC, 1 ASC` -> `ORDER BY 1 ASC, 15 ASC` in `NEIGHBOURS_OUTBOUND_SQL` / `NEIGHBOURS_BOTH_SQL`: `query_eligible_neighbours`
  re-sorts by `(neighbour_id, edge id)` in Rust, so the SQL `ORDER BY` is redundant. Leave the production code as it is.

## T-7 [High, test gap] `purge_expired` maintains `tenant_stats` by deltas and no test checks the numbers

Follow-up 3 replaced the tenant-wide recount after a purge by deltas (`purged_active_nodes`, `purged_active_edges`). That is the right design
(a recount is O(tenant), forbidden by ING-088) but the stats drive the quotas, so a drift is a correctness bug. Four mutants survive:

| mutant | what it breaks |
|---|---|
| `COUNT_ACTIVE_INCIDENT_EDGES_SQL` without `AND from_id != ?2` in the second subquery | a self-loop on a purged node is counted twice |
| `if is_active {` -> `if true {` in the ephemeral loop | a tombstoned (closed) expired ephemeral version is subtracted again |
| `cur_edges.saturating_sub(purged_active_edges)` -> `cur_edges` | edge count never decreases |
| `.saturating_add(u64::try_from(active_incident.max(0))...)` -> `.saturating_add(0)` | active incident edges are ignored |

Add deterministic tests (unit test in `sqlite::tests` or `plan_tests`, or an integration test; use `read_tenant_stats` and compare it
with a raw recount `SELECT COUNT(*) FROM nodes WHERE tenant=? AND valid_to IS NULL` / `... FROM edges ...` on a second connection or
the writer; the test may scan, it is not a hot path). After every `purge_expired` call, for **every** tenant, assert
`tenant_stats == raw recount`. Scenarios (each must fail on at least the mutant named):
1. EPHEMERAL node X (expires at T) with one active outbound and one active inbound edge, plus a surviving GLOBAL node Y; clock past T;
   purge. Node count drops by 1, edge count by 2. (kills `cur_edges`, `active_incident -> 0`)
2. Same with a self-loop X -> X in addition. Edge count drops by 3, not 4. (kills the self-loop double count)
3. EPHEMERAL node tombstoned with `tombstone_node` before it expires (stats already decremented), then expired and purged. The node
   count must not drop a second time and a different live node must still be counted. (kills `if true`)
4. Edge A -> B where both A and B are EPHEMERAL and expire in the same pass: the edge is subtracted once.
5. A TASK node purged after `tombstone_task_nodes` (closed versions only): stats unchanged by the purge, equal to the recount.
6. A second tenant with data is untouched and still equal to its recount.
Tag with `// spec: ING-088` and `// spec: INV-09`.

## T-8 [Medium, test gap] `search_nodes` plan is not locked against statistics

Mutant `CROSS JOIN nodes n` -> `JOIN nodes n` in `build_search_nodes_fts_sql` survives: without statistics the planner happens to choose the
same plan, so the plain test cannot see it, and `ing_088_plans_remain_locked_under_adversarial_statistics_and_decoy_indexes` does not
include the search statement. Add the search statement (built with `build_search_nodes_fts_sql` for a real per-tenant table) to the
adversarial test: with each of its `sqlite_stat1` distributions (including one where `nodes` looks tiny and one where it looks huge) the
plan must drive from the FTS table and probe `nodes` by `INTEGER PRIMARY KEY (rowid=?)`, with no `SCAN n`. The test must fail when
`CROSS JOIN` is replaced by `JOIN`. If SQLite really cannot be made to pick another plan for that statement under any statistics you can inject, say so
in the handoff with the evidence (the plans you got), and keep the assertion anyway.

## T-9 [Low, test gap] v4 index requirement in `verify_required_tables`

Mutant `if version >= 4 {` -> `if false {` in `verify_required_tables` survives. Add a test: a database with `user_version = 4` whose
`idx_nodes_task_purge` was dropped (`DROP INDEX`) is rejected by `verify_required_tables` and by `restore_from` with `StoreError::Corrupt`,
the restore target stays untouched, and the same database at `user_version = 3` is still accepted (the existing v3 backup test).

## Delivery

Same branch `wp/WP-M2-01`. Tests only: no production code changes. If a new test exposes a real defect, stop, keep the failing
test, report it in the handoff with the evidence and finish with `BLOCKED`; do not fix it silently. Commit this file with the first commit
(`test(gap-store): ...`, one commit is enough). Re-run the same gates as in follow-up 3 (`cargo test -p gap-store` three times,
`--features conformance`, workspace clippy, doc, fmt, the tenant proptest binary 100 times, layers, validate, refs, trace, unittest) and
append `## Follow-up 4` to `work/handoffs/WP-M2-01.md` (list each new test with the mutant it kills, and your own mutation check of the
new tests as you did in follow-up 3). Same restrictions: edit only `crates/gap-store/**`, `Cargo.lock` (cargo only), `work/steps/`,
`work/handoffs/`, `work/status.toml`; do not touch `spec/`; do not merge; do not mark the WP done. Finish with exactly one line:
`WP-M2-01 follow-up 4: READY`, `WP-M2-01 follow-up 4: READY-WITH-NOT-RUN` or `WP-M2-01 follow-up 4: BLOCKED - <reason>`, then a short
summary in Vietnamese.
