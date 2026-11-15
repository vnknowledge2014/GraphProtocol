#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::time::Duration;

use gap_core::clock::Timestamp;
use gap_core::ids::{TaskId, TenantId, TenantScope};
use tempfile::TempDir;

use super::{
    DEFAULT_APPROVAL_TTL_MS, DEFAULT_TASK_RETENTION_MS, FsArtifactBlobStore,
    INLINE_ARTIFACT_MAX_BYTES, InMemoryTaskStore, SqliteTaskStore, StoredArtifact, TaskEventRecord,
    TaskQueryFilter, TaskRecord, TaskStatus, TaskStore, TaskTransitionRequest,
};
use crate::error::StoreError;

fn tenant(slug: &str) -> TenantScope {
    TenantId::new(slug).expect("valid tenant").scope()
}

fn ts(ms: i64) -> Timestamp {
    Timestamp::from_unix_millis(ms).expect("valid timestamp")
}

// spec: RUN-080, INV-13
#[test]
fn task_fsm_exhaustive_transition_matrix_and_store_object_safety() {
    fn _assert_dyn(_store: &dyn TaskStore) {}

    for &from in TaskStatus::ALL {
        for &to in TaskStatus::ALL {
            let expected = match from {
                TaskStatus::Submitted => matches!(
                    to,
                    TaskStatus::Working | TaskStatus::Rejected | TaskStatus::Canceled
                ),
                TaskStatus::Working => matches!(
                    to,
                    TaskStatus::InputRequired
                        | TaskStatus::AuthRequired
                        | TaskStatus::Completed
                        | TaskStatus::Failed
                        | TaskStatus::Canceled
                ),
                TaskStatus::InputRequired => matches!(
                    to,
                    TaskStatus::Working
                        | TaskStatus::Canceled
                        | TaskStatus::Rejected
                        | TaskStatus::Failed
                ),
                TaskStatus::AuthRequired => {
                    matches!(
                        to,
                        TaskStatus::Working | TaskStatus::Canceled | TaskStatus::Failed
                    )
                }
                TaskStatus::Completed
                | TaskStatus::Failed
                | TaskStatus::Canceled
                | TaskStatus::Rejected => false,
            };
            assert_eq!(
                from.can_transition_to(to),
                expected,
                "FSM transition {from:?} -> {to:?}"
            );
            assert_eq!(TaskStatus::from_wire_str(from.as_wire_str()), Some(from));
        }
    }
}

fn exercise_store_backend(store: &dyn TaskStore) {
    let t1 = tenant("alpha");
    let t2 = tenant("beta");
    let tid = TaskId::new_v7();
    let t0 = ts(1_700_000_000_000);

    let initial_rec = TaskRecord {
        task_id: tid,
        status: TaskStatus::Submitted,
        payload_json: r#"{"step":0}"#.to_owned(),
        lease_owner: None,
        lease_expires_at: None,
        updated_at: t0,
    };
    let initial_evt = TaskEventRecord {
        task_id: tid,
        seq: 1,
        status: TaskStatus::Submitted,
        payload_json: r#"{"kind":"submitted"}"#.to_owned(),
        emitted_at: t0,
    };
    store.put_task(&t1, &initial_rec, &initial_evt).unwrap();

    // INV-09: Same TaskId in another tenant stays isolated.
    assert!(store.get_task(&t2, tid).unwrap().is_none());
    assert!(store.events_from(&t2, tid, 1).unwrap().is_empty());

    // Invalid jump Submitted -> Completed rejected.
    let err = store
        .transition_task(
            &t1,
            &TaskTransitionRequest {
                task_id: tid,
                to_status: TaskStatus::Completed,
                payload_json: None,
                lease_owner: None,
                lease_expires_at: None,
                event_payload_json: "{}".to_owned(),
                now: ts(1_700_000_001_000),
            },
        )
        .unwrap_err();
    assert_eq!(err, StoreError::Conflict);

    // Valid Submitted -> Working.
    let (working_rec, working_evt) = store
        .transition_task(
            &t1,
            &TaskTransitionRequest {
                task_id: tid,
                to_status: TaskStatus::Working,
                payload_json: Some(r#"{"step":1}"#.to_owned()),
                lease_owner: Some(Some("worker-1".to_owned())),
                lease_expires_at: Some(Some(ts(1_700_000_031_000))),
                event_payload_json: r#"{"kind":"working"}"#.to_owned(),
                now: ts(1_700_000_001_000),
            },
        )
        .unwrap();
    assert_eq!(working_rec.status, TaskStatus::Working);
    assert_eq!(working_evt.seq, 2);

    // Worker 2 cannot steal active lease before expiry; worker 1 can renew; after expiry worker 2 claims.
    assert!(
        !store
            .claim_lease(
                &t1,
                tid,
                "worker-2",
                ts(1_700_000_010_000),
                ts(1_700_000_040_000)
            )
            .unwrap()
    );
    assert!(
        store
            .renew_lease(&t1, tid, "worker-1", ts(1_700_000_041_000))
            .unwrap()
    );
    assert!(
        !store
            .renew_lease(&t1, tid, "worker-2", ts(1_700_000_050_000))
            .unwrap()
    );
    assert!(
        store
            .claim_lease(
                &t1,
                tid,
                "worker-2",
                ts(1_700_000_041_000),
                ts(1_700_000_071_000)
            )
            .unwrap()
    );

    // Recoverable tasks include Working.
    assert_eq!(store.list_recoverable_tasks(&t1).unwrap().len(), 1);

    // Transition Working -> InputRequired -> expires after DEFAULT_APPROVAL_TTL_MS into Rejected.
    let (_, input_evt) = store
        .transition_task(
            &t1,
            &TaskTransitionRequest {
                task_id: tid,
                to_status: TaskStatus::InputRequired,
                payload_json: None,
                lease_owner: None,
                lease_expires_at: None,
                event_payload_json: r#"{"approval":"required"}"#.to_owned(),
                now: ts(1_700_000_050_000),
            },
        )
        .unwrap();
    assert_eq!(input_evt.seq, 3);
    assert!(
        store
            .expire_stale_approvals(&t1, ts(1_700_000_060_000), DEFAULT_APPROVAL_TTL_MS)
            .unwrap()
            .is_empty()
    );
    let expired = store
        .expire_stale_approvals(
            &t1,
            ts(1_700_000_050_000 + DEFAULT_APPROVAL_TTL_MS),
            DEFAULT_APPROVAL_TTL_MS,
        )
        .unwrap();
    assert_eq!(expired.len(), 1);
    assert_eq!(expired[0].0.status, TaskStatus::Rejected);
    assert_eq!(expired[0].1.seq, 4);

    // Terminal state rejects further transitions.
    let term_err = store
        .transition_task(
            &t1,
            &TaskTransitionRequest {
                task_id: tid,
                to_status: TaskStatus::Working,
                payload_json: None,
                lease_owner: None,
                lease_expires_at: None,
                event_payload_json: "{}".to_owned(),
                now: ts(1_700_000_100_000),
            },
        )
        .unwrap_err();
    assert_eq!(term_err, StoreError::Conflict);

    // Replay strictly ascending seq from offset.
    let replay_all = store.events_from(&t1, tid, 1).unwrap();
    assert_eq!(
        replay_all.iter().map(|e| e.seq).collect::<Vec<_>>(),
        vec![1, 2, 3, 4]
    );
    let replay_tail = store.events_from(&t1, tid, 3).unwrap();
    assert_eq!(
        replay_tail.iter().map(|e| e.seq).collect::<Vec<_>>(),
        vec![3, 4]
    );

    // List filter + retention pruning after 30 days.
    assert_eq!(
        store
            .list_tasks(
                &t1,
                &TaskQueryFilter {
                    status: Some(TaskStatus::Rejected),
                    limit: Some(10),
                },
            )
            .unwrap()
            .len(),
        1
    );
    let pruned_early = store
        .prune_terminal_tasks(
            &t1,
            ts(1_700_000_050_000 + DEFAULT_APPROVAL_TTL_MS + 1000),
            DEFAULT_TASK_RETENTION_MS,
        )
        .unwrap();
    assert_eq!(pruned_early, 0);
    let pruned_due = store
        .prune_terminal_tasks(
            &t1,
            ts(1_700_000_050_000 + DEFAULT_APPROVAL_TTL_MS + DEFAULT_TASK_RETENTION_MS),
            DEFAULT_TASK_RETENTION_MS,
        )
        .unwrap();
    assert_eq!(pruned_due, 1);
    assert!(store.get_task(&t1, tid).unwrap().is_none());
}

// spec: RUN-080, RUN-081, RUN-085, RUN-086, INV-09, INV-13
#[test]
fn task_store_memory_and_sqlite_wal_durability_leases_and_retention() {
    let mem = InMemoryTaskStore::new();
    exercise_store_backend(&mem);

    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("tasks.sqlite3");
    let sql = SqliteTaskStore::open(&db_path).unwrap();
    exercise_store_backend(&sql);

    // Re-open persisted WAL DB to prove durability across restarts.
    let tid = TaskId::new_v7();
    let t = tenant("persist");
    let now = ts(1_700_001_000_000);
    let rec = TaskRecord {
        task_id: tid,
        status: TaskStatus::Submitted,
        payload_json: r#"{"resumable":true}"#.to_owned(),
        lease_owner: None,
        lease_expires_at: None,
        updated_at: now,
    };
    let evt = TaskEventRecord {
        task_id: tid,
        seq: 1,
        status: TaskStatus::Submitted,
        payload_json: r#"{"init":true}"#.to_owned(),
        emitted_at: now,
    };
    sql.put_task(&t, &rec, &evt).unwrap();
    drop(sql);

    let reopened = SqliteTaskStore::open(&db_path).unwrap();
    let loaded = reopened.get_task(&t, tid).unwrap().unwrap();
    assert_eq!(loaded, rec);
    assert_eq!(reopened.events_from(&t, tid, 1).unwrap(), vec![evt]);
    let _ = Duration::from_secs(1);
}

// spec: RUN-086, INV-09
#[test]
fn task_artifact_blob_store_offloads_over_1mib_and_enforces_tenant_isolation() {
    let dir = TempDir::new().unwrap();
    let blobs = FsArtifactBlobStore::new(dir.path().join("blobs")).unwrap();
    let t_a = tenant("tenant-a");
    let t_b = tenant("tenant-b");
    let tid = TaskId::new_v7();

    let small = br#"{"small":true}"#;
    match blobs
        .put_artifact(&t_a, tid, "application/json", small)
        .unwrap()
    {
        StoredArtifact::Inline { size_bytes, .. } => assert_eq!(size_bytes, small.len()),
        StoredArtifact::Reference(_) => panic!("<= 1 MiB artifact should remain inline"),
    }

    let large = vec![b'x'; INLINE_ARTIFACT_MAX_BYTES + 128];
    let StoredArtifact::Reference(ref_meta) = blobs
        .put_artifact(&t_a, tid, "application/octet-stream", &large)
        .unwrap()
    else {
        panic!("> 1 MiB artifact must be stored as blob ref");
    };
    assert!(ref_meta.uri.starts_with("file://"));
    assert_eq!(ref_meta.sha256.len(), 64);
    assert_eq!(blobs.read_artifact(&t_a, tid, &ref_meta).unwrap(), large);

    // Other tenant cannot read tenant-a's blob reference.
    assert_eq!(
        blobs.read_artifact(&t_b, tid, &ref_meta).unwrap_err(),
        StoreError::NotFound
    );
}
