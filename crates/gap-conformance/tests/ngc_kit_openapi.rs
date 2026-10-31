//! Black-box NGC connector conformance kit run against `gap-ngc-openapi` (`WP-M1-11`, `NGC-091`, `CMP-030..033`).

use std::path::PathBuf;
use std::sync::Arc;

use futures_util::stream;
use gap_conformance::ngc_kit::{CHECKS, Outcome, Profile, SkipReason, id, run_kit};
use gap_ngc_openapi::{
    AuthConfig, FixtureService, OpenApiConfig, OpenApiConnector, StaticCredentialSource, fixture,
};
use gap_ngc_sdk::core::{ServerConfig, ServerCore};
use gap_ngc_sdk::grpc::serve_incoming_with_shutdown;
use gap_ngc_sdk::testing::in_memory_grpc;

const MAX_IN_FLIGHT: u32 = 8;
const MAX_RESPONSE_BYTES: u64 = 8_192;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn shop_document_bytes() -> Vec<u8> {
    std::fs::read(repo_root().join("tests/fixtures/openapi/shop.openapi.json"))
        .expect("read shop.openapi.json")
}

fn openapi_profile() -> Profile {
    Profile::from_path(&repo_root().join("conformance/ngc/openapi.kit.toml"))
        .expect("parse conformance/ngc/openapi.kit.toml")
}

// spec: NGC-091, CMP-030, CMP-031, CMP-032, CMP-033, NGC-010, NGC-012, NGC-013, NGC-020, NGC-021, NGC-022, NGC-023, NGC-025, NGC-026, NGC-027, NGC-031, NGC-060, NGC-061, NGC-070, INV-12, INV-15, INV-18
#[tokio::test(start_paused = true)]
async fn ngc_kit_openapi_connector_is_compatible_with_zero_uncovered_checks() {
    let fixture = Arc::new(FixtureService::new());
    let creds =
        StaticCredentialSource::new().with_secret("shop_token", "kit-upstream-secret-token");
    let config = OpenApiConfig {
        connector_id: "gap-ngc-openapi-shop".to_owned(),
        base_url: "http://127.0.0.1:18080".to_owned(),
        auth: AuthConfig::Bearer {
            credential: "shop_token".to_owned(),
        },
        max_in_flight: MAX_IN_FLIGHT,
        max_response_bytes: MAX_RESPONSE_BYTES,
        ..OpenApiConfig::default()
    };

    let connector =
        OpenApiConnector::from_document_bytes(&shop_document_bytes(), config, &creds, fixture)
            .expect("build OpenApiConnector<Arc<FixtureService>>");

    let server_cfg = ServerConfig::default().with_max_in_flight(MAX_IN_FLIGHT as usize);
    let core = Arc::new(ServerCore::new(connector, server_cfg));
    let (client, handle) = in_memory_grpc(core);

    let profile = openapi_profile();
    let report = run_kit(&client, &profile).await;

    assert!(
        report.compatible(),
        "OpenApiConnector failed NGC kit:\n{}",
        report.render_text()
    );
    assert!(
        report.uncovered().is_empty(),
        "uncovered checks in report:\n{}",
        report.render_text()
    );
    assert_eq!(report.results.len(), CHECKS.len());

    // `mutate.idempotency` is unadvertised in v0 (read-only connector) -> `NotApplicable`;
    // every other check (including `mutate.dry_run` verifying `UNIMPLEMENTED` per NGC-031) must `Pass`.
    for res in &report.results {
        if res.info.id == id::MUTATE_IDEMPOTENCY {
            assert!(
                matches!(res.outcome, Outcome::Skip(SkipReason::NotApplicable(_))),
                "expected NotApplicable for {}, got {:?}",
                res.info.id,
                res.outcome
            );
        } else {
            assert_eq!(
                res.outcome,
                Outcome::Pass,
                "expected Pass for {}, got {:?} (notes: {:?})",
                res.info.id,
                res.outcome,
                res.notes
            );
        }
    }

    handle.shutdown().await.expect("clean gRPC shutdown");
}

// spec: NGC-091, CMP-030, CMP-033
#[tokio::test]
#[ignore = "needs loopback sockets"]
async fn ngc_kit_openapi_http_upstream_over_loopback_sockets() {
    let fixture = Arc::new(FixtureService::new());
    let rest_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind REST loopback");
    let rest_addr = rest_listener.local_addr().expect("REST local_addr");
    let rest_app = fixture::router(Arc::clone(&fixture));
    let rest_task = tokio::spawn(async move {
        let _ = axum::serve(rest_listener, rest_app).await;
    });

    let creds = StaticCredentialSource::new().with_secret("shop_token", "loopback-secret");
    let config = OpenApiConfig {
        connector_id: "gap-ngc-openapi-loopback".to_owned(),
        base_url: format!("http://{rest_addr}"),
        auth: AuthConfig::Bearer {
            credential: "shop_token".to_owned(),
        },
        max_in_flight: MAX_IN_FLIGHT,
        max_response_bytes: MAX_RESPONSE_BYTES,
        ..OpenApiConfig::default()
    };
    let connector = OpenApiConnector::new_http(&shop_document_bytes(), config, &creds)
        .expect("build OpenApiConnector<HttpUpstream>");

    let grpc_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind gRPC loopback");
    let grpc_addr = grpc_listener.local_addr().expect("gRPC local_addr");
    let incoming = stream::unfold(grpc_listener, |listener| async move {
        let res = listener.accept().await.map(|(stream, _)| stream);
        Some((res, listener))
    });

    let core = Arc::new(ServerCore::new(
        connector,
        ServerConfig::default().with_max_in_flight(MAX_IN_FLIGHT as usize),
    ));
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let grpc_task = tokio::spawn(async move {
        serve_incoming_with_shutdown(core, Box::pin(incoming), async move {
            let _ = shutdown_rx.await;
        })
        .await
    });

    let client =
        gap_conformance::ngc_kit::connect(&grpc_addr.to_string()).expect("connect gRPC client");
    let report = run_kit(&client, &openapi_profile()).await;
    assert!(report.compatible(), "{}", report.render_text());
    assert!(report.uncovered().is_empty(), "{}", report.render_text());

    let _ = shutdown_tx.send(());
    let _ = grpc_task.await;
    rest_task.abort();
}
