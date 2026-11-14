//! Signed pagination and traversal cursor primitives (`WP-M2-03`, `RUN-025`, `ING-085`).
//!
//! Cursors encode an opaque payload together with its tenant, operation hash, schema stability
//! digest, generation family, and optional point-in-time `as_of` microsecond timestamp, signed
//! with HMAC-SHA256 (`CursorKey`). Cross-tenant replay (`INV-09`), tampering, operation mismatch,
//! generation-family mismatch, or schema drift (`RUN-025`) are rejected with
//! `GAP_VALIDATION_ERROR` without exposing key material or expected bindings (`INV-15`, `GQL-072`).

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::clock::Timestamp;
use crate::error::{ErrorCode, GapError, GapResult};
use crate::ids::TenantId;
use crate::secret::SecretBytes;

/// Wire prefix identifying GAP v1 signed pagination cursors.
pub const CURSOR_PREFIX: &str = "gapc1";

/// Maximum accepted raw wire token length in bytes (`64 KiB`) to bound decoding work.
pub const MAX_CURSOR_BYTES: usize = 64 * 1024;

/// Maximum permitted `op_hash` / `schema_hash` / `generation_family` metadata string length.
const MAX_BINDING_STR_BYTES: usize = 256;

/// Domain separation label mixed into every cursor HMAC computation.
const CURSOR_HMAC_DOMAIN: &[u8] = b"gap-cursor-v1\0";

/// SHA-256 block size in bytes.
const SHA256_BLOCK_LEN: usize = 64;

/// SHA-256 digest length in bytes.
const SHA256_DIGEST_LEN: usize = 32;

/// RFC 4648 base64url alphabet (no padding).
const B64URL_ALPHABET: &[u8; 64] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

/// Zeroizing HMAC-SHA256 key used to sign and verify opaque pagination cursors (`RUN-025`).
#[derive(Clone, Debug)]
pub struct CursorKey {
    bytes: SecretBytes,
}

impl CursorKey {
    /// Wraps raw key bytes in a zeroizing [`CursorKey`].
    ///
    /// Rejects empty keys with `GAP_VALIDATION_ERROR`.
    pub fn new(bytes: Vec<u8>) -> GapResult<Self> {
        if bytes.is_empty() {
            return Err(GapError::new(
                ErrorCode::ValidationError,
                "cursor signing key must not be empty",
            ));
        }
        Ok(Self {
            bytes: SecretBytes::new(bytes),
        })
    }

    /// Constructs a [`CursorKey`] from a fixed 256-bit key array.
    #[must_use]
    pub fn from_array(bytes: [u8; 32]) -> Self {
        Self {
            bytes: SecretBytes::new(bytes.to_vec()),
        }
    }

    /// Exposes the raw key slice strictly within the signing/verification boundary.
    fn expose(&self) -> &[u8] {
        self.bytes.expose_secret().as_slice()
    }
}

/// Expected execution binding verified against a decoded cursor (`RUN-025`, `INV-09`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CursorBinding {
    /// Authenticated caller tenant (`INV-09`).
    pub tenant: TenantId,
    /// Stable digest of the operation (`op_hash`, `RUN-025`).
    pub op_hash: String,
    /// Snapshot generation family (`RUN-025`).
    pub generation_family: String,
    /// Snapshot generation number when the cursor was issued or verified (`RUN-025`).
    pub generation: u64,
    /// Digest of the field/return schema (`RUN-025`: cursors remain valid across generations
    /// within the same family iff `schema_hash` is unchanged).
    pub schema_hash: String,
}

impl CursorBinding {
    /// Validates that binding metadata fields are non-empty and within length limits.
    pub fn validate(&self) -> GapResult<()> {
        validate_binding_field(&self.op_hash, "op_hash")?;
        validate_binding_field(&self.generation_family, "generation_family")?;
        validate_binding_field(&self.schema_hash, "schema_hash")?;
        Ok(())
    }
}

/// Signed cursor payload envelope serialized before HMAC authentication (`RUN-025`, `ING-085`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CursorEnvelope<P> {
    /// Bound tenant identifier (`INV-09`).
    pub tenant: String,
    /// Bound operation digest (`RUN-025`).
    pub op_hash: String,
    /// Bound generation family (`RUN-025`).
    pub generation_family: String,
    /// Snapshot generation at cursor issuance (`RUN-025`).
    pub generation: u64,
    /// Schema stability digest (`RUN-025`).
    pub schema_hash: String,
    /// Optional point-in-time snapshot microsecond timestamp (`ING-085`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub as_of_us: Option<i64>,
    /// Microsecond timestamp at which this cursor was issued.
    pub issued_at_us: i64,
    /// Domain payload (e.g. [`gap_store::TraverseResume`] or page offset state).
    pub payload: P,
}

impl<P> CursorEnvelope<P> {
    /// Constructs a new [`CursorEnvelope`] from a [`CursorBinding`], optional `as_of`, issuance
    /// timestamp, and typed payload.
    pub fn new(
        binding: &CursorBinding,
        as_of: Option<Timestamp>,
        issued_at: Timestamp,
        payload: P,
    ) -> GapResult<Self> {
        binding.validate()?;
        Ok(Self {
            tenant: binding.tenant.as_str().to_owned(),
            op_hash: binding.op_hash.clone(),
            generation_family: binding.generation_family.clone(),
            generation: binding.generation,
            schema_hash: binding.schema_hash.clone(),
            as_of_us: as_of.map(Timestamp::unix_micros),
            issued_at_us: issued_at.unix_micros(),
            payload,
        })
    }

    /// Reconstructs the pinned `as_of` [`Timestamp`] in microsecond precision, if present (`ING-085`).
    pub fn as_of_timestamp(&self) -> GapResult<Option<Timestamp>> {
        match self.as_of_us {
            Some(us) => {
                let nanos = i128::from(us) * 1_000;
                Timestamp::from_unix_nanos(nanos)
                    .map(Some)
                    .map_err(|_| invalid_cursor("invalid cursor asOf timestamp"))
            }
            None => Ok(None),
        }
    }
}

/// Encodes and signs a [`CursorEnvelope`] into an opaque `"gapc1.<payload>.<sig>"` token (`RUN-025`).
pub fn sign_cursor<P: Serialize>(
    key: &CursorKey,
    envelope: &CursorEnvelope<P>,
) -> GapResult<String> {
    validate_binding_field(&envelope.op_hash, "op_hash")?;
    validate_binding_field(&envelope.generation_family, "generation_family")?;
    validate_binding_field(&envelope.schema_hash, "schema_hash")?;
    if envelope.tenant.is_empty() {
        return Err(GapError::new(
            ErrorCode::ValidationError,
            "cursor tenant must not be empty",
        ));
    }

    let raw_json = serde_json::to_vec(envelope).map_err(|_| {
        GapError::new(
            ErrorCode::ValidationError,
            "failed to serialize pagination cursor",
        )
    })?;
    let payload_b64 = encode_base64url(&raw_json);
    let tag = compute_cursor_mac(key.expose(), payload_b64.as_bytes());
    let tag_b64 = encode_base64url(&tag);

    let mut token =
        String::with_capacity(CURSOR_PREFIX.len() + 2 + payload_b64.len() + tag_b64.len());
    token.push_str(CURSOR_PREFIX);
    token.push('.');
    token.push_str(&payload_b64);
    token.push('.');
    token.push_str(&tag_b64);
    if token.len() > MAX_CURSOR_BYTES {
        return Err(GapError::new(
            ErrorCode::ValidationError,
            "encoded pagination cursor exceeds maximum size",
        ));
    }
    Ok(token)
}

/// Verifies a signed cursor token against `key` and `expected` execution binding (`RUN-025`, `INV-09`).
///
/// Always checks the HMAC-SHA256 tag in constant time before parsing the envelope payload.
/// Cross-generation reuse within the same `generation_family` is accepted iff `schema_hash`,
/// `op_hash`, and `tenant` all match (`RUN-025`).
pub fn verify_cursor<P: DeserializeOwned>(
    key: &CursorKey,
    token: &str,
    expected: &CursorBinding,
) -> GapResult<CursorEnvelope<P>> {
    expected.validate()?;
    if token.is_empty() || token.len() > MAX_CURSOR_BYTES {
        return Err(invalid_cursor("malformed pagination cursor"));
    }

    let mut parts = token.split('.');
    let prefix = parts
        .next()
        .ok_or_else(|| invalid_cursor("malformed pagination cursor"))?;
    let payload_b64 = parts
        .next()
        .ok_or_else(|| invalid_cursor("malformed pagination cursor"))?;
    let tag_b64 = parts
        .next()
        .ok_or_else(|| invalid_cursor("malformed pagination cursor"))?;
    if parts.next().is_some() || prefix != CURSOR_PREFIX || payload_b64.is_empty() {
        return Err(invalid_cursor("malformed pagination cursor"));
    }

    let provided_tag =
        decode_base64url(tag_b64).ok_or_else(|| invalid_cursor("malformed pagination cursor"))?;
    if provided_tag.len() != SHA256_DIGEST_LEN {
        return Err(invalid_cursor("malformed pagination cursor"));
    }

    let expected_tag = compute_cursor_mac(key.expose(), payload_b64.as_bytes());
    if !constant_time_eq(&expected_tag, &provided_tag) {
        return Err(invalid_cursor("invalid pagination cursor signature"));
    }

    let payload_bytes = decode_base64url(payload_b64)
        .ok_or_else(|| invalid_cursor("malformed pagination cursor"))?;
    let envelope: CursorEnvelope<P> = serde_json::from_slice(&payload_bytes)
        .map_err(|_| invalid_cursor("malformed pagination cursor"))?;

    if !constant_time_eq(
        envelope.tenant.as_bytes(),
        expected.tenant.as_str().as_bytes(),
    ) {
        return Err(invalid_cursor("pagination cursor tenant mismatch"));
    }
    if envelope.op_hash != expected.op_hash {
        return Err(invalid_cursor("pagination cursor operation mismatch"));
    }
    if envelope.generation_family != expected.generation_family {
        return Err(invalid_cursor(
            "pagination cursor generation family mismatch",
        ));
    }
    if envelope.schema_hash != expected.schema_hash {
        return Err(invalid_cursor("pagination cursor schema changed"));
    }

    // Validate `as_of_us` representability upfront.
    let _ = envelope.as_of_timestamp()?;

    Ok(envelope)
}

fn validate_binding_field(value: &str, label: &'static str) -> GapResult<()> {
    if value.is_empty() || value.len() > MAX_BINDING_STR_BYTES {
        return Err(GapError::new(
            ErrorCode::ValidationError,
            format!("cursor {label} length must be within 1..={MAX_BINDING_STR_BYTES} bytes"),
        ));
    }
    if value.bytes().any(|b| b == 0 || b.is_ascii_control()) {
        return Err(GapError::new(
            ErrorCode::ValidationError,
            format!("cursor {label} contains invalid control characters"),
        ));
    }
    Ok(())
}

fn invalid_cursor(reason: &'static str) -> GapError {
    GapError::new(ErrorCode::ValidationError, reason)
}

fn compute_cursor_mac(key: &[u8], payload_b64: &[u8]) -> [u8; SHA256_DIGEST_LEN] {
    let mut msg = Vec::with_capacity(CURSOR_HMAC_DOMAIN.len() + payload_b64.len());
    msg.extend_from_slice(CURSOR_HMAC_DOMAIN);
    msg.extend_from_slice(payload_b64);
    hmac_sha256(key, &msg)
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff: u8 = 0;
    for (&lhs, &rhs) in a.iter().zip(b.iter()) {
        diff |= lhs ^ rhs;
    }
    diff == 0
}

fn encode_base64url(input: &[u8]) -> String {
    let out_len = input.len().div_ceil(3) * 4;
    let mut out = String::with_capacity(out_len);
    let (chunks, rem) = input.as_chunks::<3>();
    for &[b0, b1, b2] in chunks {
        let n = (u32::from(b0) << 16) | (u32::from(b1) << 8) | u32::from(b2);
        out.push(char::from(B64URL_ALPHABET[((n >> 18) & 0x3F) as usize]));
        out.push(char::from(B64URL_ALPHABET[((n >> 12) & 0x3F) as usize]));
        out.push(char::from(B64URL_ALPHABET[((n >> 6) & 0x3F) as usize]));
        out.push(char::from(B64URL_ALPHABET[(n & 0x3F) as usize]));
    }
    match rem {
        [] => {}
        [b0] => {
            let n = u32::from(*b0) << 16;
            out.push(char::from(B64URL_ALPHABET[((n >> 18) & 0x3F) as usize]));
            out.push(char::from(B64URL_ALPHABET[((n >> 12) & 0x3F) as usize]));
        }
        [b0, b1] => {
            let n = (u32::from(*b0) << 16) | (u32::from(*b1) << 8);
            out.push(char::from(B64URL_ALPHABET[((n >> 18) & 0x3F) as usize]));
            out.push(char::from(B64URL_ALPHABET[((n >> 12) & 0x3F) as usize]));
            out.push(char::from(B64URL_ALPHABET[((n >> 6) & 0x3F) as usize]));
        }
        _ => {}
    }
    out
}

fn decode_b64url_byte(b: u8) -> Option<u8> {
    match b {
        b'A'..=b'Z' => Some(b - b'A'),
        b'a'..=b'z' => Some(b - b'a' + 26),
        b'0'..=b'9' => Some(b - b'0' + 52),
        b'-' => Some(62),
        b'_' => Some(63),
        _ => None,
    }
}

fn decode_base64url(input: &str) -> Option<Vec<u8>> {
    let bytes = input.as_bytes();
    if bytes.is_empty() || bytes.len() % 4 == 1 {
        return None;
    }
    let mut out = Vec::with_capacity((bytes.len() * 3) / 4);
    let (chunks, rem) = bytes.as_chunks::<4>();
    for &[c0, c1, c2, c3] in chunks {
        let v0 = decode_b64url_byte(c0)?;
        let v1 = decode_b64url_byte(c1)?;
        let v2 = decode_b64url_byte(c2)?;
        let v3 = decode_b64url_byte(c3)?;
        let n =
            (u32::from(v0) << 18) | (u32::from(v1) << 12) | (u32::from(v2) << 6) | u32::from(v3);
        out.push(((n >> 16) & 0xFF) as u8);
        out.push(((n >> 8) & 0xFF) as u8);
        out.push((n & 0xFF) as u8);
    }
    match rem {
        [] => {}
        [c0, c1] => {
            let v0 = decode_b64url_byte(*c0)?;
            let v1 = decode_b64url_byte(*c1)?;
            // Reject non-canonical trailing bits so wire encodings stay canonical.
            if v1 & 0x0F != 0 {
                return None;
            }
            let n = (u32::from(v0) << 18) | (u32::from(v1) << 12);
            out.push(((n >> 16) & 0xFF) as u8);
        }
        [c0, c1, c2] => {
            let v0 = decode_b64url_byte(*c0)?;
            let v1 = decode_b64url_byte(*c1)?;
            let v2 = decode_b64url_byte(*c2)?;
            if v2 & 0x03 != 0 {
                return None;
            }
            let n = (u32::from(v0) << 18) | (u32::from(v1) << 12) | (u32::from(v2) << 6);
            out.push(((n >> 16) & 0xFF) as u8);
            out.push(((n >> 8) & 0xFF) as u8);
        }
        _ => return None,
    }
    Some(out)
}

/// Computes RFC 2104 HMAC-SHA256.
fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; SHA256_DIGEST_LEN] {
    let mut normalized_key = [0_u8; SHA256_BLOCK_LEN];
    if key.len() > SHA256_BLOCK_LEN {
        let digest = sha256(key);
        normalized_key[..SHA256_DIGEST_LEN].copy_from_slice(&digest);
    } else {
        normalized_key[..key.len()].copy_from_slice(key);
    }

    let mut ipad = [0_u8; SHA256_BLOCK_LEN];
    let mut opad = [0_u8; SHA256_BLOCK_LEN];
    for i in 0..SHA256_BLOCK_LEN {
        ipad[i] = normalized_key[i] ^ 0x36;
        opad[i] = normalized_key[i] ^ 0x5c;
    }

    let mut inner = Vec::with_capacity(SHA256_BLOCK_LEN + message.len());
    inner.extend_from_slice(&ipad);
    inner.extend_from_slice(message);
    let inner_digest = sha256(&inner);

    let mut outer = [0_u8; SHA256_BLOCK_LEN + SHA256_DIGEST_LEN];
    outer[..SHA256_BLOCK_LEN].copy_from_slice(&opad);
    outer[SHA256_BLOCK_LEN..].copy_from_slice(&inner_digest);
    sha256(&outer)
}

/// Computes FIPS 180-4 SHA-256 digest in pure Rust with `#![forbid(unsafe_code)]`.
fn sha256(input: &[u8]) -> [u8; SHA256_DIGEST_LEN] {
    const K: [u32; 64] = [
        0x428a_2f98,
        0x7137_4491,
        0xb5c0_fbcf,
        0xe9b5_dba5,
        0x3956_c25b,
        0x59f1_11f1,
        0x923f_82a4,
        0xab1c_5ed5,
        0xd807_aa98,
        0x1283_5b01,
        0x2431_85be,
        0x550c_7dc3,
        0x72be_5d74,
        0x80de_b1fe,
        0x9bdc_06a7,
        0xc19b_f174,
        0xe49b_69c1,
        0xefbe_4786,
        0x0fc1_9dc6,
        0x240c_a1cc,
        0x2de9_2c6f,
        0x4a74_84aa,
        0x5cb0_a9dc,
        0x76f9_88da,
        0x983e_5152,
        0xa831_c66d,
        0xb003_27c8,
        0xbf59_7fc7,
        0xc6e0_0bf3,
        0xd5a7_9147,
        0x06ca_6351,
        0x1429_2967,
        0x27b7_0a85,
        0x2e1b_2138,
        0x4d2c_6dfc,
        0x5338_0d13,
        0x650a_7354,
        0x766a_0abb,
        0x81c2_c92e,
        0x9272_2c85,
        0xa2bf_e8a1,
        0xa81a_664b,
        0xc24b_8b70,
        0xc76c_51a3,
        0xd192_e819,
        0xd699_0624,
        0xf40e_3585,
        0x106a_a070,
        0x19a4_c116,
        0x1e37_6c08,
        0x2748_774c,
        0x34b0_bcb5,
        0x391c_0cb3,
        0x4ed8_aa4a,
        0x5b9c_ca4f,
        0x682e_6ff3,
        0x748f_82ee,
        0x78a5_636f,
        0x84c8_7814,
        0x8cc7_0208,
        0x90be_fffa,
        0xa450_6ceb,
        0xbef9_a3f7,
        0xc671_78f2,
    ];

    let mut state: [u32; 8] = [
        0x6a09_e667,
        0xbb67_ae85,
        0x3c6e_f372,
        0xa54f_f53a,
        0x510e_527f,
        0x9b05_688c,
        0x1f83_d9ab,
        0x5be0_cd19,
    ];

    let bit_len = (input.len() as u64).wrapping_mul(8);
    let pad_zeroes = (55_usize.wrapping_sub(input.len() % SHA256_BLOCK_LEN)) % SHA256_BLOCK_LEN;
    let total_len = input.len() + 1 + pad_zeroes + 8;
    let mut padded = Vec::with_capacity(total_len);
    padded.extend_from_slice(input);
    padded.push(0x80);
    padded.resize(input.len() + 1 + pad_zeroes, 0);
    padded.extend_from_slice(&bit_len.to_be_bytes());

    let (blocks, _) = padded.as_chunks::<SHA256_BLOCK_LEN>();
    for block in blocks {
        let mut w = [0_u32; 64];
        let (words, _) = block.as_chunks::<4>();
        for (dst, &bytes) in w[..16].iter_mut().zip(words.iter()) {
            *dst = u32::from_be_bytes(bytes);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }

        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = state;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let temp1 = h
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = s0.wrapping_add(maj);

            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }

        state[0] = state[0].wrapping_add(a);
        state[1] = state[1].wrapping_add(b);
        state[2] = state[2].wrapping_add(c);
        state[3] = state[3].wrapping_add(d);
        state[4] = state[4].wrapping_add(e);
        state[5] = state[5].wrapping_add(f);
        state[6] = state[6].wrapping_add(g);
        state[7] = state[7].wrapping_add(h);
    }

    let mut digest = [0_u8; SHA256_DIGEST_LEN];
    for (idx, word) in state.iter().enumerate() {
        let start = idx * 4;
        digest[start..start + 4].copy_from_slice(&word.to_be_bytes());
    }
    digest
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn binding(
        tenant: &str,
        op: &str,
        family: &str,
        generation: u64,
        schema: &str,
    ) -> CursorBinding {
        CursorBinding {
            tenant: TenantId::new(tenant).expect("tenant id"),
            op_hash: op.to_owned(),
            generation_family: family.to_owned(),
            generation,
            schema_hash: schema.to_owned(),
        }
    }

    // spec: RUN-025
    #[test]
    fn cursor_rfc4231_hmac_sha256_test_vector_matches() {
        // RFC 4231 Test Case 2: Key = "Jefe", Data = "what do ya want for nothing?"
        let tag = hmac_sha256(b"Jefe", b"what do ya want for nothing?");
        let expected: [u8; 32] = [
            0x5b, 0xdc, 0xc1, 0x46, 0xbf, 0x60, 0x75, 0x4e, 0x6a, 0x04, 0x24, 0x26, 0x08, 0x95,
            0x75, 0xc7, 0x5a, 0x00, 0x3f, 0x08, 0x9d, 0x27, 0x39, 0x83, 0x9d, 0xec, 0x58, 0xb9,
            0x64, 0xec, 0x38, 0x43,
        ];
        assert_eq!(tag, expected);

        // RFC 4648 base64url round-trip for varying byte lengths.
        for len in 0..=16 {
            let data: Vec<u8> = (0..len).map(|i| (i * 19 + 7) as u8).collect();
            let enc = encode_base64url(&data);
            if data.is_empty() {
                assert!(enc.is_empty());
            } else {
                let dec = decode_base64url(&enc).expect("valid base64url");
                assert_eq!(dec, data);
            }
        }
    }

    // spec: RUN-025, ING-085
    #[test]
    fn cursor_sign_and_verify_round_trip_preserves_as_of_and_payload() {
        let key = CursorKey::from_array([0x42_u8; 32]);
        let b = binding("acme", "op_abc", "default", 3, "schema_v1");
        let as_of = Timestamp::from_unix_nanos(1_700_000_000_123_456_000).expect("ts");
        let issued_at = Timestamp::from_unix_nanos(1_700_000_005_000_000_000).expect("issued");
        let env = CursorEnvelope::new(
            &b,
            Some(as_of),
            issued_at,
            serde_json::json!({"depth": 2, "node": "n4"}),
        )
        .expect("envelope");

        let token = sign_cursor(&key, &env).expect("sign");
        assert!(token.starts_with("gapc1."));

        // Same generation and unchanged schema succeeds.
        let decoded: CursorEnvelope<serde_json::Value> =
            verify_cursor(&key, &token, &b).expect("verify");
        assert_eq!(decoded.as_of_timestamp().expect("as_of"), Some(as_of));
        assert_eq!(
            decoded.payload,
            serde_json::json!({"depth": 2, "node": "n4"})
        );

        // Cross-generation within same generation_family and unchanged schema_hash succeeds (RUN-025).
        let next_gen = binding("acme", "op_abc", "default", 4, "schema_v1");
        let decoded_next: CursorEnvelope<serde_json::Value> =
            verify_cursor(&key, &token, &next_gen).expect("cross-gen with unchanged schema");
        assert_eq!(decoded_next.generation, 3);
    }

    // spec: RUN-025, INV-09
    #[test]
    fn inv_09_cursor_rejects_cross_tenant_replay_and_binding_mismatch() {
        let key = CursorKey::from_array([0x11_u8; 32]);
        let b = binding("tenant-a", "op_1", "fam_1", 1, "sch_1");
        let ts = Timestamp::from_unix_millis(1_700_000_000_000).expect("ts");
        let env = CursorEnvelope::new(&b, Some(ts), ts, "node-99".to_owned()).expect("env");
        let token = sign_cursor(&key, &env).expect("sign");

        // Cross-tenant replay MUST fail (`INV-09`).
        let other_tenant = binding("tenant-b", "op_1", "fam_1", 1, "sch_1");
        let err = verify_cursor::<String>(&key, &token, &other_tenant).expect_err("cross tenant");
        assert_eq!(err.code(), ErrorCode::ValidationError);
        assert!(!err.message().contains("tenant-a"));
        assert!(!err.message().contains("tenant-b"));

        // Different operation hash fails (`RUN-025`).
        let other_op = binding("tenant-a", "op_2", "fam_1", 1, "sch_1");
        assert_eq!(
            verify_cursor::<String>(&key, &token, &other_op)
                .expect_err("op mismatch")
                .code(),
            ErrorCode::ValidationError
        );

        // Different generation family fails (`RUN-025`).
        let other_fam = binding("tenant-a", "op_1", "fam_2", 1, "sch_1");
        assert_eq!(
            verify_cursor::<String>(&key, &token, &other_fam)
                .expect_err("family mismatch")
                .code(),
            ErrorCode::ValidationError
        );

        // Changed schema hash across generations fails (`RUN-025`).
        let changed_schema = binding("tenant-a", "op_1", "fam_1", 2, "sch_2");
        assert_eq!(
            verify_cursor::<String>(&key, &token, &changed_schema)
                .expect_err("schema drift")
                .code(),
            ErrorCode::ValidationError
        );

        // Wrong signing key or tampered token fails (`RUN-025`).
        let wrong_key = CursorKey::from_array([0x22_u8; 32]);
        assert_eq!(
            verify_cursor::<String>(&wrong_key, &token, &b)
                .expect_err("wrong key")
                .code(),
            ErrorCode::ValidationError
        );
        let mut tampered = token.clone().into_bytes();
        let last = tampered.len() - 1;
        tampered[last] = if tampered[last] == b'A' { b'B' } else { b'A' };
        let tampered_str = String::from_utf8(tampered).expect("ascii");
        assert_eq!(
            verify_cursor::<String>(&key, &tampered_str, &b)
                .expect_err("tamper")
                .code(),
            ErrorCode::ValidationError
        );
    }
}
