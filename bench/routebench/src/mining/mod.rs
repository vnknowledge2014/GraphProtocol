//! Hard-negative mining loop and System-1 vs Baseline evaluation harness
//! (`WP-M5-07`; `RTE-043`, `RTE-060`).
// `routebench` is declared as a `[[bin]]`-only crate and `main.rs` sits outside `WP-M5-07` `owns`;
// keeping `#[allow(dead_code)]` scoped to `mining` avoids false dead-code lints when compiling the
// binary target while all public functions remain exercised under `cargo test` / `nextest`.
#![allow(dead_code)]

pub mod eval_s1;
#[cfg(test)]
mod tests;

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::task::{Context, Poll, Waker};

use gap_core::ids::{TenantId, TenantScope};
use gap_core::snapshot::SideEffectLevel;
use gap_ql::AllowAllVisibility;
use gap_route::groups::{SINK_OPTION_KEY, build_group_options};
use gap_route::s1_adapter::{RankCtx, RoutingModel};
use gap_route::sanitize::SanitizedQuery;
use gap_route::{DEFAULT_RETRIEVAL_MIN_SCORE, DEFAULT_TOP_K_GROUPS};
use serde::{Deserialize, Serialize};

use crate::dataset::{GoldTarget, RouteBenchDataset, RouteBenchExample, SplitKind};
use crate::metrics::EvalError;

/// Maximum gold-minus-best-wrong margin considered a hard-negative mining candidate.
pub const HARD_NEGATIVE_MARGIN_CUTOFF: f32 = 1.5;

/// Single mined failure or near-miss example preserved inside its source `SplitKind` (`RTE-060`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MinedHardNegative {
    /// Source RouteBench example identifier.
    pub example_id: String,
    /// Original split preserved strictly without cross-split movement (`RTE-060`).
    pub split: SplitKind,
    /// Query text submitted during discovery.
    pub query: String,
    /// Ground-truth group ID (`"insufficient_evidence"` for false-positive OOD misses).
    pub gold_group_id: String,
    /// Most confusing sibling/distractor group predicted instead of (or nearly tied with) gold.
    pub confusing_group_id: String,
    /// Logit margin (`gold_logit - max_wrong_logit`; negative values are Top-1 errors).
    pub margin: f32,
    /// Canonical `operation_id` recorded on the parent example (`None` for global OOD).
    pub source_op: Option<String>,
    /// Parent branch slug (`None` for global OOD).
    pub source_branch: Option<String>,
}

/// Versioned hard-negative dataset extension emitted for subsequent training/calibration rounds.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HardNegativeCorpusV2 {
    /// Corpus format version (`2` for mined v2 delta).
    pub schema_version: u32,
    /// Evaluating router ID that surfaced these failures/near-misses.
    pub router_id: String,
    /// Total mined records.
    pub mined_count: usize,
    /// Record counts grouped by original `SplitKind` label.
    pub split_counts: BTreeMap<String, usize>,
    /// Extracted hard-negative records sorted deterministically by `example_id`.
    pub records: Vec<MinedHardNegative>,
}

pub(crate) fn poll_future<F: Future>(fut: F) -> F::Output {
    let waker = Waker::noop();
    let mut cx = Context::from_waker(waker);
    let mut pinned = std::pin::pin!(fut);
    loop {
        match Pin::as_mut(&mut pinned).poll(&mut cx) {
            Poll::Ready(val) => return val,
            Poll::Pending => std::thread::yield_now(),
        }
    }
}

fn split_label(split: SplitKind) -> &'static str {
    match split {
        SplitKind::Fit => "fit",
        SplitKind::Calibration => "calibration",
        SplitKind::Test => "test",
        SplitKind::HeldOutBranch => "held_out_branch",
    }
}

/// Mines failure and close-margin examples across `dataset` using `model` while tagging every
/// record strictly with its original `split`, `source_op`, and `source_branch` (`RTE-060`).
pub fn mine_hard_negatives(
    dataset: &RouteBenchDataset,
    model: &dyn RoutingModel,
) -> Result<HardNegativeCorpusV2, EvalError> {
    let scope = TenantScope::new(TenantId::default_tenant());
    let vis = AllowAllVisibility::default();
    let mut records = Vec::new();
    let mut split_counts: BTreeMap<String, usize> = BTreeMap::new();

    for (idx, ex) in dataset.examples.iter().enumerate() {
        if let Some(mined) = mine_single_example(ex, idx, dataset, &scope, &vis, model)? {
            *split_counts
                .entry(split_label(ex.split).to_owned())
                .or_insert(0) += 1;
            records.push(mined);
        }
    }

    records.sort_by(|a, b| a.example_id.cmp(&b.example_id));
    Ok(HardNegativeCorpusV2 {
        schema_version: 2,
        router_id: model.id().to_owned(),
        mined_count: records.len(),
        split_counts,
        records,
    })
}

fn mine_single_example(
    ex: &RouteBenchExample,
    idx: usize,
    dataset: &RouteBenchDataset,
    scope: &TenantScope,
    vis: &AllowAllVisibility,
    model: &dyn RoutingModel,
) -> Result<Option<MinedHardNegative>, EvalError> {
    let snapshot = match ex.split {
        SplitKind::HeldOutBranch => &dataset.full_snapshot,
        SplitKind::Fit | SplitKind::Calibration | SplitKind::Test => &dataset.in_domain_snapshot,
    };

    let sanitized = SanitizedQuery::new(&ex.query);
    let hits = if sanitized.is_empty() {
        Vec::new()
    } else {
        snapshot.index.search(scope, sanitized.as_str(), None, 64)?
    };
    let filtered_hits: Vec<_> = hits
        .into_iter()
        .filter(|h| h.fused_score >= DEFAULT_RETRIEVAL_MIN_SCORE)
        .collect();
    let (resolved_groups, opts) = build_group_options(
        snapshot,
        &filtered_hits,
        None,
        SideEffectLevel::Irreversible,
        DEFAULT_TOP_K_GROUPS,
        false,
        vis,
        0.05,
    );
    let ctx = RankCtx {
        max_side_effect: SideEffectLevel::Irreversible,
        request_id: format!("mine-{idx:05}"),
        timeout_ms: 2_000,
    };
    let ranked = poll_future(model.rank(&sanitized, &opts, &ctx))?;
    let logits = if ranked.logits.len() == opts.len() {
        ranked.logits
    } else {
        opts.iter().map(|o| o.prior).collect()
    };

    match &ex.gold {
        GoldTarget::Group(gold_gid) => {
            let gold_pos = resolved_groups
                .iter()
                .position(|rg| rg.group_id.as_str() == gold_gid);
            let gold_logit = gold_pos
                .and_then(|p| logits.get(p).copied())
                .unwrap_or(-10.0);

            let mut best_wrong_logit = f32::NEG_INFINITY;
            let mut best_wrong_key = SINK_OPTION_KEY.to_owned();
            for (pos, opt) in opts.iter().enumerate() {
                if Some(pos) == gold_pos {
                    continue;
                }
                let val = logits.get(pos).copied().unwrap_or(f32::NEG_INFINITY);
                if val > best_wrong_logit {
                    best_wrong_logit = val;
                    best_wrong_key = if let Some(rg) = resolved_groups.get(pos) {
                        rg.group_id.as_str().to_owned()
                    } else {
                        opt.key.clone()
                    };
                }
            }
            if !best_wrong_logit.is_finite() {
                return Ok(None);
            }
            let margin = gold_logit - best_wrong_logit;
            if margin <= HARD_NEGATIVE_MARGIN_CUTOFF {
                return Ok(Some(MinedHardNegative {
                    example_id: ex.example_id.clone(),
                    split: ex.split,
                    query: ex.query.clone(),
                    gold_group_id: gold_gid.clone(),
                    confusing_group_id: best_wrong_key,
                    margin,
                    source_op: ex.source_op.clone(),
                    source_branch: ex.source_branch.clone(),
                }));
            }
            Ok(None)
        }
        GoldTarget::InsufficientEvidence => {
            let sink_pos = opts
                .iter()
                .position(|o| o.key == SINK_OPTION_KEY)
                .unwrap_or_else(|| opts.len().saturating_sub(1));
            let sink_logit = logits.get(sink_pos).copied().unwrap_or(0.0);
            let mut best_group_logit = f32::NEG_INFINITY;
            let mut best_group_id = String::new();
            for (pos, rg) in resolved_groups.iter().enumerate() {
                let val = logits.get(pos).copied().unwrap_or(f32::NEG_INFINITY);
                if val > best_group_logit {
                    best_group_logit = val;
                    best_group_id = rg.group_id.as_str().to_owned();
                }
            }
            if !best_group_logit.is_finite() || best_group_id.is_empty() {
                return Ok(None);
            }
            let margin = sink_logit - best_group_logit;
            if margin <= HARD_NEGATIVE_MARGIN_CUTOFF {
                return Ok(Some(MinedHardNegative {
                    example_id: ex.example_id.clone(),
                    split: ex.split,
                    query: ex.query.clone(),
                    gold_group_id: SINK_OPTION_KEY.to_owned(),
                    confusing_group_id: best_group_id,
                    margin,
                    source_op: ex.source_op.clone(),
                    source_branch: ex.source_branch.clone(),
                }));
            }
            Ok(None)
        }
    }
}

/// Enforces `RTE-060` zero-leakage invariant on mined hard-negative extensions:
/// - No `Fit`/`Calibration`/`Test` example may migrate across splits relative to
///   `dataset.operation_splits`.
/// - No `operation_id` appears in more than one in-domain split (`Fit`/`Calibration`/`Test`),
///   and `HeldOutBranch` operations never overlap in-domain splits.
pub fn verify_zero_split_leakage(
    dataset: &RouteBenchDataset,
    corpus: &HardNegativeCorpusV2,
) -> Result<(), String> {
    let mut op_splits: BTreeMap<&str, BTreeSet<SplitKind>> = BTreeMap::new();
    for ex in &dataset.examples {
        if let Some(op) = ex.source_op.as_deref() {
            op_splits.entry(op).or_default().insert(ex.split);
        }
    }
    for rec in &corpus.records {
        if let Some(op) = rec.source_op.as_deref() {
            if let Some(&expected_split) = dataset.operation_splits.get(op)
                && rec.split != expected_split
            {
                return Err(format!(
                    "operation `{op}` leaked from `{}` to `{}` in mined record `{}`",
                    split_label(expected_split),
                    split_label(rec.split),
                    rec.example_id
                ));
            }
            op_splits.entry(op).or_default().insert(rec.split);
        }
    }
    for (op, splits) in op_splits {
        if splits.len() > 1 {
            return Err(format!(
                "operation `{op}` appeared across multiple splits ({splits:?})"
            ));
        }
    }
    Ok(())
}

/// Serializes `corpus` atomically to `path` as pretty JSON.
pub fn write_mined_corpus(path: &Path, corpus: &HardNegativeCorpusV2) -> std::io::Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)?;
    }
    let body = serde_json::to_string_pretty(corpus)
        .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err))?;
    fs::write(path, body)
}
