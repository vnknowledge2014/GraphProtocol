//! Unit tests for manifest Ed25519 verification, trust tiers, revocation, and exec allow-lists.

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use gap_core::snapshot::TrustTier;

use super::*;
use crate::authn::crypto::{Ed25519SigningKey, hex_encode, sha256};

static NEXT_TMP_ID: AtomicU64 = AtomicU64::new(1);

struct TempDir(PathBuf);

impl TempDir {
    fn new(prefix: &str) -> Self {
        let id = NEXT_TMP_ID.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("{prefix}-{}-{id}", std::process::id()));
        fs::create_dir_all(&dir).expect("create temp dir");
        Self(dir)
    }

    fn path(&self) -> &std::path::Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn sample_manifest(tier: TrustTier, digest: &str) -> BranchManifest {
    BranchManifest {
        branch_id: "orders".to_owned(),
        version: "1.2.3".to_owned(),
        trust_tier: tier,
        artifact_sha256: digest.to_owned(),
        key_id: String::new(),
        signature_b64: String::new(),
        egress_hosts: vec!["api.orders.example".to_owned()],
        exec_binaries: vec!["/usr/local/bin/orders-cli".to_owned()],
    }
}

// spec: SEC-060, SEC-061, SEC-062
#[test]
fn manifest_verification_enforces_ed25519_tiers_and_immediate_revocation() {
    let signing = Ed25519SigningKey::from_seed([0x22; 32]).expect("signing key");
    let verifying = signing.verifying_key().expect("verifying key");
    let bundle_bytes = br#"{"branch":"orders","tools":["get_order"]}"#;
    let digest = hex_encode(&sha256(bundle_bytes));

    let mut keyring = TrustedKeyRing::new();
    keyring.add_key("org-key-2026", verifying);

    let signed_t0 = sample_manifest(TrustTier::T0, &digest)
        .sign_with("org-key-2026", &signing)
        .expect("sign manifest");

    let ok_t0 = verify_branch_manifest(
        &signed_t0,
        &keyring,
        &ManifestVerificationPolicy {
            actual_artifact_sha256: digest.clone(),
            expected_t1_pinned_sha256: None,
            allow_unsigned_t1_with_digest_pin: false,
        },
    )
    .expect("valid signed T0 succeeds");
    assert_eq!(ok_t0.trust_tier, TrustTier::T0);
    assert!(ok_t0.signature_verified);
    assert!(ok_t0.auto_activate_allowed);

    // Tampered manifest field invalidates Ed25519 signature immediately.
    let mut tampered = signed_t0.clone();
    tampered
        .egress_hosts
        .push("evil.attacker.example".to_owned());
    let sig_err = verify_branch_manifest(
        &tampered,
        &keyring,
        &ManifestVerificationPolicy {
            actual_artifact_sha256: digest.clone(),
            expected_t1_pinned_sha256: None,
            allow_unsigned_t1_with_digest_pin: false,
        },
    )
    .expect_err("tampered T0 fails");
    assert!(matches!(sig_err, ManifestError::InvalidSignature(_)));

    // Unsigned T0 fails closed.
    let unsigned_t0 = sample_manifest(TrustTier::T0, &digest);
    assert!(matches!(
        verify_branch_manifest(
            &unsigned_t0,
            &keyring,
            &ManifestVerificationPolicy {
                actual_artifact_sha256: digest.clone(),
                expected_t1_pinned_sha256: None,
                allow_unsigned_t1_with_digest_pin: false,
            }
        ),
        Err(ManifestError::InvalidSignature(_))
    ));

    // Unsigned T1 requires explicit digest pin opt-in + matching hash.
    let unsigned_t1 = sample_manifest(TrustTier::T1, &digest);
    assert!(matches!(
        verify_branch_manifest(
            &unsigned_t1,
            &keyring,
            &ManifestVerificationPolicy {
                actual_artifact_sha256: digest.clone(),
                expected_t1_pinned_sha256: None,
                allow_unsigned_t1_with_digest_pin: false,
            }
        ),
        Err(ManifestError::UnsignedTierRejected {
            tier: TrustTier::T1,
            ..
        })
    ));
    let pinned_t1 = verify_branch_manifest(
        &unsigned_t1,
        &keyring,
        &ManifestVerificationPolicy {
            actual_artifact_sha256: digest.clone(),
            expected_t1_pinned_sha256: Some(digest.clone()),
            allow_unsigned_t1_with_digest_pin: true,
        },
    )
    .expect("unsigned T1 with pinned digest succeeds");
    assert!(pinned_t1.requires_sandbox);
    assert!(!pinned_t1.auto_activate_allowed);

    // Unsigned T2 allowed only as sandboxed & proposal-gated.
    let t2_outcome = verify_branch_manifest(
        &sample_manifest(TrustTier::T2, &digest),
        &keyring,
        &ManifestVerificationPolicy {
            actual_artifact_sha256: digest.clone(),
            expected_t1_pinned_sha256: None,
            allow_unsigned_t1_with_digest_pin: false,
        },
    )
    .expect("T2 allowed with quarantine/sandbox flags");
    assert!(t2_outcome.requires_sandbox);
    assert!(!t2_outcome.auto_activate_allowed);

    // Emergency key revocation & digest revocation take immediate effect (SEC-062).
    let mut key_revoked_ring = keyring.clone();
    key_revoked_ring.revoke_key("org-key-2026");
    assert!(matches!(
        verify_branch_manifest(
            &signed_t0,
            &key_revoked_ring,
            &ManifestVerificationPolicy {
                actual_artifact_sha256: digest.clone(),
                expected_t1_pinned_sha256: None,
                allow_unsigned_t1_with_digest_pin: false,
            }
        ),
        Err(ManifestError::KeyRevoked(_))
    ));

    let mut digest_revoked_ring = keyring;
    digest_revoked_ring.revoke_digest(&digest);
    assert!(matches!(
        verify_branch_manifest(
            &sample_manifest(TrustTier::T2, &digest),
            &digest_revoked_ring,
            &ManifestVerificationPolicy {
                actual_artifact_sha256: digest,
                expected_t1_pinned_sha256: None,
                allow_unsigned_t1_with_digest_pin: false,
            }
        ),
        Err(ManifestError::DigestRevoked(_))
    ));
}

// spec: SEC-061, CMP-060
#[test]
fn exec_allowlist_rechecks_sha256_before_every_launch() {
    let tmp = TempDir::new("gap-allowlist");
    let bin_path = tmp.path().join("safe-tool");
    fs::write(&bin_path, b"#!/bin/sh\necho ok\n").expect("write binary");

    let mut allowlist = ExecAllowList::new();
    let pinned = allowlist
        .register(&bin_path, None)
        .expect("register binary");

    let verified = allowlist
        .verify_before_launch(&bin_path)
        .expect("initial launch check passes");
    assert_eq!(verified.sha256_hex, pinned);

    // Reject relative path and unlisted binary
    assert!(matches!(
        allowlist.verify_before_launch(std::path::Path::new("safe-tool")),
        Err(AllowListError::NonAbsolutePath(_))
    ));
    let other_path = tmp.path().join("unlisted-tool");
    fs::write(&other_path, b"other").expect("write other");
    assert!(matches!(
        allowlist.verify_before_launch(&other_path),
        Err(AllowListError::NotAllowListed(_))
    ));

    // Mutate binary on disk -> next launch catches SHA-256 mismatch immediately (SEC-061).
    fs::write(&bin_path, b"#!/bin/sh\necho PWNED\n").expect("replace binary");
    assert!(matches!(
        allowlist.verify_before_launch(&bin_path),
        Err(AllowListError::HashMismatch { .. })
    ));
}
