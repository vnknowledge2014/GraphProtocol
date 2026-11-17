//! Process sandbox profiles for macOS `sandbox-exec` (Seatbelt) and Linux `bwrap` (`SEC-070`, `SEC-071`).

use std::ffi::OsString;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use gap_core::snapshot::TrustTier;

/// Default maximum stdout or stderr capture size per sandboxed invocation (`1 MiB`, `SEC-070`).
pub const DEFAULT_MAX_OUTPUT_BYTES: usize = 1024 * 1024;

/// Default wall-clock execution deadline for a single sandboxed CLI/tool run (`10 s`, `SEC-070`).
pub const DEFAULT_SANDBOX_TIMEOUT: Duration = Duration::from_secs(10);

/// Dangerous loader/shell environment variables ALWAYS stripped even if listed in `env_allowlist`.
pub const FORBIDDEN_ENV_KEYS: &[&str] = &[
    "LD_PRELOAD",
    "LD_LIBRARY_PATH",
    "LD_AUDIT",
    "DYLD_INSERT_LIBRARIES",
    "DYLD_LIBRARY_PATH",
    "DYLD_FRAMEWORK_PATH",
    "IFS",
    "BASH_ENV",
    "ENV",
    "CDPATH",
];

/// Resource-limit targets carried on [`SandboxProfile`] (`SEC-070`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RlimitSpec {
    /// Maximum CPU seconds (`RLIMIT_CPU`).
    pub cpu_seconds: u32,
    /// Maximum virtual address space bytes (`RLIMIT_AS`).
    pub max_memory_bytes: u64,
    /// Maximum open file descriptors (`RLIMIT_NOFILE`).
    pub max_open_files: u32,
    /// Maximum child processes/threads (`RLIMIT_NPROC`).
    pub max_procs: u32,
}

impl Default for RlimitSpec {
    fn default() -> Self {
        Self {
            cpu_seconds: 30,
            max_memory_bytes: 512 * 1024 * 1024,
            max_open_files: 64,
            max_procs: 16,
        }
    }
}

/// Complete sandboxing, resource, and environment policy for a child process (`SEC-070`, `SEC-071`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxProfile {
    /// Trust tier (`T0`, `T1`, or `T2`) controlling fail-closed fallback policy (`SEC-071`).
    pub trust_tier: TrustTier,
    /// Dedicated writable temporary working directory (`work_dir`).
    pub work_dir: PathBuf,
    /// When `None`, network access is completely denied; when `Some`, outbound traffic is pinned to the egress proxy.
    pub allow_network_via_egress_proxy: Option<SocketAddr>,
    /// Environment variable names forwarded from the host config into the scrubbed child environment.
    pub env_allowlist: Vec<String>,
    /// Wall-clock timeout before SIGKILL.
    pub timeout: Duration,
    /// Maximum stdout byte cap (`1 MiB` default).
    pub max_stdout_bytes: usize,
    /// Maximum stderr byte cap (`1 MiB` default).
    pub max_stderr_bytes: usize,
    /// Posix resource-limit targets (`RLIMIT_CPU/AS/NOFILE/NPROC`).
    pub rlimits: RlimitSpec,
    /// Explicit opt-in permitting `T1` branches to execute unsandboxed with a prominent warning when no OS sandbox works (`SEC-071`).
    ///
    /// Ignored for `T2` branches, which ALWAYS fail closed when no OS sandbox is available.
    pub allow_unsandboxed_t1: bool,
}

impl SandboxProfile {
    /// Constructs a restrictive default profile scoped to `trust_tier` and `work_dir`.
    #[must_use]
    pub fn restricted(trust_tier: TrustTier, work_dir: PathBuf) -> Self {
        Self {
            trust_tier,
            work_dir,
            allow_network_via_egress_proxy: None,
            env_allowlist: Vec::new(),
            timeout: DEFAULT_SANDBOX_TIMEOUT,
            max_stdout_bytes: DEFAULT_MAX_OUTPUT_BYTES,
            max_stderr_bytes: DEFAULT_MAX_OUTPUT_BYTES,
            rlimits: RlimitSpec::default(),
            allow_unsandboxed_t1: false,
        }
    }
}

/// Renders a restrictive macOS Seatbelt (`sandbox-exec -p`) profile (`SEC-070`).
///
/// Grants read-only access to standard OS runtime paths + `binary_path`, read/write ONLY within
/// `work_dir_canonical`, and denies network unless `allow_network_via_egress_proxy` is set.
#[must_use]
pub fn render_macos_seatbelt_profile(
    profile: &SandboxProfile,
    binary_canonical: &Path,
    work_dir_canonical: &Path,
) -> String {
    let bin_escaped = escape_seatbelt_str(&binary_canonical.to_string_lossy());
    let work_escaped = escape_seatbelt_str(&work_dir_canonical.to_string_lossy());

    let net_rule = match profile.allow_network_via_egress_proxy {
        None => "(deny network*)".to_owned(),
        Some(proxy_addr) => format!(
            "(deny network*)\n(allow network-outbound (remote ip \"{}:{}\"))",
            proxy_addr.ip(),
            proxy_addr.port()
        ),
    };

    format!(
        "(version 1)\n\
         (deny default)\n\
         (allow process-fork process-exec* process-info*)\n\
         (allow sysctl-read mach-lookup ipc-posix-shm* file-map-executable file-ioctl)\n\
         (allow signal (target self))\n\
         (allow file-read*)\n\
         (deny file-read* (subpath \"/Users\") (subpath \"/home\") (subpath \"/private/var/folders\") (subpath \"/private/tmp\"))\n\
         (allow file-read-metadata)\n\
         (allow file-read* (literal \"{bin_escaped}\") (subpath \"{work_escaped}\"))\n\
         (allow file-write* (subpath \"{work_escaped}\") (literal \"/dev/null\"))\n\
         {net_rule}\n"
    )
}

/// Renders deterministic Linux `bwrap` (bubblewrap) CLI arguments (`SEC-070`).
#[must_use]
pub fn render_linux_bwrap_args(
    profile: &SandboxProfile,
    binary_canonical: &Path,
    work_dir_canonical: &Path,
    argv: &[String],
) -> Vec<OsString> {
    let mut out: Vec<OsString> = vec![
        OsString::from("--ro-bind"),
        OsString::from("/"),
        OsString::from("/"),
        OsString::from("--dev"),
        OsString::from("/dev"),
        OsString::from("--proc"),
        OsString::from("/proc"),
        OsString::from("--tmpfs"),
        OsString::from("/tmp"),
        OsString::from("--bind"),
        work_dir_canonical.as_os_str().to_os_string(),
        work_dir_canonical.as_os_str().to_os_string(),
        OsString::from("--chdir"),
        work_dir_canonical.as_os_str().to_os_string(),
        OsString::from("--unshare-user"),
        OsString::from("--unshare-pid"),
        OsString::from("--unshare-ipc"),
        OsString::from("--unshare-uts"),
        OsString::from("--die-with-parent"),
        OsString::from("--new-session"),
        OsString::from("--cap-drop"),
        OsString::from("ALL"),
    ];

    if !binary_canonical.starts_with(work_dir_canonical) {
        out.splice(
            14..14,
            [
                OsString::from("--ro-bind"),
                binary_canonical.as_os_str().to_os_string(),
                binary_canonical.as_os_str().to_os_string(),
            ],
        );
    }

    if profile.allow_network_via_egress_proxy.is_none() {
        out.push(OsString::from("--unshare-net"));
    }

    out.push(OsString::from("--"));
    out.push(binary_canonical.as_os_str().to_os_string());
    for arg in argv {
        out.push(OsString::from(arg));
    }
    out
}

fn escape_seatbelt_str(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for ch in raw.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' | '\r' | '\0' => {}
            _ => out.push(ch),
        }
    }
    out
}
