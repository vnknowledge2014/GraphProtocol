//! Full-text search (`ContextStore::search_nodes`) over `label` and `summary` using SQLite FTS5
//! `bm25()` with quoted literal tokens (`INV-09`, `ING-065`, `m2-01-design.md` §7).

use gap_core::clock::Timestamp;
use gap_core::ids::TenantScope;
use rusqlite::params;

use crate::error::StoreError;
use crate::sqlite::crud::{caller_claims_json, materialize_node, read_node_row};
use crate::sqlite::{SqliteContextStore, map_sqlite_err, schema};
use crate::types::{
    Caller, Node, normalize_timestamp, timestamp_to_micros, validate_caller, validate_search_limit,
};

fn build_search_nodes_fts_sql(fts_table: &str) -> String {
    format!(
        "WITH
            caller_groups(val) AS (SELECT value FROM json_each(?5)),
            caller_tasks(val) AS (SELECT value FROM json_each(?6))
        SELECT
            n.id, n.node_type, n.label, n.summary, n.props_json,
            n.scope, n.base_scope, n.owner_principal, n.task_id, n.group_name, n.expires_at,
            n.prov_source, n.prov_desc_hash, n.prov_confidence, n.prov_extracted_at, n.prov_agent,
            n.valid_from, n.valid_to
        FROM {fts_table}
        JOIN nodes n
          ON n.fts_rowid = {fts_table}.rowid
        WHERE {fts_table} MATCH ?1
          AND n.tenant = ?2
          AND n.valid_from <= ?3
          AND (n.valid_to IS NULL OR ?3 < n.valid_to)
          AND (
              n.scope != 'EPHEMERAL'
              OR (n.expires_at IS NOT NULL AND n.expires_at > ?4 AND n.expires_at > ?3)
          )
          AND (
              n.base_scope = 'GLOBAL'
              OR (n.base_scope = 'PRIVATE' AND n.owner_principal = ?7)
              OR (
                  n.base_scope = 'GROUP'
                  AND n.group_name IS NOT NULL
                  AND n.group_name IN (SELECT val FROM caller_groups)
              )
              OR (
                  n.base_scope = 'TASK'
                  AND n.task_id IS NOT NULL
                  AND n.task_id IN (SELECT val FROM caller_tasks)
              )
          )
        ORDER BY bm25({fts_table}) ASC, n.id ASC
        LIMIT ?8"
    )
}

/// Splits an untrusted user query into alphanumeric tokens (`unicode61`-aligned) and quotes each
/// token so FTS5 operators (`"`, `*`, `NEAR(`, `-`, `:`, `OR`, `AND`, `NOT`, `^`, `(`, `)`) are
/// never interpreted as syntax (`m2-01-design.md` §7).
pub(crate) fn build_fts5_literal_query(raw: &str) -> Option<String> {
    let mut expr = String::new();
    for token in raw.split(|c: char| !c.is_alphanumeric()) {
        if token.is_empty() {
            continue;
        }
        if !expr.is_empty() {
            expr.push(' ');
        }
        expr.push('"');
        expr.push_str(token);
        expr.push('"');
    }
    if expr.is_empty() { None } else { Some(expr) }
}

impl SqliteContextStore {
    pub(crate) fn exec_search_nodes(
        &self,
        tenant: &TenantScope,
        caller: &Caller,
        query: &str,
        limit: u32,
        as_of: Option<Timestamp>,
    ) -> Result<Vec<Node>, StoreError> {
        validate_caller(caller)?;
        validate_search_limit(limit)?;

        let now_ts = normalize_timestamp(self.clock.now_timestamp())?;
        let now_us = timestamp_to_micros(now_ts);
        let as_of_ts = match as_of {
            Some(ts) => normalize_timestamp(ts)?,
            None => now_ts,
        };
        let as_of_us = timestamp_to_micros(as_of_ts);

        let Some(fts_expr) = build_fts5_literal_query(query) else {
            return Ok(Vec::new());
        };

        let (groups_json, tasks_json) = caller_claims_json(caller)?;
        let tenant_str = tenant.tenant().as_str();

        let guard = self.readers.checkout();
        let conn = guard.conn()?;
        let fts_table = schema::tenant_fts_table_name(tenant_str);
        if !schema::fts_table_exists(conn, &fts_table)? {
            return Ok(Vec::new());
        }
        let search_sql = build_search_nodes_fts_sql(&fts_table);
        let mut stmt = conn.prepare_cached(&search_sql).map_err(map_sqlite_err)?;

        let rows = stmt
            .query_map(
                params![
                    fts_expr,
                    tenant_str,
                    as_of_us,
                    now_us,
                    groups_json,
                    tasks_json,
                    caller.principal.as_str(),
                    i64::from(limit),
                ],
                read_node_row,
            )
            .map_err(map_sqlite_err)?;

        let mut results = Vec::new();
        for item in rows {
            let raw = item.map_err(map_sqlite_err)?;
            results.push(materialize_node(raw)?);
        }
        Ok(results)
    }
}
