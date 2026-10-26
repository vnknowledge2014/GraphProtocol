# GAP — Graph Agent Protocol

GAP (`gap/0.1`, `ADR-024`) is a typed, GraphQL-profile protocol (`ADR-001`, `GQL-001`) that unifies tools, heterogeneous data sources, code/document context graphs, and agent-to-agent tasks behind progressive-disclosure discovery (`ADR-010`), batched federation (`INV-18`), deterministic policy authorization (`INV-04`), and hot-swappable immutable snapshots (`ADR-009`, `INV-01`).

## Document Set Structure

1. **Normative Specification (`spec/`):**
   - [00 — Conventions](../../spec/00-conventions.md) through [09 — Compatibility Facades and Reference Connectors](../../spec/09-compat.md).
2. **Architecture, Verification, and Governance (`docs/`):**
   - [00 — Design Review](../../docs/00-REVIEW.md)
   - [01 — Architecture Decision Records](../../docs/01-DECISIONS.md) (`ADR-001`–`ADR-025`)
   - [02 — Implementation Plan](../../docs/02-PLAN.md)
   - [03 — Verification Strategy and Release Gates](../../docs/03-VERIFICATION.md)
   - [Agent Guide](../../docs/AGENT-GUIDE.md)
   - [Release Engineering and Error-Budget Policy](../../docs/RELEASE.md) (`OPS-040`, `OPS-041`)
   - [Unsafe Code Ledger](../../docs/UNSAFE.md) (`ADR-002`, `ARC-003`)
3. **Host Integration & Operations (`docs/hosts/`, `docs/runbooks/`):**
   - [MCP Host Integration Guides](../../docs/hosts/README.md) (`CMP-009`, `ADR-017`)
   - [Operational Runbooks Index](../../docs/runbooks/README.md) ([08-operations.md](../../spec/08-operations.md) §6)

> [!NOTE]
> Narrative guides (5-minute MCP quickstart, concepts, connector authoring, security hardening, operations, and migration guides) and automated `mdbook test` verification are completed in `WP-M7-03` ([08-operations.md](../../spec/08-operations.md) §9).
