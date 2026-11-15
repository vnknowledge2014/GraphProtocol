//! Offline split-conformal calibration CLI (`gap calibrate --dataset --out`) and snapshot
//! publication integration (`RTE-062`, `RTE-064`..`RTE-068`, `WP-M5-04`).

use std::fs;
use std::io::Write;
use std::path::PathBuf;

use clap::Parser;
use gap_core::clock::Timestamp;
use gap_core::snapshot::CalibrationTable;
use gap_route::calibration_job::{
    CalibrationExample, CalibrationMode, CalibrationPipelineReport, CalibrationSplit,
    CalibrationVersionKey, ReliabilityBin, ScheduledRecalibrationJob, WinsorizedConfig,
    run_calibration_pipeline,
};
use gap_route::s1_adapter::OPTION_FORMAT_VERSION;
use gap_snapshot::{PublishError, SnapshotHandle};
use serde_json::{Value, json};

/// Exit code `0` for successful calibration CLI execution.
pub const EXIT_CALIBRATE_OK: u8 = 0;

/// Exit code `1` for runtime / I/O / snapshot publish errors.
pub const EXIT_CALIBRATE_RUNTIME_ERROR: u8 = 1;

/// Exit code `2` for CLI argument or dataset validation errors.
pub const EXIT_CALIBRATE_USAGE_ERROR: u8 = 2;

/// Offline temperature scaling and split-conformal calibration CLI (`gap calibrate ...`).
#[derive(Debug, Clone, Parser)]
#[command(
    name = "gap calibrate",
    about = "Fit temperature scaling and split-conformal LAC thresholds from held-out datasets"
)]
pub struct CalibrateCli {
    /// Path to JSON or JSONL dataset of `{split, logits, true_index, ...}` rows.
    #[arg(long, value_name = "PATH")]
    pub dataset: PathBuf,
    /// Output directory where `calibration_table.json`, `calibration_report.json`, and
    /// `calibration_report.md` are written.
    #[arg(long, value_name = "DIR")]
    pub out: PathBuf,
    /// Bound routing model identifier (`RTE-067`).
    #[arg(long, default_value = "s1:laya-candle:f32", value_name = "MODEL_ID")]
    pub router_model: String,
    /// Bound snapshot embedding model identifier (`RTE-067`).
    #[arg(long, value_name = "EMBED_ID")]
    pub embedding_model: Option<String>,
    /// Option prompt format version (`RTE-067`).
    #[arg(long, default_value_t = OPTION_FORMAT_VERSION, value_name = "VER")]
    pub option_format_version: u32,
    /// Optional symmetric logit clipping threshold (`TM-14` winsorized NLL safeguard).
    #[arg(long, value_name = "ABS_CAP")]
    pub winsor_clip: Option<f32>,
    /// Also stage and verify snapshot publication onto an initial snapshot handle (`RTE-068`).
    #[arg(long)]
    pub publish_snapshot: bool,
    /// Print structured JSON summary to `stdout`.
    #[arg(long)]
    pub json: bool,
}

/// Summary artifact metadata returned by [`run_calibrate_cli`].
#[derive(Debug, Clone, PartialEq)]
pub struct CalibrateArtifacts {
    /// Path to serialized `CalibrationTable` JSON.
    pub table_path: PathBuf,
    /// Path to serialized structured report JSON.
    pub report_json_path: PathBuf,
    /// Path to rendered Markdown report.
    pub report_md_path: PathBuf,
    /// Resulting snapshot [`CalibrationTable`].
    pub table: CalibrationTable,
    /// Full calibration report.
    pub report: CalibrationPipelineReport,
    /// Snapshot generation published when `--publish-snapshot` was requested.
    pub published_generation: Option<u64>,
}

/// Executes [`CalibrateCli`] over an explicit `argv` slice, writing user output to `out` and
/// diagnostics to `err`.
#[must_use]
pub fn run_calibrate_with_io<W1: Write, W2: Write>(
    args: &[String],
    out: &mut W1,
    err: &mut W2,
) -> u8 {
    let argv = std::iter::once("calibrate".to_owned()).chain(args.iter().cloned());
    let cli = match CalibrateCli::try_parse_from(argv) {
        Ok(parsed) => parsed,
        Err(parse_err) => {
            let code = if parse_err.use_stderr() {
                EXIT_CALIBRATE_USAGE_ERROR
            } else {
                EXIT_CALIBRATE_OK
            };
            let _ = write!(err, "{parse_err}");
            return code;
        }
    };

    match run_calibrate_cli(&cli) {
        Ok(artifacts) => {
            if cli.json {
                let payload = json!({
                    "calibration_id": artifacts.report.calibration_id,
                    "binding": artifacts.table.model_id,
                    "overall_ece_before": artifacts.report.overall_ece_before,
                    "overall_ece_after": artifacts.report.overall_ece_after,
                    "table_path": artifacts.table_path.display().to_string(),
                    "report_json_path": artifacts.report_json_path.display().to_string(),
                    "report_md_path": artifacts.report_md_path.display().to_string(),
                    "published_generation": artifacts.published_generation,
                });
                let _ = writeln!(out, "{payload}");
            } else {
                let _ = writeln!(
                    out,
                    "Calibrated {} (ECE {:.4} -> {:.4}); wrote {}",
                    artifacts.report.calibration_id,
                    artifacts.report.overall_ece_before,
                    artifacts.report.overall_ece_after,
                    artifacts.table_path.display()
                );
            }
            EXIT_CALIBRATE_OK
        }
        Err(msg) => {
            let _ = writeln!(err, "calibrate error: {msg}");
            if msg.starts_with("dataset") || msg.starts_with("calibration example") {
                EXIT_CALIBRATE_USAGE_ERROR
            } else {
                EXIT_CALIBRATE_RUNTIME_ERROR
            }
        }
    }
}

/// Runs the calibration CLI pipeline against `cli.dataset` and writes artifacts into `cli.out`.
///
/// # Errors
///
/// Returns a descriptive error string when reading/parsing `cli.dataset`, running calibration,
/// writing output files, or snapshot publication fails.
pub fn run_calibrate_cli(cli: &CalibrateCli) -> Result<CalibrateArtifacts, String> {
    let raw = fs::read_to_string(&cli.dataset)
        .map_err(|e| format!("failed to read dataset `{}`: {e}", cli.dataset.display()))?;
    let examples = parse_calibration_dataset(&raw)?;

    let version_key = CalibrationVersionKey {
        router_model_id: cli.router_model.clone(),
        embedding_model_id: cli.embedding_model.clone(),
        option_format_version: cli.option_format_version,
    };
    let winsor = WinsorizedConfig {
        clip_logit_abs: cli.winsor_clip,
    };

    let (_record, report) = run_calibration_pipeline(version_key, &examples, winsor)
        .map_err(|e| format!("dataset validation failed: {e}"))?;
    let table = report.table.clone();

    fs::create_dir_all(&cli.out)
        .map_err(|e| format!("failed to create output dir `{}`: {e}", cli.out.display()))?;

    let table_path = cli.out.join("calibration_table.json");
    let report_json_path = cli.out.join("calibration_report.json");
    let report_md_path = cli.out.join("calibration_report.md");

    let table_json = serde_json::to_string_pretty(&table)
        .map_err(|e| format!("failed to serialize calibration table: {e}"))?;
    fs::write(&table_path, table_json)
        .map_err(|e| format!("failed to write `{}`: {e}", table_path.display()))?;

    let report_json = serde_json::to_string_pretty(&report_to_json_value(&report))
        .map_err(|e| format!("failed to serialize calibration report: {e}"))?;
    fs::write(&report_json_path, report_json)
        .map_err(|e| format!("failed to write `{}`: {e}", report_json_path.display()))?;

    let report_md = report.render_markdown();
    fs::write(&report_md_path, report_md)
        .map_err(|e| format!("failed to write `{}`: {e}", report_md_path.display()))?;

    let published_generation = if cli.publish_snapshot {
        let handle = SnapshotHandle::new(gap_core::snapshot::Snapshot::empty(0));
        let gen_id =
            install_calibration_via_snapshot_publish(&handle, table.clone(), Timestamp::UNIX_EPOCH)
                .map_err(|e| format!("snapshot publish failed: {e}"))?;
        Some(gen_id)
    } else {
        None
    };

    Ok(CalibrateArtifacts {
        table_path,
        report_json_path,
        report_md_path,
        table,
        report,
        published_generation,
    })
}

/// Installs a newly calibrated [`CalibrationTable`] through the immutable snapshot publish path
/// (`RTE-068`, `ARC-005`), returning the newly activated generation number.
///
/// # Errors
///
/// Propagates [`PublishError`] if concurrent generation publication broke monotonicity or format
/// checks.
pub fn install_calibration_via_snapshot_publish(
    handle: &SnapshotHandle,
    table: CalibrationTable,
    built_at: Timestamp,
) -> Result<u64, PublishError> {
    let current = handle.pin();
    let next = ScheduledRecalibrationJob::stage_next_snapshot(&current, table, built_at);
    handle.publish(next)
}

/// Parses a JSON array, `{ "examples": [...] }` object, or JSONL stream into [`CalibrationExample`]s.
///
/// # Errors
///
/// Returns a descriptive `dataset ...` error on malformed syntax or missing required fields.
pub fn parse_calibration_dataset(raw: &str) -> Result<Vec<CalibrationExample>, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("dataset is empty".to_owned());
    }

    let rows: Vec<Value> = if trimmed.starts_with('[') || trimmed.starts_with('{') {
        match serde_json::from_str::<Value>(trimmed) {
            Ok(Value::Array(items)) => items,
            Ok(Value::Object(mut obj)) => {
                if let Some(Value::Array(items)) = obj.remove("examples") {
                    items
                } else {
                    vec![Value::Object(obj)]
                }
            }
            Ok(_) => return Err("dataset JSON root must be an array or object".to_owned()),
            Err(_) => parse_jsonl_rows(trimmed)?,
        }
    } else {
        parse_jsonl_rows(trimmed)?
    };

    let mut out = Vec::with_capacity(rows.len());
    for (idx, row) in rows.iter().enumerate() {
        out.push(parse_example_row(row, idx)?);
    }
    Ok(out)
}

fn parse_jsonl_rows(raw: &str) -> Result<Vec<Value>, String> {
    let mut rows = Vec::new();
    for (line_idx, line) in raw.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let val: Value = serde_json::from_str(trimmed)
            .map_err(|e| format!("dataset JSONL parse failure at line {}: {e}", line_idx + 1))?;
        rows.push(val);
    }
    if rows.is_empty() {
        return Err("dataset contains zero JSONL rows".to_owned());
    }
    Ok(rows)
}

fn parse_example_row(row: &Value, idx: usize) -> Result<CalibrationExample, String> {
    let split_str = row
        .get("split")
        .and_then(Value::as_str)
        .unwrap_or("calibration");
    let split = match split_str.to_ascii_lowercase().as_str() {
        "fit" | "train" => CalibrationSplit::Fit,
        "cal" | "calibration" | "val" => CalibrationSplit::Calibration,
        "test" | "eval" => CalibrationSplit::Test,
        other => {
            return Err(format!(
                "dataset row {idx} has invalid split `{other}`; expected fit|calibration|test"
            ));
        }
    };

    let logits_arr = row
        .get("logits")
        .and_then(Value::as_array)
        .ok_or_else(|| format!("dataset row {idx} missing `logits` array"))?;
    let mut logits = Vec::with_capacity(logits_arr.len());
    for (j, v) in logits_arr.iter().enumerate() {
        let n = v
            .as_f64()
            .ok_or_else(|| format!("dataset row {idx} logit[{j}] must be numeric"))?;
        logits.push(n as f32);
    }

    let true_index = row
        .get("true_index")
        .and_then(Value::as_u64)
        .ok_or_else(|| format!("dataset row {idx} missing non-negative `true_index`"))?
        as usize;

    let sink_index = row
        .get("sink_index")
        .and_then(Value::as_u64)
        .map(|n| n as usize);
    let has_mutating_option = row
        .get("has_mutating_option")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    Ok(CalibrationExample {
        split,
        logits,
        true_index,
        sink_index,
        has_mutating_option,
    })
}

fn mode_str(mode: CalibrationMode) -> &'static str {
    match mode {
        CalibrationMode::Conformal => "conformal",
        CalibrationMode::Fallback => "fallback",
        CalibrationMode::InvalidatedBaseline => "invalidated_baseline",
    }
}

fn bin_to_json(b: &ReliabilityBin) -> Value {
    json!({
        "bin": b.bin_index,
        "lower": b.lower,
        "upper": b.upper,
        "count": b.count,
        "mean_confidence": b.mean_confidence,
        "empirical_accuracy": b.empirical_accuracy,
    })
}

fn report_to_json_value(report: &CalibrationPipelineReport) -> Value {
    let buckets: Vec<Value> = report
        .buckets
        .iter()
        .map(|b| {
            json!({
                "bucket": b.bucket.as_str(),
                "fit_count": b.fit_count,
                "cal_count": b.cal_count,
                "test_count": b.test_count,
                "temperature": b.temperature,
                "fit_nll_before": b.fit_nll_before,
                "fit_nll_after": b.fit_nll_after,
                "mode_read_only": mode_str(b.mode_read_only),
                "q_hat_read_only": b.q_hat_read_only,
                "mode_mutating": mode_str(b.mode_mutating),
                "q_hat_mutating": b.q_hat_mutating,
                "ece_before": b.ece_before,
                "ece_after": b.ece_after,
                "empirical_coverage_read_only": b.empirical_coverage_read_only,
                "empirical_coverage_mutating": b.empirical_coverage_mutating,
                "mean_set_size_read_only": b.mean_set_size_read_only,
                "mean_set_size_mutating": b.mean_set_size_mutating,
                "abstention_rate_read_only": b.abstention_rate_read_only,
                "abstention_rate_mutating": b.abstention_rate_mutating,
                "policy_empirical_coverage": b.policy_empirical_coverage,
                "reliability_before": b.reliability_before.iter().map(bin_to_json).collect::<Vec<_>>(),
                "reliability_after": b.reliability_after.iter().map(bin_to_json).collect::<Vec<_>>(),
            })
        })
        .collect();

    json!({
        "calibration_id": report.calibration_id,
        "version_key": {
            "router_model_id": report.version_key.router_model_id,
            "embedding_model_id": report.version_key.embedding_model_id,
            "option_format_version": report.version_key.option_format_version,
        },
        "fit_total": report.fit_total,
        "cal_total": report.cal_total,
        "test_total": report.test_total,
        "overall_ece_before": report.overall_ece_before,
        "overall_ece_after": report.overall_ece_after,
        "table": report.table,
        "buckets": buckets,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use std::fmt::Write as _;

    use gap_route::calibration_job::{CalibrationVersionKey, is_snapshot_calibration_valid};
    use serde_json::Value;
    use tempfile::TempDir;

    use super::*;

    fn build_jsonl_dataset() -> String {
        let mut out = String::new();
        // 200 fit + 560 cal + 400 test in bucket 3-5 (4 options).
        for idx in 0..1_160usize {
            let split = if idx < 200 {
                "fit"
            } else if idx < 760 {
                "calibration"
            } else {
                "test"
            };
            let gold = idx % 4;
            let mut logits = [0.1_f32; 4];
            logits[gold] = 3.2;
            let _ = writeln!(
                out,
                "{{\"split\":\"{split}\",\"logits\":[{:.3},{:.3},{:.3},{:.3}],\"true_index\":{gold},\"sink_index\":3,\"has_mutating_option\":{}}}",
                logits[0],
                logits[1],
                logits[2],
                logits[3],
                idx % 2 == 0
            );
        }
        out
    }

    // spec: RTE-062, RTE-064, RTE-067, RTE-068
    #[test]
    fn calibrate_cli_writes_table_and_report_and_publishes_via_snapshot_handle() {
        let dir = TempDir::new().expect("temp dir");
        let ds_path = dir.path().join("calib.jsonl");
        let out_dir = dir.path().join("out");
        fs::write(&ds_path, build_jsonl_dataset()).expect("write dataset");

        let args = vec![
            "--dataset".to_owned(),
            ds_path.display().to_string(),
            "--out".to_owned(),
            out_dir.display().to_string(),
            "--router-model".to_owned(),
            "s1:laya-candle:f32:test".to_owned(),
            "--embedding-model".to_owned(),
            "hash-384".to_owned(),
            "--publish-snapshot".to_owned(),
            "--json".to_owned(),
        ];

        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let code = run_calibrate_with_io(&args, &mut stdout, &mut stderr);
        assert_eq!(
            code,
            EXIT_CALIBRATE_OK,
            "stderr={}",
            String::from_utf8_lossy(&stderr)
        );

        let table_raw = fs::read_to_string(out_dir.join("calibration_table.json")).unwrap();
        let table: CalibrationTable = serde_json::from_str(&table_raw).unwrap();
        let key = CalibrationVersionKey {
            router_model_id: "s1:laya-candle:f32:test".to_owned(),
            embedding_model_id: Some("hash-384".to_owned()),
            option_format_version: OPTION_FORMAT_VERSION,
        };
        assert!(is_snapshot_calibration_valid(&table, &key));
        assert_eq!(table.quantile_scores.len(), 12);

        let report_raw = fs::read_to_string(out_dir.join("calibration_report.json")).unwrap();
        let report_val: Value = serde_json::from_str(&report_raw).unwrap();
        assert_eq!(report_val["cal_total"].as_u64(), Some(560));
        assert!(out_dir.join("calibration_report.md").exists());

        let cli_out: Value = serde_json::from_slice(&stdout).unwrap();
        assert_eq!(cli_out["published_generation"].as_u64(), Some(1));

        // Direct SnapshotHandle publish helper test.
        let handle = SnapshotHandle::new(gap_core::snapshot::Snapshot::empty(2));
        let gen_id =
            install_calibration_via_snapshot_publish(&handle, table, Timestamp::UNIX_EPOCH)
                .expect("publish generation 3");
        assert_eq!(gen_id, 3);
        let pinned = handle.pin();
        assert!(pinned.calibration.is_some());
    }

    // spec: RTE-062, RTE-067
    #[test]
    fn calibrate_cli_rejects_malformed_or_missing_dataset_cleanly() {
        let dir = TempDir::new().expect("temp dir");
        let bad_path = dir.path().join("bad.json");
        let out_dir = dir.path().join("out");
        fs::write(
            &bad_path,
            r#"{"split":"fit","logits":[1.0],"true_index":5}"#,
        )
        .unwrap();

        let args = vec![
            "--dataset".to_owned(),
            bad_path.display().to_string(),
            "--out".to_owned(),
            out_dir.display().to_string(),
        ];
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let code = run_calibrate_with_io(&args, &mut stdout, &mut stderr);
        assert_eq!(code, EXIT_CALIBRATE_USAGE_ERROR);
        assert!(String::from_utf8_lossy(&stderr).contains("calibrate error"));
    }
}
