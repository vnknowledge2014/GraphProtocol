//! Zeroized, non-displaying secret containers (`SEC-040`, `SEC-041`, `INV-15`).

use std::fmt;
use std::hint::black_box;
use std::sync::atomic::{Ordering, compiler_fence};

use crate::authn::crypto::ct_eq;

const REDACTED_DISPLAY: &str = "***";

fn scrub_bytes(buf: &mut [u8]) {
    for byte in buf.iter_mut() {
        *byte = 0;
    }
    let _ = black_box(buf.as_mut_ptr());
    compiler_fence(Ordering::SeqCst);
}

/// Heap-allocated byte secret that zeroizes on [`Drop`] and formats as `***` (`SEC-040`, `INV-15`).
///
/// Intentionally does not implement `Serialize`, `Deref`, or transparent `Clone` beyond explicit
/// secret-preserving duplication so secrets never leak via formatting or derivation macros.
pub struct SecretBytes {
    bytes: Vec<u8>,
}

impl SecretBytes {
    /// Wraps raw secret bytes.
    #[must_use]
    pub fn new(bytes: Vec<u8>) -> Self {
        Self { bytes }
    }

    /// Exposes the underlying secret byte slice strictly at the call boundary where needed.
    #[must_use]
    pub fn expose_secret(&self) -> &[u8] {
        &self.bytes
    }

    /// Constant-time comparison against `other`.
    #[must_use]
    pub fn ct_equals(&self, other: &[u8]) -> bool {
        ct_eq(&self.bytes, other)
    }

    /// Explicit secret duplication when callers intentionally pass an owned handle across tasks.
    #[must_use]
    pub fn duplicate(&self) -> Self {
        Self {
            bytes: self.bytes.clone(),
        }
    }

    /// Returns whether the wrapped secret is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }
}

impl Drop for SecretBytes {
    fn drop(&mut self) {
        scrub_bytes(&mut self.bytes);
    }
}

impl fmt::Debug for SecretBytes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(REDACTED_DISPLAY)
    }
}

impl fmt::Display for SecretBytes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(REDACTED_DISPLAY)
    }
}

impl PartialEq for SecretBytes {
    fn eq(&self, other: &Self) -> bool {
        self.ct_equals(other.expose_secret())
    }
}

impl Eq for SecretBytes {}

/// UTF-8 secret string backed by a zeroizable byte buffer that formats as `***` (`SEC-040`, `INV-15`).
pub struct SecretString {
    utf8_bytes: Vec<u8>,
}

impl SecretString {
    /// Wraps an owned secret string, taking ownership of its UTF-8 buffer for scrub-on-drop.
    #[must_use]
    pub fn new(value: String) -> Self {
        Self {
            utf8_bytes: value.into_bytes(),
        }
    }

    /// Wraps a borrowed secret string slice by copying its UTF-8 bytes into a zeroizing buffer.
    #[must_use]
    pub fn from_slice(value: &str) -> Self {
        Self::new(value.to_owned())
    }

    /// Exposes the underlying secret string slice at the boundary where authentication needs it.
    #[must_use]
    pub fn expose_secret(&self) -> &str {
        // Invariant: `utf8_bytes` is constructed solely from valid UTF-8 `String` / `&str`.
        std::str::from_utf8(&self.utf8_bytes).unwrap_or("")
    }

    /// Constant-time comparison against `other`.
    #[must_use]
    pub fn ct_equals(&self, other: &str) -> bool {
        ct_eq(&self.utf8_bytes, other.as_bytes())
    }

    /// Explicit secret duplication when callers intentionally pass an owned handle across tasks.
    #[must_use]
    pub fn duplicate(&self) -> Self {
        Self {
            utf8_bytes: self.utf8_bytes.clone(),
        }
    }

    /// Returns whether the secret string is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.utf8_bytes.is_empty()
    }
}

impl Drop for SecretString {
    fn drop(&mut self) {
        scrub_bytes(&mut self.utf8_bytes);
    }
}

impl fmt::Debug for SecretString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(REDACTED_DISPLAY)
    }
}

impl fmt::Display for SecretString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(REDACTED_DISPLAY)
    }
}

impl PartialEq for SecretString {
    fn eq(&self, other: &Self) -> bool {
        ct_eq(&self.utf8_bytes, &other.utf8_bytes)
    }
}

impl Eq for SecretString {}
