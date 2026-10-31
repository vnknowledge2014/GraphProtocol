# WP-M1-11 — reviewer findings, round 2

Reviewer: Claude Sonnet 5.5, 2026-10-31. Verified head `dfa9637` (your follow-up 1). R-1..R-4 read and accepted in design (`join_relative` segment-count guard, `DotSegmentPathParameter`, `unsafe_path_literal`, grouped native foreach with per-index
`INVALID_ARGUMENT`, `limit`-capped batch, `auth_parameter_collision`). T-19 found a real defect that my round 1 did not see; you stopped correctly. This file is the decision you asked for. One code change (R-5), three tests, then re-run everything.
I will re-run the gates, the `#[ignore]` tests, my 58-mutant campaign and my real-socket probe myself; do not skip any item on the assumption that I will not look.

## R-5 (high, NGC-021/022 / CMP-030) the pagination lookahead asks the upstream for more than its declared `limit.maximum`

Your analysis is right: `query` clamps `page.limit` to `config.max_page_size` (200) and then sends `limit = lim + 1` (201); `build_request` rejects that against the operation's own schema (`maximum: 200`), so any caller who asks for a page of the size the connector
itself advertises in `Describe.limits.max_page_size` gets `INVALID_ARGUMENT`. Decision: **Option A**, with the exactness of the lookahead rule kept (no trailing empty page, no cursor on an exact last page) and the upstream's declared bounds never violated.

Required:
1. Extraction records per paginated operation `limit_maximum: Option<u32>` (the `maximum` of the `limit` parameter schema) and `limit_minimum: Option<u32>` (its `minimum`, default 1). An operation whose `limit.maximum` is below 2 is **not paginated** (`has_offset_pagination = false`; a request with a page limit then gets the existing
   "does not support pagination" `INVALID_ARGUMENT`); say so in the extraction report (a `notes`/`skipped`-style entry named `pagination_unavailable_limit_maximum`, the operation itself stays exposed).
2. Effective page size `P = min(page.limit, config.max_page_size, limit_maximum - 1)` (the last term only when a maximum is declared). The upstream limit sent is `max(P + 1, limit_minimum)`, which is `<= limit_maximum` by construction. If the minimum forces more rows than `P + 1`, the connector truncates locally to `P`:
   the existing `rows.len() > P` rule already yields `next_cursor` at `offset + P`; keep it and cover it with a test.
3. `Describe.limits.max_page_size` = the minimum of `config.max_page_size` and `limit_maximum - 1` over every paginated operation, so what the connector advertises is what it accepts (the shop document therefore advertises 199).
4. The native-batch `limit` of R-3 keeps its own rule; it must never exceed the declared maximum either (it already is capped, add the assertion to T-6 if it is missing).
5. Crate docs: one paragraph on the `limit.maximum - 1` rule and why (exact last-page detection without a spurious empty page).

Tests (each must fail on the mutant named):
* **T-19 (rewrite)** on `shop.openapi.json`: `page.limit = 500` sends `limit=200` and returns at most 199 rows with a cursor at offset 199; `page.limit = 200` is the same; `page.limit = 3` sends `limit=4`; `page.limit = 0` sends no `limit`. Mutant: send `P + 1` without the `limit_maximum - 1` term (the original defect).
* **T-21** a purpose-built document (put it in `tests/fixtures/openapi/`): one paginated operation without `maximum` (P is bounded by `config.max_page_size` only: `page.limit = 500` sends `limit=201`), one with `maximum: 1` (no pagination: a request with `page.limit` is `INVALID_ARGUMENT`, a request without `page` works),
  one with `minimum: 10` and `page.limit = 3` (the upstream is asked for `limit=10`, the answer is cut to 3 rows, `next_cursor` points at offset 3, following it returns the next 3 rows). Mutant: ignore `limit_minimum`.
* **T-22** `Describe.limits.max_page_size`: 199 for the shop document; `min` over several operations for the purpose-built one; a connector with `config.max_page_size = 50` advertises 50. Mutant: advertise `config.max_page_size` unchanged.
* **T-23** end-to-end paging over the shop `customers` list (12 rows) and `blobs` (40 rows) with `page.limit` equal to the advertised `max_page_size` and with `page.limit = 5`: every row exactly once, the last page has an empty `next_cursor`, the number of upstream calls equals the number of pages. This is the property the original defect broke
  and the existing round-trip test never exercised at the advertised size.

## Process

Same rules as before (`work/steps/WP-M1-11-review-1.md` Process section); commit this file with your first commit; add a "Follow-up 2" section to `work/handoffs/WP-M1-11.md` (R-5, T-19 rewritten, T-21..T-23, gates, the mutation checks M01..M16 and the reviewer mutants of round 1 re-run, plus
one for each new test); `CARGO_HOME=.git/tmp-cargo-home` removed before the handoff; do not merge, do not mark done. If a new test exposes another defect, stop, keep the failing test and finish with BLOCKED.
Finish with exactly one line `WP-M1-11 Follow-up 2: READY`, `WP-M1-11 Follow-up 2: READY-WITH-NOT-RUN` or `WP-M1-11 Follow-up 2: BLOCKED - <reason>`, then a short summary in Vietnamese.
