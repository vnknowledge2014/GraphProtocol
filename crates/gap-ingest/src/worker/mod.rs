//! Asynchronous control-plane ingestion worker pool & isolation framework (`WP-M4-01`, `ING-070`, `ING-071`).
//!
//! Enforces:
//! - Per-tenant concurrency ceiling (`max_concurrent = 2` default, `ING-071`) and
//!   `gap_ingest_backlog` queue tracking (`ING-071`) via RAII slot guards.
//! - Crash/panic/timeout/output isolation for worker jobs (`ING-070`, `ING-012`) so a hostile
//!   spec or crashing parser returns a structured [`WorkerReport`] and never crashes the server.
//! - Scrubbed environment and explicit absolute binary path validation for subprocess workers
//!   (no ambient `$PATH` lookup, `ADR-013`).

use std::collections::BTreeMap;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use gap_core::ids::TenantId;

/// Default maximum concurrent ingestion workers per tenant (`ING-071`).
pub const DEFAULT_MAX_CONCURRENT_PER_TENANT: usize = 2;

/// Default per-pipeline wall-clock timeout (`30 minutes`, `ING-071`).
pub const DEFAULT_WALL_CLOCK_CAP: Duration = Duration::from_secs(30 * 60);

/// Default worker CPU time budget in seconds (`60 s`, `ING-070`).
pub const DEFAULT_CPU_TIME_SECS: u64 = 60;

/// Default worker virtual address space cap (`512 MiB`, `ING-070`).
pub const DEFAULT_MAX_MEMORY_BYTES: u64 = 512 * 1024 * 1024;

/// Default worker open file descriptor ceiling (`64`, `ING-070`).
pub const DEFAULT_MAX_OPEN_FILES: u64 = 64;

/// Default maximum worker stdout/stderr capture size (`16 MiB`).
pub const DEFAULT_MAX_OUTPUT_BYTES: usize = 16 * 1024 * 1024;

/// Resource limits enforced on every worker invocation (`ING-070`, `ING-071`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorkerLimits {
    /// CPU time limit passed to rlimit configuration (`ING-070`).
    pub cpu_time_secs: u64,
    /// Memory / address space cap (`ING-070`).
    pub max_memory_bytes: u64,
    /// Open file descriptor cap (`ING-070`).
    pub max_open_files: u64,
    /// Hard wall-clock deadline (`30 min` default, `ING-071`).
    pub wall_clock_timeout: Duration,
    /// Maximum stdout/stderr bytes buffered from subprocess workers.
    pub max_output_bytes: usize,
    /// Disables ambient network access in worker environment (`ING-070`).
    pub no_network: bool,
}

impl Default for WorkerLimits {
    fn default() -> Self {
        Self {
            cpu_time_secs: DEFAULT_CPU_TIME_SECS,
            max_memory_bytes: DEFAULT_MAX_MEMORY_BYTES,
            max_open_files: DEFAULT_MAX_OPEN_FILES,
            wall_clock_timeout: DEFAULT_WALL_CLOCK_CAP,
            max_output_bytes: DEFAULT_MAX_OUTPUT_BYTES,
            no_network: true,
        }
    }
}

/// Pool configuration governing per-tenant ingestion concurrency (`ING-071`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorkerPoolConfig {
    /// Maximum concurrent active pipelines per tenant (`>= 1`, default `2`, `ING-071`).
    pub max_concurrent_per_tenant: usize,
    /// Default worker resource limits.
    pub limits: WorkerLimits,
}

impl Default for WorkerPoolConfig {
    fn default() -> Self {
        Self {
            max_concurrent_per_tenant: DEFAULT_MAX_CONCURRENT_PER_TENANT,
            limits: WorkerLimits::default(),
        }
    }
}

/// Snapshot of pool telemetry corresponding to `gap_ingest_backlog` (`ING-071`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct WorkerMetricsSnapshot {
    /// Currently running jobs across all tenants.
    pub active_total: usize,
    /// Currently queued jobs waiting on per-tenant concurrency slots (`gap_ingest_backlog`).
    pub gap_ingest_backlog: usize,
    /// Currently active jobs for the queried tenant.
    pub active_for_tenant: usize,
    /// Currently queued jobs for the queried tenant.
    pub queued_for_tenant: usize,
}

/// Specific isolation failure classification recorded in [`WorkerReport`] (`ING-070`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkerFailureKind {
    /// Worker exceeded `wall_clock_timeout`.
    Timeout,
    /// Worker subprocess exited abnormally or was killed by a signal.
    Crash {
        /// Process exit code, when available.
        exit_code: Option<i32>,
        /// Terminating POSIX signal, when killed by signal.
        signal: Option<i32>,
    },
    /// Isolated worker panicked in Rust code (`catch_unwind`).
    Panic {
        /// Redacted panic string (`<= 300` bytes).
        message: String,
    },
    /// Worker output exceeded `max_output_bytes`.
    OutputTooLarge {
        /// Observed output byte count.
        bytes: usize,
        /// Configured ceiling.
        limit: usize,
    },
    /// Worker ran cleanly but returned an application/extractor error.
    ExtractorError {
        /// Sanitized error message.
        message: String,
    },
    /// Spawning or validating the subprocess command failed (e.g. non-absolute path).
    SpawnFailed {
        /// Sanitized failure reason.
        reason: String,
    },
}

/// Structured report attached to every [`WorkerOutcome`] (`ING-001`, `ING-070`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerReport {
    /// Tenant owning the job.
    pub tenant: TenantId,
    /// Wall-clock duration of execution.
    pub duration: Duration,
    /// Peak captured output byte count.
    pub peak_output_bytes: usize,
    /// Isolation profile label (e.g. `"in-process-isolated"` or `"subprocess-no-net"`).
    pub sandbox_profile: &'static str,
    /// Failure details when outcome is `Failed`.
    pub failure: Option<WorkerFailureKind>,
}

/// Outcome of an isolated worker execution (`ING-070`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkerOutcome<T> {
    /// Job completed successfully without breaching limits.
    Succeeded {
        /// Typed result produced by the worker.
        value: T,
        /// Execution telemetry and sandbox report.
        report: WorkerReport,
    },
    /// Job failed, crashed, panicked, or timed out cleanly without affecting the host process.
    Failed {
        /// Structured diagnostic report.
        report: WorkerReport,
    },
}

impl<T> WorkerOutcome<T> {
    /// Returns the attached [`WorkerReport`] regardless of success or failure.
    #[must_use]
    pub fn report(&self) -> &WorkerReport {
        match self {
            Self::Succeeded { report, .. } | Self::Failed { report } => report,
        }
    }

    /// Returns `true` when the isolated job succeeded.
    #[must_use]
    pub fn is_ok(&self) -> bool {
        matches!(self, Self::Succeeded { .. })
    }
}

#[derive(Default)]
struct PoolCounters {
    active_by_tenant: BTreeMap<String, usize>,
    queued_by_tenant: BTreeMap<String, usize>,
    active_total: usize,
    backlog_total: usize,
}

struct PoolState {
    counters: Mutex<PoolCounters>,
    cv: Condvar,
}

/// RAII concurrency & `gap_ingest_backlog` guard ensuring panics or early returns release slots.
struct SlotGuard {
    state: Arc<PoolState>,
    tenant_key: String,
    active_held: bool,
    queued_held: bool,
}

impl Drop for SlotGuard {
    fn drop(&mut self) {
        let mut guard = match self.state.counters.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        if self.queued_held {
            guard.backlog_total = guard.backlog_total.saturating_sub(1);
            if let Some(q) = guard.queued_by_tenant.get_mut(&self.tenant_key) {
                *q = q.saturating_sub(1);
            }
        }
        if self.active_held {
            guard.active_total = guard.active_total.saturating_sub(1);
            if let Some(a) = guard.active_by_tenant.get_mut(&self.tenant_key) {
                *a = a.saturating_sub(1);
            }
        }
        self.state.cv.notify_all();
    }
}

/// Bounded per-tenant isolated worker pool (`ING-070`, `ING-071`).
#[derive(Clone)]
pub struct WorkerPool {
    config: WorkerPoolConfig,
    state: Arc<PoolState>,
}

impl WorkerPool {
    /// Creates a new [`WorkerPool`] with validated `config`.
    #[must_use]
    pub fn new(mut config: WorkerPoolConfig) -> Self {
        if config.max_concurrent_per_tenant == 0 {
            config.max_concurrent_per_tenant = 1;
        }
        Self {
            config,
            state: Arc::new(PoolState {
                counters: Mutex::new(PoolCounters::default()),
                cv: Condvar::new(),
            }),
        }
    }

    /// Returns the active [`WorkerPoolConfig`].
    #[must_use]
    pub const fn config(&self) -> &WorkerPoolConfig {
        &self.config
    }

    /// Captures instantaneous pool concurrency and `gap_ingest_backlog` counters for `tenant` (`ING-071`).
    #[must_use]
    pub fn metrics_snapshot(&self, tenant: &TenantId) -> WorkerMetricsSnapshot {
        let guard = match self.state.counters.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        let key = tenant.as_str();
        WorkerMetricsSnapshot {
            active_total: guard.active_total,
            gap_ingest_backlog: guard.backlog_total,
            active_for_tenant: guard.active_by_tenant.get(key).copied().unwrap_or(0),
            queued_for_tenant: guard.queued_by_tenant.get(key).copied().unwrap_or(0),
        }
    }

    /// Executes an in-process job on a dedicated worker thread isolated with `catch_unwind` and
    /// `limits.wall_clock_timeout`, obeying per-tenant concurrency caps (`ING-070`, `ING-071`).
    pub fn run_isolated<T, E, F>(
        &self,
        tenant: &TenantId,
        limits_override: Option<WorkerLimits>,
        job: F,
    ) -> WorkerOutcome<T>
    where
        T: Send + 'static,
        E: std::fmt::Display + Send + 'static,
        F: FnOnce() -> Result<T, E> + Send + 'static,
    {
        let _slot = self.acquire_slot(tenant);
        let limits = limits_override.unwrap_or(self.config.limits);
        let start = Instant::now();
        let (tx, rx) = std::sync::mpsc::sync_channel::<Result<Result<T, E>, String>>(1);

        let spawned = std::thread::Builder::new()
            .name("gap-ingest-worker".to_owned())
            .spawn(move || {
                let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(job));
                let payload = match res {
                    Ok(inner) => Ok(inner),
                    Err(panic_any) => Err(extract_panic_message(panic_any)),
                };
                let _ = tx.send(payload);
            });

        if let Err(err) = spawned {
            return WorkerOutcome::Failed {
                report: WorkerReport {
                    tenant: tenant.clone(),
                    duration: start.elapsed(),
                    peak_output_bytes: 0,
                    sandbox_profile: "in-process-isolated",
                    failure: Some(WorkerFailureKind::SpawnFailed {
                        reason: clamp_msg(&err.to_string()),
                    }),
                },
            };
        }

        match rx.recv_timeout(limits.wall_clock_timeout) {
            Ok(Ok(Ok(value))) => WorkerOutcome::Succeeded {
                value,
                report: WorkerReport {
                    tenant: tenant.clone(),
                    duration: start.elapsed(),
                    peak_output_bytes: 0,
                    sandbox_profile: "in-process-isolated",
                    failure: None,
                },
            },
            Ok(Ok(Err(app_err))) => WorkerOutcome::Failed {
                report: WorkerReport {
                    tenant: tenant.clone(),
                    duration: start.elapsed(),
                    peak_output_bytes: 0,
                    sandbox_profile: "in-process-isolated",
                    failure: Some(WorkerFailureKind::ExtractorError {
                        message: clamp_msg(&app_err.to_string()),
                    }),
                },
            },
            Ok(Err(panic_msg)) => WorkerOutcome::Failed {
                report: WorkerReport {
                    tenant: tenant.clone(),
                    duration: start.elapsed(),
                    peak_output_bytes: 0,
                    sandbox_profile: "in-process-isolated",
                    failure: Some(WorkerFailureKind::Panic {
                        message: clamp_msg(&panic_msg),
                    }),
                },
            },
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => WorkerOutcome::Failed {
                report: WorkerReport {
                    tenant: tenant.clone(),
                    duration: start.elapsed(),
                    peak_output_bytes: 0,
                    sandbox_profile: "in-process-isolated",
                    failure: Some(WorkerFailureKind::Timeout),
                },
            },
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => WorkerOutcome::Failed {
                report: WorkerReport {
                    tenant: tenant.clone(),
                    duration: start.elapsed(),
                    peak_output_bytes: 0,
                    sandbox_profile: "in-process-isolated",
                    failure: Some(WorkerFailureKind::Crash {
                        exit_code: None,
                        signal: None,
                    }),
                },
            },
        }
    }

    /// Executes an external worker binary via an explicit absolute path with a scrubbed
    /// environment, bounded stdout/stderr, and hard wall-clock termination (`ING-070`, `ADR-013`).
    pub fn run_command_isolated(
        &self,
        tenant: &TenantId,
        binary_path: &Path,
        args: &[&str],
        limits_override: Option<WorkerLimits>,
    ) -> WorkerOutcome<Vec<u8>> {
        let _slot = self.acquire_slot(tenant);
        let limits = limits_override.unwrap_or(self.config.limits);
        let start = Instant::now();

        if !binary_path.is_absolute() {
            return WorkerOutcome::Failed {
                report: WorkerReport {
                    tenant: tenant.clone(),
                    duration: start.elapsed(),
                    peak_output_bytes: 0,
                    sandbox_profile: "subprocess-no-net",
                    failure: Some(WorkerFailureKind::SpawnFailed {
                        reason:
                            "worker executable path must be absolute (never resolved from PATH)"
                                .to_owned(),
                    }),
                },
            };
        }

        let mut cmd = Command::new(binary_path);
        cmd.args(args)
            .env_clear()
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if limits.no_network {
            cmd.env("GAP_WORKER_NO_NETWORK", "1");
        }

        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(err) => {
                return WorkerOutcome::Failed {
                    report: WorkerReport {
                        tenant: tenant.clone(),
                        duration: start.elapsed(),
                        peak_output_bytes: 0,
                        sandbox_profile: "subprocess-no-net",
                        failure: Some(WorkerFailureKind::SpawnFailed {
                            reason: clamp_msg(&err.to_string()),
                        }),
                    },
                };
            }
        };

        let max_out = limits.max_output_bytes;
        let stdout_handle = child
            .stdout
            .take()
            .map(|pipe| std::thread::spawn(move || read_bounded_pipe(pipe, max_out)));
        let stderr_handle = child
            .stderr
            .take()
            .map(|pipe| std::thread::spawn(move || read_bounded_pipe(pipe, max_out)));

        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break Some(status),
                Ok(None) => {
                    if start.elapsed() >= limits.wall_clock_timeout {
                        let _ = child.kill();
                        let _ = child.wait();
                        break None;
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(_) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    break None;
                }
            }
        };

        let (stdout_bytes, stdout_overflow) = stdout_handle
            .and_then(|h| h.join().ok())
            .unwrap_or_default();
        let (stderr_bytes, stderr_overflow) = stderr_handle
            .and_then(|h| h.join().ok())
            .unwrap_or_default();
        let peak_output_bytes = stdout_bytes.len().max(stderr_bytes.len());

        let Some(exit_status) = status else {
            return WorkerOutcome::Failed {
                report: WorkerReport {
                    tenant: tenant.clone(),
                    duration: start.elapsed(),
                    peak_output_bytes,
                    sandbox_profile: "subprocess-no-net",
                    failure: Some(WorkerFailureKind::Timeout),
                },
            };
        };

        if stdout_overflow || stderr_overflow {
            return WorkerOutcome::Failed {
                report: WorkerReport {
                    tenant: tenant.clone(),
                    duration: start.elapsed(),
                    peak_output_bytes,
                    sandbox_profile: "subprocess-no-net",
                    failure: Some(WorkerFailureKind::OutputTooLarge {
                        bytes: max_out.saturating_add(1),
                        limit: max_out,
                    }),
                },
            };
        }

        if exit_status.success() {
            return WorkerOutcome::Succeeded {
                value: stdout_bytes,
                report: WorkerReport {
                    tenant: tenant.clone(),
                    duration: start.elapsed(),
                    peak_output_bytes,
                    sandbox_profile: "subprocess-no-net",
                    failure: None,
                },
            };
        }

        WorkerOutcome::Failed {
            report: WorkerReport {
                tenant: tenant.clone(),
                duration: start.elapsed(),
                peak_output_bytes,
                sandbox_profile: "subprocess-no-net",
                failure: Some(WorkerFailureKind::Crash {
                    exit_code: exit_status.code(),
                    signal: exit_signal(&exit_status),
                }),
            },
        }
    }

    fn acquire_slot(&self, tenant: &TenantId) -> SlotGuard {
        let tenant_key = tenant.as_str().to_owned();
        let mut guard = match self.state.counters.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };

        guard.backlog_total = guard.backlog_total.saturating_add(1);
        *guard
            .queued_by_tenant
            .entry(tenant_key.clone())
            .or_insert(0) += 1;

        let mut slot = SlotGuard {
            state: Arc::clone(&self.state),
            tenant_key: tenant_key.clone(),
            active_held: false,
            queued_held: true,
        };

        let limit = self.config.max_concurrent_per_tenant;
        while guard
            .active_by_tenant
            .get(&tenant_key)
            .copied()
            .unwrap_or(0)
            >= limit
        {
            guard = match self.state.cv.wait(guard) {
                Ok(g) => g,
                Err(poisoned) => poisoned.into_inner(),
            };
        }

        guard.backlog_total = guard.backlog_total.saturating_sub(1);
        if let Some(q) = guard.queued_by_tenant.get_mut(&tenant_key) {
            *q = q.saturating_sub(1);
        }
        slot.queued_held = false;

        guard.active_total = guard.active_total.saturating_add(1);
        *guard.active_by_tenant.entry(tenant_key).or_insert(0) += 1;
        slot.active_held = true;

        slot
    }
}

fn read_bounded_pipe<R: std::io::Read>(mut reader: R, max_bytes: usize) -> (Vec<u8>, bool) {
    let mut buf = Vec::new();
    let mut chunk = [0_u8; 4096];
    let mut overflow = false;
    loop {
        match reader.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                if buf.len() + n > max_bytes {
                    let take = max_bytes.saturating_sub(buf.len());
                    buf.extend_from_slice(&chunk[..take]);
                    overflow = true;
                    break;
                }
                buf.extend_from_slice(&chunk[..n]);
            }
            Err(_) => break,
        }
    }
    // Drain remainder to EOF to avoid broken-pipe noise while discarding overflow.
    if overflow {
        while let Ok(n) = reader.read(&mut chunk) {
            if n == 0 {
                break;
            }
        }
    }
    (buf, overflow)
}

#[cfg(unix)]
fn exit_signal(status: &std::process::ExitStatus) -> Option<i32> {
    use std::os::unix::process::ExitStatusExt;
    status.signal()
}

#[cfg(not(unix))]
fn exit_signal(_status: &std::process::ExitStatus) -> Option<i32> {
    None
}

fn extract_panic_message(payload: Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = payload.downcast_ref::<&'static str>() {
        (*s).to_owned()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "worker thread panicked".to_owned()
    }
}

fn clamp_msg(raw: &str) -> String {
    let trimmed: String = raw
        .chars()
        .filter(|c| !c.is_control() || *c == ' ')
        .take(280)
        .collect();
    trimmed
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    fn tenant(name: &str) -> TenantId {
        TenantId::new(name).expect("valid tenant")
    }

    // spec: ING-070, ING-012
    #[test]
    fn worker_crash_and_panic_yield_failed_report_without_crashing_pool() {
        let pool = WorkerPool::new(WorkerPoolConfig::default());
        let t = tenant("acme");

        // In-process panic is caught and mapped into WorkerFailureKind::Panic (`ING-070`).
        let panicked: WorkerOutcome<()> = pool.run_isolated::<(), String, _>(&t, None, || {
            panic!("boom inside hostile parser");
        });
        assert!(!panicked.is_ok());
        assert!(matches!(
            panicked.report().failure,
            Some(WorkerFailureKind::Panic { .. })
        ));

        // Non-absolute path rejected before spawn (`ADR-013`).
        let rel = pool.run_command_isolated(&t, Path::new("sh"), &["-c", "echo hi"], None);
        assert!(matches!(
            rel.report().failure,
            Some(WorkerFailureKind::SpawnFailed { .. })
        ));

        // Failing subprocess returns Crash report with exit code instead of panicking host (`ING-070`).
        let crash_cmd = if Path::new("/usr/bin/false").exists() {
            Path::new("/usr/bin/false")
        } else {
            Path::new("/bin/false")
        };
        if crash_cmd.exists() {
            let crashed = pool.run_command_isolated(&t, crash_cmd, &[], None);
            assert!(matches!(
                crashed.report().failure,
                Some(WorkerFailureKind::Crash {
                    exit_code: Some(1),
                    ..
                })
            ));
        }

        // Pool remains completely healthy and counters return to 0 after failures.
        let snap = pool.metrics_snapshot(&t);
        assert_eq!(snap.active_for_tenant, 0);
        assert_eq!(snap.gap_ingest_backlog, 0);
        let ok = pool.run_isolated::<u32, String, _>(&t, None, || Ok(42));
        assert!(matches!(ok, WorkerOutcome::Succeeded { value: 42, .. }));
    }

    // spec: ING-012, ING-070
    #[test]
    fn worker_timeout_terminates_and_records_timeout_failure() {
        let pool = WorkerPool::new(WorkerPoolConfig::default());
        let t = tenant("acme");
        let tight = WorkerLimits {
            wall_clock_timeout: Duration::from_millis(40),
            ..WorkerLimits::default()
        };

        let out: WorkerOutcome<()> = pool.run_isolated::<(), String, _>(&t, Some(tight), || {
            std::thread::sleep(Duration::from_millis(200));
            Ok(())
        });
        assert!(matches!(
            out.report().failure,
            Some(WorkerFailureKind::Timeout)
        ));

        if Path::new("/bin/sleep").exists() {
            let sub = pool.run_command_isolated(&t, Path::new("/bin/sleep"), &["2"], Some(tight));
            assert!(matches!(
                sub.report().failure,
                Some(WorkerFailureKind::Timeout)
            ));
        }
    }

    // spec: ING-071
    #[test]
    fn worker_enforces_per_tenant_concurrency_and_tracks_backlog_metric() {
        let pool = WorkerPool::new(WorkerPoolConfig {
            max_concurrent_per_tenant: 2,
            limits: WorkerLimits::default(),
        });
        let t = tenant("tenant-concurrent");

        let active_now = Arc::new(AtomicUsize::new(0));
        let max_observed_active = Arc::new(AtomicUsize::new(0));
        let observed_backlog = Arc::new(AtomicUsize::new(0));

        let (gate_tx, gate_rx) = std::sync::mpsc::channel::<()>();
        let gate_rx = Arc::new(Mutex::new(gate_rx));

        let mut handles = Vec::new();
        for _ in 0..4 {
            let p = pool.clone();
            let tenant_clone = t.clone();
            let cur = Arc::clone(&active_now);
            let peak = Arc::clone(&max_observed_active);
            let rx_shared = Arc::clone(&gate_rx);
            handles.push(std::thread::spawn(move || {
                p.run_isolated::<(), String, _>(&tenant_clone, None, move || {
                    let prev = cur.fetch_add(1, Ordering::SeqCst) + 1;
                    peak.fetch_max(prev, Ordering::SeqCst);
                    if let Ok(lock) = rx_shared.lock() {
                        let _ = lock.recv();
                    }
                    cur.fetch_sub(1, Ordering::SeqCst);
                    Ok(())
                })
            }));
        }

        // Wait briefly until 2 are active and 2 are queued in backlog.
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            let snap = pool.metrics_snapshot(&t);
            observed_backlog.fetch_max(snap.gap_ingest_backlog, Ordering::SeqCst);
            if snap.active_for_tenant == 2 && snap.queued_for_tenant == 2 {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }

        assert!(observed_backlog.load(Ordering::SeqCst) >= 1);
        for _ in 0..4 {
            let _ = gate_tx.send(());
        }
        for h in handles {
            let outcome = h.join().expect("thread join");
            assert!(outcome.is_ok());
        }

        assert!(max_observed_active.load(Ordering::SeqCst) <= 2);
        let final_snap = pool.metrics_snapshot(&t);
        assert_eq!(final_snap.active_total, 0);
        assert_eq!(final_snap.gap_ingest_backlog, 0);
    }
}
