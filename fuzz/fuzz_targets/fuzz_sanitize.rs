//! Fuzz target for untrusted text NFKC/control/marker sanitizer (`WP-M2-05`, `RTE-020`, `RTE-021`).
#![no_main]
#![forbid(unsafe_code)]
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::todo,
    clippy::unimplemented
)]

use gap_route::sanitize::{DEFAULT_MAX_QUERY_CHARS, SPECIAL_TOKEN_MARKERS, sanitize};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    let once = sanitize(text, DEFAULT_MAX_QUERY_CHARS);
    let twice = sanitize(&once, DEFAULT_MAX_QUERY_CHARS);
    if once != twice {
        std::process::abort();
    }
    if once.chars().count() > DEFAULT_MAX_QUERY_CHARS {
        std::process::abort();
    }
    let lower = once.to_ascii_lowercase();
    for marker in SPECIAL_TOKEN_MARKERS {
        if lower.contains(&marker.to_ascii_lowercase()) {
            std::process::abort();
        }
    }
    if let Some(open) = once.find("<|")
        && once[open + 2..].contains("|>")
    {
        std::process::abort();
    }
});
