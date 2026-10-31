//! The `conformance/gql` corpus run against the in-process offline server (`--offline`), plus the
//! cross-cutting checks a black-box fixture cannot express (`GQL-072` message bounds, malformed-input
//! robustness). Test names contain `conformance_gql` (acceptance filter of `WP-M1-02`).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeSet;
use std::path::PathBuf;

use gap_conformance::cli::{EXIT_OK, run_cli_from_args};
use gap_conformance::driver::ConformanceDriver;
use gap_conformance::fixture::{
    FixtureExecution, FixtureRequest, LoadedFixture, load_fixtures_dir,
};
use gap_conformance::offline::{OfflineDriver, group_by_schema};
use serde_json::Value;

fn corpus_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../conformance/gql")
}

fn requests(fixture: &LoadedFixture) -> Vec<&FixtureRequest> {
    match &fixture.execution {
        FixtureExecution::Single { request, .. } => vec![request],
        FixtureExecution::Steps(steps) => steps.iter().map(|s| &s.request).collect(),
    }
}

// spec: GQL-001, GQL-002, GQL-003, GQL-040, GQL-041, GQL-050, GQL-061, GQL-070
#[tokio::test]
async fn conformance_gql_core_corpus_passes_against_the_offline_server() {
    let dir = corpus_dir();
    let report_path =
        std::env::temp_dir().join(format!("gap-conformance-gql-{}.json", std::process::id()));
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let code = run_cli_from_args(
        [
            "gap-conformance".as_ref(),
            "--class".as_ref(),
            "Core".as_ref(),
            "--offline".as_ref(),
            dir.as_os_str(),
            "--report".as_ref(),
            report_path.as_os_str(),
        ],
        &mut stdout,
        &mut stderr,
    )
    .await;
    let out = String::from_utf8_lossy(&stdout);
    let report: Value =
        serde_json::from_str(&std::fs::read_to_string(&report_path).expect("report"))
            .expect("report json");
    std::fs::remove_file(&report_path).ok();
    let failing: Vec<String> = report["fixtures"]
        .as_array()
        .expect("fixtures")
        .iter()
        .filter(|f| f["verdict"] != "pass")
        .map(|f| format!("{}: {}", f["file"], f["detail"]))
        .collect();
    assert_eq!(
        code,
        EXIT_OK,
        "{out}\n{}\nfailing: {failing:#?}",
        String::from_utf8_lossy(&stderr)
    );
    assert!(failing.is_empty(), "{failing:#?}");

    let total = report["summary"]["total"].as_u64().expect("total");
    assert!(
        total >= 50,
        "the Core corpus must stay broad: {total} fixtures"
    );
    let covered: BTreeSet<&str> = report["coverage"]
        .as_object()
        .expect("coverage")
        .iter()
        .filter(|(_, entry)| entry["covered"] == true)
        .map(|(id, _)| id.as_str())
        .collect();
    for id in [
        "GQL-001", "GQL-002", "GQL-003", "GQL-040", "GQL-041", "GQL-050", "GQL-061", "GQL-070",
    ] {
        assert!(
            covered.contains(id),
            "Core corpus must cover {id}; covered: {covered:?}"
        );
    }
}

// spec: GQL-072, INV-15
#[tokio::test]
async fn conformance_gql_error_messages_are_short_and_carry_a_correlation_id() {
    let dir = corpus_dir();
    let fixtures = load_fixtures_dir(&dir).expect("corpus loads");
    let groups = group_by_schema(&dir, &fixtures).expect("schemas read");
    let principals = BTreeSet::new();
    let mut checked = 0_usize;
    for group in groups.values() {
        let driver = OfflineDriver::new(&group.name, &group.sdl, &principals).expect("server");
        for fixture in &group.fixtures {
            for request in requests(fixture) {
                let observed = driver.execute(request, None).await.expect("response");
                for error in observed.errors.as_array().expect("errors array") {
                    let message = error["message"].as_str().expect("message");
                    assert!(
                        message.chars().count() <= 300,
                        "{}: error message longer than 300 characters: {message}",
                        fixture.relative_path
                    );
                    assert!(
                        error["extensions"]["correlationId"]
                            .as_str()
                            .is_some_and(|c| !c.is_empty()),
                        "{}: error without extensions.correlationId: {error}",
                        fixture.relative_path
                    );
                    assert!(
                        error["extensions"]["code"]
                            .as_str()
                            .is_some_and(|c| c.starts_with("GAP_")),
                        "{}: error without a GAP_ code: {error}",
                        fixture.relative_path
                    );
                    checked += 1;
                }
            }
        }
    }
    assert!(
        checked >= 30,
        "the corpus must exercise many error responses: {checked}"
    );
}

// spec: GQL-001, GQL-002, GQL-003
#[tokio::test]
async fn conformance_gql_malformed_documents_never_cause_a_server_error() {
    let dir = corpus_dir();
    let fixtures = load_fixtures_dir(&dir).expect("corpus loads");
    let groups = group_by_schema(&dir, &fixtures).expect("schemas read");
    let principals = BTreeSet::new();
    let mut mutated = 0_usize;
    for group in groups.values() {
        let driver = OfflineDriver::new(&group.name, &group.sdl, &principals).expect("server");
        for fixture in &group.fixtures {
            for request in requests(fixture) {
                let chars: Vec<char> = request.query.chars().collect();
                // Truncation at every character boundary of short documents, a coarse stride of long ones;
                // plus every single-character deletion of short documents.
                let stride = if chars.len() <= 160 {
                    1
                } else {
                    chars.len() / 40
                };
                let mut variants: Vec<String> = (0..=chars.len())
                    .step_by(stride)
                    .map(|n| chars[..n].iter().collect())
                    .collect();
                if chars.len() <= 160 {
                    variants.extend((0..chars.len()).map(|skip| {
                        chars
                            .iter()
                            .enumerate()
                            .filter(|(i, _)| *i != skip)
                            .map(|(_, c)| *c)
                            .collect()
                    }));
                }
                for query in variants {
                    let mut mutant = request.clone();
                    mutant.query = query.clone();
                    let observed = driver.execute(&mutant, None).await.expect("response");
                    assert!(
                        matches!(observed.status, 200 | 400 | 422),
                        "status {} for mutated document {query:?} of {}",
                        observed.status,
                        fixture.relative_path
                    );
                    if observed.status != 200 {
                        assert!(observed.data_null, "request error carries data: {query:?}");
                        assert!(
                            !observed.errors.as_array().expect("errors").is_empty(),
                            "request error without errors: {query:?}"
                        );
                    }
                    mutated += 1;
                }
            }
        }
    }
    assert!(mutated > 2_000, "mutation coverage too small: {mutated}");
}
