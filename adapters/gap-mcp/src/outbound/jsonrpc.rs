//! JSON-RPC 2.0 framing for MCP (`CMP-010`): strict, size-capped, panic-free decoding.

use serde_json::{Map, Value, json};

use super::error::{MAX_SERVER_TEXT_CHARS, McpError, sanitize_text};

/// Default cap on one frame (a line on stdio, an event or a body on HTTP): 4 MiB.
pub const DEFAULT_MAX_FRAME_BYTES: usize = 4 * 1024 * 1024;

/// A message received from the server.
#[derive(Debug, Clone, PartialEq)]
pub enum Incoming {
    /// Reply to one of our requests.
    Response {
        /// Id of the request this answers.
        id: Value,
        /// `result` or the sanitised error object.
        outcome: Result<Value, McpError>,
    },
    /// Request initiated by the server (sampling, roots, ping in the legacy era).
    Request {
        /// Server-chosen id to echo.
        id: Value,
        /// Method name.
        method: String,
    },
    /// Notification (no id).
    Notification {
        /// Method name.
        method: String,
    },
}

/// Encodes a request frame (one line, no trailing newline).
#[must_use]
pub fn encode_request(id: u64, method: &str, params: &Value) -> String {
    json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}).to_string()
}

/// Encodes a notification frame.
#[must_use]
pub fn encode_notification(method: &str, params: &Value) -> String {
    json!({"jsonrpc": "2.0", "method": method, "params": params}).to_string()
}

/// Encodes an error reply to a server-initiated request.
#[must_use]
pub fn encode_error_reply(id: &Value, code: i64, message: &str) -> String {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}}).to_string()
}

/// Encodes an empty-result reply (used for the legacy `ping`).
#[must_use]
pub fn encode_empty_reply(id: &Value) -> String {
    json!({"jsonrpc": "2.0", "id": id, "result": {}}).to_string()
}

/// Decodes one frame. Never panics; rejects oversized, non-object, non-2.0 and ambiguous frames.
pub fn decode_frame(bytes: &[u8], max_bytes: usize) -> Result<Incoming, McpError> {
    if bytes.len() > max_bytes {
        return Err(McpError::FrameTooLarge);
    }
    let value: Value =
        serde_json::from_slice(bytes).map_err(|_| McpError::Protocol("frame is not valid JSON"))?;
    let Value::Object(mut obj) = value else {
        return Err(McpError::Protocol("frame is not a JSON object"));
    };
    if obj.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return Err(McpError::Protocol("frame is not JSON-RPC 2.0"));
    }
    let id = obj.remove("id");
    let method = obj.remove("method");
    match (id, method) {
        (Some(id), Some(Value::String(method))) => {
            check_id(&id)?;
            Ok(Incoming::Request { id, method })
        }
        (None, Some(Value::String(method))) => Ok(Incoming::Notification { method }),
        (Some(id), None) => {
            check_id(&id)?;
            decode_response(id, &mut obj)
        }
        _ => Err(McpError::Protocol("frame has neither id nor method")),
    }
}

fn check_id(id: &Value) -> Result<(), McpError> {
    match id {
        Value::Number(_) | Value::String(_) => Ok(()),
        _ => Err(McpError::Protocol("id is not a number or a string")),
    }
}

fn decode_response(id: Value, obj: &mut Map<String, Value>) -> Result<Incoming, McpError> {
    match (obj.remove("result"), obj.remove("error")) {
        (Some(result), None) => Ok(Incoming::Response {
            id,
            outcome: Ok(result),
        }),
        (None, Some(Value::Object(err))) => {
            let code = err.get("code").and_then(Value::as_i64).unwrap_or(-32603);
            let message = err
                .get("message")
                .and_then(Value::as_str)
                .map(|m| sanitize_text(m, MAX_SERVER_TEXT_CHARS))
                .unwrap_or_default();
            Ok(Incoming::Response {
                id,
                outcome: Err(McpError::Rpc { code, message }),
            })
        }
        _ => Err(McpError::Protocol(
            "response must carry exactly one of result and error",
        )),
    }
}

/// Returns the numeric id of a reply, if it is one of ours (we only allocate numeric ids).
#[must_use]
pub fn numeric_id(id: &Value) -> Option<u64> {
    id.as_u64()
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    // spec: CMP-010
    #[test]
    fn decodes_responses_errors_requests_and_notifications() {
        let ok = decode_frame(br#"{"jsonrpc":"2.0","id":7,"result":{"a":1}}"#, 1024).expect("ok");
        assert_eq!(
            ok,
            Incoming::Response {
                id: json!(7),
                outcome: Ok(json!({"a": 1}))
            }
        );
        let err = decode_frame(
            br#"{"jsonrpc":"2.0","id":8,"error":{"code":-32601,"message":"nope\u0000\n"}}"#,
            1024,
        )
        .expect("err");
        assert_eq!(
            err,
            Incoming::Response {
                id: json!(8),
                outcome: Err(McpError::Rpc {
                    code: -32601,
                    message: "nope".to_owned()
                })
            }
        );
        let req = decode_frame(
            br#"{"jsonrpc":"2.0","id":"s","method":"sampling/createMessage","params":{}}"#,
            1024,
        )
        .expect("req");
        assert_eq!(
            req,
            Incoming::Request {
                id: json!("s"),
                method: "sampling/createMessage".to_owned()
            }
        );
        let note = decode_frame(
            br#"{"jsonrpc":"2.0","method":"notifications/message"}"#,
            1024,
        )
        .expect("note");
        assert_eq!(
            note,
            Incoming::Notification {
                method: "notifications/message".to_owned()
            }
        );
    }

    // spec: CMP-010
    #[test]
    fn rejects_malformed_oversized_and_ambiguous_frames() {
        for bad in [
            &b"not json"[..],
            b"[]",
            b"42",
            br#"{"id":1,"result":{}}"#,
            br#"{"jsonrpc":"1.0","id":1,"result":{}}"#,
            br#"{"jsonrpc":"2.0","id":1}"#,
            br#"{"jsonrpc":"2.0","id":1,"result":{},"error":{"code":1,"message":"x"}}"#,
            br#"{"jsonrpc":"2.0","id":true,"result":{}}"#,
            br#"{"jsonrpc":"2.0"}"#,
        ] {
            assert!(
                decode_frame(bad, 1024).is_err(),
                "{}",
                String::from_utf8_lossy(bad)
            );
        }
        let big = format!(
            r#"{{"jsonrpc":"2.0","id":1,"result":"{}"}}"#,
            "x".repeat(2_000)
        );
        assert_eq!(
            decode_frame(big.as_bytes(), 1_000),
            Err(McpError::FrameTooLarge)
        );
        assert!(decode_frame(big.as_bytes(), 10_000).is_ok());
    }

    // spec: CMP-010
    #[test]
    fn encoders_produce_single_line_json_rpc() {
        let line = encode_request(3, "tools/list", &json!({"cursor": "a\nb"}));
        assert!(!line.contains('\n'));
        let v: Value = serde_json::from_str(&line).expect("json");
        assert_eq!(v["id"], json!(3));
        assert_eq!(v["jsonrpc"], json!("2.0"));
        assert!(!encode_notification("notifications/initialized", &json!({})).contains('\n'));
    }

    proptest! {
        // spec: CMP-010 (never panics on any byte sequence)
        #[test]
        fn decoding_never_panics(bytes in proptest::collection::vec(any::<u8>(), 0..512)) {
            let _ = decode_frame(&bytes, 4096);
        }
    }
}
