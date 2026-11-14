//! Integration test wrapper forwarding `cargo nextest --test chaos` to `tests/chaos/chaos.rs` (`WP-M3-08`).
#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

include!("../../../tests/chaos/chaos.rs");
