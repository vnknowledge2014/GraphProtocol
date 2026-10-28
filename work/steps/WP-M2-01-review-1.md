# WP-M2-01 review findings, round 1 (reviewer, 2026-10-28)

Reviewed commit `0f85f09`. What I ran: every gate offline (fmt, clippy, build, doc, layers, validate, refs, trace, tools
unittests: green; `cargo nextest` for the workspace: 223/224), an independent Python oracle written from
`work/context/m2-01-design.md` (not from your code) against about 7,000 random programs (two tenants, all five scopes,
ephemeral expiry, as-of queries, edge types, scope filters, caps, paging, put/tombstone/purge/tombstone_task, search),
and a reading of `crud.rs`, `traverse.rs`, `search.rs`, `maintenance.rs`, `backup.rs`, `mod.rs`. Tenant isolation, scope
visibility, indistinguishability, temporal versions, counts, BFS/SHORTEST, purge and task tombstoning all agreed with the
oracle. Four findings need work. A mutation run (about 128 mutants) is still in progress; its survivors will come as
round 2.

## R-1 [High, test] Flaky proptest `inv_09_three_tenant_interleaved_sequences_match_isolated_models`

It fails about 20% of runs (5 of 25 with `target/debug/deps/tenant_proptest-* -q`; nextest failed it in my workspace run):
`assertion failed: res.is_err()` at `crates/gap-store/tests/tenant_proptest.rs:507`. Minimal input found by proptest:
`PutGlobal{slot 4}`, `TombstoneGlobal{slot 4}`, `DeleteGlobal{slot 4}` for the same tenant and caller.

Cause: the model forgets a node on tombstone and therefore expects `delete_node` to fail, but the store is right: per
design section 4 `delete_node` is a hard delete that also purges the history of a tombstoned node, so it returns `Ok`.
Your three `cargo test` runs simply did not draw that sequence.

Required:
1. Fix the model so tombstoned history stays deletable (`delete_node` after `tombstone_node` is `Ok` for a caller that could see the
   node, a second delete is `NotFound`, a delete of a never-written id is `NotFound`).
2. Add deterministic unit tests for exactly these cases, including `get_node(as_of = before the tombstone)` returning the
   node before the delete and `None` after it.
3. Run the proptest binary at least 100 times in a row and report the count of failures (must be 0). A test that passes 3 runs
   out of 3 is not evidence for a randomized test.

## R-2 [High, implementation] WEIGHTED order breaks ING-067 for zero-weight edges; paging loses nodes or loops forever

ING-067: "equal-cost ties are broken by the smaller node id, so cursors stay deterministic". `run_weighted` records nodes in
Dijkstra pop order, which is not sorted by `(cost, node_id)` when an edge has weight `0` (valid: weight >= 0). The resume
token is then applied with `partition_point` on that unsorted sequence.

Repro 1 (3 nodes): edges `a -> z` weight 1 and `z -> m` weight 0, start `a`, direction OUTBOUND, `max_depth` 2, WEIGHTED.
Expected `[a, m, z]` (m and z both cost 1, `m < z`); actual `[a, z, m]`. With `first = 2`, page 1 is `[a, z]` with resume
`(cost 1.0, "z")` and page 2 is EMPTY: `m` is never returned.

Repro 2 (5 nodes, direction BOTH, `first = 2`): ids `n4` (start), `n11`, `n1`, `n0`, `s1`; edges
`n11 -> n4` w 0.5, `n4 -> n11` (no weight), `n1 -> n4` w 1, `n11 -> n1` w 0.5, `n0 -> n1` w 0, `s1 -> n1` (no weight),
`n11 -> n0` w 3. Unpaged result is `[n4, n11, n1, n0, s1]` (expected `[n4, n11, n0, n1, s1]`), and following `resume` returns
`[n1, n0]` with resume `(1.0, "n0")` on every page, forever. A client or the WP-M2-03 resolver that follows `resume` never
terminates.

Also at the visited cap the node that is included depends on discovery order instead of `(cost, id)` (I saw `{n20, n0, s3}`
where the rule gives `{n20, n0, s2}`), and `stats.depth_reached` can differ.

Fix: do not cut at the cap while nodes of the same cost as the last settled node can still be produced (zero-weight edges can
add more nodes at that cost, possibly with smaller ids). Finish the whole equal-cost group, sort the settled nodes by
`(cost_bits, node_id)`, then take the first `visited_cap`; `truncated` is true iff more reachable nodes remain after that
prefix. A node's depth is the smallest number of hops among its minimum-cost paths. Resume stays `(cost_bits, node_id)` and is
then sound.

Required tests: both repros with literal expected orders; paging with `first = 1` and `first = 2` concatenates to the unpaged
order with no duplicate and no repeated page (assert the page count); a zero-weight cycle; truncation where an equal-cost group
straddles the cap; a proptest over random graphs with weights from `{0, 0.5, 1, 2}` asserting (a) order sorted by
`(cost, id)`, (b) paging concatenation equals the unpaged result.

## R-3 [Medium, implementation] `open` and `restore_from` read the whole database file into memory

`probe_existing_file_readonly` (`sqlite/mod.rs`, `let header = fs::read(path)`) and `verify_sqlite_header` (`sqlite/backup.rs`,
`let bytes = fs::read(path)`) load the entire file only to compare the first 16 bytes. The default ceiling is 5 M nodes and
20 M edges per tenant, so a real database is gigabytes: every start and every restore spikes memory by the database size.

Fix: open the file and read only the header bytes needed (for example `File::open` then `Read::take(N).read_to_end`), keeping the
same results (`Corrupt` for a short file or a wrong header, `NotFound` for a missing path, `Backend` for other I/O errors).
Test the three outcomes with literal files; add a comment on why the whole file must not be read.

## R-4 [Medium, design flaw in my section 7] `search_nodes` ranking leaks across tenants

`ORDER BY bm25(nodes_fts)` uses FTS5 statistics of the whole table (document frequencies and average document length), so
another tenant's data changes this tenant's ranking. Repro: tenant `ta` has `short` = label `alpha gamma` and `long` = label
`alpha alpha delta delta delta delta delta delta`; query `alpha` returns `[short, long]`. After tenant `tb` writes 40 nodes with
label `zzz` and a 150-token summary (each `w0 w1 ... w149`, under the 1,024-byte limit) the same query for `ta` returns
`[long, short]`. A tenant can therefore infer statistics of another tenant's corpus and perturb its results (INV-09, TM-09).
The set of matches is correct; only the order leaks.

Decision (reviewer): one FTS5 table per tenant.
- Table name `nodes_fts_<shard>`, `<shard>` = lowercase hex of the first 8 bytes of `blake3(tenant id)`. Only that hex may be
  interpolated into SQL; never the tenant string.
- Create it lazily inside the write transaction (`CREATE VIRTUAL TABLE IF NOT EXISTS`) before the first FTS insert of the tenant.
  Insert, delete, purge and search use the tenant's table. A tenant without a table gets an empty search result, not an error.
  The `tenant` column is no longer needed in the FTS table.
- Migration v2 to v3 (bump `FORMAT_VERSION`, keep the step list ordered, one transaction per step): for every tenant present in `nodes`
  create its table, copy its rows, then drop the old shared `nodes_fts`. `verify_required_tables` must be updated. Backup and
  restore must keep working (the shard tables are ordinary tables of the file).
- If you believe this is impossible with the allowed crates or breaks an invariant, stop and report BLOCKED with the reason
  instead of improvising.

Required tests: the repro above with literal expected orders (identical order for `ta` with and without `tb`'s data); a term that
exists only in `tb` returns nothing for `ta`; delete and purge remove only the tenant's own rows; migration from a raw-SQL v2
database keeps search results; two tenants whose hex shards differ; a tenant id such as `a-b_c` works (identifier safety).

## What to do

Work on the same branch `wp/WP-M2-01`, one Conventional Commit per finding, in this order: R-1, R-2, R-3, R-4. Also commit this
findings file in the first commit (it is under `work/steps/`, which you own). Re-run every gate and report the new counts, run
`cargo test -p gap-store --locked --offline` three times and the tenant proptest binary 100 times. Append a section
`## Follow-up 1` to `work/handoffs/WP-M2-01.md` (what changed, per finding the test names, the new gate output, any
deviation or UNSPECIFIED choice). Do not edit anything outside `crates/gap-store/**`, `Cargo.lock` (cargo only), `work/steps/`,
`work/handoffs/` and `work/status.toml`. Do not merge, do not mark the WP done. Finish with exactly one line:
`WP-M2-01 follow-up 1: READY`, `WP-M2-01 follow-up 1: READY-WITH-NOT-RUN` or `WP-M2-01 follow-up 1: BLOCKED - <reason>`,
followed by a short summary in Vietnamese.
