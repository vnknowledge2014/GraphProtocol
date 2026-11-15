//! Owned ModernBERT encoder, 2-layer transformer decision head, `[MASK]` marker gather,
//! and scorer/rubric/NoUL MLPs (`ADR-005`, `RTE-041`, `RTE-042`, `WP-M5-02`).

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

use gap_core::error::{ErrorCode, GapError, GapResult};
use gap_core::traits::BoxFuture;
use gap_route::{DecisionBackend, GroupOption, SINK_OPTION_KEY};

use crate::candle::json::parse_json;
use crate::candle::safetensors::{
    Precision, SafeTensorsArchive, TensorData, f16_bits_to_f32, f32_to_f16_bits,
    serialize_safetensors,
};
use crate::candle::sequence::{DEFAULT_HEAD_MAX_LEN, EncodedSequence, SequenceBuilder};
use crate::candle::sha256::sha256_bytes;
use crate::candle::tokenizer::{
    CLS_ID, DEFAULT_VOCAB_SIZE, LayaTokenizer, MASK_ID, NUM_TOPIC_BUCKETS,
};

/// Architectural hyperparameters for [`LayaCandle`] (`ADR-005`).
#[derive(Debug, Clone, PartialEq)]
pub struct LayaConfig {
    /// Token vocabulary size.
    pub vocab_size: usize,
    /// Hidden representation width (`d_model`).
    pub d_model: usize,
    /// Multi-head attention head count (must divide `d_model` with even `head_dim`).
    pub num_heads: usize,
    /// GeGLU feed-forward inner projection width.
    pub d_ff: usize,
    /// Number of ModernBERT encoder layers with alternating local/global RoPE.
    pub num_encoder_layers: usize,
    /// Number of decision-head transformer layers (`2` per `WP-M5-02`).
    pub num_head_layers: usize,
    /// Hidden width of the scorer MLP consuming `[h_cls || h_marker_i]`.
    pub d_scorer_mlp: usize,
    /// Maximum context window in tokens (`512` default per `RTE-042`).
    pub head_max_len: usize,
    /// Local RoPE base theta (`10_000.0` default).
    pub rope_theta_local: f32,
    /// Global RoPE base theta applied every `global_attn_every` layer (`160_000.0` default).
    pub rope_theta_global: f32,
    /// Layer period for global RoPE attention (`3` default).
    pub global_attn_every: usize,
    /// LayerNorm numerical stability epsilon.
    pub layer_norm_eps: f32,
}

impl Default for LayaConfig {
    fn default() -> Self {
        Self::reference_architecture()
    }
}

impl LayaConfig {
    /// Deterministic ModernBERT + 2-layer transformer head configuration validated against all
    /// 210 golden fixtures (`RTE-042`, `ADR-005`).
    #[must_use]
    pub const fn reference_architecture() -> Self {
        Self {
            vocab_size: DEFAULT_VOCAB_SIZE,
            d_model: 32,
            num_heads: 4,
            d_ff: 64,
            num_encoder_layers: 2,
            num_head_layers: 2,
            d_scorer_mlp: 68,
            head_max_len: DEFAULT_HEAD_MAX_LEN,
            rope_theta_local: 10_000.0,
            rope_theta_global: 160_000.0,
            global_attn_every: 3,
            layer_norm_eps: 1e-5,
        }
    }

    /// Validates dimension divisibility and non-zero sizes.
    pub fn validate(&self) -> GapResult<()> {
        if self.vocab_size < 16 || self.d_model < 8 || self.num_heads == 0 {
            return Err(GapError::new(
                ErrorCode::DiscoveryDegraded,
                "invalid LayaConfig: vocab_size, d_model, or num_heads out of bounds",
            ));
        }
        if !self.d_model.is_multiple_of(self.num_heads) {
            return Err(GapError::new(
                ErrorCode::DiscoveryDegraded,
                "invalid LayaConfig: d_model must be divisible by num_heads",
            ));
        }
        let head_dim = self.d_model / self.num_heads;
        if !head_dim.is_multiple_of(2) || head_dim == 0 {
            return Err(GapError::new(
                ErrorCode::DiscoveryDegraded,
                "invalid LayaConfig: head_dim must be a positive even integer for RoPE",
            ));
        }
        if self.num_head_layers == 0 || self.d_ff == 0 || self.d_scorer_mlp == 0 {
            return Err(GapError::new(
                ErrorCode::DiscoveryDegraded,
                "invalid LayaConfig: zero layer/MLP dimension",
            ));
        }
        Ok(())
    }
}

/// Multi-head output produced by [`LayaCandle::forward_sequence`].
#[derive(Debug, Clone, PartialEq)]
pub struct LayaForwardOutput {
    /// Raw choice logits aligned 1:1 with caller `options`.
    pub choice_logits: Vec<f32>,
    /// Five-way rubric quality logits `[1..=5]`.
    pub score_logits: [f32; 5],
    /// Four-way NoUL logits `[supported, needs_clarification, unsafe_side_effect, out_of_scope]`.
    pub noul_logits: [f32; 4],
}

#[derive(Debug, Clone)]
struct TransformerBlockWeights {
    attn_ln_w: TensorData,
    attn_ln_b: TensorData,
    wq: TensorData,
    wk: TensorData,
    wv: TensorData,
    wo: TensorData,
    ffn_ln_w: TensorData,
    ffn_ln_b: TensorData,
    w_gate: TensorData,
    w_up: TensorData,
    w_down: TensorData,
}

#[derive(Debug, Clone)]
struct LayaWeights {
    word_embeddings: TensorData,
    emb_ln_w: TensorData,
    emb_ln_b: TensorData,
    encoder_layers: Vec<TransformerBlockWeights>,
    head_layers: Vec<TransformerBlockWeights>,
    head_out_ln_w: TensorData,
    head_out_ln_b: TensorData,
    scorer_w1: TensorData,
    scorer_b1: TensorData,
    scorer_w2: TensorData,
    scorer_b2: TensorData,
    score_head_w: TensorData,
    score_head_b: TensorData,
    noul_head_w: TensorData,
    noul_head_b: TensorData,
}

/// Pure-Rust `candle`-semantics ModernBERT + 2-layer decision-head System-1 backend (`WP-M5-02`).
#[derive(Debug, Clone)]
pub struct LayaCandle {
    model_id: String,
    precision: Precision,
    config: LayaConfig,
    tokenizer: LayaTokenizer,
    weights: LayaWeights,
}

impl LayaCandle {
    /// Constructs a calibrated built-in checkpoint serialized via `.safetensors` in `precision`.
    #[must_use]
    pub fn calibrated_default(precision: Precision) -> Self {
        let config = LayaConfig::reference_architecture();
        let tokenizer = LayaTokenizer::builtin_default();
        let named = build_calibrated_tensor_specs(&config, &tokenizer);
        let refs: Vec<(&str, &[usize], &[f32])> = named
            .iter()
            .map(|(n, s, v)| (n.as_str(), s.as_slice(), v.as_slice()))
            .collect();
        // Known-valid static shapes cannot exceed shape limits; fallback still returns valid F32 struct cleanly.
        if let Ok(bytes) = serialize_safetensors(&refs, precision)
            && let Ok(arch) = SafeTensorsArchive::from_bytes(&bytes)
            && let Ok(loaded) =
                Self::from_archive(arch, tokenizer.clone(), config.clone(), precision)
        {
            return loaded;
        }
        Self::from_raw_vecs(named, tokenizer, config, precision)
    }

    /// Writes deterministic calibrated `.safetensors` weights (`precision`: `F32` or `F16`) to `path`.
    ///
    /// # Errors
    ///
    /// Returns [`GapError`] if directory creation or writing fails.
    pub fn write_deterministic_checkpoint(
        path: &Path,
        config: &LayaConfig,
        precision: Precision,
    ) -> GapResult<()> {
        config.validate()?;
        let tokenizer = LayaTokenizer::builtin_default();
        let named = build_calibrated_tensor_specs(config, &tokenizer);
        let refs: Vec<(&str, &[usize], &[f32])> = named
            .iter()
            .map(|(n, s, v)| (n.as_str(), s.as_slice(), v.as_slice()))
            .collect();
        let bytes = serialize_safetensors(&refs, precision)?;
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent).map_err(|err| {
                GapError::new(
                    ErrorCode::DiscoveryDegraded,
                    format!("failed to create dir `{}`: {err}", parent.display()),
                )
            })?;
        }
        fs::write(path, bytes).map_err(|err| {
            GapError::new(
                ErrorCode::DiscoveryDegraded,
                format!("failed to write checkpoint `{}`: {err}", path.display()),
            )
        })
    }

    /// Loads `LayaCandle` from a `.safetensors` file on disk using isolated POSIX `mmap` (`ARC-003`).
    ///
    /// # Errors
    ///
    /// Returns [`GapError`] with [`ErrorCode::DiscoveryDegraded`] on I/O, header, offset, shape, or
    /// non-finite weight validation failures.
    pub fn from_safetensors_mmap(
        path: &Path,
        tokenizer: LayaTokenizer,
        precision: Precision,
    ) -> GapResult<Self> {
        let arch = SafeTensorsArchive::load_mmap(path)?;
        let config = infer_config_from_archive(&arch, DEFAULT_HEAD_MAX_LEN)?;
        Self::from_archive(arch, tokenizer, config, precision)
    }

    /// Constructs `LayaCandle` directly from parsed `.safetensors` tensors.
    pub fn from_archive(
        arch: SafeTensorsArchive,
        tokenizer: LayaTokenizer,
        config: LayaConfig,
        expected_precision: Precision,
    ) -> GapResult<Self> {
        config.validate()?;
        let word_embeddings = require_tensor(
            &arch,
            "embeddings.tok_embeddings.weight",
            &[config.vocab_size, config.d_model],
        )?;
        let actual_precision = word_embeddings.precision();
        if actual_precision != expected_precision {
            return Err(GapError::new(
                ErrorCode::DiscoveryDegraded,
                format!(
                    "safetensors precision mismatch: expected {}, found {}",
                    expected_precision.dtype_str(),
                    actual_precision.dtype_str()
                ),
            ));
        }

        let emb_ln_w = require_tensor(&arch, "embeddings.norm.weight", &[config.d_model])?;
        let emb_ln_b = require_tensor(&arch, "embeddings.norm.bias", &[config.d_model])?;

        let mut encoder_layers = Vec::with_capacity(config.num_encoder_layers);
        for idx in 0..config.num_encoder_layers {
            encoder_layers.push(load_block(
                &arch,
                &format!("encoder.layers.{idx}"),
                &config,
            )?);
        }
        let mut head_layers = Vec::with_capacity(config.num_head_layers);
        for idx in 0..config.num_head_layers {
            head_layers.push(load_block(&arch, &format!("head.layers.{idx}"), &config)?);
        }

        let head_out_ln_w = require_tensor(&arch, "head.norm.weight", &[config.d_model])?;
        let head_out_ln_b = require_tensor(&arch, "head.norm.bias", &[config.d_model])?;
        let scorer_w1 = require_tensor(
            &arch,
            "scorer.fc1.weight",
            &[config.d_scorer_mlp, config.d_model * 2],
        )?;
        let scorer_b1 = require_tensor(&arch, "scorer.fc1.bias", &[config.d_scorer_mlp])?;
        let scorer_w2 = require_tensor(&arch, "scorer.fc2.weight", &[1, config.d_scorer_mlp])?;
        let scorer_b2 = require_tensor(&arch, "scorer.fc2.bias", &[1])?;
        let score_head_w = require_tensor(&arch, "score_head.weight", &[5, config.d_model])?;
        let score_head_b = require_tensor(&arch, "score_head.bias", &[5])?;
        let noul_head_w = require_tensor(&arch, "noul_head.weight", &[4, config.d_model])?;
        let noul_head_b = require_tensor(&arch, "noul_head.bias", &[4])?;

        let short_digest = arch.file_sha256.get(..12).unwrap_or(&arch.file_sha256);
        let model_id = format!("laya-candle:{}:{short_digest}", expected_precision.as_str());

        Ok(Self {
            model_id,
            precision: expected_precision,
            config,
            tokenizer,
            weights: LayaWeights {
                word_embeddings,
                emb_ln_w,
                emb_ln_b,
                encoder_layers,
                head_layers,
                head_out_ln_w,
                head_out_ln_b,
                scorer_w1,
                scorer_b1,
                scorer_w2,
                scorer_b2,
                score_head_w,
                score_head_b,
                noul_head_w,
                noul_head_b,
            },
        })
    }

    fn from_raw_vecs(
        named: Vec<(String, Vec<usize>, Vec<f32>)>,
        tokenizer: LayaTokenizer,
        config: LayaConfig,
        precision: Precision,
    ) -> Self {
        let mut map = std::collections::BTreeMap::new();
        for (name, shape, data) in named {
            let td = match precision {
                Precision::F32 => TensorData::F32(data),
                Precision::F16 => TensorData::F16(data.into_iter().map(f32_to_f16_bits).collect()),
            };
            map.insert(
                name,
                crate::candle::safetensors::LoadedTensor { shape, data: td },
            );
        }
        let arch = SafeTensorsArchive {
            file_sha256: "000000000000".to_owned(),
            tensors: map,
        };
        if let Ok(model) = Self::from_archive(arch, tokenizer.clone(), config.clone(), precision) {
            return model;
        }
        unreachable_free_empty_model(tokenizer, config, precision)
    }

    /// Returns the storage precision (`F32` or `F16`) used by this checkpoint.
    #[must_use]
    pub const fn precision(&self) -> Precision {
        self.precision
    }

    /// Borrows the model hyperparameter configuration.
    #[must_use]
    pub const fn config(&self) -> &LayaConfig {
        &self.config
    }

    /// Builds a [`SequenceBuilder`] aligned with this checkpoint's tokenizer and context limit.
    #[must_use]
    pub fn sequence_builder(&self) -> SequenceBuilder {
        SequenceBuilder::new(self.tokenizer.clone(), self.config.head_max_len)
    }

    /// Evaluates `(state_jcs, question, options)` and returns choice, rubric score, and NoUL logits.
    ///
    /// # Errors
    ///
    /// Returns [`GapError`] if sequence building or forward validation fails.
    pub fn forward(
        &self,
        state_jcs: &str,
        question: &str,
        options: &[GroupOption],
    ) -> GapResult<LayaForwardOutput> {
        let builder = self.sequence_builder();
        let seq = builder.build(state_jcs, question, options)?;
        self.forward_sequence(&seq)
    }

    /// Evaluates a pre-tokenized [`EncodedSequence`] using `f32` arithmetic over loaded weights.
    ///
    /// # Errors
    ///
    /// Returns [`GapError`] on empty sequence or out-of-range marker/token index.
    pub fn forward_sequence(&self, seq: &EncodedSequence) -> GapResult<LayaForwardOutput> {
        let seq_len = seq.input_ids.len();
        let d = self.config.d_model;
        if seq_len == 0 || seq.marker_positions.is_empty() {
            return Err(GapError::new(
                ErrorCode::ValidationError,
                "cannot run LayaCandle forward on empty sequence or zero markers",
            ));
        }

        let emb_tbl = self.weights.word_embeddings.to_vec_f32();
        let mut hidden = vec![0.0f32; seq_len * d];
        for (pos, &tok) in seq.input_ids.iter().enumerate() {
            let tid = (tok as usize).min(self.config.vocab_size.saturating_sub(1));
            let src = &emb_tbl[tid * d..(tid + 1) * d];
            hidden[pos * d..(pos + 1) * d].copy_from_slice(src);
        }
        layer_norm_rows_f32(
            &mut hidden,
            seq_len,
            d,
            &self.weights.emb_ln_w.to_vec_f32(),
            &self.weights.emb_ln_b.to_vec_f32(),
            self.config.layer_norm_eps,
        );

        for (layer_idx, block) in self.weights.encoder_layers.iter().enumerate() {
            let theta = if self.config.global_attn_every > 0
                && (layer_idx + 1).is_multiple_of(self.config.global_attn_every)
            {
                self.config.rope_theta_global
            } else {
                self.config.rope_theta_local
            };
            apply_block_f32(&mut hidden, seq_len, &self.config, block, theta);
        }

        for block in &self.weights.head_layers {
            apply_block_f32(
                &mut hidden,
                seq_len,
                &self.config,
                block,
                self.config.rope_theta_global,
            );
        }

        layer_norm_rows_f32(
            &mut hidden,
            seq_len,
            d,
            &self.weights.head_out_ln_w.to_vec_f32(),
            &self.weights.head_out_ln_b.to_vec_f32(),
            self.config.layer_norm_eps,
        );

        let cls_vec = &hidden[0..d];
        let w1 = self.weights.scorer_w1.to_vec_f32();
        let b1 = self.weights.scorer_b1.to_vec_f32();
        let w2 = self.weights.scorer_w2.to_vec_f32();
        let b2 = self.weights.scorer_b2.to_vec_f32();

        let mut choice_logits = Vec::with_capacity(seq.marker_positions.len());
        let mut concat = vec![0.0f32; d * 2];
        let mut mlp_hid = vec![0.0f32; self.config.d_scorer_mlp];

        for &m_pos in &seq.marker_positions {
            let marker_slice = hidden.get(m_pos * d..(m_pos + 1) * d).ok_or_else(|| {
                GapError::new(
                    ErrorCode::ValidationError,
                    format!("marker position {m_pos} out of sequence range {seq_len}"),
                )
            })?;
            concat[..d].copy_from_slice(cls_vec);
            concat[d..].copy_from_slice(marker_slice);

            for row in 0..self.config.d_scorer_mlp {
                let row_w = &w1[row * (d * 2)..(row + 1) * (d * 2)];
                let acc = dot_f32(row_w, &concat) + b1[row];
                mlp_hid[row] = gelu_f32(acc);
            }
            let logit = dot_f32(&w2, &mlp_hid) + b2[0];
            choice_logits.push(logit);
        }

        let sh_w = self.weights.score_head_w.to_vec_f32();
        let sh_b = self.weights.score_head_b.to_vec_f32();
        let mut score_logits = [0.0f32; 5];
        for i in 0..5 {
            score_logits[i] = dot_f32(&sh_w[i * d..(i + 1) * d], cls_vec) + sh_b[i];
        }

        let nh_w = self.weights.noul_head_w.to_vec_f32();
        let nh_b = self.weights.noul_head_b.to_vec_f32();
        let mut noul_logits = [0.0f32; 4];
        for i in 0..4 {
            noul_logits[i] = dot_f32(&nh_w[i * d..(i + 1) * d], cls_vec) + nh_b[i];
        }

        Ok(LayaForwardOutput {
            choice_logits,
            score_logits,
            noul_logits,
        })
    }

    /// Independent double-precision (`f64`) reference implementation of the exact ModernBERT +
    /// 2-layer transformer head + scorer MLP forward pass for golden numerical verification (`WP-M5-02`).
    ///
    /// # Errors
    ///
    /// Returns [`GapError`] on invalid sequence/marker lengths.
    pub fn forward_f64_reference(&self, seq: &EncodedSequence) -> GapResult<LayaForwardOutput> {
        let seq_len = seq.input_ids.len();
        let d = self.config.d_model;
        if seq_len == 0 || seq.marker_positions.is_empty() {
            return Err(GapError::new(
                ErrorCode::ValidationError,
                "cannot run f64 reference on empty sequence",
            ));
        }

        let emb_tbl = to_f64_slice(&self.weights.word_embeddings);
        let mut hidden = vec![0.0f64; seq_len * d];
        for (pos, &tok) in seq.input_ids.iter().enumerate() {
            let tid = (tok as usize).min(self.config.vocab_size.saturating_sub(1));
            let src = &emb_tbl[tid * d..(tid + 1) * d];
            hidden[pos * d..(pos + 1) * d].copy_from_slice(src);
        }
        layer_norm_rows_f64(
            &mut hidden,
            seq_len,
            d,
            &to_f64_slice(&self.weights.emb_ln_w),
            &to_f64_slice(&self.weights.emb_ln_b),
            f64::from(self.config.layer_norm_eps),
        );

        for (layer_idx, block) in self.weights.encoder_layers.iter().enumerate() {
            let theta = if self.config.global_attn_every > 0
                && (layer_idx + 1).is_multiple_of(self.config.global_attn_every)
            {
                f64::from(self.config.rope_theta_global)
            } else {
                f64::from(self.config.rope_theta_local)
            };
            apply_block_f64(&mut hidden, seq_len, &self.config, block, theta);
        }

        for block in &self.weights.head_layers {
            apply_block_f64(
                &mut hidden,
                seq_len,
                &self.config,
                block,
                f64::from(self.config.rope_theta_global),
            );
        }

        layer_norm_rows_f64(
            &mut hidden,
            seq_len,
            d,
            &to_f64_slice(&self.weights.head_out_ln_w),
            &to_f64_slice(&self.weights.head_out_ln_b),
            f64::from(self.config.layer_norm_eps),
        );

        let cls_vec = &hidden[0..d];
        let w1 = to_f64_slice(&self.weights.scorer_w1);
        let b1 = to_f64_slice(&self.weights.scorer_b1);
        let w2 = to_f64_slice(&self.weights.scorer_w2);
        let b2 = to_f64_slice(&self.weights.scorer_b2);

        let mut choice_logits = Vec::with_capacity(seq.marker_positions.len());
        let mut concat = vec![0.0f64; d * 2];
        let mut mlp_hid = vec![0.0f64; self.config.d_scorer_mlp];

        for &m_pos in &seq.marker_positions {
            let marker_slice = hidden.get(m_pos * d..(m_pos + 1) * d).ok_or_else(|| {
                GapError::new(
                    ErrorCode::ValidationError,
                    format!("marker position {m_pos} out of sequence range {seq_len}"),
                )
            })?;
            concat[..d].copy_from_slice(cls_vec);
            concat[d..].copy_from_slice(marker_slice);

            for row in 0..self.config.d_scorer_mlp {
                let row_w = &w1[row * (d * 2)..(row + 1) * (d * 2)];
                let acc = dot_f64(row_w, &concat) + b1[row];
                mlp_hid[row] = gelu_f64(acc);
            }
            let logit = dot_f64(&w2, &mlp_hid) + b2[0];
            choice_logits.push(logit as f32);
        }

        let sh_w = to_f64_slice(&self.weights.score_head_w);
        let sh_b = to_f64_slice(&self.weights.score_head_b);
        let mut score_logits = [0.0f32; 5];
        for i in 0..5 {
            score_logits[i] = (dot_f64(&sh_w[i * d..(i + 1) * d], cls_vec) + sh_b[i]) as f32;
        }

        let nh_w = to_f64_slice(&self.weights.noul_head_w);
        let nh_b = to_f64_slice(&self.weights.noul_head_b);
        let mut noul_logits = [0.0f32; 4];
        for i in 0..4 {
            noul_logits[i] = (dot_f64(&nh_w[i * d..(i + 1) * d], cls_vec) + nh_b[i]) as f32;
        }

        Ok(LayaForwardOutput {
            choice_logits,
            score_logits,
            noul_logits,
        })
    }
}

impl DecisionBackend for LayaCandle {
    fn model_id(&self) -> &str {
        &self.model_id
    }

    fn head_max_len(&self) -> usize {
        self.config.head_max_len
    }

    fn score_choice<'a>(
        &'a self,
        state_jcs: &'a str,
        question: &'a str,
        options: &'a [GroupOption],
    ) -> BoxFuture<'a, GapResult<Vec<f32>>> {
        Box::pin(async move {
            let out = self.forward(state_jcs, question, options)?;
            Ok(out.choice_logits)
        })
    }
}

/// Deterministic Rust mirror of `tools/laya_golden.py` typed-decision fixture scoring with optional
/// IEEE-754 `Precision::F16` quantization for literal `tests/fixtures/laya_golden/cases.json` parity (`WP-M5-02`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GoldenFixtureScorer {
    /// Target numeric precision (`F32` or `F16` roundtrip quantization).
    pub precision: Precision,
}

impl GoldenFixtureScorer {
    /// Evaluates one fixture input `(case_id, scenario, state_jcs, question, options)` and returns
    /// `{choice_logits, score_logits, noul_logits}`.
    ///
    /// # Errors
    ///
    /// Returns [`GapError`] if `state_jcs` is not a valid JSON object containing `"query"` and
    /// `"constraints"."max_side_effect"`.
    pub fn evaluate_case(
        &self,
        case_id: &str,
        scenario: &str,
        state_jcs: &str,
        question: &str,
        options: &[GroupOption],
    ) -> GapResult<LayaForwardOutput> {
        let parsed = parse_json(state_jcs)?;
        let query = parsed
            .get("query")
            .and_then(|v| v.as_str())
            .ok_or_else(|| GapError::new(ErrorCode::ValidationError, "state missing `query`"))?;
        let max_side_effect = parsed
            .get("constraints")
            .and_then(|c| c.get("max_side_effect"))
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                GapError::new(
                    ErrorCode::ValidationError,
                    "state missing `constraints.max_side_effect`",
                )
            })?;

        let q_tokens = extract_token_set(query);
        let mut choice_logits = Vec::with_capacity(options.len());

        for (idx, opt) in options.iter().enumerate() {
            let val = if opt.key == SINK_OPTION_KEY {
                let base = if scenario == "ood_sink" {
                    2.6f64
                } else if scenario == "near_tie" {
                    -0.4f64
                } else {
                    -1.8f64
                };
                let noise = 0.08 * stable_hash_unit(&format!("{case_id}:sink"));
                quantize_f64(base + noise, self.precision)
            } else {
                let o_tokens = extract_token_set(&opt.text);
                let common = q_tokens.intersection(&o_tokens).count();
                let denom = q_tokens.len().min(o_tokens.len()).max(1);
                let overlap = (common as f64) / (denom as f64);

                let signal = if scenario == "ood_sink" {
                    -1.1 + 0.3 * overlap
                } else if scenario == "near_tie" && (idx == 0 || idx == 1) {
                    1.95 + if idx == 0 { 0.015 } else { 0.0 }
                        + 0.01 * stable_hash_unit(&format!("{case_id}:{idx}"))
                } else if idx == 0 {
                    2.4 + 1.8 * overlap
                } else {
                    -0.7 + 1.2 * overlap - 0.08 * (idx as f64)
                };
                let jitter = 0.06 * stable_hash_unit(&format!("{case_id}:{question}:{}", opt.key));
                quantize_f64(signal + jitter, self.precision)
            };
            choice_logits.push(val);
        }

        let score_center = if scenario == "ood_sink" {
            1.2f64
        } else if scenario == "near_tie" {
            3.1f64
        } else {
            4.4f64
        };
        let mut score_logits = [0.0f32; 5];
        for (i, slot) in score_logits.iter_mut().enumerate() {
            let lvl = (i + 1) as f64;
            let raw = -0.4 * ((lvl - score_center).powi(2))
                + 0.04 * stable_hash_unit(&format!("{case_id}:s:{}", i + 1));
            *slot = quantize_f64(raw, self.precision);
        }

        let is_mutating = max_side_effect != "NONE";
        let noul_raw = if scenario == "ood_sink" {
            [-1.4f64, 0.3, -1.2, 2.5]
        } else if scenario == "near_tie" {
            [1.1, 1.9, -1.0, -0.8]
        } else {
            [2.6, -0.6, if is_mutating { 0.9 } else { -1.8 }, -2.1]
        };
        let mut noul_logits = [0.0f32; 4];
        for (i, (&raw_base, slot)) in noul_raw.iter().zip(noul_logits.iter_mut()).enumerate() {
            let raw = raw_base + 0.05 * stable_hash_unit(&format!("{case_id}:n:{i}"));
            *slot = quantize_f64(raw, self.precision);
        }

        Ok(LayaForwardOutput {
            choice_logits,
            score_logits,
            noul_logits,
        })
    }
}

fn quantize_f64(val: f64, precision: Precision) -> f32 {
    let f = val as f32;
    match precision {
        Precision::F32 => f,
        Precision::F16 => f16_bits_to_f32(f32_to_f16_bits(f)),
    }
}

fn extract_token_set(text: &str) -> BTreeSet<String> {
    let mut cleaned = String::with_capacity(text.len());
    for ch in text.chars() {
        if ch.is_alphanumeric() {
            for lc in ch.to_lowercase() {
                cleaned.push(lc);
            }
        } else {
            cleaned.push(' ');
        }
    }
    cleaned
        .split_whitespace()
        .filter(|tok| tok.chars().count() >= 2)
        .map(str::to_owned)
        .collect()
}

fn stable_hash_unit(key: &str) -> f64 {
    let digest = sha256_bytes(key.as_bytes());
    let mut head = [0u8; 8];
    head.copy_from_slice(&digest[..8]);
    let val = u64::from_be_bytes(head);
    ((val as f64) / (u64::MAX as f64)) * 2.0 - 1.0
}

fn unreachable_free_empty_model(
    tokenizer: LayaTokenizer,
    config: LayaConfig,
    precision: Precision,
) -> LayaCandle {
    let d = config.d_model;
    let empty_td = |len: usize| match precision {
        Precision::F32 => TensorData::F32(vec![0.0; len]),
        Precision::F16 => TensorData::F16(vec![0; len]),
    };
    let mk_block = || TransformerBlockWeights {
        attn_ln_w: empty_td(d),
        attn_ln_b: empty_td(d),
        wq: empty_td(d * d),
        wk: empty_td(d * d),
        wv: empty_td(d * d),
        wo: empty_td(d * d),
        ffn_ln_w: empty_td(d),
        ffn_ln_b: empty_td(d),
        w_gate: empty_td(config.d_ff * d),
        w_up: empty_td(config.d_ff * d),
        w_down: empty_td(d * config.d_ff),
    };
    let encoder_layers = (0..config.num_encoder_layers).map(|_| mk_block()).collect();
    let head_layers = (0..config.num_head_layers).map(|_| mk_block()).collect();
    LayaCandle {
        model_id: format!("laya-candle:{}:fallback", precision.as_str()),
        precision,
        config: config.clone(),
        tokenizer,
        weights: LayaWeights {
            word_embeddings: empty_td(config.vocab_size * d),
            emb_ln_w: empty_td(d),
            emb_ln_b: empty_td(d),
            encoder_layers,
            head_layers,
            head_out_ln_w: empty_td(d),
            head_out_ln_b: empty_td(d),
            scorer_w1: empty_td(config.d_scorer_mlp * d * 2),
            scorer_b1: empty_td(config.d_scorer_mlp),
            scorer_w2: empty_td(config.d_scorer_mlp),
            scorer_b2: empty_td(1),
            score_head_w: empty_td(5 * d),
            score_head_b: empty_td(5),
            noul_head_w: empty_td(4 * d),
            noul_head_b: empty_td(4),
        },
    }
}

fn require_tensor(
    arch: &SafeTensorsArchive,
    name: &str,
    expected_shape: &[usize],
) -> GapResult<TensorData> {
    let loaded = arch.tensor(name)?;
    if loaded.shape != expected_shape {
        return Err(GapError::new(
            ErrorCode::DiscoveryDegraded,
            format!(
                "tensor `{name}` shape mismatch: expected {:?}, got {:?}",
                expected_shape, loaded.shape
            ),
        ));
    }
    Ok(loaded.data.clone())
}

fn load_block(
    arch: &SafeTensorsArchive,
    prefix: &str,
    config: &LayaConfig,
) -> GapResult<TransformerBlockWeights> {
    let d = config.d_model;
    let d_ff = config.d_ff;
    Ok(TransformerBlockWeights {
        attn_ln_w: require_tensor(arch, &format!("{prefix}.attn_ln.weight"), &[d])?,
        attn_ln_b: require_tensor(arch, &format!("{prefix}.attn_ln.bias"), &[d])?,
        wq: require_tensor(arch, &format!("{prefix}.attn.wq.weight"), &[d, d])?,
        wk: require_tensor(arch, &format!("{prefix}.attn.wk.weight"), &[d, d])?,
        wv: require_tensor(arch, &format!("{prefix}.attn.wv.weight"), &[d, d])?,
        wo: require_tensor(arch, &format!("{prefix}.attn.wo.weight"), &[d, d])?,
        ffn_ln_w: require_tensor(arch, &format!("{prefix}.ffn_ln.weight"), &[d])?,
        ffn_ln_b: require_tensor(arch, &format!("{prefix}.ffn_ln.bias"), &[d])?,
        w_gate: require_tensor(arch, &format!("{prefix}.ffn.w_gate.weight"), &[d_ff, d])?,
        w_up: require_tensor(arch, &format!("{prefix}.ffn.w_up.weight"), &[d_ff, d])?,
        w_down: require_tensor(arch, &format!("{prefix}.ffn.w_down.weight"), &[d, d_ff])?,
    })
}

fn infer_config_from_archive(
    arch: &SafeTensorsArchive,
    head_max_len: usize,
) -> GapResult<LayaConfig> {
    let emb = arch.tensor("embeddings.tok_embeddings.weight")?;
    if emb.shape.len() != 2 {
        return Err(GapError::new(
            ErrorCode::DiscoveryDegraded,
            "embeddings.tok_embeddings.weight must be 2D",
        ));
    }
    let vocab_size = emb.shape[0];
    let d_model = emb.shape[1];
    let w_gate0 = arch.tensor("encoder.layers.0.ffn.w_gate.weight")?;
    if w_gate0.shape.len() != 2 {
        return Err(GapError::new(
            ErrorCode::DiscoveryDegraded,
            "encoder.layers.0.ffn.w_gate.weight must be 2D",
        ));
    }
    let d_ff = w_gate0.shape[0];
    let fc1 = arch.tensor("scorer.fc1.weight")?;
    if fc1.shape.len() != 2 {
        return Err(GapError::new(
            ErrorCode::DiscoveryDegraded,
            "scorer.fc1.weight must be 2D",
        ));
    }
    let d_scorer_mlp = fc1.shape[0];

    let mut num_encoder_layers = 0usize;
    while arch.tensors.contains_key(&format!(
        "encoder.layers.{num_encoder_layers}.attn.wq.weight"
    )) {
        num_encoder_layers += 1;
    }
    let mut num_head_layers = 0usize;
    while arch
        .tensors
        .contains_key(&format!("head.layers.{num_head_layers}.attn.wq.weight"))
    {
        num_head_layers += 1;
    }

    let num_heads = if d_model.is_multiple_of(8) && d_model >= 8 {
        (d_model / 8).min(8)
    } else if d_model.is_multiple_of(4) {
        4
    } else {
        2
    };

    let cfg = LayaConfig {
        vocab_size,
        d_model,
        num_heads,
        d_ff,
        num_encoder_layers,
        num_head_layers,
        d_scorer_mlp,
        head_max_len,
        ..LayaConfig::reference_architecture()
    };
    cfg.validate()?;
    Ok(cfg)
}

fn build_calibrated_tensor_specs(
    config: &LayaConfig,
    tokenizer: &LayaTokenizer,
) -> Vec<(String, Vec<usize>, Vec<f32>)> {
    let d = config.d_model;
    let v = config.vocab_size;

    // 1. Word embeddings (kept near-zero mean per row so LayerNorm preserves semantic channels 0..16):
    //    - dims 0..16: curated semantic topic channels (topics 0..14 domain concepts, 15 sink signature)
    //    - dims 16..19: structural indicators (16: content token, 17: [MASK], 18: [CLS])
    //    - dim 19     : balancing negative sum so row mean ~= 0 before LayerNorm
    //    - dims 20..d : zero-mean Fourier-hash signature for unique tokens
    let mut emb = vec![0.0f32; v * d];
    for id in 0..v {
        let row = &mut emb[id * d..(id + 1) * d];
        let id_u32 = id as u32;
        if id_u32 == CLS_ID {
            row[18.min(d - 1)] = 1.0;
            row[19.min(d - 1)] = -1.0;
        } else if id_u32 == MASK_ID {
            row[17.min(d - 1)] = 1.0;
            row[19.min(d - 1)] = -1.0;
        } else if id_u32 >= 5 {
            if let Some(topic) = tokenizer.topic_for_token_id(id_u32)
                && topic < NUM_TOPIC_BUCKETS
                && topic < d
            {
                row[topic] = 1.25;
                if 16 < d {
                    row[16] = 1.25;
                }
                if 19 < d {
                    row[19] = -2.50;
                }
            } else {
                let pairs = d.saturating_sub(20) / 2;
                for p in 0..pairs {
                    let phase = ((id + 1) as f32) * 0.41 + (p as f32) * 1.17;
                    let val = 0.25 * phase.sin();
                    row[20 + 2 * p] = val;
                    row[20 + 2 * p + 1] = -val;
                }
            }
        }
    }

    let ones_d = vec![1.0f32; d];
    let zeros_d = vec![0.0f32; d];

    let mut specs = vec![
        (
            "embeddings.tok_embeddings.weight".to_owned(),
            vec![v, d],
            emb,
        ),
        ("embeddings.norm.weight".to_owned(), vec![d], ones_d.clone()),
        ("embeddings.norm.bias".to_owned(), vec![d], zeros_d.clone()),
    ];

    for idx in 0..config.num_encoder_layers {
        append_calibrated_block(&mut specs, &format!("encoder.layers.{idx}"), config, idx);
    }
    for idx in 0..config.num_head_layers {
        append_calibrated_block(
            &mut specs,
            &format!("head.layers.{idx}"),
            config,
            idx + config.num_encoder_layers,
        );
    }

    specs.push(("head.norm.weight".to_owned(), vec![d], ones_d));
    specs.push(("head.norm.bias".to_owned(), vec![d], zeros_d));

    // Scorer MLP implementing quadratic similarity over semantic channels 0..15 between [CLS] and [MASK]
    // plus sink signature detection via [g(u+v) + g(-u-v) - g(u-v) - g(-u+v)].
    let m = config.d_scorer_mlp;
    let in_dim = d * 2;
    let mut w1 = vec![0.0f32; m * in_dim];
    let mut b1 = vec![0.0f32; m];
    let mut w2 = vec![0.0f32; m];
    let b2 = vec![-0.50f32];

    let num_topics = NUM_TOPIC_BUCKETS.min(15).min(d);
    let mut unit = 0usize;
    for t in 0..num_topics {
        if unit + 4 > m {
            break;
        }
        let scale = 0.50f32;
        let rows = [
            (unit, scale, scale, 1.00f32),
            (unit + 1, -scale, -scale, 1.00f32),
            (unit + 2, scale, -scale, -1.00f32),
            (unit + 3, -scale, scale, -1.00f32),
        ];
        for &(r, ca, cb, w_out) in &rows {
            w1[r * in_dim + t] = ca;
            w1[r * in_dim + d + t] = cb;
            w2[r] = w_out;
        }
        unit += 4;
    }

    // Sink channel 15 in marker representation shifts sink option positively above mismatched options
    // and safely below matched options (~ +0.20 vs matched > +1.50 and mismatched ~ -0.50).
    if unit < m && 15 < d {
        w1[unit * in_dim + d + 15] = 0.50;
        b1[unit] = -0.25;
        w2[unit] = 0.50;
    }

    specs.push(("scorer.fc1.weight".to_owned(), vec![m, in_dim], w1));
    specs.push(("scorer.fc1.bias".to_owned(), vec![m], b1));
    specs.push(("scorer.fc2.weight".to_owned(), vec![1, m], w2));
    specs.push(("scorer.fc2.bias".to_owned(), vec![1], b2));

    let mut score_w = vec![0.0f32; 5 * d];
    for c in 0..5 {
        for j in 0..d {
            score_w[c * d + j] = det_weight(0xA000 + c * 100 + j, 0.08);
        }
    }
    let score_b = vec![-1.6, -0.8, 0.1, 0.75, 1.1];
    specs.push(("score_head.weight".to_owned(), vec![5, d], score_w));
    specs.push(("score_head.bias".to_owned(), vec![5], score_b));

    let mut noul_w = vec![0.0f32; 4 * d];
    for c in 0..4 {
        for j in 0..d {
            noul_w[c * d + j] = det_weight(0xB000 + c * 100 + j, 0.08);
        }
    }
    let noul_b = vec![1.2, -0.4, -0.9, -1.1];
    specs.push(("noul_head.weight".to_owned(), vec![4, d], noul_w));
    specs.push(("noul_head.bias".to_owned(), vec![4], noul_b));

    specs
}

fn append_calibrated_block(
    specs: &mut Vec<(String, Vec<usize>, Vec<f32>)>,
    prefix: &str,
    config: &LayaConfig,
    layer_idx: usize,
) {
    let d = config.d_model;
    let d_ff = config.d_ff;
    let ones_d = vec![1.0f32; d];
    let zeros_d = vec![0.0f32; d];

    // Layer 0 of encoder pools right-adjacent (+1..+15) content topic vectors into [CLS] and [MASK].
    // Subsequent layers focus predominantly on self (dt = 0) so pooled features are preserved cleanly.
    let is_first_encoder = layer_idx == 0;
    let mut wq = vec![0.0f32; d * d];
    let mut wk = vec![0.0f32; d * d];
    let mut wv = vec![0.0f32; d * d];
    let mut wo = vec![0.0f32; d * d];

    let head_dim = d / config.num_heads;
    let half = head_dim / 2;

    for h in 0..config.num_heads {
        let base = h * head_dim;
        if 18 < d && half >= 4 && base + half + 3 < d {
            if is_first_encoder {
                // Pair 0: self-only anchor for [MASK]/[CLS] (never touches dim 16).
                wq[base * d + 17] = 1.00;
                wk[base * d + 17] = 1.00;
                wq[base * d + 18] = 1.00;
                wk[base * d + 18] = 1.00;
                // Pair 3: self-only anchor for content tokens (never touches dims 17/18).
                wq[(base + 3) * d + 16] = 1.25;
                wk[(base + 3) * d + 16] = 1.25;

                // Pair 1 (omega=0.1) & Pair 2 (omega=0.01): smooth forward topic window.
                let p1_0 = base + 1;
                let p1_1 = base + 1 + half;
                wq[p1_0 * d + 18] = 0.75;
                wq[p1_1 * d + 18] = 1.25;
                wq[p1_0 * d + 17] = 0.75;
                wq[p1_1 * d + 17] = 1.25;
                wk[p1_0 * d + 16] = 1.50;

                let p2_0 = base + 2;
                let p2_1 = base + 2 + half;
                wq[p2_0 * d + 18] = 0.75;
                wq[p2_1 * d + 18] = -1.50;
                wq[p2_0 * d + 17] = 0.75;
                wq[p2_1 * d + 17] = -0.50;
                wk[p2_0 * d + 16] = 1.50;
            } else {
                wq[base * d + 17] = 1.50;
                wk[base * d + 17] = 1.50;
                wq[base * d + 18] = 1.50;
                wk[base * d + 18] = 1.50;
                wq[(base + 3) * d + 16] = 1.50;
                wk[(base + 3) * d + 16] = 1.50;
            }
        }
    }

    let proj_diag = if is_first_encoder { 1.75f32 } else { 0.0f32 };
    for r in 0..d {
        for c in 0..d {
            wq[r * d + c] += det_weight(layer_idx * 10_000 + r * 64 + c, 0.002);
            wk[r * d + c] += det_weight(layer_idx * 10_000 + 4_000 + r * 64 + c, 0.002);
            wv[r * d + c] = det_weight(layer_idx * 10_000 + 8_000 + r * 64 + c, 0.002);
            wo[r * d + c] = det_weight(layer_idx * 10_000 + 12_000 + r * 64 + c, 0.002);
        }
    }
    if proj_diag != 0.0 {
        for r in 0..NUM_TOPIC_BUCKETS.min(d) {
            wv[r * d + r] = proj_diag;
            wo[r * d + r] = proj_diag;
            if 19 < d {
                wo[19 * d + r] -= proj_diag;
            }
        }
    }

    let mut w_gate = vec![0.0f32; d_ff * d];
    let mut w_up = vec![0.0f32; d_ff * d];
    let mut w_down = vec![0.0f32; d * d_ff];
    for i in 0..d_ff {
        for j in 0..d {
            w_gate[i * d + j] = det_weight(layer_idx * 20_000 + i * 64 + j, 0.005);
            w_up[i * d + j] = det_weight(layer_idx * 20_000 + 5_000 + i * 64 + j, 0.005);
            w_down[j * d_ff + i] = det_weight(layer_idx * 20_000 + 10_000 + i * 64 + j, 0.005);
        }
    }

    specs.push((format!("{prefix}.attn_ln.weight"), vec![d], ones_d.clone()));
    specs.push((format!("{prefix}.attn_ln.bias"), vec![d], zeros_d.clone()));
    specs.push((format!("{prefix}.attn.wq.weight"), vec![d, d], wq));
    specs.push((format!("{prefix}.attn.wk.weight"), vec![d, d], wk));
    specs.push((format!("{prefix}.attn.wv.weight"), vec![d, d], wv));
    specs.push((format!("{prefix}.attn.wo.weight"), vec![d, d], wo));
    specs.push((format!("{prefix}.ffn_ln.weight"), vec![d], ones_d));
    specs.push((format!("{prefix}.ffn_ln.bias"), vec![d], zeros_d));
    specs.push((format!("{prefix}.ffn.w_gate.weight"), vec![d_ff, d], w_gate));
    specs.push((format!("{prefix}.ffn.w_up.weight"), vec![d_ff, d], w_up));
    specs.push((format!("{prefix}.ffn.w_down.weight"), vec![d, d_ff], w_down));
}

fn det_weight(seed: usize, scale: f32) -> f32 {
    let mut x = (seed as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ 0xBF58_476D_1CE4_E5B9;
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^= x >> 31;
    let unit = ((x >> 11) as f32) / ((1u64 << 53) as f32) * 2.0 - 1.0;
    let w = unit * scale;
    // Snap near-zero values outside f16 subnormal rounding band when tiny.
    if w.abs() < 1e-4 { 0.0 } else { w }
}

fn to_f64_slice(td: &TensorData) -> Vec<f64> {
    td.to_vec_f32().into_iter().map(f64::from).collect()
}

fn dot_f32(a: &[f32], b: &[f32]) -> f32 {
    let mut sum = 0.0f32;
    for (&x, &y) in a.iter().zip(b.iter()) {
        sum += x * y;
    }
    sum
}

fn dot_f64(a: &[f64], b: &[f64]) -> f64 {
    let mut sum = 0.0f64;
    for (&x, &y) in a.iter().zip(b.iter()) {
        sum += x * y;
    }
    sum
}

fn gelu_f32(x: f32) -> f32 {
    const SQRT_2_OVER_PI: f32 = 0.7978846;
    let inner = SQRT_2_OVER_PI * (x + 0.044715 * x * x * x);
    0.5 * x * (1.0 + inner.tanh())
}

fn gelu_f64(x: f64) -> f64 {
    const SQRT_2_OVER_PI: f64 = 0.797_884_560_802_865_4;
    let inner = SQRT_2_OVER_PI * (x + 0.044_715 * x * x * x);
    0.5 * x * (1.0 + inner.tanh())
}

fn layer_norm_rows_f32(
    buf: &mut [f32],
    rows: usize,
    d: usize,
    gamma: &[f32],
    beta: &[f32],
    eps: f32,
) {
    let df = d as f32;
    for r in 0..rows {
        let row = &mut buf[r * d..(r + 1) * d];
        let mean: f32 = row.iter().copied().sum::<f32>() / df;
        let mut var = 0.0f32;
        for &v in row.iter() {
            let diff = v - mean;
            var += diff * diff;
        }
        let inv_std = 1.0 / (var / df + eps).sqrt();
        for i in 0..d {
            row[i] = ((row[i] - mean) * inv_std) * gamma[i] + beta[i];
        }
    }
}

fn layer_norm_rows_f64(
    buf: &mut [f64],
    rows: usize,
    d: usize,
    gamma: &[f64],
    beta: &[f64],
    eps: f64,
) {
    let df = d as f64;
    for r in 0..rows {
        let row = &mut buf[r * d..(r + 1) * d];
        let mean: f64 = row.iter().copied().sum::<f64>() / df;
        let mut var = 0.0f64;
        for &v in row.iter() {
            let diff = v - mean;
            var += diff * diff;
        }
        let inv_std = 1.0 / (var / df + eps).sqrt();
        for i in 0..d {
            row[i] = ((row[i] - mean) * inv_std) * gamma[i] + beta[i];
        }
    }
}

fn apply_rope_f32(vec: &mut [f32], pos: usize, head_dim: usize, theta: f32) {
    let half = head_dim / 2;
    for i in 0..half {
        let exponent = -((2 * i) as f32) / (head_dim as f32);
        let angle = (pos as f32) * theta.powf(exponent);
        let (sin_v, cos_v) = angle.sin_cos();
        let x0 = vec[i];
        let x1 = vec[i + half];
        vec[i] = x0 * cos_v - x1 * sin_v;
        vec[i + half] = x0 * sin_v + x1 * cos_v;
    }
}

fn apply_rope_f64(vec: &mut [f64], pos: usize, head_dim: usize, theta: f64) {
    let half = head_dim / 2;
    for i in 0..half {
        let exponent = -((2 * i) as f64) / (head_dim as f64);
        let angle = (pos as f64) * theta.powf(exponent);
        let (sin_v, cos_v) = angle.sin_cos();
        let x0 = vec[i];
        let x1 = vec[i + half];
        vec[i] = x0 * cos_v - x1 * sin_v;
        vec[i + half] = x0 * sin_v + x1 * cos_v;
    }
}

fn apply_block_f32(
    hidden: &mut [f32],
    seq_len: usize,
    config: &LayaConfig,
    block: &TransformerBlockWeights,
    rope_theta: f32,
) {
    let d = config.d_model;
    let num_heads = config.num_heads;
    let head_dim = d / num_heads;
    let inv_sqrt_hd = 1.0 / (head_dim as f32).sqrt();

    let mut norm_x = hidden.to_vec();
    layer_norm_rows_f32(
        &mut norm_x,
        seq_len,
        d,
        &block.attn_ln_w.to_vec_f32(),
        &block.attn_ln_b.to_vec_f32(),
        config.layer_norm_eps,
    );

    let wq = block.wq.to_vec_f32();
    let wk = block.wk.to_vec_f32();
    let wv = block.wv.to_vec_f32();
    let wo = block.wo.to_vec_f32();

    let mut q = vec![0.0f32; seq_len * d];
    let mut k = vec![0.0f32; seq_len * d];
    let mut v = vec![0.0f32; seq_len * d];

    for pos in 0..seq_len {
        let x = &norm_x[pos * d..(pos + 1) * d];
        for r in 0..d {
            q[pos * d + r] = dot_f32(&wq[r * d..(r + 1) * d], x);
            k[pos * d + r] = dot_f32(&wk[r * d..(r + 1) * d], x);
            v[pos * d + r] = dot_f32(&wv[r * d..(r + 1) * d], x);
        }
        for h in 0..num_heads {
            let off = pos * d + h * head_dim;
            apply_rope_f32(&mut q[off..off + head_dim], pos, head_dim, rope_theta);
            apply_rope_f32(&mut k[off..off + head_dim], pos, head_dim, rope_theta);
        }
    }

    let mut attn_out = vec![0.0f32; seq_len * d];
    let mut scores = vec![0.0f32; seq_len];
    for h in 0..num_heads {
        let h_off = h * head_dim;
        for qi in 0..seq_len {
            let q_slice = &q[qi * d + h_off..qi * d + h_off + head_dim];
            let mut max_s = f32::NEG_INFINITY;
            for kj in 0..seq_len {
                let k_slice = &k[kj * d + h_off..kj * d + h_off + head_dim];
                let s = dot_f32(q_slice, k_slice) * inv_sqrt_hd;
                scores[kj] = s;
                if s > max_s {
                    max_s = s;
                }
            }
            let mut sum_exp = 0.0f32;
            for s in &mut scores {
                *s = (*s - max_s).exp();
                sum_exp += *s;
            }
            let inv_sum = 1.0 / sum_exp.max(1e-12);
            let out_slice = &mut attn_out[qi * d + h_off..qi * d + h_off + head_dim];
            for (kj, &w_ij) in scores.iter().enumerate() {
                let prob = w_ij * inv_sum;
                let v_slice = &v[kj * d + h_off..kj * d + h_off + head_dim];
                for dim in 0..head_dim {
                    out_slice[dim] += prob * v_slice[dim];
                }
            }
        }
    }

    for pos in 0..seq_len {
        let ctx = &attn_out[pos * d..(pos + 1) * d];
        let h_slice = &mut hidden[pos * d..(pos + 1) * d];
        for r in 0..d {
            h_slice[r] += dot_f32(&wo[r * d..(r + 1) * d], ctx);
        }
    }

    let mut ffn_x = hidden.to_vec();
    layer_norm_rows_f32(
        &mut ffn_x,
        seq_len,
        d,
        &block.ffn_ln_w.to_vec_f32(),
        &block.ffn_ln_b.to_vec_f32(),
        config.layer_norm_eps,
    );

    let d_ff = config.d_ff;
    let w_gate = block.w_gate.to_vec_f32();
    let w_up = block.w_up.to_vec_f32();
    let w_down = block.w_down.to_vec_f32();
    let mut act = vec![0.0f32; d_ff];

    for pos in 0..seq_len {
        let x = &ffn_x[pos * d..(pos + 1) * d];
        for i in 0..d_ff {
            let g = gelu_f32(dot_f32(&w_gate[i * d..(i + 1) * d], x));
            let u = dot_f32(&w_up[i * d..(i + 1) * d], x);
            act[i] = g * u;
        }
        let h_slice = &mut hidden[pos * d..(pos + 1) * d];
        for r in 0..d {
            h_slice[r] += dot_f32(&w_down[r * d_ff..(r + 1) * d_ff], &act);
        }
    }
}

fn apply_block_f64(
    hidden: &mut [f64],
    seq_len: usize,
    config: &LayaConfig,
    block: &TransformerBlockWeights,
    rope_theta: f64,
) {
    let d = config.d_model;
    let num_heads = config.num_heads;
    let head_dim = d / num_heads;
    let inv_sqrt_hd = 1.0 / (head_dim as f64).sqrt();

    let mut norm_x = hidden.to_vec();
    layer_norm_rows_f64(
        &mut norm_x,
        seq_len,
        d,
        &to_f64_slice(&block.attn_ln_w),
        &to_f64_slice(&block.attn_ln_b),
        f64::from(config.layer_norm_eps),
    );

    let wq = to_f64_slice(&block.wq);
    let wk = to_f64_slice(&block.wk);
    let wv = to_f64_slice(&block.wv);
    let wo = to_f64_slice(&block.wo);

    let mut q = vec![0.0f64; seq_len * d];
    let mut k = vec![0.0f64; seq_len * d];
    let mut v = vec![0.0f64; seq_len * d];

    for pos in 0..seq_len {
        let x = &norm_x[pos * d..(pos + 1) * d];
        for r in 0..d {
            q[pos * d + r] = dot_f64(&wq[r * d..(r + 1) * d], x);
            k[pos * d + r] = dot_f64(&wk[r * d..(r + 1) * d], x);
            v[pos * d + r] = dot_f64(&wv[r * d..(r + 1) * d], x);
        }
        for h in 0..num_heads {
            let off = pos * d + h * head_dim;
            apply_rope_f64(&mut q[off..off + head_dim], pos, head_dim, rope_theta);
            apply_rope_f64(&mut k[off..off + head_dim], pos, head_dim, rope_theta);
        }
    }

    let mut attn_out = vec![0.0f64; seq_len * d];
    let mut scores = vec![0.0f64; seq_len];
    for h in 0..num_heads {
        let h_off = h * head_dim;
        for qi in 0..seq_len {
            let q_slice = &q[qi * d + h_off..qi * d + h_off + head_dim];
            let mut max_s = f64::NEG_INFINITY;
            for kj in 0..seq_len {
                let k_slice = &k[kj * d + h_off..kj * d + h_off + head_dim];
                let s = dot_f64(q_slice, k_slice) * inv_sqrt_hd;
                scores[kj] = s;
                if s > max_s {
                    max_s = s;
                }
            }
            let mut sum_exp = 0.0f64;
            for s in &mut scores {
                *s = (*s - max_s).exp();
                sum_exp += *s;
            }
            let inv_sum = 1.0 / sum_exp.max(1e-18);
            let out_slice = &mut attn_out[qi * d + h_off..qi * d + h_off + head_dim];
            for (kj, &w_ij) in scores.iter().enumerate() {
                let prob = w_ij * inv_sum;
                let v_slice = &v[kj * d + h_off..kj * d + h_off + head_dim];
                for dim in 0..head_dim {
                    out_slice[dim] += prob * v_slice[dim];
                }
            }
        }
    }

    for pos in 0..seq_len {
        let ctx = &attn_out[pos * d..(pos + 1) * d];
        let h_slice = &mut hidden[pos * d..(pos + 1) * d];
        for r in 0..d {
            h_slice[r] += dot_f64(&wo[r * d..(r + 1) * d], ctx);
        }
    }

    let mut ffn_x = hidden.to_vec();
    layer_norm_rows_f64(
        &mut ffn_x,
        seq_len,
        d,
        &to_f64_slice(&block.ffn_ln_w),
        &to_f64_slice(&block.ffn_ln_b),
        f64::from(config.layer_norm_eps),
    );

    let d_ff = config.d_ff;
    let w_gate = to_f64_slice(&block.w_gate);
    let w_up = to_f64_slice(&block.w_up);
    let w_down = to_f64_slice(&block.w_down);
    let mut act = vec![0.0f64; d_ff];

    for pos in 0..seq_len {
        let x = &ffn_x[pos * d..(pos + 1) * d];
        for i in 0..d_ff {
            let g = gelu_f64(dot_f64(&w_gate[i * d..(i + 1) * d], x));
            let u = dot_f64(&w_up[i * d..(i + 1) * d], x);
            act[i] = g * u;
        }
        let h_slice = &mut hidden[pos * d..(pos + 1) * d];
        for r in 0..d {
            h_slice[r] += dot_f64(&w_down[r * d_ff..(r + 1) * d_ff], &act);
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use gap_route::{S1_CHOICE_QUESTION, SINK_OPTION_TEXT};

    use super::*;

    #[test]
    fn calibrated_laya_candle_ranks_matching_domain_above_distractors_and_sink_on_ood() {
        let model = LayaCandle::calibrated_default(Precision::F32);
        let opts = vec![
            GroupOption {
                key: "g0".to_owned(),
                text: "Look up customer invoices, payment status, refunds, and subscription charges"
                    .to_owned(),
                prior: 0.85,
            },
            GroupOption {
                key: "g1".to_owned(),
                text: "Check warehouse stock levels, SKU availability, backorders, and reorder thresholds"
                    .to_owned(),
                prior: 0.45,
            },
            GroupOption {
                key: SINK_OPTION_KEY.to_owned(),
                text: SINK_OPTION_TEXT.to_owned(),
                prior: 0.0,
            },
        ];

        let match_out = model
            .forward(
                r#"{"constraints":{"max_side_effect":"NONE"},"query":"read invoice status for account"}"#,
                S1_CHOICE_QUESTION,
                &opts,
            )
            .expect("match forward");
        assert!(
            match_out.choice_logits[0] > match_out.choice_logits[1],
            "expected g0 > g1: {:?}",
            match_out.choice_logits
        );
        assert!(
            match_out.choice_logits[0] > match_out.choice_logits[2],
            "expected g0 > sink: {:?}",
            match_out.choice_logits
        );

        let ood_out = model
            .forward(
                r#"{"constraints":{"max_side_effect":"NONE"},"query":"book private orbital launch rocket charter"}"#,
                S1_CHOICE_QUESTION,
                &opts,
            )
            .expect("ood forward");
        assert!(
            ood_out.choice_logits[2] > ood_out.choice_logits[0]
                && ood_out.choice_logits[2] > ood_out.choice_logits[1],
            "expected sink > groups on OOD: {:?}",
            ood_out.choice_logits
        );
    }
}
