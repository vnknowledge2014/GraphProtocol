//! Unit tests for typed `ArgvTemplate` rendering, sandboxed `CliConnector` execution, and `NGC-082` caller auth.

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use gap_core::error::NgcErrorCode;
use gap_core::snapshot::TrustTier;
use gap_proto::ngc::v1::{
    CallContext, DescribeRequest, OpKind, Principal, PrincipalKind, SideEffectHint,
};
use gap_sec::authn::json::JsonValue;
use gap_sec::manifest::ExecAllowList;
use gap_sec::sandbox::{SandboxBackendOverride, SandboxLauncher, SandboxProfile};

use super::*;

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

fn write_script(path: &Path, body: &str) {
    fs::write(path, body).expect("write script");
    let mut perms = fs::metadata(path).expect("metadata").permissions();
    perms.set_mode(0o755);
    fs::set_permissions(path, perms).expect("chmod 0755");
}

// spec: CMP-060, SEC-061, SEC-070
// TM-07
#[test]
fn cli_argv_template_blocks_option_injection_and_executes_sandboxed_connector() {
    let tmp = TempDir::new("gap-cli-conn");
    let work = tmp.path().join("work");
    let script = tmp.path().join("orders_cli.sh");
    write_script(
        &script,
        "#!/bin/sh\n# Prints JSON object containing received argv tokens\nprintf '{\"op\":\"%s\",\"id\":\"%s\",\"limit\":%s}' \"$1\" \"$3\" \"$5\"\n",
    );

    let mut allow = ExecAllowList::new();
    allow.register(&script, None).expect("register cli");
    let launcher = Arc::new(SandboxLauncher::new(allow).with_backend_override(
        SandboxBackendOverride::ForceUnavailable("unit test harness".to_owned()),
    ));

    let mut profile = SandboxProfile::restricted(TrustTier::T1, work);
    profile.allow_unsandboxed_t1 = true;

    let template = ArgvTemplate::new(vec![
        TemplateToken::Literal("get-order".to_owned()),
        TemplateToken::BoundArg {
            flag: Some("--id".to_owned()),
            arg_name: "id".to_owned(),
            constraint: ArgConstraint::Identifier { max_len: 32 },
            required: true,
        },
        TemplateToken::BoundArg {
            flag: Some("--limit".to_owned()),
            arg_name: "limit".to_owned(),
            constraint: ArgConstraint::Integer {
                min: Some(1),
                max: Some(100),
            },
            required: true,
        },
    ]);

    let connector = CliConnector::new(CliConnectorConfig {
        connector_id: "orders-cli".to_owned(),
        connector_version: "0.1.0".to_owned(),
        schema_sdl: "type Order { id: ID! }".to_owned(),
        launcher,
        auth: CliGatewayAuth::AllowLocalStdioUnauthenticated,
        operations: BTreeMap::from([(
            "getOrder".to_owned(),
            CliOperationSpec {
                name: "getOrder".to_owned(),
                kind: OpKind::Query,
                description: "Read order via CLI".to_owned(),
                side_effect: SideEffectHint::SeNone,
                idempotent: true,
                supports_dry_run: false,
                binary_path: script,
                template,
                sandbox_profile: profile,
                env_source: BTreeMap::new(),
            },
        )]),
    });

    // Rejects leading `-` option smuggling & shell metacharacters without launching binary.
    let evil_args = BTreeMap::from([
        ("id".to_owned(), JsonValue::String("--exec=sh".to_owned())),
        ("limit".to_owned(), JsonValue::Int(10)),
    ]);
    let inject_err = connector
        .execute_json_with_metadata("getOrder", &evil_args, &BTreeMap::new(), None, None)
        .expect_err("leading dash blocked");
    assert_eq!(inject_err.code, NgcErrorCode::InvalidArgument);

    let shell_args = BTreeMap::from([
        (
            "id".to_owned(),
            JsonValue::String("ord1;rm -rf /".to_owned()),
        ),
        ("limit".to_owned(), JsonValue::Int(10)),
    ]);
    let meta_err = connector
        .execute_json_with_metadata("getOrder", &shell_args, &BTreeMap::new(), None, None)
        .expect_err("shell characters blocked");
    assert_eq!(meta_err.code, NgcErrorCode::InvalidArgument);

    // Valid invocation produces parsed JSON.
    let valid_args = BTreeMap::from([
        ("id".to_owned(), JsonValue::String("ord_101".to_owned())),
        ("limit".to_owned(), JsonValue::Int(25)),
    ]);
    let json_val = connector
        .execute_json_with_metadata("getOrder", &valid_args, &BTreeMap::new(), None, None)
        .expect("valid CLI invocation succeeds");
    assert_eq!(
        json_val
            .as_object()
            .and_then(|o| o.get("id"))
            .and_then(JsonValue::as_str),
        Some("ord_101")
    );
}

// spec: NGC-082
#[test]
fn cli_connector_never_accepts_ctx_principal_as_sole_authentication() {
    let tmp = TempDir::new("gap-cli-auth");
    let work = tmp.path().join("work");
    let script = tmp.path().join("echo.sh");
    write_script(&script, "#!/bin/sh\necho '{\"ok\":true}'\n");

    let mut allow = ExecAllowList::new();
    allow.register(&script, None).expect("register cli");
    let launcher = Arc::new(SandboxLauncher::new(allow).with_backend_override(
        SandboxBackendOverride::ForceUnavailable("unit test harness".to_owned()),
    ));
    let mut profile = SandboxProfile::restricted(TrustTier::T1, work);
    profile.allow_unsandboxed_t1 = true;

    let connector = CliConnector::new(CliConnectorConfig {
        connector_id: "auth-cli".to_owned(),
        connector_version: "0.1.0".to_owned(),
        schema_sdl: "type Query { ping: String }".to_owned(),
        launcher,
        auth: CliGatewayAuth::RequireBearerToken {
            expected_token: "gateway-secret-token-xyz".to_owned(),
        },
        operations: BTreeMap::from([(
            "ping".to_owned(),
            CliOperationSpec {
                name: "ping".to_owned(),
                kind: OpKind::Query,
                description: "Ping".to_owned(),
                side_effect: SideEffectHint::SeNone,
                idempotent: true,
                supports_dry_run: false,
                binary_path: script,
                template: ArgvTemplate::new(vec![]),
                sandbox_profile: profile,
                env_source: BTreeMap::new(),
            },
        )]),
    });

    // Attacker supplies populated `ctx.principal` claiming admin identity, WITHOUT valid gateway authorization metadata.
    let spoofed_ctx = CallContext {
        tenant_id: "acme".to_owned(),
        request_id: "req-spoof".to_owned(),
        traceparent: String::new(),
        deadline_unix_ms: 0,
        remaining_cost_units: 100,
        principal: Some(Principal {
            id: "admin@acme.com".to_owned(),
            kind: PrincipalKind::Human as i32,
            delegation_chain: Vec::new(),
            scopes: vec!["admin:*".to_owned()],
            attributes: std::collections::HashMap::new(),
        }),
        hop_count: 0,
        trace_path: Vec::new(),
        idempotency_key: String::new(),
        snapshot_generation: 1,
    };

    let no_auth_err = connector
        .execute_json_with_metadata(
            "ping",
            &BTreeMap::new(),
            &BTreeMap::new(),
            Some(&spoofed_ctx),
            None,
        )
        .expect_err("ctx.principal alone MUST NOT authenticate caller (NGC-082)");
    assert_eq!(no_auth_err.code, NgcErrorCode::Unauthenticated);

    let wrong_token_meta =
        BTreeMap::from([("authorization".to_owned(), "Bearer wrong-token".to_owned())]);
    let wrong_err = connector
        .describe_with_metadata(DescribeRequest::default(), &wrong_token_meta)
        .expect_err("invalid bearer token rejected despite ctx.principal");
    assert_eq!(wrong_err.code, NgcErrorCode::Unauthenticated);

    // Valid gateway metadata succeeds even alongside informational CallContext.
    let valid_meta = BTreeMap::from([(
        "authorization".to_owned(),
        "Bearer gateway-secret-token-xyz".to_owned(),
    )]);
    let ok = connector
        .execute_json_with_metadata(
            "ping",
            &BTreeMap::new(),
            &valid_meta,
            Some(&spoofed_ctx),
            None,
        )
        .expect("valid gateway auth metadata passes");
    assert_eq!(
        ok.as_object()
            .and_then(|o| o.get("ok"))
            .and_then(JsonValue::as_bool),
        Some(true)
    );
}
