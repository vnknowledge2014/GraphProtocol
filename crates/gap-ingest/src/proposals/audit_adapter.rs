//! Minimal RFC 8259 JSON parser into [`CisgValue`] and a persistent [`AuditLog`] adapter backed
//! by [`ContextStore`] (`WP-M4-07`; `INV-14`, `SEC-090`).

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use gap_core::clock::Clock;
use gap_core::ids::{TenantId, TenantScope};
use gap_ql::sha256_bytes;
use gap_store::audit::{AuditChain, AuditEntry, AuditLog, AuditRecord, AuditVerifyReport};
use gap_store::error::StoreError;
use gap_store::store::ContextStore;
use gap_store::types::{Caller, NewNode, NodeId, NodeType, Provenance, ScopeSpec};

use crate::cisg::{CisgValue, DEFAULT_EXTRACT_MAX_DEPTH};
use crate::proposals::types::ProposalError;

const AUDIT_PROVENANCE_SOURCE: &str = "gap-ingest.proposals.audit";
const AUDIT_WRITER_PRINCIPAL: &str = "gap-ingest-audit";
const AUDIT_GENESIS_HASH: [u8; 32] = [0u8; 32];

/// Interim hash-chained [`AuditLog`] backed by a tenant-isolated [`ContextStore`] (`INV-14`, `SEC-090`).
///
/// Each chain maintains a current head pointer node (`audit_head:<chain>`) and sequential record
/// nodes (`audit_rec:<chain>:<seq>`). Uses SHA-256 chain linking until `WP-M6-11` supplies the
/// dedicated BLAKE3 audit backend; any [`AuditLog`] implementation may be injected into
/// [`crate::proposals::ProposalService`].
#[derive(Clone)]
pub struct ContextStoreAuditLog {
    store: Arc<dyn ContextStore>,
    clock: Arc<dyn Clock>,
    system_scope: TenantScope,
    append_guard: Arc<Mutex<()>>,
}

impl std::fmt::Debug for ContextStoreAuditLog {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ContextStoreAuditLog")
            .field("system_tenant", &self.system_scope.tenant().as_str())
            .finish_non_exhaustive()
    }
}

impl ContextStoreAuditLog {
    /// Creates a new store-backed [`ContextStoreAuditLog`].
    pub fn new(store: Arc<dyn ContextStore>, clock: Arc<dyn Clock>) -> Result<Self, ProposalError> {
        let system_tenant = TenantId::new("system")
            .map_err(|e| ProposalError::Validation(format!("invalid system tenant: {e}")))?;
        Ok(Self {
            store,
            clock,
            system_scope: system_tenant.scope(),
            append_guard: Arc::new(Mutex::new(())),
        })
    }

    fn resolve_chain<'a>(&'a self, chain: &'a AuditChain) -> (&'a TenantScope, String) {
        match chain {
            AuditChain::Tenant(scope) => (scope, format!("tenant:{}", scope.tenant().as_str())),
            AuditChain::System => (&self.system_scope, "system".to_owned()),
        }
    }

    fn writer_caller() -> Caller {
        Caller::new(AUDIT_WRITER_PRINCIPAL)
    }

    fn head_node_id(chain_key: &str) -> Result<NodeId, StoreError> {
        NodeId::new(format!("audit_head:{chain_key}"))
    }

    fn record_node_id(chain_key: &str, seq: u64) -> Result<NodeId, StoreError> {
        NodeId::new(format!("audit_rec:{chain_key}:{seq:020}"))
    }
}

impl AuditLog for ContextStoreAuditLog {
    fn append_chain(
        &self,
        chain: &AuditChain,
        entry: &AuditEntry,
    ) -> Result<AuditRecord, StoreError> {
        let _guard = self
            .append_guard
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let (scope, chain_key) = self.resolve_chain(chain);
        let caller = Self::writer_caller();
        let head_id = Self::head_node_id(&chain_key)?;

        let (prev_seq, prev_hash) = match self.chain_head(chain)? {
            Some(pair) => pair,
            None => (0, AUDIT_GENESIS_HASH),
        };
        let seq = prev_seq.checked_add(1).ok_or(StoreError::QuotaExceeded)?;
        let hash = compute_record_hash(seq, &prev_hash, entry)?;

        let rec_id = Self::record_node_id(&chain_key, seq)?;
        let rec_type = NodeType::new("AuditRecord")?;
        let head_type = NodeType::new("AuditChainHead")?;
        let ts_micros = entry.ts.unix_micros();
        let actor_chain_json = encode_string_array(&entry.actor_chain)?;

        let mut rec_node = NewNode {
            id: Some(rec_id),
            node_type: rec_type,
            label: format!("audit seq {seq}: {}", entry.action),
            summary: Some(entry.decision.clone()),
            props: Default::default(),
            scope: ScopeSpec::Global,
            provenance: Provenance {
                source: AUDIT_PROVENANCE_SOURCE.to_owned(),
                desc_hash: None,
                confidence: None,
                extracted_at: Some(self.clock.now_timestamp()),
                agent: entry.actor_chain.first().cloned(),
            },
        };
        rec_node.props.insert("seq".to_owned(), (seq as i64).into());
        rec_node.props.insert("ts_us".to_owned(), ts_micros.into());
        rec_node
            .props
            .insert("actor_chain_json".to_owned(), actor_chain_json.into());
        rec_node
            .props
            .insert("action".to_owned(), entry.action.clone().into());
        rec_node
            .props
            .insert("resource".to_owned(), entry.resource.clone().into());
        rec_node
            .props
            .insert("decision".to_owned(), entry.decision.clone().into());
        rec_node
            .props
            .insert("details_json".to_owned(), entry.details_json.clone().into());
        rec_node
            .props
            .insert("prev_hash".to_owned(), hex_encode(&prev_hash).into());
        rec_node
            .props
            .insert("hash".to_owned(), hex_encode(&hash).into());

        let mut head_node = NewNode {
            id: Some(head_id),
            node_type: head_type,
            label: format!("audit head {chain_key}"),
            summary: None,
            props: Default::default(),
            scope: ScopeSpec::Global,
            provenance: Provenance::new(AUDIT_PROVENANCE_SOURCE),
        };
        head_node
            .props
            .insert("head_seq".to_owned(), (seq as i64).into());
        head_node
            .props
            .insert("head_hash".to_owned(), hex_encode(&hash).into());

        self.store
            .put_batch(scope, &caller, &[rec_node, head_node], &[])?;

        Ok(AuditRecord {
            seq,
            entry: entry.clone(),
            prev_hash,
            hash,
        })
    }

    fn chain_head(&self, chain: &AuditChain) -> Result<Option<(u64, [u8; 32])>, StoreError> {
        let (scope, chain_key) = self.resolve_chain(chain);
        let caller = Self::writer_caller();
        let head_id = Self::head_node_id(&chain_key)?;
        let Some(node) = self.store.get_node(scope, &caller, &head_id, None)? else {
            return Ok(None);
        };
        let seq = node
            .props
            .get("head_seq")
            .and_then(|v| v.as_u64())
            .ok_or(StoreError::Corrupt)?;
        let hash_hex = node
            .props
            .get("head_hash")
            .and_then(|v| v.as_str())
            .ok_or(StoreError::Corrupt)?;
        let hash = hex_decode_32(hash_hex)?;
        Ok(Some((seq, hash)))
    }

    fn verify_chain(
        &self,
        chain: &AuditChain,
        last_n: Option<u64>,
    ) -> Result<AuditVerifyReport, StoreError> {
        let (scope, chain_key) = self.resolve_chain(chain);
        let caller = Self::writer_caller();
        let Some((head_seq, head_hash)) = self.chain_head(chain)? else {
            return Ok(AuditVerifyReport::Intact {
                head_seq: 0,
                head_hash: AUDIT_GENESIS_HASH,
            });
        };
        if head_seq == 0 {
            return Ok(AuditVerifyReport::Intact {
                head_seq: 0,
                head_hash: AUDIT_GENESIS_HASH,
            });
        }

        let start_seq = match last_n {
            Some(n) if n > 0 && head_seq >= n => head_seq - n + 1,
            _ => 1,
        };

        let mut expected_prev = if start_seq == 1 {
            AUDIT_GENESIS_HASH
        } else {
            let prev_id = Self::record_node_id(&chain_key, start_seq - 1)?;
            let Some(prev_node) = self.store.get_node(scope, &caller, &prev_id, None)? else {
                return Ok(AuditVerifyReport::Inconsistent {
                    seq: start_seq - 1,
                    reason: "missing anchor record".to_owned(),
                });
            };
            let Some(hex_str) = prev_node.props.get("hash").and_then(|v| v.as_str()) else {
                return Ok(AuditVerifyReport::Inconsistent {
                    seq: start_seq - 1,
                    reason: "missing hash property".to_owned(),
                });
            };
            hex_decode_32(hex_str)?
        };

        for seq in start_seq..=head_seq {
            let rec_id = Self::record_node_id(&chain_key, seq)?;
            let Some(node) = self.store.get_node(scope, &caller, &rec_id, None)? else {
                return Ok(AuditVerifyReport::Inconsistent {
                    seq,
                    reason: "missing sequence record".to_owned(),
                });
            };
            let record = match decode_record_node(&node) {
                Ok(rec) => rec,
                Err(reason) => {
                    return Ok(AuditVerifyReport::Inconsistent { seq, reason });
                }
            };
            if record.seq != seq || record.prev_hash != expected_prev {
                return Ok(AuditVerifyReport::Inconsistent {
                    seq,
                    reason: "sequence or prev_hash mismatch".to_owned(),
                });
            }
            let recomputed = compute_record_hash(record.seq, &record.prev_hash, &record.entry)?;
            if recomputed != record.hash {
                return Ok(AuditVerifyReport::Inconsistent {
                    seq,
                    reason: "digest mismatch".to_owned(),
                });
            }
            expected_prev = record.hash;
        }

        if expected_prev != head_hash {
            return Ok(AuditVerifyReport::Inconsistent {
                seq: head_seq,
                reason: "chain head hash mismatch".to_owned(),
            });
        }

        Ok(AuditVerifyReport::Intact {
            head_seq,
            head_hash,
        })
    }
}

fn decode_record_node(node: &gap_store::types::Node) -> Result<AuditRecord, String> {
    let seq = node
        .props
        .get("seq")
        .and_then(|v| v.as_u64())
        .ok_or_else(|| "missing seq".to_owned())?;
    let ts_us = node
        .props
        .get("ts_us")
        .and_then(|v| v.as_i64())
        .ok_or_else(|| "missing ts_us".to_owned())?;
    let ts = gap_core::clock::Timestamp::from_unix_nanos(i128::from(ts_us) * 1_000)
        .map_err(|_| "invalid ts_us".to_owned())?;
    let actor_chain_raw = node
        .props
        .get("actor_chain_json")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "missing actor_chain_json".to_owned())?;
    let CisgValue::Array(actors_val) =
        parse_cisg_json(actor_chain_raw).map_err(|e| format!("corrupt actor_chain: {e}"))?
    else {
        return Err("actor_chain_json must be array".to_owned());
    };
    let mut actor_chain = Vec::with_capacity(actors_val.len());
    for item in actors_val {
        let CisgValue::String(s) = item else {
            return Err("actor_chain item must be string".to_owned());
        };
        actor_chain.push(s);
    }
    let action = node
        .props
        .get("action")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "missing action".to_owned())?
        .to_owned();
    let resource = node
        .props
        .get("resource")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "missing resource".to_owned())?
        .to_owned();
    let decision = node
        .props
        .get("decision")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "missing decision".to_owned())?
        .to_owned();
    let details_json = node
        .props
        .get("details_json")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "missing details_json".to_owned())?
        .to_owned();
    let prev_hex = node
        .props
        .get("prev_hash")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "missing prev_hash".to_owned())?;
    let hash_hex = node
        .props
        .get("hash")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "missing hash".to_owned())?;
    let prev_hash = hex_decode_32(prev_hex).map_err(|_| "invalid prev_hash".to_owned())?;
    let hash = hex_decode_32(hash_hex).map_err(|_| "invalid hash".to_owned())?;

    Ok(AuditRecord {
        seq,
        entry: AuditEntry {
            ts,
            actor_chain,
            action,
            resource,
            decision,
            details_json,
        },
        prev_hash,
        hash,
    })
}

fn encode_string_array(items: &[String]) -> Result<String, StoreError> {
    let val = CisgValue::Array(items.iter().map(|s| CisgValue::String(s.clone())).collect());
    val.to_canonical_json().map_err(|_| StoreError::Backend)
}

fn compute_record_hash(
    seq: u64,
    prev_hash: &[u8; 32],
    entry: &AuditEntry,
) -> Result<[u8; 32], StoreError> {
    let mut obj = BTreeMap::new();
    obj.insert("seq".to_owned(), CisgValue::Int(seq as i64));
    obj.insert("ts".to_owned(), CisgValue::String(entry.ts.to_string()));
    obj.insert(
        "actor_chain".to_owned(),
        CisgValue::Array(
            entry
                .actor_chain
                .iter()
                .map(|s| CisgValue::String(s.clone()))
                .collect(),
        ),
    );
    obj.insert("action".to_owned(), CisgValue::String(entry.action.clone()));
    obj.insert(
        "resource".to_owned(),
        CisgValue::String(entry.resource.clone()),
    );
    obj.insert(
        "decision".to_owned(),
        CisgValue::String(entry.decision.clone()),
    );
    obj.insert(
        "details_json".to_owned(),
        CisgValue::String(entry.details_json.clone()),
    );
    let canonical = CisgValue::Object(obj)
        .to_canonical_json()
        .map_err(|_| StoreError::Backend)?;
    let mut material = Vec::with_capacity(32 + canonical.len());
    material.extend_from_slice(prev_hash);
    material.extend_from_slice(canonical.as_bytes());
    Ok(sha256_bytes(&material))
}

fn hex_encode(bytes: &[u8; 32]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(64);
    for b in bytes {
        out.push(char::from(HEX[(b >> 4) as usize]));
        out.push(char::from(HEX[(b & 0x0f) as usize]));
    }
    out
}

fn hex_decode_32(s: &str) -> Result<[u8; 32], StoreError> {
    if s.len() != 64 {
        return Err(StoreError::Corrupt);
    }
    let bytes = s.as_bytes();
    let mut out = [0u8; 32];
    for (idx, [hi_b, lo_b]) in bytes.as_chunks::<2>().0.iter().enumerate() {
        let hi = hex_nibble(*hi_b).ok_or(StoreError::Corrupt)?;
        let lo = hex_nibble(*lo_b).ok_or(StoreError::Corrupt)?;
        out[idx] = (hi << 4) | lo;
    }
    Ok(out)
}

const fn hex_nibble(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Bounded recursive-descent RFC 8259 JSON parser producing [`CisgValue`] without extra dependencies.
pub(crate) fn parse_cisg_json(input: &str) -> Result<CisgValue, ProposalError> {
    let mut parser = JsonParser {
        bytes: input.as_bytes(),
        pos: 0,
    };
    parser.skip_ws();
    let value = parser.parse_value(0)?;
    parser.skip_ws();
    if parser.pos != parser.bytes.len() {
        return Err(ProposalError::Corrupt(
            "trailing bytes in stored JSON payload".to_owned(),
        ));
    }
    Ok(value)
}

struct JsonParser<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> JsonParser<'a> {
    fn skip_ws(&mut self) {
        while let Some(&b) = self.bytes.get(self.pos) {
            if matches!(b, b' ' | b'\t' | b'\r' | b'\n') {
                self.pos += 1;
            } else {
                break;
            }
        }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn next(&mut self) -> Option<u8> {
        let b = self.bytes.get(self.pos).copied()?;
        self.pos += 1;
        Some(b)
    }

    fn expect(&mut self, expected: u8) -> Result<(), ProposalError> {
        match self.next() {
            Some(b) if b == expected => Ok(()),
            _ => Err(ProposalError::Corrupt(format!(
                "expected `{}` in JSON at offset {}",
                char::from(expected),
                self.pos
            ))),
        }
    }

    fn parse_value(&mut self, depth: usize) -> Result<CisgValue, ProposalError> {
        if depth > DEFAULT_EXTRACT_MAX_DEPTH {
            return Err(ProposalError::Corrupt("JSON nesting too deep".to_owned()));
        }
        self.skip_ws();
        let Some(b) = self.peek() else {
            return Err(ProposalError::Corrupt("unexpected EOF in JSON".to_owned()));
        };
        match b {
            b'{' => self.parse_object(depth + 1),
            b'[' => self.parse_array(depth + 1),
            b'"' => self.parse_string().map(CisgValue::String),
            b't' => self.consume_literal(b"true", CisgValue::Bool(true)),
            b'f' => self.consume_literal(b"false", CisgValue::Bool(false)),
            b'n' => self.consume_literal(b"null", CisgValue::Null),
            b'-' | b'0'..=b'9' => self.parse_number(),
            _ => Err(ProposalError::Corrupt(format!(
                "unexpected byte `{b}` at JSON offset {}",
                self.pos
            ))),
        }
    }

    fn consume_literal(&mut self, lit: &[u8], out: CisgValue) -> Result<CisgValue, ProposalError> {
        if self.bytes.get(self.pos..self.pos + lit.len()) == Some(lit) {
            self.pos += lit.len();
            Ok(out)
        } else {
            Err(ProposalError::Corrupt(format!(
                "invalid literal in JSON at offset {}",
                self.pos
            )))
        }
    }

    fn parse_object(&mut self, depth: usize) -> Result<CisgValue, ProposalError> {
        self.expect(b'{')?;
        let mut map = BTreeMap::new();
        self.skip_ws();
        if self.peek() == Some(b'}') {
            self.pos += 1;
            return Ok(CisgValue::Object(map));
        }
        loop {
            self.skip_ws();
            let key = self.parse_string()?;
            self.skip_ws();
            self.expect(b':')?;
            let val = self.parse_value(depth)?;
            map.insert(key, val);
            self.skip_ws();
            match self.next() {
                Some(b',') => {}
                Some(b'}') => break,
                _ => {
                    return Err(ProposalError::Corrupt(format!(
                        "expected `,` or `}}` in JSON object at offset {}",
                        self.pos
                    )));
                }
            }
        }
        Ok(CisgValue::Object(map))
    }

    fn parse_array(&mut self, depth: usize) -> Result<CisgValue, ProposalError> {
        self.expect(b'[')?;
        let mut items = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b']') {
            self.pos += 1;
            return Ok(CisgValue::Array(items));
        }
        loop {
            let val = self.parse_value(depth)?;
            items.push(val);
            self.skip_ws();
            match self.next() {
                Some(b',') => {}
                Some(b']') => break,
                _ => {
                    return Err(ProposalError::Corrupt(format!(
                        "expected `,` or `]` in JSON array at offset {}",
                        self.pos
                    )));
                }
            }
        }
        Ok(CisgValue::Array(items))
    }

    fn parse_string(&mut self) -> Result<String, ProposalError> {
        self.expect(b'"')?;
        let mut out = String::new();
        loop {
            let Some(b) = self.next() else {
                return Err(ProposalError::Corrupt(
                    "unterminated JSON string".to_owned(),
                ));
            };
            match b {
                b'"' => return Ok(out),
                b'\\' => {
                    let esc = self.next().ok_or_else(|| {
                        ProposalError::Corrupt("unterminated JSON escape".to_owned())
                    })?;
                    match esc {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'b' => out.push('\u{0008}'),
                        b'f' => out.push('\u{000C}'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => {
                            let code = self.parse_hex4()?;
                            if (0xD800..=0xDBFF).contains(&code) {
                                // Handle UTF-16 surrogate pair
                                if self.next() != Some(b'\\') || self.next() != Some(b'u') {
                                    return Err(ProposalError::Corrupt(
                                        "incomplete UTF-16 surrogate in JSON".to_owned(),
                                    ));
                                }
                                let low = self.parse_hex4()?;
                                if !(0xDC00..=0xDFFF).contains(&low) {
                                    return Err(ProposalError::Corrupt(
                                        "invalid low UTF-16 surrogate in JSON".to_owned(),
                                    ));
                                }
                                let cp = 0x1_0000
                                    + ((u32::from(code) - 0xD800) << 10)
                                    + (u32::from(low) - 0xDC00);
                                let ch = char::from_u32(cp).ok_or_else(|| {
                                    ProposalError::Corrupt(
                                        "invalid Unicode scalar in JSON".to_owned(),
                                    )
                                })?;
                                out.push(ch);
                            } else {
                                let ch = char::from_u32(u32::from(code)).ok_or_else(|| {
                                    ProposalError::Corrupt(
                                        "invalid Unicode scalar in JSON".to_owned(),
                                    )
                                })?;
                                out.push(ch);
                            }
                        }
                        _ => {
                            return Err(ProposalError::Corrupt(format!(
                                "invalid escape sequence `\\{}` in JSON",
                                char::from(esc)
                            )));
                        }
                    }
                }
                _ => {
                    // Undo single byte and decode full UTF-8 sequence starting here.
                    self.pos -= 1;
                    let rest = std::str::from_utf8(&self.bytes[self.pos..]).map_err(|_| {
                        ProposalError::Corrupt("invalid UTF-8 in JSON string".to_owned())
                    })?;
                    let ch = rest.chars().next().ok_or_else(|| {
                        ProposalError::Corrupt("unexpected end of UTF-8 in JSON string".to_owned())
                    })?;
                    self.pos += ch.len_utf8();
                    out.push(ch);
                }
            }
        }
    }

    fn parse_hex4(&mut self) -> Result<u16, ProposalError> {
        let mut val: u16 = 0;
        for _ in 0..4 {
            let b = self
                .next()
                .ok_or_else(|| ProposalError::Corrupt("truncated \\uXXXX in JSON".to_owned()))?;
            let nib = hex_nibble(b)
                .ok_or_else(|| ProposalError::Corrupt("invalid hex digit in \\uXXXX".to_owned()))?;
            val = (val << 4) | u16::from(nib);
        }
        Ok(val)
    }

    fn parse_number(&mut self) -> Result<CisgValue, ProposalError> {
        let start = self.pos;
        if self.peek() == Some(b'-') {
            self.pos += 1;
        }
        let mut is_float = false;
        while let Some(b) = self.peek() {
            if b.is_ascii_digit() {
                self.pos += 1;
            } else {
                break;
            }
        }
        if self.peek() == Some(b'.') {
            is_float = true;
            self.pos += 1;
            while let Some(b) = self.peek() {
                if b.is_ascii_digit() {
                    self.pos += 1;
                } else {
                    break;
                }
            }
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            is_float = true;
            self.pos += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.pos += 1;
            }
            while let Some(b) = self.peek() {
                if b.is_ascii_digit() {
                    self.pos += 1;
                } else {
                    break;
                }
            }
        }
        let raw = std::str::from_utf8(&self.bytes[start..self.pos])
            .map_err(|_| ProposalError::Corrupt("invalid UTF-8 in JSON number".to_owned()))?;
        if is_float {
            let num: f64 = raw
                .parse()
                .map_err(|_| ProposalError::Corrupt(format!("invalid float `{raw}`")))?;
            if !num.is_finite() {
                return Err(ProposalError::Corrupt("non-finite JSON float".to_owned()));
            }
            Ok(CisgValue::Float(num))
        } else {
            let num: i64 = raw
                .parse()
                .map_err(|_| ProposalError::Corrupt(format!("invalid integer `{raw}`")))?;
            Ok(CisgValue::Int(num))
        }
    }
}
