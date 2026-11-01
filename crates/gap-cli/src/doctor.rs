//! `gap doctor [--config gap.toml]` configuration and posture diagnostics (`OPS-044`, `OPS-001`,
//! `SEC-062`, `SEC-072`, `Brief §3.4`).

use std::fs;
use std::io::Write;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use gap_core::config::{BranchProtocol, GapConfig, McpTransportKind, Profile, SecretRef};
use gap_core::snapshot::TrustTier;
use gap_mcp::outbound::launch::{AllowedBinary, verify_binary};
use gap_server::{EXIT_OK, EXIT_RUNTIME_ERROR};

/// Status of an individual `gap doctor` check (`Brief §3.4`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckStatus {
    /// Check passed (`[ok]`).
    Ok,
    /// Non-fatal posture warning (`[warn]`).
    Warn,
    /// Blocking check failure (`[fail]`; causes non-zero exit code).
    Fail,
    /// Check belongs to a later milestone (`[skipped]`).
    Skipped,
}

impl CheckStatus {
    const fn tag(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Warn => "warn",
            Self::Fail => "fail",
            Self::Skipped => "skipped",
        }
    }
}

fn emit_check(
    stdout: &mut dyn Write,
    fail_count: &mut usize,
    status: CheckStatus,
    name: &str,
    detail: &str,
    fix: Option<&str>,
) {
    if status == CheckStatus::Fail {
        *fail_count = fail_count.saturating_add(1);
    }
    match fix {
        Some(hint) => {
            let _ = writeln!(stdout, "[{}] {name}: {detail} (fix: {hint})", status.tag());
        }
        None => {
            let _ = writeln!(stdout, "[{}] {name}: {detail}", status.tag());
        }
    }
}

fn emit_m1_skipped(stdout: &mut dyn Write, fail_count: &mut usize) {
    for name in ["runtime.toolchain", "security.sandbox", "models.s1"] {
        emit_check(
            stdout,
            fail_count,
            CheckStatus::Skipped,
            name,
            "skipped (not available in M1)",
            None,
        );
    }
}

const fn profile_label(profile: Profile) -> &'static str {
    match profile {
        Profile::Dev => "dev",
        Profile::Prod => "prod",
    }
}

fn resolve_base_dir(config_path: &Path) -> PathBuf {
    match config_path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => PathBuf::from("."),
    }
}

fn check_referenced_files(
    cfg: &GapConfig,
    base_dir: &Path,
    stdout: &mut dyn Write,
    fail_count: &mut usize,
) {
    let mut problems = Vec::new();

    for (idx, branch) in cfg.branches.iter().enumerate() {
        if branch.protocol == BranchProtocol::Openapi
            && let Some(oa) = branch.openapi.as_ref()
        {
            let full_path = base_dir.join(&oa.spec);
            match fs::metadata(&full_path) {
                Ok(meta) if meta.is_file() => match fs::read(&full_path) {
                    Ok(bytes) if !bytes.is_empty() => {}
                    Ok(_) => {
                        problems.push(format!(
                            "branches[{idx}].openapi.spec `{}` is empty",
                            oa.spec
                        ));
                    }
                    Err(err) => {
                        problems.push(format!(
                            "branches[{idx}].openapi.spec `{}` is unreadable ({err})",
                            oa.spec
                        ));
                    }
                },
                Ok(_) => {
                    problems.push(format!(
                        "branches[{idx}].openapi.spec `{}` is not a regular file",
                        oa.spec
                    ));
                }
                Err(err) => {
                    problems.push(format!(
                        "branches[{idx}].openapi.spec `{}` is missing or unreadable ({err})",
                        oa.spec
                    ));
                }
            }
        }
    }

    let mut file_secrets: Vec<(&str, &SecretRef)> = Vec::new();
    if let Some(s) = cfg.server.tls_cert_secret.as_ref() {
        file_secrets.push(("server.tls_cert_secret", s));
    }
    if let Some(s) = cfg.server.tls_key_secret.as_ref() {
        file_secrets.push(("server.tls_key_secret", s));
    }
    if let Some(s) = cfg.auth.introspection_client_secret.as_ref() {
        file_secrets.push(("auth.introspection_client_secret", s));
    }
    if let Some(s) = cfg.security.approval_signing_key_secret.as_ref() {
        file_secrets.push(("security.approval_signing_key_secret", s));
    }
    if let Some(s) = cfg.telemetry.otlp_auth_header_secret.as_ref() {
        file_secrets.push(("telemetry.otlp_auth_header_secret", s));
    }
    for branch in &cfg.branches {
        if let Some(oa) = branch.openapi.as_ref()
            && let Some(s) = oa.auth.secret.as_ref()
        {
            file_secrets.push(("branches.openapi.auth.secret", s));
        }
        if let Some(mcp) = branch.mcp.as_ref()
            && let Some(s) = mcp.auth.secret.as_ref()
        {
            file_secrets.push(("branches.mcp.auth.secret", s));
        }
    }

    for (field, sec) in file_secrets {
        if let SecretRef::File { path } = sec {
            match fs::metadata(path) {
                Ok(meta) if meta.is_file() => {
                    if let Err(err) = fs::File::open(path) {
                        problems.push(format!("{field} `{path}` is not readable ({err})"));
                    }
                }
                Ok(_) => {
                    problems.push(format!("{field} `{path}` is not a regular file"));
                }
                Err(err) => {
                    problems.push(format!("{field} `{path}` is missing ({err})"));
                }
            }
        }
    }

    if problems.is_empty() {
        emit_check(
            stdout,
            fail_count,
            CheckStatus::Ok,
            "config.files",
            "all referenced branch files exist and are readable",
            None,
        );
    } else {
        emit_check(
            stdout,
            fail_count,
            CheckStatus::Fail,
            "config.files",
            &problems.join("; "),
            Some(
                "create or fix permissions on every referenced OpenAPI/secret file relative to gap.toml",
            ),
        );
    }
}

fn check_listen_addr(cfg: &GapConfig, stdout: &mut dyn Write, fail_count: &mut usize) {
    match cfg.server.listen.parse::<SocketAddr>() {
        Err(err) => {
            emit_check(
                stdout,
                fail_count,
                CheckStatus::Fail,
                "server.listen",
                &format!("invalid socket address `{}` ({err})", cfg.server.listen),
                Some("set [server].listen to `<ip>:<port>`, for example `127.0.0.1:8080`"),
            );
        }
        Ok(addr) if addr.ip().is_unspecified() && cfg.profile == Profile::Prod => {
            emit_check(
                stdout,
                fail_count,
                CheckStatus::Fail,
                "server.listen",
                &format!("wildcard bind address `{addr}` is forbidden in `prod`"),
                Some("bind [server].listen to a specific interface address or `127.0.0.1:8080`"),
            );
        }
        Ok(addr) if addr.ip().is_unspecified() => {
            emit_check(
                stdout,
                fail_count,
                CheckStatus::Warn,
                "server.listen",
                &format!("wildcard bind address `{addr}` exposes dev server beyond loopback"),
                Some("set [server].listen = \"127.0.0.1:8080\" for local development"),
            );
        }
        Ok(addr) => {
            emit_check(
                stdout,
                fail_count,
                CheckStatus::Ok,
                "server.listen",
                &format!("valid socket address {addr}"),
                None,
            );
        }
    }
}

fn check_api_keys(cfg: &GapConfig, stdout: &mut dyn Write, fail_count: &mut usize) {
    if cfg.profile == Profile::Prod && cfg.api_keys.is_empty() {
        emit_check(
            stdout,
            fail_count,
            CheckStatus::Fail,
            "auth.api_keys",
            "profile = \"prod\" has no [[api_keys]] configured",
            Some("add at least one [[api_keys]] entry with a 64-hex blake3 `key_hash`"),
        );
    } else if cfg.api_keys.is_empty() {
        emit_check(
            stdout,
            fail_count,
            CheckStatus::Warn,
            "auth.api_keys",
            "no [[api_keys]] configured in dev profile",
            Some("configure [[api_keys]] before switching to profile = \"prod\""),
        );
    } else {
        emit_check(
            stdout,
            fail_count,
            CheckStatus::Ok,
            "auth.api_keys",
            &format!("{} API key(s) configured", cfg.api_keys.len()),
            None,
        );
    }
}

fn check_t2_branches(cfg: &GapConfig, stdout: &mut dyn Write, fail_count: &mut usize) {
    let t2_ids: Vec<&str> = cfg
        .branches
        .iter()
        .filter(|b| b.trust_tier() == TrustTier::T2)
        .map(|b| b.id.as_str())
        .collect();
    if t2_ids.is_empty() {
        emit_check(
            stdout,
            fail_count,
            CheckStatus::Ok,
            "branches.trust",
            "no T2 (untrusted) branches configured",
            None,
        );
    } else {
        emit_check(
            stdout,
            fail_count,
            CheckStatus::Warn,
            "branches.trust",
            &format!(
                "T2 (untrusted) branch(es) configured: {}",
                t2_ids.join(", ")
            ),
            Some(
                "T2 branches are hidden in prod and spotlighted in dev; promote to trust = \"T1\" only after review",
            ),
        );
    }
}

fn check_stdio_mcp_branches(cfg: &GapConfig, stdout: &mut dyn Write, fail_count: &mut usize) {
    let mut stdio_count = 0_usize;
    let mut failures = Vec::new();

    for branch in &cfg.branches {
        if branch.protocol != BranchProtocol::Mcp {
            continue;
        }
        let Some(mcp) = branch.mcp.as_ref() else {
            continue;
        };
        if mcp.transport != McpTransportKind::Stdio {
            continue;
        }
        stdio_count = stdio_count.saturating_add(1);

        if cfg.profile == Profile::Prod {
            failures.push(format!(
                "branch `{}` uses transport = \"stdio\" in prod (refused until WP-M6-09 sandbox)",
                branch.id
            ));
            continue;
        }

        let cmd = mcp.command.clone().unwrap_or_default();
        let sha256 = mcp.sha256.clone().unwrap_or_default();
        let entry = AllowedBinary {
            path: PathBuf::from(&cmd),
            sha256,
            args: mcp.args.clone(),
            env: Vec::new(),
        };
        if let Err(err) = verify_binary(&entry) {
            failures.push(format!("branch `{}`: {err}", branch.id));
        }
    }

    if !failures.is_empty() {
        emit_check(
            stdout,
            fail_count,
            CheckStatus::Fail,
            "branches.mcp_stdio",
            &failures.join("; "),
            Some(
                "ensure `branches.mcp.command` is an existing regular-file absolute path and `branches.mcp.sha256` matches its 64-hex sha256 digest",
            ),
        );
    } else if stdio_count == 0 {
        emit_check(
            stdout,
            fail_count,
            CheckStatus::Ok,
            "branches.mcp_stdio",
            "no stdio MCP branches configured",
            None,
        );
    } else {
        emit_check(
            stdout,
            fail_count,
            CheckStatus::Ok,
            "branches.mcp_stdio",
            &format!("verified {stdio_count} stdio MCP executable(s)"),
            None,
        );
    }
}

/// Runs `gap doctor` against `config_path`, emitting `[ok]`, `[warn]`, `[fail]`, and `[skipped]`
/// lines and returning `0` iff zero checks fail.
pub fn run_doctor(config_path: &Path, stdout: &mut dyn Write, _stderr: &mut dyn Write) -> i32 {
    let mut fail_count = 0_usize;

    let cfg = match GapConfig::load_from_path_with_process_env(config_path) {
        Ok(c) => {
            emit_check(
                stdout,
                &mut fail_count,
                CheckStatus::Ok,
                "config.parse",
                &format!(
                    "parsed {} (profile={})",
                    config_path.display(),
                    profile_label(c.profile)
                ),
                None,
            );
            c
        }
        Err(err) => {
            emit_check(
                stdout,
                &mut fail_count,
                CheckStatus::Fail,
                "config.parse",
                &err.to_string(),
                Some(&format!(
                    "fix the configuration error in `{}` and verify with `gap config check {}`",
                    config_path.display(),
                    config_path.display()
                )),
            );
            emit_m1_skipped(stdout, &mut fail_count);
            return EXIT_RUNTIME_ERROR;
        }
    };

    let base_dir = resolve_base_dir(config_path);
    check_referenced_files(&cfg, &base_dir, stdout, &mut fail_count);
    check_listen_addr(&cfg, stdout, &mut fail_count);
    check_api_keys(&cfg, stdout, &mut fail_count);
    check_t2_branches(&cfg, stdout, &mut fail_count);
    check_stdio_mcp_branches(&cfg, stdout, &mut fail_count);
    emit_m1_skipped(stdout, &mut fail_count);

    if fail_count == 0 {
        EXIT_OK
    } else {
        EXIT_RUNTIME_ERROR
    }
}
