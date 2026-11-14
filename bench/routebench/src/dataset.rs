//! RouteBench v1 dataset loader, paraphrase/hard-negative synthesizer, and zero-leakage
//! splitter (`RTE-060`, `ADR-007`).

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use gap_core::ids::{BranchId, BranchUid, GroupId, OperationId, TenantId};
use gap_core::snapshot::{
    BranchDescriptor, BranchLimits, BranchStatus, Capabilities, CostHint, Endpoint, Example,
    OpKind, OperationDescriptor, Protocol, SideEffect, SideEffectLevel, Snapshot, TrustTier,
};
use gap_index::HybridIndexConfig;
use gap_snapshot::groups::{AssembledGroups, GroupAssignmentConfig, assign_capability_groups};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

/// Dataset construction and I/O failures (`WP-M2-06`).
#[derive(Debug, Error)]
pub enum DatasetError {
    /// Reading a spec or cache file failed.
    #[error("dataset I/O error at `{path}`: {source}")]
    Io {
        /// File or directory path involved.
        path: String,
        /// Underlying OS error.
        #[source]
        source: std::io::Error,
    },
    /// Parsing a JSON specification or cached synthesis payload failed.
    #[error("dataset JSON error in `{path}`: {source}")]
    Json {
        /// File path or cache key involved.
        path: String,
        /// Underlying JSON error.
        #[source]
        source: serde_json::Error,
    },
    /// Specification violated required schema or GAP identifier rules.
    #[error("invalid dataset specification `{path}`: {reason}")]
    InvalidSpec {
        /// Path or dataset identifier.
        path: String,
        /// Detailed validation reason.
        reason: String,
    },
    /// Core snapshot/group error.
    #[error("snapshot/group assembly error: {0}")]
    Assembly(String),
}

/// Evaluation split following `RTE-060` zero-leakage partition rules.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SplitKind {
    /// 60% of in-domain operations (temperature scaling / threshold fitting).
    Fit,
    /// 20% of in-domain operations (split-conformal quantile calibration).
    Calibration,
    /// 20% of in-domain operations (held-out test evaluation).
    Test,
    /// ~10% of whole branches held out for cold-start evaluation.
    HeldOutBranch,
}

/// Origin category of a RouteBench query (`RTE-060`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceKind {
    /// Positive `OperationDescriptor.examples` intent.
    PositiveExample,
    /// Hard negative derived from a sibling operation or negative example.
    HardNegativeSibling,
    /// Deterministically synthesized or cached paraphrase of an operation.
    Paraphrase,
    /// Out-of-domain or adversarial near-miss expecting `insufficient_evidence`.
    OodNegative,
}

/// Ground-truth target for a RouteBench example (`gold_group | insufficient_evidence`).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", content = "group_id", rename_all = "snake_case")]
pub enum GoldTarget {
    /// Expected capability group ID.
    Group(String),
    /// Out-of-domain or unanswerable intent expecting abstention.
    InsufficientEvidence,
}

/// Single labeled RouteBench example (`RTE-060`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RouteBenchExample {
    /// Stable deterministic example identifier.
    pub example_id: String,
    /// Zero-leakage partition assignment.
    pub split: SplitKind,
    /// Query text submitted to discovery.
    pub query: String,
    /// Originating branch slug (`None` for global OOD negatives).
    pub source_branch: Option<String>,
    /// Originating operation ID (`None` for global OOD negatives).
    pub source_op: Option<String>,
    /// Expected target (`Group(gid)` or `InsufficientEvidence`).
    pub gold: GoldTarget,
    /// Effective side-effect level associated with `gold` (`NONE` for OOD).
    pub gold_side_effect: SideEffectLevel,
    /// Provenance category.
    pub source_kind: SourceKind,
}

/// Configuration knobs for RouteBench dataset generation.
#[derive(Debug, Clone)]
pub struct DatasetConfig {
    /// Deterministic PRNG seed (`RTE-060`).
    pub seed: u64,
    /// Optional disk directory caching paraphrase/hard-negative synthesis outputs.
    pub cache_dir: Option<PathBuf>,
    /// Paraphrases synthesized per in-domain operation.
    pub paraphrases_per_op: usize,
    /// Hard-negative sibling prompts synthesized per in-domain operation.
    pub hard_negatives_per_op: usize,
    /// Number of OOD negatives generated across `Fit`/`Calibration`/`Test`.
    pub ood_negative_count: usize,
    /// Smoke mode cap keeping dataset compact (< 2s execution).
    pub smoke: bool,
}

impl Default for DatasetConfig {
    fn default() -> Self {
        Self {
            seed: 20_261_115,
            cache_dir: None,
            paraphrases_per_op: 4,
            hard_negatives_per_op: 2,
            ood_negative_count: 30,
            smoke: false,
        }
    }
}

/// Pluggable paraphrase and hard-negative synthesizer (`WP-M2-06`).
///
/// Implementations may call an external LLM offline or use deterministic seed templates; caching
/// ensures repeated runs with the same `(op_id, desc_hash, seed)` reuse cached outputs cleanly.
pub trait ParaphraseGenerator: Send + Sync {
    /// Generates up to `count` positive paraphrases for `op`.
    fn generate_paraphrases(
        &self,
        op: &OperationDescriptor,
        branch: &BranchDescriptor,
        count: usize,
        seed: u64,
    ) -> Result<Vec<String>, DatasetError>;

    /// Generates up to `count` sibling hard-negative queries where another sibling `OperationId` in
    /// `siblings` is the true target while reusing distracting domain terms.
    fn generate_hard_negatives(
        &self,
        op: &OperationDescriptor,
        _branch: &BranchDescriptor,
        siblings: &[(&OperationDescriptor, &BranchDescriptor)],
        count: usize,
        seed: u64,
    ) -> Result<Vec<(String, OperationId)>, DatasetError>;
}

/// Simple deterministic `SplitMix64` PRNG helper (identical to `gap-route` tests).
#[derive(Debug, Clone, Copy)]
pub struct SplitMix64(pub u64);

impl SplitMix64 {
    /// Advances PRNG state and returns a pseudo-random `u64`.
    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    /// Returns a uniform index in `0..bound` (assumes `bound > 0`).
    #[must_use]
    pub fn next_usize(&mut self, bound: usize) -> usize {
        if bound == 0 {
            return 0;
        }
        (self.next_u64() as usize) % bound
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CachedSynthesis {
    paraphrases: Vec<String>,
    hard_negatives: Vec<(String, String)>,
}

/// Deterministic seeded paraphrase & hard-negative generator with optional on-disk JSON cache.
#[derive(Debug, Clone, Default)]
pub struct DeterministicSeededGenerator {
    /// Optional directory where `(op_id, desc_hash, seed)` responses are cached.
    pub cache_dir: Option<PathBuf>,
}

impl DeterministicSeededGenerator {
    /// Creates a generator with an optional cache directory.
    #[must_use]
    pub fn new(cache_dir: Option<PathBuf>) -> Self {
        Self { cache_dir }
    }

    fn cache_path(
        &self,
        op: &OperationDescriptor,
        seed: u64,
        tag: &str,
        count: usize,
    ) -> Option<PathBuf> {
        let dir = self.cache_dir.as_ref()?;
        let desc_hex = hex_encode(&op.desc_hash);
        let key_input = format!("{}:{desc_hex}:{seed}:{tag}:{count}", op.id.as_str());
        let key = blake3::hash(key_input.as_bytes()).to_hex();
        Some(dir.join(format!("{}.json", &key.as_str()[..24])))
    }

    fn read_cache(&self, path: &Path) -> Result<Option<CachedSynthesis>, DatasetError> {
        if !path.exists() {
            return Ok(None);
        }
        let raw = fs::read_to_string(path).map_err(|source| DatasetError::Io {
            path: path.display().to_string(),
            source,
        })?;
        let parsed: CachedSynthesis =
            serde_json::from_str(&raw).map_err(|source| DatasetError::Json {
                path: path.display().to_string(),
                source,
            })?;
        Ok(Some(parsed))
    }

    fn write_cache(&self, path: &Path, entry: &CachedSynthesis) -> Result<(), DatasetError> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|source| DatasetError::Io {
                path: parent.display().to_string(),
                source,
            })?;
        }
        let payload = serde_json::to_string_pretty(entry).map_err(|source| DatasetError::Json {
            path: path.display().to_string(),
            source,
        })?;
        fs::write(path, payload).map_err(|source| DatasetError::Io {
            path: path.display().to_string(),
            source,
        })?;
        Ok(())
    }
}

impl ParaphraseGenerator for DeterministicSeededGenerator {
    fn generate_paraphrases(
        &self,
        op: &OperationDescriptor,
        branch: &BranchDescriptor,
        count: usize,
        seed: u64,
    ) -> Result<Vec<String>, DatasetError> {
        if count == 0 {
            return Ok(Vec::new());
        }
        if let Some(path) = self.cache_path(op, seed, "para", count)
            && let Some(cached) = self.read_cache(&path)?
        {
            return Ok(cached.paraphrases);
        }

        let words = humanize_camel_or_snake(op.id.name());
        let domain_hint = humanize_camel_or_snake(branch.id.as_str());
        let summary = first_positive_or_summary(op);
        let arg_words = extract_arg_names(&op.sdl);

        let mut pool = vec![
            format!("{words} for {domain_hint}"),
            format!("need to {words} in {domain_hint}: {summary}"),
            summary.clone(),
        ];
        if !arg_words.is_empty() {
            pool.push(format!("{words} using {}", arg_words.join(" and ")));
        }
        for ex in op.examples.iter().filter(|e| e.positive) {
            pool.push(format!("please {} ({})", ex.intent, words));
        }

        let mut rng = SplitMix64(hash_u64(seed, op.id.as_str()));
        let mut selected = Vec::with_capacity(count);
        for idx in 0..count {
            let pick = if idx < pool.len() {
                idx
            } else {
                rng.next_usize(pool.len())
            };
            let suffix = if idx >= pool.len() {
                format!(" [v{}]", rng.next_u64() % 97)
            } else {
                String::new()
            };
            selected.push(format!("{}{suffix}", pool[pick]));
        }

        if let Some(path) = self.cache_path(op, seed, "para", count) {
            let entry = CachedSynthesis {
                paraphrases: selected.clone(),
                hard_negatives: Vec::new(),
            };
            self.write_cache(&path, &entry)?;
        }
        Ok(selected)
    }

    fn generate_hard_negatives(
        &self,
        op: &OperationDescriptor,
        _branch: &BranchDescriptor,
        siblings: &[(&OperationDescriptor, &BranchDescriptor)],
        count: usize,
        seed: u64,
    ) -> Result<Vec<(String, OperationId)>, DatasetError> {
        if count == 0 {
            return Ok(Vec::new());
        }
        let mut eligible: Vec<(&OperationDescriptor, &BranchDescriptor)> = siblings
            .iter()
            .copied()
            .filter(|(sib, _)| sib.id != op.id)
            .collect();
        if eligible.is_empty() {
            return Ok(Vec::new());
        }

        if let Some(path) = self.cache_path(op, seed, "hardneg", count)
            && let Some(cached) = self.read_cache(&path)?
        {
            let mut restored = Vec::with_capacity(cached.hard_negatives.len());
            for (q, id_str) in cached.hard_negatives {
                if let Some((sib, _)) = eligible.iter().find(|(s, _)| s.id.as_str() == id_str) {
                    restored.push((q, sib.id.clone()));
                }
            }
            if !restored.is_empty() {
                return Ok(restored);
            }
        }

        eligible.sort_by(|a, b| a.0.id.as_str().cmp(b.0.id.as_str()));
        let mut rng = SplitMix64(hash_u64(seed ^ 0x51a9_7f13, op.id.as_str()));
        let distractor = humanize_camel_or_snake(op.id.name());
        let mut out = Vec::with_capacity(count);
        for idx in 0..count {
            let pick = (idx + rng.next_usize(eligible.len())) % eligible.len();
            let (target_op, target_branch) = eligible[pick];
            let target_words = humanize_camel_or_snake(target_op.id.name());
            let target_summary = first_positive_or_summary(target_op);
            let query = format!(
                "{target_words} ({}; not {distractor} in {})",
                target_summary,
                humanize_camel_or_snake(target_branch.id.as_str())
            );
            out.push((query, target_op.id.clone()));
        }

        if let Some(path) = self.cache_path(op, seed, "hardneg", count) {
            let entry = CachedSynthesis {
                paraphrases: Vec::new(),
                hard_negatives: out
                    .iter()
                    .map(|(q, id)| (q.clone(), id.as_str().to_owned()))
                    .collect(),
            };
            self.write_cache(&path, &entry)?;
        }
        Ok(out)
    }
}

/// Loaded and partitioned RouteBench v1 benchmark corpus (`RTE-060`).
#[derive(Clone)]
pub struct RouteBenchDataset {
    /// All loaded branch descriptors (ordered by `branch.id`).
    pub all_branches: Vec<Arc<BranchDescriptor>>,
    /// Branch UIDs assigned to the in-domain corpus (~90%).
    pub in_domain_branch_uids: BTreeSet<BranchUid>,
    /// Branch UIDs held out strictly for cold-start testing (~10%).
    pub held_out_branch_uids: BTreeSet<BranchUid>,
    /// Mapping from canonical `OperationId` string to its `SplitKind`.
    pub operation_splits: BTreeMap<String, SplitKind>,
    /// Mapping from `OperationId` string to assembled `GroupId`.
    pub op_to_group: BTreeMap<String, GroupId>,
    /// Compiled snapshot containing **all** branches (for full / held-out cold-start evaluation).
    pub full_snapshot: Arc<Snapshot>,
    /// Compiled snapshot containing **in-domain** branches only (`Fit`/`Calibration`/`Test`).
    pub in_domain_snapshot: Arc<Snapshot>,
    /// Partitioned benchmark examples across all four splits.
    pub examples: Vec<RouteBenchExample>,
}

impl RouteBenchDataset {
    /// Returns an iterator of examples belonging to `split`.
    pub fn examples_for(&self, split: SplitKind) -> impl Iterator<Item = &RouteBenchExample> {
        self.examples.iter().filter(move |e| e.split == split)
    }
}

/// Builds the full RouteBench v1 dataset from `data_dir` (`RTE-060`).
pub fn build_dataset(
    data_dir: &Path,
    config: &DatasetConfig,
    generator: &dyn ParaphraseGenerator,
) -> Result<RouteBenchDataset, DatasetError> {
    let mut all_branches = load_branches_from_dir(data_dir)?;
    if all_branches.is_empty() {
        return Err(DatasetError::InvalidSpec {
            path: data_dir.display().to_string(),
            reason: "no branch specifications found".to_owned(),
        });
    }
    all_branches.sort_by(|a, b| a.id.as_str().cmp(b.id.as_str()));

    // Assemble groups deterministically across all branches (`ADR-006`).
    let assembled = assign_capability_groups(&all_branches, &GroupAssignmentConfig::new())
        .map_err(|e| DatasetError::Assembly(e.to_string()))?;
    let mut op_to_group_str: BTreeMap<String, GroupId> = BTreeMap::new();
    for (op_id, gid) in &assembled.op_to_group {
        op_to_group_str.insert(op_id.as_str().to_owned(), gid.clone());
    }

    // Hold out 10% of whole branches (at least 1 branch when >= 4 branches exist) for cold start.
    let held_out_count = if all_branches.len() >= 4 {
        ((all_branches.len() as f64) * 0.10).ceil().max(1.0) as usize
    } else {
        0
    };
    let mut ranked_branches: Vec<(u64, &Arc<BranchDescriptor>)> = all_branches
        .iter()
        .map(|b| (hash_u64(config.seed, b.id.as_str()), b))
        .collect();
    ranked_branches.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.id.cmp(&b.1.id)));

    let mut held_out_uids = BTreeSet::new();
    let mut in_domain_uids = BTreeSet::new();
    for (idx, (_, br)) in ranked_branches.into_iter().enumerate() {
        if idx < held_out_count {
            held_out_uids.insert(br.uid);
        } else {
            in_domain_uids.insert(br.uid);
        }
    }

    // Partition all in-domain operations by hashed OperationId (60% fit, 20% cal, 20% test).
    let mut in_domain_ops: Vec<(u64, &Arc<BranchDescriptor>, &Arc<OperationDescriptor>)> =
        Vec::new();
    for br in &all_branches {
        if held_out_uids.contains(&br.uid) {
            continue;
        }
        for op in &br.operations {
            in_domain_ops.push((hash_u64(config.seed ^ 0x99ab, op.id.as_str()), br, op));
        }
    }
    in_domain_ops.sort_by(|a, b| {
        a.0.cmp(&b.0)
            .then_with(|| a.2.id.as_str().cmp(b.2.id.as_str()))
    });

    let mut operation_splits = BTreeMap::new();
    let n_ops = in_domain_ops.len();
    let (fit_end, cal_end) = partition_indices_60_20_20(n_ops);
    for (idx, (_, _, op)) in in_domain_ops.iter().enumerate() {
        let split = if idx < fit_end {
            SplitKind::Fit
        } else if idx < cal_end {
            SplitKind::Calibration
        } else {
            SplitKind::Test
        };
        operation_splits.insert(op.id.as_str().to_owned(), split);
    }

    // Compile both full and in-domain snapshots with group-level HybridElementIndex.
    let full_snapshot = Arc::new(build_snapshot_from_groups(&all_branches, &assembled)?);
    let in_domain_branches: Vec<Arc<BranchDescriptor>> = all_branches
        .iter()
        .filter(|b| in_domain_uids.contains(&b.uid))
        .cloned()
        .collect();
    let in_domain_assembled =
        assign_capability_groups(&in_domain_branches, &GroupAssignmentConfig::new())
            .map_err(|e| DatasetError::Assembly(e.to_string()))?;
    let in_domain_snapshot = Arc::new(build_snapshot_from_groups(
        &in_domain_branches,
        &in_domain_assembled,
    )?);

    // Index all operations by split for safe sibling hard-negative generation (no split leakage!).
    let mut ops_by_split: BTreeMap<SplitKind, Vec<(&OperationDescriptor, &BranchDescriptor)>> =
        BTreeMap::new();
    for br in &all_branches {
        for op in &br.operations {
            let split = if held_out_uids.contains(&br.uid) {
                SplitKind::HeldOutBranch
            } else {
                operation_splits
                    .get(op.id.as_str())
                    .copied()
                    .unwrap_or(SplitKind::Test)
            };
            ops_by_split
                .entry(split)
                .or_default()
                .push((op.as_ref(), br.as_ref()));
        }
    }

    let para_cap = if config.smoke {
        config.paraphrases_per_op.min(2)
    } else {
        config.paraphrases_per_op
    };
    let neg_cap = if config.smoke {
        config.hard_negatives_per_op.min(1)
    } else {
        config.hard_negatives_per_op
    };

    let mut examples = Vec::new();
    let mut seq = 0usize;

    for br in &all_branches {
        for op in &br.operations {
            let split = if held_out_uids.contains(&br.uid) {
                SplitKind::HeldOutBranch
            } else {
                operation_splits
                    .get(op.id.as_str())
                    .copied()
                    .unwrap_or(SplitKind::Test)
            };
            let gid = op_to_group_str
                .get(op.id.as_str())
                .ok_or_else(|| DatasetError::Assembly(format!("missing GroupId for {}", op.id)))?;

            // Positive examples from OperationDescriptor.
            let pos_exs: Vec<&Example> = op.examples.iter().filter(|e| e.positive).collect();
            if pos_exs.is_empty() {
                seq += 1;
                examples.push(RouteBenchExample {
                    example_id: format!("ex-{seq:05}"),
                    split,
                    query: first_positive_or_summary(op),
                    source_branch: Some(br.id.as_str().to_owned()),
                    source_op: Some(op.id.as_str().to_owned()),
                    gold: GoldTarget::Group(gid.as_str().to_owned()),
                    gold_side_effect: op.side_effect.level,
                    source_kind: SourceKind::PositiveExample,
                });
            } else {
                for ex in pos_exs {
                    seq += 1;
                    examples.push(RouteBenchExample {
                        example_id: format!("ex-{seq:05}"),
                        split,
                        query: ex.intent.clone(),
                        source_branch: Some(br.id.as_str().to_owned()),
                        source_op: Some(op.id.as_str().to_owned()),
                        gold: GoldTarget::Group(gid.as_str().to_owned()),
                        gold_side_effect: op.side_effect.level,
                        source_kind: SourceKind::PositiveExample,
                    });
                }
            }

            // Synthesized / cached paraphrases (always tied to `op`'s own split -> zero leakage).
            let paraphrases = generator.generate_paraphrases(op, br, para_cap, config.seed)?;
            for pq in paraphrases {
                seq += 1;
                examples.push(RouteBenchExample {
                    example_id: format!("ex-{seq:05}"),
                    split,
                    query: pq,
                    source_branch: Some(br.id.as_str().to_owned()),
                    source_op: Some(op.id.as_str().to_owned()),
                    gold: GoldTarget::Group(gid.as_str().to_owned()),
                    gold_side_effect: op.side_effect.level,
                    source_kind: SourceKind::Paraphrase,
                });
            }

            // Hard-negative sibling queries using ONLY sibling ops from the SAME split (`RTE-060`).
            let same_split_ops = ops_by_split.get(&split).map(Vec::as_slice).unwrap_or(&[]);
            let hard_negs =
                generator.generate_hard_negatives(op, br, same_split_ops, neg_cap, config.seed)?;
            for (nq, target_op_id) in hard_negs {
                if let Some(target_gid) = op_to_group_str.get(target_op_id.as_str()) {
                    let target_se = same_split_ops
                        .iter()
                        .find(|(o, _)| o.id == target_op_id)
                        .map_or(SideEffectLevel::None, |(o, _)| o.side_effect.level);
                    seq += 1;
                    examples.push(RouteBenchExample {
                        example_id: format!("ex-{seq:05}"),
                        split,
                        query: nq,
                        source_branch: Some(br.id.as_str().to_owned()),
                        source_op: Some(target_op_id.as_str().to_owned()),
                        gold: GoldTarget::Group(target_gid.as_str().to_owned()),
                        gold_side_effect: target_se,
                        source_kind: SourceKind::HardNegativeSibling,
                    });
                }
            }
        }
    }

    // Add out-of-domain & adversarial near-miss negatives partitioned across Fit / Cal / Test.
    let ood_total = if config.smoke {
        config.ood_negative_count.min(18)
    } else {
        config.ood_negative_count
    };
    let ood_queries = build_ood_negative_corpus(ood_total);
    for (idx, q) in ood_queries.into_iter().enumerate() {
        let split = match idx % 5 {
            0..=2 => SplitKind::Fit,
            3 => SplitKind::Calibration,
            _ => SplitKind::Test,
        };
        seq += 1;
        examples.push(RouteBenchExample {
            example_id: format!("ood-{seq:05}"),
            split,
            query: q,
            source_branch: None,
            source_op: None,
            gold: GoldTarget::InsufficientEvidence,
            gold_side_effect: SideEffectLevel::None,
            source_kind: SourceKind::OodNegative,
        });
    }

    Ok(RouteBenchDataset {
        all_branches,
        in_domain_branch_uids: in_domain_uids,
        held_out_branch_uids: held_out_uids,
        operation_splits,
        op_to_group: op_to_group_str,
        full_snapshot,
        in_domain_snapshot,
        examples,
    })
}

fn build_snapshot_from_groups(
    branches: &[Arc<BranchDescriptor>],
    assembled: &AssembledGroups,
) -> Result<Snapshot, DatasetError> {
    let mut base = Snapshot::empty(1);
    for br in branches {
        base.branches.insert(br.uid, Arc::clone(br));
    }
    assembled
        .apply_to_snapshot(
            &base,
            TenantId::default_tenant(),
            HybridIndexConfig::default(),
        )
        .map_err(|e| DatasetError::Assembly(e.to_string()))
}

fn partition_indices_60_20_20(n: usize) -> (usize, usize) {
    if n < 3 {
        return (n, n);
    }
    let mut fit = ((n as f64) * 0.60).round() as usize;
    fit = fit.clamp(1, n.saturating_sub(2));
    let rem = n - fit;
    let cal = ((rem as f64) * 0.50).floor().max(1.0) as usize;
    let cal_end = (fit + cal).min(n - 1);
    (fit, cal_end)
}

fn build_ood_negative_corpus(limit: usize) -> Vec<String> {
    let base = [
        "book round trip flight tickets to osaka next friday",
        "check local cpu thermals and fan rpm telemetry",
        "compile custom wgsl raymarching fragment shader",
        "reserve veterinarian dental cleaning slot for poodle",
        "rotate webhook hmac signing secret in aws secrets manager",
        "trigger kubernetes canary rollout rollback on cluster west",
        "translate legal contract paragraph into german",
        "rebase git feature branch onto upstream main",
        "query postgres vacuum freeze transaction id wraparound",
        "convert wav audio recording into flac lossless format",
        "schedule warehouse forklift battery maintenance check",
        "calculate mortgage amortization over thirty years",
        "provision terraform s3 replication bucket encryption",
        "parse pcap packet capture tls handshake cipher",
        "generate qr code voucher for restaurant discount",
        "calibrate thermistor adc lookup table for oven controller",
        "inspect linux kernel dmesg nvme timeout messages",
        "render blender volumetric fog animation keyframes",
        "verify stripe webhook tax withholding certificate pdf",
        "invite github organization sponsors billing auditor",
        "ship live goldfish overnight express aquarium box",
        "export shop point of sale hardware receipt printer logs",
        "reset customer biometric passkey hardware token",
        "reindex lucene stemmer synonyms for catalan dictionary",
        "configure border gateway protocol bgp autonomous system peer",
        "benchmark llama weight quantization perplexity on wikitext",
        "download sentinel satellite infrared vegetation imagery",
        "synthesize drum bassline midi sequence at 128 bpm",
        "sign notarized power of attorney document pdf",
        "inspect solar inverter grid frequency droop curve",
    ];
    base.iter().take(limit).map(|s| (*s).to_owned()).collect()
}

/// Loads all branch specs (`walking_skeleton.json` + `*.openapi.json`) from `dir`.
pub fn load_branches_from_dir(dir: &Path) -> Result<Vec<Arc<BranchDescriptor>>, DatasetError> {
    let mut entries: Vec<PathBuf> = fs::read_dir(dir)
        .map_err(|source| DatasetError::Io {
            path: dir.display().to_string(),
            source,
        })?
        .filter_map(|e| e.ok().map(|d| d.path()))
        .collect();
    entries.sort();

    let mut out = Vec::new();
    for path in entries {
        let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        if name == "walking_skeleton.json" {
            out.extend(load_walking_skeleton(&path)?);
        } else if name.ends_with(".openapi.json") {
            out.push(load_openapi_branch(&path)?);
        }
    }
    Ok(out)
}

#[derive(Debug, Deserialize)]
struct WalkingFile {
    branches: Vec<WalkingBranch>,
}

#[derive(Debug, Deserialize)]
struct WalkingBranch {
    id: String,
    protocol: String,
    trust: String,
    synopsis: String,
    operations: Vec<WalkingOp>,
}

#[derive(Debug, Deserialize)]
struct WalkingOp {
    name: String,
    kind: String,
    side_effect: String,
    idempotent: bool,
    sdl: String,
    summary: String,
    #[serde(default)]
    examples: Vec<WalkingExample>,
}

#[derive(Debug, Deserialize)]
struct WalkingExample {
    intent: String,
    positive: bool,
}

fn load_walking_skeleton(path: &Path) -> Result<Vec<Arc<BranchDescriptor>>, DatasetError> {
    let raw = fs::read_to_string(path).map_err(|source| DatasetError::Io {
        path: path.display().to_string(),
        source,
    })?;
    let parsed: WalkingFile = serde_json::from_str(&raw).map_err(|source| DatasetError::Json {
        path: path.display().to_string(),
        source,
    })?;

    let mut branches = Vec::with_capacity(parsed.branches.len());
    for wb in parsed.branches {
        let branch_id = BranchId::new(&wb.id).map_err(|e| DatasetError::InvalidSpec {
            path: path.display().to_string(),
            reason: e.to_string(),
        })?;
        let protocol = match wb.protocol.as_str() {
            "mcp" => Protocol::Mcp,
            "graphql" => Protocol::Graphql,
            "grpc" => Protocol::Grpc,
            "sql" => Protocol::Sql,
            _ => Protocol::Rest,
        };
        let trust = match wb.trust.as_str() {
            "T0" => TrustTier::T0,
            "T2" => TrustTier::T2,
            _ => TrustTier::T1,
        };

        let mut ops = Vec::with_capacity(wb.operations.len());
        for wo in wb.operations {
            let op_id = OperationId::from_parts(&branch_id, &wo.name).map_err(|e| {
                DatasetError::InvalidSpec {
                    path: path.display().to_string(),
                    reason: e.to_string(),
                }
            })?;
            let kind = match wo.kind.as_str() {
                "MUTATION" => OpKind::Mutation,
                "SUBSCRIPTION" => OpKind::Subscription,
                "TASK" => OpKind::Task,
                _ => OpKind::Query,
            };
            let se = match wo.side_effect.as_str() {
                "NONE" => SideEffectLevel::None,
                "REVERSIBLE" => SideEffectLevel::Reversible,
                "IRREVERSIBLE" => SideEffectLevel::Irreversible,
                _ => SideEffectLevel::Unknown,
            };
            let mut examples: Vec<Example> = wo
                .examples
                .into_iter()
                .map(|ex| Example {
                    intent: ex.intent,
                    positive: ex.positive,
                    query_snippet: None,
                })
                .collect();
            examples.push(Example {
                intent: wo.summary.clone(),
                positive: true,
                query_snippet: Some(wo.sdl.clone()),
            });
            let desc_hash =
                *blake3::hash(format!("{}:{}", wo.sdl, wo.summary).as_bytes()).as_bytes();
            ops.push(Arc::new(OperationDescriptor {
                id: op_id,
                group: GroupId::from_bytes([0u8; 16]),
                kind,
                side_effect: SideEffect {
                    level: se,
                    verified: true,
                    requires_approval: matches!(se, SideEffectLevel::Irreversible),
                },
                idempotent: wo.idempotent,
                cost: CostHint::default(),
                desc_hash,
                sdl: Arc::from(wo.sdl),
                examples,
            }));
        }

        let uid = deterministic_branch_uid(branch_id.as_str()).map_err(|e| {
            DatasetError::InvalidSpec {
                path: path.display().to_string(),
                reason: e,
            }
        })?;
        let spec_hash = *blake3::hash(wb.synopsis.as_bytes()).as_bytes();
        branches.push(Arc::new(BranchDescriptor {
            uid,
            id: branch_id.clone(),
            protocol,
            trust,
            endpoint: Endpoint {
                uri: format!("https://{}.bench.local", branch_id.as_str()),
                transport: "http".to_owned(),
            },
            spec_hash,
            pinned_desc_hash: spec_hash,
            capabilities: Capabilities::default(),
            limits: BranchLimits::default(),
            synopsis: wb.synopsis,
            operations: ops,
            status: BranchStatus::Active,
        }));
    }
    Ok(branches)
}

fn load_openapi_branch(path: &Path) -> Result<Arc<BranchDescriptor>, DatasetError> {
    let raw = fs::read_to_string(path).map_err(|source| DatasetError::Io {
        path: path.display().to_string(),
        source,
    })?;
    let doc: Value = serde_json::from_str(&raw).map_err(|source| DatasetError::Json {
        path: path.display().to_string(),
        source,
    })?;

    let default_slug = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("openapi_branch")
        .split('.')
        .next()
        .unwrap_or("openapi_branch")
        .to_ascii_lowercase();
    let slug = doc
        .get("x-gap-branch")
        .and_then(Value::as_str)
        .unwrap_or(&default_slug);
    let branch_id = BranchId::new(slug).map_err(|e| DatasetError::InvalidSpec {
        path: path.display().to_string(),
        reason: e.to_string(),
    })?;

    let synopsis = doc
        .get("info")
        .and_then(|i| i.get("description").or_else(|| i.get("title")))
        .and_then(Value::as_str)
        .unwrap_or("OpenAPI benchmark branch")
        .to_owned();

    let paths_obj =
        doc.get("paths")
            .and_then(Value::as_object)
            .ok_or_else(|| DatasetError::InvalidSpec {
                path: path.display().to_string(),
                reason: "OpenAPI spec missing `paths` object".to_owned(),
            })?;

    let mut ops = Vec::new();
    for (http_path, path_item) in paths_obj {
        let Some(methods) = path_item.as_object() else {
            continue;
        };
        for (method, op_val) in methods {
            let lower_m = method.to_ascii_lowercase();
            if !matches!(
                lower_m.as_str(),
                "get" | "post" | "put" | "patch" | "delete" | "head" | "options"
            ) {
                continue;
            }
            let Some(op_obj) = op_val.as_object() else {
                continue;
            };
            let op_name = op_obj
                .get("operationId")
                .and_then(Value::as_str)
                .ok_or_else(|| DatasetError::InvalidSpec {
                    path: path.display().to_string(),
                    reason: format!("operation `{method} {http_path}` missing operationId"),
                })?;
            let summary = op_obj
                .get("summary")
                .or_else(|| op_obj.get("description"))
                .and_then(Value::as_str)
                .unwrap_or(op_name);

            let mut args = Vec::new();
            if let Some(params) = op_obj.get("parameters").and_then(Value::as_array) {
                for p in params {
                    let Some(pname) = p.get("name").and_then(Value::as_str) else {
                        continue;
                    };
                    let required = p.get("required").and_then(Value::as_bool).unwrap_or(false);
                    let ptype = p
                        .get("schema")
                        .and_then(|s| s.get("type"))
                        .and_then(Value::as_str)
                        .unwrap_or("string");
                    let gql_ty = match ptype {
                        "integer" => "Int",
                        "number" => "Float",
                        "boolean" => "Boolean",
                        _ => "String",
                    };
                    let bang = if required { "!" } else { "" };
                    args.push(format!("{pname}: {gql_ty}{bang}"));
                }
            }
            let sdl = if args.is_empty() {
                format!("{op_name}: JSON")
            } else {
                format!("{op_name}({}): JSON", args.join(", "))
            };

            let is_read = matches!(lower_m.as_str(), "get" | "head" | "options");
            let kind = if is_read {
                OpKind::Query
            } else {
                OpKind::Mutation
            };
            let se = if is_read {
                SideEffectLevel::None
            } else if lower_m == "delete" {
                SideEffectLevel::Irreversible
            } else {
                SideEffectLevel::Reversible
            };

            let op_id = OperationId::from_parts(&branch_id, op_name).map_err(|e| {
                DatasetError::InvalidSpec {
                    path: path.display().to_string(),
                    reason: e.to_string(),
                }
            })?;
            let desc_hash = *blake3::hash(format!("{sdl}:{summary}").as_bytes()).as_bytes();
            let examples = vec![
                Example {
                    intent: summary.to_owned(),
                    positive: true,
                    query_snippet: Some(sdl.clone()),
                },
                Example {
                    intent: format!("{} ({http_path})", humanize_camel_or_snake(op_name)),
                    positive: true,
                    query_snippet: None,
                },
            ];
            ops.push(Arc::new(OperationDescriptor {
                id: op_id,
                group: GroupId::from_bytes([0u8; 16]),
                kind,
                side_effect: SideEffect {
                    level: se,
                    verified: true,
                    requires_approval: matches!(se, SideEffectLevel::Irreversible),
                },
                idempotent: is_read || lower_m == "put",
                cost: CostHint::default(),
                desc_hash,
                sdl: Arc::from(sdl),
                examples,
            }));
        }
    }

    let uid =
        deterministic_branch_uid(branch_id.as_str()).map_err(|e| DatasetError::InvalidSpec {
            path: path.display().to_string(),
            reason: e,
        })?;
    let spec_hash = *blake3::hash(raw.as_bytes()).as_bytes();
    Ok(Arc::new(BranchDescriptor {
        uid,
        id: branch_id.clone(),
        protocol: Protocol::Rest,
        trust: TrustTier::T1,
        endpoint: Endpoint {
            uri: format!("https://{}.openapi.local", branch_id.as_str()),
            transport: "http".to_owned(),
        },
        spec_hash,
        pinned_desc_hash: spec_hash,
        capabilities: Capabilities::default(),
        limits: BranchLimits::default(),
        synopsis,
        operations: ops,
        status: BranchStatus::Active,
    }))
}

fn deterministic_branch_uid(slug: &str) -> Result<BranchUid, String> {
    let mut bytes = *blake3::hash(format!("routebench-branch:{slug}").as_bytes()).as_bytes();
    bytes[6] = (bytes[6] & 0x0f) | 0x70;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex = hex_encode(&bytes[..16]);
    let formatted = format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    );
    BranchUid::parse(&formatted).map_err(|e| e.to_string())
}

fn hash_u64(seed: u64, text: &str) -> u64 {
    let mut input = Vec::with_capacity(8 + text.len());
    input.extend_from_slice(&seed.to_le_bytes());
    input.extend_from_slice(text.as_bytes());
    let digest = blake3::hash(&input);
    let mut buf = [0u8; 8];
    buf.copy_from_slice(&digest.as_bytes()[..8]);
    u64::from_le_bytes(buf)
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

fn humanize_camel_or_snake(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for (idx, ch) in s.chars().enumerate() {
        if ch == '_' || ch == '-' || ch == '.' {
            out.push(' ');
        } else if ch.is_ascii_uppercase() && idx > 0 {
            out.push(' ');
            out.push(ch.to_ascii_lowercase());
        } else {
            out.push(ch.to_ascii_lowercase());
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn first_positive_or_summary(op: &OperationDescriptor) -> String {
    op.examples.iter().find(|ex| ex.positive).map_or_else(
        || humanize_camel_or_snake(op.id.name()),
        |ex| ex.intent.clone(),
    )
}

fn extract_arg_names(sdl: &str) -> Vec<String> {
    let Some(start) = sdl.find('(') else {
        return Vec::new();
    };
    let Some(end) = sdl[start..].find(')') else {
        return Vec::new();
    };
    let inner = &sdl[start + 1..start + end];
    inner
        .split(',')
        .filter_map(|part| part.split(':').next())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect()
}
