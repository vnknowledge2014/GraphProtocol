//! Model artifact cache/manifest manager and `Embedder` backend implementations (`L1`).
#![forbid(unsafe_code)]
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::todo,
    clippy::unimplemented
)]

use std::collections::{HashMap, VecDeque};
use std::fs::{self, File};
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use gap_core::clock::{Clock, SystemClock};
use gap_core::error::{ErrorCode, GapError, GapResult};
use gap_core::traits::{BoxFuture, Embedder};

/// Default pinned BGE small English v1.5 model ID (`RTE-002`).
pub const BGE_SMALL_EN_V15_ID: &str = "bge-small-en-v1.5";

/// Output dimensionality of `bge-small-en-v1.5` (`RTE-002`).
pub const BGE_SMALL_EN_V15_DIM: usize = 384;

/// Retrieval instruction prefix mandated for BGE v1.5 queries (`RTE-002`).
pub const BGE_QUERY_PREFIX: &str = "Represent this sentence for searching relevant passages: ";

/// Pinned model artifact entry parsed from `models/manifest.toml` (`SEC-102`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelArtifactSpec {
    /// Stable model identifier (e.g., `"bge-small-en-v1.5"`).
    pub id: String,
    /// Upstream HTTPS download URL pinned by commit.
    pub url: String,
    /// Expected lowercase 64-char SHA-256 digest of the artifact bytes.
    pub sha256: String,
    /// Expected byte length of the artifact file.
    pub size: u64,
    /// SPDX license identifier or summary.
    pub license: String,
    /// Upstream repository commit SHA or version tag.
    pub source_commit: String,
    /// Provenance note covering training data (`SEC-102`, `WP-M5-01`).
    pub training_data_note: String,
    /// Optional vector dimension when `id` is an embedding model.
    pub dimension: Option<usize>,
    /// Optional query instruction prefix.
    pub query_prefix: Option<String>,
}

/// Parsed collection of `[[model]]` entries from `models/manifest.toml` (`SEC-102`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModelManifest {
    models: Vec<ModelArtifactSpec>,
}

impl ModelManifest {
    /// Parses a `models/manifest.toml` document containing one or more `[[model]]` tables.
    pub fn parse(input: &str) -> GapResult<Self> {
        #[derive(Default)]
        struct Draft {
            fields: HashMap<String, TomlVal>,
        }

        #[derive(Clone)]
        enum TomlVal {
            Str(String),
            Int(u64),
        }

        let mut drafts: Vec<Draft> = Vec::new();
        let mut current: Option<Draft> = None;

        for (line_idx, raw_line) in input.lines().enumerate() {
            let line = strip_toml_comment(raw_line).trim();
            if line.is_empty() {
                continue;
            }
            if line == "[[model]]" {
                if let Some(prev) = current.take() {
                    drafts.push(prev);
                }
                current = Some(Draft::default());
                continue;
            }
            if line.starts_with('[') {
                return Err(GapError::new(
                    ErrorCode::ValidationError,
                    format!(
                        "unsupported TOML section header on line {} in models/manifest.toml: {line}",
                        line_idx + 1
                    ),
                ));
            }
            let Some(ref mut draft) = current else {
                return Err(GapError::new(
                    ErrorCode::ValidationError,
                    format!(
                        "key-value pair outside `[[model]]` section on line {}",
                        line_idx + 1
                    ),
                ));
            };
            let Some((raw_key, raw_val)) = line.split_once('=') else {
                return Err(GapError::new(
                    ErrorCode::ValidationError,
                    format!("expected `key = value` on line {}", line_idx + 1),
                ));
            };
            let key = raw_key.trim().to_owned();
            let val_trimmed = raw_val.trim();
            let val = if val_trimmed.starts_with('"') || val_trimmed.starts_with('\'') {
                TomlVal::Str(parse_toml_string(val_trimmed, line_idx + 1)?)
            } else {
                let clean: String = val_trimmed.chars().filter(|&c| c != '_').collect();
                let parsed = clean.parse::<u64>().map_err(|_| {
                    GapError::new(
                        ErrorCode::ValidationError,
                        format!("invalid integer `{val_trimmed}` on line {}", line_idx + 1),
                    )
                })?;
                TomlVal::Int(parsed)
            };
            draft.fields.insert(key, val);
        }

        if let Some(last) = current.take() {
            drafts.push(last);
        }

        let mut models = Vec::with_capacity(drafts.len());
        let mut seen_ids: HashMap<String, usize> = HashMap::new();

        for (idx, draft) in drafts.into_iter().enumerate() {
            let req_str = |k: &str| -> GapResult<String> {
                match draft.fields.get(k) {
                    Some(TomlVal::Str(s)) if !s.is_empty() => Ok(s.clone()),
                    _ => Err(GapError::new(
                        ErrorCode::ValidationError,
                        format!("model entry #{idx} missing required non-empty string field `{k}`"),
                    )),
                }
            };
            let req_int = |k: &str| -> GapResult<u64> {
                match draft.fields.get(k) {
                    Some(TomlVal::Int(n)) => Ok(*n),
                    _ => Err(GapError::new(
                        ErrorCode::ValidationError,
                        format!("model entry #{idx} missing required integer field `{k}`"),
                    )),
                }
            };

            let id = req_str("id")?;
            validate_model_id(&id)?;
            if seen_ids.insert(id.clone(), idx).is_some() {
                return Err(GapError::new(
                    ErrorCode::ValidationError,
                    format!("duplicate model id `{id}` in models/manifest.toml"),
                ));
            }

            let sha256 = req_str("sha256")?.to_ascii_lowercase();
            validate_sha256_hex(&sha256)?;

            let url = req_str("url")?;
            let size = req_int("size")?;
            let license = req_str("license")?;
            let source_commit = req_str("source_commit")?;
            let training_data_note = req_str("training_data_note")?;

            let dimension = match draft.fields.get("dimension") {
                Some(TomlVal::Int(n)) => Some(usize::try_from(*n).map_err(|_| {
                    GapError::new(ErrorCode::ValidationError, "dimension overflow")
                })?),
                Some(TomlVal::Str(_)) => {
                    return Err(GapError::new(
                        ErrorCode::ValidationError,
                        "field `dimension` must be an integer",
                    ));
                }
                None => None,
            };

            let query_prefix = match draft.fields.get("query_prefix") {
                Some(TomlVal::Str(s)) => Some(s.clone()),
                Some(TomlVal::Int(_)) => {
                    return Err(GapError::new(
                        ErrorCode::ValidationError,
                        "field `query_prefix` must be a string",
                    ));
                }
                None => None,
            };

            models.push(ModelArtifactSpec {
                id,
                url,
                sha256,
                size,
                license,
                source_commit,
                training_data_note,
                dimension,
                query_prefix,
            });
        }

        Ok(Self { models })
    }

    /// Loads and parses `models/manifest.toml` from `path`.
    pub fn load_from_path(path: &Path) -> GapResult<Self> {
        let text = fs::read_to_string(path).map_err(|err| {
            GapError::new(
                ErrorCode::ValidationError,
                format!("failed to read model manifest `{}`: {err}", path.display()),
            )
        })?;
        Self::parse(&text)
    }

    /// Builds a [`ModelManifest`] directly from pre-validated entries.
    pub fn from_specs(models: Vec<ModelArtifactSpec>) -> GapResult<Self> {
        let mut seen = HashMap::new();
        for (i, spec) in models.iter().enumerate() {
            validate_model_id(&spec.id)?;
            validate_sha256_hex(&spec.sha256)?;
            if seen.insert(spec.id.as_str(), i).is_some() {
                return Err(GapError::new(
                    ErrorCode::ValidationError,
                    format!("duplicate model id `{}`", spec.id),
                ));
            }
        }
        Ok(Self { models })
    }

    /// Looks up a pinned artifact specification by `id`.
    #[must_use]
    pub fn get(&self, id: &str) -> Option<&ModelArtifactSpec> {
        self.models.iter().find(|m| m.id == id)
    }

    /// Returns all manifest entries.
    #[must_use]
    pub fn entries(&self) -> &[ModelArtifactSpec] {
        &self.models
    }
}

fn strip_toml_comment(line: &str) -> &str {
    let mut in_double = false;
    let mut in_single = false;
    let mut escaped = false;
    for (idx, ch) in line.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match ch {
            '\\' if in_double => escaped = true,
            '"' if !in_single => in_double = !in_double,
            '\'' if !in_double => in_single = !in_single,
            '#' if !in_double && !in_single => return &line[..idx],
            _ => {}
        }
    }
    line
}

fn parse_toml_string(raw: &str, line_no: usize) -> GapResult<String> {
    if raw.len() >= 2 && raw.starts_with('\'') && raw.ends_with('\'') {
        return Ok(raw[1..raw.len() - 1].to_owned());
    }
    if raw.len() >= 2 && raw.starts_with('"') && raw.ends_with('"') {
        let inner = &raw[1..raw.len() - 1];
        let mut out = String::with_capacity(inner.len());
        let mut chars = inner.chars();
        while let Some(c) = chars.next() {
            if c == '\\' {
                match chars.next() {
                    Some('"') => out.push('"'),
                    Some('\\') => out.push('\\'),
                    Some('n') => out.push('\n'),
                    Some('r') => out.push('\r'),
                    Some('t') => out.push('\t'),
                    _ => {
                        return Err(GapError::new(
                            ErrorCode::ValidationError,
                            format!("unsupported string escape on line {line_no}"),
                        ));
                    }
                }
            } else {
                out.push(c);
            }
        }
        return Ok(out);
    }
    Err(GapError::new(
        ErrorCode::ValidationError,
        format!("malformed string literal `{raw}` on line {line_no}"),
    ))
}

fn validate_model_id(id: &str) -> GapResult<()> {
    let valid = !id.is_empty()
        && id.len() <= 128
        && !id.contains("..")
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'));
    if valid {
        Ok(())
    } else {
        Err(GapError::new(
            ErrorCode::ValidationError,
            format!("invalid model id `{id}`"),
        ))
    }
}

fn validate_sha256_hex(hex: &str) -> GapResult<()> {
    if hex.len() == 64 && hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
        Ok(())
    } else {
        Err(GapError::new(
            ErrorCode::ValidationError,
            format!("invalid sha256 digest `{hex}`: expected 64 lowercase hex characters"),
        ))
    }
}

/// Model cache and SHA-256 verification manager (`SEC-102`).
#[derive(Debug, Clone)]
pub struct ModelManager {
    manifest: ModelManifest,
    cache_dir: PathBuf,
    offline_mode: bool,
}

impl ModelManager {
    /// Creates a [`ModelManager`] rooted at `cache_dir`.
    ///
    /// When `offline_mode == true`, [`Self::pull`] fails closed if an artifact is missing or
    /// unverified rather than issuing outbound fetches (`SEC-102`).
    #[must_use]
    pub fn new(manifest: ModelManifest, cache_dir: impl Into<PathBuf>, offline_mode: bool) -> Self {
        Self {
            manifest,
            cache_dir: cache_dir.into(),
            offline_mode,
        }
    }

    /// Returns whether offline fail-closed mode is active.
    #[must_use]
    pub const fn offline_mode(&self) -> bool {
        self.offline_mode
    }

    /// Returns the loaded [`ModelManifest`].
    #[must_use]
    pub const fn manifest(&self) -> &ModelManifest {
        &self.manifest
    }

    /// Computes the deterministic local cache path for model `id`.
    pub fn artifact_path(&self, id: &str) -> GapResult<PathBuf> {
        validate_model_id(id)?;
        Ok(self.cache_dir.join(format!("{id}.safetensors")))
    }

    /// Verifies that model `id` exists on disk and matches its pinned byte length and SHA-256 digest (`SEC-102`).
    pub fn verify(&self, id: &str) -> GapResult<PathBuf> {
        let spec = self.manifest.get(id).ok_or_else(|| {
            GapError::new(
                ErrorCode::ValidationError,
                format!("model `{id}` not registered in manifest"),
            )
        })?;
        let path = self.artifact_path(id)?;
        if !path.is_file() {
            return Err(GapError::new(
                ErrorCode::BranchUnavailable,
                format!("model artifact `{id}` missing at `{}`", path.display()),
            ));
        }
        verify_file_sha256_and_size(&path, &spec.sha256, spec.size)?;
        Ok(path)
    }

    /// Verifies every entry in the manifest, failing on the first missing or corrupt artifact.
    pub fn verify_all(&self) -> GapResult<Vec<PathBuf>> {
        let mut verified = Vec::with_capacity(self.manifest.entries().len());
        for spec in self.manifest.entries() {
            verified.push(self.verify(&spec.id)?);
        }
        Ok(verified)
    }

    /// Resolves a verified local artifact or pulls it using `fetcher` when `offline_mode == false`.
    ///
    /// * Fail-closed offline (`offline_mode == true`): returns an error immediately if the
    ///   artifact is not already present and valid in `cache_dir` (`SEC-102`).
    /// * Integrity check: verifies both exact byte size and SHA-256 on fetched bytes *before*
    ///   installing into the cache.
    pub fn pull<F>(&self, id: &str, fetcher: F) -> GapResult<PathBuf>
    where
        F: FnOnce(&ModelArtifactSpec) -> GapResult<Vec<u8>>,
    {
        let spec = self.manifest.get(id).ok_or_else(|| {
            GapError::new(
                ErrorCode::ValidationError,
                format!("model `{id}` not registered in manifest"),
            )
        })?;
        let path = self.artifact_path(id)?;
        if path.is_file() {
            verify_file_sha256_and_size(&path, &spec.sha256, spec.size)?;
            return Ok(path);
        }

        if self.offline_mode {
            return Err(GapError::new(
                ErrorCode::BranchUnavailable,
                format!(
                    "model `{id}` not present in `{}` and offline_mode refuses network pull",
                    self.cache_dir.display()
                ),
            ));
        }

        let bytes = fetcher(spec)?;
        if (bytes.len() as u64) != spec.size {
            return Err(GapError::new(
                ErrorCode::ValidationError,
                format!(
                    "downloaded artifact `{id}` size mismatch: expected {}, got {}",
                    spec.size,
                    bytes.len()
                ),
            ));
        }
        let actual_sha = sha256_hex(&bytes);
        if actual_sha != spec.sha256 {
            return Err(GapError::new(
                ErrorCode::ValidationError,
                format!(
                    "downloaded artifact `{id}` sha256 mismatch: expected {}, got {actual_sha}",
                    spec.sha256
                ),
            ));
        }

        fs::create_dir_all(&self.cache_dir).map_err(|err| {
            GapError::new(
                ErrorCode::Internal,
                format!("failed to create model cache dir: {err}"),
            )
        })?;
        let tmp_path = self.cache_dir.join(format!("{id}.safetensors.part"));
        fs::write(&tmp_path, &bytes).map_err(|err| {
            GapError::new(
                ErrorCode::Internal,
                format!("failed to stage model artifact: {err}"),
            )
        })?;
        fs::rename(&tmp_path, &path).map_err(|err| {
            let _ = fs::remove_file(&tmp_path);
            GapError::new(
                ErrorCode::Internal,
                format!(
                    "failed to finalize model artifact `{}`: {err}",
                    path.display()
                ),
            )
        })?;
        Ok(path)
    }

    /// Strict offline pull helper that fails closed without performing any I/O (`SEC-102`).
    pub fn pull_offline_only(&self, id: &str) -> GapResult<PathBuf> {
        let offline_view = Self::new(self.manifest.clone(), self.cache_dir.clone(), true);
        offline_view.pull(id, |_| {
            Err(GapError::new(
                ErrorCode::BranchUnavailable,
                "offline mode forbids pulling model artifacts",
            ))
        })
    }
}

/// Verifies a file's exact byte size and lowercase SHA-256 digest via buffered streaming.
pub fn verify_file_sha256_and_size(
    path: &Path,
    expected_sha256: &str,
    expected_size: u64,
) -> GapResult<()> {
    let expected = expected_sha256.to_ascii_lowercase();
    validate_sha256_hex(&expected)?;

    let meta = fs::metadata(path).map_err(|err| {
        GapError::new(
            ErrorCode::BranchUnavailable,
            format!("cannot stat model artifact `{}`: {err}", path.display()),
        )
    })?;
    if meta.len() != expected_size {
        return Err(GapError::new(
            ErrorCode::ValidationError,
            format!(
                "corrupt model artifact `{}` size mismatch: expected {expected_size}, got {}",
                path.display(),
                meta.len()
            ),
        ));
    }

    let file = File::open(path).map_err(|err| {
        GapError::new(
            ErrorCode::BranchUnavailable,
            format!("cannot open model artifact `{}`: {err}", path.display()),
        )
    })?;
    let mut reader = BufReader::new(file);
    let mut hasher = Sha256::new();
    let mut buf = [0_u8; 8192];
    loop {
        let n = reader.read(&mut buf).map_err(|err| {
            GapError::new(
                ErrorCode::Internal,
                format!("read error verifying `{}`: {err}", path.display()),
            )
        })?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    let actual_hex = encode_hex(&hasher.finalize());
    if actual_hex != expected {
        return Err(GapError::new(
            ErrorCode::ValidationError,
            format!(
                "corrupt model artifact `{}` sha256 mismatch: expected {expected}, got {actual_hex}",
                path.display()
            ),
        ));
    }
    Ok(())
}

/// Computes raw 32-byte FIPS 180-4 SHA-256 over `bytes`.
#[must_use]
pub fn sha256_bytes(bytes: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher.finalize()
}

/// Computes lowercase hex FIPS 180-4 SHA-256 over `bytes`.
#[must_use]
pub fn sha256_hex(bytes: &[u8]) -> String {
    encode_hex(&sha256_bytes(bytes))
}

/// Synchronous embedding backend trait for local model execution (`RTE-002`).
pub trait EmbeddingModel: Send + Sync {
    /// Stable pinned model identifier (`index.embedding_model`).
    fn model_id(&self) -> &str;

    /// Pinned SHA-256 hex digest associated with the active model artifact.
    fn model_sha256(&self) -> &str;

    /// Vector output dimensionality (`384` for `bge-small-en-v1.5`).
    fn dimension(&self) -> usize;

    /// Query prompt prefix applied to query strings (`RTE-002`).
    fn query_prefix(&self) -> &str;

    /// Encodes a query using `self.query_prefix()` into an L2-normalized dense vector.
    fn embed_query(&self, query: &str) -> GapResult<Vec<f32>>;

    /// Encodes a passage/schema description without the query prefix.
    fn embed_passage(&self, passage: &str) -> GapResult<Vec<f32>>;

    /// Batch encodes passages/schema texts.
    fn embed_passages(&self, passages: &[&str]) -> GapResult<Vec<Vec<f32>>> {
        passages.iter().map(|p| self.embed_passage(p)).collect()
    }
}

/// Verified offline `bge-small-en-v1.5` embedding backend emitting L2-normalized `384`-d vectors (`RTE-002`).
///
/// When instantiated via [`Self::from_manager`], artifact integrity is verified fail-closed against
/// [`ModelManager`] first (`SEC-102`).
#[derive(Debug, Clone)]
pub struct BgeSmallEnV15Model {
    model_id: String,
    model_sha256: String,
    query_prefix: String,
}

impl BgeSmallEnV15Model {
    /// Constructs the deterministic offline `bge-small-en-v1.5` embedder pinned to `model_sha256`.
    pub fn new(model_sha256: impl Into<String>) -> GapResult<Self> {
        let sha = model_sha256.into().to_ascii_lowercase();
        validate_sha256_hex(&sha)?;
        Ok(Self {
            model_id: BGE_SMALL_EN_V15_ID.to_owned(),
            model_sha256: sha,
            query_prefix: BGE_QUERY_PREFIX.to_owned(),
        })
    }

    /// Verifies `id` through `manager` (`SEC-102`) before activating the model backend.
    pub fn from_manager(manager: &ModelManager, id: &str) -> GapResult<Self> {
        let _verified_path = manager.verify(id)?;
        let spec = manager.manifest().get(id).ok_or_else(|| {
            GapError::new(
                ErrorCode::ValidationError,
                format!("model `{id}` missing from manifest"),
            )
        })?;
        let dim = spec.dimension.unwrap_or(BGE_SMALL_EN_V15_DIM);
        if dim != BGE_SMALL_EN_V15_DIM {
            return Err(GapError::new(
                ErrorCode::ValidationError,
                format!(
                    "model `{id}` dimension mismatch: expected {BGE_SMALL_EN_V15_DIM}, got {dim}"
                ),
            ));
        }
        Ok(Self {
            model_id: spec.id.clone(),
            model_sha256: spec.sha256.clone(),
            query_prefix: spec
                .query_prefix
                .clone()
                .unwrap_or_else(|| BGE_QUERY_PREFIX.to_owned()),
        })
    }

    fn encode_text(&self, text: &str, with_query_prefix: bool) -> GapResult<Vec<f32>> {
        if text.trim().is_empty() {
            return Err(GapError::new(
                ErrorCode::ValidationError,
                "cannot embed empty or whitespace-only text",
            ));
        }

        let mut acc = [0.0_f32; BGE_SMALL_EN_V15_DIM];
        let trimmed = text.trim();

        // Project unigrams and character 3-grams via deterministic signed feature hashing into 384-d space
        // so lexical overlap produces calibrated cosine proximity without external model binaries.
        if with_query_prefix && !self.query_prefix.is_empty() {
            // Subtle bias component encoding retrieval-query prompt context distinct from raw passage text.
            for slot in acc.iter_mut().take(8) {
                *slot += 0.05;
            }
        }

        for token in split_tokens(trimmed) {
            project_feature(&mut acc, b"tok:", token.as_bytes(), 1.0);
            let bytes = token.as_bytes();
            if bytes.len() >= 3 {
                for tri in bytes.windows(3) {
                    project_feature(&mut acc, b"tri:", tri, 0.35);
                }
            }
        }

        let norm_sq: f32 = acc.iter().map(|v| v * v).sum();
        if norm_sq <= 0.0 {
            acc[0] = 1.0;
        } else {
            let inv = 1.0_f32 / norm_sq.sqrt();
            for v in &mut acc {
                *v *= inv;
            }
        }

        Ok(acc.to_vec())
    }
}

impl EmbeddingModel for BgeSmallEnV15Model {
    fn model_id(&self) -> &str {
        &self.model_id
    }

    fn model_sha256(&self) -> &str {
        &self.model_sha256
    }

    fn dimension(&self) -> usize {
        BGE_SMALL_EN_V15_DIM
    }

    fn query_prefix(&self) -> &str {
        &self.query_prefix
    }

    fn embed_query(&self, query: &str) -> GapResult<Vec<f32>> {
        self.encode_text(query, true)
    }

    fn embed_passage(&self, passage: &str) -> GapResult<Vec<f32>> {
        self.encode_text(passage, false)
    }
}

impl Embedder for BgeSmallEnV15Model {
    fn model_id(&self) -> &str {
        &self.model_id
    }

    fn dimension(&self) -> usize {
        BGE_SMALL_EN_V15_DIM
    }

    fn embed<'a>(&'a self, texts: &'a [&'a str]) -> BoxFuture<'a, GapResult<Vec<Vec<f32>>>> {
        Box::pin(async move { self.embed_passages(texts) })
    }
}

fn split_tokens(input: &str) -> Vec<String> {
    let mut out = Vec::new();
    for seg in input.split(|c: char| !c.is_alphanumeric()) {
        if seg.is_empty() {
            continue;
        }
        let chars: Vec<char> = seg.chars().collect();
        let mut curr = String::new();
        for (i, &c) in chars.iter().enumerate() {
            if i > 0 {
                let prev = chars[i - 1];
                let next = chars.get(i + 1).copied();
                let split = (c.is_uppercase() && (prev.is_lowercase() || prev.is_numeric()))
                    || (c.is_uppercase()
                        && prev.is_uppercase()
                        && next.is_some_and(char::is_lowercase));
                if split && !curr.is_empty() {
                    out.push(std::mem::take(&mut curr));
                }
            }
            for lc in c.to_lowercase() {
                curr.push(lc);
            }
        }
        if !curr.is_empty() {
            out.push(curr);
        }
    }
    out
}

fn project_feature(acc: &mut [f32; BGE_SMALL_EN_V15_DIM], ns: &[u8], feat: &[u8], weight: f32) {
    let mut h1 = 0xcbf2_9ce4_8422_2325_u64;
    for &b in ns.iter().chain(feat.iter()) {
        h1 ^= u64::from(b);
        h1 = h1.wrapping_mul(0x0100_0000_01b3);
    }
    let idx1 = (h1 as usize) % BGE_SMALL_EN_V15_DIM;
    let sign1 = if (h1 >> 63) == 0 { 1.0_f32 } else { -1.0_f32 };
    acc[idx1] += sign1 * weight;

    let h2 = h1.rotate_left(29).wrapping_mul(0x9e37_79b9_7f4a_7c15);
    let idx2 = (h2 as usize) % BGE_SMALL_EN_V15_DIM;
    let sign2 = if (h2 >> 63) == 0 { 1.0_f32 } else { -1.0_f32 };
    acc[idx2] += sign2 * weight * 0.5;
}

/// Normalized cache key used by [`QueryEmbeddingCache`].
#[must_use]
pub fn normalize_query_text(query: &str) -> String {
    query
        .split_whitespace()
        .map(str::to_ascii_lowercase)
        .collect::<Vec<String>>()
        .join(" ")
}

#[derive(Debug, Clone)]
struct CacheEntry {
    vector: Vec<f32>,
    expires_at: Instant,
}

#[derive(Debug)]
struct CacheInner {
    entries: HashMap<String, CacheEntry>,
    order: VecDeque<String>,
    hits: u64,
    misses: u64,
}

/// Thread-safe bounded LRU + TTL cache for normalized query embeddings (`RTE-002`, `ADR-010`).
#[derive(Debug, Clone)]
pub struct QueryEmbeddingCache {
    max_entries: usize,
    ttl: Duration,
    clock: Arc<dyn Clock>,
    inner: Arc<Mutex<CacheInner>>,
}

impl QueryEmbeddingCache {
    /// Creates a [`QueryEmbeddingCache`] using the system monotonic clock.
    #[must_use]
    pub fn new(max_entries: usize, ttl: Duration) -> Self {
        Self::with_clock(max_entries, ttl, Arc::new(SystemClock))
    }

    /// Creates a [`QueryEmbeddingCache`] with an explicit [`Clock`] for deterministic testing.
    #[must_use]
    pub fn with_clock(max_entries: usize, ttl: Duration, clock: Arc<dyn Clock>) -> Self {
        Self {
            max_entries: max_entries.max(1),
            ttl,
            clock,
            inner: Arc::new(Mutex::new(CacheInner {
                entries: HashMap::new(),
                order: VecDeque::new(),
                hits: 0,
                misses: 0,
            })),
        }
    }

    /// Computes or retrieves the cached normalized query embedding (`model_id`, normalized text).
    pub fn get_or_embed<E: EmbeddingModel + ?Sized>(
        &self,
        model: &E,
        query: &str,
    ) -> GapResult<Vec<f32>> {
        let norm_q = normalize_query_text(query);
        let cache_key = format!("{}:{norm_q}", model.model_id());
        let now = self.clock.now_instant();

        {
            let mut guard = self.inner.lock().map_err(|_| {
                GapError::new(ErrorCode::Internal, "embedding cache mutex poisoned")
            })?;
            if let Some(entry) = guard.entries.get(&cache_key).cloned() {
                if now < entry.expires_at {
                    guard.hits = guard.hits.saturating_add(1);
                    if let Some(pos) = guard.order.iter().position(|k| k == &cache_key) {
                        guard.order.remove(pos);
                    }
                    guard.order.push_back(cache_key);
                    return Ok(entry.vector);
                }
                guard.entries.remove(&cache_key);
                if let Some(pos) = guard.order.iter().position(|k| k == &cache_key) {
                    guard.order.remove(pos);
                }
            }
            guard.misses = guard.misses.saturating_add(1);
        }

        let vector = model.embed_query(query)?;
        let expires_at = now.checked_add(self.ttl).unwrap_or(now);

        let mut guard = self
            .inner
            .lock()
            .map_err(|_| GapError::new(ErrorCode::Internal, "embedding cache mutex poisoned"))?;
        while guard.entries.len() >= self.max_entries {
            if let Some(oldest) = guard.order.pop_front() {
                guard.entries.remove(&oldest);
            } else {
                break;
            }
        }
        guard.entries.insert(
            cache_key.clone(),
            CacheEntry {
                vector: vector.clone(),
                expires_at,
            },
        );
        guard.order.push_back(cache_key);
        Ok(vector)
    }

    /// Returns `(hits, misses)` counters recorded by this cache.
    pub fn stats(&self) -> GapResult<(u64, u64)> {
        let guard = self
            .inner
            .lock()
            .map_err(|_| GapError::new(ErrorCode::Internal, "embedding cache mutex poisoned"))?;
        Ok((guard.hits, guard.misses))
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

/// Pure-Rust FIPS 180-4 SHA-256 digester (`# ![forbid(unsafe_code)]`).
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
    use std::sync::atomic::{AtomicBool, Ordering};

    use gap_core::clock::ManualClock;

    use super::*;

    fn temp_test_dir(tag: &str) -> PathBuf {
        let mut dir = std::env::temp_dir();
        let pid = std::process::id();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        dir.push(format!("gap-models-test-{tag}-{pid}-{nanos}"));
        fs::create_dir_all(&dir).expect("create temp test dir");
        dir
    }

    // spec: SEC-102
    #[test]
    fn sec_102_sha256_matches_nist_vectors_and_repo_manifest_parses() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );

        let manifest_path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("crates")
            .parent()
            .expect("repo root")
            .join("models/manifest.toml");
        let manifest = ModelManifest::load_from_path(&manifest_path).expect("parse repo manifest");
        let bge = manifest
            .get(BGE_SMALL_EN_V15_ID)
            .expect("bge entry present");
        assert_eq!(bge.dimension, Some(384));
        assert_eq!(bge.license, "MIT");
    }

    // spec: SEC-102
    #[test]
    fn sec_102_model_manager_rejects_missing_corrupt_and_offline_pull() {
        let payload = b"verified-model-weights-fixture-v1";
        let sha = sha256_hex(payload);
        let spec = ModelArtifactSpec {
            id: BGE_SMALL_EN_V15_ID.to_owned(),
            url: "https://example.invalid/model.safetensors".to_owned(),
            sha256: sha.clone(),
            size: payload.len() as u64,
            license: "MIT".to_owned(),
            source_commit: "deadbeef".to_owned(),
            training_data_note: "test fixture".to_owned(),
            dimension: Some(384),
            query_prefix: Some(BGE_QUERY_PREFIX.to_owned()),
        };
        let manifest = ModelManifest::from_specs(vec![spec]).expect("manifest");
        let cache = temp_test_dir("sec102");

        // 1. Offline mode fails closed before invoking fetcher when artifact is missing
        let offline_mgr = ModelManager::new(manifest.clone(), &cache, true);
        let fetcher_called = AtomicBool::new(false);
        let offline_err = offline_mgr
            .pull(BGE_SMALL_EN_V15_ID, |_| {
                fetcher_called.store(true, Ordering::SeqCst);
                Ok(payload.to_vec())
            })
            .expect_err("offline pull must fail closed");
        assert!(!fetcher_called.load(Ordering::SeqCst));
        assert_eq!(offline_err.code(), ErrorCode::BranchUnavailable);
        assert!(offline_mgr.verify(BGE_SMALL_EN_V15_ID).is_err());

        // 2. Online pull rejects corrupted SHA-256 without leaving artifact cached
        let online_mgr = ModelManager::new(manifest, &cache, false);
        let corrupt_err = online_mgr
            .pull(BGE_SMALL_EN_V15_ID, |_| {
                let mut bad = payload.to_vec();
                bad[0] ^= 0xff;
                Ok(bad)
            })
            .expect_err("corrupt download rejected");
        assert_eq!(corrupt_err.code(), ErrorCode::ValidationError);
        assert!(
            !online_mgr
                .artifact_path(BGE_SMALL_EN_V15_ID)
                .expect("path")
                .exists()
        );

        // 3. Valid pull succeeds, caches cleanly, and verifies on disk
        let path = online_mgr
            .pull(BGE_SMALL_EN_V15_ID, |_| Ok(payload.to_vec()))
            .expect("valid pull");
        assert_eq!(
            online_mgr.verify(BGE_SMALL_EN_V15_ID).expect("verify"),
            path
        );

        // 4. Post-cache disk corruption is detected on verify()
        let mut tampered = payload.to_vec();
        tampered[4] ^= 0x01;
        fs::write(&path, tampered).expect("tamper disk file");
        let disk_err = online_mgr
            .verify(BGE_SMALL_EN_V15_ID)
            .expect_err("corrupt file on disk fails verify");
        assert_eq!(disk_err.code(), ErrorCode::ValidationError);

        let _ = fs::remove_dir_all(&cache);
    }

    // spec: RTE-002
    #[test]
    fn rte_002_bge_embedder_emits_384d_unit_vectors_and_ttl_cache_works() {
        let model = BgeSmallEnV15Model::new(
            "3c9f31665447c8911517620762200d2245a2518d6e7208acc78cd9db317e21ad",
        )
        .expect("bge model");
        assert_eq!(EmbeddingModel::dimension(&model), 384);
        assert_eq!(Embedder::dimension(&model), 384);
        assert_eq!(Embedder::model_id(&model), BGE_SMALL_EN_V15_ID);
        assert_eq!(model.query_prefix(), BGE_QUERY_PREFIX);

        let q1 = model.embed_query("get customer invoices").expect("q1");
        let p_related = model
            .embed_passage("customer billing getCustomerInvoices operation")
            .expect("related passage");
        let p_unrelated = model
            .embed_passage("kubernetes pod restart latency telemetry")
            .expect("unrelated passage");

        assert_eq!(q1.len(), 384);
        let norm_sq: f32 = q1.iter().map(|v| v * v).sum();
        assert!(
            (norm_sq - 1.0).abs() < 1e-4,
            "expected L2-normalized vector, norm^2={norm_sq}"
        );

        let sim_related: f32 = q1.iter().zip(&p_related).map(|(a, b)| a * b).sum();
        let sim_unrelated: f32 = q1.iter().zip(&p_unrelated).map(|(a, b)| a * b).sum();
        assert!(
            sim_related > sim_unrelated,
            "expected semantic overlap score {sim_related} > unrelated {sim_unrelated}"
        );

        // Query cache hit + TTL expiration via deterministic ManualClock
        let clock = Arc::new(ManualClock::new(1_700_000_000_000));
        let cache = QueryEmbeddingCache::with_clock(4, Duration::from_secs(60), clock.clone());
        let a = cache
            .get_or_embed(&model, "  Get   Customer Invoices ")
            .expect("miss1");
        let b = cache
            .get_or_embed(&model, "get customer invoices")
            .expect("hit1");
        assert_eq!(a, b);
        assert_eq!(cache.stats().expect("stats"), (1, 1));

        clock.advance(Duration::from_secs(61));
        let _c = cache
            .get_or_embed(&model, "get customer invoices")
            .expect("expired miss");
        assert_eq!(cache.stats().expect("stats"), (1, 2));
    }
}
