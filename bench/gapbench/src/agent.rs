//! Benchmark agent drivers for Direct-MCP vs GAP-Facade execution (`ADR-021`, `CMP-008`).

use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use gap_core::context::RequestContext;
use gap_core::traits::{HttpExchangeRequest, HttpModule as _};
use gap_mcp::inbound::McpFacade;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpStream;

use crate::synth::{
    ApiLookupTask, BenchError, CodeGraphFixture, CodeGraphTask, CrossBranchJoinTask,
    SyntheticSupergraph,
};

/// Strategy under test (`DirectMcp` vs `GapFacade`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ModeKind {
    /// Conventional MCP client receiving all `N` tool definitions (or FS tools) directly.
    DirectMcp,
    /// GAP stateless MCP `2026-07-28` 5-tool progressive-disclosure facade.
    GapFacade,
}

impl ModeKind {
    /// Stable lowercase identifier serialized into reports.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::DirectMcp => "direct_mcp",
            Self::GapFacade => "gap_facade",
        }
    }
}

/// Single prompt+completion round-trip exchange recorded for token accounting.
#[derive(Debug, Clone)]
pub struct TurnExchange {
    /// Dynamic prompt / tool-call arguments emitted by the agent (UTF-8 bytes).
    pub prompt_bytes: usize,
    /// Response / tool-output payload returned by the server (UTF-8 bytes).
    pub response_bytes: usize,
    /// `true` once this call returns substantive domain data (rather than pure discovery metadata).
    pub is_useful_call: bool,
}

/// Complete execution trace of one benchmark task.
#[derive(Debug, Clone)]
pub struct TaskTrace {
    /// Stable task identifier.
    pub task_id: String,
    /// Execution mode (`direct_mcp` vs `gap_facade`).
    pub mode: ModeKind,
    /// Static `tools/list` prefix byte length presented at session start.
    pub tools_list_bytes: usize,
    /// Static system + initial user task prompt byte length.
    pub initial_prompt_bytes: usize,
    /// Ordered tool round-trips executed for this task.
    pub turns: Vec<TurnExchange>,
    /// Whether the task met all ground-truth assertions.
    pub succeeded: bool,
    /// Task accuracy in `[0.0, 1.0]` (`1.0` exact match or set F1 for `code_graph`).
    pub accuracy: f64,
    /// End-to-end wall-clock execution duration in milliseconds.
    pub latency_ms: f64,
}

/// Configuration for the optional provider-agnostic OpenAI-compatible LLM client.
#[derive(Debug, Clone)]
pub struct LlmConfig {
    /// Base HTTP URL (`http://host:port` or `http://host:port/v1/chat/completions`).
    pub endpoint: String,
    /// Model identifier forwarded in the JSON body.
    pub model: String,
    /// Optional bearer credential resolved from `GAPBENCH_LLM_API_KEY`.
    pub api_key: Option<String>,
}

/// Invokes GAP's in-process stateless MCP `2026-07-28` facade (`McpFacade::handle`).
pub async fn mcp_facade_call(
    facade: &McpFacade,
    ctx: &RequestContext,
    method: &str,
    tool_name: Option<&str>,
    mut params: Value,
) -> Result<(Vec<u8>, Value), BenchError> {
    if let Some(obj) = params.as_object_mut() {
        obj.insert(
            "_meta".to_owned(),
            json!({
                "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                "io.modelcontextprotocol/clientCapabilities": {}
            }),
        );
    }
    let body = serde_json::to_vec(&json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": method,
        "params": params
    }))?;

    let mut headers = vec![
        ("content-type".to_owned(), "application/json".to_owned()),
        (
            "accept".to_owned(),
            "application/json, text/event-stream".to_owned(),
        ),
        ("mcp-protocol-version".to_owned(), "2026-07-28".to_owned()),
        ("mcp-method".to_owned(), method.to_owned()),
    ];
    if let Some(name) = tool_name {
        headers.push(("mcp-name".to_owned(), name.to_owned()));
    }

    let req = HttpExchangeRequest {
        method: "POST".to_owned(),
        path: "/mcp".to_owned(),
        query: None,
        headers,
        body,
    };
    let resp = facade.handle(ctx, req).await?;
    if resp.status >= 400 {
        let preview = String::from_utf8_lossy(&resp.body);
        return Err(BenchError::Cli(format!(
            "MCP facade returned HTTP {}: {preview}",
            resp.status
        )));
    }
    let parsed: Value = serde_json::from_slice(&resp.body)?;
    Ok((resp.body, parsed))
}

/// Fetches the static 5-tool `tools/list` response bytes directly from `McpFacade`.
pub async fn fetch_gap_tools_list_bytes(
    facade: &McpFacade,
    ctx: &RequestContext,
) -> Result<Vec<u8>, BenchError> {
    let (bytes, parsed) = mcp_facade_call(facade, ctx, "tools/list", None, json!({})).await?;
    if parsed.get("error").is_some() {
        return Err(BenchError::Cli(format!(
            "tools/list JSON-RPC error: {parsed}"
        )));
    }
    Ok(bytes)
}

/// Abstract agent interface allowing deterministic scripted runs or provider-agnostic HTTP LLM runs.
#[derive(Debug, Clone)]
pub enum BenchAgent {
    /// Deterministic local scripted harness (default `--scripted`).
    Scripted,
    /// Optional provider-agnostic OpenAI-compatible HTTP endpoint driver.
    Llm(LlmConfig),
}

impl BenchAgent {
    /// Executes one API-lookup task under either `DirectMcp` or `GapFacade`.
    pub async fn run_api_lookup(
        &self,
        sg: &SyntheticSupergraph,
        task: &ApiLookupTask,
        mode: ModeKind,
        gap_tools_list_bytes: usize,
    ) -> Result<TaskTrace, BenchError> {
        let started = Instant::now();
        let llm_note = match self {
            Self::Scripted => 0usize,
            Self::Llm(cfg) => {
                let hint = query_openai_compat(
                    cfg,
                    &task.prompt,
                    &["gap_search", "gap_query", &task.expected_op_name],
                )
                .await?;
                hint.len()
            }
        };

        let mut turns = Vec::new();
        let (tools_list_bytes, succeeded) = match mode {
            ModeKind::DirectMcp => {
                let args = json!({ "id": task.arg_id });
                let call_req = json!({
                    "name": task.expected_op_name,
                    "arguments": args
                });
                let call_req_bytes = serde_json::to_vec(&call_req)?.len() + llm_note;
                let resp_bytes = sg.call_direct_tool(&task.expected_op_name, &args)?;
                let parsed: Value = serde_json::from_slice(&resp_bytes)?;
                let got_code = parsed["result"]["structuredContent"]["code"]
                    .as_str()
                    .unwrap_or_default();
                let ok = got_code == task.expected_code;
                turns.push(TurnExchange {
                    prompt_bytes: call_req_bytes,
                    response_bytes: resp_bytes.len(),
                    is_useful_call: true,
                });
                (sg.direct_tools_list_bytes.len(), ok)
            }
            ModeKind::GapFacade => {
                let ctx = sg.make_request_context()?;
                let search_params = json!({
                    "name": "gap_search",
                    "arguments": {
                        "query": task.search_query,
                        "limit": 5
                    }
                });
                let search_req_len = serde_json::to_vec(&search_params)?.len() + llm_note;
                let (search_resp_bytes, search_val) = mcp_facade_call(
                    &sg.facade,
                    &ctx,
                    "tools/call",
                    Some("gap_search"),
                    search_params,
                )
                .await?;
                turns.push(TurnExchange {
                    prompt_bytes: search_req_len,
                    response_bytes: search_resp_bytes.len(),
                    is_useful_call: false,
                });

                let hits = search_val["result"]["structuredContent"]["data"]["__search"]["hits"]
                    .as_array()
                    .ok_or_else(|| {
                        BenchError::Cli(format!(
                            "missing gap_search hits for `{}`: {search_val}",
                            task.task_id
                        ))
                    })?;
                let matched_field = hits
                    .iter()
                    .filter_map(|h| h["name"].as_str())
                    .find(|n| *n == task.expected_op_name)
                    .or_else(|| hits.first().and_then(|h| h["name"].as_str()))
                    .ok_or_else(|| {
                        BenchError::Cli(format!("empty gap_search hits for `{}`", task.task_id))
                    })?;

                let gql = format!(
                    "{{ {matched_field}(id: {}) {{ id code summary metric }} }}",
                    task.arg_id
                );
                let query_params = json!({
                    "name": "gap_query",
                    "arguments": { "query": gql }
                });
                let query_req_len = serde_json::to_vec(&query_params)?.len();
                let (query_resp_bytes, query_val) = mcp_facade_call(
                    &sg.facade,
                    &ctx,
                    "tools/call",
                    Some("gap_query"),
                    query_params,
                )
                .await?;
                let got_code =
                    query_val["result"]["structuredContent"]["data"][matched_field]["code"]
                        .as_str()
                        .unwrap_or_default();
                let ok = query_val["result"]["isError"] == json!(false)
                    && matched_field == task.expected_op_name
                    && got_code == task.expected_code;
                turns.push(TurnExchange {
                    prompt_bytes: query_req_len,
                    response_bytes: query_resp_bytes.len(),
                    is_useful_call: true,
                });
                (gap_tools_list_bytes, ok)
            }
        };

        Ok(TaskTrace {
            task_id: task.task_id.clone(),
            mode,
            tools_list_bytes,
            initial_prompt_bytes: task.prompt.len(),
            turns,
            succeeded,
            accuracy: if succeeded { 1.0 } else { 0.0 },
            latency_ms: started.elapsed().as_secs_f64() * 1_000.0,
        })
    }

    /// Executes one cross-branch join task under either `DirectMcp` or `GapFacade`.
    pub async fn run_cross_branch_join(
        &self,
        sg: &SyntheticSupergraph,
        task: &CrossBranchJoinTask,
        mode: ModeKind,
        gap_tools_list_bytes: usize,
    ) -> Result<TaskTrace, BenchError> {
        let started = Instant::now();
        let llm_note = match self {
            Self::Scripted => 0usize,
            Self::Llm(cfg) => {
                let hint = query_openai_compat(
                    cfg,
                    &task.prompt,
                    &[
                        "shop_customer",
                        "shop_orders",
                        "order_shipment",
                        "notes_for_customer",
                    ],
                )
                .await?;
                hint.len()
            }
        };

        let mut turns = Vec::new();
        let (tools_list_bytes, succeeded) = match mode {
            ModeKind::DirectMcp => {
                // Step 1: fetch customer
                let req1 = json!({"name": "shop_customer", "arguments": {"id": task.customer_id}});
                let resp1 =
                    sg.call_direct_tool("shop_customer", &json!({"id": task.customer_id}))?;
                let v1: Value = serde_json::from_slice(&resp1)?;
                let cname = v1["result"]["structuredContent"]["name"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned();
                turns.push(TurnExchange {
                    prompt_bytes: serde_json::to_vec(&req1)?.len() + llm_note,
                    response_bytes: resp1.len(),
                    is_useful_call: true,
                });

                // Step 2: fetch customer orders
                let req2 =
                    json!({"name": "shop_orders", "arguments": {"customer_id": task.customer_id}});
                let resp2 =
                    sg.call_direct_tool("shop_orders", &json!({"customer_id": task.customer_id}))?;
                let v2: Value = serde_json::from_slice(&resp2)?;
                let orders = v2["result"]["structuredContent"]["orders"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default();
                let first_order_id = orders
                    .first()
                    .and_then(|o| o["id"].as_i64())
                    .unwrap_or(task.primary_order_id);
                turns.push(TurnExchange {
                    prompt_bytes: serde_json::to_vec(&req2)?.len(),
                    response_bytes: resp2.len(),
                    is_useful_call: true,
                });

                // Step 3: fetch shipment for first order
                let req3 =
                    json!({"name": "order_shipment", "arguments": {"order_id": first_order_id}});
                let resp3 =
                    sg.call_direct_tool("order_shipment", &json!({"order_id": first_order_id}))?;
                let v3: Value = serde_json::from_slice(&resp3)?;
                let carrier = v3["result"]["structuredContent"]["carrier"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned();
                turns.push(TurnExchange {
                    prompt_bytes: serde_json::to_vec(&req3)?.len(),
                    response_bytes: resp3.len(),
                    is_useful_call: true,
                });

                // Step 4: fetch customer support notes
                let req4 = json!({"name": "notes_for_customer", "arguments": {"customer_id": task.customer_id}});
                let resp4 = sg.call_direct_tool(
                    "notes_for_customer",
                    &json!({"customer_id": task.customer_id}),
                )?;
                let v4: Value = serde_json::from_slice(&resp4)?;
                let note_summary = v4["result"]["structuredContent"]["notes"][0]["summary"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned();
                turns.push(TurnExchange {
                    prompt_bytes: serde_json::to_vec(&req4)?.len(),
                    response_bytes: resp4.len(),
                    is_useful_call: true,
                });

                let ok = cname == task.expected_customer_name
                    && orders.len() == task.expected_order_count
                    && carrier == task.expected_carrier
                    && note_summary == task.expected_note_summary;
                (sg.direct_tools_list_bytes.len(), ok)
            }
            ModeKind::GapFacade => {
                let ctx = sg.make_request_context()?;
                // Discovery pass confirming federated root fields across shop + notes
                let search_params = json!({
                    "name": "gap_search",
                    "arguments": {
                        "query": "customer orders notes shipment",
                        "limit": 8
                    }
                });
                let search_req_len = serde_json::to_vec(&search_params)?.len() + llm_note;
                let (search_resp_bytes, _search_val) = mcp_facade_call(
                    &sg.facade,
                    &ctx,
                    "tools/call",
                    Some("gap_search"),
                    search_params,
                )
                .await?;
                turns.push(TurnExchange {
                    prompt_bytes: search_req_len,
                    response_bytes: search_resp_bytes.len(),
                    is_useful_call: false,
                });

                // Single federated GAP-QL execution across shop + notes branches
                let gql = format!(
                    "{{ shop_customer(id: {cid}) {{ id name tier region }} shop_orders(customer_id: {cid}) {{ id customer_id total_cents status }} order_shipment(order_id: {oid}) {{ order_id carrier tracking_code status }} notes_for_customer(customer_id: {cid}) {{ note_id customer_id summary author }} }}",
                    cid = task.customer_id,
                    oid = task.primary_order_id,
                );
                let query_params = json!({
                    "name": "gap_query",
                    "arguments": { "query": gql }
                });
                let query_req_len = serde_json::to_vec(&query_params)?.len();
                let (query_resp_bytes, query_val) = mcp_facade_call(
                    &sg.facade,
                    &ctx,
                    "tools/call",
                    Some("gap_query"),
                    query_params,
                )
                .await?;
                let data = &query_val["result"]["structuredContent"]["data"];
                let cname = data["shop_customer"]["name"].as_str().unwrap_or_default();
                let order_count = data["shop_orders"].as_array().map_or(0, Vec::len);
                let carrier = data["order_shipment"]["carrier"]
                    .as_str()
                    .unwrap_or_default();
                let note_summary = data["notes_for_customer"][0]["summary"]
                    .as_str()
                    .unwrap_or_default();
                let ok = query_val["result"]["isError"] == json!(false)
                    && cname == task.expected_customer_name
                    && order_count == task.expected_order_count
                    && carrier == task.expected_carrier
                    && note_summary == task.expected_note_summary;
                turns.push(TurnExchange {
                    prompt_bytes: query_req_len,
                    response_bytes: query_resp_bytes.len(),
                    is_useful_call: true,
                });
                (gap_tools_list_bytes, ok)
            }
        };

        Ok(TaskTrace {
            task_id: task.task_id.clone(),
            mode,
            tools_list_bytes,
            initial_prompt_bytes: task.prompt.len(),
            turns,
            succeeded,
            accuracy: if succeeded { 1.0 } else { 0.0 },
            latency_ms: started.elapsed().as_secs_f64() * 1_000.0,
        })
    }

    /// Executes one code-graph caller-impact task under either `DirectMcp` (file-scan baseline)
    /// or `GapFacade` (`SqliteContextStore` FTS5 + `traverse`).
    pub async fn run_code_graph(
        &self,
        cg: &CodeGraphFixture,
        task: &CodeGraphTask,
        mode: ModeKind,
        gap_tools_list_bytes: usize,
    ) -> Result<TaskTrace, BenchError> {
        let started = Instant::now();
        let llm_note = match self {
            Self::Scripted => 0usize,
            Self::Llm(cfg) => {
                let hint = query_openai_compat(
                    cfg,
                    &task.prompt,
                    &["list_files", "grep_files", "read_file", "gap_query"],
                )
                .await?;
                hint.len()
            }
        };

        let mut turns = Vec::new();
        let (tools_list_bytes, predicted_callers) = match mode {
            ModeKind::DirectMcp => {
                let mut found_callers = BTreeSet::new();

                // 1. Enumerate repository files
                let req_ls = json!({"name": "list_files", "arguments": {}});
                let resp_ls = cg.list_files_json()?;
                turns.push(TurnExchange {
                    prompt_bytes: serde_json::to_vec(&req_ls)?.len() + llm_note,
                    response_bytes: resp_ls.len(),
                    is_useful_call: false,
                });

                // 2. Grep for direct references to target_symbol
                let req_grep1 =
                    json!({"name": "grep_files", "arguments": {"pattern": task.target_symbol}});
                let (matched1, resp_grep1) = cg.grep_files_json(&task.target_symbol)?;
                turns.push(TurnExchange {
                    prompt_bytes: serde_json::to_vec(&req_grep1)?.len(),
                    response_bytes: resp_grep1.len(),
                    is_useful_call: true,
                });

                // 3. Read each matched file to identify which `pub fn` encloses the call site
                let mut hop1_callers = BTreeSet::new();
                for path in matched1 {
                    let req_read = json!({"name": "read_file", "arguments": {"path": path}});
                    let resp_read = cg.read_file_json(&path)?;
                    let v: Value = serde_json::from_slice(&resp_read)?;
                    let content = v["result"]["structuredContent"]["content"]
                        .as_str()
                        .unwrap_or_default();
                    for caller_fn in find_enclosing_callers(content, &task.target_symbol) {
                        hop1_callers.insert(caller_fn.clone());
                        found_callers.insert(caller_fn);
                    }
                    turns.push(TurnExchange {
                        prompt_bytes: serde_json::to_vec(&req_read)?.len(),
                        response_bytes: resp_read.len(),
                        is_useful_call: true,
                    });
                }

                // 4. For each depth-1 caller, grep + read matching files to obtain depth-2 callers
                for d1 in hop1_callers {
                    let req_grep2 = json!({"name": "grep_files", "arguments": {"pattern": d1}});
                    let (matched2, resp_grep2) = cg.grep_files_json(&d1)?;
                    turns.push(TurnExchange {
                        prompt_bytes: serde_json::to_vec(&req_grep2)?.len(),
                        response_bytes: resp_grep2.len(),
                        is_useful_call: true,
                    });
                    for path in matched2 {
                        let req_read2 = json!({"name": "read_file", "arguments": {"path": path}});
                        let resp_read2 = cg.read_file_json(&path)?;
                        let v: Value = serde_json::from_slice(&resp_read2)?;
                        let content = v["result"]["structuredContent"]["content"]
                            .as_str()
                            .unwrap_or_default();
                        for caller_fn in find_enclosing_callers(content, &d1) {
                            found_callers.insert(caller_fn);
                        }
                        turns.push(TurnExchange {
                            prompt_bytes: serde_json::to_vec(&req_read2)?.len(),
                            response_bytes: resp_read2.len(),
                            is_useful_call: true,
                        });
                    }
                }

                (cg.direct_fs_tools_list_bytes.len(), found_callers)
            }
            ModeKind::GapFacade => {
                // Single ContextStore recursive `traverse(Inbound, maxDepth: 2)` via `gap_query`
                let gql = format!(
                    "{{ gap {{ traverse(from: \"fn:{sym}\", edgeTypes: [\"CALLS\"], direction: INBOUND, maxDepth: 2, strategy: BFS, first: 50) {{ nodes {{ id label summary }} edges {{ type from to }} stats {{ visited returned depthReached }} }} }} }}",
                    sym = task.target_symbol
                );
                let query_params = json!({
                    "name": "gap_query",
                    "arguments": { "query": gql }
                });
                let prompt_bytes = serde_json::to_vec(&query_params)?.len() + llm_note;
                let (callers, resp_bytes) = cg.query_context_graph_impact(&task.target_symbol)?;
                turns.push(TurnExchange {
                    prompt_bytes,
                    response_bytes: resp_bytes.len(),
                    is_useful_call: true,
                });
                (gap_tools_list_bytes, callers)
            }
        };

        let accuracy = set_f1(&predicted_callers, &task.expected_callers);
        let succeeded = predicted_callers == task.expected_callers;

        Ok(TaskTrace {
            task_id: task.task_id.clone(),
            mode,
            tools_list_bytes,
            initial_prompt_bytes: task.prompt.len(),
            turns,
            succeeded,
            accuracy,
            latency_ms: started.elapsed().as_secs_f64() * 1_000.0,
        })
    }
}

fn find_enclosing_callers(source: &str, callee: &str) -> Vec<String> {
    let mut callers = Vec::new();
    let mut current_fn: Option<String> = None;
    let call_pat = format!("{callee}(");

    for line in source.lines() {
        let trimmed = line.trim();
        if let Some(after) = trimmed.strip_prefix("pub fn ")
            && let Some((name, _)) = after.split_once('(')
        {
            current_fn = Some(name.trim().to_owned());
        } else if line == "}" {
            current_fn = None;
        } else if trimmed.contains(&call_pat)
            && let Some(ref caller_name) = current_fn
            && caller_name != callee
            && !callers.contains(caller_name)
        {
            callers.push(caller_name.clone());
        }
    }
    callers
}

#[allow(clippy::cast_precision_loss)]
fn set_f1(predicted: &BTreeSet<String>, expected: &BTreeSet<String>) -> f64 {
    if predicted.is_empty() && expected.is_empty() {
        return 1.0;
    }
    if predicted.is_empty() || expected.is_empty() {
        return 0.0;
    }
    let tp = predicted.intersection(expected).count() as f64;
    let prec = tp / (predicted.len() as f64);
    let rec = tp / (expected.len() as f64);
    if prec + rec == 0.0 {
        0.0
    } else {
        2.0 * prec * rec / (prec + rec)
    }
}

/// Optional HTTP client for OpenAI-compatible `/v1/chat/completions` gateways (`http://` local servers).
/// Fails closed cleanly without external network access when offline or given unsupported schemes.
async fn query_openai_compat(
    cfg: &LlmConfig,
    task_prompt: &str,
    tool_names: &[&str],
) -> Result<String, BenchError> {
    let trimmed = cfg.endpoint.trim();
    let Some(rest) = trimmed.strip_prefix("http://") else {
        return Err(BenchError::Llm(format!(
            "unsupported endpoint scheme `{trimmed}` (gapbench offline build supports `http://` local gateways such as Ollama/LiteLLM)"
        )));
    };
    let (host_port, base_path) = match rest.split_once('/') {
        Some((hp, p)) => (hp, format!("/{p}")),
        None => (rest, String::new()),
    };
    let path = if base_path.ends_with("/chat/completions") {
        base_path
    } else if base_path.ends_with("/v1") {
        format!("{base_path}/chat/completions")
    } else if base_path.is_empty() || base_path == "/" {
        "/v1/chat/completions".to_owned()
    } else {
        format!("{}/v1/chat/completions", base_path.trim_end_matches('/'))
    };

    let body = serde_json::to_string(&json!({
        "model": cfg.model,
        "temperature": 0.0,
        "max_tokens": 64,
        "messages": [
            {
                "role": "system",
                "content": format!("Available tools sample: {}", tool_names.join(", "))
            },
            {
                "role": "user",
                "content": task_prompt
            }
        ]
    }))?;

    let mut stream = tokio::time::timeout(Duration::from_secs(5), TcpStream::connect(host_port))
        .await
        .map_err(|_| BenchError::Llm(format!("timeout connecting to `{host_port}`")))?
        .map_err(|e| BenchError::Llm(format!("cannot connect to `{host_port}`: {e}")))?;

    let auth_header = cfg
        .api_key
        .as_deref()
        .map(|k| format!("Authorization: Bearer {k}\r\n"))
        .unwrap_or_default();

    let http_req = format!(
        "POST {path} HTTP/1.1\r\nHost: {host_port}\r\nContent-Type: application/json\r\n{auth_header}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream
        .write_all(http_req.as_bytes())
        .await
        .map_err(|e| BenchError::Llm(format!("write failed: {e}")))?;

    let mut raw = Vec::new();
    tokio::time::timeout(Duration::from_secs(15), stream.read_to_end(&mut raw))
        .await
        .map_err(|_| BenchError::Llm("timeout reading LLM completion response".to_owned()))?
        .map_err(|e| BenchError::Llm(format!("read failed: {e}")))?;

    let text = String::from_utf8_lossy(&raw);
    let (_head, payload) = text
        .split_once("\r\n\r\n")
        .ok_or_else(|| BenchError::Llm("malformed HTTP response from LLM endpoint".to_owned()))?;
    let parsed: Value = serde_json::from_str(payload.trim())
        .map_err(|e| BenchError::Llm(format!("invalid LLM JSON body: {e}")))?;
    Ok(parsed["choices"][0]["message"]["content"]
        .as_str()
        .unwrap_or_default()
        .to_owned())
}
