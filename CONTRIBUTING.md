# Contributing to GAP (Graph Agent Protocol)

Thank you for contributing to GAP. Both human engineers and AI coding agents follow the same spec-driven, work-package-isolated workflow (`ADR-022`, `docs/AGENT-GUIDE.md`).

## 1. Source of Truth

When documents disagree, resolve conflicts in this order and update lower-precedence files in the same change set:

1. Latest **ADR** in `docs/01-DECISIONS.md`
2. Normative specification in `spec/` (`00` through `09`)
3. Work-package definitions in `work/work-packages.toml`
4. `docs/AGENT-GUIDE.md`
5. Code comments

Never implement behaviour that is not covered by the specification or an accepted ADR. If you find a specification gap or defect, stop and follow the deviation protocol in `docs/AGENT-GUIDE.md` §9.

## 2. Prerequisites

- **Rust `1.98.1`** (edition 2024, pinned via `rust-toolchain.toml` with `rustfmt` and `clippy`; see `ADR-002`)
- **Python 3.11+** (standard-library `tomllib` for `tools/*.py`)
- **`cargo-nextest`**, **`cargo-deny`**, and **`just`** (used across workspace verification recipes)

## 3. Work-Package (WP) Workflow (`ADR-022`)

All changes land through a single Work Package at a time:

1. **Pick a ready WP:**
   ```bash
   python3 tools/wp.py ready
   ```
2. **Claim it and create a dedicated branch:**
   ```bash
   python3 tools/wp.py set WP-M0-01 doing
   # GitButler virtual branch when configured, or standard git branch:
   git checkout -b wp/WP-M0-01
   ```
   One WP = one branch = one reviewable change set.
3. **Read only what applies:** `AGENTS.md`, `.agent/rules/gap-protocol.md`, `docs/AGENT-GUIDE.md`, `spec/00-conventions.md`, `spec/01-architecture.md`, and the `spec_refs` listed on your WP.
4. **Expand into bite-sized steps before coding:** create `work/steps/<WP-ID>.md` (at most 12 steps, 2–5 minutes each, recording assumptions, rejected simpler alternatives, risks, exact file paths, and verification commands; see `docs/AGENT-GUIDE.md` §4).
5. **Respect path ownership (`owns`):** modify only files matching your WP's `owns` globs (plus `work/steps/<WP-ID>.md`, `work/handoffs/<WP-ID>.md`, and `work/status.toml`). If you need an edit in another WP's scope, open a request note under `work/requests/` instead of editing cross-boundary files.
6. **TDD with spec traceability:**
   - Write the failing test first.
   - Annotate requirement tests with `// spec: <ID>` or `#[doc = "spec: <ID>"]` (for example, `// spec: ARC-003`).
   - Name invariant tests `inv_<nn>_<what>` (for example, `inv_01_single_snapshot_per_request` for `INV-01`).
7. **Rust & architecture quality bar (`ARC-001`, `ARC-002`, `ARC-003`, `ADR-003`):**
   - `#![forbid(unsafe_code)]` in every crate except `gap-s1` (where each block requires `// SAFETY:` and an entry in `docs/UNSAFE.md`).
   - No `unwrap`, `expect`, `panic!`, `todo!`, or `unimplemented!` outside test targets (`clippy.toml` + crate-level `#![deny(...)]`).
   - No ML dependencies on the typed execution path (`ARC-002`).
   - No unmeasured performance or token claims in docs or comments (`ADR-021`).
8. **Run all gates before finishing:**
   - Every command in the WP's `acceptance` list
   - `python3 tools/wp.py validate`
   - `python3 tools/check_layers.py`
   - `python3 tools/check_refs.py`
   - `just verify` (once CI/justfile recipes land)
9. **Write the handoff and mark done:** record outputs and notes in `work/handoffs/<WP-ID>.md` (`docs/AGENT-GUIDE.md` §10), then run `python3 tools/wp.py set <WP-ID> done`.

## 4. Commit Message Convention (Conventional Commits v1.0.0)

All commits MUST follow [Conventional Commits 1.0.0](https://www.conventionalcommits.org/en/v1.0.0/):

```text
<type>(<scope>): <imperative summary in <= 72 chars>

<optional body wrapped at 72 chars explaining why>

Refs: <WP-ID>; spec: <comma-separated spec/ADR IDs>
```

Allowed types: `feat`, `fix`, `docs`, `style`, `refactor`, `perf`, `test`, `build`, `ci`, `chore`, `revert`.
Use the crate or subsystem name as `<scope>` (for example, `feat(gap-core): ...`, `chore(repo): ...`, `chore(work): ...`).
Breaking pre-1.0 changes are permitted only in minor releases and require a migration note in `CHANGELOG.md` (`ADR-024`, `OPS-040`).

## 5. Licensing

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in GAP is licensed under the Apache License, Version 2.0 (`LICENSE`). New third-party dependencies must pass `cargo deny` licence checks (`ADR-002`).
