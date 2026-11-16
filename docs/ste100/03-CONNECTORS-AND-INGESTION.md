# Connectors, Ingestion Proposals, and Snapshots `[DESCRIPTIVE]`

## 1. Six Reference Connectors (`NGC-001`)

GAP ships six `connector` adapters inside `adapters/`:

1. `gap-ngc-openapi` converts OpenAPI 3.0 specifications and groups keys via `x-gap-in-param`.
2. `gap-ngc-graphql` forwards typed operations and batched selections to downstream GraphQL services.
3. `gap-ngc-grpc` compiles Protobuf descriptors with pure-Rust `protox` without an external `protoc` binary.
4. `gap-ngc-sql` maps database tables and foreign keys into typed graph nodes and edges.
5. `gap-ngc-cli` wraps allow-listed command binaries inside an OS sandbox (`SEC-070`).
6. `gap-ngc-code` parses repository files with `tree-sitter` to build compact AST code graphs.

## 2. Safe Ingestion and Join Verification (`ADR-012`)

Discovered schemas never overwrite active traffic automatically. The ingestion pipeline extracts candidate schemas, cleans untrusted text, and stages a `proposal` for review. Cross-branch `@join` edges require statistical containment checks on real sample data before approval (`ADR-012`). Once an operator approves a `proposal`, GAP builds a new immutable `snapshot` `generation` and swaps it atomically (`INV-01`).

## 3. Extract a Schema and Create a Proposal `[PROCEDURAL]`

> **NOTE:** Specify `--tenant` to keep extraction workers and stores inside one `tenant` scope (`ING-070`).

1. Run `gap ingest once` against `gap.toml` or a single OpenAPI `--spec` file.
2. Inspect standard output to confirm deterministic extraction finishes with exit code `0`.

```bash
./target/dist/gap ingest once --config examples/walking-skeleton/gap.toml --tenant default
```

## 4. Inspect and Approve a Proposal `[PROCEDURAL]`

> **WARNING:** Reject any `proposal` that shows unexpected `@sideEffect` downgrades or instruction text inside descriptions.

1. Run `gap proposals list` to view pending schema, join, or description proposals.
2. Run `gap proposals show <ID>` to examine field types and hash diffs.
3. Approve safe proposals with `gap proposals approve <ID>`.
4. Reject unsafe proposals with `gap proposals deny <ID>`.

```bash
./target/dist/gap proposals list
./target/dist/gap proposals approve <ID>
```

## 5. Inspect and Compare Snapshots `[PROCEDURAL]`

1. Run `gap snapshot show` to inspect active `branch` endpoints and operations.
2. Run `gap snapshot diff` to compare two configuration or `snapshot` files before rollout (`RUN-009`).

```bash
./target/dist/gap snapshot show --config examples/walking-skeleton/gap.toml
./target/dist/gap snapshot diff --before old.gap.toml --after new.gap.toml
```
