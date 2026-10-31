//! The reqwest-backed client against a real loopback HTTP server (the reviewer runs these with `--ignored`).

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use gap_mcp::outbound::McpError;
use gap_mcp::outbound::transport::http::{HttpPost, HttpRequest, ReqwestPost};

// spec: CMP-010, SEC-075
#[tokio::test]
#[ignore = "needs loopback sockets"]
async fn outbound_reqwest_client_does_not_follow_redirects_and_reports_the_status() {
    let followed = Arc::new(AtomicUsize::new(0));
    let hits = Arc::clone(&followed);
    let app = axum::Router::new()
        .route(
            "/mcp",
            axum::routing::post(|| async {
                (
                    axum::http::StatusCode::FOUND,
                    [("location", "/elsewhere")],
                    "",
                )
            }),
        )
        .route(
            "/elsewhere",
            axum::routing::any(move || {
                let hits = Arc::clone(&hits);
                async move {
                    hits.fetch_add(1, Ordering::SeqCst);
                    "followed"
                }
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    let client = ReqwestPost::new(&format!("http://{addr}/mcp"), None, false).expect("client");
    let reply = client
        .post(HttpRequest {
            headers: vec![("content-type".to_owned(), "application/json".to_owned())],
            body: b"{}".to_vec(),
            timeout: Duration::from_secs(5),
        })
        .await
        .map_err(|e: McpError| e)
        .expect("reply");
    assert_eq!(
        reply.status, 302,
        "a redirect must be reported, never followed"
    );
    assert_eq!(followed.load(Ordering::SeqCst), 0);
    server.abort();
}
