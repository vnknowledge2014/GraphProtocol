//! Integration test wrapper forwarding `cargo nextest --test soak` to `tests/soak/soak.rs` (`WP-M7-07`).
#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

include!("../../../tests/soak/soak.rs");
