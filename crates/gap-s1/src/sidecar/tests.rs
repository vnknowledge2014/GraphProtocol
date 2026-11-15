#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll, Waker};
use std::thread;
use std::time::{Duration, Instant};

use gap_core::error::{ErrorCode, GapError, GapResult};
use gap_route::s1_adapter::{DecisionBackend, S1_CHOICE_QUESTION};
use gap_route::{GroupOption, SINK_OPTION_KEY, SINK_OPTION_TEXT};

use super::egress::{
    DefaultSidecarEgressGuard, EgressGuardHook, ParsedEndpoint, resolve_and_validate,
};
use super::protocol::{SystemOneRequest, read_http_response};
use super::{HttpSidecar, LoopbackSidecarServer, SidecarConfig};
use crate::candle::{LayaCandle, Precision, parse_json};

fn block_on_ready<F: std::future::Future>(fut: F) -> F::Output {
    let mut fut = std::pin::pin!(fut);
    let waker = Waker::noop();
    let mut cx = Context::from_waker(waker);
    match fut.as_mut().poll(&mut cx) {
        Poll::Ready(val) => val,
        Poll::Pending => panic!("expected synchronous sidecar future to complete immediately"),
    }
}

fn sample_options() -> Vec<GroupOption> {
    vec![
        GroupOption {
            key: "g0".to_owned(),
            text: "Create or fetch customer invoices in billing ledger".to_owned(),
            prior: 0.85,
        },
        GroupOption {
            key: "g1".to_owned(),
            text: "List warehouse shipment tracking updates".to_owned(),
            prior: 0.45,
        },
        GroupOption {
            key: SINK_OPTION_KEY.to_owned(),
            text: SINK_OPTION_TEXT.to_owned(),
            prior: 0.15,
        },
    ]
}

// spec: RTE-041, RTE-044
// Design ref: ADR-005
#[test]
fn sidecar_single_and_batch_roundtrip_matches_laya_candle_golden_oracle() {
    let candle = Arc::new(LayaCandle::calibrated_default(Precision::F32));
    let oracle_backend: Arc<dyn DecisionBackend> = Arc::clone(&candle) as Arc<dyn DecisionBackend>;
    let server = LoopbackSidecarServer::start(oracle_backend).expect("start oracle sidecar");

    let cfg = SidecarConfig::loopback(server.endpoint_url());
    let sidecar = HttpSidecar::new(cfg).expect("valid loopback sidecar");

    let opts_a = sample_options();
    let opts_b = vec![
        GroupOption {
            key: "g0".to_owned(),
            text: "Reset customer password token in identity directory".to_owned(),
            prior: 0.80,
        },
        GroupOption {
            key: SINK_OPTION_KEY.to_owned(),
            text: SINK_OPTION_TEXT.to_owned(),
            prior: 0.15,
        },
    ];
    let state_a = r#"{"constraints":{"max_side_effect":"NONE"},"query":"list billing invoices"}"#;
    let state_b = r#"{"constraints":{"max_side_effect":"REVERSIBLE"},"query":"reset password"}"#;

    let direct_a = candle
        .forward(state_a, S1_CHOICE_QUESTION, &opts_a)
        .expect("candle direct forward a")
        .choice_logits;
    let direct_b = candle
        .forward(state_b, S1_CHOICE_QUESTION, &opts_b)
        .expect("candle direct forward b")
        .choice_logits;

    let sidecar_a = block_on_ready(sidecar.score_choice(state_a, S1_CHOICE_QUESTION, &opts_a))
        .expect("single sidecar request");
    assert_eq!(sidecar_a.len(), direct_a.len());
    for (s, d) in sidecar_a.iter().zip(direct_a.iter()) {
        assert!((s - d).abs() <= 1e-5, "single mismatch: {s} vs {d}");
    }

    let batch_out = sidecar
        .score_choice_batch(
            &[
                (state_a, S1_CHOICE_QUESTION, opts_a.as_slice()),
                (state_b, S1_CHOICE_QUESTION, opts_b.as_slice()),
            ],
            Duration::from_millis(500),
        )
        .expect("batch sidecar request");
    assert_eq!(batch_out.len(), 2);
    for (s, d) in batch_out[0].iter().zip(direct_a.iter()) {
        assert!((s - d).abs() <= 1e-5, "batch[0] mismatch: {s} vs {d}");
    }
    for (s, d) in batch_out[1].iter().zip(direct_b.iter()) {
        assert!((s - d).abs() <= 1e-5, "batch[1] mismatch: {s} vs {d}");
    }

    // Also verify `omni decide --endpoint` compatible request JSON roundtrip + chunked HTTP parser.
    let omni_req = SystemOneRequest {
        head: "choice".to_owned(),
        state_jcs: state_a.to_owned(),
        question: S1_CHOICE_QUESTION.to_owned(),
        options: opts_a,
        temperature: 1.0,
    };
    let parsed_req = parse_json(&omni_req.to_json_string()).expect("valid request JSON");
    assert_eq!(
        parsed_req.get("head").and_then(|v| v.as_str()),
        Some("choice")
    );

    let chunked_raw = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n19\r\n{\"logits\":[1.0,0.2,-0.5]}\r\n0\r\n\r\n";
    let mut cursor = std::io::Cursor::new(&chunked_raw[..]);
    let frame = read_http_response(&mut cursor, 4096).expect("chunked decode");
    assert_eq!(frame.status_code, 200);
    assert_eq!(frame.body, r#"{"logits":[1.0,0.2,-0.5]}"#);
}

// spec: RTE-044
#[test]
fn sidecar_enforces_io_timeout_and_response_size_cap_rte_044() {
    // 1. Stalling server exceeds `score_choice_with_timeout` budget promptly.
    let stall_listener = TcpListener::bind("127.0.0.1:0").expect("bind stall listener");
    let stall_addr = stall_listener.local_addr().unwrap();
    let stall_handle = thread::spawn(move || {
        if let Ok((mut stream, _)) = stall_listener.accept() {
            let mut drain = [0u8; 256];
            let _ = stream.read(&mut drain);
            thread::sleep(Duration::from_millis(350));
        }
    });

    let mut cfg = SidecarConfig::loopback(format!("http://{stall_addr}"));
    cfg.io_timeout = Duration::from_millis(60);
    let sidecar = HttpSidecar::new(cfg).expect("sidecar config");
    let opts = sample_options();

    let started = Instant::now();
    let timeout_err = block_on_ready(sidecar.score_choice_with_timeout(
        r#"{"query":"stall"}"#,
        S1_CHOICE_QUESTION,
        &opts,
        Duration::from_millis(50),
    ))
    .expect_err("stalling sidecar must fail on read timeout");
    assert_eq!(timeout_err.code(), ErrorCode::DiscoveryDegraded);
    assert!(
        started.elapsed() < Duration::from_millis(300),
        "sidecar timeout exceeded wall-clock bound: {:?}",
        started.elapsed()
    );
    let _ = stall_handle.join();

    // 2. Oversized response body violates `max_response_bytes`.
    let big_listener = TcpListener::bind("127.0.0.1:0").expect("bind big listener");
    let big_addr = big_listener.local_addr().unwrap();
    let big_handle = thread::spawn(move || {
        if let Ok((mut stream, _)) = big_listener.accept() {
            let mut drain = [0u8; 512];
            let _ = stream.read(&mut drain);
            let pad = "x".repeat(4096);
            let body =
                format!("{{\"model_id\":\"big\",\"logits\":[0.1,0.2,0.3],\"pad\":\"{pad}\"}}");
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(resp.as_bytes());
        }
    });

    let mut small_cap_cfg = SidecarConfig::loopback(format!("http://{big_addr}"));
    small_cap_cfg.max_response_bytes = 512;
    let small_sidecar = HttpSidecar::new(small_cap_cfg).expect("capped sidecar");
    let size_err = small_sidecar
        .score_choice_sync(
            r#"{"query":"big"}"#,
            S1_CHOICE_QUESTION,
            &opts,
            Duration::from_millis(250),
        )
        .expect_err("oversized body must be rejected");
    assert!(size_err.to_string().contains("response_too_large"));
    let _ = big_handle.join();
}

#[derive(Debug)]
struct CountingEgressHook {
    calls: AtomicUsize,
    block_port: u16,
}

impl EgressGuardHook for CountingEgressHook {
    fn check_endpoint(
        &self,
        endpoint: &ParsedEndpoint,
        _resolved: &[std::net::SocketAddr],
    ) -> GapResult<()> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        if endpoint.port == self.block_port {
            return Err(GapError::new(
                ErrorCode::DiscoveryDegraded,
                "custom hook blocked port",
            ));
        }
        Ok(())
    }
}

// spec: RTE-041, RTE-044
#[test]
fn sidecar_egress_guard_blocks_metadata_private_userinfo_and_unpermitted_loopback() {
    for bad_url in [
        "https://example.com:8443",
        "http://user:pass@example.com:8080",
        "http://example.com:8080/api?debug=1",
        "http://example.com:8080/../secret",
        "http://127.0.0.1:0/v1/systemone",
    ] {
        assert!(
            ParsedEndpoint::parse(bad_url).is_err(),
            "URL parser must reject `{bad_url}`"
        );
    }

    // Strict guard blocks metadata, RFC1918 private subnets, and loopback unless permitted.
    let strict = DefaultSidecarEgressGuard::strict();
    for blocked in [
        "http://169.254.169.254:80",
        "http://100.100.100.200:80",
        "http://[fd00:ec2::254]:80",
        "http://metadata.google.internal:80",
        "http://10.1.2.3:8080",
        "http://172.16.4.5:8080",
        "http://192.168.1.50:8080",
        "http://127.0.0.1:8080",
        "http://[::1]:8080",
    ] {
        let ep = ParsedEndpoint::parse(blocked).expect("syntactically valid blocked target");
        let err = resolve_and_validate(&ep, &strict)
            .expect_err(&format!("strict egress guard must reject `{blocked}`"));
        assert_eq!(err.code(), ErrorCode::DiscoveryDegraded);
    }

    // Loopback-allowed permits 127.0.0.1 while STILL blocking cloud metadata and private CIDRs.
    let loopback_ok = DefaultSidecarEgressGuard::loopback_allowed();
    let local_ep = ParsedEndpoint::parse("http://127.0.0.1:8080/v1/systemone").unwrap();
    assert!(resolve_and_validate(&local_ep, &loopback_ok).is_ok());
    assert_eq!(local_ep.base_path, "");
    assert_eq!(local_ep.request_path("/v1/systemone"), "/v1/systemone");

    let meta_ep = ParsedEndpoint::parse("http://169.254.169.254:80").unwrap();
    assert!(resolve_and_validate(&meta_ep, &loopback_ok).is_err());

    // Custom EgressGuardHook is invoked before opening any TCP connection.
    let hook = Arc::new(CountingEgressHook {
        calls: AtomicUsize::new(0),
        block_port: 19_999,
    });
    let mut custom_cfg = SidecarConfig::loopback("http://127.0.0.1:19999");
    custom_cfg.egress_guard = Arc::clone(&hook) as Arc<dyn EgressGuardHook>;
    let blocked_sidecar = HttpSidecar::new(custom_cfg).unwrap();
    let opts = sample_options();
    let err = blocked_sidecar
        .score_choice_sync(
            r#"{"query":"blocked"}"#,
            S1_CHOICE_QUESTION,
            &opts,
            Duration::from_millis(100),
        )
        .expect_err("custom hook must block connect");
    assert!(err.to_string().contains("custom hook blocked port"));
    assert_eq!(hook.calls.load(Ordering::Relaxed), 1);
}
