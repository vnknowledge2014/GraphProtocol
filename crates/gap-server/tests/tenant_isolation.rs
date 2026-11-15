//! Integration test entry point forwarding to `tests/security/tenant/tenant_isolation.rs` (`WP-M6-06`).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

include!("../../../tests/security/tenant/tenant_isolation.rs");
