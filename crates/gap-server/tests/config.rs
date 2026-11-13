//! Config loading, relative OpenAPI verification, `GAP_*` override, and canary secret tests (`Brief §4`).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::path::{Path, PathBuf};

use gap_core::config::{Profile, SecretRef};
use gap_core::ids::BranchId;
use gap_server::assemble::{ServerConfigError, ServerFile, derive_branch_uid};
use tempfile::TempDir;

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("canonical repo root")
}

// spec: OPS-001, INV-15
#[test]
fn walking_skeleton_gap_toml_loads_and_matches_documented_api_key_hash() {
    let skeleton_toml = repo_root().join("examples/walking-skeleton/gap.toml");
    let sf = ServerFile::load(&skeleton_toml).expect("walking-skeleton gap.toml must load");

    assert_eq!(sf.config().profile, Profile::Dev);
    assert_eq!(sf.config().branches.len(), 2);
    assert_eq!(sf.config().branches[0].id.as_str(), "shop");
    assert_eq!(sf.config().branches[1].id.as_str(), "notes");
    assert_eq!(sf.config().api_keys.len(), 1);

    let expected_hash = blake3::hash(b"gap_dev_skeleton_key_01")
        .to_hex()
        .to_string();
    assert_eq!(sf.config().api_keys[0].key_hash, expected_hash);

    let shop_id = BranchId::new("shop").unwrap();
    let doc_bytes = sf
        .openapi_document(&shop_id)
        .expect("preloaded shop.openapi.json bytes");
    assert!(doc_bytes.len() > 100);
}

// spec: OPS-001
#[test]
fn config_missing_or_unreadable_openapi_file_errors_naming_branch_spec_path() {
    let tmp = TempDir::new().unwrap();
    let present_spec = tmp.path().join("ok.openapi.json");
    fs::write(
        &present_spec,
        r#"{"openapi":"3.0.3","info":{"title":"ok","version":"1"},"paths":{}}"#,
    )
    .unwrap();

    // Missing file on `branches[1].openapi.spec`
    let gap_toml = tmp.path().join("gap.toml");
    fs::write(
        &gap_toml,
        r#"
profile = "dev"

[[branches]]
id = "alpha"
protocol = "openapi"
[branches.openapi]
spec = "ok.openapi.json"
base_url = "http://127.0.0.1:9101"

[[branches]]
id = "bravo"
protocol = "openapi"
[branches.openapi]
spec = "missing.openapi.json"
base_url = "http://127.0.0.1:9102"
"#,
    )
    .unwrap();

    let err = ServerFile::load(&gap_toml).expect_err("missing OpenAPI file must fail at load");
    let rendered = err.to_string();
    assert!(
        rendered.contains("branches[1].openapi.spec"),
        "error must name `branches[1].openapi.spec`, got: {rendered}"
    );
    match err {
        ServerConfigError::BranchFile { field_path, .. } => {
            assert_eq!(field_path, "branches[1].openapi.spec");
        }
        other => panic!("expected ServerConfigError::BranchFile, got: {other:?}"),
    }

    // Unreadable path (a directory instead of a readable file) on `branches[0].openapi.spec`
    let dir_spec = tmp.path().join("dir_not_a_file.json");
    fs::create_dir(&dir_spec).unwrap();
    let gap_toml_dir = tmp.path().join("gap_dir.toml");
    fs::write(
        &gap_toml_dir,
        r#"
profile = "dev"

[[branches]]
id = "alpha"
protocol = "openapi"
[branches.openapi]
spec = "dir_not_a_file.json"
base_url = "http://127.0.0.1:9101"
"#,
    )
    .unwrap();
    let dir_err =
        ServerFile::load(&gap_toml_dir).expect_err("directory as OpenAPI spec must fail at load");
    assert!(
        dir_err.to_string().contains("branches[0].openapi.spec"),
        "error must name `branches[0].openapi.spec`, got: {dir_err}"
    );
}

// spec: OPS-001
#[test]
fn config_env_override_of_core_key_still_works() {
    let skeleton_toml = repo_root().join("examples/walking-skeleton/gap.toml");
    let sf = ServerFile::load_with_env(
        &skeleton_toml,
        [
            ("GAP_SERVER__LISTEN", "127.0.0.1:18080"),
            ("GAP_LIMITS__MAX_COST_UNITS", "25000"),
            ("GAP_GATEWAY__FACADE_SPOTLIGHT", "false"),
        ],
    )
    .expect("load_with_env must apply GAP_* overrides");

    assert_eq!(sf.config().server.listen, "127.0.0.1:18080");
    assert_eq!(sf.config().limits.max_cost_units, 25_000);
    assert!(!sf.config().gateway.facade_spotlight);
}

// spec: INV-15
#[test]
fn inv_15_no_canary_secret_appears_in_any_startup_or_secret_error() {
    let canary_inline = "sk-canary-inline-77889900-never-echo";
    let canary_env = "sk-canary-env-11223344-never-echo";
    let canary_file = "sk-canary-file-55667788-never-echo";

    let tmp = TempDir::new().unwrap();
    let inline_toml = tmp.path().join("inline.toml");
    fs::write(
        &inline_toml,
        format!(
            "profile = \"dev\"\n[[branches]]\nid = \"shop\"\nprotocol = \"mcp\"\n[branches.mcp]\ntransport = \"http\"\nurl = \"http://127.0.0.1:9102/mcp\"\n[branches.mcp.auth]\nkind = \"bearer\"\nsecret = \"{canary_inline}\"\n"
        ),
    )
    .unwrap();
    let inline_err = ServerFile::load(&inline_toml).expect_err("inline secret must fail");
    assert!(!inline_err.to_string().contains(canary_inline));
    assert!(!format!("{inline_err:?}").contains(canary_inline));

    // Valid config with env:// and file:/// secret refs; verify resolved SecretString redacts Debug/Display
    // and missing/vault errors never leak canary values.
    let secret_file = tmp.path().join("upstream.token");
    fs::write(&secret_file, format!("{canary_file}\n")).unwrap();

    let valid_toml = tmp.path().join("valid.toml");
    fs::write(&valid_toml, "profile = \"dev\"\n").unwrap();
    let sf = ServerFile::load_with_env(&valid_toml, [("UPSTREAM_TOKEN", canary_env)]).unwrap();

    let env_ref: SecretRef = "env://UPSTREAM_TOKEN".parse().unwrap();
    let resolved_env = sf
        .resolve_secret(&env_ref, "branches[0].mcp.auth.secret")
        .expect("resolve env:// secret");
    assert_eq!(resolved_env.expose_secret(), canary_env);
    assert!(!format!("{resolved_env:?}").contains(canary_env));
    assert!(!format!("{resolved_env}").contains(canary_env));

    let file_ref: SecretRef = format!("file://{}", secret_file.display()).parse().unwrap();
    let resolved_file = sf
        .resolve_secret(&file_ref, "branches[0].openapi.auth.secret")
        .expect("resolve file:/// secret");
    assert_eq!(resolved_file.expose_secret(), canary_file);
    assert!(!format!("{resolved_file:?}").contains(canary_file));
    assert!(!format!("{resolved_file}").contains(canary_file));

    let missing_env: SecretRef = "env://MISSING_UPSTREAM_TOKEN".parse().unwrap();
    let missing_err = sf
        .resolve_secret(&missing_env, "branches[0].mcp.auth.secret")
        .expect_err("missing env var must fail");
    assert!(!missing_err.to_string().contains(canary_env));
    assert!(
        missing_err
            .to_string()
            .contains("env://MISSING_UPSTREAM_TOKEN")
    );

    let vault_ref: SecretRef = "secret://vault/prod-token".parse().unwrap();
    let vault_err = sf
        .resolve_secret(&vault_ref, "branches[0].mcp.auth.secret")
        .expect_err("vault provider must fail closed in M1");
    assert!(!vault_err.to_string().contains(canary_env));
    assert!(!vault_err.to_string().contains(canary_file));
}

// spec: OPS-001
#[test]
fn derive_branch_uid_is_deterministic_uuid_v7_and_distinct_per_slug() {
    let shop = BranchId::new("shop").unwrap();
    let notes = BranchId::new("notes").unwrap();

    let u1 = derive_branch_uid(&shop);
    let u2 = derive_branch_uid(&shop);
    let u_notes = derive_branch_uid(&notes);

    assert_eq!(
        u1, u2,
        "same branch id must always derive the same BranchUid"
    );
    assert_ne!(
        u1, u_notes,
        "distinct branch ids must derive distinct BranchUids"
    );
    assert_eq!(u1.as_uuid().get_version_num(), 7);
}
