//! Conformance runner executing `conformance/mcp/[0-9]*.json` with `spec/02-gap-ql.md` §11 subset matching (`CMP-001`..`005`, `CMP-007`, `CMP-008`, `SEC-083`).

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use gap_core::traits::{GraphqlEngine, HttpExchangeRequest, HttpModule};
use gap_mcp::inbound::{McpFacade, StdioServer};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use super::support::{
    FakeEngine, FakeOperationCatalog, FakeTaskBackend, compose_n_branch_snapshot,
    compose_two_branch_snapshot, make_ctx, workspace_root,
};

#[derive(Debug, Deserialize)]
struct McpFixture {
    name: String,
    class: Vec<String>,
    spec: Vec<String>,
    setup: FixtureSetup,
    steps: Vec<FixtureStep>,
}

#[derive(Debug, Deserialize)]
struct FixtureSetup {
    snapshot: String,
    principal: PrincipalSetup,
}

#[derive(Debug, Deserialize)]
struct PrincipalSetup {
    subject: String,
    tenant: String,
    #[serde(default)]
    scopes: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct FixtureStep {
    request: StepRequest,
    expect: StepExpect,
}

#[derive(Debug, Deserialize)]
struct StepRequest {
    http: Option<HttpStepRequest>,
    body: Option<Value>,
    stdio: Option<Value>,
}

#[derive(Debug, Deserialize)]
struct HttpStepRequest {
    method: String,
    path: String,
    #[serde(default)]
    headers: BTreeMap<String, String>,
}

#[derive(Debug, Deserialize)]
struct StepExpect {
    status: Option<u16>,
    headers: Option<BTreeMap<String, String>>,
    body: Option<Value>,
}

fn is_valid_spec_id(id: &str) -> bool {
    let Some((prefix, digits)) = id.split_once('-') else {
        return false;
    };
    let mut chars = prefix.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    first.is_ascii_uppercase()
        && chars.all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
        && (2..=3).contains(&digits.len())
        && digits.chars().all(|c| c.is_ascii_digit())
}

fn numbers_equal(a: &serde_json::Number, b: &serde_json::Number) -> bool {
    if let (Some(ia), Some(ib)) = (a.as_i64(), b.as_i64()) {
        return ia == ib;
    }
    if let (Some(ua), Some(ub)) = (a.as_u64(), b.as_u64()) {
        return ua == ub;
    }
    if let (Some(fa), Some(fb)) = (a.as_f64(), b.as_f64()) {
        return fa == fb;
    }
    false
}

/// Subset-match semantics per `spec/02-gap-ql.md` §11.
fn subset_matches(expected: &Value, observed: &Value) -> bool {
    match (expected, observed) {
        (Value::Null, Value::Null) => true,
        (Value::Bool(a), Value::Bool(b)) => a == b,
        (Value::String(a), Value::String(b)) => a == b,
        (Value::Number(a), Value::Number(b)) => numbers_equal(a, b),
        (Value::Array(exp_items), Value::Array(obs_items)) => {
            exp_items.len() == obs_items.len()
                && exp_items
                    .iter()
                    .zip(obs_items.iter())
                    .all(|(e, o)| subset_matches(e, o))
        }
        (Value::Object(exp_map), Value::Array(obs_items))
            if exp_map.get("unordered") == Some(&Value::Bool(true)) =>
        {
            let Some(Value::Array(exp_items)) = exp_map.get("items") else {
                return false;
            };
            if exp_items.len() != obs_items.len() {
                return false;
            }
            fn match_bipartite(exp: &[Value], obs: &[Value], used: &mut [bool]) -> bool {
                let Some((first, rest)) = exp.split_first() else {
                    return true;
                };
                for (i, candidate) in obs.iter().enumerate() {
                    if !used[i] && subset_matches(first, candidate) {
                        used[i] = true;
                        if match_bipartite(rest, obs, used) {
                            return true;
                        }
                        used[i] = false;
                    }
                }
                false
            }
            let mut used = vec![false; obs_items.len()];
            match_bipartite(exp_items, obs_items, &mut used)
        }
        (Value::Object(exp_map), Value::Object(obs_map)) => exp_map
            .iter()
            .all(|(k, exp_val)| obs_map.get(k).is_some_and(|obs_val| subset_matches(exp_val, obs_val))),
        _ => false,
    }
}

// spec: CMP-001, CMP-002, CMP-003, CMP-004, CMP-005, CMP-007, CMP-008, SEC-083
#[tokio::test]
async fn conformance_facade_mcp_fixtures_pass_and_cover_spec_ids() {
    let dir = workspace_root().join("conformance/mcp");
    let mut entries: Vec<_> = std::fs::read_dir(&dir)
        .expect("read conformance/mcp")
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| {
            p.extension().is_some_and(|ext| ext == "json")
                && p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n != "tools.expected.json")
        })
        .collect();
    entries.sort();
    assert!(
        entries.len() >= 12,
        "expected >= 12 Facade-MCP fixtures, found {}",
        entries.len()
    );

    let mut covered_specs: BTreeSet<String> = BTreeSet::new();

    for path in &entries {
        let raw = std::fs::read_to_string(path).expect("read fixture");
        let fixture: McpFixture = serde_json::from_str(&raw)
            .unwrap_or_else(|e| panic!("invalid fixture {}: {e}", path.display()));
        assert!(
            fixture.class.iter().any(|c| c == "Facade-MCP"),
            "fixture {} missing class Facade-MCP",
            fixture.name
        );
        assert!(
            !fixture.spec.is_empty() && fixture.spec.iter().all(|s| is_valid_spec_id(s)),
            "fixture {} has invalid spec ids: {:?}",
            fixture.name,
            fixture.spec
        );
        assert!(
            !fixture.steps.is_empty(),
            "fixture {} has empty steps",
            fixture.name
        );

        let snap = if fixture.setup.snapshot == "two-branch" {
            compose_two_branch_snapshot(true, 1)
        } else if let Some(n_str) = fixture.setup.snapshot.strip_prefix("n-branches:") {
            let n: usize = n_str.parse().expect("valid branch count");
            compose_n_branch_snapshot(n)
        } else {
            panic!("unsupported snapshot preset `{}`", fixture.setup.snapshot);
        };

        let scope_refs: Vec<&str> = fixture.setup.principal.scopes.iter().map(String::as_str).collect();
        let ctx = make_ctx(
            snap,
            &fixture.setup.principal.subject,
            &fixture.setup.principal.tenant,
            &scope_refs,
        );

        let facade = McpFacade::builder(FakeEngine::two_branch() as Arc<dyn GraphqlEngine>)
            .operations(FakeOperationCatalog::default_catalog())
            .tasks(FakeTaskBackend::seeded())
            .build()
            .expect("build facade");

        // Persistent stdio stream across steps of the same fixture
        let (mut stdio_in, server_read) = tokio::io::duplex(64 * 1024);
        let (server_write, stdio_out) = tokio::io::duplex(64 * 1024);
        let mut stdio_lines = BufReader::new(stdio_out).lines();
        let ctx_for_stdio = ctx.clone();
        let facade_for_stdio = facade.clone();
        let stdio_task = tokio::spawn(async move {
            let srv = StdioServer::new(facade_for_stdio, Arc::new(move || Ok(ctx_for_stdio.clone())));
            srv.serve(BufReader::new(server_read), server_write).await
        });

        let mut last_session_id: Option<String> = None;

        for (step_idx, step) in fixture.steps.iter().enumerate() {
            if let Some(http_req) = &step.request.http {
                let mut headers = Vec::with_capacity(http_req.headers.len());
                for (k, v) in &http_req.headers {
                    let resolved = if v == "$session_id" {
                        last_session_id
                            .clone()
                            .expect("$session_id used before initialize")
                    } else {
                        v.clone()
                    };
                    headers.push((k.to_ascii_lowercase(), resolved));
                }
                let body_bytes = match &step.request.body {
                    Some(v) => serde_json::to_vec(v).expect("serialize step body"),
                    None => Vec::new(),
                };
                let resp = facade
                    .handle(
                        &ctx,
                        HttpExchangeRequest {
                            method: http_req.method.clone(),
                            path: http_req.path.clone(),
                            query: None,
                            headers,
                            body: body_bytes,
                        },
                    )
                    .await
                    .expect("HttpModule::handle");

                let mut obs_headers: BTreeMap<String, String> = BTreeMap::new();
                for (k, v) in &resp.headers {
                    let lower = k.to_ascii_lowercase();
                    obs_headers
                        .entry(lower)
                        .and_modify(|existing| {
                            existing.push_str(", ");
                            existing.push_str(v);
                        })
                        .or_insert_with(|| v.clone());
                }
                if let Some(sid) = obs_headers.get("mcp-session-id") {
                    last_session_id = Some(sid.clone());
                }

                if let Some(exp_status) = step.expect.status {
                    assert_eq!(
                        resp.status,
                        exp_status,
                        "fixture `{}` ({}) step {step_idx} status mismatch; body={}",
                        fixture.name,
                        path.display(),
                        String::from_utf8_lossy(&resp.body)
                    );
                }
                if let Some(exp_headers) = &step.expect.headers {
                    for (ek, ev) in exp_headers {
                        let actual = obs_headers.get(&ek.to_ascii_lowercase());
                        assert_eq!(
                            actual,
                            Some(ev),
                            "fixture `{}` step {step_idx} header `{ek}` mismatch",
                            fixture.name
                        );
                    }
                }
                if let Some(exp_body) = &step.expect.body {
                    let obs_body: Value = serde_json::from_slice(&resp.body).unwrap_or_else(|e| {
                        panic!(
                            "fixture `{}` step {step_idx} expected JSON body, got `{}` ({e})",
                            fixture.name,
                            String::from_utf8_lossy(&resp.body)
                        )
                    });
                    assert!(
                        subset_matches(exp_body, &obs_body),
                        "fixture `{}` ({}) step {step_idx} body mismatch:\nexpected subset: {}\nobserved: {}",
                        fixture.name,
                        path.display(),
                        serde_json::to_string_pretty(exp_body).unwrap_or_default(),
                        serde_json::to_string_pretty(&obs_body).unwrap_or_default()
                    );
                }
            } else if let Some(stdio_msg) = &step.request.stdio {
                let encoded = format!("{}\n", serde_json::to_string(stdio_msg).expect("json"));
                stdio_in.write_all(encoded.as_bytes()).await.expect("stdio write");
                stdio_in.flush().await.expect("stdio flush");

                if let Some(exp_body) = &step.expect.body {
                    let line = stdio_lines
                        .next_line()
                        .await
                        .expect("stdio read")
                        .unwrap_or_else(|| {
                            panic!(
                                "fixture `{}` step {step_idx} unexpected EOF on stdio",
                                fixture.name
                            )
                        });
                    let obs_body: Value = serde_json::from_str(&line).expect("stdio JSON response");
                    assert!(
                        subset_matches(exp_body, &obs_body),
                        "fixture `{}` ({}) step {step_idx} stdio body mismatch:\nexpected subset: {}\nobserved: {}",
                        fixture.name,
                        path.display(),
                        serde_json::to_string_pretty(exp_body).unwrap_or_default(),
                        serde_json::to_string_pretty(&obs_body).unwrap_or_default()
                    );
                }
            } else {
                panic!("fixture `{}` step {step_idx} has neither http nor stdio", fixture.name);
            }
        }

        drop(stdio_in);
        stdio_task.await.expect("stdio join").expect("stdio serve");

        for sid in fixture.spec {
            covered_specs.insert(sid);
        }
    }

    for required in [
        "CMP-001", "CMP-002", "CMP-003", "CMP-004", "CMP-005", "CMP-007", "CMP-008", "SEC-083",
    ] {
        assert!(
            covered_specs.contains(required),
            "conformance fixtures did not cover required spec id `{required}` (covered: {covered_specs:?})"
        );
    }

    // Sanity check unordered subset matcher too
    assert!(subset_matches(
        &json!({ "unordered": true, "items": [{ "a": 1 }, { "a": 2.0 }] }),
        &json!([{ "a": 2, "extra": true }, { "a": 1 }])
    ));
}
