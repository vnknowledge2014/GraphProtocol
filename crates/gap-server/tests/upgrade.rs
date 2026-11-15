//! Integration test wrapper forwarding `cargo nextest --test upgrade` to `tests/upgrade/upgrade.rs` (`WP-M7-07`).
#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

include!("../../../tests/upgrade/upgrade.rs");
