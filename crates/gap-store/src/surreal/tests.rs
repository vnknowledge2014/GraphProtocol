//! Unit and conformance tests for the optional SurrealDB 3.x backend (`ADR-020`, `ADR-002`).

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::Arc;
use std::thread;

use gap_core::clock::{Clock, ManualClock, Timestamp};
use gap_core::ids::{TaskId, TenantId};

use crate::conformance::run_context_store_suite;
use crate::error::StoreError;
use crate::store::ContextStore;
use crate::surreal::{
    HttpSurrealTransport, SURREAL_SCHEMA_SURREALQL, SurrealConfig, SurrealContextStore,
    SurrealTaskStore, SurrealTransport, tenant_record_key, tokenize_alphanumeric,
};
use crate::tasks::{
    TaskEventRecord, TaskQueryFilter, TaskRecord, TaskStatus, TaskStore, TaskTransitionRequest,
};
use crate::types::{Caller, NewEdge, NewNode, NodeId, NodeType, Provenance, ScopeSpec};

// spec: INV-09, ING-065, ING-066, ING-067
#[test]
fn surreal_passes_reusable_context_store_conformance_suite() {
    let report = run_context_store_suite(|clock: Arc<ManualClock>| {
        let clock_dyn: Arc<dyn Clock> = clock;
        SurrealContextStore::open(SurrealConfig::in_memory(), clock_dyn)
    });
    assert!(
        report.failed.is_empty(),
        "surreal conformance failures: {:?}",
        report.failed
    );
    assert_eq!(report.passed, 6);
}

// spec: RUN-080, RUN-081
#[test]
fn surreal_task_store_covers_full_lifecycle_leases_and_pruning() {
    let tenant_a = TenantId::new("acme").expect("tenant").scope();
    let tenant_b = TenantId::new("globex").expect("tenant").scope();
    let clock = Arc::new(ManualClock::new(1_700_000_000_000));
    let clock_dyn: Arc<dyn Clock> = Arc::clone(&clock) as Arc<dyn Clock>;
    let ctx_store = SurrealContextStore::open(SurrealConfig::in_memory(), clock_dyn).unwrap();
    let task_store = SurrealTaskStore::from_context_store(&ctx_store);

    let tid1 = TaskId::from_unix_ms(1_700_000_000_000, [0x11; 10]);
    let tid2 = TaskId::from_unix_ms(1_700_000_000_010, [0x22; 10]);
    let t0 = clock.now_timestamp();

    task_store
        .put_task(
            &tenant_a,
            &TaskRecord {
                task_id: tid1,
                status: TaskStatus::Working,
                payload_json: r#"{"step":0}"#.to_owned(),
                lease_owner: Some("worker-1".to_owned()),
                lease_expires_at: Some(Timestamp::from_unix_millis(1_700_000_030_000).unwrap()),
                updated_at: t0,
            },
            &TaskEventRecord {
                task_id: tid1,
                seq: 1,
                status: TaskStatus::Working,
                payload_json: r#"{"init":true}"#.to_owned(),
                emitted_at: t0,
            },
        )
        .expect("put working task");

    // Cross-tenant isolation (`INV-09`)
    assert!(task_store.get_task(&tenant_b, tid1).unwrap().is_none());
    assert!(
        task_store
            .list_recoverable_tasks(&tenant_b)
            .unwrap()
            .is_empty()
    );

    // Worker 2 cannot hijack active lease before expiry; worker 1 can checkpoint progress
    let before_exp = Timestamp::from_unix_millis(1_700_000_010_000).unwrap();
    assert!(
        !task_store
            .claim_lease(
                &tenant_a,
                tid1,
                "worker-2",
                before_exp,
                Timestamp::from_unix_millis(1_700_000_040_000).unwrap(),
            )
            .unwrap()
    );
    let (cp_rec, cp_ev) = task_store
        .checkpoint_progress(
            &tenant_a,
            tid1,
            "worker-1",
            r#"{"step":1}"#,
            r#"{"pct":50}"#,
            before_exp,
        )
        .unwrap();
    assert_eq!(cp_ev.seq, 2);
    assert_eq!(cp_rec.payload_json, r#"{"step":1}"#);
    assert!(
        task_store
            .checkpoint_progress(&tenant_a, tid1, "worker-2", "{}", "{}", before_exp,)
            .is_err()
    );

    // Recoverable task list includes tid1 while Working
    assert_eq!(
        task_store.list_recoverable_tasks(&tenant_a).unwrap().len(),
        1
    );

    // Complete tid1 and insert stale InputRequired tid2 for expiry + prune verification
    let done_ts = Timestamp::from_unix_millis(1_700_000_020_000).unwrap();
    let (done_rec, done_ev) = task_store
        .transition_task(
            &tenant_a,
            &TaskTransitionRequest {
                task_id: tid1,
                to_status: TaskStatus::Completed,
                payload_json: Some(r#"{"done":true}"#.to_owned()),
                lease_owner: None,
                lease_expires_at: None,
                event_payload_json: r#"{"done":true}"#.to_owned(),
                now: done_ts,
            },
        )
        .unwrap();
    assert_eq!(done_rec.status, TaskStatus::Completed);
    assert_eq!(done_ev.seq, 3);
    assert_eq!(task_store.events_from(&tenant_a, tid1, 2).unwrap().len(), 2);

    task_store
        .put_task(
            &tenant_a,
            &TaskRecord {
                task_id: tid2,
                status: TaskStatus::InputRequired,
                payload_json: "{}".to_owned(),
                lease_owner: None,
                lease_expires_at: None,
                updated_at: t0,
            },
            &TaskEventRecord {
                task_id: tid2,
                seq: 1,
                status: TaskStatus::InputRequired,
                payload_json: "{}".to_owned(),
                emitted_at: t0,
            },
        )
        .unwrap();

    let later = Timestamp::from_unix_millis(1_700_000_100_000).unwrap();
    let expired = task_store
        .expire_stale_approvals(&tenant_a, later, 10_000)
        .unwrap();
    assert_eq!(expired.len(), 1);
    assert_eq!(expired[0].0.status, TaskStatus::Rejected);

    let all_tasks = task_store
        .list_tasks(&tenant_a, &TaskQueryFilter::default())
        .unwrap();
    assert_eq!(all_tasks.len(), 2);

    let pruned = task_store
        .prune_terminal_tasks(&tenant_a, later, 5_000)
        .unwrap();
    assert_eq!(pruned, 1);
    assert!(task_store.get_task(&tenant_a, tid1).unwrap().is_none());
}

// spec: INV-09, OPS-042
#[test]
fn surreal_schema_quotas_and_http_wire_transport_work_cleanly() {
    // 1. Every index in SURREAL_SCHEMA_SURREALQL leads with `tenant` (`INV-09`).
    for line in SURREAL_SCHEMA_SURREALQL.lines() {
        if let Some(after_fields) = line.split("FIELDS ").nth(1) {
            let first_field = after_fields.split(',').next().unwrap_or("").trim();
            assert!(
                first_field.starts_with("tenant"),
                "Surreal index must lead with `tenant`: {line}"
            );
        }
    }

    // 2. Hostile tenant / ID key escaping is deterministic and collision-resistant.
    let k1 = tenant_record_key("nodes", "acme", "svc:'; DROP TABLE nodes;--");
    let k2 = tenant_record_key("nodes", "globex", "svc:'; DROP TABLE nodes;--");
    assert_ne!(k1, k2);
    assert!(k1.starts_with("nodes:t"));
    assert_eq!(
        tokenize_alphanumeric("zephyr OR * NEAR(foo:bar)"),
        vec!["zephyr", "or", "near", "foo", "bar"]
    );

    // 3. Quota enforcement cleanly rejects writes over `max_nodes_per_tenant` / `max_edges_per_tenant`.
    let clock: Arc<dyn Clock> = Arc::new(ManualClock::new(1_700_000_000_000));
    let mut cfg = SurrealConfig::in_memory();
    cfg.max_nodes_per_tenant = 1;
    let store = SurrealContextStore::open(cfg, clock).unwrap();
    let tenant = TenantId::new("acme").unwrap().scope();
    let caller = Caller::new("alice");
    let mk_node = |id: &str| NewNode {
        id: Some(NodeId::new(id).unwrap()),
        node_type: NodeType::new("Item").unwrap(),
        label: id.to_owned(),
        summary: None,
        props: Default::default(),
        scope: ScopeSpec::Global,
        provenance: Provenance::new("test"),
    };
    let res = store.put_batch(
        &tenant,
        &caller,
        &[mk_node("n:1"), mk_node("n:2")],
        &[] as &[NewEdge],
    );
    assert_eq!(res, Err(StoreError::QuotaExceeded));
    assert!(
        store
            .get_node(&tenant, &caller, &NodeId::new("n:1").unwrap(), None)
            .unwrap()
            .is_none()
    );

    // 4. HttpSurrealTransport sends valid SurrealDB 3.x HTTP/1.1 `/sql` wire framing.
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let addr = listener.local_addr().expect("local addr");
    let server = thread::spawn(move || {
        let (mut sock, _) = listener.accept().expect("accept");
        let mut buf = [0u8; 4096];
        let mut collected = Vec::new();
        loop {
            let n = sock.read(&mut buf).unwrap_or(0);
            if n == 0 {
                break;
            }
            collected.extend_from_slice(&buf[..n]);
            let req_text = String::from_utf8_lossy(&collected);
            if let Some((head, body)) = req_text.split_once("\r\n\r\n") {
                let expected_len = head
                    .lines()
                    .find_map(|l| l.strip_prefix("Content-Length: "))
                    .and_then(|v| v.trim().parse::<usize>().ok())
                    .unwrap_or(0);
                if body.len() >= expected_len {
                    break;
                }
            }
        }
        let req_str = String::from_utf8(collected).expect("utf8");
        assert!(req_str.starts_with("POST /sql HTTP/1.1\r\n"));
        assert!(req_str.contains("NS: gap\r\n"));
        assert!(req_str.contains("DB: context\r\n"));
        assert!(req_str.contains("Authorization: Basic "));
        assert!(req_str.contains("DEFINE TABLE OVERWRITE nodes SCHEMAFULL;"));

        let resp_body = r#"[{"status":"OK","time":"1ms","result":null}]"#;
        let resp = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            resp_body.len(),
            resp_body
        );
        sock.write_all(resp.as_bytes()).expect("write resp");
    });

    let mut remote_cfg = SurrealConfig::remote(format!("http://{addr}"));
    remote_cfg.username = Some("root".to_owned());
    remote_cfg.password = Some("secret".to_owned());
    let transport = HttpSurrealTransport::new(&remote_cfg).expect("transport");
    let reply = transport
        .execute_surrealql("DEFINE TABLE OVERWRITE nodes SCHEMAFULL;")
        .expect("execute_surrealql");
    assert_eq!(reply.len(), 1);
    assert_eq!(reply[0]["status"], "OK");
    server.join().expect("server thread");
}
