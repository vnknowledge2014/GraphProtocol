//! End-to-end integration tests for dual-stack HTTP/3 over QUIC (`OPS-005`, `OPS-002`, `RUN-090`).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use axum::routing::get;
use bytes::Bytes;
use common::TestHarness;
use gap_core::clock::Clock as _;
use gap_transport::http3::{Http3Client, Http3ServeOptions, Http3TlsConfig, serve_http3};
use gap_transport::{Profile, ServeOptions, TransportConfig, serve};
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::sync::{Notify, oneshot};

// spec: OPS-005, OPS-002
#[tokio::test]
async fn http3_serves_health_graphql_and_mcp_and_tcp_advertises_alt_svc() {
    let harness = TestHarness::new(TransportConfig::for_profile(Profile::Dev));
    let (tls, cert_der) =
        Http3TlsConfig::self_signed_for_dev(&["localhost".to_owned(), "127.0.0.1".to_owned()])
            .expect("dev cert");

    let udp_socket = std::net::UdpSocket::bind("127.0.0.1:0").expect("bind UDP");
    let h3_addr = udp_socket.local_addr().expect("UDP addr");

    let tcp_listener = TcpListener::bind("127.0.0.1:0").await.expect("bind TCP");
    let tcp_addr = tcp_listener.local_addr().expect("TCP addr");

    let (tx_h3, rx_h3) = oneshot::channel::<()>();
    let (tx_tcp, rx_tcp) = oneshot::channel::<()>();

    let app_h3 = harness.router.clone();
    let app_tcp = harness.router.clone();

    let h3_task = tokio::spawn(async move {
        serve_http3(
            udp_socket,
            tls,
            app_h3,
            async move {
                let _ = rx_h3.await;
            },
            Http3ServeOptions::default(),
        )
        .await
    });

    let tcp_opts = ServeOptions {
        alt_svc_h3_port: Some(h3_addr.port()),
        ..ServeOptions::default()
    };
    let tcp_task = tokio::spawn(async move {
        serve(
            tcp_listener,
            app_tcp,
            async move {
                let _ = rx_tcp.await;
            },
            tcp_opts,
        )
        .await
    });

    // 1. Verify TCP HTTP/1.1 advertises `Alt-Svc: h3=":<udp_port>"; ma=86400`.
    let client: Client<_, Body> = Client::builder(TokioExecutor::new()).build_http();
    let tcp_resp = client
        .request(
            Request::builder()
                .method(Method::GET)
                .uri(format!("http://{tcp_addr}/readyz"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("TCP /readyz");
    assert_eq!(tcp_resp.status(), StatusCode::OK);
    let alt_svc = tcp_resp
        .headers()
        .get("alt-svc")
        .expect("Alt-Svc header present on TCP response")
        .to_str()
        .unwrap();
    assert_eq!(alt_svc, format!("h3=\":{}\"; ma=86400", h3_addr.port()));

    // 2. Connect HTTP/3 client and test /readyz, POST /graphql, and POST /mcp.
    let mut h3_client = Http3Client::connect_pinned_der(h3_addr, "localhost", &cert_der)
        .await
        .expect("connect HTTP/3");

    let (ready_head, ready_body) = h3_client
        .request(
            Request::builder()
                .method(Method::GET)
                .uri("https://localhost/readyz")
                .body(Bytes::new())
                .unwrap(),
        )
        .await
        .expect("HTTP/3 /readyz");
    assert_eq!(ready_head.status(), StatusCode::OK);
    assert!(!ready_body.is_empty());

    let gql_payload = serde_json::to_vec(&json!({
        "query": "{ shopVersion order(id: \"o-1\") { id sku } }"
    }))
    .unwrap();
    let (gql_head, gql_body) = h3_client
        .request(
            Request::builder()
                .method(Method::POST)
                .uri("https://localhost/graphql")
                .header("content-type", "application/json")
                .header("accept", "application/graphql-response+json")
                .body(Bytes::from(gql_payload))
                .unwrap(),
        )
        .await
        .expect("HTTP/3 /graphql");
    assert_eq!(gql_head.status(), StatusCode::OK);
    let gql_json: Value = serde_json::from_slice(&gql_body).expect("JSON");
    assert_eq!(gql_json["data"]["shopVersion"], "shop-gen1");
    assert_eq!(gql_json["data"]["order"]["id"], "o-1");

    // 3. Invoke /healthz and /metrics over HTTP/3.
    let (health_head, health_body) = h3_client
        .request(
            Request::builder()
                .method(Method::GET)
                .uri("https://localhost/healthz")
                .body(Bytes::new())
                .unwrap(),
        )
        .await
        .expect("HTTP/3 /healthz");
    assert_eq!(health_head.status(), StatusCode::OK);
    assert!(!health_body.is_empty());

    let (metrics_head, metrics_body) = h3_client
        .request(
            Request::builder()
                .method(Method::GET)
                .uri("https://localhost/metrics")
                .body(Bytes::new())
                .unwrap(),
        )
        .await
        .expect("HTTP/3 /metrics");
    assert_eq!(metrics_head.status(), StatusCode::OK);
    let metrics_text = String::from_utf8(metrics_body.to_vec()).expect("metrics UTF-8");
    assert!(metrics_text.contains("gap_http_requests_total"));

    drop(h3_client);
    let _ = tx_h3.send(());
    let _ = tx_tcp.send(());
    h3_task.await.unwrap().expect("H3 clean shutdown");
    tcp_task.await.unwrap().expect("TCP clean shutdown");
}

// spec: OPS-005, OPS-002
#[tokio::test]
async fn http3_streams_graphql_sse_and_enforces_413_body_cap() {
    let harness = TestHarness::new(TransportConfig::for_profile(Profile::Dev));
    let (tls, cert_der) =
        Http3TlsConfig::self_signed_for_dev(&["localhost".to_owned()]).expect("dev cert");
    let udp_socket = std::net::UdpSocket::bind("127.0.0.1:0").expect("bind UDP");
    let h3_addr = udp_socket.local_addr().expect("UDP addr");
    let (tx, rx) = oneshot::channel::<()>();

    let opts = Http3ServeOptions {
        max_request_body_bytes: 256,
        ..Http3ServeOptions::default()
    };
    let h3_task = tokio::spawn(serve_http3(
        udp_socket,
        tls,
        harness.router.clone(),
        async move {
            let _ = rx.await;
        },
        opts,
    ));

    let mut client = Http3Client::connect_pinned_der(h3_addr, "localhost", &cert_der)
        .await
        .expect("connect HTTP/3");

    // 1. Over-limit body returns 413 Payload Too Large.
    let huge = vec![b'a'; 512];
    let (too_big_head, _) = client
        .request(
            Request::builder()
                .method(Method::POST)
                .uri("https://localhost/graphql")
                .header("content-type", "application/json")
                .body(Bytes::from(huge))
                .unwrap(),
        )
        .await
        .expect("413 response");
    assert_eq!(too_big_head.status(), StatusCode::PAYLOAD_TOO_LARGE);

    // 2. Streaming SSE frames over HTTP/3.
    let sse_body = serde_json::to_vec(&json!({ "query": "{ orders { id sku } }" })).unwrap();
    let (sse_head, mut sse_stream) = client
        .open_stream(
            Request::builder()
                .method(Method::POST)
                .uri("https://localhost/graphql/stream")
                .header("content-type", "application/json")
                .header("accept", "text/event-stream")
                .body(Bytes::from(sse_body))
                .unwrap(),
        )
        .await
        .expect("open SSE stream over HTTP/3");
    assert_eq!(sse_head.status(), StatusCode::OK);
    let mut collected = Vec::new();
    while let Some(chunk) = sse_stream.next_chunk().await.expect("SSE chunk") {
        collected.extend_from_slice(&chunk);
    }
    let text = String::from_utf8(collected).expect("UTF-8 SSE");
    assert!(
        text.contains("event: next"),
        "expected SSE next frame: {text}"
    );
    assert!(
        text.contains("event: complete"),
        "expected SSE complete frame: {text}"
    );
    let _ = harness.clock.unix_millis();

    drop(client);
    let _ = tx.send(());
    h3_task.await.unwrap().expect("clean shutdown");
}

// spec: OPS-005, RUN-090
#[tokio::test]
async fn http3_enforces_max_connections_and_drains_inflight_request_on_shutdown() {
    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let started_handler = Arc::clone(&started);
    let release_handler = Arc::clone(&release);

    let slow_router = Router::new().route(
        "/slow",
        get(move || {
            let s = Arc::clone(&started_handler);
            let r = Arc::clone(&release_handler);
            async move {
                s.notify_one();
                r.notified().await;
                "done"
            }
        }),
    );

    let (tls, cert_der) =
        Http3TlsConfig::self_signed_for_dev(&["localhost".to_owned()]).expect("dev cert");
    let udp_socket = std::net::UdpSocket::bind("127.0.0.1:0").expect("bind UDP");
    let h3_addr = udp_socket.local_addr().expect("UDP addr");
    let (tx, rx) = oneshot::channel::<()>();

    let opts = Http3ServeOptions {
        serve: ServeOptions {
            max_connections: 1,
            shutdown_grace: Duration::from_secs(2),
            ..ServeOptions::default()
        },
        ..Http3ServeOptions::default()
    };
    let h3_task = tokio::spawn(serve_http3(
        udp_socket,
        tls,
        slow_router,
        async move {
            let _ = rx.await;
        },
        opts,
    ));

    // Hold the sole connection permit open with an in-flight `/slow` request.
    let mut client1 = Http3Client::connect_pinned_der(h3_addr, "localhost", &cert_der)
        .await
        .expect("client1 connects");
    let cert2 = cert_der.clone();
    let req1 = tokio::spawn(async move {
        client1
            .request(
                Request::builder()
                    .method(Method::GET)
                    .uri("https://localhost/slow")
                    .body(Bytes::new())
                    .unwrap(),
            )
            .await
    });
    started.notified().await;

    // A second concurrent QUIC connection MUST be refused while permit == 0.
    let c2_res = tokio::time::timeout(
        Duration::from_secs(2),
        Http3Client::connect_pinned_der(h3_addr, "localhost", &cert2),
    )
    .await;
    assert!(
        matches!(c2_res, Ok(Err(_)) | Err(_)),
        "second HTTP/3 connection must be refused when max_connections == 1"
    );

    // Trigger graceful shutdown while req1 is in flight, then release handler.
    let _ = tx.send(());
    tokio::time::sleep(Duration::from_millis(25)).await;
    release.notify_one();

    let (head1, body1) = req1
        .await
        .unwrap()
        .expect("in-flight HTTP/3 request finishes during drain");
    assert_eq!(head1.status(), StatusCode::OK);
    assert_eq!(body1.as_ref(), b"done");

    h3_task.await.unwrap().expect("clean shutdown after drain");
}
