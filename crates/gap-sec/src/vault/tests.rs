#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeMap;
use std::fs;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use gap_core::clock::ManualClock;

use super::{
    EnvVault, ExternalKmsBackend, ExternalKmsVault, FileVault, KeychainBackend, OsKeychainVault,
    REDACTED_TOKEN, Redactor, RotatingVault, SecretBytes, SecretString, Vault, VaultError,
};

struct MockKeychain(BTreeMap<(String, String), String>);

impl KeychainBackend for MockKeychain {
    fn lookup(&self, service: &str, account: &str) -> Result<SecretString, VaultError> {
        self.0
            .get(&(service.to_owned(), account.to_owned()))
            .map(|v| SecretString::from_slice(v))
            .ok_or_else(|| VaultError::NotFound {
                key: account.to_owned(),
            })
    }
}

struct CountingKms {
    calls: AtomicUsize,
    current: std::sync::Mutex<String>,
}

impl ExternalKmsBackend for CountingKms {
    fn fetch_external(&self, secret_path: &str) -> Result<SecretBytes, VaultError> {
        if secret_path != "kms/branch-billing" {
            return Err(VaultError::NotFound {
                key: secret_path.to_owned(),
            });
        }
        self.calls.fetch_add(1, Ordering::Relaxed);
        let val = self.current.lock().expect("unpoisoned").clone();
        Ok(SecretBytes::new(val.into_bytes()))
    }
}

// spec: SEC-040, SEC-041, INV-15
#[test]
fn sec_040_041_secret_containers_mask_formatting_and_redact_vault_errors() {
    let s = SecretString::from_slice("super-secret-value-12345");
    let b = SecretBytes::new(b"binary-secret-999".to_vec());

    assert_eq!(format!("{s:?}"), REDACTED_TOKEN);
    assert_eq!(format!("{s}"), REDACTED_TOKEN);
    assert_eq!(format!("{b:?}"), REDACTED_TOKEN);
    assert_eq!(format!("{b}"), REDACTED_TOKEN);
    assert!(!format!("{s:?}").contains("super-secret"));
    assert!(s.ct_equals("super-secret-value-12345"));
    assert!(!s.ct_equals("wrong"));
}

struct TempDir(std::path::PathBuf);

impl TempDir {
    fn new(prefix: &str) -> Self {
        static SEQ: AtomicUsize = AtomicUsize::new(0);
        let id = SEQ.fetch_add(1, Ordering::Relaxed);
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

// spec: SEC-040
#[cfg(unix)]
#[test]
fn sec_040_file_vault_enforces_0600_and_rotates_via_rotating_vault_without_restart() {
    use std::os::unix::fs::PermissionsExt;

    let dir = TempDir::new("gap-sec-vault-test");
    let secret_path = dir.path().join("billing_api_key");
    fs::write(&secret_path, "v1-secret\n").expect("write v1");

    // 0644 must be rejected cleanly.
    fs::set_permissions(&secret_path, fs::Permissions::from_mode(0o644)).expect("chmod 0644");
    let fv = FileVault::new(dir.path());
    assert!(matches!(
        fv.get_secret("billing_api_key"),
        Err(VaultError::InsecurePermissions { mode: 0o644, .. })
    ));

    // 0600 succeeds cleanly.
    fs::set_permissions(&secret_path, fs::Permissions::from_mode(0o600)).expect("chmod 0600");
    let clock = Arc::new(ManualClock::new(1_730_000_000_000));
    let rotating = RotatingVault::new(Arc::new(fv), clock.clone(), 5_000);
    assert_eq!(
        rotating
            .get_secret("billing_api_key")
            .expect("get v1")
            .expose_secret(),
        "v1-secret"
    );

    // Simulate external secret rotation on disk and advance clock beyond TTL.
    fs::write(&secret_path, "v2-rotated-secret\n").expect("write v2");
    clock.advance(Duration::from_secs(6));
    assert_eq!(
        rotating
            .get_secret("billing_api_key")
            .expect("get v2 without restart")
            .expose_secret(),
        "v2-rotated-secret"
    );
}

// spec: SEC-040
#[test]
fn sec_040_env_keychain_and_external_kms_vault_providers_work_with_hot_rotation() {
    let mut vars = BTreeMap::new();
    vars.insert(
        "GAP_SECRET_CRM_TOKEN".to_owned(),
        "crm-tok-alpha".to_owned(),
    );
    let env = EnvVault::from_map("GAP_SECRET_", vars);
    assert_eq!(
        env.get_secret("crm.token")
            .expect("env secret")
            .expose_secret(),
        "crm-tok-alpha"
    );

    let mut kc_entries = BTreeMap::new();
    kc_entries.insert(
        ("gap-gateway".to_owned(), "shop_pat".to_owned()),
        "kc-secret-beta".to_owned(),
    );
    let kc = OsKeychainVault::new("gap-gateway", Arc::new(MockKeychain(kc_entries)));
    assert_eq!(
        kc.get_secret("shop_pat")
            .expect("kc secret")
            .expose_secret(),
        "kc-secret-beta"
    );

    let kms = Arc::new(CountingKms {
        calls: AtomicUsize::new(0),
        current: std::sync::Mutex::new("kms-v1".to_owned()),
    });
    let kms_vault = Arc::new(ExternalKmsVault::new(kms.clone()));
    let clock = Arc::new(ManualClock::new(1_730_000_000_000));
    let rotating = RotatingVault::new(kms_vault, clock.clone(), 60_000);

    assert_eq!(
        rotating
            .get_secret("kms/branch-billing")
            .expect("v1")
            .expose_secret(),
        "kms-v1"
    );
    assert_eq!(
        rotating
            .get_secret("kms/branch-billing")
            .expect("cached v1")
            .expose_secret(),
        "kms-v1"
    );
    assert_eq!(kms.calls.load(Ordering::Relaxed), 1);

    *kms.current.lock().expect("unpoisoned") = "kms-v2".to_owned();
    rotating.invalidate_key("kms/branch-billing");
    assert_eq!(
        rotating
            .get_secret("kms/branch-billing")
            .expect("rotated v2")
            .expose_secret(),
        "kms-v2"
    );
    assert_eq!(kms.calls.load(Ordering::Relaxed), 2);
}

// spec: SEC-041, SEC-042, INV-15
#[test]
fn sec_041_042_redactor_scrubs_bearer_jwt_pem_cloud_keys_userinfo_and_enricher_input() {
    let redactor = Redactor::new();
    let registered = SecretString::from_slice("CANARY_CUSTOM_SECRET_XYZ_9988");
    redactor.register_secret(&registered);

    let sample = concat!(
        "log line Authorization: Bearer topsecret_bearer_token_abc123 ",
        "jwt=eyJhbGciOiJFZERTQSJ9.eyJzdWIiOiJhbGljZSJ9.sigpart987 ",
        "aws=AKIAIOSFODNN7EXAMPLE ",
        "gh=ghp_1234567890abcdef1234567890 ",
        "oai=sk-proj_1234567890abcdef1234 ",
        "url=https://alice:hunter2@db.internal:5432/prod ",
        "-----BEGIN PRIVATE KEY-----\nMIIBVQIBADANBgkqhkiG\n-----END PRIVATE KEY----- ",
        "client_secret=\"raw_client_sec_00\" ",
        "custom=CANARY_CUSTOM_SECRET_XYZ_9988"
    );

    let scrubbed = redactor.redact_log(sample);
    for leaked in [
        "topsecret_bearer_token_abc123",
        "eyJhbGciOiJFZERTQSJ9",
        "AKIAIOSFODNN7EXAMPLE",
        "ghp_1234567890abcdef1234567890",
        "sk-proj_1234567890abcdef1234",
        "alice:hunter2",
        "MIIBVQIBADANBgkqhkiG",
        "raw_client_sec_00",
        "CANARY_CUSTOM_SECRET_XYZ_9988",
    ] {
        assert!(
            !scrubbed.contains(leaked),
            "leaked `{leaked}` in `{scrubbed}`"
        );
    }

    let capped = redactor.sanitize_enricher_input(sample, 48);
    assert!(capped.len() <= 48);
    assert!(!capped.contains("topsecret"));
}
