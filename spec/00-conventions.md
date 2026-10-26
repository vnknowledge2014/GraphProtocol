# GAP Specification — 00 Conventions

**Spec version:** `0.1.0-draft` · **Wire string:** `gap/0.1` · **Language of normative text:** English
**Status of this document set:** normative for implementers (human or AI). Rationale lives in `docs/01-DECISIONS.md` (ADRs); where this spec and an ADR disagree, the ADR with the later date wins and the spec MUST be fixed in the same change set.

## 1. Normative language
The key words **MUST, MUST NOT, SHOULD, SHOULD NOT, MAY** are to be interpreted as in RFC 2119 / RFC 8174 when in capitals.

## 2. Requirement identifiers and traceability
Every normative statement carries an identifier `<AREA>-<NNN>`:

| Area | File | Area | File |
|---|---|---|---|
| `ARC` | 01-architecture | `RUN` | 06-runtime |
| `GQL` | 02-gap-ql | `SEC` | 07-security |
| `NGC` | 03-ngc | `OPS` | 08-operations |
| `RTE` | 04-routing | `CMP` | 09-compat |
| `ING` | 05-ingestion | | |

- Each `MUST`/`MUST NOT` requirement MUST be referenced by at least one automated test via a marker comment/attribute `spec: <ID>` (Rust: `#[doc = "spec: RUN-012"]` on the test fn, or a `// spec: RUN-012` line; conformance fixtures: `"spec": ["GQL-021"]`).
- `tools/spec_trace.py` (WP-M0-07) fails CI if a MUST has no test reference, or a test references an unknown ID.
- Requirement IDs are never reused. Removed requirements are marked `(withdrawn)`.

## 3. Kinds of numbers
| Marker | Meaning | Enforcement |
|---|---|---|
| **Limit** | Hard behavior the implementation MUST enforce (e.g. max depth 12). Configurable only within the stated range. | Unit/conformance tests |
| **SLO** | Operational objective for a reference environment (§4). Missing an SLO is a release-blocking defect once the milestone that owns it is complete. | Benchmarks in CI (nightly) |
| **Target** | Design goal not yet measured; MUST be turned into a Gate or SLO by the milestone named next to it. | Bench harness |
| **Gate** | Quantitative acceptance criterion checked by a named job before a milestone or feature flag can be completed/enabled. | CI / manual review record |

Normative documents MUST NOT contain unmeasured performance or cost claims (ADR-021).

## 4. Reference environment (for SLOs)
Apple M-class laptop (≥ 8 performance cores, 16 GB RAM) **and** Linux x86_64 (8 vCPU, 32 GB RAM, no GPU), release build, single `gap-server` process, connectors mocked in-process with 1 ms service time unless stated. GPU/Metal numbers are labeled. Results record CPU model, commit hash, and `Cargo.lock` hash.

## 5. Conformance classes
| Class | Contents | Required for |
|---|---|---|
| **Core** | GAP-QL profile, Gateway HTTP, NGC `Describe/Query`, snapshot, deterministic planner, errors | any implementation |
| **Discovery** | `__metaGraph`, `__search`, `gap.resolve`, abstention semantics | claim "GAP Discovery" |
| **Federation** | foreach joins, remote predicates, partial errors, circuit breaking, LB | claim "GAP Federation" |
| **Tasks** | Task API, approvals, durable recovery, A2A binding | claim "GAP Tasks" |
| **Secure** | authn/z, policy, trust tiers, audit, sandbox, SSRF guard | production deployment |
| **Facade-MCP / Facade-A2A** | inbound meta-tools / agent card | host interoperability |

`crates/gap-conformance` is a black-box runner (HTTP/gRPC/stdio) driven by fixtures in `conformance/`; a server passes a class iff all fixtures tagged with that class pass.

## 6. Glossary
- **Branch** — a registered upstream capability provider reachable through an NGC connector (DB, REST/OpenAPI, gRPC, GraphQL, MCP server, A2A agent, CLI, code/doc graph).
- **Connector (NGC server)** — process/library implementing `proto/gap/ngc/v1/ngc.proto` for a branch.
- **Operation** — a callable unit exposed by a branch (query field, mutation, task, tool). **Element** — any searchable schema item (operation, type, field, node category, example).
- **Capability group** — set of interchangeable operations (ADR-006). **Replica** — a member of a group.
- **Snapshot** — immutable compiled supergraph + indexes + policy bindings, identified by `generation` (monotonic `u64`).
- **Plane** — execution / discovery / control (ADR-003).
- **Trust tier** — T0/T1/T2 (ADR-013). **Proposal** — pending request to add/change a branch.
- **Principal** — authenticated caller (human, agent, service) with a delegation chain. **Tenant** — isolation domain.
- **Task** — durable long-running unit of work with the A2A-compatible state machine (ADR-016).
- **S1 / System-1** — optional calibrated small decision model used by the discovery plane (ADR-005). **Baseline router** — retrieval-only ranking.
- **CISG** — Canonical Intermediate Schema Graph produced by deterministic extractors.

## 7. Identifier and encoding rules
- `TenantId`, `BranchId` (human-readable slug `[a-z][a-z0-9_-]{1,62}`), `BranchUid` (UUIDv7, stable across generations), `OperationId` (`<branch>.<name>`; names `[A-Za-z_][A-Za-z0-9_]*`), `GroupId` (blake3-128 hex of canonical group signature), `TaskId`/`ProposalId`/`ApprovalId` (UUIDv7).
- Hashes: blake3-256 hex unless stated; `desc_hash` = blake3 over canonical JSON (RFC 8785 JCS) of `{name, description, input_schema, output_schema, annotations}`.
- Time: RFC 3339 UTC with `Z`; durations in integer milliseconds in APIs (`*Ms`), microseconds only in telemetry (`*_us`).
- JSON canonicalization for any hashed or model-input JSON: RFC 8785 (JCS) with object keys sorted; numbers per ECMAScript.
- Text is UTF-8, NFC-normalized at ingress; model inputs additionally NFKC (ADR-005).

## 8. Configuration
Single TOML file `gap.toml` (schema in `docs/config.schema.json`, generated by `gap config schema`), overridable by `GAP_*` environment variables (`GAP_SERVER__LISTEN=…`, double underscore = nesting). Secrets are referenced (`secret://vault/<name>`, `env://NAME`, `file:///path`), never inlined. Unknown keys are errors. Reload semantics in `spec/06-runtime.md` §9.

## 9. Compatibility and evolution
- Pre-1.0: breaking changes only in minor versions with a migration note in `CHANGELOG.md` and a conformance fixture diff.
- Post-1.0: SemVer; servers MUST accept requests from clients of the previous minor (N-1); deprecations last ≥ 2 minors; `@deprecated` + `extensions.warnings` signal them.
- Feature negotiation: clients send `extensions.gap.version = "gap/0.1"` and optional `features: [..]`; servers answer with the supported subset in `extensions.gap.features`. Unknown features are ignored, unknown *required* features (`requires: [..]`) yield `GAP_UNSUPPORTED_FEATURE`.
