//! Executable allow-list with canonical path enforcement and per-launch SHA-256 re-verification (`SEC-061`, `CMP-060`).

use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use gap_core::error::{ErrorCode, GapError};

use crate::authn::crypto::{ct_eq, hex_encode, sha256};

/// Verified executable metadata returned on every allowed launch (`SEC-061`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedExecBinary {
    /// Canonicalized filesystem path on disk.
    pub canonical_path: PathBuf,
    /// Lowercase hex SHA-256 digest verified at launch time.
    pub sha256_hex: String,
}

/// Errors emitted when registering or validating an executable against [`ExecAllowList`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AllowListError {
    /// Supplied path is relative or empty.
    NonAbsolutePath(PathBuf),
    /// Binary is not present in the allow-list (`SEC-061`).
    NotAllowListed(PathBuf),
    /// Canonical path changed (e.g. symlink retargeting after registration).
    CanonicalPathMismatch {
        /// Registered canonical path.
        expected: PathBuf,
        /// Observed canonical path on disk.
        actual: PathBuf,
    },
    /// Failed to stat or read binary bytes.
    IoError(String),
    /// Target is not a regular file (directory, FIFO, device).
    NotARegularFile(PathBuf),
    /// Live SHA-256 digest on disk diverged from the pinned hash (`SEC-061` binary replacement).
    HashMismatch {
        /// Binary path.
        path: PathBuf,
        /// Pinned expected digest.
        expected: String,
        /// Live digest computed before spawn.
        actual: String,
    },
    /// Supplied hash string is not a 64-char hex SHA-256 digest.
    InvalidHashFormat(String),
}

impl AllowListError {
    /// Converts an allow-list error into a [`GapError`].
    #[must_use]
    pub fn into_gap_error(self) -> GapError {
        let code = match &self {
            Self::NonAbsolutePath(_) | Self::InvalidHashFormat(_) => ErrorCode::ValidationError,
            Self::NotAllowListed(_)
            | Self::CanonicalPathMismatch { .. }
            | Self::HashMismatch { .. } => ErrorCode::Forbidden,
            Self::IoError(_) | Self::NotARegularFile(_) => ErrorCode::BranchUnavailable,
        };
        GapError::new(code, self.to_string())
    }
}

impl fmt::Display for AllowListError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NonAbsolutePath(p) => {
                write!(
                    f,
                    "exec allow-list requires absolute binary path, got `{}`",
                    p.display()
                )
            }
            Self::NotAllowListed(p) => {
                write!(
                    f,
                    "executable `{}` is not in branch exec allow-list",
                    p.display()
                )
            }
            Self::CanonicalPathMismatch { expected, actual } => write!(
                f,
                "executable canonical path changed from `{}` to `{}`",
                expected.display(),
                actual.display()
            ),
            Self::IoError(msg) => write!(f, "executable allow-list I/O error: {msg}"),
            Self::NotARegularFile(p) => {
                write!(f, "executable `{}` is not a regular file", p.display())
            }
            Self::HashMismatch {
                path,
                expected,
                actual,
            } => write!(
                f,
                "executable `{}` SHA-256 changed: expected {expected}, got {actual}",
                path.display()
            ),
            Self::InvalidHashFormat(s) => write!(f, "invalid SHA-256 hex digest `{s}`"),
        }
    }
}

impl std::error::Error for AllowListError {}

/// Administrator-approved executable allow-list keyed by canonical binary path (`SEC-061`, `CMP-060`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExecAllowList {
    entries: BTreeMap<PathBuf, String>,
}

impl ExecAllowList {
    /// Creates an empty executable allow-list.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers an executable by canonicalizing `binary_path` and pinning its SHA-256 digest.
    ///
    /// If `expected_sha256` is `Some`, fails if the live file digest differs; otherwise computes and
    /// pins the current on-disk SHA-256 digest. Also records the raw `binary_path` mapping ONLY if
    /// it equals the canonical path, and always resolves symlinks strictly.
    ///
    /// # Errors
    ///
    /// Returns [`AllowListError`] if `binary_path` is relative, unreadable, non-regular, or hash-mismatched.
    pub fn register(
        &mut self,
        binary_path: &Path,
        expected_sha256: Option<&str>,
    ) -> Result<String, AllowListError> {
        if !binary_path.is_absolute() {
            return Err(AllowListError::NonAbsolutePath(binary_path.to_path_buf()));
        }
        let canonical = fs::canonicalize(binary_path)
            .map_err(|err| AllowListError::IoError(format!("{}: {err}", binary_path.display())))?;
        let actual_hash = hash_regular_file(&canonical)?;

        if let Some(expected_raw) = expected_sha256 {
            let expected = expected_raw.trim().to_ascii_lowercase();
            if expected.len() != 64
                || !expected
                    .bytes()
                    .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
            {
                return Err(AllowListError::InvalidHashFormat(expected_raw.to_owned()));
            }
            if !ct_eq(expected.as_bytes(), actual_hash.as_bytes()) {
                return Err(AllowListError::HashMismatch {
                    path: canonical,
                    expected,
                    actual: actual_hash,
                });
            }
        }

        self.entries.insert(canonical, actual_hash.clone());
        Ok(actual_hash)
    }

    /// Returns the number of allow-listed executables.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Returns `true` if no executables are registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Re-verifies path allow-listing, canonical symlink resolution, and live SHA-256 hash on EVERY launch (`SEC-061`).
    ///
    /// # Errors
    ///
    /// Returns [`AllowListError`] if the binary is absent, non-absolute, replaced, or tampered on disk.
    pub fn verify_before_launch(
        &self,
        binary_path: &Path,
    ) -> Result<VerifiedExecBinary, AllowListError> {
        if !binary_path.is_absolute() {
            return Err(AllowListError::NonAbsolutePath(binary_path.to_path_buf()));
        }
        let canonical = fs::canonicalize(binary_path)
            .map_err(|err| AllowListError::IoError(format!("{}: {err}", binary_path.display())))?;
        let Some(expected_hash) = self.entries.get(&canonical) else {
            return Err(AllowListError::NotAllowListed(binary_path.to_path_buf()));
        };
        let actual_hash = hash_regular_file(&canonical)?;
        if !ct_eq(expected_hash.as_bytes(), actual_hash.as_bytes()) {
            return Err(AllowListError::HashMismatch {
                path: canonical,
                expected: expected_hash.clone(),
                actual: actual_hash,
            });
        }
        Ok(VerifiedExecBinary {
            canonical_path: canonical,
            sha256_hex: actual_hash,
        })
    }
}

fn hash_regular_file(path: &Path) -> Result<String, AllowListError> {
    let meta = fs::metadata(path)
        .map_err(|err| AllowListError::IoError(format!("{}: {err}", path.display())))?;
    if !meta.is_file() {
        return Err(AllowListError::NotARegularFile(path.to_path_buf()));
    }
    let bytes = fs::read(path)
        .map_err(|err| AllowListError::IoError(format!("{}: {err}", path.display())))?;
    Ok(hex_encode(&sha256(&bytes)))
}
