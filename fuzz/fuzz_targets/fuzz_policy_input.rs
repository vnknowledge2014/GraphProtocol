//! Fuzz target for Cedar authorization schema and policy input parser (`WP-M6-05`, `SEC-020`).
#![no_main]
#![forbid(unsafe_code)]
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::todo,
    clippy::unimplemented
)]

use gap_sec::authz::{CedarSchema, CompiledPolicySet, DEFAULT_CEDAR_SCHEMA};
use libfuzzer_sys::fuzz_target;

const MAX_POLICY_BYTES: usize = 32 * 1024;

fuzz_target!(|data: &[u8]| {
    if data.len() > MAX_POLICY_BYTES {
        return;
    }
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    let _ = CedarSchema::parse(text);
    let _ = CompiledPolicySet::compile(DEFAULT_CEDAR_SCHEMA, &[text]);
});
