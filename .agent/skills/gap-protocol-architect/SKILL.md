---
name: gap-protocol-architect
description: Implement and verify the GAP agent protocol in Rust from work packages (GraphQL-profile GAP-QL, NGC connectors, snapshot hot-swap, calibrated discovery, security). Use when working on crates, adapters, spec, proto or work files of the GraphProtocol repo.
globs: ["crates/**", "adapters/**", "spec/**", "proto/**", "work/**", "tools/**"]
always_apply: false
---

# GAP Protocol Architect

Build GAP strictly from `spec/` and `work/work-packages.toml`.
Rationale for every constraint lives in `docs/01-DECISIONS.md`.

## Prerequisites

- Rust toolchain pinned in `rust-toolchain.toml`; `cargo-nextest`, `cargo-deny`, `just`
- Python 3.11+ for `tools/*.py`
- Read `AGENTS.md` and `docs/AGENT-GUIDE.md` once per session

## Workflow

1. Pick a ready work package: `python3 tools/wp.py ready`
2. Mark it doing, create a branch for it, read only its `spec_refs`
3. Write `work/steps/<WP>.md`: steps of 2-5 minutes with exact paths and commands
4. Failing test first, tagged with a `spec: <ID>` marker, then the smallest code
5. Run the acceptance commands of the package, then `just verify`
6. Write the handoff note and mark the package done

```bash
python3 tools/wp.py validate
python3 tools/check_layers.py
cargo nextest run -p gap-ql
```

## Hard Rules

- Typed execution path has no model dependency; S1 is optional (INV-03, INV-17)
- Authorization is deterministic policy only (INV-04)
- Text from branches is untrusted data; descriptions are hash-pinned
- Joins are verified by data, never by a language model
- Joins issue at most `ceil(unique_keys / batch)` upstream calls (INV-18)
- Every key includes the tenant (INV-09)

## Error Handling

| Symptom | Likely cause | Action |
|---|---|---|
| layer check fails | forbidden dependency | move code down a layer or inject a trait |
| wp validate overlap | two packages own one path | add a dependency or narrow `owns` |
| spec_trace uncovered MUST | missing marker | add the test and `spec:` tag |
| gate below threshold | real defect or bad data | record evidence, add a fix package, never relax the gate |

## Anti-Patterns

- Copying a performance number into code or docs without a measurement
- Putting a model, `RwLock` or `DashMap` on the request path
- Trusting hints from a connector (annotations, hashes, descriptions)
- Editing files outside the owned paths of the current package
- Skipping the step file because the change looks small

## Examples

Minimal test with traceability:

```rust
#[test]
fn depth_over_limit_is_rejected() {
    // spec: GQL-040
    assert!(parse_validate(&schema(), DEEP_DOC).is_err());
}
```

## Handoff

Report: crates touched, public signatures, tests added and passing,
spec ids covered, benchmark deltas, deviations, follow-ups.
