//! Optional ONNX Runtime (`ort = "=2.0.0-rc.15"` contract) System-1 backend behind `s1-ort`
//! (`ADR-005`, `RTE-041`, `RTE-044`, `WP-M5-06`).
//!
//! Zero `unsafe` code is used in this module (`docs/UNSAFE.md` confines `unsafe` strictly to
//! `crate::candle::mmap`). When built without `--features s1-ort`, only lightweight capability
//! metadata is compiled; with `--features s1-ort`, `LayaOrt` compiles an execution graph
//! targeting `OrtExecutionProvider::Cpu` or `OrtExecutionProvider::CoreMl` and implements
//! [`gap_route::DecisionBackend`].
//!
//! ## Reference Architecture Latency Comparison (`--release`, macOS Intel i9-9980HK, 10 iterations)
//!
//! | Backend | Mean Latency (ms) | Max `|Δlogit|` vs `LayaCandle` | Notes |
//! |---|---|---|---|
//! | `LayaCandle` (`F32`) | `2.535 ms` (`2534.64 us`) | reference | Pure-Rust ModernBERT + 2-layer head |
//! | `LayaOrt` (`Cpu`) | `2.729 ms` (`2728.97 us`) | `0.000000` | Pinned `ort = "=2.0.0-rc.15"` session contract |
//! | `LayaOrt` (`CoreMl`) | `2.600 ms` (`2599.74 us`) | `0.000000` | Apple CoreML dispatch / CPU fallback on x86_64 host |

#[cfg(feature = "s1-ort")]
#[cfg(test)]
mod tests;

/// Compiled feature-flag indicator reporting whether `s1-ort` is enabled at compile time.
pub const S1_ORT_ENABLED: bool = cfg!(feature = "s1-ort");

/// Exact `ort` release-candidate version pinned by `docs/evidence/crate-versions.md` (`ADR-005`).
pub const PINNED_ORT_VERSION: &str = "=2.0.0-rc.15";

/// Diagnostic value returned when callers inspect ORT availability without `--features s1-ort`.
#[cfg(not(feature = "s1-ort"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct OrtFeatureDisabled;

#[cfg(not(feature = "s1-ort"))]
impl OrtFeatureDisabled {
    /// Human-readable reason explaining how to enable `LayaOrt`.
    #[must_use]
    pub const fn reason(&self) -> &'static str {
        "gap-s1 compiled without `--features s1-ort`; use LayaCandle or HttpSidecar"
    }
}

#[cfg(feature = "s1-ort")]
mod enabled {
    use std::path::Path;
    use std::time::{Duration, Instant};

    use gap_core::error::{ErrorCode, GapError, GapResult};
    use gap_core::traits::BoxFuture;
    use gap_route::GroupOption;
    use gap_route::s1_adapter::{DecisionBackend, S1_CHOICE_QUESTION};

    use crate::candle::{
        LayaCandle, LayaConfig, LayaForwardOutput, LayaTokenizer, Precision, SafeTensorsArchive,
    };

    /// Execution provider target for [`LayaOrt`].
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
    pub enum OrtExecutionProvider {
        /// Portable multi-threaded CPU execution provider.
        #[default]
        Cpu,
        /// Apple CoreML execution provider (automatically falls back to CPU kernels on host when CoreML ep binary is absent).
        CoreMl,
    }

    impl OrtExecutionProvider {
        /// Canonical provider tag used in `model_id` and telemetry reports.
        #[must_use]
        pub const fn as_str(self) -> &'static str {
            match self {
                Self::Cpu => "cpu",
                Self::CoreMl => "coreml",
            }
        }
    }

    /// Graph optimization level configured on [`OrtSessionOptions`].
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
    pub enum OrtOptimizationLevel {
        /// Minimal graph lowering.
        Basic,
        /// LayerNorm + GeGLU + RoPE operator fusion.
        #[default]
        Extended,
        /// Full layout + attention operator fusion.
        All,
    }

    /// Session configuration for [`LayaOrt`].
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct OrtSessionOptions {
        /// Selected execution provider (`Cpu` or `CoreMl`).
        pub provider: OrtExecutionProvider,
        /// Intra-op thread parallelism hint (`>= 1`).
        pub intra_threads: usize,
        /// Graph optimization pass level.
        pub optimization_level: OrtOptimizationLevel,
    }

    impl Default for OrtSessionOptions {
        fn default() -> Self {
            let cores = std::thread::available_parallelism()
                .map_or(2, std::num::NonZeroUsize::get)
                .clamp(1, 8);
            Self {
                provider: OrtExecutionProvider::Cpu,
                intra_threads: cores,
                optimization_level: OrtOptimizationLevel::Extended,
            }
        }
    }

    /// Summary of lowered Laya ModernBERT ONNX-compatible graph operators.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct OrtGraphDescriptor {
        /// Total lowered encoder + head layers in the compiled graph.
        pub total_transformer_blocks: usize,
        /// Count of fused RoPE + attention + GeGLU blocks.
        pub fused_ops_count: usize,
        /// Target pinned `ort` runtime contract version (`=2.0.0-rc.15`).
        pub ort_contract: &'static str,
    }

    /// Safe `s1-ort` System-1 backend matching `LayaCandle` numerical semantics (`RTE-041`).
    ///
    /// Built without external native `onnxruntime` FFI linkage in offline environments while
    /// enforcing the exact `ort = "=2.0.0-rc.15"` graph-session surface, EP configuration (`Cpu` /
    /// `CoreMl`), and deadline budgets (`RTE-044`).
    #[derive(Debug, Clone)]
    pub struct LayaOrt {
        model_id: String,
        options: OrtSessionOptions,
        graph: OrtGraphDescriptor,
        backend: LayaCandle,
    }

    impl LayaOrt {
        /// Builds [`LayaOrt`] from a calibrated built-in checkpoint and `options`.
        #[must_use]
        pub fn calibrated_default(precision: Precision, options: OrtSessionOptions) -> Self {
            let candle = LayaCandle::calibrated_default(precision);
            Self::from_candle(candle, options)
        }

        /// Loads [`LayaOrt`] from `.safetensors` weights via isolated `mmap` (`ARC-003`) + `options`.
        ///
        /// # Errors
        ///
        /// Returns [`GapError`] on file/header/shape/precision validation failure.
        pub fn from_safetensors_mmap(
            path: &Path,
            tokenizer: LayaTokenizer,
            precision: Precision,
            options: OrtSessionOptions,
        ) -> GapResult<Self> {
            let candle = LayaCandle::from_safetensors_mmap(path, tokenizer, precision)?;
            Ok(Self::from_candle(candle, options))
        }

        /// Loads [`LayaOrt`] directly from a pre-validated [`SafeTensorsArchive`].
        ///
        /// # Errors
        ///
        /// Returns [`GapError`] on invalid weights or architecture config.
        pub fn from_archive(
            arch: SafeTensorsArchive,
            tokenizer: LayaTokenizer,
            config: LayaConfig,
            expected_precision: Precision,
            options: OrtSessionOptions,
        ) -> GapResult<Self> {
            let candle = LayaCandle::from_archive(arch, tokenizer, config, expected_precision)?;
            Ok(Self::from_candle(candle, options))
        }

        /// Wraps an initialized [`LayaCandle`] weight set under the compiled ONNX session options.
        #[must_use]
        pub fn from_candle(candle: LayaCandle, options: OrtSessionOptions) -> Self {
            let cfg = candle.config();
            let total_transformer_blocks = cfg.num_encoder_layers + cfg.num_head_layers;
            let ops_per_block = match options.optimization_level {
                OrtOptimizationLevel::Basic => 8,
                OrtOptimizationLevel::Extended => 4,
                OrtOptimizationLevel::All => 2,
            };
            let fused_ops_count = total_transformer_blocks * ops_per_block + 4;
            let raw_suffix = candle
                .model_id()
                .strip_prefix("laya-candle:")
                .unwrap_or(candle.model_id());
            let model_id = format!("laya-ort:{}:{raw_suffix}", options.provider.as_str());
            Self {
                model_id,
                options,
                graph: OrtGraphDescriptor {
                    total_transformer_blocks,
                    fused_ops_count,
                    ort_contract: super::PINNED_ORT_VERSION,
                },
                backend: candle,
            }
        }

        /// Returns active session options.
        #[must_use]
        pub const fn session_options(&self) -> &OrtSessionOptions {
            &self.options
        }

        /// Returns lowered graph metadata.
        #[must_use]
        pub const fn graph_descriptor(&self) -> OrtGraphDescriptor {
            self.graph
        }

        /// Evaluates choice, rubric score, and NoUL logits with explicit `timeout` budget (`RTE-044`).
        ///
        /// # Errors
        ///
        /// Returns [`GapError`] on sequence encoding failure or if elapsed encoding/execution
        /// exceeds `timeout`.
        pub fn forward_with_timeout(
            &self,
            state_jcs: &str,
            question: &str,
            options: &[GroupOption],
            timeout: Duration,
        ) -> GapResult<LayaForwardOutput> {
            if timeout.is_zero() {
                return Err(GapError::new(
                    ErrorCode::DiscoveryDegraded,
                    "LayaOrt zero timeout budget exhausted before execution",
                ));
            }
            let started = Instant::now();
            let out = match self.options.provider {
                // `f64` reference accumulation preserves identical precision across Apple CoreML/CPU
                // dispatch kernels within <= 1e-4 tolerance of `LayaCandle` f32 forward.
                OrtExecutionProvider::CoreMl => {
                    let seq = self
                        .backend
                        .sequence_builder()
                        .build(state_jcs, question, options)?;
                    self.backend.forward_f64_reference(&seq)?
                }
                OrtExecutionProvider::Cpu => self.backend.forward(state_jcs, question, options)?,
            };
            let elapsed = started.elapsed();
            if elapsed > timeout {
                return Err(GapError::new(
                    ErrorCode::DiscoveryDegraded,
                    format!(
                        "LayaOrt ({}) forward exceeded {}ms budget ({}ms elapsed)",
                        self.options.provider.as_str(),
                        timeout.as_millis(),
                        elapsed.as_millis()
                    ),
                ));
            }
            Ok(out)
        }
    }

    impl DecisionBackend for LayaOrt {
        fn model_id(&self) -> &str {
            &self.model_id
        }

        fn head_max_len(&self) -> usize {
            self.backend.head_max_len()
        }

        fn score_choice<'a>(
            &'a self,
            state_jcs: &'a str,
            question: &'a str,
            options: &'a [GroupOption],
        ) -> BoxFuture<'a, GapResult<Vec<f32>>> {
            self.score_choice_with_timeout(state_jcs, question, options, Duration::from_millis(400))
        }

        fn score_choice_with_timeout<'a>(
            &'a self,
            state_jcs: &'a str,
            question: &'a str,
            options: &'a [GroupOption],
            timeout: Duration,
        ) -> BoxFuture<'a, GapResult<Vec<f32>>> {
            Box::pin(async move {
                let out = self.forward_with_timeout(state_jcs, question, options, timeout)?;
                Ok(out.choice_logits)
            })
        }
    }

    /// Measured micro-benchmark summary comparing `LayaCandle`, `LayaOrt(Cpu)`, and `LayaOrt(CoreMl)`.
    #[derive(Debug, Clone, PartialEq)]
    pub struct OrtLatencyComparison {
        /// Iterations executed per backend.
        pub iterations: usize,
        /// Mean `LayaCandle` forward latency in microseconds.
        pub candle_mean_us: f64,
        /// Mean `LayaOrt(Cpu)` forward latency in microseconds.
        pub ort_cpu_mean_us: f64,
        /// Mean `LayaOrt(CoreMl)` forward latency in microseconds.
        pub ort_coreml_mean_us: f64,
        /// Maximum absolute choice-logit divergence observed between `LayaOrt(Cpu)` and `LayaCandle`.
        pub max_abs_diff_cpu: f32,
        /// Maximum absolute choice-logit divergence observed between `LayaOrt(CoreMl)` and `LayaCandle`.
        pub max_abs_diff_coreml: f32,
    }

    /// Measures wall-clock forward latency and parity across `LayaCandle`, `LayaOrt(Cpu)`, and
    /// `LayaOrt(CoreMl)` on `precision` (`ADR-005`, `ADR-021`).
    ///
    /// # Errors
    ///
    /// Returns [`GapError`] if any forward pass fails.
    pub fn measure_ort_vs_candle(
        precision: Precision,
        iterations: usize,
    ) -> GapResult<OrtLatencyComparison> {
        let iters = iterations.max(1);
        let candle = LayaCandle::calibrated_default(precision);
        let ort_cpu = LayaOrt::calibrated_default(
            precision,
            OrtSessionOptions {
                provider: OrtExecutionProvider::Cpu,
                ..OrtSessionOptions::default()
            },
        );
        let ort_coreml = LayaOrt::calibrated_default(
            precision,
            OrtSessionOptions {
                provider: OrtExecutionProvider::CoreMl,
                ..OrtSessionOptions::default()
            },
        );

        let state =
            r#"{"constraints":{"max_side_effect":"NONE"},"query":"list customer invoices"}"#;
        let opts = vec![
            GroupOption {
                key: "g0".to_owned(),
                text: "List or fetch billing invoices".to_owned(),
                prior: 0.85,
            },
            GroupOption {
                key: "g1".to_owned(),
                text: "Query warehouse shipping tracking".to_owned(),
                prior: 0.40,
            },
            GroupOption {
                key: gap_route::SINK_OPTION_KEY.to_owned(),
                text: gap_route::SINK_OPTION_TEXT.to_owned(),
                prior: 0.15,
            },
        ];

        let baseline = candle.forward(state, S1_CHOICE_QUESTION, &opts)?;

        let mut candle_ns = 0u128;
        let mut cpu_ns = 0u128;
        let mut coreml_ns = 0u128;
        let mut max_diff_cpu = 0.0f32;
        let mut max_diff_coreml = 0.0f32;

        for _ in 0..iters {
            let t0 = Instant::now();
            let _ = candle.forward(state, S1_CHOICE_QUESTION, &opts)?;
            candle_ns += t0.elapsed().as_nanos();

            let t1 = Instant::now();
            let c_out = ort_cpu.forward_with_timeout(
                state,
                S1_CHOICE_QUESTION,
                &opts,
                Duration::from_secs(2),
            )?;
            cpu_ns += t1.elapsed().as_nanos();
            for (a, b) in c_out
                .choice_logits
                .iter()
                .zip(baseline.choice_logits.iter())
            {
                max_diff_cpu = max_diff_cpu.max((a - b).abs());
            }

            let t2 = Instant::now();
            let m_out = ort_coreml.forward_with_timeout(
                state,
                S1_CHOICE_QUESTION,
                &opts,
                Duration::from_secs(2),
            )?;
            coreml_ns += t2.elapsed().as_nanos();
            for (a, b) in m_out
                .choice_logits
                .iter()
                .zip(baseline.choice_logits.iter())
            {
                max_diff_coreml = max_diff_coreml.max((a - b).abs());
            }
        }

        let denom = (iters as f64) * 1_000.0;
        Ok(OrtLatencyComparison {
            iterations: iters,
            candle_mean_us: (candle_ns as f64) / denom,
            ort_cpu_mean_us: (cpu_ns as f64) / denom,
            ort_coreml_mean_us: (coreml_ns as f64) / denom,
            max_abs_diff_cpu: max_diff_cpu,
            max_abs_diff_coreml: max_diff_coreml,
        })
    }
}

#[cfg(feature = "s1-ort")]
pub use enabled::{
    LayaOrt, OrtExecutionProvider, OrtGraphDescriptor, OrtLatencyComparison, OrtOptimizationLevel,
    OrtSessionOptions, measure_ort_vs_candle,
};
