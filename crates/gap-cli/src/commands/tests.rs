//! Unit and integration tests for `crates/gap-cli/src/commands` (`OPS-044`, `SEC-072`,
//! `SEC-062`, `SEC-040`, `SEC-102`, `SEC-020`, `SEC-092`, `SEC-093`, `ING-052`, `ING-066`,
//! `RUN-080`).

use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

use gap_core::clock::{Clock, SystemClock, Timestamp};
use gap_core::ids::{TaskId, TenantId};
use gap_models::sha256_hex;
use gap_sec::authz::{DEFAULT_CEDAR_POLICIES, DEFAULT_CEDAR_SCHEMA};
use gap_store::audit::{AuditChain, AuditEntry, AuditLog, SqliteAuditConfig, SqliteAuditLog};
use gap_store::tasks::sqlite::SqliteTaskStore;
use gap_store::tasks::{TaskEventRecord, TaskRecord, TaskStatus, TaskStore};
use gap_store::{
    BaseScope, Caller, ContextStore, NewNode, NodeType, Provenance, ScopeSpec, SqliteConfig,
    SqliteContextStore,
};
use serde_json::Value;
use tempfile::TempDir;

use super::{EXIT_CONFIG_ERROR, EXIT_NOT_FOUND, EXIT_OK, EXIT_SECURITY_DENIED, run_with_io};

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

async fn run_cmd(args: &[&str]) -> (i32, String, String) {
    let mut full = Vec::with_capacity(args.len() + 1);
    full.push("gap");
    full.extend_from_slice(args);
    let mut out = Vec::new();
    let mut err = Vec::new();
    let code = run_with_io(full, &mut out, &mut err).await;
    (
        code,
        String::from_utf8(out).unwrap(),
        String::from_utf8(err).unwrap(),
    )
}

// spec: OPS-044
#[tokio::test]
async fn help_and_completions_cover_all_ops_044_commands_and_shells() {
    let (code, stdout, stderr) = run_cmd(&["--help"]).await;
    assert_eq!(code, EXIT_OK, "stderr={stderr}");
    for cmd in [
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
        "completions",
    ] {
        assert!(stdout.contains(cmd), "missing `{cmd}` in help:\n{stdout}");
    }
    assert!(
        stdout.contains("Exit codes:"),
        "help must document exit codes:\n{stdout}"
    );

    for (shell, expected) in [
        ("bash", "complete -F _gap_completions gap"),
        ("zsh", "#compdef gap"),
        ("fish", "complete -c gap"),
        ("powershell", "Register-ArgumentCompleter"),
    ] {
        let (c_code, c_out, c_err) = run_cmd(&["completions", shell]).await;
        assert_eq!(c_code, EXIT_OK, "shell={shell} err={c_err}");
        assert!(c_out.contains(expected), "shell={shell} out={c_out}");

        let (j_code, j_out, _) = run_cmd(&["--json", "completions", shell]).await;
        assert_eq!(j_code, EXIT_OK);
        let val: Value = serde_json::from_str(&j_out).unwrap();
        assert_eq!(val["shell"], shell);
        assert!(val["script"].as_str().unwrap().contains(expected));
    }
}

// spec: OPS-044, SEC-072, SEC-062, SEC-040, SEC-102, SEC-020
#[tokio::test]
async fn doctor_full_reports_json_checks_including_exec_allowlist_vault_and_egress() {
    let skeleton = repo_root().join("examples/walking-skeleton/gap.toml");
    let (code, stdout, stderr) =
        run_cmd(&["--json", "doctor", "--config", skeleton.to_str().unwrap()]).await;
    assert_eq!(code, EXIT_OK, "stdout={stdout}\nstderr={stderr}");
    let report: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(report["fail_count"], 0);
    let checks = report["checks"].as_array().unwrap();
    assert!(
        checks
            .iter()
            .any(|c| c["id"] == "security.egress_guard" && c["status"] == "ok")
    );
    assert!(
        checks
            .iter()
            .any(|c| c["id"] == "security.policy" && c["status"] == "ok")
    );

    // Verify stdio allow-list + 0600 file vault secret posture (`SEC-062`, `SEC-040`)
    let tmp = TempDir::new().unwrap();
    let bin = tmp.path().join("stdio-mcp");
    let bin_bytes = b"#!/bin/sh\necho ok\n";
    fs::write(&bin, bin_bytes).unwrap();
    let bin_sha = sha256_hex(bin_bytes);

    let secret_file = tmp.path().join("upstream.key");
    fs::write(&secret_file, "dummy-token-material\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&secret_file, fs::Permissions::from_mode(0o644)).unwrap();
    }

    let toml_path = tmp.path().join("gap.toml");
    fs::write(
        &toml_path,
        format!(
            r#"
profile = "dev"

[[branches]]
id = "stdio_tool"
protocol = "mcp"
[branches.mcp]
transport = "stdio"
command = "{}"
sha256 = "{bin_sha}"

[security]
approval_signing_key_secret = "file://{}"
"#,
            bin.display(),
            secret_file.display()
        ),
    )
    .unwrap();

    // 0644 file secret must cause doctor fail (`SEC-040`)
    let (bad_code, bad_out, _) =
        run_cmd(&["--json", "doctor", "--config", toml_path.to_str().unwrap()]).await;
    #[cfg(unix)]
    {
        assert_eq!(bad_code, EXIT_CONFIG_ERROR);
        let bad_val: Value = serde_json::from_str(&bad_out).unwrap();
        assert!(bad_val["fail_count"].as_u64().unwrap() >= 1);
    }

    // Tighten permissions to 0600 -> allow-list + vault both pass
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&secret_file, fs::Permissions::from_mode(0o600)).unwrap();
    }
    let (ok_code, ok_out, ok_err) =
        run_cmd(&["--json", "doctor", "--config", toml_path.to_str().unwrap()]).await;
    assert_eq!(ok_code, EXIT_OK, "ok_out={ok_out}\nok_err={ok_err}");
    let ok_val: Value = serde_json::from_str(&ok_out).unwrap();
    assert_eq!(ok_val["fail_count"], 0);
}

// spec: OPS-044, SEC-102
#[tokio::test]
async fn models_verify_and_pull_enforce_sha256_integrity_and_fail_closed_offline() {
    let tmp = TempDir::new().unwrap();
    let cache = tmp.path().join("cache");
    fs::create_dir_all(&cache).unwrap();

    let payload = b"model-weights-v1";
    let digest = sha256_hex(payload);
    let manifest_path = tmp.path().join("manifest.toml");
    fs::write(
        &manifest_path,
        format!(
            r#"
[[model]]
id = "tiny-s1"
url = "https://example.invalid/tiny-s1.safetensors"
sha256 = "{digest}"
size = {}
license = "Apache-2.0"
source_commit = "0000000"
training_data_note = "synthetic test model"
"#,
            payload.len()
        ),
    )
    .unwrap();

    // Missing artifact fails offline pull/verify with EXIT_SECURITY_DENIED (3)
    let (miss_code, miss_out, _) = run_cmd(&[
        "--json",
        "models",
        "pull",
        "--manifest",
        manifest_path.to_str().unwrap(),
        "--cache",
        cache.to_str().unwrap(),
    ])
    .await;
    assert_eq!(miss_code, EXIT_SECURITY_DENIED);
    assert_eq!(
        serde_json::from_str::<Value>(&miss_out).unwrap()["ok"],
        false
    );

    // Populate matching cache artifact -> verify succeeds
    fs::write(cache.join("tiny-s1.safetensors"), payload).unwrap();
    let (ok_code, ok_out, _) = run_cmd(&[
        "--json",
        "models",
        "verify",
        "--manifest",
        manifest_path.to_str().unwrap(),
        "--cache",
        cache.to_str().unwrap(),
    ])
    .await;
    assert_eq!(ok_code, EXIT_OK);
    assert_eq!(serde_json::from_str::<Value>(&ok_out).unwrap()["ok"], true);

    // Unknown model ID -> EXIT_NOT_FOUND (4)
    let (nf_code, _, _) = run_cmd(&[
        "--json",
        "models",
        "verify",
        "--manifest",
        manifest_path.to_str().unwrap(),
        "--cache",
        cache.to_str().unwrap(),
        "--id",
        "missing-model",
    ])
    .await;
    assert_eq!(nf_code, EXIT_NOT_FOUND);
}

// spec: OPS-044, ING-052, RUN-007
#[tokio::test]
async fn snapshot_show_diff_and_rollback_work_end_to_end() {
    let tmp = TempDir::new().unwrap();
    let before_json = tmp.path().join("before.json");
    let after_json = tmp.path().join("after.json");
    fs::write(
        &before_json,
        r#"{"generation":1,"branches":[{"id":"shop","protocol":"REST","trust":"T1","operations":[{"name":"getItem","sdl":"getItem: String","side_effect":"NONE"}]}]}"#,
    )
    .unwrap();
    fs::write(
        &after_json,
        r#"{"generation":2,"branches":[{"id":"shop","protocol":"REST","trust":"T1","operations":[{"name":"getItem","sdl":"getItem: String","side_effect":"NONE"},{"name":"listItems","sdl":"listItems: [String]","side_effect":"NONE"}]}]}"#,
    )
    .unwrap();

    let (show_code, show_out, _) = run_cmd(&[
        "--json",
        "snapshot",
        "show",
        "--config",
        before_json.to_str().unwrap(),
    ])
    .await;
    assert_eq!(show_code, EXIT_OK);
    let show_val: Value = serde_json::from_str(&show_out).unwrap();
    assert_eq!(show_val["branch_count"], 1);
    assert_eq!(show_val["operation_count"], 1);

    let (diff_code, diff_out, _) = run_cmd(&[
        "--json",
        "snapshot",
        "diff",
        "--before",
        before_json.to_str().unwrap(),
        "--after",
        after_json.to_str().unwrap(),
    ])
    .await;
    assert_eq!(diff_code, EXIT_OK);
    let diff_val: Value = serde_json::from_str(&diff_out).unwrap();
    assert_eq!(diff_val["operations"].as_array().unwrap().len(), 1);

    let (rb_code, rb_out, rb_err) = run_cmd(&[
        "--json",
        "snapshot",
        "rollback",
        "--to",
        "1",
        "--before",
        "2",
        "--reason",
        "regression test",
    ])
    .await;
    assert_eq!(rb_code, EXIT_OK, "err={rb_err}");
    let rb_val: Value = serde_json::from_str(&rb_out).unwrap();
    assert_eq!(rb_val["active_generation"], 3);
}

// spec: OPS-044, SEC-020
#[tokio::test]
async fn policy_check_and_reload_validate_and_reject_invalid_cedar() {
    let tmp = TempDir::new().unwrap();
    let pdir = tmp.path().join("policies");
    fs::create_dir_all(&pdir).unwrap();
    fs::write(pdir.join("schema.cedarschema"), DEFAULT_CEDAR_SCHEMA).unwrap();
    fs::write(pdir.join("default.cedar"), DEFAULT_CEDAR_POLICIES).unwrap();

    let (chk_code, chk_out, chk_err) =
        run_cmd(&["--json", "policy", "check", "--dir", pdir.to_str().unwrap()]).await;
    assert_eq!(chk_code, EXIT_OK, "err={chk_err}");
    let chk_val: Value = serde_json::from_str(&chk_out).unwrap();
    assert_eq!(chk_val["ok"], true);

    let (rel_code, rel_out, _) = run_cmd(&[
        "--json",
        "policy",
        "reload",
        "--dir",
        pdir.to_str().unwrap(),
    ])
    .await;
    assert_eq!(rel_code, EXIT_OK);
    let rel_val: Value = serde_json::from_str(&rel_out).unwrap();
    assert_eq!(rel_val["action"], "reload");

    // Corrupt Cedar syntax -> EXIT_SECURITY_DENIED (3)
    fs::write(pdir.join("broken.cedar"), "permit(not_valid_syntax").unwrap();
    let (bad_code, _, _) =
        run_cmd(&["--json", "policy", "check", "--dir", pdir.to_str().unwrap()]).await;
    assert_eq!(bad_code, EXIT_SECURITY_DENIED);
}

// spec: OPS-044, SEC-092, SEC-093
#[tokio::test]
async fn audit_verify_and_export_prov_jsonld_succeed_on_sqlite_chain() {
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("audit.db");
    {
        let log = SqliteAuditLog::open(SqliteAuditConfig::file(&db_path)).unwrap();
        let entry = AuditEntry {
            ts: SystemClock.now_timestamp(),
            actor_chain: vec!["user:alice".to_owned(), "agent:planner".to_owned()],
            action: "snapshot.publish".to_owned(),
            resource: "snapshot:g2".to_owned(),
            decision: "ALLOW".to_owned(),
            details_json: r#"{"generation":2}"#.to_owned(),
        };
        log.append_chain(&AuditChain::System, &entry).unwrap();
    }

    let (v_code, v_out, v_err) = run_cmd(&[
        "--json",
        "audit",
        "verify",
        "--db",
        db_path.to_str().unwrap(),
        "--nightly",
    ])
    .await;
    assert_eq!(v_code, EXIT_OK, "err={v_err}");
    assert_eq!(serde_json::from_str::<Value>(&v_out).unwrap()["ok"], true);

    let (e_code, e_out, e_err) = run_cmd(&[
        "audit",
        "export",
        "--db",
        db_path.to_str().unwrap(),
        "--format",
        "prov-jsonld",
    ])
    .await;
    assert_eq!(e_code, EXIT_OK, "err={e_err}");
    let prov: Value = serde_json::from_str(&e_out).unwrap();
    assert!(prov.get("@context").is_some());
    assert!(!prov["@graph"].as_array().unwrap().is_empty());
}

// spec: OPS-044, RUN-080, ING-066, ING-010
#[tokio::test]
async fn ingest_tasks_purge_bench_and_serve_check_only_execute_cleanly() {
    let skeleton = repo_root().join("examples/walking-skeleton/gap.toml");
    let (srv_code, srv_out, srv_err) = run_cmd(&[
        "--json",
        "serve",
        "--config",
        skeleton.to_str().unwrap(),
        "--check-only",
    ])
    .await;
    assert_eq!(srv_code, EXIT_OK, "err={srv_err}");
    assert_eq!(serde_json::from_str::<Value>(&srv_out).unwrap()["ok"], true);

    let (ing_code, ing_out, ing_err) = run_cmd(&[
        "--json",
        "ingest",
        "once",
        "--config",
        skeleton.to_str().unwrap(),
    ])
    .await;
    assert_eq!(ing_code, EXIT_OK, "err={ing_err}");
    let ing_val: Value = serde_json::from_str(&ing_out).unwrap();
    assert_eq!(ing_val["extracted"].as_array().unwrap().len(), 1);

    let (b_code, b_out, _) = run_cmd(&["--json", "bench", "--iterations", "2"]).await;
    assert_eq!(b_code, EXIT_OK);
    assert_eq!(serde_json::from_str::<Value>(&b_out).unwrap()["ok"], true);

    // Durable tasks list/show/cancel
    let tmp = TempDir::new().unwrap();
    let tasks_db = tmp.path().join("tasks.db");
    let tid = TenantId::new("acme").unwrap();
    let task_id = TaskId::new_v7();
    let now = Timestamp::UNIX_EPOCH;
    {
        let store = SqliteTaskStore::open(&tasks_db).unwrap();
        let rec = TaskRecord {
            task_id,
            status: TaskStatus::Working,
            payload_json: r#"{"step":1}"#.to_owned(),
            lease_owner: Some("worker-1".to_owned()),
            lease_expires_at: None,
            updated_at: now,
        };
        let ev = TaskEventRecord {
            task_id,
            seq: 1,
            status: TaskStatus::Working,
            payload_json: "{}".to_owned(),
            emitted_at: now,
        };
        store.put_task(&tid.scope(), &rec, &ev).unwrap();
    }

    let (tl_code, tl_out, _) = run_cmd(&[
        "--json",
        "tasks",
        "list",
        "--db",
        tasks_db.to_str().unwrap(),
        "--tenant",
        "acme",
    ])
    .await;
    assert_eq!(tl_code, EXIT_OK);
    assert_eq!(
        serde_json::from_str::<Value>(&tl_out).unwrap()["tasks"]
            .as_array()
            .unwrap()
            .len(),
        1
    );

    let task_id_str = task_id.to_string();
    let (tc_code, tc_out, tc_err) = run_cmd(&[
        "--json",
        "tasks",
        "cancel",
        &task_id_str,
        "--db",
        tasks_db.to_str().unwrap(),
        "--tenant",
        "acme",
    ])
    .await;
    assert_eq!(tc_code, EXIT_OK, "err={tc_err}");
    assert_eq!(
        serde_json::from_str::<Value>(&tc_out).unwrap()["status"],
        "CANCELED"
    );

    // Purge expired ephemeral context node (`ING-066`)
    let ctx_db = tmp.path().join("state.db");
    {
        let clock = Arc::new(gap_core::clock::ManualClock::new(10_000));
        let store = SqliteContextStore::open(SqliteConfig::new(&ctx_db), clock).unwrap();
        let caller = Caller::new("alice");
        store
            .put_node(
                &tid.scope(),
                &caller,
                NewNode {
                    id: None,
                    node_type: NodeType::new("Note").unwrap(),
                    scope: ScopeSpec::Ephemeral {
                        base: BaseScope::Global,
                        expires_at: Some(Timestamp::from_unix_millis(20_000).unwrap()),
                    },
                    label: "old note".to_owned(),
                    summary: Some("expired".to_owned()),
                    props: serde_json::Map::new(),
                    provenance: Provenance::new("cli-test"),
                },
            )
            .unwrap();
    }

    let (p_code, p_out, p_err) =
        run_cmd(&["--json", "purge", "--db", ctx_db.to_str().unwrap()]).await;
    assert_eq!(p_code, EXIT_OK, "err={p_err}");
    let p_val: Value = serde_json::from_str(&p_out).unwrap();
    assert_eq!(p_val["purged_ephemeral_nodes"], 1);
}
