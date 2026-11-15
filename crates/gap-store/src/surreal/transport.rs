//! SurrealDB 3.x configuration, pluggable transport, HTTP `/sql` client, and transactional
//! in-memory SurrealQL engine (`ADR-020`, `ADR-002`).

use std::collections::{BTreeMap, HashMap};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::{Arc, RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::time::Duration;

use gap_core::ids::TaskId;
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;

use crate::error::StoreError;
use crate::tasks::{TaskEventRecord, TaskRecord};
use crate::types::{
    DEFAULT_MAX_EDGES_PER_TENANT, DEFAULT_MAX_NODES_PER_TENANT, EdgeResolution, Scope,
};

/// Configuration for [`crate::surreal::SurrealContextStore`] (`ADR-020`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SurrealConfig {
    /// Optional HTTP endpoint for an external SurrealDB 3.x server (e.g. `http://127.0.0.1:8000`).
    /// When `None`, the backend operates purely on the embedded deterministic engine.
    pub endpoint: Option<String>,
    /// SurrealDB namespace (`NS` header).
    pub namespace: String,
    /// SurrealDB database (`DB` header).
    pub database: String,
    /// Optional HTTP Basic auth username for remote SurrealDB servers.
    pub username: Option<String>,
    /// Optional HTTP Basic auth password for remote SurrealDB servers.
    pub password: Option<String>,
    /// Per-tenant active node ceiling (`ING-064`).
    pub max_nodes_per_tenant: u64,
    /// Per-tenant active edge ceiling (`ING-064`).
    pub max_edges_per_tenant: u64,
    /// TCP connect/read timeout when `endpoint` is configured.
    pub timeout: Duration,
}

impl Default for SurrealConfig {
    fn default() -> Self {
        Self {
            endpoint: None,
            namespace: "gap".to_owned(),
            database: "context".to_owned(),
            username: None,
            password: None,
            max_nodes_per_tenant: DEFAULT_MAX_NODES_PER_TENANT,
            max_edges_per_tenant: DEFAULT_MAX_EDGES_PER_TENANT,
            timeout: Duration::from_secs(5),
        }
    }
}

impl SurrealConfig {
    /// Creates an offline embedded configuration using `gap` / `context`.
    #[must_use]
    pub fn in_memory() -> Self {
        Self::default()
    }

    /// Creates a configuration pointing at an HTTP SurrealDB 3.x `/sql` endpoint.
    #[must_use]
    pub fn remote(endpoint: impl Into<String>) -> Self {
        Self {
            endpoint: Some(endpoint.into()),
            ..Self::default()
        }
    }
}

/// Statement-level transport abstraction for communicating with SurrealDB 3.x (`ADR-020`,
/// `ADR-002`).
///
/// Keeping communication behind a narrow SurrealQL HTTP/RPC trait ensures the workspace does not
/// pull any BSL-1.1 crate into default builds or `Cargo.lock`.
pub trait SurrealTransport: Send + Sync {
    /// Executes one or more SurrealQL statements and returns the parsed JSON response array.
    fn execute_surrealql(&self, surrealql: &str) -> Result<Vec<JsonValue>, StoreError>;
}

/// Stdlib HTTP/1.1 SurrealDB 3.x `/sql` wire transport (`http://host:port`) with Basic auth
/// and bounded timeouts (`ADR-020`).
#[derive(Debug, Clone)]
pub struct HttpSurrealTransport {
    authority: String,
    base_path: String,
    namespace: String,
    database: String,
    auth_header: Option<String>,
    timeout: Duration,
}

impl HttpSurrealTransport {
    /// Parses an `http://host[:port][/prefix]` SurrealDB 3.x endpoint URL.
    pub fn new(config: &SurrealConfig) -> Result<Self, StoreError> {
        let raw = config.endpoint.as_deref().ok_or(StoreError::Invalid {
            field: "endpoint",
            reason: "surreal endpoint must not be empty",
        })?;
        let rest = raw.strip_prefix("http://").ok_or(StoreError::Invalid {
            field: "endpoint",
            reason: "surreal HTTP transport expects an http:// endpoint",
        })?;
        if rest.is_empty() || rest.contains('@') || rest.chars().any(char::is_control) {
            return Err(StoreError::Invalid {
                field: "endpoint",
                reason: "invalid surreal endpoint host",
            });
        }
        let (host_port, base_path) = match rest.split_once('/') {
            Some((h, p)) => (h, format!("/{}", p.trim_matches('/'))),
            None => (rest, String::new()),
        };
        if host_port.is_empty() {
            return Err(StoreError::Invalid {
                field: "endpoint",
                reason: "missing surreal endpoint host",
            });
        }
        let authority = if host_port.contains(':') {
            host_port.to_owned()
        } else {
            format!("{host_port}:8000")
        };
        let auth_header = match (&config.username, &config.password) {
            (Some(u), Some(p)) => Some(format!(
                "Basic {}",
                encode_base64(format!("{u}:{p}").as_bytes())
            )),
            (None, None) => None,
            _ => {
                return Err(StoreError::Invalid {
                    field: "username",
                    reason: "both username and password must be provided together",
                });
            }
        };
        Ok(Self {
            authority,
            base_path,
            namespace: config.namespace.clone(),
            database: config.database.clone(),
            auth_header,
            timeout: config.timeout,
        })
    }

    /// Formats the HTTP/1.1 `POST /sql` request payload without performing network I/O.
    #[must_use]
    pub fn format_http_request(&self, surrealql: &str) -> Vec<u8> {
        let path = if self.base_path.is_empty() || self.base_path == "/" {
            "/sql".to_owned()
        } else {
            format!("{}/sql", self.base_path)
        };
        let mut req = format!(
            "POST {path} HTTP/1.1\r\n\
             Host: {}\r\n\
             Accept: application/json\r\n\
             Content-Type: text/plain; charset=utf-8\r\n\
             NS: {}\r\n\
             DB: {}\r\n\
             Connection: close\r\n\
             Content-Length: {}\r\n",
            self.authority,
            self.namespace,
            self.database,
            surrealql.len(),
        );
        if let Some(ref auth) = self.auth_header {
            req.push_str("Authorization: ");
            req.push_str(auth);
            req.push_str("\r\n");
        }
        req.push_str("\r\n");
        let mut bytes = req.into_bytes();
        bytes.extend_from_slice(surrealql.as_bytes());
        bytes
    }
}

impl SurrealTransport for HttpSurrealTransport {
    fn execute_surrealql(&self, surrealql: &str) -> Result<Vec<JsonValue>, StoreError> {
        let addr = self
            .authority
            .parse::<std::net::SocketAddr>()
            .or_else(|_| {
                use std::net::ToSocketAddrs as _;
                self.authority
                    .to_socket_addrs()
                    .ok()
                    .and_then(|mut it| it.next())
                    .ok_or(())
            })
            .map_err(|()| StoreError::Backend)?;
        let mut stream =
            TcpStream::connect_timeout(&addr, self.timeout).map_err(|_| StoreError::Backend)?;
        let _ = stream.set_read_timeout(Some(self.timeout));
        let _ = stream.set_write_timeout(Some(self.timeout));

        let payload = self.format_http_request(surrealql);
        stream
            .write_all(&payload)
            .map_err(|_| StoreError::Backend)?;
        stream.flush().map_err(|_| StoreError::Backend)?;

        let mut raw = Vec::new();
        stream
            .read_to_end(&mut raw)
            .map_err(|_| StoreError::Backend)?;
        let text = String::from_utf8(raw).map_err(|_| StoreError::Backend)?;
        let (head, body) = text.split_once("\r\n\r\n").ok_or(StoreError::Backend)?;
        let status_code = head
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .and_then(|code| code.parse::<u16>().ok())
            .ok_or(StoreError::Backend)?;
        if !(200..300).contains(&status_code) {
            return Err(StoreError::Backend);
        }
        let parsed: JsonValue =
            serde_json::from_str(body.trim()).map_err(|_| StoreError::Corrupt)?;
        match parsed {
            JsonValue::Array(arr) => Ok(arr),
            other => Ok(vec![other]),
        }
    }
}

fn encode_base64(input: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = chunk[0];
        let b1 = if chunk.len() > 1 { chunk[1] } else { 0 };
        let b2 = if chunk.len() > 2 { chunk[2] } else { 0 };
        out.push(TABLE[usize::from(b0 >> 2)] as char);
        out.push(TABLE[usize::from(((b0 & 0x03) << 4) | (b1 >> 4))] as char);
        out.push(if chunk.len() > 1 {
            TABLE[usize::from(((b1 & 0x0f) << 2) | (b2 >> 6))] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[usize::from(b2 & 0x3f)] as char
        } else {
            '='
        });
    }
    out
}

/// Stored bi-temporal node version inside the SurrealDB state model.
#[derive(Debug, Clone)]
pub(crate) struct StoredNodeVersion {
    pub id: String,
    pub node_type: String,
    pub label: String,
    pub summary: Option<String>,
    pub props_json: String,
    pub scope: Scope,
    pub base_scope: Scope,
    pub owner_principal: String,
    pub task_id: Option<TaskId>,
    pub group_name: Option<String>,
    pub expires_at_us: Option<i64>,
    pub prov_source: String,
    pub prov_desc_hash: Option<String>,
    pub prov_confidence: Option<f32>,
    pub prov_extracted_at_us: Option<i64>,
    pub prov_agent: Option<String>,
    pub valid_from_us: i64,
    pub valid_to_us: Option<i64>,
    pub content_hash: [u8; 32],
}

/// Stored bi-temporal edge version inside the SurrealDB state model.
#[derive(Debug, Clone)]
pub(crate) struct StoredEdgeVersion {
    pub id: String,
    pub edge_type: String,
    pub from_id: String,
    pub to_id: String,
    pub props_json: String,
    pub confidence: Option<f32>,
    pub resolution: EdgeResolution,
    pub prov_source: String,
    pub prov_desc_hash: Option<String>,
    pub prov_confidence: Option<f32>,
    pub prov_extracted_at_us: Option<i64>,
    pub prov_agent: Option<String>,
    pub valid_from_us: i64,
    pub valid_to_us: Option<i64>,
    pub content_hash: [u8; 32],
}

/// Per-tenant active counters (`tenant_stats` table).
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct TenantStats {
    pub active_nodes: u64,
    pub active_edges: u64,
}

/// Per-tenant task row matching `tasks` table.
#[derive(Debug, Clone)]
pub(crate) struct StoredTask {
    pub record: TaskRecord,
    pub last_seq: u64,
}

/// Full tenant-partitioned Surreal state snapshot (allows deterministic atomic batch rollback).
#[derive(Debug, Clone, Default)]
pub(crate) struct EngineState {
    pub nodes_by_tenant: BTreeMap<String, Vec<StoredNodeVersion>>,
    pub edges_by_tenant: BTreeMap<String, Vec<StoredEdgeVersion>>,
    pub stats_by_tenant: BTreeMap<String, TenantStats>,
    pub tasks_by_tenant: BTreeMap<String, HashMap<TaskId, StoredTask>>,
    pub task_events_by_tenant: BTreeMap<String, HashMap<TaskId, Vec<TaskEventRecord>>>,
    pub executed_ddl: Vec<String>,
}

/// Deterministic in-memory SurrealQL 3.x engine (`ADR-020`) supporting atomic transactions
/// across nodes, edges, tenant stats, and durable tasks, with optional audit capture of emitted
/// SurrealQL schema statements.
#[derive(Debug, Default)]
pub struct InMemorySurrealEngine {
    inner: RwLock<EngineState>,
}

impl InMemorySurrealEngine {
    /// Creates an empty transactional Surreal engine.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub(crate) fn read(&self) -> Result<RwLockReadGuard<'_, EngineState>, StoreError> {
        self.inner.read().map_err(|_| StoreError::Backend)
    }

    pub(crate) fn write(&self) -> Result<RwLockWriteGuard<'_, EngineState>, StoreError> {
        self.inner.write().map_err(|_| StoreError::Backend)
    }

    /// Returns recorded DDL statements executed against this engine instance.
    pub fn recorded_ddl(&self) -> Result<Vec<String>, StoreError> {
        let guard = self.read()?;
        Ok(guard.executed_ddl.clone())
    }
}

impl SurrealTransport for InMemorySurrealEngine {
    fn execute_surrealql(&self, surrealql: &str) -> Result<Vec<JsonValue>, StoreError> {
        let mut guard = self.write()?;
        guard.executed_ddl.push(surrealql.to_owned());
        Ok(vec![serde_json::json!({ "status": "OK", "result": null })])
    }
}

/// Shared handle to an [`InMemorySurrealEngine`].
pub(crate) type SharedEngine = Arc<InMemorySurrealEngine>;
