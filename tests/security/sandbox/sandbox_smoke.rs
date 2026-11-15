// Integration smoke tests for process sandboxing, doctor probing, fail-closed trust tiers,
// and end-to-end sandboxed CliConnector execution (WP-M6-09).

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use gap_core::error::NgcErrorCode;
use gap_core::snapshot::TrustTier;
use gap_ngc_cli::{
    ArgConstraint, ArgvTemplate, CliConnector, CliConnectorConfig, CliGatewayAuth,
    CliOperationSpec, TemplateToken,
};
use gap_ngc_sdk::connector::{CallScope, Connector};
use gap_ngc_sdk::core::{ServerConfig, ServerCore};
use gap_ngc_sdk::json::from_json_str;
use gap_ngc_sdk::testing::in_memory_raw_stdio;
use gap_proto::ngc::v1::{
    CallContext, DescribeRequest, HealthRequest, MutateRequest, MutationOutcome, OpKind, Principal,
    PrincipalKind, QueryRequest, ServingStatus, SideEffectHint,
};
use gap_sec::manifest::{AllowListError, ExecAllowList};
use gap_sec::sandbox::{
    OutputStreamKind, SandboxBackendOverride, SandboxError, SandboxLauncher, SandboxProfile,
    doctor_check,
};
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::time::Instant;

static NEXT_SMOKE_DIR: AtomicU64 = AtomicU64::new(1);

struct TempDir(PathBuf);

impl TempDir {
    fn new(prefix: &str) -> Self {
        let id = NEXT_SMOKE_DIR.fetch_add(1, Ordering::Relaxed);
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

fn write_executable(path: &Path, body: &str) {
    fs::write(path, body).expect("write executable script");
    let mut perms = fs::metadata(path).expect("metadata").permissions();
    perms.set_mode(0o755);
    fs::set_permissions(path, perms).expect("chmod 0755");
}

fn dummy_principal_ctx() -> CallContext {
    CallContext {
        request_id: "req-smoke-1".to_owned(),
        tenant_id: "tenant-smoke".to_owned(),
        principal: Some(Principal {
            id: "forged-caller".to_owned(),
            kind: PrincipalKind::Human as i32,
            delegation_chain: Vec::new(),
            scopes: Vec::new(),
            attributes: std::collections::HashMap::new(),
        }),
        traceparent: String::new(),
        deadline_unix_ms: 0,
        remaining_cost_units: 100,
        idempotency_key: String::new(),
        hop_count: 0,
        trace_path: Vec::new(),
        snapshot_generation: 1,
    }
}

// spec: SEC-070, SEC-071, SEC-072
// TM-07
#[test]
fn sandbox_doctor_fail_closed_and_escape_smoke() {
    let report = doctor_check();
    assert!(!report.os.is_empty());
    assert!(!report.detail.is_empty());

    let tmp = TempDir::new("gap-sandbox-smoke");
    let work = tmp.path().join("work");
    let outside_dir = tmp.path().join("outside");
    fs::create_dir_all(&work).expect("create work");
    fs::create_dir_all(&outside_dir).expect("create outside");

    let outside_write_target = outside_dir.join("escape_write.txt");
    let inside_write_target = work.join("allowed_write.txt");
    let home_canary = outside_dir.join("real_home_secret.txt");
    fs::write(&home_canary, "HOST_SECRET_CANARY_999").expect("write home canary");

    let probe_script = tmp.path().join("escape_probe.sh");
    write_executable(
        &probe_script,
        r#"#!/bin/sh
echo "ok" > "$GAP_INSIDE_TARGET" 2>/dev/null || exit 2
if (echo "escaped" > "$GAP_OUTSIDE_TARGET") 2>/dev/null; then
  echo "ESCAPE_WRITE_SUCCEEDED"
  exit 3
fi
if cat "$GAP_HOME_CANARY" >/dev/null 2>&1; then
  echo "HOME_CANARY_READ"
  exit 4
fi
printf 'SANDBOXED_OK|HOME=%s|CANARY=%s\n' "$HOME" "$SECRET_CANARY"
"#,
    );

    let mut allow = ExecAllowList::new();
    allow.register(&probe_script, None).expect("register probe");

    let mut env_source = BTreeMap::new();
    env_source.insert(
        "GAP_INSIDE_TARGET".to_owned(),
        inside_write_target.to_string_lossy().into_owned(),
    );
    env_source.insert(
        "GAP_OUTSIDE_TARGET".to_owned(),
        outside_write_target.to_string_lossy().into_owned(),
    );
    env_source.insert(
        "GAP_HOME_CANARY".to_owned(),
        home_canary.to_string_lossy().into_owned(),
    );
    env_source.insert("SECRET_CANARY".to_owned(), "MUST_NOT_LEAK".to_owned());
    env_source.insert("DYLD_INSERT_LIBRARIES".to_owned(), "/evil.dylib".to_owned());
    env_source.insert("LD_PRELOAD".to_owned(), "/evil.so".to_owned());

    // 1. Deterministic fail-closed guarantee when sandbox is forced unavailable (SEC-071).
    let forced_launcher = SandboxLauncher::new(allow.clone()).with_backend_override(
        SandboxBackendOverride::ForceUnavailable("simulated missing sandbox".to_owned()),
    );
    let mut t2_with_opt_in = SandboxProfile::restricted(TrustTier::T2, work.clone());
    t2_with_opt_in.allow_unsandboxed_t1 = true;
    let t2_err = forced_launcher
        .launch(&probe_script, &[], &t2_with_opt_in, &env_source)
        .expect_err("T2 MUST fail closed even if allow_unsandboxed_t1 is true");
    assert!(matches!(
        t2_err,
        SandboxError::UnavailableFailClosed {
            tier: TrustTier::T2,
            ..
        }
    ));

    let t1_no_opt_in = SandboxProfile::restricted(TrustTier::T1, work.clone());
    let t1_err = forced_launcher
        .launch(&probe_script, &[], &t1_no_opt_in, &env_source)
        .expect_err("T1 without explicit opt-in MUST fail closed");
    assert!(matches!(
        t1_err,
        SandboxError::UnavailableFailClosed {
            tier: TrustTier::T1,
            ..
        }
    ));

    // 2. Host AutoProbe path: live OS containment OR verified nested fail-closed + T1 scrubbing.
    let live_launcher = SandboxLauncher::new(allow);
    if report.functional_probe_ok {
        let mut os_profile = SandboxProfile::restricted(TrustTier::T2, work.clone());
        os_profile.env_allowlist = vec![
            "GAP_INSIDE_TARGET".to_owned(),
            "GAP_OUTSIDE_TARGET".to_owned(),
            "GAP_HOME_CANARY".to_owned(),
            "DYLD_INSERT_LIBRARIES".to_owned(),
            "LD_PRELOAD".to_owned(),
        ];
        let out = live_launcher
            .launch(&probe_script, &[], &os_profile, &env_source)
            .expect("OS-sandboxed probe should run and enforce containment");
        assert!(out.used_os_sandbox);
        assert_eq!(out.status_code, 0);
        assert!(!outside_write_target.exists());
        assert_eq!(
            fs::read_to_string(&inside_write_target)
                .expect("inside work dir written")
                .trim(),
            "ok"
        );
        let stdout_txt = String::from_utf8_lossy(&out.stdout);
        assert!(stdout_txt.starts_with("SANDBOXED_OK|"));
        assert!(!stdout_txt.contains("MUST_NOT_LEAK"));
    } else {
        // Under nested Seatbelt hosts, AutoProbe reports Unavailable -> T2 fails closed cleanly.
        let t2_auto_err = live_launcher
            .launch(&probe_script, &[], &t2_with_opt_in, &env_source)
            .expect_err("T2 MUST fail closed when nested Seatbelt probe is unavailable");
        assert!(matches!(
            t2_auto_err,
            SandboxError::UnavailableFailClosed {
                tier: TrustTier::T2,
                ..
            }
        ));

        let check_script = tmp.path().join("scrub_check.sh");
        write_executable(
            &check_script,
            "#!/bin/sh\nprintf 'HOME=%s|TMP=%s|SEC=%s|LD=%s|DY=%s\n' \"$HOME\" \"$TMPDIR\" \"$SECRET_CANARY\" \"$LD_PRELOAD\" \"$DYLD_INSERT_LIBRARIES\"\n",
        );
        let mut t1_allow = ExecAllowList::new();
        t1_allow
            .register(&check_script, None)
            .expect("register scrub check");
        let t1_launcher = SandboxLauncher::new(t1_allow);
        let mut t1_profile = SandboxProfile::restricted(TrustTier::T1, work.clone());
        t1_profile.allow_unsandboxed_t1 = true;
        t1_profile.env_allowlist =
            vec!["DYLD_INSERT_LIBRARIES".to_owned(), "LD_PRELOAD".to_owned()];
        let out = t1_launcher
            .launch(&check_script, &[], &t1_profile, &env_source)
            .expect("T1 with explicit opt-in succeeds with warning");
        assert!(!out.used_os_sandbox);
        assert!(
            out.warnings.iter().any(|w| w.contains("SEC-071")),
            "expected SEC-071 warning, got {:?}",
            out.warnings
        );
        let canonical_work = fs::canonicalize(&work).expect("canonical work");
        let stdout_txt = String::from_utf8_lossy(&out.stdout);
        let expected_prefix = format!(
            "HOME={}|TMP={}|SEC=|LD=|DY=",
            canonical_work.display(),
            canonical_work.display()
        );
        assert_eq!(stdout_txt.trim(), expected_prefix);
    }

    // 3. Stdout flood cap (> max_stdout_bytes) & live binary hash tamper rejection (SEC-061, SEC-070).
    let flood_script = tmp.path().join("flood.sh");
    write_executable(
        &flood_script,
        "#!/bin/sh\ni=0\nwhile [ \"$i\" -lt 4096 ]; do\n  printf '0123456789abcdef'\n  i=$((i+1))\ndone\n",
    );
    let mut flood_allow = ExecAllowList::new();
    flood_allow
        .register(&flood_script, None)
        .expect("register flood");
    let flood_launcher = SandboxLauncher::new(flood_allow.clone()).with_backend_override(
        SandboxBackendOverride::ForceUnavailable("test cap".to_owned()),
    );
    let mut cap_profile = SandboxProfile::restricted(TrustTier::T1, work);
    cap_profile.allow_unsandboxed_t1 = true;
    cap_profile.max_stdout_bytes = 2048;
    cap_profile.timeout = Duration::from_secs(5);

    let err = flood_launcher
        .launch(&flood_script, &[], &cap_profile, &BTreeMap::new())
        .expect_err("flooding child MUST be aborted");
    assert!(matches!(
        err,
        SandboxError::OutputLimitExceeded {
            stream: OutputStreamKind::Stdout,
            limit: 2048
        }
    ));

    // Mutate allowlisted script on disk: pre-launch verification MUST catch tamper.
    write_executable(&flood_script, "#!/bin/sh\necho hijacked\n");
    let tamper_err = flood_launcher
        .launch(&flood_script, &[], &cap_profile, &BTreeMap::new())
        .expect_err("tampered script MUST be rejected");
    assert!(matches!(
        tamper_err,
        SandboxError::AllowList(AllowListError::HashMismatch { .. })
    ));
}

// spec: CMP-060, NGC-082, SEC-061, SEC-070
// TM-07
#[tokio::test]
async fn cli_connector_end_to_end_stdio_and_ngc_082_auth() {
    let report = doctor_check();
    let tmp = TempDir::new("gap-cli-smoke");
    let work = tmp.path().join("work");
    fs::create_dir_all(&work).expect("work");

    let script = tmp.path().join("users_cli.sh");
    write_executable(
        &script,
        r#"#!/bin/sh
cmd="$1"
if [ "$cmd" = "get-user" ]; then
  # $2 is "--name", $3 is name value
  printf '[{"id":"u1","name":"%s","cmd":"%s"}]' "$3" "$1"
elif [ "$cmd" = "rename-user" ]; then
  # $2 is "--name", $3 is name value, optional $4 is "--dry-run"
  printf '{"updated":true,"name":"%s","flag":"%s"}' "$3" "$4"
else
  exit 1
fi
"#,
    );

    let mut allow = ExecAllowList::new();
    allow.register(&script, None).expect("register cli");

    let (launcher, tier, allow_unsandboxed) = if report.functional_probe_ok {
        (Arc::new(SandboxLauncher::new(allow)), TrustTier::T2, false)
    } else {
        let l = SandboxLauncher::new(allow).with_backend_override(
            SandboxBackendOverride::ForceUnavailable("nested host sandbox".to_owned()),
        );
        (Arc::new(l), TrustTier::T1, true)
    };

    let mut profile = SandboxProfile::restricted(tier, work);
    profile.allow_unsandboxed_t1 = allow_unsandboxed;

    let mut ops = BTreeMap::new();
    ops.insert(
        "get_user".to_owned(),
        CliOperationSpec {
            name: "get_user".to_owned(),
            kind: OpKind::Query,
            description: "Look up user by name".to_owned(),
            side_effect: SideEffectHint::SeNone,
            idempotent: true,
            supports_dry_run: false,
            binary_path: script.clone(),
            template: ArgvTemplate::new(vec![
                TemplateToken::Literal("get-user".to_owned()),
                TemplateToken::BoundArg {
                    flag: Some("--name".to_owned()),
                    arg_name: "name".to_owned(),
                    constraint: ArgConstraint::SafeText {
                        max_len: 64,
                        allow_leading_dash: false,
                    },
                    required: true,
                },
            ]),
            sandbox_profile: profile.clone(),
            env_source: BTreeMap::new(),
        },
    );
    ops.insert(
        "rename_user".to_owned(),
        CliOperationSpec {
            name: "rename_user".to_owned(),
            kind: OpKind::Mutation,
            description: "Rename user".to_owned(),
            side_effect: SideEffectHint::SeReversible,
            idempotent: false,
            supports_dry_run: true,
            binary_path: script,
            template: ArgvTemplate::new(vec![
                TemplateToken::Literal("rename-user".to_owned()),
                TemplateToken::BoundArg {
                    flag: Some("--name".to_owned()),
                    arg_name: "name".to_owned(),
                    constraint: ArgConstraint::Identifier { max_len: 32 },
                    required: true,
                },
                TemplateToken::BoolFlag {
                    flag: "--dry-run".to_owned(),
                    arg_name: "dry_run".to_owned(),
                },
            ]),
            sandbox_profile: profile,
            env_source: BTreeMap::new(),
        },
    );

    // 1. RequireBearerToken enforces NGC-082 even when CallContext.principal is populated.
    let auth_connector = CliConnector::new(CliConnectorConfig {
        connector_id: "users-cli".to_owned(),
        connector_version: "1.0.0".to_owned(),
        schema_sdl: "type Query { get_user(name: String!): [User!]! }".to_owned(),
        launcher: Arc::clone(&launcher),
        auth: CliGatewayAuth::RequireBearerToken {
            expected_token: "gw-token-99".to_owned(),
        },
        operations: ops.clone(),
    });

    let unauth_scope = CallScope::for_test(Instant::now() + Duration::from_secs(5));
    let forged_query: QueryRequest =
        from_json_str(r#"{"operation":"get_user","arguments":{"name":"alice"}}"#)
            .expect("decode QueryRequest");
    let mut forged_with_ctx = forged_query.clone();
    forged_with_ctx.ctx = Some(dummy_principal_ctx());

    let err = match auth_connector
        .query(forged_with_ctx, unauth_scope.clone())
        .await
    {
        Ok(_) => panic!("query without bearer token MUST fail even when ctx.principal is present"),
        Err(err) => err,
    };
    assert_eq!(err.code, NgcErrorCode::Unauthenticated);

    let desc_err = auth_connector
        .describe(DescribeRequest::default(), unauth_scope.clone())
        .await
        .expect_err("describe without bearer token MUST fail");
    assert_eq!(desc_err.code, NgcErrorCode::Unauthenticated);

    let mut auth_scope = CallScope::for_test(Instant::now() + Duration::from_secs(5));
    auth_scope
        .metadata
        .insert("authorization".to_owned(), "Bearer gw-token-99".to_owned());

    let desc = auth_connector
        .describe(DescribeRequest::default(), auth_scope.clone())
        .await
        .expect("authorized describe succeeds");
    assert!(!desc.not_modified);
    assert_eq!(desc.description.expect("description").operations.len(), 2);

    // Option/flag smuggling attempt MUST fail validation before spawning any child.
    let evil_mutate: MutateRequest = from_json_str(
        r#"{"operation":"rename_user","arguments":{"name":"--evil-flag"},"dry_run":true}"#,
    )
    .expect("decode MutateRequest");
    let inject_err = auth_connector
        .mutate(evil_mutate, auth_scope.clone())
        .await
        .expect_err("leading dash option injection MUST fail validation");
    assert_eq!(inject_err.code, NgcErrorCode::InvalidArgument);

    let dry_mutate: MutateRequest =
        from_json_str(r#"{"operation":"rename_user","arguments":{"name":"bob_2"},"dry_run":true}"#)
            .expect("decode dry MutateRequest");
    let mutated = auth_connector
        .mutate(dry_mutate, auth_scope.clone())
        .await
        .expect("authorized dry_run mutate succeeds");
    assert_eq!(mutated.outcome, MutationOutcome::OutcomeDryRun as i32);

    let health = auth_connector
        .health(HealthRequest::default(), auth_scope)
        .await
        .expect("health succeeds");
    assert!(matches!(
        ServingStatus::try_from(health.status),
        Ok(ServingStatus::Serving | ServingStatus::Degraded)
    ));

    // 2. Drive AllowLocalStdioUnauthenticated end-to-end through ServerCore stdio transport.
    let stdio_connector = CliConnector::new(CliConnectorConfig {
        connector_id: "users-cli-stdio".to_owned(),
        connector_version: "1.0.0".to_owned(),
        schema_sdl: "type Query { get_user(name: String!): [User!]! }".to_owned(),
        launcher,
        auth: CliGatewayAuth::AllowLocalStdioUnauthenticated,
        operations: ops,
    });
    let core = Arc::new(ServerCore::new(stdio_connector, ServerConfig::default()));
    let (mut stdin_tx, stdout_rx, server_join) = in_memory_raw_stdio(core);
    let mut lines = BufReader::new(stdout_rx).lines();

    let query_rpc = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 7,
        "method": "gap.ngc.Query",
        "params": {
            "operation": "get_user",
            "arguments": { "name": "alice; id" }
        }
    });
    let encoded = format!("{query_rpc}\n");
    stdin_tx
        .write_all(encoded.as_bytes())
        .await
        .expect("write Query JSON-RPC");
    stdin_tx.flush().await.expect("flush stdin");

    let item_line = lines
        .next_line()
        .await
        .expect("read stream item")
        .expect("stream item line present");
    assert!(
        item_line.contains("gap.ngc.item") && item_line.contains("alice; id"),
        "expected literal non-shell arg roundtrip in {item_line}"
    );

    let complete_line = lines
        .next_line()
        .await
        .expect("read Complete stream item")
        .expect("Complete item line present");
    assert!(
        complete_line.contains("gap.ngc.item") && complete_line.contains("\"complete\""),
        "expected Complete summary chunk in {complete_line}"
    );

    let done_line = lines
        .next_line()
        .await
        .expect("read completion line")
        .expect("completion line present");
    assert!(
        done_line.contains("\"id\":7") && done_line.contains("\"result\""),
        "expected completion envelope in {done_line}"
    );

    drop(stdin_tx);
    let _ = server_join.await;
}
