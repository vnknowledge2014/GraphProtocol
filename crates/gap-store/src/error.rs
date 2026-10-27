//! Typed storage errors and `GapError` redaction boundary (`INV-15`, `GQL-072`).

use gap_core::error::{ErrorCode, GapError};
use thiserror::Error;

/// Typed error returned by [`crate::ContextStore`] and [`crate::ContextMaintenance`] operations.
///
/// By `INV-15` and `ING-065`, `Display` and `Debug` never contain SQL text, filesystem paths,
/// property payloads, secrets, or identifiers of nodes invisible to the caller.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum StoreError {
    /// Input failed deterministic limit or format validation (`GAP_VALIDATION_ERROR`).
    #[error("invalid `{field}`: {reason}")]
    Invalid {
        /// Validated field identifier.
        field: &'static str,
        /// Sanitized static validation reason.
        reason: &'static str,
    },
    /// Target entity does not exist or is invisible to the caller (`ING-065`).
    #[error("entity not found")]
    NotFound,
    /// Write conflicted with existing store or filesystem state.
    #[error("conflicting store state")]
    Conflict,
    /// Per-tenant node or edge ceiling exceeded (`ING-064`).
    #[error("tenant storage quota exceeded")]
    QuotaExceeded,
    /// On-disk `PRAGMA user_version` is newer than this binary supports.
    #[error("store format version {found} exceeds supported version {supported}")]
    FormatTooNew {
        /// Format version stored in `PRAGMA user_version`.
        found: u32,
        /// Maximum format version supported by this binary.
        supported: u32,
    },
    /// Database file failed header, schema, or `PRAGMA integrity_check` verification.
    #[error("store data is corrupt")]
    Corrupt,
    /// Lock contention exceeded the configured `busy_timeout`.
    #[error("store is busy")]
    Busy,
    /// Opaque internal backend failure.
    #[error("internal storage error")]
    Backend,
}

impl StoreError {
    /// Stable static category label suitable for structured telemetry (`INV-15`).
    #[must_use]
    pub const fn kind_str(&self) -> &'static str {
        match self {
            Self::Invalid { .. } => "invalid",
            Self::NotFound => "not_found",
            Self::Conflict => "conflict",
            Self::QuotaExceeded => "quota_exceeded",
            Self::FormatTooNew { .. } => "format_too_new",
            Self::Corrupt => "corrupt",
            Self::Busy => "busy",
            Self::Backend => "backend",
        }
    }

    /// Converts a [`StoreError`] into a wire-safe [`GapError`] (`INV-15`, `ADR-023`).
    ///
    /// [`StoreError::Invalid`] maps to `GAP_VALIDATION_ERROR`; every other variant logs a
    /// sanitized warning keyed by `correlation_id` (without props, paths, SQL, or secrets) and
    /// returns the opaque `GapError::internal(correlation_id)`.
    #[must_use]
    pub fn into_gap_error(self, correlation_id: impl Into<String>) -> GapError {
        let cid = correlation_id.into();
        match self {
            Self::Invalid { field, reason } => GapError::new(
                ErrorCode::ValidationError,
                format!("invalid `{field}`: {reason}"),
            )
            .with_correlation_id(cid),
            other => {
                tracing::warn!(
                    correlation_id = %cid,
                    store_error_kind = other.kind_str(),
                    store_error = %other,
                    "context store operation failed"
                );
                GapError::internal(cid)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use gap_core::error::ErrorCode;

    use super::StoreError;

    // spec: INV-15, GQL-072 (ADR-023)
    #[test]
    fn inv_15_store_error_into_gap_error_maps_invalid_and_redacts_internal_variants() {
        let invalid = StoreError::Invalid {
            field: "label",
            reason: "exceeds 512 bytes",
        };
        let gap_invalid = invalid.into_gap_error("corr-001");
        assert_eq!(gap_invalid.code(), ErrorCode::ValidationError);
        assert_eq!(gap_invalid.correlation_id(), Some("corr-001"));
        assert_eq!(gap_invalid.message(), "invalid `label`: exceeds 512 bytes");
        assert!(!gap_invalid.retry().retryable);

        let non_validation_cases = [
            StoreError::NotFound,
            StoreError::Conflict,
            StoreError::QuotaExceeded,
            StoreError::FormatTooNew {
                found: 99,
                supported: 2,
            },
            StoreError::Corrupt,
            StoreError::Busy,
            StoreError::Backend,
        ];
        for (idx, err) in non_validation_cases.into_iter().enumerate() {
            let rendered = err.to_string();
            assert!(!rendered.contains("SELECT"));
            assert!(!rendered.contains("/tmp"));
            assert!(!rendered.contains("secret"));

            let cid = format!("corr-internal-{idx}");
            let gap_err = err.into_gap_error(&cid);
            assert_eq!(gap_err.code(), ErrorCode::Internal);
            assert_eq!(gap_err.message(), "internal error");
            assert_eq!(gap_err.correlation_id(), Some(cid.as_str()));
            assert!(!gap_err.retry().retryable);
        }
    }
}
