//! ML-free hybrid element index (BM25, flat SIMD, HNSW, and reciprocal rank fusion) (`L1`).
#![forbid(unsafe_code)]
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::todo,
    clippy::unimplemented
)]

use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap, HashSet};

use gap_core::error::{ErrorCode, GapError, GapResult};
use gap_core::ids::{BranchUid, OperationId, TenantId, TenantScope};
use gap_core::traits::{ElementKind, ElementSearch, SearchHit};

/// Default threshold above which dense search switches from flat SIMD scan to HNSW (`RTE-001`, `RTE-006`).
pub const DEFAULT_HNSW_THRESHOLD: usize = 20_000;

/// Reciprocal rank fusion smoothing constant (`RTE-001`).
pub const RRF_K: f32 = 60.0;

/// Normalization denominator `(2 / (RRF_K + 1))` when fusing lexical + dense channels (`RTE-001`).
pub const RRF_TWO_CHANNEL_NORM: f32 = 2.0 / (RRF_K + 1.0);

/// Single-channel RRF normalization denominator (`1 / (RRF_K + 1)`).
pub const RRF_ONE_CHANNEL_NORM: f32 = 1.0 / (RRF_K + 1.0);

/// Default BM25 tuning and field weights (`RTE-004`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Bm25Config {
    /// Term-frequency saturation parameter (`1.2`).
    pub k1: f32,
    /// Document length normalization parameter (`0.75`).
    pub b: f32,
    /// Weight assigned to element name tokens (`3.0`).
    pub weight_name: f32,
    /// Weight assigned to path/type tokens (`2.0`).
    pub weight_path: f32,
    /// Weight assigned to description/text tokens (`1.0`).
    pub weight_text: f32,
    /// Weight assigned to example snippets (`1.5`).
    pub weight_examples: f32,
}

impl Default for Bm25Config {
    fn default() -> Self {
        Self {
            k1: 1.2,
            b: 0.75,
            weight_name: 3.0,
            weight_path: 2.0,
            weight_text: 1.0,
            weight_examples: 1.5,
        }
    }
}

/// HNSW graph construction and query parameters (`RTE-001`, `RTE-006`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HnswConfig {
    /// Maximum out-degree for upper layers (`M = 16`).
    pub m: usize,
    /// Maximum out-degree for base layer `0` (`M_max0 = 32`).
    pub m_max0: usize,
    /// Beam width during index construction (`200`).
    pub ef_construction: usize,
    /// Beam width during query search (`64`).
    pub ef_search: usize,
}

impl Default for HnswConfig {
    fn default() -> Self {
        Self {
            m: 16,
            m_max0: 32,
            ef_construction: 200,
            ef_search: 64,
        }
    }
}

/// Configuration for building a [`HybridElementIndex`].
#[derive(Debug, Clone, PartialEq)]
pub struct HybridIndexConfig {
    /// Pinned embedding model identifier associated with dense vectors (`RTE-002`).
    pub embedding_model_id: Option<String>,
    /// Vector count above which dense search switches from [`FlatVectorIndex`] to [`HnswIndex`].
    pub hnsw_threshold: usize,
    /// BM25 scoring configuration.
    pub bm25: Bm25Config,
    /// HNSW configuration used when `vector_count > hnsw_threshold`.
    pub hnsw: HnswConfig,
    /// Multiplier for per-channel candidate count before RRF fusion.
    pub rrf_candidate_multiplier: usize,
    /// Minimum per-channel candidate pool before RRF fusion.
    pub rrf_min_candidates: usize,
}

impl Default for HybridIndexConfig {
    fn default() -> Self {
        Self {
            embedding_model_id: None,
            hnsw_threshold: DEFAULT_HNSW_THRESHOLD,
            bm25: Bm25Config::default(),
            hnsw: HnswConfig::default(),
            rrf_candidate_multiplier: 4,
            rrf_min_candidates: 64,
        }
    }
}

/// Schema/catalog element document indexed by [`HybridElementIndex`].
#[derive(Debug, Clone, PartialEq)]
pub struct ElementDoc {
    /// Tenant owning this element (`INV-09`).
    pub tenant: TenantId,
    /// Globally unique element identifier within `(tenant)` used as canonical sort key (`RTE-010`).
    pub element_id: String,
    /// Element classification (`Operation`, `Type`, `Field`, `NodeCategory`, `Example`).
    pub kind: ElementKind,
    /// Owning branch UID, when scoped to a branch.
    pub branch_uid: Option<BranchUid>,
    /// Associated operation ID, when `kind == ElementKind::Operation`.
    pub operation_id: Option<OperationId>,
    /// Primary identifier name (`weight = 3.0`).
    pub name: String,
    /// Structural path / parent type chain (`weight = 2.0`).
    pub path: String,
    /// Natural-language summary/description (`weight = 1.0`).
    pub text: String,
    /// Usage/query examples (`weight = 1.5`).
    pub examples: Vec<String>,
    /// Optional dense vector embedding (normalized on index build if non-zero).
    pub embedding: Option<Vec<f32>>,
}

impl ElementDoc {
    /// Constructs a new [`ElementDoc`] under [`TenantId::default_tenant`].
    #[must_use]
    pub fn new(element_id: impl Into<String>, kind: ElementKind, name: impl Into<String>) -> Self {
        Self {
            tenant: TenantId::default_tenant(),
            element_id: element_id.into(),
            kind,
            branch_uid: None,
            operation_id: None,
            name: name.into(),
            path: String::new(),
            text: String::new(),
            examples: Vec::new(),
            embedding: None,
        }
    }

    /// Sets the owning tenant (`INV-09`).
    #[must_use]
    pub fn with_tenant(mut self, tenant: TenantId) -> Self {
        self.tenant = tenant;
        self
    }

    /// Sets the owning branch UID.
    #[must_use]
    pub fn with_branch_uid(mut self, branch_uid: BranchUid) -> Self {
        self.branch_uid = Some(branch_uid);
        self
    }

    /// Sets the operation identifier.
    #[must_use]
    pub fn with_operation_id(mut self, operation_id: OperationId) -> Self {
        self.operation_id = Some(operation_id);
        self
    }

    /// Sets the structural path (`weight = 2.0`).
    #[must_use]
    pub fn with_path(mut self, path: impl Into<String>) -> Self {
        self.path = path.into();
        self
    }

    /// Sets the description text (`weight = 1.0`).
    #[must_use]
    pub fn with_text(mut self, text: impl Into<String>) -> Self {
        self.text = text.into();
        self
    }

    /// Sets example strings (`weight = 1.5`).
    #[must_use]
    pub fn with_examples(mut self, examples: Vec<String>) -> Self {
        self.examples = examples;
        self
    }

    /// Sets the dense embedding vector.
    #[must_use]
    pub fn with_embedding(mut self, embedding: Vec<f32>) -> Self {
        self.embedding = Some(embedding);
        self
    }
}

/// Bit-packed per-element visibility mask checked *during* search (`RTE-005`, `INV-05`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VisibilityBitset {
    words: Vec<u64>,
    len: usize,
    visible_count: usize,
}

impl VisibilityBitset {
    /// Creates a bitset of `len` elements with all bits set visible (`true`).
    #[must_use]
    pub fn new_all_visible(len: usize) -> Self {
        let word_count = len.div_ceil(64);
        let mut words = vec![u64::MAX; word_count];
        let rem = len % 64;
        if rem != 0
            && let Some(last) = words.last_mut()
        {
            *last = (1_u64 << rem) - 1;
        }
        Self {
            words,
            len,
            visible_count: len,
        }
    }

    /// Creates a bitset of `len` elements with all bits hidden (`false`).
    #[must_use]
    pub fn new_all_hidden(len: usize) -> Self {
        let word_count = len.div_ceil(64);
        Self {
            words: vec![0_u64; word_count],
            len,
            visible_count: 0,
        }
    }

    /// Builds a [`VisibilityBitset`] from explicit boolean flags.
    #[must_use]
    pub fn from_bools(flags: &[bool]) -> Self {
        let mut set = Self::new_all_hidden(flags.len());
        for (idx, &flag) in flags.iter().enumerate() {
            if flag {
                set.set(idx, true);
            }
        }
        set
    }

    /// Sets visibility for `index`; out-of-bounds indices are safely ignored.
    pub fn set(&mut self, index: usize, visible: bool) {
        if index >= self.len {
            return;
        }
        let word_idx = index / 64;
        let mask = 1_u64 << (index % 64);
        if let Some(word) = self.words.get_mut(word_idx) {
            let currently_visible = (*word & mask) != 0;
            if visible && !currently_visible {
                *word |= mask;
                self.visible_count = self.visible_count.saturating_add(1);
            } else if !visible && currently_visible {
                *word &= !mask;
                self.visible_count = self.visible_count.saturating_sub(1);
            }
        }
    }

    /// Returns `true` iff element `index` is within bounds and visible.
    #[must_use]
    #[inline]
    pub fn is_visible(&self, index: usize) -> bool {
        if index >= self.len {
            return false;
        }
        let word_idx = index / 64;
        let mask = 1_u64 << (index % 64);
        self.words
            .get(word_idx)
            .is_some_and(|word| (*word & mask) != 0)
    }

    /// Number of elements tracked by this bitset.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Returns `true` if `len == 0`.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Count of elements currently marked visible.
    #[must_use]
    pub const fn visible_count(&self) -> usize {
        self.visible_count
    }
}

/// Identifier-aware tokenizer splitting `snake_case`, `camelCase`, `PascalCase`, `kebab-case`,
/// dot paths, and acronym runs into lowercased tokens (`RTE-003`).
#[must_use]
pub fn tokenize_identifier_aware(input: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    for segment in input.split(|c: char| !c.is_alphanumeric()) {
        if segment.is_empty() {
            continue;
        }
        split_camel_into(segment, &mut tokens);
    }
    tokens
}

fn split_camel_into(segment: &str, out: &mut Vec<String>) {
    let chars: Vec<char> = segment.chars().collect();
    if chars.is_empty() {
        return;
    }

    let mut current = String::new();
    for (idx, &ch) in chars.iter().enumerate() {
        if idx > 0 {
            let prev = chars[idx - 1];
            let next = chars.get(idx + 1).copied();
            let boundary = (ch.is_uppercase() && (prev.is_lowercase() || prev.is_numeric()))
                || (ch.is_uppercase()
                    && prev.is_uppercase()
                    && next.is_some_and(char::is_lowercase));
            if boundary && !current.is_empty() {
                out.push(std::mem::take(&mut current));
            }
        }
        for lower in ch.to_lowercase() {
            current.push(lower);
        }
    }
    if !current.is_empty() {
        out.push(current);
    }
}

/// Computes an 8-lane unrolled SIMD-friendly dot product between equal-length `f32` slices (`RTE-006`).
///
/// Missing lanes beyond the shorter slice length contribute `0.0`.
#[must_use]
#[inline]
pub fn dot_product_8x(a: &[f32], b: &[f32]) -> f32 {
    let len = a.len().min(b.len());
    let a = &a[..len];
    let b = &b[..len];

    let mut acc = [0.0_f32; 8];
    let (chunks_a, rem_a) = a.as_chunks::<8>();
    let (chunks_b, rem_b) = b.as_chunks::<8>();

    for (ca, cb) in chunks_a.iter().zip(chunks_b.iter()) {
        acc[0] += ca[0] * cb[0];
        acc[1] += ca[1] * cb[1];
        acc[2] += ca[2] * cb[2];
        acc[3] += ca[3] * cb[3];
        acc[4] += ca[4] * cb[4];
        acc[5] += ca[5] * cb[5];
        acc[6] += ca[6] * cb[6];
        acc[7] += ca[7] * cb[7];
    }

    let mut tail = 0.0_f32;
    for (x, y) in rem_a.iter().zip(rem_b.iter()) {
        tail += x * y;
    }

    acc[0] + acc[1] + acc[2] + acc[3] + acc[4] + acc[5] + acc[6] + acc[7] + tail
}

/// Normalizes a vector in-place to unit L2 norm if its norm is strictly positive.
///
/// Returns `Err` if any entry is non-finite.
pub fn normalize_l2(vec: &mut [f32]) -> GapResult<()> {
    if vec.iter().any(|v| !v.is_finite()) {
        return Err(GapError::new(
            ErrorCode::ValidationError,
            "vector contains non-finite value",
        ));
    }
    let norm_sq = dot_product_8x(vec, vec);
    if norm_sq > 0.0 {
        let inv_norm = 1.0_f32 / norm_sq.sqrt();
        for x in vec.iter_mut() {
            let scaled = *x * inv_norm;
            *x = if scaled == 0.0 { 0.0 } else { scaled };
        }
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq)]
struct Posting {
    doc_id: usize,
    weighted_tf: f32,
}

#[derive(Debug, Clone, PartialEq)]
struct RankedDoc {
    doc_id: usize,
    score: f32,
    tie_key: String,
}

impl RankedDoc {
    fn better_than(&self, other: &Self) -> bool {
        match self.score.total_cmp(&other.score) {
            Ordering::Greater => true,
            Ordering::Less => false,
            Ordering::Equal => self.tie_key < other.tie_key,
        }
    }
}

/// Borrowed input tuple `(tenant, element_id, name, path, text, examples)` for [`Bm25Index::build`].
pub type Bm25InputRow<'a> = (
    &'a TenantId,
    &'a str,
    &'a str,
    &'a str,
    &'a str,
    &'a [String],
);

/// Inverted BM25 index over identifier-tokenized weighted element fields (`RTE-004`).
#[derive(Debug, Clone)]
pub struct Bm25Index {
    config: Bm25Config,
    doc_count: usize,
    avg_dl: f32,
    doc_lengths: Vec<f32>,
    doc_ids_canonical: Vec<String>,
    doc_tenants: Vec<TenantId>,
    postings: HashMap<String, Vec<Posting>>,
}

impl Bm25Index {
    /// Builds a [`Bm25Index`] over canonically ordered docs.
    #[must_use]
    pub fn build(docs: &[Bm25InputRow<'_>], config: Bm25Config) -> Self {
        let doc_count = docs.len();
        let mut postings: HashMap<String, Vec<Posting>> = HashMap::new();
        let mut doc_lengths = Vec::with_capacity(doc_count);
        let mut doc_ids_canonical = Vec::with_capacity(doc_count);
        let mut doc_tenants = Vec::with_capacity(doc_count);
        let mut total_dl = 0.0_f32;

        for (doc_id, &(tenant, element_id, name, path, text, examples)) in docs.iter().enumerate() {
            let mut tf_map: HashMap<String, f32> = HashMap::new();
            Self::accumulate_field(&mut tf_map, name, config.weight_name);
            Self::accumulate_field(&mut tf_map, path, config.weight_path);
            Self::accumulate_field(&mut tf_map, text, config.weight_text);
            for ex in examples {
                Self::accumulate_field(&mut tf_map, ex, config.weight_examples);
            }

            let mut dl = 0.0_f32;
            // Sort terms so postings build sequence is canonical.
            let mut terms: Vec<(String, f32)> = tf_map.into_iter().collect();
            terms.sort_by(|a, b| a.0.cmp(&b.0));

            for (term, weighted_tf) in terms {
                dl += weighted_tf;
                postings.entry(term).or_default().push(Posting {
                    doc_id,
                    weighted_tf,
                });
            }

            total_dl += dl;
            doc_lengths.push(dl);
            doc_ids_canonical.push(element_id.to_owned());
            doc_tenants.push(tenant.clone());
        }

        let avg_dl = if doc_count == 0 {
            1.0
        } else {
            (total_dl / doc_count as f32).max(1e-6)
        };

        Self {
            config,
            doc_count,
            avg_dl,
            doc_lengths,
            doc_ids_canonical,
            doc_tenants,
            postings,
        }
    }

    fn accumulate_field(tf_map: &mut HashMap<String, f32>, input: &str, weight: f32) {
        if weight <= 0.0 || input.is_empty() {
            return;
        }
        for tok in tokenize_identifier_aware(input) {
            *tf_map.entry(tok).or_insert(0.0) += weight;
        }
    }

    /// Searches BM25 postings while enforcing `tenant` isolation (`INV-09`) and `visibility`
    /// masking *during* posting iteration (`RTE-005`, `INV-05`).
    #[must_use]
    pub fn search(
        &self,
        tenant: &TenantId,
        query: &str,
        visibility: &VisibilityBitset,
        limit: usize,
    ) -> Vec<(usize, f32)> {
        if limit == 0 || self.doc_count == 0 || visibility.visible_count() == 0 {
            return Vec::new();
        }
        let query_tokens = tokenize_identifier_aware(query);
        if query_tokens.is_empty() {
            return Vec::new();
        }

        // Deduplicate/weight query terms deterministically by sorted term.
        let mut q_tf: HashMap<String, usize> = HashMap::new();
        for tok in query_tokens {
            *q_tf.entry(tok).or_insert(0) += 1;
        }
        let mut unique_q: Vec<(String, usize)> = q_tf.into_iter().collect();
        unique_q.sort_by(|a, b| a.0.cmp(&b.0));

        let n = self.doc_count as f32;
        let mut scores: HashMap<usize, f32> = HashMap::new();

        for (term, q_count) in unique_q {
            let Some(list) = self.postings.get(&term) else {
                continue;
            };
            let df = list.len() as f32;
            let idf = (((n - df + 0.5) / (df + 0.5)) + 1.0).ln();
            let q_mult = q_count as f32;

            for posting in list {
                let doc_id = posting.doc_id;
                if self.doc_tenants.get(doc_id) != Some(tenant) || !visibility.is_visible(doc_id) {
                    continue;
                }
                let dl = self.doc_lengths.get(doc_id).copied().unwrap_or(self.avg_dl);
                let norm = 1.0 - self.config.b + self.config.b * (dl / self.avg_dl);
                let tf = posting.weighted_tf;
                let denom = tf + self.config.k1 * norm.max(1e-6);
                let term_score = idf * ((tf * (self.config.k1 + 1.0)) / denom) * q_mult;
                *scores.entry(doc_id).or_insert(0.0) += term_score;
            }
        }

        let mut ranked: Vec<RankedDoc> = scores
            .into_iter()
            .filter_map(|(doc_id, score)| {
                let tie_key = self.doc_ids_canonical.get(doc_id)?.clone();
                Some(RankedDoc {
                    doc_id,
                    score,
                    tie_key,
                })
            })
            .collect();

        ranked.sort_by(|a, b| {
            if a.better_than(b) {
                Ordering::Less
            } else if b.better_than(a) {
                Ordering::Greater
            } else {
                Ordering::Equal
            }
        });
        ranked.truncate(limit);
        ranked.into_iter().map(|r| (r.doc_id, r.score)).collect()
    }
}

/// Flat exact cosine-similarity vector index (`<= 20_000` vectors, `RTE-006`).
#[derive(Debug, Clone)]
pub struct FlatVectorIndex {
    dim: usize,
    vectors: Vec<f32>,
    doc_ids: Vec<usize>,
    tie_keys: Vec<String>,
    tenants: Vec<TenantId>,
}

impl FlatVectorIndex {
    /// Constructs a [`FlatVectorIndex`] from normalized dense vectors mapped to canonical `doc_id`s.
    pub fn build(dim: usize, entries: Vec<(usize, TenantId, String, Vec<f32>)>) -> GapResult<Self> {
        if dim == 0 && !entries.is_empty() {
            return Err(GapError::new(
                ErrorCode::ValidationError,
                "dense index dimension must be > 0",
            ));
        }
        let mut vectors = Vec::with_capacity(entries.len() * dim);
        let mut doc_ids = Vec::with_capacity(entries.len());
        let mut tie_keys = Vec::with_capacity(entries.len());
        let mut tenants = Vec::with_capacity(entries.len());

        for (doc_id, tenant, tie_key, mut vec) in entries {
            if vec.len() != dim {
                return Err(GapError::new(
                    ErrorCode::ValidationError,
                    format!(
                        "embedding dimension mismatch for `{tie_key}`: expected {dim}, got {}",
                        vec.len()
                    ),
                ));
            }
            normalize_l2(&mut vec)?;
            vectors.extend_from_slice(&vec);
            doc_ids.push(doc_id);
            tie_keys.push(tie_key);
            tenants.push(tenant);
        }

        Ok(Self {
            dim,
            vectors,
            doc_ids,
            tie_keys,
            tenants,
        })
    }

    /// Dimensionality of stored vectors.
    #[must_use]
    pub const fn dimension(&self) -> usize {
        self.dim
    }

    /// Number of indexed vectors.
    #[must_use]
    pub fn len(&self) -> usize {
        self.doc_ids.len()
    }

    /// Returns `true` when zero vectors are indexed.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.doc_ids.is_empty()
    }

    /// Searches exact cosine similarity while checking `tenant` + `visibility` *before* scoring (`RTE-005`).
    pub fn search(
        &self,
        tenant: &TenantId,
        query: &[f32],
        visibility: &VisibilityBitset,
        limit: usize,
    ) -> GapResult<Vec<(usize, f32)>> {
        if limit == 0 || self.is_empty() || visibility.visible_count() == 0 {
            return Ok(Vec::new());
        }
        if query.len() != self.dim {
            return Err(GapError::new(
                ErrorCode::ValidationError,
                format!(
                    "query embedding dimension mismatch: expected {}, got {}",
                    self.dim,
                    query.len()
                ),
            ));
        }
        let mut norm_query = query.to_vec();
        normalize_l2(&mut norm_query)?;

        let mut ranked: Vec<RankedDoc> = Vec::new();
        for (slot, (&doc_id, row_tenant)) in
            self.doc_ids.iter().zip(self.tenants.iter()).enumerate()
        {
            if row_tenant != tenant || !visibility.is_visible(doc_id) {
                continue;
            }
            let start = slot * self.dim;
            let vec_slice = &self.vectors[start..start + self.dim];
            let sim = dot_product_8x(&norm_query, vec_slice);
            ranked.push(RankedDoc {
                doc_id,
                score: sim,
                tie_key: self.tie_keys[slot].clone(),
            });
        }

        ranked.sort_by(|a, b| {
            if a.better_than(b) {
                Ordering::Less
            } else if b.better_than(a) {
                Ordering::Greater
            } else {
                Ordering::Equal
            }
        });
        ranked.truncate(limit);
        Ok(ranked.into_iter().map(|r| (r.doc_id, r.score)).collect())
    }
}

#[derive(Debug, Clone, Copy)]
struct HeapCandidate {
    slot: usize,
    sim: f32,
}

impl PartialEq for HeapCandidate {
    fn eq(&self, other: &Self) -> bool {
        self.slot == other.slot && self.sim.to_bits() == other.sim.to_bits()
    }
}
impl Eq for HeapCandidate {}

/// Max-heap wrapper (highest similarity pops first; lower `slot` wins ties).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct MaxSimCandidate(HeapCandidate);

impl Ord for MaxSimCandidate {
    fn cmp(&self, other: &Self) -> Ordering {
        match self.0.sim.total_cmp(&other.0.sim) {
            Ordering::Equal => other.0.slot.cmp(&self.0.slot),
            ord => ord,
        }
    }
}
impl PartialOrd for MaxSimCandidate {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Min-heap wrapper (lowest similarity pops first; higher `slot` evicted first on tie).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct MinSimCandidate(HeapCandidate);

impl Ord for MinSimCandidate {
    fn cmp(&self, other: &Self) -> Ordering {
        match other.0.sim.total_cmp(&self.0.sim) {
            Ordering::Equal => self.0.slot.cmp(&other.0.slot),
            ord => ord,
        }
    }
}
impl PartialOrd for MinSimCandidate {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Pure-Rust deterministic HNSW approximate nearest neighbor index (`> 20_000` vectors, `RTE-001`, `RTE-006`).
#[derive(Debug, Clone)]
pub struct HnswIndex {
    dim: usize,
    config: HnswConfig,
    vectors: Vec<f32>,
    doc_ids: Vec<usize>,
    tie_keys: Vec<String>,
    tenants: Vec<TenantId>,
    /// `layers[slot][layer]` stores neighbor `slot` indices.
    layers: Vec<Vec<Vec<usize>>>,
    entry_slot: Option<usize>,
    max_level: usize,
}

impl HnswIndex {
    /// Builds a deterministic [`HnswIndex`] with heuristic neighbor pruning (`M = 16`, `ef_construction = 200`).
    pub fn build(
        dim: usize,
        config: HnswConfig,
        entries: Vec<(usize, TenantId, String, Vec<f32>)>,
    ) -> GapResult<Self> {
        if dim == 0 && !entries.is_empty() {
            return Err(GapError::new(
                ErrorCode::ValidationError,
                "HNSW vector dimension must be > 0",
            ));
        }
        if config.m < 2 || config.ef_construction == 0 || config.ef_search == 0 {
            return Err(GapError::new(
                ErrorCode::ValidationError,
                "invalid HNSW parameters: require m >= 2, ef_construction > 0, ef_search > 0",
            ));
        }

        let n = entries.len();
        let mut vectors = Vec::with_capacity(n * dim);
        let mut doc_ids = Vec::with_capacity(n);
        let mut tie_keys = Vec::with_capacity(n);
        let mut tenants = Vec::with_capacity(n);

        for (doc_id, tenant, tie_key, mut vec) in entries {
            if vec.len() != dim {
                return Err(GapError::new(
                    ErrorCode::ValidationError,
                    format!(
                        "HNSW dimension mismatch for `{tie_key}`: expected {dim}, got {}",
                        vec.len()
                    ),
                ));
            }
            normalize_l2(&mut vec)?;
            vectors.extend_from_slice(&vec);
            doc_ids.push(doc_id);
            tie_keys.push(tie_key);
            tenants.push(tenant);
        }

        let mut index = Self {
            dim,
            config,
            vectors,
            doc_ids,
            tie_keys,
            tenants,
            layers: vec![Vec::new(); n],
            entry_slot: None,
            max_level: 0,
        };

        for slot in 0..n {
            index.insert_slot(slot);
        }

        Ok(index)
    }

    /// Dimensionality of vectors in this HNSW graph.
    #[must_use]
    pub const fn dimension(&self) -> usize {
        self.dim
    }

    /// Number of indexed vectors.
    #[must_use]
    pub fn len(&self) -> usize {
        self.doc_ids.len()
    }

    /// Returns `true` if empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.doc_ids.is_empty()
    }

    fn slice(&self, slot: usize) -> &[f32] {
        let start = slot * self.dim;
        &self.vectors[start..start + self.dim]
    }

    #[inline]
    fn sim(&self, query: &[f32], slot: usize) -> f32 {
        dot_product_8x(query, self.slice(slot))
    }

    fn deterministic_level(&self, slot: usize) -> usize {
        // SplitMix64 mixing on canonical slot + tie_key hash for deterministic exponential levels (`RTE-010`).
        let mut h = (slot as u64).wrapping_add(0x9E37_79B9_7F4A_7C15);
        for &b in self.tie_keys[slot].as_bytes() {
            h = h.wrapping_mul(0xBF58_476D_1CE4_E5B9) ^ u64::from(b);
        }
        h = (h ^ (h >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        h = (h ^ (h >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        h ^= h >> 31;
        let u = ((h >> 11) as f64 + 1.0) / ((1_u64 << 53) as f64 + 1.0);
        let ml = 1.0 / (self.config.m as f64).ln();
        let lvl = (-u.ln() * ml).floor();
        (lvl as usize).min(16)
    }

    fn insert_slot(&mut self, slot: usize) {
        let level = self.deterministic_level(slot);
        self.layers[slot] = vec![Vec::new(); level + 1];

        let Some(mut curr_entry) = self.entry_slot else {
            self.entry_slot = Some(slot);
            self.max_level = level;
            return;
        };

        let query_vec = self.slice(slot).to_vec();
        let mut curr_sim = self.sim(&query_vec, curr_entry);

        if self.max_level > level {
            for lc in ((level + 1)..=self.max_level).rev() {
                let (next_entry, next_sim) =
                    self.greedy_step_layer(&query_vec, curr_entry, curr_sim, lc);
                curr_entry = next_entry;
                curr_sim = next_sim;
            }
        }

        let top_connect = level.min(self.max_level);
        for lc in (0..=top_connect).rev() {
            let candidates =
                self.search_layer_raw(&query_vec, curr_entry, self.config.ef_construction, lc);
            if let Some(best) = candidates.first() {
                curr_entry = best.slot;
            }
            let max_links = if lc == 0 {
                self.config.m_max0
            } else {
                self.config.m
            };
            let selected = self.select_neighbors_heuristic(&query_vec, &candidates, max_links);
            self.layers[slot][lc] = selected.clone();

            for &nb in &selected {
                self.layers[nb][lc].push(slot);
                if self.layers[nb][lc].len() > max_links {
                    let nb_vec = self.slice(nb).to_vec();
                    let mut nb_cands: Vec<HeapCandidate> = self.layers[nb][lc]
                        .iter()
                        .map(|&other| HeapCandidate {
                            slot: other,
                            sim: dot_product_8x(&nb_vec, self.slice(other)),
                        })
                        .collect();
                    Self::sort_desc(&mut nb_cands);
                    nb_cands.dedup_by_key(|c| c.slot);
                    self.layers[nb][lc] =
                        self.select_neighbors_heuristic(&nb_vec, &nb_cands, max_links);
                }
            }
        }

        if level > self.max_level {
            self.entry_slot = Some(slot);
            self.max_level = level;
        }
    }

    fn greedy_step_layer(
        &self,
        query: &[f32],
        mut curr: usize,
        mut curr_sim: f32,
        layer: usize,
    ) -> (usize, f32) {
        loop {
            let mut improved = false;
            for &nb in &self.layers[curr][layer] {
                let nb_sim = self.sim(query, nb);
                if nb_sim > curr_sim || (nb_sim.to_bits() == curr_sim.to_bits() && nb < curr) {
                    curr = nb;
                    curr_sim = nb_sim;
                    improved = true;
                }
            }
            if !improved {
                break;
            }
        }
        (curr, curr_sim)
    }

    fn sort_desc(cands: &mut [HeapCandidate]) {
        cands.sort_by(|a, b| match b.sim.total_cmp(&a.sim) {
            Ordering::Equal => a.slot.cmp(&b.slot),
            ord => ord,
        });
    }

    fn select_neighbors_heuristic(
        &self,
        _target: &[f32],
        candidates_desc: &[HeapCandidate],
        max_links: usize,
    ) -> Vec<usize> {
        let mut selected: Vec<usize> = Vec::with_capacity(max_links);
        let mut discarded: Vec<usize> = Vec::new();

        for &cand in candidates_desc {
            if selected.len() >= max_links {
                break;
            }
            let cand_vec = self.slice(cand.slot);
            let closer_to_existing = selected
                .iter()
                .any(|&sel| dot_product_8x(cand_vec, self.slice(sel)) > cand.sim);
            if !closer_to_existing {
                selected.push(cand.slot);
            } else {
                discarded.push(cand.slot);
            }
        }

        // keepPrunedConnections = true for strong manifold connectivity and >= 0.99 recall.
        for d in discarded {
            if selected.len() >= max_links {
                break;
            }
            selected.push(d);
        }
        selected
    }

    fn search_layer_raw(
        &self,
        query: &[f32],
        entry: usize,
        ef: usize,
        layer: usize,
    ) -> Vec<HeapCandidate> {
        let entry_sim = self.sim(query, entry);
        let entry_cand = HeapCandidate {
            slot: entry,
            sim: entry_sim,
        };

        let mut visited: HashSet<usize> = HashSet::with_capacity(ef * 2);
        visited.insert(entry);

        let mut candidates = BinaryHeap::new();
        candidates.push(MaxSimCandidate(entry_cand));

        let mut results = BinaryHeap::new();
        results.push(MinSimCandidate(entry_cand));

        while let Some(MaxSimCandidate(curr)) = candidates.pop() {
            let worst_res = results.peek().map_or(f32::NEG_INFINITY, |m| m.0.sim);
            if curr.sim < worst_res && results.len() >= ef {
                break;
            }

            if let Some(neighbors) = self.layers[curr.slot].get(layer) {
                for &nb in neighbors {
                    if !visited.insert(nb) {
                        continue;
                    }
                    let nb_sim = self.sim(query, nb);
                    let nb_cand = HeapCandidate {
                        slot: nb,
                        sim: nb_sim,
                    };
                    let worst_now = results.peek().map_or(f32::NEG_INFINITY, |m| m.0.sim);
                    if results.len() < ef || nb_sim > worst_now {
                        candidates.push(MaxSimCandidate(nb_cand));
                        results.push(MinSimCandidate(nb_cand));
                        if results.len() > ef {
                            let _ = results.pop();
                        }
                    }
                }
            }
        }

        let mut out: Vec<HeapCandidate> = results.into_iter().map(|m| m.0).collect();
        Self::sort_desc(&mut out);
        out
    }

    /// Approximate top-k cosine search using `ef_search` while recording *only* visible + tenant-matching
    /// elements into the result heap during graph traversal (`RTE-005`, `INV-05`).
    pub fn search(
        &self,
        tenant: &TenantId,
        query: &[f32],
        visibility: &VisibilityBitset,
        limit: usize,
    ) -> GapResult<Vec<(usize, f32)>> {
        if limit == 0 || self.is_empty() || visibility.visible_count() == 0 {
            return Ok(Vec::new());
        }
        if query.len() != self.dim {
            return Err(GapError::new(
                ErrorCode::ValidationError,
                format!(
                    "query embedding dimension mismatch: expected {}, got {}",
                    self.dim,
                    query.len()
                ),
            ));
        }

        let mut norm_query = query.to_vec();
        normalize_l2(&mut norm_query)?;

        let Some(mut curr_entry) = self.entry_slot else {
            return Ok(Vec::new());
        };
        let mut curr_sim = self.sim(&norm_query, curr_entry);

        for lc in (1..=self.max_level).rev() {
            let (next_entry, next_sim) =
                self.greedy_step_layer(&norm_query, curr_entry, curr_sim, lc);
            curr_entry = next_entry;
            curr_sim = next_sim;
        }

        let ef = self.config.ef_search.max(limit * 2);
        let mut visited: HashSet<usize> = HashSet::with_capacity(ef * 2);
        visited.insert(curr_entry);

        let entry_cand = HeapCandidate {
            slot: curr_entry,
            sim: curr_sim,
        };
        let mut explore_cands = BinaryHeap::new();
        explore_cands.push(MaxSimCandidate(entry_cand));

        let mut explore_beam = BinaryHeap::new();
        explore_beam.push(MinSimCandidate(entry_cand));

        // Visible-only result set updated strictly when visibility + tenant pass during traversal.
        let mut visible_results: Vec<RankedDoc> = Vec::new();
        if self.slot_visible(curr_entry, tenant, visibility) {
            visible_results.push(RankedDoc {
                doc_id: self.doc_ids[curr_entry],
                score: curr_sim,
                tie_key: self.tie_keys[curr_entry].clone(),
            });
        }

        while let Some(MaxSimCandidate(curr)) = explore_cands.pop() {
            let beam_floor = explore_beam.peek().map_or(f32::NEG_INFINITY, |m| m.0.sim);
            if curr.sim < beam_floor && explore_beam.len() >= ef {
                break;
            }

            if let Some(neighbors) = self.layers[curr.slot].first() {
                for &nb in neighbors {
                    if !visited.insert(nb) {
                        continue;
                    }
                    let nb_sim = self.sim(&norm_query, nb);
                    let nb_cand = HeapCandidate {
                        slot: nb,
                        sim: nb_sim,
                    };
                    if self.slot_visible(nb, tenant, visibility) {
                        visible_results.push(RankedDoc {
                            doc_id: self.doc_ids[nb],
                            score: nb_sim,
                            tie_key: self.tie_keys[nb].clone(),
                        });
                    }
                    let beam_floor_now = explore_beam.peek().map_or(f32::NEG_INFINITY, |m| m.0.sim);
                    if explore_beam.len() < ef || nb_sim > beam_floor_now {
                        explore_cands.push(MaxSimCandidate(nb_cand));
                        explore_beam.push(MinSimCandidate(nb_cand));
                        if explore_beam.len() > ef {
                            let _ = explore_beam.pop();
                        }
                    }
                }
            }
        }

        // If aggressive visibility masking hides most nodes near the entry point so graph traversal
        // did not surface enough visible candidates, scan remaining unvisited visible slots so hidden
        // neighbors never starve visible ones.
        if visible_results.len() < limit && visibility.visible_count() > visible_results.len() {
            for slot in 0..self.doc_ids.len() {
                if !visited.contains(&slot) && self.slot_visible(slot, tenant, visibility) {
                    visible_results.push(RankedDoc {
                        doc_id: self.doc_ids[slot],
                        score: self.sim(&norm_query, slot),
                        tie_key: self.tie_keys[slot].clone(),
                    });
                }
            }
        }

        visible_results.sort_by(|a, b| {
            if a.better_than(b) {
                Ordering::Less
            } else if b.better_than(a) {
                Ordering::Greater
            } else {
                Ordering::Equal
            }
        });
        visible_results.truncate(limit);
        Ok(visible_results
            .into_iter()
            .map(|r| (r.doc_id, r.score))
            .collect())
    }

    #[inline]
    fn slot_visible(&self, slot: usize, tenant: &TenantId, visibility: &VisibilityBitset) -> bool {
        let doc_id = self.doc_ids[slot];
        self.tenants.get(slot) == Some(tenant) && visibility.is_visible(doc_id)
    }
}

#[derive(Debug, Clone)]
enum DenseBackend {
    None,
    Flat(FlatVectorIndex),
    Hnsw(HnswIndex),
}

/// Complete ML-free hybrid retrieval index combining [`Bm25Index`] and [`FlatVectorIndex`] / [`HnswIndex`]
/// with Reciprocal Rank Fusion (`RTE-001`..`RTE-010`, `INV-05`, `INV-09`).
#[derive(Debug, Clone)]
pub struct HybridElementIndex {
    config: HybridIndexConfig,
    docs: Vec<ElementDoc>,
    bm25: Bm25Index,
    dense: DenseBackend,
    default_visibility: VisibilityBitset,
    index_hash: [u8; 32],
}

impl HybridElementIndex {
    /// Builds a deterministic [`HybridElementIndex`], canonically sorting elements by `(tenant, element_id)`
    /// so equal document sets yield identical [`Self::index_hash`] and query ordering regardless of input order (`RTE-010`).
    pub fn build(mut docs: Vec<ElementDoc>, config: HybridIndexConfig) -> GapResult<Self> {
        // Canonical sort (`RTE-010`)
        docs.sort_by(|a, b| {
            a.tenant
                .as_str()
                .cmp(b.tenant.as_str())
                .then_with(|| a.element_id.cmp(&b.element_id))
        });

        // Reject duplicate `(tenant, element_id)`
        for pair in docs.windows(2) {
            if pair[0].tenant == pair[1].tenant && pair[0].element_id == pair[1].element_id {
                return Err(GapError::new(
                    ErrorCode::ValidationError,
                    format!(
                        "duplicate element_id `{}` for tenant `{}`",
                        pair[0].element_id, pair[0].tenant
                    ),
                ));
            }
        }

        // Validate uniform dense embedding dimensionality if any embeddings are present.
        let mut inferred_dim: Option<usize> = None;
        let mut dense_entries: Vec<(usize, TenantId, String, Vec<f32>)> = Vec::new();
        for (doc_id, doc) in docs.iter_mut().enumerate() {
            if let Some(ref mut emb) = doc.embedding {
                match inferred_dim {
                    None => {
                        if emb.is_empty() {
                            return Err(GapError::new(
                                ErrorCode::ValidationError,
                                format!("empty embedding vector on element `{}`", doc.element_id),
                            ));
                        }
                        inferred_dim = Some(emb.len());
                    }
                    Some(dim) if emb.len() != dim => {
                        return Err(GapError::new(
                            ErrorCode::ValidationError,
                            format!(
                                "embedding dimension mismatch on `{}`: expected {dim}, got {}",
                                doc.element_id,
                                emb.len()
                            ),
                        ));
                    }
                    Some(_) => {}
                }
                normalize_l2(emb)?;
                dense_entries.push((
                    doc_id,
                    doc.tenant.clone(),
                    doc.element_id.clone(),
                    emb.clone(),
                ));
            }
        }

        let bm25_inputs: Vec<Bm25InputRow<'_>> = docs
            .iter()
            .map(|d| {
                (
                    &d.tenant,
                    d.element_id.as_str(),
                    d.name.as_str(),
                    d.path.as_str(),
                    d.text.as_str(),
                    d.examples.as_slice(),
                )
            })
            .collect();
        let bm25 = Bm25Index::build(&bm25_inputs, config.bm25);

        let dense = match inferred_dim {
            None => DenseBackend::None,
            Some(dim) if dense_entries.len() <= config.hnsw_threshold => {
                DenseBackend::Flat(FlatVectorIndex::build(dim, dense_entries)?)
            }
            Some(dim) => DenseBackend::Hnsw(HnswIndex::build(dim, config.hnsw, dense_entries)?),
        };

        let default_visibility = VisibilityBitset::new_all_visible(docs.len());
        let index_hash = compute_index_hash(&docs, &config);

        Ok(Self {
            config,
            docs,
            bm25,
            dense,
            default_visibility,
            index_hash,
        })
    }

    /// Returns `true` when the dense backend uses HNSW (`> hnsw_threshold`).
    #[must_use]
    pub const fn uses_hnsw(&self) -> bool {
        matches!(self.dense, DenseBackend::Hnsw(_))
    }

    /// Deterministic SHA-256 hash over canonical sorted index contents (`RTE-010`).
    #[must_use]
    pub const fn index_hash(&self) -> &[u8; 32] {
        &self.index_hash
    }

    /// Lowercase hex encoding of [`Self::index_hash`].
    #[must_use]
    pub fn index_hash_hex(&self) -> String {
        encode_hex(&self.index_hash)
    }

    /// Returns a fresh all-visible [`VisibilityBitset`] matching this index length.
    #[must_use]
    pub fn all_visible_bitset(&self) -> VisibilityBitset {
        VisibilityBitset::new_all_visible(self.docs.len())
    }

    /// Looks up the canonical internal index position (`doc_id`) for `(tenant, element_id)`.
    #[must_use]
    pub fn doc_index_of(&self, tenant: &TenantId, element_id: &str) -> Option<usize> {
        self.docs
            .binary_search_by(|d| {
                d.tenant
                    .as_str()
                    .cmp(tenant.as_str())
                    .then_with(|| d.element_id.as_str().cmp(element_id))
            })
            .ok()
    }

    /// Performs hybrid BM25 + dense vector + RRF search enforcing tenant isolation (`INV-09`)
    /// and [`VisibilityBitset`] filtering *during* search (`RTE-005`, `INV-05`).
    pub fn search_with_visibility(
        &self,
        tenant: &TenantScope,
        query: &str,
        query_embedding: Option<&[f32]>,
        visibility: &VisibilityBitset,
        limit: usize,
    ) -> GapResult<Vec<SearchHit>> {
        if limit == 0 || self.docs.is_empty() || visibility.visible_count() == 0 {
            return Ok(Vec::new());
        }

        let candidate_k = limit
            .saturating_mul(self.config.rrf_candidate_multiplier)
            .max(self.config.rrf_min_candidates);

        let bm25_hits = self
            .bm25
            .search(tenant.tenant(), query, visibility, candidate_k);

        let dense_hits = match (query_embedding, &self.dense) {
            (Some(q_vec), DenseBackend::Flat(flat)) => {
                flat.search(tenant.tenant(), q_vec, visibility, candidate_k)?
            }
            (Some(q_vec), DenseBackend::Hnsw(hnsw)) => {
                hnsw.search(tenant.tenant(), q_vec, visibility, candidate_k)?
            }
            (Some(_), DenseBackend::None) => {
                return Err(GapError::new(
                    ErrorCode::ValidationError,
                    "query embedding provided for index without dense vectors",
                ));
            }
            (None, _) => Vec::new(),
        };

        let fused = fuse_rrf(
            &self.docs,
            &bm25_hits,
            &dense_hits,
            query_embedding.is_some() && !matches!(self.dense, DenseBackend::None),
            limit,
        );
        Ok(fused)
    }

    /// Wraps this index with an explicit [`VisibilityBitset`] to expose [`ElementSearch`] per principal.
    #[must_use]
    pub fn scoped<'a>(&'a self, visibility: VisibilityBitset) -> ScopedElementSearch<'a> {
        ScopedElementSearch {
            index: self,
            visibility,
        }
    }
}

impl ElementSearch for HybridElementIndex {
    fn embedding_model_id(&self) -> Option<&str> {
        self.config.embedding_model_id.as_deref()
    }

    fn element_count(&self) -> usize {
        self.docs.len()
    }

    fn search(
        &self,
        tenant: &TenantScope,
        query: &str,
        query_embedding: Option<&[f32]>,
        limit: usize,
    ) -> GapResult<Vec<SearchHit>> {
        self.search_with_visibility(
            tenant,
            query,
            query_embedding,
            &self.default_visibility,
            limit,
        )
    }
}

/// Borrowed view combining a [`HybridElementIndex`] with an authorized [`VisibilityBitset`].
#[derive(Debug, Clone)]
pub struct ScopedElementSearch<'a> {
    index: &'a HybridElementIndex,
    visibility: VisibilityBitset,
}

impl ElementSearch for ScopedElementSearch<'_> {
    fn embedding_model_id(&self) -> Option<&str> {
        self.index.embedding_model_id()
    }

    fn element_count(&self) -> usize {
        self.visibility.visible_count()
    }

    fn search(
        &self,
        tenant: &TenantScope,
        query: &str,
        query_embedding: Option<&[f32]>,
        limit: usize,
    ) -> GapResult<Vec<SearchHit>> {
        self.index
            .search_with_visibility(tenant, query, query_embedding, &self.visibility, limit)
    }
}

/// Applies Reciprocal Rank Fusion `sum 1 / (60 + rank)` normalized by `2 / 61` (two channels)
/// or `1 / 61` (single lexical channel), returning deterministic top-`limit` hits (`RTE-001`, `RTE-010`).
#[must_use]
pub fn fuse_rrf(
    docs: &[ElementDoc],
    bm25_hits: &[(usize, f32)],
    dense_hits: &[(usize, f32)],
    has_dense_channel: bool,
    limit: usize,
) -> Vec<SearchHit> {
    #[derive(Default)]
    struct Accum {
        lexical_score: f32,
        dense_score: Option<f32>,
        rrf_raw: f32,
    }

    let mut by_doc: HashMap<usize, Accum> = HashMap::new();
    for (rank_zero, &(doc_id, score)) in bm25_hits.iter().enumerate() {
        let rank = (rank_zero + 1) as f32;
        let entry = by_doc.entry(doc_id).or_default();
        entry.lexical_score = score;
        entry.rrf_raw += 1.0 / (RRF_K + rank);
    }

    for (rank_zero, &(doc_id, score)) in dense_hits.iter().enumerate() {
        let rank = (rank_zero + 1) as f32;
        let entry = by_doc.entry(doc_id).or_default();
        entry.dense_score = Some(score);
        entry.rrf_raw += 1.0 / (RRF_K + rank);
    }

    let norm = if has_dense_channel {
        RRF_TWO_CHANNEL_NORM
    } else {
        RRF_ONE_CHANNEL_NORM
    };

    let mut fused: Vec<(usize, SearchHit)> = by_doc
        .into_iter()
        .filter_map(|(doc_id, accum)| {
            let doc = docs.get(doc_id)?;
            Some((
                doc_id,
                SearchHit {
                    element_id: doc.element_id.clone(),
                    kind: doc.kind,
                    branch_uid: doc.branch_uid,
                    operation_id: doc.operation_id.clone(),
                    lexical_score: accum.lexical_score,
                    dense_score: accum.dense_score,
                    fused_score: accum.rrf_raw / norm,
                },
            ))
        })
        .collect();

    fused.sort_by(|a, b| match b.1.fused_score.total_cmp(&a.1.fused_score) {
        Ordering::Equal => a.1.element_id.cmp(&b.1.element_id),
        ord => ord,
    });
    fused.truncate(limit);
    fused.into_iter().map(|(_, hit)| hit).collect()
}

fn compute_index_hash(docs: &[ElementDoc], config: &HybridIndexConfig) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(b"gap-index-v1");
    if let Some(ref model_id) = config.embedding_model_id {
        hash_len_prefixed(&mut hasher, model_id.as_bytes());
    } else {
        hasher.update(&[0]);
    }
    hasher.update(&(config.hnsw_threshold as u64).to_le_bytes());
    hasher.update(&config.bm25.k1.to_bits().to_le_bytes());
    hasher.update(&config.bm25.b.to_bits().to_le_bytes());
    hasher.update(&config.bm25.weight_name.to_bits().to_le_bytes());
    hasher.update(&config.bm25.weight_path.to_bits().to_le_bytes());
    hasher.update(&config.bm25.weight_text.to_bits().to_le_bytes());
    hasher.update(&config.bm25.weight_examples.to_bits().to_le_bytes());

    hasher.update(&(docs.len() as u64).to_le_bytes());
    for doc in docs {
        hash_len_prefixed(&mut hasher, doc.tenant.as_str().as_bytes());
        hash_len_prefixed(&mut hasher, doc.element_id.as_bytes());
        hasher.update(&[kind_tag(doc.kind)]);
        match doc.branch_uid {
            Some(uid) => {
                hasher.update(&[1]);
                hasher.update(uid.as_uuid().as_bytes());
            }
            None => hasher.update(&[0]),
        }
        match &doc.operation_id {
            Some(op) => {
                hasher.update(&[1]);
                hash_len_prefixed(&mut hasher, op.as_str().as_bytes());
            }
            None => hasher.update(&[0]),
        }
        hash_len_prefixed(&mut hasher, doc.name.as_bytes());
        hash_len_prefixed(&mut hasher, doc.path.as_bytes());
        hash_len_prefixed(&mut hasher, doc.text.as_bytes());
        hasher.update(&(doc.examples.len() as u64).to_le_bytes());
        for ex in &doc.examples {
            hash_len_prefixed(&mut hasher, ex.as_bytes());
        }
        match &doc.embedding {
            Some(vec) => {
                hasher.update(&[1]);
                hasher.update(&(vec.len() as u64).to_le_bytes());
                for &v in vec {
                    let canonical = if v == 0.0 { 0.0_f32 } else { v };
                    hasher.update(&canonical.to_bits().to_le_bytes());
                }
            }
            None => hasher.update(&[0]),
        }
    }

    hasher.finalize()
}

fn hash_len_prefixed(hasher: &mut Sha256, bytes: &[u8]) {
    hasher.update(&(bytes.len() as u64).to_le_bytes());
    hasher.update(bytes);
}

const fn kind_tag(kind: ElementKind) -> u8 {
    match kind {
        ElementKind::Operation => 1,
        ElementKind::Type => 2,
        ElementKind::Field => 3,
        ElementKind::NodeCategory => 4,
        ElementKind::Example => 5,
    }
}

fn encode_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(char::from(HEX[usize::from(b >> 4)]));
        out.push(char::from(HEX[usize::from(b & 0x0f)]));
    }
    out
}

/// Minimal pure-Rust FIPS 180-4 SHA-256 used for deterministic `index_hash` computation (`L1`, `#![forbid(unsafe_code)]`).
struct Sha256 {
    state: [u32; 8],
    bit_len: u64,
    buffer: [u8; 64],
    buffer_len: usize,
}

impl Sha256 {
    const K: [u32; 64] = [
        0x428a_2f98,
        0x7137_4491,
        0xb5c0_fbcf,
        0xe9b5_dba5,
        0x3956_c25b,
        0x59f1_11f1,
        0x923f_82a4,
        0xab1c_5ed5,
        0xd807_aa98,
        0x1283_5b01,
        0x2431_85be,
        0x550c_7dc3,
        0x72be_5d74,
        0x80de_b1fe,
        0x9bdc_06a7,
        0xc19b_f174,
        0xe49b_69c1,
        0xefbe_4786,
        0x0fc1_9dc6,
        0x240c_a1cc,
        0x2de9_2c6f,
        0x4a74_84aa,
        0x5cb0_a9dc,
        0x76f9_88da,
        0x983e_5152,
        0xa831_c66d,
        0xb003_27c8,
        0xbf59_7fc7,
        0xc6e0_0bf3,
        0xd5a7_9147,
        0x06ca_6351,
        0x1429_2967,
        0x27b7_0a85,
        0x2e1b_2138,
        0x4d2c_6dfc,
        0x5338_0d13,
        0x650a_7354,
        0x766a_0abb,
        0x81c2_c92e,
        0x9272_2c85,
        0xa2bf_e8a1,
        0xa81a_664b,
        0xc24b_8b70,
        0xc76c_51a3,
        0xd192_e819,
        0xd699_0624,
        0xf40e_3585,
        0x106a_a070,
        0x19a4_c116,
        0x1e37_6c08,
        0x2748_774c,
        0x34b0_bcb5,
        0x391c_0cb3,
        0x4ed8_aa4a,
        0x5b9c_ca4f,
        0x682e_6ff3,
        0x748f_82ee,
        0x78a5_636f,
        0x84c8_7814,
        0x8cc7_0208,
        0x90be_fffa,
        0xa450_6ceb,
        0xbef9_a3f7,
        0xc671_78f2,
    ];

    const fn new() -> Self {
        Self {
            state: [
                0x6a09_e667,
                0xbb67_ae85,
                0x3c6e_f372,
                0xa54f_f53a,
                0x510e_527f,
                0x9b05_688c,
                0x1f83_d9ab,
                0x5be0_cd19,
            ],
            bit_len: 0,
            buffer: [0; 64],
            buffer_len: 0,
        }
    }

    fn update(&mut self, mut input: &[u8]) {
        self.bit_len = self.bit_len.wrapping_add((input.len() as u64) * 8);
        if self.buffer_len > 0 {
            let take = (64 - self.buffer_len).min(input.len());
            self.buffer[self.buffer_len..self.buffer_len + take].copy_from_slice(&input[..take]);
            self.buffer_len += take;
            input = &input[take..];
            if self.buffer_len == 64 {
                let block = self.buffer;
                self.compress(&block);
                self.buffer_len = 0;
            }
        }
        while input.len() >= 64 {
            let mut block = [0_u8; 64];
            block.copy_from_slice(&input[..64]);
            self.compress(&block);
            input = &input[64..];
        }
        if !input.is_empty() {
            self.buffer[..input.len()].copy_from_slice(input);
            self.buffer_len = input.len();
        }
    }

    fn compress(&mut self, block: &[u8; 64]) {
        let mut w = [0_u32; 64];
        for (i, slot) in w.iter_mut().enumerate().take(16) {
            let off = i * 4;
            *slot =
                u32::from_be_bytes([block[off], block[off + 1], block[off + 2], block[off + 3]]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }

        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = self.state;
        for (i, &wi) in w.iter().enumerate() {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let temp1 = h
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(Self::K[i])
                .wrapping_add(wi);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = s0.wrapping_add(maj);
            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }

        self.state[0] = self.state[0].wrapping_add(a);
        self.state[1] = self.state[1].wrapping_add(b);
        self.state[2] = self.state[2].wrapping_add(c);
        self.state[3] = self.state[3].wrapping_add(d);
        self.state[4] = self.state[4].wrapping_add(e);
        self.state[5] = self.state[5].wrapping_add(f);
        self.state[6] = self.state[6].wrapping_add(g);
        self.state[7] = self.state[7].wrapping_add(h);
    }

    fn finalize(mut self) -> [u8; 32] {
        self.buffer[self.buffer_len] = 0x80;
        self.buffer_len += 1;
        if self.buffer_len > 56 {
            for slot in &mut self.buffer[self.buffer_len..] {
                *slot = 0;
            }
            let block = self.buffer;
            self.compress(&block);
            self.buffer = [0; 64];
            self.buffer_len = 0;
        }
        for slot in &mut self.buffer[self.buffer_len..56] {
            *slot = 0;
        }
        self.buffer[56..64].copy_from_slice(&self.bit_len.to_be_bytes());
        let block = self.buffer;
        self.compress(&block);

        let mut out = [0_u8; 32];
        for (i, word) in self.state.iter().enumerate() {
            out[i * 4..(i + 1) * 4].copy_from_slice(&word.to_be_bytes());
        }
        out
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
mod tests {
    use super::*;

    fn unit_vec(dim: usize, seed: u64) -> Vec<f32> {
        let mut s = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut v = Vec::with_capacity(dim);
        for _ in 0..dim {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            let val = ((s & 0xFFFF) as f32 / 32_768.0) - 1.0;
            v.push(val);
        }
        normalize_l2(&mut v).expect("finite unit_vec");
        v
    }

    fn default_scope() -> TenantScope {
        TenantId::default_tenant().scope()
    }

    // spec: RTE-003
    #[test]
    fn rte_003_identifier_aware_tokenization_splits_cases_and_paths() {
        assert_eq!(
            tokenize_identifier_aware("billing.getUserID_v2-token"),
            vec!["billing", "get", "user", "id", "v2", "token"]
        );
        assert_eq!(
            tokenize_identifier_aware("HTTPSServerPort"),
            vec!["https", "server", "port"]
        );
        assert_eq!(
            tokenize_identifier_aware("oauth2RefreshToken"),
            vec!["oauth2", "refresh", "token"]
        );
    }

    // spec: RTE-004
    #[test]
    fn rte_004_bm25_weights_name_above_path_above_examples_above_text() {
        let docs = vec![
            ElementDoc::new("op.name", ElementKind::Operation, "refundInvoice")
                .with_text("handles ledger record"),
            ElementDoc::new("op.path", ElementKind::Operation, "execute")
                .with_path("billing.refund.invoice")
                .with_text("handles ledger record"),
            ElementDoc::new("op.example", ElementKind::Operation, "execute")
                .with_text("handles ledger record")
                .with_examples(vec!["refund invoice now".to_owned()]),
            ElementDoc::new("op.text", ElementKind::Operation, "execute")
                .with_text("refund invoice entry in ledger"),
        ];
        let index = HybridElementIndex::build(docs, HybridIndexConfig::default())
            .expect("build bm25 index");
        let hits = index
            .search(&default_scope(), "refund invoice", None, 10)
            .expect("bm25 search");

        let ordered_ids: Vec<&str> = hits.iter().map(|h| h.element_id.as_str()).collect();
        assert_eq!(
            ordered_ids,
            vec!["op.name", "op.path", "op.example", "op.text"]
        );
        assert!(hits[0].fused_score > 0.99);
    }

    // spec: RTE-005, INV-05
    #[test]
    fn rte_005_inv_05_hidden_elements_never_returned_or_displace_visible_topk() {
        let mut docs = Vec::new();
        for i in 0..8 {
            let mut emb = vec![0.0_f32; 8];
            emb[0] = 1.0 - (i as f32) * 0.05;
            emb[1] = (i as f32) * 0.05;
            docs.push(
                ElementDoc::new(format!("op.{i:02}"), ElementKind::Operation, "listInvoices")
                    .with_text("list customer invoices")
                    .with_embedding(emb),
            );
        }
        let index =
            HybridElementIndex::build(docs, HybridIndexConfig::default()).expect("build index");

        // Hide the top 4 exact matches (`op.00..op.03`). Asking for limit = 3 must return
        // `op.04, op.05, op.06` rather than post-filtering an empty top-3.
        let mut vis = index.all_visible_bitset();
        for i in 0..4 {
            let id = format!("op.{i:02}");
            let idx = index
                .doc_index_of(&TenantId::default_tenant(), &id)
                .expect("doc exists");
            vis.set(idx, false);
        }

        let q = vec![1.0_f32, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0];
        let hits = index
            .search_with_visibility(&default_scope(), "list invoices", Some(&q), &vis, 3)
            .expect("visibility search");

        assert_eq!(hits.len(), 3);
        let ids: Vec<&str> = hits.iter().map(|h| h.element_id.as_str()).collect();
        assert_eq!(ids, vec!["op.04", "op.05", "op.06"]);
        for hidden in ["op.00", "op.01", "op.02", "op.03"] {
            assert!(!ids.contains(&hidden));
        }

        // Cross-tenant isolation (`INV-09`)
        let other = TenantId::new("acme").expect("tenant").scope();
        let other_hits = index
            .search_with_visibility(&other, "list invoices", Some(&q), &vis, 10)
            .expect("other tenant");
        assert!(other_hits.is_empty());
    }

    // spec: RTE-006
    #[test]
    fn rte_006_flat_simd_vector_search_matches_exact_dot_and_switches_over_20k() {
        // Under threshold -> uses flat SIMD index
        let docs = vec![
            ElementDoc::new("vec.a", ElementKind::Operation, "alpha")
                .with_embedding(vec![1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]),
            ElementDoc::new("vec.b", ElementKind::Operation, "beta")
                .with_embedding(vec![0.8, 0.6, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]),
        ];
        let idx =
            HybridElementIndex::build(docs, HybridIndexConfig::default()).expect("flat build");
        assert!(!idx.uses_hnsw());
        let q = vec![1.0_f32, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0];
        let hits = idx
            .search(&default_scope(), "", Some(&q), 2)
            .expect("search");
        assert_eq!(hits[0].element_id, "vec.a");
        assert!((hits[0].dense_score.expect("dense") - 1.0).abs() < 1e-5);
        assert_eq!(hits[1].element_id, "vec.b");
        assert!((hits[1].dense_score.expect("dense") - 0.8).abs() < 1e-5);
    }

    // spec: RTE-001, RTE-006
    #[test]
    fn rte_001_hnsw_threshold_above_20k_and_recall_at_10_above_99_percent() {
        // 1. Verify default HNSW activation threshold strictly > 20_000 in HybridElementIndex
        let mut docs = Vec::with_capacity(20_005);
        for i in 0..20_005 {
            docs.push(
                ElementDoc::new(format!("el.{i:05}"), ElementKind::Operation, "op")
                    .with_embedding(unit_vec(8, i as u64 + 1)),
            );
        }
        let cfg = HybridIndexConfig {
            hnsw: HnswConfig {
                m: 4,
                m_max0: 8,
                ef_construction: 8,
                ef_search: 16,
            },
            ..HybridIndexConfig::default()
        };
        let hybrid = HybridElementIndex::build(docs, cfg).expect("hnsw above 20k");
        assert!(hybrid.uses_hnsw());

        // 2. Measure recall@10 >= 0.99 against exact FlatVectorIndex on 1,200 16-d vectors
        let dim = 16;
        let count = 1_200;
        let mut flat_entries = Vec::with_capacity(count);
        for i in 0..count {
            let v = unit_vec(dim, (i as u64 + 100) * 17);
            flat_entries.push((i, TenantId::default_tenant(), format!("n.{i:04}"), v));
        }
        let flat = FlatVectorIndex::build(dim, flat_entries.clone()).expect("flat exact");
        let hnsw = HnswIndex::build(
            dim,
            HnswConfig {
                m: 16,
                m_max0: 32,
                ef_construction: 200,
                ef_search: 64,
            },
            flat_entries,
        )
        .expect("hnsw");
        let vis = VisibilityBitset::new_all_visible(count);
        let tenant = TenantId::default_tenant();

        let num_queries = 40_usize;
        let k = 10_usize;
        let mut total_overlap = 0_usize;
        for q_idx in 0..num_queries {
            // Query near an existing manifold point + perturbation
            let mut q = unit_vec(dim, ((q_idx * 29 + 5) as u64 + 100) * 17);
            q[0] += 0.05;
            normalize_l2(&mut q).expect("norm q");

            let exact = flat.search(&tenant, &q, &vis, k).expect("exact");
            let approx = hnsw.search(&tenant, &q, &vis, k).expect("approx");
            let exact_set: HashSet<usize> = exact.into_iter().map(|(id, _)| id).collect();
            let hits = approx
                .iter()
                .filter(|(id, _)| exact_set.contains(id))
                .count();
            total_overlap += hits;
        }
        let recall = total_overlap as f64 / (num_queries * k) as f64;
        assert!(
            recall >= 0.99,
            "expected HNSW recall@10 >= 0.99 against exact flat scan, got {recall:.4}"
        );
    }

    // spec: RTE-001, RTE-005
    #[test]
    fn rte_001_rrf_normalization_and_hnsw_visibility_masking() {
        let dim = 8;
        let count = 64;
        let entries: Vec<_> = (0..count)
            .map(|i| {
                (
                    i,
                    TenantId::default_tenant(),
                    format!("h.{i:02}"),
                    unit_vec(dim, (i + 1) as u64),
                )
            })
            .collect();
        let hnsw = HnswIndex::build(dim, HnswConfig::default(), entries).expect("hnsw");
        let mut vis = VisibilityBitset::new_all_visible(count);
        for i in 0..60 {
            vis.set(i, false);
        }
        let tenant = TenantId::default_tenant();
        let q = unit_vec(dim, 1);
        let res = hnsw.search(&tenant, &q, &vis, 4).expect("masked hnsw");
        assert_eq!(res.len(), 4);
        assert!(res.iter().all(|(doc_id, _)| *doc_id >= 60));

        // Verify RRF formula 1 / (60 + rank) normalized by 2 / 61
        let docs = vec![
            ElementDoc::new("a", ElementKind::Operation, "a"),
            ElementDoc::new("b", ElementKind::Operation, "b"),
        ];
        let fused = fuse_rrf(&docs, &[(0, 5.0), (1, 2.0)], &[(0, 0.99)], true, 2);
        assert_eq!(fused[0].element_id, "a");
        assert!((fused[0].fused_score - 1.0).abs() < 1e-6);
        // `b` appears only in BM25 at rank 2: (1 / 62) / (2 / 61) = 61 / 124
        let expected_b = (1.0_f32 / 62.0) / RRF_TWO_CHANNEL_NORM;
        assert!((fused[1].fused_score - expected_b).abs() < 1e-6);
    }

    // spec: RTE-010, INV-05
    #[test]
    fn rte_010_inv_05_deterministic_build_hash_and_tie_breaking() {
        let doc_a = ElementDoc::new("alpha.createOrder", ElementKind::Operation, "order")
            .with_text("create order")
            .with_embedding(vec![0.5, 0.5, 0.5, 0.5]);
        let doc_b = ElementDoc::new("beta.createOrder", ElementKind::Operation, "order")
            .with_text("create order")
            .with_embedding(vec![0.5, 0.5, 0.5, 0.5]);

        let idx1 = HybridElementIndex::build(
            vec![doc_a.clone(), doc_b.clone()],
            HybridIndexConfig::default(),
        )
        .expect("idx1");
        let idx2 = HybridElementIndex::build(vec![doc_b, doc_a], HybridIndexConfig::default())
            .expect("idx2");

        assert_eq!(idx1.index_hash(), idx2.index_hash());
        assert_eq!(idx1.index_hash_hex(), idx2.index_hash_hex());

        let q = vec![0.5_f32, 0.5, 0.5, 0.5];
        let hits1 = idx1
            .search(&default_scope(), "order", Some(&q), 10)
            .expect("hits1");
        let hits2 = idx2
            .search(&default_scope(), "order", Some(&q), 10)
            .expect("hits2");
        assert_eq!(hits1, hits2);
        assert_eq!(hits1[0].element_id, "alpha.createOrder");
        assert_eq!(hits1[1].element_id, "beta.createOrder");
    }
}
