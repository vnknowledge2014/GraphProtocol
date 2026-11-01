//! In-process integration tests for `gap-cli` (`OPS-044`, `OPS-001`, `SEC-062`, `INV-15`,
//! `Brief §4`, zero sockets, zero child processes).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;
use tempfile::TempDir;

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("canonical repo root")
}

async fn run_gap(args: &[&str]) -> (i32, String, String) {
    let mut full_args = Vec::with_capacity(args.len() + 1);
    full_args.push("gap");
    full_args.extend_from_slice(args);
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let code = gap_cli::cli::run_with_io(full_args, &mut stdout, &mut stderr).await;
    (
        code,
        String::from_utf8(stdout).expect("stdout UTF-8"),
        String::from_utf8(stderr).expect("stderr UTF-8"),
    )
}

// spec: OPS-044
#[tokio::test]
async fn help_lists_every_ops_044_command_and_subcommand() {
    let (code, stdout, stderr) = run_gap(&["--help"]).await;
    assert_eq!(code, 0, "stderr={stderr}");
    for top in [
        "serve",
        "init",
        "doctor",
        "config",
        "models",
        "proposals",
        "snapshot",
        "policy",
        "audit",
        "calibrate",
        "bench",
        "ingest",
        "tasks",
        "purge",
    ] {
        assert!(
            stdout.contains(top),
            "`gap --help` must list `{top}`, got:\n{stdout}"
        );
    }

    let subcommand_groups: &[(&str, &[&str])] = &[
        ("config", &["schema", "check"]),
        ("models", &["pull", "verify"]),
        ("proposals", &["list", "show", "approve", "deny"]),
        ("snapshot", &["show", "diff", "rollback"]),
        ("policy", &["check", "reload"]),
        ("audit", &["verify", "export"]),
        ("ingest", &["run", "once"]),
        ("tasks", &["list", "show", "cancel"]),
    ];
    for (group, subs) in subcommand_groups {
        let (sub_code, sub_out, sub_err) = run_gap(&[group, "--help"]).await;
        assert_eq!(sub_code, 0, "group={group}, stderr={sub_err}");
        for sub in *subs {
            assert!(
                sub_out.contains(sub),
                "`gap {group} --help` must list `{sub}`, got:\n{sub_out}"
            );
        }
    }
}

// spec: OPS-001, OPS-044
#[tokio::test]
async fn config_schema_matches_docs_config_schema_json_and_contains_branches_and_api_keys() {
    let (code, stdout, stderr) = run_gap(&["config", "schema"]).await;
    assert_eq!(code, 0, "stderr={stderr}");

    let parsed: Value = serde_json::from_str(&stdout).expect("schema must be valid JSON");
    assert!(parsed["properties"].get("branches").is_some());
    assert!(parsed["properties"].get("api_keys").is_some());

    let canonical =
        fs::read_to_string(repo_root().join("docs/config.schema.json")).expect("read canonical");
    assert_eq!(
        stdout, canonical,
        "`gap config schema` must match `docs/config.schema.json` byte-for-byte"
    );
}

// spec: OPS-001, OPS-044, INV-15
#[tokio::test]
async fn config_check_accepts_walking_skeleton_and_exits_2_on_broken_or_missing_files() {
    let skeleton = repo_root().join("examples/walking-skeleton/gap.toml");
    let (code, stdout, stderr) = run_gap(&["config", "check", skeleton.to_str().unwrap()]).await;
    assert_eq!(code, 0, "stderr={stderr}");
    assert!(stdout.contains("ok:"));

    // Nonexistent config -> exit 2
    let (missing_code, _, _) = run_gap(&["config", "check", "/nonexistent/gap.toml"]).await;
    assert_eq!(missing_code, 2);

    // serve with nonexistent config -> exit 2 (before any socket bind)
    let (serve_bad_code, _, _) = run_gap(&["serve", "--config", "/nonexistent/gap.toml"]).await;
    assert_eq!(serve_bad_code, 2);

    // Missing OpenAPI document -> exit 2 naming branches[0].openapi.spec
    let tmp = TempDir::new().unwrap();
    let broken = tmp.path().join("gap.toml");
    fs::write(
        &broken,
        r#"
profile = "dev"

[[branches]]
id = "shop"
protocol = "openapi"
[branches.openapi]
spec = "missing.openapi.json"
base_url = "http://127.0.0.1:9101"
"#,
    )
    .unwrap();
    let (b_code, _, b_err) = run_gap(&["config", "check", broken.to_str().unwrap()]).await;
    assert_eq!(b_code, 2);
    assert!(
        b_err.contains("branches[0].openapi.spec"),
        "expected branches[0].openapi.spec in stderr, got: {b_err}"
    );

    // Inline secret canary -> exit 2 and canary never echoed
    let canary = "sk-cli-canary-secret-11223344";
    let secret_toml = tmp.path().join("secret.toml");
    fs::write(
        &secret_toml,
        format!(
            r#"
profile = "dev"
[auth]
introspection_client_secret = "{canary}"
"#
        ),
    )
    .unwrap();
    let (s_code, s_out, s_err) = run_gap(&["config", "check", secret_toml.to_str().unwrap()]).await;
    assert_eq!(s_code, 2);
    assert!(!s_out.contains(canary));
    assert!(!s_err.contains(canary));
}

// spec: OPS-044, OPS-001
#[tokio::test]
async fn init_writes_walking_skeleton_template_passes_config_check_and_respects_force() {
    let tmp = TempDir::new().unwrap();
    let project_dir = tmp.path().join("my-gap-project");

    let (code, _out, err) = run_gap(&["init", project_dir.to_str().unwrap()]).await;
    assert_eq!(code, 0, "init failed: {err}");

    for rel in [
        "gap.toml",
        "shop.openapi.json",
        "README.md",
        "fixtures/rest_fixture.py",
        "fixtures/mcp_fixture.py",
    ] {
        assert!(
            project_dir.join(rel).is_file(),
            "`gap init` must create `{rel}`"
        );
    }

    // Generated gap.toml must pass `gap config check`.
    let generated_toml = project_dir.join("gap.toml");
    let (chk_code, _, chk_err) =
        run_gap(&["config", "check", generated_toml.to_str().unwrap()]).await;
    assert_eq!(chk_code, 0, "generated gap.toml failed check: {chk_err}");

    // Modify gap.toml and verify `gap init` without `--force` refuses to overwrite it.
    let custom_marker = "# user-custom-sentinel\n";
    fs::write(&generated_toml, custom_marker).unwrap();
    let (no_force_code, _, no_force_err) = run_gap(&["init", project_dir.to_str().unwrap()]).await;
    assert_eq!(
        no_force_code, 2,
        "init without --force must fail when files exist: {no_force_err}"
    );
    assert_eq!(
        fs::read_to_string(&generated_toml).unwrap(),
        custom_marker,
        "init without --force must not overwrite existing file"
    );

    // With `--force`, overwrites cleanly and passes `config check` again.
    let (force_code, _, force_err) =
        run_gap(&["init", project_dir.to_str().unwrap(), "--force"]).await;
    assert_eq!(force_code, 0, "init --force failed: {force_err}");
    assert_ne!(fs::read_to_string(&generated_toml).unwrap(), custom_marker);
    let (rechk_code, _, _) = run_gap(&["config", "check", generated_toml.to_str().unwrap()]).await;
    assert_eq!(rechk_code, 0);
}

// spec: OPS-044, SEC-070
#[cfg(unix)]
#[tokio::test]
async fn init_refuses_symlinks_escaping_target_dir_even_with_force() {
    use std::os::unix::fs::symlink;

    let outside = TempDir::new().unwrap();
    let victim = outside.path().join("victim.txt");
    fs::write(&victim, "DO_NOT_OVERWRITE").unwrap();

    // Case 1: target DIR itself is a symlink pointing outside.
    let ws1 = TempDir::new().unwrap();
    let sym_dir = ws1.path().join("sym-target");
    symlink(outside.path(), &sym_dir).unwrap();
    let (c1, _, e1) = run_gap(&["init", sym_dir.to_str().unwrap(), "--force"]).await;
    assert_ne!(c1, 0, "init must refuse symlinked target DIR");
    assert!(e1.contains("symlink"), "stderr={e1}");
    assert!(
        !outside.path().join("gap.toml").exists(),
        "init must never write into symlinked outside dir"
    );

    // Case 2: gap.toml inside target DIR is a symlink to outside victim.txt.
    let ws2 = TempDir::new().unwrap();
    symlink(&victim, ws2.path().join("gap.toml")).unwrap();
    let (c2, _, e2) = run_gap(&["init", ws2.path().to_str().unwrap(), "--force"]).await;
    assert_ne!(
        c2, 0,
        "init --force must refuse symlinked file escaping target DIR"
    );
    assert!(e2.contains("symlink"), "stderr={e2}");
    assert_eq!(
        fs::read_to_string(&victim).unwrap(),
        "DO_NOT_OVERWRITE",
        "victim outside DIR must remain untouched"
    );

    // Case 3: `fixtures` subdirectory inside target DIR is a symlink to outside.
    let ws3 = TempDir::new().unwrap();
    symlink(outside.path(), ws3.path().join("fixtures")).unwrap();
    let (c3, _, e3) = run_gap(&["init", ws3.path().to_str().unwrap(), "--force"]).await;
    assert_ne!(
        c3, 0,
        "init --force must refuse symlinked subdirectory escaping target DIR"
    );
    assert!(e3.contains("symlink"), "stderr={e3}");
    assert!(
        !outside.path().join("rest_fixture.py").exists(),
        "init must never write into symlinked fixtures dir"
    );
}

// spec: OPS-044, OPS-001, SEC-072
#[tokio::test]
async fn doctor_passes_walking_skeleton_warns_on_t2_and_marks_m1_unavailable_checks_skipped() {
    let skeleton = repo_root().join("examples/walking-skeleton/gap.toml");
    let (code, stdout, stderr) = run_gap(&["doctor", "--config", skeleton.to_str().unwrap()]).await;
    assert_eq!(
        code, 0,
        "doctor failed on walking-skeleton: stdout={stdout}\nstderr={stderr}"
    );
    assert!(stdout.contains("[ok]"));
    assert!(!stdout.contains("[fail]"));
    assert!(stdout.contains("skipped (not available in M1)"));

    // T2 branch produces `[warn]` with fix hint and still exits 0.
    let tmp = TempDir::new().unwrap();
    let t2_toml = tmp.path().join("gap.toml");
    fs::write(
        &t2_toml,
        r#"
profile = "dev"

[[branches]]
id = "quarantined_mcp"
protocol = "mcp"
trust = "T2"
[branches.mcp]
transport = "http"
url = "http://127.0.0.1:9102/mcp"
"#,
    )
    .unwrap();
    let (w_code, w_out, w_err) = run_gap(&["doctor", "--config", t2_toml.to_str().unwrap()]).await;
    assert_eq!(w_code, 0, "warn-only doctor run must exit 0: {w_err}");
    assert!(
        w_out.contains("[warn]") && w_out.contains("quarantined_mcp") && w_out.contains("fix:"),
        "expected T2 warning naming branch + fix hint, got:\n{w_out}"
    );
}

// spec: OPS-044, OPS-001
#[tokio::test]
async fn doctor_fails_when_config_cannot_parse() {
    let tmp = TempDir::new().unwrap();
    let bad_toml = tmp.path().join("gap.toml");
    fs::write(&bad_toml, "profile = [not_a_string]\n").unwrap();

    let (code, stdout, stderr) = run_gap(&["doctor", "--config", bad_toml.to_str().unwrap()]).await;
    assert_ne!(code, 0);
    let combined = format!("{stdout}{stderr}");
    assert!(combined.contains("[fail]") && combined.contains("fix:"));
}

// spec: OPS-044, OPS-001
#[tokio::test]
async fn doctor_fails_when_referenced_openapi_file_missing() {
    let tmp = TempDir::new().unwrap();
    let gap_toml = tmp.path().join("gap.toml");
    fs::write(
        &gap_toml,
        r#"
profile = "dev"

[[branches]]
id = "shop"
protocol = "openapi"
[branches.openapi]
spec = "does_not_exist.openapi.json"
base_url = "http://127.0.0.1:9101"
"#,
    )
    .unwrap();

    let (code, stdout, _stderr) =
        run_gap(&["doctor", "--config", gap_toml.to_str().unwrap()]).await;
    assert_ne!(code, 0);
    assert!(
        stdout.contains("[fail]")
            && stdout.contains("does_not_exist.openapi.json")
            && stdout.contains("fix:"),
        "expected [fail] naming missing spec + fix, got:\n{stdout}"
    );
}

// spec: OPS-044, OPS-002
#[tokio::test]
async fn doctor_fails_when_server_listen_invalid_socket_addr() {
    let tmp = TempDir::new().unwrap();
    let gap_toml = tmp.path().join("gap.toml");
    fs::write(
        &gap_toml,
        r#"
profile = "dev"
[server]
listen = "not-a-socket-addr"
"#,
    )
    .unwrap();

    let (code, stdout, _stderr) =
        run_gap(&["doctor", "--config", gap_toml.to_str().unwrap()]).await;
    assert_ne!(code, 0);
    assert!(
        stdout.contains("[fail]") && stdout.contains("server.listen") && stdout.contains("fix:"),
        "expected [fail] on server.listen, got:\n{stdout}"
    );
}

// spec: OPS-044, OPS-001, OPS-002
#[tokio::test]
async fn doctor_fails_when_server_listen_is_wildcard_in_prod() {
    let tmp = TempDir::new().unwrap();
    let key_hash = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    for wildcard in ["0.0.0.0:8080", "[::]:8080"] {
        let gap_toml = tmp.path().join("gap.toml");
        fs::write(
            &gap_toml,
            format!(
                r#"
profile = "prod"

[server]
listen = "{wildcard}"

[[api_keys]]
id = "prod-key"
key_hash = "{key_hash}"
scopes = ["read"]
"#
            ),
        )
        .unwrap();

        let (code, stdout, _stderr) =
            run_gap(&["doctor", "--config", gap_toml.to_str().unwrap()]).await;
        assert_ne!(code, 0, "wildcard {wildcard} in prod must fail doctor");
        assert!(
            stdout.contains("[fail]")
                && stdout.contains("server.listen")
                && stdout.contains("fix:"),
            "expected [fail] on wildcard {wildcard}, got:\n{stdout}"
        );
    }
}

// spec: OPS-044, OPS-001, SEC-002
#[tokio::test]
async fn doctor_fails_when_prod_has_no_api_keys() {
    let tmp = TempDir::new().unwrap();
    let gap_toml = tmp.path().join("gap.toml");
    fs::write(
        &gap_toml,
        r#"
profile = "prod"

[server]
listen = "127.0.0.1:8080"
"#,
    )
    .unwrap();

    let (code, stdout, _stderr) =
        run_gap(&["doctor", "--config", gap_toml.to_str().unwrap()]).await;
    assert_ne!(code, 0, "prod config without api_keys must fail doctor");
    assert!(
        stdout.contains("[fail]") && stdout.contains("auth.api_keys") && stdout.contains("fix:"),
        "expected [fail] on auth.api_keys, got:\n{stdout}"
    );
}

// spec: OPS-044, SEC-062
#[tokio::test]
async fn doctor_fails_when_stdio_mcp_binary_missing_or_sha256_mismatches() {
    let tmp = TempDir::new().unwrap();
    let fake_bin = tmp.path().join("mcp-tool-bin");
    fs::write(&fake_bin, b"#!/bin/sh\necho ok\n").unwrap();

    // Wrong sha256 -> doctor must fail
    let wrong_sha = "0000000000000000000000000000000000000000000000000000000000000000";
    let bad_hash_toml = tmp.path().join("bad_hash.toml");
    fs::write(
        &bad_hash_toml,
        format!(
            r#"
profile = "dev"

[[branches]]
id = "local_stdio"
protocol = "mcp"
[branches.mcp]
transport = "stdio"
command = "{}"
sha256 = "{wrong_sha}"
"#,
            fake_bin.display()
        ),
    )
    .unwrap();

    let (code_mismatch, out_mismatch, _) =
        run_gap(&["doctor", "--config", bad_hash_toml.to_str().unwrap()]).await;
    assert_ne!(code_mismatch, 0);
    assert!(
        out_mismatch.contains("[fail]")
            && out_mismatch.contains("branches.mcp_stdio")
            && out_mismatch.contains("fix:"),
        "expected [fail] on sha256 mismatch, got:\n{out_mismatch}"
    );

    // Missing executable -> doctor must fail
    let missing_bin = tmp.path().join("does-not-exist-bin");
    let missing_toml = tmp.path().join("missing_bin.toml");
    fs::write(
        &missing_toml,
        format!(
            r#"
profile = "dev"

[[branches]]
id = "local_stdio"
protocol = "mcp"
[branches.mcp]
transport = "stdio"
command = "{}"
sha256 = "{wrong_sha}"
"#,
            missing_bin.display()
        ),
    )
    .unwrap();
    let (code_missing, out_missing, _) =
        run_gap(&["doctor", "--config", missing_toml.to_str().unwrap()]).await;
    assert_ne!(code_missing, 0);
    assert!(
        out_missing.contains("[fail]") && out_missing.contains("branches.mcp_stdio"),
        "expected [fail] on missing stdio binary, got:\n{out_missing}"
    );
}

// spec: OPS-044
#[tokio::test]
async fn later_wp_commands_print_single_line_with_wp_id_and_exit_2() {
    let cases: &[(&[&str], &str)] = &[
        (&["models", "pull"], "WP-M2-04"),
        (&["models", "verify"], "WP-M2-04"),
        (&["ingest", "run"], "WP-M4-01"),
        (&["ingest", "once"], "WP-M4-01"),
        (&["proposals", "list"], "WP-M4-07"),
        (&["proposals", "show", "prop-1"], "WP-M4-07"),
        (&["proposals", "approve", "prop-1"], "WP-M4-07"),
        (&["proposals", "deny", "prop-1"], "WP-M4-07"),
        (&["snapshot", "show"], "WP-M4-09"),
        (&["snapshot", "diff"], "WP-M4-09"),
        (&["snapshot", "rollback"], "WP-M4-09"),
        (&["calibrate"], "WP-M5-04"),
        (&["tasks", "list"], "WP-M6-02"),
        (&["tasks", "show", "task-1"], "WP-M6-02"),
        (&["tasks", "cancel", "task-1"], "WP-M6-02"),
        (&["policy", "check"], "WP-M6-05"),
        (&["policy", "reload"], "WP-M6-05"),
        (&["audit", "verify"], "WP-M6-11"),
        (&["audit", "export"], "WP-M6-11"),
        (&["purge"], "WP-M7-01"),
        (&["bench"], "WP-M7-05"),
    ];

    for (argv, expected_wp) in cases {
        let (code, stdout, stderr) = run_gap(argv).await;
        assert_eq!(code, 2, "argv={argv:?} must exit 2");
        let combined = format!("{stdout}{stderr}");
        let lines: Vec<&str> = combined.lines().collect();
        assert_eq!(
            lines.len(),
            1,
            "argv={argv:?} must emit exactly one line, got: {combined:?}"
        );
        assert_eq!(
            lines[0],
            format!("not available in this build ({expected_wp})"),
            "argv={argv:?}"
        );
    }
}
