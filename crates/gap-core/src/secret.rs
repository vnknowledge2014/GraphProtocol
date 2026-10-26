//! Redacted, zeroize-on-drop `Secret<T>` wrapper (`INV-15`, `SEC-040`, `SEC-041`).

use std::fmt;

use serde::{Serialize, Serializer};
use zeroize::Zeroize;

/// Fixed redaction marker emitted by [`fmt::Debug`] and [`fmt::Display`] (`SEC-041`, `INV-15`).
pub const REDACTED_MARKER: &str = "***";

/// Wrapper around sensitive material (`T: Zeroize`) enforcing `INV-15`:
///
/// - [`fmt::Debug`] and [`fmt::Display`] always print [`REDACTED_MARKER`] (`"***"`).
/// - [`Drop`] zeroizes the inner memory via [`Zeroize::zeroize`].
/// - [`Serialize`] refuses to serialize and never emits the secret value (`SEC-040`, `INV-15`).
pub struct Secret<T: Zeroize> {
    inner: T,
}

impl<T: Zeroize> Secret<T> {
    /// Wraps `value` in a [`Secret`].
    #[must_use]
    pub const fn new(value: T) -> Self {
        Self { inner: value }
    }

    /// Explicitly borrows the underlying secret material.
    ///
    /// Callers MUST ensure the borrowed reference never flows into logs, traces, metrics,
    /// snapshots, error messages, or model inputs (`INV-15`).
    #[must_use]
    pub const fn expose_secret(&self) -> &T {
        &self.inner
    }
}

impl<T: Zeroize + Clone> Clone for Secret<T> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl<T: Zeroize> Zeroize for Secret<T> {
    fn zeroize(&mut self) {
        self.inner.zeroize();
    }
}

impl<T: Zeroize> Drop for Secret<T> {
    fn drop(&mut self) {
        self.inner.zeroize();
    }
}

impl<T: Zeroize> fmt::Debug for Secret<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(REDACTED_MARKER)
    }
}

impl<T: Zeroize> fmt::Display for Secret<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(REDACTED_MARKER)
    }
}

impl<T: Zeroize> Serialize for Secret<T> {
    fn serialize<S>(&self, _serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        Err(serde::ser::Error::custom(
            "Secret<T> must never be serialized (INV-15)",
        ))
    }
}

/// Convenience alias for `Secret<String>`.
pub type SecretString = Secret<String>;

/// Convenience alias for `Secret<Vec<u8>>`.
pub type SecretBytes = Secret<Vec<u8>>;

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    struct ZeroizeProbe {
        bytes: Vec<u8>,
        zeroed_count: Arc<AtomicUsize>,
    }

    impl Zeroize for ZeroizeProbe {
        fn zeroize(&mut self) {
            self.bytes.zeroize();
            if self.bytes.iter().all(|&b| b == 0) {
                self.zeroed_count.fetch_add(1, Ordering::SeqCst);
            }
        }
    }

    // spec: INV-15, SEC-040, SEC-041
    #[test]
    fn inv_15_secret_redacts_debug_display_serialize_and_zeroizes() {
        let canary = "sk-live-canary-9876543210-do-not-leak";
        let secret = SecretString::new(canary.to_owned());

        assert_eq!(secret.expose_secret(), canary);
        let dbg = format!("{secret:?}");
        let disp = format!("{secret}");
        assert_eq!(dbg, REDACTED_MARKER);
        assert_eq!(disp, REDACTED_MARKER);
        assert!(!dbg.contains(canary));
        assert!(!disp.contains(canary));

        let ser_err = serde_json::to_string(&secret).expect_err("Secret<T> Serialize must fail");
        assert!(!ser_err.to_string().contains(canary));

        let zeroed_count = Arc::new(AtomicUsize::new(0));
        {
            let probe = Secret::new(ZeroizeProbe {
                bytes: canary.as_bytes().to_vec(),
                zeroed_count: Arc::clone(&zeroed_count),
            });
            assert_eq!(probe.expose_secret().bytes, canary.as_bytes());
        }
        assert_eq!(
            zeroed_count.load(Ordering::SeqCst),
            1,
            "Secret<T> Drop must invoke Zeroize and wipe secret bytes"
        );
    }
}
