//! Real loopback TCP smoke test (`#[ignore = "needs loopback sockets"]` for sandbox runs; executed
//! by the reviewer per `Brief §4` and `§6`).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use common::TestHarness;
use gap_transport::{Profile, TransportConfig};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

// spec: GQL-003, OPS-002
#[tokio::test]
#[ignore = "needs loopback sockets"]
async fn loopback_axum_serve_raw_http_smoke() {
    let harness = TestHarness::new(TransportConfig::for_profile(Profile::Dev));
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind loopback");
    let addr = listener.local_addr().expect("local_addr");
    let app = harness.router.clone();

    let server_task = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let body_bytes =
        serde_json::to_vec(&json!({ "query": "{ order(id: \"ord-tcp\") { id sku } }" }))
            .expect("json");
    let raw_http = format!(
        "POST /graphql HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\nAccept: application/graphql-response+json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body_bytes.len()
    );

    let mut stream = TcpStream::connect(addr).await.expect("connect loopback");
    stream
        .write_all(raw_http.as_bytes())
        .await
        .expect("write headers");
    stream.write_all(&body_bytes).await.expect("write body");

    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await.expect("read response");
    server_task.abort();

    let raw_resp = String::from_utf8(buf).expect("valid utf8 HTTP response");
    let (head, body_part) = raw_resp
        .split_once("\r\n\r\n")
        .expect("HTTP header/body separator");
    assert!(
        head.starts_with("HTTP/1.1 200 OK"),
        "unexpected status line: {head}"
    );
    assert!(
        head.to_ascii_lowercase()
            .contains("content-type: application/graphql-response+json"),
        "unexpected headers: {head}"
    );

    let parsed: Value = serde_json::from_str(body_part).expect("parse JSON body");
    assert_eq!(parsed["data"]["order"]["id"], "ord-tcp");
    assert_eq!(parsed["extensions"]["gap"]["generation"], 1);
}
