//! JSON and Markdown report serializers for RouteBench v1 (`RTE-061`, `ADR-007`).

use std::fmt::Write as _;
use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::metrics::{
    BucketCalibrationSummary, GateGS1Evaluation, MetricWithCi, ReliabilityBin, SplitEvaluation,
};

/// Errors raised while writing RouteBench report artifacts.
#[derive(Debug, Error)]
pub enum ReportError {
    /// Writing files failed.
    #[error("failed writing report to `{path}`: {source}")]
    Io {
        /// Destination file path.
        path: String,
        /// Underlying OS error.
        #[source]
        source: std::io::Error,
    },
    /// Serializing JSON failed.
    #[error("failed serializing JSON report: {0}")]
    Json(#[from] serde_json::Error),
}

/// Counts of branches, operations, and examples in the evaluated dataset.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DatasetSummary {
    /// Total branch count across all loaded specs.
    pub total_branches: usize,
    /// In-domain branch count.
    pub in_domain_branches: usize,
    /// Cold-start held-out branch count (`RTE-060`).
    pub held_out_branches: usize,
    /// Number of operations in `Fit`, `Calibration`, `Test`, `HeldOutBranch`.
    pub fit_operations: usize,
    /// Calibration operation count.
    pub calibration_operations: usize,
    /// Test operation count.
    pub test_operations: usize,
    /// Example counts across splits.
    pub fit_examples: usize,
    /// Calibration example count.
    pub calibration_examples: usize,
    /// Test example count.
    pub test_examples: usize,
    /// Held-out cold-start example count.
    pub held_out_examples: usize,
}

/// Top-level JSON report emitted by `routebench` (`ADR-007`, `RTE-061`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RouteBenchReport {
    /// Seed used for synthesis and bootstrap intervals.
    pub seed: u64,
    /// Whether `--smoke` mode was active.
    pub smoke: bool,
    /// Bootstrap resamples computed per metric.
    pub bootstrap_iters: usize,
    /// Evaluated router identifier (e.g., `"baseline"`).
    pub router_id: String,
    /// Dataset split inventory summary.
    pub dataset: DatasetSummary,
    /// Per-bucket fitted temperatures and conformal calibration metadata.
    pub calibration_buckets: Vec<BucketCalibrationSummary>,
    /// Held-out in-domain `Test` split evaluation (`RTE-061`).
    pub test_metrics: SplitEvaluation,
    /// Cold-start `HeldOutBranch` evaluation (`RTE-060`).
    pub held_out_metrics: SplitEvaluation,
    /// Gate `G-S1` check against SLOs and calibration requirements (`RTE-069`).
    pub gate_gs1: GateGS1Evaluation,
}

/// Writes `report.json` and `report.md` into `out_dir`, creating directories as needed.
pub fn write_reports(out_dir: &Path, report: &RouteBenchReport) -> Result<(), ReportError> {
    fs::create_dir_all(out_dir).map_err(|source| ReportError::Io {
        path: out_dir.display().to_string(),
        source,
    })?;

    let json_path = out_dir.join("report.json");
    let json_body = serde_json::to_string_pretty(report)?;
    fs::write(&json_path, format!("{json_body}\n")).map_err(|source| ReportError::Io {
        path: json_path.display().to_string(),
        source,
    })?;

    let md_path = out_dir.join("report.md");
    let md_body = render_markdown(report);
    fs::write(&md_path, md_body).map_err(|source| ReportError::Io {
        path: md_path.display().to_string(),
        source,
    })?;

    Ok(())
}

/// Formats `report` as human-readable Markdown with CI tables and reliability bins.
#[must_use]
pub fn render_markdown(report: &RouteBenchReport) -> String {
    let mut out = String::with_capacity(2048);
    let _ = writeln!(out, "# RouteBench v1 Report");
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "- **Router:** `{}` | **Seed:** `{}` | **Smoke:** `{}` | **Bootstrap resamples:** `{}`",
        report.router_id, report.seed, report.smoke, report.bootstrap_iters
    );
    let _ = writeln!(
        out,
        "- **Branches:** {} total ({} in-domain, {} held-out) | **Examples:** fit={}, cal={}, test={}, held-out={}",
        report.dataset.total_branches,
        report.dataset.in_domain_branches,
        report.dataset.held_out_branches,
        report.dataset.fit_examples,
        report.dataset.calibration_examples,
        report.dataset.test_examples,
        report.dataset.held_out_examples,
    );
    let _ = writeln!(out);

    append_split_table(
        &mut out,
        "Test Split Metrics (95% Bootstrap CI)",
        &report.test_metrics,
    );
    append_split_table(
        &mut out,
        "Held-Out Branch Cold-Start Metrics (95% Bootstrap CI)",
        &report.held_out_metrics,
    );

    let _ = writeln!(out, "## Per-Bucket Calibration");
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "| Bucket | Mode | N_fit | N_cal | T_hat | alpha | q_hat | ECE before | ECE after |"
    );
    let _ = writeln!(out, "|---|---|---|---|---|---|---|---|---|");
    for b in &report.calibration_buckets {
        let q_str = b
            .q_hat
            .map_or_else(|| "fallback".to_owned(), |v| format!("{v:.4}"));
        let _ = writeln!(
            out,
            "| {} | `{}` | {} | {} | {:.3} | {:.2} | {} | {:.4} | {:.4} |",
            b.bucket,
            b.mode,
            b.fit_count,
            b.cal_count,
            b.temperature,
            b.alpha,
            q_str,
            b.ece_before,
            b.ece_after
        );
    }
    let _ = writeln!(out);

    append_reliability_table(
        &mut out,
        "Test Split Reliability Diagram (15 Bins)",
        &report.test_metrics.reliability_bins,
    );

    let _ = writeln!(out, "## Gate G-S1 Summary (`RTE-069`)");
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "- **Meets baseline delta (>= +0.03 top-1 or sel@70):** `{}`",
        report.gate_gs1.accuracy_gain_ok
    );
    let _ = writeln!(
        out,
        "- **ECE_15 <= 0.05:** `{}` (actual: `{:.4}`)",
        report.gate_gs1.ece_ok, report.test_metrics.ece_15.estimate
    );
    let _ = writeln!(
        out,
        "- **Conformal coverage >= 1 - alpha - 0.02:** `{}` (actual: `{:.4}`, target: `{:.4}`)",
        report.gate_gs1.coverage_ok,
        report.test_metrics.conformal_empirical_coverage.estimate,
        report.gate_gs1.coverage_target
    );
    let _ = writeln!(
        out,
        "- **Latency p95 <= SLO ({:.1} ms):** `{}` (actual: `{:.3} ms`)",
        report.gate_gs1.slo_p95_ms, report.gate_gs1.latency_ok, report.test_metrics.p95_latency_ms
    );
    let _ = writeln!(
        out,
        "- **Overall G-S1 verdict:** **{}**",
        if report.gate_gs1.overall_pass {
            "PASS"
        } else {
            "BASELINE_FALLBACK"
        }
    );
    out
}

fn fmt_ci(m: &MetricWithCi) -> String {
    format!(
        "{:.4} [{:.4}, {:.4}]",
        m.estimate, m.ci_low_95, m.ci_high_95
    )
}

fn append_split_table(out: &mut String, title: &str, m: &SplitEvaluation) {
    let _ = writeln!(out, "## {title}");
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "Evaluated `{}` examples (`{}` in-domain, `{}` OOD); latency p50 `{:.3} ms`, p95 `{:.3} ms`.",
        m.total_examples, m.in_domain_examples, m.ood_examples, m.p50_latency_ms, m.p95_latency_ms
    );
    let _ = writeln!(out);
    let _ = writeln!(out, "| Metric | Estimate [95% CI] |");
    let _ = writeln!(out, "|---|---|");
    let _ = writeln!(out, "| Top-1 accuracy | {} |", fmt_ci(&m.top1));
    let _ = writeln!(out, "| Top-3 recall | {} |", fmt_ci(&m.top3));
    let _ = writeln!(out, "| MRR | {} |", fmt_ci(&m.mrr));
    let _ = writeln!(out, "| ECE_15 | {} |", fmt_ci(&m.ece_15));
    let _ = writeln!(out, "| Brier score | {} |", fmt_ci(&m.brier));
    let _ = writeln!(out, "| Mean NLL | {} |", fmt_ci(&m.nll));
    let _ = writeln!(
        out,
        "| Selective acc @ 50% | {} |",
        fmt_ci(&m.selective_accuracy_50)
    );
    let _ = writeln!(
        out,
        "| Selective acc @ 70% | {} |",
        fmt_ci(&m.selective_accuracy_70)
    );
    let _ = writeln!(
        out,
        "| Selective acc @ 90% | {} |",
        fmt_ci(&m.selective_accuracy_90)
    );
    let _ = writeln!(
        out,
        "| Abstention AUROC | {} |",
        fmt_ci(&m.abstention_auroc)
    );
    let _ = writeln!(
        out,
        "| Conformal coverage | {} |",
        fmt_ci(&m.conformal_empirical_coverage)
    );
    let _ = writeln!(out, "| Mean set size | {} |", fmt_ci(&m.mean_set_size));
    let _ = writeln!(out, "| Ambiguity rate | {} |", fmt_ci(&m.ambiguity_rate));
    let _ = writeln!(
        out,
        "| OOD abstention precision | {} |",
        fmt_ci(&m.ood_abstention_precision)
    );
    let _ = writeln!(out);
}

fn append_reliability_table(out: &mut String, title: &str, bins: &[ReliabilityBin]) {
    let _ = writeln!(out, "## {title}");
    let _ = writeln!(out);
    let _ = writeln!(out, "| Bin | Range | Count | Avg Conf | Avg Acc |");
    let _ = writeln!(out, "|---|---|---|---|---|");
    for bin in bins {
        let _ = writeln!(
            out,
            "| {} | [{:.2}, {:.2}) | {} | {:.4} | {:.4} |",
            bin.bin_index, bin.low, bin.high, bin.count, bin.avg_confidence, bin.avg_accuracy
        );
    }
    let _ = writeln!(out);
}
