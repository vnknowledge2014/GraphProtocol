//! Process connector sandboxing (`sandbox-exec` / `bwrap`), fail-closed trust tiers, and `gap doctor` checks (`SEC-070`, `SEC-071`, `SEC-072`, `WP-M6-09`).

pub mod launcher;
pub mod profile;

#[cfg(test)]
mod tests;

pub use launcher::{
    OutputStreamKind, PreparedSandboxedCommand, SandboxBackendKind, SandboxBackendOverride,
    SandboxDoctorReport, SandboxError, SandboxLauncher, SandboxOutput, doctor_check,
};
pub use profile::{
    DEFAULT_MAX_OUTPUT_BYTES, DEFAULT_SANDBOX_TIMEOUT, FORBIDDEN_ENV_KEYS, RlimitSpec,
    SandboxProfile, render_linux_bwrap_args, render_macos_seatbelt_profile,
};
