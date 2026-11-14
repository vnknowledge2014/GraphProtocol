# Laya Weight Provenance, Licence, and Risk Assessment

- **Work package:** `WP-M5-01`
- **Spec refs:** `SEC-102`, `ADR-005`, `ADR-007`, `ADR-008`, `RTE-042`, `RTE-062`, `RTE-064`, `RTE-065`, `RTE-067`, `RTE-069`, `INV-04`, `INV-05`, `INV-17`

## 1. Known vs. Unknown Provenance

| Property | Known / Observed State | Unknown / Gap |
|---|---|---|
| Architecture & heads | Multi-head typed decision model (`choice`, 5-class `score`, `noul`) consuming `RTE-042` JCS `state`, `question`, and `options` | Full pretraining corpus mixture (`training_data_note = "unstated by authors"`) |
| Licence (`models/manifest.toml`) | Apache-2.0 metadata recorded for `laya-typed-decisions`, `laya-base`, and experimental `laya-multilingual` | Underlying training-set attribution cannot be asserted solely from repository cards |
| Calibration state | Raw vendor logits are uncalibrated (`ADR-005`, `ADR-008`) | Domain-specific temperature `T_b` and split-conformal quantile `q_hat_b` depend on GAP deployment corpora (`RTE-062`, `RTE-064`) |
| Multilingual variant | `laya-multilingual` measured at chance-level (`0.342`) in `ADR-005` review | No production SLO improvement expected without GAP multilingual fine-tuning |
| Offline digest pinning | `models/manifest.toml` records schema-valid placeholder hashes (`sha256_verification_status = "offline-unpinned-requires-online-verify"`) while operating offline | Exact release weight bytes and live HuggingFace SHA-256 digests MUST be pinned online via `gap models pull --verify` before enabling S1 weights |

## 2. Legal-Review Checklist (`SEC-102`)

1. **Weights out of Git:** No model weights or binary tensors are committed to version control (`models/manifest.toml` stores metadata and SHA-256 digests only).
2. **Closed-world supply chain (`SEC-102`):** `gap models pull` verifies exact SHA-256 and byte length before activating cached weights; `offline_mode = true` fails closed without network access.
3. **Training provenance disclosure:** Every Laya entry carries `training_data_note = "unstated by authors"` explicitly so downstream operators do not assume audited dataset lineage.
4. **Checklist verdict:** **Conditional approval for optional advisory discovery ranking only** (`laya-typed-decisions`); `laya-multilingual` is restricted to `experimental-non-default` evaluation and MUST NOT be enabled by default (`ADR-005`).

## 3. Architectural Risk Containment

- **Zero authorization influence (`INV-04`, `INV-05`):** S1 runs strictly after tenant authentication and Cedar `Discover` visibility filtering; branch/operation identifiers and `TrustTier` labels never enter S1 input text (`RTE-042`).
- **Fail-open to ML-free baseline (`INV-17`):** Missing, corrupt, unverified, or timed-out weights cleanly degrade discovery to `BaselineRouter` with `GAP_DISCOVERY_DEGRADED`; GAP-QL validation and execution paths never depend on ML.
- **Input hygiene (`RTE-020`, `RTE-021`):** All queries and branch descriptions pass idempotent NFKC/control/special-token sanitization before tokenization.

## 4. Mitigation Plan (`ADR-007`, `ADR-008`)

1. **Runtime parity check (`tools/laya_golden.py` + `tests/fixtures/laya_golden/cases.json`):** `210` golden fixtures spanning buckets `2`, `3-5`, `6-10`, `11-16`, `insufficient_evidence` sink cases, near-ties, and Unicode inputs verify Candle/ORT tensor parity before any runtime backend ships (`WP-M5-02`/`WP-M5-03`).
2. **Post-hoc calibration & conformal abstention (`RTE-062`..`RTE-067`):** Always fit bucket temperature `T_b in [0.5, 5.0]` and split-conformal LAC thresholds pinned to `(router_model_id, embedding_model_id, option_format_version)`.
3. **Deployment gate (`RTE-069`) & fine-tuning:** S1 ships disabled unless RouteBench satisfies Gate G-S1 (`>= +3 pts` top-1 or selective accuracy over baseline, `ECE_15 <= 0.05`, empirical coverage `>= 1 - alpha - 0.02`); subsequent fine-tuning uses GAP-owned synthetic and operator-labelled feedback triples (`routing_feedback`) on top of `laya-base` (`ADR-005`, `ADR-007`).
