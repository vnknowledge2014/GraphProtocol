//! Unit tests for Seatbelt/bwrap profiles, fail-closed trust tiers, scrubbed env, timeout, and output caps.

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use gap_core::error::ErrorCode;
use gap_core::snapshot::TrustTier;

use super::*;
use crate::manifest::ExecAllowList;

static NEXT_TMP_ID: AtomicU64 = AtomicU64::new(1);

struct TempDir(PathBuf);

impl TempDir {
    fn new(prefix: &str) -> Self {
        let id = NEXT_TMP_ID.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("{prefix}-{}-{id}", std::process::id()));
        fs::create_dir_all(&dir).expect("create temp dir");
        Self(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn write_executable_script(path: &Path, body: &str) {
    fs::write(path, body).expect("write script");
    let mut perms = fs::metadata(path).expect("metadata").permissions();
    perms.set_mode(0o755);
    fs::set_permissions(path, perms).expect("chmod 0755");
}

// spec: SEC-070, SEC-072
#[test]
fn sandbox_profiles_render_seatbelt_and_bwrap_and_doctor_probe_runs() {
    let tmp = TempDir::new("gap-sandbox-prof");
    let work = tmp.path().join("work");
    let bin = tmp.path().join("tool-bin");
    fs::create_dir_all(&work).expect("work dir");
    write_executable_script(&bin, "#!/bin/sh\necho ok\n");

    let profile = SandboxProfile::restricted(TrustTier::T2, work.clone());
    let seatbelt = render_macos_seatbelt_profile(&profile, &bin, &work);
    assert!(seatbelt.contains("(deny default)"));
    assert!(seatbelt.contains("(deny network*)"));
    assert!(seatbelt.contains("file-write*"));

    let bwrap_args = render_linux_bwrap_args(&profile, &bin, &work, &["--status".to_owned()]);
    let joined: Vec<String> = bwrap_args
        .iter()
        .map(|s| s.to_string_lossy().into_owned())
        .collect();
    assert!(joined.contains(&"--ro-bind".to_owned()));
    assert!(joined.contains(&"--unshare-net".to_owned()));
    assert!(joined.contains(&"--cap-drop".to_owned()));
    assert!(joined.contains(&"--new-session".to_owned()));
    let bin_str = bin.to_string_lossy().into_owned();
    assert!(
        joined.iter().filter(|a| *a == &bin_str).count() >= 3,
        "expected `--ro-bind <bin> <bin>` plus command argv in {joined:?}"
    );

    let mut proxy_profile = profile;
    proxy_profile.allow_network_via_egress_proxy =
        Some(std::net::SocketAddr::from(([127, 0, 0, 1], 9090)));
    let seatbelt_net = render_macos_seatbelt_profile(&proxy_profile, &bin, &work);
    assert!(seatbelt_net.contains("127.0.0.1:9090"));

    // `doctor_check()` returns a clean diagnostic report regardless of whether outer container permits nesting.
    let report = doctor_check();
    assert!(!report.os.is_empty());
    assert!(!report.detail.is_empty());
}

// spec: SEC-070, SEC-071
#[test]
fn sandbox_launcher_enforces_fail_closed_tiers_env_scrubbing_timeout_and_output_cap() {
    let tmp = TempDir::new("gap-sandbox-exec");
    let work = tmp.path().join("work");
    let ok_script = tmp.path().join("echo_env.sh");
    let flood_script = tmp.path().join("flood.sh");
    let slow_script = tmp.path().join("sleep.sh");

    write_executable_script(
        &ok_script,
        "#!/bin/sh\necho \"SAFE=$SAFE_VAR|SECRET=${SECRET_TOKEN:-scrubbed}|DYLD=${DYLD_INSERT_LIBRARIES:-scrubbed}\"\n",
    );
    write_executable_script(
        &flood_script,
        "#!/bin/sh\ni=0\nwhile [ \"$i\" -lt 200 ]; do\n  printf 'AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA\\n'\n  i=$((i + 1))\ndone\n",
    );
    write_executable_script(&slow_script, "#!/bin/sh\nsleep 2\n");

    let mut allow = ExecAllowList::new();
    allow.register(&ok_script, None).expect("register ok");
    allow.register(&flood_script, None).expect("register flood");
    allow.register(&slow_script, None).expect("register slow");

    let env_source = BTreeMap::from([
        ("SAFE_VAR".to_owned(), "visible".to_owned()),
        ("SECRET_TOKEN".to_owned(), "should-never-leak".to_owned()),
        (
            "DYLD_INSERT_LIBRARIES".to_owned(),
            "/tmp/evil.dylib".to_owned(),
        ),
    ]);

    // 1. Unavailable sandbox -> T2 ALWAYS fails closed (SEC-071) even if allow_unsandboxed_t1=true
    let unavailable_launcher = SandboxLauncher::new(allow.clone()).with_backend_override(
        SandboxBackendOverride::ForceUnavailable("simulated missing bwrap".to_owned()),
    );

    let mut t2_profile = SandboxProfile::restricted(TrustTier::T2, work.clone());
    t2_profile.allow_unsandboxed_t1 = true;
    let t2_err = unavailable_launcher
        .launch(&ok_script, &[], &t2_profile, &env_source)
        .expect_err("T2 must fail closed when sandbox unavailable");
    assert!(matches!(
        t2_err,
        SandboxError::UnavailableFailClosed {
            tier: TrustTier::T2,
            ..
        }
    ));
    assert_eq!(t2_err.into_gap_error().code(), ErrorCode::Forbidden);

    // 2. T1 without opt-in also fails closed; T1 WITH opt-in runs with prominent warning + scrubbed env
    let t1_strict = SandboxProfile::restricted(TrustTier::T1, work.clone());
    assert!(matches!(
        unavailable_launcher.launch(&ok_script, &[], &t1_strict, &env_source),
        Err(SandboxError::UnavailableFailClosed {
            tier: TrustTier::T1,
            ..
        })
    ));

    let mut t1_optin = SandboxProfile::restricted(TrustTier::T1, work.clone());
    t1_optin.allow_unsandboxed_t1 = true;
    t1_optin.env_allowlist = vec![
        "SAFE_VAR".to_owned(),
        "DYLD_INSERT_LIBRARIES".to_owned(), // must STILL be scrubbed by FORBIDDEN_ENV_KEYS
    ];
    let out = unavailable_launcher
        .launch(&ok_script, &[], &t1_optin, &env_source)
        .expect("T1 explicit opt-in succeeds");
    assert_eq!(out.status_code, 0);
    assert!(!out.used_os_sandbox);
    assert!(!out.warnings.is_empty());
    let stdout_str = String::from_utf8_lossy(&out.stdout);
    assert!(stdout_str.contains("SAFE=visible|SECRET=scrubbed|DYLD=scrubbed"));

    // 3. Stdout limit cap stops flooding child cleanly without OOM
    let mut cap_profile = t1_optin.clone();
    cap_profile.max_stdout_bytes = 256;
    let flood_err = unavailable_launcher
        .launch(&flood_script, &[], &cap_profile, &env_source)
        .expect_err("stdout flood must fail with OutputLimitExceeded");
    assert!(matches!(
        flood_err,
        SandboxError::OutputLimitExceeded {
            stream: OutputStreamKind::Stdout,
            limit: 256
        }
    ));

    // 4. Wall-clock timeout kills child promptly
    let mut timeout_profile = t1_optin;
    timeout_profile.timeout = Duration::from_millis(80);
    let timeout_err = unavailable_launcher
        .launch(&slow_script, &[], &timeout_profile, &env_source)
        .expect_err("slow script must time out");
    assert!(matches!(timeout_err, SandboxError::TimedOut { .. }));
}

// spec: SEC-070
#[test]
fn sandbox_mcp_stdio_command_prepares_scrubbed_allowlisted_child() {
    let tmp = TempDir::new("gap-sandbox-mcp");
    let work = tmp.path().join("work");
    let mcp_bin = tmp.path().join("mcp-server.sh");
    write_executable_script(&mcp_bin, "#!/bin/sh\necho '{\"jsonrpc\":\"2.0\"}'\n");

    let mut allow = ExecAllowList::new();
    let expected_hash = allow.register(&mcp_bin, None).expect("register mcp bin");

    let launcher = SandboxLauncher::new(allow).with_backend_override(
        SandboxBackendOverride::ForceUnavailable("deterministic unit check".to_owned()),
    );
    let mut profile = SandboxProfile::restricted(TrustTier::T1, work);
    profile.allow_unsandboxed_t1 = true;
    profile.env_allowlist = vec!["MCP_MODE".to_owned()];

    let env_src = BTreeMap::from([
        ("MCP_MODE".to_owned(), "stdio".to_owned()),
        ("AWS_SECRET_ACCESS_KEY".to_owned(), "topsecret".to_owned()),
    ]);

    let (mut child, prepared) = launcher
        .spawn_mcp_stdio(&mcp_bin, &["--stdio".to_owned()], &profile, &env_src)
        .expect("spawn sandboxed mcp stdio command");
    assert_eq!(prepared.verified_binary.sha256_hex, expected_hash);
    assert_eq!(
        prepared.clean_env.get("MCP_MODE").map(String::as_str),
        Some("stdio")
    );
    assert!(!prepared.clean_env.contains_key("AWS_SECRET_ACCESS_KEY"));
    let status = child.wait().expect("wait mcp stub");
    assert!(status.success());
}
