//! GAP-Bench token-efficiency and latency benchmark harness (`L5`, `WP-M2-07`, `ADR-021`).
#![forbid(unsafe_code)]
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::todo,
    clippy::unimplemented
)]

pub mod agent;
pub mod metrics;
pub mod synth;

use std::env;
use std::path::PathBuf;
use std::process::ExitCode;

use agent::{BenchAgent, LlmConfig};
use metrics::{run_benchmark, write_report_bundle};
use synth::{BenchError, SuiteFilter};

#[derive(Debug, Clone)]
struct CliArgs {
    suite: SuiteFilter,
    n_values: Vec<usize>,
    scripted: bool,
    llm_endpoint: Option<String>,
    llm_model: Option<String>,
    out_dir: PathBuf,
    verify_results: Option<PathBuf>,
    help: bool,
}

impl Default for CliArgs {
    fn default() -> Self {
        Self {
            suite: SuiteFilter::All,
            n_values: vec![10, 50, 200, 1000],
            scripted: false,
            llm_endpoint: None,
            llm_model: None,
            out_dir: PathBuf::from("target/gapbench-out"),
            verify_results: None,
            help: false,
        }
    }
}

fn parse_cli(args: impl Iterator<Item = String>) -> Result<CliArgs, BenchError> {
    let mut cfg = CliArgs::default();
    let mut it = args.skip(1);

    while let Some(arg) = it.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                cfg.help = true;
            }
            "--suite" => {
                let val = it
                    .next()
                    .ok_or_else(|| BenchError::Cli("missing value after `--suite`".to_owned()))?;
                cfg.suite = SuiteFilter::parse(&val)?;
            }
            "--n" => {
                let raw = it
                    .next()
                    .ok_or_else(|| BenchError::Cli("missing CSV list after `--n`".to_owned()))?;
                let mut vals = Vec::new();
                for part in raw.split(',') {
                    let trimmed = part.trim();
                    if trimmed.is_empty() {
                        continue;
                    }
                    let parsed = trimmed.parse::<usize>().map_err(|_| {
                        BenchError::Cli(format!("invalid integer `{trimmed}` in `--n`"))
                    })?;
                    vals.push(parsed);
                }
                if vals.is_empty() {
                    return Err(BenchError::Cli(
                        "`--n` must specify at least one integer".to_owned(),
                    ));
                }
                cfg.n_values = vals;
            }
            "--scripted" => {
                cfg.scripted = true;
            }
            "--llm-endpoint" => {
                let val = it.next().ok_or_else(|| {
                    BenchError::Cli("missing URL after `--llm-endpoint`".to_owned())
                })?;
                cfg.llm_endpoint = Some(val);
            }
            "--llm-model" => {
                let val = it.next().ok_or_else(|| {
                    BenchError::Cli("missing model name after `--llm-model`".to_owned())
                })?;
                cfg.llm_model = Some(val);
            }
            "--out" => {
                let val = it
                    .next()
                    .ok_or_else(|| BenchError::Cli("missing directory after `--out`".to_owned()))?;
                cfg.out_dir = PathBuf::from(val);
            }
            "--verify-results" => {
                let val = it.next().ok_or_else(|| {
                    BenchError::Cli("missing directory after `--verify-results`".to_owned())
                })?;
                cfg.verify_results = Some(PathBuf::from(val));
            }
            other => {
                return Err(BenchError::Cli(format!(
                    "unrecognized argument `{other}` (pass `--help` for usage)"
                )));
            }
        }
    }

    Ok(cfg)
}

fn build_agent(cfg: &CliArgs) -> Result<BenchAgent, BenchError> {
    if cfg.scripted || cfg.llm_endpoint.is_none() {
        return Ok(BenchAgent::Scripted);
    }
    let endpoint = cfg
        .llm_endpoint
        .clone()
        .ok_or_else(|| BenchError::Cli("missing `--llm-endpoint`".to_owned()))?;
    let model = cfg
        .llm_model
        .clone()
        .unwrap_or_else(|| "gpt-4o-mini".to_owned());
    let api_key = env::var("GAPBENCH_LLM_API_KEY")
        .ok()
        .filter(|v| !v.trim().is_empty());
    Ok(BenchAgent::Llm(LlmConfig {
        endpoint,
        model,
        api_key,
    }))
}

fn print_usage() {
    println!(
        "GAP-Bench v1 harness (ADR-021, CMP-008, RTE-034)\n\
         \n\
         USAGE:\n\
         \tgapbench [OPTIONS]\n\
         \n\
         OPTIONS:\n\
         \t--suite <all|api_lookup|cross_branch_join|code_graph>   Suite selector (default: all)\n\
         \t--n <csv>                                                Comma-separated N sweep (default: 10,50,200,1000)\n\
         \t--scripted                                               Use deterministic scripted agent (offline)\n\
         \t--llm-endpoint <url>                                     Optional OpenAI-compatible HTTP endpoint\n\
         \t--llm-model <name>                                       Model id forwarded to `--llm-endpoint`\n\
         \t--out <dir>                                              Output directory for baseline.json + README.md\n\
         \t--verify-results <dir>                                   Validate existing baseline.json + README.md bundle\n\
         \t-h, --help                                               Print help\n"
    );
}

fn verify_results_bundle(
    dir: &std::path::Path,
) -> Result<crate::metrics::GapBenchReport, BenchError> {
    let json_path = dir.join("baseline.json");
    let readme_path = dir.join("README.md");
    let raw_json = std::fs::read_to_string(&json_path)
        .map_err(|e| BenchError::Cli(format!("failed to read `{}`: {e}", json_path.display())))?;
    let report: crate::metrics::GapBenchReport = serde_json::from_str(&raw_json)
        .map_err(|e| BenchError::Cli(format!("failed to parse `{}`: {e}", json_path.display())))?;
    let readme = std::fs::read_to_string(&readme_path)
        .map_err(|e| BenchError::Cli(format!("failed to read `{}`: {e}", readme_path.display())))?;
    if report.discovery_scaling.len() < 4 {
        return Err(BenchError::Cli(format!(
            "expected >= 4 discovery N-sweep rows in `{}` (found {})",
            json_path.display(),
            report.discovery_scaling.len()
        )));
    }
    for required_suite in ["api_lookup", "cross_branch_join", "code_graph"] {
        if !report
            .suite_comparisons
            .iter()
            .any(|r| r.suite == required_suite)
        {
            return Err(BenchError::Cli(format!(
                "missing `{required_suite}` comparison rows in `{}`",
                json_path.display()
            )));
        }
    }
    if !report.gates.all_evaluated_gates_pass {
        return Err(BenchError::Cli(format!(
            "benchmark gates did not pass in `{}`",
            json_path.display()
        )));
    }
    if !readme.contains("G-F1") || !readme.contains("G-C1") {
        return Err(BenchError::Cli(format!(
            "`{}` missing G-F1 / G-C1 gate summary",
            readme_path.display()
        )));
    }
    Ok(report)
}

async fn async_main(cfg: CliArgs) -> Result<(), BenchError> {
    if cfg.help {
        print_usage();
        return Ok(());
    }

    if let Some(dir) = &cfg.verify_results {
        let report = verify_results_bundle(dir)?;
        println!(
            "GAP-Bench results verified: dir={}, scaling_rows={}, suite_rows={}, g_f1={}, g_c1={}, all_gates={}",
            dir.display(),
            report.discovery_scaling.len(),
            report.suite_comparisons.len(),
            report.gates.g_f1_pass,
            report.gates.g_c1_pass,
            report.gates.all_evaluated_gates_pass,
        );
        return Ok(());
    }

    let agent = build_agent(&cfg)?;
    let report = run_benchmark(&agent, cfg.suite, &cfg.n_values).await?;
    write_report_bundle(&cfg.out_dir, &report)?;

    println!(
        "GAP-Bench complete: out={}, g_f1={}, g_c1={}, search_recall_min={:.4}, search_p95_max={:.3}ms, all_gates={}",
        cfg.out_dir.display(),
        report.gates.g_f1_pass,
        report.gates.g_c1_pass,
        report.gates.min_search_recall_at_10,
        report.gates.max_search_p95_ms,
        report.gates.all_evaluated_gates_pass,
    );

    if !report.gates.all_evaluated_gates_pass {
        return Err(BenchError::Cli(
            "one or more GAP-Bench M2 gates failed threshold checks".to_owned(),
        ));
    }

    Ok(())
}

#[tokio::main]
async fn main() -> ExitCode {
    let cfg = match parse_cli(env::args()) {
        Ok(c) => c,
        Err(err) => {
            eprintln!("error: {err}");
            return ExitCode::from(2);
        }
    };

    match async_main(cfg).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("error: {err}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use tempfile::TempDir;

    use crate::agent::{BenchAgent, fetch_gap_tools_list_bytes};
    use crate::metrics::{
        GATE_GC1_MAX_RATIO, GATE_GF1_MAX_TOKENS, GATE_SEARCH_MAX_P95_MS,
        GATE_SEARCH_MIN_RECALL_AT_10, GapBenchReport, estimate_tokens_ceil4, run_benchmark,
        write_report_bundle,
    };
    use crate::synth::{SuiteFilter, build_synthetic_supergraph};

    // spec: CMP-008
    #[tokio::test]
    async fn cmp_008_gap_facade_tools_list_stays_under_1200_tokens_across_n_sweep() {
        let mut prev_direct_bytes = 0usize;
        for n in [10usize, 50, 200, 1000] {
            let sg = build_synthetic_supergraph(n).unwrap();
            let ctx = sg.make_request_context().unwrap();
            let gap_bytes = fetch_gap_tools_list_bytes(&sg.facade, &ctx).await.unwrap();
            let gap_tokens_gql022 = gap_route::estimate_tokens(gap_bytes.len());
            let gap_tokens_ceil4 = estimate_tokens_ceil4(gap_bytes.len());
            assert!(
                gap_tokens_gql022 <= GATE_GF1_MAX_TOKENS,
                "G-F1 violation at N={n}: gql022={gap_tokens_gql022}"
            );
            assert!(
                gap_tokens_ceil4 <= GATE_GF1_MAX_TOKENS,
                "G-F1 violation at N={n}: ceil4={gap_tokens_ceil4}"
            );
            assert!(
                sg.direct_tools_list_bytes.len() > prev_direct_bytes,
                "Direct-MCP tools/list must scale monotonically with N={n}"
            );
            prev_direct_bytes = sg.direct_tools_list_bytes.len();
        }
    }

    // spec: RTE-034
    #[tokio::test]
    async fn rte_034_meta_graph_and_search_latency_scale_across_synthetic_branches() {
        let report = run_benchmark(
            &BenchAgent::Scripted,
            SuiteFilter::ApiLookup,
            &[10, 50, 200],
        )
        .await
        .unwrap();
        assert_eq!(report.discovery_scaling.len(), 3);
        for row in &report.discovery_scaling {
            assert!(
                row.meta_graph_tokens > 0 && row.meta_graph_tokens < 4_000,
                "bounded L0 __metaGraph size failed at N={}: {}",
                row.n_tools,
                row.meta_graph_tokens
            );
            assert!(
                row.search_recall_at_10 >= GATE_SEARCH_MIN_RECALL_AT_10,
                "__search recall@10 < 0.95 at N={}: {}",
                row.n_tools,
                row.search_recall_at_10
            );
            assert!(
                row.search_p95_ms <= GATE_SEARCH_MAX_P95_MS,
                "__search p95 > 40ms at N={}: {}",
                row.n_tools,
                row.search_p95_ms
            );
        }
    }

    // spec: CMP-008
    #[tokio::test]
    async fn cmp_008_cross_branch_join_and_code_graph_beat_direct_mcp_tokens() {
        let report = run_benchmark(&BenchAgent::Scripted, SuiteFilter::All, &[50])
            .await
            .unwrap();

        let join_row = report
            .suite_comparisons
            .iter()
            .find(|r| r.suite == "cross_branch_join")
            .unwrap();
        assert!(join_row.gap.mean_round_trips < join_row.direct.mean_round_trips);
        assert!(
            join_row.gap.uncached_tokens_per_success < join_row.direct.uncached_tokens_per_success
        );
        assert!(join_row.gap.success_rate >= 1.0);

        let code_row = report
            .suite_comparisons
            .iter()
            .find(|r| r.suite == "code_graph")
            .unwrap();
        assert!(
            code_row.uncached_token_ratio <= GATE_GC1_MAX_RATIO,
            "G-C1 violation: ratio={}",
            code_row.uncached_token_ratio
        );
        assert!(code_row.direct.uncached_tokens_per_success > 0.0);
        assert!((code_row.direct.mean_accuracy - 1.0).abs() < f64::EPSILON);
        assert!(code_row.gap.mean_accuracy >= code_row.direct.mean_accuracy);
        assert!((code_row.gap.mean_accuracy - 1.0).abs() < f64::EPSILON);
        assert!(report.gates.g_c1_pass);
    }

    // spec: CMP-008, RTE-034
    #[tokio::test]
    async fn cmp_008_rte_034_cli_smoke_writes_baseline_json_and_readme() {
        let dir = TempDir::new().unwrap();
        let report = run_benchmark(&BenchAgent::Scripted, SuiteFilter::All, &[10, 50])
            .await
            .unwrap();
        assert!(report.gates.all_evaluated_gates_pass);
        write_report_bundle(dir.path(), &report).unwrap();

        let json_raw = std::fs::read_to_string(dir.path().join("baseline.json")).unwrap();
        let parsed: GapBenchReport = serde_json::from_str(&json_raw).unwrap();
        assert_eq!(parsed.discovery_scaling.len(), 2);
        assert!(parsed.gates.g_f1_pass);
        assert!(parsed.gates.g_c1_pass);

        let md_raw = std::fs::read_to_string(dir.path().join("README.md")).unwrap();
        assert!(md_raw.contains("G-F1"));
        assert!(md_raw.contains("G-C1"));
    }

    // spec: CMP-008
    #[tokio::test]
    async fn cmp_008_verify_results_validates_complete_bundle_and_rejects_partial() {
        let dir = TempDir::new().unwrap();
        let full = run_benchmark(
            &BenchAgent::Scripted,
            SuiteFilter::All,
            &[10, 50, 200, 1000],
        )
        .await
        .unwrap();
        write_report_bundle(dir.path(), &full).unwrap();
        let verified = super::verify_results_bundle(dir.path()).unwrap();
        assert_eq!(verified.discovery_scaling.len(), 4);

        let partial = run_benchmark(&BenchAgent::Scripted, SuiteFilter::ApiLookup, &[10])
            .await
            .unwrap();
        write_report_bundle(dir.path(), &partial).unwrap();
        assert!(super::verify_results_bundle(dir.path()).is_err());
    }
}
