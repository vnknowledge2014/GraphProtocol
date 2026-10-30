# NGC connector conformance kit (`NGC-091`, `WP-M1-04`)

A black-box kit that drives a Native Graph Connector through its gRPC binding and tells you, check by check, whether it keeps the
promises of `spec/03-ngc.md`. A connector may call itself **NGC-compatible** only if the kit reports no failure and no
uncovered capability.

```bash
# the connector under test listens on plain-text gRPC (loopback or private address; TLS is not supported by the kit yet)
cargo run -p gap-conformance -- --connector-kit \
    --connector 127.0.0.1:50051 \
    --profile conformance/ngc/my-connector.kit.toml \
    --report target/kit-report.json
```

Exit code `0`: NGC-compatible. `1`: a check failed or an advertised capability could not be probed. `2`: usage or profile error.

## Why a profile

The kit cannot guess which of a connector's operations is a keyed read, which one never ends, or which one mutates. The connector's
owner says so in a **profile**, `conformance/ngc/<name>.kit.toml`. The extension is `.toml` on purpose: the GAP fixture loader
reads every `*.json` under `conformance/` and rejects unknown keys (`VER-010`). Unknown keys in a profile are rejected too.
A working example is [`sdk-reference.kit.toml`](sdk-reference.kit.toml); the kit proves itself against it (see "Self-test").

| Table | Fields | Needed by |
|---|---|---|
| `name` | profile name shown in the report | all |
| `[timing]` | `deadline_ms` (200), `grace_ms` (2000), `health_budget_ms` (500), `call_timeout_ms` (10000), `describe_budget_ms` (off) | all |
| `[read]` | `operation`, `arguments`, `key_var`, `keys` (>= 2 existing, distinct), `absent_key?`, `failing_key?`, `row_key?`, `upstream_batch?` | `foreach.*`, `errors.mapping` |
| `[page]` | `operation`, `arguments`, `limit` (smaller than the data set), `order_by?`, `other_arguments?`, `max_pages` (200) | `query.order`, `page.stability` |
| `[slow]` | `operation` (a `Query` that stays open until cancelled), `arguments`, `calls` (8, capped by `limits.max_in_flight`) | `deadline`, `cancel` |
| `[mutate]` | `operation`, `arguments`, `[mutate.probe]` = a `Query` whose row count is the number of applied effects | `mutate.*` |
| `[size]` | `operation`, `arguments`, `expected_total_rows`, `resumable` | `limits.response_size` |
| `[errors.invalid_query]` | `operation`, `arguments` the connector must refuse with `INVALID_ARGUMENT` | `errors.mapping` |

## Checks

Each check ends **pass**, **fail** or **skip**. A skip is `not_applicable` (the capability is not advertised: nothing to check) or
`not_covered` (advertised, but the profile cannot probe it: this blocks compatibility, add the table).

| Check | Spec | What it proves |
|---|---|---|
| `describe.valid` | NGC-010/012/013/015 | SDL parses (`gap-ql`, GAP reserved names rejected), operation names, only local `$ref`, `foreach_batch` implies `max_batch_size >= 1` |
| `describe.stable` | NGC-010 | two calls are equal, `not_modified` only for the hash it issued |
| `health` | NGC-070 | `SERVING` (or `DEGRADED`) within 500 ms |
| `foreach.correctness` | NGC-020 | one closing chunk per index, caller's indices kept, solo == batched == reversed, equal values both served, absent key empty, failing key isolated |
| `foreach.n_plus_one` | NGC-026, INV-18 | `Complete.upstream_calls <= ceil(distinct keys / upstream batch)`; emulated foreach is bounded by one call per distinct key and reported |
| `query.order` | NGC-021 | first page deterministic, `order_by` ascending and descending honored |
| `deadline` | NGC-025, INV-12 | a passed deadline fails `DEADLINE_EXCEEDED` at once; a deadline that passes mid-call ends the call; the capacity comes back |
| `cancel` | NGC-025, INV-12 | after a client cancel a fresh batch of slow calls is accepted within `grace_ms` (the capacity comes back) |
| `mutate.idempotency` | NGC-030 | a repeat key replays (`was_replayed`) with the same result, one effect only (with a probe), a new key and a new tenant are new requests |
| `mutate.dry_run` | NGC-031 | `DRY_RUN` with a report, nothing changed, the key of the dry run is not consumed; unadvertised dry run fails `UNIMPLEMENTED` |
| `page.stability` | NGC-021/022 | no overlap or gap, repeatable, page size does not change the result, cursor bound to arguments and tenant, garbage cursor `INVALID_ARGUMENT` |
| `errors.mapping` | NGC-023/060/061, INV-15 | unknown operation, invalid arguments, unadvertised predicate operator rejected (never ignored); no echo of the caller's credential, no stack trace |
| `limits.response_size` | NGC-027 | answers over `max_response_bytes` are cut and flagged `truncated`; `next_cursor` resumes and drains all rows exactly once |

Deadline and cancel are proven through capacity, not only through status codes: an SDK-based server drops the connector future by
itself, so the kit also checks that the slots a call held are free again. Run it on an otherwise idle connector.

Not checked (no way to observe them black-box): cursor expiry of >= 10 minutes (NGC-022), the `Describe` p95 SLO unless
`describe_budget_ms` is set, field-level scope filtering (NGC-024), idempotency retention of >= 24 h.

## Self-test

`cargo nextest run -p gap-conformance -E 'test(ngc_kit)'` runs the kit against `ngc_kit::reference::ReferenceConnector`, a small
connector built on the SDK's `MockConnector`:

* the correct connector passes all 13 checks, in memory and over a real loopback TCP socket through the CLI;
* each of 27 deliberate flaws (`Flaw`) fails exactly the checks it targets, and every check is failed by at least one flaw;
* capability gating skips what a minimal connector does not advertise; a profile that cannot probe an advertised capability is
  reported as `not_covered`; a connector without a readable `Describe` fails `describe.valid` and nothing else runs.
