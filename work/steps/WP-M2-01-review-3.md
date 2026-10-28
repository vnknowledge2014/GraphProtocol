# WP-M2-01 review findings, round 3 (reviewer, 2026-10-28): BFS and SHORTEST do not scale

Reviewed commit `ed54b0f`. Follow-up 2 is verified: I re-ran the 31 real-gap mutants against it and 30 are killed by the tests you
named. The remaining one, `[73]`, is an equivalent mutant (in `run_weighted` the post-cap probe expansion cannot change the result:
`truncated` is already true once `settled_nodes.len() > cap`). That closes the test-gap work. This round is a different kind of
finding: a performance defect that no functional test or oracle can see because they all use graphs of a few dozen nodes.

## R-5 [High, implementation] BFS and SHORTEST (the recursive CTE) cost O(nodes) per level on a fresh database

Measured with a release build on random directed graphs, edges unique, `Durability::Off`, depth 4, `first` 100:

| graph | strategy | `visited_cap` | time | visited |
|---|---|---|---|---|
| 2,000 nodes, 10,000 edges | BFS | 10 | 0.23 s | 10 |
| 2,000 nodes, 10,000 edges | BFS | 100,000 | 1.58 s | 600 |
| 10,000 nodes, 50,000 edges | BFS | 10 | 3.88 s | 10 |
| 10,000 nodes, 50,000 edges | BFS | 100,000 | 18.7 s | 772 |
| 10,000 nodes, 50,000 edges | SHORTEST | 100,000 | 18.8 s | 772 |
| 10,000 nodes, 50,000 edges | BFS INBOUND | 10 | 2.2 s | 10 |
| 10,000 nodes, 50,000 edges | BFS BOTH, depth 3 | 10 | did not finish in 10 minutes | |
| any of the above | DFS and WEIGHTED | 10 and 100,000 | 0.00 to 0.1 s | |

The cost grows faster than the edge count and is unrelated to how many nodes are visited, so at the ceilings of ING-064
(5 M nodes, 20 M edges) BFS is unusable. DFS and WEIGHTED, which are driven from Rust with the indexed neighbour query, are fast.

Cause, reproduced in plain SQLite with your exact `CREATE TABLE` / `CREATE INDEX` text from `schema.rs` and the CTE text of
`bfs_cte_sql!` (OUTBOUND), 10,000 nodes and 50,000 edges:
- after `ANALYZE`: plan `SEARCH e USING INDEX idx_edges_from (tenant=? AND from_id=? AND valid_from<?)`, 0.00 s;
- on a database without statistics (which is what the store creates and never analyses): plan
  `SEARCH e USING INDEX idx_edges_to (tenant=? AND to_id=? AND valid_from<?)`, 8.65 s. The planner picks the join order
  `nv` (scan of `nodes`) then `e` by `to_id`, so every recursive step walks the node table.
The BOTH variant has the `(e.from_id = bfs.node_id OR e.to_id = bfs.node_id)` join and the `CASE` target, which is worse.

The new rule is ING-088 in `spec/05-ingestion.md` (I commit it myself before you start; do not touch `spec/`): no statement of
the hot-path operations may scan `nodes` or `edges` of a tenant, and plans must not depend on `ANALYZE`.

### Required fix

Make the plan of BFS and SHORTEST deterministic and index-driven for OUTBOUND, INBOUND and BOTH. Either:
1. keep the recursive CTE but fix the join order and the indexes explicitly: `FROM bfs CROSS JOIN edges e INDEXED BY idx_edges_from ... CROSS JOIN
   nodes nv INDEXED BY idx_nodes_temporal ...` (`idx_edges_to` for INBOUND), and for BOTH use two recursive terms (SQLite accepts several
   recursive SELECTs joined by `UNION`) instead of the `OR` join; or
2. replace the CTE by a level-by-level walk driven from Rust with the same indexed neighbour query that DFS and WEIGHTED already use
   (a frontier per depth, deterministic order (depth, id), the visited cap and the resume key unchanged). If you choose this, say so under
   Deviations, because the WP text says "recursive-CTE traversal"; the reason is ING-088.
Whichever you pick, results MUST NOT change: all existing traversal tests, the reference-model proptest and the conformance suite must
stay green without edits to expected values. Do not add `ANALYZE` or `PRAGMA optimize` as the fix (the plan must not depend on statistics),
though running `PRAGMA optimize` on close is allowed as an extra.

### Required tests (deterministic, no timing)

1. **Plan tests.** On a database that has never been analysed (check that `sqlite_stat1` does not exist), build 3,000 nodes and 15,000 unique
   edges through `put_batch`, then for BFS and SHORTEST in OUTBOUND, INBOUND and BOTH, and for the DFS/WEIGHTED neighbour query, obtain the
   `EXPLAIN QUERY PLAN` of the statement the store really runs (expose the SQL text through a `pub(crate)` constant or accessor, do not copy it into the
   test) and assert: every access to `edges` uses `idx_edges_from` or `idx_edges_to`, every access to `nodes` uses an index, and no row has `SCAN nodes`
   or `SCAN edges`.
2. **Work counters.** Run each traversal (all four strategies, all three directions, `visited_cap` 10) on that graph and assert, with rusqlite's
   `Statement::get_status(StatementStatus::FullscanStep)` (or the equivalent counter), that the number of full-scan steps is 0 and that the number of
   rows read is below a literal bound proportional to the visited nodes times their degree (state the bound in a comment).
3. **Same for the other hot paths.** Plan assertions (no `SCAN` of `nodes` or `edges`, no dependence on statistics) for `get_node`, `get_edge`,
   `put_batch` lookups, `search_nodes` (the join from the tenant FTS table to `nodes`), `tombstone_node`, `delete_node`, `tombstone_task_nodes` and
   `purge_expired`. `counts` is exempt (it is proportional to the tenant by definition: document it in the test). If a statement does scan, fix it with an
   index (a new migration step, version bump and migration test) or report it as a finding with the plan; do not weaken the assertion.
4. Tag the tests `// spec: ING-088` (plus INV-09 where tenant partitioning matters).

### Reviewer re-check

After you finish I will rebuild my benchmark (10,000 nodes / 50,000 edges and 20,000 / 100,000; every strategy and direction) and expect BFS and SHORTEST in the
same range as DFS (milliseconds), re-run the differential oracle (7,000 random programs must still agree), the nextest gates, and mutation tests of the
plan assertions (for example removing `INDEXED BY` or the join-order hint must fail a test).

## Delivery

Same branch `wp/WP-M2-01`. One Conventional Commit for the fix (`fix(gap-store): ...`, mention ING-088) and one for the tests; commit this file
in the first commit (the spec change is already committed by me). Re-run all gates (`cargo test -p gap-store` three times, `--features conformance`, workspace clippy, doc, fmt, the
tenant proptest binary 100 times, layers, validate, refs, trace, unittest) and append `## Follow-up 3` to `work/handoffs/WP-M2-01.md` with the new
timings you measure (state your method), the plans before and after, and the deviation if you pick option 2. Same restrictions: edit only
`crates/gap-store/**`, `Cargo.lock` (cargo only), `work/steps/`, `work/handoffs/`, `work/status.toml`; do not merge; do not mark the WP done. Finish with exactly
one line: `WP-M2-01 follow-up 3: READY`, `WP-M2-01 follow-up 3: READY-WITH-NOT-RUN` or `WP-M2-01 follow-up 3: BLOCKED - <reason>`, then a short summary in Vietnamese.
