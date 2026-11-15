//! Pure-Rust, zero-`unsafe` cryptographic primitives shared across `gap-sec` and M6 modules (`WP-M6-01`).

pub mod argon2;
pub mod ed25519;
pub mod hash;
pub mod rsa;

pub use argon2::{
    Argon2Params, argon2id_hash, argon2id_hash_full, argon2id_phc_encode, argon2id_phc_verify,
};
pub use ed25519::{Ed25519SigningKey, Ed25519VerifyingKey};
pub use hash::{
    base64_decode, base64_encode, base64url_decode, base64url_encode, blake3_hash, ct_eq,
    hex_decode, hex_encode, hmac_sha256, sha256, sha512,
};
pub use rsa::{MIN_RSA_MODULUS_BITS, rsa_rs256_verify};
