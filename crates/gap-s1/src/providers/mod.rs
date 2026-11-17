//! Multi-provider System-1 (`Laya` / `@receptron/laya`, TypeSafe `Jev`, Google
//! `Gemini Decision 1`) wire codecs and adapters (`ADR-028`, `RTE-047`..`RTE-049`).

pub mod gemini;
pub mod http_client;
pub mod jev;
pub mod receptron_laya;
pub mod types;

pub use gemini::{
    GEMINI_DECISION_1_DEFAULT_MODEL, GEMINI_MAX_QUESTIONS_PER_CALL, GeminiDecisionBackend,
    GeminiDecisionConfig,
};
pub use http_client::{DecisionHttpResponse, DecisionHttpTransport, TcpHttpDecisionTransport};
pub use jev::{JEV_DEFAULT_MODEL, JevBackend, JevConfig};
pub use receptron_laya::{
    RECEPTON_LAYA_DEFAULT_MODEL, ReceptronLayaBackend, ReceptronLayaConfig,
    resolve_receptron_laya_cache_dir,
};
pub use types::{
    DecisionAnswer, DecisionBatchResponse, DecisionQuestion, QuestionKind,
    align_choice_probabilities_to_logits, prob_to_logit,
};
