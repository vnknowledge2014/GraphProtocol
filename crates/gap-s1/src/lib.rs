//! Optional System-1 (`Laya`) `DecisionBackend` implementations (`LayaCandle`, `LayaOrt`, `HttpSidecar`) (`L3`).
// ADR-002 / ARC-003: gap-s1 is the sole crate exempted from #![forbid(unsafe_code)].
// Audited POSIX mmap blocks live exclusively in `src/candle/mmap.rs` (`UNS-001`..`UNS-005`, tracked in `docs/UNSAFE.md`).
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::todo,
    clippy::unimplemented
)]

pub mod candle;
pub mod ort;
pub mod providers;
pub mod sidecar;

pub use candle::{LayaCandle, LayaConfig, LayaForwardOutput, LayaTokenizer, Precision};
pub use providers::{
    DecisionAnswer, DecisionBatchResponse, DecisionHttpResponse, DecisionHttpTransport,
    DecisionQuestion, GEMINI_DECISION_1_DEFAULT_MODEL, GEMINI_MAX_QUESTIONS_PER_CALL,
    GeminiDecisionBackend, GeminiDecisionConfig, JEV_DEFAULT_MODEL, JevBackend, JevConfig,
    QuestionKind, RECEPTON_LAYA_DEFAULT_MODEL, ReceptronLayaBackend, ReceptronLayaConfig,
    TcpHttpDecisionTransport, align_choice_probabilities_to_logits, prob_to_logit,
    resolve_receptron_laya_cache_dir,
};
