//! Kit self-test (`WP-M1-04`): the reference connector passes every check, each deliberate flaw fails exactly its checks,
//! capability gating skips what is not advertised, and a profile that cannot probe an advertised capability is reported.

use std::collections::BTreeSet;
use std::sync::Arc;

use gap_ngc_sdk::core::{ServerConfig, ServerCore};
use gap_ngc_sdk::mock::MockConnector;
use gap_ngc_sdk::testing::in_memory_grpc;

use super::reference::{Flaw, ReferenceConnector};
use super::{CHECKS, KitReport, Outcome, Profile, SkipReason, id, run_kit};

fn reference_profile() -> Profile {
    Profile::from_toml_str(include_str!(
        "../../../../conformance/ngc/sdk-reference.kit.toml"
    ))
    .expect("the shipped reference profile is valid")
}

async fn run(connector: ReferenceConnector, profile: &Profile) -> KitReport {
    let (client, _handle) = connector.in_memory();
    run_kit(&client, profile).await
}

fn names(ids: Vec<&'static str>) -> BTreeSet<&'static str> {
    ids.into_iter().collect()
}

// spec: NGC-091, NGC-010, NGC-012, NGC-013, NGC-020, NGC-021, NGC-022, NGC-023, NGC-025, NGC-026, NGC-027, NGC-030, NGC-031, NGC-061, NGC-070
#[tokio::test(start_paused = true)]
async fn ngc_kit_reference_connector_passes_every_check() {
    let report = run(ReferenceConnector::new(Flaw::None), &reference_profile()).await;
    for r in &report.results {
        assert_eq!(r.outcome, Outcome::Pass, "{}: {:?}", r.info.id, r.outcome);
    }
    assert_eq!(report.results.len(), CHECKS.len());
    assert!(report.compatible());
    assert_eq!(report.connector_id, "gap-ngc-reference");
}

// spec: NGC-091, NGC-010, NGC-012, NGC-013, NGC-020, NGC-021, NGC-022, NGC-023, NGC-025, NGC-026, NGC-027, NGC-030, NGC-031, NGC-061, NGC-070
#[tokio::test(start_paused = true)]
async fn ngc_kit_each_flaw_fails_exactly_its_checks() {
    let profile = reference_profile();
    let mut mismatches = Vec::new();
    let mut failed_somewhere = BTreeSet::new();
    for flaw in Flaw::ALL {
        let report = run(ReferenceConnector::new(flaw), &profile).await;
        let got = names(report.failed());
        let want: BTreeSet<&str> = flaw.targets().iter().copied().collect();
        if got != want || !report.uncovered().is_empty() {
            mismatches.push(format!(
                "{flaw:?}: failed {got:?}, expected {want:?}, uncovered {:?}",
                report.uncovered()
            ));
        }
        failed_somewhere.extend(got);
    }
    assert!(mismatches.is_empty(), "{}", mismatches.join("\n"));
    // "a deliberately broken mock fails each check": no check is left that no flaw can make fail.
    for info in &CHECKS {
        assert!(
            failed_somewhere.contains(info.id),
            "no flaw makes `{}` fail",
            info.id
        );
    }
}

// spec: NGC-091, NGC-015, NGC-030, NGC-031
#[tokio::test(start_paused = true)]
async fn ngc_kit_skips_features_the_connector_does_not_advertise() {
    let report = run(
        ReferenceConnector::new(Flaw::None).minimal(),
        &reference_profile(),
    )
    .await;
    for check in [
        id::FOREACH_CORRECTNESS,
        id::FOREACH_N_PLUS_ONE,
        id::QUERY_ORDER,
        id::MUTATE_IDEMPOTENCY,
        id::PAGE_STABILITY,
        id::LIMITS_RESPONSE_SIZE,
    ] {
        let outcome = &report.result(check).expect("check exists").outcome;
        assert!(
            matches!(outcome, Outcome::Skip(SkipReason::NotApplicable(_))),
            "{check}: {outcome:?}"
        );
    }
    for check in [
        id::DESCRIBE_VALID,
        id::DESCRIBE_STABLE,
        id::HEALTH,
        id::DEADLINE,
        id::CANCEL,
        id::MUTATE_DRY_RUN,
        id::ERRORS_MAPPING,
    ] {
        let outcome = &report.result(check).expect("check exists").outcome;
        assert_eq!(outcome, &Outcome::Pass, "{check}");
    }
    assert!(report.compatible(), "{}", report.render_text());
}

// spec: NGC-091
#[tokio::test(start_paused = true)]
async fn ngc_kit_reports_advertised_capabilities_the_profile_cannot_probe() {
    let profile = Profile::from_toml_str(
        "name = \"bare\"\n[read]\noperation = \"customers\"\nkey_var = \"id\"\nkeys = [1, 2]\n",
    )
    .expect("bare profile");
    let report = run(ReferenceConnector::new(Flaw::None), &profile).await;
    assert!(report.failed().is_empty(), "{}", report.render_text());
    assert_eq!(
        names(report.uncovered()),
        BTreeSet::from([
            id::DEADLINE,
            id::CANCEL,
            id::MUTATE_IDEMPOTENCY,
            id::MUTATE_DRY_RUN,
            id::QUERY_ORDER,
            id::PAGE_STABILITY,
            id::LIMITS_RESPONSE_SIZE,
        ])
    );
    assert!(!report.compatible());
    let json = report.to_json();
    assert_eq!(json["compatible"], false);
    assert!(
        json["passed_spec_ids"]
            .as_array()
            .is_some_and(|ids| ids.iter().any(|v| v == "NGC-020"))
    );
}

// spec: NGC-091, NGC-010
#[tokio::test(start_paused = true)]
async fn ngc_kit_without_a_readable_describe_nothing_else_can_run() {
    let core = Arc::new(ServerCore::new(
        MockConnector::new(),
        ServerConfig::default(),
    ));
    let (client, _handle) = in_memory_grpc(core);
    let report = run_kit(&client, &reference_profile()).await;
    assert_eq!(names(report.failed()), BTreeSet::from([id::DESCRIBE_VALID]));
    assert_eq!(report.uncovered().len(), CHECKS.len() - 1);
    assert!(!report.compatible());
    assert!(report.connector_id.is_empty());
}

// spec: NGC-091, NGC-001
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ngc_kit_cli_passes_the_reference_connector_over_real_loopback_tcp() {
    // Find a free loopback port, then serve the reference connector on it with the SDK's real gRPC server.
    let free = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = free.local_addr().expect("addr");
    drop(free);
    let config =
        ServerConfig::default().with_max_in_flight(super::reference::MAX_IN_FLIGHT as usize);
    let core = Arc::new(ServerCore::new(
        ReferenceConnector::new(Flaw::None).into_mock(),
        config,
    ));
    let server = tokio::spawn(gap_ngc_sdk::grpc::serve_tcp(core, addr));
    let mut listening = false;
    for _ in 0..100 {
        if tokio::net::TcpStream::connect(addr).await.is_ok() {
            listening = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(listening, "the reference server did not start listening");

    let dir = tempfile::tempdir().expect("tempdir");
    let report_path = dir.path().join("kit-report.json");
    let addr_text = addr.to_string();
    let args = [
        "gap-conformance",
        "--connector-kit",
        "--connector",
        addr_text.as_str(),
        "--profile",
        concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../conformance/ngc/sdk-reference.kit.toml"
        ),
        "--report",
        report_path.to_str().expect("utf8 path"),
    ];
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = crate::cli::run_cli_from_args(args, &mut out, &mut err).await;
    let text = String::from_utf8_lossy(&out).into_owned();
    assert_eq!(code, 0, "{text}\n{}", String::from_utf8_lossy(&err));
    assert!(text.contains("RESULT: NGC-compatible"), "{text}");

    let json: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&report_path).expect("report"))
            .expect("json");
    assert_eq!(json["compatible"], true);
    let checks = json["checks"].as_array().expect("checks");
    assert_eq!(checks.len(), CHECKS.len());
    assert!(checks.iter().all(|c| c["status"] == "pass"), "{json}");
    server.abort();
}
