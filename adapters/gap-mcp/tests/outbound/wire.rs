//! Wire fixtures taken from the pinned specs (`third_party/specs/mcp-2026-07-28`, `mcp-2025-11-25`) run through the real decoder, session and mapper.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use gap_core::error::NgcErrorCode;
use gap_mcp::outbound::jsonrpc::{Incoming, decode_frame};
use gap_mcp::outbound::transport::{CallOptions, HeaderHints, McpTransport, Reply};
use gap_mcp::outbound::{McpConfig, McpConnector, McpError};
use gap_ngc_sdk::connector::Connector;
use gap_proto::ngc::v1::DescribeRequest;
use serde_json::{Value, json};

use super::{collect, query, scope};

fn fixture(name: &str) -> Value {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/mcp")
        .join(name);
    serde_json::from_slice(
        &std::fs::read(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display())),
    )
    .expect("fixture JSON")
}

fn result_of(name: &str) -> Value {
    let frame = fixture(name);
    match decode_frame(frame.to_string().as_bytes(), 1 << 20).expect("decode") {
        Incoming::Response {
            outcome: Ok(result),
            ..
        } => result,
        other => panic!("{name}: not a successful response: {other:?}"),
    }
}

/// A transport that answers each method from a script of spec fixtures.
struct Scripted {
    answers: Mutex<Vec<(String, Result<Value, McpError>)>>,
    seen: Mutex<Vec<(String, Value)>>,
}

impl Scripted {
    fn new(answers: Vec<(&str, Result<Value, McpError>)>) -> Self {
        Self {
            answers: Mutex::new(
                answers
                    .into_iter()
                    .map(|(m, r)| (m.to_owned(), r))
                    .collect(),
            ),
            seen: Mutex::new(Vec::new()),
        }
    }
}

impl McpTransport for Scripted {
    async fn request(
        &self,
        method: &str,
        params: Value,
        _hints: HeaderHints,
        _opts: CallOptions,
    ) -> Result<Reply, McpError> {
        self.seen
            .lock()
            .expect("seen")
            .push((method.to_owned(), params));
        let mut answers = self.answers.lock().expect("answers");
        let position = answers
            .iter()
            .position(|(m, _)| m == method)
            .unwrap_or_else(|| panic!("unexpected method {method}"));
        let (_, result) = answers.remove(position);
        result.map(|value| Reply {
            value,
            session_id: Some("S".to_owned()),
        })
    }

    async fn notify(
        &self,
        _method: &str,
        _params: Value,
        _hints: HeaderHints,
    ) -> Result<(), McpError> {
        Ok(())
    }
}

// spec: CMP-010, CMP-011, CMP-012
#[tokio::test(start_paused = true)]
async fn outbound_wire_fixtures_of_the_modern_spec_decode_and_map() {
    let empty_page =
        json!({"resultType": "complete", "tools": [], "ttlMs": 0, "cacheScope": "public"});
    let transport = Arc::new(Scripted::new(vec![
        (
            "server/discover",
            Ok(result_of("spec-2026-07-28-discover.json")),
        ),
        (
            "tools/list",
            Ok(result_of("spec-2026-07-28-tools-list-page1.json")),
        ),
        ("tools/list", Ok(empty_page)),
        (
            "resources/list",
            Ok(
                json!({"resultType": "complete", "resources": [], "ttlMs": 0, "cacheScope": "private"}),
            ),
        ),
        (
            "tools/call",
            Ok(result_of("spec-2026-07-28-tools-call-text.json")),
        ),
    ]));
    let connector =
        McpConnector::new(Arc::clone(&transport), McpConfig::default()).expect("connector");
    let description = connector
        .describe(DescribeRequest::default(), scope(30))
        .await
        .expect("describe")
        .description
        .expect("description");
    assert_eq!(description.operations.len(), 1);
    assert_eq!(description.operations[0].name, "get_weather");
    assert!(
        description
            .schema_sdl
            .contains("get_weather(location: String!): JSON"),
        "{}",
        description.schema_sdl
    );
    let discovery = connector.discovery().expect("cached discovery");
    assert_eq!(
        (discovery.ttl_ms, discovery.cache_scope.as_deref()),
        (Some(300_000), Some("public"))
    );
    // the second page request carried the cursor of the first page
    let seen = transport.seen.lock().expect("seen").clone();
    let second = seen
        .iter()
        .filter(|(m, _)| m == "tools/list")
        .nth(1)
        .expect("second page");
    assert_eq!(second.1["cursor"], json!("next-page-cursor"));
    // the text result becomes one `text` row (the text is not JSON)
    let got = collect(
        connector
            .query(
                query("get_weather", json!({"location": "New York"})),
                scope(30),
            )
            .await
            .expect("query"),
    )
    .await
    .expect("rows");
    assert_eq!(got.rows[&0].len(), 1);
    assert!(
        got.rows[&0][0]["text"]
            .as_str()
            .expect("text")
            .contains("Partly cloudy")
    );
}

// spec: CMP-013, CMP-012
#[tokio::test(start_paused = true)]
async fn outbound_input_required_example_of_the_spec_is_refused_not_dropped() {
    let transport = Arc::new(Scripted::new(vec![
        (
            "server/discover",
            Ok(result_of("spec-2026-07-28-discover.json")),
        ),
        (
            "tools/list",
            Ok(
                json!({"resultType": "complete", "tools": [{"name": "login", "inputSchema": {"type": "object"}}], "ttlMs": 0, "cacheScope": "private"}),
            ),
        ),
        (
            "resources/list",
            Ok(
                json!({"resultType": "complete", "resources": [], "ttlMs": 0, "cacheScope": "private"}),
            ),
        ),
        (
            "tools/call",
            Ok(result_of("spec-2026-07-28-tools-call-input-required.json")),
        ),
    ]));
    let connector = McpConnector::new(transport, McpConfig::default()).expect("connector");
    let stream = connector
        .query(query("login", json!({})), scope(30))
        .await
        .expect("query");
    let err = collect(stream)
        .await
        .expect_err("input_required must not be silently dropped");
    assert_eq!(err.code, NgcErrorCode::UpstreamError);
    assert!(err.message().contains("input_required"));
    assert!(
        !err.message().contains("eyJsb2NhdGlvbiI6Ik5ldyBZb3JrIn0"),
        "requestState is opaque and never echoed"
    );
}

// spec: CMP-010
#[tokio::test(start_paused = true)]
async fn outbound_wire_fixture_of_the_legacy_spec_initialize_selects_the_legacy_era() {
    let transport = Arc::new(Scripted::new(vec![
        (
            "server/discover",
            Err(McpError::Rpc {
                code: -32601,
                message: "Method not found".to_owned(),
            }),
        ),
        (
            "initialize",
            Ok(result_of("spec-2025-11-25-initialize.json")),
        ),
        // legacy results carry no resultType and no caching hints
        (
            "tools/list",
            Ok(json!({"tools": [{"name": "legacy_tool", "inputSchema": {"type": "object"}}]})),
        ),
        ("resources/list", Ok(json!({"resources": []}))),
    ]));
    let connector =
        McpConnector::new(Arc::clone(&transport), McpConfig::default()).expect("connector");
    let description = connector
        .describe(DescribeRequest::default(), scope(30))
        .await
        .expect("describe")
        .description
        .expect("description");
    assert_eq!(description.operations[0].name, "legacy_tool");
    assert_eq!(
        connector.session().era(),
        Some(gap_mcp::outbound::Era::Legacy)
    );
    assert_eq!(connector.session().session_id().as_deref(), Some("S"));
    let seen = transport.seen.lock().expect("seen").clone();
    assert_eq!(seen[1].1["protocolVersion"], json!("2025-11-25"));
    assert!(
        seen[1].1["capabilities"].get("sampling").is_none()
            && seen[1].1["capabilities"].get("roots").is_none()
    );
}

// spec: CMP-010
#[tokio::test(start_paused = true)]
async fn outbound_modern_envelope_table_missing_unknown_and_absent_caching_hints() {
    // absent ttlMs and cacheScope are tolerated (the caching page says to assume 0 and private), a missing or unknown resultType is not
    let mk = |tools: Value| {
        Arc::new(Scripted::new(vec![
            (
                "server/discover",
                Ok(result_of("spec-2026-07-28-discover.json")),
            ),
            ("tools/list", Ok(tools)),
            (
                "resources/list",
                Ok(json!({"resultType": "complete", "resources": []})),
            ),
        ]))
    };
    let ok = McpConnector::new(mk(json!({"resultType": "complete", "tools": [{"name": "a", "inputSchema": {"type": "object"}}]})), McpConfig::default()).expect("c");
    ok.describe(DescribeRequest::default(), scope(30))
        .await
        .expect("absent caching hints are fine");
    let d = ok.discovery().expect("discovery");
    assert_eq!(
        (d.ttl_ms, d.cache_scope.as_deref()),
        (Some(0), Some("private"))
    );
    for bad in [
        json!({"tools": []}),
        json!({"resultType": "weird", "tools": []}),
        json!({"resultType": "complete", "tools": "nope"}),
    ] {
        let c = McpConnector::new(mk(bad.clone()), McpConfig::default()).expect("c");
        assert!(
            c.describe(DescribeRequest::default(), scope(30))
                .await
                .is_err(),
            "{bad}"
        );
    }
}

// spec: CMP-010
#[tokio::test(start_paused = true)]
async fn outbound_a_discover_result_without_result_type_does_not_select_the_modern_era() {
    let mut discover = result_of("spec-2026-07-28-discover.json");
    discover
        .as_object_mut()
        .expect("object")
        .remove("resultType");
    let transport = Arc::new(Scripted::new(vec![
        ("server/discover", Ok(discover)),
        (
            "tools/list",
            Ok(json!({"resultType": "complete", "tools": [], "ttlMs": 0, "cacheScope": "private"})),
        ),
        (
            "resources/list",
            Ok(
                json!({"resultType": "complete", "resources": [], "ttlMs": 0, "cacheScope": "private"}),
            ),
        ),
    ]));
    let connector = McpConnector::new(transport, McpConfig::default()).expect("connector");
    let err = connector
        .describe(DescribeRequest::default(), scope(30))
        .await
        .expect_err("must not be accepted");
    assert_eq!(err.code, NgcErrorCode::UpstreamError);
    assert!(connector.session().era().is_none());
}
