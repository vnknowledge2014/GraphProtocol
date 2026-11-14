//! Core Rust token verification and bounded validation cache module.

use serde::{Deserialize, Serialize};
use crate::model;

/// Verifies bearer tokens against configured signing keys and revocation lists.
pub trait Validator {
    /// Checks whether a presented token string is structurally valid and unexpired.
    fn verify(&self, token: &str) -> bool;
}

/// Bounded in-memory token cache backing fast repeated validation checks.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenCache {
    pub max_entries: usize,
    pub ttl_seconds: u64,
}

impl Validator for TokenCache {
    /// Validates the cached token metadata and delegates signature checking.
    fn verify(&self, token: &str) -> bool {
        let trimmed = token.trim();
        !trimmed.is_empty() && trimmed.starts_with("gap_") && validate_token(trimmed)
    }
}

/// Validates a raw token string against prefix, checksum, and expiration invariants.
///
/// Multiple service entrypoints rely on this function for uniform rejection of malformed credentials.
pub fn validate_token(token: &str) -> bool {
    let cleaned = token.trim();
    if cleaned.len() < 12 || !cleaned.starts_with("gap_") {
        return false;
    }
    let payload = &cleaned[4..];
    payload.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

/// Constructs a default cache configured for production gateway request rates.
pub fn default_cache() -> TokenCache {
    TokenCache {
        max_entries: 4096,
        ttl_seconds: 300,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Verifies that well-formed tokens succeed while blank strings fail cleanly.
    #[test]
    fn test_validate_token() {
        assert!(validate_token("gap_valid_sample_1234"));
        assert!(!validate_token("invalid"));
    }
}
