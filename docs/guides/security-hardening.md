# Security Hardening Guide

Normative specification: [07 — Security, Policy, and Audit](../../spec/07-security.md) (`SEC-001`..`SEC-103`), [09 — Compatibility Facades](../../spec/09-compat.md) (`CMP-006`, `CMP-007`), `INV-04`, `INV-09`, `INV-15`.

## 1. Production Profile Defaults (`OPS-001`)

Always set `profile = "prod"` in production `gap.toml` deployments. In `prod` profile, `gap doctor` and startup config validation enforce:

- `auth.allow_anonymous = false` (`INV-04`, `CMP-006`)
- API keys stored exclusively as 64-character lowercase `BLAKE3` hex digests (`key_hash`), never plaintext secrets
- Explicit `tls_cert` and `tls_key` secrets required fail-closed whenever `server.http3_listen` is configured in `prod`, with TLS 1.3 `0-RTT` (`max_early_data_size = 0`) disabled against replay (`OPS-005`, `ADR-026`)
- TLS / mTLS enabled on downstream production connectors where configured
- Strict SSRF egress filtering denying loopback, RFC 1918 private ranges, link-local metadata endpoints (`169.254.169.254`), and DNS-rebinding redirects
- Fail-closed behavior on policy compile errors or unverified pinned S1/embedding model digests (`SEC-102`)

```bash
gap doctor --json --config gap.toml
```

## 2. Authentication & RFC 9728 OAuth Challenge (`CMP-006`)

HTTP clients present `Authorization: Bearer <token>` on `/graphql`, `/graphql/stream`, and `/mcp`. Unauthenticated requests in `prod` receive `HTTP 401` with `WWW-Authenticate: Bearer resource_metadata=".../.well-known/oauth-protected-resource"`. Upstream connector credentials live only in the GAP vault/config and are NEVER forwarded from the client's incoming bearer token (`INV-15`, Rule 5).

## 3. Deterministic Cedar Policies & Side-Effect Approvals (`ADR-015`, `INV-04`)

Authorization runs deterministically in `gap-sec` before execution planning reaches connectors:

- Read queries require matching tenant + operation scope.
- Field-level `@pii` redaction strips sensitive scalar fields (`customer.email`, `customer.phone`) when the caller lacks PII scopes (`RUN-061`).
- High-impact `@sideEffect` mutations transition through dry-run preview and durable `INPUT_REQUIRED` human approval tasks (`ADR-015`, `ADR-016`, `gap_task(action: "decide")`) rather than executing irreversibly inside an unconfirmed tool call.

Hot-reload and verify Cedar policies without restarting `gap`:

```bash
gap policy check --policy policies.cedar --config gap.toml
```

## 4. Untrusted Data & Rug-Pull Protection (`SEC-020`, `CMP-007`, `ING-070`)

- **Stdio MCP Allow-List (`SEC-072`):** Stdio child-process branches MUST match configured allowed binary paths and environment scrubs; relative `PATH` lookups are rejected in `prod`.
- **Anti-Rug-Pull Gates (`ING-070`):** Downstream connector descriptor changes (new parameters, widened descriptions, escalated `@sideEffect` annotations) do NOT go live silently. They enter deterministic validation gates and human/policy approval proposals (`gap proposals list`) before publishing a new snapshot.
- **`<gap:untrusted>` Envelopes (`CMP-007`):** Outputs from `T2` branches are sanitized and bracketed in `<gap:untrusted source="...">…</gap:untrusted>`.

## 5. Tamper-Evident Audit Hash Chains & Supply Chain (`SEC-092`, `SEC-100`, `SEC-102`)

Every tenant maintains an append-only BLAKE3 hash chain linking `(seq, prev_hash, timestamp_ms, actor, action, outcome)` into `entry_hash`. Periodically verify audit integrity (`AuditVerifyFail` alert triggers on any mismatch):

```bash
gap audit verify --chain system --json
gap audit export --chain system --format json-ld
```

Model artifacts (S1 router, embeddings) MUST carry pinned SHA-256/BLAKE3 hashes verified before loading (`SEC-102`):

```rust
fn check_sha256_pin(expected_hex: &str, actual_hex: &str) -> Result<(), &'static str> {
    if expected_hex.len() != 64 || expected_hex != actual_hex {
        return Err("SEC-102 digest mismatch");
    }
    Ok(())
}

fn main() {
    let valid = "a".repeat(64);
    assert!(check_sha256_pin(&valid, &valid).is_ok());
    assert!(check_sha256_pin(&valid, &"b".repeat(64)).is_err());
}
```
