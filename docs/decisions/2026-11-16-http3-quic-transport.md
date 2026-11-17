# ADR-026 Context Memo — Dual-Stack HTTP/3 over QUIC (`RFC 9114`, `OPS-005`)

- **Date:** `2026-11-16`
- **Status:** Accepted (`ADR-026`, supersedes the QUIC deferral clause of `ADR-019`)
- **Work Package:** `WP-M7-12`

## 1. Context

GAP `v0.1.0-rc.1` runs an L4 transport stack (`crates/gap-transport`) supporting HTTP/1.1, HTTP/2, `graphql-sse`, gRPC, and `stdio`. Users deploying GAP across mobile/edge agent clients and multi-hop federated gateways requested native **HTTP/3 over QUIC (`RFC 9114`)** support without sacrificing deterministic security guarantees, wait-free snapshot isolation (`INV-01`/`INV-02`), or zero-ML hot-path purity (`ARC-002`).

Because GAP already enforces clean five-layer crate boundaries (`L0`–`L6`, verified by `tools/check_layers.py`), L4 transport additions require zero changes to `gap-ql` (`L1`), `gap-plan` (`L2`), `gap-snapshot` (`L3`), or `gap-exec` (`L3`).

## 2. Options Considered

1. **External reverse proxy (Nginx/Caddy/Envoy QUIC termination only):**
   - *Pros:* Zero new Rust dependencies.
   - *Cons:* Requires an extra sidecar/daemon even for local single-binary deployments, cannot test `Alt-Svc` + HTTP/3 end-to-end inside `gap-server`, and pushes connection-cap/body-limit accounting across process boundaries.
2. **In-process dual-stack QUIC listener (`quinn 0.11.12` + `h3 0.0.8` + `h3-quinn 0.0.10` with `rustls 0.23.46` `ring` provider) in `gap-transport`:**
   - *Pros:* Pure-Rust/verified cryptography, shares the exact same `axum::Router` and `GatewayPipelineService` as HTTP/1.1 and HTTP/2, preserves `#![forbid(unsafe_code)]` in all workspace L4 crates, preserves single-version `axum`, `hyper`, `prost`, and `tower` pins (`ADR-002`), and ships inside one single binary (`gap serve`).
   - *Cons:* Adds pure-Rust/ring crypto dependencies (verified against `deny.toml` SPDX allow-list) and requires explicit replay/TLS posture controls.

## 3. Decision & Security Controls

Select **Option 2 (in-process dual-stack HTTP/3 over QUIC)**:

1. **Configuration (`[server]` in `gap.toml`):**
   - `server.http3_listen` (`Option<String>`, overridable via `GAP_SERVER__HTTP3_LISTEN`): UDP bind socket address. When `None`, behavior is byte-for-byte unchanged (HTTP/1.1 + HTTP/2 TCP only).
   - `server.tls_self_signed_dev` (`bool`, default `false`): strictly permitted in `profile = "dev"` to generate an ephemeral `rcgen` TLS 1.3 certificate for `localhost` / `127.0.0.1` when `tls_cert_secret` / `tls_key_secret` are omitted.
2. **Fail-closed TLS & Replay Defense (`OPS-005`, `INV-15`):**
   - TLS 1.3 ALPN `h3` is mandatory; `max_early_data_size = 0` disables QUIC `0-RTT` early data so non-idempotent GraphQL mutations and MCP `tools/call` operations can never be replayed.
   - `profile = "prod"` rejects startup with exit code `2` (`ConfigError::Validation`) if `http3_listen` is set without both `tls_cert_secret` and `tls_key_secret`, or if `tls_self_signed_dev = true`.
3. **Resource Bounding & Graceful Shutdown (`RUN-090`, `INV-02`):**
   - QUIC connections are bounded by `ServeOptions::max_connections` (`Arc<Semaphore>` with immediate `incoming.refuse()` on saturation).
   - Incoming HTTP/3 request streams enforce `LimitsConfig::max_request_body_bytes` (returning HTTP `413` before buffering unbounded payloads).
   - Response bodies stream frame-by-frame (`http_body_util::BodyExt::frame`) so `graphql-sse` streams work identically over HTTP/3 and HTTP/2.
   - TCP HTTP responses advertise `Alt-Svc: h3=":<udp_port>"; ma=86400` whenever the HTTP/3 listener is active.
   - Both TCP and QUIC listeners share the two-stage `Shutdown` signal (`SIGTERM`/`SIGINT`) and drain within `server.shutdown_grace_ms`.
