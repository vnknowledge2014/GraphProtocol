//! SQLite [`crate::ContextStore`] and [`crate::ContextMaintenance`] backend (`ADR-020`, `ADR-025`, `ING-064`..`ING-067`).

pub(crate) mod crud;
pub mod schema;
pub(crate) mod search;
pub(crate) mod traverse;

use std::fs;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::Duration;

use gap_core::clock::{Clock, Timestamp};
use gap_core::ids::TenantScope;
use rusqlite::limits::Limit;
use rusqlite::{Connection, ErrorCode as SqliteErrorCode, OpenFlags};
use uuid::{Builder as UuidBuilder, Uuid};

use crate::error::StoreError;
pub use crate::sqlite::schema::{FORMAT_VERSION, Migration};
use crate::store::ContextStore;
use crate::types::{
    BatchResult, Caller, DEFAULT_MAX_EDGES_PER_TENANT, DEFAULT_MAX_NODES_PER_TENANT, Edge, EdgeId,
    GraphCounts, NewEdge, NewNode, Node, NodeId, TraverseQuery, TraverseResult,
};

/// Default SQLite `busy_timeout` in milliseconds (`m2-01-design.md` §8).
pub const DEFAULT_BUSY_TIMEOUT_MS: u32 = 5_000;

/// Default read-only connection pool size (`m2-01-design.md` §8).
pub const DEFAULT_READER_POOL_SIZE: usize = 4;

/// Default `SQLITE_LIMIT_LENGTH` cap in bytes (`16 MiB`, `m2-01-design.md` §8).
pub const DEFAULT_MAX_LENGTH_BYTES: i32 = 16 * 1024 * 1024;

/// Default `SQLITE_LIMIT_SQL_LENGTH` cap in bytes (`1 MiB`, `m2-01-design.md` §8).
pub const DEFAULT_MAX_SQL_LENGTH_BYTES: i32 = 1_048_576;

/// Default `SQLITE_LIMIT_VARIABLE_NUMBER` cap (`m2-01-design.md` §8).
pub const DEFAULT_MAX_VARIABLE_NUMBER: i32 = 32_766;

const SQLITE_MAGIC_HEADER: &[u8; 16] = b"SQLite format 3\0";

/// WAL `PRAGMA synchronous` durability mode (`m2-01-design.md` §8).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Durability {
    /// `PRAGMA synchronous = FULL` (default).
    #[default]
    Full,
    /// `PRAGMA synchronous = NORMAL`.
    Normal,
    /// `PRAGMA synchronous = OFF` (test/ephemeral benches only).
    Off,
}

impl Durability {
    const fn pragma_sql(self) -> &'static str {
        match self {
            Self::Full => "PRAGMA synchronous = FULL;",
            Self::Normal => "PRAGMA synchronous = NORMAL;",
            Self::Off => "PRAGMA synchronous = OFF;",
        }
    }
}

/// Configuration for [`SqliteContextStore`] (`ING-064`, `m2-01-design.md` §8).
#[derive(Debug, Clone)]
pub struct SqliteConfig {
    /// Filesystem path of the SQLite database file.
    pub path: PathBuf,
    /// WAL `synchronous` setting (default [`Durability::Full`]).
    pub durability: Durability,
    /// Number of read-only connections in the reader pool (default `4`).
    pub reader_pool_size: NonZeroUsize,
    /// Per-connection `busy_timeout` in milliseconds (default `5,000`).
    pub busy_timeout_ms: u32,
    /// Per-tenant active node ceiling (default `5,000,000`, `ING-064`).
    pub max_nodes_per_tenant: u64,
    /// Per-tenant active edge ceiling (default `20,000,000`, `ING-064`).
    pub max_edges_per_tenant: u64,
    /// `SQLITE_LIMIT_LENGTH` cap in bytes.
    pub max_length_bytes: i32,
    /// `SQLITE_LIMIT_SQL_LENGTH` cap in bytes.
    pub max_sql_length_bytes: i32,
    /// `SQLITE_LIMIT_VARIABLE_NUMBER` cap.
    pub max_variable_number: i32,
}

impl SqliteConfig {
    /// Creates a [`SqliteConfig`] for `path` with spec defaults.
    #[must_use]
    pub fn new(path: impl Into<PathBuf>) -> Self {
        let pool_size = match NonZeroUsize::new(DEFAULT_READER_POOL_SIZE) {
            Some(nz) => nz,
            None => NonZeroUsize::MIN,
        };
        Self {
            path: path.into(),
            durability: Durability::Full,
            reader_pool_size: pool_size,
            busy_timeout_ms: DEFAULT_BUSY_TIMEOUT_MS,
            max_nodes_per_tenant: DEFAULT_MAX_NODES_PER_TENANT,
            max_edges_per_tenant: DEFAULT_MAX_EDGES_PER_TENANT,
            max_length_bytes: DEFAULT_MAX_LENGTH_BYTES,
            max_sql_length_bytes: DEFAULT_MAX_SQL_LENGTH_BYTES,
            max_variable_number: DEFAULT_MAX_VARIABLE_NUMBER,
        }
    }
}

pub(crate) fn lock_unpoisoned<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// Maps a [`rusqlite::Error`] into a wire-safe [`StoreError`] without leaking SQL text or paths (`INV-15`).
pub(crate) fn map_sqlite_err(err: rusqlite::Error) -> StoreError {
    if let Some(code) = err.sqlite_error_code() {
        match code {
            SqliteErrorCode::DatabaseBusy | SqliteErrorCode::DatabaseLocked => {
                return StoreError::Busy;
            }
            SqliteErrorCode::DatabaseCorrupt | SqliteErrorCode::NotADatabase => {
                return StoreError::Corrupt;
            }
            SqliteErrorCode::ConstraintViolation => {
                return StoreError::Conflict;
            }
            other => {
                tracing::warn!(sqlite_code = ?other, "sqlite engine error");
                return StoreError::Backend;
            }
        }
    }
    tracing::warn!("sqlite driver error without extended code");
    StoreError::Backend
}

pub(crate) fn apply_connection_limits(
    conn: &Connection,
    config: &SqliteConfig,
) -> Result<(), StoreError> {
    if config.max_length_bytes <= 0
        || config.max_sql_length_bytes <= 0
        || config.max_variable_number <= 0
    {
        return Err(StoreError::Invalid {
            field: "sqlite_limits",
            reason: "connection limits must be positive",
        });
    }
    conn.set_limit(Limit::SQLITE_LIMIT_LENGTH, config.max_length_bytes)
        .map_err(map_sqlite_err)?;
    conn.set_limit(Limit::SQLITE_LIMIT_SQL_LENGTH, config.max_sql_length_bytes)
        .map_err(map_sqlite_err)?;
    conn.set_limit(
        Limit::SQLITE_LIMIT_VARIABLE_NUMBER,
        config.max_variable_number,
    )
    .map_err(map_sqlite_err)?;
    Ok(())
}

fn configure_writer_connection(conn: &Connection, config: &SqliteConfig) -> Result<(), StoreError> {
    conn.busy_timeout(Duration::from_millis(u64::from(config.busy_timeout_ms)))
        .map_err(map_sqlite_err)?;
    apply_connection_limits(conn, config)?;
    conn.execute_batch(
        "PRAGMA trusted_schema = OFF;
         PRAGMA cell_size_check = ON;
         PRAGMA foreign_keys = ON;",
    )
    .map_err(map_sqlite_err)?;
    let mode: String = conn
        .query_row("PRAGMA journal_mode = WAL;", [], |row| row.get(0))
        .map_err(map_sqlite_err)?;
    if !mode.eq_ignore_ascii_case("wal") {
        tracing::warn!(journal_mode = %mode, "sqlite failed to enter WAL mode");
        return Err(StoreError::Backend);
    }
    conn.execute_batch(config.durability.pragma_sql())
        .map_err(map_sqlite_err)?;
    Ok(())
}

pub(crate) fn open_configured_reader(config: &SqliteConfig) -> Result<Connection, StoreError> {
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let conn = Connection::open_with_flags(&config.path, flags).map_err(map_sqlite_err)?;
    conn.busy_timeout(Duration::from_millis(u64::from(config.busy_timeout_ms)))
        .map_err(map_sqlite_err)?;
    apply_connection_limits(&conn, config)?;
    conn.execute_batch(
        "PRAGMA trusted_schema = OFF;
         PRAGMA cell_size_check = ON;
         PRAGMA foreign_keys = ON;
         PRAGMA query_only = ON;",
    )
    .map_err(map_sqlite_err)?;
    Ok(conn)
}

/// Probes an existing non-empty file read-only so `user_version > FORMAT_VERSION` or non-SQLite
/// files fail *before* any write handle or WAL header mutation occurs (`m2-01-design.md` §8).
fn probe_existing_file_readonly(path: &Path) -> Result<(), StoreError> {
    let Ok(meta) = fs::metadata(path) else {
        return Ok(());
    };
    if meta.len() == 0 {
        return Ok(());
    }
    let header = fs::read(path).map_err(|_| StoreError::Backend)?;
    if header.len() < SQLITE_MAGIC_HEADER.len() || &header[..16] != SQLITE_MAGIC_HEADER {
        return Err(StoreError::Corrupt);
    }
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let conn = Connection::open_with_flags(path, flags).map_err(map_sqlite_err)?;
    conn.execute_batch(
        "PRAGMA trusted_schema = OFF;
         PRAGMA cell_size_check = ON;",
    )
    .map_err(map_sqlite_err)?;
    let found = schema::read_user_version(&conn)?;
    if found > FORMAT_VERSION {
        return Err(StoreError::FormatTooNew {
            found,
            supported: FORMAT_VERSION,
        });
    }
    Ok(())
}

pub(crate) struct ReaderPool {
    conns: Mutex<Vec<Connection>>,
    available: Condvar,
}

impl ReaderPool {
    fn new(conns: Vec<Connection>) -> Self {
        Self {
            conns: Mutex::new(conns),
            available: Condvar::new(),
        }
    }

    pub(crate) fn checkout(&self) -> ReaderGuard<'_> {
        let mut guard = lock_unpoisoned(&self.conns);
        loop {
            if let Some(conn) = guard.pop() {
                return ReaderGuard {
                    pool: self,
                    conn: Some(conn),
                };
            }
            guard = match self.available.wait(guard) {
                Ok(g) => g,
                Err(poisoned) => poisoned.into_inner(),
            };
        }
    }
}

/// RAII checkout guard returning a read-only [`Connection`] to [`ReaderPool`] on drop.
pub(crate) struct ReaderGuard<'a> {
    pool: &'a ReaderPool,
    conn: Option<Connection>,
}

impl ReaderGuard<'_> {
    pub(crate) fn conn(&self) -> Result<&Connection, StoreError> {
        self.conn.as_ref().ok_or(StoreError::Backend)
    }
}

impl Drop for ReaderGuard<'_> {
    fn drop(&mut self) {
        if let Some(conn) = self.conn.take() {
            let mut guard = lock_unpoisoned(&self.pool.conns);
            guard.push(conn);
            drop(guard);
            self.pool.available.notify_one();
        }
    }
}

/// SQLite-backed [`crate::ContextStore`] and [`crate::ContextMaintenance`] (`ADR-020`, `ADR-025`).
pub struct SqliteContextStore {
    pub(crate) config: SqliteConfig,
    pub(crate) clock: Arc<dyn Clock>,
    pub(crate) writer: Mutex<Connection>,
    pub(crate) readers: ReaderPool,
    uuid_seq: AtomicU64,
}

impl std::fmt::Debug for SqliteContextStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SqliteContextStore")
            .field("durability", &self.config.durability)
            .field("reader_pool_size", &self.config.reader_pool_size)
            .field("max_nodes_per_tenant", &self.config.max_nodes_per_tenant)
            .field("max_edges_per_tenant", &self.config.max_edges_per_tenant)
            .finish_non_exhaustive()
    }
}

impl SqliteContextStore {
    /// Opens or creates a WAL-enabled SQLite context store at `config.path`, applying migrations
    /// up to [`FORMAT_VERSION`].
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::FormatTooNew`] without modifying the database file if its
    /// `PRAGMA user_version` exceeds [`FORMAT_VERSION`].
    pub fn open(config: SqliteConfig, clock: Arc<dyn Clock>) -> Result<Self, StoreError> {
        if let Some(parent) = config.path.parent()
            && !parent.as_os_str().is_empty()
            && !parent.exists()
        {
            fs::create_dir_all(parent).map_err(|_| StoreError::Backend)?;
        }

        probe_existing_file_readonly(&config.path)?;

        let writer_flags = OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_CREATE
            | OpenFlags::SQLITE_OPEN_NO_MUTEX;
        let mut writer =
            Connection::open_with_flags(&config.path, writer_flags).map_err(map_sqlite_err)?;
        configure_writer_connection(&writer, &config)?;
        schema::apply_migrations(&mut writer)?;
        schema::verify_required_tables(&writer)?;

        let mut reader_conns = Vec::with_capacity(config.reader_pool_size.get());
        for _ in 0..config.reader_pool_size.get() {
            reader_conns.push(open_configured_reader(&config)?);
        }

        Ok(Self {
            config,
            clock,
            writer: Mutex::new(writer),
            readers: ReaderPool::new(reader_conns),
            uuid_seq: AtomicU64::new(0),
        })
    }

    /// Returns a reference to the injected [`Clock`].
    #[must_use]
    pub fn clock(&self) -> &Arc<dyn Clock> {
        &self.clock
    }

    /// Reads the active on-disk `PRAGMA user_version` through the read-only pool.
    pub fn format_version(&self) -> Result<u32, StoreError> {
        let guard = self.readers.checkout();
        schema::read_user_version(guard.conn()?)
    }

    /// Runs a non-blocking `PRAGMA wal_checkpoint(PASSIVE)` on the writer connection.
    pub fn wal_checkpoint_passive(&self) -> Result<(), StoreError> {
        let writer = lock_unpoisoned(&self.writer);
        writer
            .execute_batch("PRAGMA wal_checkpoint(PASSIVE);")
            .map_err(map_sqlite_err)
    }

    /// Mints an RFC 9562 UUIDv7 using the injected [`Clock`] (`INV-02`, never `SystemTime::now`)
    /// plus a monotonic counter and CSPRNG entropy.
    pub(crate) fn mint_uuid_v7(&self) -> Uuid {
        let unix_ms = u64::try_from(self.clock.unix_millis().max(0)).unwrap_or(0);
        let seq = self.uuid_seq.fetch_add(1, Ordering::Relaxed);
        let entropy = *Uuid::new_v4().as_bytes();
        let mut rand_bytes = [0u8; 10];
        let seq_hi = ((seq >> 8) as u8) & 0x0f;
        let seq_lo = (seq & 0xff) as u8;
        rand_bytes[0] = (entropy[0] & 0xf0) | seq_hi;
        rand_bytes[1] = seq_lo;
        rand_bytes[2..10].copy_from_slice(&entropy[2..10]);
        UuidBuilder::from_unix_timestamp_millis(unix_ms, &rand_bytes).into_uuid()
    }
}

impl ContextStore for SqliteContextStore {
    fn put_batch(
        &self,
        tenant: &TenantScope,
        writer: &Caller,
        nodes: &[NewNode],
        edges: &[NewEdge],
    ) -> Result<BatchResult, StoreError> {
        self.exec_put_batch(tenant, writer, nodes, edges)
    }

    fn get_node(
        &self,
        tenant: &TenantScope,
        caller: &Caller,
        id: &NodeId,
        as_of: Option<Timestamp>,
    ) -> Result<Option<Node>, StoreError> {
        self.exec_get_node(tenant, caller, id, as_of)
    }

    fn get_edge(
        &self,
        tenant: &TenantScope,
        caller: &Caller,
        id: &EdgeId,
        as_of: Option<Timestamp>,
    ) -> Result<Option<Edge>, StoreError> {
        self.exec_get_edge(tenant, caller, id, as_of)
    }

    fn tombstone_node(
        &self,
        tenant: &TenantScope,
        writer: &Caller,
        id: &NodeId,
    ) -> Result<(), StoreError> {
        self.exec_tombstone_node(tenant, writer, id)
    }

    fn delete_node(
        &self,
        tenant: &TenantScope,
        writer: &Caller,
        id: &NodeId,
    ) -> Result<(), StoreError> {
        self.exec_delete_node(tenant, writer, id)
    }

    fn counts(
        &self,
        tenant: &TenantScope,
        caller: &Caller,
        as_of: Option<Timestamp>,
    ) -> Result<GraphCounts, StoreError> {
        self.exec_counts(tenant, caller, as_of)
    }

    fn traverse(
        &self,
        tenant: &TenantScope,
        caller: &Caller,
        query: &TraverseQuery,
    ) -> Result<TraverseResult, StoreError> {
        self.exec_traverse(tenant, caller, query)
    }

    fn search_nodes(
        &self,
        tenant: &TenantScope,
        caller: &Caller,
        query: &str,
        limit: u32,
        as_of: Option<Timestamp>,
    ) -> Result<Vec<Node>, StoreError> {
        self.exec_search_nodes(tenant, caller, query, limit, as_of)
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::num::NonZeroUsize;
    use std::sync::{Arc, Barrier};
    use std::thread;

    use gap_core::clock::ManualClock;
    use rusqlite::limits::Limit;
    use rusqlite::{Connection, TransactionBehavior, params};
    use tempfile::TempDir;

    use super::*;

    // spec: ING-064, INV-02 (ADR-020)
    #[test]
    fn fresh_and_version_0_databases_migrate_to_format_version() {
        let dir = TempDir::new().expect("tempdir");
        let clock: Arc<dyn Clock> = Arc::new(ManualClock::new(1_700_000_000_000));

        // 1. Fresh non-existent file.
        let fresh_path = dir.path().join("fresh.db");
        let store =
            SqliteContextStore::open(SqliteConfig::new(&fresh_path), Arc::clone(&clock))
                .expect("open fresh");
        {
            let reader = store.readers.checkout();
            assert_eq!(
                schema::read_user_version(reader.conn().expect("conn")).expect("ver"),
                FORMAT_VERSION
            );
        }
        drop(store);

        // 2. Existing version-0 SQLite database created via raw SQL.
        let v0_path = dir.path().join("v0.db");
        {
            let raw = Connection::open(&v0_path).expect("raw v0");
            raw.execute_batch(
                "PRAGMA journal_mode = DELETE;
                 PRAGMA user_version = 0;
                 CREATE TABLE bootstrap_marker (k TEXT PRIMARY KEY);
                 DROP TABLE bootstrap_marker;",
            )
            .expect("init v0");
        }
        let store_v0 = SqliteContextStore::open(SqliteConfig::new(&v0_path), clock)
            .expect("upgrade v0 -> FORMAT_VERSION");
        let reader = store_v0.readers.checkout();
        assert_eq!(
            schema::read_user_version(reader.conn().expect("conn")).expect("ver"),
            FORMAT_VERSION
        );
    }

    // spec: ING-064, ING-062 (ADR-020)
    #[test]
    fn previous_version_v1_raw_sql_database_upgrades_and_preserves_existing_rows() {
        let dir = TempDir::new().expect("tempdir");
        let v1_path = dir.path().join("v1.db");
        {
            let raw = Connection::open(&v1_path).expect("open raw v1");
            raw.execute_batch(
                "CREATE TABLE tenant_stats (
                    tenant TEXT NOT NULL,
                    node_count INTEGER NOT NULL DEFAULT 0,
                    edge_count INTEGER NOT NULL DEFAULT 0,
                    PRIMARY KEY (tenant)
                ) WITHOUT ROWID;
                CREATE TABLE nodes (
                    fts_rowid INTEGER PRIMARY KEY,
                    tenant TEXT NOT NULL,
                    id TEXT NOT NULL,
                    node_type TEXT NOT NULL,
                    label TEXT NOT NULL,
                    summary TEXT,
                    props_json TEXT NOT NULL,
                    scope TEXT NOT NULL,
                    base_scope TEXT NOT NULL,
                    owner_principal TEXT NOT NULL,
                    task_id TEXT,
                    group_name TEXT,
                    expires_at INTEGER,
                    prov_source TEXT NOT NULL,
                    prov_desc_hash TEXT,
                    prov_confidence REAL,
                    prov_extracted_at INTEGER,
                    prov_agent TEXT,
                    valid_from INTEGER NOT NULL,
                    valid_to INTEGER,
                    content_hash BLOB NOT NULL
                );
                CREATE UNIQUE INDEX idx_nodes_current ON nodes (tenant, id) WHERE valid_to IS NULL;
                CREATE INDEX idx_nodes_temporal ON nodes (tenant, id, valid_from, valid_to);
                CREATE TABLE edges (
                    edge_rowid INTEGER PRIMARY KEY,
                    tenant TEXT NOT NULL,
                    id TEXT NOT NULL,
                    edge_type TEXT NOT NULL,
                    from_id TEXT NOT NULL,
                    to_id TEXT NOT NULL,
                    props_json TEXT NOT NULL,
                    confidence REAL,
                    resolution TEXT NOT NULL,
                    prov_source TEXT NOT NULL,
                    prov_desc_hash TEXT,
                    prov_confidence REAL,
                    prov_extracted_at INTEGER,
                    prov_agent TEXT,
                    valid_from INTEGER NOT NULL,
                    valid_to INTEGER,
                    content_hash BLOB NOT NULL
                );
                CREATE UNIQUE INDEX idx_edges_current_quad ON edges (tenant, edge_type, from_id, to_id) WHERE valid_to IS NULL;
                CREATE UNIQUE INDEX idx_edges_current_id ON edges (tenant, id) WHERE valid_to IS NULL;
                CREATE INDEX idx_edges_from ON edges (tenant, from_id, valid_from, valid_to);
                CREATE INDEX idx_edges_to ON edges (tenant, to_id, valid_from, valid_to);
                CREATE INDEX idx_edges_id_temporal ON edges (tenant, id, valid_from, valid_to);
                CREATE VIRTUAL TABLE nodes_fts USING fts5(tenant UNINDEXED, label, summary);
                PRAGMA user_version = 1;",
            )
            .expect("apply raw v1 ddl");
            raw.execute(
                "INSERT INTO tenant_stats (tenant, node_count, edge_count) VALUES (?1, 1, 0)",
                params!["acme"],
            )
            .expect("insert stats");
            raw.execute(
                "INSERT INTO nodes (
                    fts_rowid, tenant, id, node_type, label, summary, props_json,
                    scope, base_scope, owner_principal, task_id, group_name, expires_at,
                    prov_source, prov_desc_hash, prov_confidence, prov_extracted_at, prov_agent,
                    valid_from, valid_to, content_hash
                ) VALUES (
                    1, 'acme', 'file:README.md', 'Doc', 'README', 'overview', '{}',
                    'GLOBAL', 'GLOBAL', 'ingester', NULL, NULL, NULL,
                    'git', NULL, NULL, NULL, NULL,
                    1700000000000000, NULL, X'01020304'
                )",
                [],
            )
            .expect("insert v1 node");
            raw.execute(
                "INSERT INTO nodes_fts (rowid, tenant, label, summary) VALUES (1, 'acme', 'README', 'overview')",
                [],
            )
            .expect("insert v1 fts");
        }

        let clock: Arc<dyn Clock> = Arc::new(ManualClock::new(1_700_000_000_000));
        let store =
            SqliteContextStore::open(SqliteConfig::new(&v1_path), clock).expect("migrate v1 -> v2");
        let reader_guard = store.readers.checkout();
        let reader = reader_guard.conn().expect("conn");
        assert_eq!(schema::read_user_version(reader).expect("ver"), 2);

        let preserved_label: String = reader
            .query_row(
                "SELECT label FROM nodes WHERE tenant = 'acme' AND id = 'file:README.md'",
                [],
                |row| row.get(0),
            )
            .expect("preserved row");
        assert_eq!(preserved_label, "README");

        let v2_idx_count: i64 = reader
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'index' AND name IN ('idx_nodes_task', 'idx_nodes_base_task', 'idx_nodes_ephemeral')",
                [],
                |row| row.get(0),
            )
            .expect("v2 indexes");
        assert_eq!(v2_idx_count, 3);
    }

    // spec: ING-064 (ADR-020)
    #[test]
    fn higher_user_version_fails_with_format_too_new_and_stays_byte_identical() {
        let dir = TempDir::new().expect("tempdir");
        let future_path = dir.path().join("future.db");
        {
            let raw = Connection::open(&future_path).expect("create future db");
            raw.execute_batch(
                "PRAGMA journal_mode = DELETE;
                 CREATE TABLE future_tbl (x INTEGER PRIMARY KEY);
                 INSERT INTO future_tbl (x) VALUES (42);
                 PRAGMA user_version = 99;",
            )
            .expect("setup future db");
        }
        let bytes_before = fs::read(&future_path).expect("read before");
        assert!(!bytes_before.is_empty());

        let clock: Arc<dyn Clock> = Arc::new(ManualClock::new(1_700_000_000_000));
        let err = SqliteContextStore::open(SqliteConfig::new(&future_path), clock)
            .expect_err("must reject newer format");
        assert_eq!(
            err,
            StoreError::FormatTooNew {
                found: 99,
                supported: FORMAT_VERSION,
            }
        );

        let bytes_after = fs::read(&future_path).expect("read after");
        assert_eq!(bytes_before, bytes_after, "database file must remain byte-identical");
        assert!(!dir.path().join("future.db-wal").exists());
        assert!(!dir.path().join("future.db-shm").exists());
    }

    // spec: INV-09, INV-19
    #[test]
    fn inv_09_every_table_has_tenant_and_every_index_leads_with_tenant() {
        let dir = TempDir::new().expect("tempdir");
        let db_path = dir.path().join("layout.db");
        let clock: Arc<dyn Clock> = Arc::new(ManualClock::new(1_700_000_000_000));
        let store =
            SqliteContextStore::open(SqliteConfig::new(&db_path), clock).expect("open store");
        let reader_guard = store.readers.checkout();
        let reader = reader_guard.conn().expect("conn");

        for table in ["tenant_stats", "nodes", "edges", "nodes_fts"] {
            let mut stmt = reader
                .prepare(&format!("PRAGMA table_info({table});"))
                .expect("table_info");
            let cols: Vec<String> = stmt
                .query_map([], |row| row.get::<_, String>(1))
                .expect("cols")
                .collect::<Result<Vec<_>, _>>()
                .expect("collect cols");
            assert!(
                cols.iter().any(|c| c == "tenant"),
                "table `{table}` must carry a `tenant` column, found {cols:?}"
            );
        }

        let mut idx_stmt = reader
            .prepare(
                "SELECT name, tbl_name FROM sqlite_master
                 WHERE type = 'index' AND tbl_name IN ('tenant_stats', 'nodes', 'edges')",
            )
            .expect("list indexes");
        let indexes: Vec<(String, String)> = idx_stmt
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .expect("query indexes")
            .collect::<Result<Vec<_>, _>>()
            .expect("collect indexes");
        assert!(!indexes.is_empty());

        for (idx_name, tbl_name) in indexes {
            let mut info_stmt = reader
                .prepare(&format!("PRAGMA index_info(\"{idx_name}\");"))
                .expect("index_info");
            let first_col: String = info_stmt
                .query_row([], |row| row.get::<_, String>(2))
                .expect("leading col");
            assert_eq!(
                first_col, "tenant",
                "index `{idx_name}` on `{tbl_name}` must lead with `tenant`"
            );
        }
    }

    // spec: ING-064, INV-02
    #[test]
    fn pragmas_durability_and_rusqlite_limits_are_enforced_on_writer_and_readers() {
        let dir = TempDir::new().expect("tempdir");
        let db_path = dir.path().join("pragmas.db");
        let mut cfg = SqliteConfig::new(&db_path);
        cfg.durability = Durability::Normal;
        cfg.max_length_bytes = 2_097_152;
        cfg.max_sql_length_bytes = 262_144;
        cfg.max_variable_number = 999;

        let clock: Arc<dyn Clock> = Arc::new(ManualClock::new(1_700_000_000_000));
        let store = SqliteContextStore::open(cfg, clock).expect("open store");

        let writer = lock_unpoisoned(&store.writer);
        let jmode: String = writer
            .query_row("PRAGMA journal_mode;", [], |r| r.get(0))
            .expect("journal_mode");
        assert_eq!(jmode.to_ascii_lowercase(), "wal");
        let fk: i64 = writer
            .query_row("PRAGMA foreign_keys;", [], |r| r.get(0))
            .expect("fk");
        assert_eq!(fk, 1);
        let trusted: i64 = writer
            .query_row("PRAGMA trusted_schema;", [], |r| r.get(0))
            .expect("trusted_schema");
        assert_eq!(trusted, 0);
        let cell_check: i64 = writer
            .query_row("PRAGMA cell_size_check;", [], |r| r.get(0))
            .expect("cell_size_check");
        assert_eq!(cell_check, 1);
        // synchronous = NORMAL == 1 (FULL == 2).
        let sync_val: i64 = writer
            .query_row("PRAGMA synchronous;", [], |r| r.get(0))
            .expect("synchronous");
        assert_eq!(sync_val, 1);
        assert_eq!(writer.limit(Limit::SQLITE_LIMIT_LENGTH).expect("lim"), 2_097_152);
        assert_eq!(
            writer.limit(Limit::SQLITE_LIMIT_SQL_LENGTH).expect("lim"),
            262_144
        );
        assert_eq!(
            writer.limit(Limit::SQLITE_LIMIT_VARIABLE_NUMBER).expect("lim"),
            999
        );
        drop(writer);

        let reader_guard = store.readers.checkout();
        let reader = reader_guard.conn().expect("conn");
        let query_only: i64 = reader
            .query_row("PRAGMA query_only;", [], |r| r.get(0))
            .expect("query_only");
        assert_eq!(query_only, 1);
        assert_eq!(reader.limit(Limit::SQLITE_LIMIT_LENGTH).expect("lim"), 2_097_152);
        assert_eq!(
            reader.limit(Limit::SQLITE_LIMIT_SQL_LENGTH).expect("lim"),
            262_144
        );
        assert_eq!(
            reader.limit(Limit::SQLITE_LIMIT_VARIABLE_NUMBER).expect("lim"),
            999
        );
    }

    // spec: INV-02, ING-064
    #[test]
    fn wal_concurrent_readers_never_see_busy_while_writer_holds_transaction() {
        let dir = TempDir::new().expect("tempdir");
        let db_path = dir.path().join("concurrent_wal.db");
        let mut cfg = SqliteConfig::new(&db_path);
        cfg.reader_pool_size = NonZeroUsize::new(4).expect("nonzero");

        let clock: Arc<dyn Clock> = Arc::new(ManualClock::new(1_700_000_000_000));
        let store = Arc::new(SqliteContextStore::open(cfg, clock).expect("open store"));

        {
            let writer = lock_unpoisoned(&store.writer);
            writer
                .execute(
                    "INSERT INTO tenant_stats (tenant, node_count, edge_count) VALUES ('t1', 10, 20)",
                    [],
                )
                .expect("seed stats");
        }

        let reader_threads = 4usize;
        let writer_locked_barrier = Arc::new(Barrier::new(reader_threads + 1));
        let readers_done_barrier = Arc::new(Barrier::new(reader_threads + 1));

        let store_for_writer = Arc::clone(&store);
        let b1_w = Arc::clone(&writer_locked_barrier);
        let b2_w = Arc::clone(&readers_done_barrier);
        let writer_handle = thread::spawn(move || {
            let mut writer = lock_unpoisoned(&store_for_writer.writer);
            let tx = writer
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .expect("begin immediate write tx");
            tx.execute(
                "UPDATE tenant_stats SET node_count = 99 WHERE tenant = 't1'",
                [],
            )
            .expect("uncommitted update");

            // Release all reader threads while this IMMEDIATE write transaction is open!
            b1_w.wait();
            // Wait until every reader has queried its snapshot before committing.
            b2_w.wait();

            tx.commit().expect("commit write tx");
        });

        let mut handles = Vec::with_capacity(reader_threads);
        for _ in 0..reader_threads {
            let store_r = Arc::clone(&store);
            let b1_r = Arc::clone(&writer_locked_barrier);
            let b2_r = Arc::clone(&readers_done_barrier);
            handles.push(thread::spawn(move || {
                b1_r.wait();
                for _ in 0..25 {
                    let reader_guard = store_r.readers.checkout();
                    let reader = reader_guard.conn().expect("conn");
                    let snapshot_count: i64 = reader
                        .query_row(
                            "SELECT node_count FROM tenant_stats WHERE tenant = 't1'",
                            [],
                            |r| r.get(0),
                        )
                        .expect("WAL snapshot read must never fail with Busy");
                    assert_eq!(snapshot_count, 10);
                }
                b2_r.wait();
            }));
        }

        writer_handle.join().expect("join writer");
        for h in handles {
            h.join().expect("join reader");
        }

        let reader_guard = store.readers.checkout();
        let reader = reader_guard.conn().expect("conn");
        let committed_count: i64 = reader
            .query_row(
                "SELECT node_count FROM tenant_stats WHERE tenant = 't1'",
                [],
                |r| r.get(0),
            )
            .expect("post-commit read");
        assert_eq!(committed_count, 99);

        let u1 = store.mint_uuid_v7();
        let u2 = store.mint_uuid_v7();
        assert_eq!(u1.get_version_num(), 7);
        assert_eq!(u2.get_version_num(), 7);
        assert!(u1 < u2, "monotonic UUIDv7 sequence under fixed ManualClock");
    }
}
