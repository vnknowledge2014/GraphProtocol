//! Outbound MCP branch connector (`WP-M1-09`): an MCP server (stdio or Streamable HTTP, stateless `2026-07-28` with a stateful
//! `2025-11-25` fallback) exposed as a Native Graph Connector (`CMP-010..015`).
//!
//! * [`jsonrpc`]: strict, size-capped JSON-RPC framing.
//! * [`transport`]: [`transport::stdio::StdioTransport`] (newline-delimited) and [`transport::http::HttpTransport`] (`POST`, JSON or SSE replies).
//! * [`session`]: era negotiation, `_meta` on every modern request, one re-initialisation on `404`.
//! * [`map`]: tools to operations and SDL, result rows. Annotations are untrusted hints (`ADR-015`).
//! * [`connector`]: [`McpConnector`], generic over the transport; `foreach` is emulated with bounded fan-out and honest `upstream_calls`.
//! * [`launch`]: stdio children start only through an allow-list entry (the sandbox is `WP-M6-09`).
//! * [`fake`]: an in-memory MCP server used by the tests and by the NGC kit test of `gap-conformance`.
//!
//! The decision about the `rmcp` crate is recorded in the handoff: it is not vendored, so this crate is the thin JSON-RPC client the work
//! package allows as the alternative.

pub mod config;
pub mod connector;
pub mod error;
pub mod fake;
pub mod jsonrpc;
pub mod launch;
pub mod map;
pub mod session;
pub mod transport;

pub use config::{ConfigError, McpConfig, OpClassification};
pub use connector::{Discovery, McpConnector};
pub use error::{McpError, Redactor};
pub use launch::{AllowedBinary, LaunchError, LaunchPolicy};
pub use session::{Era, Session};
pub use transport::McpTransport;
pub use transport::http::{HttpTransport, ReqwestPost};
pub use transport::stdio::StdioTransport;
