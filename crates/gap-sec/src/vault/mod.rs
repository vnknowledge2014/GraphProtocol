//! Secret vaulting, zeroize-on-drop wrappers, and multi-sink redaction (`SEC-040`..`SEC-042`, `INV-15`).

pub mod provider;
pub mod redactor;
pub mod secret;

#[cfg(test)]
mod tests;

pub use provider::{
    DEFAULT_SECRET_TTL_MS, EnvVault, ExternalKmsBackend, ExternalKmsVault, FileVault,
    KeychainBackend, MacOsSecurityCliBackend, OsKeychainVault, RotatingVault, SecretKey, Vault,
    VaultError,
};
pub use redactor::{REDACTED_TOKEN, Redactor};
pub use secret::{SecretBytes, SecretString};
