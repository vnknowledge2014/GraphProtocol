//! Bound, expiring pagination cursors (`NGC-022`).

use std::collections::BTreeMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use gap_core::error::{NgcErrorCode, NgcErrorInfo};
use gap_proto::ngc::v1::{Expr, QueryRequest, expr};
use serde::{Deserialize, Serialize};

const CURSOR_VERSION: u8 = 1;

/// Default cursor lifetime (`>= 10 min` per `NGC-022`).
pub const CURSOR_TTL: Duration = Duration::from_secs(15 * 60);

#[derive(Debug, Serialize, Deserialize)]
struct CursorToken {
    v: u8,
    op: String,
    args_hash: String,
    tenant: String,
    offset: usize,
    expires_at_unix_ms: u64,
}

fn invalid(message: impl Into<String>) -> NgcErrorInfo {
    NgcErrorInfo::new(NgcErrorCode::InvalidArgument, message)
}

fn now_unix_ms() -> u64 {
    u64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis(),
    )
    .unwrap_or(u64::MAX)
}

fn encode_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        out.push(char::from(HEX[usize::from(byte >> 4)]));
        out.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    out
}

fn decode_hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn decode_hex(hex: &str) -> Option<Vec<u8>> {
    if hex.is_empty() || !hex.len().is_multiple_of(2) {
        return None;
    }
    let bytes = hex.as_bytes();
    let mut out = Vec::with_capacity(bytes.len() / 2);
    let mut idx = 0;
    while idx < bytes.len() {
        let hi = decode_hex_nibble(bytes[idx])?;
        let lo = decode_hex_nibble(bytes[idx + 1])?;
        out.push((hi << 4) | lo);
        idx += 2;
    }
    Some(out)
}

fn canonical_prost_value(value: &prost_types::Value) -> String {
    use prost_types::value::Kind;
    match &value.kind {
        None | Some(Kind::NullValue(_)) => "null".to_owned(),
        Some(Kind::BoolValue(b)) => b.to_string(),
        Some(Kind::NumberValue(n)) => {
            let normalized = if *n == 0.0 { 0.0 } else { *n };
            format!("{normalized}")
        }
        Some(Kind::StringValue(s)) => serde_json::to_string(s).unwrap_or_else(|_| s.clone()),
        Some(Kind::ListValue(list)) => {
            let items: Vec<String> = list.values.iter().map(canonical_prost_value).collect();
            format!("[{}]", items.join(","))
        }
        Some(Kind::StructValue(st)) => canonical_prost_struct(st),
    }
}

fn canonical_prost_struct(st: &prost_types::Struct) -> String {
    let entries: BTreeMap<&str, String> = st
        .fields
        .iter()
        .map(|(k, v)| (k.as_str(), canonical_prost_value(v)))
        .collect();
    let rendered: Vec<String> = entries
        .into_iter()
        .map(|(k, v)| {
            let key = serde_json::to_string(k).unwrap_or_else(|_| k.to_owned());
            format!("{key}:{v}")
        })
        .collect();
    format!("{{{}}}", rendered.join(","))
}

fn canonical_predicate(predicate: &Expr) -> String {
    match &predicate.node {
        None => "none".to_owned(),
        Some(expr::Node::Exists(path)) => format!("exists({path})"),
        Some(expr::Node::Not(child)) => format!("not({})", canonical_predicate(child)),
        Some(expr::Node::And(list)) => {
            let items: Vec<String> = list.exprs.iter().map(canonical_predicate).collect();
            format!("and([{}])", items.join(","))
        }
        Some(expr::Node::Or(list)) => {
            let items: Vec<String> = list.exprs.iter().map(canonical_predicate).collect();
            format!("or([{}])", items.join(","))
        }
        Some(expr::Node::Cmp(cmp)) => format!(
            "cmp({},{},{})",
            cmp.path,
            cmp.op,
            cmp.value
                .as_ref()
                .map_or_else(|| "none".to_owned(), canonical_prost_value)
        ),
    }
}

/// Computes a deterministic hash binding a cursor to the query arguments, predicate, order, and selection.
#[must_use]
pub fn query_hash(request: &QueryRequest) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(request.operation.as_bytes());
    hasher.update(b"\x00");
    if let Some(arguments) = &request.arguments {
        hasher.update(canonical_prost_struct(arguments).as_bytes());
    }
    hasher.update(b"\x00");
    if let Some(predicate) = &request.predicate {
        hasher.update(canonical_predicate(predicate).as_bytes());
    }
    hasher.update(b"\x00");
    for ob in &request.order_by {
        hasher.update(ob.path.as_bytes());
        hasher.update(&[u8::from(ob.descending)]);
        hasher.update(b"\x00");
    }
    if let Some(selection) = &request.selection {
        let mut fields: Vec<&str> = selection.fields.iter().map(|f| f.name.as_str()).collect();
        fields.sort_unstable();
        for f in fields {
            hasher.update(f.as_bytes());
            hasher.update(b"\x00");
        }
    }
    hasher.finalize().to_hex().to_string()
}

/// Encodes a cursor with the default 15-minute TTL (`>= 10 min`, `NGC-022`).
pub fn encode(
    op: &str,
    args_hash: &str,
    tenant: &str,
    offset: usize,
) -> Result<String, NgcErrorInfo> {
    encode_with_ttl(op, args_hash, tenant, offset, CURSOR_TTL)
}

/// Encodes a cursor with a custom TTL (intended for expiry tests).
pub fn encode_with_ttl(
    op: &str,
    args_hash: &str,
    tenant: &str,
    offset: usize,
    ttl: Duration,
) -> Result<String, NgcErrorInfo> {
    let ttl_ms = u64::try_from(ttl.as_millis()).unwrap_or(u64::MAX);
    let token = CursorToken {
        v: CURSOR_VERSION,
        op: op.to_owned(),
        args_hash: args_hash.to_owned(),
        tenant: tenant.to_owned(),
        offset,
        expires_at_unix_ms: now_unix_ms().saturating_add(ttl_ms),
    };
    let encoded = serde_json::to_vec(&token)
        .map_err(|_| NgcErrorInfo::new(NgcErrorCode::Internal, "failed to serialize cursor"))?;
    Ok(encode_hex(&encoded))
}

/// Decodes and validates a cursor against expected operation, query hash, tenant, and expiry.
pub fn decode(
    expected_op: &str,
    expected_args_hash: &str,
    expected_tenant: &str,
    token: &str,
) -> Result<usize, NgcErrorInfo> {
    let raw = decode_hex(token).ok_or_else(|| invalid("invalid pagination cursor encoding"))?;
    let parsed: CursorToken =
        serde_json::from_slice(&raw).map_err(|_| invalid("malformed pagination cursor payload"))?;
    if parsed.v != CURSOR_VERSION {
        return Err(invalid("unsupported pagination cursor version"));
    }
    if parsed.op != expected_op
        || parsed.args_hash != expected_args_hash
        || parsed.tenant != expected_tenant
    {
        return Err(invalid(
            "pagination cursor does not match operation, arguments, or tenant",
        ));
    }
    if now_unix_ms() > parsed.expires_at_unix_ms {
        return Err(invalid("pagination cursor has expired"));
    }
    Ok(parsed.offset)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gap_proto::ngc::v1::{Cmp, PredicateOp};

    fn sample_request() -> QueryRequest {
        QueryRequest {
            operation: "list_customers".to_owned(),
            predicate: Some(Expr {
                node: Some(expr::Node::Cmp(Cmp {
                    path: "tier".to_owned(),
                    op: PredicateOp::PEq as i32,
                    value: Some(prost_types::Value {
                        kind: Some(prost_types::value::Kind::StringValue("gold".to_owned())),
                    }),
                })),
            }),
            ..Default::default()
        }
    }

    // spec: NGC-022
    #[test]
    fn cursor_roundtrips_and_rejects_mismatched_scope_or_expiry() -> Result<(), NgcErrorInfo> {
        let hash = query_hash(&sample_request());
        let token = encode("list_customers", &hash, "tenant-a", 12)?;
        assert_eq!(decode("list_customers", &hash, "tenant-a", &token)?, 12);
        assert!(decode("other_op", &hash, "tenant-a", &token).is_err());
        assert!(decode("list_customers", "different-hash", "tenant-a", &token).is_err());
        assert!(decode("list_customers", &hash, "tenant-b", &token).is_err());
        assert!(decode("list_customers", &hash, "tenant-a", "not-hex").is_err());

        let expired = encode_with_ttl("list_customers", &hash, "tenant-a", 5, Duration::ZERO)?;
        std::thread::sleep(Duration::from_millis(2));
        assert!(decode("list_customers", &hash, "tenant-a", &expired).is_err());
        Ok(())
    }
}
