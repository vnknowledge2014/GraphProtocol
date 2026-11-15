//! Pluggable `Vault` providers (`env`, `file` `0600`, OS keychain, external KMS, hot rotation) (`SEC-040`).

use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, RwLock};
use std::time::UNIX_EPOCH;

use gap_core::clock::Clock;

use super::secret::{SecretBytes, SecretString};

/// Default TTL (`60 s`) before [`RotatingVault`] re-fetches a cached secret without restart (`SEC-040`).
pub const DEFAULT_SECRET_TTL_MS: i64 = 60_000;

/// Validated logical secret identifier.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SecretKey(String);

impl SecretKey {
    /// Validates a logical secret key (`1..=128` ASCII `[A-Za-z0-9._:/-]`).
    ///
    /// # Errors
    ///
    /// Returns [`VaultError::InvalidKey`] if empty, oversized, or containing disallowed bytes.
    pub fn new(raw: impl Into<String>) -> Result<Self, VaultError> {
        let s = raw.into();
        let valid = !s.is_empty()
            && s.len() <= 128
            && s.bytes().all(|b| {
                b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b':' | b'/')
            });
        if !valid {
            return Err(VaultError::InvalidKey { key: s });
        }
        Ok(Self(s))
    }

    /// Returns the validated secret key string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Typed vault failure (`INV-15`: never contains secret material in Display or Debug).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VaultError {
    /// Requested secret key violated naming constraints or contained path traversal.
    InvalidKey {
        /// Key identifier (never secret bytes).
        key: String,
    },
    /// Requested secret key does not exist in this provider.
    NotFound {
        /// Key identifier.
        key: String,
    },
    /// Secret file on disk violated `0600` (`mode & 0o077 != 0`) permission requirements (`SEC-040`).
    InsecurePermissions {
        /// Key identifier.
        key: String,
        /// Observed Unix permission bits (`& 0o777`).
        mode: u32,
    },
    /// Secret file contained invalid UTF-8 when read as a text secret.
    InvalidUtf8 {
        /// Key identifier.
        key: String,
    },
    /// Underlying filesystem, OS keychain, or KMS provider failed.
    ProviderUnavailable {
        /// Sanitized static failure reason.
        reason: &'static str,
    },
}

impl fmt::Display for VaultError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidKey { key } => write!(f, "invalid secret key `{key}`"),
            Self::NotFound { key } => write!(f, "secret `{key}` not found"),
            Self::InsecurePermissions { key, mode } => write!(
                f,
                "secret file `{key}` has insecure permissions {mode:#05o}; 0600 required"
            ),
            Self::InvalidUtf8 { key } => write!(f, "secret `{key}` is not valid UTF-8"),
            Self::ProviderUnavailable { reason } => write!(f, "vault provider error: {reason}"),
        }
    }
}

impl std::error::Error for VaultError {}

/// Synchronous credential vault abstraction (`SEC-040`).
///
/// Secrets are fetched on demand at connector boundary setup and held in [`SecretString`] /
/// [`SecretBytes`] containers rather than long-lived plaintext configs.
pub trait Vault: Send + Sync {
    /// Fetches a UTF-8 secret string for `key`.
    fn get_secret(&self, key: &str) -> Result<SecretString, VaultError>;

    /// Fetches raw binary secret bytes for `key`.
    fn get_secret_bytes(&self, key: &str) -> Result<SecretBytes, VaultError> {
        self.get_secret(key)
            .map(|s| SecretBytes::new(s.expose_secret().as_bytes().to_vec()))
    }

    /// Optional backend version/mtime fingerprint used by [`RotatingVault`] to detect edits early.
    fn version_hint(&self, _key: &str) -> Option<u64> {
        None
    }
}

/// Environment-variable secret provider reading `<prefix><KEY>` (`SEC-040`).
#[derive(Debug, Clone)]
pub struct EnvVault {
    prefix: String,
    override_env: Option<BTreeMap<String, String>>,
}

impl EnvVault {
    /// Creates an [`EnvVault`] inspecting process environment variables with `prefix`.
    #[must_use]
    pub fn with_prefix(prefix: impl Into<String>) -> Self {
        Self {
            prefix: prefix.into(),
            override_env: None,
        }
    }

    /// Creates a hermetic [`EnvVault`] backed by an explicit map (deterministic tests).
    #[must_use]
    pub fn from_map(prefix: impl Into<String>, vars: BTreeMap<String, String>) -> Self {
        Self {
            prefix: prefix.into(),
            override_env: Some(vars),
        }
    }

    fn env_var_name(&self, key: &SecretKey) -> String {
        let mut out = self.prefix.clone();
        for ch in key.as_str().chars() {
            if ch.is_ascii_alphanumeric() {
                out.push(ch.to_ascii_uppercase());
            } else {
                out.push('_');
            }
        }
        out
    }
}

impl Vault for EnvVault {
    fn get_secret(&self, key: &str) -> Result<SecretString, VaultError> {
        let valid_key = SecretKey::new(key)?;
        let var_name = self.env_var_name(&valid_key);
        let val = if let Some(map) = &self.override_env {
            map.get(&var_name).cloned()
        } else {
            std::env::var(&var_name).ok()
        };
        let Some(secret) = val else {
            return Err(VaultError::NotFound {
                key: valid_key.as_str().to_owned(),
            });
        };
        Ok(SecretString::new(secret))
    }
}

/// Filesystem secret directory provider strictly enforcing Unix `0600` (`mode & 0o077 == 0`) (`SEC-040`).
#[derive(Debug, Clone)]
pub struct FileVault {
    root: PathBuf,
}

impl FileVault {
    /// Creates a [`FileVault`] rooted at `root`.
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    fn resolve_path(&self, key: &str) -> Result<PathBuf, VaultError> {
        let valid = SecretKey::new(key)?;
        let raw = valid.as_str();
        if raw.contains("..") || raw.contains('/') || raw.contains('\\') {
            return Err(VaultError::InvalidKey {
                key: raw.to_owned(),
            });
        }
        Ok(self.root.join(raw))
    }

    fn enforce_permissions(key: &str, meta: &fs::Metadata) -> Result<(), VaultError> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = meta.permissions().mode() & 0o777;
            if mode & 0o077 != 0 {
                return Err(VaultError::InsecurePermissions {
                    key: key.to_owned(),
                    mode,
                });
            }
        }
        #[cfg(not(unix))]
        let _ = (key, meta);
        Ok(())
    }
}

impl Vault for FileVault {
    fn get_secret(&self, key: &str) -> Result<SecretString, VaultError> {
        let bytes = self.get_secret_bytes(key)?;
        let text = std::str::from_utf8(bytes.expose_secret())
            .map_err(|_| VaultError::InvalidUtf8 {
                key: key.to_owned(),
            })?
            .trim_end_matches(['\r', '\n'])
            .to_owned();
        Ok(SecretString::new(text))
    }

    fn get_secret_bytes(&self, key: &str) -> Result<SecretBytes, VaultError> {
        let path = self.resolve_path(key)?;
        let meta = fs::symlink_metadata(&path).map_err(|err| {
            if err.kind() == std::io::ErrorKind::NotFound {
                VaultError::NotFound {
                    key: key.to_owned(),
                }
            } else {
                VaultError::ProviderUnavailable {
                    reason: "failed to stat secret file",
                }
            }
        })?;
        if meta.file_type().is_symlink() || !meta.file_type().is_file() {
            return Err(VaultError::InvalidKey {
                key: key.to_owned(),
            });
        }
        Self::enforce_permissions(key, &meta)?;
        let bytes = fs::read(&path).map_err(|_| VaultError::ProviderUnavailable {
            reason: "failed to read secret file",
        })?;
        Ok(SecretBytes::new(bytes))
    }

    fn version_hint(&self, key: &str) -> Option<u64> {
        let path = self.resolve_path(key).ok()?;
        let modified = fs::metadata(path).ok()?.modified().ok()?;
        let nanos = modified.duration_since(UNIX_EPOCH).ok()?.as_nanos();
        u64::try_from(nanos).ok()
    }
}

/// Abstract OS keychain adapter (`SEC-040`: macOS Keychain / Linux Secret Service).
pub trait KeychainBackend: Send + Sync {
    /// Looks up `(service, account)` in the OS keychain.
    fn lookup(&self, service: &str, account: &str) -> Result<SecretString, VaultError>;
}

/// Native macOS `/usr/bin/security find-generic-password` keychain lookup backend (`SEC-040`).
#[derive(Debug, Default, Clone, Copy)]
pub struct MacOsSecurityCliBackend;

impl KeychainBackend for MacOsSecurityCliBackend {
    fn lookup(&self, service: &str, account: &str) -> Result<SecretString, VaultError> {
        if !cfg!(target_os = "macos") {
            return Err(VaultError::ProviderUnavailable {
                reason: "macOS Keychain backend unavailable on this OS",
            });
        }
        let sec_bin = Path::new("/usr/bin/security");
        if !sec_bin.exists() {
            return Err(VaultError::ProviderUnavailable {
                reason: "/usr/bin/security binary not present",
            });
        }
        let output = Command::new(sec_bin)
            .args(["find-generic-password", "-s", service, "-a", account, "-w"])
            .output()
            .map_err(|_| VaultError::ProviderUnavailable {
                reason: "failed to execute macOS Keychain lookup",
            })?;
        if !output.status.success() {
            return Err(VaultError::NotFound {
                key: account.to_owned(),
            });
        }
        let secret = String::from_utf8(output.stdout)
            .map_err(|_| VaultError::InvalidUtf8 {
                key: account.to_owned(),
            })?
            .trim_end_matches(['\r', '\n'])
            .to_owned();
        Ok(SecretString::new(secret))
    }
}

/// Vault provider backed by an OS keychain service (`SEC-040`).
pub struct OsKeychainVault {
    service: String,
    backend: Arc<dyn KeychainBackend>,
}

impl OsKeychainVault {
    /// Creates an [`OsKeychainVault`] scoped to `service`.
    #[must_use]
    pub fn new(service: impl Into<String>, backend: Arc<dyn KeychainBackend>) -> Self {
        Self {
            service: service.into(),
            backend,
        }
    }

    /// Creates an [`OsKeychainVault`] targeting the macOS Keychain CLI utility.
    #[must_use]
    pub fn macos_default(service: impl Into<String>) -> Self {
        Self::new(service, Arc::new(MacOsSecurityCliBackend))
    }
}

impl Vault for OsKeychainVault {
    fn get_secret(&self, key: &str) -> Result<SecretString, VaultError> {
        let valid = SecretKey::new(key)?;
        self.backend.lookup(&self.service, valid.as_str())
    }
}

/// Extension hook for external HashiCorp Vault / cloud KMS secret providers (`SEC-040`).
pub trait ExternalKmsBackend: Send + Sync {
    /// Resolves a secret version from an external KMS or secrets manager.
    fn fetch_external(&self, secret_path: &str) -> Result<SecretBytes, VaultError>;
}

/// Adapts an [`ExternalKmsBackend`] to [`Vault`].
pub struct ExternalKmsVault {
    backend: Arc<dyn ExternalKmsBackend>,
}

impl ExternalKmsVault {
    /// Wraps `backend`.
    #[must_use]
    pub fn new(backend: Arc<dyn ExternalKmsBackend>) -> Self {
        Self { backend }
    }
}

impl Vault for ExternalKmsVault {
    fn get_secret(&self, key: &str) -> Result<SecretString, VaultError> {
        let bytes = self.get_secret_bytes(key)?;
        let text = String::from_utf8(bytes.expose_secret().to_vec()).map_err(|_| {
            VaultError::InvalidUtf8 {
                key: key.to_owned(),
            }
        })?;
        Ok(SecretString::new(text))
    }

    fn get_secret_bytes(&self, key: &str) -> Result<SecretBytes, VaultError> {
        let valid = SecretKey::new(key)?;
        self.backend.fetch_external(valid.as_str())
    }
}

struct CachedSecret {
    value: SecretBytes,
    fetched_at_ms: i64,
    version_hint: Option<u64>,
}

/// TTL + file-watch hot-reloading wrapper enabling secret rotation without gateway restart (`SEC-040`).
pub struct RotatingVault {
    inner: Arc<dyn Vault>,
    clock: Arc<dyn Clock>,
    ttl_ms: i64,
    cache: RwLock<BTreeMap<String, CachedSecret>>,
}

impl RotatingVault {
    /// Creates a [`RotatingVault`] wrapping `inner` with TTL `ttl_ms`.
    #[must_use]
    pub fn new(inner: Arc<dyn Vault>, clock: Arc<dyn Clock>, ttl_ms: i64) -> Self {
        Self {
            inner,
            clock,
            ttl_ms: ttl_ms.max(0),
            cache: RwLock::new(BTreeMap::new()),
        }
    }

    /// Forces immediate cache eviction across all cached secrets (`SEC-040` manual rotation trigger).
    pub fn invalidate_all(&self) {
        if let Ok(mut guard) = self.cache.write() {
            guard.clear();
        }
    }

    /// Evicts a single secret key so the next call fetches the newly rotated secret immediately.
    pub fn invalidate_key(&self, key: &str) {
        if let Ok(mut guard) = self.cache.write() {
            guard.remove(key);
        }
    }
}

impl Vault for RotatingVault {
    fn get_secret(&self, key: &str) -> Result<SecretString, VaultError> {
        let bytes = self.get_secret_bytes(key)?;
        let text = std::str::from_utf8(bytes.expose_secret())
            .map_err(|_| VaultError::InvalidUtf8 {
                key: key.to_owned(),
            })?
            .trim_end_matches(['\r', '\n'])
            .to_owned();
        Ok(SecretString::new(text))
    }

    fn get_secret_bytes(&self, key: &str) -> Result<SecretBytes, VaultError> {
        let valid = SecretKey::new(key)?;
        let now_ms = self.clock.unix_millis();
        let current_hint = self.inner.version_hint(valid.as_str());

        if let Ok(guard) = self.cache.read()
            && let Some(entry) = guard.get(valid.as_str())
        {
            let not_expired = now_ms.saturating_sub(entry.fetched_at_ms) < self.ttl_ms;
            let same_version = current_hint.is_none() || entry.version_hint == current_hint;
            if not_expired && same_version {
                return Ok(entry.value.duplicate());
            }
        }

        let fresh = self.inner.get_secret_bytes(valid.as_str())?;
        if let Ok(mut guard) = self.cache.write() {
            guard.insert(
                valid.as_str().to_owned(),
                CachedSecret {
                    value: fresh.duplicate(),
                    fetched_at_ms: now_ms,
                    version_hint: self.inner.version_hint(valid.as_str()),
                },
            );
        }
        Ok(fresh)
    }

    fn version_hint(&self, key: &str) -> Option<u64> {
        self.inner.version_hint(key)
    }
}
