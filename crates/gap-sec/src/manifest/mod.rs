//! Ed25519 branch manifest verification, trust-tier rules, revocation, and exec allow-lists (`SEC-060`, `SEC-061`, `SEC-062`, `WP-M6-09`).

pub mod allowlist;
pub mod verify;

#[cfg(test)]
mod tests;

pub use allowlist::{AllowListError, ExecAllowList, VerifiedExecBinary};
pub use verify::{
    BranchManifest, ManifestError, ManifestVerificationPolicy, TrustedKeyRing,
    VerifiedManifestOutcome, verify_branch_manifest,
};
