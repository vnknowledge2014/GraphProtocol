//! Errors of the outbound MCP connector (`CMP-010..013`, `INV-15`).
//!
//! Server-provided text only ever enters an error after [`sanitize_text`] (control characters removed, clipped) and
//! after the connector's own secrets were replaced by [`Redactor`].

use gap_core::error::{NgcErrorCode, NgcErrorInfo};

/// Failure of an MCP exchange.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum McpError {
    /// The server answered with a JSON-RPC error object.
    #[error("MCP server returned JSON-RPC error {code}")]
    Rpc {
        /// JSON-RPC error code.
        code: i64,
        /// Sanitised, clipped server message (untrusted).
        message: String,
    },
    /// The transport closed (EOF or writer gone) before the reply arrived.
    #[error("MCP transport closed")]
    Closed,
    /// The per-call timeout elapsed.
    #[error("MCP request timed out")]
    Timeout,
    /// The caller cancelled the request.
    #[error("MCP request cancelled")]
    Cancelled,
    /// A frame or event exceeded the configured size cap.
    #[error("MCP frame exceeds the size cap")]
    FrameTooLarge,
    /// The peer violated the protocol (malformed frame, missing envelope field, wrong reply).
    #[error("MCP protocol violation: {0}")]
    Protocol(&'static str),
    /// The HTTP server no longer knows the session (`404`, legacy era).
    #[error("MCP HTTP session not found")]
    SessionNotFound,
    /// Unexpected HTTP status.
    #[error("MCP HTTP status {0}")]
    HttpStatus(u16),
    /// Connection establishment failed.
    #[error("MCP connection failed")]
    Connect,
    /// Any other transport failure.
    #[error("MCP transport error")]
    Transport,
}

/// Maximum number of characters of server text kept in an error.
pub const MAX_SERVER_TEXT_CHARS: usize = 200;

/// Removes control characters from `raw` and clips it to `max_chars` characters.
#[must_use]
pub fn sanitize_text(raw: &str, max_chars: usize) -> String {
    raw.chars()
        .filter(|ch| !ch.is_control() || *ch == ' ')
        .take(max_chars)
        .collect()
}

/// Replaces every occurrence of a known secret in server-provided text (`INV-15`).
#[derive(Debug, Clone, Default)]
pub struct Redactor {
    secrets: Vec<String>,
}

impl Redactor {
    /// Creates a redactor for `secrets` (empty strings are ignored).
    #[must_use]
    pub fn new(secrets: impl IntoIterator<Item = String>) -> Self {
        Self {
            secrets: secrets.into_iter().filter(|s| !s.is_empty()).collect(),
        }
    }

    /// Returns `text` with every secret replaced by `[REDACTED]`.
    #[must_use]
    pub fn apply(&self, text: &str) -> String {
        let mut out = text.to_owned();
        for secret in &self.secrets {
            out = out.replace(secret.as_str(), "[REDACTED]");
        }
        out
    }
}

impl McpError {
    /// Maps this error to the NGC error a caller sees; server text is sanitised and redacted (`NGC-061`, `INV-15`).
    #[must_use]
    pub fn into_ngc(self, redactor: &Redactor) -> NgcErrorInfo {
        match self {
            Self::Timeout => NgcErrorInfo::new(
                NgcErrorCode::DeadlineExceeded,
                "upstream MCP request timed out",
            ),
            Self::Cancelled => NgcErrorInfo::new(NgcErrorCode::Cancelled, "call cancelled"),
            Self::Closed | Self::Connect => NgcErrorInfo::new(
                NgcErrorCode::Unavailable,
                "upstream MCP server is unavailable",
            )
            .with_retry(true, 100),
            Self::Rpc { code, message } => {
                let text = redactor.apply(&sanitize_text(&message, MAX_SERVER_TEXT_CHARS));
                NgcErrorInfo::new(
                    NgcErrorCode::UpstreamError,
                    format!("upstream MCP server returned error {code}: {text}"),
                )
            }
            Self::FrameTooLarge => NgcErrorInfo::new(
                NgcErrorCode::UpstreamError,
                "upstream MCP frame exceeds the size cap",
            ),
            Self::Protocol(what) => NgcErrorInfo::new(
                NgcErrorCode::UpstreamError,
                format!("upstream MCP protocol violation: {what}"),
            ),
            Self::SessionNotFound => {
                NgcErrorInfo::new(NgcErrorCode::Unavailable, "upstream MCP session expired")
                    .with_retry(true, 0)
            }
            Self::HttpStatus(status) => NgcErrorInfo::new(
                NgcErrorCode::UpstreamError,
                format!("upstream MCP HTTP status {status}"),
            ),
            Self::Transport => {
                NgcErrorInfo::new(NgcErrorCode::UpstreamError, "upstream MCP transport error")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // spec: INV-15, NGC-061
    #[test]
    fn server_text_is_sanitised_clipped_and_redacted() {
        let canary = "sk-live-CANARY-123";
        let long = format!("boom\u{0}\n\u{1b}[31m {canary} {}", "x".repeat(1_000));
        let err = McpError::Rpc {
            code: -32000,
            message: sanitize_text(&long, MAX_SERVER_TEXT_CHARS),
        };
        let ngc = err.into_ngc(&Redactor::new([canary.to_owned()]));
        assert_eq!(ngc.code, NgcErrorCode::UpstreamError);
        assert!(
            !ngc.message().contains(canary),
            "secret leaked: {}",
            ngc.message()
        );
        assert!(!ngc.message().contains('\u{1b}') && !ngc.message().contains('\n'));
        assert!(ngc.message().chars().count() <= 300);
    }

    // spec: NGC-060
    #[test]
    fn transport_errors_map_to_the_documented_codes() {
        let r = Redactor::default();
        assert_eq!(
            McpError::Timeout.into_ngc(&r).code,
            NgcErrorCode::DeadlineExceeded
        );
        assert_eq!(
            McpError::Cancelled.into_ngc(&r).code,
            NgcErrorCode::Cancelled
        );
        assert_eq!(
            McpError::Closed.into_ngc(&r).code,
            NgcErrorCode::Unavailable
        );
        assert_eq!(
            McpError::Protocol("x").into_ngc(&r).code,
            NgcErrorCode::UpstreamError
        );
        assert_eq!(
            McpError::FrameTooLarge.into_ngc(&r).code,
            NgcErrorCode::UpstreamError
        );
    }
}
