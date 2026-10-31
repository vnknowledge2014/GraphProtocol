//! OpenAPI `3.0.x` / `3.1.x` JSON document parser and local `$ref` resolver (`ING-010`, `ING-012`, `NGC-013`).

use std::collections::BTreeMap;
use std::fmt;

use serde::de::{DeserializeSeed, MapAccess, SeqAccess, Visitor};
use serde_json::{Map, Value};
use thiserror::Error;

/// Default maximum raw OpenAPI document size in bytes (`8 MiB`, `ING-012`, `spec/05-ingestion.md` §2).
pub const DEFAULT_MAX_DOCUMENT_BYTES: usize = 8 * 1024 * 1024;

/// Default maximum JSON nesting depth accepted while parsing or expanding a document (`ING-012`).
pub const DEFAULT_MAX_DOCUMENT_DEPTH: usize = 64;

/// Default maximum JSON item/node count accepted while parsing or expanding a document (`ING-012`).
pub const DEFAULT_MAX_DOCUMENT_ITEMS: usize = 10_000;

/// Default maximum `$ref` resolution hop depth along any reference chain (`ING-012`, `NGC-013`).
pub const DEFAULT_MAX_REF_HOPS: usize = 32;

/// Hard stack-safety ceiling on recursion depth regardless of caller configuration (`ING-012`).
const HARD_MAX_STACK_DEPTH: usize = 256;

/// Maximum diagnostic snippet length included in [`ExtractError`] variants.
const MAX_ERROR_SNIPPET_CHARS: usize = 128;

/// Resource bounds enforced when parsing and resolving an OpenAPI 3.x document (`ING-012`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExtractLimits {
    /// Maximum raw input byte length before JSON parsing begins.
    pub max_bytes: usize,
    /// Maximum JSON container nesting depth.
    pub max_depth: usize,
    /// Maximum total JSON items/nodes permitted during parsing or `$ref` expansion.
    pub max_items: usize,
    /// Maximum local `$ref` hops permitted along a reference chain.
    pub max_ref_hops: usize,
}

impl Default for ExtractLimits {
    fn default() -> Self {
        Self {
            max_bytes: DEFAULT_MAX_DOCUMENT_BYTES,
            max_depth: DEFAULT_MAX_DOCUMENT_DEPTH,
            max_items: DEFAULT_MAX_DOCUMENT_ITEMS,
            max_ref_hops: DEFAULT_MAX_REF_HOPS,
        }
    }
}

/// Errors produced while parsing an OpenAPI document or resolving schemas (`ING-012`, `NGC-013`).
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ExtractError {
    /// Document byte length exceeded [`ExtractLimits::max_bytes`].
    #[error("OpenAPI document size {len} bytes exceeds limit of {max_bytes} bytes")]
    TooLarge {
        /// Actual byte length.
        len: usize,
        /// Configured byte cap.
        max_bytes: usize,
    },
    /// JSON nesting depth exceeded [`ExtractLimits::max_depth`].
    #[error("OpenAPI document nesting depth exceeds limit of {max_depth}")]
    DepthExceeded {
        /// Configured nesting cap.
        max_depth: usize,
    },
    /// Total JSON node or expanded `$ref` node count exceeded [`ExtractLimits::max_items`].
    #[error("OpenAPI document item count exceeds limit of {max_items}")]
    ItemLimitExceeded {
        /// Configured item cap.
        max_items: usize,
    },
    /// Input was not valid JSON.
    #[error("malformed OpenAPI JSON: {0}")]
    InvalidJson(String),
    /// A required top-level OpenAPI field was missing.
    #[error("missing required OpenAPI field `{0}`")]
    MissingField(&'static str),
    /// The `openapi` version string was not `3.0.x` or `3.1.x`.
    #[error("unsupported OpenAPI version `{0}` (expected 3.0.x or 3.1.x)")]
    UnsupportedOpenApiVersion(String),
    /// A structural requirement of OpenAPI 3.x was violated.
    #[error("invalid OpenAPI structure: {0}")]
    InvalidStructure(String),
    /// Non-local or malformed `$ref` target (`NGC-013`, `spec/05-ingestion.md` §2).
    #[error("rejected non-local or invalid $ref `{0}` (only `#/...` pointers are allowed)")]
    ExternalRefRejected(String),
    /// Local JSON pointer did not match any target node in the document.
    #[error("unresolved local $ref `{0}`")]
    UnresolvedRef(String),
    /// Cyclic `$ref` graph detected.
    #[error("cyclic $ref detected at `{0}`")]
    CyclicRef(String),
    /// `$ref` resolution chain exceeded [`ExtractLimits::max_ref_hops`].
    #[error("$ref chain depth exceeds limit of {max_ref_hops}")]
    RefHopLimitExceeded {
        /// Configured hop cap.
        max_ref_hops: usize,
    },
}

fn clip_str(text: &str) -> String {
    let mut out = String::new();
    for (idx, ch) in text.chars().enumerate() {
        if idx >= MAX_ERROR_SNIPPET_CHARS {
            out.push_str("...");
            break;
        }
        out.push(ch);
    }
    out
}

#[derive(Default)]
struct ParseTracker {
    items_seen: usize,
    hit_depth: bool,
    hit_items: bool,
}

impl ParseTracker {
    fn bump_item<E: serde::de::Error>(&mut self, max_items: usize) -> Result<(), E> {
        self.items_seen = self.items_seen.saturating_add(1);
        if self.items_seen > max_items {
            self.hit_items = true;
            return Err(E::custom("item limit exceeded"));
        }
        Ok(())
    }
}

struct BoundedValueSeed<'a> {
    depth: usize,
    max_depth: usize,
    max_items: usize,
    tracker: &'a mut ParseTracker,
}

impl<'de> DeserializeSeed<'de> for BoundedValueSeed<'_> {
    type Value = Value;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        if self.depth > self.max_depth {
            self.tracker.hit_depth = true;
            return Err(serde::de::Error::custom("depth limit exceeded"));
        }
        self.tracker.bump_item(self.max_items)?;
        deserializer.deserialize_any(self)
    }
}

impl<'de> Visitor<'de> for BoundedValueSeed<'_> {
    type Value = Value;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("any bounded JSON value")
    }

    fn visit_bool<E>(self, v: bool) -> Result<Self::Value, E> {
        Ok(Value::Bool(v))
    }

    fn visit_i64<E>(self, v: i64) -> Result<Self::Value, E> {
        Ok(Value::Number(v.into()))
    }

    fn visit_u64<E>(self, v: u64) -> Result<Self::Value, E> {
        Ok(Value::Number(v.into()))
    }

    fn visit_f64<E>(self, v: f64) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        serde_json::Number::from_f64(v)
            .map(Value::Number)
            .ok_or_else(|| E::custom("non-finite JSON number"))
    }

    fn visit_str<E>(self, v: &str) -> Result<Self::Value, E> {
        Ok(Value::String(v.to_owned()))
    }

    fn visit_string<E>(self, v: String) -> Result<Self::Value, E> {
        Ok(Value::String(v))
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E> {
        Ok(Value::Null)
    }

    fn visit_none<E>(self) -> Result<Self::Value, E> {
        Ok(Value::Null)
    }

    fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let next_depth = self.depth.saturating_add(1);
        if next_depth > self.max_depth {
            self.tracker.hit_depth = true;
            return Err(serde::de::Error::custom("depth limit exceeded"));
        }
        let mut out = Vec::new();
        while let Some(elem) = seq.next_element_seed(BoundedValueSeed {
            depth: next_depth,
            max_depth: self.max_depth,
            max_items: self.max_items,
            tracker: self.tracker,
        })? {
            out.push(elem);
        }
        Ok(Value::Array(out))
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let next_depth = self.depth.saturating_add(1);
        if next_depth > self.max_depth {
            self.tracker.hit_depth = true;
            return Err(serde::de::Error::custom("depth limit exceeded"));
        }
        let mut sorted: BTreeMap<String, Value> = BTreeMap::new();
        while let Some(key) = map.next_key::<String>()? {
            let val = map.next_value_seed(BoundedValueSeed {
                depth: next_depth,
                max_depth: self.max_depth,
                max_items: self.max_items,
                tracker: self.tracker,
            })?;
            sorted.insert(key, val);
        }
        let mut obj = Map::with_capacity(sorted.len());
        for (k, v) in sorted {
            obj.insert(k, v);
        }
        Ok(Value::Object(obj))
    }
}

fn parse_bounded_json(bytes: &[u8], limits: ExtractLimits) -> Result<Value, ExtractError> {
    if bytes.len() > limits.max_bytes {
        return Err(ExtractError::TooLarge {
            len: bytes.len(),
            max_bytes: limits.max_bytes,
        });
    }
    let effective_depth = limits.max_depth.min(HARD_MAX_STACK_DEPTH);
    let mut tracker = ParseTracker::default();
    let mut de = serde_json::Deserializer::from_slice(bytes);
    let seed = BoundedValueSeed {
        depth: 0,
        max_depth: effective_depth,
        max_items: limits.max_items,
        tracker: &mut tracker,
    };
    let value = match seed.deserialize(&mut de) {
        Ok(v) => v,
        Err(err) => {
            if tracker.hit_depth || err.to_string().contains("recursion limit exceeded") {
                return Err(ExtractError::DepthExceeded {
                    max_depth: limits.max_depth,
                });
            }
            if tracker.hit_items {
                return Err(ExtractError::ItemLimitExceeded {
                    max_items: limits.max_items,
                });
            }
            return Err(ExtractError::InvalidJson(clip_str(&err.to_string())));
        }
    };
    de.end()
        .map_err(|err| ExtractError::InvalidJson(clip_str(&err.to_string())))?;
    Ok(value)
}

/// Unescapes a single RFC 6901 JSON Pointer segment (`~1` -> `/`, `~0` -> `~`).
pub(crate) fn unescape_pointer_segment(
    segment: &str,
    full_ref: &str,
) -> Result<String, ExtractError> {
    if !segment.contains('~') {
        return Ok(segment.to_owned());
    }
    let mut out = String::with_capacity(segment.len());
    let mut chars = segment.chars();
    while let Some(ch) = chars.next() {
        if ch == '~' {
            match chars.next() {
                Some('0') => out.push('~'),
                Some('1') => out.push('/'),
                _ => return Err(ExtractError::ExternalRefRejected(clip_str(full_ref))),
            }
        } else {
            out.push(ch);
        }
    }
    Ok(out)
}

#[derive(Default)]
struct RefExpandState {
    stack: Vec<String>,
    items_emitted: usize,
}

/// Parsed, bounded OpenAPI `3.0.x` / `3.1.x` document (`ING-010`, `ING-012`).
#[derive(Debug, Clone, PartialEq)]
pub struct OpenApiDocument {
    openapi_version: String,
    spec_hash: String,
    servers: Vec<String>,
    root: Value,
    limits: ExtractLimits,
}

impl OpenApiDocument {
    /// Parses `bytes` under default [`ExtractLimits`].
    pub fn parse(bytes: &[u8]) -> Result<Self, ExtractError> {
        Self::parse_with_limits(bytes, ExtractLimits::default())
    }

    /// Parses `bytes` under explicit `limits`.
    pub fn parse_with_limits(bytes: &[u8], limits: ExtractLimits) -> Result<Self, ExtractError> {
        let root = parse_bounded_json(bytes, limits)?;
        let Some(root_obj) = root.as_object() else {
            return Err(ExtractError::InvalidStructure(
                "root document must be a JSON object".to_owned(),
            ));
        };

        let openapi_val = root_obj
            .get("openapi")
            .ok_or(ExtractError::MissingField("openapi"))?;
        let Some(openapi_str) = openapi_val.as_str() else {
            return Err(ExtractError::InvalidStructure(
                "field `openapi` must be a string".to_owned(),
            ));
        };
        if !(openapi_str.starts_with("3.0.") || openapi_str.starts_with("3.1.")) {
            return Err(ExtractError::UnsupportedOpenApiVersion(clip_str(
                openapi_str,
            )));
        }

        let info_val = root_obj
            .get("info")
            .ok_or(ExtractError::MissingField("info"))?;
        let Some(info_obj) = info_val.as_object() else {
            return Err(ExtractError::InvalidStructure(
                "field `info` must be an object".to_owned(),
            ));
        };
        for req_info in ["title", "version"] {
            match info_obj.get(req_info) {
                Some(Value::String(s)) if !s.is_empty() => {}
                _ => {
                    return Err(ExtractError::InvalidStructure(format!(
                        "`info.{req_info}` must be a non-empty string"
                    )));
                }
            }
        }

        let paths_val = root_obj
            .get("paths")
            .ok_or(ExtractError::MissingField("paths"))?;
        if !paths_val.is_object() {
            return Err(ExtractError::InvalidStructure(
                "field `paths` must be an object".to_owned(),
            ));
        }

        let mut servers = Vec::new();
        if let Some(servers_val) = root_obj.get("servers") {
            let Some(arr) = servers_val.as_array() else {
                return Err(ExtractError::InvalidStructure(
                    "field `servers` must be an array".to_owned(),
                ));
            };
            for entry in arr {
                let Some(url) = entry
                    .as_object()
                    .and_then(|o| o.get("url"))
                    .and_then(Value::as_str)
                else {
                    return Err(ExtractError::InvalidStructure(
                        "each `servers[]` item must be an object with a string `url`".to_owned(),
                    ));
                };
                servers.push(url.to_owned());
            }
        }

        let spec_hash = blake3::hash(bytes).to_hex().to_string();
        Ok(Self {
            openapi_version: openapi_str.to_owned(),
            spec_hash,
            servers,
            root,
            limits,
        })
    }

    /// Returns the validated OpenAPI version string (`3.0.x` or `3.1.x`).
    #[must_use]
    pub fn openapi_version(&self) -> &str {
        &self.openapi_version
    }

    /// Returns the hex-encoded `blake3` digest of the raw document bytes (`NGC-010`).
    #[must_use]
    pub fn spec_hash(&self) -> &str {
        &self.spec_hash
    }

    /// Returns the raw `servers[].url` entries recorded from the document (never followed).
    #[must_use]
    pub fn servers(&self) -> &[String] {
        &self.servers
    }

    /// Returns the canonicalized root JSON value.
    #[must_use]
    pub fn root(&self) -> &Value {
        &self.root
    }

    /// Returns the [`ExtractLimits`] bound to this document.
    #[must_use]
    pub const fn limits(&self) -> ExtractLimits {
        self.limits
    }

    /// Resolves a single local `#/...` JSON pointer against the document root (`NGC-013`).
    pub fn resolve_local_ref<'a>(&'a self, pointer: &str) -> Result<&'a Value, ExtractError> {
        let Some(rest) = pointer.strip_prefix("#/") else {
            return Err(ExtractError::ExternalRefRejected(clip_str(pointer)));
        };
        let mut cursor = &self.root;
        for raw_seg in rest.split('/') {
            let seg = unescape_pointer_segment(raw_seg, pointer)?;
            cursor = match cursor {
                Value::Object(map) => map
                    .get(&seg)
                    .ok_or_else(|| ExtractError::UnresolvedRef(clip_str(pointer)))?,
                Value::Array(arr) => {
                    let idx = seg
                        .parse::<usize>()
                        .map_err(|_| ExtractError::UnresolvedRef(clip_str(pointer)))?;
                    arr.get(idx)
                        .ok_or_else(|| ExtractError::UnresolvedRef(clip_str(pointer)))?
                }
                _ => return Err(ExtractError::UnresolvedRef(clip_str(pointer))),
            };
        }
        Ok(cursor)
    }

    /// Recursively inlines all local `#/...` `$ref` nodes inside `node`, enforcing cycle, hop,
    /// depth, and item caps (`ING-012`, `NGC-013`).
    pub fn inline_refs(&self, node: &Value) -> Result<Value, ExtractError> {
        let mut state = RefExpandState::default();
        self.expand_node(node, 0, 0, false, &mut state)
    }

    fn expand_node(
        &self,
        node: &Value,
        depth: usize,
        hops: usize,
        is_named_map: bool,
        state: &mut RefExpandState,
    ) -> Result<Value, ExtractError> {
        let max_depth = self.limits.max_depth.min(HARD_MAX_STACK_DEPTH);
        if depth > max_depth {
            return Err(ExtractError::DepthExceeded {
                max_depth: self.limits.max_depth,
            });
        }
        state.items_emitted = state.items_emitted.saturating_add(1);
        if state.items_emitted > self.limits.max_items {
            return Err(ExtractError::ItemLimitExceeded {
                max_items: self.limits.max_items,
            });
        }

        match node {
            Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => Ok(node.clone()),
            Value::Array(items) => {
                let mut out = Vec::with_capacity(items.len());
                for item in items {
                    out.push(self.expand_node(
                        item,
                        depth.saturating_add(1),
                        hops,
                        false,
                        state,
                    )?);
                }
                Ok(Value::Array(out))
            }
            Value::Object(map) => {
                if !is_named_map && let Some(ref_val) = map.get("$ref") {
                    let Some(ref_str) = ref_val.as_str() else {
                        return Err(ExtractError::InvalidStructure(
                            "`$ref` keyword must be a string".to_owned(),
                        ));
                    };
                    if state.stack.iter().any(|seen| seen == ref_str) {
                        return Err(ExtractError::CyclicRef(clip_str(ref_str)));
                    }
                    let next_hops = hops.saturating_add(1);
                    if next_hops > self.limits.max_ref_hops {
                        return Err(ExtractError::RefHopLimitExceeded {
                            max_ref_hops: self.limits.max_ref_hops,
                        });
                    }
                    let target = self.resolve_local_ref(ref_str)?;
                    state.stack.push(ref_str.to_owned());
                    let resolved =
                        self.expand_node(target, depth.saturating_add(1), next_hops, false, state);
                    state.stack.pop();
                    let mut expanded = resolved?;
                    if map.len() > 1
                        && let Value::Object(ref mut resolved_map) = expanded
                    {
                        for (k, v) in map {
                            if k == "$ref" {
                                continue;
                            }
                            let child = self.expand_node(
                                v,
                                depth.saturating_add(1),
                                hops,
                                k == "properties" || k == "$defs",
                                state,
                            )?;
                            resolved_map.insert(k.clone(), child);
                        }
                    }
                    return Ok(expanded);
                }

                let mut out = Map::with_capacity(map.len());
                for (k, v) in map {
                    let child_is_named_map =
                        !is_named_map && (k == "properties" || k == "$defs" || k == "definitions");
                    let expanded_val = self.expand_node(
                        v,
                        depth.saturating_add(1),
                        hops,
                        child_is_named_map,
                        state,
                    )?;
                    out.insert(k.clone(), expanded_val);
                }
                Ok(Value::Object(out))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use proptest::prelude::*;
    use serde_json::json;

    use super::*;

    fn fixture_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/openapi")
    }

    // spec: ING-010, NGC-010
    #[test]
    fn parses_shop_document_records_servers_and_computes_blake3_spec_hash() {
        let bytes = std::fs::read(fixture_dir().join("shop.openapi.json")).expect("read shop");
        let doc = OpenApiDocument::parse(&bytes).expect("parse shop");
        assert_eq!(doc.openapi_version(), "3.1.0");
        assert_eq!(doc.servers(), &["https://api.shop.internal/v1".to_owned()]);
        let expected_hash = blake3::hash(&bytes).to_hex().to_string();
        assert_eq!(doc.spec_hash(), expected_hash);

        // OpenAPI 3.0.x is accepted too; 2.0 and 4.0 are rejected.
        let v30 = br#"{"openapi":"3.0.3","info":{"title":"t","version":"1"},"paths":{}}"#;
        assert_eq!(
            OpenApiDocument::parse(v30)
                .expect("3.0.3")
                .openapi_version(),
            "3.0.3"
        );
        let swagger2 = br#"{"swagger":"2.0","info":{"title":"t","version":"1"},"paths":{}}"#;
        assert!(matches!(
            OpenApiDocument::parse(swagger2),
            Err(ExtractError::MissingField("openapi"))
        ));
        let v20 = br#"{"openapi":"2.0.0","info":{"title":"t","version":"1"},"paths":{}}"#;
        assert!(matches!(
            OpenApiDocument::parse(v20),
            Err(ExtractError::UnsupportedOpenApiVersion(_))
        ));
    }

    // spec: NGC-013, ING-012
    #[test]
    fn ref_table_handles_local_unescaping_and_rejects_external_cycles_and_hop_overflow() {
        let raw = json!({
            "openapi": "3.1.0",
            "info": { "title": "Refs", "version": "1.0.0" },
            "paths": {},
            "components": {
                "schemas": {
                    "a/b": { "type": "string", "minLength": 2 },
                    "tilde~name": { "$ref": "#/components/schemas/a~1b" },
                    "tilde~1literal": { "type": "integer" }
                }
            }
        });
        let doc = OpenApiDocument::parse(&serde_json::to_vec(&raw).expect("json")).expect("parse");

        // `#/components/schemas/a~1b` unescapes `~1` -> `/`
        let resolved = doc
            .inline_refs(&json!({ "$ref": "#/components/schemas/tilde~0name" }))
            .expect("inline escaped ref");
        assert_eq!(resolved, json!({ "type": "string", "minLength": 2 }));

        // RFC 6901 order: `~01` -> `~1`, NOT `/`
        let resolved_literal = doc
            .inline_refs(&json!({ "$ref": "#/components/schemas/tilde~01literal" }))
            .expect("inline ~01");
        assert_eq!(resolved_literal, json!({ "type": "integer" }));

        // External / remote / file / relative / bare-# refs are all rejected
        for bad_ref in [
            "https://evil.example/schema.json#/Thing",
            "http://169.254.169.254/latest/meta-data",
            "file:///etc/passwd",
            "other.json#/components/schemas/Customer",
            "#",
            "",
        ] {
            let err = doc
                .inline_refs(&json!({ "$ref": bad_ref }))
                .expect_err("external ref must fail");
            assert!(
                matches!(err, ExtractError::ExternalRefRejected(_)),
                "unexpected error for {bad_ref}: {err:?}"
            );
        }

        // Missing local target -> UnresolvedRef
        assert!(matches!(
            doc.inline_refs(&json!({ "$ref": "#/components/schemas/DoesNotExist" })),
            Err(ExtractError::UnresolvedRef(_))
        ));
    }

    // spec: ING-012, NGC-013
    #[test]
    fn hostile_corpus_terminates_within_bounds_and_rejects_oversized_50mib_slice() {
        let hostile = fixture_dir().join("hostile");

        let deep = std::fs::read(hostile.join("deep_nesting.json")).expect("deep");
        assert!(matches!(
            OpenApiDocument::parse(&deep),
            Err(ExtractError::DepthExceeded { .. })
        ));

        let wide = std::fs::read(hostile.join("wide_object.json")).expect("wide");
        assert!(matches!(
            OpenApiDocument::parse(&wide),
            Err(ExtractError::ItemLimitExceeded { .. })
        ));

        let huge_enum = std::fs::read(hostile.join("huge_enum.json")).expect("huge_enum");
        assert!(matches!(
            OpenApiDocument::parse(&huge_enum),
            Err(ExtractError::ItemLimitExceeded { .. })
        ));

        let oversized = std::fs::read(hostile.join("oversized_10mib.json")).expect("10mib");
        assert!(matches!(
            OpenApiDocument::parse(&oversized),
            Err(ExtractError::TooLarge { .. })
        ));

        // 50 MiB string slice rejected immediately before JSON parsing
        let fifty_mib = vec![b'a'; 50 * 1024 * 1024];
        assert!(matches!(
            OpenApiDocument::parse(&fifty_mib),
            Err(ExtractError::TooLarge { .. })
        ));

        // Cyclic and long-chain documents parse at top level, then fail boundedly when resolving schemas
        let cyclic_bytes = std::fs::read(hostile.join("cyclic_schema.json")).expect("cyclic");
        let cyclic_doc = OpenApiDocument::parse(&cyclic_bytes).expect("parse cyclic");
        assert!(matches!(
            cyclic_doc.inline_refs(&json!({ "$ref": "#/components/schemas/NodeA" })),
            Err(ExtractError::CyclicRef(_))
        ));

        let chain_bytes = std::fs::read(hostile.join("long_ref_chain.json")).expect("chain");
        let chain_doc = OpenApiDocument::parse(&chain_bytes).expect("parse chain");
        assert!(matches!(
            chain_doc.inline_refs(&json!({ "$ref": "#/components/schemas/Hop0" })),
            Err(ExtractError::RefHopLimitExceeded { .. })
        ));

        // Internal servers document succeeds boundedly and records the untrusted server URLs
        let servers_bytes = std::fs::read(hostile.join("internal_servers.json")).expect("servers");
        let servers_doc = OpenApiDocument::parse(&servers_bytes).expect("parse internal servers");
        assert_eq!(servers_doc.servers().len(), 3);
        assert_eq!(
            servers_doc.servers()[0],
            "http://169.254.169.254/latest/meta-data/iam/security-credentials/"
        );

        // Exponential diamond-$ref DAG bomb is stopped by max_items during inline_refs
        let mut schemas = serde_json::Map::new();
        for i in 0..20 {
            let next_ref = format!("#/components/schemas/D{}", i + 1);
            schemas.insert(
                format!("D{i}"),
                json!({
                    "type": "object",
                    "properties": {
                        "left": { "$ref": next_ref },
                        "right": { "$ref": next_ref }
                    }
                }),
            );
        }
        schemas.insert("D20".to_owned(), json!({ "type": "string" }));
        let bomb_raw = json!({
            "openapi": "3.1.0",
            "info": { "title": "Diamond", "version": "1.0.0" },
            "paths": {},
            "components": { "schemas": schemas }
        });
        let bomb_doc =
            OpenApiDocument::parse(&serde_json::to_vec(&bomb_raw).expect("json")).expect("parse");
        assert!(matches!(
            bomb_doc.inline_refs(&json!({ "$ref": "#/components/schemas/D0" })),
            Err(ExtractError::ItemLimitExceeded { .. })
        ));

        // Exact `max_bytes` and `max_ref_hops` boundary checks (`==` succeeds, `+1` fails).
        let minimal_bytes = serde_json::to_vec(&json!({
            "openapi": "3.1.0",
            "info": { "title": "Exact", "version": "1.0.0" },
            "paths": {}
        }))
        .expect("json");
        assert!(
            OpenApiDocument::parse_with_limits(
                &minimal_bytes,
                ExtractLimits {
                    max_bytes: minimal_bytes.len(),
                    ..ExtractLimits::default()
                },
            )
            .is_ok()
        );
        assert!(matches!(
            OpenApiDocument::parse_with_limits(
                &minimal_bytes,
                ExtractLimits {
                    max_bytes: minimal_bytes.len() - 1,
                    ..ExtractLimits::default()
                },
            ),
            Err(ExtractError::TooLarge { .. })
        ));

        let exact_hops_doc = OpenApiDocument::parse_with_limits(
            &serde_json::to_vec(&json!({
                "openapi": "3.1.0",
                "info": { "title": "Hops", "version": "1.0.0" },
                "paths": {},
                "components": {
                    "schemas": {
                        "H0": { "$ref": "#/components/schemas/H1" },
                        "H1": { "$ref": "#/components/schemas/H2" },
                        "H2": { "type": "string" }
                    }
                }
            }))
            .expect("json"),
            ExtractLimits {
                max_ref_hops: 2,
                ..ExtractLimits::default()
            },
        )
        .expect("parse");
        assert!(
            exact_hops_doc
                .inline_refs(&json!({ "$ref": "#/components/schemas/H0" }))
                .is_err()
        );
        let enough_hops_doc = OpenApiDocument::parse_with_limits(
            &serde_json::to_vec(&json!({
                "openapi": "3.1.0",
                "info": { "title": "Hops", "version": "1.0.0" },
                "paths": {},
                "components": {
                    "schemas": {
                        "H0": { "$ref": "#/components/schemas/H1" },
                        "H1": { "$ref": "#/components/schemas/H2" },
                        "H2": { "type": "string" }
                    }
                }
            }))
            .expect("json"),
            ExtractLimits {
                max_ref_hops: 3,
                ..ExtractLimits::default()
            },
        )
        .expect("parse");
        assert_eq!(
            enough_hops_doc
                .inline_refs(&json!({ "$ref": "#/components/schemas/H0" }))
                .expect("3 hops allowed"),
            json!({ "type": "string" })
        );
    }

    proptest! {
        #![proptest_config(ProptestConfig {
            failure_persistence: None,
            ..ProptestConfig::default()
        })]

        // spec: ING-012
        #[test]
        fn proptest_random_bytes_never_panic(bytes in proptest::collection::vec(any::<u8>(), 0..4096)) {
            let limits = ExtractLimits {
                max_bytes: 2048,
                max_depth: 16,
                max_items: 256,
                max_ref_hops: 8,
            };
            let _ = OpenApiDocument::parse_with_limits(&bytes, limits);
        }
    }
}
