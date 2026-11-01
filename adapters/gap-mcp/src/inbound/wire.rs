//! HTTP and `stdio` JSON-RPC 2.0 wire handling (`CMP-001`, `INV-01`, `INV-15`).

use gap_core::context::RequestContext;
use gap_core::error::{GapError, GapResult};
use gap_core::traits::{HttpExchangeRequest, HttpExchangeResponse};
use serde_json::{Map, Value, json};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};
use uuid::Uuid;

use super::{ContextFactory, FacadeConfig, FacadeInner, resources, tools};

/// Stateless MCP revision (`2026-07-28`).
pub(crate) const VERSION_MODERN: &str = "2026-07-28";
/// Compatibility stateful MCP revision (`2025-11-25`).
pub(crate) const VERSION_LEGACY: &str = "2025-11-25";
/// Versions supported by this facade (`CMP-001`).
pub(crate) const SUPPORTED_VERSIONS: [&str; 2] = [VERSION_MODERN, VERSION_LEGACY];

/// `_meta` key carrying the protocol version in stateless `2026-07-28` requests.
pub(crate) const META_PROTOCOL_VERSION: &str = "io.modelcontextprotocol/protocolVersion";
/// `_meta` key carrying client capabilities in stateless `2026-07-28` requests.
pub(crate) const META_CLIENT_CAPABILITIES: &str = "io.modelcontextprotocol/clientCapabilities";
/// `_meta` key carrying server identity in stateless `2026-07-28` responses.
pub(crate) const META_SERVER_INFO: &str = "io.modelcontextprotocol/serverInfo";

/// Public TTL in milliseconds advertised on static discovery/resource list responses (`60s`).
pub(crate) const PUBLIC_TTL_MS: u64 = 60_000;

/// JSON-RPC 2.0 Parse error.
pub(crate) const ERR_PARSE_ERROR: i64 = -32700;
/// JSON-RPC 2.0 Invalid Request.
pub(crate) const ERR_INVALID_REQUEST: i64 = -32600;
/// JSON-RPC 2.0 Method not found.
pub(crate) const ERR_METHOD_NOT_FOUND: i64 = -32601;
/// JSON-RPC 2.0 Invalid params.
pub(crate) const ERR_INVALID_PARAMS: i64 = -32602;
/// JSON-RPC 2.0 Internal error.
pub(crate) const ERR_INTERNAL_ERROR: i64 = -32603;
/// MCP `2026-07-28` `HeaderMismatchError`.
pub(crate) const ERR_HEADER_MISMATCH: i64 = -32020;
/// MCP `2026-07-28` `UnsupportedProtocolVersionError`.
pub(crate) const ERR_UNSUPPORTED_VERSION: i64 = -32022;

/// Protocol era negotiated or carried per request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WireEra {
    /// Stateless MCP `2026-07-28`.
    Modern,
    /// Stateful compatibility MCP `2025-11-25`.
    Legacy,
}

/// Structured JSON-RPC protocol error prior to HTTP/stdio serialization.
#[derive(Debug, Clone)]
pub(crate) struct RpcError {
    pub(crate) http_status: u16,
    pub(crate) code: i64,
    pub(crate) message: String,
    pub(crate) data: Option<Value>,
}

impl RpcError {
    pub(crate) fn parse_error() -> Self {
        Self {
            http_status: 400,
            code: ERR_PARSE_ERROR,
            message: "Parse error".to_owned(),
            data: None,
        }
    }

    pub(crate) fn invalid_request(http_status: u16, message: impl Into<String>) -> Self {
        Self {
            http_status,
            code: ERR_INVALID_REQUEST,
            message: message.into(),
            data: None,
        }
    }

    pub(crate) fn header_mismatch(message: impl Into<String>) -> Self {
        Self {
            http_status: 400,
            code: ERR_HEADER_MISMATCH,
            message: message.into(),
            data: None,
        }
    }

    pub(crate) fn unsupported_version(requested: &str) -> Self {
        Self {
            http_status: 400,
            code: ERR_UNSUPPORTED_VERSION,
            message: "Unsupported MCP protocol version".to_owned(),
            data: Some(json!({
                "supported": SUPPORTED_VERSIONS,
                "requested": requested,
            })),
        }
    }

    pub(crate) fn invalid_params(message: impl Into<String>) -> Self {
        Self {
            http_status: 400,
            code: ERR_INVALID_PARAMS,
            message: message.into(),
            data: None,
        }
    }

    pub(crate) fn method_not_found(method: &str) -> Self {
        Self {
            http_status: 404,
            code: ERR_METHOD_NOT_FOUND,
            message: format!("Method not found: {method}"),
            data: None,
        }
    }

    /// Constructs an opaque `-32603` internal error carrying only `correlationId` (`INV-15`).
    pub(crate) fn internal_from_gap(err: &GapError) -> Self {
        let cid = err
            .extensions
            .correlation_id
            .clone()
            .unwrap_or_else(|| Uuid::now_v7().to_string());
        Self {
            http_status: 500,
            code: ERR_INTERNAL_ERROR,
            message: "Internal error".to_owned(),
            data: Some(json!({ "correlationId": cid })),
        }
    }

    pub(crate) fn into_jsonrpc_body(self, id: Value) -> Value {
        let mut err_obj = Map::new();
        err_obj.insert("code".to_owned(), json!(self.code));
        err_obj.insert("message".to_owned(), json!(self.message));
        if let Some(d) = self.data {
            err_obj.insert("data".to_owned(), d);
        }
        json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": Value::Object(err_obj),
        })
    }
}

#[derive(Debug)]
struct RpcEnvelope {
    id: Option<Value>,
    method: String,
    params: Map<String, Value>,
}

fn json_http_response(
    status: u16,
    mut extra_headers: Vec<(String, String)>,
    body_val: &Value,
) -> HttpExchangeResponse {
    extra_headers.insert(
        0,
        ("content-type".to_owned(), "application/json".to_owned()),
    );
    let body = serde_json::to_vec(body_val).unwrap_or_else(|_| {
        b"{\"jsonrpc\":\"2.0\",\"id\":null,\"error\":{\"code\":-32603,\"message\":\"Internal error\"}}"
            .to_vec()
    });
    HttpExchangeResponse {
        status,
        headers: extra_headers,
        body,
    }
}

fn error_http_response(
    err: RpcError,
    id: Value,
    extra_headers: Vec<(String, String)>,
) -> HttpExchangeResponse {
    let status = err.http_status;
    let payload = err.into_jsonrpc_body(id);
    json_http_response(status, extra_headers, &payload)
}

fn single_header<'a>(headers: &'a [(String, String)], name: &str) -> Result<Option<&'a str>, ()> {
    let mut found: Option<&'a str> = None;
    for (k, v) in headers {
        if k.eq_ignore_ascii_case(name) {
            if found.is_some() {
                return Err(());
            }
            found = Some(v.as_str());
        }
    }
    Ok(found)
}

fn is_valid_http_field_value(s: &str) -> bool {
    s.bytes().all(|b| b == b'\t' || (0x20..=0x7E).contains(&b))
}

fn is_loopback_origin(origin: &str) -> bool {
    let rest = if let Some(r) = origin.strip_prefix("http://") {
        r
    } else if let Some(r) = origin.strip_prefix("https://") {
        r
    } else {
        return false;
    };
    if rest.is_empty()
        || rest
            .bytes()
            .any(|b| matches!(b, b'/' | b'?' | b'#' | b'@' | b' ' | b'\t') || !b.is_ascii_graphic())
    {
        return false;
    }
    if let Some(after_v6) = rest.strip_prefix("[::1]") {
        if after_v6.is_empty() {
            return true;
        }
        return after_v6
            .strip_prefix(':')
            .and_then(|p| p.parse::<u16>().ok())
            .is_some_and(|port| port > 0);
    }
    let (host, port_opt) = match rest.split_once(':') {
        Some((h, p)) => (h, Some(p)),
        None => (rest, None),
    };
    if let Some(port_str) = port_opt
        && port_str.parse::<u16>().ok().is_none_or(|port| port == 0)
    {
        return false;
    }
    host.eq_ignore_ascii_case("localhost") || host == "127.0.0.1"
}

fn check_origin(headers: &[(String, String)], config: &FacadeConfig) -> Result<(), RpcError> {
    let origin = match single_header(headers, "origin") {
        Ok(Some(v)) => v.trim(),
        Ok(None) => return Ok(()),
        Err(()) => return Err(RpcError::invalid_request(403, "Duplicate Origin header")),
    };
    let allowed = if config.allowed_origins.is_empty() {
        is_loopback_origin(origin)
    } else {
        config.allowed_origins.iter().any(|o| o == origin)
    };
    if allowed {
        Ok(())
    } else {
        Err(RpcError::invalid_request(403, "Origin is not allowed"))
    }
}

fn check_content_type_and_accept(headers: &[(String, String)]) -> Result<(), RpcError> {
    let ct = match single_header(headers, "content-type") {
        Ok(Some(v)) => v,
        Ok(None) | Err(()) => {
            return Err(RpcError::invalid_request(
                415,
                "Content-Type must be application/json",
            ));
        }
    };
    let media_type = ct.split(';').next().unwrap_or("").trim();
    if !media_type.eq_ignore_ascii_case("application/json") {
        return Err(RpcError::invalid_request(
            415,
            "Content-Type must be application/json",
        ));
    }

    let mut has_json = false;
    let mut has_sse = false;
    for (k, v) in headers {
        if k.eq_ignore_ascii_case("accept") {
            for item in v.split(',') {
                let mime = item.split(';').next().unwrap_or("").trim();
                if mime.eq_ignore_ascii_case("application/json") {
                    has_json = true;
                } else if mime.eq_ignore_ascii_case("text/event-stream") {
                    has_sse = true;
                }
            }
        }
    }
    if has_json && has_sse {
        Ok(())
    } else {
        Err(RpcError::invalid_request(
            406,
            "Accept header must list both application/json and text/event-stream",
        ))
    }
}

fn b64_sextet(b: u8) -> Option<u8> {
    match b {
        b'A'..=b'Z' => Some(b - b'A'),
        b'a'..=b'z' => Some(b - b'a' + 26),
        b'0'..=b'9' => Some(b - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    }
}

fn decode_standard_base64(encoded: &str) -> Result<Vec<u8>, ()> {
    let bytes = encoded.as_bytes();
    let (chunks, rem) = bytes.as_chunks::<4>();
    if !rem.is_empty() {
        return Err(());
    }
    let mut out = Vec::with_capacity(chunks.len() * 3);
    let total_chunks = chunks.len();
    for (idx, chunk) in chunks.iter().enumerate() {
        let is_last = idx + 1 == total_chunks;
        let v0 = b64_sextet(chunk[0]).ok_or(())?;
        let v1 = b64_sextet(chunk[1]).ok_or(())?;
        out.push((v0 << 2) | (v1 >> 4));
        if chunk[2] == b'=' {
            if !is_last || chunk[3] != b'=' || (v1 & 0x0F) != 0 {
                return Err(());
            }
        } else {
            let v2 = b64_sextet(chunk[2]).ok_or(())?;
            out.push((v1 << 4) | (v2 >> 2));
            if chunk[3] == b'=' {
                if !is_last || (v2 & 0x03) != 0 {
                    return Err(());
                }
            } else {
                let v3 = b64_sextet(chunk[3]).ok_or(())?;
                out.push((v2 << 6) | v3);
            }
        }
    }
    Ok(out)
}

fn decode_mcp_name_header(raw: &str) -> Result<String, RpcError> {
    if !is_valid_http_field_value(raw) {
        return Err(RpcError::header_mismatch(
            "Mcp-Name contains invalid HTTP header bytes",
        ));
    }
    let trimmed = raw.trim();
    if let Some(rest) = trimmed.strip_prefix("=?base64?") {
        let Some(encoded) = rest.strip_suffix("?=") else {
            return Err(RpcError::header_mismatch(
                "Malformed Mcp-Name base64 sentinel",
            ));
        };
        let bytes = decode_standard_base64(encoded)
            .map_err(|()| RpcError::header_mismatch("Invalid base64 in Mcp-Name sentinel"))?;
        String::from_utf8(bytes)
            .map_err(|_| RpcError::header_mismatch("Decoded Mcp-Name sentinel is not valid UTF-8"))
    } else {
        Ok(trimmed.to_owned())
    }
}

fn parse_rpc_envelope(bytes: &[u8]) -> Result<RpcEnvelope, (RpcError, Value)> {
    let val: Value =
        serde_json::from_slice(bytes).map_err(|_| (RpcError::parse_error(), Value::Null))?;
    let Value::Object(mut obj) = val else {
        let msg = if val.is_array() {
            "JSON-RPC batch requests are not supported"
        } else {
            "JSON-RPC payload must be a JSON object"
        };
        return Err((RpcError::invalid_request(400, msg), Value::Null));
    };

    let id = match obj.remove("id") {
        None => None,
        Some(v @ (Value::String(_) | Value::Number(_))) => Some(v),
        Some(_) => {
            return Err((
                RpcError::invalid_request(400, "JSON-RPC id must be a string or number"),
                Value::Null,
            ));
        }
    };
    let err_id = id.clone().unwrap_or(Value::Null);

    if obj.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return Err((
            RpcError::invalid_request(400, "jsonrpc field must equal \"2.0\""),
            err_id,
        ));
    }

    let Some(Value::String(method)) = obj.remove("method") else {
        return Err((
            RpcError::invalid_request(400, "JSON-RPC method must be a non-empty string"),
            err_id,
        ));
    };
    if method.is_empty() {
        return Err((
            RpcError::invalid_request(400, "JSON-RPC method must not be empty"),
            err_id,
        ));
    }

    let params = match obj.remove("params") {
        None => Map::new(),
        Some(Value::Object(map)) => map,
        Some(_) => {
            return Err((
                RpcError::invalid_params("JSON-RPC params must be an object"),
                err_id,
            ));
        }
    };

    Ok(RpcEnvelope { id, method, params })
}

pub(crate) fn server_info_meta(config: &FacadeConfig) -> Value {
    json!({
        META_SERVER_INFO: {
            "name": config.server_name,
            "version": config.server_version,
        }
    })
}

pub(crate) fn server_capabilities_json() -> Value {
    json!({
        "tools": { "listChanged": false },
        "resources": { "subscribe": false, "listChanged": false }
    })
}

fn build_initialize_result(
    config: &FacadeConfig,
    params: &Map<String, Value>,
) -> Result<Value, RpcError> {
    let Some(ver) = params.get("protocolVersion").and_then(Value::as_str) else {
        return Err(RpcError::invalid_params(
            "missing string params.protocolVersion",
        ));
    };
    if ver != VERSION_LEGACY {
        return Err(RpcError::unsupported_version(ver));
    }
    Ok(json!({
        "protocolVersion": VERSION_LEGACY,
        "capabilities": server_capabilities_json(),
        "serverInfo": {
            "name": config.server_name,
            "version": config.server_version,
        }
    }))
}

fn validate_modern_meta(
    params: &Map<String, Value>,
    http_protocol_header: Option<&str>,
) -> Result<(), RpcError> {
    let Some(meta) = params.get("_meta").and_then(Value::as_object) else {
        return Err(RpcError::invalid_params("missing object params._meta"));
    };
    let Some(body_ver) = meta.get(META_PROTOCOL_VERSION).and_then(Value::as_str) else {
        return Err(RpcError::invalid_params(
            "missing string params._meta[\"io.modelcontextprotocol/protocolVersion\"]",
        ));
    };
    if let Some(hdr_ver) = http_protocol_header
        && hdr_ver != body_ver
    {
        return Err(RpcError::header_mismatch(
            "MCP-Protocol-Version header does not match params._meta protocolVersion",
        ));
    }
    if body_ver != VERSION_MODERN {
        return Err(RpcError::unsupported_version(body_ver));
    }
    if meta
        .get(META_CLIENT_CAPABILITIES)
        .and_then(Value::as_object)
        .is_none()
    {
        return Err(RpcError::invalid_params(
            "missing object params._meta[\"io.modelcontextprotocol/clientCapabilities\"]",
        ));
    }
    Ok(())
}

fn validate_modern_http_headers(
    headers: &[(String, String)],
    body_method: &str,
    params: &Map<String, Value>,
) -> Result<String, RpcError> {
    let mcp_method = match single_header(headers, "mcp-method") {
        Ok(Some(v)) if is_valid_http_field_value(v) => v.trim(),
        _ => {
            return Err(RpcError::header_mismatch(
                "Missing or invalid Mcp-Method header",
            ));
        }
    };
    // Body method is ALWAYS the source of truth; header is only compared against it (`CMP-001`).
    if mcp_method != body_method {
        return Err(RpcError::header_mismatch(
            "Mcp-Method header does not match JSON-RPC method",
        ));
    }

    if body_method == "tools/call" || body_method == "resources/read" {
        let raw_mcp_name = match single_header(headers, "mcp-name") {
            Ok(Some(v)) => v,
            _ => {
                return Err(RpcError::header_mismatch(
                    "Missing or duplicate Mcp-Name header",
                ));
            }
        };
        let decoded_name = decode_mcp_name_header(raw_mcp_name)?;
        let target_key = if body_method == "tools/call" {
            "name"
        } else {
            "uri"
        };
        let Some(body_target) = params.get(target_key).and_then(Value::as_str) else {
            return Err(RpcError::invalid_params(format!(
                "missing string params.{target_key}"
            )));
        };
        if decoded_name != body_target {
            return Err(RpcError::header_mismatch(
                "Mcp-Name header does not match JSON-RPC target",
            ));
        }
    }

    let proto_ver = match single_header(headers, "mcp-protocol-version") {
        Ok(Some(v)) if is_valid_http_field_value(v) && !v.trim().is_empty() => v.trim().to_owned(),
        _ => {
            return Err(RpcError::header_mismatch(
                "Missing or invalid MCP-Protocol-Version header",
            ));
        }
    };
    Ok(proto_ver)
}

async fn dispatch_rpc(
    inner: &FacadeInner,
    ctx: &RequestContext,
    era: WireEra,
    body_method: &str,
    params: &Map<String, Value>,
) -> Result<Value, RpcError> {
    match body_method {
        "server/discover" if era == WireEra::Modern => Ok(json!({
            "resultType": "complete",
            "supportedVersions": SUPPORTED_VERSIONS,
            "capabilities": server_capabilities_json(),
            "ttlMs": PUBLIC_TTL_MS,
            "cacheScope": "public",
            "_meta": server_info_meta(&inner.config),
        })),
        "ping" => match era {
            WireEra::Modern => Ok(json!({
                "resultType": "complete",
                "_meta": server_info_meta(&inner.config),
            })),
            WireEra::Legacy => Ok(json!({})),
        },
        "tools/list" => Ok(tools::list_tools(inner, era)),
        "tools/call" => tools::call_tool(inner, ctx, era, params).await,
        "resources/list" => Ok(resources::list_resources(inner, era)),
        "resources/templates/list" => Ok(resources::list_resource_templates(inner, era)),
        "resources/read" => resources::read_resource(inner, ctx, era, params).await,
        _ => Err(RpcError::method_not_found(body_method)),
    }
}

/// Handles a single Streamable HTTP exchange (`POST /mcp` or legacy `DELETE /mcp`).
pub(crate) async fn handle_http(
    inner: &FacadeInner,
    ctx: &RequestContext,
    req: HttpExchangeRequest,
) -> GapResult<HttpExchangeResponse> {
    if let Err(err) = check_origin(&req.headers, &inner.config) {
        return Ok(error_http_response(err, Value::Null, Vec::new()));
    }

    if req.method.eq_ignore_ascii_case("DELETE") {
        match single_header(&req.headers, "mcp-session-id") {
            Ok(Some(sid)) if inner.sessions.remove(sid.trim()) => {
                return Ok(HttpExchangeResponse {
                    status: 200,
                    headers: Vec::new(),
                    body: Vec::new(),
                });
            }
            Err(()) => {
                return Ok(error_http_response(
                    RpcError::invalid_request(400, "Duplicate MCP-Session-Id header"),
                    Value::Null,
                    Vec::new(),
                ));
            }
            Ok(_) => {}
        }
        return Ok(error_http_response(
            RpcError::invalid_request(405, "Method not allowed"),
            Value::Null,
            vec![("allow".to_owned(), "POST".to_owned())],
        ));
    }

    if !req.method.eq_ignore_ascii_case("POST") {
        return Ok(error_http_response(
            RpcError::invalid_request(405, "Method not allowed"),
            Value::Null,
            vec![("allow".to_owned(), "POST".to_owned())],
        ));
    }

    if req.body.len() > inner.config.max_request_bytes {
        return Ok(error_http_response(
            RpcError::invalid_request(413, "Request payload exceeds max_request_bytes"),
            Value::Null,
            Vec::new(),
        ));
    }

    if let Err(err) = check_content_type_and_accept(&req.headers) {
        return Ok(error_http_response(err, Value::Null, Vec::new()));
    }

    let envelope = match parse_rpc_envelope(&req.body) {
        Ok(env) => env,
        Err((err, id)) => return Ok(error_http_response(err, id, Vec::new())),
    };

    let has_modern_meta = envelope
        .params
        .get("_meta")
        .and_then(Value::as_object)
        .and_then(|m| m.get(META_PROTOCOL_VERSION))
        .is_some();
    let has_modern_header = matches!(
        single_header(&req.headers, "mcp-protocol-version"),
        Ok(Some(v)) if v.trim() == VERSION_MODERN
    );

    // 1. Legacy 2025-11-25 initialize handshake
    if envelope.method == "initialize" && !has_modern_meta && !has_modern_header {
        let Some(req_id) = envelope.id else {
            return Ok(error_http_response(
                RpcError::invalid_request(400, "initialize must carry a request id"),
                Value::Null,
                Vec::new(),
            ));
        };
        return match build_initialize_result(&inner.config, &envelope.params) {
            Ok(result) => {
                let sid = inner.sessions.mint();
                Ok(json_http_response(
                    200,
                    vec![("mcp-session-id".to_owned(), sid)],
                    &json!({
                        "jsonrpc": "2.0",
                        "id": req_id,
                        "result": result,
                    }),
                ))
            }
            Err(err) => Ok(error_http_response(err, req_id, Vec::new())),
        };
    }

    // 2. Legacy 2025-11-25 subsequent session request (carrying MCP-Session-Id without modern 2026-07-28 markers)
    if !has_modern_meta && !has_modern_header {
        let err_id = envelope.id.clone().unwrap_or(Value::Null);
        match single_header(&req.headers, "mcp-session-id") {
            Err(()) => {
                return Ok(error_http_response(
                    RpcError::invalid_request(400, "Duplicate MCP-Session-Id header"),
                    err_id,
                    Vec::new(),
                ));
            }
            Ok(Some(sid)) => {
                match single_header(&req.headers, "mcp-protocol-version") {
                    Err(()) => {
                        return Ok(error_http_response(
                            RpcError::invalid_request(400, "Duplicate MCP-Protocol-Version header"),
                            err_id,
                            Vec::new(),
                        ));
                    }
                    Ok(Some(ver)) if ver.trim() != VERSION_LEGACY => {
                        return Ok(error_http_response(
                            RpcError::unsupported_version(ver.trim()),
                            err_id,
                            Vec::new(),
                        ));
                    }
                    Ok(_) => {}
                }
                if !inner.sessions.touch(sid.trim()) {
                    return Ok(error_http_response(
                        RpcError::invalid_request(404, "Unknown or expired MCP-Session-Id"),
                        err_id,
                        Vec::new(),
                    ));
                }
                let Some(req_id) = envelope.id else {
                    if envelope.method.starts_with("notifications/") {
                        return Ok(HttpExchangeResponse {
                            status: 202,
                            headers: Vec::new(),
                            body: Vec::new(),
                        });
                    }
                    return Ok(error_http_response(
                        RpcError::invalid_request(400, "Missing request id"),
                        Value::Null,
                        Vec::new(),
                    ));
                };
                return match dispatch_rpc(
                    inner,
                    ctx,
                    WireEra::Legacy,
                    &envelope.method,
                    &envelope.params,
                )
                .await
                {
                    Ok(result) => Ok(json_http_response(
                        200,
                        Vec::new(),
                        &json!({
                            "jsonrpc": "2.0",
                            "id": req_id,
                            "result": result,
                        }),
                    )),
                    Err(err) => Ok(error_http_response(err, req_id, Vec::new())),
                };
            }
            Ok(None) => {
                if matches!(
                    single_header(&req.headers, "mcp-protocol-version"),
                    Ok(Some(ver)) if ver.trim() == VERSION_LEGACY
                ) {
                    return Ok(error_http_response(
                        RpcError::invalid_request(400, "Missing MCP-Session-Id header"),
                        err_id,
                        Vec::new(),
                    ));
                }
            }
        }
    }

    // 3. Stateless 2026-07-28 HTTP handling
    let err_id = envelope.id.clone().unwrap_or(Value::Null);
    let hdr_proto =
        match validate_modern_http_headers(&req.headers, &envelope.method, &envelope.params) {
            Ok(v) => v,
            Err(err) => return Ok(error_http_response(err, err_id, Vec::new())),
        };
    if let Err(err) = validate_modern_meta(&envelope.params, Some(hdr_proto.as_str())) {
        return Ok(error_http_response(err, err_id, Vec::new()));
    }

    let Some(req_id) = envelope.id else {
        if envelope.method.starts_with("notifications/") {
            return Ok(HttpExchangeResponse {
                status: 202,
                headers: Vec::new(),
                body: Vec::new(),
            });
        }
        return Ok(error_http_response(
            RpcError::invalid_request(400, "Missing request id"),
            Value::Null,
            Vec::new(),
        ));
    };

    match dispatch_rpc(
        inner,
        ctx,
        WireEra::Modern,
        &envelope.method,
        &envelope.params,
    )
    .await
    {
        Ok(result) => Ok(json_http_response(
            200,
            Vec::new(),
            &json!({
                "jsonrpc": "2.0",
                "id": req_id,
                "result": result,
            }),
        )),
        Err(err) => Ok(error_http_response(err, req_id, Vec::new())),
    }
}

async fn write_stdio_json<W: AsyncWrite + Unpin>(writer: &mut W, payload: &Value) -> GapResult<()> {
    let mut bytes = serde_json::to_vec(payload).map_err(|e| {
        GapError::new(
            gap_core::error::ErrorCode::Internal,
            format!("stdio response serialization failed: {e}"),
        )
    })?;
    bytes.push(b'\n');
    writer.write_all(&bytes).await.map_err(|e| {
        GapError::new(
            gap_core::error::ErrorCode::Internal,
            format!("stdio write failed: {e}"),
        )
    })?;
    writer.flush().await.map_err(|e| {
        GapError::new(
            gap_core::error::ErrorCode::Internal,
            format!("stdio flush failed: {e}"),
        )
    })
}

async fn handle_stdio_line(
    inner: &FacadeInner,
    ctx_factory: &ContextFactory,
    legacy_active: &mut bool,
    raw_line: &[u8],
) -> Option<Value> {
    let envelope = match parse_rpc_envelope(raw_line) {
        Ok(env) => env,
        Err((err, id)) => return Some(err.into_jsonrpc_body(id)),
    };

    let has_meta = envelope.params.contains_key("_meta");

    if envelope.method == "initialize" && !has_meta {
        let Some(req_id) = envelope.id else {
            return Some(
                RpcError::invalid_request(400, "initialize must carry a request id")
                    .into_jsonrpc_body(Value::Null),
            );
        };
        return match build_initialize_result(&inner.config, &envelope.params) {
            Ok(result) => {
                *legacy_active = true;
                Some(json!({
                    "jsonrpc": "2.0",
                    "id": req_id,
                    "result": result,
                }))
            }
            Err(err) => Some(err.into_jsonrpc_body(req_id)),
        };
    }

    if envelope.id.is_none() {
        if envelope.method.starts_with("notifications/") {
            return None;
        }
        return Some(
            RpcError::invalid_request(400, "Missing request id").into_jsonrpc_body(Value::Null),
        );
    }
    let req_id = envelope.id.unwrap_or(Value::Null);

    let era = if !has_meta && *legacy_active {
        WireEra::Legacy
    } else {
        if let Err(err) = validate_modern_meta(&envelope.params, None) {
            return Some(err.into_jsonrpc_body(req_id));
        }
        WireEra::Modern
    };

    let ctx = match (ctx_factory)() {
        Ok(c) => c,
        Err(gap_err) => {
            return Some(RpcError::internal_from_gap(&gap_err).into_jsonrpc_body(req_id));
        }
    };

    match dispatch_rpc(inner, &ctx, era, &envelope.method, &envelope.params).await {
        Ok(result) => Some(json!({
            "jsonrpc": "2.0",
            "id": req_id,
            "result": result,
        })),
        Err(err) => Some(err.into_jsonrpc_body(req_id)),
    }
}

/// Serves bounded newline-delimited JSON-RPC frames over `stdio` until EOF (`CMP-001`, `INV-01`).
pub(crate) async fn serve_stdio<R, W>(
    inner: &FacadeInner,
    ctx_factory: &ContextFactory,
    mut reader: R,
    mut writer: W,
) -> GapResult<()>
where
    R: AsyncBufRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let max_bytes = inner.config.max_request_bytes;
    let mut line_buf: Vec<u8> = Vec::with_capacity(max_bytes.min(4096));
    let mut oversize = false;
    let mut legacy_active = false;

    loop {
        let buf = reader.fill_buf().await.map_err(|e| {
            GapError::new(
                gap_core::error::ErrorCode::Internal,
                format!("stdio read failed: {e}"),
            )
        })?;

        if buf.is_empty() {
            if oversize {
                let err_payload =
                    RpcError::invalid_request(413, "Request frame exceeds max_request_bytes")
                        .into_jsonrpc_body(Value::Null);
                write_stdio_json(&mut writer, &err_payload).await?;
            } else if !line_buf.iter().all(u8::is_ascii_whitespace) {
                if line_buf.last() == Some(&b'\r') {
                    line_buf.pop();
                }
                if let Some(reply) =
                    handle_stdio_line(inner, ctx_factory, &mut legacy_active, &line_buf).await
                {
                    write_stdio_json(&mut writer, &reply).await?;
                }
            }
            break;
        }

        if let Some(nl_pos) = buf.iter().position(|&b| b == b'\n') {
            let chunk = &buf[..nl_pos];
            let was_oversize = oversize || line_buf.len().saturating_add(chunk.len()) > max_bytes;
            if !was_oversize {
                line_buf.extend_from_slice(chunk);
            }
            reader.consume(nl_pos + 1);

            if was_oversize {
                line_buf.clear();
                oversize = false;
                let err_payload =
                    RpcError::invalid_request(413, "Request frame exceeds max_request_bytes")
                        .into_jsonrpc_body(Value::Null);
                write_stdio_json(&mut writer, &err_payload).await?;
                continue;
            }

            if line_buf.last() == Some(&b'\r') {
                line_buf.pop();
            }
            if !line_buf.iter().all(u8::is_ascii_whitespace)
                && let Some(reply) =
                    handle_stdio_line(inner, ctx_factory, &mut legacy_active, &line_buf).await
            {
                write_stdio_json(&mut writer, &reply).await?;
            }
            line_buf.clear();
            oversize = false;
        } else {
            let n = buf.len();
            if !oversize {
                if line_buf.len().saturating_add(n) <= max_bytes {
                    line_buf.extend_from_slice(buf);
                } else {
                    line_buf.clear();
                    oversize = true;
                }
            }
            reader.consume(n);
        }
    }

    Ok(())
}
