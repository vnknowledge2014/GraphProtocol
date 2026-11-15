//! OS sandbox launcher, doctor check, scrubbed environment, timeout kill, and bounded stdio readers (`SEC-070`, `SEC-071`, `SEC-072`).

use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use gap_core::error::{ErrorCode, GapError};
use gap_core::snapshot::TrustTier;

use crate::manifest::{AllowListError, ExecAllowList, VerifiedExecBinary};

use super::profile::{
    FORBIDDEN_ENV_KEYS, SandboxProfile, render_linux_bwrap_args, render_macos_seatbelt_profile,
};

/// Host OS sandbox mechanism detected or configured (`SEC-070`, `SEC-072`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SandboxBackendKind {
    /// macOS Seatbelt (`/usr/bin/sandbox-exec`).
    MacOsSeatbelt,
    /// Linux bubblewrap (`bwrap`).
    LinuxBwrap,
    /// No functional OS sandbox mechanism is available.
    Unavailable {
        /// Diagnostic reason why sandboxing cannot be applied.
        reason: String,
    },
}

/// Diagnostic report returned by [`doctor_check`] (`SEC-072`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxDoctorReport {
    /// Target host OS string (`std::env::consts::OS`).
    pub os: &'static str,
    /// Detected sandbox backend category.
    pub backend: SandboxBackendKind,
    /// Whether the platform sandbox executable was located.
    pub binary_present: bool,
    /// Whether a minimal probe invocation succeeded inside the OS sandbox.
    pub functional_probe_ok: bool,
    /// Human-readable status summary.
    pub detail: String,
}

/// Optional probe override for deterministic fail-closed testing (`SEC-071`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum SandboxBackendOverride {
    /// Probe the real host OS sandbox via [`doctor_check`].
    #[default]
    AutoProbe,
    /// Pretend no OS sandbox is available (for testing `T2` fail-closed & `T1` opt-in).
    ForceUnavailable(String),
}

/// Verified & sandbox-wrapped command ready to spawn (used by `CliConnector` and MCP stdio launches).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedSandboxedCommand {
    /// Executable passed directly to `std::process::Command::new` (no shell).
    pub program: PathBuf,
    /// Direct `argv` passed without shell interpretation (`CMP-060`).
    pub args: Vec<String>,
    /// Canonical working directory (`profile.work_dir`).
    pub cwd: PathBuf,
    /// Strictly scrubbed environment map.
    pub clean_env: BTreeMap<String, String>,
    /// Live-verified SHA-256 metadata of the underlying branch executable.
    pub verified_binary: VerifiedExecBinary,
    /// `true` when OS sandbox (`sandbox-exec`/`bwrap`) is wrapping the child.
    pub used_os_sandbox: bool,
    /// Prominent warnings (e.g. `SEC-071` unsandboxed `T1` fallback notice).
    pub warnings: Vec<String>,
}

/// Bounded output collected from a completed sandboxed child process (`SEC-070`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxOutput {
    /// Exit status code (`-1` if terminated without exit code).
    pub status_code: i32,
    /// Captured stdout bytes (strictly `<= profile.max_stdout_bytes`).
    pub stdout: Vec<u8>,
    /// Captured stderr bytes (strictly `<= profile.max_stderr_bytes`).
    pub stderr: Vec<u8>,
    /// Verified executable SHA-256 recorded at spawn time.
    pub verified_sha256: String,
    /// `true` when real OS sandboxing wrapped the process.
    pub used_os_sandbox: bool,
    /// Prominent warnings emitted during launch (e.g. `T1` unsandboxed opt-in).
    pub warnings: Vec<String>,
}

/// Which output pipe exceeded its byte ceiling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputStreamKind {
    /// Standard output.
    Stdout,
    /// Standard error.
    Stderr,
}

impl fmt::Display for OutputStreamKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Stdout => f.write_str("stdout"),
            Self::Stderr => f.write_str("stderr"),
        }
    }
}

/// Errors returned by [`SandboxLauncher`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SandboxError {
    /// Allow-list path/hash check failed before spawn (`SEC-061`).
    AllowList(AllowListError),
    /// No functional OS sandbox exists and policy forbids unsandboxed fallback (`SEC-071`).
    UnavailableFailClosed {
        /// Branch trust tier.
        tier: TrustTier,
        /// Diagnostic reason.
        reason: String,
    },
    /// Working directory setup failed.
    WorkDirInvalid(String),
    /// Process creation or OS wait failed.
    SpawnFailed(String),
    /// Child process exceeded wall-clock timeout and was terminated (`SEC-070`).
    TimedOut {
        /// Configured wall-clock deadline.
        timeout: Duration,
    },
    /// Child flooded `stdout` or `stderr` past `max_stdout_bytes`/`max_stderr_bytes` (`SEC-070`).
    OutputLimitExceeded {
        /// Which pipe overflowed.
        stream: OutputStreamKind,
        /// Configured byte ceiling.
        limit: usize,
    },
    /// Command returned non-zero exit code.
    NonZeroExit {
        /// Exit code returned by child.
        status_code: i32,
        /// Truncated stderr excerpt.
        stderr_preview: String,
    },
}

impl SandboxError {
    /// Maps a [`SandboxError`] into a gateway [`GapError`].
    #[must_use]
    pub fn into_gap_error(self) -> GapError {
        match self {
            Self::AllowList(err) => err.into_gap_error(),
            Self::UnavailableFailClosed { .. } => {
                GapError::new(ErrorCode::Forbidden, self.to_string())
            }
            Self::TimedOut { .. } => GapError::new(ErrorCode::BranchTimeout, self.to_string()),
            Self::OutputLimitExceeded { .. } => {
                GapError::new(ErrorCode::ResponseTooLarge, self.to_string())
            }
            Self::WorkDirInvalid(_) | Self::SpawnFailed(_) | Self::NonZeroExit { .. } => {
                GapError::new(ErrorCode::BranchError, self.to_string())
            }
        }
    }
}

impl fmt::Display for SandboxError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AllowList(err) => err.fmt(f),
            Self::UnavailableFailClosed { tier, reason } => write!(
                f,
                "sandbox unavailable for {tier:?}; failing closed ({reason})"
            ),
            Self::WorkDirInvalid(msg) => write!(f, "invalid sandbox work_dir: {msg}"),
            Self::SpawnFailed(msg) => write!(f, "failed to launch sandboxed child: {msg}"),
            Self::TimedOut { timeout } => {
                write!(
                    f,
                    "sandboxed process exceeded wall-clock timeout of {timeout:?}"
                )
            }
            Self::OutputLimitExceeded { stream, limit } => {
                write!(
                    f,
                    "sandboxed process {stream} exceeded cap of {limit} bytes"
                )
            }
            Self::NonZeroExit {
                status_code,
                stderr_preview,
            } => write!(
                f,
                "sandboxed process exited with status {status_code}: {stderr_preview}"
            ),
        }
    }
}

impl std::error::Error for SandboxError {}

/// Probes host OS sandbox availability and functional execution (`SEC-072` for `gap doctor`).
#[must_use]
pub fn doctor_check() -> SandboxDoctorReport {
    let os = std::env::consts::OS;
    if cfg!(target_os = "macos") {
        let seatbelt = Path::new("/usr/bin/sandbox-exec");
        if !seatbelt.is_file() {
            return SandboxDoctorReport {
                os,
                backend: SandboxBackendKind::Unavailable {
                    reason: "/usr/bin/sandbox-exec not found".to_owned(),
                },
                binary_present: false,
                functional_probe_ok: false,
                detail: "macOS Seatbelt binary missing".to_owned(),
            };
        }
        let probe = Command::new(seatbelt)
            .args(["-p", "(version 1)(allow default)", "/usr/bin/true"])
            .stdin(Stdio::null())
            .output();
        match probe {
            Ok(out) if out.status.success() => SandboxDoctorReport {
                os,
                backend: SandboxBackendKind::MacOsSeatbelt,
                binary_present: true,
                functional_probe_ok: true,
                detail: "macOS Seatbelt sandbox-exec probe succeeded".to_owned(),
            },
            Ok(out) => {
                let err_txt = String::from_utf8_lossy(&out.stderr).trim().to_owned();
                let detail = if err_txt.is_empty() {
                    format!("sandbox-exec probe exited with {}", out.status)
                } else {
                    err_txt
                };
                SandboxDoctorReport {
                    os,
                    backend: SandboxBackendKind::Unavailable {
                        reason: detail.clone(),
                    },
                    binary_present: true,
                    functional_probe_ok: false,
                    detail,
                }
            }
            Err(err) => SandboxDoctorReport {
                os,
                backend: SandboxBackendKind::Unavailable {
                    reason: err.to_string(),
                },
                binary_present: true,
                functional_probe_ok: false,
                detail: err.to_string(),
            },
        }
    } else if cfg!(target_os = "linux") {
        let Some(bwrap_path) = find_bwrap_binary() else {
            return SandboxDoctorReport {
                os,
                backend: SandboxBackendKind::Unavailable {
                    reason: "bubblewrap (`bwrap`) binary not found".to_owned(),
                },
                binary_present: false,
                functional_probe_ok: false,
                detail: "Linux bubblewrap (`bwrap`) not found".to_owned(),
            };
        };
        let probe = Command::new(&bwrap_path)
            .args([
                "--ro-bind",
                "/",
                "/",
                "--unshare-pid",
                "--die-with-parent",
                "/bin/true",
            ])
            .stdin(Stdio::null())
            .output();
        match probe {
            Ok(out) if out.status.success() => SandboxDoctorReport {
                os,
                backend: SandboxBackendKind::LinuxBwrap,
                binary_present: true,
                functional_probe_ok: true,
                detail: format!("Linux bwrap ({}) probe succeeded", bwrap_path.display()),
            },
            Ok(out) => {
                let err_txt = String::from_utf8_lossy(&out.stderr).trim().to_owned();
                let detail = if err_txt.is_empty() {
                    format!("bwrap probe exited with {}", out.status)
                } else {
                    err_txt
                };
                SandboxDoctorReport {
                    os,
                    backend: SandboxBackendKind::Unavailable {
                        reason: detail.clone(),
                    },
                    binary_present: true,
                    functional_probe_ok: false,
                    detail,
                }
            }
            Err(err) => SandboxDoctorReport {
                os,
                backend: SandboxBackendKind::Unavailable {
                    reason: err.to_string(),
                },
                binary_present: true,
                functional_probe_ok: false,
                detail: err.to_string(),
            },
        }
    } else {
        SandboxDoctorReport {
            os,
            backend: SandboxBackendKind::Unavailable {
                reason: format!("unsupported OS `{os}` for local process sandbox"),
            },
            binary_present: false,
            functional_probe_ok: false,
            detail: format!("no OS sandbox backend implemented for `{os}`"),
        }
    }
}

fn find_bwrap_binary() -> Option<PathBuf> {
    for candidate in ["/usr/bin/bwrap", "/bin/bwrap", "/usr/local/bin/bwrap"] {
        let p = PathBuf::from(candidate);
        if p.is_file() {
            return Some(p);
        }
    }
    if let Some(path_var) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path_var) {
            let p = dir.join("bwrap");
            if p.is_file() {
                return Some(p);
            }
        }
    }
    None
}

/// Fail-closed sandboxed process launcher enforcing `ExecAllowList`, env scrubbing, timeout, and output caps (`SEC-070`, `SEC-071`).
#[derive(Debug, Clone)]
pub struct SandboxLauncher {
    allow_list: ExecAllowList,
    backend_override: SandboxBackendOverride,
}

impl SandboxLauncher {
    /// Creates a launcher using the host's live OS sandbox probe (`AutoProbe`).
    #[must_use]
    pub const fn new(allow_list: ExecAllowList) -> Self {
        Self {
            allow_list,
            backend_override: SandboxBackendOverride::AutoProbe,
        }
    }

    /// Overrides backend probing (primarily for deterministic `SEC-071` fail-closed tests).
    #[must_use]
    pub fn with_backend_override(mut self, mode: SandboxBackendOverride) -> Self {
        self.backend_override = mode;
        self
    }

    /// Returns a reference to the backing executable allow-list.
    #[must_use]
    pub const fn allow_list(&self) -> &ExecAllowList {
        &self.allow_list
    }

    /// Prepares a validated, env-scrubbed, sandbox-wrapped command for either CLI execution or MCP `stdio` launch (`SEC-061`, `SEC-070`, `SEC-071`).
    ///
    /// # Errors
    ///
    /// Returns [`SandboxError`] if allow-list/hash check fails or `T2`/strict `T1` cannot be sandboxed.
    pub fn prepare_command(
        &self,
        binary_path: &Path,
        argv: &[String],
        profile: &SandboxProfile,
        env_source: &BTreeMap<String, String>,
    ) -> Result<PreparedSandboxedCommand, SandboxError> {
        let verified_binary = self
            .allow_list
            .verify_before_launch(binary_path)
            .map_err(SandboxError::AllowList)?;

        fs::create_dir_all(&profile.work_dir).map_err(|err| {
            SandboxError::WorkDirInvalid(format!("{}: {err}", profile.work_dir.display()))
        })?;
        let cwd = fs::canonicalize(&profile.work_dir).map_err(|err| {
            SandboxError::WorkDirInvalid(format!("{}: {err}", profile.work_dir.display()))
        })?;

        let clean_env = build_scrubbed_env(profile, &cwd, env_source);
        let backend = self.resolve_backend();

        let mut warnings = Vec::new();
        let (program, args, used_os_sandbox) = match backend {
            SandboxBackendKind::MacOsSeatbelt => {
                let seatbelt_profile =
                    render_macos_seatbelt_profile(profile, &verified_binary.canonical_path, &cwd);
                let mut full_args = Vec::with_capacity(argv.len() + 3);
                full_args.push("-p".to_owned());
                full_args.push(seatbelt_profile);
                full_args.push(
                    verified_binary
                        .canonical_path
                        .to_string_lossy()
                        .into_owned(),
                );
                full_args.extend(argv.iter().cloned());
                (PathBuf::from("/usr/bin/sandbox-exec"), full_args, true)
            }
            SandboxBackendKind::LinuxBwrap => {
                let bwrap_bin =
                    find_bwrap_binary().ok_or_else(|| SandboxError::UnavailableFailClosed {
                        tier: profile.trust_tier,
                        reason: "bwrap binary disappeared".to_owned(),
                    })?;
                let raw_args =
                    render_linux_bwrap_args(profile, &verified_binary.canonical_path, &cwd, argv);
                let full_args = raw_args
                    .into_iter()
                    .map(|os| os.to_string_lossy().into_owned())
                    .collect();
                (bwrap_bin, full_args, true)
            }
            SandboxBackendKind::Unavailable { reason } => {
                if matches!(profile.trust_tier, TrustTier::T0 | TrustTier::T1)
                    && profile.allow_unsandboxed_t1
                {
                    warnings.push(format!(
                        "WARNING (SEC-071): executing `{}` under {:?} WITHOUT OS sandbox ({reason})",
                        verified_binary.canonical_path.display(),
                        profile.trust_tier
                    ));
                    (verified_binary.canonical_path.clone(), argv.to_vec(), false)
                } else {
                    return Err(SandboxError::UnavailableFailClosed {
                        tier: profile.trust_tier,
                        reason,
                    });
                }
            }
        };

        Ok(PreparedSandboxedCommand {
            program,
            args,
            cwd,
            clean_env,
            verified_binary,
            used_os_sandbox,
            warnings,
        })
    }

    /// Spawns a long-lived MCP `stdio` server through the exact same allow-list, scrubbed environment,
    /// and OS sandbox profile (`SEC-070`, `WP-M6-09` MCP stdio integration).
    ///
    /// # Errors
    ///
    /// Returns [`SandboxError`] if verification/sandboxing fails or spawning fails.
    pub fn spawn_mcp_stdio(
        &self,
        binary_path: &Path,
        argv: &[String],
        profile: &SandboxProfile,
        env_source: &BTreeMap<String, String>,
    ) -> Result<(Child, PreparedSandboxedCommand), SandboxError> {
        let prepared = self.prepare_command(binary_path, argv, profile, env_source)?;
        let mut cmd = build_os_command(&prepared);
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let child = cmd
            .spawn()
            .map_err(|err| SandboxError::SpawnFailed(err.to_string()))?;
        Ok((child, prepared))
    }

    /// Executes an allow-listed command synchronously with timeout enforcement and `1 MiB` bounded
    /// stdout/stderr collection (`SEC-070`).
    ///
    /// # Errors
    ///
    /// Returns [`SandboxError`] on allow-list rejection, fail-closed sandbox unavailability, timeout,
    /// stdout/stderr overflow, or spawn error.
    pub fn launch(
        &self,
        binary_path: &Path,
        argv: &[String],
        profile: &SandboxProfile,
        env_source: &BTreeMap<String, String>,
    ) -> Result<SandboxOutput, SandboxError> {
        let prepared = self.prepare_command(binary_path, argv, profile, env_source)?;
        let mut cmd = build_os_command(&prepared);
        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = cmd
            .spawn()
            .map_err(|err| SandboxError::SpawnFailed(err.to_string()))?;

        let child_stdout = child
            .stdout
            .take()
            .ok_or_else(|| SandboxError::SpawnFailed("missing piped stdout".to_owned()))?;
        let child_stderr = child
            .stderr
            .take()
            .ok_or_else(|| SandboxError::SpawnFailed("missing piped stderr".to_owned()))?;

        let aborted = Arc::new(AtomicBool::new(false));
        let max_out = profile.max_stdout_bytes;
        let max_err = profile.max_stderr_bytes;

        let out_abort = Arc::clone(&aborted);
        let out_handle =
            thread::spawn(move || read_bounded_stream(child_stdout, max_out, out_abort));
        let err_abort = Arc::clone(&aborted);
        let err_handle =
            thread::spawn(move || read_bounded_stream(child_stderr, max_err, err_abort));

        let deadline = Instant::now() + profile.timeout;
        let mut exit_status = None;
        let mut timed_out = false;

        loop {
            if aborted.load(Ordering::Acquire) {
                let _ = child.kill();
                let _ = child.wait();
                break;
            }
            match child.try_wait() {
                Ok(Some(status)) => {
                    exit_status = Some(status);
                    break;
                }
                Ok(None) => {
                    if Instant::now() >= deadline {
                        timed_out = true;
                        aborted.store(true, Ordering::Release);
                        let _ = child.kill();
                        let _ = child.wait();
                        break;
                    }
                    thread::sleep(Duration::from_millis(5));
                }
                Err(err) => {
                    aborted.store(true, Ordering::Release);
                    let _ = child.kill();
                    let _ = out_handle.join();
                    let _ = err_handle.join();
                    return Err(SandboxError::SpawnFailed(err.to_string()));
                }
            }
        }

        let out_res = out_handle
            .join()
            .map_err(|_| SandboxError::SpawnFailed("stdout reader panicked".to_owned()))?;
        let err_res = err_handle
            .join()
            .map_err(|_| SandboxError::SpawnFailed("stderr reader panicked".to_owned()))?;

        if timed_out {
            return Err(SandboxError::TimedOut {
                timeout: profile.timeout,
            });
        }

        let stdout = match out_res {
            BoundedRead::Ok(bytes) => bytes,
            BoundedRead::Overflow => {
                return Err(SandboxError::OutputLimitExceeded {
                    stream: OutputStreamKind::Stdout,
                    limit: max_out,
                });
            }
            BoundedRead::Io(msg) => return Err(SandboxError::SpawnFailed(msg)),
        };

        let stderr = match err_res {
            BoundedRead::Ok(bytes) => bytes,
            BoundedRead::Overflow => {
                return Err(SandboxError::OutputLimitExceeded {
                    stream: OutputStreamKind::Stderr,
                    limit: max_err,
                });
            }
            BoundedRead::Io(msg) => return Err(SandboxError::SpawnFailed(msg)),
        };

        let status_code = exit_status.and_then(|s| s.code()).unwrap_or(-1);
        Ok(SandboxOutput {
            status_code,
            stdout,
            stderr,
            verified_sha256: prepared.verified_binary.sha256_hex,
            used_os_sandbox: prepared.used_os_sandbox,
            warnings: prepared.warnings,
        })
    }

    fn resolve_backend(&self) -> SandboxBackendKind {
        match &self.backend_override {
            SandboxBackendOverride::AutoProbe => doctor_check().backend,
            SandboxBackendOverride::ForceUnavailable(reason) => SandboxBackendKind::Unavailable {
                reason: reason.clone(),
            },
        }
    }
}

fn build_os_command(prepared: &PreparedSandboxedCommand) -> Command {
    let mut cmd = Command::new(&prepared.program);
    cmd.args(&prepared.args);
    cmd.current_dir(&prepared.cwd);
    cmd.env_clear();
    cmd.envs(&prepared.clean_env);
    cmd
}

fn build_scrubbed_env(
    profile: &SandboxProfile,
    cwd: &Path,
    env_source: &BTreeMap<String, String>,
) -> BTreeMap<String, String> {
    let mut clean = BTreeMap::new();
    let work_str = cwd.to_string_lossy().into_owned();
    clean.insert("PATH".to_owned(), "/usr/bin:/bin".to_owned());
    clean.insert("HOME".to_owned(), work_str.clone());
    clean.insert("TMPDIR".to_owned(), work_str);

    for allowed_key in &profile.env_allowlist {
        if FORBIDDEN_ENV_KEYS
            .iter()
            .any(|banned| banned.eq_ignore_ascii_case(allowed_key))
        {
            continue;
        }
        if matches!(allowed_key.as_str(), "PATH" | "HOME" | "TMPDIR") {
            continue;
        }
        if let Some(val) = env_source.get(allowed_key) {
            clean.insert(allowed_key.clone(), val.clone());
        }
    }
    clean
}

enum BoundedRead {
    Ok(Vec<u8>),
    Overflow,
    Io(String),
}

fn read_bounded_stream<R: Read>(
    mut reader: R,
    limit: usize,
    aborted: Arc<AtomicBool>,
) -> BoundedRead {
    let mut out = Vec::new();
    let mut chunk = [0_u8; 8192];
    loop {
        if aborted.load(Ordering::Acquire) {
            return BoundedRead::Ok(out);
        }
        match reader.read(&mut chunk) {
            Ok(0) => return BoundedRead::Ok(out),
            Ok(n) => {
                if out.len().saturating_add(n) > limit {
                    aborted.store(true, Ordering::Release);
                    return BoundedRead::Overflow;
                }
                out.extend_from_slice(&chunk[..n]);
            }
            Err(err) => return BoundedRead::Io(err.to_string()),
        }
    }
}
