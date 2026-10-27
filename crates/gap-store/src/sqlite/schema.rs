//! SQLite schema, `PRAGMA user_version` migrations, and physical tenant-leading index layout
//! (`INV-09`, `INV-19`, `ADR-020`, `ING-064`).

use rusqlite::{Connection, Transaction, TransactionBehavior};

use crate::error::StoreError;
use crate::sqlite::map_sqlite_err;

/// Current on-disk SQLite schema version stored in `PRAGMA user_version` (`m2-01-design.md` §8).
pub const FORMAT_VERSION: u32 = 2;

/// Ordered, transactional schema migration step (`m2-01-design.md` §8).
pub trait Migration: Send + Sync {
    /// Version recorded in `PRAGMA user_version` once this step commits (`>= 1`).
    fn target_version(&self) -> u32;

    /// Static human-readable step name for diagnostics.
    fn name(&self) -> &'static str;

    /// Applies the DDL/DML changes inside an active `IMMEDIATE` transaction.
    fn up(&self, tx: &Transaction<'_>) -> Result<(), StoreError>;
}

const MIGRATION_V1_DDL: &str = "
CREATE TABLE tenant_stats (
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

CREATE UNIQUE INDEX idx_nodes_current
    ON nodes (tenant, id)
    WHERE valid_to IS NULL;

CREATE INDEX idx_nodes_temporal
    ON nodes (tenant, id, valid_from, valid_to);

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

CREATE UNIQUE INDEX idx_edges_current_quad
    ON edges (tenant, edge_type, from_id, to_id)
    WHERE valid_to IS NULL;

CREATE UNIQUE INDEX idx_edges_current_id
    ON edges (tenant, id)
    WHERE valid_to IS NULL;

CREATE INDEX idx_edges_from
    ON edges (tenant, from_id, valid_from, valid_to);

CREATE INDEX idx_edges_to
    ON edges (tenant, to_id, valid_from, valid_to);

CREATE INDEX idx_edges_id_temporal
    ON edges (tenant, id, valid_from, valid_to);

CREATE VIRTUAL TABLE nodes_fts USING fts5(
    tenant UNINDEXED,
    label,
    summary
);
";

const SET_USER_VERSION_1: &str = "PRAGMA user_version = 1;";

const MIGRATION_V2_DDL: &str = "
CREATE INDEX idx_nodes_task
    ON nodes (tenant, scope, task_id, valid_to);

CREATE INDEX idx_nodes_base_task
    ON nodes (tenant, base_scope, task_id, valid_to);

CREATE INDEX idx_nodes_ephemeral
    ON nodes (tenant, scope, expires_at);
";

const SET_USER_VERSION_2: &str = "PRAGMA user_version = 2;";

struct CreateBaseSchemaV1;

impl Migration for CreateBaseSchemaV1 {
    fn target_version(&self) -> u32 {
        1
    }

    fn name(&self) -> &'static str {
        "create_base_schema_v1"
    }

    fn up(&self, tx: &Transaction<'_>) -> Result<(), StoreError> {
        tx.execute_batch(MIGRATION_V1_DDL).map_err(map_sqlite_err)?;
        tx.execute_batch(SET_USER_VERSION_1).map_err(map_sqlite_err)
    }
}

struct AddLifecycleIndexesV2;

impl Migration for AddLifecycleIndexesV2 {
    fn target_version(&self) -> u32 {
        2
    }

    fn name(&self) -> &'static str {
        "add_lifecycle_indexes_v2"
    }

    fn up(&self, tx: &Transaction<'_>) -> Result<(), StoreError> {
        tx.execute_batch(MIGRATION_V2_DDL).map_err(map_sqlite_err)?;
        tx.execute_batch(SET_USER_VERSION_2).map_err(map_sqlite_err)
    }
}

static MIGRATION_STEP_1: CreateBaseSchemaV1 = CreateBaseSchemaV1;
static MIGRATION_STEP_2: AddLifecycleIndexesV2 = AddLifecycleIndexesV2;

/// Ordered built-in schema migrations (`1..=FORMAT_VERSION`).
pub static MIGRATIONS: &[&dyn Migration] = &[&MIGRATION_STEP_1, &MIGRATION_STEP_2];

/// Reads `PRAGMA user_version` as `u32` (treating negative values as corrupt).
pub fn read_user_version(conn: &Connection) -> Result<u32, StoreError> {
    let raw: i64 = conn
        .query_row("PRAGMA user_version;", [], |row| row.get(0))
        .map_err(map_sqlite_err)?;
    u32::try_from(raw).map_err(|_| StoreError::Corrupt)
}

/// Applies any pending migrations from `current_version` up to [`FORMAT_VERSION`], one transaction
/// per step (`m2-01-design.md` §8).
pub fn apply_migrations(conn: &mut Connection) -> Result<u32, StoreError> {
    let mut version = read_user_version(conn)?;
    if version > FORMAT_VERSION {
        return Err(StoreError::FormatTooNew {
            found: version,
            supported: FORMAT_VERSION,
        });
    }
    for step in MIGRATIONS {
        let target = step.target_version();
        if version < target {
            if version + 1 != target {
                return Err(StoreError::Backend);
            }
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(map_sqlite_err)?;
            step.up(&tx)?;
            tx.commit().map_err(map_sqlite_err)?;
            version = read_user_version(conn)?;
            if version != target {
                tracing::warn!(
                    migration = step.name(),
                    expected = target,
                    found = version,
                    "sqlite migration user_version mismatch"
                );
                return Err(StoreError::Backend);
            }
        }
    }
    Ok(version)
}

/// Verifies that all required tables exist for a migrated database (`user_version >= 1`).
pub fn verify_required_tables(conn: &Connection) -> Result<(), StoreError> {
    const CHECK_TABLES_SQL: &str = "
        SELECT COUNT(*)
        FROM sqlite_master
        WHERE type = 'table'
          AND name IN ('tenant_stats', 'nodes', 'edges', 'nodes_fts')
    ";
    let count: i64 = conn
        .query_row(CHECK_TABLES_SQL, [], |row| row.get(0))
        .map_err(map_sqlite_err)?;
    if count != 4 {
        return Err(StoreError::Corrupt);
    }
    Ok(())
}
