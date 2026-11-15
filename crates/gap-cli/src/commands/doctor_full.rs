//! Full M7 `gap doctor` diagnostics (`OPS-044`, `SEC-072`, `SEC-062`, `SEC-040`, `SEC-102`).
//!
//! Runs baseline configuration checks from [`crate::doctor::run_doctor`] together with live
//! OS sandbox probing, egress guard self-tests, exec allow-list binary SHA-256 verification,
//! `SecretRef` / `0600` file vault posture checks, model artifact SHA-256 verification, and
//! Cedar policy compilation checks.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use gap_core::config::{GapConfig, McpTransportKind, Profile, SecretRef};
use gap_models::{ModelManager, ModelManifest};
use gap_sec::authz::CompiledPolicySet;
use gap_sec::egress::EgressGuard;
use gap_sec::manifest::ExecAllowList;
use gap_sec::sandbox::{SandboxBackendKind, doctor_check as sandbox_doctor_check};
use gap_server::{EXIT_CONFIG_ERROR, EXIT_OK};
use serde_json::{Value, json};

/// Severity status of one `gap doctor` check entry (`OPS-044`, `SEC-072`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DoctorStatus {
    /// Check passed cleanly.
    Ok,
    /// Non-fatal operational or offline baseline warning.
    Warn,
    /// Blocking configuration, security, or integrity failure.
    Fail,
    /// Optional subsystem not configured at this path.
    Skip,
}

impl DoctorStatus {
    /// Human-readable bracketed tag used in plain text output.
    #[must_use]
    pub const fn tag(self) -> &'static str {
        match self {
            Self::Ok => "[ok]",
            Self::Warn => "[warn]",
            Self::Fail => "[fail]",
            Self::Skip => "[skip]",
        }
    }

    /// Lowercase JSON status string.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Warn => "warn",
            Self::Fail => "fail",
            Self::Skip => "skip",
        }
    }
}

/// Single diagnostic finding in [`DoctorReport`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DoctorCheckEntry {
    /// Stable check identifier (e.g. `security.sandbox`, `branches.exec_allowlist`).
    pub id: String,
    /// Check outcome status.
    pub status: DoctorStatus,
    /// Human-readable summary message (never contains secret material).
    pub message: String,
    /// Optional remediation hint when status is `Warn` or `Fail`.
    pub fix: Option<String>,
}

impl DoctorCheckEntry {
    /// Serializes this entry into a JSON object.
    #[must_use]
    pub fn to_json_value(&self) -> Value {
        let mut obj = serde_json::Map::new();
        obj.insert("id".to_owned(), Value::String(self.id.clone()));
        obj.insert(
            "status".to_owned(),
            Value::String(self.status.as_str().to_owned()),
        );
        obj.insert("message".to_owned(), Value::String(self.message.clone()));
        if let Some(ref fix) = self.fix {
            obj.insert("fix".to_owned(), Value::String(fix.clone()));
        }
        Value::Object(obj)
    }
}

/// Complete structured report produced by `gap doctor` (`OPS-044`, `SEC-072`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DoctorReport {
    /// Path of the inspected `gap.toml` configuration file.
    pub config_path: String,
    /// Count of passing checks.
    pub ok_count: usize,
    /// Count of warning checks.
    pub warn_count: usize,
    /// Count of failing checks.
    pub fail_count: usize,
    /// Count of skipped checks.
    pub skip_count: usize,
    /// Ordered list of diagnostic check entries.
    pub checks: Vec<DoctorCheckEntry>,
}

impl DoctorReport {
    /// Converts the full doctor report into a structured JSON object.
    #[must_use]
    pub fn to_json_value(&self) -> Value {
        let checks_json: Vec<Value> = self
            .checks
            .iter()
            .map(DoctorCheckEntry::to_json_value)
            .collect();
        json!({
            "config_path": self.config_path,
            "ok_count": self.ok_count,
            "warn_count": self.warn_count,
            "fail_count": self.fail_count,
            "skip_count": self.skip_count,
            "checks": checks_json,
        })
    }

    fn new(config_path: &Path) -> Self {
        Self {
            config_path: config_path.display().to_string(),
            ok_count: 0,
            warn_count: 0,
            fail_count: 0,
            skip_count: 0,
            checks: Vec::new(),
        }
    }

    fn push(&mut self, entry: DoctorCheckEntry) {
        match entry.status {
            DoctorStatus::Ok => self.ok_count += 1,
            DoctorStatus::Warn => self.warn_count += 1,
            DoctorStatus::Fail => self.fail_count += 1,
            DoctorStatus::Skip => self.skip_count += 1,
        }
        self.checks.push(entry);
    }

    /// Exit code implied by this report (`0` when `fail_count == 0`, otherwise `EXIT_CONFIG_ERROR`).
    #[must_use]
    pub const fn exit_code(&self) -> i32 {
        if self.fail_count == 0 {
            EXIT_OK
        } else {
            EXIT_CONFIG_ERROR
        }
    }

    /// Renders human-readable diagnostic lines and summary totals into `out`.
    pub fn write_text(&self, out: &mut dyn Write) {
        for c in &self.checks {
            match &c.fix {
                Some(fix) => {
                    let _ = writeln!(
                        out,
                        "{} {}: {} (fix: {fix})",
                        c.status.tag(),
                        c.id,
                        c.message
                    );
                }
                None => {
                    let _ = writeln!(out, "{} {}: {}", c.status.tag(), c.id, c.message);
                }
            }
        }
        let _ = writeln!(
            out,
            "summary: {} ok, {} warn, {} fail, {} skip",
            self.ok_count, self.warn_count, self.fail_count, self.skip_count
        );
    }
}

/// Input options for [`evaluate_doctor_full`].
#[derive(Debug, Clone)]
pub struct DoctorOptions {
    /// Path to `gap.toml`.
    pub config_path: PathBuf,
    /// Optional override path to `models/manifest.toml`.
    pub models_manifest: Option<PathBuf>,
    /// Optional model cache directory used for `SEC-102` verification.
    pub models_cache: Option<PathBuf>,
    /// Emit structured JSON instead of human-readable text.
    pub json: bool,
}

impl DoctorOptions {
    /// Builds default [`DoctorOptions`] targeting `config_path`.
    #[must_use]
    pub fn new(config_path: impl Into<PathBuf>) -> Self {
        Self {
            config_path: config_path.into(),
            models_manifest: None,
            models_cache: None,
            json: false,
        }
    }
}

/// Evaluates all configuration, sandbox, egress, allow-list, vault, model, and policy checks
/// (`OPS-044`, `SEC-072`, `SEC-062`, `SEC-040`, `SEC-102`, `SEC-020`).
#[must_use]
pub fn evaluate_doctor_full(opts: &DoctorOptions) -> DoctorReport {
    let mut report = DoctorReport::new(&opts.config_path);

    // 1. Run baseline M1 config & filesystem checks, replacing M1 placeholder "skipped" lines
    // with the real M7 checks below.
    let mut legacy_stdout = Vec::new();
    let mut legacy_stderr = Vec::new();
    let legacy_exit =
        crate::doctor::run_doctor(&opts.config_path, &mut legacy_stdout, &mut legacy_stderr);
    let legacy_text = String::from_utf8_lossy(&legacy_stdout);
    for entry in parse_legacy_lines(&legacy_text) {
        report.push(entry);
    }
    let err_text = String::from_utf8_lossy(&legacy_stderr);
    if legacy_exit != 0 && report.fail_count == 0 && !err_text.trim().is_empty() {
        report.push(DoctorCheckEntry {
            id: "config.parse".to_owned(),
            status: DoctorStatus::Fail,
            message: err_text.trim().to_owned(),
            fix: Some("fix TOML syntax or invalid fields in gap.toml".to_owned()),
        });
    }

    let loaded_cfg = GapConfig::load_from_path_with_process_env(&opts.config_path).ok();

    // 2. SEC-072: Live OS sandbox probe (`sandbox-exec` / `bwrap`).
    check_sandbox(&mut report, loaded_cfg.as_ref());

    // 3. SEC-072: Built-in EgressGuard SSRF matrix self-test.
    check_egress_guard(&mut report);

    // 4. SEC-062: ExecAllowList live canonical + SHA-256 verification for stdio MCP branches.
    if let Some(cfg) = loaded_cfg.as_ref() {
        check_exec_allowlist(&mut report, cfg);
        check_vault_posture(&mut report, cfg);
        check_policy_dir(&mut report, &opts.config_path, cfg);
    }

    // 5. SEC-102: Model manifest & local cache artifact hash verification.
    check_models(&mut report, opts);

    report
}

/// Runs `evaluate_doctor_full` and writes either JSON or human-readable output, returning `0` on
/// success (`fail_count == 0`) or `EXIT_CONFIG_ERROR` (`2`) on failures.
pub fn run_doctor_full(
    opts: &DoctorOptions,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> i32 {
    let report = evaluate_doctor_full(opts);
    if opts.json {
        match serde_json::to_string_pretty(&report.to_json_value()) {
            Ok(text) => {
                let _ = writeln!(stdout, "{text}");
            }
            Err(err) => {
                let _ = writeln!(stderr, "error: failed to serialize doctor report: {err}");
                return EXIT_CONFIG_ERROR;
            }
        }
    } else {
        report.write_text(stdout);
    }
    report.exit_code()
}

fn parse_legacy_lines(text: &str) -> Vec<DoctorCheckEntry> {
    let mut out = Vec::new();
    for raw_line in text.lines() {
        let line = raw_line.trim();
        if line.is_empty()
            || line.starts_with("summary:")
            || line.contains("skipped (not available in M1)")
        {
            continue;
        }
        let (status, rest) = if let Some(r) = line.strip_prefix("[ok]") {
            (DoctorStatus::Ok, r.trim())
        } else if let Some(r) = line.strip_prefix("[warn]") {
            (DoctorStatus::Warn, r.trim())
        } else if let Some(r) = line.strip_prefix("[fail]") {
            (DoctorStatus::Fail, r.trim())
        } else if let Some(r) = line.strip_prefix("[skip]") {
            (DoctorStatus::Skip, r.trim())
        } else {
            continue;
        };

        let (id, body) = match rest.split_once(':') {
            Some((id, msg)) => (id.trim().to_owned(), msg.trim().to_owned()),
            None => ("legacy".to_owned(), rest.to_owned()),
        };

        let (message, fix) = match body.split_once("(fix:") {
            Some((msg, fix_tail)) => (
                msg.trim().to_owned(),
                Some(fix_tail.trim_end_matches(')').trim().to_owned()),
            ),
            None => (body, None),
        };

        out.push(DoctorCheckEntry {
            id,
            status,
            message,
            fix,
        });
    }
    out
}

fn check_sandbox(report: &mut DoctorReport, cfg: Option<&GapConfig>) {
    let probe = sandbox_doctor_check();
    let backend_label = match &probe.backend {
        SandboxBackendKind::MacOsSeatbelt => "macos-seatbelt",
        SandboxBackendKind::LinuxBwrap => "linux-bwrap",
        SandboxBackendKind::Unavailable { .. } => "unavailable",
    };

    if probe.functional_probe_ok {
        report.push(DoctorCheckEntry {
            id: "security.sandbox".to_owned(),
            status: DoctorStatus::Ok,
            message: format!(
                "OS sandbox `{backend_label}` operational ({})",
                probe.detail
            ),
            fix: None,
        });
        return;
    }

    let is_prod = cfg.is_some_and(|c| c.profile == Profile::Prod);
    let has_stdio_t2 = cfg.is_some_and(|c| {
        c.branches.iter().any(|b| {
            b.trust_tier() == gap_core::snapshot::TrustTier::T2
                && b.mcp
                    .as_ref()
                    .is_some_and(|m| m.transport == McpTransportKind::Stdio)
        })
    });
    let status = if (is_prod || has_stdio_t2) && !probe.functional_probe_ok {
        DoctorStatus::Fail
    } else {
        DoctorStatus::Warn
    };

    report.push(DoctorCheckEntry {
        id: "security.sandbox".to_owned(),
        status,
        message: format!("OS sandbox `{backend_label}` probe unavailable ({})", probe.detail),
        fix: Some(
            "ensure `/usr/bin/sandbox-exec` (macOS) or `bwrap` (Linux) works cleanly without nesting blocks"
                .to_owned(),
        ),
    });
}

fn check_egress_guard(report: &mut DoctorReport) {
    match EgressGuard::self_test() {
        Ok(()) => report.push(DoctorCheckEntry {
            id: "security.egress_guard".to_owned(),
            status: DoctorStatus::Ok,
            message: "egress SSRF, scheme, redirect, and DNS-rebinding self-test matrix passed"
                .to_owned(),
            fix: None,
        }),
        Err(err) => report.push(DoctorCheckEntry {
            id: "security.egress_guard".to_owned(),
            status: DoctorStatus::Fail,
            message: format!("egress guard self-test failed: {err}"),
            fix: Some("inspect gap-sec egress IP/DNS blocking implementation".to_owned()),
        }),
    }
}

fn check_exec_allowlist(report: &mut DoctorReport, cfg: &GapConfig) {
    let mut allowlist = ExecAllowList::new();
    let mut stdio_count = 0usize;

    for branch in &cfg.branches {
        let Some(mcp) = &branch.mcp else {
            continue;
        };
        if mcp.transport != McpTransportKind::Stdio {
            continue;
        }
        stdio_count += 1;
        let cmd = mcp.command.as_deref().unwrap_or_default();
        let Some(expected_sha) = mcp.sha256.as_deref() else {
            report.push(DoctorCheckEntry {
                id: format!("branches.{}.exec_allowlist", branch.id),
                status: DoctorStatus::Fail,
                message: format!("stdio binary `{cmd}` missing pinned `sha256` (SEC-062)"),
                fix: Some("pin lowercase 64-char sha256 digest in [[branches]] config".to_owned()),
            });
            continue;
        };
        let bin_path = Path::new(cmd);
        match allowlist
            .register(bin_path, Some(expected_sha))
            .and_then(|_| allowlist.verify_before_launch(bin_path))
        {
            Ok(verified) => report.push(DoctorCheckEntry {
                id: format!("branches.{}.exec_allowlist", branch.id),
                status: DoctorStatus::Ok,
                message: format!(
                    "verified `{}` sha256={}...",
                    verified.canonical_path.display(),
                    &verified.sha256_hex[..12.min(verified.sha256_hex.len())]
                ),
                fix: None,
            }),
            Err(err) => report.push(DoctorCheckEntry {
                id: format!("branches.{}.exec_allowlist", branch.id),
                status: DoctorStatus::Fail,
                message: format!("allow-list verification failed for `{cmd}`: {err}"),
                fix: Some("set absolute executable path and matching 64-char sha256".to_owned()),
            }),
        }
    }

    if stdio_count == 0 {
        report.push(DoctorCheckEntry {
            id: "branches.exec_allowlist".to_owned(),
            status: DoctorStatus::Ok,
            message: "no stdio MCP binaries configured".to_owned(),
            fix: None,
        });
    }
}

fn check_vault_posture(report: &mut DoctorReport, cfg: &GapConfig) {
    let mut refs: Vec<(&str, &SecretRef)> = Vec::new();
    if let Some(ref sec) = cfg.auth.introspection_client_secret {
        refs.push(("auth.introspection_client_secret", sec));
    }
    if let Some(ref sec) = cfg.security.approval_signing_key_secret {
        refs.push(("security.approval_signing_key_secret", sec));
    }
    if let Some(ref sec) = cfg.telemetry.otlp_auth_header_secret {
        refs.push(("telemetry.otlp_auth_header_secret", sec));
    }
    for branch in &cfg.branches {
        if let Some(ref openapi) = branch.openapi
            && let Some(ref sec) = openapi.auth.secret
        {
            refs.push(("branches.openapi.auth.secret", sec));
        }
        if let Some(ref mcp) = branch.mcp
            && let Some(ref sec) = mcp.auth.secret
        {
            refs.push(("branches.mcp.auth.secret", sec));
        }
    }

    if refs.is_empty() {
        report.push(DoctorCheckEntry {
            id: "security.vault".to_owned(),
            status: DoctorStatus::Ok,
            message: "no external secret references configured; inline secrets absent (INV-15)"
                .to_owned(),
            fix: None,
        });
        return;
    }

    let is_prod = cfg.profile == Profile::Prod;
    for (field_name, secret_ref) in refs {
        match secret_ref {
            SecretRef::Env { var } => match std::env::var_os(var) {
                Some(val) if !val.is_empty() => report.push(DoctorCheckEntry {
                    id: format!("security.vault.{field_name}"),
                    status: DoctorStatus::Ok,
                    message: format!("env secret `{var}` is present (value redacted)"),
                    fix: None,
                }),
                _ => report.push(DoctorCheckEntry {
                    id: format!("security.vault.{field_name}"),
                    status: if is_prod {
                        DoctorStatus::Fail
                    } else {
                        DoctorStatus::Warn
                    },
                    message: format!("env secret `{var}` is unset or empty"),
                    fix: Some(format!("export `{var}` in runtime environment")),
                }),
            },
            SecretRef::File { path } => match fs::symlink_metadata(path) {
                Ok(meta) => {
                    if !meta.file_type().is_file() {
                        report.push(DoctorCheckEntry {
                            id: format!("security.vault.{field_name}"),
                            status: DoctorStatus::Fail,
                            message: format!("file secret `{path}` is not a regular file"),
                            fix: Some("point file:// secret to a regular 0600 file".to_owned()),
                        });
                        continue;
                    }
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt;
                        let mode = meta.permissions().mode() & 0o777;
                        if mode & 0o077 != 0 {
                            report.push(DoctorCheckEntry {
                                id: format!("security.vault.{field_name}"),
                                status: DoctorStatus::Fail,
                                message: format!(
                                    "file secret `{path}` permissions are {mode:#05o}; expected 0600 (SEC-040)"
                                ),
                                fix: Some(format!("run `chmod 0600 {path}`")),
                            });
                            continue;
                        }
                    }
                    report.push(DoctorCheckEntry {
                        id: format!("security.vault.{field_name}"),
                        status: DoctorStatus::Ok,
                        message: format!("file secret `{path}` exists with strict permissions"),
                        fix: None,
                    });
                }
                Err(err) => report.push(DoctorCheckEntry {
                    id: format!("security.vault.{field_name}"),
                    status: DoctorStatus::Fail,
                    message: format!("file secret `{path}` unreadable: {err}"),
                    fix: Some(format!("create `0600` secret file at `{path}`")),
                }),
            },
            SecretRef::Vault { provider, name } => report.push(DoctorCheckEntry {
                id: format!("security.vault.{field_name}"),
                status: DoctorStatus::Ok,
                message: format!(
                    "external vault reference `secret://{provider}/{name}` structurally valid (resolved at connection time)"
                ),
                fix: None,
            }),
        }
    }
}

fn check_policy_dir(report: &mut DoctorReport, config_path: &Path, cfg: &GapConfig) {
    let base_dir = config_path.parent().unwrap_or_else(|| Path::new("."));
    let policy_dir = base_dir.join(&cfg.security.policy_dir);
    let schema_file = policy_dir.join("schema.cedarschema");

    if schema_file.is_file() {
        let mut policy_sources = Vec::new();
        if let Ok(entries) = fs::read_dir(&policy_dir) {
            let mut paths: Vec<PathBuf> = entries
                .filter_map(Result::ok)
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|ext| ext == "cedar"))
                .collect();
            paths.sort();
            for p in paths {
                match fs::read_to_string(&p) {
                    Ok(content) => policy_sources.push(content),
                    Err(err) => {
                        report.push(DoctorCheckEntry {
                            id: "security.policy".to_owned(),
                            status: DoctorStatus::Fail,
                            message: format!("failed reading policy `{}`: {err}", p.display()),
                            fix: Some("ensure all `.cedar` files are readable UTF-8".to_owned()),
                        });
                        return;
                    }
                }
            }
        }
        let schema_src = match fs::read_to_string(&schema_file) {
            Ok(s) => s,
            Err(err) => {
                report.push(DoctorCheckEntry {
                    id: "security.policy".to_owned(),
                    status: DoctorStatus::Fail,
                    message: format!(
                        "failed reading policy schema `{}`: {err}",
                        schema_file.display()
                    ),
                    fix: Some("fix schema file permissions/encoding".to_owned()),
                });
                return;
            }
        };
        let refs: Vec<&str> = policy_sources.iter().map(String::as_str).collect();
        match CompiledPolicySet::compile(&schema_src, &refs) {
            Ok(compiled) => report.push(DoctorCheckEntry {
                id: "security.policy".to_owned(),
                status: DoctorStatus::Ok,
                message: format!(
                    "compiled {} Cedar policies (version {}...)",
                    compiled.policy_count(),
                    &compiled.policy_version_hex()[..12]
                ),
                fix: None,
            }),
            Err(err) => report.push(DoctorCheckEntry {
                id: "security.policy".to_owned(),
                status: DoctorStatus::Fail,
                message: format!("Cedar policy compilation failed: {err}"),
                fix: Some("validate `.cedar` syntax and schema conformance".to_owned()),
            }),
        }
    } else {
        match CompiledPolicySet::default_builtin() {
            Ok(compiled) => report.push(DoctorCheckEntry {
                id: "security.policy".to_owned(),
                status: DoctorStatus::Ok,
                message: format!(
                    "built-in fail-closed Cedar policy set active ({} policies, version {}...)",
                    compiled.policy_count(),
                    &compiled.policy_version_hex()[..12]
                ),
                fix: None,
            }),
            Err(err) => report.push(DoctorCheckEntry {
                id: "security.policy".to_owned(),
                status: DoctorStatus::Fail,
                message: format!("built-in Cedar policy set failed: {err}"),
                fix: Some("inspect gap-sec default policy definitions".to_owned()),
            }),
        }
    }
}

fn check_models(report: &mut DoctorReport, opts: &DoctorOptions) {
    let manifest_path = match &opts.models_manifest {
        Some(p) => p.clone(),
        None => {
            let default_manifest = PathBuf::from("models/manifest.toml");
            if !default_manifest.is_file() {
                report.push(DoctorCheckEntry {
                    id: "models.s1".to_owned(),
                    status: DoctorStatus::Skip,
                    message: "no models/manifest.toml at default path; S1 routing uses baseline"
                        .to_owned(),
                    fix: None,
                });
                return;
            }
            default_manifest
        }
    };

    let manifest = match ModelManifest::load_from_path(&manifest_path) {
        Ok(m) => m,
        Err(err) => {
            report.push(DoctorCheckEntry {
                id: "models.s1".to_owned(),
                status: DoctorStatus::Fail,
                message: format!(
                    "invalid model manifest `{}`: {err}",
                    manifest_path.display()
                ),
                fix: Some("fix TOML fields in models/manifest.toml".to_owned()),
            });
            return;
        }
    };

    let cache_dir = opts
        .models_cache
        .clone()
        .unwrap_or_else(|| PathBuf::from(".gap/models"));
    let manager = ModelManager::new(manifest.clone(), cache_dir, true);
    match manager.verify_all() {
        Ok(paths) => report.push(DoctorCheckEntry {
            id: "models.s1".to_owned(),
            status: DoctorStatus::Ok,
            message: format!("all {} model artifacts verified against pinned sha256", paths.len()),
            fix: None,
        }),
        Err(err) => report.push(DoctorCheckEntry {
            id: "models.s1".to_owned(),
            status: DoctorStatus::Warn,
            message: format!(
                "model cache incomplete or unverified offline ({}); runtime will degrade safely to router baseline (RTE-044): {err}",
                manifest.entries().len()
            ),
            fix: Some("run `gap models pull` in an online environment to populate cache".to_owned()),
        }),
    }
}
