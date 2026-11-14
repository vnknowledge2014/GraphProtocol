//! SQL driver abstraction and cancellation-aware SQLite implementation (`CMP-050`, `CMP-052`).

use std::collections::BTreeMap;
use std::future::Future;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use gap_ngc_sdk::connector::CallScope;
use rusqlite::types::ValueRef;
use thiserror::Error;
use tokio::time::Instant;

use crate::config::SqlType;

/// Typed SQL bind parameter produced exclusively by [`crate::template`].
#[derive(Debug, Clone, PartialEq)]
pub enum SqlParam {
    Null,
    Bool(bool),
    Integer(i64),
    Real(f64),
    Text(String),
    Blob(Vec<u8>),
    Array(Vec<SqlParam>),
}

/// Typed SQL result cell returned by a [`Driver`].
#[derive(Debug, Clone, PartialEq)]
pub enum SqlValue {
    Null,
    Bool(bool),
    Integer(i64),
    Real(f64),
    Text(String),
    Blob(Vec<u8>),
}

impl SqlValue {
    /// Converts a result cell into a protobuf [`prost_types::Value`], guided by its declared [`SqlType`] when present.
    #[must_use]
    pub fn into_prost_typed(self, sql_type: Option<&SqlType>) -> prost_types::Value {
        use prost_types::value::Kind;
        let kind = match (self, sql_type) {
            (SqlValue::Null, _) => Kind::NullValue(0),
            (SqlValue::Bool(b), _) => Kind::BoolValue(b),
            (SqlValue::Integer(n), Some(SqlType::Boolean)) => Kind::BoolValue(n != 0),
            (SqlValue::Integer(i), _) => Kind::NumberValue(i as f64),
            (SqlValue::Real(f), _) => Kind::NumberValue(if f == 0.0 { 0.0 } else { f }),
            (SqlValue::Text(s), Some(SqlType::Json)) => {
                if let Ok(json_val) = serde_json::from_str::<serde_json::Value>(&s) {
                    return gap_ngc_sdk::error::json_to_prost_value(&json_val);
                }
                Kind::StringValue(s)
            }
            (SqlValue::Text(s), _) => Kind::StringValue(s),
            (SqlValue::Blob(b), _) => Kind::StringValue(encode_hex(&b)),
        };
        prost_types::Value { kind: Some(kind) }
    }

    /// Converts a result cell into a protobuf [`prost_types::Value`].
    #[must_use]
    pub fn into_prost(self) -> prost_types::Value {
        self.into_prost_typed(None)
    }
}

fn encode_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(char::from(HEX[usize::from(b >> 4)]));
        out.push(char::from(HEX[usize::from(b & 0x0f)]));
    }
    out
}

#[derive(Debug, Error)]
pub enum DriverError {
    #[error("SQL execution failed: {0}")]
    Sql(String),
    #[error("statement timeout ({0:?}) exceeded before execution")]
    Timeout(Duration),
    #[error("statement cancelled by caller")]
    Cancelled,
    #[error("connection error: {0}")]
    Connection(String),
    #[error("type conversion error: {0}")]
    TypeConversion(String),
}

impl DriverError {
    /// Translates driver failures into NGC error codes without leaking host details (`NGC-060`, `NGC-061`).
    #[must_use]
    pub fn into_ngc(self) -> gap_core::error::NgcErrorInfo {
        use gap_core::error::NgcErrorCode;
        match &self {
            DriverError::Timeout(_) => {
                gap_core::error::NgcErrorInfo::new(NgcErrorCode::DeadlineExceeded, self.to_string())
            }
            DriverError::Cancelled => gap_core::error::NgcErrorInfo::new(
                NgcErrorCode::Cancelled,
                "SQL statement cancelled",
            ),
            DriverError::Sql(msg) if msg.contains("interrupted") || msg.contains("deadline") => {
                gap_core::error::NgcErrorInfo::new(
                    NgcErrorCode::DeadlineExceeded,
                    "SQL statement cancelled or deadline exceeded",
                )
            }
            DriverError::TypeConversion(_) => {
                gap_core::error::NgcErrorInfo::new(NgcErrorCode::InvalidArgument, self.to_string())
            }
            _ => gap_core::error::NgcErrorInfo::new(NgcErrorCode::UpstreamError, self.to_string()),
        }
    }
}

pub type SqlRow = BTreeMap<String, SqlValue>;

/// Executes generated, parameterized read-only statements (`CMP-050`).
pub trait Driver: Send + Sync + 'static {
    /// Returns at most `row_cap` rows and observes `scope` cancellation and deadline.
    fn query(
        &self,
        sql: String,
        params: Vec<SqlParam>,
        row_cap: usize,
        scope: CallScope,
    ) -> impl Future<Output = Result<Vec<SqlRow>, DriverError>> + Send;
}

/// A SQLite connection guarded by a mutex and queried on the blocking pool.
#[derive(Clone)]
pub struct SqliteDriver {
    conn: Arc<Mutex<rusqlite::Connection>>,
}

impl std::fmt::Debug for SqliteDriver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SqliteDriver").finish_non_exhaustive()
    }
}

impl SqliteDriver {
    /// Opens an existing SQLite database read-only (`CMP-050`).
    pub fn open(path: &str) -> Result<Self, DriverError> {
        let conn = rusqlite::Connection::open_with_flags(
            path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(|e| DriverError::Connection(e.to_string()))?;
        Self::from_connection(conn)
    }

    /// Wraps an existing connection and switches it to `PRAGMA query_only = ON`.
    /// This is useful for an in-memory database initialized before handing it over.
    pub fn from_connection(conn: rusqlite::Connection) -> Result<Self, DriverError> {
        conn.pragma_update(None, "query_only", "ON")
            .map_err(|e| DriverError::Sql(e.to_string()))?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }
}

const WATCHDOG_RUNNING: u8 = 0;
const WATCHDOG_TIMEOUT: u8 = 1;
const WATCHDOG_CANCELLED: u8 = 2;

struct CancelBlockingQuery {
    state: Arc<AtomicU8>,
    done: Arc<(Mutex<bool>, Condvar)>,
    completed: bool,
}

impl Drop for CancelBlockingQuery {
    fn drop(&mut self) {
        if !self.completed {
            let _ = self.state.compare_exchange(
                WATCHDOG_RUNNING,
                WATCHDOG_CANCELLED,
                Ordering::AcqRel,
                Ordering::Acquire,
            );
            let (lock, cvar) = &*self.done;
            let _guard = lock
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            cvar.notify_all();
        }
    }
}

struct InterruptWatchdog {
    done: Arc<(Mutex<bool>, Condvar)>,
    thread: Option<JoinHandle<()>>,
}

impl InterruptWatchdog {
    fn start(
        interrupt: rusqlite::InterruptHandle,
        deadline: Instant,
        is_cancelled: impl Fn() -> bool + Send + 'static,
        state: Arc<AtomicU8>,
        done: Arc<(Mutex<bool>, Condvar)>,
        rt_handle: Option<tokio::runtime::Handle>,
    ) -> Result<Self, DriverError> {
        let done_worker = Arc::clone(&done);
        let thread = std::thread::Builder::new()
            .name("gap-sqlite-watchdog".into())
            .spawn(move || {
                let _rt_guard = rt_handle.as_ref().map(tokio::runtime::Handle::enter);
                let (lock, cvar) = &*done_worker;
                let mut guard = lock
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                while !*guard {
                    if is_cancelled() || state.load(Ordering::Acquire) == WATCHDOG_CANCELLED {
                        let _ = state.compare_exchange(
                            WATCHDOG_RUNNING,
                            WATCHDOG_CANCELLED,
                            Ordering::AcqRel,
                            Ordering::Acquire,
                        );
                        interrupt.interrupt();
                        break;
                    }
                    let now = Instant::now();
                    if now >= deadline || state.load(Ordering::Acquire) == WATCHDOG_TIMEOUT {
                        let _ = state.compare_exchange(
                            WATCHDOG_RUNNING,
                            WATCHDOG_TIMEOUT,
                            Ordering::AcqRel,
                            Ordering::Acquire,
                        );
                        interrupt.interrupt();
                        break;
                    }
                    let slice = deadline
                        .saturating_duration_since(now)
                        .min(Duration::from_millis(10));
                    let (next_guard, _) = cvar
                        .wait_timeout(guard, slice)
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    guard = next_guard;
                }
            })
            .map_err(|e| DriverError::Connection(format!("watchdog thread spawn: {e}")))?;

        Ok(Self {
            done,
            thread: Some(thread),
        })
    }
}

impl Drop for InterruptWatchdog {
    fn drop(&mut self) {
        {
            let (lock, cvar) = &*self.done;
            let mut finished = lock
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *finished = true;
            cvar.notify_all();
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn check_abort(state: &AtomicU8, is_cancelled: bool, deadline: Instant) -> Result<(), DriverError> {
    if is_cancelled || state.load(Ordering::Acquire) == WATCHDOG_CANCELLED {
        return Err(DriverError::Cancelled);
    }
    if state.load(Ordering::Acquire) == WATCHDOG_TIMEOUT || Instant::now() >= deadline {
        return Err(DriverError::Timeout(Duration::ZERO));
    }
    Ok(())
}

fn map_sqlite_error(
    err: rusqlite::Error,
    state: &AtomicU8,
    is_cancelled: bool,
    deadline: Instant,
) -> DriverError {
    if is_cancelled || state.load(Ordering::Acquire) == WATCHDOG_CANCELLED {
        return DriverError::Cancelled;
    }
    if state.load(Ordering::Acquire) == WATCHDOG_TIMEOUT || Instant::now() >= deadline {
        return DriverError::Timeout(Duration::ZERO);
    }
    if let rusqlite::Error::SqliteFailure(ref sqlite_err, _) = err
        && sqlite_err.code == rusqlite::ErrorCode::OperationInterrupted
    {
        return DriverError::Timeout(Duration::ZERO);
    }
    DriverError::Sql(err.to_string())
}

impl Driver for SqliteDriver {
    fn query(
        &self,
        sql: String,
        params: Vec<SqlParam>,
        row_cap: usize,
        scope: CallScope,
    ) -> impl Future<Output = Result<Vec<SqlRow>, DriverError>> + Send {
        let conn = Arc::clone(&self.conn);
        async move {
            if scope.cancel.is_cancelled() {
                return Err(DriverError::Cancelled);
            }
            if Instant::now() >= scope.deadline {
                return Err(DriverError::Timeout(Duration::ZERO));
            }
            let state = Arc::new(AtomicU8::new(WATCHDOG_RUNNING));
            let done = Arc::new((Mutex::new(false), Condvar::new()));
            let mut cancel_on_drop = CancelBlockingQuery {
                state: Arc::clone(&state),
                done: Arc::clone(&done),
                completed: false,
            };
            let rt_handle = tokio::runtime::Handle::try_current().ok();
            let worker_state = Arc::clone(&state);
            let worker_done = Arc::clone(&done);
            let cancel_token = scope.cancel.clone();
            let deadline = scope.deadline;
            let result = tokio::task::spawn_blocking(move || {
                let lock = conn
                    .lock()
                    .map_err(|_| DriverError::Connection("mutex poisoned".into()))?;
                check_abort(&worker_state, cancel_token.is_cancelled(), deadline)?;

                let watchdog_cancel = cancel_token.clone();
                let _watchdog = InterruptWatchdog::start(
                    lock.get_interrupt_handle(),
                    deadline,
                    move || watchdog_cancel.is_cancelled(),
                    Arc::clone(&worker_state),
                    worker_done,
                    rt_handle,
                )?;

                let mut stmt = lock.prepare(&sql).map_err(|e| {
                    map_sqlite_error(e, &worker_state, cancel_token.is_cancelled(), deadline)
                })?;
                let col_names: Vec<String> = stmt.column_names().map(str::to_owned).collect();

                let rusql_params: Vec<Box<dyn rusqlite::types::ToSql>> = params
                    .into_iter()
                    .map(
                        |p| -> Result<Box<dyn rusqlite::types::ToSql>, DriverError> {
                            Ok(match p {
                                SqlParam::Null => Box::new(rusqlite::types::Null),
                                SqlParam::Bool(b) => Box::new(b),
                                SqlParam::Integer(i) => Box::new(i),
                                SqlParam::Real(f) => Box::new(f),
                                SqlParam::Text(s) => Box::new(s),
                                SqlParam::Blob(b) => Box::new(b),
                                SqlParam::Array(_) => {
                                    return Err(DriverError::TypeConversion(
                                        "SQLite does not support array parameters".into(),
                                    ));
                                }
                            })
                        },
                    )
                    .collect::<Result<_, _>>()?;
                let param_refs: Vec<&dyn rusqlite::types::ToSql> =
                    rusql_params.iter().map(|b| b.as_ref()).collect();

                check_abort(&worker_state, cancel_token.is_cancelled(), deadline)?;
                let mut rows = stmt.query(param_refs.as_slice()).map_err(|e| {
                    map_sqlite_error(e, &worker_state, cancel_token.is_cancelled(), deadline)
                })?;
                let cap = row_cap.max(1);
                let mut result = Vec::with_capacity(cap.min(256));
                while result.len() < cap {
                    check_abort(&worker_state, cancel_token.is_cancelled(), deadline)?;
                    let row = rows.next().map_err(|e| {
                        map_sqlite_error(e, &worker_state, cancel_token.is_cancelled(), deadline)
                    })?;
                    let Some(row) = row else { break };
                    let mut map = BTreeMap::new();
                    for (i, col) in col_names.iter().enumerate() {
                        let val = match row.get_ref(i).map_err(|e| {
                            map_sqlite_error(
                                e,
                                &worker_state,
                                cancel_token.is_cancelled(),
                                deadline,
                            )
                        })? {
                            ValueRef::Null => SqlValue::Null,
                            ValueRef::Integer(n) => SqlValue::Integer(n),
                            ValueRef::Real(f) => SqlValue::Real(f),
                            ValueRef::Text(t) => SqlValue::Text(
                                std::str::from_utf8(t)
                                    .map_err(|e| DriverError::TypeConversion(e.to_string()))?
                                    .to_owned(),
                            ),
                            ValueRef::Blob(b) => SqlValue::Blob(b.to_vec()),
                        };
                        map.insert(col.clone(), val);
                    }
                    result.push(map);
                }
                Ok(result)
            })
            .await
            .map_err(|e| DriverError::Connection(format!("blocking query join: {e}")))?;
            cancel_on_drop.completed = true;
            result
        }
    }
}
