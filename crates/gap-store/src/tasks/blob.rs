//! Tenant-partitioned artifact blob offloading (`RUN-086`, `INV-09`).
//!
//! Task artifacts larger than [`INLINE_ARTIFACT_MAX_BYTES`] (`1 MiB`) are persisted on the local
//! filesystem under a content-addressed path and referenced by `{ uri, sha256 }` rather than being
//! embedded inline in [`crate::tasks::TaskRecord`] or [`crate::tasks::TaskEventRecord`].

use std::fmt::Write as _;
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};

use gap_core::ids::{TaskId, TenantScope};
use serde::{Deserialize, Serialize};

use crate::error::StoreError;

/// Inline artifact threshold (`1 MiB`, `RUN-086`).
pub const INLINE_ARTIFACT_MAX_BYTES: usize = 1024 * 1024;

/// Maximum artifact size accepted by [`FsArtifactBlobStore`] (`64 MiB` safety cap).
pub const MAX_ARTIFACT_BLOB_BYTES: usize = 64 * 1024 * 1024;

/// Externalized artifact reference stored in task records when payload `> 1 MiB` (`RUN-086`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactRef {
    /// Storage URI (`file://...`) pointing at the immutable blob.
    pub uri: String,
    /// Lowercase hex BLAKE3 integrity digest of the persisted bytes.
    pub sha256: String,
    /// Uncompressed payload size in bytes.
    pub size_bytes: usize,
    /// Declared MIME media type (e.g. `application/json`).
    pub mime_type: String,
}

/// Stored representation returned by [`FsArtifactBlobStore::put_artifact`] (`RUN-086`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum StoredArtifact {
    /// Payload fit within [`INLINE_ARTIFACT_MAX_BYTES`] and is kept inline.
    Inline {
        /// UTF-8 text or encoded payload data.
        data: String,
        /// Declared MIME media type.
        mime_type: String,
        /// Payload size in bytes.
        size_bytes: usize,
    },
    /// Payload exceeded [`INLINE_ARTIFACT_MAX_BYTES`] and was written to blob storage.
    Reference(ArtifactRef),
}

/// Local filesystem content-addressed blob store partitioned by tenant (`RUN-086`, `INV-09`).
#[derive(Debug, Clone)]
pub struct FsArtifactBlobStore {
    root: PathBuf,
    inline_threshold_bytes: usize,
}

impl FsArtifactBlobStore {
    /// Opens or initializes an artifact blob store rooted at `root`.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Backend`] if the root directory cannot be created.
    pub fn new(root: impl Into<PathBuf>) -> Result<Self, StoreError> {
        let root = root.into();
        fs::create_dir_all(&root).map_err(|_| StoreError::Backend)?;
        Ok(Self {
            root,
            inline_threshold_bytes: INLINE_ARTIFACT_MAX_BYTES,
        })
    }

    /// Returns the filesystem root directory of this blob store.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Persists an artifact payload for `(tenant, task_id)`, storing payloads strictly larger than
    /// [`INLINE_ARTIFACT_MAX_BYTES`] out-of-line by `{ uri, sha256 }` (`RUN-086`).
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Invalid`] if `mime_type` is empty or `bytes` exceeds the hard cap,
    /// or [`StoreError::Backend`] on I/O failure.
    pub fn put_artifact(
        &self,
        tenant: &TenantScope,
        task_id: TaskId,
        mime_type: &str,
        bytes: &[u8],
    ) -> Result<StoredArtifact, StoreError> {
        if mime_type.trim().is_empty() {
            return Err(StoreError::Invalid {
                field: "mime_type",
                reason: "mime_type must not be empty",
            });
        }
        if bytes.len() > MAX_ARTIFACT_BLOB_BYTES {
            return Err(StoreError::Invalid {
                field: "artifact_bytes",
                reason: "artifact exceeds 64 MiB limit",
            });
        }
        if bytes.len() <= self.inline_threshold_bytes {
            let text = String::from_utf8(bytes.to_vec()).map_err(|_| StoreError::Invalid {
                field: "artifact_bytes",
                reason: "inline artifact must be valid UTF-8",
            })?;
            return Ok(StoredArtifact::Inline {
                data: text,
                mime_type: mime_type.to_owned(),
                size_bytes: bytes.len(),
            });
        }

        let digest_hex = blake3_hex(bytes);
        let dir = self
            .root
            .join(tenant_shard(tenant.tenant().as_str()))
            .join(task_id.to_string());
        fs::create_dir_all(&dir).map_err(|_| StoreError::Backend)?;

        let blob_path = dir.join(format!("{digest_hex}.blob"));
        if !blob_path.exists() {
            let mut file = File::create(&blob_path).map_err(|_| StoreError::Backend)?;
            file.write_all(bytes).map_err(|_| StoreError::Backend)?;
            file.sync_all().map_err(|_| StoreError::Backend)?;
        }

        Ok(StoredArtifact::Reference(ArtifactRef {
            uri: format!("file://{}", blob_path.display()),
            sha256: digest_hex,
            size_bytes: bytes.len(),
            mime_type: mime_type.to_owned(),
        }))
    }

    /// Reads and integrity-checks an offloaded blob referenced by `artifact` under `tenant` (`INV-09`).
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Invalid`] if `artifact` is malformed or points outside the tenant shard,
    /// [`StoreError::NotFound`] if missing, or [`StoreError::Corrupt`] if digest verification fails.
    pub fn read_artifact(
        &self,
        tenant: &TenantScope,
        task_id: TaskId,
        artifact: &ArtifactRef,
    ) -> Result<Vec<u8>, StoreError> {
        let shard = tenant_shard(tenant.tenant().as_str());
        let expected_path = self
            .root
            .join(&shard)
            .join(task_id.to_string())
            .join(format!("{}.blob", artifact.sha256));
        if !is_lower_hex_64(&artifact.sha256) {
            return Err(StoreError::Invalid {
                field: "sha256",
                reason: "expected 64 lowercase hex characters",
            });
        }
        let expected_uri = format!("file://{}", expected_path.display());
        if artifact.uri != expected_uri {
            return Err(StoreError::NotFound);
        }
        let bytes = match fs::read(&expected_path) {
            Ok(b) => b,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Err(StoreError::NotFound);
            }
            Err(_) => return Err(StoreError::Backend),
        };
        if blake3_hex(&bytes) != artifact.sha256 || bytes.len() != artifact.size_bytes {
            return Err(StoreError::Corrupt);
        }
        Ok(bytes)
    }
}

fn tenant_shard(tenant_id: &str) -> String {
    let digest = blake3::hash(tenant_id.as_bytes());
    let mut out = String::with_capacity(22);
    out.push_str("t_");
    for b in &digest.as_bytes()[..10] {
        let _ = write!(&mut out, "{b:02x}");
    }
    out
}

fn blake3_hex(bytes: &[u8]) -> String {
    let digest = blake3::hash(bytes);
    let mut out = String::with_capacity(64);
    for b in digest.as_bytes() {
        let _ = write!(&mut out, "{b:02x}");
    }
    out
}

fn is_lower_hex_64(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}
