//! Lifecycle maintenance (`ContextMaintenance`: `tombstone_task_nodes` and `purge_expired`)
//! for the SQLite backend (`ING-066`, `INV-09`, `m2-01-design.md` §5).

use std::collections::BTreeSet;

use gap_core::ids::{TaskId, TenantScope};
use rusqlite::{TransactionBehavior, params};

use crate::error::StoreError;
use crate::sqlite::crud::{read_tenant_stats, write_tenant_stats};
use crate::sqlite::{SqliteContextStore, lock_unpoisoned, map_sqlite_err};
use crate::store::ContextMaintenance;
use crate::types::{PurgePolicy, PurgeReport, normalize_timestamp, timestamp_to_micros};

const SELECT_ACTIVE_TASK_NODES_SQL: &str = "
    SELECT fts_rowid, id, valid_from
    FROM nodes
    WHERE tenant = ?1
      AND base_scope = 'TASK'
      AND task_id = ?2
      AND valid_to IS NULL
";

const CLOSE_TASK_NODE_ROW_SQL: &str = "
    UPDATE nodes
    SET valid_to = ?1
    WHERE tenant = ?2 AND fts_rowid = ?3 AND valid_to IS NULL
";

const CLOSE_TASK_INCIDENT_EDGES_SQL: &str = "
    UPDATE edges
    SET valid_to = MAX(?1, valid_from)
    WHERE tenant = ?2
      AND (from_id = ?3 OR to_id = ?3)
      AND valid_to IS NULL
";

const LIST_KNOWN_TENANTS_SQL: &str = "
    SELECT tenant FROM tenant_stats
    UNION
    SELECT DISTINCT tenant FROM nodes
";

const SELECT_EXPIRED_EPHEMERAL_ROWS_SQL: &str = "
    SELECT fts_rowid, id
    FROM nodes
    WHERE tenant = ?1
      AND scope = 'EPHEMERAL'
      AND expires_at IS NOT NULL
      AND expires_at <= ?2
";

const SELECT_EXPIRED_TASK_ROWS_SQL: &str = "
    SELECT n.fts_rowid, n.id
    FROM nodes n
    WHERE n.tenant = ?1
      AND n.scope = 'TASK'
      AND n.valid_to IS NOT NULL
      AND n.valid_to <= ?2
      AND NOT EXISTS (
          SELECT 1
          FROM nodes other
          WHERE other.tenant = n.tenant
            AND other.id = n.id
            AND (other.valid_to IS NULL OR other.valid_to > ?2)
      )
";

const DELETE_PURGED_FTS_ROW_SQL: &str = "
    DELETE FROM nodes_fts
    WHERE rowid = ?1 AND tenant = ?2
";

const DELETE_PURGED_NODE_ROW_SQL: &str = "
    DELETE FROM nodes
    WHERE tenant = ?1 AND fts_rowid = ?2
";

const NODE_HAS_ANY_REMAINING_VERSION_SQL: &str = "
    SELECT EXISTS (
        SELECT 1
        FROM nodes
        WHERE tenant = ?1 AND id = ?2
    )
";

const DELETE_PURGED_INCIDENT_EDGES_SQL: &str = "
    DELETE FROM edges
    WHERE tenant = ?1
      AND (from_id = ?2 OR to_id = ?2)
";

const COUNT_ACTIVE_TENANT_NODES_SQL: &str = "
    SELECT COUNT(*)
    FROM nodes
    WHERE tenant = ?1 AND valid_to IS NULL
";

const COUNT_ACTIVE_TENANT_EDGES_SQL: &str = "
    SELECT COUNT(*)
    FROM edges
    WHERE tenant = ?1 AND valid_to IS NULL
";

impl SqliteContextStore {
    pub(crate) fn exec_tombstone_task_nodes(
        &self,
        tenant: &TenantScope,
        task_id: TaskId,
    ) -> Result<u64, StoreError> {
        let now_ts = normalize_timestamp(self.clock.now_timestamp())?;
        let now_us = timestamp_to_micros(now_ts);
        let tenant_str = tenant.tenant().as_str();
        let task_str = task_id.to_string();

        let mut writer_conn = lock_unpoisoned(&self.writer);
        let tx = writer_conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(map_sqlite_err)?;

        let active_rows: Vec<(i64, String, i64)> = {
            let mut stmt = tx
                .prepare_cached(SELECT_ACTIVE_TASK_NODES_SQL)
                .map_err(map_sqlite_err)?;
            let mapped = stmt
                .query_map(params![tenant_str, task_str], |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, i64>(2)?,
                    ))
                })
                .map_err(map_sqlite_err)?;
            let mut collected = Vec::new();
            for item in mapped {
                collected.push(item.map_err(map_sqlite_err)?);
            }
            collected
        };

        if active_rows.is_empty() {
            return Ok(0);
        }

        let mut closed_nodes: u64 = 0;
        let mut closed_edges: u64 = 0;
        {
            let mut close_node_stmt = tx
                .prepare_cached(CLOSE_TASK_NODE_ROW_SQL)
                .map_err(map_sqlite_err)?;
            let mut close_edges_stmt = tx
                .prepare_cached(CLOSE_TASK_INCIDENT_EDGES_SQL)
                .map_err(map_sqlite_err)?;

            for (fts_rowid, node_id, valid_from_us) in &active_rows {
                let eff_now = now_us.max(*valid_from_us);
                let changed_node = close_node_stmt
                    .execute(params![eff_now, tenant_str, fts_rowid])
                    .map_err(map_sqlite_err)?;
                closed_nodes =
                    closed_nodes.saturating_add(u64::try_from(changed_node).unwrap_or(0));

                let changed_edges = close_edges_stmt
                    .execute(params![eff_now, tenant_str, node_id])
                    .map_err(map_sqlite_err)?;
                closed_edges =
                    closed_edges.saturating_add(u64::try_from(changed_edges).unwrap_or(0));
            }
        }

        let (cur_nodes, cur_edges) = read_tenant_stats(&tx, tenant_str)?;
        write_tenant_stats(
            &tx,
            tenant_str,
            cur_nodes.saturating_sub(closed_nodes),
            cur_edges.saturating_sub(closed_edges),
        )?;

        tx.commit().map_err(map_sqlite_err)?;
        Ok(closed_nodes)
    }

    pub(crate) fn exec_purge_expired(
        &self,
        policy: &PurgePolicy,
    ) -> Result<PurgeReport, StoreError> {
        let now_ts = normalize_timestamp(self.clock.now_timestamp())?;
        let now_us = timestamp_to_micros(now_ts);
        let retention_us = i64::try_from(policy.task_retention.as_micros()).unwrap_or(i64::MAX);
        let task_cutoff_us = now_us.saturating_sub(retention_us);

        let mut writer_conn = lock_unpoisoned(&self.writer);
        let tx = writer_conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(map_sqlite_err)?;

        let tenants: Vec<String> = {
            let mut stmt = tx
                .prepare_cached(LIST_KNOWN_TENANTS_SQL)
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

        let mut report = PurgeReport::default();

        if tenants.is_empty() {
            return Ok(report);
        }

        {
            let mut sel_ephemeral = tx
                .prepare_cached(SELECT_EXPIRED_EPHEMERAL_ROWS_SQL)
                .map_err(map_sqlite_err)?;
            let mut sel_task = tx
                .prepare_cached(SELECT_EXPIRED_TASK_ROWS_SQL)
                .map_err(map_sqlite_err)?;
            let mut del_fts = tx
                .prepare_cached(DELETE_PURGED_FTS_ROW_SQL)
                .map_err(map_sqlite_err)?;
            let mut del_node = tx
                .prepare_cached(DELETE_PURGED_NODE_ROW_SQL)
                .map_err(map_sqlite_err)?;
            let mut has_remaining_ver = tx
                .prepare_cached(NODE_HAS_ANY_REMAINING_VERSION_SQL)
                .map_err(map_sqlite_err)?;
            let mut del_edges = tx
                .prepare_cached(DELETE_PURGED_INCIDENT_EDGES_SQL)
                .map_err(map_sqlite_err)?;
            let mut cnt_nodes = tx
                .prepare_cached(COUNT_ACTIVE_TENANT_NODES_SQL)
                .map_err(map_sqlite_err)?;
            let mut cnt_edges = tx
                .prepare_cached(COUNT_ACTIVE_TENANT_EDGES_SQL)
                .map_err(map_sqlite_err)?;

            for tenant_str in &tenants {
                let mut candidate_node_ids: BTreeSet<String> = BTreeSet::new();
                let mut touched_tenant = false;

                let ephemeral_rows: Vec<(i64, String)> = {
                    let mapped = sel_ephemeral
                        .query_map(params![tenant_str, now_us], |row| {
                            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
                        })
                        .map_err(map_sqlite_err)?;
                    let mut out = Vec::new();
                    for item in mapped {
                        out.push(item.map_err(map_sqlite_err)?);
                    }
                    out
                };

                for (fts_rowid, node_id) in ephemeral_rows {
                    del_fts
                        .execute(params![fts_rowid, tenant_str])
                        .map_err(map_sqlite_err)?;
                    let deleted = del_node
                        .execute(params![tenant_str, fts_rowid])
                        .map_err(map_sqlite_err)?;
                    if deleted > 0 {
                        report.purged_ephemeral_nodes = report
                            .purged_ephemeral_nodes
                            .saturating_add(u64::try_from(deleted).unwrap_or(0));
                        candidate_node_ids.insert(node_id);
                        touched_tenant = true;
                    }
                }

                let task_rows: Vec<(i64, String)> = {
                    let mapped = sel_task
                        .query_map(params![tenant_str, task_cutoff_us], |row| {
                            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
                        })
                        .map_err(map_sqlite_err)?;
                    let mut out = Vec::new();
                    for item in mapped {
                        out.push(item.map_err(map_sqlite_err)?);
                    }
                    out
                };

                for (fts_rowid, node_id) in task_rows {
                    del_fts
                        .execute(params![fts_rowid, tenant_str])
                        .map_err(map_sqlite_err)?;
                    let deleted = del_node
                        .execute(params![tenant_str, fts_rowid])
                        .map_err(map_sqlite_err)?;
                    if deleted > 0 {
                        report.purged_task_nodes = report
                            .purged_task_nodes
                            .saturating_add(u64::try_from(deleted).unwrap_or(0));
                        candidate_node_ids.insert(node_id);
                        touched_tenant = true;
                    }
                }

                for node_id in &candidate_node_ids {
                    let still_exists: bool = has_remaining_ver
                        .query_row(params![tenant_str, node_id], |row| row.get(0))
                        .map_err(map_sqlite_err)?;
                    if !still_exists {
                        let deleted_edges = del_edges
                            .execute(params![tenant_str, node_id])
                            .map_err(map_sqlite_err)?;
                        report.purged_edges = report
                            .purged_edges
                            .saturating_add(u64::try_from(deleted_edges).unwrap_or(0));
                    }
                }

                if touched_tenant {
                    let active_nodes_i64: i64 = cnt_nodes
                        .query_row(params![tenant_str], |row| row.get(0))
                        .map_err(map_sqlite_err)?;
                    let active_edges_i64: i64 = cnt_edges
                        .query_row(params![tenant_str], |row| row.get(0))
                        .map_err(map_sqlite_err)?;
                    write_tenant_stats(
                        &tx,
                        tenant_str,
                        u64::try_from(active_nodes_i64.max(0)).unwrap_or(0),
                        u64::try_from(active_edges_i64.max(0)).unwrap_or(0),
                    )?;
                }
            }
        }

        tx.commit().map_err(map_sqlite_err)?;
        Ok(report)
    }
}

impl ContextMaintenance for SqliteContextStore {
    fn tombstone_task_nodes(
        &self,
        tenant: &TenantScope,
        task_id: TaskId,
    ) -> Result<u64, StoreError> {
        self.exec_tombstone_task_nodes(tenant, task_id)
    }

    fn purge_expired(&self, policy: &PurgePolicy) -> Result<PurgeReport, StoreError> {
        self.exec_purge_expired(policy)
    }
}
