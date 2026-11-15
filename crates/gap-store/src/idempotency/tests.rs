#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::{Arc, Barrier};
use std::thread;
use std::time::Duration;

use gap_core::clock::{Clock, ManualClock, Timestamp};
use gap_core::ids::TenantId;
use tempfile::tempdir;

use crate::error::StoreError;

use super::{
    DEFAULT_IDEMPOTENCY_TTL_MS, IdempotencyOutcome, IdempotencyStatus, IdempotencyStore,
    InMemoryIdempotencyStore, SqliteIdempotencyStore,
};

fn hash_of(tag: &[u8]) -> [u8; 32] {
    *blake3::hash(tag).as_bytes()
}

fn ttl_after(clock: &ManualClock, delta_ms: i64) -> Timestamp {
    let now_ms = clock.now_timestamp().unix_millis();
    Timestamp::from_unix_millis(now_ms + delta_ms).expect("valid future timestamp")
}

fn verify_store_lifecycle(store: &dyn IdempotencyStore, clock: &ManualClock) {
    let acme = TenantId::new("acme").expect("tenant").scope();
    let beta = TenantId::new("beta").expect("tenant").scope();
    let h1 = hash_of(b"op=createOrder;amount=100");
    let h2 = hash_of(b"op=createOrder;amount=200");
    let exp = ttl_after(clock, DEFAULT_IDEMPOTENCY_TTL_MS);

    // 1. Fresh reservation succeeds and returns Reserved.
    assert_eq!(
        store
            .check_or_reserve(&acme, "key-1", h1, exp)
            .expect("reserve"),
        IdempotencyOutcome::Reserved
    );

    // 2. Same tenant + key + identical hash while in progress -> InProgress.
    assert_eq!(
        store
            .check_or_reserve(&acme, "key-1", h1, exp)
            .expect("in progress"),
        IdempotencyOutcome::InProgress
    );

    // 3. Same key + different hash -> Conflict (GAP_IDEMPOTENCY_CONFLICT).
    assert_eq!(
        store
            .check_or_reserve(&acme, "key-1", h2, exp)
            .expect("conflict"),
        IdempotencyOutcome::Conflict
    );

    // 4. Other tenant using identical key & different hash is isolated (INV-09).
    assert_eq!(
        store
            .check_or_reserve(&beta, "key-1", h2, exp)
            .expect("beta reserve"),
        IdempotencyOutcome::Reserved
    );

    // 5. Completing with mismatched hash is rejected with Conflict.
    assert_eq!(
        store.complete(&acme, "key-1", h2, Some("bad")),
        Err(StoreError::Conflict)
    );

    // 6. Completing with matching hash transitions to Completed -> Replay without re-execution.
    store
        .complete(&acme, "key-1", h1, Some("{\"orderId\":\"ord-42\"}"))
        .expect("complete");
    assert_eq!(
        store
            .check_or_reserve(&acme, "key-1", h1, exp)
            .expect("replay"),
        IdempotencyOutcome::Replay {
            result_ref: Some("{\"orderId\":\"ord-42\"}".to_owned()),
        }
    );
    let rec = store
        .get(&acme, "key-1")
        .expect("get")
        .expect("record present");
    assert_eq!(rec.status, IdempotencyStatus::Completed);
    assert_eq!(rec.result_ref.as_deref(), Some("{\"orderId\":\"ord-42\"}"));

    // 7. Advancing clock past 24 h expires entry and allows clean re-reservation with new hash.
    clock.advance(Duration::from_millis(
        u64::try_from(DEFAULT_IDEMPOTENCY_TTL_MS).expect("positive") + 1,
    ));
    assert!(store.get(&acme, "key-1").expect("expired get").is_none());
    let exp2 = ttl_after(clock, DEFAULT_IDEMPOTENCY_TTL_MS);
    assert_eq!(
        store
            .check_or_reserve(&acme, "key-1", h2, exp2)
            .expect("re-reserve"),
        IdempotencyOutcome::Reserved
    );

    // 8. Releasing in-progress reservation removes only matching in-progress entry.
    store
        .release_in_progress(&acme, "key-1", h2)
        .expect("release");
    assert!(store.get(&acme, "key-1").expect("released").is_none());
}

// spec: RUN-072, INV-09
#[test]
fn idempotency_store_trait_object_safe_and_memory_lifecycle() {
    fn _assert_dyn(_store: &dyn IdempotencyStore) {}
    let clock = Arc::new(ManualClock::new(1_700_000_000_000));
    let store = InMemoryIdempotencyStore::with_clock(Arc::clone(&clock) as Arc<dyn Clock>);
    verify_store_lifecycle(&store, &clock);
}

// spec: RUN-072, INV-09
#[test]
fn idempotency_sqlite_persistence_and_ttl_expiry() {
    let dir = tempdir().expect("tempdir");
    let db_path = dir.path().join("idempotency.sqlite");
    let clock = Arc::new(ManualClock::new(1_700_000_000_000));

    {
        let store =
            SqliteIdempotencyStore::open_with_clock(&db_path, Arc::clone(&clock) as Arc<dyn Clock>)
                .expect("open sqlite");
        verify_store_lifecycle(&store, &clock);

        // Write durable completed record for persistence across reopen.
        let acme = TenantId::new("acme").expect("tenant").scope();
        let h_persist = hash_of(b"persist-op");
        let exp = ttl_after(&clock, DEFAULT_IDEMPOTENCY_TTL_MS);
        assert_eq!(
            store
                .check_or_reserve(&acme, "persist-1", h_persist, exp)
                .expect("reserve"),
            IdempotencyOutcome::Reserved
        );
        store
            .complete(&acme, "persist-1", h_persist, Some("{\"ok\":true}"))
            .expect("complete");
    }

    // Reopen database and verify durable replay survives restart.
    let reopened =
        SqliteIdempotencyStore::open_with_clock(&db_path, Arc::clone(&clock) as Arc<dyn Clock>)
            .expect("reopen sqlite");
    let acme = TenantId::new("acme").expect("tenant").scope();
    let h_persist = hash_of(b"persist-op");
    let exp = ttl_after(&clock, DEFAULT_IDEMPOTENCY_TTL_MS);
    assert_eq!(
        reopened
            .check_or_reserve(&acme, "persist-1", h_persist, exp)
            .expect("replayed after reopen"),
        IdempotencyOutcome::Replay {
            result_ref: Some("{\"ok\":true}".to_owned()),
        }
    );
}

// spec: RUN-072, INV-09
#[test]
fn idempotency_concurrent_duplicate_keys_allow_single_reserved_winner() {
    let dir = tempdir().expect("tempdir");
    let db_path = dir.path().join("concurrent_idemp.sqlite");
    let store = Arc::new(SqliteIdempotencyStore::open(&db_path).expect("open sqlite"));

    let workers = 8usize;
    let barrier = Arc::new(Barrier::new(workers));
    let hash = hash_of(b"concurrent-mutation");
    let expires = Timestamp::from_unix_millis(
        gap_core::clock::SystemClock.now_timestamp().unix_millis() + DEFAULT_IDEMPOTENCY_TTL_MS,
    )
    .expect("future ts");

    let mut handles = Vec::with_capacity(workers);
    for _ in 0..workers {
        let store_ref = Arc::clone(&store);
        let b = Arc::clone(&barrier);
        handles.push(thread::spawn(move || {
            let tenant = TenantId::new("acme").expect("tenant").scope();
            b.wait();
            store_ref
                .check_or_reserve(&tenant, "dup-key-1", hash, expires)
                .expect("check_or_reserve")
        }));
    }

    let mut reserved_count = 0usize;
    let mut in_progress_count = 0usize;
    for h in handles {
        match h.join().expect("thread join") {
            IdempotencyOutcome::Reserved => reserved_count += 1,
            IdempotencyOutcome::InProgress => in_progress_count += 1,
            other => panic!("unexpected outcome: {other:?}"),
        }
    }
    assert_eq!(reserved_count, 1);
    assert_eq!(in_progress_count, workers - 1);
}
