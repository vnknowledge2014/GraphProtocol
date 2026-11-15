//! Constant-time comparisons, encodings, SHA-256, SHA-512, HMAC-SHA256, and BLAKE3 (`WP-M6-01`).

use gap_core::error::{ErrorCode, GapError, GapResult};

/// Compares two byte slices in constant time with respect to their contents.
///
/// Length mismatch returns `false` without inspecting or leaking secret bytes.
#[must_use]
pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0_u8;
    for (&x, &y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

const HEX_LOWER: &[u8; 16] = b"0123456789abcdef";

/// Encodes `bytes` as lowercase hexadecimal.
#[must_use]
pub fn hex_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(char::from(HEX_LOWER[usize::from(b >> 4)]));
        out.push(char::from(HEX_LOWER[usize::from(b & 0x0f)]));
    }
    out
}

/// Decodes a hexadecimal string into raw bytes.
///
/// # Errors
///
/// Returns [`ErrorCode::ValidationError`] if the string has odd length or non-hex bytes.
pub fn hex_decode(raw: &str) -> GapResult<Vec<u8>> {
    let bytes = raw.as_bytes();
    if !bytes.len().is_multiple_of(2) {
        return Err(GapError::new(
            ErrorCode::ValidationError,
            "hex input must have even length",
        ));
    }
    let mut out = Vec::with_capacity(bytes.len() / 2);
    for pair in bytes.as_chunks::<2>().0 {
        let hi = hex_nibble(pair[0])?;
        let lo = hex_nibble(pair[1])?;
        out.push((hi << 4) | lo);
    }
    Ok(out)
}

fn hex_nibble(b: u8) -> GapResult<u8> {
    match b {
        b'0'..=b'9' => Ok(b - b'0'),
        b'a'..=b'f' => Ok(b - b'a' + 10),
        b'A'..=b'F' => Ok(b - b'A' + 10),
        _ => Err(GapError::new(
            ErrorCode::ValidationError,
            "invalid hex character",
        )),
    }
}

const B64_STD: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
const B64_URL: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

/// Encodes `bytes` using unpadded Base64url (`RFC 4648` §5), as required by JWS/JWT.
#[must_use]
pub fn base64url_encode(bytes: &[u8]) -> String {
    encode_b64(bytes, B64_URL, false)
}

/// Encodes `bytes` using standard Base64 with `=` padding (`RFC 4648` §4).
#[must_use]
pub fn base64_encode(bytes: &[u8]) -> String {
    encode_b64(bytes, B64_STD, true)
}

fn encode_b64(bytes: &[u8], alphabet: &[u8; 64], pad: bool) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = usize::from(chunk[0]);
        let b1 = chunk.get(1).copied().map_or(0, usize::from);
        let b2 = chunk.get(2).copied().map_or(0, usize::from);
        let triple = (b0 << 16) | (b1 << 8) | b2;
        out.push(char::from(alphabet[(triple >> 18) & 0x3f]));
        out.push(char::from(alphabet[(triple >> 12) & 0x3f]));
        if chunk.len() > 1 {
            out.push(char::from(alphabet[(triple >> 6) & 0x3f]));
        } else if pad {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(char::from(alphabet[triple & 0x3f]));
        } else if pad {
            out.push('=');
        }
    }
    out
}

/// Decodes unpadded or optionally padded Base64url (`RFC 4648` §5).
///
/// # Errors
///
/// Returns [`ErrorCode::ValidationError`] if the input contains invalid Base64url characters or padding.
pub fn base64url_decode(raw: &str) -> GapResult<Vec<u8>> {
    decode_b64(raw, true)
}

/// Decodes standard Base64 (`RFC 4648` §4), accepting optional trailing `=` padding.
///
/// # Errors
///
/// Returns [`ErrorCode::ValidationError`] if the input contains invalid characters or length.
pub fn base64_decode(raw: &str) -> GapResult<Vec<u8>> {
    decode_b64(raw, false)
}

fn decode_b64(raw: &str, url: bool) -> GapResult<Vec<u8>> {
    let trimmed = raw.trim_end_matches('=');
    let pad_len = raw.len() - trimmed.len();
    if pad_len > 2 {
        return Err(GapError::new(
            ErrorCode::ValidationError,
            "invalid base64 padding",
        ));
    }
    if pad_len > 0 && !raw.len().is_multiple_of(4) {
        return Err(GapError::new(
            ErrorCode::ValidationError,
            "padded base64 input length must be a multiple of 4",
        ));
    }
    if trimmed.len() % 4 == 1 {
        return Err(GapError::new(
            ErrorCode::ValidationError,
            "invalid base64 length",
        ));
    }
    let mut vals = Vec::with_capacity(trimmed.len());
    for b in trimmed.bytes() {
        let v = match b {
            b'A'..=b'Z' => b - b'A',
            b'a'..=b'z' => b - b'a' + 26,
            b'0'..=b'9' => b - b'0' + 52,
            b'-' if url => 62,
            b'_' if url => 63,
            b'+' if !url => 62,
            b'/' if !url => 63,
            _ => {
                return Err(GapError::new(
                    ErrorCode::ValidationError,
                    "invalid base64 character",
                ));
            }
        };
        vals.push(v);
    }

    let mut out = Vec::with_capacity(vals.len() * 3 / 4);
    for quad in vals.chunks(4) {
        let a = u32::from(quad[0]);
        let b = u32::from(quad[1]);
        out.push(((a << 2) | (b >> 4)) as u8);
        if quad.len() >= 3 {
            let c = u32::from(quad[2]);
            out.push((((b & 0x0f) << 4) | (c >> 2)) as u8);
            if quad.len() == 4 {
                let d = u32::from(quad[3]);
                out.push((((c & 0x03) << 6) | d) as u8);
            }
        }
    }
    Ok(out)
}

const SHA256_K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

/// Computes SHA-256 (`FIPS 180-4`) over `data`.
#[must_use]
pub fn sha256(data: &[u8]) -> [u8; 32] {
    let mut state: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    let mut buf = Vec::with_capacity(data.len() + 72);
    buf.extend_from_slice(data);
    buf.push(0x80);
    while (buf.len() % 64) != 56 {
        buf.push(0);
    }
    let bit_len = (data.len() as u64).wrapping_mul(8);
    buf.extend_from_slice(&bit_len.to_be_bytes());

    for block in buf.as_chunks::<64>().0 {
        let mut w = [0_u32; 64];
        for (i, word) in block.as_chunks::<4>().0.iter().enumerate() {
            w[i] = u32::from_be_bytes(*word);
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
                .wrapping_add(SHA256_K[i])
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
        let next = [a, b, c, d, e, f, g, h];
        for (dst, add) in state.iter_mut().zip(next.iter()) {
            *dst = dst.wrapping_add(*add);
        }
    }

    let mut out = [0_u8; 32];
    for (i, word) in state.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&word.to_be_bytes());
    }
    out
}

/// Computes HMAC-SHA256 (`RFC 2104`) over `key` and `data`.
#[must_use]
pub fn hmac_sha256(key: &[u8], data: &[u8]) -> [u8; 32] {
    let mut block_key = [0_u8; 64];
    if key.len() > 64 {
        let hashed = sha256(key);
        block_key[..32].copy_from_slice(&hashed);
    } else {
        block_key[..key.len()].copy_from_slice(key);
    }

    let mut inner = Vec::with_capacity(64 + data.len());
    for &b in &block_key {
        inner.push(b ^ 0x36);
    }
    inner.extend_from_slice(data);
    let inner_hash = sha256(&inner);

    let mut outer = Vec::with_capacity(64 + 32);
    for &b in &block_key {
        outer.push(b ^ 0x5c);
    }
    outer.extend_from_slice(&inner_hash);
    sha256(&outer)
}

const SHA512_K: [u64; 80] = [
    0x428a2f98d728ae22,
    0x7137449123ef65cd,
    0xb5c0fbcfec4d3b2f,
    0xe9b5dba58189dbbc,
    0x3956c25bf348b538,
    0x59f111f1b605d019,
    0x923f82a4af194f9b,
    0xab1c5ed5da6d8118,
    0xd807aa98a3030242,
    0x12835b0145706fbe,
    0x243185be4ee4b28c,
    0x550c7dc3d5ffb4e2,
    0x72be5d74f27b896f,
    0x80deb1fe3b1696b1,
    0x9bdc06a725c71235,
    0xc19bf174cf692694,
    0xe49b69c19ef14ad2,
    0xefbe4786384f25e3,
    0x0fc19dc68b8cd5b5,
    0x240ca1cc77ac9c65,
    0x2de92c6f592b0275,
    0x4a7484aa6ea6e483,
    0x5cb0a9dcbd41fbd4,
    0x76f988da831153b5,
    0x983e5152ee66dfab,
    0xa831c66d2db43210,
    0xb00327c898fb213f,
    0xbf597fc7beef0ee4,
    0xc6e00bf33da88fc2,
    0xd5a79147930aa725,
    0x06ca6351e003826f,
    0x142929670a0e6e70,
    0x27b70a8546d22ffc,
    0x2e1b21385c26c926,
    0x4d2c6dfc5ac42aed,
    0x53380d139d95b3df,
    0x650a73548baf63de,
    0x766a0abb3c77b2a8,
    0x81c2c92e47edaee6,
    0x92722c851482353b,
    0xa2bfe8a14cf10364,
    0xa81a664bbc423001,
    0xc24b8b70d0f89791,
    0xc76c51a30654be30,
    0xd192e819d6ef5218,
    0xd69906245565a910,
    0xf40e35855771202a,
    0x106aa07032bbd1b8,
    0x19a4c116b8d2d0c8,
    0x1e376c085141ab53,
    0x2748774cdf8eeb99,
    0x34b0bcb5e19b48a8,
    0x391c0cb3c5c95a63,
    0x4ed8aa4ae3418acb,
    0x5b9cca4f7763e373,
    0x682e6ff3d6b2b8a3,
    0x748f82ee5defb2fc,
    0x78a5636f43172f60,
    0x84c87814a1f0ab72,
    0x8cc702081a6439ec,
    0x90befffa23631e28,
    0xa4506cebde82bde9,
    0xbef9a3f7b2c67915,
    0xc67178f2e372532b,
    0xca273eceea26619c,
    0xd186b8c721c0c207,
    0xeada7dd6cde0eb1e,
    0xf57d4f7fee6ed178,
    0x06f067aa72176fba,
    0x0a637dc5a2c898a6,
    0x113f9804bef90dae,
    0x1b710b35131c471b,
    0x28db77f523047d84,
    0x32caab7b40c72493,
    0x3c9ebe0a15c9bebc,
    0x431d67c49c100d4c,
    0x4cc5d4becb3e42b6,
    0x597f299cfc657e2a,
    0x5fcb6fab3ad6faec,
    0x6c44198c4a475817,
];

/// Computes SHA-512 (`FIPS 180-4`) over `data`.
#[must_use]
pub fn sha512(data: &[u8]) -> [u8; 64] {
    let mut state: [u64; 8] = [
        0x6a09e667f3bcc908,
        0xbb67ae8584caa73b,
        0x3c6ef372fe94f82b,
        0xa54ff53a5f1d36f1,
        0x510e527fade682d1,
        0x9b05688c2b3e6c1f,
        0x1f83d9abfb41bd6b,
        0x5be0cd19137e2179,
    ];
    let mut buf = Vec::with_capacity(data.len() + 144);
    buf.extend_from_slice(data);
    buf.push(0x80);
    while (buf.len() % 128) != 112 {
        buf.push(0);
    }
    let bit_len = (data.len() as u128).wrapping_mul(8);
    buf.extend_from_slice(&bit_len.to_be_bytes());

    for block in buf.as_chunks::<128>().0 {
        let mut w = [0_u64; 80];
        for (i, word) in block.as_chunks::<8>().0.iter().enumerate() {
            w[i] = u64::from_be_bytes(*word);
        }
        for i in 16..80 {
            let s0 = w[i - 15].rotate_right(1) ^ w[i - 15].rotate_right(8) ^ (w[i - 15] >> 7);
            let s1 = w[i - 2].rotate_right(19) ^ w[i - 2].rotate_right(61) ^ (w[i - 2] >> 6);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = state;
        for i in 0..80 {
            let s1 = e.rotate_right(14) ^ e.rotate_right(18) ^ e.rotate_right(41);
            let ch = (e & f) ^ ((!e) & g);
            let temp1 = h
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(SHA512_K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(28) ^ a.rotate_right(34) ^ a.rotate_right(39);
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
        let next = [a, b, c, d, e, f, g, h];
        for (dst, add) in state.iter_mut().zip(next.iter()) {
            *dst = dst.wrapping_add(*add);
        }
    }

    let mut out = [0_u8; 64];
    for (i, word) in state.iter().enumerate() {
        out[i * 8..i * 8 + 8].copy_from_slice(&word.to_be_bytes());
    }
    out
}

const BLAKE3_IV: [u32; 8] = [
    0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19,
];
const BLAKE3_MSG_PERMUTATION: [usize; 16] = [2, 6, 3, 10, 7, 0, 4, 13, 1, 11, 12, 5, 9, 14, 15, 8];
const CHUNK_START: u32 = 1 << 0;
const CHUNK_END: u32 = 1 << 1;
const PARENT: u32 = 1 << 2;
const ROOT: u32 = 1 << 3;

fn blake3_g(state: &mut [u32; 16], a: usize, b: usize, c: usize, d: usize, mx: u32, my: u32) {
    state[a] = state[a].wrapping_add(state[b]).wrapping_add(mx);
    state[d] = (state[d] ^ state[a]).rotate_right(16);
    state[c] = state[c].wrapping_add(state[d]);
    state[b] = (state[b] ^ state[c]).rotate_right(12);
    state[a] = state[a].wrapping_add(state[b]).wrapping_add(my);
    state[d] = (state[d] ^ state[a]).rotate_right(8);
    state[c] = state[c].wrapping_add(state[d]);
    state[b] = (state[b] ^ state[c]).rotate_right(7);
}

fn blake3_round(state: &mut [u32; 16], m: &[u32; 16]) {
    blake3_g(state, 0, 4, 8, 12, m[0], m[1]);
    blake3_g(state, 1, 5, 9, 13, m[2], m[3]);
    blake3_g(state, 2, 6, 10, 14, m[4], m[5]);
    blake3_g(state, 3, 7, 11, 15, m[6], m[7]);
    blake3_g(state, 0, 5, 10, 15, m[8], m[9]);
    blake3_g(state, 1, 6, 11, 12, m[10], m[11]);
    blake3_g(state, 2, 7, 8, 13, m[12], m[13]);
    blake3_g(state, 3, 4, 9, 14, m[14], m[15]);
}

fn blake3_compress(
    cv: &[u32; 8],
    block: &[u8; 64],
    counter: u64,
    block_len: u32,
    flags: u32,
) -> [u32; 16] {
    let mut m = [0_u32; 16];
    for (i, w) in block.as_chunks::<4>().0.iter().enumerate() {
        m[i] = u32::from_le_bytes(*w);
    }
    let mut state = [
        cv[0],
        cv[1],
        cv[2],
        cv[3],
        cv[4],
        cv[5],
        cv[6],
        cv[7],
        BLAKE3_IV[0],
        BLAKE3_IV[1],
        BLAKE3_IV[2],
        BLAKE3_IV[3],
        counter as u32,
        (counter >> 32) as u32,
        block_len,
        flags,
    ];
    for round in 0..7 {
        blake3_round(&mut state, &m);
        if round < 6 {
            let mut permuted = [0_u32; 16];
            for (dst, &src_idx) in permuted.iter_mut().zip(BLAKE3_MSG_PERMUTATION.iter()) {
                *dst = m[src_idx];
            }
            m = permuted;
        }
    }
    for i in 0..8 {
        state[i] ^= state[i + 8];
        state[i + 8] ^= cv[i];
    }
    state
}

fn words_to_cv_bytes(words: &[u32; 8]) -> [u8; 32] {
    let mut out = [0_u8; 32];
    for (i, w) in words.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&w.to_le_bytes());
    }
    out
}

#[derive(Clone, Copy)]
struct Blake3Output {
    cv: [u32; 8],
    block: [u8; 64],
    counter: u64,
    block_len: u32,
    flags: u32,
}

impl Blake3Output {
    fn chaining_value(&self) -> [u32; 8] {
        let out = blake3_compress(
            &self.cv,
            &self.block,
            self.counter,
            self.block_len,
            self.flags,
        );
        let mut next = [0_u32; 8];
        next.copy_from_slice(&out[..8]);
        next
    }

    fn root_hash_bytes(&self) -> [u8; 32] {
        let out = blake3_compress(
            &self.cv,
            &self.block,
            self.counter,
            self.block_len,
            self.flags | ROOT,
        );
        let mut next = [0_u32; 8];
        next.copy_from_slice(&out[..8]);
        words_to_cv_bytes(&next)
    }
}

fn parent_output(left: [u32; 8], right: [u32; 8], key_words: [u32; 8], flags: u32) -> Blake3Output {
    let mut block = [0_u8; 64];
    block[..32].copy_from_slice(&words_to_cv_bytes(&left));
    block[32..].copy_from_slice(&words_to_cv_bytes(&right));
    Blake3Output {
        cv: key_words,
        block,
        counter: 0,
        block_len: 64,
        flags: PARENT | flags,
    }
}

fn compress_chunk_output(chunk: &[u8], chunk_counter: u64) -> Blake3Output {
    let mut cv = BLAKE3_IV;
    if chunk.is_empty() {
        return Blake3Output {
            cv,
            block: [0_u8; 64],
            counter: chunk_counter,
            block_len: 0,
            flags: CHUNK_START | CHUNK_END,
        };
    }
    let blocks = chunk.len().div_ceil(64);
    let mut last = Blake3Output {
        cv,
        block: [0_u8; 64],
        counter: chunk_counter,
        block_len: 0,
        flags: 0,
    };
    for block_idx in 0..blocks {
        let start = block_idx * 64;
        let end = (start + 64).min(chunk.len());
        let slice = &chunk[start..end];
        let mut block = [0_u8; 64];
        block[..slice.len()].copy_from_slice(slice);
        let mut flags = 0_u32;
        if block_idx == 0 {
            flags |= CHUNK_START;
        }
        if block_idx + 1 == blocks {
            flags |= CHUNK_END;
            last = Blake3Output {
                cv,
                block,
                counter: chunk_counter,
                block_len: slice.len() as u32,
                flags,
            };
        } else {
            let compressed = blake3_compress(&cv, &block, chunk_counter, slice.len() as u32, flags);
            cv.copy_from_slice(&compressed[..8]);
        }
    }
    last
}

/// Computes the canonical 32-byte BLAKE3 digest over `data`.
#[must_use]
pub fn blake3_hash(data: &[u8]) -> [u8; 32] {
    if data.len() <= 1024 {
        return compress_chunk_output(data, 0).root_hash_bytes();
    }

    let mut stack: Vec<[u32; 8]> = Vec::new();
    let chunks = data.chunks(1024);
    let total_chunks = data.len().div_ceil(1024);
    let mut final_output: Option<Blake3Output> = None;

    for (chunk_idx, chunk) in chunks.enumerate() {
        let out = compress_chunk_output(chunk, chunk_idx as u64);
        if chunk_idx + 1 == total_chunks {
            final_output = Some(out);
            break;
        }
        let mut cv = out.chaining_value();
        let mut total = (chunk_idx + 1) as u64;
        while total & 1 == 0 {
            if let Some(left) = stack.pop() {
                cv = parent_output(left, cv, BLAKE3_IV, 0).chaining_value();
            }
            total >>= 1;
        }
        stack.push(cv);
    }

    let mut acc = match final_output {
        Some(out) => out,
        None => compress_chunk_output(&[], 0),
    };
    while let Some(left) = stack.pop() {
        acc = parent_output(left, acc.chaining_value(), BLAKE3_IV, 0);
    }
    acc.root_hash_bytes()
}
