//! Guarded specification fetcher (`WP-M4-01`, `ING-011`).
//!
//! By default [`DenyAllSourceFetcher`] rejects all remote fetches until `EgressGuard` (`WP-M6-08`)
//! is wired in, ensuring extractors remain pure functions of [`crate::cisg::SourceHandle`] bytes.

use std::fmt;
use std::time::Duration;

use crate::cisg::sha256_hex;

/// Default maximum specification download size (`50 MiB`, `spec/05-ingestion.md` §2).
pub const DEFAULT_FETCH_MAX_BYTES: usize = 50 * 1024 * 1024;

/// Default fetch wall-clock timeout (`30 s`).
pub const DEFAULT_FETCH_TIMEOUT: Duration = Duration::from_secs(30);

/// Resource limits governing a single remote specification fetch (`ING-011`, `ING-012`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FetchLimits {
    /// Maximum tolerated response body size in bytes.
    pub max_bytes: usize,
    /// Maximum wall-clock timeout.
    pub timeout: Duration,
}

impl Default for FetchLimits {
    fn default() -> Self {
        Self {
            max_bytes: DEFAULT_FETCH_MAX_BYTES,
            timeout: DEFAULT_FETCH_TIMEOUT,
        }
    }
}

/// Verified specification bytes returned by [`SourceFetcher::fetch`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchedSource {
    /// Canonicalized source URL.
    pub canonical_url: String,
    /// Optional normalized MIME type reported by upstream.
    pub content_type: Option<String>,
    /// Raw immutable response bytes.
    pub bytes: Vec<u8>,
    /// Lowercase hex SHA-256 digest of `bytes` (`ING-010`).
    pub sha256: String,
}

impl FetchedSource {
    /// Validates response size against `limits.max_bytes` and computes its SHA-256 hash.
    pub fn from_bytes(
        canonical_url: impl Into<String>,
        content_type: Option<String>,
        bytes: Vec<u8>,
        limits: &FetchLimits,
    ) -> Result<Self, FetchError> {
        if bytes.len() > limits.max_bytes {
            return Err(FetchError::TooLarge {
                bytes: bytes.len(),
                limit: limits.max_bytes,
            });
        }
        let sha256 = sha256_hex(&bytes);
        Ok(Self {
            canonical_url: canonical_url.into(),
            content_type,
            bytes,
            sha256,
        })
    }
}

/// Typed failure returned by [`SourceFetcher`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FetchError {
    /// Fetch blocked by egress / SSRF policy (including default deny-all mode).
    DeniedByPolicy {
        /// Sanitized policy denial reason.
        reason: String,
    },
    /// Remote payload exceeded `FetchLimits::max_bytes`.
    TooLarge {
        /// Observed byte length.
        bytes: usize,
        /// Configured ceiling.
        limit: usize,
    },
    /// Fetch exceeded `FetchLimits::timeout`.
    Timeout {
        /// Configured timeout duration.
        limit: Duration,
    },
    /// Malformed, non-HTTP(S), or relative URL rejected prior to network dial.
    InvalidUrl {
        /// Sanitized validation error reason.
        reason: String,
    },
}

impl fmt::Display for FetchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DeniedByPolicy { reason } => write!(f, "fetch denied by policy: {reason}"),
            Self::TooLarge { bytes, limit } => {
                write!(f, "fetched response {bytes} bytes exceeds limit {limit}")
            }
            Self::Timeout { limit } => write!(f, "fetch timed out after {:?}", limit),
            Self::InvalidUrl { reason } => write!(f, "invalid fetch URL: {reason}"),
        }
    }
}

impl std::error::Error for FetchError {}

/// Controlled specification fetch interface used outside sandboxed extractors (`ING-011`, `ING-070`).
pub trait SourceFetcher: Send + Sync {
    /// Fetches a specification from `url` strictly within `limits`.
    fn fetch(&self, url: &str, limits: &FetchLimits) -> Result<FetchedSource, FetchError>;
}

/// Default deny-all [`SourceFetcher`] active until `EgressGuard` (`WP-M6-08`) is wired (`ING-011`).
#[derive(Debug, Default, Clone, Copy)]
pub struct DenyAllSourceFetcher;

impl DenyAllSourceFetcher {
    /// Creates a new deny-all fetcher instance.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

impl SourceFetcher for DenyAllSourceFetcher {
    fn fetch(&self, url: &str, _limits: &FetchLimits) -> Result<FetchedSource, FetchError> {
        if url.trim().is_empty() || (!url.starts_with("https://") && !url.starts_with("http://")) {
            return Err(FetchError::InvalidUrl {
                reason: "only explicit http:// or https:// URLs are valid source targets"
                    .to_owned(),
            });
        }
        Err(FetchError::DeniedByPolicy {
            reason: "outbound source fetching is disabled by default without an allow-listed EgressGuard"
                .to_owned(),
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    // spec: ING-011
    #[test]
    fn fetch_deny_all_rejects_network_requests_and_validates_urls() {
        let fetcher = DenyAllSourceFetcher::new();
        let limits = FetchLimits::default();

        let err_https = fetcher
            .fetch("https://example.com/openapi.json", &limits)
            .expect_err("deny-all blocks https");
        assert!(matches!(err_https, FetchError::DeniedByPolicy { .. }));

        let err_rel = fetcher
            .fetch("file:///etc/passwd", &limits)
            .expect_err("invalid scheme");
        assert!(matches!(err_rel, FetchError::InvalidUrl { .. }));

        // Bounded constructor checks max_bytes cleanly.
        let small = FetchLimits {
            max_bytes: 4,
            timeout: Duration::from_secs(1),
        };
        assert!(matches!(
            FetchedSource::from_bytes("https://example.com/spec", None, b"12345".to_vec(), &small),
            Err(FetchError::TooLarge { bytes: 5, limit: 4 })
        ));
    }
}
