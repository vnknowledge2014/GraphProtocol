//! Integration tests for `gap-ngc-code` (`ING-060`..`ING-064`).

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use gap_core::clock::{Clock, ManualClock};
use gap_core::error::NgcErrorCode;
use gap_core::ids::{TenantId, TenantScope};
use gap_ngc_code::{
    CodeGraphConnector, CodeIndexConfig, CodeIndexer, EDGE_PREDICATES, GRAMMAR_VERSIONS,
    GoldenGraphSnapshot, NODE_CATEGORIES, estimate_tokens_gql022,
};
use gap_ngc_sdk::connector::{CallScope, Connector};
use gap_ngc_sdk::error::{json_map_to_prost_struct, prost_struct_to_json_map};
use gap_proto::ngc::v1::{
    DescribeRequest, HealthRequest, QueryRequest, QueryResponse, ServingStatus, VariableSet,
    query_response,
};
use gap_store::{Caller, ContextStore, NodeId, SqliteConfig, SqliteContextStore};
use serde_json::{Map as JsonMap, Value as JsonValue, json};
use tempfile::TempDir;
use tokio::time::Instant;

fn fixture_repo_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/repos/polyglot_sample")
}

fn fixture_golden_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/repos/polyglot_sample.golden.json")
}

fn open_test_store(dir: &Path, clock: Arc<dyn Clock>) -> Arc<SqliteContextStore> {
    let db_path = dir.join("code_graph.sqlite");
    Arc::new(SqliteContextStore::open(SqliteConfig::new(db_path), clock).expect("open store"))
}

fn copy_dir_recursive(src: &Path, dst: &Path) {
    fs::create_dir_all(dst).expect("create dst dir");
    for entry in fs::read_dir(src).expect("read src dir") {
        let entry = entry.expect("dir entry");
        let path = entry.path();
        let target = dst.join(entry.file_name());
        if path.is_dir() {
            copy_dir_recursive(&path, &target);
        } else {
            fs::copy(&path, &target).expect("copy fixture file");
        }
    }
}

fn json_struct(val: JsonValue) -> prost_types::Struct {
    let map = val
        .as_object()
        .cloned()
        .expect("JSON value must be an object");
    json_map_to_prost_struct(&map)
}

async fn collect_query_rows(
    connector: &CodeGraphConnector<SqliteContextStore>,
    req: QueryRequest,
) -> Vec<(u32, Vec<JsonMap<String, JsonValue>>, Option<i32>)> {
    let scope = CallScope::for_test(Instant::now() + Duration::from_secs(10));
    let mut stream = connector.query(req, scope).await.expect("query succeeds");
    let mut chunks = Vec::new();
    while let Some(item) = stream.next().await {
        let resp: QueryResponse = item.expect("stream item ok");
        match resp.chunk {
            Some(query_response::Chunk::Rows(rc)) => {
                let err_code = rc.error.map(|e| e.code);
                let rows = rc
                    .rows
                    .iter()
                    .map(prost_struct_to_json_map)
                    .collect::<Vec<_>>();
                chunks.push((rc.foreach_index, rows, err_code));
            }
            Some(query_response::Chunk::Complete(_)) => {}
            None => {}
        }
    }
    chunks
}

// spec: ING-060
#[test]
fn ing_060_polyglot_graph_emits_all_categories_predicates_and_scip_resolution() {
    let tmp = TempDir::new().expect("temp dir");
    let clock = Arc::new(ManualClock::new(1_700_000_000_000));
    let store = open_test_store(tmp.path(), clock);
    let tenant = TenantScope::new(TenantId::new("acme").expect("tenant"));
    let writer = Caller::new("indexer-svc");

    let repo_root = fixture_repo_dir();
    let cfg = CodeIndexConfig::new("polyglot_sample", &repo_root)
        .with_scip_paths(vec![repo_root.join("scip.json")]);
    let mut indexer = CodeIndexer::new(cfg, Arc::clone(&store), tenant.clone(), writer);
    let report = indexer.reindex_full().expect("index with SCIP");
    assert!(
        report.parse_failures.is_empty(),
        "unexpected failures: {:?}",
        report.parse_failures
    );

    let snap = indexer
        .export_golden_snapshot(None)
        .expect("export snapshot");

    let found_categories: BTreeSet<&str> =
        snap.nodes.iter().map(|n| n.node_type.as_str()).collect();
    for required in NODE_CATEGORIES {
        assert!(
            found_categories.contains(required),
            "missing node category `{required}` in {found_categories:?}"
        );
    }

    let found_predicates: BTreeSet<&str> =
        snap.edges.iter().map(|e| e.edge_type.as_str()).collect();
    for required in EDGE_PREDICATES {
        assert!(
            found_predicates.contains(required),
            "missing edge predicate `{required}` in {found_predicates:?}"
        );
    }

    // Check SCIP-confirmed EXACT call edge vs heuristic AST call edge.
    let node_labels: BTreeMap<&str, &str> = snap
        .nodes
        .iter()
        .map(|n| (n.id.as_str(), n.label.as_str()))
        .collect();

    let scip_edge = snap
        .edges
        .iter()
        .find(|e| {
            e.edge_type == "CALLS"
                && node_labels.get(e.from.as_str()) == Some(&"Dispatch")
                && node_labels.get(e.to.as_str()) == Some(&"validate_token")
        })
        .expect("Dispatch -> validate_token edge");
    assert_eq!(scip_edge.resolution, "EXACT");
    assert!((scip_edge.confidence - 1.0).abs() < 1e-6);

    let heuristic_edge = snap
        .edges
        .iter()
        .find(|e| {
            e.edge_type == "CALLS"
                && node_labels.get(e.from.as_str()) == Some(&"run_job")
                && node_labels.get(e.to.as_str()) == Some(&"validate_token")
        })
        .expect("run_job -> validate_token edge");
    assert_eq!(heuristic_edge.resolution, "HEURISTIC");
    assert!((heuristic_edge.confidence - 0.80).abs() < 1e-6);

    // Without SCIP, Dispatch -> validate_token falls back to HEURISTIC.
    let tmp_no_scip = TempDir::new().expect("temp dir 2");
    let store2 = open_test_store(
        tmp_no_scip.path(),
        Arc::new(ManualClock::new(1_700_000_000_000)),
    );
    let mut indexer_no_scip = CodeIndexer::new(
        CodeIndexConfig::new("polyglot_sample", &repo_root),
        store2,
        tenant,
        Caller::new("indexer-svc"),
    );
    indexer_no_scip.reindex_full().expect("index without SCIP");
    let snap2 = indexer_no_scip
        .export_golden_snapshot(None)
        .expect("snapshot 2");
    let non_scip_edge = snap2
        .edges
        .iter()
        .find(|e| e.edge_type == "CALLS" && e.from == scip_edge.from && e.to == scip_edge.to)
        .expect("Dispatch -> validate_token without SCIP");
    assert_eq!(non_scip_edge.resolution, "HEURISTIC");
}

// spec: ING-061
#[test]
fn ing_061_all_1_0_languages_and_isolated_parse_failure() {
    let tmp_repo = TempDir::new().expect("repo copy dir");
    copy_dir_recursive(&fixture_repo_dir(), tmp_repo.path());

    // Add two broken files and one unsupported binary file.
    fs::write(
        tmp_repo.path().join("src/broken.rs"),
        b"pub fn incomplete_parse() {\n    let x = 1;\n",
    )
    .expect("write broken rs");
    fs::write(
        tmp_repo.path().join("src/nul.py"),
        b"def bad():\n\x00    return 1\n",
    )
    .expect("write nul py");
    fs::write(tmp_repo.path().join("logo.png"), [0x89, b'P', b'N', b'G'])
        .expect("write unsupported asset");

    let tmp_db = TempDir::new().expect("db dir");
    let store = open_test_store(tmp_db.path(), Arc::new(ManualClock::new(1_700_000_000_000)));
    let tenant = TenantScope::new(TenantId::new("acme").expect("tenant"));
    let mut indexer = CodeIndexer::new(
        CodeIndexConfig::new("isolated_test", tmp_repo.path()),
        store,
        tenant,
        Caller::new("indexer-svc"),
    );

    let report = indexer
        .index_incremental()
        .expect("run continues despite malformed files");
    assert_eq!(report.files_scanned, 9);
    assert_eq!(report.files_reindexed, 7);
    assert_eq!(report.parse_failures.len(), 2);
    let failed_paths: BTreeSet<&str> = report
        .parse_failures
        .iter()
        .map(|f| f.path.as_str())
        .collect();
    assert!(failed_paths.contains("src/broken.rs"));
    assert!(failed_paths.contains("src/nul.py"));

    let snap = indexer.export_golden_snapshot(None).expect("snapshot");
    let pinned_map: BTreeMap<&str, &str> = GRAMMAR_VERSIONS.iter().copied().collect();
    let mut observed_langs = BTreeSet::new();
    for node in snap.nodes.iter().filter(|n| n.node_type == "File") {
        let lang = node
            .props
            .get("language")
            .and_then(|v| v.as_str())
            .expect("language");
        let ver = node
            .props
            .get("grammar_version")
            .and_then(|v| v.as_str())
            .expect("grammar_version");
        assert_eq!(Some(&ver), pinned_map.get(lang));
        observed_langs.insert(lang.to_owned());
    }
    for expected_lang in ["rust", "typescript", "python", "go", "java", "markdown"] {
        assert!(
            observed_langs.contains(expected_lang),
            "missing healthy language `{expected_lang}` in {observed_langs:?}"
        );
    }
}

// spec: ING-062
#[test]
fn ing_062_incremental_hashing_tombstone_asof_and_golden_reproducibility() {
    let repo_root = fixture_repo_dir();
    let db_a = TempDir::new().expect("db a");
    let db_b = TempDir::new().expect("db b");
    let tenant = TenantScope::new(TenantId::new("acme").expect("tenant"));
    let writer = Caller::new("indexer-svc");

    let cfg_a = CodeIndexConfig::new("polyglot_sample", &repo_root)
        .with_scip_paths(vec![repo_root.join("scip.json")]);
    let cfg_b = cfg_a.clone();

    let mut idx_a = CodeIndexer::new(
        cfg_a,
        open_test_store(db_a.path(), Arc::new(ManualClock::new(1_700_000_000_000))),
        tenant.clone(),
        writer.clone(),
    );
    let mut idx_b = CodeIndexer::new(
        cfg_b,
        open_test_store(db_b.path(), Arc::new(ManualClock::new(1_700_000_500_000))),
        tenant.clone(),
        writer.clone(),
    );

    idx_a.reindex_full().expect("index a");
    idx_b.reindex_full().expect("index b");

    let snap_a = idx_a.export_golden_snapshot(None).expect("snap a");
    let snap_b = idx_b.export_golden_snapshot(None).expect("snap b");
    assert_eq!(
        snap_a, snap_b,
        "full re-index must be reproducible across fresh stores"
    );

    let golden_path = fixture_golden_path();
    if std::env::var_os("GAP_UPDATE_GOLDEN").is_some() {
        let rendered = serde_json::to_string_pretty(&snap_a).expect("serialize golden");
        fs::write(&golden_path, format!("{rendered}\n")).expect("write golden fixture");
    }
    let expected_raw = fs::read_to_string(&golden_path).expect("read committed golden fixture");
    let expected: GoldenGraphSnapshot =
        serde_json::from_str(&expected_raw).expect("parse committed golden fixture");
    assert_eq!(
        snap_a, expected,
        "snapshot drifted from committed polyglot_sample.golden.json"
    );

    // Verify incremental hashing + tombstones + temporal as_of.
    let tmp_repo = TempDir::new().expect("temp repo");
    copy_dir_recursive(&repo_root, tmp_repo.path());
    let db_inc = TempDir::new().expect("db inc");
    let clock = Arc::new(ManualClock::new(1_700_000_000_000));
    let store_inc = open_test_store(db_inc.path(), clock.clone());
    let mut inc = CodeIndexer::new(
        CodeIndexConfig::new("polyglot_inc", tmp_repo.path()),
        Arc::clone(&store_inc),
        tenant.clone(),
        writer.clone(),
    );

    let r1 = inc.index_incremental().expect("initial pass");
    assert_eq!(r1.files_reindexed, 7);
    assert_eq!(r1.files_unchanged, 0);

    // Unchanged pass skips all 7 files.
    let r2 = inc.index_incremental().expect("unchanged pass");
    assert_eq!(r2.files_reindexed, 0);
    assert_eq!(r2.files_unchanged, 7);
    assert_eq!(r2.nodes_written, 0);
    assert_eq!(r2.edges_written, 0);

    // Edit only src/worker.py after advancing clock.
    clock.advance(Duration::from_secs(10));
    let t_before_delete = clock.now_timestamp();
    let py_path = tmp_repo.path().join("src/worker.py");
    let mut py_src = fs::read_to_string(&py_path).expect("read worker.py");
    py_src.push_str("\ndef audit_worker() -> bool:\n    return validate_token(\"gap_audit\")\n");
    fs::write(&py_path, py_src).expect("update worker.py");

    let r3 = inc.index_incremental().expect("modified pass");
    assert_eq!(r3.files_reindexed, 1);
    assert_eq!(r3.files_unchanged, 6);
    assert_eq!(r3.files_tombstoned, 0);
    let snap_inc = inc
        .export_golden_snapshot(None)
        .expect("snapshot with new fn");
    assert!(
        snap_inc.nodes.iter().any(|n| n.label == "audit_worker"),
        "expected newly added audit_worker symbol"
    );

    // Delete src/Validator.java after advancing clock.
    clock.advance(Duration::from_secs(10));
    let t_exists = clock.now_timestamp();
    clock.advance(Duration::from_secs(10));
    fs::remove_file(tmp_repo.path().join("src/Validator.java")).expect("remove Validator.java");

    let r4 = inc.index_incremental().expect("delete pass");
    assert_eq!(r4.files_tombstoned, 1);
    assert_eq!(r4.files_reindexed, 0);
    assert_eq!(r4.files_unchanged, 6);

    let java_file_id = NodeId::new("file::polyglot_inc::src/Validator.java").expect("node id");
    assert!(
        store_inc
            .get_node(&tenant, &writer, &java_file_id, None)
            .expect("get current")
            .is_none(),
        "deleted file must be tombstoned at current time"
    );
    assert!(
        store_inc
            .get_node(&tenant, &writer, &java_file_id, Some(t_exists))
            .expect("get historical")
            .is_some(),
        "deleted file must still exist at historical as_of"
    );
    let _ = t_before_delete;
}

// spec: ING-063
#[tokio::test]
async fn ing_063_summaries_le_40_tokens_and_return_fields_slice_token_efficiency() {
    // Verify <= 40-token summaries across polyglot_sample.
    let db_poly = TempDir::new().expect("db poly");
    let store_poly = open_test_store(
        db_poly.path(),
        Arc::new(ManualClock::new(1_700_000_000_000)),
    );
    let tenant = TenantScope::new(TenantId::new("acme").expect("tenant"));
    let writer = Caller::new("indexer-svc");

    let repo_root = fixture_repo_dir();
    let mut poly_idx = CodeIndexer::new(
        CodeIndexConfig::new("polyglot_sample", &repo_root),
        Arc::clone(&store_poly),
        tenant.clone(),
        writer.clone(),
    );
    poly_idx.reindex_full().expect("index polyglot");
    let poly_snap = poly_idx.export_golden_snapshot(None).expect("snap");
    for node in &poly_snap.nodes {
        if node.node_type == "Symbol" || node.node_type == "Doc" {
            let sum = node
                .summary
                .as_deref()
                .expect("Symbol and Doc nodes must have summaries");
            assert!(
                estimate_tokens_gql022(sum) <= 40,
                "summary `{sum}` for `{}` exceeded 40 tokens",
                node.id
            );
        }
    }

    // Build a realistic 15-file repository in a temp dir so raw file scanning baseline > 4,000 tokens,
    // and benchmark answering "what calls verify_session / where is verify_session documented" with
    // <= 15% of file-scanning tokens using `returnFields`.
    let bench_repo = TempDir::new().expect("bench repo");
    let src_dir = bench_repo.path().join("src");
    let docs_dir = bench_repo.path().join("docs");
    fs::create_dir_all(&src_dir).expect("src");
    fs::create_dir_all(&docs_dir).expect("docs");

    fs::write(
        docs_dir.join("adr-002-sessions.md"),
        "# ADR-002: Session Token Verification\n\n\
         Every external adapter invokes `verify_session` prior to issuing database queries.\n\n\
         ## Rollout Notes\n\n\
         Legacy gateways are phased out across shards cleanly.\n",
    )
    .expect("write doc");

    let mut target_rs = String::from(
        "//! Core cryptographic session verification.\n\n\
         /// Verifies signed session credentials before tenant dispatch.\n\
         pub fn verify_session(token: &str) -> bool {\n    token.starts_with(\"gap_\")\n}\n\n",
    );
    for helper_i in 0..10 {
        target_rs.push_str(&format!(
            "/// Helper utility {helper_i} computing auxiliary audit checksums across buffered frames.\n\
             pub fn audit_checksum_{helper_i}(seed: u64) -> u64 {{\n    let a = seed.wrapping_mul(31);\n    let b = a ^ 0x9e37_79b9_7f4a_7c15;\n    b.rotate_left(7)\n}}\n\n"
        ));
    }
    fs::write(src_dir.join("auth.rs"), target_rs).expect("write auth.rs");

    for mod_i in 0..14 {
        let mut content = format!("//! Gateway worker shard {mod_i}.\n\n");
        if mod_i < 2 {
            content.push_str(&format!(
                "/// Ingress handler {mod_i} enforcing session verification before dispatch.\n\
                 pub fn handle_shard_{mod_i}(tok: &str) -> bool {{\n    verify_session(tok)\n}}\n\n"
            ));
        }
        for pad_i in 0..8 {
            content.push_str(&format!(
                "/// Internal background maintenance routine {pad_i} for shard {mod_i} telemetry normalization.\n\
                 pub fn shard_{mod_i}_maintenance_{pad_i}(input: &str) -> usize {{\n\
                     let trimmed = input.trim();\n\
                     trimmed.bytes().filter(|b| b.is_ascii_alphanumeric()).count() + {pad_i}\n\
                 }}\n\n"
            ));
        }
        fs::write(src_dir.join(format!("shard_{mod_i}.rs")), content).expect("write shard");
    }

    let db_bench = TempDir::new().expect("db bench");
    let store_bench = open_test_store(
        db_bench.path(),
        Arc::new(ManualClock::new(1_700_000_000_000)),
    );
    let mut bench_idx = CodeIndexer::new(
        CodeIndexConfig::new("bench_repo", bench_repo.path()),
        Arc::clone(&store_bench),
        tenant.clone(),
        writer.clone(),
    );
    bench_idx.reindex_full().expect("index bench repo");

    // Compute file-scanning baseline tokens over all files.
    let mut baseline_bytes = 0usize;
    for entry in fs::read_dir(&src_dir).expect("read src") {
        baseline_bytes += fs::metadata(entry.expect("e").path()).expect("m").len() as usize;
    }
    baseline_bytes += fs::metadata(docs_dir.join("adr-002-sessions.md"))
        .expect("m")
        .len() as usize;
    let baseline_tokens = ((baseline_bytes as f64) / 3.6).ceil() as u32;

    let connector = CodeGraphConnector::new("code-graph", Arc::clone(&store_bench), tenant, writer);
    let chunks = collect_query_rows(
        &connector,
        QueryRequest {
            ctx: None,
            operation: String::from("code_callers"),
            arguments: Some(json_struct(json!({
                "symbol": "verify_session",
                "edgeTypes": ["CALLS", "DOCUMENTS"],
                "maxDepth": 1,
                "returnFields": ["id", "type", "label", "summary"]
            }))),
            selection: None,
            predicate: None,
            order_by: Vec::new(),
            page: None,
            foreach: Vec::new(),
            as_of: String::new(),
        },
    )
    .await;

    let (_, rows, err) = &chunks[0];
    assert!(err.is_none());
    let labels: BTreeSet<&str> = rows
        .iter()
        .filter_map(|r| r.get("label").and_then(|v| v.as_str()))
        .collect();
    assert!(labels.contains("handle_shard_0"));
    assert!(labels.contains("handle_shard_1"));
    assert!(labels.contains("ADR-002: Session Token Verification"));

    // Confirm sliced response omitted `props` & `provenance`.
    for row in rows {
        assert!(!row.contains_key("props"));
        assert!(!row.contains_key("provenance"));
    }

    let sliced_json = serde_json::to_string(rows).expect("serialize sliced rows");
    let graph_tokens = estimate_tokens_gql022(&sliced_json);
    let ratio = f64::from(graph_tokens) / f64::from(baseline_tokens);
    assert!(
        ratio <= 0.15,
        "expected sliced code_callers ratio <= 0.15, got {ratio:.4} ({graph_tokens}/{baseline_tokens})"
    );
}

// spec: ING-064
#[tokio::test]
async fn ing_064_connector_describe_query_foreach_and_strict_bounds() {
    let tmp = TempDir::new().expect("temp dir");
    let store = open_test_store(tmp.path(), Arc::new(ManualClock::new(1_700_000_000_000)));
    let tenant = TenantScope::new(TenantId::new("acme").expect("tenant"));
    let writer = Caller::new("indexer-svc");

    let repo_root = fixture_repo_dir();
    let cfg = CodeIndexConfig::new("polyglot_sample", &repo_root)
        .with_scip_paths(vec![repo_root.join("scip.json")]);
    let mut indexer = CodeIndexer::new(cfg, Arc::clone(&store), tenant.clone(), writer.clone());
    indexer.reindex_full().expect("reindex");

    let connector = CodeGraphConnector::new("code-graph", Arc::clone(&store), tenant, writer);

    // Health check.
    let health = connector
        .health(
            HealthRequest::default(),
            CallScope::for_test(Instant::now() + Duration::from_secs(5)),
        )
        .await
        .expect("health ok");
    assert_eq!(health.status, ServingStatus::Serving as i32);

    // Describe & known_spec_hash caching.
    let desc = connector
        .describe(
            DescribeRequest::default(),
            CallScope::for_test(Instant::now() + Duration::from_secs(5)),
        )
        .await
        .expect("describe ok");
    assert!(!desc.not_modified);
    let d = desc.description.expect("description present");
    let caps = d.capabilities.expect("capabilities");
    assert!(caps.native_traverse);
    assert!(caps.as_of);
    assert!(caps.foreach_batch);
    let limits = d.limits.expect("limits");
    assert_eq!(limits.max_page_size, 1_000);
    let meta = d.meta.expect("meta");
    assert_eq!(meta.node_categories.len(), NODE_CATEGORIES.len());
    assert_eq!(meta.edge_predicates.len(), EDGE_PREDICATES.len());

    let cached_desc = connector
        .describe(
            DescribeRequest {
                known_spec_hash: d.spec_hash,
            },
            CallScope::for_test(Instant::now() + Duration::from_secs(5)),
        )
        .await
        .expect("cached describe");
    assert!(cached_desc.not_modified);
    assert!(cached_desc.description.is_none());

    // Bounded validation checks (depth > 16, first > 1000, visitedCap == 0).
    for (label, args) in [
        (
            "maxDepth=17",
            json!({ "from": "repo::polyglot_sample", "maxDepth": 17 }),
        ),
        (
            "maxDepth=0",
            json!({ "from": "repo::polyglot_sample", "maxDepth": 0 }),
        ),
        (
            "first=1001",
            json!({ "from": "repo::polyglot_sample", "first": 1001 }),
        ),
        (
            "visitedCap=0",
            json!({ "from": "repo::polyglot_sample", "visitedCap": 0 }),
        ),
        (
            "visitedCap=1000001",
            json!({ "from": "repo::polyglot_sample", "visitedCap": 1_000_001 }),
        ),
    ] {
        let err = connector
            .query(
                QueryRequest {
                    ctx: None,
                    operation: String::from("code_traverse"),
                    arguments: Some(json_struct(args)),
                    selection: None,
                    predicate: None,
                    order_by: Vec::new(),
                    page: None,
                    foreach: Vec::new(),
                    as_of: String::new(),
                },
                CallScope::for_test(Instant::now() + Duration::from_secs(5)),
            )
            .await
            .map(|_| ())
            .expect_err(&format!("expected InvalidArgument for {label}"));
        assert_eq!(err.code, NgcErrorCode::InvalidArgument);
    }

    // `code_traverse` with `WEIGHTED` + cursor pagination (`first: 3` -> `after: next_cursor`).
    let mut page1_stream = connector
        .query(
            QueryRequest {
                ctx: None,
                operation: String::from("code_traverse"),
                arguments: Some(json_struct(json!({
                    "from": "repo::polyglot_sample",
                    "strategy": "WEIGHTED",
                    "direction": "OUTBOUND",
                    "maxDepth": 2,
                    "first": 3,
                    "returnFields": ["id", "label"]
                }))),
                selection: None,
                predicate: None,
                order_by: Vec::new(),
                page: None,
                foreach: Vec::new(),
                as_of: String::new(),
            },
            CallScope::for_test(Instant::now() + Duration::from_secs(5)),
        )
        .await
        .expect("page1 stream");
    let mut next_cursor = String::new();
    let mut page1_ids = Vec::new();
    while let Some(item) = page1_stream.next().await {
        match item.expect("page1 ok").chunk {
            Some(query_response::Chunk::Rows(rc)) => {
                for r in &rc.rows {
                    let m = prost_struct_to_json_map(r);
                    page1_ids.push(m["id"].as_str().expect("id").to_owned());
                }
            }
            Some(query_response::Chunk::Complete(c)) => next_cursor = c.next_cursor,
            None => {}
        }
    }
    assert_eq!(page1_ids.len(), 3);
    assert!(!next_cursor.is_empty(), "expected non-empty resume cursor");

    let page2_chunks = collect_query_rows(
        &connector,
        QueryRequest {
            ctx: None,
            operation: String::from("code_traverse"),
            arguments: Some(json_struct(json!({
                "from": "repo::polyglot_sample",
                "strategy": "WEIGHTED",
                "direction": "OUTBOUND",
                "maxDepth": 2,
                "first": 3,
                "after": next_cursor,
                "returnFields": ["id", "label"]
            }))),
            selection: None,
            predicate: None,
            order_by: Vec::new(),
            page: None,
            foreach: Vec::new(),
            as_of: String::new(),
        },
    )
    .await;
    let page2_ids: BTreeSet<String> = page2_chunks[0]
        .1
        .iter()
        .map(|m| m["id"].as_str().expect("id").to_owned())
        .collect();
    assert!(!page2_ids.is_empty());
    for id in &page1_ids {
        assert!(
            !page2_ids.contains(id),
            "cursor must not duplicate page 1 node `{id}`"
        );
    }

    // `foreach` mixed batch isolates per-index error while preserving valid index + `upstream_calls == 1`.
    let scope = CallScope::for_test(Instant::now() + Duration::from_secs(10));
    let mut batch_stream = connector
        .query(
            QueryRequest {
                ctx: None,
                operation: String::from("code_traverse"),
                arguments: Some(json_struct(json!({
                    "returnFields": ["id", "label"]
                }))),
                selection: None,
                predicate: None,
                order_by: Vec::new(),
                page: None,
                foreach: vec![
                    VariableSet {
                        index: 0,
                        values: Some(json_struct(json!({
                            "from": "repo::polyglot_sample",
                            "maxDepth": 1,
                            "first": 10
                        }))),
                    },
                    VariableSet {
                        index: 1,
                        values: Some(json_struct(json!({
                            "from": "repo::polyglot_sample",
                            "maxDepth": 99
                        }))),
                    },
                ],
                as_of: String::new(),
            },
            scope,
        )
        .await
        .expect("foreach stream opens");

    let mut seen_idx0 = false;
    let mut seen_idx1_err = false;
    let mut upstream_calls = 0;
    while let Some(item) = batch_stream.next().await {
        match item.expect("chunk ok").chunk {
            Some(query_response::Chunk::Rows(rc)) if rc.foreach_index == 0 => {
                assert!(rc.error.is_none());
                assert!(!rc.rows.is_empty());
                seen_idx0 = true;
            }
            Some(query_response::Chunk::Rows(rc)) if rc.foreach_index == 1 => {
                assert!(rc.error.is_some());
                seen_idx1_err = true;
            }
            Some(query_response::Chunk::Complete(c)) => {
                upstream_calls = c.upstream_calls;
            }
            _ => {}
        }
    }
    assert!(seen_idx0);
    assert!(seen_idx1_err);
    assert_eq!(upstream_calls, 1);
}
