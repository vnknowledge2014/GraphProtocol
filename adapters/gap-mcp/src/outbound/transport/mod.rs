//! Transports of the outbound MCP connector (`CMP-010`): newline-delimited stdio and `POST` Streamable HTTP.

use std::future::Future;
use std::time::Duration;

use serde_json::Value;
use tokio_util::sync::CancellationToken;

use super::error::McpError;

pub mod http;
pub mod stdio;

/// MCP protocol version of the stateless era (`2026-07-28`).
pub const VERSION_MODERN: &str = "2026-07-28";
/// MCP protocol version of the stateful legacy era (`2025-11-25`).
pub const VERSION_LEGACY: &str = "2025-11-25";

/// Per-call options.
#[derive(Debug, Clone)]
pub struct CallOptions {
    /// Per-attempt timeout.
    pub timeout: Duration,
    /// Cancelled by the caller (client cancel or deadline).
    pub cancel: CancellationToken,
}

/// Era-dependent hints a transport may turn into headers (ignored by stdio).
#[derive(Debug, Clone, Default)]
pub struct HeaderHints {
    /// Protocol version in use (`MCP-Protocol-Version`); `None` while probing.
    pub protocol_version: Option<String>,
    /// `Mcp-Name` for `tools/call` and `resources/read` (modern era).
    pub mcp_name: Option<String>,
    /// `MCP-Session-Id` (legacy era).
    pub session_id: Option<String>,
}

impl HeaderHints {
    /// `true` when the hints select the stateless `2026-07-28` era.
    #[must_use]
    pub fn is_modern(&self) -> bool {
        self.protocol_version.as_deref() == Some(VERSION_MODERN)
    }
}

/// Successful reply of a request.
#[derive(Debug, Clone, PartialEq)]
pub struct Reply {
    /// The JSON-RPC `result`.
    pub value: Value,
    /// `MCP-Session-Id` the server issued with this reply, if any.
    pub session_id: Option<String>,
}

/// Minimal MCP transport (`CMP-010`). Implementations must be cancel-safe: dropping the future cancels the request.
pub trait McpTransport: Send + Sync + 'static {
    /// Sends a request and waits for its reply, the timeout or the cancellation.
    fn request(
        &self,
        method: &str,
        params: Value,
        hints: HeaderHints,
        opts: CallOptions,
    ) -> impl Future<Output = Result<Reply, McpError>> + Send;

    /// Sends a notification.
    fn notify(
        &self,
        method: &str,
        params: Value,
        hints: HeaderHints,
    ) -> impl Future<Output = Result<(), McpError>> + Send;

    /// Ends the legacy HTTP session (`DELETE`, `CMP-010`); a no-op on stdio and on the stateless era.
    fn close(&self, _hints: HeaderHints) -> impl Future<Output = Result<(), McpError>> + Send {
        std::future::ready(Ok(()))
    }
}
