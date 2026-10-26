# GAP Specification — 03 Native Graph Connector (NGC)

Normative IDs `NGC-*`. Wire form: `proto/gap/ngc/v1/ngc.proto`. Rationale: ADR-012, 014, 015, 016. Informed by Hasura NDC (capabilities/schema/query/mutation, `foreach` variable sets, remote predicates) but **not** wire-compatible with it.

## 1. Roles and bindings
A *connector* exposes one branch. GAP's `ConnectorLayer` is the only client. Bindings:
- **NGC-001 gRPC (primary).** Service `gap.ngc.v1.Connector`. TLS 1.3 required off-host; mTLS RECOMMENDED; unix-domain sockets allowed on-host.
- **NGC-002 HTTP/JSON mapping.** For simple connectors: `POST /gap/ngc/v1/{Describe|Query|Mutate|Health|Sketch|Explain}` with the protobuf JSON mapping; `Query` returns `application/x-ndjson` (one `QueryResponse` per line). Subscriptions via SSE; tasks via the same verbs. Behavior MUST be identical to gRPC.
- **NGC-003 stdio.** Local-process connectors speak newline-delimited JSON-RPC 2.0 with the same method names (`gap.ngc.Describe`, …) on stdin/stdout; stderr is logs only. Process connectors run sandboxed (SEC-070).
- **NGC-004** `ngc_version` in `Describe` MUST equal the major.minor of `gap/0.x`; GAP refuses connectors with a different minor pre-1.0.

## 2. Describe
- **NGC-010** `Describe` MUST be side-effect free, idempotent and fast (SLO p95 ≤ 100 ms for ≤ 5,000 operations). It MAY return `not_modified` when `known_spec_hash` matches.
- **NGC-011** `spec_hash` is the connector's own change detector. GAP MUST compute its **own** `desc_hash` per operation and per branch from the received content (spec/00 §7) and MUST NOT trust any connector-provided hash for drift decisions (ADR-014).
- **NGC-012** `schema_sdl` MUST be valid GraphQL SDL, define only branch-local types, and contain no GAP reserved names (GQL-004). Operation `name` MUST match `[A-Za-z_][A-Za-z0-9_]*`.
- **NGC-013** `input_schema`/`output_schema` are JSON Schema 2020-12. Unknown keywords are ignored; `$ref` MUST be local (`#/…`) — remote refs are rejected (SSRF).
- **NGC-014** `side_effect_hint`, `idempotent_hint`, `annotations` and all free text (`description`, `examples`) are **untrusted** (ADR-015, ADR-014). They influence only discovery text (after sanitization) and *proposed* classification; they never grant execution rights.
- **NGC-015** `limits.max_batch_size = 0` ⇒ the connector does not support `foreach`; GAP then issues one call per key **only if** the plan's join node is marked `allow_per_key_fallback` (default false; joins to such branches are rejected at composition with `GAP_JOIN_NOT_BATCHABLE`, honoring INV-18).
- **NGC-016** Examples SHOULD include ≥ 3 positive and ≥ 2 negative intents per operation; they seed RouteBench (RTE-060) and retrieval.

## 3. Query
- **NGC-020 foreach.** If `foreach` is non-empty, the connector MUST evaluate the operation once per `VariableSet` *semantically independently*, stream results tagged by `foreach_index`, and MAY interleave indices. Every index MUST end with exactly one chunk having `last_for_index = true`. A failing index returns `error` on its closing chunk without affecting others.
- **NGC-021 Ordering.** Within one index, row order MUST follow `order_by` when supported and be deterministic (stable tiebreak by primary key) when `page` is used.
- **NGC-022 Pagination.** `Page.cursor` is opaque to GAP. Cursor tokens MUST be bound to (operation, arguments hash, tenant) and expire ≥ 10 minutes after issue.
- **NGC-023 Predicate pushdown.** The planner sends `predicate` only with operators advertised in `capabilities.predicate_ops`. A connector receiving an unadvertised operator MUST fail `INVALID_ARGUMENT` (never ignore it silently).
- **NGC-024 Selection.** Connectors SHOULD honor `selection` to project columns/fields (token and bandwidth control) and MUST NOT return fields the principal's `scopes` do not allow.
- **NGC-025 Deadlines.** Connectors MUST stop work and return `DEADLINE_EXCEEDED` when `ctx.deadline_unix_ms` passes and MUST observe gRPC cancellation promptly (INV-12).
- **NGC-026 Stats.** `Complete.upstream_calls` MUST report the number of upstream requests made to serve the call; a foreach call of N keys SHOULD report ≈ `⌈N / upstream_batch⌉`, never N when batching is possible (used by the N+1 audit, VER-040).
- **NGC-027 Size.** Responses larger than `limits.max_response_bytes` MUST be truncated with `Complete.truncated = true` and a `next_cursor` when resumable.

## 4. Mutate
- **NGC-030 Idempotency.** For a given `(tenant, operation, ctx.idempotency_key)` the connector MUST apply the effect at most once and return the original result with `was_replayed = true` on repeats within its retention window (≥ 24 h) — if `capabilities.idempotency`. If a connector does not support idempotency, GAP MUST treat all its mutations as non-retryable and `idempotent=false`.
- **NGC-031 Dry run.** If `dry_run`, the connector MUST NOT change state and MUST return `outcome = DRY_RUN` with a `dry_run_report`; if unsupported it MUST return `UNIMPLEMENTED` (GAP maps to `GAP_DRY_RUN_UNSUPPORTED`).
- **NGC-032 Approval proof.** When policy required approval, GAP attaches `ApprovalProof` signed with the gateway key. Connectors with high-risk operations SHOULD verify it (public key distributed in the connector config) and MUST reject mismatching `(operation, arguments hash, idempotency_key)`.
- **NGC-033 Compensation.** If `compensation_token` is returned, GAP stores it with the audit record so the declared `@compensate` operation can be invoked with it (saga mode, RUN-074).

## 5. Subscribe and Tasks
- **NGC-040** `Subscribe` streams `Event`s with monotonically comparable `resume_token`; connectors SHOULD support resumption; otherwise GAP restarts from "now" and emits a `GAP_PARTIAL_RESULT` warning to subscribers.
- **NGC-041 Task semantics** (only if `capabilities.tasks`): `StartTask` returns `SUBMITTED` or `WORKING` immediately; `seq` strictly increases per task; `INPUT_REQUIRED`/`AUTH_REQUIRED` carry `input_request`; terminal states (`COMPLETED, FAILED, CANCELED, REJECTED`) are final; `CancelTask` is idempotent and returns the post-cancel status (possibly still `WORKING` if cancellation is asynchronous). `WatchTask(from_seq)` replays every status with `seq ≥ from_seq` then follows live.
- **NGC-042** A GAP node acting as a connector for another GAP node MUST implement `Describe/Query/Mutate/Task*` over its own supergraph, set `ctx.hop_count + 1`, append its id to `trace_path`, and reject calls where `hop_count > max_hops` or its id is already in `trace_path` (`GAP_LOOP_DETECTED`, INV-20).

## 6. Sketch (join verification support)
- **NGC-050** `Sketch` returns statistics over at most `sample_size` rows (cap: `sketch.max_sample`, default 100,000) of `field_path` produced by a *read* operation, hashing each non-null canonical value as `blake3(salt ‖ canonical_value)` truncated to 64 bits and returning the `bottom_k` smallest distinct hashes. Raw values MUST NOT be returned.
- **NGC-051** Containment of A in B is estimated by GAP as `|bottomk(A) ∩ bottomk(B)|` over the first `k_eff = min(k, |A|, |B|)` hashes of the *union's* bottom-k, with error bound reported by `tools`/`gap-ingest` (ADR-012 thresholds). Connectors MUST use the exact salt supplied.
- **NGC-052** Sketching is subject to the same authorization, rate limits and audit as `Query`; it runs as the ingestion service principal with read-only scopes.

## 7. Errors
- **NGC-060** Connectors MUST map failures to `NgcError.code`; GAP translates to `GAP_*` (spec/02 §10): `UNAVAILABLE/UPSTREAM_ERROR → GAP_BRANCH_ERROR` (retryable per flag), `DEADLINE_EXCEEDED → GAP_BRANCH_TIMEOUT`, `PERMISSION_DENIED → GAP_FORBIDDEN`, `RESOURCE_EXHAUSTED → GAP_RATE_LIMITED`.
- **NGC-061** `NgcError.message` MUST NOT contain credentials, internal hostnames or stack traces; connectors log those with the `request_id`.
- **NGC-062** Fatal call-level failures use gRPC status codes consistent with `ErrorCode`; per-index failures use `RowSetChunk.error`.

## 8. Health, readiness and limits
- **NGC-070** `Health` MUST respond within 500 ms; `DEGRADED` signals reduced capacity (GAP reduces the connector's capacity weight by 50% until `SERVING`). `NOT_SERVING` opens the circuit immediately (RTE-083).
- **NGC-071** Connectors MUST enforce their own `max_in_flight` and respond `RESOURCE_EXHAUSTED` (retryable, with `retry_after_ms`) when saturated.

## 9. Credentials and identity
- **NGC-080** GAP resolves branch credentials from the vault (SEC-040) and attaches them as call metadata `authorization` (or mTLS identity) scoped to the connector; the end-user/agent bearer token is **never** forwarded (SEC-011).
- **NGC-081** When `auth.needs_end_user_delegation`, GAP obtains a delegated token via OAuth 2.0 Token Exchange (RFC 8693) with `resource = auth.resource_indicator` (RFC 8707) and attaches that instead.
- **NGC-082** `ctx.principal` is informational for audit/row-level logic; connectors MUST NOT use it as the sole authentication of GAP.

## 10. Connector SDK and conformance
- **NGC-090** `gap-ngc-sdk` provides `trait Connector` (async, cancellation-safe), a gRPC/HTTP/stdio server adapter, deadline/cancel helpers, a `foreach` helper that deduplicates keys and chunks upstream calls, and a mock for tests.
- **NGC-091** `gap-conformance --connector <addr>` runs the connector kit (`conformance/ngc/*`): Describe validity (SDL parses; names; schema refs), foreach correctness (order/independence/partial failure), N+1 audit (`upstream_calls`), deadline/cancel, idempotency, dry-run, pagination stability, error mapping, size limits. A connector may claim "NGC-compatible" only if all applicable fixtures pass (capability-gated).
- **NGC-092** Reference connectors shipped in-repo (adapters/): `openapi`, `grpc`, `graphql`, `sql` (read-only by default), `cli` (sandboxed), `code` (tree-sitter graph), `mcp` (outbound), `a2a` (outbound).

## 11. Mapping notes (informative)
| NDC concept | NGC | Difference |
|---|---|---|
| `/capabilities` + `/schema` | `Describe` | merged; includes semantic docs, examples, side-effect *hints* |
| `/query` with `variables` | `Query` with `foreach` | per-index streaming + partial failure |
| `/mutation` | `Mutate` | idempotency, dry-run, approval proof |
| – | `Sketch`, `Task*`, `Subscribe` | join verification, A2A-style tasks, events |
