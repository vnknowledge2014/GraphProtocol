//! Recursive conformance fixture loader and schema validation (`VER-010`, `spec/02-gap-ql.md` §11).

use std::collections::BTreeSet;
use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;
use thiserror::Error;

/// Conformance class declared inside a fixture's `"class"` array (`VER-010`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ConformanceClass {
    /// Core GAP-QL parsing, validation, and execution (`Core`).
    Core,
    /// Progressive-disclosure `__metaGraph`, `__search`, and `gap.resolve` (`Discovery`).
    Discovery,
    /// Cross-branch joins and batched `foreach` (`Federation`).
    Federation,
    /// Long-running tasks (`Tasks`).
    Tasks,
    /// Authentication, authorization, redaction, and security invariants (`Secure`).
    Secure,
    /// Inbound MCP 2026-07-28 facade (`Facade-MCP`).
    FacadeMcp,
    /// Inbound/outbound A2A v1.0 facade (`Facade-A2A`).
    FacadeA2a,
    /// Native Graph Connector contract (`NGC`).
    Ngc,
}

impl ConformanceClass {
    /// Canonical wire string for this conformance class.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Core => "Core",
            Self::Discovery => "Discovery",
            Self::Federation => "Federation",
            Self::Tasks => "Tasks",
            Self::Secure => "Secure",
            Self::FacadeMcp => "Facade-MCP",
            Self::FacadeA2a => "Facade-A2A",
            Self::Ngc => "NGC",
        }
    }
}

impl fmt::Display for ConformanceClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Error returned when parsing an unknown [`ConformanceClass`] name.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error(
    "unknown conformance class `{0}` (expected one of: Core, Discovery, Federation, Tasks, Secure, Facade-MCP, Facade-A2A, NGC)"
)]
pub struct UnknownConformanceClass(pub String);

impl FromStr for ConformanceClass {
    type Err = UnknownConformanceClass;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "Core" => Ok(Self::Core),
            "Discovery" => Ok(Self::Discovery),
            "Federation" => Ok(Self::Federation),
            "Tasks" => Ok(Self::Tasks),
            "Secure" => Ok(Self::Secure),
            "Facade-MCP" => Ok(Self::FacadeMcp),
            "Facade-A2A" => Ok(Self::FacadeA2a),
            "NGC" => Ok(Self::Ngc),
            other => Err(UnknownConformanceClass(other.to_owned())),
        }
    }
}

impl Serialize for ConformanceClass {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for ConformanceClass {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        raw.parse().map_err(serde::de::Error::custom)
    }
}

/// GraphQL-over-HTTP request payload inside a conformance fixture (`spec/02-gap-ql.md` §§1, 11).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FixtureRequest {
    /// GraphQL executable document string.
    pub query: String,
    /// Optional GraphQL variables map/value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub variables: Option<Value>,
    /// Optional selected operation name.
    #[serde(
        default,
        rename = "operationName",
        skip_serializing_if = "Option::is_none"
    )]
    pub operation_name: Option<String>,
    /// Optional GraphQL `extensions` object (e.g. `extensions.gap`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extensions: Option<Value>,
}

/// Single sequential `{request, expect}` step inside a multi-step fixture (`spec/02-gap-ql.md` §11).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FixtureStep {
    /// Request sent on this step.
    pub request: FixtureRequest,
    /// Expected subset envelope for this step's response.
    pub expect: Value,
}

/// Exclusive execution payload of a fixture: either a single `request` + `expect` or non-empty `steps`.
#[derive(Debug, Clone, PartialEq)]
pub enum FixtureExecution {
    /// Single request and expected response subset.
    Single {
        /// Request sent to target.
        request: FixtureRequest,
        /// Expected subset matched against `ObservedResponse`.
        expect: Value,
    },
    /// Sequential steps executed in order on the same principal; stops at the first failure.
    Steps(Vec<FixtureStep>),
}

/// Validated conformance fixture loaded from disk (`VER-010`, `spec/02-gap-ql.md` §11).
#[derive(Debug, Clone, PartialEq)]
pub struct LoadedFixture {
    /// Filesystem path of the JSON file.
    pub path: PathBuf,
    /// Portable forward-slash relative path under the fixtures root (used in reports and sorting).
    pub relative_path: String,
    /// Human-readable fixture name (`"name"`).
    pub name: String,
    /// Non-empty conformance classes (`"class"` — never derived from directory names).
    pub class: Vec<ConformanceClass>,
    /// Normative spec requirement IDs exercised by this fixture (`"spec"`).
    pub spec: Vec<String>,
    /// Optional relative GraphQL schema path (`"schema"`).
    pub schema: Option<String>,
    /// Optional logical principal name resolved via `--principals` (`"principal"`).
    pub principal: Option<String>,
    /// Single-step or multi-step execution specification.
    pub execution: FixtureExecution,
}

/// Specific reason a fixture file or directory could not be loaded (`VER-010`).
#[derive(Debug, Error)]
pub enum FixtureErrorKind {
    /// Underlying filesystem read failure.
    #[error("I/O error: {0}")]
    Io(#[source] std::io::Error),
    /// Malformed JSON syntax.
    #[error("invalid JSON: {0}")]
    InvalidJson(String),
    /// Top-level JSON value was not an object.
    #[error("top-level fixture value must be a JSON object")]
    InvalidRootType,
    /// Unrecognized top-level property.
    #[error("unknown top-level key `{key}`")]
    UnknownTopLevelKey {
        /// Offending property name.
        key: String,
    },
    /// Required `"name"` field was omitted.
    #[error("missing required field `name`")]
    MissingName,
    /// `"name"` field was empty or whitespace-only.
    #[error("field `name` must not be empty")]
    EmptyName,
    /// Required `"class"` field was omitted.
    #[error("missing required field `class`")]
    MissingClass,
    /// `"class"` array was empty.
    #[error("field `class` must be a non-empty array")]
    EmptyClass,
    /// `"class"` contained an unrecognized class name.
    #[error("{0}")]
    UnknownClass(#[from] UnknownConformanceClass),
    /// Required `"spec"` field was omitted.
    #[error("missing required field `spec`")]
    MissingSpec,
    /// `"spec"` array was empty.
    #[error("field `spec` must be a non-empty array")]
    EmptySpec,
    /// `"spec"` array contained a non-string JSON value.
    #[error(
        "invalid `spec[{index}]` entry `{entry}`: expected a string matching ^[A-Z][A-Z0-9]*-[0-9]{{2,3}}$"
    )]
    InvalidSpecEntryType {
        /// Zero-based index inside `"spec"`.
        index: usize,
        /// Serialized JSON representation of the offending entry.
        entry: String,
    },
    /// `"spec"` array contained a string not matching `^[A-Z][A-Z0-9]*-[0-9]{2,3}$`.
    #[error("invalid `spec[{index}]` ID `{id}`: must match ^[A-Z][A-Z0-9]*-[0-9]{{2,3}}$")]
    InvalidSpecId {
        /// Zero-based index inside `"spec"`.
        index: usize,
        /// Offending spec ID string.
        id: String,
    },
    /// Both `"request"` and `"steps"` were present.
    #[error("fixture specifies both `request` and `steps`; exactly one is allowed")]
    BothRequestAndSteps,
    /// Neither `"request"` nor `"steps"` was present.
    #[error("fixture specifies neither `request` nor `steps`; exactly one is required")]
    NeitherRequestNorSteps,
    /// `"request"` was used without `"expect"`.
    #[error("fixture with `request` is missing required field `expect`")]
    MissingExpect,
    /// Top-level `"expect"` was supplied alongside `"steps"`.
    #[error("fixture with `steps` must not also define top-level `expect`")]
    UnexpectedExpectWithSteps,
    /// `"steps"` array was empty.
    #[error("field `steps` must be a non-empty array")]
    EmptySteps,
    /// A field had the wrong JSON type.
    #[error("field `{field}` must be {expected}")]
    InvalidFieldType {
        /// Field name.
        field: &'static str,
        /// Expected JSON type description.
        expected: &'static str,
    },
    /// Invalid `request` object structure.
    #[error("invalid `request`: {0}")]
    InvalidRequest(String),
    /// Invalid item inside `steps`.
    #[error("invalid `steps[{index}]`: {reason}")]
    InvalidStep {
        /// Zero-based step index.
        index: usize,
        /// Parse/validation failure detail.
        reason: String,
    },
}

/// Typed fixture loading error always naming the offending path (`VER-010`).
#[derive(Debug, Error)]
#[error("fixture `{}`: {kind}", path.display())]
pub struct FixtureLoadError {
    /// Offending file or directory path.
    pub path: PathBuf,
    /// Detailed error classification.
    #[source]
    pub kind: FixtureErrorKind,
}

/// Allowed top-level JSON keys in a conformance fixture (`VER-010`, `spec/02-gap-ql.md` §11).
const ALLOWED_TOP_LEVEL_KEYS: &[&str] = &[
    "name",
    "class",
    "spec",
    "schema",
    "principal",
    "request",
    "steps",
    "expect",
];

/// Returns `true` iff `id` matches `^[A-Z][A-Z0-9]*-[0-9]{2,3}$` (`VER-010`).
///
/// Examples of valid IDs: `GQL-070`, `INV-15`, `ADR-002`, `VER-010`.
#[must_use]
pub fn is_valid_spec_id(id: &str) -> bool {
    let Some((prefix, digits)) = id.split_once('-') else {
        return false;
    };
    let Some((&first, rest)) = prefix.as_bytes().split_first() else {
        return false;
    };
    if !first.is_ascii_uppercase() {
        return false;
    }
    if !rest
        .iter()
        .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
    {
        return false;
    }
    (2..=3).contains(&digits.len()) && digits.bytes().all(|b| b.is_ascii_digit())
}

/// Loads and validates a single conformance fixture JSON file.
///
/// # Errors
///
/// Returns [`FixtureLoadError`] naming `path` if the file cannot be read, contains invalid JSON,
/// or violates the `spec/02-gap-ql.md` §11 / `VER-010` fixture schema.
pub fn load_fixture_file(
    path: &Path,
    relative_path: impl Into<String>,
) -> Result<LoadedFixture, FixtureLoadError> {
    let fail = |kind: FixtureErrorKind| FixtureLoadError {
        path: path.to_path_buf(),
        kind,
    };

    let raw = std::fs::read_to_string(path).map_err(|err| fail(FixtureErrorKind::Io(err)))?;
    let root_val: Value = serde_json::from_str(&raw)
        .map_err(|err| fail(FixtureErrorKind::InvalidJson(err.to_string())))?;

    let Some(map) = root_val.as_object() else {
        return Err(fail(FixtureErrorKind::InvalidRootType));
    };

    for key in map.keys() {
        if !ALLOWED_TOP_LEVEL_KEYS.contains(&key.as_str()) {
            return Err(fail(FixtureErrorKind::UnknownTopLevelKey {
                key: key.clone(),
            }));
        }
    }

    let Some(name_val) = map.get("name") else {
        return Err(fail(FixtureErrorKind::MissingName));
    };
    let Some(name_str) = name_val.as_str() else {
        return Err(fail(FixtureErrorKind::InvalidFieldType {
            field: "name",
            expected: "a string",
        }));
    };
    if name_str.trim().is_empty() {
        return Err(fail(FixtureErrorKind::EmptyName));
    }

    let Some(class_val) = map.get("class") else {
        return Err(fail(FixtureErrorKind::MissingClass));
    };
    let Some(class_arr) = class_val.as_array() else {
        return Err(fail(FixtureErrorKind::InvalidFieldType {
            field: "class",
            expected: "an array of class strings",
        }));
    };
    if class_arr.is_empty() {
        return Err(fail(FixtureErrorKind::EmptyClass));
    }

    let mut classes = Vec::with_capacity(class_arr.len());
    for item in class_arr {
        let Some(raw_cls) = item.as_str() else {
            return Err(fail(FixtureErrorKind::InvalidFieldType {
                field: "class",
                expected: "an array of class strings",
            }));
        };
        let parsed = raw_cls
            .parse::<ConformanceClass>()
            .map_err(|err| fail(FixtureErrorKind::UnknownClass(err)))?;
        classes.push(parsed);
    }

    let Some(spec_val) = map.get("spec") else {
        return Err(fail(FixtureErrorKind::MissingSpec));
    };
    let Some(spec_arr) = spec_val.as_array() else {
        return Err(fail(FixtureErrorKind::InvalidFieldType {
            field: "spec",
            expected: "a non-empty array of spec ID strings",
        }));
    };
    if spec_arr.is_empty() {
        return Err(fail(FixtureErrorKind::EmptySpec));
    }

    let mut seen_specs = BTreeSet::new();
    let mut spec = Vec::with_capacity(spec_arr.len());
    for (index, item) in spec_arr.iter().enumerate() {
        let Some(s) = item.as_str() else {
            return Err(fail(FixtureErrorKind::InvalidSpecEntryType {
                index,
                entry: item.to_string(),
            }));
        };
        if !is_valid_spec_id(s) {
            return Err(fail(FixtureErrorKind::InvalidSpecId {
                index,
                id: s.to_owned(),
            }));
        }
        // Mutation-harness note (D7): collapses intra-fixture duplicate spec IDs to their first
        // occurrence; covered by `collapses_duplicate_spec_ids_to_first_occurrence_and_accepts_valid_shapes`.
        if seen_specs.insert(s.to_owned()) {
            spec.push(s.to_owned());
        }
    }

    let schema = parse_optional_non_empty_string(map.get("schema"), "schema").map_err(&fail)?;
    let principal =
        parse_optional_non_empty_string(map.get("principal"), "principal").map_err(&fail)?;

    let has_request = map.contains_key("request");
    let has_steps = map.contains_key("steps");
    if has_request && has_steps {
        return Err(fail(FixtureErrorKind::BothRequestAndSteps));
    }
    if !has_request && !has_steps {
        return Err(fail(FixtureErrorKind::NeitherRequestNorSteps));
    }

    let execution = if let Some(req_val) = map.get("request") {
        let request: FixtureRequest = serde_json::from_value(req_val.clone())
            .map_err(|err| fail(FixtureErrorKind::InvalidRequest(err.to_string())))?;
        let Some(expect_val) = map.get("expect") else {
            return Err(fail(FixtureErrorKind::MissingExpect));
        };
        if !expect_val.is_object() {
            return Err(fail(FixtureErrorKind::InvalidFieldType {
                field: "expect",
                expected: "a JSON object",
            }));
        }
        FixtureExecution::Single {
            request,
            expect: expect_val.clone(),
        }
    } else {
        if map.contains_key("expect") {
            return Err(fail(FixtureErrorKind::UnexpectedExpectWithSteps));
        }
        let Some(steps_arr) = map.get("steps").and_then(Value::as_array) else {
            return Err(fail(FixtureErrorKind::InvalidFieldType {
                field: "steps",
                expected: "a non-empty array of {request, expect} objects",
            }));
        };
        if steps_arr.is_empty() {
            return Err(fail(FixtureErrorKind::EmptySteps));
        }
        let mut steps = Vec::with_capacity(steps_arr.len());
        for (index, step_val) in steps_arr.iter().enumerate() {
            let step: FixtureStep = serde_json::from_value(step_val.clone()).map_err(|err| {
                fail(FixtureErrorKind::InvalidStep {
                    index,
                    reason: err.to_string(),
                })
            })?;
            if !step.expect.is_object() {
                return Err(fail(FixtureErrorKind::InvalidStep {
                    index,
                    reason: "step `expect` must be a JSON object".to_owned(),
                }));
            }
            steps.push(step);
        }
        FixtureExecution::Steps(steps)
    };

    Ok(LoadedFixture {
        path: path.to_path_buf(),
        relative_path: relative_path.into(),
        name: name_str.to_owned(),
        class: classes,
        spec,
        schema,
        principal,
        execution,
    })
}

fn parse_optional_non_empty_string(
    val: Option<&Value>,
    field: &'static str,
) -> Result<Option<String>, FixtureErrorKind> {
    let Some(v) = val else {
        return Ok(None);
    };
    if v.is_null() {
        return Ok(None);
    }
    let Some(s) = v.as_str() else {
        return Err(FixtureErrorKind::InvalidFieldType {
            field,
            expected: "a non-empty string",
        });
    };
    if s.trim().is_empty() {
        return Err(FixtureErrorKind::InvalidFieldType {
            field,
            expected: "a non-empty string",
        });
    }
    Ok(Some(s.to_owned()))
}

/// Recursively discovers and loads all `*.json` conformance fixtures under `dir` in deterministic
/// lexicographic relative-path order (`VER-010`).
///
/// Non-`.json` files (such as `README.md` or `.graphql` schemas) are ignored.
///
/// # Errors
///
/// Returns [`FixtureLoadError`] naming the first offending path if directory traversal fails or
/// any `*.json` file fails validation.
pub fn load_fixtures_dir(dir: &Path) -> Result<Vec<LoadedFixture>, FixtureLoadError> {
    let mut discovered: Vec<(String, PathBuf)> = Vec::new();
    collect_json_files(dir, dir, &mut discovered)?;
    discovered.sort_by(|(rel_a, _), (rel_b, _)| rel_a.cmp(rel_b));

    let mut fixtures = Vec::with_capacity(discovered.len());
    for (rel_path, full_path) in discovered {
        fixtures.push(load_fixture_file(&full_path, rel_path)?);
    }
    Ok(fixtures)
}

fn collect_json_files(
    root: &Path,
    current: &Path,
    out: &mut Vec<(String, PathBuf)>,
) -> Result<(), FixtureLoadError> {
    let read_dir = std::fs::read_dir(current).map_err(|err| FixtureLoadError {
        path: current.to_path_buf(),
        kind: FixtureErrorKind::Io(err),
    })?;

    let mut entries: Vec<std::fs::DirEntry> = Vec::new();
    for entry_res in read_dir {
        let entry = entry_res.map_err(|err| FixtureLoadError {
            path: current.to_path_buf(),
            kind: FixtureErrorKind::Io(err),
        })?;
        entries.push(entry);
    }
    entries.sort_by_key(std::fs::DirEntry::file_name);

    for entry in entries {
        let path = entry.path();
        let file_type = entry.file_type().map_err(|err| FixtureLoadError {
            path: path.clone(),
            kind: FixtureErrorKind::Io(err),
        })?;
        if file_type.is_dir() {
            collect_json_files(root, &path, out)?;
        } else if file_type.is_file()
            && path.extension().and_then(|ext| ext.to_str()) == Some("json")
        {
            let rel = normalize_relative_path(root, &path);
            out.push((rel, path));
        }
    }
    Ok(())
}

fn normalize_relative_path(root: &Path, path: &Path) -> String {
    let stripped = path.strip_prefix(root).unwrap_or(path);
    let mut parts: Vec<String> = Vec::new();
    for component in stripped.components() {
        if let std::path::Component::Normal(seg) = component {
            parts.push(seg.to_string_lossy().into_owned());
        }
    }
    parts.join("/")
}

#[cfg(test)]
mod tests {
    use std::fs;

    use serde_json::json;
    use tempfile::TempDir;

    use super::*;

    fn write_file(root: &Path, rel: &str, contents: &str) -> PathBuf {
        let full = root.join(rel);
        if let Some(parent) = full.parent() {
            fs::create_dir_all(parent).expect("create parent dirs");
        }
        fs::write(&full, contents).expect("write fixture");
        full
    }

    // spec: GQL-002 (VER-010, VER-012)
    #[test]
    fn loads_recursive_sorted_json_fixtures_and_takes_class_from_field_not_directory() {
        let tmp = TempDir::new().expect("tempdir");

        // Non-JSON files under the directory must be ignored.
        write_file(tmp.path(), "README.md", "# ignored");
        write_file(
            tmp.path(),
            "schemas/billing.graphql",
            "type Query { ok: Boolean }",
        );

        // Place a `Secure` + `NGC` fixture inside a directory named `core/` to prove class comes
        // strictly from the JSON `class` field, never from the directory name.
        write_file(
            tmp.path(),
            "core/z_last.json",
            &json!({
                "name": "multi-step auth flow",
                "class": ["Secure", "NGC"],
                "spec": ["INV-15", "NGC-091"],
                "principal": "agent-admin",
                "steps": [
                    {
                        "request": {"query": "{ gap { ok } }"},
                        "expect": {"status": 200}
                    },
                    {
                        "request": {
                            "query": "mutation Run($x: Int!) { doIt(x: $x) }",
                            "variables": {"x": 7},
                            "operationName": "Run",
                            "extensions": {"gap": {"dryRun": true}}
                        },
                        "expect": {"status": 200, "dataNull": false}
                    }
                ]
            })
            .to_string(),
        );

        write_file(
            tmp.path(),
            "a_first/01_unknown_field.json",
            &json!({
                "name": "unknown field gets didYouMean",
                "class": ["Core"],
                "spec": ["GQL-070", "GQL-062"],
                "schema": "schemas/billing.graphql",
                "principal": "agent-readonly",
                "request": {"query": "{ billing { invoce(id:\"1\"){total} } }"},
                "expect": {"status": 200, "dataNull": true}
            })
            .to_string(),
        );

        write_file(
            tmp.path(),
            "core/a_middle.json",
            &json!({
                "name": "middle fixture",
                "class": ["Discovery"],
                "spec": ["GQL-002"],
                "request": {"query": "{ __metaGraph { generation } }"},
                "expect": {"status": 200}
            })
            .to_string(),
        );

        let loaded = load_fixtures_dir(tmp.path()).expect("fixtures should load");
        let rel_paths: Vec<&str> = loaded.iter().map(|f| f.relative_path.as_str()).collect();
        assert_eq!(
            rel_paths,
            vec![
                "a_first/01_unknown_field.json",
                "core/a_middle.json",
                "core/z_last.json",
            ]
        );

        assert_eq!(loaded[0].name, "unknown field gets didYouMean");
        assert_eq!(loaded[0].class, vec![ConformanceClass::Core]);
        assert_eq!(loaded[0].spec, vec!["GQL-070", "GQL-062"]);
        assert_eq!(loaded[0].schema.as_deref(), Some("schemas/billing.graphql"));
        assert_eq!(loaded[0].principal.as_deref(), Some("agent-readonly"));

        // `core/a_middle.json` has `"class": ["Discovery"]` despite living in `core/`.
        assert_eq!(loaded[1].class, vec![ConformanceClass::Discovery]);

        // `core/z_last.json` has `"class": ["Secure", "NGC"]` and 2 steps.
        assert_eq!(
            loaded[2].class,
            vec![ConformanceClass::Secure, ConformanceClass::Ngc]
        );
        match &loaded[2].execution {
            FixtureExecution::Steps(steps) => {
                assert_eq!(steps.len(), 2);
                assert_eq!(steps[1].request.operation_name.as_deref(), Some("Run"));
            }
            FixtureExecution::Single { .. } => panic!("expected Steps execution"),
        }
    }

    // spec: GQL-002, NGC-091 (VER-010)
    #[test]
    fn accepts_exact_eight_conformance_classes_from_literal_list() {
        let tmp = TempDir::new().expect("tempdir");
        // Literal list per review policy (never derived from implementation constants).
        let valid_classes = [
            ("Core", ConformanceClass::Core),
            ("Discovery", ConformanceClass::Discovery),
            ("Federation", ConformanceClass::Federation),
            ("Tasks", ConformanceClass::Tasks),
            ("Secure", ConformanceClass::Secure),
            ("Facade-MCP", ConformanceClass::FacadeMcp),
            ("Facade-A2A", ConformanceClass::FacadeA2a),
            ("NGC", ConformanceClass::Ngc),
        ];

        for (idx, (raw_name, expected_variant)) in valid_classes.iter().enumerate() {
            let file_name = format!("c_{idx:02}.json");
            let path = write_file(
                tmp.path(),
                &file_name,
                &json!({
                    "name": format!("class check {raw_name}"),
                    "class": [raw_name],
                    "spec": ["GQL-002"],
                    "request": {"query": "{ ok }"},
                    "expect": {"status": 200}
                })
                .to_string(),
            );
            let fixture = load_fixture_file(&path, &file_name).expect("valid class must parse");
            assert_eq!(fixture.class, vec![*expected_variant]);
            assert_eq!(fixture.class[0].to_string(), *raw_name);
        }
    }

    // spec: GQL-002 (VER-010)
    #[test]
    fn rejects_invalid_json_naming_file() {
        let tmp = TempDir::new().expect("tempdir");
        let path = write_file(tmp.path(), "broken.json", "{ not valid json ");
        let err = load_fixtures_dir(tmp.path()).expect_err("invalid JSON must fail");
        assert_eq!(err.path, path);
        assert!(matches!(err.kind, FixtureErrorKind::InvalidJson(_)));
        assert!(err.to_string().contains("broken.json"));
    }

    // spec: GQL-002 (VER-010)
    #[test]
    fn rejects_non_object_root_naming_file() {
        let tmp = TempDir::new().expect("tempdir");
        let path = write_file(tmp.path(), "array_root.json", "[1, 2, 3]");
        let err = load_fixtures_dir(tmp.path()).expect_err("array root must fail");
        assert_eq!(err.path, path);
        assert!(matches!(err.kind, FixtureErrorKind::InvalidRootType));
        assert!(err.to_string().contains("array_root.json"));
    }

    // spec: GQL-002 (VER-010)
    #[test]
    fn rejects_unknown_top_level_key_naming_file() {
        let tmp = TempDir::new().expect("tempdir");
        let path = write_file(
            tmp.path(),
            "unknown_key.json",
            &json!({
                "name": "bad key",
                "class": ["Core"],
                "spec": ["GQL-002"],
                "request": {"query": "{ ok }"},
                "expect": {"status": 200},
                "unexpectedField": true
            })
            .to_string(),
        );
        let err = load_fixtures_dir(tmp.path()).expect_err("unknown top-level key must fail");
        assert_eq!(err.path, path);
        assert!(
            matches!(&err.kind, FixtureErrorKind::UnknownTopLevelKey { key } if key == "unexpectedField")
        );
        assert!(err.to_string().contains("unknown_key.json"));
    }

    // spec: GQL-002 (VER-010)
    #[test]
    fn rejects_missing_name_naming_file() {
        let tmp = TempDir::new().expect("tempdir");
        let path = write_file(
            tmp.path(),
            "missing_name.json",
            &json!({
                "class": ["Core"],
                "spec": ["GQL-002"],
                "request": {"query": "{ ok }"},
                "expect": {"status": 200}
            })
            .to_string(),
        );
        let err = load_fixtures_dir(tmp.path()).expect_err("missing name must fail");
        assert_eq!(err.path, path);
        assert!(matches!(err.kind, FixtureErrorKind::MissingName));
        assert!(err.to_string().contains("missing_name.json"));
    }

    // spec: GQL-002 (VER-010)
    #[test]
    fn rejects_empty_and_whitespace_name_naming_file() {
        for (file, bad_name) in [("empty_name.json", ""), ("blank_name.json", "   \t\n")] {
            let tmp = TempDir::new().expect("tempdir");
            let path = write_file(
                tmp.path(),
                file,
                &json!({
                    "name": bad_name,
                    "class": ["Core"],
                    "spec": ["GQL-002"],
                    "request": {"query": "{ ok }"},
                    "expect": {"status": 200}
                })
                .to_string(),
            );
            let err = load_fixtures_dir(tmp.path()).expect_err("empty/blank name must fail");
            assert_eq!(err.path, path);
            assert!(matches!(err.kind, FixtureErrorKind::EmptyName));
            assert!(err.to_string().contains(file));
        }
    }

    // spec: GQL-002 (VER-010)
    #[test]
    fn rejects_missing_class_naming_file() {
        let tmp = TempDir::new().expect("tempdir");
        let path = write_file(
            tmp.path(),
            "missing_class.json",
            &json!({
                "name": "no class",
                "spec": ["GQL-002"],
                "request": {"query": "{ ok }"},
                "expect": {"status": 200}
            })
            .to_string(),
        );
        let err = load_fixtures_dir(tmp.path()).expect_err("missing class must fail");
        assert_eq!(err.path, path);
        assert!(matches!(err.kind, FixtureErrorKind::MissingClass));
        assert!(err.to_string().contains("missing_class.json"));
    }

    // spec: GQL-002 (VER-010)
    #[test]
    fn rejects_empty_class_naming_file() {
        let tmp = TempDir::new().expect("tempdir");
        let path = write_file(
            tmp.path(),
            "empty_class.json",
            &json!({
                "name": "empty class",
                "class": [],
                "spec": ["GQL-002"],
                "request": {"query": "{ ok }"},
                "expect": {"status": 200}
            })
            .to_string(),
        );
        let err = load_fixtures_dir(tmp.path()).expect_err("empty class must fail");
        assert_eq!(err.path, path);
        assert!(matches!(err.kind, FixtureErrorKind::EmptyClass));
        assert!(err.to_string().contains("empty_class.json"));
    }

    // spec: GQL-002 (VER-010)
    #[test]
    fn rejects_unknown_class_and_case_mismatch_naming_file() {
        for bad_class in ["core", "ngc", "FacadeMCP", "GraphQL", ""] {
            let tmp = TempDir::new().expect("tempdir");
            let path = write_file(
                tmp.path(),
                "bad_class.json",
                &json!({
                    "name": "bad class",
                    "class": [bad_class],
                    "spec": ["GQL-002"],
                    "request": {"query": "{ ok }"},
                    "expect": {"status": 200}
                })
                .to_string(),
            );
            let err = load_fixtures_dir(tmp.path()).expect_err("unknown class must fail");
            assert_eq!(err.path, path);
            assert!(
                matches!(&err.kind, FixtureErrorKind::UnknownClass(UnknownConformanceClass(c)) if c == bad_class)
            );
            assert!(err.to_string().contains("bad_class.json"));
        }
    }

    // spec: GQL-002 (VER-010)
    #[test]
    fn rejects_both_request_and_steps_naming_file() {
        let tmp = TempDir::new().expect("tempdir");
        let path = write_file(
            tmp.path(),
            "both_req_steps.json",
            &json!({
                "name": "both",
                "class": ["Core"],
                "spec": ["GQL-002"],
                "request": {"query": "{ ok }"},
                "expect": {"status": 200},
                "steps": [{"request": {"query": "{ ok }"}, "expect": {"status": 200}}]
            })
            .to_string(),
        );
        let err = load_fixtures_dir(tmp.path()).expect_err("both request and steps must fail");
        assert_eq!(err.path, path);
        assert!(matches!(err.kind, FixtureErrorKind::BothRequestAndSteps));
        assert!(err.to_string().contains("both_req_steps.json"));
    }

    // spec: GQL-002 (VER-010)
    #[test]
    fn rejects_neither_request_nor_steps_naming_file() {
        let tmp = TempDir::new().expect("tempdir");
        let path = write_file(
            tmp.path(),
            "neither_req_steps.json",
            &json!({
                "name": "neither",
                "class": ["Core"],
                "spec": ["GQL-002"],
                "expect": {"status": 200}
            })
            .to_string(),
        );
        let err = load_fixtures_dir(tmp.path()).expect_err("neither request nor steps must fail");
        assert_eq!(err.path, path);
        assert!(matches!(err.kind, FixtureErrorKind::NeitherRequestNorSteps));
        assert!(err.to_string().contains("neither_req_steps.json"));
    }

    // spec: GQL-002 (VER-010)
    #[test]
    fn rejects_empty_steps_naming_file() {
        let tmp = TempDir::new().expect("tempdir");
        let path = write_file(
            tmp.path(),
            "empty_steps.json",
            &json!({
                "name": "empty steps",
                "class": ["Core"],
                "spec": ["GQL-002"],
                "steps": []
            })
            .to_string(),
        );
        let err = load_fixtures_dir(tmp.path()).expect_err("empty steps must fail");
        assert_eq!(err.path, path);
        assert!(matches!(err.kind, FixtureErrorKind::EmptySteps));
        assert!(err.to_string().contains("empty_steps.json"));
    }

    // spec: GQL-002 (VER-010)
    #[test]
    fn rejects_missing_expect_with_request_and_top_level_expect_with_steps() {
        let tmp_missing = TempDir::new().expect("tempdir");
        let missing_path = write_file(
            tmp_missing.path(),
            "missing_expect.json",
            &json!({
                "name": "no expect",
                "class": ["Core"],
                "spec": ["GQL-002"],
                "request": {"query": "{ ok }"}
            })
            .to_string(),
        );
        let err1 = load_fixtures_dir(tmp_missing.path()).expect_err("missing expect must fail");
        assert_eq!(err1.path, missing_path);
        assert!(matches!(err1.kind, FixtureErrorKind::MissingExpect));
        assert!(err1.to_string().contains("missing_expect.json"));

        let tmp_extra = TempDir::new().expect("tempdir");
        let extra_path = write_file(
            tmp_extra.path(),
            "extra_expect.json",
            &json!({
                "name": "extra expect",
                "class": ["Core"],
                "spec": ["GQL-002"],
                "steps": [{"request": {"query": "{ ok }"}, "expect": {"status": 200}}],
                "expect": {"status": 200}
            })
            .to_string(),
        );
        let err2 =
            load_fixtures_dir(tmp_extra.path()).expect_err("top-level expect with steps must fail");
        assert_eq!(err2.path, extra_path);
        assert!(matches!(
            err2.kind,
            FixtureErrorKind::UnexpectedExpectWithSteps
        ));
        assert!(err2.to_string().contains("extra_expect.json"));
    }

    // spec: GQL-002 (VER-010)
    #[test]
    fn rejects_unknown_key_inside_request_or_step_naming_file() {
        let tmp_req = TempDir::new().expect("tempdir");
        let req_path = write_file(
            tmp_req.path(),
            "bad_request.json",
            &json!({
                "name": "bad request",
                "class": ["Core"],
                "spec": ["GQL-002"],
                "request": {"query": "{ ok }", "typoKey": 1},
                "expect": {"status": 200}
            })
            .to_string(),
        );
        let err1 = load_fixtures_dir(tmp_req.path()).expect_err("unknown request key must fail");
        assert_eq!(err1.path, req_path);
        assert!(matches!(err1.kind, FixtureErrorKind::InvalidRequest(_)));

        let tmp_step = TempDir::new().expect("tempdir");
        let step_path = write_file(
            tmp_step.path(),
            "bad_step.json",
            &json!({
                "name": "bad step",
                "class": ["Core"],
                "spec": ["GQL-002"],
                "steps": [{"request": {"query": "{ ok }"}, "expect": {"status": 200}, "extra": 1}]
            })
            .to_string(),
        );
        let err2 = load_fixtures_dir(tmp_step.path()).expect_err("unknown step key must fail");
        assert_eq!(err2.path, step_path);
        assert!(matches!(
            err2.kind,
            FixtureErrorKind::InvalidStep { index: 0, .. }
        ));
    }

    // spec: GQL-002 (VER-010)
    #[test]
    fn rejects_invalid_field_types_and_roundtrips_class_serde() {
        for (file, payload) in [
            (
                "bad_name_type.json",
                json!({"name": 42, "class": ["Core"], "spec": ["GQL-002"], "request": {"query": "{ ok }"}, "expect": {"status": 200}}),
            ),
            (
                "bad_class_type.json",
                json!({"name": "x", "class": "Core", "spec": ["GQL-002"], "request": {"query": "{ ok }"}, "expect": {"status": 200}}),
            ),
            (
                "bad_class_elem.json",
                json!({"name": "x", "class": [1], "spec": ["GQL-002"], "request": {"query": "{ ok }"}, "expect": {"status": 200}}),
            ),
            (
                "bad_spec_type.json",
                json!({"name": "x", "class": ["Core"], "spec": "GQL-002", "request": {"query": "{ ok }"}, "expect": {"status": 200}}),
            ),
            (
                "bad_schema_empty.json",
                json!({"name": "x", "class": ["Core"], "spec": ["GQL-002"], "schema": "  ", "request": {"query": "{ ok }"}, "expect": {"status": 200}}),
            ),
            (
                "bad_principal_empty.json",
                json!({"name": "x", "class": ["Core"], "spec": ["GQL-002"], "principal": "", "request": {"query": "{ ok }"}, "expect": {"status": 200}}),
            ),
            (
                "bad_expect_scalar.json",
                json!({"name": "x", "class": ["Core"], "spec": ["GQL-002"], "request": {"query": "{ ok }"}, "expect": 200}),
            ),
            (
                "bad_step_expect_scalar.json",
                json!({"name": "x", "class": ["Core"], "spec": ["GQL-002"], "steps": [{"request": {"query": "{ ok }"}, "expect": [200]}]}),
            ),
        ] {
            let tmp = TempDir::new().expect("tempdir");
            let path = write_file(tmp.path(), file, &payload.to_string());
            let err = load_fixtures_dir(tmp.path()).expect_err("invalid field type must fail");
            assert_eq!(err.path, path);
            assert!(err.to_string().contains(file));
        }

        let encoded = serde_json::to_string(&ConformanceClass::FacadeMcp).expect("serialize class");
        assert_eq!(encoded, "\"Facade-MCP\"");
        let decoded: ConformanceClass = serde_json::from_str(&encoded).expect("deserialize class");
        assert_eq!(decoded, ConformanceClass::FacadeMcp);
        assert!(serde_json::from_str::<ConformanceClass>("\"NotAClass\"").is_err());
    }

    // spec: GQL-002 (VER-010)
    #[test]
    fn rejects_missing_spec_naming_file() {
        let tmp = TempDir::new().expect("tempdir");
        let path = write_file(
            tmp.path(),
            "missing_spec.json",
            &json!({
                "name": "missing spec",
                "class": ["Core"],
                "request": {"query": "{ ok }"},
                "expect": {"status": 200}
            })
            .to_string(),
        );
        let err = load_fixtures_dir(tmp.path()).expect_err("missing spec must fail");
        assert_eq!(err.path, path);
        assert!(matches!(err.kind, FixtureErrorKind::MissingSpec));
        assert!(err.to_string().contains("missing_spec.json"));
    }

    // spec: GQL-002 (VER-010)
    #[test]
    fn rejects_empty_spec_array_naming_file() {
        let tmp = TempDir::new().expect("tempdir");
        let path = write_file(
            tmp.path(),
            "empty_spec.json",
            &json!({
                "name": "empty spec",
                "class": ["Core"],
                "spec": [],
                "request": {"query": "{ ok }"},
                "expect": {"status": 200}
            })
            .to_string(),
        );
        let err = load_fixtures_dir(tmp.path()).expect_err("empty spec array must fail");
        assert_eq!(err.path, path);
        assert!(matches!(err.kind, FixtureErrorKind::EmptySpec));
        assert!(err.to_string().contains("empty_spec.json"));
    }

    // spec: GQL-002 (VER-010)
    #[test]
    fn rejects_non_string_spec_entry_naming_file_and_entry() {
        for (file, bad_entry, expected_rendered) in [
            ("non_string_num.json", json!(123), "123"),
            ("non_string_bool.json", json!(true), "true"),
            ("non_string_null.json", json!(null), "null"),
            (
                "non_string_obj.json",
                json!({"id": "GQL-070"}),
                "{\"id\":\"GQL-070\"}",
            ),
        ] {
            let tmp = TempDir::new().expect("tempdir");
            let path = write_file(
                tmp.path(),
                file,
                &json!({
                    "name": "non-string spec entry",
                    "class": ["Core"],
                    "spec": ["GQL-070", bad_entry],
                    "request": {"query": "{ ok }"},
                    "expect": {"status": 200}
                })
                .to_string(),
            );
            let err = load_fixtures_dir(tmp.path()).expect_err("non-string spec entry must fail");
            assert_eq!(err.path, path);
            assert!(
                matches!(
                    &err.kind,
                    FixtureErrorKind::InvalidSpecEntryType { index: 1, entry }
                        if entry == expected_rendered
                ),
                "unexpected error kind: {:?}",
                err.kind
            );
            let msg = err.to_string();
            assert!(msg.contains(file), "error must name file `{file}`: {msg}");
            assert!(
                msg.contains(expected_rendered),
                "error must name offending entry `{expected_rendered}`: {msg}"
            );
        }
    }

    // spec: GQL-002 (VER-010)
    #[test]
    fn rejects_bad_shape_spec_id_naming_file_and_entry() {
        for bad_id in [
            "not an id",
            "",
            "   ",
            "GQL",
            "GQL-",
            "-15",
            "9GQL-15",
            "GQL-1",
            "GQL_A-15",
            "GQL-1-2",
            "GQL-15 ",
            " GQL-15",
        ] {
            let tmp = TempDir::new().expect("tempdir");
            let path = write_file(
                tmp.path(),
                "bad_shape_spec.json",
                &json!({
                    "name": "bad shape spec",
                    "class": ["Core"],
                    "spec": [bad_id],
                    "request": {"query": "{ ok }"},
                    "expect": {"status": 200}
                })
                .to_string(),
            );
            let err = load_fixtures_dir(tmp.path()).expect_err("bad shape spec ID must fail");
            assert_eq!(err.path, path);
            assert!(
                matches!(
                    &err.kind,
                    FixtureErrorKind::InvalidSpecId { index: 0, id } if id == bad_id
                ),
                "unexpected error kind for `{bad_id}`: {:?}",
                err.kind
            );
            let msg = err.to_string();
            assert!(msg.contains("bad_shape_spec.json"));
            assert!(
                msg.contains(bad_id),
                "error must name offending entry `{bad_id}`: {msg}"
            );
        }
    }

    // spec: GQL-002 (VER-010)
    #[test]
    fn rejects_lowercase_spec_id_naming_file_and_entry() {
        for lower_id in ["gql-070", "Gql-070", "inv-15", "a2A-01"] {
            let tmp = TempDir::new().expect("tempdir");
            let path = write_file(
                tmp.path(),
                "lowercase_spec.json",
                &json!({
                    "name": "lowercase spec",
                    "class": ["Core"],
                    "spec": ["GQL-070", lower_id],
                    "request": {"query": "{ ok }"},
                    "expect": {"status": 200}
                })
                .to_string(),
            );
            let err = load_fixtures_dir(tmp.path()).expect_err("lowercase spec ID must fail");
            assert_eq!(err.path, path);
            assert!(
                matches!(
                    &err.kind,
                    FixtureErrorKind::InvalidSpecId { index: 1, id } if id == lower_id
                ),
                "unexpected error kind for `{lower_id}`: {:?}",
                err.kind
            );
            let msg = err.to_string();
            assert!(msg.contains("lowercase_spec.json"));
            assert!(msg.contains(lower_id));
        }
    }

    // spec: GQL-002 (VER-010)
    #[test]
    fn rejects_four_digit_spec_id_naming_file_and_entry() {
        for four_digit_id in ["GQL-0070", "ADR-0002", "INV-1234"] {
            let tmp = TempDir::new().expect("tempdir");
            let path = write_file(
                tmp.path(),
                "four_digit_spec.json",
                &json!({
                    "name": "four digit spec",
                    "class": ["Core"],
                    "spec": [four_digit_id],
                    "request": {"query": "{ ok }"},
                    "expect": {"status": 200}
                })
                .to_string(),
            );
            let err = load_fixtures_dir(tmp.path()).expect_err("4-digit spec ID must fail");
            assert_eq!(err.path, path);
            assert!(
                matches!(
                    &err.kind,
                    FixtureErrorKind::InvalidSpecId { index: 0, id } if id == four_digit_id
                ),
                "unexpected error kind for `{four_digit_id}`: {:?}",
                err.kind
            );
            let msg = err.to_string();
            assert!(msg.contains("four_digit_spec.json"));
            assert!(msg.contains(four_digit_id));
        }
    }

    // spec: GQL-002 (VER-010)
    #[test]
    fn collapses_duplicate_spec_ids_to_first_occurrence_and_accepts_valid_shapes() {
        let tmp = TempDir::new().expect("tempdir");
        let path = write_file(
            tmp.path(),
            "dedup_spec.json",
            &json!({
                "name": "dedup spec IDs",
                "class": ["Core"],
                "spec": [
                    "GQL-070",
                    "INV-15",
                    "GQL-070",
                    "ADR-002",
                    "VER-010",
                    "INV-15",
                    "A2A-01",
                    "ADR-002"
                ],
                "request": {"query": "{ ok }"},
                "expect": {"status": 200}
            })
            .to_string(),
        );
        let loaded = load_fixture_file(&path, "dedup_spec.json").expect("valid spec IDs must load");
        assert_eq!(
            loaded.spec,
            vec!["GQL-070", "INV-15", "ADR-002", "VER-010", "A2A-01"]
        );
    }

    // spec: GQL-002 (VER-010)
    #[test]
    fn d3a_rejects_spec_id_with_trailing_letter_in_two_char_digits() {
        let tmp = TempDir::new().expect("tempdir");
        let path = write_file(
            tmp.path(),
            "d3_inv_1a.json",
            &json!({
                "name": "bad digit letter",
                "class": ["Core"],
                "spec": ["INV-1A"],
                "request": {"query": "{ ok }"},
                "expect": {"status": 200}
            })
            .to_string(),
        );
        let err = load_fixtures_dir(tmp.path()).expect_err("`INV-1A` must be rejected");
        assert_eq!(err.path, path);
        assert!(matches!(
            &err.kind,
            FixtureErrorKind::InvalidSpecId { index: 0, id } if id == "INV-1A"
        ));
        let msg = err.to_string();
        assert!(msg.contains("d3_inv_1a.json"));
        assert!(msg.contains("INV-1A"));
    }

    // spec: GQL-002 (VER-010)
    #[test]
    fn d3b_rejects_spec_id_with_leading_letter_in_three_char_digits() {
        let tmp = TempDir::new().expect("tempdir");
        let path = write_file(
            tmp.path(),
            "d3_inv_a12.json",
            &json!({
                "name": "bad digit letter",
                "class": ["Core"],
                "spec": ["INV-A12"],
                "request": {"query": "{ ok }"},
                "expect": {"status": 200}
            })
            .to_string(),
        );
        let err = load_fixtures_dir(tmp.path()).expect_err("`INV-A12` must be rejected");
        assert_eq!(err.path, path);
        assert!(matches!(
            &err.kind,
            FixtureErrorKind::InvalidSpecId { index: 0, id } if id == "INV-A12"
        ));
        let msg = err.to_string();
        assert!(msg.contains("d3_inv_a12.json"));
        assert!(msg.contains("INV-A12"));
    }

    // spec: GQL-002 (VER-010)
    #[test]
    fn d3c_rejects_spec_id_with_lowercase_hex_letter_in_digits() {
        let tmp = TempDir::new().expect("tempdir");
        let path = write_file(
            tmp.path(),
            "d3_gql_0x1.json",
            &json!({
                "name": "bad digit hex letter",
                "class": ["Core"],
                "spec": ["GQL-0x1"],
                "request": {"query": "{ ok }"},
                "expect": {"status": 200}
            })
            .to_string(),
        );
        let err = load_fixtures_dir(tmp.path()).expect_err("`GQL-0x1` must be rejected");
        assert_eq!(err.path, path);
        assert!(matches!(
            &err.kind,
            FixtureErrorKind::InvalidSpecId { index: 0, id } if id == "GQL-0x1"
        ));
        let msg = err.to_string();
        assert!(msg.contains("d3_gql_0x1.json"));
        assert!(msg.contains("GQL-0x1"));
    }

    // spec: GQL-002 (VER-010)
    #[test]
    fn d4a_treats_null_principal_as_absent() {
        let tmp = TempDir::new().expect("tempdir");
        let path = write_file(
            tmp.path(),
            "null_principal.json",
            &json!({
                "name": "null principal treated as absent",
                "class": ["Core"],
                "spec": ["GQL-002"],
                "principal": null,
                "request": {"query": "{ ok }"},
                "expect": {"status": 200}
            })
            .to_string(),
        );
        let loaded =
            load_fixture_file(&path, "null_principal.json").expect("null principal must load");
        assert_eq!(loaded.principal, None);
    }

    // spec: GQL-002 (VER-010)
    #[test]
    fn d4b_treats_null_schema_as_absent() {
        let tmp = TempDir::new().expect("tempdir");
        let path = write_file(
            tmp.path(),
            "null_schema.json",
            &json!({
                "name": "null schema treated as absent",
                "class": ["Core"],
                "spec": ["GQL-002"],
                "schema": null,
                "request": {"query": "{ ok }"},
                "expect": {"status": 200}
            })
            .to_string(),
        );
        let loaded = load_fixture_file(&path, "null_schema.json").expect("null schema must load");
        assert_eq!(loaded.schema, None);
    }

    // spec: GQL-002 (VER-010)
    #[test]
    fn h2_load_fixtures_dir_sorts_relative_paths_lexicographically_including_dot_before_slash() {
        let tmp = TempDir::new().expect("tempdir");
        let make_fixture = |name: &str| -> String {
            json!({
                "name": name,
                "class": ["Core"],
                "spec": ["GQL-002"],
                "request": {"query": "{ ok }"},
                "expect": {"status": 200}
            })
            .to_string()
        };

        // Created in reverse order across nested subdirectories, including `a/z.json` vs `a.json`
        // and `b/z.json` vs `b.json` (`.` = 0x2E < `/` = 0x2F, `-` = 0x2D < `.` = 0x2E).
        write_file(tmp.path(), "z/last.json", &make_fixture("f11"));
        write_file(tmp.path(), "m/sub/item.json", &make_fixture("f10"));
        write_file(tmp.path(), "b/z.json", &make_fixture("f09"));
        write_file(tmp.path(), "b/a.json", &make_fixture("f08"));
        write_file(tmp.path(), "b.json", &make_fixture("f07"));
        write_file(tmp.path(), "a0/first.json", &make_fixture("f06"));
        write_file(tmp.path(), "a/z.json", &make_fixture("f05"));
        write_file(tmp.path(), "a/m/deep.json", &make_fixture("f04"));
        write_file(tmp.path(), "a/a.json", &make_fixture("f03"));
        write_file(tmp.path(), "a.json", &make_fixture("f02"));
        write_file(tmp.path(), "a-0.json", &make_fixture("f01"));

        let loaded = load_fixtures_dir(tmp.path()).expect("all 11 fixtures must load");
        let rel_paths: Vec<&str> = loaded.iter().map(|f| f.relative_path.as_str()).collect();
        assert_eq!(
            rel_paths,
            vec![
                "a-0.json",
                "a.json",
                "a/a.json",
                "a/m/deep.json",
                "a/z.json",
                "a0/first.json",
                "b.json",
                "b/a.json",
                "b/z.json",
                "m/sub/item.json",
                "z/last.json",
            ]
        );
    }
}
