//! Multi-language source code and technical documentation graph `NativeGraphConnector` (`L4`, `ING-060`..`ING-064`).
#![forbid(unsafe_code)]
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::todo,
    clippy::unimplemented
)]

pub mod connector;
pub mod extract;
pub mod indexer;
pub mod model;

pub use connector::CodeGraphConnector;
pub use indexer::{CodeIndexConfig, CodeIndexer};
pub use model::{
    CodeNgcError, DocKind, EDGE_CALLS, EDGE_CONTAINS, EDGE_DEFINES, EDGE_DEPENDS_ON,
    EDGE_DOCUMENTS, EDGE_EXTENDS, EDGE_IMPLEMENTS, EDGE_IMPORTS, EDGE_PREDICATES, EDGE_REFERENCES,
    EDGE_SUPERSEDES, EDGE_TESTS, EXACT_EDGE_CONFIDENCE, FileParseFailure, GRAMMAR_VERSIONS,
    GoldenEdge, GoldenGraphSnapshot, GoldenNode, HEURISTIC_EDGE_CONFIDENCE, IndexReport, Language,
    MAX_SUMMARY_UTF8_BYTES, NODE_CATEGORIES, NODE_CONCEPT, NODE_DOC, NODE_FILE, NODE_MODULE,
    NODE_REPO, NODE_SYMBOL, NODE_TEST, ScipCallEdge, ScipIndex, ScipIndexFile, ScipOccurrence,
    SymbolKind, estimate_tokens_gql022, summarize_text,
};
