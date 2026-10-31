//! Configuration of the outbound MCP connector (`CMP-010..015`, `INV-15`).

use std::collections::BTreeSet;
use std::fmt;
use std::time::Duration;

use super::jsonrpc::DEFAULT_MAX_FRAME_BYTES;

/// Admin classification of tools (`ADR-015`): tools named here are `MUTATION`, everything else `QUERY`.
///
/// MCP annotations such as `readOnlyHint` are untrusted hints and never change the kind.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OpClassification {
    /// Original (server-side) tool names that an admin classified as mutations.
    pub mutations: BTreeSet<String>,
}

/// Invalid configuration.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConfigError {
    /// A limit that must be positive is zero.
    #[error("limit `{0}` must be greater than zero")]
    InvalidLimit(&'static str),
    /// The connector id is empty.
    #[error("connector_id must not be empty")]
    EmptyConnectorId,
}

/// Settings of an [`super::McpConnector`].
#[derive(Clone)]
pub struct McpConfig {
    /// Connector id reported in `Describe`.
    pub connector_id: String,
    /// Connector version reported in `Describe`.
    pub connector_version: String,
    /// Client name sent in `clientInfo`.
    pub client_name: String,
    /// Client version sent in `clientInfo`.
    pub client_version: String,
    /// Per-call timeout (`Limits.default_timeout_ms`).
    pub call_timeout: Duration,
    /// Timeout of discovery calls.
    pub discovery_timeout: Duration,
    /// `Limits.max_in_flight`; excess calls fail `RESOURCE_EXHAUSTED` at once (no queueing).
    pub max_in_flight: u32,
    /// Concurrency of the emulated `foreach` (`CMP-015`, default 4).
    pub fanout_concurrency: usize,
    /// `Limits.max_batch_size` of the emulated `foreach` (`CMP-015`, default 64).
    pub max_batch_size: u32,
    /// Cap on one frame (a stdio line, an HTTP body, an SSE event).
    pub max_frame_bytes: usize,
    /// Cap on the pages drained per list call.
    pub max_pages: usize,
    /// Cap on the items collected per list call.
    pub max_items: usize,
    /// `Limits.max_response_bytes`: rows beyond it are cut and the answer is flagged `truncated`.
    pub max_response_bytes: u64,
    /// Admin classification of tools.
    pub classification: OpClassification,
    /// Secret values that must never appear in an error (replaced by `[REDACTED]`).
    pub redact: Vec<String>,
}

impl Default for McpConfig {
    fn default() -> Self {
        Self {
            connector_id: "gap-mcp".to_owned(),
            connector_version: env!("CARGO_PKG_VERSION").to_owned(),
            client_name: "gap-mcp".to_owned(),
            client_version: env!("CARGO_PKG_VERSION").to_owned(),
            call_timeout: Duration::from_secs(30),
            discovery_timeout: Duration::from_secs(10),
            max_in_flight: 64,
            fanout_concurrency: 4,
            max_batch_size: 64,
            max_frame_bytes: DEFAULT_MAX_FRAME_BYTES,
            max_pages: 100,
            max_items: 10_000,
            max_response_bytes: 4 * 1024 * 1024,
            classification: OpClassification::default(),
            redact: Vec::new(),
        }
    }
}

impl fmt::Debug for McpConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("McpConfig")
            .field("connector_id", &self.connector_id)
            .field("call_timeout", &self.call_timeout)
            .field("max_in_flight", &self.max_in_flight)
            .field("fanout_concurrency", &self.fanout_concurrency)
            .field("max_batch_size", &self.max_batch_size)
            .field("classification", &self.classification)
            .field("redact", &format_args!("[{} redacted]", self.redact.len()))
            .finish_non_exhaustive()
    }
}

impl McpConfig {
    /// Checks the invariants the connector relies on.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.connector_id.trim().is_empty() {
            return Err(ConfigError::EmptyConnectorId);
        }
        for (name, ok) in [
            ("max_in_flight", self.max_in_flight > 0),
            ("fanout_concurrency", self.fanout_concurrency > 0),
            ("max_batch_size", self.max_batch_size > 0),
            ("max_frame_bytes", self.max_frame_bytes > 0),
            ("max_pages", self.max_pages > 0),
            ("max_items", self.max_items > 0),
            ("max_response_bytes", self.max_response_bytes > 0),
        ] {
            if !ok {
                return Err(ConfigError::InvalidLimit(name));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // spec: INV-15, CMP-010
    #[test]
    fn config_validates_limits_and_debug_hides_secrets() {
        assert!(McpConfig::default().validate().is_ok());
        for mutate in [
            (|c: &mut McpConfig| c.max_in_flight = 0) as fn(&mut McpConfig),
            |c| c.fanout_concurrency = 0,
            |c| c.max_batch_size = 0,
            |c| c.max_pages = 0,
            |c| c.connector_id = " ".to_owned(),
        ] {
            let mut c = McpConfig::default();
            mutate(&mut c);
            assert!(c.validate().is_err());
        }
        let c = McpConfig {
            redact: vec!["CANARY-secret".to_owned()],
            ..McpConfig::default()
        };
        assert!(!format!("{c:?}").contains("CANARY"));
    }
}
