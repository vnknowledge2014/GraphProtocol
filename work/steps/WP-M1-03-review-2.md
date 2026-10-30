# WP-M1-03 — reviewer findings, round 2

Reviewer: Claude Sonnet 5.5, 2026-10-30. Verified head `a365f2c` (your follow-up 1). Independently re-checked and green: workspace gates (nextest 343/343, deny, layers, validate, refs, trace, unittest),
R-1 (a NaN row over HTTP now ends with one `{"error":{"code":"INTERNAL",...}}` line), R-3 (`Health` answers SERVING while the cap is saturated and a second `Query` still gets `RESOURCE_EXHAUSTED`),
the real-transport smoke suite (gRPC TCP + unix socket, real HTTP, real stdio child) and the unknown-key injection test. Two defects are new (they come from the round-1 fixes), the rest are test gaps found by
my mutation campaign (69 string mutants over core, error, connector, json, foreach, http, stdio, grpc; most were killed, the survivors are listed below). Code fixes first, tests second; one commit each.

## R-5 (medium) the stdio in-flight id is released AFTER the final response is queued

`stdio.rs`: every spawned call does `let _entry = entry_guard;` and sends its last line (`send_unary` / the `{"end":true}` line of `send_stream` / the final error line) while the guard is still alive.
A client that reuses an id the moment it reads the final response can therefore be rejected with `-32600 request id is already in flight`. Reviewer stress test (multi-thread runtime, in-memory duplex, 20,000 sequential
`Query` calls that all use `id: 1`, each sent as soon as the previous `end` line was read): 0 rejections on an idle machine, **62 spurious rejections under 32 busy-loop processes**; unary calls showed 0 in that run but have the same ordering.
Required: release the id entry BEFORE the last line of a call is sent (compute the final line, drop the entry guard, then send), for unary results, stream `end` lines and error responses, so that when a client can see the
final response the id is already free (NGC-095: "an id may be reused once its call has finished").
Test (deterministic, not a timing test): make the final send block (a full output channel or a writer that is not drained), assert that at that moment the id is already absent from the in-flight map (for a unary call and for a stream);
plus keep a stress loop (for example 2,000 immediate reuses on a multi-thread runtime) as a smoke test.

## R-6 (high) `GrpcClient` streams keep yielding errors after the decode-size error

Reviewer probe: server streams one 8 KiB row, `GrpcClient::with_max_decoding_message_size(4096)`: the first item is `Err(RESOURCE_EXHAUSTED)` (correct) but the stream then yields **8,209 more `Err(INTERNAL "protocol error: received message with invalid compression flag ...")` items**:
tonic keeps parsing the body of the oversized message as new frames. With the default 16 MiB bound a hostile connector gets millions of error items out of one message, so the bound does not protect the gateway and a consumer that loops until `None` spins.
Required: every client-side stream (`query`, `subscribe`, `watch_task`) ends after its first `Err` (fuse it: no further item after an error, the underlying body is dropped), whatever the error.
Tests: the oversized-message case above yields exactly one item, the `RESOURCE_EXHAUSTED` error, then `None`; a server that fails after two good items yields exactly 2 items + 1 error + `None`; the stream never yields after an error (a small property over a scripted sequence of Ok/Err items).

## Test gaps (tests only, no source change unless a test reveals a defect)

The mutants below survived on `a365f2c` (each is the one-line change named in my campaign, exact patterns are in `~/.claude/reviewer-tools/gap/m103/mutate.py`; names are the mutant ids):

* **T-1 (C14)** `deadline_unix_ms` negative (e.g. `-1`) is a deadline in the past: `DEADLINE_EXCEEDED` without invoking the connector (core-level test; all three transports not needed).
* **T-2 (C10)** a call whose transport cancellation token is already cancelled fails `CANCELLED` and never invokes the connector (core-level, unary and stream).
* **T-3 (C07, C08)** the connector returns successfully but the wall clock (a `ManualClock`) has passed `deadline_unix_ms` while tokio time has not: the unary call and the stream-open both fail `DEADLINE_EXCEEDED`, the result/stream is not delivered.
* **T-4 (C12, C15)** cooperative cancellation: when the deadline fires or the client cancels, the `CancellationToken` in the connector's `CallScope` is cancelled (unary and stream; assert from inside the connector, for example a future/stream that observes `scope.cancel.cancelled()` and records it).
* **T-5 (K05, K06)** `CallScope` `Debug` redacts `cookie`, `set-cookie` and `proxy-authorization` values too (add them to the redaction test next to `authorization`; also assert that harmless keys such as `traceparent` stay readable).
* **T-6 (F03)** cancelling a `foreach` (not a deadline) stops further upstream batches: with more batches than `max_in_flight`, cancel after the first batch started and assert the upstream call counter stops growing.
* **T-7 (F07)** `into_query_stream`: `Complete.rows_total` is the total number of rows (use several chunks per index and duplicated keys), not the number of chunks; `upstream_calls` matches the handle.
* **T-8 (H04, H05)** the HTTP body limit boundary: a body of exactly `max_request_bytes` is accepted, one byte more is `413`, for a `Content-Length` request and for a chunked request.
* **T-9 (H09)** `Content-Type: application/json; charset=utf-8` (and upper-case `Application/JSON`) is accepted; `application/jsonx`, `text/json` and a missing header are `415`.
* **T-10 (H08)** the SSE response carries `Cache-Control: no-cache` and `Content-Type: text/event-stream` (assert the headers).
* **T-11 (H11)** a stalled `Health` request body is answered `504 DEADLINE_EXCEEDED` after `health_timeout` (not `default_timeout`).
* **T-12 (S01, S02)** the stdio line limit boundary: a line of exactly `max_request_bytes` bytes is accepted (both when it arrives in one read and when it arrives in several partial reads), one byte more is the fatal `-32600`.
* **T-13 (C09, G01, S03)** two tests only fail by hanging when their feature regresses (a mutant that lets `serve_tcp` bind a non-loopback address, and one that drops the partial-line timer both made `cargo test` hang for 8 minutes): wrap the awaits in `tokio::time::timeout` so a regression fails fast;
  and add a test that a `stream`-holding call keeps its `max_in_flight` permit until the stream ends (a second `Query` is `RESOURCE_EXHAUSTED` while the first stream is open, accepted after it ended).

Equivalent mutants (no test needed): S04 (EOF `session_cancel.cancel()` — `abort_all` drops the call guards which cancel anyway), S05 (CRLF: `serde_json` ignores a trailing `\r`), F09 (the unfold pre-cancel check is repeated by the biased select).

## Process

Same rules as before: edit only `crates/gap-ngc-sdk/**`, `Cargo.lock` (cargo only), `work/steps/`, `work/handoffs/`, `work/status.toml`; commit this file with your first fix; add a "Follow-up 2" section to `work/handoffs/WP-M1-03.md`
(changes, tests added, gates, mutation checks M01..M11 re-run); `CARGO_HOME=.git/tmp-cargo-home` removed before the handoff; do not merge, do not mark done.
Finish with exactly one line `WP-M1-03 Follow-up 2: READY`, `WP-M1-03 Follow-up 2: READY-WITH-NOT-RUN` or `WP-M1-03 Follow-up 2: BLOCKED - <reason>`, then a short summary in Vietnamese.
