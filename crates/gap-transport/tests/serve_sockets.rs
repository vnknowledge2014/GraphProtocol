//! The hardened listener (`serve`): header-read timeout, connection cap, graceful drain (`RUN-090`, `INV-02`).
//! Real loopback sockets: `#[ignore]`d in sandboxes without sockets, run by the reviewer with `--ignored`.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::TestHarness;
use gap_core::context::RequestContext;
use gap_core::error::GapResult;
use gap_core::traits::{BoxFuture, HttpExchangeRequest, HttpExchangeResponse, HttpModule};
use gap_transport::{Profile, ServeOptions, TransportConfig, serve};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;

#[derive(Debug)]
struct SlowModule {
    delay: Duration,
    started: Arc<Mutex<u32>>,
}

impl HttpModule for SlowModule {
    fn module_name(&self) -> &'static str {
        "slow"
    }

    fn mount_prefixes(&self) -> &'static [&'static str] {
        &["/slow"]
    }

    fn handle<'a>(
        &'a self,
        _ctx: &'a RequestContext,
        _req: HttpExchangeRequest,
    ) -> BoxFuture<'a, GapResult<HttpExchangeResponse>> {
        Box::pin(async move {
            *self.started.lock().unwrap() += 1;
            tokio::time::sleep(self.delay).await;
            Ok(HttpExchangeResponse {
                status: 200,
                headers: Vec::new(),
                body: b"done".to_vec(),
            })
        })
    }
}

struct Running {
    addr: std::net::SocketAddr,
    stop: Option<oneshot::Sender<()>>,
    task: tokio::task::JoinHandle<std::io::Result<()>>,
    started: Arc<Mutex<u32>>,
    _harness: TestHarness,
}

async fn start(options: ServeOptions, delay: Duration) -> Running {
    let started = Arc::new(Mutex::new(0));
    let module: Arc<dyn HttpModule> = Arc::new(SlowModule {
        delay,
        started: Arc::clone(&started),
    });
    let harness =
        TestHarness::with_modules(TransportConfig::for_profile(Profile::Dev), vec![module]);
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind loopback");
    let addr = listener.local_addr().unwrap();
    let (stop, stopped) = oneshot::channel::<()>();
    let app = harness.router.clone();
    let task = tokio::spawn(async move {
        serve(
            listener,
            app,
            async move {
                let _ = stopped.await;
            },
            options,
        )
        .await
    });
    Running {
        addr,
        stop: Some(stop),
        task,
        started,
        _harness: harness,
    }
}

async fn read_to_end_within(stream: &mut TcpStream, limit: Duration) -> (String, bool) {
    let mut out = Vec::new();
    let closed = tokio::time::timeout(limit, async {
        let mut buf = [0_u8; 4096];
        loop {
            match stream.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => out.extend_from_slice(&buf[..n]),
            }
        }
    })
    .await
    .is_ok();
    (String::from_utf8_lossy(&out).into_owned(), closed)
}

fn options(header_ms: u64, max_connections: usize, grace_ms: u64) -> ServeOptions {
    ServeOptions {
        header_read_timeout: Duration::from_millis(header_ms),
        max_connections,
        shutdown_grace: Duration::from_millis(grace_ms),
    }
}

// spec: OPS-001
#[test]
fn serve_options_reject_unusable_values() {
    assert!(ServeOptions::default().validate().is_ok());
    for bad in [options(0, 10, 100), options(100, 0, 100)] {
        assert_eq!(
            bad.validate().unwrap_err().kind(),
            std::io::ErrorKind::InvalidInput
        );
    }
}

// spec: OPS-002
#[tokio::test]
#[ignore = "needs loopback sockets"]
async fn a_normal_request_is_served_with_keep_alive() {
    let server = start(options(2_000, 16, 1_000), Duration::from_millis(1)).await;
    let mut stream = TcpStream::connect(server.addr).await.unwrap();
    for _ in 0..2 {
        stream
            .write_all(b"GET /slow HTTP/1.1\r\nHost: x\r\n\r\n")
            .await
            .unwrap();
        let mut buf = [0_u8; 1024];
        let n = tokio::time::timeout(Duration::from_secs(3), stream.read(&mut buf))
            .await
            .expect("answered in time")
            .unwrap();
        let text = String::from_utf8_lossy(&buf[..n]);
        assert!(text.starts_with("HTTP/1.1 200"), "{text}");
        assert!(text.ends_with("done"), "{text}");
    }
    assert_eq!(
        *server.started.lock().unwrap(),
        2,
        "both requests used one connection"
    );
}

// spec: INV-02, OPS-001
#[tokio::test]
#[ignore = "needs loopback sockets"]
async fn a_connection_that_never_finishes_its_request_head_is_closed() {
    let server = start(options(300, 16, 1_000), Duration::from_millis(1)).await;
    let mut stream = TcpStream::connect(server.addr).await.unwrap();
    stream
        .write_all(b"POST /slow HTTP/1.1\r\nHost: x\r\n")
        .await
        .unwrap();
    let started = std::time::Instant::now();
    let (_, closed) = read_to_end_within(&mut stream, Duration::from_secs(5)).await;
    assert!(
        closed,
        "a half-sent request head must not hold the connection open"
    );
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "closed after {:?}, the timeout is 300 ms",
        started.elapsed()
    );
    assert_eq!(
        *server.started.lock().unwrap(),
        0,
        "the half request never reached a handler"
    );
}

// spec: INV-02
#[tokio::test]
#[ignore = "needs loopback sockets"]
async fn connections_beyond_the_cap_are_closed_at_once_and_freed_slots_are_reused() {
    let server = start(options(5_000, 2, 1_000), Duration::from_millis(1)).await;
    let idle_a = TcpStream::connect(server.addr).await.unwrap();
    let idle_b = TcpStream::connect(server.addr).await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;

    let mut third = TcpStream::connect(server.addr).await.unwrap();
    let _ = third
        .write_all(b"GET /slow HTTP/1.1\r\nHost: x\r\n\r\n")
        .await;
    let (text, closed) = read_to_end_within(&mut third, Duration::from_secs(2)).await;
    assert!(
        closed && text.is_empty(),
        "the third connection is shed, not queued: {text:?}"
    );

    // Freeing a slot lets a new connection through.
    drop(idle_a);
    tokio::time::sleep(Duration::from_millis(200)).await;
    let mut fourth = TcpStream::connect(server.addr).await.unwrap();
    fourth
        .write_all(b"GET /slow HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();
    let (text, _) = read_to_end_within(&mut fourth, Duration::from_secs(3)).await;
    assert!(text.starts_with("HTTP/1.1 200"), "{text}");
    drop(idle_b);
}

// spec: RUN-090
#[tokio::test]
#[ignore = "needs loopback sockets"]
async fn shutdown_lets_in_flight_requests_finish_and_refuses_new_connections() {
    let mut server = start(options(2_000, 16, 5_000), Duration::from_millis(600)).await;
    let mut stream = TcpStream::connect(server.addr).await.unwrap();
    stream
        .write_all(b"GET /slow HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(
        *server.started.lock().unwrap(),
        1,
        "the request is in flight"
    );

    server.stop.take().unwrap().send(()).unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        TcpStream::connect(server.addr).await.is_err(),
        "the listener is closed as soon as the shutdown signal arrives"
    );

    let (text, _) = read_to_end_within(&mut stream, Duration::from_secs(3)).await;
    assert!(
        text.starts_with("HTTP/1.1 200") && text.ends_with("done"),
        "{text:?}"
    );
    let finished = tokio::time::timeout(Duration::from_secs(3), server.task)
        .await
        .expect("serve returns once the in-flight request is done");
    finished.unwrap().unwrap();
}

// spec: RUN-090
#[tokio::test]
#[ignore = "needs loopback sockets"]
async fn shutdown_gives_up_after_the_grace_period() {
    let mut server = start(options(2_000, 16, 300), Duration::from_secs(30)).await;
    let mut stream = TcpStream::connect(server.addr).await.unwrap();
    stream
        .write_all(b"GET /slow HTTP/1.1\r\nHost: x\r\n\r\n")
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(150)).await;
    server.stop.take().unwrap().send(()).unwrap();
    let started = std::time::Instant::now();
    tokio::time::timeout(Duration::from_secs(3), server.task)
        .await
        .expect("serve must return after the grace period even with a stuck request")
        .unwrap()
        .unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "{:?}",
        started.elapsed()
    );
}
