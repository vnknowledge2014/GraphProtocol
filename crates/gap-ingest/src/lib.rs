//! Control-plane ingestion pipeline: deterministic extractors (`CISG`), optional enrichment,
//! verification gates, drift detection, and proposals (`L2`).
#![forbid(unsafe_code)]
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::todo,
    clippy::unimplemented
)]

pub mod cisg;
pub mod drift;
pub mod enrich;
pub mod extractors;
pub mod fetch;
pub mod gates;
pub mod lint;
pub mod proposals;
pub mod worker;

pub use cisg::{
    ArgDef, CandidateJoin, Cisg, CisgValue, DocBlob, ExtractError, ExtractLimits, Extractor,
    ExtractorInfo, FieldDef, FieldRef, JoinCardinality, JoinEvidence, KeyDef, LintStatus,
    OperationDef, Pagination, SourceHandle, SourceRef, TypeDef, TypeRef, UntrustedText, sha256_hex,
};
pub use fetch::{DenyAllSourceFetcher, FetchError, FetchLimits, FetchedSource, SourceFetcher};
pub use worker::{
    WorkerFailureKind, WorkerLimits, WorkerMetricsSnapshot, WorkerOutcome, WorkerPool,
    WorkerPoolConfig, WorkerReport,
};
