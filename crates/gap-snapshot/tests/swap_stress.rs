//! Integration test entry point forwarding to `tests/swap_stress/swap_stress.rs` (`WP-M4-08`).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

include!("../../../tests/swap_stress/swap_stress.rs");
