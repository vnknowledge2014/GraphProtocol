# GAP Conformance Suite (`gap-conformance`)

Black-box protocol conformance fixtures and runner documentation (`VER-010`, `VER-011`, `VER-012`).

Fixtures in this directory define the language-independent wire contract of the Graph Agent Protocol (`VER-012`). Any conforming GAP server—regardless of implementation language—must satisfy the same JSON fixtures without relying on Rust-internal types.

## Directory Layout (`VER-010`)

`gap-conformance` recursively walks `--fixtures <DIR>` (default: `conformance`), loading every `*.json` file in lexicographically sorted relative-path order:

```text
conformance/
├── README.md
├── schemas/          # Optional composed GraphQL schemas referenced via "schema" (.graphql, ignored by *.json scan)
├── gql/              # Core GAP-QL validation, execution, and error-envelope fixtures
├── discovery/        # Progressive-disclosure discovery fixtures
├── federation/       # Cross-branch join and batched federation fixtures
├── tasks/            # Async task lifecycle fixtures
├── secure/           # Authorization, redaction, and tenant-isolation fixtures
├── mcp/              # MCP facade fixtures
├── a2a/              # A2A facade fixtures
└── ngc/              # NGC connector conformance fixtures
```

**Class authority rule:** Subdirectory names are organizational only. A fixture's conformance class membership comes strictly from its top-level `"class"` JSON array, validated against the eight protocol classes:

`Core` · `Discovery` · `Federation` · `Tasks` · `Secure` · `Facade-MCP` · `Facade-A2A` · `NGC`

## Fixture Format & Matching Semantics

Normative fixture syntax and response-matching rules are specified in [`spec/02-gap-ql.md` §11](../spec/02-gap-ql.md):

- **Required top-level fields:**
  - `"name"`: non-empty string identifying the test case.
  - `"class"`: non-empty array of valid conformance class names.
  - `"spec"`: array of normative spec requirement IDs (e.g. `["GQL-070", "GQL-062"]`).
  - Either `"request"` + `"expect"` **XOR** `"steps"` (a non-empty array of `{ "request", "expect" }` objects executed sequentially on the same principal; execution halts at the first failing or erroring step).
- **Optional top-level fields:**
  - `"schema"`: relative path to the composed supergraph SDL used by offline/spawned servers.
  - `"principal"`: logical caller identity resolved via `--principals <FILE>`.
- **Validation:** Unknown top-level/request/step keys, missing or empty `"name"`, empty or unknown `"class"`, invalid JSON, empty `"steps"`, or providing both/neither of `"request"` and `"steps"` fail fixture loading with a typed error naming the offending file.
- **Subset matching (`spec/02-gap-ql.md` §11):**
  - Each step's `"expect"` is matched against the normalized observed response `{ "status", "data", "errors", "extensions", "headers", "dataNull" }`, where `"headers"` keys are lower-cased and `"dataNull"` is derived strictly from `data == null` (never trusted from the wire).
  - Objects match recursively as key subsets; scalars compare equal (numbers compare by mathematical value); arrays match index-wise with equal length unless wrapped in `{ "unordered": true, "items": [...] }`, which requires equal length and a one-to-one bipartite pairing. Mismatches report the JSON path of the first difference.

## CLI Modes (`VER-011`)

```bash
# Inspect CLI options
cargo run -p gap-conformance -- --help

# Run Core and Secure fixtures against a live HTTP endpoint and emit a JSON report
cargo run -p gap-conformance -- \
  --target http://127.0.0.1:8080/graphql \
  --fixtures conformance \
  --class Core \
  --class Secure \
  --principals principals.json \
  --report report.json
```

| Flag | Status | Behavior |
|---|---|---|
| `--target <URL>` | Active (`WP-M0-09`) | POSTs `{query, variables?, operationName?, extensions?}` as `application/json` to `<URL>` **verbatim** (no path appended) with `Accept: application/graphql-response+json, application/json;q=0.9` (`GQL-003`). `https://` targets return a typed error because `reqwest` is compiled without a TLS backend (`ADR-002`). |
| `--fixtures <DIR>` | Active (`WP-M0-09`) | Root directory recursively scanned for `*.json` fixtures (default: `conformance`). |
| `--class <CLASS>` | Active (`WP-M0-09`) | Repeatable class filter. Unknown class names fail CLI parsing with exit code `2`. |
| `--principals <FILE>` | Active (`WP-M0-09`) | JSON file mapping fixture `"principal"` names to bearer tokens (`INV-15`). |
| `--report <PATH>` | Active (`WP-M0-09`) | Writes the deterministic JSON conformance and spec-coverage report to `<PATH>`. |
| `--offline <DIR>` | Staged (`WP-M1-02`, `WP-M1-08`) | Parsed by CLI; exits with code `2` naming `WP-M1-02` and `WP-M1-08`. |
| `--spawn <EXAMPLE>` | Staged (`WP-M1-12`, `WP-M1-13`) | Parsed by CLI; exits with code `2` naming `WP-M1-12` and `WP-M1-13`. |
| `--connector-kit` | Staged (`WP-M1-04`) | Parsed by CLI; exits with code `2` naming `WP-M1-04` (`NGC-091`). |

## Exit Codes

| Exit Code | Meaning |
|---|---|
| `0` | All executed fixtures passed (`verdict == "pass"`). |
| `1` | Conformance failure: at least one executed fixture produced `verdict == "fail"` or `verdict == "error"`. |
| `2` | Usage, configuration, fixture-load, principals-load, unsupported-scheme (`https://`), or staged-mode error. |

## Deterministic JSON Report Format (`--report`)

`--report <PATH>` writes pretty-printed JSON with deterministic `BTreeMap` ordering for `coverage` and omits wall-clock timestamps unless a `gap_core::clock::Clock` is explicitly injected (`INV-02`):

```json
{
  "runner": {
    "name": "gap-conformance",
    "version": "0.1.0"
  },
  "target": "http://127.0.0.1:8080/graphql",
  "fixtures": [
    {
      "name": "unknown field gets didYouMean",
      "file": "gql/unknown_field.json",
      "class": ["Core"],
      "spec": ["GQL-070", "GQL-062"],
      "verdict": "pass",
      "detail": null
    }
  ],
  "summary": {
    "total": 1,
    "passed": 1,
    "failed": 0,
    "errors": 0,
    "spec_ids_total": 2,
    "spec_ids_covered": 2
  },
  "coverage": {
    "GQL-062": {
      "covered": true,
      "passed": 1,
      "fixtures": [
        {
          "name": "unknown field gets didYouMean",
          "file": "gql/unknown_field.json",
          "verdict": "pass"
        }
      ]
    },
    "GQL-070": {
      "covered": true,
      "passed": 1,
      "fixtures": [
        {
          "name": "unknown field gets didYouMean",
          "file": "gql/unknown_field.json",
          "verdict": "pass"
        }
      ]
    }
  }
}
```

Per `spec/02-gap-ql.md` §11, a requirement ID in `coverage` has `"covered": true` **if and only if** `passed >= 1` (fixtures ending in `"fail"` or `"error"` are listed under `fixtures` for traceability but do not mark the spec ID as covered).

## Principals File (`--principals`, `INV-15`)

`--principals <FILE>` points to a UTF-8 JSON object mapping logical principal names referenced by fixtures (`"principal": "agent-readonly"`) to bearer tokens:

```json
{
  "agent-readonly": "replace-with-ephemeral-test-token",
  "agent-admin": "replace-with-ephemeral-admin-token"
}
```

Security and execution guarantees (`INV-15`, `SEC-040`, `SEC-041`):
- Tokens are wrapped immediately in `gap_core::secret::SecretString` (zeroized on drop) and attached to outbound HTTP requests as sensitive `Authorization: Bearer <token>` headers.
- Bearer tokens **never** appear in `tracing` events/spans, `Debug`/`Display` output, transport or JSON parse errors, CLI output, or `--report` JSON (verified by `gap_testkit::canary::CanarySink`).
- Any fixture naming a `"principal"` absent from `--principals` (or executed when `--principals` is omitted) is recorded with `"verdict": "error"` and a diagnostic reason—never skipped.
