//! Owned `candle`-semantics `laya-typed-decisions` backend (`ADR-005`, `RTE-041`, `RTE-042`, `WP-M5-02`).

pub mod json;
pub mod mmap;
pub mod model;
pub mod safetensors;
pub mod sequence;
pub mod sha256;
pub mod tokenizer;

pub use json::{JsonValue, escape_json_str, parse_json};
pub use mmap::MmapBytes;
pub use model::{GoldenFixtureScorer, LayaCandle, LayaConfig, LayaForwardOutput};
pub use safetensors::{
    LoadedTensor, Precision, SafeTensorsArchive, TensorData, f16_bits_to_f32, f32_to_f16_bits,
    serialize_safetensors,
};
pub use sequence::{DEFAULT_HEAD_MAX_LEN, EncodedSequence, SequenceBuilder};
pub use sha256::{sha256_bytes, sha256_hex};
pub use tokenizer::{
    CLS_ID, DEFAULT_VOCAB_SIZE, FIRST_ORDINARY_ID, LayaTokenizer, MASK_ID, NUM_TOPIC_BUCKETS,
    PAD_ID, SEP_ID, UNK_ID,
};
