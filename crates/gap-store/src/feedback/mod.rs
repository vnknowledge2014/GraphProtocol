//! Discovery feedback and online calibration sample storage (`WP-M5-05`;
//! `RTE-110`, `RTE-112`, `SEC-110`, `TM-14`, `INV-09`).
//!
//! Persists routing decisions strictly without client-supplied outcomes (`TM-14`), derives
//! implicit feedback solely from server-observed executions within a 5-minute trace/session window
//! (`RTE-110`), routes manual administrator labels (`USER_LABEL`) through the tamper-evident
//! [`AuditLog`], enforces PII minimization and bounded retention (`SEC-110`), and exposes a
//! read-only export interface for offline calibration jobs (`RTE-112`).

#[cfg(test)]
mod tests;

use std::fs;
use std::path::Path;
use std::sync::{Mutex, MutexGuard};

use gap_core::clock::Timestamp;
use gap_core::ids::TenantScope;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::audit::{AuditChain, AuditEntry, AuditLog};
use crate::error::StoreError;

/// Maximum elapsed window (`5 minutes`) between discovery and server-observed execution
/// attribution (`RTE-110`).
pub const ATTRIBUTION_WINDOW_MS: i64 = 300_000;

/// Maximum permitted retention horizon (`7 days`) for PII-minimized feedback rows (`SEC-110`).
pub const DEFAULT_FEEDBACK_RETENTION_MS: i64 = 7 * 24 * 60 * 60 * 1_000;

/// Maximum character length of optional redacted query snippets (`SEC-110`).
pub const MAX_REDACTED_QUERY_CHARS: usize = 256;

/// Discovery feedback outcome classification persisted in `routing_feedback` (`RTE-110`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum FeedbackOutcomeKind {
    /// Initial state when recorded on discovery (`TM-14`: never client-supplied).
    None,
    /// Server observed successful execution of an operation in the predicted/selected group.
    ExecutedOk,
    /// Server observed failed execution of an operation in the predicted/selected group.
    ExecutedError,
    /// Server observed execution of a different group within 5 minutes (implicit negative).
    ExecutedOther,
    /// Explicit administrator review label recorded through [`AuditLog`].
    UserLabel,
}

impl FeedbackOutcomeKind {
    /// Canonical database & schema string representation.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::None => "NONE",
            Self::ExecutedOk => "EXECUTED_OK",
            Self::ExecutedError => "EXECUTED_ERROR",
            Self::ExecutedOther => "EXECUTED_OTHER",
            Self::UserLabel => "USER_LABEL",
        }
    }

    /// Parses a persisted string tag into [`FeedbackOutcomeKind`].
    #[must_use]
    pub fn from_str_code(raw: &str) -> Option<Self> {
        match raw {
            "NONE" => Some(Self::None),
            "EXECUTED_OK" => Some(Self::ExecutedOk),
            "EXECUTED_ERROR" => Some(Self::ExecutedError),
            "EXECUTED_OTHER" => Some(Self::ExecutedOther),
            "USER_LABEL" => Some(Self::UserLabel),
            _ => None,
        }
    }
}

/// PII-minimized input accepted when logging a discovery routing event (`RTE-110`, `SEC-110`, `TM-14`).
///
/// Notice there is intentionally no client-supplied `outcome` field: initial persisted outcome is
/// unconditionally [`FeedbackOutcomeKind::None`] (`TM-14`).
#[derive(Debug, Clone, PartialEq)]
pub struct NewDiscoveryFeedback {
    /// Stable per-request feedback record identifier.
    pub feedback_id: String,
    /// Server clock timestamp when discovery completed.
    pub ts: Timestamp,
    /// Correlation `trace_id` observed by the server gateway.
    pub trace_id: String,
    /// Optional server session identifier used alongside `trace_id` within the 5-minute window.
    pub session_id: Option<String>,
    /// Lowercase 64-character hex digest (`BLAKE3`/`SHA-256`) of the sanitized query (`SEC-110`).
    pub query_hash_hex: String,
    /// Optional redacted query text (dev/explicit capture only, `<= 256` chars, PII-checked).
    pub redacted_query: Option<String>,
    /// Candidate capability group IDs evaluated during routing.
    pub candidate_group_ids: Vec<String>,
    /// Model logits aligned with `candidate_group_ids` (plus trailing sink if present).
    pub option_logits: Vec<f32>,
    /// Top-1 predicted group ID, if the router did not abstain.
    pub selected_group_id: Option<String>,
    /// Conformal prediction set `C(X) \ {sink}` presented to the caller.
    pub prediction_set_ids: Vec<String>,
    /// Abstention reason string if the outcome was abstention.
    pub abstained_reason: Option<String>,
    /// Active [`gap_core::snapshot::CalibrationTable`] ID bound to this decision.
    pub calibration_id: Option<String>,
}

/// Tenant-scoped persisted row in `routing_feedback` (`RTE-110`, `INV-09`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StoredFeedbackRow {
    /// Owning tenant identifier (`INV-09`).
    pub tenant: String,
    /// Primary record identifier.
    pub feedback_id: String,
    /// Discovery server timestamp in Unix milliseconds.
    pub ts_ms: i64,
    /// Correlation `trace_id`.
    pub trace_id: String,
    /// Optional server `session_id`.
    pub session_id: Option<String>,
    /// Query cryptographic digest (`SEC-110`).
    pub query_hash_hex: String,
    /// Optional PII-redacted query snippet (`None` by default in prod, `SEC-110`).
    pub redacted_query: Option<String>,
    /// Candidate capability group IDs evaluated.
    pub candidate_group_ids: Vec<String>,
    /// Pre-calibration or raw decision logits.
    pub option_logits: Vec<f32>,
    /// Predicted top-1 group ID, if any.
    pub selected_group_id: Option<String>,
    /// Conformal prediction set group IDs.
    pub prediction_set_ids: Vec<String>,
    /// Abstention reason tag, if abstained.
    pub abstained_reason: Option<String>,
    /// Calibration table ID active during routing.
    pub calibration_id: Option<String>,
    /// Current server-attributed or admin-audited outcome (`TM-14`).
    pub outcome: FeedbackOutcomeKind,
    /// Server-observed executed operation or admin gold label ID.
    pub attributed_operation_id: Option<String>,
    /// Server attribution timestamp in Unix milliseconds.
    pub attributed_at_ms: Option<i64>,
}

/// Computes a lowercase 64-char `BLAKE3` query digest suitable for [`NewDiscoveryFeedback::query_hash_hex`].
#[must_use]
pub fn derive_query_hash_hex(sanitized_query: &str) -> String {
    blake3::hash(sanitized_query.as_bytes())
        .to_hex()
        .to_string()
}

/// Enforces `TM-14`: rejects any client-supplied discovery/feedback JSON object attempting to inject
/// outcome or attribution labels.
///
/// # Errors
///
/// Returns [`StoreError::Invalid`] if forbidden client outcome keys appear.
pub fn validate_no_client_outcome_override(payload: &Value) -> Result<(), StoreError> {
    const FORBIDDEN_KEYS: &[&str] = &[
        "outcome",
        "user_label",
        "executed_ok",
        "executed_error",
        "executed_other",
        "gold_group_id",
        "attributed_operation_id",
        "attributed_at_ms",
    ];
    let Some(map) = payload.as_object() else {
        return Err(StoreError::Invalid {
            field: "payload",
            reason: "feedback payload must be a JSON object",
        });
    };
    for &key in FORBIDDEN_KEYS {
        if map.contains_key(key) {
            return Err(StoreError::Invalid {
                field: "outcome",
                reason: "client-supplied feedback outcome overrides are forbidden (TM-14)",
            });
        }
    }
    Ok(())
}

/// SQLite-backed durable store for `routing_feedback` (`RTE-110`, `SEC-110`, `TM-14`, `INV-09`).
#[derive(Debug)]
pub struct SqliteFeedbackStore {
    conn: Mutex<Connection>,
}

impl SqliteFeedbackStore {
    /// Opens or initializes a durable WAL-mode SQLite feedback database at `path`.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Invalid`] if `path` is empty, or [`StoreError::Backend`] if SQLite
    /// initialization fails.
    pub fn open(path: &Path) -> Result<Self, StoreError> {
        if path.as_os_str().is_empty() {
            return Err(StoreError::Invalid {
                field: "path",
                reason: "feedback sqlite path must not be empty",
            });
        }
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent).map_err(|_| StoreError::Backend)?;
        }
        let conn = Connection::open(path).map_err(|_| StoreError::Backend)?;
        Self::configure_and_init(conn, true)
    }

    /// Opens an isolated in-memory SQLite feedback database suitable for hermetic tests.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Backend`] if in-memory initialization fails.
    pub fn open_in_memory() -> Result<Self, StoreError> {
        let conn = Connection::open_in_memory().map_err(|_| StoreError::Backend)?;
        Self::configure_and_init(conn, false)
    }

    fn configure_and_init(conn: Connection, enable_wal: bool) -> Result<Self, StoreError> {
        conn.execute_batch("PRAGMA busy_timeout = 5000; PRAGMA foreign_keys = ON;")
            .map_err(|_| StoreError::Backend)?;
        if enable_wal {
            let _ = conn.pragma_update(None, "journal_mode", "WAL");
            let _ = conn.pragma_update(None, "synchronous", "FULL");
        }
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS routing_feedback (
                tenant TEXT NOT NULL,
                feedback_id TEXT NOT NULL,
                ts_ms INTEGER NOT NULL,
                trace_id TEXT NOT NULL,
                session_id TEXT,
                query_hash_hex TEXT NOT NULL,
                redacted_query TEXT,
                candidate_group_ids_json TEXT NOT NULL,
                option_logits_json TEXT NOT NULL,
                selected_group_id TEXT,
                prediction_set_ids_json TEXT NOT NULL,
                abstained_reason TEXT,
                calibration_id TEXT,
                outcome TEXT NOT NULL,
                attributed_operation_id TEXT,
                attributed_at_ms INTEGER,
                PRIMARY KEY (tenant, feedback_id)
            );
            CREATE INDEX IF NOT EXISTS idx_routing_feedback_trace
                ON routing_feedback (tenant, trace_id, ts_ms);
            CREATE INDEX IF NOT EXISTS idx_routing_feedback_session
                ON routing_feedback (tenant, session_id, ts_ms);
            CREATE INDEX IF NOT EXISTS idx_routing_feedback_ts
                ON routing_feedback (tenant, ts_ms);",
        )
        .map_err(|_| StoreError::Backend)?;

        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    fn lock_conn(&self) -> Result<MutexGuard<'_, Connection>, StoreError> {
        self.conn.lock().map_err(|_| StoreError::Backend)
    }

    /// Records a newly observed discovery decision under `scope` with initial outcome `NONE`
    /// (`RTE-110`, `SEC-110`, `TM-14`, `INV-09`).
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Invalid`] if `feedback_id`, `trace_id`, `query_hash_hex`, logits, or
    /// `redacted_query` violate length/PII validation rules, [`StoreError::Conflict`] on duplicate
    /// `(tenant, feedback_id)`, or [`StoreError::Backend`] on database errors.
    pub fn record_discovery(
        &self,
        scope: &TenantScope,
        input: NewDiscoveryFeedback,
    ) -> Result<String, StoreError> {
        validate_new_feedback(&input)?;

        let candidate_json =
            serde_json::to_string(&input.candidate_group_ids).map_err(|_| StoreError::Backend)?;
        let logits_json =
            serde_json::to_string(&input.option_logits).map_err(|_| StoreError::Backend)?;
        let pred_set_json =
            serde_json::to_string(&input.prediction_set_ids).map_err(|_| StoreError::Backend)?;

        let conn = self.lock_conn()?;
        let inserted = conn.execute(
            "INSERT INTO routing_feedback (
                tenant, feedback_id, ts_ms, trace_id, session_id, query_hash_hex, redacted_query,
                candidate_group_ids_json, option_logits_json, selected_group_id,
                prediction_set_ids_json, abstained_reason, calibration_id, outcome,
                attributed_operation_id, attributed_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, NULL, NULL)",
            params![
                scope.tenant().as_str(),
                input.feedback_id,
                input.ts.unix_millis(),
                input.trace_id,
                input.session_id,
                input.query_hash_hex,
                input.redacted_query,
                candidate_json,
                logits_json,
                input.selected_group_id,
                pred_set_json,
                input.abstained_reason,
                input.calibration_id,
                FeedbackOutcomeKind::None.as_str(),
            ],
        );

        match inserted {
            Ok(_) => Ok(input.feedback_id),
            Err(rusqlite::Error::SqliteFailure(err, _))
                if err.code == rusqlite::ErrorCode::ConstraintViolation =>
            {
                Err(StoreError::Conflict)
            }
            Err(_) => Err(StoreError::Backend),
        }
    }

    /// Attributes matching unattributed discovery rows under `scope` from a server-observed
    /// execution within the [`ATTRIBUTION_WINDOW_MS`] (`5-minute`) window (`RTE-110`, `TM-14`).
    ///
    /// Rules (`RTE-110`):
    /// - Only rows owned by `scope.tenant()` (`INV-09`) with `outcome == NONE` and
    ///   `0 <= observed_at - row.ts <= 300_000 ms` matching `trace_id` or `session_id` qualify.
    /// - If `executed_group_id` matches the predicted `selected_group_id` or belongs to
    ///   `prediction_set_ids`: `ExecutedOk` when `succeeded`, `ExecutedError` when `!succeeded`.
    /// - Otherwise: `ExecutedOther` (implicit negative signal).
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Invalid`] if identifiers are empty or invalid, or
    /// [`StoreError::Backend`] on database errors.
    #[allow(clippy::too_many_arguments)]
    pub fn attribute_server_execution(
        &self,
        scope: &TenantScope,
        trace_id: Option<&str>,
        session_id: Option<&str>,
        executed_group_id: &str,
        executed_op_id: &str,
        succeeded: bool,
        observed_at: Timestamp,
    ) -> Result<usize, StoreError> {
        let trace_opt = trace_id.map(str::trim).filter(|s| !s.is_empty());
        let session_opt = session_id.map(str::trim).filter(|s| !s.is_empty());
        if trace_opt.is_none() && session_opt.is_none() {
            return Err(StoreError::Invalid {
                field: "trace_id",
                reason: "at least one of trace_id or session_id is required for server attribution",
            });
        }
        if executed_group_id.trim().is_empty() || executed_op_id.trim().is_empty() {
            return Err(StoreError::Invalid {
                field: "executed_op_id",
                reason: "executed_group_id and executed_op_id must not be empty",
            });
        }

        let observed_ms = observed_at.unix_millis();
        let tenant = scope.tenant().as_str();
        let mut conn = self.lock_conn()?;
        let tx = conn.transaction().map_err(|_| StoreError::Backend)?;

        let candidates: Vec<(String, Option<String>, String)> = {
            let mut stmt = tx
                .prepare(
                    "SELECT feedback_id, selected_group_id, prediction_set_ids_json
                     FROM routing_feedback
                     WHERE tenant = ?1
                       AND outcome = 'NONE'
                       AND ts_ms <= ?2
                       AND (?2 - ts_ms) <= ?3
                       AND ((?4 IS NOT NULL AND trace_id = ?4)
                            OR (?5 IS NOT NULL AND session_id = ?5))
                     ORDER BY ts_ms ASC, feedback_id ASC",
                )
                .map_err(|_| StoreError::Backend)?;
            let mut rows = stmt
                .query(params![
                    tenant,
                    observed_ms,
                    ATTRIBUTION_WINDOW_MS,
                    trace_opt,
                    session_opt,
                ])
                .map_err(|_| StoreError::Backend)?;

            let mut collected = Vec::new();
            while let Some(row) = rows.next().map_err(|_| StoreError::Backend)? {
                let fid: String = row.get(0).map_err(|_| StoreError::Backend)?;
                let sel: Option<String> = row.get(1).map_err(|_| StoreError::Backend)?;
                let pred_json: String = row.get(2).map_err(|_| StoreError::Backend)?;
                collected.push((fid, sel, pred_json));
            }
            collected
        };

        let mut updated = 0usize;
        for (fid, sel_opt, pred_json) in candidates {
            let pred_set: Vec<String> =
                serde_json::from_str(&pred_json).map_err(|_| StoreError::Backend)?;
            let matched_predicted = sel_opt.as_deref() == Some(executed_group_id)
                || pred_set.iter().any(|g| g == executed_group_id);
            let outcome = if matched_predicted {
                if succeeded {
                    FeedbackOutcomeKind::ExecutedOk
                } else {
                    FeedbackOutcomeKind::ExecutedError
                }
            } else {
                FeedbackOutcomeKind::ExecutedOther
            };

            let changed = tx
                .execute(
                    "UPDATE routing_feedback
                     SET outcome = ?3,
                         attributed_operation_id = ?4,
                         attributed_at_ms = ?5
                     WHERE tenant = ?1 AND feedback_id = ?2 AND outcome = 'NONE'",
                    params![tenant, fid, outcome.as_str(), executed_op_id, observed_ms,],
                )
                .map_err(|_| StoreError::Backend)?;
            updated = updated.saturating_add(changed);
        }

        tx.commit().map_err(|_| StoreError::Backend)?;
        Ok(updated)
    }

    /// Applies an explicit administrator `USER_LABEL` to `feedback_id` under `scope`, recording an
    /// immutable audit entry on `audit_log` before committing the label (`RTE-110`, `TM-14`, `INV-09`).
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Invalid`] if `admin_principal` or `reason` is empty,
    /// [`StoreError::NotFound`] if the feedback row is absent or owned by another tenant, or
    /// propagates audit/database storage errors without mutating the feedback row.
    #[allow(clippy::too_many_arguments)]
    pub fn apply_admin_user_label(
        &self,
        scope: &TenantScope,
        feedback_id: &str,
        gold_group_id: Option<&str>,
        admin_principal: &str,
        reason: &str,
        now: Timestamp,
        audit_log: &dyn AuditLog,
    ) -> Result<(), StoreError> {
        if feedback_id.trim().is_empty()
            || admin_principal.trim().is_empty()
            || reason.trim().is_empty()
        {
            return Err(StoreError::Invalid {
                field: "admin_principal",
                reason: "feedback_id, admin_principal, and reason must not be empty",
            });
        }
        if let Some(gid) = gold_group_id
            && gid.trim().is_empty()
        {
            return Err(StoreError::Invalid {
                field: "gold_group_id",
                reason: "gold_group_id cannot be blank when provided",
            });
        }

        let tenant = scope.tenant().as_str();
        let conn = self.lock_conn()?;
        let exists: Option<String> = conn
            .query_row(
                "SELECT feedback_id FROM routing_feedback WHERE tenant = ?1 AND feedback_id = ?2",
                params![tenant, feedback_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(|_| StoreError::Backend)?;
        if exists.is_none() {
            return Err(StoreError::NotFound);
        }
        drop(conn);

        // Append to tamper-evident tenant audit chain BEFORE mutating the feedback row (`TM-14`).
        let details = json!({
            "feedback_id": feedback_id,
            "gold_group_id": gold_group_id,
            "reason": reason,
        });
        let entry = AuditEntry {
            ts: now,
            actor_chain: vec![admin_principal.to_owned()],
            action: "routing.feedback.user_label".to_owned(),
            resource: format!("routing_feedback:{feedback_id}"),
            decision: "ALLOW".to_owned(),
            details_json: details.to_string(),
        };
        audit_log.append_chain(&AuditChain::Tenant(scope.clone()), &entry)?;

        let conn = self.lock_conn()?;
        let updated = conn
            .execute(
                "UPDATE routing_feedback
                 SET outcome = ?3,
                     attributed_operation_id = ?4,
                     attributed_at_ms = ?5
                 WHERE tenant = ?1 AND feedback_id = ?2",
                params![
                    tenant,
                    feedback_id,
                    FeedbackOutcomeKind::UserLabel.as_str(),
                    gold_group_id,
                    now.unix_millis(),
                ],
            )
            .map_err(|_| StoreError::Backend)?;
        if updated == 0 {
            return Err(StoreError::NotFound);
        }
        Ok(())
    }

    /// Fetches a single tenant-scoped feedback row (`INV-09`).
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Backend`] on query or decode failures.
    pub fn get(
        &self,
        scope: &TenantScope,
        feedback_id: &str,
    ) -> Result<Option<StoredFeedbackRow>, StoreError> {
        let conn = self.lock_conn()?;
        let mut stmt = conn
            .prepare(
                "SELECT tenant, feedback_id, ts_ms, trace_id, session_id, query_hash_hex,
                        redacted_query, candidate_group_ids_json, option_logits_json,
                        selected_group_id, prediction_set_ids_json, abstained_reason,
                        calibration_id, outcome, attributed_operation_id, attributed_at_ms
                 FROM routing_feedback
                 WHERE tenant = ?1 AND feedback_id = ?2",
            )
            .map_err(|_| StoreError::Backend)?;
        stmt.query_row(params![scope.tenant().as_str(), feedback_id], decode_row)
            .optional()
            .map_err(|_| StoreError::Backend)
    }

    /// Purges feedback rows under `scope` older than `retention_ms` relative to `now` (`SEC-110`,
    /// `INV-09`). Enforces `0 < retention_ms <= DEFAULT_FEEDBACK_RETENTION_MS` (7 days).
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Invalid`] if `retention_ms` is outside `1..=DEFAULT_FEEDBACK_RETENTION_MS`,
    /// or [`StoreError::Backend`] on database errors.
    pub fn purge_expired(
        &self,
        scope: &TenantScope,
        now: Timestamp,
        retention_ms: i64,
    ) -> Result<usize, StoreError> {
        if retention_ms <= 0 || retention_ms > DEFAULT_FEEDBACK_RETENTION_MS {
            return Err(StoreError::Invalid {
                field: "retention_ms",
                reason: "retention_ms must lie within 1..=604800000 ms (7 days)",
            });
        }
        let cutoff = now.unix_millis().saturating_sub(retention_ms);
        let conn = self.lock_conn()?;
        conn.execute(
            "DELETE FROM routing_feedback WHERE tenant = ?1 AND ts_ms < ?2",
            params![scope.tenant().as_str(), cutoff],
        )
        .map_err(|_| StoreError::Backend)
    }

    /// Read-only export of attributed (`EXECUTED_OK`, `EXECUTED_OTHER`, `USER_LABEL`) rows for
    /// offline calibration batch jobs (`RTE-112`: never mutates live router weights).
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Invalid`] if `limit == 0`, or [`StoreError::Backend`] on query error.
    pub fn export_calibration_samples(
        &self,
        scope: &TenantScope,
        limit: usize,
    ) -> Result<Vec<StoredFeedbackRow>, StoreError> {
        if limit == 0 {
            return Err(StoreError::Invalid {
                field: "limit",
                reason: "limit must be >= 1",
            });
        }
        let limit_i64 = i64::try_from(limit).unwrap_or(i64::MAX);
        let conn = self.lock_conn()?;
        let mut stmt = conn
            .prepare(
                "SELECT tenant, feedback_id, ts_ms, trace_id, session_id, query_hash_hex,
                        redacted_query, candidate_group_ids_json, option_logits_json,
                        selected_group_id, prediction_set_ids_json, abstained_reason,
                        calibration_id, outcome, attributed_operation_id, attributed_at_ms
                 FROM routing_feedback
                 WHERE tenant = ?1
                   AND outcome IN ('EXECUTED_OK', 'EXECUTED_OTHER', 'USER_LABEL')
                 ORDER BY ts_ms ASC, feedback_id ASC
                 LIMIT ?2",
            )
            .map_err(|_| StoreError::Backend)?;
        let mut rows = stmt
            .query(params![scope.tenant().as_str(), limit_i64])
            .map_err(|_| StoreError::Backend)?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().map_err(|_| StoreError::Backend)? {
            out.push(decode_row(row).map_err(|_| StoreError::Backend)?);
        }
        Ok(out)
    }
}

fn validate_new_feedback(input: &NewDiscoveryFeedback) -> Result<(), StoreError> {
    if input.feedback_id.trim().is_empty() || input.feedback_id.len() > 128 {
        return Err(StoreError::Invalid {
            field: "feedback_id",
            reason: "feedback_id must be 1..=128 characters",
        });
    }
    if input.trace_id.trim().is_empty() || input.trace_id.len() > 128 {
        return Err(StoreError::Invalid {
            field: "trace_id",
            reason: "trace_id must be 1..=128 characters",
        });
    }
    if input.query_hash_hex.len() != 64
        || !input
            .query_hash_hex
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
    {
        return Err(StoreError::Invalid {
            field: "query_hash_hex",
            reason: "query_hash_hex must be a 64-char lowercase hex digest (SEC-110)",
        });
    }
    if input.option_logits.iter().any(|v| !v.is_finite()) {
        return Err(StoreError::Invalid {
            field: "option_logits",
            reason: "option_logits must be finite floats",
        });
    }
    if let Some(q) = input.redacted_query.as_deref() {
        if q.chars().count() > MAX_REDACTED_QUERY_CHARS {
            return Err(StoreError::Invalid {
                field: "redacted_query",
                reason: "redacted_query exceeds MAX_REDACTED_QUERY_CHARS (SEC-110)",
            });
        }
        if contains_unredacted_pii_or_secret(q) {
            return Err(StoreError::Invalid {
                field: "redacted_query",
                reason: "redacted_query contains unredacted email/secret/control bytes (SEC-110)",
            });
        }
    }
    Ok(())
}

fn contains_unredacted_pii_or_secret(text: &str) -> bool {
    if text.bytes().any(|b| b < 0x20 || b == 0x7f) {
        return true;
    }
    let lower = text.to_ascii_lowercase();
    for tok in lower.split_whitespace() {
        if tok.contains('@') && tok.contains('.') {
            return true;
        }
        if tok.starts_with("sk-")
            || tok.starts_with("ghp_")
            || tok.starts_with("glpat-")
            || tok.starts_with("akia")
        {
            return true;
        }
    }
    false
}

fn decode_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<StoredFeedbackRow> {
    let candidate_json: String = row.get(7)?;
    let logits_json: String = row.get(8)?;
    let pred_json: String = row.get(10)?;
    let outcome_raw: String = row.get(13)?;

    let candidate_group_ids: Vec<String> = serde_json::from_str(&candidate_json).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(7, rusqlite::types::Type::Text, Box::new(e))
    })?;
    let option_logits: Vec<f32> = serde_json::from_str(&logits_json).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(8, rusqlite::types::Type::Text, Box::new(e))
    })?;
    let prediction_set_ids: Vec<String> = serde_json::from_str(&pred_json).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(10, rusqlite::types::Type::Text, Box::new(e))
    })?;
    let outcome = FeedbackOutcomeKind::from_str_code(&outcome_raw).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            13,
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "invalid FeedbackOutcomeKind",
            )),
        )
    })?;

    Ok(StoredFeedbackRow {
        tenant: row.get(0)?,
        feedback_id: row.get(1)?,
        ts_ms: row.get(2)?,
        trace_id: row.get(3)?,
        session_id: row.get(4)?,
        query_hash_hex: row.get(5)?,
        redacted_query: row.get(6)?,
        candidate_group_ids,
        option_logits,
        selected_group_id: row.get(9)?,
        prediction_set_ids,
        abstained_reason: row.get(11)?,
        calibration_id: row.get(12)?,
        outcome,
        attributed_operation_id: row.get(14)?,
        attributed_at_ms: row.get(15)?,
    })
}
