//! A real child process behind the launch policy (needs a Python 3 interpreter; the reviewer runs these with `--ignored`).

use std::path::PathBuf;
use std::sync::Arc;

use gap_mcp::outbound::launch::{AllowedBinary, LaunchError, LaunchPolicy, spawn_stdio};
use gap_mcp::outbound::{McpConfig, McpConnector};
use gap_ngc_sdk::connector::Connector;
use gap_proto::ngc::v1::DescribeRequest;
use serde_json::json;
use sha2::{Digest, Sha256};

use super::{collect, query, scope};

fn python() -> PathBuf {
    PathBuf::from(
        std::env::var("GAP_MCP_TEST_PYTHON").unwrap_or_else(|_| "/usr/bin/python3".to_owned()),
    )
}

fn stub() -> String {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/mcp/stub_server.py")
        .canonicalize()
        .expect("stub path")
        .to_string_lossy()
        .into_owned()
}

fn entry(sha256: String) -> AllowedBinary {
    AllowedBinary {
        path: python(),
        sha256,
        args: vec![stub()],
        env: vec![("PYTHONUNBUFFERED".to_owned(), "1".to_owned())],
    }
}

fn sha_of(path: &PathBuf) -> String {
    Sha256::digest(std::fs::read(path).expect("read interpreter"))
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

// spec: CMP-010, ADR-013
#[tokio::test]
#[ignore = "needs child processes"]
async fn outbound_real_stdio_child_runs_only_through_a_verified_allow_list_entry() {
    let good = entry(sha_of(&python()));
    let policy = LaunchPolicy::DevAllowList(vec![good.clone()]);
    let (transport, mut child) = spawn_stdio(&policy, &python(), 1 << 20).expect("spawn");
    let connector =
        McpConnector::new(Arc::new(transport), McpConfig::default()).expect("connector");
    let described = connector
        .describe(DescribeRequest::default(), scope(30))
        .await
        .expect("describe")
        .description
        .expect("description");
    assert_eq!(described.operations[0].name, "echo");
    let got = collect(
        connector
            .query(query("echo", json!({"id": 5})), scope(30))
            .await
            .expect("query"),
    )
    .await
    .expect("rows");
    assert_eq!(got.rows[&0][0]["echo"]["id"].as_f64(), Some(5.0));
    // the child receives only the variables of the allow-list entry (the environment is cleared)
    let env = collect(
        connector
            .query(query("env", json!({})), scope(30))
            .await
            .expect("query"),
    )
    .await
    .expect("rows");
    let names: Vec<&str> = env.rows[&0][0]["names"]
        .as_array()
        .expect("names")
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    assert!(names.contains(&"PYTHONUNBUFFERED"));
    assert!(
        !names.contains(&"PATH") && !names.contains(&"HOME"),
        "the child must not inherit the parent's environment: {names:?}"
    );
    let _ = child.kill().await;

    // a wrong pin is refused before anything runs
    let wrong = LaunchPolicy::DevAllowList(vec![entry("00".repeat(32))]);
    assert!(matches!(
        spawn_stdio(&wrong, &python(), 1 << 20),
        Err(LaunchError::HashMismatch)
    ));
    assert!(matches!(
        spawn_stdio(&LaunchPolicy::Refuse, &python(), 1 << 20),
        Err(LaunchError::Refused)
    ));
}
