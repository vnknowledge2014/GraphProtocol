//! Integration test invoking `gap_store::conformance::run_context_store_suite` across the public
//! crate boundary against [`gap_store::SqliteContextStore`] (`m2-01-design.md` §10).

#![cfg(feature = "conformance")]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use gap_core::clock::Clock;
use gap_store::conformance::run_context_store_suite;
use gap_store::{Durability, SqliteConfig, SqliteContextStore};
use tempfile::TempDir;

// spec: INV-09, INV-19, ING-062, ING-064, ING-065, ING-066, ING-067, ING-080, ING-081, ING-082, ING-083, ING-084, ING-085, ING-086, ING-087 (ADR-020, ADR-025)
#[test]
fn sqlite_backend_conforms_to_context_store_suite() {
    let dir = TempDir::new().expect("tempdir");
    let seq = AtomicUsize::new(0);

    let report = run_context_store_suite(|clock| {
        let idx = seq.fetch_add(1, Ordering::Relaxed);
        let mut cfg = SqliteConfig::new(dir.path().join(format!("suite_{idx}.db")));
        cfg.durability = Durability::Off;
        let clock_dyn: Arc<dyn Clock> = clock;
        SqliteContextStore::open(cfg, clock_dyn)
    });

    assert!(
        report.is_ok(),
        "SqliteContextStore failed conformance cases: {:?}",
        report.failed
    );
    assert_eq!(report.passed, 6);
}
