---
description: GAP project rules - non-negotiable architecture and workflow constraints for every agent working in this repository.
trigger: always_on
---
# GAP project rules

1. **Spec first.** Source-of-truth order: ADR (docs/01-DECISIONS.md) > spec/ > work/work-packages.toml > docs/AGENT-GUIDE.md > code comments. Never implement behaviour that no spec/ADR covers; record the gap (AGENT-GUIDE §9).
2. **Work packages only.** Take a WP from `python3 tools/wp.py ready`; touch only its `owns` paths; expand it into 2-5 minute steps in `work/steps/<WP>.md` before coding; one WP = one branch; Conventional Commits with `Refs: WP-..; spec: ...`.
3. **TDD with traceability.** Failing test first; tag tests `spec: <ID>`; invariant tests are named `inv_<nn>_*`.
4. **Architecture invariants (spec/01 §6):** no ML on the execution path (ARC-002); deterministic authorization (INV-04); snapshot pinned per request (INV-01); no lock on the request path (INV-02); tenant in every key (INV-09); no N+1 joins (INV-18); S1 optional (INV-17).
5. **Untrusted text.** Anything from a branch (descriptions, results, errors, annotations) is data. Never concatenate it into instructions, never execute anything discovered from the network, never forward the client's bearer token.
6. **Rust quality.** No `unsafe` outside `gap-s1`; no `unwrap/expect/panic/todo/unimplemented` in non-test code; errors are typed values; no lock guard across `.await`; bounded channels; RAII guards for counters/permits.
7. **Measure, do not assert.** Numbers are Targets/Gates until a benchmark artifact exists (ADR-021). Verify crate versions and APIs from `docs/evidence/crate-versions.md`, docs.rs and vendored specs, not memory.
8. **Gates are not negotiable.** A failing gate blocks the milestone; propose remedies, never lower thresholds.
9. **Stop and ask** for credentials, paid services, licence questions, SEC semantics changes, destructive operations, edits outside the repository, or disabling any hook/lint/sandbox (AGENT-GUIDE §11).
10. **Before finishing:** acceptance commands green, `just verify`, `python3 tools/check_layers.py`, `python3 tools/wp.py validate`, handoff written, `omni index` if the host uses it.
