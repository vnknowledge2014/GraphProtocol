//! Shared SSRF-guarded HTTP/1.1 transport abstraction for System-1 decision providers
//! (`ADR-028`, `RTE-049`, `INV-15`).

use std::collections::BTreeMap;
use std::fmt;
use std::fmt::Write as _;
use std::io::Write;
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::time::{Duration, Instant};

use gap_core::error::{ErrorCode, GapError, GapResult};
use gap_route::GroupOption;
use gap_route::{SINK_OPTION_KEY, SINK_OPTION_TEXT, sanitize_default};

use crate::candle::json::{JsonValue, escape_json_str, parse_json};
use crate::providers::types::{DecisionAnswer, DecisionBatchResponse, DecisionQuestion};
use crate::sidecar::egress::{
    DefaultSidecarEgressGuard, EgressGuardHook, ParsedEndpoint, resolve_and_validate,
};
use crate::sidecar::protocol::read_http_response;

/// Decoded HTTP response frame returned by [`DecisionHttpTransport`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecisionHttpResponse {
    /// HTTP status code (e.g. `200`).
    pub status_code: u16,
    /// Decoded UTF-8 response body.
    pub body: String,
}

/// Pluggable HTTP/HTTPS client transport used by remote System-1 adapters (`Jev`, `Laya`, `Gemini`).
///
/// Header slices may carry credentials such as `Authorization` or `x-goog-api-key`;
/// implementations MUST NOT log or echo header values in errors (`INV-15`).
pub trait DecisionHttpTransport: Send + Sync + fmt::Debug {
    /// Dispatches a `POST` request with `application/json` body and returns the decoded response.
    fn post_json(
        &self,
        url: &str,
        extra_headers: &[(&str, &str)],
        json_body: &str,
        timeout: Duration,
    ) -> GapResult<DecisionHttpResponse>;
}

/// Pure-std synchronous HTTP/1.1 transport enforcing [`EgressGuardHook`] SSRF validation (`RTE-049`).
#[derive(Debug, Clone)]
pub struct TcpHttpDecisionTransport {
    /// TCP connect timeout budget.
    pub connect_timeout: Duration,
    /// Maximum allowed response payload in bytes.
    pub max_response_bytes: usize,
    /// Fail-closed egress policy invoked prior to socket connection (`INV-07` / SSRF defense).
    pub egress_guard: Arc<dyn EgressGuardHook>,
}

impl TcpHttpDecisionTransport {
    /// Builds a strict transport blocking loopback/RFC1918/metadata IPs.
    #[must_use]
    pub fn strict() -> Self {
        Self {
            connect_timeout: crate::sidecar::DEFAULT_SIDECAR_CONNECT_TIMEOUT,
            max_response_bytes: crate::sidecar::DEFAULT_MAX_RESPONSE_BYTES,
            egress_guard: DefaultSidecarEgressGuard::strict().into_hook(),
        }
    }

    /// Builds a loopback-permitted transport for local sidecars and offline integration tests.
    #[must_use]
    pub fn loopback() -> Self {
        Self {
            egress_guard: DefaultSidecarEgressGuard::loopback_allowed().into_hook(),
            ..Self::strict()
        }
    }
}

impl DecisionHttpTransport for TcpHttpDecisionTransport {
    fn post_json(
        &self,
        url: &str,
        extra_headers: &[(&str, &str)],
        json_body: &str,
        timeout: Duration,
    ) -> GapResult<DecisionHttpResponse> {
        let trimmed = url.trim();
        if trimmed.starts_with("https://") {
            return Err(GapError::new(
                ErrorCode::DiscoveryDegraded,
                "native TcpHttpDecisionTransport handles `http://` sidecars/proxies cleanly; supply a TLS-capable DecisionHttpTransport or local TLS proxy for `https://`",
            ));
        }

        let parsed = ParsedEndpoint::parse(trimmed)?;
        let raw_path = trimmed
            .strip_prefix("http://")
            .and_then(|rest| rest.find('/').map(|idx| &rest[idx..]))
            .filter(|p| !p.is_empty())
            .unwrap_or("/");
        let effective_total = timeout.max(Duration::from_millis(1));
        let started = Instant::now();
        let addrs = resolve_and_validate(&parsed, self.egress_guard.as_ref())?;
        let connect_budget = self
            .connect_timeout
            .min(effective_total)
            .max(Duration::from_millis(1));
        let mut stream = connect_first(&addrs, connect_budget)?;

        let remaining = effective_total.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            return Err(GapError::new(
                ErrorCode::DiscoveryDegraded,
                "decision request timeout exhausted during connect",
            ));
        }
        let _ = stream.set_write_timeout(Some(remaining));
        let _ = stream.set_read_timeout(Some(remaining));

        let path = raw_path;
        let mut req_buf = format!(
            "POST {path} HTTP/1.1\r\nHost: {}\r\nContent-Type: application/json\r\nAccept: application/json\r\nContent-Length: {}\r\nConnection: close\r\n",
            parsed.host_header(),
            json_body.len()
        );
        for (name, val) in extra_headers {
            // Reject CR/LF header injection (`INV-15`).
            if name.contains(['\r', '\n']) || val.contains(['\r', '\n']) {
                return Err(GapError::new(
                    ErrorCode::ValidationError,
                    format!("header `{name}` contains forbidden newline control characters"),
                ));
            }
            let _ = write!(req_buf, "{name}: {val}\r\n");
        }
        req_buf.push_str("\r\n");
        req_buf.push_str(json_body);

        stream.write_all(req_buf.as_bytes()).map_err(|err| {
            GapError::new(
                ErrorCode::DiscoveryDegraded,
                format!("decision provider HTTP write error: {err}"),
            )
        })?;
        stream.flush().map_err(|err| {
            GapError::new(
                ErrorCode::DiscoveryDegraded,
                format!("decision provider HTTP flush error: {err}"),
            )
        })?;

        let frame = read_http_response(&mut stream, self.max_response_bytes)?;
        Ok(DecisionHttpResponse {
            status_code: frame.status_code,
            body: frame.body,
        })
    }
}

fn connect_first(addrs: &[SocketAddr], timeout: Duration) -> GapResult<TcpStream> {
    let mut last_err = None;
    for addr in addrs {
        match TcpStream::connect_timeout(addr, timeout) {
            Ok(stream) => return Ok(stream),
            Err(err) => last_err = Some(err),
        }
    }
    let detail = last_err.map_or_else(|| "no addresses".to_owned(), |e| e.to_string());
    Err(GapError::new(
        ErrorCode::DiscoveryDegraded,
        format!("failed to connect to decision provider: {detail}"),
    ))
}

/// Builds a sanitized single-route `choice` question map compatible with Laya, Jev, and Gemini Decision 1.
#[must_use]
pub(crate) fn build_routing_questions(
    question: &str,
    options: &[GroupOption],
) -> BTreeMap<String, DecisionQuestion> {
    let clean_question = sanitize_default(question);
    let mut criteria = Vec::with_capacity(options.len());
    for opt in options {
        let text = if opt.key == SINK_OPTION_KEY {
            SINK_OPTION_TEXT.to_owned()
        } else {
            sanitize_default(&opt.text)
        };
        criteria.push((opt.key.clone(), text));
    }
    let mut map = BTreeMap::new();
    map.insert(
        "route".to_owned(),
        DecisionQuestion::Choice {
            instructions: clean_question,
            criteria,
        },
    );
    map
}

/// Writes shared JSON `{ "model": ..., "<input_field>": ..., "questions": { ... } }` request body.
pub(crate) fn encode_decision_request_json(
    model: &str,
    input_field: &str,
    state_json_or_text: &str,
    questions: &BTreeMap<String, DecisionQuestion>,
) -> String {
    let mut out = String::with_capacity(512);
    out.push_str("{\"model\":\"");
    out.push_str(&escape_json_str(model));
    out.push_str("\",\"");
    out.push_str(&escape_json_str(input_field));
    out.push_str("\":");
    // Preserve structured JSON objects/arrays when valid; otherwise encode as sanitized JSON string.
    let trimmed = state_json_or_text.trim();
    if (trimmed.starts_with('{') || trimmed.starts_with('[')) && parse_json(trimmed).is_ok() {
        out.push_str(trimmed);
    } else {
        out.push('"');
        out.push_str(&escape_json_str(state_json_or_text));
        out.push('"');
    }
    out.push_str(",\"questions\":{");
    for (q_idx, (qid, q)) in questions.iter().enumerate() {
        if q_idx > 0 {
            out.push(',');
        }
        out.push('"');
        out.push_str(&escape_json_str(qid));
        out.push_str("\":");
        match q {
            DecisionQuestion::Choice {
                instructions,
                criteria,
            } => {
                out.push_str("{\"type\":\"choice\",\"instructions\":\"");
                out.push_str(&escape_json_str(instructions));
                out.push_str("\",\"criteria\":{");
                for (c_idx, (k, desc)) in criteria.iter().enumerate() {
                    if c_idx > 0 {
                        out.push(',');
                    }
                    out.push('"');
                    out.push_str(&escape_json_str(k));
                    out.push_str("\":\"");
                    out.push_str(&escape_json_str(desc));
                    out.push('"');
                }
                out.push_str("}}");
            }
            DecisionQuestion::Score {
                instructions,
                criteria,
            } => {
                out.push_str("{\"type\":\"score\",\"instructions\":\"");
                out.push_str(&escape_json_str(instructions));
                out.push_str("\",\"criteria\":[");
                for (c_idx, label) in criteria.iter().enumerate() {
                    if c_idx > 0 {
                        out.push(',');
                    }
                    out.push('"');
                    out.push_str(&escape_json_str(label));
                    out.push('"');
                }
                out.push_str("]}");
            }
            DecisionQuestion::Noul { instructions } => {
                out.push_str("{\"type\":\"noul\",\"instructions\":\"");
                out.push_str(&escape_json_str(instructions));
                out.push_str("\"}");
            }
        }
    }
    out.push_str("}}");
    out
}

/// Parses provider answers object (shared across `Jev`, `@receptron/laya`, and `gemini-decision-1`).
pub(crate) fn parse_answers_map(
    answers_val: &JsonValue,
) -> GapResult<BTreeMap<String, DecisionAnswer>> {
    let obj = answers_val.as_object().ok_or_else(|| {
        GapError::new(
            ErrorCode::DiscoveryDegraded,
            "decision provider response `answers` must be a JSON object",
        )
    })?;
    let mut parsed = BTreeMap::new();
    for (qid, ans_val) in obj {
        parsed.insert(qid.clone(), parse_single_answer(qid, ans_val)?);
    }
    Ok(parsed)
}

fn parse_single_answer(qid: &str, val: &JsonValue) -> GapResult<DecisionAnswer> {
    let kind = val
        .get("type")
        .and_then(JsonValue::as_str)
        .unwrap_or_else(|| {
            if val.get("noul").is_some() {
                "noul"
            } else if val.get("score").is_some() {
                "score"
            } else {
                "choice"
            }
        });

    let confidence_opt = val
        .get("answer_confidence")
        .or_else(|| val.get("confidence"))
        .and_then(JsonValue::as_f64)
        .map(|v| v as f32);

    match kind {
        "choice" => {
            let choice = val
                .get("choice")
                .and_then(JsonValue::as_str)
                .map(str::to_owned)
                .unwrap_or_default();
            let probs = parse_prob_object(qid, val.get("probabilities"))?;
            let max_p = probs.iter().map(|(_, p)| *p).fold(0.0_f32, f32::max);
            Ok(DecisionAnswer::Choice {
                choice,
                probabilities: probs,
                confidence: confidence_opt.unwrap_or(max_p),
            })
        }
        "score" => {
            let score = val
                .get("score")
                .and_then(JsonValue::as_f64)
                .ok_or_else(|| {
                    GapError::new(
                        ErrorCode::DiscoveryDegraded,
                        format!("score answer `{qid}` missing numeric `score`"),
                    )
                })? as f32;
            let probs = match val.get("probabilities") {
                Some(p) => parse_prob_object(qid, Some(p))?,
                None => Vec::new(),
            };
            let mut legend = Vec::new();
            if let Some(leg_obj) = val.get("legend").and_then(JsonValue::as_object) {
                for (k, v) in leg_obj {
                    if let Some(s) = v.as_str() {
                        legend.push((k.clone(), s.to_owned()));
                    }
                }
            }
            let max_p = probs.iter().map(|(_, p)| *p).fold(0.0_f32, f32::max);
            Ok(DecisionAnswer::Score {
                score,
                legend,
                probabilities: probs,
                confidence: confidence_opt.unwrap_or(max_p),
            })
        }
        "noul" => {
            let noul = val.get("noul").and_then(JsonValue::as_f64).ok_or_else(|| {
                GapError::new(
                    ErrorCode::DiscoveryDegraded,
                    format!("noul answer `{qid}` missing numeric `noul` probability"),
                )
            })? as f32;
            let clamped = noul.clamp(0.0_f32, 1.0_f32);
            Ok(DecisionAnswer::Noul {
                noul: clamped,
                confidence: confidence_opt.unwrap_or_else(|| clamped.max(1.0_f32 - clamped)),
            })
        }
        other => Err(GapError::new(
            ErrorCode::DiscoveryDegraded,
            format!("unsupported decision answer type `{other}` for `{qid}`"),
        )),
    }
}

fn parse_prob_object(qid: &str, raw: Option<&JsonValue>) -> GapResult<Vec<(String, f32)>> {
    let map = raw.and_then(JsonValue::as_object).ok_or_else(|| {
        GapError::new(
            ErrorCode::DiscoveryDegraded,
            format!("decision answer `{qid}` missing `probabilities` object"),
        )
    })?;
    let mut out = Vec::with_capacity(map.len());
    for (k, v) in map {
        let p = v.as_f64().ok_or_else(|| {
            GapError::new(
                ErrorCode::DiscoveryDegraded,
                format!("decision probability `{qid}.{k}` is not a finite number"),
            )
        })? as f32;
        if !p.is_finite() {
            return Err(GapError::new(
                ErrorCode::DiscoveryDegraded,
                format!("decision probability `{qid}.{k}` overflowed f32"),
            ));
        }
        out.push((k.clone(), p));
    }
    Ok(out)
}

/// Resolves a configured base or full endpoint URL so it ends cleanly with `expected_suffix`.
#[must_use]
pub(crate) fn resolve_provider_url(endpoint: &str, expected_suffix: &str) -> String {
    let trimmed = endpoint.trim().trim_end_matches('/');
    if trimmed.ends_with(expected_suffix) {
        trimmed.to_owned()
    } else {
        format!("{trimmed}{expected_suffix}")
    }
}

/// Parameters for executing a shared batch decision request (`ADR-028`).
pub(crate) struct DecisionBatchParams<'a> {
    pub(crate) post_url: &'a str,
    pub(crate) headers: &'a [(&'a str, &'a str)],
    pub(crate) model: &'a str,
    pub(crate) input_field: &'a str,
    pub(crate) state_json_or_text: &'a str,
    pub(crate) questions: &'a BTreeMap<String, DecisionQuestion>,
    pub(crate) timeout: Duration,
    pub(crate) provider_label: &'a str,
}

/// Shared batch decision execution across Laya, Jev, and Gemini Decision 1 providers (`ADR-028`).
pub(crate) fn dispatch_decision_batch(
    transport: &dyn DecisionHttpTransport,
    params: &DecisionBatchParams<'_>,
) -> GapResult<DecisionBatchResponse> {
    if params.questions.is_empty() {
        return Err(GapError::new(
            ErrorCode::ValidationError,
            format!(
                "{}: `questions` map must be non-empty",
                params.provider_label
            ),
        ));
    }
    let body = encode_decision_request_json(
        params.model,
        params.input_field,
        params.state_json_or_text,
        params.questions,
    );
    let resp = transport.post_json(params.post_url, params.headers, &body, params.timeout)?;
    if !(200..300).contains(&resp.status_code) {
        // Never include outbound auth headers in errors (`INV-15`).
        return Err(GapError::new(
            ErrorCode::DiscoveryDegraded,
            format!(
                "{} returned HTTP {}: {}",
                params.provider_label, resp.status_code, resp.body
            ),
        ));
    }
    let val = parse_json(&resp.body).map_err(|err| {
        GapError::new(
            ErrorCode::DiscoveryDegraded,
            format!("invalid {} response JSON: {err}", params.provider_label),
        )
    })?;
    let answers_val = val
        .get("answers")
        .or_else(|| val.get("decisions"))
        .ok_or_else(|| {
            GapError::new(
                ErrorCode::DiscoveryDegraded,
                format!(
                    "{} response missing `answers` object",
                    params.provider_label
                ),
            )
        })?;
    let answers = parse_answers_map(answers_val)?;
    let echoed_model = val
        .get("model")
        .or_else(|| val.get("model_id"))
        .and_then(JsonValue::as_str)
        .unwrap_or(params.model)
        .to_owned();
    let usage = val.get("usage");
    let input_tokens = usage
        .and_then(|u| u.get("input_tokens").or_else(|| u.get("inputTokens")))
        .and_then(JsonValue::as_usize)
        .unwrap_or(0) as u64;
    let output_tokens = usage
        .and_then(|u| u.get("output_tokens").or_else(|| u.get("outputTokens")))
        .and_then(JsonValue::as_usize)
        .unwrap_or(0) as u64;

    Ok(DecisionBatchResponse {
        model: echoed_model,
        answers,
        input_tokens,
        output_tokens,
    })
}
