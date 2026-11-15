//! Ed25519 `T0` branch manifest verification, `T1`/`T2` trust-tier rules, and key/digest revocation (`SEC-060`, `SEC-061`, `SEC-062`).

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use gap_core::error::{ErrorCode, GapError};
use gap_core::snapshot::TrustTier;

use crate::authn::crypto::{
    Ed25519SigningKey, Ed25519VerifyingKey, base64_decode, base64_encode, ct_eq,
};
use crate::authn::json::JsonValue;

/// Declarative branch artifact & capability manifest (`SEC-060`, `SEC-061`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BranchManifest {
    /// Target branch identifier.
    pub branch_id: String,
    /// Manifest semantic or build version.
    pub version: String,
    /// Declared trust tier (`T0`, `T1`, `T2`).
    pub trust_tier: TrustTier,
    /// Lowercase hex SHA-256 digest of the branch bundle/spec artifact.
    pub artifact_sha256: String,
    /// Verifying key identifier registered in [`TrustedKeyRing`].
    pub key_id: String,
    /// Standard base64-encoded 64-byte Ed25519 signature over [`Self::canonical_signing_bytes`].
    pub signature_b64: String,
    /// Declared outbound egress host allow-list entries.
    pub egress_hosts: Vec<String>,
    /// Declared local CLI executable paths.
    pub exec_binaries: Vec<String>,
}

impl BranchManifest {
    /// Encodes the canonical RFC 8785 JCS signing payload (`key_id` included; `signature_b64` excluded).
    #[must_use]
    pub fn canonical_signing_bytes(&self) -> Vec<u8> {
        let mut root = BTreeMap::new();
        root.insert(
            "artifact_sha256".to_owned(),
            JsonValue::String(self.artifact_sha256.to_ascii_lowercase()),
        );
        root.insert(
            "branch_id".to_owned(),
            JsonValue::String(self.branch_id.clone()),
        );
        root.insert(
            "egress_hosts".to_owned(),
            JsonValue::Array(
                self.egress_hosts
                    .iter()
                    .map(|h| JsonValue::String(h.clone()))
                    .collect(),
            ),
        );
        root.insert(
            "exec_binaries".to_owned(),
            JsonValue::Array(
                self.exec_binaries
                    .iter()
                    .map(|b| JsonValue::String(b.clone()))
                    .collect(),
            ),
        );
        root.insert("key_id".to_owned(), JsonValue::String(self.key_id.clone()));
        let tier_str = match self.trust_tier {
            TrustTier::T0 => "T0",
            TrustTier::T1 => "T1",
            TrustTier::T2 => "T2",
        };
        root.insert(
            "trust_tier".to_owned(),
            JsonValue::String(tier_str.to_owned()),
        );
        root.insert(
            "version".to_owned(),
            JsonValue::String(self.version.clone()),
        );

        let payload = JsonValue::Object(root).to_jcs_string();
        let mut domain_sep = b"gap.manifest.v1:".to_vec();
        domain_sep.extend_from_slice(payload.as_bytes());
        domain_sep
    }

    /// Signs this manifest in-place using `key_id` and an Ed25519 signing key (`SEC-060`).
    ///
    /// # Errors
    ///
    /// Returns [`ManifestError::Crypto`] if Ed25519 signing fails.
    pub fn sign_with(
        mut self,
        key_id: impl Into<String>,
        signing_key: &Ed25519SigningKey,
    ) -> Result<Self, ManifestError> {
        self.key_id = key_id.into();
        let bytes = self.canonical_signing_bytes();
        let sig = signing_key
            .sign(&bytes)
            .map_err(|err| ManifestError::Crypto(err.to_string()))?;
        self.signature_b64 = base64_encode(&sig);
        Ok(self)
    }
}

/// Pinned public verifying keys and emergency revocation lists (`SEC-060`, `SEC-062`).
#[derive(Debug, Clone, Default)]
pub struct TrustedKeyRing {
    keys: BTreeMap<String, Ed25519VerifyingKey>,
    revoked_keys: BTreeSet<String>,
    revoked_digests: BTreeSet<String>,
}

impl TrustedKeyRing {
    /// Creates an empty key ring.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a pinned public key under `key_id`.
    pub fn add_key(&mut self, key_id: impl Into<String>, key: Ed25519VerifyingKey) {
        self.keys.insert(key_id.into(), key);
    }

    /// Revokes a signing key ID; takes immediate effect without waiting for TTLs (`SEC-062`).
    pub fn revoke_key(&mut self, key_id: impl Into<String>) {
        self.revoked_keys.insert(key_id.into());
    }

    /// Revokes an artifact SHA-256 hex digest across all tiers (`SEC-062`).
    pub fn revoke_digest(&mut self, sha256_hex: impl AsRef<str>) {
        self.revoked_digests
            .insert(sha256_hex.as_ref().trim().to_ascii_lowercase());
    }

    /// Returns `true` if `key_id` has been revoked.
    #[must_use]
    pub fn is_key_revoked(&self, key_id: &str) -> bool {
        self.revoked_keys.contains(key_id)
    }

    /// Returns `true` if `sha256_hex` has been revoked.
    #[must_use]
    pub fn is_digest_revoked(&self, sha256_hex: &str) -> bool {
        self.revoked_digests
            .contains(&sha256_hex.trim().to_ascii_lowercase())
    }
}

/// Verification policy inputs passed into [`verify_branch_manifest`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestVerificationPolicy {
    /// Computed SHA-256 hex digest of the downloaded/local bundle bytes on disk.
    pub actual_artifact_sha256: String,
    /// Optional admin-pinned digest for `T1` unsigned manifests (`SEC-061`).
    pub expected_t1_pinned_sha256: Option<String>,
    /// Explicitly permits unsigned `T1` manifests when `expected_t1_pinned_sha256` matches (`SEC-061`).
    pub allow_unsigned_t1_with_digest_pin: bool,
}

/// Verification outcome governing downstream activation & sandboxing (`SEC-060`, `SEC-061`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VerifiedManifestOutcome {
    /// Verified branch trust tier.
    pub trust_tier: TrustTier,
    /// `true` when an Ed25519 manifest signature verified cleanly against [`TrustedKeyRing`].
    pub signature_verified: bool,
    /// `true` when process execution for this branch must run under OS sandboxing (`SEC-070`, `SEC-071`).
    pub requires_sandbox: bool,
    /// `true` when activation may proceed without a fresh admin quorum (`T0` signed only).
    pub auto_activate_allowed: bool,
}

/// Failure returned by [`verify_branch_manifest`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManifestError {
    /// Key ID has been revoked (`SEC-062`).
    KeyRevoked(String),
    /// Artifact SHA-256 digest has been revoked (`SEC-062`).
    DigestRevoked(String),
    /// Bundle bytes on disk do not match manifest `artifact_sha256` or pinned digest (`SEC-061`).
    DigestMismatch {
        /// Expected hex digest.
        expected: String,
        /// Actual hex digest.
        actual: String,
    },
    /// Required signature missing, malformed, unknown key, or invalid cryptographic signature (`SEC-060`).
    InvalidSignature(String),
    /// Unsigned `T1` manifest rejected because digest-pin opt-in was absent or mismatched (`SEC-061`).
    UnsignedTierRejected {
        /// Tier that rejected unsigned artifact.
        tier: TrustTier,
        /// Explanation.
        reason: String,
    },
    /// Signing/cryptographic encoding error.
    Crypto(String),
}

impl ManifestError {
    /// Maps the failure into a [`GapError`].
    #[must_use]
    pub fn into_gap_error(self) -> GapError {
        let code = match &self {
            Self::KeyRevoked(_) | Self::DigestRevoked(_) | Self::UnsignedTierRejected { .. } => {
                ErrorCode::Forbidden
            }
            Self::InvalidSignature(_) | Self::DigestMismatch { .. } => ErrorCode::Unauthenticated,
            Self::Crypto(_) => ErrorCode::ValidationError,
        };
        GapError::new(code, self.to_string())
    }
}

impl fmt::Display for ManifestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::KeyRevoked(id) => write!(f, "manifest signing key `{id}` is revoked"),
            Self::DigestRevoked(h) => write!(f, "artifact digest `{h}` is revoked"),
            Self::DigestMismatch { expected, actual } => {
                write!(
                    f,
                    "artifact digest mismatch: expected {expected}, got {actual}"
                )
            }
            Self::InvalidSignature(msg) => write!(f, "manifest signature invalid: {msg}"),
            Self::UnsignedTierRejected { tier, reason } => {
                write!(f, "unsigned manifest rejected for {tier:?}: {reason}")
            }
            Self::Crypto(msg) => write!(f, "manifest crypto error: {msg}"),
        }
    }
}

impl std::error::Error for ManifestError {}

/// Verifies a [`BranchManifest`] against [`TrustedKeyRing`] and tier rules (`SEC-060`, `SEC-061`, `SEC-062`).
///
/// # Errors
///
/// Returns [`ManifestError`] when a key/digest is revoked, hashes diverge, or signature/tier rules fail.
pub fn verify_branch_manifest(
    manifest: &BranchManifest,
    keyring: &TrustedKeyRing,
    policy: &ManifestVerificationPolicy,
) -> Result<VerifiedManifestOutcome, ManifestError> {
    let manifest_digest = manifest.artifact_sha256.trim().to_ascii_lowercase();
    let actual_digest = policy.actual_artifact_sha256.trim().to_ascii_lowercase();

    if !manifest.key_id.is_empty() && keyring.is_key_revoked(&manifest.key_id) {
        return Err(ManifestError::KeyRevoked(manifest.key_id.clone()));
    }
    if keyring.is_digest_revoked(&manifest_digest) {
        return Err(ManifestError::DigestRevoked(manifest_digest));
    }
    if keyring.is_digest_revoked(&actual_digest) {
        return Err(ManifestError::DigestRevoked(actual_digest));
    }
    if !is_valid_sha256_hex(&manifest_digest)
        || !is_valid_sha256_hex(&actual_digest)
        || !ct_eq(manifest_digest.as_bytes(), actual_digest.as_bytes())
    {
        return Err(ManifestError::DigestMismatch {
            expected: manifest_digest,
            actual: actual_digest,
        });
    }

    let has_signature = !manifest.signature_b64.trim().is_empty();
    let signature_verified = if has_signature {
        verify_ed25519_sig(manifest, keyring)?;
        true
    } else {
        false
    };

    match manifest.trust_tier {
        TrustTier::T0 => {
            if !signature_verified {
                return Err(ManifestError::InvalidSignature(
                    "T0 manifests require a valid Ed25519 signature".to_owned(),
                ));
            }
            Ok(VerifiedManifestOutcome {
                trust_tier: TrustTier::T0,
                signature_verified: true,
                requires_sandbox: false,
                auto_activate_allowed: true,
            })
        }
        TrustTier::T1 => {
            if signature_verified {
                return Ok(VerifiedManifestOutcome {
                    trust_tier: TrustTier::T1,
                    signature_verified: true,
                    requires_sandbox: true,
                    auto_activate_allowed: true,
                });
            }
            if !policy.allow_unsigned_t1_with_digest_pin {
                return Err(ManifestError::UnsignedTierRejected {
                    tier: TrustTier::T1,
                    reason: "unsigned T1 manifest requires allow_unsigned_t1_with_digest_pin"
                        .to_owned(),
                });
            }
            let Some(expected_pin) = policy
                .expected_t1_pinned_sha256
                .as_deref()
                .map(|s| s.trim().to_ascii_lowercase())
            else {
                return Err(ManifestError::UnsignedTierRejected {
                    tier: TrustTier::T1,
                    reason: "missing expected_t1_pinned_sha256 pin".to_owned(),
                });
            };
            if !ct_eq(expected_pin.as_bytes(), actual_digest.as_bytes()) {
                return Err(ManifestError::DigestMismatch {
                    expected: expected_pin,
                    actual: actual_digest,
                });
            }
            Ok(VerifiedManifestOutcome {
                trust_tier: TrustTier::T1,
                signature_verified: false,
                requires_sandbox: true,
                auto_activate_allowed: false,
            })
        }
        TrustTier::T2 => Ok(VerifiedManifestOutcome {
            trust_tier: TrustTier::T2,
            signature_verified,
            requires_sandbox: true,
            auto_activate_allowed: false,
        }),
    }
}

fn verify_ed25519_sig(
    manifest: &BranchManifest,
    keyring: &TrustedKeyRing,
) -> Result<(), ManifestError> {
    if manifest.key_id.trim().is_empty() {
        return Err(ManifestError::InvalidSignature(
            "missing signing key_id".to_owned(),
        ));
    }
    let key = keyring.keys.get(&manifest.key_id).ok_or_else(|| {
        ManifestError::InvalidSignature(format!("unknown key_id `{}`", manifest.key_id))
    })?;
    let sig_bytes = base64_decode(manifest.signature_b64.trim()).map_err(|err| {
        ManifestError::InvalidSignature(format!("invalid base64 signature: {err}"))
    })?;
    let msg = manifest.canonical_signing_bytes();
    key.verify(&msg, &sig_bytes)
        .map_err(|err| ManifestError::InvalidSignature(err.to_string()))
}

fn is_valid_sha256_hex(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}
