//! Integration test entry point forwarding to `tests/security/secrets/secret_canary.rs` (`WP-M6-07`).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

include!("../../../tests/security/secrets/secret_canary.rs");
