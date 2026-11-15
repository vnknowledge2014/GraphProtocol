// N-1 -> N upgrade compatibility and online backup/restore drill tests
// (`WP-M7-07`, `OPS-041`, `OPS-043`, `RUN-091`).

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use gap_core::clock::{Clock, SystemClock, Timestamp};
use gap_core::config::{BranchProtocol, BranchSpec};
use gap_core::ids::{BranchUid, TaskId, TenantId, TenantScope};
use gap_core::snapshot::{SNAPSHOT_FORMAT_VERSION, Snapshot};
use gap_core::traits::BoxFuture;
use gap_mcp::outbound::fake::{FakeCall, FakeMcpServer, FakeTool, ServerEra};
use gap_mcp::outbound::transport::http::HttpTransport;
use gap_mcp::outbound::{McpConfig, McpConnector};
use gap_ngc_openapi::{
    AuthConfig, OpenApiConfig, OpenApiConnector, StaticCredentialSource, Upstream, UpstreamError,
    UpstreamRequest, UpstreamResponse,
};
use gap_server::assemble::{
    BranchBuildError, BranchFactory, BranchRuntime, ServerFile, assemble, wrap_connector,
};
use gap_snapshot::{PublishError, SnapshotHandle};
use gap_store::audit::{
    AuditChain, AuditEntry, AuditLog, AuditVerifyReport, SqliteAuditConfig, SqliteAuditLog,
};
use gap_store::idempotency::{IdempotencyOutcome, IdempotencyStore, SqliteIdempotencyStore};
use gap_store::tasks::{
    SqliteTaskStore, TaskEventRecord, TaskRecord, TaskStatus, TaskStore, TaskTransitionRequest,
};
use gap_store::types::{
    Caller, EdgeId, EdgeResolution, EdgeType, GraphCounts, NewEdge, NewNode, NodeId, NodeType,
    Provenance, ScopeSpec,
};
use gap_store::{ContextStore, FORMAT_VERSION, SqliteConfig, SqliteContextStore, StoreError};
use serde_json::{Map as JsonMap, json};
use tempfile::TempDir;

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("canonical repo root")
}

fn tenant(slug: &str) -> TenantScope {
    TenantId::new(slug).expect("valid tenant").scope()
}

fn ts(ms: i64) -> Timestamp {
    Timestamp::from_unix_millis(ms).expect("valid timestamp")
}

#[derive(Debug, Clone, Default)]
struct UpgradeShopUpstream;

impl Upstream for UpgradeShopUpstream {
    async fn send(&self, request: UpstreamRequest) -> Result<UpstreamResponse, UpstreamError> {
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(
            axum::http::header::CONTENT_TYPE,
            axum::http::HeaderValue::from_static("application/json"),
        );
        let body = if request.path_and_query.starts_with("/customers/1") {
            json!({"id": 1, "name": "Ada Lovelace", "tier": "vip"})
        } else {
            json!([])
        };
        Ok(UpstreamResponse {
            status: axum::http::StatusCode::OK,
            headers,
            body: serde_json::to_vec(&body).unwrap(),
        })
    }
}

#[derive(Debug, Default)]
struct UpgradeBranchFactory;

impl BranchFactory for UpgradeBranchFactory {
    fn build_branch<'a>(
        &'a self,
        server_file: &'a ServerFile,
        _branch_index: usize,
        branch: &'a BranchSpec,
        _uid: BranchUid,
    ) -> BoxFuture<'a, Result<BranchRuntime, BranchBuildError>> {
        Box::pin(async move {
            match branch.protocol {
                BranchProtocol::Openapi => {
                    let doc_bytes = server_file.openapi_document(&branch.id).ok_or_else(|| {
                        BranchBuildError::ConnectorInit {
                            branch_id: branch.id.clone(),
                            reason: "missing OpenAPI document".to_owned(),
                        }
                    })?;
                    let spec = branch.openapi.as_ref().unwrap();
                    let cfg = OpenApiConfig {
                        connector_id: branch.id.to_string(),
                        base_url: spec.base_url.clone(),
                        allow_insecure_upstream: spec.allow_insecure_upstream,
                        auth: AuthConfig::None,
                        ..OpenApiConfig::default()
                    };
                    let creds = StaticCredentialSource::new();
                    let connector = OpenApiConnector::from_document_bytes(
                        doc_bytes,
                        cfg,
                        &creds,
                        UpgradeShopUpstream,
                    )
                    .map_err(|err| BranchBuildError::ConnectorInit {
                        branch_id: branch.id.clone(),
                        reason: err.to_string(),
                    })?;
                    wrap_connector(&branch.id, connector).await
                }
                BranchProtocol::Mcp => {
                    let fake = FakeMcpServer::new(ServerEra::Modern);
                    let mut tool = FakeTool::new(
                        "customer_notes",
                        json!({
                            "type": "object",
                            "required": ["customer_id"],
                            "additionalProperties": false,
                            "properties": { "customer_id": { "type": "integer" } }
                        }),
                    );
                    tool.description = "Fetch customer notes".to_owned();
                    fake.set_tools(vec![tool]);
                    fake.set_call_handler(|_name, args| {
                        FakeCall::Reply(json!({
                            "content": [{"type": "text", "text": "ok"}],
                            "structuredContent": args,
                            "isError": false
                        }))
                    });
                    let transport = Arc::new(HttpTransport::new(fake.http_client(), 1 << 20));
                    let connector = McpConnector::new(
                        transport,
                        McpConfig {
                            connector_id: branch.id.to_string(),
                            ..McpConfig::default()
                        },
                    )
                    .map_err(|err| BranchBuildError::ConnectorInit {
                        branch_id: branch.id.clone(),
                        reason: err.to_string(),
                    })?;
                    let mut rt = wrap_connector(&branch.id, connector).await?;
                    rt.handle = Box::new((rt.handle, fake));
                    Ok(rt)
                }
            }
        })
    }
}

// spec: OPS-041
#[test]
fn ops_041_sqlite_format_version_migrations_and_future_version_guard() {
    let dir = TempDir::new().expect("temp dir");
    let db_path = dir.path().join("context.db");
    let backup_path = dir.path().join("context.bak.db");
    let future_path = dir.path().join("future.db");
    let t = tenant("acme-upgrade");
    let caller = Caller::new("upgrade-bot");

    // 1. Empty store opens at user_version = 0 and executes sequential 1 -> 2 -> 3 -> 4 migrations.
    let store =
        SqliteContextStore::open(SqliteConfig::new(&db_path), Arc::new(SystemClock)).expect("open");
    let n1 = store
        .put_node(
            &t,
            &caller,
            NewNode {
                id: Some(NodeId::new("svc:orders").expect("node id")),
                node_type: NodeType::new("Service").expect("node type"),
                label: "Orders Service".to_owned(),
                summary: Some("Handles customer invoices and billing".to_owned()),
                props: JsonMap::new(),
                scope: ScopeSpec::Global,
                provenance: Provenance::new("wp-m7-07"),
            },
        )
        .expect("put node 1");
    let n2 = store
        .put_node(
            &t,
            &caller,
            NewNode {
                id: Some(NodeId::new("svc:ledger").expect("node id")),
                node_type: NodeType::new("Service").expect("node type"),
                label: "Ledger Service".to_owned(),
                summary: Some("Immutable accounting ledger".to_owned()),
                props: JsonMap::new(),
                scope: ScopeSpec::Global,
                provenance: Provenance::new("wp-m7-07"),
            },
        )
        .expect("put node 2");
    let _e = store
        .put_edge(
            &t,
            &caller,
            NewEdge {
                id: Some(EdgeId::new("edge:orders->ledger").expect("edge id")),
                edge_type: EdgeType::new("CALLS").expect("edge type"),
                from: n1.clone(),
                to: n2.clone(),
                props: JsonMap::new(),
                confidence: Some(1.0),
                resolution: EdgeResolution::Exact,
                provenance: Provenance::new("wp-m7-07"),
            },
        )
        .expect("put edge");

    // 2. Online backup (`backup_to`) exports a self-contained SQLite file at FORMAT_VERSION.
    store.backup_to(&backup_path).expect("online backup");
    drop(store);

    // 3. Simulate an N+1 binary's SQLite database by bumping header `user_version` (bytes 60..64)
    //    to `FORMAT_VERSION + 1`; opening or restoring MUST fail closed with `FormatTooNew`.
    let mut future_bytes = fs::read(&backup_path).expect("read backup");
    assert!(future_bytes.len() >= 100, "valid SQLite header");
    future_bytes[60..64].copy_from_slice(&(FORMAT_VERSION + 1).to_be_bytes());
    fs::write(&future_path, &future_bytes).expect("write future db");

    let err = SqliteContextStore::open(SqliteConfig::new(&future_path), Arc::new(SystemClock))
        .expect_err("N+1 database must fail closed");
    assert_eq!(
        err,
        StoreError::FormatTooNew {
            found: FORMAT_VERSION + 1,
            supported: FORMAT_VERSION,
        }
    );
}

// spec: OPS-041
#[tokio::test]
async fn ops_041_snapshot_format_and_server_config_backward_compatibility() {
    let sf = ServerFile::load(repo_root().join("examples/walking-skeleton/gap.toml"))
        .expect("load reference gap.toml");
    let assembled = assemble(&sf, &UpgradeBranchFactory)
        .await
        .expect("assemble walking-skeleton");
    assert_eq!(assembled.snapshot_handle.generation(), 1);

    let initial = Snapshot::empty(1);
    let handle = SnapshotHandle::new(initial.clone());

    let valid_next = initial.next_generation(2, ts(1_700_000_100_000), "upgrade-test", None);
    assert_eq!(handle.publish(valid_next), Ok(2));

    let mut unsupported_format =
        handle
            .pin()
            .next_generation(3, ts(1_700_000_200_000), "upgrade-test", None);
    unsupported_format.format_version = SNAPSHOT_FORMAT_VERSION + 1;
    assert_eq!(
        handle.publish(unsupported_format),
        Err(PublishError::UnsupportedFormat {
            got: SNAPSHOT_FORMAT_VERSION + 1,
            supported: SNAPSHOT_FORMAT_VERSION,
        })
    );

    let skipped_gen =
        handle
            .pin()
            .next_generation(99, ts(1_700_000_300_000), "upgrade-test", None);
    assert_eq!(
        handle.publish(skipped_gen),
        Err(PublishError::NonMonotonic {
            expected: 3,
            got: 99,
        })
    );
    assert_eq!(handle.generation(), 2);
}

// spec: OPS-043, RUN-091
#[test]
fn ops_043_backup_and_restore_drill_verifies_context_audit_idempotency_and_tasks() {
    let live_dir = TempDir::new().expect("live dir");
    let backup_dir = TempDir::new().expect("backup dir");
    let restore_dir = TempDir::new().expect("restore dir");

    let tenant_scope = tenant("tenant-dr");
    let caller = Caller::new("dr-operator");
    let t0 = ts(1_700_000_000_000);
    let task_id = TaskId::new_v7();
    let req_hash = [0x42_u8; 32];

    let live_ctx = live_dir.path().join("context.db");
    let live_audit = live_dir.path().join("audit.db");
    let live_tasks = live_dir.path().join("tasks.db");
    let live_idem = live_dir.path().join("idempotency.db");

    // 1. Populate all four durable stores during live traffic.
    let ctx_store = SqliteContextStore::open(SqliteConfig::new(&live_ctx), Arc::new(SystemClock))
        .expect("open context store");
    let node_id = ctx_store
        .put_node(
            &tenant_scope,
            &caller,
            NewNode {
                id: Some(NodeId::new("doc:runbook").expect("node id")),
                node_type: NodeType::new("Document").expect("node type"),
                label: "Disaster Recovery Runbook".to_owned(),
                summary: Some("Verified backup and restore procedure".to_owned()),
                props: JsonMap::new(),
                scope: ScopeSpec::Global,
                provenance: Provenance::new("dr-test"),
            },
        )
        .expect("put node");

    let audit_log = SqliteAuditLog::open(SqliteAuditConfig::file(&live_audit)).expect("open audit");
    let audit_entry = AuditEntry {
        ts: t0,
        actor_chain: vec!["dr-operator".to_owned()],
        action: "mutate.backupDrill".to_owned(),
        resource: "doc:runbook".to_owned(),
        decision: "ALLOW".to_owned(),
        details_json: r#"{"phase":"pre-backup"}"#.to_owned(),
    };
    let rec_tenant = audit_log
        .append(&tenant_scope, &audit_entry)
        .expect("append tenant audit");
    let rec_sys = audit_log
        .append_chain(&AuditChain::System, &audit_entry)
        .expect("append system audit");
    audit_log.flush().expect("flush audit");

    let task_store = SqliteTaskStore::open(&live_tasks).expect("open tasks");
    task_store
        .put_task(
            &tenant_scope,
            &TaskRecord {
                task_id,
                status: TaskStatus::Submitted,
                payload_json: r#"{"op":"syncInvoices"}"#.to_owned(),
                lease_owner: None,
                lease_expires_at: None,
                updated_at: t0,
            },
            &TaskEventRecord {
                task_id,
                seq: 1,
                status: TaskStatus::Submitted,
                payload_json: "{}".to_owned(),
                emitted_at: t0,
            },
        )
        .expect("put task");
    task_store
        .transition_task(
            &tenant_scope,
            &TaskTransitionRequest {
                task_id,
                to_status: TaskStatus::Working,
                payload_json: None,
                lease_owner: Some(Some("worker-pre-crash".to_owned())),
                lease_expires_at: Some(Some(ts(1_700_000_010_000))),
                event_payload_json: r#"{"worker":"worker-pre-crash"}"#.to_owned(),
                now: ts(1_700_000_001_000),
            },
        )
        .expect("transition to Working");

    let idem_exp = Timestamp::from_unix_millis(SystemClock.now_timestamp().unix_millis() + 3_600_000)
        .expect("future idem expiry");
    let idem_store = SqliteIdempotencyStore::open(&live_idem).expect("open idempotency");
    assert_eq!(
        idem_store
            .check_or_reserve(&tenant_scope, "idem-key-1", req_hash, idem_exp)
            .expect("reserve key"),
        IdempotencyOutcome::Reserved
    );
    idem_store
        .complete(&tenant_scope, "idem-key-1", req_hash, Some("tx-99"))
        .expect("complete key");

    // 2. Capture backups (online SQLite backup API for context store + clean quiesced files for ledger stores).
    let bak_ctx = backup_dir.path().join("context.db");
    ctx_store.backup_to(&bak_ctx).expect("online backup_to");
    drop(ctx_store);
    drop(audit_log);
    drop(task_store);
    drop(idem_store);

    let bak_audit = backup_dir.path().join("audit.db");
    let bak_tasks = backup_dir.path().join("tasks.db");
    let bak_idem = backup_dir.path().join("idempotency.db");
    fs::copy(&live_audit, &bak_audit).expect("backup audit db");
    fs::copy(&live_tasks, &bak_tasks).expect("backup tasks db");
    fs::copy(&live_idem, &bak_idem).expect("backup idem db");

    // 3. Corrupt backup copy test: truncated/non-SQLite backup must fail closed with StoreError::Corrupt.
    let corrupt_bak = backup_dir.path().join("corrupt.db");
    fs::write(&corrupt_bak, vec![0xDE_u8; 128]).expect("write corrupt backup");
    let corrupt_target = restore_dir.path().join("corrupt-target.db");
    assert_eq!(
        SqliteContextStore::restore_from(&corrupt_bak, &corrupt_target),
        Err(StoreError::Corrupt)
    );
    assert!(
        !corrupt_target.exists(),
        "failed restore must leave target path absent"
    );

    // 4. Restore verified backups to a brand-new host directory (`restore_dir`).
    let rest_ctx = restore_dir.path().join("context.db");
    let rest_audit = restore_dir.path().join("audit.db");
    let rest_tasks = restore_dir.path().join("tasks.db");
    let rest_idem = restore_dir.path().join("idempotency.db");

    SqliteContextStore::restore_from(&bak_ctx, &rest_ctx).expect("restore_from context");
    // Overwriting an existing DB must fail closed with StoreError::Conflict.
    assert_eq!(
        SqliteContextStore::restore_from(&bak_ctx, &rest_ctx),
        Err(StoreError::Conflict)
    );
    fs::copy(&bak_audit, &rest_audit).expect("restore audit");
    fs::copy(&bak_tasks, &rest_tasks).expect("restore tasks");
    fs::copy(&bak_idem, &rest_idem).expect("restore idem");

    let restored_ctx =
        SqliteContextStore::open(SqliteConfig::new(&rest_ctx), Arc::new(SystemClock))
            .expect("open restored context");
    assert_eq!(
        restored_ctx.counts(&tenant_scope, &caller, None).unwrap(),
        GraphCounts { nodes: 1, edges: 0 }
    );
    assert!(
        restored_ctx
            .get_node(&tenant_scope, &caller, &node_id, None)
            .unwrap()
            .is_some()
    );
    assert_eq!(
        restored_ctx
            .search_nodes(&tenant_scope, &caller, "Disaster Recovery", 5, None)
            .unwrap()
            .len(),
        1
    );

    let restored_audit =
        SqliteAuditLog::open(SqliteAuditConfig::file(&rest_audit)).expect("open restored audit");
    assert_eq!(
        restored_audit.verify(&tenant_scope, None).unwrap(),
        AuditVerifyReport::Intact {
            head_seq: 1,
            head_hash: rec_tenant.hash,
        }
    );
    assert_eq!(
        restored_audit.verify_chain(&AuditChain::System, None).unwrap(),
        AuditVerifyReport::Intact {
            head_seq: 1,
            head_hash: rec_sys.hash,
        }
    );

    // RUN-091: Restored Working task with expired lease is recovered and reclaimed by restarted worker.
    let restored_tasks = SqliteTaskStore::open(&rest_tasks).expect("open restored tasks");
    let recoverable = restored_tasks
        .list_recoverable_tasks(&tenant_scope)
        .expect("list recoverable tasks");
    assert_eq!(recoverable.len(), 1);
    assert_eq!(recoverable[0].task_id, task_id);
    assert!(
        restored_tasks
            .claim_lease(
                &tenant_scope,
                task_id,
                "worker-restored",
                ts(1_700_000_020_000),
                ts(1_700_000_050_000),
            )
            .expect("claim expired lease after restore")
    );
    let (completed, _) = restored_tasks
        .transition_task(
            &tenant_scope,
            &TaskTransitionRequest {
                task_id,
                to_status: TaskStatus::Completed,
                payload_json: Some(r#"{"op":"syncInvoices","done":true}"#.to_owned()),
                lease_owner: None,
                lease_expires_at: None,
                event_payload_json: r#"{"restored":true}"#.to_owned(),
                now: ts(1_700_000_021_000),
            },
        )
        .expect("complete recovered task");
    assert_eq!(completed.status, TaskStatus::Completed);

    let restored_idem =
        SqliteIdempotencyStore::open(&rest_idem).expect("open restored idempotency");
    assert_eq!(
        restored_idem
            .check_or_reserve(&tenant_scope, "idem-key-1", req_hash, idem_exp)
            .expect("check restored idempotency"),
        IdempotencyOutcome::Replay {
            result_ref: Some("tx-99".to_owned()),
        }
    );
}
