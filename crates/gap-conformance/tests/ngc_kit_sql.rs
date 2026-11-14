//! Black-box NGC connector conformance kit run against `gap-ngc-sql` (`WP-M3-05`, `NGC-096`, `CMP-050..052`).

use std::path::PathBuf;
use std::sync::Arc;

use gap_conformance::ngc_kit::{CHECKS, Outcome, Profile, SkipReason, id, run_kit};
use gap_ngc_sdk::core::{ServerConfig, ServerCore};
use gap_ngc_sdk::testing::in_memory_grpc;
use gap_ngc_sql::{
    ColumnDef, Dialect, OperationConfig, OperationKind, SqlConfig, SqlConnector, SqlType,
    SqliteDriver,
};

const MAX_IN_FLIGHT: u32 = 8;
const MAX_BATCH_SIZE: u32 = 50;
const MAX_PAGE_SIZE: u32 = 100;
const MAX_RESPONSE_BYTES: u64 = 768;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn shop_sql() -> String {
    std::fs::read_to_string(repo_root().join("tests/fixtures/sql/shop.sql")).expect("read shop.sql")
}

fn sql_profile() -> Profile {
    Profile::from_path(&repo_root().join("conformance/ngc/sql.kit.toml"))
        .expect("parse conformance/ngc/sql.kit.toml")
}

fn customer_columns() -> Vec<ColumnDef> {
    vec![
        ColumnDef {
            name: "id".to_owned(),
            sql_type: SqlType::Integer,
            nullable: false,
        },
        ColumnDef {
            name: "name".to_owned(),
            sql_type: SqlType::Text,
            nullable: false,
        },
        ColumnDef {
            name: "tier".to_owned(),
            sql_type: SqlType::Text,
            nullable: false,
        },
        ColumnDef {
            name: "credit_cents".to_owned(),
            sql_type: SqlType::Integer,
            nullable: false,
        },
        ColumnDef {
            name: "region".to_owned(),
            sql_type: SqlType::Text,
            nullable: false,
        },
        ColumnDef {
            name: "active".to_owned(),
            sql_type: SqlType::Boolean,
            nullable: false,
        },
        ColumnDef {
            name: "notes".to_owned(),
            sql_type: SqlType::Text,
            nullable: true,
        },
    ]
}

fn build_shop_connector() -> SqlConnector<SqliteDriver> {
    let conn = rusqlite::Connection::open_in_memory().expect("open in-memory sqlite");
    conn.execute_batch(&shop_sql()).expect("seed shop.sql");
    let driver = SqliteDriver::from_connection(conn).expect("enable query_only");
    let cols = customer_columns();
    let config = SqlConfig {
        connector_id: "gap-ngc-sql-shop".to_owned(),
        dialect: Dialect::Sqlite,
        operations: vec![
            OperationConfig {
                name: "customer".to_owned(),
                table: "customers".to_owned(),
                kind: OperationKind::GetByKey,
                columns: cols.clone(),
                key_columns: vec!["id".to_owned()],
                row_cap: 100,
                statement_timeout_ms: 5_000,
                simulated_latency_ms: 0,
            },
            OperationConfig {
                name: "customers".to_owned(),
                table: "customers".to_owned(),
                kind: OperationKind::List,
                columns: cols.clone(),
                key_columns: vec!["id".to_owned()],
                row_cap: 100,
                statement_timeout_ms: 5_000,
                simulated_latency_ms: 0,
            },
            OperationConfig {
                name: "slow".to_owned(),
                table: "customers".to_owned(),
                kind: OperationKind::List,
                columns: cols,
                key_columns: vec!["id".to_owned()],
                row_cap: 100,
                statement_timeout_ms: 5_000,
                simulated_latency_ms: 5_000,
            },
        ],
        max_in_flight: MAX_IN_FLIGHT,
        max_batch_size: MAX_BATCH_SIZE,
        max_page_size: MAX_PAGE_SIZE,
        max_response_bytes: MAX_RESPONSE_BYTES,
        ..SqlConfig::default()
    };
    SqlConnector::new(config, driver).expect("construct SqlConnector")
}

// spec: NGC-096, CMP-050, CMP-051, CMP-052, NGC-010, NGC-012, NGC-013, NGC-020, NGC-021, NGC-022, NGC-023, NGC-025, NGC-026, NGC-027, NGC-031, NGC-050, NGC-060, NGC-061, NGC-070, INV-12, INV-15, INV-18
#[tokio::test(start_paused = true)]
async fn ngc_kit_sql_connector_is_compatible_with_zero_uncovered_checks() {
    let connector = build_shop_connector();
    let server_cfg = ServerConfig::default().with_max_in_flight(MAX_IN_FLIGHT as usize);
    let core = Arc::new(ServerCore::new(connector, server_cfg));
    let (client, handle) = in_memory_grpc(core);

    let profile = sql_profile();
    let report = run_kit(&client, &profile).await;

    assert!(
        report.compatible(),
        "SqlConnector failed NGC kit:\n{}",
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
