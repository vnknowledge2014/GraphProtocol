//! Core graph schema constants, deterministic summaries, optional SCIP index importer,
//! and canonical golden snapshot types (`ING-060`..`ING-064`).

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use gap_core::error::{GapError, NgcErrorCode, NgcErrorInfo};
use gap_store::{EdgeResolution, StoreError};
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Maximum UTF-8 byte length corresponding to `ceil(bytes / 3.6) <= 40` (`ING-063`, `GQL-022`).
pub const MAX_SUMMARY_UTF8_BYTES: usize = 144;

/// Default confidence for structurally verified edges (`DEFINES`, `CONTAINS`, `IMPORTS`, `IMPLEMENTS`, `EXTENDS`, SCIP-backed).
pub const EXACT_EDGE_CONFIDENCE: f32 = 1.0;

/// Default confidence for heuristic AST-inferred edges (`CALLS`, `REFERENCES`, `TESTS`, `DOCUMENTS`).
pub const HEURISTIC_EDGE_CONFIDENCE: f32 = 0.80;

/// Canonical node categories emitted by `gap-ngc-code` (`ING-060`).
pub const NODE_REPO: &str = "Repo";
/// File node category (`ING-060`).
pub const NODE_FILE: &str = "File";
/// Module / package node category (`ING-060`).
pub const NODE_MODULE: &str = "Module";
/// Code symbol node category (`ING-060`).
pub const NODE_SYMBOL: &str = "Symbol";
/// Documentation node category (`ING-060`).
pub const NODE_DOC: &str = "Doc";
/// Test case / test function node category (`ING-060`).
pub const NODE_TEST: &str = "Test";
/// Conceptual / architecture topic node category (`ING-060`).
pub const NODE_CONCEPT: &str = "Concept";

/// All 7 node categories advertised in `MetaSummary.node_categories` (`ING-060`).
pub const NODE_CATEGORIES: &[&str] = &[
    NODE_REPO,
    NODE_FILE,
    NODE_MODULE,
    NODE_SYMBOL,
    NODE_DOC,
    NODE_TEST,
    NODE_CONCEPT,
];

/// Edge predicate linking modules/files to defined declarations (`ING-060`).
pub const EDGE_DEFINES: &str = "DEFINES";
/// Edge predicate linking repositories/files/modules/sections to child elements (`ING-060`).
pub const EDGE_CONTAINS: &str = "CONTAINS";
/// Edge predicate for import / dependency statements (`ING-060`).
pub const EDGE_IMPORTS: &str = "IMPORTS";
/// Edge predicate for function/method call sites (`ING-060`).
pub const EDGE_CALLS: &str = "CALLS";
/// Edge predicate for trait/interface implementation (`ING-060`).
pub const EDGE_IMPLEMENTS: &str = "IMPLEMENTS";
/// Edge predicate for class/interface inheritance (`ING-060`).
pub const EDGE_EXTENDS: &str = "EXTENDS";
/// Edge predicate for identifier or path cross-references (`ING-060`).
pub const EDGE_REFERENCES: &str = "REFERENCES";
/// Edge predicate linking documentation to files, symbols, or concepts (`ING-060`).
pub const EDGE_DOCUMENTS: &str = "DOCUMENTS";
/// Edge predicate linking tests to target symbols (`ING-060`).
pub const EDGE_TESTS: &str = "TESTS";
/// Edge predicate linking files/modules to external/internal package dependencies (`ING-060`).
pub const EDGE_DEPENDS_ON: &str = "DEPENDS_ON";
/// Edge predicate recording revision succession on modified files/symbols (`ING-060`, `ING-062`).
pub const EDGE_SUPERSEDES: &str = "SUPERSEDES";

/// All 11 edge predicates advertised in `MetaSummary.edge_predicates` (`ING-060`).
pub const EDGE_PREDICATES: &[&str] = &[
    EDGE_DEFINES,
    EDGE_CONTAINS,
    EDGE_IMPORTS,
    EDGE_CALLS,
    EDGE_IMPLEMENTS,
    EDGE_EXTENDS,
    EDGE_REFERENCES,
    EDGE_DOCUMENTS,
    EDGE_TESTS,
    EDGE_DEPENDS_ON,
    EDGE_SUPERSEDES,
];

/// Pinned language grammar versions in 1.0 (`ING-061`).
pub const GRAMMAR_VERSIONS: &[(&str, &str)] = &[
    ("rust", "0.24.2-pinned"),
    ("typescript", "0.23.2-pinned"),
    ("javascript", "0.23.2-pinned"),
    ("python", "0.25.0-pinned"),
    ("go", "0.25.0-pinned"),
    ("java", "0.23.5-pinned"),
    ("markdown", "0.3.2-pinned"),
];

/// Errors returned by `gap-ngc-code` indexing and query operations.
#[derive(Debug, Error)]
pub enum CodeNgcError {
    /// Context store operation failure.
    #[error("context store error: {0}")]
    Store(#[from] StoreError),
    /// Core GAP domain error.
    #[error("core error: {0}")]
    Core(#[from] GapError),
    /// Filesystem I/O failure.
    #[error("I/O error at {path}: {source}")]
    Io {
        /// Path associated with the I/O failure.
        path: String,
        /// Underlying OS error.
        #[source]
        source: std::io::Error,
    },
    /// Invalid JSON payload (such as a malformed SCIP file).
    #[error("JSON parse error: {0}")]
    Json(#[from] serde_json::Error),
    /// Caller supplied an out-of-range argument (`ING-064`).
    #[error("invalid argument `{field}`: {reason}")]
    InvalidArgument {
        /// Argument name.
        field: &'static str,
        /// Explanation of constraint violation.
        reason: String,
    },
}

impl From<CodeNgcError> for NgcErrorInfo {
    fn from(err: CodeNgcError) -> Self {
        match err {
            CodeNgcError::InvalidArgument { .. } => {
                NgcErrorInfo::new(NgcErrorCode::InvalidArgument, err.to_string())
            }
            CodeNgcError::Store(StoreError::Invalid { .. }) => {
                NgcErrorInfo::new(NgcErrorCode::InvalidArgument, err.to_string())
            }
            CodeNgcError::Store(StoreError::NotFound) => {
                NgcErrorInfo::new(NgcErrorCode::NotFound, err.to_string())
            }
            other => NgcErrorInfo::new(NgcErrorCode::Internal, other.to_string()),
        }
    }
}

/// Supported repository languages in 1.0 (`ING-061`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Language {
    /// Rust (`.rs`)
    Rust,
    /// TypeScript (`.ts`, `.tsx`, `.mts`, `.cts`)
    TypeScript,
    /// JavaScript (`.js`, `.jsx`, `.mjs`, `.cjs`)
    JavaScript,
    /// Python (`.py`)
    Python,
    /// Go (`.go`)
    Go,
    /// Java (`.java`)
    Java,
    /// Markdown (`.md`, `.markdown`)
    Markdown,
}

impl Language {
    /// Maps a file extension to a supported 1.0 [`Language`], or `None` for unsupported files.
    #[must_use]
    pub fn from_path(path: &Path) -> Option<Self> {
        let ext = path.extension()?.to_str()?.to_ascii_lowercase();
        match ext.as_str() {
            "rs" => Some(Self::Rust),
            "ts" | "tsx" | "mts" | "cts" => Some(Self::TypeScript),
            "js" | "jsx" | "mjs" | "cjs" => Some(Self::JavaScript),
            "py" => Some(Self::Python),
            "go" => Some(Self::Go),
            "java" => Some(Self::Java),
            "md" | "markdown" => Some(Self::Markdown),
            _ => None,
        }
    }

    /// Canonical lowercase language identifier.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Rust => "rust",
            Self::TypeScript => "typescript",
            Self::JavaScript => "javascript",
            Self::Python => "python",
            Self::Go => "go",
            Self::Java => "java",
            Self::Markdown => "markdown",
        }
    }

    /// Pinned grammar version identifier (`ING-061`).
    #[must_use]
    pub fn grammar_version(self) -> &'static str {
        let target = self.as_str();
        GRAMMAR_VERSIONS
            .iter()
            .find_map(|(name, ver)| (*name == target).then_some(*ver))
            .unwrap_or("0.0.0-unknown")
    }
}

/// Code symbol sub-classification (`Symbol(kind: ...)`, `ING-060`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SymbolKind {
    /// Top-level or module function.
    Function,
    /// Method bound to a type/struct/class.
    Method,
    /// Product data structure.
    Struct,
    /// Tagged union / enum type.
    Enum,
    /// Rust trait.
    Trait,
    /// Object-oriented class.
    Class,
    /// TypeScript / Go / Java interface.
    Interface,
    /// Compile-time constant.
    Const,
    /// Type alias.
    Type,
}

impl SymbolKind {
    /// Lowercase wire identifier matching `ING-060`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Function => "function",
            Self::Method => "method",
            Self::Struct => "struct",
            Self::Enum => "enum",
            Self::Trait => "trait",
            Self::Class => "class",
            Self::Interface => "interface",
            Self::Const => "const",
            Self::Type => "type",
        }
    }
}

/// Documentation sub-classification (`Doc(kind: ...)`, `ING-060`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DocKind {
    /// Project README documentation.
    Readme,
    /// Architecture Decision Record.
    Adr,
    /// API documentation artifact.
    Api,
    /// Inline or extracted comment block.
    Comment,
    /// Markdown heading section.
    MarkdownSection,
}

impl DocKind {
    /// Lowercase wire identifier matching `ING-060`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Readme => "readme",
            Self::Adr => "adr",
            Self::Api => "api",
            Self::Comment => "comment",
            Self::MarkdownSection => "markdown_section",
        }
    }
}

/// Deterministic tokenizer-agnostic token estimator matching `GQL-022` (`ceil(bytes / 3.6)`).
#[must_use]
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
pub fn estimate_tokens_gql022(text: &str) -> u32 {
    if text.is_empty() {
        return 0;
    }
    ((text.len() as f64) / 3.6).ceil() as u32
}

/// Builds a deterministic summary guaranteed `<= 40` tokens (`<= 144` UTF-8 bytes) from an
/// optional doc comment (first sentence) and normalized signature or heading (`ING-063`).
#[must_use]
pub fn summarize_text(doc_comment: Option<&str>, signature_or_heading: &str) -> String {
    let cleaned_doc = doc_comment.map(clean_first_sentence).unwrap_or_default();
    let cleaned_sig = normalize_whitespace(signature_or_heading);

    let combined = match (cleaned_doc.is_empty(), cleaned_sig.is_empty()) {
        (false, false) => {
            if cleaned_doc.len() + cleaned_sig.len() + 3 <= MAX_SUMMARY_UTF8_BYTES {
                format!("{cleaned_doc} [{cleaned_sig}]")
            } else {
                cleaned_doc
            }
        }
        (false, true) => cleaned_doc,
        (true, false) => cleaned_sig,
        (true, true) => String::from("undocumented element"),
    };

    truncate_utf8_on_word_boundary(&combined, MAX_SUMMARY_UTF8_BYTES)
}

fn clean_first_sentence(raw: &str) -> String {
    let mut parts = Vec::new();
    for line in raw.lines() {
        let stripped = line
            .trim()
            .trim_start_matches("///")
            .trim_start_matches("//!")
            .trim_start_matches("//")
            .trim_start_matches("/**")
            .trim_start_matches("*/")
            .trim_end_matches("*/")
            .trim_start_matches('*')
            .trim_start_matches('#')
            .trim_start_matches("\"\"\"")
            .trim_end_matches("\"\"\"")
            .trim_start_matches("'''")
            .trim_end_matches("'''")
            .trim();
        if stripped.is_empty() {
            if !parts.is_empty() {
                break;
            }
            continue;
        }
        parts.push(stripped.to_owned());
        if stripped.ends_with('.') || stripped.ends_with('!') || stripped.ends_with('?') {
            break;
        }
    }
    let joined = normalize_whitespace(&parts.join(" "));
    if let Some(pos) = joined.find(". ") {
        joined[..=pos].trim().to_owned()
    } else {
        joined
    }
}

fn normalize_whitespace(input: &str) -> String {
    input.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn truncate_utf8_on_word_boundary(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_owned();
    }
    if max_bytes <= 3 {
        let mut end = max_bytes.min(text.len());
        while end > 0 && !text.is_char_boundary(end) {
            end -= 1;
        }
        return text[..end].to_owned();
    }
    let budget = max_bytes - 3;
    let mut end = budget.min(text.len());
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    let slice = &text[..end];
    let trimmed = slice
        .rfind(' ')
        .filter(|pos| *pos >= end / 2)
        .map_or(slice, |pos| &slice[..pos])
        .trim_end();
    format!("{trimmed}...")
}

/// Optional SCIP/LSIF JSON import record upgrading matching edges to `EXACT` (`ING-060`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ScipIndexFile {
    /// Precise call pairs recorded by an external compiler/SCIP indexer.
    #[serde(default)]
    pub calls: Vec<ScipCallEdge>,
    /// Optional occurrence list referencing symbols in files.
    #[serde(default)]
    pub occurrences: Vec<ScipOccurrence>,
}

/// Precise SCIP/LSIF call edge record (`caller -> callee`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScipCallEdge {
    /// Caller symbol identifier (simple name or qualified path).
    pub caller: String,
    /// Callee symbol identifier (simple name or qualified path).
    pub callee: String,
    /// Optional repository-relative source path.
    #[serde(default)]
    pub file: Option<String>,
}

/// Optional SCIP occurrence entry referencing a symbol.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScipOccurrence {
    /// Symbol name referenced.
    pub symbol: String,
    /// Enclosing caller symbol name, if known.
    #[serde(default)]
    pub caller: Option<String>,
    /// Role tag (`"call"`, `"reference"`, `"definition"`).
    #[serde(default)]
    pub role: String,
}

/// Loaded precision index from `ingest.code.scip_paths[]` (`ING-060`).
#[derive(Debug, Clone, Default)]
pub struct ScipIndex {
    exact_pairs: BTreeSet<(String, String)>,
}

impl ScipIndex {
    /// Loads and merges one or more JSON SCIP/LSIF edge manifests (`ING-060`).
    ///
    /// # Errors
    ///
    /// Returns [`CodeNgcError::Io`] or [`CodeNgcError::Json`] if a configured file cannot be read or parsed.
    pub fn load_paths(paths: &[PathBuf]) -> Result<Self, CodeNgcError> {
        let mut exact_pairs = BTreeSet::new();
        for path in paths {
            let bytes = fs::read(path).map_err(|source| CodeNgcError::Io {
                path: path.display().to_string(),
                source,
            })?;
            let parsed: ScipIndexFile = serde_json::from_slice(&bytes)?;
            for edge in parsed.calls {
                exact_pairs.insert((edge.caller, edge.callee));
            }
            for occ in parsed.occurrences {
                if let Some(caller) = occ.caller
                    && (occ.role == "call" || occ.role == "reference" || occ.role.is_empty())
                {
                    exact_pairs.insert((caller, occ.symbol));
                }
            }
        }
        Ok(Self { exact_pairs })
    }

    /// Returns `true` if SCIP confirmed a precise edge between `caller` and `callee`.
    #[must_use]
    pub fn confirms_call(&self, caller: &str, callee: &str) -> bool {
        self.exact_pairs
            .contains(&(caller.to_owned(), callee.to_owned()))
    }
}

/// Non-fatal parse error recorded for an isolated malformed file (`ING-061`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileParseFailure {
    /// Repository-relative POSIX file path.
    pub path: String,
    /// Language tag if detected.
    pub language: Option<String>,
    /// Failure description.
    pub reason: String,
}

/// Summary metrics emitted after an incremental or full repository index pass (`ING-061`, `ING-062`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexReport {
    /// Supported files examined in the repository.
    pub files_scanned: usize,
    /// Files skipped because their `blake3` digest matched the prior index state.
    pub files_unchanged: usize,
    /// Files parsed and written during this pass.
    pub files_reindexed: usize,
    /// Files missing on disk whose graph subtrees were tombstoned (`valid_to`, `ING-062`).
    pub files_tombstoned: usize,
    /// Total active/updated nodes written during this pass.
    pub nodes_written: usize,
    /// Total active/updated edges written during this pass.
    pub edges_written: usize,
    /// Isolated per-file parse failures (never aborts remaining files, `ING-061`).
    pub parse_failures: Vec<FileParseFailure>,
}

/// Deterministic canonical node entry for golden graph snapshot comparisons (`ING-062`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GoldenNode {
    /// Stable node identifier.
    pub id: String,
    /// Node category (`Repo`, `File`, `Module`, `Symbol`, `Doc`, `Test`, `Concept`).
    pub node_type: String,
    /// Short human-readable label.
    pub label: String,
    /// Optional `<= 40` token summary.
    pub summary: Option<String>,
    /// Sub-kind or repository path attribute (`kind`, `language`, `file`).
    pub props: serde_json::Map<String, serde_json::Value>,
}

/// Deterministic canonical edge entry for golden graph snapshot comparisons (`ING-060`, `ING-062`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GoldenEdge {
    /// Edge predicate (`DEFINES`, `CALLS`, etc.).
    pub edge_type: String,
    /// Source node ID.
    pub from: String,
    /// Target node ID.
    pub to: String,
    /// `EXACT` or `HEURISTIC`.
    pub resolution: String,
    /// Confidence in `[0.0, 1.0]`.
    pub confidence: f32,
}

/// Deterministic snapshot representation used for golden graph fixtures and reproducibility (`ING-062`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GoldenGraphSnapshot {
    /// Sorted canonical nodes.
    pub nodes: Vec<GoldenNode>,
    /// Sorted canonical edges.
    pub edges: Vec<GoldenEdge>,
}

impl GoldenGraphSnapshot {
    /// Ensures deterministic canonical ordering regardless of filesystem traversal order.
    pub fn canonicalize(&mut self) {
        self.nodes.sort_by(|a, b| a.id.cmp(&b.id));
        self.edges.sort_by(|a, b| {
            a.edge_type
                .cmp(&b.edge_type)
                .then_with(|| a.from.cmp(&b.from))
                .then_with(|| a.to.cmp(&b.to))
                .then_with(|| a.resolution.cmp(&b.resolution))
        });
    }

    /// Maps an `EdgeResolution` to its canonical uppercase string.
    #[must_use]
    pub const fn resolution_str(res: EdgeResolution) -> &'static str {
        res.as_str()
    }
}
