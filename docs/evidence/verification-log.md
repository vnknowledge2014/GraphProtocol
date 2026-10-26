# Verification log of the review (2026-10-26)

What was actually checked while producing `docs/00-REVIEW.md` and the spec set, how to repeat it, and what it found. Anything not listed here was **not** verified.

| # | Check | How (repeatable) | Result | Effect |
|---|---|---|---|---|
| 1 | Old EBNF completeness | script: collect nonterminals used vs defined in `gap-agent-spec-and-skill.md` §I | 21 undefined nonterminals | F-09 → ADR-001 |
| 2 | `ArcSwap` cost claims | `cd docs/evidence/arcswap-bench && cargo run --release` (arc-swap 1.9.3, rustc 1.98.1) | load 15 ns; store 0.16–23 µs (p99 59 µs @256 readers); 400 MB drop 28.9 ms on swap thread | F-01 → ADR-009 |
| 3 | Crate versions/licences/deps | crates.io API (`/api/v1/crates/<name>`, `/versions/<v>/dependencies`) for ~75 crates | table in `crate-versions.md`; `tonic 0.12.3` needs `axum ^0.7`; `laya-onnx` 404; `ort` rc; `surrealdb` licence `non-standard` | F-07, F-08 → ADR-002 |
| 4 | open-jev status | fetched README (`kyegomez/open-jev`) | "unofficial research implementation with random weights"; crate `openjev` reserved | F-02 → ADR-004 |
| 5 | Laya checkpoints/claims | fetched READMEs (`NandhaKishorM/laya`, `receptron/laya`, `aovestdipaperino/laya-rust`) + model card `laya-multilingual` | multilingual 0.342 on typed decisions; typed-decisions 0.766; "ships uncalibrated"; training data unstated; per-workload recalibration required | F-03, F-05 → ADR-005/007 |
| 6 | MCP / A2A state | web search 2026-10-26 | MCP `2026-07-28` stateless; A2A v1.0 stable (03/2026), AAIF-hosted 08/2026; tool-poisoning/rug-pull consensus mitigations | F-21, F-23 → ADR-016/017, SEC |
| 7 | omni DAG schema | `omni docs dag-patterns/swarm-engine`; grep for `write_scopes` | field absent; documented fields only | F-10 → ADR-022 |
| 8 | Generated DAGs accepted | `omni run --estimate-dag work/dag/m0.json` and `m1.json` | accepted (budgets ~100k tokens/task) | AGENT-GUIDE §4 |
| 9 | Skill quality | `omni skill validate .agent/skills/gap-protocol-architect/SKILL.md` | 30/30 Grade A, 0 security findings | – |
| 10 | Wire contracts compile | `protoc -I proto gap/ngc/v1/ngc.proto gap/gateway/v1/gateway.proto` and `protox` 0.10 | OK after fixing enum value collision `REJECTED` (MutationOutcome vs TaskState) | NGC-001 |
| 11 | Built-in SDL valid | `graphql-js` 16 `buildSchema` + `validateSchema` on spec/02 §3 + §5 blocks | OK after adding missing `enum Format` | GQL-010 |
| 12 | Work-package plan consistent | `python3 tools/wp.py validate` | initially 10 ownership overlaps between concurrent WPs → fixed by dependencies/narrowed `owns`; now OK (85 WPs, acyclic, spec refs resolve) | ADR-022 |
| 13 | Dangling requirement references | `python3 tools/check_refs.py` | initially 3 dangling (GQL-040, GQL-041, RUN-063) → fixed; now OK | spec hygiene |
| 14 | Local System-1 stand-in | `omni decide --state … --choice a,b,c --abstain --json` | returned in ~12 ms, `backend: local-laya-calibrated`, two options with identical probabilities (0.2381) | Observation only: appears to be a lightweight local engine, *not* a 421M transformer; do not use as ground truth (AGENT-GUIDE §12) |

## Explicitly NOT verified (open items with owners)
- Laya model accuracy/ECE/latency on GAP workloads (WP-M5-08 will measure); vendor numbers only.
- Apollo Router / Hasura NDC implementation details (taken from sub-agent reports; consistent with public architecture, not re-read from source).
- `rmcp` 3.7 conformance with MCP `2026-07-28` (WP-M1-09), MCP `_meta` key names and MRTR/input-required shape (WP-M0-03 SPEC-NOTES), A2A v1.0 exact state names (WP-M6-03).
- `bge-small-en-v1.5` licence/availability and candle support at the pinned version (WP-M2-04).
- GraphQL specification edition and `@oneOf` support level in `apollo-compiler` 1.33 (WP-M0-03, WP-M1-01).
- macOS `sandbox-exec` long-term availability (risk R-10).
- Every statement of the form "p95 ≤ X ms" in `spec/` is a Target/SLO awaiting its benchmark.
