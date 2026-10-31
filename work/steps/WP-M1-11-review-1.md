# WP-M1-11 — reviewer findings, round 1

Reviewer: Claude Sonnet 5.5, 2026-10-31. Verified head `db6a712`. Independently re-checked and green: the workspace gates (fmt, clippy, build, doc, nextest 406/406, deny, layers, validate, refs, trace, unittest), both
`#[ignore]` loopback tests, the kit through real TCP gRPC in front of a real HTTP upstream, a hostile-document corpus of my own (20 documents: `$ref` doubling bombs at depth 14..28, 20,000 operations, 1 MiB path segment,
6 MiB of descriptions, 5,000 duplicate `operationId`s, colliding schema names, deep `allOf`, 200,000-value enum: every one ended in a bounded `Err` or `Ok` in under 0.2 s and 230 MiB peak), and an independent oracle for the
request builder (906 hostile values through `build_request`, parsed back with Python `urllib`). One defect is high (it escapes a parameter slot on the real transport), two are medium; the rest are test gaps.
Code fixes first, tests second; one commit each.

## R-1 (high, CMP-030) the values `.` and `..` of a path parameter escape their slot on the real transport

Reviewer probe over a real loopback socket (`HttpUpstream` against `fixture::router`, operation `product`, argument `sku`): path built by `build_request` against the path the server received.

| `sku` | built | received |
|---|---|---|
| `widget-1` | `/products/widget-1` | `/products/widget-1` |
| `..` | `/products/%2E%2E` | **`/`** |
| `.` | `/products/%2E` | **`/products/`** |

The other 12 probe values (`a/b`, `%2e%2e`, `..%2f`, `?x=1`, `#frag`, `\`, `a b`, `é`, `../admin`, `%2E`, `a\..\b`) arrive intact. Cause: `ValidatedBaseUrl::join_relative` calls `Url::set_path`, and WHATWG URL parsing collapses `.` and `..`
segments **including their percent-encoded spellings** (`%2e`, `%2E%2E`): the `%2E%2E` produced by `encode_path_segment_value` is undone before the bytes reach `reqwest`. With the shop template
`/customers/{id}/orders` (probe on the `url` crate itself) `id = ".."` becomes `/orders`: the argument changes the path structure outside its slot, which CMP-030 forbids. Your tests and mutants M01/M02 compare strings
*before* `Url`, and `FixtureService` is in memory, so none of them sees this. The oracle found no other violation in 906 values (slot, round trip, no query, no fragment).

Required:
1. `build_request` refuses a path-parameter value that is exactly `.` or `..` with `INVALID_ARGUMENT` (message names the parameter, never the value): the URL layer cannot carry it, and no real key is a dot segment. Replace the `%2E` special case in
   `encode_path_segment_value`. In `foreach` it is an isolated per-index `INVALID_ARGUMENT`. Update the crate docs.
2. Defence in depth in `join_relative`: after `set_path`/`set_query` the resulting `target.path()` must have exactly the number of segments of the path that was passed in, and none of its segments may be a dot segment in any spelling
   (`.`, `..`, `%2e`, `%2E%2E`, `.%2e`, `%2e.`) or contain a backslash; otherwise `AuthError::MalformedBaseUrlComponents`. (A literal equality check of the two paths is not enough: `Url` also re-encodes harmless characters.)
3. Extraction: a path template whose literal segment is a dot segment in any spelling, or contains `\`, `?`, `#`, `%`, a space or a control character, makes the operation **skipped** with a reason (`unsafe_path_literal`).
   Reviewer probe: `tests/fixtures/openapi/hostile/path_literals.json` (copy below) is accepted today with 5 operations: `/a/../b`, `/c/%2e%2e/d`, `/e\f`, `/g?x`, `/h#i`; after the fix all five are skipped and the shop document is unchanged.

Tests (T-1..T-3, each must fail on the mutant it names):
* **T-1** (no sockets): property test over arbitrary strings plus the 14 values above: for the shop `product` operation run `build_request`, then `ValidatedBaseUrl::join_relative(...)` and parse the resulting `Url`: the path has exactly the template's segment
  count, `path_segments()` percent-decoded equals the template literals plus the argument, and `.`/`..` are refused by the builder. Mutant: accept `.`/`..` again (the old `%2E` behaviour). A second mutant: remove check 2 of `join_relative` and assert that a hand-built
  `/products/%2E%2E` is rejected there.
* **T-2** (`#[ignore = "needs loopback sockets"]`): the 14-value table over a real socket: for every value the builder accepts, the path the server recorded equals the built path; `.` and `..` are refused before any request is sent (request counter stays 0).
* **T-3** the five hostile literals above in `tests/fixtures/openapi/hostile/path_literals.json` are skipped with `unsafe_path_literal`, the shop document still yields the same operations (golden unchanged).

## R-2 (medium, NGC-020) native `foreach` silently drops every per-index variable except the key

`start_native_foreach`: the batch request is built from `base_args` plus the key list only. Any other variable carried by an index's `VariableSet` is validated against the keyed operation and then **ignored**: for example `{id: 1, "X-Request-Tag": "a"}` is answered
as if the header had not been asked for. NGC-020 requires each index to be evaluated semantically independently, so an index must either get what it asked for or fail; silently different rows are the worst outcome.
Required: group the sets of one chunk by their non-key variables (canonical JSON, deterministic order). A non-key variable that is also a parameter of the batch target operation is sent with that group's batch call (one upstream call per group and per
`max_batch_size` keys, every attempt counted in `upstream_calls`). Any other non-key variable fails **that index** with `INVALID_ARGUMENT` ("variable `x` is not supported by the batch endpoint of `customer`"), never ignored. The key-only case stays exactly as it is (one call per chunk).
Tests (T-4, T-5): (T-4) sets `{id:1,"X-Request-Tag":"a"}` and `{id:2}` over the shop native batch: index 0 fails `INVALID_ARGUMENT`, index 1 succeeds, the recorded request list contains no `ids=1`; (T-5) a small purpose-built OpenAPI document in
`tests/fixtures/openapi/` (for example `batch_vars.openapi.json`: keyed read and batch endpoint sharing a `fields` query parameter) with sets that differ in `fields`: two upstream calls, each carrying the right `fields` and its own keys, results equal to the solo reads.

## R-3 (medium, CMP-032) a native batch endpoint that pages server side turns trailing keys into "absent"

`start_native_foreach` sends `ids=k1,k2,...` and treats every key without a row as absent. If the batch target is a paginated list (`limit`/`offset` declared, as in the shop document) and the server applies a default page size smaller than the key list,
the missing keys are reported as empty indices with no error. Required: when the batch target declares `limit`, send `limit = <number of keys in this call>` (the extension's `key_field` is a unique key, document that in the crate docs and the
brief's follow-ups) and cap the effective native `max_batch_size` at the declared `maximum` of `limit` (so a chunk never asks for more rows than the endpoint may return); `Describe.limits.max_batch_size` reflects the cap.
Test (T-6): a scripted upstream (`push_scripted_response`) or a document whose `limit` has `maximum: 5` and `x-gap-in-param.max_batch_size: 50`: the advertised batch size is 5, a foreach of 12 distinct keys makes 3 upstream calls, and every recorded request carries `limit` equal to its key count.

## R-4 (medium, CMP-031 / INV-15) a caller-declared query parameter can shadow the connector's API key

`ResolvedAuth::apply` appends the `ApiKey { location: Query }` pair **after** the built query. If the OpenAPI document declares a query parameter with the same name (say `api_key`) the caller can pass one, so the request carries two `api_key=` pairs
and the caller's comes first; most servers read the first. The header variant is safe (`retain` removes the caller's header before the credential is pushed; mutant A05 shows that nothing tests it).
Required: at construction, an operation that declares a query parameter named like the configured API-key query name (compare case-insensitively) is skipped with the reason `auth_parameter_collision` (it appears in the extraction report); the header variant keeps the `retain` rule.
Test (T-11, below): both variants.

## Test gaps (tests only, no source change unless a test reveals a defect)

The mutants below survived on `db6a712` (58 one-line string mutants over request, upstream, auth and connector, run on a copy with the `#[ignore]` loopback tests included: 31 killed, 27 survived, 6 of those are equivalent, see the end). Each item says what the mutant did, so no
access to my scripts is needed; every new test must fail on its mutant.

* **T-7 (Q12, Q15)** `null` arguments: a required argument given as `null` is `INVALID_ARGUMENT` unless its schema type includes `null`; an optional argument given as `null` is skipped and never sent (query, header: `{"limit": null}` builds no `limit=` pair). While there, drop the tautology
  `value_matches_single_type("null", &Value::Null) &&` in `validate_object_map`.
* **T-8 (Q16)** `enum` validation is untested: `sort` (enum) with a value outside the enum is `INVALID_ARGUMENT`, a member passes, and numeric enum members compare by value (`1` and `1.0`).
* **T-9 (Q04)** query parameter *names* are percent-encoded: a purpose-built document with a query parameter named `a b&c=d` builds `a%20b%26c%3Dd=<value>`.
* **T-10 (U06, U07)** a header parameter named `Accept-Encoding` declared by the document is never sent (assert on the recorded request); the default `Accept: application/json` is sent when the operation declares no `Accept` and `allow_binary = false`, and is not forced when `allow_binary = true`.
* **T-11 (A05, R-4)** header collision: the operation declares a header parameter with the same name as the configured API-key header, the caller passes a value: exactly one header of that name reaches the upstream and it is the connector's credential; the query variant is skipped as in R-4.
* **T-12 (A07)** `Basic { user }` with an empty user, a `:` or a control character is refused when the connector is built (`InvalidAuthParameter`), and the error text never contains the resolved secret.
* **T-13 (C01, C03)** a cursor is bound to the cursor version and to the **operation**: a cursor issued by `customers` replayed on `orders` with equal arguments is `INVALID_ARGUMENT` (the arguments hash alone does not cover this), and a cursor whose `v` is not 1 is rejected.
* **T-14 (C04)** upstream `401` and `403` both map to `UPSTREAM_ERROR` with the fixed message; assert the message text and that it contains neither the upstream body nor the credential.
* **T-15 (C07, C08)** retry boundaries: after the last retry fails with 429/503 the error is `UNAVAILABLE` with `retry_after_ms` equal to the capped delay and **no extra sleep** (paused-time elapsed equals exactly the waits of the two retries); a delay that ends exactly at the deadline fails at once without sleeping,
  one millisecond earlier it sleeps and retries.
* **T-16 (C10, C11)** boundaries: a page whose rows exactly fill `max_response_bytes` is not truncated (`truncated = false`, empty `next_cursor`), one byte less capacity cuts one row; a last page that holds exactly `limit` rows (the upstream is asked for `limit + 1` and returns `limit`) has an empty `next_cursor`,
  a page that gets `limit + 1` rows has a cursor and returns `limit` rows.
* **T-17 (C16, C18)** `as_of` set is `INVALID_ARGUMENT`; `page` together with `foreach` and `order_by` together with `foreach` are `INVALID_ARGUMENT`; the upstream is never called (counter stays 0).
* **T-18 (C19)** emulated `foreach` concurrency has a lower bound too: 8 distinct keys, `fanout_concurrency = 4`, handler delay set: `max_concurrent_seen == 4`, and every upstream request carries exactly one key (batch size 1).
* **T-19 (C23)** `page.limit` above `max_page_size` is clamped: `limit = 500` with `max_page_size = 200` sends `limit=201` upstream (the lookahead), and `limit = 0` sends no `limit` at all.
* **T-20 (C24, C25)** `order_by` with an empty path and `order_by` with two columns are `INVALID_ARGUMENT`; descending sends `sort=-<path>`; an operation without `sort` rejects any `order_by`.

Equivalent mutants (no test needed): Q05 and Q06 (`http::HeaderValue::from_str` rejects the same bytes), U08 (`content_length()` and the header agree), A01 and A02 (`Url::set_path`/`set_query` re-encode `#` and cannot change the host; keep both guards when you rewrite
`join_relative` for R-1), C21 (`current_capacity_weight` clamps again).

## Process

Same rules as before: edit only the paths of `work/context/m1-11-brief.md` section 1, `Cargo.lock` (cargo only), `work/steps/`, `work/handoffs/`, `work/status.toml`; commit this file with your first fix; add a "Follow-up 1" section to
`work/handoffs/WP-M1-11.md` (changes, tests added, gates, the mutation checks M01..M16 re-run, and the new ones named above); `CARGO_HOME=.git/tmp-cargo-home` removed before the handoff; do not merge, do not mark done.
If a new test exposes another defect, stop, keep the failing test and finish with BLOCKED.
Finish with exactly one line `WP-M1-11 Follow-up 1: READY`, `WP-M1-11 Follow-up 1: READY-WITH-NOT-RUN` or `WP-M1-11 Follow-up 1: BLOCKED - <reason>`, then a short summary in Vietnamese.

## Appendix: `path_literals.json` (the reviewer's hostile document; create it under `tests/fixtures/openapi/hostile/`)

```json
{"openapi":"3.0.3","info":{"title":"h","version":"1"},"paths":{
 "/a/../b":{"get":{"operationId":"a","responses":{"200":{"description":"ok"}}}},
 "/c/%2e%2e/d":{"get":{"operationId":"c","responses":{"200":{"description":"ok"}}}},
 "/e\\f":{"get":{"operationId":"e","responses":{"200":{"description":"ok"}}}},
 "/g?x":{"get":{"operationId":"g","responses":{"200":{"description":"ok"}}}},
 "/h#i":{"get":{"operationId":"h","responses":{"200":{"description":"ok"}}}}},
 "components":{"schemas":{}}}
```
