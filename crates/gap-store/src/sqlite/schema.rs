//! SQLite schema, `PRAGMA user_version` migrations, per-tenant FTS5 shard naming, and physical
//! tenant-leading index layout (`INV-09`, `INV-19`, `ADR-020`, `ING-064`).

use std::fmt::Write as _;

use rusqlite::{Connection, Transaction, TransactionBehavior, params};

use crate::error::StoreError;
use crate::sqlite::map_sqlite_err;

/// Current on-disk SQLite schema version stored in `PRAGMA user_version` (`m2-01-design.md` §8).
pub const FORMAT_VERSION: u32 = 4;

/// Derives the per-tenant FTS5 virtual table name `nodes_fts_<shard>`, where `<shard>` is the
/// 16-character lowercase hex encoding of the first 8 bytes of `blake3(tenant_id)` (`INV-09`,
/// `TM-09`, `R-4`).
///
/// Security invariant: ONLY this fixed `[0-9a-f]{16}` hex digest may be interpolated into SQL
/// statements; the raw `tenant_id` string is never interpolated into SQL.
#[must_use]
pub fn tenant_fts_table_name(tenant_id: &str) -> String {
    let digest = blake3::hash(tenant_id.as_bytes());
    let mut table = String::with_capacity(10 + 16);
    table.push_str("nodes_fts_");
    for byte in &digest.as_bytes()[..8] {
        let _ = write!(&mut table, "{byte:02x}");
    }
    debug_assert_eq!(table.len(), 26);
    debug_assert!(
        table[10..]
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    );
    table
}

fn assert_valid_fts_shard_identifier(fts_table: &str) -> Result<(), StoreError> {
    let Some(hex_part) = fts_table.strip_prefix("nodes_fts_") else {
        return Err(StoreError::Backend);
    };
    if hex_part.len() != 16
        || !hex_part
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(StoreError::Backend);
    }
    Ok(())
}

/// Lazily creates the per-tenant FTS5 virtual table inside an active write transaction (`R-4`).
pub(crate) fn ensure_tenant_fts_table(
    tx: &Transaction<'_>,
    fts_table: &str,
) -> Result<(), StoreError> {
    assert_valid_fts_shard_identifier(fts_table)?;
    let ddl = format!("CREATE VIRTUAL TABLE IF NOT EXISTS {fts_table} USING fts5(label, summary);");
    tx.execute_batch(&ddl).map_err(map_sqlite_err)
}

/// Checks whether `table_name` exists in `sqlite_master` (`type = 'table'`).
pub(crate) fn fts_table_exists(conn: &Connection, table_name: &str) -> Result<bool, StoreError> {
    const TABLE_EXISTS_SQL: &str =
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1)";
    let mut stmt = conn
        .prepare_cached(TABLE_EXISTS_SQL)
        .map_err(map_sqlite_err)?;
    stmt.query_row(params![table_name], |row| row.get::<_, bool>(0))
        .map_err(map_sqlite_err)
}

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

struct SplitFtsPerTenantV3;

impl Migration for SplitFtsPerTenantV3 {
    fn target_version(&self) -> u32 {
        3
    }

    fn name(&self) -> &'static str {
        "split_fts_per_tenant_v3"
    }

    fn up(&self, tx: &Transaction<'_>) -> Result<(), StoreError> {
        let tenants: Vec<String> = {
            let mut stmt = tx
                .prepare("SELECT DISTINCT tenant FROM nodes ORDER BY tenant ASC")
                .map_err(map_sqlite_err)?;
            let mapped = stmt
                .query_map([], |row| row.get::<_, String>(0))
                .map_err(map_sqlite_err)?;
            let mut list = Vec::new();
            for item in mapped {
                list.push(item.map_err(map_sqlite_err)?);
            }
            list
        };

        for tenant_str in &tenants {
            let fts_table = tenant_fts_table_name(tenant_str);
            ensure_tenant_fts_table(tx, &fts_table)?;
            let copy_sql = format!(
                "INSERT INTO {fts_table} (rowid, label, summary)
                 SELECT fts_rowid, label, COALESCE(summary, '')
                 FROM nodes
                 WHERE tenant = ?1
                 ORDER BY fts_rowid ASC"
            );
            tx.execute(&copy_sql, params![tenant_str])
                .map_err(map_sqlite_err)?;
        }

        tx.execute_batch(
            "DROP TABLE IF EXISTS nodes_fts;
             PRAGMA user_version = 3;",
        )
        .map_err(map_sqlite_err)
    }
}

const MIGRATION_V4_DDL: &str = "
CREATE INDEX idx_nodes_task_purge
    ON nodes (tenant, scope, valid_to);

INSERT OR IGNORE INTO tenant_stats (tenant, node_count, edge_count)
SELECT
    n.tenant,
    COALESCE(SUM(CASE WHEN n.valid_to IS NULL THEN 1 ELSE 0 END), 0),
    COALESCE((
        SELECT COUNT(*)
        FROM edges e
        WHERE e.tenant = n.tenant AND e.valid_to IS NULL
    ), 0)
FROM nodes n
GROUP BY n.tenant;

PRAGMA user_version = 4;
";

struct AddTaskPurgeIndexV4;

impl Migration for AddTaskPurgeIndexV4 {
    fn target_version(&self) -> u32 {
        4
    }

    fn name(&self) -> &'static str {
        "add_task_purge_index_v4"
    }

    fn up(&self, tx: &Transaction<'_>) -> Result<(), StoreError> {
        tx.execute_batch(MIGRATION_V4_DDL).map_err(map_sqlite_err)
    }
}

static MIGRATION_STEP_1: CreateBaseSchemaV1 = CreateBaseSchemaV1;
static MIGRATION_STEP_2: AddLifecycleIndexesV2 = AddLifecycleIndexesV2;
static MIGRATION_STEP_3: SplitFtsPerTenantV3 = SplitFtsPerTenantV3;
static MIGRATION_STEP_4: AddTaskPurgeIndexV4 = AddTaskPurgeIndexV4;

/// Ordered built-in schema migrations (`1..=FORMAT_VERSION`).
pub static MIGRATIONS: &[&dyn Migration] = &[
    &MIGRATION_STEP_1,
    &MIGRATION_STEP_2,
    &MIGRATION_STEP_3,
    &MIGRATION_STEP_4,
];

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

/// Verifies that all required tables exist for a migrated or restored database (`user_version >= 1`).
pub fn verify_required_tables(conn: &Connection) -> Result<(), StoreError> {
    const CHECK_CORE_TABLES_SQL: &str = "
        SELECT COUNT(*)
        FROM sqlite_master
        WHERE type = 'table'
          AND name IN ('tenant_stats', 'nodes', 'edges')
    ";
    let core_count: i64 = conn
        .query_row(CHECK_CORE_TABLES_SQL, [], |row| row.get(0))
        .map_err(map_sqlite_err)?;
    if core_count != 3 {
        return Err(StoreError::Corrupt);
    }

    let version = read_user_version(conn)?;
    if version == 0 {
        return Err(StoreError::Corrupt);
    }
    if version <= 2 {
        if !fts_table_exists(conn, "nodes_fts")? {
            return Err(StoreError::Corrupt);
        }
        return Ok(());
    }

    // `version >= 3`: shared `nodes_fts` must not exist, and every tenant present in `nodes`
    // must have its per-tenant `nodes_fts_<shard>` virtual table.
    if fts_table_exists(conn, "nodes_fts")? {
        return Err(StoreError::Corrupt);
    }
    if version >= 4 {
        let has_purge_idx: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'index' AND name = 'idx_nodes_task_purge')",
                [],
                |row| row.get(0),
            )
            .map_err(map_sqlite_err)?;
        if !has_purge_idx {
            return Err(StoreError::Corrupt);
        }
    }
    let mut stmt = conn
        .prepare("SELECT DISTINCT tenant FROM nodes")
        .map_err(map_sqlite_err)?;
    let mapped = stmt
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(map_sqlite_err)?;
    for item in mapped {
        let tenant_str = item.map_err(map_sqlite_err)?;
        let fts_table = tenant_fts_table_name(&tenant_str);
        if !fts_table_exists(conn, &fts_table)? {
            return Err(StoreError::Corrupt);
        }
    }
    Ok(())
}
