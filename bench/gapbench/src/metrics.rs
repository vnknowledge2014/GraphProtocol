//! Token accounting (`GQL-022`), prompt-cache costing, discovery SLO measurements (`RTE-034`),
//! gate evaluation (`G-F1`, `G-C1`), and JSON/Markdown report generation (`ADR-021`, `CMP-008`).

use std::fmt::Write as _;
use std::fs;
use std::path::Path;
use std::process::Command;
use std::time::Instant;

use gap_route::{SearchRequest, estimate_tokens};
use serde::{Deserialize, Serialize};

use crate::agent::{BenchAgent, ModeKind, TaskTrace, fetch_gap_tools_list_bytes};
use crate::synth::{
    BenchError, SuiteFilter, SyntheticSupergraph, build_code_graph_fixture,
    build_synthetic_supergraph,
};

/// G-F1 maximum allowed `tools/list` tokens for the GAP MCP facade (`CMP-003`, `CMP-008`).
pub const GATE_GF1_MAX_TOKENS: u32 = 1_200;

/// G-C1 maximum allowed GAP/file-scan token ratio for repository impact analysis (`docs/gates/M2`).
pub const GATE_GC1_MAX_RATIO: f64 = 0.15;

/// Minimum required retrieval recall@10 across `N` (`docs/gates/M2`, `RTE-005`).
pub const GATE_SEARCH_MIN_RECALL_AT_10: f64 = 0.95;

/// Maximum allowed `__search` p95 latency in milliseconds (`docs/gates/M2`, `RTE-005`).
pub const GATE_SEARCH_MAX_P95_MS: f64 = 40.0;

/// Tokenizer-agnostic `ceil(bytes / 4)` approximation paired with `GQL-022` `ceil(bytes / 3.6)`.
#[must_use]
pub const fn estimate_tokens_ceil4(utf8_bytes: usize) -> u32 {
    if utf8_bytes == 0 {
        return 0;
    }
    let val = utf8_bytes.div_ceil(4);
    if val > u32::MAX as usize {
        u32::MAX
    } else {
        val as u32
    }
}

/// Environment & build provenance recorded inside `baseline.json` (`ADR-021`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnvironmentInfo {
    /// Git commit SHA (short) captured at run time.
    pub git_commit: String,
    /// Target operating system (`std::env::consts::OS`).
    pub os: String,
    /// Target CPU architecture (`std::env::consts::ARCH`).
    pub arch: String,
    /// Build optimization profile (`release` vs `debug`).
    pub build_profile: String,
    /// Agent driver identifier (`scripted` or `llm:<model>`).
    pub agent_mode: String,
    /// Exact CLI sweep parameters (`n` values).
    pub n_sweep: Vec<usize>,
    /// Selected suite filter.
    pub suite_filter: SuiteFilter,
}

/// Discovery and `tools/list` scaling row measured at tool count `N` (`CMP-008`, `RTE-034`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscoveryScaleRow {
    /// Total tool / operation count `N`.
    pub n_tools: usize,
    /// Number of composed branches `B`.
    pub branches: usize,
    /// Static 5-tool GAP MCP `tools/list` payload size (UTF-8 bytes).
    pub gap_tools_list_bytes: usize,
    /// Static 5-tool GAP MCP `tools/list` tokens (`GQL-022`: `ceil(bytes / 3.6)`).
    pub gap_tools_list_tokens: u32,
    /// Static 5-tool GAP MCP `tools/list` tokens (`ceil(bytes / 4)`).
    pub gap_tools_list_ceil4_tokens: u32,
    /// Direct-MCP `tools/list` payload size (UTF-8 bytes) for `N` tools.
    pub direct_tools_list_bytes: usize,
    /// Direct-MCP `tools/list` tokens (`GQL-022`).
    pub direct_tools_list_tokens: u32,
    /// Direct-MCP `tools/list` tokens (`ceil(bytes / 4)`).
    pub direct_tools_list_ceil4_tokens: u32,
    /// L0 `__metaGraph` token footprint reported by `RouteDiscoveryService::meta_graph` (`RTE-034`).
    pub meta_graph_tokens: u32,
    /// Number of branch synopses returned inside L0 `__metaGraph` (`<= 100`, `GQL-023`).
    pub meta_graph_branches: usize,
    /// Mean `__search` recall@10 over `search_cases.len()` labeled intents.
    pub search_recall_at_10: f64,
    /// Median (`p50`) `__search` latency in milliseconds.
    pub search_p50_ms: f64,
    /// 95th-percentile (`p95`) `__search` latency in milliseconds.
    pub search_p95_ms: f64,
}

/// Aggregated benchmark metrics for one `(suite, n_tools, mode)` cell (`ADR-021`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SuiteModeMetrics {
    /// Suite identifier (`api_lookup`, `cross_branch_join`, `code_graph`).
    pub suite: String,
    /// Supergraph tool count `N` (`0` / workspace scope for `code_graph`).
    pub n_tools: usize,
    /// Agent harness mode (`direct_mcp` or `gap_facade`).
    pub mode: String,
    /// Number of executed tasks.
    pub task_count: usize,
    /// Static `tools/list` tokens loaded in prompt prefix (`GQL-022`).
    pub tools_list_tokens: u32,
    /// Prompt tokens consumed before the first substantive data-returning call (`ADR-021`).
    pub prompt_tokens_before_first_useful_call: f64,
    /// Mean effective tokens per successful task without prompt caching (`ADR-021`).
    pub uncached_tokens_per_success: f64,
    /// Mean effective tokens per successful task with Anthropic/OpenAI-style prompt caching
    /// (`1.25x` cold prefix write amortized over suite tasks + `0.10x` cached prefix reads per
    /// round-trip + `1.0x` dynamic tokens).
    pub cached_tokens_per_success: f64,
    /// Fraction of tasks passing exact verification (`0.0..=1.0`).
    pub success_rate: f64,
    /// Mean ground-truth task accuracy (`0.0..=1.0`, set F1 on `code_graph`).
    pub mean_accuracy: f64,
    /// Mean tool round-trips per task.
    pub mean_round_trips: f64,
    /// Median task execution latency (ms).
    pub p50_latency_ms: f64,
    /// 95th-percentile task execution latency (ms).
    pub p95_latency_ms: f64,
    /// 99th-percentile task execution latency (ms).
    pub p99_latency_ms: f64,
}

/// Side-by-side comparison ratio for one `(suite, n_tools)` cell.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SuiteComparisonRow {
    /// Suite identifier (`api_lookup`, `cross_branch_join`, `code_graph`).
    pub suite: String,
    /// Supergraph tool count `N`.
    pub n_tools: usize,
    /// Direct-MCP baseline metrics.
    pub direct: SuiteModeMetrics,
    /// GAP MCP facade metrics.
    pub gap: SuiteModeMetrics,
    /// `gap / direct` uncached token ratio (`< 1.0` means GAP uses fewer tokens).
    pub uncached_token_ratio: f64,
    /// `gap / direct` prompt-cached token ratio.
    pub cached_token_ratio: f64,
    /// `direct / gap` uncached token reduction factor.
    pub uncached_speedup_factor: f64,
}

/// Binary milestone / architecture gate verdicts evaluated on measured artifacts (`ADR-021`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GateSummary {
    /// `G-F1`: `GapFacade` `tools/list <= 1,200` tokens across all tested `N`.
    pub g_f1_pass: bool,
    /// Worst-case (maximum) GAP `tools/list` tokens across tested `N` (`GQL-022`).
    pub g_f1_max_tokens_gql022: u32,
    /// Worst-case (maximum) GAP `tools/list` tokens (`ceil(bytes / 4)`).
    pub g_f1_max_tokens_ceil4: u32,
    /// `G-C1`: `code_graph` uncached token ratio `<= 0.15` at `>=` equal accuracy (`true` when `code_graph` ran).
    pub g_c1_evaluated: bool,
    /// Whether `G-C1` passed.
    pub g_c1_pass: bool,
    /// Observed `code_graph` uncached token ratio (`gap / direct`).
    pub g_c1_uncached_token_ratio: Option<f64>,
    /// Observed `code_graph` cached token ratio (`gap / direct`).
    pub g_c1_cached_token_ratio: Option<f64>,
    /// Observed GAP vs Direct `code_graph` accuracy.
    pub g_c1_gap_accuracy: Option<f64>,
    /// Observed Direct `code_graph` accuracy.
    pub g_c1_direct_accuracy: Option<f64>,
    /// M2 discovery gate: `__search` recall@10 `>= 0.95` across all `N`.
    pub search_recall_pass: bool,
    /// Minimum observed `__search` recall@10 across `N`.
    pub min_search_recall_at_10: f64,
    /// M2 discovery gate: `__search` p95 `<= 40.0` ms across all `N`.
    pub search_p95_pass: bool,
    /// Maximum observed `__search` p95 latency (ms) across `N`.
    pub max_search_p95_ms: f64,
    /// Conjunction of all evaluated gates.
    pub all_evaluated_gates_pass: bool,
}

/// Complete JSON report serialized to `baseline.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GapBenchReport {
    /// Report schema identifier.
    pub schema_version: String,
    /// Build, host, git commit, and invocation metadata.
    pub environment: EnvironmentInfo,
    /// Per-`N` `tools/list`, `__metaGraph`, and `__search` scaling metrics.
    pub discovery_scaling: Vec<DiscoveryScaleRow>,
    /// Per-`(suite, N)` comparison rows.
    pub suite_comparisons: Vec<SuiteComparisonRow>,
    /// Evaluated M2 / M1 gate verdicts.
    pub gates: GateSummary,
}

/// Executes the full GAP-Bench v1 sweep across `n_values` and `suite_filter`.
pub async fn run_benchmark(
    agent: &BenchAgent,
    suite_filter: SuiteFilter,
    n_values: &[usize],
) -> Result<GapBenchReport, BenchError> {
    if n_values.is_empty() {
        return Err(BenchError::Cli(
            "`--n` must provide at least one integer tool count".to_owned(),
        ));
    }

    let git_commit = capture_git_commit();
    let build_profile = if cfg!(debug_assertions) {
        "debug".to_owned()
    } else {
        "release".to_owned()
    };
    let agent_mode = match agent {
        BenchAgent::Scripted => "scripted".to_owned(),
        BenchAgent::Llm(cfg) => format!("llm:{}", cfg.model),
    };

    let mut discovery_scaling = Vec::with_capacity(n_values.len());
    let mut suite_comparisons = Vec::new();
    let mut shared_gap_tools_list_bytes = 0usize;

    for &n_tools in n_values {
        let sg = build_synthetic_supergraph(n_tools)?;
        let ctx = sg.make_request_context()?;
        let gap_tools_list_vec = fetch_gap_tools_list_bytes(&sg.facade, &ctx).await?;
        shared_gap_tools_list_bytes = gap_tools_list_vec.len();

        let mg = sg.discovery.meta_graph(&ctx);
        let (recall_at_10, search_p50_ms, search_p95_ms) = measure_search_slo(&sg).await?;

        discovery_scaling.push(DiscoveryScaleRow {
            n_tools,
            branches: sg.branch_count,
            gap_tools_list_bytes: gap_tools_list_vec.len(),
            gap_tools_list_tokens: estimate_tokens(gap_tools_list_vec.len()),
            gap_tools_list_ceil4_tokens: estimate_tokens_ceil4(gap_tools_list_vec.len()),
            direct_tools_list_bytes: sg.direct_tools_list_bytes.len(),
            direct_tools_list_tokens: estimate_tokens(sg.direct_tools_list_bytes.len()),
            direct_tools_list_ceil4_tokens: estimate_tokens_ceil4(sg.direct_tools_list_bytes.len()),
            meta_graph_tokens: mg.tokens_estimate,
            meta_graph_branches: mg.branches.len(),
            search_recall_at_10: recall_at_10,
            search_p50_ms,
            search_p95_ms,
        });

        if suite_filter.includes(SuiteFilter::ApiLookup) {
            let mut direct_traces = Vec::with_capacity(sg.api_lookup_tasks.len());
            let mut gap_traces = Vec::with_capacity(sg.api_lookup_tasks.len());
            for task in &sg.api_lookup_tasks {
                direct_traces.push(
                    agent
                        .run_api_lookup(&sg, task, ModeKind::DirectMcp, gap_tools_list_vec.len())
                        .await?,
                );
                gap_traces.push(
                    agent
                        .run_api_lookup(&sg, task, ModeKind::GapFacade, gap_tools_list_vec.len())
                        .await?,
                );
            }
            let direct =
                summarize_traces("api_lookup", n_tools, ModeKind::DirectMcp, &direct_traces);
            let gap = summarize_traces("api_lookup", n_tools, ModeKind::GapFacade, &gap_traces);
            suite_comparisons.push(build_comparison_row("api_lookup", n_tools, direct, gap));
        }

        if suite_filter.includes(SuiteFilter::CrossBranchJoin) {
            let mut direct_traces = Vec::with_capacity(sg.cross_branch_tasks.len());
            let mut gap_traces = Vec::with_capacity(sg.cross_branch_tasks.len());
            for task in &sg.cross_branch_tasks {
                direct_traces.push(
                    agent
                        .run_cross_branch_join(
                            &sg,
                            task,
                            ModeKind::DirectMcp,
                            gap_tools_list_vec.len(),
                        )
                        .await?,
                );
                gap_traces.push(
                    agent
                        .run_cross_branch_join(
                            &sg,
                            task,
                            ModeKind::GapFacade,
                            gap_tools_list_vec.len(),
                        )
                        .await?,
                );
            }
            let direct = summarize_traces(
                "cross_branch_join",
                n_tools,
                ModeKind::DirectMcp,
                &direct_traces,
            );
            let gap = summarize_traces(
                "cross_branch_join",
                n_tools,
                ModeKind::GapFacade,
                &gap_traces,
            );
            suite_comparisons.push(build_comparison_row(
                "cross_branch_join",
                n_tools,
                direct,
                gap,
            ));
        }
    }

    if suite_filter.includes(SuiteFilter::CodeGraph) {
        if shared_gap_tools_list_bytes == 0 {
            let sg = build_synthetic_supergraph(10)?;
            let ctx = sg.make_request_context()?;
            shared_gap_tools_list_bytes = fetch_gap_tools_list_bytes(&sg.facade, &ctx).await?.len();
        }
        let cg = build_code_graph_fixture()?;
        let mut direct_traces = Vec::with_capacity(cg.tasks.len());
        let mut gap_traces = Vec::with_capacity(cg.tasks.len());
        for task in &cg.tasks {
            direct_traces.push(
                agent
                    .run_code_graph(&cg, task, ModeKind::DirectMcp, shared_gap_tools_list_bytes)
                    .await?,
            );
            gap_traces.push(
                agent
                    .run_code_graph(&cg, task, ModeKind::GapFacade, shared_gap_tools_list_bytes)
                    .await?,
            );
        }
        let direct = summarize_traces("code_graph", 24, ModeKind::DirectMcp, &direct_traces);
        let gap = summarize_traces("code_graph", 24, ModeKind::GapFacade, &gap_traces);
        suite_comparisons.push(build_comparison_row("code_graph", 24, direct, gap));
    }

    let gates = evaluate_gates(&discovery_scaling, &suite_comparisons);

    Ok(GapBenchReport {
        schema_version: "gapbench/v1".to_owned(),
        environment: EnvironmentInfo {
            git_commit,
            os: std::env::consts::OS.to_owned(),
            arch: std::env::consts::ARCH.to_owned(),
            build_profile,
            agent_mode,
            n_sweep: n_values.to_vec(),
            suite_filter,
        },
        discovery_scaling,
        suite_comparisons,
        gates,
    })
}

#[allow(clippy::cast_precision_loss)]
async fn measure_search_slo(sg: &SyntheticSupergraph) -> Result<(f64, f64, f64), BenchError> {
    let ctx = sg.make_request_context()?;
    let mut latencies_ms = Vec::with_capacity(sg.search_cases.len());
    let mut hits_at_10 = 0usize;

    // Warm up allocator / index access once so p95 reflects steady-state retrieval.
    if let Some(first) = sg.search_cases.first() {
        let _ = sg
            .discovery
            .search(
                &ctx,
                SearchRequest {
                    query: first.query.clone(),
                    first: 10,
                    ..SearchRequest::default()
                },
            )
            .await?;
    }

    for case in &sg.search_cases {
        let started = Instant::now();
        let resp = sg
            .discovery
            .search(
                &ctx,
                SearchRequest {
                    query: case.query.clone(),
                    first: 10,
                    ..SearchRequest::default()
                },
            )
            .await?;
        latencies_ms.push(started.elapsed().as_secs_f64() * 1_000.0);
        if resp
            .hits
            .iter()
            .take(10)
            .any(|h| h.name == case.expected_op_name)
        {
            hits_at_10 += 1;
        }
    }

    let total = sg.search_cases.len().max(1) as f64;
    let recall = (hits_at_10 as f64) / total;
    let p50 = percentile(&latencies_ms, 0.50);
    let p95 = percentile(&latencies_ms, 0.95);
    Ok((recall, p50, p95))
}

#[allow(clippy::cast_precision_loss)]
fn summarize_traces(
    suite: &str,
    n_tools: usize,
    mode: ModeKind,
    traces: &[TaskTrace],
) -> SuiteModeMetrics {
    let count = traces.len().max(1);
    let task_count_f = count as f64;

    let mut tools_list_tokens = 0u32;
    let mut first_useful_tokens_sum = 0.0;
    let mut uncached_success_tokens_sum = 0.0;
    let mut cached_success_tokens_sum = 0.0;
    let mut successes = 0usize;
    let mut accuracy_sum = 0.0;
    let mut round_trips_sum = 0usize;
    let mut latencies_ms = Vec::with_capacity(traces.len());

    for trace in traces {
        tools_list_tokens = estimate_tokens(trace.tools_list_bytes);
        let static_prefix_bytes = trace.tools_list_bytes + trace.initial_prompt_bytes;
        let static_prefix_tokens = f64::from(estimate_tokens(static_prefix_bytes));

        let mut cumulative_dynamic_bytes = 0usize;
        let mut uncached_task_tokens = 0.0;
        let mut cached_task_tokens = 0.0;
        let mut prompt_before_useful: Option<f64> = None;

        for (turn_idx, turn) in trace.turns.iter().enumerate() {
            let input_dyn_bytes = cumulative_dynamic_bytes + turn.prompt_bytes;
            let input_dyn_tokens = f64::from(estimate_tokens(input_dyn_bytes));
            let output_tokens = f64::from(estimate_tokens(turn.response_bytes));

            if turn.is_useful_call && prompt_before_useful.is_none() {
                prompt_before_useful = Some(static_prefix_tokens + input_dyn_tokens);
            }

            // Uncached accounting: static prefix is billed cleanly at 1.0x every round trip.
            uncached_task_tokens += static_prefix_tokens + input_dyn_tokens + output_tokens;

            // Prompt-cached accounting: static prefix incurs 1.25x cache write amortized across
            // the suite session on turn 0, plus 0.10x cached read per round trip; dynamic tail = 1.0x.
            let prefix_write_amortized = if turn_idx == 0 {
                (1.25 * static_prefix_tokens) / task_count_f
            } else {
                0.0
            };
            cached_task_tokens += prefix_write_amortized
                + (0.10 * static_prefix_tokens)
                + input_dyn_tokens
                + output_tokens;

            cumulative_dynamic_bytes += turn.prompt_bytes + turn.response_bytes;
        }

        let before_useful = prompt_before_useful.unwrap_or_else(|| {
            static_prefix_tokens + f64::from(estimate_tokens(cumulative_dynamic_bytes))
        });
        first_useful_tokens_sum += before_useful;

        if trace.succeeded {
            successes += 1;
            uncached_success_tokens_sum += uncached_task_tokens;
            cached_success_tokens_sum += cached_task_tokens;
        }
        accuracy_sum += trace.accuracy;
        round_trips_sum += trace.turns.len();
        latencies_ms.push(trace.latency_ms);
    }

    let denom_success = if successes > 0 {
        successes as f64
    } else {
        f64::INFINITY
    };

    SuiteModeMetrics {
        suite: suite.to_owned(),
        n_tools,
        mode: mode.as_str().to_owned(),
        task_count: traces.len(),
        tools_list_tokens,
        prompt_tokens_before_first_useful_call: first_useful_tokens_sum / task_count_f,
        uncached_tokens_per_success: uncached_success_tokens_sum / denom_success,
        cached_tokens_per_success: cached_success_tokens_sum / denom_success,
        success_rate: (successes as f64) / task_count_f,
        mean_accuracy: accuracy_sum / task_count_f,
        mean_round_trips: (round_trips_sum as f64) / task_count_f,
        p50_latency_ms: percentile(&latencies_ms, 0.50),
        p95_latency_ms: percentile(&latencies_ms, 0.95),
        p99_latency_ms: percentile(&latencies_ms, 0.99),
    }
}

fn build_comparison_row(
    suite: &str,
    n_tools: usize,
    direct: SuiteModeMetrics,
    gap: SuiteModeMetrics,
) -> SuiteComparisonRow {
    let uncached_ratio = if direct.uncached_tokens_per_success > 0.0 {
        gap.uncached_tokens_per_success / direct.uncached_tokens_per_success
    } else {
        0.0
    };
    let cached_ratio = if direct.cached_tokens_per_success > 0.0 {
        gap.cached_tokens_per_success / direct.cached_tokens_per_success
    } else {
        0.0
    };
    let speedup = if gap.uncached_tokens_per_success > 0.0 {
        direct.uncached_tokens_per_success / gap.uncached_tokens_per_success
    } else {
        0.0
    };
    SuiteComparisonRow {
        suite: suite.to_owned(),
        n_tools,
        direct,
        gap,
        uncached_token_ratio: uncached_ratio,
        cached_token_ratio: cached_ratio,
        uncached_speedup_factor: speedup,
    }
}

fn evaluate_gates(
    discovery: &[DiscoveryScaleRow],
    comparisons: &[SuiteComparisonRow],
) -> GateSummary {
    let g_f1_max_tokens_gql022 = discovery
        .iter()
        .map(|d| d.gap_tools_list_tokens)
        .max()
        .unwrap_or(0);
    let g_f1_max_tokens_ceil4 = discovery
        .iter()
        .map(|d| d.gap_tools_list_ceil4_tokens)
        .max()
        .unwrap_or(0);
    let g_f1_pass = !discovery.is_empty()
        && g_f1_max_tokens_gql022 <= GATE_GF1_MAX_TOKENS
        && g_f1_max_tokens_ceil4 <= GATE_GF1_MAX_TOKENS;

    let min_search_recall_at_10 = discovery
        .iter()
        .map(|d| d.search_recall_at_10)
        .fold(f64::INFINITY, f64::min);
    let min_search_recall_at_10 = if min_search_recall_at_10.is_finite() {
        min_search_recall_at_10
    } else {
        0.0
    };
    let search_recall_pass =
        !discovery.is_empty() && min_search_recall_at_10 >= GATE_SEARCH_MIN_RECALL_AT_10;

    let max_search_p95_ms = discovery
        .iter()
        .map(|d| d.search_p95_ms)
        .fold(0.0_f64, f64::max);
    let search_p95_pass = !discovery.is_empty() && max_search_p95_ms <= GATE_SEARCH_MAX_P95_MS;

    let code_row = comparisons.iter().find(|r| r.suite == "code_graph");
    let (g_c1_evaluated, g_c1_pass, g_c1_uncached, g_c1_cached, g_c1_gap_acc, g_c1_direct_acc) =
        if let Some(row) = code_row {
            let pass = row.direct.uncached_tokens_per_success > 0.0
                && row.direct.success_rate >= 1.0
                && row.uncached_token_ratio <= GATE_GC1_MAX_RATIO
                && row.gap.mean_accuracy >= row.direct.mean_accuracy
                && row.gap.success_rate >= 1.0;
            (
                true,
                pass,
                Some(row.uncached_token_ratio),
                Some(row.cached_token_ratio),
                Some(row.gap.mean_accuracy),
                Some(row.direct.mean_accuracy),
            )
        } else {
            (false, true, None, None, None, None)
        };

    let all_evaluated_gates_pass =
        g_f1_pass && search_recall_pass && search_p95_pass && (!g_c1_evaluated || g_c1_pass);

    GateSummary {
        g_f1_pass,
        g_f1_max_tokens_gql022,
        g_f1_max_tokens_ceil4,
        g_c1_evaluated,
        g_c1_pass,
        g_c1_uncached_token_ratio: g_c1_uncached,
        g_c1_cached_token_ratio: g_c1_cached,
        g_c1_gap_accuracy: g_c1_gap_acc,
        g_c1_direct_accuracy: g_c1_direct_acc,
        search_recall_pass,
        min_search_recall_at_10,
        search_p95_pass,
        max_search_p95_ms,
        all_evaluated_gates_pass,
    }
}

#[allow(clippy::cast_precision_loss)]
fn percentile(values: &[f64], q: f64) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let idx = (((sorted.len() - 1) as f64) * q).ceil() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

fn capture_git_commit() -> String {
    let Ok(out) = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
    else {
        return "unknown".to_owned();
    };
    if out.status.success() {
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    } else {
        "unknown".to_owned()
    }
}

/// Writes `baseline.json` and `README.md` into `out_dir`.
pub fn write_report_bundle(out_dir: &Path, report: &GapBenchReport) -> Result<(), BenchError> {
    fs::create_dir_all(out_dir)?;
    let json_path = out_dir.join("baseline.json");
    let readme_path = out_dir.join("README.md");

    let json_body = serde_json::to_string_pretty(report)?;
    fs::write(&json_path, json_body)?;

    let md_body = render_markdown_report(report);
    fs::write(&readme_path, md_body)?;
    Ok(())
}

/// Renders the Markdown baseline report for `bench/results/gapbench/README.md` (`ADR-021`).
#[must_use]
pub fn render_markdown_report(report: &GapBenchReport) -> String {
    let mut md = String::with_capacity(4096);
    let _ = writeln!(
        md,
        "# GAP-Bench v1 Baseline Results (`ADR-021`, `CMP-008`, `RTE-034`)"
    );
    let _ = writeln!(md);
    let _ = writeln!(
        md,
        "Measured in-process end-to-end comparison between **Direct-MCP** (loading all `N` tool schemas into prompt context, plus filesystem grep/read loops for code navigation) and the **GAP MCP `2026-07-28` Facade** (`McpFacade` over `TransportEngine`, `ExecLayer`, `RouteDiscoveryService`, and `SqliteContextStore`)."
    );
    let _ = writeln!(md);
    let _ = writeln!(md, "## Environment & Provenance");
    let _ = writeln!(md);
    let _ = writeln!(md, "- **Git commit:** `{}`", report.environment.git_commit);
    let _ = writeln!(
        md,
        "- **Host platform:** `{}/{}` (`{}` profile)",
        report.environment.os, report.environment.arch, report.environment.build_profile
    );
    let _ = writeln!(
        md,
        "- **Agent driver:** `{}`",
        report.environment.agent_mode
    );
    let _ = writeln!(md, "- **N-sweep:** `{:?}`", report.environment.n_sweep);
    let _ = writeln!(
        md,
        "- **Token estimators:** Primary `GQL-022` `ceil(utf8_bytes / 3.6)`; secondary `ceil(utf8_bytes / 4)`. Cached token accounting bills `1.25x` cold prefix write amortized over suite tasks + `0.10x` cached prefix read per round-trip + `1.0x` dynamic turn tokens."
    );
    let _ = writeln!(md);

    let _ = writeln!(md, "## Gate Verdicts");
    let _ = writeln!(md);
    let _ = writeln!(md, "| Gate | Criterion | Measured | Verdict |");
    let _ = writeln!(md, "|---|---|---|---|");
    let _ = writeln!(
        md,
        "| `G-F1` (`CMP-008`) | `gap_tools_list <= 1,200` tokens across `N` | max `{}` (`GQL-022`), `{}` (`ceil/4`) | **{}** |",
        report.gates.g_f1_max_tokens_gql022,
        report.gates.g_f1_max_tokens_ceil4,
        if report.gates.g_f1_pass {
            "PASS"
        } else {
            "FAIL"
        }
    );
    if report.gates.g_c1_evaluated {
        let ratio = report.gates.g_c1_uncached_token_ratio.unwrap_or(1.0);
        let gap_acc = report.gates.g_c1_gap_accuracy.unwrap_or(0.0);
        let dir_acc = report.gates.g_c1_direct_accuracy.unwrap_or(0.0);
        let _ = writeln!(
            md,
            "| `G-C1` (M2) | `code_graph` tokens `<= 15%` of file-scan at `>=` accuracy | `{:.2}%` uncached (`{:.3}` vs `{:.3}` accuracy) | **{}** |",
            ratio * 100.0,
            gap_acc,
            dir_acc,
            if report.gates.g_c1_pass {
                "PASS"
            } else {
                "FAIL"
            }
        );
    }
    let _ = writeln!(
        md,
        "| `__search` Recall@10 (M2) | `recall@10 >= 0.95` across `N` | min `{:.4}` | **{}** |",
        report.gates.min_search_recall_at_10,
        if report.gates.search_recall_pass {
            "PASS"
        } else {
            "FAIL"
        }
    );
    let _ = writeln!(
        md,
        "| `__search` Latency (M2) | `p95 <= 40.0 ms` across `N` | max `{:.3} ms` | **{}** |",
        report.gates.max_search_p95_ms,
        if report.gates.search_p95_pass {
            "PASS"
        } else {
            "FAIL"
        }
    );
    let _ = writeln!(md);

    let _ = writeln!(
        md,
        "## Discovery & Schema Footprint Scaling (`tools/list`, `__metaGraph`, `__search`)"
    );
    let _ = writeln!(md);
    let _ = writeln!(
        md,
        "| `N` Tools | Branches | GAP `tools/list` (`GQL-022`) | Direct-MCP `tools/list` (`GQL-022`) | `__metaGraph` Tokens | `__search` Recall@10 | `__search` p50 (ms) | `__search` p95 (ms) |"
    );
    let _ = writeln!(md, "|---:|---:|---:|---:|---:|---:|---:|---:|");
    for row in &report.discovery_scaling {
        let _ = writeln!(
            md,
            "| {} | {} | {} | {} | {} ({} br) | {:.4} | {:.3} | {:.3} |",
            row.n_tools,
            row.branches,
            row.gap_tools_list_tokens,
            row.direct_tools_list_tokens,
            row.meta_graph_tokens,
            row.meta_graph_branches,
            row.search_recall_at_10,
            row.search_p50_ms,
            row.search_p95_ms,
        );
    }
    let _ = writeln!(md);

    let _ = writeln!(
        md,
        "## End-to-End Task Suites (`Direct-MCP` vs `GAP-Facade`)"
    );
    let _ = writeln!(md);
    let _ = writeln!(
        md,
        "| Suite | `N` | Direct 1st-Call Prompt | GAP 1st-Call Prompt | Direct Uncached / Succ | GAP Uncached / Succ | Reduction | Direct Cached / Succ | GAP Cached / Succ | Round-Trips (D / G) | Success (D / G) | Latency p50/p95 ms (G) |"
    );
    let _ = writeln!(
        md,
        "|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|"
    );
    for row in &report.suite_comparisons {
        let _ = writeln!(
            md,
            "| `{}` | {} | {:.1} | {:.1} | {:.1} | {:.1} | {:.2}x ({:.1}%) | {:.1} | {:.1} | {:.1} / {:.1} | {:.0}% / {:.0}% | {:.3} / {:.3} |",
            row.suite,
            row.n_tools,
            row.direct.prompt_tokens_before_first_useful_call,
            row.gap.prompt_tokens_before_first_useful_call,
            row.direct.uncached_tokens_per_success,
            row.gap.uncached_tokens_per_success,
            row.uncached_speedup_factor,
            row.uncached_token_ratio * 100.0,
            row.direct.cached_tokens_per_success,
            row.gap.cached_tokens_per_success,
            row.direct.mean_round_trips,
            row.gap.mean_round_trips,
            row.direct.success_rate * 100.0,
            row.gap.success_rate * 100.0,
            row.gap.p50_latency_ms,
            row.gap.p95_latency_ms,
        );
    }
    md
}
