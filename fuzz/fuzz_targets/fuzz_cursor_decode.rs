//! Fuzz target for signed pagination and traversal cursor decoder (`WP-M2-03`, `GQL-021`).
#![no_main]
#![forbid(unsafe_code)]
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::todo,
    clippy::unimplemented
)]

use gap_core::ids::TenantId;
use gap_exec::join::decode_and_verify_cursor;
use libfuzzer_sys::fuzz_target;

const MAX_CURSOR_BYTES: usize = 16 * 1024;

fuzz_target!(|data: &[u8]| {
    if data.len() > MAX_CURSOR_BYTES {
        return;
    }
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    let Ok(tenant) = TenantId::new("tenant-fuzz") else {
        return;
    };
    let _ = decode_and_verify_cursor(
        text,
        b"fuzz-hmac-secret-32-bytes-000000",
        "op_hash_fuzz",
        1,
        &tenant,
        "schema_hash_fuzz",
    );
});
