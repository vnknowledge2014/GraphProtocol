# GAP Specification — 06 Runtime (execution plane)

Normative IDs `RUN-*`. Rationale: ADR-003, 009, 011, 012, 015, 016. The Apollo Router four-stage `tower` idea is kept; stages are renamed for GAP's responsibilities.

## 1. Pipeline (tower layers)
Order (outermost → innermost): `TransportMux` → `GatewayLayer` → `OperationLayer` → `AuthzLayer` → `PlanLayer` → `ExecLayer` → `ConnectorLayer` (one per branch).
| Layer | Responsibility | Fails with |
|---|---|---|
| `TransportMux` | HTTP/2, gRPC, stdio, MCP/A2A facades → canonical `GapRequest` | transport errors |
| `GatewayLayer` | authn (SEC-001), tenant resolution, body-size limit, per-subject rate limit, load shedding (§5), request id/trace | `GAP_UNAUTHENTICATED`, `GAP_RATE_LIMITED`, `GAP_OVERLOADED` |
| `OperationLayer` | persisted-op lookup, mode check (GQL-060), parse + validate (GQL-002), complexity/cost (GQL-041) | `GAP_*_ERROR`, `GAP_COMPLEXITY_EXCEEDED` |
| `AuthzLayer` | policy evaluation per field/operation/action, redaction plan | `GAP_FORBIDDEN`, `GAP_APPROVAL_REQUIRED` |
| `PlanLayer` | plan cache lookup/compile (§3) | `GAP_CYCLE_UNBOUNDED`, validation |
| `ExecLayer` | run the plan: fetch/join/stream; assemble response; tasks | per-field errors |
| `ConnectorLayer` | per-branch: replica choice, breaker, retry, deadline, credentials, pool, NGC client | `GAP_BRANCH_*` |

- **RUN-001** Each layer is a `tower::Service` with a typed request/response; cross-cutting plugins are `tower::Layer`s registered at compile time (ADR-019). Layers MUST be cancel-safe.
- **RUN-002** One OpenTelemetry span per layer with names in spec/08 §2; the request span carries `gap.generation`, `gap.tenant` (hashed if configured), `gap.operation_name`.

## 2. Snapshots and publication (ADR-009)
```rust
pub struct SnapshotHandle { cell: ArcSwap<Snapshot>, ring: Mutex<VecDeque<Arc<Snapshot>>> /* control plane only */, reclaimer: ReclaimTx }
impl SnapshotHandle {
    #[inline] pub fn pin(&self) -> Arc<Snapshot> { self.cell.load_full() }                // request path (INV-01)
    pub async fn publish(&self, next: Snapshot, opts: PublishOpts) -> Result<Generation, PublishError>;   // Publisher actor only
}
```
- **RUN-003** Only the `Publisher` actor calls `publish`; it serializes publications (`generation = prev + 1`).
- **RUN-004** `publish`: (1) assert `next.format_version` supported; (2) prewarm: re-plan top-100 plan-cache keys against `next`, count failures; (3) if failures > 5% and `!breaking_change_ack` ⇒ abort `PublishError::PrewarmRegression`; (4) `cell.store(Arc::new(next))`; (5) push the previous `Arc` to `reclaimer` (dedicated thread; drop happens there); (6) start the 60 s watch (ADR-009 §5); (7) append audit record.
- **RUN-005** *Cost before execution:* `PlanLayer` computes planned cost (GQL-041) and rejects over-budget documents before any connector call.
- **RUN-006** In-flight requests and Tasks keep their pinned `Arc<Snapshot>`; Tasks that outlive `task.max_snapshot_age` (default 24 h) re-pin at step boundaries only.
- **RUN-007** `rollback(to: Generation)` is a normal publish of a retained generation with a new, higher generation number (history stays monotonic) and mark `bad=true` on the replaced one.
- **RUN-008** Snapshot swap SLO tests (ADR-009 §3) run in CI nightly with `swap_stress`: 1,000 concurrent request tasks, 100 swaps, assert zero errors, zero torn reads (each response's `generation` ∈ the set of published generations and all data came from that generation's fixtures), p99 swap ≤ 250 µs at 256 busy reader threads.
- **RUN-009** *Composition and namespacing.* The composer (`gap-snapshot`) is a pure, deterministic function from the branches' `Describe` results to the supergraph. (a) Branches are processed sorted by `BranchId`; the same inputs in any order give a byte-identical supergraph SDL and the same `branch_set_hash`; a repeated `BranchId` or `BranchUid` is an error. (b) Every non-root named type (object, interface, union, enum, input, custom scalar) a branch defines is exposed as `<ns>_<Name>`, where `<ns>` is the `BranchId` with `-` replaced by `_`, and every reference to it inside that branch is rewritten; GAP built-in types, the standard scalars and GAP directives are shared and never renamed. (c) The branch's `Query`, `Mutation` and `Subscription` fields are merged onto the built-in root types under their own names; every root field has exactly one owning branch. Two branches defining the same root field, or a type name produced twice (for example branch `a` with type `b_C` and branch `a_b` with type `C`), is `GAP_AMBIGUOUS_FIELD` naming both branches; a branch defining `gap`, `__search`, `__metaGraph` or any `__`-prefixed name is `GAP_RESERVED_NAME` (GQL-004). (d) Each operation of a branch's `Describe` result must match exactly one root field of the root type of its kind (QUERY, MUTATION, SUBSCRIPTION) and every root field of the branch must have an operation, otherwise the branch is rejected with `GAP_VALIDATION_ERROR`. (e) The composer attaches `@branch` to every root field and object type of the branch; authored `@branch`, `@capabilityGroup`, `@provenance` and `@join(status:)` are removed and reported as `GAP_DIRECTIVE_IGNORED` warnings (GQL-030); `@sideEffect` of a `T2` branch is forced to `UNKNOWN` (GQL-032). (f) The composer also returns the field-ownership map `(type, field) -> BranchUid`, consistent with the `@branch` directives of the composed schema, and GAP computes every `desc_hash` itself (NGC-011).

## 3. Planning
- **RUN-010** `Plan` is a tree/DAG of nodes: `Fetch{branch, op, selection, pushdown}`, `Flatten{path}`, `Join{lhs, rhs, keys, cardinality, strategy}`, `Parallel[..]`, `Sequence[..]`, `Traverse{...}`, `Mutate{...}`, `Gate{approval}`. Plans are pure data, serializable, hashable (`plan_hash`) and printable (`PlanExplanation`).
- **RUN-011** *Field ownership:* each field resolves to exactly one `@branch` owner or a `@join` edge. Ambiguous ownership is a composition error (`GAP_AMBIGUOUS_FIELD`) — never resolved by a model.
- **RUN-012** *Pushdown rules:* push `where` predicates, ordering, limits and projections to a branch iff its capabilities advertise them (NGC-023); otherwise evaluate in the engine and emit `GAP_PUSHDOWN_FALLBACK` (warning) when the fallback may fetch more than `limits.fallback_rows` (default 10,000), failing with `GAP_JOIN_LIMIT_EXCEEDED` beyond it.
- **RUN-013** *Determinism:* for fixed `(document, variables, snapshot, policy_version, principal_class)` the plan is byte-identical (stable sort of independent nodes by `(branch_id, op_id, path)`); tests compare `plan_hash` across 1,000 repeated plannings and across process restarts.
- **RUN-014** *Plan cache:* `moka` with key per ADR-009 §6, capacity `plan_cache.max_entries` (default 10,000), TTL 1 h, weigher by serialized size. Plans embed the `policy_version` they were authorized under; a policy bump invalidates by key.
- **RUN-015** *Stage parallelism:* independent `Fetch` nodes execute concurrently, bounded by `exec.max_parallel_fetches` (default 8) and per-branch bulkhead = `capacity`.

## 4. Joins (the N+1 guarantee)
- **RUN-020** *Foreach batching.* For a `Join` over LHS rows: (1) extract key tuples from LHS rows in order; (2) skip rows whose key contains `null` (the joined field is `null`, no error); (3) deduplicate keys by canonical encoding (JCS of typed values, integer/string distinction preserved); (4) split unique keys into chunks of `chunk = min(branch.max_batch_size, plan.max_batch=1000)`; (5) issue **one** `Query{foreach}` per chunk (chunks run concurrently up to `max(1, branch.max_in_flight/2)`); (6) stitch by hash lookup preserving LHS order. Total upstream calls ≤ `⌈unique_keys/chunk⌉` (INV-18); asserted in tests using `Complete.upstream_calls`.
- **RUN-021** *Cardinality.* `ONE_TO_ONE/MANY_TO_ONE`: RHS must yield ≤ 1 row per key; more ⇒ `GAP_JOIN_KEY_NOT_UNIQUE` for that key (first row kept only if `join.on_duplicate = FIRST`, default `ERROR`). `ONE_TO_MANY`: list ordered by RHS ordering (stable). `MANY_TO_MANY` is not expressible; use an explicit junction type (two joins).
- **RUN-022** *Limits.* `max_join_rows` (default 100,000 stitched rows per join node) and `max_join_keys` (default 50,000 unique keys); exceeding ⇒ `GAP_JOIN_LIMIT_EXCEEDED` with the observed counts; execution of that join stops (other fields continue).
- **RUN-023** *Remote predicates.* When a filter on entity A references a condition on entity B (different branches), execute B's predicate subtree first, collect up to `max_pushdown_keys` (default 5,000) keys, and inject `IN (keys)` into A's predicate. If more keys: fall back to fetching A with a safe cap and filtering in-engine, with `GAP_PUSHDOWN_FALLBACK`.
- **RUN-024** *Partial failure.* Default `onError = PARTIAL`: a failing chunk nulls only the joined fields of the LHS rows it covers and adds one error per failed chunk with `path`s of the affected rows (collapsed to the list path + `extensions.affectedIndexes`); `@budget(failFast: true)` aborts the whole operation on the first non-retryable branch error. `partial = true` in the response (GQL-051).
- **RUN-025** *Pagination.* Connection fields paginate independently; a join never hides pagination of its LHS; `after` cursors are signed, bound to `(operation hash, generation family, tenant)` and valid across generations only when the field's schema is unchanged.
- **RUN-026** *Consistency.* No cross-branch snapshot isolation is promised. Each branch call is read-committed at best; responses include `extensions.gap.asOf` per branch when known. If the request supplies `asOf` and a branch lacks `capabilities.as_of`, the engine returns a warning (not an error) unless `@budget(strictAsOf: true)`.
- **RUN-027** *Join strategy selection:* `FOREACH_BATCH` is default; `REMOTE_PREDICATE` only for filter-driven joins (RUN-023). A join targeting a branch with `max_batch_size = 0` is rejected at composition (`GAP_JOIN_NOT_BATCHABLE`, NGC-015).

## 5. Execution control
- **RUN-030** *Deadlines.* `request.deadline = now + min(client deadlineMs, gateway.max_deadline)`; every connector call gets `deadline = min(request.deadline − margin, now + branch.default_timeout)` where `margin = max(50 ms, 5% of remaining)`. Expired ⇒ `GAP_DEADLINE_EXCEEDED` (partial results retained per RUN-024).
- **RUN-031** *Cancellation.* Client disconnect, HTTP/2 RST_STREAM, task cancel, or deadline expiry cancels the request's `CancellationToken`; the engine drops futures (RAII guards release permits/in-flight), and sends gRPC cancellation to in-flight connector calls within 50 ms.
- **RUN-032** *Bulkheads.* Each branch has a semaphore of size `capacity`; acquisition waits at most `queue_wait_ms` (default 50 ms) then fails fast `GAP_BRANCH_UNAVAILABLE(retry.afterMs)`.
- **RUN-040** *Concurrency limit & shedding.* The gateway runs an adaptive concurrency limiter (gradient/AIMD on request latency; initial 256, min 16, max 8,192). When saturated it sheds by priority: `interactive` (default) > `batch` (header `X-Gap-Priority: batch`) > `background` (tasks/ingestion). Shed ⇒ HTTP 503 + `Retry-After` for whole-request refusal or `GAP_OVERLOADED` in `errors[]` for per-field.
- **RUN-050** *Per-subject limits:* token bucket per `(tenant, principal)` (`governor`), default 50 rps burst 100; per-branch limits per tenant; cost-based limiting: planned cost units/min per principal (default 600,000). Exceed ⇒ `GAP_RATE_LIMITED` with `retry.afterMs`.
- **RUN-051** Backpressure on streaming responses: bounded channels (64 events); slow consumer for > `stream.stall_ms` (default 10 s) ⇒ stream closed with `GAP_CANCELLED`/`GAP_RATE_LIMITED`, resources released.

## 6. Response assembly
- **RUN-060** Errors follow GraphQL propagation rules; non-null violation nulls the nearest nullable ancestor. The engine MUST enforce `max_response_bytes` incrementally (stop early, return `GAP_RESPONSE_TOO_LARGE` with the truncation point) rather than building the full body first.
- **RUN-061** Redaction plan from AuthzLayer is applied during assembly (fields marked `@pii` or policy-redacted are removed or masked with `"***"`; the mask kind is policy-defined) before bytes leave the process.
- **RUN-062** `@defer/@stream` incremental delivery is optional in 1.0 (feature flag `incremental`, GraphQL-over-HTTP multipart/SSE); when disabled the server answers `GAP_UNSUPPORTED_FEATURE` only if `requires` lists it.

## 7. Mutations (ADR-015)
Pipeline for each mutation field: `authz(action=Mutate)` → `side-effect policy` → `approval gate` → `idempotency check` → `[dry-run]` → `connector.Mutate` → `audit append` → `result`.
- **RUN-070** *Side-effect policy:* `NONE` ⇒ allowed by ordinary authz. `REVERSIBLE` ⇒ requires declared `@compensate`; ordinary authz. `IRREVERSIBLE` and `UNKNOWN` ⇒ approval gate unless a Cedar rule in `policies/auto-approve.cedar` matches `(principal, operation, arguments constraints)`.
- **RUN-071** *Approval gate:* the operation is converted into (or runs inside) a Task; it stops in `INPUT_REQUIRED` with an `Approval{summary, sideEffect, expiresAt}` where `summary` is generated **deterministically** from the operation, arguments (redacted) and target — never from model text. Approval by a *different* principal than the requester is required when `policy.four_eyes = true`. On approval, GAP signs an `ApprovalProof` (NGC-032) bound to `(operation, args_hash, idempotency_key)`.
- **RUN-072** *Idempotency:* key = client-provided `Idempotency-Key`/`extensions.gap.idempotencyKey` or server-generated UUIDv7 stored with the Task. `IdempotencyStore` maps `(tenant, key) → (request_hash, status, result_ref, expires_at)` with 24 h TTL; same key + different request hash ⇒ `GAP_IDEMPOTENCY_CONFLICT`; same key + same hash while in progress ⇒ wait (bounded by deadline) then return the same result; completed ⇒ replay stored result without calling the connector.
- **RUN-073** *Dry run:* `@dryRun` (or `dryRun: true`) calls `Mutate(dry_run=true)` where supported (NGC-031); for unsupported connectors returns `GAP_DRY_RUN_UNSUPPORTED` — never "simulated" success.
- **RUN-074** *Saga mode (opt-in, `@budget(saga: true)` or task `strategy=SEQUENTIAL` with `compensate=true`):* steps execute in order; on failure of step *k* the engine invokes `@compensate` operations of completed steps in reverse order with their `compensation_token`s, recording each outcome; the final state is `FAILED` with a per-step ledger. Compensation failure ⇒ `FAILED` with `needs_manual_intervention=true` and an alert. No atomicity is claimed.
- **RUN-075** Every mutation attempt (including denied/dry-run) appends an audit record (INV-14) with principal chain, operation, `args_hash`, decision, approval id, outcome.

## 8. Tasks (ADR-016)
**FSM** (⊥ = terminal):
```
SUBMITTED → WORKING | REJECTED⊥ | CANCELED⊥
WORKING → INPUT_REQUIRED | AUTH_REQUIRED | COMPLETED⊥ | FAILED⊥ | CANCELED⊥
INPUT_REQUIRED → WORKING | CANCELED⊥ | REJECTED⊥ (denied / expired) | FAILED⊥
AUTH_REQUIRED → WORKING | CANCELED⊥ | FAILED⊥
```
- **RUN-080** State transitions are persisted (`TaskStore`, WAL) *before* being acknowledged/emitted; each emits a `TaskEvent` with strictly increasing `seq` per task (INV-13). `gapTaskEvents(fromSeq)` replays then follows; SSE `Last-Event-ID` maps to `seq`.
- **RUN-081** *Lease/heartbeat:* a worker holds a lease (30 s, renewed every 10 s) on a `WORKING` task. On startup/expiry, another worker (or the same after restart) resumes steps that are `resumable` (idempotent or checkpointed); non-resumable in-flight steps are marked `FAILED(code=GAP_INTERNAL, reason=crash)` unless they carry an idempotency key and a connector replay-safe flag.
- **RUN-082** *Strategies:* `SINGLE` (one step), `PARALLEL` (all steps without `dependsOn`, results merged as an array of step results), `SEQUENTIAL` (each step depends on the previous), `DAG` (explicit `dependsOn`; cycles rejected at submit). Step `gapql` runs through the normal pipeline *as the submitting principal*; step `delegate` calls the target branch's `StartTask` (A2A-style) and mirrors its states.
- **RUN-083** *Deadline & hops:* task `deadlineAt` bounds all steps; delegated calls get `hop_count+1`, remaining deadline, and `trace_path` (INV-20).
- **RUN-084** *Cancellation:* `cancelTask` is idempotent; moves to `CANCELED` after propagating `CancelTask` to active delegated steps and cancelling local futures; compensation is **not** automatic unless saga mode.
- **RUN-085** *Approval expiry:* default 24 h (`approval.ttl`); expiry ⇒ `REJECTED(reason=approval_expired)`.
- **RUN-086** Task retention: terminal tasks kept `task.retention` (default 30 d); artifacts > 1 MiB are stored by reference (`uri`+`sha256`) in the blob store (local FS by default).

## 9. Configuration reload
| Setting class | Reload |
|---|---|
| policies (Cedar), rate limits, routing thresholds, log levels, feature flags | hot (`SIGHUP` or `gap admin reload`) — bumps `policy_version`/`config_version`; invalid config ⇒ rejected, old config retained, alert |
| branch registry, schema, indexes, calibration | via snapshot publish only |
| listeners, TLS certs (files watched; rotation hot), thread pools, store paths | listeners/certs hot where supported; others restart |

## 10. Shutdown and restart
- **RUN-090** On SIGTERM: stop accepting, mark not-ready (`/readyz` 503) immediately, drain in-flight up to `shutdown.grace` (default 30 s) with new tasks refused, persist task states, flush audit/OTel, close connectors. A second SIGTERM forces exit.
- **RUN-091** Restart safety: after crash, the audit chain verifies (SEC-092), `TaskStore` recovery runs before readiness, idempotency store survives, plan cache starts cold but prewarmed from the persisted hot-key list.

## 11. Required tests (spec IDs)
Determinism of plans (RUN-013) · cost-before-execution (RUN-005) · N+1 guard on 1k LHS rows with duplicates and nulls (RUN-020) · cardinality errors (RUN-021) · join limits (RUN-022) · remote-predicate fallback (RUN-023) · partial failure matrix (RUN-024) · deadline propagation and cancel within 50 ms (RUN-030/031) · bulkhead/shedding under load (RUN-032/040) · swap stress & prewarm gate (RUN-004/008) · task FSM exhaustive transition test + crash/restart recovery (RUN-080/081) · approval flow including four-eyes and expiry (RUN-071/085) · idempotent replay and conflict (RUN-072) · saga compensation ordering and failure ledger (RUN-074) · graceful shutdown with in-flight requests (RUN-090).
