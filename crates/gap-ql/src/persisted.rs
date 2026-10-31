//! SHA-256 persisted operation identifiers, Apollo APQ lookup (`GQL-061`), and `OperationMode`
//! (`open | hybrid | safelist`) document allow-listing (`GQL-060`; `spec/02-gap-ql.md` §§7, 10).

use std::collections::HashMap;
use std::fmt;

use apollo_compiler::ast::{self, Definition, OperationType, Selection};
use gap_core::config::OperationMode;
use gap_core::error::{ErrorCode, GapError, GapResult};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Computes the raw 32-byte FIPS 180-4 SHA-256 digest of `data` via `sha2::Sha256` (`GQL-061`).
#[must_use]
pub fn sha256_bytes(data: &[u8]) -> [u8; 32] {
    Sha256::digest(data).into()
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
    use std::fmt::Write as _;

    use proptest::prelude::*;
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
    proptest! {
        #![proptest_config(ProptestConfig {
            cases: 1_000,
            failure_persistence: None,
            ..ProptestConfig::default()
        })]

        #[test]
        fn prop_sha256_bytes_and_hex_match_sha2_crate_for_1000_random_inputs(
            data in proptest::collection::vec(any::<u8>(), 0..=4096)
        ) {
            let mut hasher = Sha256::new();
            hasher.update(&data);
            let expected_bytes: [u8; 32] = hasher.finalize().into();

            let mut expected_hex = String::with_capacity(64);
            for b in expected_bytes {
                write!(&mut expected_hex, "{b:02x}").expect("format hex byte");
            }

            prop_assert_eq!(sha256_bytes(&data), expected_bytes);
            prop_assert_eq!(sha256_hex(&data), expected_hex);
        }
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
