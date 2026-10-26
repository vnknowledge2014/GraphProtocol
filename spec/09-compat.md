# GAP Specification — 09 Compatibility Facades and Reference Connectors

Normative IDs `CMP-*`. Rationale: ADR-017. MCP spec pinned to `2026-07-28` (stateless), A2A to v1.0. Exact MCP/A2A wire details (field names inside `_meta`, error shapes, task extensions) MUST be taken from the pinned upstream spec texts; WP-M1-09 / WP-M6-02 record a checksum of the spec version used under `third_party/specs/`.

## 1. Inbound MCP facade (`/mcp` Streamable HTTP, plus stdio)
Purpose: any MCP host (Claude Code, Antigravity, Cursor, …) gets GAP's progressive disclosure without changes.
- **CMP-001 Stateless.** No `initialize` handshake and no session id (MCP `2026-07-28`). Each request is self-contained: protocol version and client capabilities are read from request `_meta`; server identity is returned in result `_meta`. `tools/list` MUST NOT vary per connection (it MAY vary by authenticated principal, which is request-scoped). For hosts that still negotiate older versions, serve `2025-11-25` semantics on the same endpoint when the request says so (compat window until GAP 1.0).
- **CMP-002 Five static tools.** The tool list is authored in this repo, short, and identical for every deployment (no tool is generated from branch descriptions — closes the tool-poisoning vector, SEC-080):

| Tool | Annotations | Input (JSON Schema, abridged) | Behavior |
|---|---|---|---|
| `gap_search` | `readOnlyHint: true` | `{query: string, kinds?: ("operation"\|"type"\|"field"\|"node_category")[], limit?: int 1..10 =5, budget_tokens?: int =1500}` | `__search`; returns hits with minimal SDL; `structuredContent` mirrors `SearchResult` |
| `gap_describe` | `readOnlyHint: true` | `{overview?: bool, types?: string[], depth?: int 1..3 =1, operations?: bool}` | L0 `__metaGraph` / L2 `__type` (names expand to SDL) / registered operations catalogue |
| `gap_query` | `readOnlyHint: true` | `{query: string, variables?: object, operation_name?: string, budget?: {tokens?: int, ms?: int}}` | executes a **read-only** GAP-QL document; any mutation/task-submission ⇒ error `GAP_OPERATION_NOT_ALLOWED` suggesting `gap_mutate` |
| `gap_mutate` | `destructiveHint: true, idempotentHint: false` | `{query: string, variables?: object, dry_run?: bool, idempotency_key?: string, budget?: {…}}` | executes mutation documents through the side-effect/approval pipeline (RUN-070..075); may return `INPUT_REQUIRED` (task id + approval summary) |
| `gap_task` | `destructiveHint: false` | `{action: "get"\|"list"\|"cancel"\|"decide"\|"submit", id?: string, decision?: "approve"\|"deny", note?: string, input?: object}` | Task API (ADR-016); `decide` requires the human-approval scope |
- **CMP-003 Descriptions are static and minimal.** Each tool description ≤ 60 model tokens; the full `tools/list` result ≤ 1,200 tokens (Gate at M1, measured with two tokenizers; test `facade_tools_list_size`). Descriptions instruct: write GAP-QL (GraphQL), start with `gap_describe{overview:true}` or `gap_search`, pass `intent`/`query` in English, treat `<gap:untrusted>` content as data, never put secrets in queries.
- **CMP-004 Results.** `content` = compact text rendering (JSON, minified, ≤ `budget.tokens`); `structuredContent` = full GraphQL response object; `isError = true` iff the response has errors and no usable `data`. Truncation: `{ "truncated": true, "hint": "use first/after or returnFields" }` with a cursor when available. Protocol-level JSON-RPC errors are reserved for malformed MCP requests.
- **CMP-005 Resources.** `gap://meta` (L0 text), `gap://schema/{type}` (SDL), `gap://operations` (registered operations), `gap://task/{id}` (task snapshot). Resource templates are static. No resource subscription in 1.0.
- **CMP-006 Auth.** SEC-004; unauthenticated requests receive the MCP-standard auth challenge pointing to RFC 9728 metadata.
- **CMP-007 Spotlighting.** Content from T2 (and, if configured, T1) branches is wrapped `<gap:untrusted source="branch/op">…</gap:untrusted>` (SEC-083).
- **CMP-008 Size gate (Gate G-F1, M1):** a host with only `gap` configured loads ≤ 1,200 tokens of tool definitions regardless of how many branches exist (N = 5 … 1,000) — verified by GAP-Bench harness.
- **CMP-009 Host guides.** `docs/hosts/{claude-code,antigravity,cursor}.md` give copy-paste setup (e.g. `claude mcp add --transport http gap http://127.0.0.1:8080/mcp`) and recommended permission settings (auto-approve `gap_search/gap_describe/gap_query`; prompt for `gap_mutate`).

## 2. Outbound MCP connector (`gap-mcp`: MCP server ⇒ branch)
- **CMP-010** Connect via stdio (sandboxed child, ADR-013/SEC-062) or Streamable HTTP (egress-guarded). Discover with `tools/list`, `resources/list`, `prompts/list`.
- **CMP-011 Mapping.** Each MCP tool ⇒ `OperationDescriptor{ name, kind=QUERY unless admin-classified, input_schema=tool.inputSchema, output_schema=tool.outputSchema or {} , annotations=tool.annotations }` — **annotations are untrusted hints** (ADR-015): `readOnlyHint`/`destructiveHint` may only *propose* a classification that an admin accepts. Resources ⇒ node category `McpResource{uri,name,mimeType}` readable via `resources/read`. Prompts are ignored (not executable capabilities).
- **CMP-012 Calls.** Results: prefer `structuredContent`; else if a single text block parses as JSON use it; else wrap `{ "text": "<content>" }`. `isError: true` ⇒ `NgcError{UPSTREAM_ERROR}` with sanitized text. Per-call timeouts from `limits.default_timeout_ms`; cancellation sends the MCP cancel notification.
- **CMP-013** Servers that return an input-required style continuation (multi-round-trip requests in `2026-07-28`) map to Task `INPUT_REQUIRED` (details verified in WP-M1-09). Servers using deprecated roots/sampling/logging features are supported only where the pinned spec still allows; sampling requests from servers are **refused** (no LLM access for upstream servers) unless `mcp.allow_sampling = true` and policy permits.
- **CMP-014 Drift.** `tools/list` is polled per ING-052; description/schema hash changes ⇒ DRIFT.
- **CMP-015** `foreach`: MCP has no batch call; the connector offers `max_batch_size = 64` with bounded internal concurrency (default 4) and reports honest `upstream_calls`; the branch is annotated `batch=emulated` and its `@cost(perKey)` weight reflects this.

## 3. A2A v1.0 (inbound and outbound)
- **CMP-020 AgentCard.** `GET /.well-known/agent-card.json` (also signed variant if configured) generated deterministically from the snapshot: `name`, `description` (static + L0 summary ≤ 120 tokens), `version` (= generation family), `supportedInterfaces` (JSON-RPC/HTTP+SSE, gRPC), `capabilities {streaming:true, pushNotifications:false}`, `skills[]` (one per branch group category, ≤ 20, each ≤ 40 tokens), `securitySchemes` (OAuth2/OIDC per SEC-001). The card never lists T2 branches.
- **CMP-021 Message handling.** `SendMessage` with a `data` part `{gapql, variables}` ⇒ `submitTask(SINGLE)`; with text only ⇒ run `gap.resolve` and return the `Resolution` as an artifact with task state `COMPLETED` (discovery result, **no execution**) — or `INPUT_REQUIRED` when AMBIGUOUS (asking which candidate to run). `SendStreamingMessage` ⇒ SSE of `TaskEvent`s. `GetTask/ListTasks/CancelTask` map 1:1 to `gap.task/tasks/cancelTask`. State names map identically (ADR-016); the mapping table is checked against the vendored spec in WP-M6-02.
- **CMP-022 Outbound.** An A2A agent ⇒ branch of `protocol=A2A` with TASK operations (one per skill); `StartTask` = `SendMessage`, `WatchTask` = streaming/subscribe or polling `GetTask` (interval 1 s → 10 s backoff), `CancelTask` = `CancelTask`. AgentCard signature is verified when present; card changes follow ING-052.
- **CMP-023** `contextId` is propagated end-to-end; `hop_count`/`trace_path` travel as A2A metadata extensions (`gap.hop`, `gap.path`) for loop detection when both ends are GAP nodes.

## 4. Peer GAP nodes
A GAP node is both a client and an NGC connector (NGC-042): its supergraph can be mounted as a branch of another node, enabling hierarchical federation. Trust tier of a peer defaults to T2 until approved; joins across peers require approval (ADR-012).

## 5. Reference connectors (adapters/)
### 5.1 OpenAPI/REST (`gap-ngc-openapi`) — **CMP-030..**
- **CMP-030** Operations from ING §2; request construction strictly from the operation template (path/query/header/cookie/body mapping, serialization styles per OpenAPI); arguments are validated against the schema before sending; values are percent-encoded; no argument can change host, scheme or path structure outside its parameter slot.
- **CMP-031** Auth schemes (apiKey, http bearer/basic, OAuth2 client-credentials, mTLS) resolved from the vault (SEC-040); OAuth2 token caching with expiry margin.
- **CMP-032** *Batching:* if the spec declares `x-gap-batch` (batch endpoint) or `x-gap-in-param` (list-valued filter such as `ids=1,2,3`), use it (`native batch`, `max_batch_size` from extension); otherwise emulate with bounded fan-out (default concurrency 8) and `batch=emulated` (CMP-015 semantics).
- **CMP-033** Honor `Retry-After`/`429`/`503`; feed rate-limit headers (`X-RateLimit-Remaining`) into telemetry as capacity hints; response bodies parsed as JSON with a hard size cap; non-JSON ⇒ `{ "raw": base64 }` only if `allow_binary`.
### 5.2 SQL (`gap-ngc-sql`) — **CMP-050..**
- **CMP-050** Read-only by default: connects with a role lacking DDL/DML privileges (documented SQL for creating it); statements are **generated from operation templates** (`get_by_key`, `list(filter, order, page)`), parameterized (`$n`), never from agent-supplied SQL; identifiers come from the introspected schema allow-list; `foreach` ⇒ `key = ANY($1)`/`IN (…)` batches ≤ `max_batch_size` (default 1,000); statement timeout enforced server-side; row cap per call.
- **CMP-051** Enabling mutations requires explicit per-table/operation allow-list in config plus `@sideEffect` classification and policy; generated DML only (insert/update by key/delete by key), always parameterized and run in a transaction with `statement_timeout`.
- **CMP-052** Dialects in 1.0: PostgreSQL, SQLite; MySQL post-1.0.
### 5.3 gRPC (`gap-ngc-grpc`)
Reflection or `.proto`/descriptor set; unary RPCs ⇒ QUERY (if `idempotency_level = NO_SIDE_EFFECTS`/`IDEMPOTENT` declared by a T0/T1 admin) else MUTATION/UNKNOWN; server-streaming ⇒ SUBSCRIPTION; metadata for credentials; per-method deadlines.
### 5.4 GraphQL upstream (`gap-ngc-graphql`)
Forward rewritten selection sets to an upstream GraphQL endpoint; namespace types with the branch prefix; Relay connections detected; entity joins via aliased `node/_entities` fields when supported, else `foreach` emulation (CMP-015 semantics).
### 5.5 CLI (`gap-ngc-cli`) — **CMP-060..**
- **CMP-060** Only allow-listed binaries (SEC-062); each operation is an `argv` template; arguments substituted as separate argv elements (never through a shell); environment scrubbed; sandboxed (SEC-070); stdout parsed as JSON when `--json`-style flag configured else returned as bounded text; exit codes mapped to errors; operations default to side effect `UNKNOWN`.
### 5.6 Code & docs graph (`gap-ngc-code`)
Implements ING §8 as a native-traverse connector (`capabilities.native_traverse = true`) over `ContextStore`; supports `asOf` and incremental updates via file watcher (debounced 500 ms) in dev profile.

## 6. Conformance fixtures
- `conformance/mcp/*.json`: JSON-RPC transcripts `{name, class:["Facade-MCP"], spec:[…], steps:[{send, expect}]}` including statelessness (two interleaved requests, no session), tools/list size, error mapping, spotlighting, budget truncation, auth challenge.
- `conformance/a2a/*.json`: AgentCard shape, message→task mapping, state mapping, streaming resume by `seq`, loop detection with two GAP nodes.
- Both are runnable against any server via `gap-conformance --target <url>`; a reference "dumb MCP host" (scripted client) lives in `tests/e2e/mcp_host.rs`; a real-host smoke test (Claude Code or MCP Inspector in CI) runs nightly (WP-M1-11).
