//! Differential validation corpus (`GQL-001`, `GQL-002`): every document of
//! `tests/differential/corpus.json` has the verdict the GraphQL specification requires (`expect`);
//! `gap-ql` must produce it, and graphql-js (pinned in `tests/differential/package.json`) must agree except
//! where the corpus documents a deviation in `allow`.
//!
//! The graphql-js leg needs `node` and an installed graphql 16.x package: it is `#[ignore]`d and runs with
//! `GAP_GRAPHQL_JS_DIR=<dir of the graphql package> cargo test -p gap-conformance --test differential_gql -- --ignored`
//! (CI installs it with `npm ci`; offline any local copy of the pinned major works).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Command;

use gap_core::error::ErrorCode;
use gap_ql::{AllowAllVisibility, Schema, Valid, validate_document_with_visibility};
use serde_json::{Map, Value};

fn differential_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/differential")
}

struct Entry {
    id: String,
    doc: String,
    expect: String,
    variables: Map<String, Value>,
    allow: BTreeMap<String, String>,
}

fn load_corpus() -> (String, Vec<Entry>) {
    let dir = differential_dir();
    let corpus: Value = serde_json::from_str(
        &std::fs::read_to_string(dir.join("corpus.json")).expect("corpus.json"),
    )
    .expect("valid corpus json");
    let schema = std::fs::read_to_string(dir.join(corpus["schema"].as_str().expect("schema path")))
        .expect("schema file");
    let entries = corpus["documents"]
        .as_array()
        .expect("documents")
        .iter()
        .map(|d| Entry {
            id: d["id"].as_str().expect("id").to_owned(),
            doc: d["doc"].as_str().expect("doc").to_owned(),
            expect: d["expect"].as_str().expect("expect").to_owned(),
            variables: d["variables"].as_object().cloned().unwrap_or_default(),
            allow: d["allow"]
                .as_object()
                .map(|m| {
                    m.iter()
                        .map(|(k, v)| (k.clone(), v.as_str().expect("allow verdict").to_owned()))
                        .collect()
                })
                .unwrap_or_default(),
        })
        .collect();
    (schema, entries)
}

/// `valid`, `invalid` (parsed but failed validation) or `syntax` (failed to parse), as gap-ql sees it.
fn gap_ql_verdict(schema: &Valid<Schema>, entry: &Entry) -> &'static str {
    match validate_document_with_visibility(
        schema,
        &entry.doc,
        None,
        &entry.variables,
        &AllowAllVisibility::default(),
    ) {
        Ok(_) => "valid",
        Err(errors) if errors.iter().any(|e| e.code() == ErrorCode::ParseError) => "syntax",
        Err(_) => "invalid",
    }
}

fn entry_notes_every_allow(entries: &[Entry], corpus_text: &str) {
    for entry in entries.iter().filter(|e| !e.allow.is_empty()) {
        assert!(
            corpus_text.contains(&format!("\"id\": \"{}\"", entry.id)),
            "{}",
            entry.id
        );
    }
}

// spec: GQL-001, GQL-002
#[test]
fn differential_corpus_matches_the_specification_in_gap_ql() {
    let (sdl, entries) = load_corpus();
    let schema = Schema::parse_and_validate(sdl, "schema.graphql").expect("corpus schema is valid");
    assert!(
        entries.len() >= 60,
        "the corpus must stay broad: {} documents",
        entries.len()
    );

    let mut wrong = Vec::new();
    for entry in &entries {
        let got = gap_ql_verdict(&schema, entry);
        if got != entry.expect {
            wrong.push(format!(
                "{}: spec says {}, gap-ql says {got}",
                entry.id, entry.expect
            ));
        }
    }
    assert!(
        wrong.is_empty(),
        "gap-ql disagrees with the GraphQL specification:\n{}",
        wrong.join("\n")
    );
    entry_notes_every_allow(
        &entries,
        &std::fs::read_to_string(differential_dir().join("corpus.json")).expect("corpus"),
    );
}

// spec: GQL-001, GQL-002
#[test]
#[ignore = "needs node and graphql-js: set GAP_GRAPHQL_JS_DIR to a graphql 16.x package directory"]
fn differential_graphql_js_agrees_with_gap_ql() {
    let dir = std::env::var("GAP_GRAPHQL_JS_DIR")
        .expect("GAP_GRAPHQL_JS_DIR must point at an installed graphql 16.x package directory");
    let output = Command::new("node")
        .arg(differential_dir().join("validate.mjs"))
        .env("GAP_GRAPHQL_JS_DIR", dir)
        .output()
        .expect("node must be installed");
    assert!(
        output.status.success(),
        "validate.mjs failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let js: Value = serde_json::from_slice(&output.stdout).expect("validate.mjs prints JSON");
    assert!(
        js["__graphql_js_version"]
            .as_str()
            .is_some_and(|v| v.starts_with("16.")),
        "pinned major is graphql-js 16: {}",
        js["__graphql_js_version"]
    );

    let (sdl, entries) = load_corpus();
    let schema = Schema::parse_and_validate(sdl, "schema.graphql").expect("corpus schema is valid");
    let mut disagreements = Vec::new();
    for entry in &entries {
        let gap = gap_ql_verdict(&schema, entry);
        let reference = js[&entry.id]["verdict"]
            .as_str()
            .expect("graphql-js verdict");
        let allowed_js = entry.allow.get("graphql-js").map(String::as_str);
        let allowed_gap = entry.allow.get("gap-ql").map(String::as_str);
        let js_ok = reference == entry.expect || allowed_js == Some(reference);
        let gap_ok = gap == entry.expect || allowed_gap == Some(gap);
        if !(js_ok && gap_ok) {
            disagreements.push(format!(
                "{}: spec={} graphql-js={reference} gap-ql={gap}",
                entry.id, entry.expect
            ));
        }
    }
    assert!(
        disagreements.is_empty(),
        "unlisted disagreements (add an `allow` entry with a note only for a deliberate deviation):\n{}",
        disagreements.join("\n")
    );
}
