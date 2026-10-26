//! SHA-256 persisted operation identifiers, Apollo APQ lookup (`GQL-061`), and `OperationMode`
//! (`open | hybrid | safelist`) document allow-listing (`GQL-060`; `spec/02-gap-ql.md` §§7, 10).

use std::collections::HashMap;
use std::fmt;

use apollo_compiler::ast::{self, Definition, OperationType, Selection};
use gap_core::config::OperationMode;
use gap_core::error::{ErrorCode, GapError, GapResult};
use serde::{Deserialize, Serialize};

/// FIPS 180-4 SHA-256 initial hash words (`H^(0)`).
const SHA256_H0: [u32; 8] = [
    0x6a09_e667,
    0xbb67_ae85,
    0x3c6e_f372,
    0xa54f_f53a,
    0x510e_527f,
    0x9b05_688c,
    0x1f83_d9ab,
    0x5be0_cd19,
];

/// FIPS 180-4 SHA-256 round constants (`K_0..K_63`).
const SHA256_K: [u32; 64] = [
    0x428a_2f98,
    0x7137_4491,
    0xb5c0_fbcf,
    0xe9b5_dba5,
    0x3956_c25b,
    0x59f1_11f1,
    0x923f_82a4,
    0xab1c_5ed5,
    0xd807_aa98,
    0x1283_5b01,
    0x2431_85be,
    0x550c_7dc3,
    0x72be_5d74,
    0x80de_b1fe,
    0x9bdc_06a7,
    0xc19b_f174,
    0xe49b_69c1,
    0xefbe_4786,
    0x0fc1_9dc6,
    0x240c_a1cc,
    0x2de9_2c6f,
    0x4a74_84aa,
    0x5cb0_a9dc,
    0x76f9_88da,
    0x983e_5152,
    0xa831_c66d,
    0xb003_27c8,
    0xbf59_7fc7,
    0xc6e0_0bf3,
    0xd5a7_9147,
    0x06ca_6351,
    0x1429_2967,
    0x27b7_0a85,
    0x2e1b_2138,
    0x4d2c_6dfc,
    0x5338_0d13,
    0x650a_7354,
    0x766a_0abb,
    0x81c2_c92e,
    0x9272_2c85,
    0xa2bf_e8a1,
    0xa81a_664b,
    0xc24b_8b70,
    0xc76c_51a3,
    0xd192_e819,
    0xd699_0624,
    0xf40e_3585,
    0x106a_a070,
    0x19a4_c116,
    0x1e37_6c08,
    0x2748_774c,
    0x34b0_bcb5,
    0x391c_0cb3,
    0x4ed8_aa4a,
    0x5b9c_ca4f,
    0x682e_6ff3,
    0x748f_82ee,
    0x78a5_636f,
    0x84c8_7814,
    0x8cc7_0208,
    0x90be_fffa,
    0xa450_6ceb,
    0xbef9_a3f7,
    0xc671_78f2,
];

fn sha256_compress_block(state: &mut [u32; 8], block: &[u8; 64]) {
    let mut w = [0_u32; 64];
    for (i, word) in w.iter_mut().take(16).enumerate() {
        let base = i * 4;
        *word = u32::from_be_bytes([
            block[base],
            block[base + 1],
            block[base + 2],
            block[base + 3],
        ]);
    }
    for i in 16..64 {
        let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
        let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
        w[i] = w[i - 16]
            .wrapping_add(s0)
            .wrapping_add(w[i - 7])
            .wrapping_add(s1);
    }

    let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = *state;
    for i in 0..64 {
        let sum1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
        let ch = (e & f) ^ ((!e) & g);
        let temp1 = h
            .wrapping_add(sum1)
            .wrapping_add(ch)
            .wrapping_add(SHA256_K[i])
            .wrapping_add(w[i]);
        let sum0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
        let maj = (a & b) ^ (a & c) ^ (b & c);
        let temp2 = sum0.wrapping_add(maj);

        h = g;
        g = f;
        f = e;
        e = d.wrapping_add(temp1);
        d = c;
        c = b;
        b = a;
        a = temp1.wrapping_add(temp2);
    }

    state[0] = state[0].wrapping_add(a);
    state[1] = state[1].wrapping_add(b);
    state[2] = state[2].wrapping_add(c);
    state[3] = state[3].wrapping_add(d);
    state[4] = state[4].wrapping_add(e);
    state[5] = state[5].wrapping_add(f);
    state[6] = state[6].wrapping_add(g);
    state[7] = state[7].wrapping_add(h);
}

/// Computes the raw 32-byte FIPS 180-4 SHA-256 digest of `data` in safe pure Rust (`GQL-061`).
#[must_use]
pub fn sha256_bytes(data: &[u8]) -> [u8; 32] {
    let mut state = SHA256_H0;
    let (blocks, rem) = data.as_chunks::<64>();
    for block in blocks {
        sha256_compress_block(&mut state, block);
    }
    let mut tail = [0_u8; 128];
    tail[..rem.len()].copy_from_slice(rem);
    tail[rem.len()] = 0x80;

    let bit_len = (data.len() as u64).wrapping_mul(8).to_be_bytes();
    if rem.len() < 56 {
        tail[56..64].copy_from_slice(&bit_len);
        let mut final_block = [0_u8; 64];
        final_block.copy_from_slice(&tail[..64]);
        sha256_compress_block(&mut state, &final_block);
    } else {
        tail[120..128].copy_from_slice(&bit_len);
        let mut block1 = [0_u8; 64];
        block1.copy_from_slice(&tail[..64]);
        sha256_compress_block(&mut state, &block1);
        let mut block2 = [0_u8; 64];
        block2.copy_from_slice(&tail[64..128]);
        sha256_compress_block(&mut state, &block2);
    }

    let mut out = [0_u8; 32];
    for (idx, word) in state.iter().enumerate() {
        let bytes = word.to_be_bytes();
        let base = idx * 4;
        out[base..base + 4].copy_from_slice(&bytes);
    }
    out
}

/// Computes the 64-character lowercase hexadecimal FIPS 180-4 SHA-256 digest of `data` (`GQL-061`).
#[must_use]
pub fn sha256_hex(data: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let digest = sha256_bytes(data);
    let mut out = String::with_capacity(64);
    for byte in digest {
        out.push(char::from(HEX[usize::from(byte >> 4)]));
        out.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    out
}

/// Validated 64-character lowercase hexadecimal SHA-256 persisted operation identifier (`GQL-061`).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PersistedOperationId(String);

impl PersistedOperationId {
    /// Computes the SHA-256 [`PersistedOperationId`] of a GraphQL `document` (`GQL-061`).
    #[must_use]
    pub fn from_document(document: &str) -> Self {
        Self(sha256_hex(document.as_bytes()))
    }

    /// Parses and normalizes a 64-character hexadecimal SHA-256 string into a [`PersistedOperationId`].
    ///
    /// # Errors
    ///
    /// Returns [`ErrorCode::ValidationError`] if `raw` is not 64 ASCII hexadecimal characters.
    pub fn parse(raw: &str) -> GapResult<Self> {
        if raw.len() != 64 || !raw.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(GapError::new(
                ErrorCode::ValidationError,
                "invalid persistedQuery.sha256Hash: expected 64 hexadecimal characters (GQL-061)",
            ));
        }
        Ok(Self(raw.to_ascii_lowercase()))
    }

    /// Borrows the canonical 64-character lowercase hexadecimal SHA-256 string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl AsRef<str> for PersistedOperationId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for PersistedOperationId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Read-only lookup interface over admin-registered persisted GraphQL operations (`GQL-061`).
///
/// Per `GQL-061`, client requests MUST NOT register new operations automatically; registration is an
/// administrative action recorded in the audit log.
pub trait PersistedOperationStore: Send + Sync {
    /// Looks up the GraphQL document registered under `id`, if present.
    fn get_document(&self, id: &PersistedOperationId) -> Option<String>;

    /// Returns `true` if `id` is registered in the persisted operation catalogue.
    fn contains(&self, id: &PersistedOperationId) -> bool {
        self.get_document(id).is_some()
    }
}

/// Empty [`PersistedOperationStore`] containing no registered operations.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct EmptyPersistedStore;

impl PersistedOperationStore for EmptyPersistedStore {
    fn get_document(&self, _id: &PersistedOperationId) -> Option<String> {
        None
    }

    fn contains(&self, _id: &PersistedOperationId) -> bool {
        false
    }
}

/// In-memory catalogue of admin-registered persisted operations (`GQL-061`).
#[derive(Debug, Default, Clone)]
pub struct InMemoryPersistedStore {
    entries: HashMap<PersistedOperationId, String>,
}

impl InMemoryPersistedStore {
    /// Creates an empty [`InMemoryPersistedStore`].
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers `document` under its SHA-256 [`PersistedOperationId`] and returns the ID.
    pub fn register(&mut self, document: impl Into<String>) -> PersistedOperationId {
        let doc = document.into();
        let id = PersistedOperationId::from_document(&doc);
        self.entries.insert(id.clone(), doc);
        id
    }

    /// Removes the operation registered under `id`, returning its document if it existed.
    pub fn remove(&mut self, id: &PersistedOperationId) -> Option<String> {
        self.entries.remove(id)
    }

    /// Returns the number of registered operations.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Returns `true` when no operations are registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

impl PersistedOperationStore for InMemoryPersistedStore {
    fn get_document(&self, id: &PersistedOperationId) -> Option<String> {
        self.entries.get(id).cloned()
    }

    fn contains(&self, id: &PersistedOperationId) -> bool {
        self.entries.contains_key(id)
    }
}

/// Resolved GraphQL document text paired with its SHA-256 [`PersistedOperationId`] and safelist
/// registration status (`GQL-060`, `GQL-061`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedDocument {
    /// Executable GraphQL query text to parse and validate.
    pub query: String,
    /// Canonical SHA-256 digest of [`Self::query`].
    pub operation_id: PersistedOperationId,
    /// Whether [`Self::operation_id`] is present in the admin-registered [`PersistedOperationStore`].
    pub is_registered: bool,
}

fn extract_apq_hash(
    extensions: Option<&serde_json::Map<String, serde_json::Value>>,
) -> GapResult<Option<PersistedOperationId>> {
    let Some(ext_map) = extensions else {
        return Ok(None);
    };
    let Some(pq_val) = ext_map.get("persistedQuery") else {
        return Ok(None);
    };
    let Some(pq_obj) = pq_val.as_object() else {
        return Err(GapError::new(
            ErrorCode::ValidationError,
            "`extensions.persistedQuery` must be a JSON object (GQL-061)",
        ));
    };

    if let Some(version_val) = pq_obj.get("version") {
        let Some(version_num) = version_val.as_i64() else {
            return Err(GapError::new(
                ErrorCode::ValidationError,
                "`extensions.persistedQuery.version` must be an integer (GQL-061)",
            ));
        };
        if version_num != 1 {
            return Err(GapError::new(
                ErrorCode::UnsupportedFeature,
                format!("unsupported persistedQuery version {version_num}; expected 1 (GQL-061)"),
            ));
        }
    }

    let Some(hash_val) = pq_obj.get("sha256Hash") else {
        return Err(GapError::new(
            ErrorCode::ValidationError,
            "`extensions.persistedQuery.sha256Hash` is required when `persistedQuery` is present (GQL-061)",
        ));
    };
    let Some(hash_str) = hash_val.as_str() else {
        return Err(GapError::new(
            ErrorCode::ValidationError,
            "`extensions.persistedQuery.sha256Hash` must be a string (GQL-061)",
        ));
    };

    PersistedOperationId::parse(hash_str).map(Some)
}

/// Resolves a GraphQL request's `query` and optional Apollo APQ `extensions.persistedQuery` against
/// `store` (`GQL-061`).
///
/// Client requests supplying both `query` and `sha256Hash` have their SHA-256 verified, and NEVER
/// auto-register new operations in `store`.
pub fn resolve_persisted_operation(
    query: Option<&str>,
    extensions: Option<&serde_json::Map<String, serde_json::Value>>,
    store: &dyn PersistedOperationStore,
) -> GapResult<ResolvedDocument> {
    let apq_id = extract_apq_hash(extensions)?;

    match (query, apq_id) {
        (None, Some(id)) => {
            let Some(doc) = store.get_document(&id) else {
                return Err(GapError::new(
                    ErrorCode::PersistedOperationNotFound,
                    format!("persisted operation `{id}` was not found (GQL-061)"),
                ));
            };
            Ok(ResolvedDocument {
                query: doc,
                operation_id: id,
                is_registered: true,
            })
        }
        (Some(q), Some(expected_id)) => {
            let actual_id = PersistedOperationId::from_document(q);
            if actual_id != expected_id {
                return Err(GapError::new(
                    ErrorCode::ValidationError,
                    format!(
                        "persistedQuery.sha256Hash `{expected_id}` does not match query SHA-256 `{actual_id}` (GQL-061)"
                    ),
                ));
            }
            let is_registered = store.contains(&actual_id);
            Ok(ResolvedDocument {
                query: q.to_owned(),
                operation_id: actual_id,
                is_registered,
            })
        }
        (Some(q), None) => {
            let actual_id = PersistedOperationId::from_document(q);
            let is_registered = store.contains(&actual_id);
            Ok(ResolvedDocument {
                query: q.to_owned(),
                operation_id: actual_id,
                is_registered,
            })
        }
        (None, None) => Err(GapError::new(
            ErrorCode::ParseError,
            "request must provide `query` or `extensions.persistedQuery.sha256Hash`",
        )),
    }
}

fn selections_contain_task_submission(selections: &[Selection], inside_gap_ns: bool) -> bool {
    for sel in selections {
        match sel {
            Selection::Field(field) => {
                let fname = field.name.as_str();
                if inside_gap_ns && fname == "submitTask" {
                    return true;
                }
                let next_inside_gap = inside_gap_ns || fname == "gap";
                if selections_contain_task_submission(&field.selection_set, next_inside_gap) {
                    return true;
                }
            }
            Selection::InlineFragment(inline) => {
                let cond_is_gap_mut = inline
                    .type_condition
                    .as_ref()
                    .is_some_and(|c| c.as_str() == "GapMutation");
                if selections_contain_task_submission(
                    &inline.selection_set,
                    inside_gap_ns || cond_is_gap_mut,
                ) {
                    return true;
                }
            }
            Selection::FragmentSpread(_) => {}
        }
    }
    false
}

fn ast_contains_mutation_or_task_submission(ast_doc: &ast::Document) -> bool {
    for def in &ast_doc.definitions {
        match def {
            Definition::OperationDefinition(op) => {
                if op.operation_type == OperationType::Mutation {
                    return true;
                }
                if selections_contain_task_submission(&op.selection_set, false) {
                    return true;
                }
            }
            Definition::FragmentDefinition(frag) => {
                let cond = frag.type_condition.as_str();
                if cond == "Mutation" || cond == "GapMutation" {
                    return true;
                }
                if selections_contain_task_submission(&frag.selection_set, false) {
                    return true;
                }
            }
            _ => {}
        }
    }
    false
}

/// Enforces `gateway.operation_mode` (`open | hybrid | safelist`) on a parsed GraphQL document
/// (`GQL-060`).
///
/// - [`OperationMode::Open`]: arbitrary queries and mutations are allowed.
/// - [`OperationMode::Hybrid`]: arbitrary documents are allowed iff they contain no mutation or
///   task submission; mutations and task submissions require registered persisted operations.
/// - [`OperationMode::Safelist`]: only registered persisted operations are allowed.
pub fn enforce_operation_mode(
    mode: OperationMode,
    is_registered: bool,
    ast_doc: &ast::Document,
) -> GapResult<()> {
    match mode {
        OperationMode::Open => Ok(()),
        OperationMode::Safelist => {
            if is_registered {
                Ok(())
            } else {
                Err(GapError::new(
                    ErrorCode::OperationNotAllowed,
                    "operation rejected: `safelist` mode allows only registered persisted operations (GQL-060)",
                ))
            }
        }
        OperationMode::Hybrid => {
            if is_registered || !ast_contains_mutation_or_task_submission(ast_doc) {
                Ok(())
            } else {
                Err(GapError::new(
                    ErrorCode::OperationNotAllowed,
                    "operation rejected: `hybrid` mode requires mutations and task submissions to use registered operations (GQL-060)",
                ))
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::validate::parse_executable_ast;

    // spec: GQL-061
    #[test]
    fn sha256_matches_nist_fips_180_4_test_vectors() {
        // 1. Empty string
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        // 2. "abc"
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        // 3. 56-byte NIST two-block boundary vector
        assert_eq!(
            sha256_hex(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
        // 4. Exact 64-byte single-chunk boundary ("a" * 64)
        let sixty_four_a = vec![b'a'; 64];
        assert_eq!(
            sha256_hex(&sixty_four_a),
            "ffe054fe7ae0cb6dc65c3af9b61d5209f439851db43d0ba5997337df154668eb"
        );
        // 5. Multi-chunk 1,000 'a' bytes
        let thousand_a = vec![b'a'; 1_000];
        assert_eq!(
            sha256_hex(&thousand_a),
            "41edece42d63e8d9bf515a9ba6932e1c20cbc9f5a5d134645adb5db1b9737ea3"
        );
    }

    // spec: GQL-061
    #[test]
    fn persisted_operation_apq_lookup_verification_and_no_auto_registration() {
        let mut store = InMemoryPersistedStore::new();
        assert!(store.is_empty());

        let registered_query = "query GetMeta { gap { metaGraph { generation } } }";
        let reg_id = store.register(registered_query);
        assert_eq!(store.len(), 1);
        assert!(store.contains(&reg_id));

        // 1. Registered hash-only lookup succeeds (even with uppercase hex input normalized)
        let ext_reg = json!({
            "persistedQuery": {
                "version": 1,
                "sha256Hash": reg_id.as_str().to_ascii_uppercase()
            }
        });
        let resolved = resolve_persisted_operation(None, ext_reg.as_object(), &store)
            .expect("registered APQ hash-only lookup");
        assert_eq!(resolved.query, registered_query);
        assert_eq!(resolved.operation_id, reg_id);
        assert!(resolved.is_registered);

        // 2. Unregistered hash-only lookup -> GAP_PERSISTED_OPERATION_NOT_FOUND
        let missing_id = PersistedOperationId::from_document("{ __typename }");
        let ext_missing = json!({
            "persistedQuery": { "version": 1, "sha256Hash": missing_id.as_str() }
        });
        let err = resolve_persisted_operation(None, ext_missing.as_object(), &store)
            .expect_err("unregistered hash must fail");
        assert_eq!(err.code(), ErrorCode::PersistedOperationNotFound);

        // 3. Unregistered query + matching hash succeeds with is_registered = false and does NOT mutate store
        let adhoc_query = "{ __typename }";
        let resolved_adhoc =
            resolve_persisted_operation(Some(adhoc_query), ext_missing.as_object(), &store)
                .expect("matching query + hash");
        assert!(!resolved_adhoc.is_registered);
        assert_eq!(store.len(), 1);
        assert!(!store.contains(&missing_id));

        // 4. Query + mismatched hash -> GAP_VALIDATION_ERROR
        let mismatch_err =
            resolve_persisted_operation(Some(adhoc_query), ext_reg.as_object(), &store)
                .expect_err("mismatched hash must fail");
        assert_eq!(mismatch_err.code(), ErrorCode::ValidationError);

        // 5. Unsupported APQ version -> GAP_UNSUPPORTED_FEATURE
        let bad_ver = json!({
            "persistedQuery": { "version": 2, "sha256Hash": reg_id.as_str() }
        });
        let ver_err = resolve_persisted_operation(None, bad_ver.as_object(), &store)
            .expect_err("APQ v2 must fail");
        assert_eq!(ver_err.code(), ErrorCode::UnsupportedFeature);

        // 6. Malformed hex hash -> GAP_VALIDATION_ERROR
        let bad_hex = json!({
            "persistedQuery": { "version": 1, "sha256Hash": "not-hex" }
        });
        let hex_err = resolve_persisted_operation(None, bad_hex.as_object(), &store)
            .expect_err("invalid hex must fail");
        assert_eq!(hex_err.code(), ErrorCode::ValidationError);

        // 7. Neither query nor persistedQuery -> GAP_PARSE_ERROR
        let empty_err = resolve_persisted_operation(None, None, &store)
            .expect_err("missing query and hash must fail");
        assert_eq!(empty_err.code(), ErrorCode::ParseError);
    }

    // spec: GQL-060
    #[test]
    fn operation_mode_enforces_open_hybrid_and_safelist_matrix() {
        let read_ast = parse_executable_ast("{ gap { metaGraph { generation } } }", "read.graphql")
            .expect("parse read");
        let mut_ast = parse_executable_ast(
            "mutation Cancel { gap { cancelTask(id: \"t1\") { id } } }",
            "mut.graphql",
        )
        .expect("parse mutation");
        let task_ast = parse_executable_ast(
            "mutation Submit($in: SubmitTaskInput!) { gap { submitTask(input: $in) { id } } }",
            "task.graphql",
        )
        .expect("parse submitTask");
        let sneaky_frag_task_ast = parse_executable_ast(
            "query Q { __typename } fragment F on GapMutation { submitTask(input: { steps: [] }) { id } }",
            "frag.graphql",
        )
        .expect("parse fragment task");

        // Open: everything allowed even when unregistered
        for ast in [&read_ast, &mut_ast, &task_ast, &sneaky_frag_task_ast] {
            enforce_operation_mode(OperationMode::Open, false, ast).expect("open allows all");
        }

        // Hybrid: unregistered read allowed; unregistered mutation & task submission rejected; registered allowed
        enforce_operation_mode(OperationMode::Hybrid, false, &read_ast)
            .expect("hybrid allows unregistered read");
        for ast in [&mut_ast, &task_ast, &sneaky_frag_task_ast] {
            let err = enforce_operation_mode(OperationMode::Hybrid, false, ast)
                .expect_err("hybrid rejects unregistered mutation/task");
            assert_eq!(err.code(), ErrorCode::OperationNotAllowed);
            enforce_operation_mode(OperationMode::Hybrid, true, ast)
                .expect("hybrid allows registered mutation/task");
        }

        // Safelist: unregistered read & mutation rejected; registered allowed
        for ast in [&read_ast, &mut_ast, &task_ast] {
            let err = enforce_operation_mode(OperationMode::Safelist, false, ast)
                .expect_err("safelist rejects unregistered document");
            assert_eq!(err.code(), ErrorCode::OperationNotAllowed);
            enforce_operation_mode(OperationMode::Safelist, true, ast)
                .expect("safelist allows registered document");
        }
    }
}
