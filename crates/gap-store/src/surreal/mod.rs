//! Optional SurrealDB 3.x `ContextStore` and `TaskStore` backend behind feature `store-surreal`
//! (`WP-M7-11`, `ADR-020`, `ADR-002`, `OPS-042`).
//!
//! # Licensing & Feature Isolation (`ADR-002`, `ADR-020`, Gate `G-21`)
//!
//! Upstream SurrealDB 3.x is distributed under the **Business Source License 1.1 (BSL-1.1)**, which
//! conflicts with GAP's Apache-2.0 default distribution policy enforced in `deny.toml`. Accordingly:
//!
//! 1. **Disabled by default:** The Cargo feature `store-surreal` is opt-in and excluded from
//!    `gap-store`'s default feature set and default binaries.
//! 2. **Zero BSL crate contamination:** Enabling `--features store-surreal` compiles this module's
//!    `ContextStore`, `ContextMaintenance`, and `TaskStore` backend using GAP's SurrealQL 3.x
//!    schema model and a thin wire [`SurrealTransport`] (`HttpSurrealTransport` over `/sql` plus a
//!    deterministic `InMemorySurrealEngine` for offline verification), keeping `Cargo.lock` and
//!    default builds 100% Apache-2.0 / MIT / BSD / Unicode-3.0 / Zlib clean under `cargo deny`.

/// Returns `true` when `gap-store` was compiled with the optional `store-surreal` feature enabled
/// (`OPS-042`, `ADR-020`).
#[must_use]
pub const fn feature_enabled() -> bool {
    cfg!(feature = "store-surreal")
}

#[cfg(feature = "store-surreal")]
pub mod context;
#[cfg(feature = "store-surreal")]
pub mod schema;
#[cfg(feature = "store-surreal")]
pub mod tasks;
#[cfg(feature = "store-surreal")]
pub mod transport;

#[cfg(feature = "store-surreal")]
pub use context::SurrealContextStore;
#[cfg(feature = "store-surreal")]
pub use schema::{SURREAL_SCHEMA_SURREALQL, tenant_record_key, tokenize_alphanumeric};
#[cfg(feature = "store-surreal")]
pub use tasks::SurrealTaskStore;
#[cfg(feature = "store-surreal")]
pub use transport::{HttpSurrealTransport, InMemorySurrealEngine, SurrealConfig, SurrealTransport};

#[cfg(all(test, feature = "store-surreal"))]
mod tests;

#[cfg(all(test, not(feature = "store-surreal")))]
mod default_feature_tests {
    use super::feature_enabled;

    // spec: OPS-042
    #[test]
    fn default_build_excludes_surreal_backend() {
        assert!(!feature_enabled());
    }
}
