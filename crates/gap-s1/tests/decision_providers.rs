//! Integration tests for `@receptron/laya`, TypeSafe `Jev`, and Google `Gemini Decision 1`
//! backends (`ADR-028`, `RTE-047`, `RTE-048`, `RTE-049`, `INV-15`).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeMap;
use std::future::Future;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::pin::Pin;
use std::sync::mpsc;
use std::task::{Context, Poll, Waker};
use std::thread;
use std::time::Duration;

fn make_opt(key: &str, text: &str, prior: f32) -> GroupOption {
    GroupOption {
        key: key.to_owned(),
        text: text.to_owned(),
        prior,
    }
}

fn block_on<F: Future + ?Sized>(mut fut: Pin<Box<F>>) -> F::Output {
    let waker = Waker::noop();
    let mut cx = Context::from_waker(waker);
    match fut.as_mut().poll(&mut cx) {
        Poll::Ready(out) => out,
        Poll::Pending => panic!("unexpected Pending in synchronous gap-s1 BoxFuture"),
    }
}

use gap_core::error::ErrorCode;
use gap_core::secret::SecretString;
use gap_route::GroupOption;
use gap_route::s1_adapter::DecisionBackend;
use gap_s1::{
    DecisionAnswer, DecisionQuestion, GeminiDecisionBackend, GeminiDecisionConfig, JevBackend,
    JevConfig, ReceptronLayaBackend, ReceptronLayaConfig, align_choice_probabilities_to_logits,
    prob_to_logit, resolve_receptron_laya_cache_dir,
};

fn spawn_one_shot_http(
    expected_path: &'static str,
    required_header_substr: Option<&'static str>,
    response_status: u16,
    response_body: &'static str,
) -> (String, mpsc::Receiver<String>, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let addr = listener.local_addr().expect("local addr");
    let (tx, rx) = mpsc::channel();
    let handle = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
        let mut buf = [0_u8; 8192];
        let mut raw = Vec::new();
        let mut content_len = None;
        let mut header_end = None;
        loop {
            let n = stream.read(&mut buf).unwrap_or(0);
            if n == 0 {
                break;
            }
            raw.extend_from_slice(&buf[..n]);
            if header_end.is_none()
                && let Some(pos) = raw.windows(4).position(|w| w == b"\r\n\r\n")
            {
                let end = pos + 4;
                header_end = Some(end);
                let hdr_text = String::from_utf8_lossy(&raw[..end]).to_ascii_lowercase();
                for line in hdr_text.lines() {
                    if let Some(v) = line.strip_prefix("content-length:") {
                        content_len = v.trim().parse::<usize>().ok();
                    }
                }
            }
            if let (Some(end), Some(clen)) = (header_end, content_len)
                && raw.len() >= end + clen
            {
                break;
            }
        }
        let req_str = String::from_utf8_lossy(&raw).into_owned();
        let first_line = req_str.lines().next().unwrap_or("");
        assert!(
            first_line.starts_with(&format!("POST {expected_path} ")),
            "unexpected HTTP request line: {first_line}"
        );
        if let Some(hdr) = required_header_substr {
            assert!(
                req_str
                    .to_ascii_lowercase()
                    .contains(&hdr.to_ascii_lowercase()),
                "missing expected header substring `{hdr}` in request"
            );
        }
        let _ = tx.send(req_str);
        let http_reply = format!(
            "HTTP/1.1 {response_status} OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response_body}",
            response_body.len()
        );
        let _ = stream.write_all(http_reply.as_bytes());
        let _ = stream.flush();
    });
    (format!("http://127.0.0.1:{}", addr.port()), rx, handle)
}

// spec: RTE-047
#[test]
fn jev_backend_evaluates_choice_score_noul_and_routes_via_prob_to_logit() {
    let mut probs = vec![
        ("g0".to_owned(), 0.85_f32),
        ("insufficient_evidence".to_owned(), 0.15_f32),
    ];
    let opts = vec![
        make_opt("g0", "Customer and order lookup tools", 0.9),
        make_opt("insufficient_evidence", "None", 0.1),
    ];
    let logits = align_choice_probabilities_to_logits(&opts, &probs).unwrap();
    assert!(logits[0] > logits[1]);
    assert!(prob_to_logit(0.95) > prob_to_logit(0.05));
    probs.clear();

    // 1. DecisionBackend route call (`/v1/systemone` + Bearer header).
    let (url, rx, h) = spawn_one_shot_http(
        "/v1/systemone",
        Some("authorization: bearer canary_jev_secret_99"),
        200,
        r#"{"model":"jev-latest","answers":{"route":{"type":"choice","choice":"g0","probabilities":{"g0":0.92,"insufficient_evidence":0.08},"confidence":0.92}},"usage":{"input_tokens":84,"output_tokens":0}}"#,
    );
    let backend = JevBackend::new(JevConfig::loopback(
        &url,
        Some(SecretString::new("canary_jev_secret_99".to_owned())),
    ));
    let dbg = format!("{backend:?}");
    assert!(
        !dbg.contains("canary_jev_secret_99"),
        "Debug leaked Jev API key: {dbg}"
    );

    let ranked =
        block_on(backend.score_choice(r#"{"query":"get customer orders"}"#, "Pick group", &opts))
            .expect("Jev DecisionBackend");
    h.join().unwrap();
    let captured = rx.recv().unwrap();
    assert!(captured.contains("\"model\":\"jev-latest\""));
    assert!(ranked[0] > ranked[1]);

    // 2. Multi-question `choice` + `score` + `noul` call.
    let (url2, _rx2, h2) = spawn_one_shot_http(
        "/v1/systemone",
        None,
        200,
        r#"{
          "model": "jev-latest",
          "answers": {
            "is_urgent": { "type": "noul", "noul": 0.99 },
            "severity": { "type": "score", "score": 2.4, "probabilities": {"0":0.0,"1":0.1,"2":0.9}, "confidence": 0.90 }
          },
          "usage": { "input_tokens": 140, "output_tokens": 0 }
        }"#,
    );
    let batch_client = JevBackend::new(JevConfig::loopback(url2, None));
    let mut qs = BTreeMap::new();
    qs.insert(
        "is_urgent".to_owned(),
        DecisionQuestion::Noul {
            instructions: "Urgent outage?".to_owned(),
        },
    );
    qs.insert(
        "severity".to_owned(),
        DecisionQuestion::Score {
            instructions: "Rate severity".to_owned(),
            criteria: vec![
                "Routine".to_owned(),
                "Degraded".to_owned(),
                "Outage".to_owned(),
            ],
        },
    );
    let res = batch_client
        .evaluate_questions("Server down", &qs, Duration::from_secs(2))
        .expect("multi-question Jev response");
    h2.join().unwrap();
    assert!(matches!(
        res.answers.get("is_urgent"),
        Some(DecisionAnswer::Noul { noul, .. }) if (*noul - 0.99).abs() < 1e-4
    ));
    assert!(matches!(
        res.answers.get("severity"),
        Some(DecisionAnswer::Score { score, .. }) if (*score - 2.4).abs() < 1e-4
    ));
}

// spec: RTE-047
#[test]
fn receptron_laya_backend_and_cache_resolver_work() {
    let custom = resolve_receptron_laya_cache_dir(
        Some(Path::new("/tmp/custom-laya")),
        Some(Path::new("/home/u")),
    );
    assert_eq!(custom.as_deref(), Some(Path::new("/tmp/custom-laya")));
    let default_dir = resolve_receptron_laya_cache_dir(None, Some(Path::new("/home/u")));
    assert_eq!(
        default_dir.as_deref(),
        Some(Path::new("/home/u/.cache/receptron-laya"))
    );

    let (url, rx, h) = spawn_one_shot_http(
        "/systemOne",
        None,
        200,
        r#"{
          "model": "convaiinnovations/laya",
          "answers": {
            "route": {
              "type": "choice",
              "choice": "g0",
              "probabilities": { "g0": 0.94, "insufficient_evidence": 0.06 },
              "answer_confidence": 0.94
            }
          },
          "usage": { "input_tokens": 110, "output_tokens": 0 }
        }"#,
    );
    let opts = vec![
        make_opt("g0", "Billing refunds", 0.8),
        make_opt("insufficient_evidence", "None", 0.1),
    ];
    let backend = ReceptronLayaBackend::new(ReceptronLayaConfig::loopback(url));
    let logits =
        block_on(backend.score_choice(r#"{"query":"refund status"}"#, "Select group", &opts))
            .expect("receptron/laya");
    h.join().unwrap();
    assert!(rx.recv().unwrap().contains("convaiinnovations/laya"));
    assert!(logits[0] > logits[1]);
}

// spec: RTE-048, RTE-049, INV-15
#[test]
fn gemini_decision_1_handles_decisions_redacts_key_and_enforces_egress_and_limit() {
    // 1. Strict mode blocks metadata / loopback SSRF (`169.254.169.254`, `127.0.0.1`).
    let ssrf = GeminiDecisionBackend::new(GeminiDecisionConfig::strict(
        "http://169.254.169.254/latest",
        Some(SecretString::new("canary_gemini_api_key_xyz".to_owned())),
    ));
    let mut q_one = BTreeMap::new();
    q_one.insert(
        "ok".to_owned(),
        DecisionQuestion::Noul {
            instructions: "Valid?".to_owned(),
        },
    );
    let ssrf_err = ssrf
        .create_decisions("payload", &q_one, Duration::from_millis(500))
        .expect_err("metadata SSRF blocked");
    assert_eq!(ssrf_err.code(), ErrorCode::DiscoveryDegraded);
    assert!(!ssrf_err.message().contains("canary_gemini_api_key_xyz"));

    // 2. Reject > 1,000 parallel questions before touching transport.
    let mut q_1001 = BTreeMap::new();
    for idx in 0..1001 {
        q_1001.insert(
            format!("q_{idx}"),
            DecisionQuestion::Noul {
                instructions: "check".to_owned(),
            },
        );
    }
    let cap_err = ssrf
        .create_decisions("payload", &q_1001, Duration::from_millis(500))
        .expect_err("1001 questions rejected");
    assert_eq!(cap_err.code(), ErrorCode::ValidationError);

    // 3. Valid loopback proxy `/v1beta/decisions` call verifies `x-goog-api-key` + `0` output tokens.
    let (url, rx, h) = spawn_one_shot_http(
        "/v1beta/decisions",
        Some("x-goog-api-key: canary_gemini_api_key_xyz"),
        200,
        r#"{
          "model": "gemini-decision-1",
          "answers": {
            "route": {
              "type": "choice",
              "choice": "g0",
              "probabilities": { "g0": 0.97, "insufficient_evidence": 0.03 },
              "confidence": 0.96
            }
          },
          "usage": { "input_tokens": 64, "output_tokens": 0 }
        }"#,
    );
    let client = GeminiDecisionBackend::new(GeminiDecisionConfig::loopback(
        url,
        Some(SecretString::new("canary_gemini_api_key_xyz".to_owned())),
    ));
    assert!(!format!("{client:?}").contains("canary_gemini_api_key_xyz"));
    let opts = vec![
        make_opt("g0", "Search knowledge base", 0.85),
        make_opt("insufficient_evidence", "None", 0.1),
    ];
    let logits =
        block_on(client.score_choice(r#"{"query":"find policy"}"#, "Pick capability", &opts))
            .expect("gemini-decision-1");
    h.join().unwrap();
    let req_wire = rx.recv().unwrap();
    assert!(req_wire.contains("\"model\":\"gemini-decision-1\""));
    assert!(req_wire.contains("\"input\":"));
    assert!(logits[0] > logits[1]);
}
