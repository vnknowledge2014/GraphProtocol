# WP-M1-03 brief — `gap-ngc-sdk` (Connector trait, gRPC/HTTP/stdio servers, foreach helper, mock)

Status: binding for WP-M1-03 (reviewer, 2026-10-29). Deliverables and acceptance commands are those of `work/work-packages.toml`; this file makes them concrete.
If the two disagree the work package wins; say so in the handoff. Normative inputs: `spec/03-ngc.md` (NGC-001..003, 020, 025, 026, 060..063, 071, 090 and the
new NGC-093..096), `proto/gap/ngc/v1/ngc.proto` through the `gap-proto` crate, INV-12/15/18 (`spec/01-architecture.md`), `gap-core` (`Clock`, `NgcErrorInfo`, `NgcErrorCode`).

## 1. Scope

In: `crates/gap-ngc-sdk/**`. Out (stop and ask before touching): `crates/gap-proto/**` (its generated types and golden are frozen; if a `.proto` defect blocks you, stop), `crates/gap-core/**`,
root `Cargo.toml`, `spec/`, `third_party/`, `.local/`, and every consumer crate (`gap-conformance`, `gap-testkit`, `gap-server`, adapters keep compiling untouched).
Not in this WP: TLS/mTLS (no `rustls` is vendored; NGC-093 g makes the server fail closed instead), gRPC reflection, reference adapters (`adapters/*`), conformance fixtures,
HTTP/stdio *clients* beyond what the tests need, auth/JWT (the SDK only passes call metadata through).

## 2. Dependencies (vendored; anything else is a stop-and-ask)

`crates/gap-ngc-sdk/Cargo.toml` declares exactly (sorted, `name.workspace = true` unless features are shown). The root catalogue already contains every name (the reviewer added
`futures-util`, `http-body-util`, `hyper-util`, `tokio-util` before dispatch); do not edit it.

```toml
[dependencies]
axum.workspace = true
bytes.workspace = true
futures-util.workspace = true
gap-core.workspace = true
gap-proto.workspace = true
http.workspace = true
hyper-util = { workspace = true, features = ["tokio"] }
prost.workspace = true
prost-reflect = { workspace = true, features = ["serde"] }
prost-types.workspace = true
serde = { workspace = true, features = ["derive"] }
serde_json.workspace = true
thiserror.workspace = true
tokio = { workspace = true, features = ["io-std", "io-util", "macros", "net", "rt", "sync", "time"] }
tokio-stream = { workspace = true, features = ["net"] }
tokio-util.workspace = true
tonic.workspace = true
tonic-prost.workspace = true
tracing.workspace = true

[dev-dependencies]
http-body-util.workspace = true
proptest.workspace = true
tokio = { workspace = true, features = ["macros", "rt-multi-thread", "test-util", "time"] }
tower = { workspace = true, features = ["util"] }
```

`Cargo.lock` grows offline by the missing packages (cargo does it; it is the only lock change). The sandbox cannot read `~/.cargo/config.toml`: use `CARGO_HOME=.git/tmp-cargo-home` for cargo and delete
that directory before the handoff. `cargo nextest` is not installed for you: run `cargo test -p gap-ngc-sdk --locked --offline` (3 runs) and report nextest NOT RUN; the reviewer runs it.

## 3. Design (binding shape; names inside a module are yours unless stated)

Crate header as the other crates (`#![forbid(unsafe_code)]`, `deny(clippy::unwrap_used, expect_used, panic, todo, unimplemented)`; tests may use `expect`). Modules:

1. `error` — conversions both ways between `gap_core::error::NgcErrorInfo` and `gap_proto::ngc::v1::NgcError` (NGC-063; unknown wire codes map to `ERROR_CODE_UNSPECIFIED` semantics, never panic; `details` Struct <-> JSON map);
   `NgcErrorInfo` -> `tonic::Status` (code table: INVALID_ARGUMENT->InvalidArgument, NOT_FOUND->NotFound, PERMISSION_DENIED->PermissionDenied, UNAUTHENTICATED->Unauthenticated, RESOURCE_EXHAUSTED->ResourceExhausted,
   FAILED_PRECONDITION->FailedPrecondition, DEADLINE_EXCEEDED->DeadlineExceeded, CANCELLED->Cancelled, UNAVAILABLE and UPSTREAM_ERROR->Unavailable, UNIMPLEMENTED->Unimplemented, INTERNAL and ERROR_CODE_UNSPECIFIED->Internal, CONFLICT->Aborted)
   that ALSO carries the prost-encoded `NgcError` in binary metadata `ngc-error-bin`, and the reverse (prefer the metadata, fall back to the code table); the HTTP status table of NGC-094.
2. `connector` — `trait Connector: Send + Sync + 'static` with one method per RPC taking `(&self, request, CallScope)`; unary methods return `impl Future<Output = NgcResult<Response>> + Send`, streaming ones
   (`query`, `subscribe`, `watch_task`) return `impl Future<Output = NgcResult<BoxStream<Item>>> + Send` where `BoxStream<T> = Pin<Box<dyn Stream<Item = NgcResult<T>> + Send>>` and `NgcResult<T> = Result<T, NgcErrorInfo>`.
   Required: `describe`, `query`. Default `health` = SERVING. Defaults returning `UNIMPLEMENTED`: `mutate`, `subscribe`, `start_task`, `get_task`, `cancel_task`, `watch_task`, `sketch`, `explain`
   (`mutate` with `dry_run = true` and no override stays `UNIMPLEMENTED`, NGC-031). No `async-trait`; the trait is used through generics (`C: Connector`), never `dyn`.
   Doc comment: implementations MUST be cancellation-safe (the adapters drop the future/stream at any await point on cancel or deadline, NGC-090).
   `CallScope`: `cancel: tokio_util::sync::CancellationToken` (fires on client cancel and on deadline), `deadline: tokio::time::Instant`, `request_id`, `tenant_id`, `traceparent`, and `metadata: BTreeMap<String, String>` (lower-case keys;
   gRPC metadata / HTTP headers / empty on stdio). Its `Debug` MUST redact `authorization` and every `*-bin`/`*token*`/`*secret*`/`*key*` value (INV-15, NGC-080) — test it.
3. `core` — `ServerConfig` (`clock: Arc<dyn gap_core::Clock>` default `SystemClock`, `default_timeout` 60 s, `health_timeout` 500 ms, `max_in_flight` set by the operator (default 256; the connector's own `Describe` limits are not consulted),
   `max_request_bytes` 4 MiB, `max_error_details_bytes` 4 KiB, a bounded `stream_buffer`, `allow_insecure_non_loopback: false`) and `ServerCore<C>` holding the connector, the config and the in-flight semaphore. It is the ONLY place that implements NGC-093
   (deadline, cancellation, in-flight cap, panic isolation, error clamping); the three transports are thin: decode -> core -> encode. Deadline math uses `clock.unix_millis()` against `ctx.deadline_unix_ms`, converted to a `tokio::time::Instant`,
   so tests can pause tokio time and drive a `ManualClock`.
4. `json` — protobuf JSON mapping over `prost-reflect`: one process-wide `DescriptorPool` built once from `gap_proto::DESCRIPTOR_SET` (`OnceLock`), functions converting a static prost message to canonical JSON bytes/value and back
   (static -> `DynamicMessage` -> serde; options: canonical lowerCamelCase, 64-bit integers as strings, enums by name, defaults omitted; parsing accepts both camelCase and snake_case, ignores unknown fields like the binary decoder,
   rejects unknown enum names and wrong types with `INVALID_ARGUMENT`). Used by `http` and `stdio`; no hand-written JSON mapper.
5. `grpc` — a `tonic` service implementing `gap_proto::ngc::v1::connector_server::Connector` over `ServerCore`, `serve_incoming` (generic stream of IO, used by tests and unix sockets), `serve_uds`, `serve_tcp` (refuses a non-loopback address
   unless `allow_insecure_non_loopback`, NGC-093 g; message-size limits from the config), and a small typed `GrpcClient` that maps `Status` back to `NgcErrorInfo`.
6. `http` — `pub fn router<C>(core: Arc<ServerCore<C>>) -> axum::Router` implementing NGC-002 + NGC-094 (strict method/path/content-type, body limit, NDJSON for `Query`, SSE for `Subscribe`/`WatchTask`).
7. `stdio` — `serve_stdio(core, reader, writer)` generic over `AsyncBufRead`/`AsyncWrite` and `serve_process_stdio(core)` over tokio stdin/stdout, implementing NGC-095 (concurrent requests, cancel notification, bounded line length, EOF = cancel all and return).
8. `foreach` — NGC-096. Suggested shape (adjust, but keep the semantics): `fn foreach<F, Fut>(sets: Vec<VariableSet>, cfg: ForeachConfig, scope: &CallScope, upstream: F) -> NgcResult<ForeachRun>` where `upstream(Vec<Struct>) -> Fut`,
   `Fut: Future<Output = NgcResult<Vec<NgcResult<Vec<Struct>>>>>` returns one result per distinct set, same order and length (a length mismatch fails that batch with `INTERNAL`); `ForeachRun` exposes the chunk stream and, once it ended, `upstream_calls`.
9. `mock` — `MockConnector`: scripted responses per method (closures), call counters, in-flight high-water mark, a drop guard that records when a future/stream was dropped (cancellation tests), configurable delays (tokio time). Public (NGC-090), no feature flag.
10. `testing` — `in_memory_grpc(core) -> (GrpcClient, Handle)` over `tokio::io::duplex` (no sockets) plus a helper to drive `stdio` over a duplex.

## 4. Tests (binding)

* **No sockets, no child processes** in tests: your sandbox cannot run them. gRPC and stdio run over `tokio::io::duplex`, HTTP over `tower::ServiceExt::oneshot` on the `Router`. The reviewer runs the loopback/UDS smoke tests separately.
* **Transport matrix**: one scenario table executed through gRPC, HTTP and stdio, comparing normalised outcomes (rows, error code, retryable, retry_after, closing chunks): describe; query; foreach with 4 indices incl. one failing and one empty;
  optional RPC not overridden -> UNIMPLEMENTED; mutate dry_run unsupported; deadline already past (connector never invoked); deadline passing mid-stream (connector stream dropped, last item DEADLINE_EXCEEDED); cancel mid-stream (drop observed);
  `max_in_flight` saturation (immediate retryable RESOURCE_EXHAUSTED with `retry_after_ms`, no queueing); connector panic (INTERNAL with a fixed message, server keeps serving); oversized request; every one of the 14 `ErrorCode` values mapped on each transport;
  an error message of 10,000 characters and a details object over 4 KiB are clamped/dropped (NGC-061).
* **Per-transport**: HTTP 405/404/415/413, NDJSON line framing and the mid-stream `{"error":…}` line, SSE `event: error`, `Retry-After`; stdio -32700/-32600/-32601/-32602, batch array rejected, interleaved concurrent requests, cancel notification, EOF behaviour,
  stdout carries protocol lines only; gRPC `ngc-error-bin` round trip and fallback to the code table; plaintext non-loopback bind refused.
* **JSON mapping**: canonical form checks (int64 as string, bytes base64, enum names, `Struct`/`Value`/`Timestamp`, oneof, map), snake_case and camelCase accepted, unknown field ignored, unknown enum name rejected, round trip static -> JSON -> static equal for a message of every
  shape class; plus a proptest over randomly built `DynamicMessage`s of every message in `DESCRIPTOR_SET` (as in `gap-proto/tests/wire_roundtrip.rs`).
* **foreach properties** (proptest against a naive reference evaluator): results per index equal the reference, rows of one index keep upstream order, exactly one closing chunk per index, `upstream_calls == ceil(distinct / batch)`,
  observed concurrency never above `max_in_flight`, duplicates evaluated once, failing batch/element isolated, cancel/deadline stop further upstream calls, `max_batch_size = 0` rejected.
* `CallScope` Debug redaction; deterministic tests only (no sleeps on wall time: use `tokio::time::pause` + `ManualClock`); run 3x without flakiness.
* Every test carries `// spec: <ID>` (NGC-001, 002, 003, 020, 025, 026, 060, 061, 062, 063, 071, 090, 093..096) and the traceability tool stays green.

## 5. Process rules

Follow `docs/AGENT-GUIDE.md` (step expansion, TDD, handoff). Branch `wp/WP-M1-03` from `main`; claim with `python3 tools/wp.py set WP-M1-03 doing`; write `work/steps/WP-M1-03.md`; small Conventional Commits (`feat(gap-ngc-sdk): ...`).
Edit only `crates/gap-ngc-sdk/**`, `Cargo.lock` (cargo only), `work/steps/`, `work/handoffs/`, `work/status.toml`. Run before the handoff: `cargo fmt --all --check`, `cargo clippy --workspace --all-targets --locked --offline -- -D warnings`,
`RUSTDOCFLAGS='-D warnings' cargo doc --workspace --no-deps --locked --offline`, `cargo build --workspace --locked --offline`, `cargo test -p gap-ngc-sdk --locked --offline` (3 runs), `python3 tools/check_layers.py`, `tools/wp.py validate`, `tools/check_refs.py`,
`tools/spec_trace.py --warn`, the tools unit tests. Mutation checks (required in the handoff; edit, show the named test failing, restore): flip the deadline comparison; drop the in-flight release; skip the dedupe; chunk size off by one; drop the closing chunk of an empty index;
report keys instead of calls in `upstream_calls`; return a panic message; forward the `authorization` value in `Debug`; accept GET on the HTTP path; skip the NDJSON newline; ignore the stdio cancel notification.
Do not merge; do not mark the work package done. Finish with exactly one line `WP-M1-03: READY`, `WP-M1-03: READY-WITH-NOT-RUN` or `WP-M1-03: BLOCKED - <reason>`, then a short summary in Vietnamese.

## 6. What the reviewer will check

Scope against `owns`; nextest, deny and the gate list; a clean-room offline rebuild; an independent protobuf-JSON oracle (Python) against the SDK's JSON for every message shape; a differential foreach oracle; loopback gRPC/HTTP/UDS and a real stdio
child process smoke test; a mutation campaign on core/foreach/transport code; slow-loris and oversized-line probes on HTTP and stdio; flakiness under CPU load.
