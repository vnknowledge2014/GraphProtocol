//! GAP gateway server library: configuration-driven assembly of execution, transport, and protocol
//! facades (`L5`, `ARC-002`, `ARC-007`, `OPS-001`, `OPS-002`, `RUN-090`, `SEC-004`, `INV-01`, `INV-15`).
//!
//! # Scope in `M1` (`WP-M1-12`)
//!
//! This crate wires:
//! - [`assemble`]: loads `gap.toml` ([`assemble::ServerFile`]), resolves relative OpenAPI documents and
//!   redacted [`SecretRef`](gap_core::config::SecretRef) credentials, derives deterministic UUIDv7
//!   [`BranchUid`](gap_core::ids::BranchUid)s when omitted, starts embedded OpenAPI and MCP connectors
//!   through [`assemble::BranchFactory`], composes the initial immutable [`Snapshot`](gap_core::snapshot::Snapshot),
//!   registers branch clients in [`ConnectorRegistry`](gap_exec::ConnectorRegistry), builds
//!   [`AppState`](gap_transport::AppState), and mounts [`McpFacade`](gap_mcp::inbound::McpFacade) behind
//!   shared authentication and snapshot pinning.
//! - [`run`]: serves HTTP (`/graphql`, `/graphql/stream`, `/mcp`, `/healthz`, `/readyz`, `/metrics`) via
//!   [`run::serve_http`] with two-stage `SIGTERM`/`SIGINT` [`run::Shutdown`] (`RUN-090`), serves MCP over
//!   newline-delimited `stdio` via [`run::serve_stdio`] (`SEC-004`), and configures `stderr`-only structured
//!   logging (`OPS-011`).
//!
//! # Deferred Beyond `M1`
//!
//! The following capabilities are intentionally not wired in `M1` and belong to later work packages:
//! - TLS termination (`[server].tls_cert_secret` / `tls_key_secret`), OAuth 2.1 / JWT / JWKS verification,
//!   and `401 WWW-Authenticate ... resource_metadata` challenges (`CMP-006`, `WP-M6-01`; `M1` supports the
//!   `dev` profile and `blake3` API keys only).
//! - Dedicated `/admin/*` (`[server].admin_listen`) and gRPC gateway (`[server].grpc_listen`) listeners
//!   (`OPS-002`; not served in `M1`).
//! - Model loading, System-1 (`Laya`) discovery routing (`wiring_s1` remains a stub until `WP-M5-03`),
//!   ingestion proposals (`WP-M4-07`), calibration (`WP-M5-04`), audit hash chains (`WP-M6-11`), durable
//!   task storage (`WP-M6-02`), OS process sandboxing (`WP-M6-09`), and Cedar policy evaluation (`WP-M6-05`).
#![forbid(unsafe_code)]
#![deny(
    missing_docs,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::todo,
    clippy::unimplemented
)]

pub mod assemble;
pub mod run;
pub mod wiring_s1;

pub use assemble::{
    AssembleError, Assembled, BranchBuildError, BranchFactory, BranchKeepAlive, BranchRuntime,
    DefaultBranchFactory, ServerConfigError, ServerFile, assemble, assemble_default,
    derive_branch_uid, wrap_connector,
};
pub use run::{RunError, Shutdown, init_tracing, serve_http, serve_stdio};
