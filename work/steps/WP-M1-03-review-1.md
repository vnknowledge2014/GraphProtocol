# WP-M1-03 — reviewer findings, round 1

Reviewer: Claude Sonnet 5.5, 2026-10-29. Verified head `0cf757d` (+ my spec commit `8de852a`, already on your branch: it clarifies NGC-093 (c)/(i), NGC-094, NGC-095).
Everything else passed independently: workspace gates (nextest 338/338, deny, layers, validate, refs, trace, unittest), a clean-room offline rebuild with an empty `CARGO_HOME`,
tests 3x and under 16 busy loops, real loopback gRPC (TCP and unix socket), a real HTTP server and a real stdio child process answering identical scenarios identically
(describe, health, rows, fail, foreach with duplicates and one failing key, deadline), hostile inputs (5 MiB body, chunked over-limit body, 100,000-deep JSON, 5 MiB stdio line,
stalled partial lines, garbage, batch arrays), and a descriptor-guided unknown-key injection test over all 42 messages. Four defects remain. Fix them test-first, one commit each.

## R-1 (high) HTTP streaming silently turns a JSON failure into `{}` — data loss

`crates/gap-ngc-sdk/src/http.rs`: `ndjson_stream_response` does `to_json_vec(&item).unwrap_or_else(|_| b"{}".to_vec())` and `sse_stream_response` does `to_json_string(&item).unwrap_or_else(|_| "{}".to_owned())`.
Repro (reviewer probe): a `Query` that returns one `RowSetChunk` whose row has `NumberValue(f64::NAN)` (`Struct`/`Value` cannot be rendered as JSON): over gRPC the row arrives, over stdio the call ends with an `INTERNAL`
error, over HTTP the client receives `200 OK` with the body `{}` — the chunk, its `last_for_index = true` and the row are all lost without any error, so a `foreach` consumer waits for a closing chunk that never comes (NGC-020).
Required (NGC-094, new sentence): a stream element that cannot be rendered as JSON ends the stream with an `INTERNAL` error element — NDJSON line `{"error": <NgcError JSON>}`, SSE `event: error` — and nothing after it; never a placeholder.
Also audit every other `unwrap_or_else(|_| ...)`/`unwrap_or_default()` fallback in the adapters that substitutes data for a failure (the unary paths already return an error response; the `{"code":"INTERNAL",...}` fallbacks for the
error envelope itself are fine).
Tests: add the NaN-row scenario to the transport matrix: gRPC delivers the row; HTTP NDJSON, HTTP SSE (a `Subscribe` event carrying a non-finite number) and stdio each end with exactly one `INTERNAL` error element and no `{}`.

## R-2 (medium) stdio accepts a request id that is already in flight

`crates/gap-ngc-sdk/src/stdio.rs` `register` does `guard.insert(key, tok)`: a second request with the same `id` overwrites the first call's cancellation token, and the first call's `InFlightEntryGuard` later removes the
*second* call's entry. Repro (reviewer probe): two `gap.ngc.Query` requests with `id: 91` on a connector that never ends, then `{"method":"gap.ngc.cancel","params":{"id":91}}`: exactly one `CANCELLED` response arrives, the other call
never gets a response and cannot be cancelled any more (until EOF).
Required (NGC-095, new sentence): a request whose `id` is already in flight is rejected with `-32600` (message without the payload) and does not touch the running call; the entry guard removes only its own token.
Tests: duplicate id while the first call is running -> `-32600` for the second, the first still answers exactly once after `cancel`; the same id may be reused after the first call finished; a numeric id `1` and a string id `"1"` are different ids.

## R-3 (medium) `Health` is counted against `max_in_flight`

`ServerCore::prepare_call` takes a permit for every call including `Health`. Repro (reviewer probe, `ServerConfig::with_max_in_flight(1)` and one open `Query` stream): `Health` fails with retryable `RESOURCE_EXHAUSTED`, so a busy but healthy
connector looks broken exactly when the router needs the signal (NGC-070, RTE-083).
Required (NGC-093 (c), amended): `Health` does not take a permit (it keeps its 500 ms bound and panic isolation). While you are there: the `is_health` handling inside the `Some(c)` deadline branch of `prepare_call` is dead code
(`Health` has no `ctx`; mutation C02 survived) — remove it.
Tests: saturate the cap with open streams, then `Health` answers `SERVING` on gRPC, HTTP and stdio while another `Query` still gets `RESOURCE_EXHAUSTED`; the in-flight count returns to zero afterwards.

## R-4 (medium) `GrpcClient` decodes messages of any size

`GrpcClient::from_channel` sets `max_decoding_message_size(usize::MAX)`. The client is what the gateway (WP-M1-07) and the conformance runner use against connectors that are not trusted: a hostile or buggy connector can make it buffer an
arbitrarily large message (tonic's own default is 4 MiB).
Required (NGC-093 (i), new): a default bound of 16 MiB (`pub const DEFAULT_CLIENT_MAX_DECODING_BYTES`), `GrpcClient::with_max_decoding_message_size(usize)`, and a decode-size failure surfaces as `RESOURCE_EXHAUSTED` (not `OUT_OF_RANGE`/`UNKNOWN`).
Tests: a server streaming a message larger than the configured client bound -> `RESOURCE_EXHAUSTED`; a message just under the bound passes; the default constant is 16 MiB.

## Notes (no change required unless you disagree)

* `json_mapping::proptest_json_roundtrip_covers_every_message_in_descriptor_set` takes 17-30 s (42 s under load) in a debug build: the whole crate stays under a minute, but if you can cut it to 64 cases per message without losing the shape coverage, do.
* Mutation results of the reviewer's own campaign (about 60 mutants over core/error/connector/json/foreach/http/stdio/grpc) are still running on the old head; the surviving mutants will come as follow-up 2 (tests only) after this round.
  Known so far: C06 (stream wall-clock deadline `>=` vs `>`) survives — add a test with a `ManualClock` exactly at `deadline_unix_ms` on an open stream.

## Process

Same rules as the brief (section 5): edit only `crates/gap-ngc-sdk/**`, `Cargo.lock` (cargo only), `work/steps/`, `work/handoffs/`, `work/status.toml`; commit this file together with your fixes; extend `work/handoffs/WP-M1-03.md` with a
"Follow-up 1" section (what changed, tests added, gates, which of your mutation checks M01..M11 you re-ran); `CARGO_HOME=.git/tmp-cargo-home` removed before the handoff; do not merge, do not mark done.
Finish with exactly one line `WP-M1-03 Follow-up 1: READY`, `WP-M1-03 Follow-up 1: READY-WITH-NOT-RUN` or `WP-M1-03 Follow-up 1: BLOCKED - <reason>`, then a short summary in Vietnamese.
