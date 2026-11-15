//! Wire codecs and bounded HTTP/1.1 framing for `/v1/systemone` and `/v1/systemone/batch`
//! (`RTE-041`, `RTE-042`, `RTE-044`, `WP-M5-06`).

use std::fmt::Write as _;
use std::io::Read;

use gap_core::error::{ErrorCode, GapError, GapResult};
use gap_route::GroupOption;

use crate::candle::json::{JsonValue, escape_json_str, parse_json};

/// Maximum HTTP response header block (`16 KiB`).
pub const MAX_HTTP_HEADER_BYTES: usize = 16 * 1024;

/// Maximum single HTTP response body (`1 MiB` default).
pub const DEFAULT_MAX_RESPONSE_BYTES: usize = 1024 * 1024;

/// Single System-1 `/v1/systemone` request payload (`omni decide --endpoint` compatible).
#[derive(Debug, Clone, PartialEq)]
pub struct SystemOneRequest {
    /// Classification head name (`"choice"`).
    pub head: String,
    /// Canonical JCS state JSON string.
    pub state_jcs: String,
    /// Choice question prompt.
    pub question: String,
    /// Candidate options in caller-supplied order.
    pub options: Vec<GroupOption>,
    /// Inference temperature (`1.0` when raw uncalibrated logits are requested per `RTE-041`).
    pub temperature: f32,
}

impl SystemOneRequest {
    /// Serializes a single request as compact JSON compatible with `/v1/systemone` and `omni decide`.
    #[must_use]
    pub fn to_json_string(&self) -> String {
        let mut out = String::with_capacity(256);
        write_request_json(&mut out, self);
        out
    }

    /// Parses a single `/v1/systemone` request JSON document (tolerating either `state` object or string).
    ///
    /// # Errors
    ///
    /// Returns [`GapError`] if required fields are missing or invalid.
    pub fn from_json_value(val: &JsonValue) -> GapResult<Self> {
        let head = val
            .get("head")
            .and_then(JsonValue::as_str)
            .unwrap_or("choice")
            .to_owned();
        if head != "choice" {
            return Err(GapError::new(
                ErrorCode::ValidationError,
                format!("unsupported /v1/systemone head `{head}`; expected `choice`"),
            ));
        }

        let state_jcs = match val.get("state").or_else(|| val.get("state_jcs")) {
            Some(JsonValue::String(s)) => s.clone(),
            Some(other) => serialize_json_value(other),
            None => "{}".to_owned(),
        };

        let question = val
            .get("question")
            .and_then(JsonValue::as_str)
            .unwrap_or(gap_route::s1_adapter::S1_CHOICE_QUESTION)
            .to_owned();

        let opts_val = val
            .get("options")
            .and_then(JsonValue::as_array)
            .ok_or_else(|| {
                GapError::new(
                    ErrorCode::ValidationError,
                    "missing `options` array in /v1/systemone request",
                )
            })?;
        if opts_val.is_empty() {
            return Err(GapError::new(
                ErrorCode::ValidationError,
                "`options` array must be non-empty",
            ));
        }

        let mut options = Vec::with_capacity(opts_val.len());
        for (idx, entry) in opts_val.iter().enumerate() {
            match entry {
                JsonValue::Object(_) => {
                    let key = entry
                        .get("key")
                        .and_then(JsonValue::as_str)
                        .map_or_else(|| format!("g{idx}"), str::to_owned);
                    let text = entry
                        .get("text")
                        .or_else(|| entry.get("label"))
                        .and_then(JsonValue::as_str)
                        .ok_or_else(|| {
                            GapError::new(
                                ErrorCode::ValidationError,
                                format!("option[{idx}] missing `text` field"),
                            )
                        })?
                        .to_owned();
                    let prior = entry
                        .get("prior")
                        .and_then(JsonValue::as_f64)
                        .map_or(0.0, |v| v as f32);
                    options.push(GroupOption { key, text, prior });
                }
                JsonValue::String(text) => {
                    options.push(GroupOption {
                        key: format!("g{idx}"),
                        text: text.clone(),
                        prior: 0.0,
                    });
                }
                _ => {
                    return Err(GapError::new(
                        ErrorCode::ValidationError,
                        format!("option[{idx}] must be an object or string"),
                    ));
                }
            }
        }

        let temperature = val
            .get("temperature")
            .and_then(JsonValue::as_f64)
            .map_or(1.0, |v| v as f32);

        Ok(Self {
            head,
            state_jcs,
            question,
            options,
            temperature,
        })
    }
}

/// Batch System-1 `/v1/systemone/batch` request payload.
#[derive(Debug, Clone, PartialEq)]
pub struct SystemOneBatchRequest<'a> {
    /// Ordered slice of choice requests.
    pub requests: &'a [SystemOneRequest],
}

impl<'a> SystemOneBatchRequest<'a> {
    /// Serializes the batch request as compact JSON.
    #[must_use]
    pub fn to_json_string(&self) -> String {
        let mut out = String::with_capacity(256 * self.requests.len().max(1));
        out.push_str("{\"requests\":[");
        for (idx, req) in self.requests.iter().enumerate() {
            if idx > 0 {
                out.push(',');
            }
            write_request_json(&mut out, req);
        }
        out.push_str("]}");
        out
    }
}

/// Decoded single `/v1/systemone` response payload.
#[derive(Debug, Clone, PartialEq)]
pub struct SystemOneResponse {
    /// Optional server-reported model identifier.
    pub model_id: Option<String>,
    /// Raw logits aligned with caller options (`RTE-041`).
    pub logits: Vec<f32>,
}

impl SystemOneResponse {
    /// Serializes a single response into compact JSON.
    #[must_use]
    pub fn to_json_string(model_id: &str, logits: &[f32]) -> String {
        let mut out = String::with_capacity(128);
        out.push_str("{\"model_id\":\"");
        out.push_str(&escape_json_str(model_id));
        out.push_str("\",\"logits\":[");
        write_f32_slice(&mut out, logits);
        out.push_str("]}");
        out
    }

    /// Parses and validates `body` expecting `expected_len` finite logits.
    ///
    /// # Errors
    ///
    /// Returns [`GapError`] on malformed JSON, missing `logits`, non-finite entries, or length mismatch.
    pub fn parse(body: &str, expected_len: usize) -> GapResult<Self> {
        let val = parse_json(body).map_err(|err| {
            GapError::new(
                ErrorCode::DiscoveryDegraded,
                format!("invalid /v1/systemone response JSON: {err}"),
            )
        })?;
        Self::from_json_value(&val, expected_len)
    }

    /// Extracts validated logits from a parsed JSON response object.
    ///
    /// # Errors
    ///
    /// Returns [`GapError`] on missing fields, non-finite numbers, or count mismatch.
    pub fn from_json_value(val: &JsonValue, expected_len: usize) -> GapResult<Self> {
        let model_id = val
            .get("model_id")
            .and_then(JsonValue::as_str)
            .map(str::to_owned);
        let arr = val
            .get("logits")
            .or_else(|| val.get("scores"))
            .and_then(JsonValue::as_array)
            .ok_or_else(|| {
                GapError::new(
                    ErrorCode::DiscoveryDegraded,
                    "/v1/systemone response missing `logits` array",
                )
            })?;
        if arr.len() != expected_len {
            return Err(GapError::new(
                ErrorCode::DiscoveryDegraded,
                format!(
                    "/v1/systemone logit length mismatch: expected {expected_len}, got {}",
                    arr.len()
                ),
            ));
        }

        let mut logits = Vec::with_capacity(expected_len);
        for (idx, item) in arr.iter().enumerate() {
            let num = item.as_f64().ok_or_else(|| {
                GapError::new(
                    ErrorCode::DiscoveryDegraded,
                    format!("/v1/systemone logit[{idx}] is not a finite number"),
                )
            })?;
            let f = num as f32;
            if !f.is_finite() {
                return Err(GapError::new(
                    ErrorCode::DiscoveryDegraded,
                    format!("/v1/systemone logit[{idx}] overflowed f32"),
                ));
            }
            logits.push(f);
        }

        Ok(Self { model_id, logits })
    }
}

/// Batch response serializer and decoder for `/v1/systemone/batch`.
#[derive(Debug, Clone, PartialEq)]
pub struct SystemOneBatchResponse {
    /// Optional server-reported model identifier.
    pub model_id: Option<String>,
    /// Per-request decoded results aligned 1:1 with caller requests.
    pub results: Vec<SystemOneResponse>,
}

impl SystemOneBatchResponse {
    /// Serializes a batch response into compact JSON.
    #[must_use]
    pub fn to_json_string(model_id: &str, logits_batch: &[Vec<f32>]) -> String {
        let mut out = String::with_capacity(128 * logits_batch.len().max(1));
        out.push_str("{\"model_id\":\"");
        out.push_str(&escape_json_str(model_id));
        out.push_str("\",\"results\":[");
        for (idx, logits) in logits_batch.iter().enumerate() {
            if idx > 0 {
                out.push(',');
            }
            out.push_str("{\"logits\":[");
            write_f32_slice(&mut out, logits);
            out.push_str("]}");
        }
        out.push_str("]}");
        out
    }

    /// Parses and validates a `/v1/systemone/batch` JSON body against `expected_option_lens`.
    ///
    /// # Errors
    ///
    /// Returns [`GapError`] on JSON syntax errors, result count mismatch, or invalid logits.
    pub fn parse(body: &str, expected_option_lens: &[usize]) -> GapResult<Self> {
        let val = parse_json(body).map_err(|err| {
            GapError::new(
                ErrorCode::DiscoveryDegraded,
                format!("invalid /v1/systemone/batch response JSON: {err}"),
            )
        })?;
        let model_id = val
            .get("model_id")
            .and_then(JsonValue::as_str)
            .map(str::to_owned);
        let arr = val
            .get("results")
            .and_then(JsonValue::as_array)
            .ok_or_else(|| {
                GapError::new(
                    ErrorCode::DiscoveryDegraded,
                    "/v1/systemone/batch response missing `results` array",
                )
            })?;
        if arr.len() != expected_option_lens.len() {
            return Err(GapError::new(
                ErrorCode::DiscoveryDegraded,
                format!(
                    "/v1/systemone/batch result count mismatch: expected {}, got {}",
                    expected_option_lens.len(),
                    arr.len()
                ),
            ));
        }

        let mut results = Vec::with_capacity(arr.len());
        for (item, &expected_len) in arr.iter().zip(expected_option_lens.iter()) {
            results.push(SystemOneResponse::from_json_value(item, expected_len)?);
        }
        Ok(Self { model_id, results })
    }
}

fn write_request_json(out: &mut String, req: &SystemOneRequest) {
    out.push_str("{\"head\":\"");
    out.push_str(&escape_json_str(&req.head));
    out.push_str("\",\"state\":\"");
    out.push_str(&escape_json_str(&req.state_jcs));
    out.push_str("\",\"question\":\"");
    out.push_str(&escape_json_str(&req.question));
    out.push_str("\",\"options\":[");
    for (idx, opt) in req.options.iter().enumerate() {
        if idx > 0 {
            out.push(',');
        }
        out.push_str("{\"key\":\"");
        out.push_str(&escape_json_str(&opt.key));
        out.push_str("\",\"text\":\"");
        out.push_str(&escape_json_str(&opt.text));
        out.push_str("\",\"prior\":");
        let safe_prior = if opt.prior.is_finite() {
            opt.prior
        } else {
            0.0
        };
        let _ = write!(out, "{safe_prior:.6}");
        out.push('}');
    }
    out.push_str("],\"temperature\":");
    let safe_temp = if req.temperature.is_finite() && req.temperature > 0.0 {
        req.temperature
    } else {
        1.0
    };
    let _ = write!(out, "{safe_temp:.6}");
    out.push('}');
}

fn write_f32_slice(out: &mut String, vals: &[f32]) {
    for (idx, &v) in vals.iter().enumerate() {
        if idx > 0 {
            out.push(',');
        }
        let safe = if v.is_finite() { v } else { 0.0 };
        let _ = write!(out, "{safe}");
    }
}

/// Canonical deterministic serializer for parsed [`JsonValue`] trees (using sorted `BTreeMap` keys).
#[must_use]
pub fn serialize_json_value(val: &JsonValue) -> String {
    let mut out = String::new();
    write_json_value(&mut out, val);
    out
}

fn write_json_value(out: &mut String, val: &JsonValue) {
    match val {
        JsonValue::Null => out.push_str("null"),
        JsonValue::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        JsonValue::Number(n) => {
            if n.fract() == 0.0 && *n >= (i64::MIN as f64) && *n <= (i64::MAX as f64) {
                let _ = write!(out, "{}", *n as i64);
            } else {
                let _ = write!(out, "{n}");
            }
        }
        JsonValue::String(s) => {
            out.push('"');
            out.push_str(&escape_json_str(s));
            out.push('"');
        }
        JsonValue::Array(items) => {
            out.push('[');
            for (idx, item) in items.iter().enumerate() {
                if idx > 0 {
                    out.push(',');
                }
                write_json_value(out, item);
            }
            out.push(']');
        }
        JsonValue::Object(map) => {
            out.push('{');
            for (idx, (k, v)) in map.iter().enumerate() {
                if idx > 0 {
                    out.push(',');
                }
                out.push('"');
                out.push_str(&escape_json_str(k));
                out.push_str("\":");
                write_json_value(out, v);
            }
            out.push('}');
        }
    }
}

/// Decoded bounded HTTP/1.1 response message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpResponseFrame {
    /// HTTP status code (e.g. `200`).
    pub status_code: u16,
    /// UTF-8 body bytes validated against `max_body_bytes`.
    pub body: String,
}

/// Reads an HTTP/1.1 response from `reader`, enforcing `MAX_HTTP_HEADER_BYTES` and `max_body_bytes`.
///
/// # Errors
///
/// Returns [`GapError`] on I/O timeout/error, malformed HTTP status/headers, chunked framing errors,
/// or body size exceeding `max_body_bytes`.
pub fn read_http_response<R: Read>(
    reader: &mut R,
    max_body_bytes: usize,
) -> GapResult<HttpResponseFrame> {
    let mut header_buf = Vec::with_capacity(512);
    let mut byte = [0u8; 1];
    let mut header_end = None;

    while header_buf.len() < MAX_HTTP_HEADER_BYTES {
        let n = reader.read(&mut byte).map_err(|err| {
            GapError::new(
                ErrorCode::DiscoveryDegraded,
                format!("sidecar HTTP response read error: {err}"),
            )
        })?;
        if n == 0 {
            break;
        }
        header_buf.push(byte[0]);
        if header_buf.ends_with(b"\r\n\r\n") {
            header_end = Some(header_buf.len());
            break;
        }
    }

    let _ = header_end.ok_or_else(|| {
        GapError::new(
            ErrorCode::DiscoveryDegraded,
            "sidecar HTTP response missing header terminator or exceeded 16 KiB header limit",
        )
    })?;

    let header_str = std::str::from_utf8(&header_buf).map_err(|_| {
        GapError::new(
            ErrorCode::DiscoveryDegraded,
            "sidecar HTTP response headers are not valid UTF-8",
        )
    })?;

    let mut lines = header_str.split("\r\n");
    let status_line = lines.next().unwrap_or_default();
    let status_code = parse_status_line(status_line)?;

    let mut content_length: Option<usize> = None;
    let mut is_chunked = false;
    for line in lines {
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            let lower = name.trim().to_ascii_lowercase();
            let val_trimmed = value.trim();
            if lower == "content-length" {
                let parsed = val_trimmed.parse::<usize>().map_err(|_| {
                    GapError::new(
                        ErrorCode::DiscoveryDegraded,
                        format!("invalid Content-Length header `{val_trimmed}`"),
                    )
                })?;
                if parsed > max_body_bytes {
                    return Err(GapError::new(
                        ErrorCode::DiscoveryDegraded,
                        format!(
                            "sidecar response_too_large: Content-Length {parsed} exceeds cap {max_body_bytes}"
                        ),
                    ));
                }
                content_length = Some(parsed);
            } else if lower == "transfer-encoding"
                && val_trimmed.to_ascii_lowercase().contains("chunked")
            {
                is_chunked = true;
            }
        }
    }

    let raw_body = if is_chunked {
        read_chunked_body(reader, max_body_bytes)?
    } else if let Some(len) = content_length {
        let mut body = vec![0u8; len];
        reader.read_exact(&mut body).map_err(|err| {
            GapError::new(
                ErrorCode::DiscoveryDegraded,
                format!("sidecar HTTP body read_exact failed: {err}"),
            )
        })?;
        body
    } else {
        let cap_plus_one = max_body_bytes.saturating_add(1);
        let mut body = Vec::new();
        reader
            .take(cap_plus_one as u64)
            .read_to_end(&mut body)
            .map_err(|err| {
                GapError::new(
                    ErrorCode::DiscoveryDegraded,
                    format!("sidecar HTTP body read_to_end failed: {err}"),
                )
            })?;
        if body.len() > max_body_bytes {
            return Err(GapError::new(
                ErrorCode::DiscoveryDegraded,
                format!("sidecar response_too_large: body exceeded {max_body_bytes} bytes"),
            ));
        }
        body
    };

    let body = String::from_utf8(raw_body).map_err(|_| {
        GapError::new(
            ErrorCode::DiscoveryDegraded,
            "sidecar HTTP response body is not valid UTF-8",
        )
    })?;

    Ok(HttpResponseFrame { status_code, body })
}

fn parse_status_line(line: &str) -> GapResult<u16> {
    let mut parts = line.split_whitespace();
    let proto = parts.next().unwrap_or_default();
    let code_str = parts.next().unwrap_or_default();
    if !proto.starts_with("HTTP/1.") {
        return Err(GapError::new(
            ErrorCode::DiscoveryDegraded,
            format!("unexpected sidecar HTTP status line `{line}`"),
        ));
    }
    code_str.parse::<u16>().map_err(|_| {
        GapError::new(
            ErrorCode::DiscoveryDegraded,
            format!("invalid sidecar HTTP status code `{code_str}`"),
        )
    })
}

fn read_chunked_body<R: Read>(reader: &mut R, max_body_bytes: usize) -> GapResult<Vec<u8>> {
    let mut out = Vec::new();
    loop {
        let size_line = read_crlf_line(reader, 128)?;
        let hex_part = size_line.split(';').next().unwrap_or_default().trim();
        let chunk_len = usize::from_str_radix(hex_part, 16).map_err(|_| {
            GapError::new(
                ErrorCode::DiscoveryDegraded,
                format!("invalid HTTP chunk size `{hex_part}`"),
            )
        })?;
        if chunk_len == 0 {
            let mut crlf = [0u8; 2];
            reader.read_exact(&mut crlf).map_err(|err| {
                GapError::new(
                    ErrorCode::DiscoveryDegraded,
                    format!("failed to read terminal chunk CRLF: {err}"),
                )
            })?;
            return Ok(out);
        }
        if out.len().saturating_add(chunk_len) > max_body_bytes {
            return Err(GapError::new(
                ErrorCode::DiscoveryDegraded,
                format!("sidecar response_too_large: chunked body exceeds {max_body_bytes} bytes"),
            ));
        }
        let prev_len = out.len();
        out.resize(prev_len + chunk_len, 0);
        reader.read_exact(&mut out[prev_len..]).map_err(|err| {
            GapError::new(
                ErrorCode::DiscoveryDegraded,
                format!("failed to read HTTP chunk body: {err}"),
            )
        })?;
        let mut crlf = [0u8; 2];
        reader.read_exact(&mut crlf).map_err(|err| {
            GapError::new(
                ErrorCode::DiscoveryDegraded,
                format!("failed to read post-chunk CRLF: {err}"),
            )
        })?;
    }
}

fn read_crlf_line<R: Read>(reader: &mut R, max_len: usize) -> GapResult<String> {
    let mut buf = Vec::with_capacity(32);
    let mut b = [0u8; 1];
    while buf.len() < max_len {
        let n = reader.read(&mut b).map_err(|err| {
            GapError::new(
                ErrorCode::DiscoveryDegraded,
                format!("HTTP line read error: {err}"),
            )
        })?;
        if n == 0 {
            return Err(GapError::new(
                ErrorCode::DiscoveryDegraded,
                "unexpected EOF while reading HTTP line",
            ));
        }
        buf.push(b[0]);
        if buf.ends_with(b"\r\n") {
            buf.truncate(buf.len() - 2);
            return String::from_utf8(buf).map_err(|_| {
                GapError::new(
                    ErrorCode::DiscoveryDegraded,
                    "non-UTF-8 HTTP chunk size line",
                )
            });
        }
    }
    Err(GapError::new(
        ErrorCode::DiscoveryDegraded,
        "HTTP chunk header line exceeded max length",
    ))
}
