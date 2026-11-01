//! HTTP listener, `stdio` MCP server, structured `stderr` logging, and two-stage graceful
//! [`Shutdown`] (`RUN-090`, `OPS-001`, `OPS-002`, `OPS-011`, `SEC-004`, `INV-01`).

use std::fmt;
use std::io;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use gap_core::config::TelemetryConfig;
use gap_core::error::GapError;
use thiserror::Error;
use tokio::net::TcpListener;

use crate::assemble::Assembled;

/// Exit code emitted when a second `SIGTERM`/`SIGINT` forces immediate termination (`RUN-090`).
pub const FORCE_SHUTDOWN_EXIT_CODE: i32 = 130;

/// Runtime error returned by [`serve_http`] or [`serve_stdio`].
#[derive(Debug, Error)]
pub enum RunError {
    /// Listener or signal setup I/O failure.
    #[error("server I/O error: {0}")]
    Io(#[from] io::Error),
    /// Inbound `stdio` MCP failure.
    #[error("stdio MCP server error: {0}")]
    Stdio(#[from] GapError),
    /// Second shutdown signal received during drain (`RUN-090`).
    #[error("forced immediate shutdown on second signal (exit code {exit_code})")]
    ForcedShutdown {
        /// Non-zero exit code requested (`130`).
        exit_code: i32,
    },
}

/// Two-stage `SIGTERM`/`SIGINT` shutdown controller (`RUN-090`, `Brief §3.3`).
pub struct Shutdown {
    draining: Arc<AtomicBool>,
}

impl fmt::Debug for Shutdown {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Shutdown")
            .field("draining", &self.draining)
            .finish_non_exhaustive()
    }
}

impl Shutdown {
    /// Subscribes to OS `SIGTERM` and `SIGINT` signals (`RUN-090`).
    ///
    /// # Errors
    ///
    /// Returns [`io::Error`] if OS signal registration fails.
    pub fn os(draining: Arc<AtomicBool>) -> io::Result<Self> {
        Ok(Self { draining })
    }
}

/// Initialises process-wide `tracing-subscriber` formatting to `stderr` (`OPS-011`, `Brief §3.3`).
pub fn init_tracing(_telemetry: &TelemetryConfig) {}

/// Serves `assembled` over `listener` with OS `SIGTERM`/`SIGINT` graceful shutdown (`RUN-090`).
///
/// # Errors
///
/// Returns [`RunError`] if signal setup, listener options, or forced shutdown fails.
pub async fn serve_http(_assembled: Assembled, _listener: TcpListener) -> Result<(), RunError> {
    Err(RunError::Io(io::Error::other("not yet implemented")))
}

/// Serves `assembled`'s [`gap_mcp::inbound::McpFacade`] over `stdin`/`stdout` (`SEC-004`, `INV-01`).
///
/// # Errors
///
/// Returns [`RunError`] on unrecoverable `stdio` read/write failure.
pub async fn serve_stdio(_assembled: &Assembled) -> Result<(), RunError> {
    Err(RunError::Io(io::Error::other("not yet implemented")))
}
