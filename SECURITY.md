# Security Policy

## Supported Versions

GAP is currently in pre-1.0 development (`0.1.0-draft`, wire string `gap/0.1`). Security fixes are applied to the `main` branch and included in the next release cut.

| Version | Supported |
|---|---|
| `main` (`0.1.0-draft`) | Yes |
| Older pre-release snapshots | No |

## Reporting a Vulnerability

**Do NOT open public GitHub issues, pull requests, or discussions containing exploit details or unpatched security findings.**

Please report suspected vulnerabilities privately through **GitHub Private Vulnerability Reporting** (the repository's *Security -> Report a vulnerability* advisory form). If private advisory submission is unavailable in your environment, contact the repository maintainers directly through a private channel before disclosing any details publicly.

Include as much of the following information as possible:

1. **Affected component(s):** crate, adapter, protocol surface (Gateway HTTP/gRPC, NGC, MCP facade, A2A binding, CLI, ingestion pipeline), and commit hash.
2. **Security invariant or threat category:** relevant section of `spec/07-security.md` or architecture invariant if known (for example, `INV-04`, `INV-07`, `INV-09`, `INV-15`, `ADR-013`, `ADR-014`).
3. **Minimal reproduction:** configuration, request/connector payload, and steps required to trigger the issue without destructive side effects.
4. **Impact assessment:** what trust boundary is crossed (for example, authorization bypass, cross-tenant read/write, T2 sandbox escape, SSRF guard bypass, description-pinning/drift bypass, secret exposure in logs/traces/snapshots, or untrusted branch output escalating into control/execution decisions).

## Response Process

- **Acknowledgement:** within **72 hours** of receipt.
- **Triage & severity assessment:** within **7 calendar days**, confirming reproduction, affected versions, and remediation plan.
- **Coordinated disclosure:** once a fix and regression test are merged and released, we publish a security advisory crediting the reporter (unless anonymity is requested).

## High-Priority Security Boundaries

Reports demonstrating a bypass of any of the following GAP guarantees are always in scope:

- Deterministic policy enforcement with default-deny semantics (`INV-04`)
- Trust-tier quarantine and process sandboxing for untrusted (`T2`) or CLI branches (`INV-07`, `ADR-013`, `ADR-014`)
- Strict tenant isolation across caches, stores, telemetry, and audit logs (`INV-09`)
- Secret redaction across snapshots, logs, traces, metrics, errors, and model inputs (`INV-15`)
- Outbound SSRF protection, description hash pinning (`desc_hash`), and prompt-injection containment (`ADR-014`)
