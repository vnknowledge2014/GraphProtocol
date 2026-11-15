//! Pure-Rust RFC 9106 Argon2id password/API-key hashing and PHC formatting (`WP-M6-01`).

use gap_core::error::{ErrorCode, GapError, GapResult};

use super::hash::{base64_decode, base64_encode, ct_eq};

const BLAKE2B_IV: [u64; 8] = [
    0x6a09e667f3bcc908,
    0xbb67ae8584caa73b,
    0x3c6ef372fe94f82b,
    0xa54ff53a5f1d36f1,
    0x510e527fade682d1,
    0x9b05688c2b3e6c1f,
    0x1f83d9abfb41bd6b,
    0x5be0cd19137e2179,
];

const BLAKE2B_SIGMA: [[usize; 16]; 10] = [
    [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15],
    [14, 10, 4, 8, 9, 15, 13, 6, 1, 12, 0, 2, 11, 7, 5, 3],
    [11, 8, 12, 0, 5, 2, 15, 13, 10, 14, 3, 6, 7, 1, 9, 4],
    [7, 9, 3, 1, 13, 12, 11, 14, 2, 6, 5, 10, 4, 0, 15, 8],
    [9, 0, 5, 7, 2, 4, 10, 15, 14, 1, 11, 12, 6, 8, 3, 13],
    [2, 12, 6, 10, 0, 11, 8, 3, 4, 13, 7, 5, 15, 14, 1, 9],
    [12, 5, 1, 15, 14, 13, 4, 10, 0, 7, 6, 3, 9, 2, 8, 11],
    [13, 11, 7, 14, 12, 1, 3, 9, 5, 0, 15, 4, 8, 6, 2, 10],
    [6, 15, 14, 9, 11, 3, 0, 8, 12, 2, 13, 7, 1, 4, 10, 5],
    [10, 2, 8, 4, 7, 6, 1, 5, 15, 11, 9, 14, 3, 12, 13, 0],
];

fn blake2b_g(v: &mut [u64; 16], a: usize, b: usize, c: usize, d: usize, x: u64, y: u64) {
    v[a] = v[a].wrapping_add(v[b]).wrapping_add(x);
    v[d] = (v[d] ^ v[a]).rotate_right(32);
    v[c] = v[c].wrapping_add(v[d]);
    v[b] = (v[b] ^ v[c]).rotate_right(24);
    v[a] = v[a].wrapping_add(v[b]).wrapping_add(y);
    v[d] = (v[d] ^ v[a]).rotate_right(16);
    v[c] = v[c].wrapping_add(v[d]);
    v[b] = (v[b] ^ v[c]).rotate_right(63);
}

fn blake2b_compress(h: &mut [u64; 8], block: &[u8; 128], t_lo: u64, t_hi: u64, last: bool) {
    let mut v = [0_u64; 16];
    v[..8].copy_from_slice(h);
    v[8..].copy_from_slice(&BLAKE2B_IV);
    v[12] ^= t_lo;
    v[13] ^= t_hi;
    if last {
        v[14] = !v[14];
    }

    let mut m = [0_u64; 16];
    for (i, w) in block.as_chunks::<8>().0.iter().enumerate() {
        m[i] = u64::from_le_bytes(*w);
    }

    for round in 0..12 {
        let s = &BLAKE2B_SIGMA[round % 10];
        blake2b_g(&mut v, 0, 4, 8, 12, m[s[0]], m[s[1]]);
        blake2b_g(&mut v, 1, 5, 9, 13, m[s[2]], m[s[3]]);
        blake2b_g(&mut v, 2, 6, 10, 14, m[s[4]], m[s[5]]);
        blake2b_g(&mut v, 3, 7, 11, 15, m[s[6]], m[s[7]]);
        blake2b_g(&mut v, 0, 5, 10, 15, m[s[8]], m[s[9]]);
        blake2b_g(&mut v, 1, 6, 11, 12, m[s[10]], m[s[11]]);
        blake2b_g(&mut v, 2, 7, 8, 13, m[s[12]], m[s[13]]);
        blake2b_g(&mut v, 3, 4, 9, 14, m[s[14]], m[s[15]]);
    }

    for i in 0..8 {
        h[i] ^= v[i] ^ v[i + 8];
    }
}

fn blake2b_hash(out_len: usize, input: &[u8]) -> Vec<u8> {
    let mut h = BLAKE2B_IV;
    h[0] ^= 0x0101_0000 ^ (out_len as u64);

    let mut offset = 0_usize;
    let total = input.len();
    while total.saturating_sub(offset) > 128 {
        let mut block = [0_u8; 128];
        block.copy_from_slice(&input[offset..offset + 128]);
        offset += 128;
        blake2b_compress(&mut h, &block, offset as u64, 0, false);
    }

    let mut last_block = [0_u8; 128];
    let rem = total - offset;
    last_block[..rem].copy_from_slice(&input[offset..]);
    blake2b_compress(&mut h, &last_block, total as u64, 0, true);

    let mut digest = Vec::with_capacity(64);
    for word in h {
        digest.extend_from_slice(&word.to_le_bytes());
    }
    digest.truncate(out_len);
    digest
}

fn blake2b_long(out_len: usize, input: &[u8]) -> Vec<u8> {
    let mut seed = Vec::with_capacity(4 + input.len());
    seed.extend_from_slice(&(out_len as u32).to_le_bytes());
    seed.extend_from_slice(input);

    if out_len <= 64 {
        return blake2b_hash(out_len, &seed);
    }

    let mut out = Vec::with_capacity(out_len);
    let mut prev = blake2b_hash(64, &seed);
    out.extend_from_slice(&prev[..32]);
    let mut remaining = out_len - 32;
    while remaining > 64 {
        prev = blake2b_hash(64, &prev);
        out.extend_from_slice(&prev[..32]);
        remaining -= 32;
    }
    let tail = blake2b_hash(remaining, &prev);
    out.extend_from_slice(&tail);
    out
}

#[derive(Clone)]
struct Block([u64; 128]);

impl Block {
    const fn zero() -> Self {
        Self([0_u64; 128])
    }

    fn from_bytes(bytes: &[u8]) -> Self {
        let mut words = [0_u64; 128];
        for (i, w) in bytes.as_chunks::<8>().0.iter().take(128).enumerate() {
            words[i] = u64::from_le_bytes(*w);
        }
        Self(words)
    }

    fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(1024);
        for w in self.0 {
            out.extend_from_slice(&w.to_le_bytes());
        }
        out
    }

    fn xor_with(&mut self, rhs: &Self) {
        for (dst, src) in self.0.iter_mut().zip(rhs.0.iter()) {
            *dst ^= *src;
        }
    }
}

fn f_blamka(x: u64, y: u64) -> u64 {
    let x_lo = u64::from(x as u32);
    let y_lo = u64::from(y as u32);
    x.wrapping_add(y)
        .wrapping_add(2_u64.wrapping_mul(x_lo.wrapping_mul(y_lo)))
}

fn blamka_g(s: &mut [u64; 16], a: usize, b: usize, c: usize, d: usize) {
    s[a] = f_blamka(s[a], s[b]);
    s[d] = (s[d] ^ s[a]).rotate_right(32);
    s[c] = f_blamka(s[c], s[d]);
    s[b] = (s[b] ^ s[c]).rotate_right(24);
    s[a] = f_blamka(s[a], s[b]);
    s[d] = (s[d] ^ s[a]).rotate_right(16);
    s[c] = f_blamka(s[c], s[d]);
    s[b] = (s[b] ^ s[c]).rotate_right(63);
}

fn permute_p(s: &mut [u64; 16]) {
    blamka_g(s, 0, 4, 8, 12);
    blamka_g(s, 1, 5, 9, 13);
    blamka_g(s, 2, 6, 10, 14);
    blamka_g(s, 3, 7, 11, 15);
    blamka_g(s, 0, 5, 10, 15);
    blamka_g(s, 1, 6, 11, 12);
    blamka_g(s, 2, 7, 8, 13);
    blamka_g(s, 3, 4, 9, 14);
}

fn compress_g(x: &Block, y: &Block) -> Block {
    let mut r = Block::zero();
    for i in 0..128 {
        r.0[i] = x.0[i] ^ y.0[i];
    }

    let mut q = r.clone();
    for row in 0..8 {
        let base = row * 16;
        let mut s = [0_u64; 16];
        s.copy_from_slice(&q.0[base..base + 16]);
        permute_p(&mut s);
        q.0[base..base + 16].copy_from_slice(&s);
    }

    let mut z = q;
    for col in 0..8 {
        let c = col * 2;
        let mut s = [0_u64; 16];
        for row in 0..8 {
            s[row * 2] = z.0[row * 16 + c];
            s[row * 2 + 1] = z.0[row * 16 + c + 1];
        }
        permute_p(&mut s);
        for row in 0..8 {
            z.0[row * 16 + c] = s[row * 2];
            z.0[row * 16 + c + 1] = s[row * 2 + 1];
        }
    }

    z.xor_with(&r);
    z
}

fn next_address_block(
    pass: u32,
    lane: u32,
    slice: u32,
    memory_blocks: u32,
    t_cost: u32,
    counter: u32,
) -> Block {
    let zero = Block::zero();
    let mut input = Block::zero();
    input.0[0] = u64::from(pass);
    input.0[1] = u64::from(lane);
    input.0[2] = u64::from(slice);
    input.0[3] = u64::from(memory_blocks);
    input.0[4] = u64::from(t_cost);
    input.0[5] = 2; // Argon2id type identifier
    input.0[6] = u64::from(counter);
    let mid = compress_g(&zero, &input);
    compress_g(&zero, &mid)
}

/// Argon2id parameters (`RFC 9106`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Argon2Params {
    /// Memory cost in KiB (default `19_456` KiB for production API keys).
    pub m_cost_kib: u32,
    /// Time cost / passes (default `2`).
    pub t_cost: u32,
    /// Parallelism / lanes (default `1`).
    pub p_cost: u32,
    /// Derived tag length in bytes (default `32`).
    pub tag_len: usize,
}

impl Default for Argon2Params {
    fn default() -> Self {
        Self {
            m_cost_kib: 19_456,
            t_cost: 2,
            p_cost: 1,
            tag_len: 32,
        }
    }
}

impl Argon2Params {
    /// Lightweight preset for fast deterministic unit tests (`1024` KiB, `t=1`, `p=1`).
    #[must_use]
    pub const fn testing() -> Self {
        Self {
            m_cost_kib: 1024,
            t_cost: 1,
            p_cost: 1,
            tag_len: 32,
        }
    }
}

/// Computes RFC 9106 Argon2id (`v=19`) key derivation with empty key and associated data.
///
/// # Errors
///
/// Returns [`ErrorCode::ValidationError`] if parameters violate RFC 9106 bounds.
pub fn argon2id_hash(password: &[u8], salt: &[u8], params: Argon2Params) -> GapResult<Vec<u8>> {
    argon2id_hash_full(password, salt, &[], &[], params)
}

/// Computes RFC 9106 Argon2id (`v=19`) with optional secret key `K` and associated data `X`.
///
/// # Errors
///
/// Returns [`ErrorCode::ValidationError`] if parameters violate RFC 9106 bounds.
pub fn argon2id_hash_full(
    password: &[u8],
    salt: &[u8],
    secret: &[u8],
    associated_data: &[u8],
    params: Argon2Params,
) -> GapResult<Vec<u8>> {
    if params.p_cost == 0 || params.t_cost == 0 || params.tag_len < 4 || salt.len() < 8 {
        return Err(GapError::new(
            ErrorCode::ValidationError,
            "invalid Argon2id parameters",
        ));
    }
    let min_m = params.p_cost.saturating_mul(8);
    if params.m_cost_kib < min_m {
        return Err(GapError::new(
            ErrorCode::ValidationError,
            "Argon2id m_cost_kib below 8 * p_cost minimum",
        ));
    }
    let sync_points = 4_u32;
    let lane_group = params.p_cost.saturating_mul(sync_points);
    let memory_blocks = (params.m_cost_kib / lane_group) * lane_group;
    let lane_length = memory_blocks / params.p_cost;
    let segment_length = lane_length / sync_points;
    let p = usize::try_from(params.p_cost)
        .map_err(|_| GapError::new(ErrorCode::ValidationError, "Argon2id p_cost overflow"))?;
    let q = usize::try_from(lane_length)
        .map_err(|_| GapError::new(ErrorCode::ValidationError, "Argon2id lane_length overflow"))?;
    let seg_len = usize::try_from(segment_length)
        .map_err(|_| GapError::new(ErrorCode::ValidationError, "Argon2id segment overflow"))?;

    let mut h0_in =
        Vec::with_capacity(40 + password.len() + salt.len() + secret.len() + associated_data.len());
    h0_in.extend_from_slice(&params.p_cost.to_le_bytes());
    h0_in.extend_from_slice(&(params.tag_len as u32).to_le_bytes());
    h0_in.extend_from_slice(&params.m_cost_kib.to_le_bytes());
    h0_in.extend_from_slice(&params.t_cost.to_le_bytes());
    h0_in.extend_from_slice(&0x13_u32.to_le_bytes());
    h0_in.extend_from_slice(&2_u32.to_le_bytes());
    for slice in [password, salt, secret, associated_data] {
        h0_in.extend_from_slice(&(slice.len() as u32).to_le_bytes());
        h0_in.extend_from_slice(slice);
    }
    let h0 = blake2b_hash(64, &h0_in);

    let total_blocks = p * q;
    let mut mem = vec![Block::zero(); total_blocks];

    for lane in 0..p {
        let mut seed0 = Vec::with_capacity(72);
        seed0.extend_from_slice(&h0);
        seed0.extend_from_slice(&0_u32.to_le_bytes());
        seed0.extend_from_slice(&(lane as u32).to_le_bytes());
        mem[lane * q] = Block::from_bytes(&blake2b_long(1024, &seed0));

        let mut seed1 = Vec::with_capacity(72);
        seed1.extend_from_slice(&h0);
        seed1.extend_from_slice(&1_u32.to_le_bytes());
        seed1.extend_from_slice(&(lane as u32).to_le_bytes());
        mem[lane * q + 1] = Block::from_bytes(&blake2b_long(1024, &seed1));
    }

    for pass in 0..params.t_cost {
        for slice in 0..sync_points {
            let slice_usize = slice as usize;
            let data_independent = pass == 0 && slice < 2;
            for lane in 0..p {
                let start_idx = if pass == 0 && slice == 0 { 2 } else { 0 };
                let mut addr_counter = 1_u32;
                let mut addr_block = Block::zero();
                if data_independent {
                    addr_block = next_address_block(
                        pass,
                        lane as u32,
                        slice,
                        memory_blocks,
                        params.t_cost,
                        addr_counter,
                    );
                }

                for index in start_idx..seg_len {
                    let curr_offset = slice_usize * seg_len + index;
                    let prev_offset = if curr_offset == 0 {
                        q - 1
                    } else {
                        curr_offset - 1
                    };

                    let pseudo_rand = if data_independent {
                        if index > start_idx && (index % 128 == 0) {
                            addr_counter += 1;
                            addr_block = next_address_block(
                                pass,
                                lane as u32,
                                slice,
                                memory_blocks,
                                params.t_cost,
                                addr_counter,
                            );
                        }
                        addr_block.0[index % 128]
                    } else {
                        mem[lane * q + prev_offset].0[0]
                    };

                    let j1 = u64::from(pseudo_rand as u32);
                    let j2 = pseudo_rand >> 32;
                    let ref_lane = if pass == 0 && slice == 0 {
                        lane
                    } else {
                        (j2 % u64::from(params.p_cost)) as usize
                    };

                    let same_lane = ref_lane == lane;
                    let ref_area_size = if pass == 0 {
                        if same_lane {
                            slice_usize * seg_len + index - 1
                        } else if index == 0 {
                            slice_usize * seg_len - 1
                        } else {
                            slice_usize * seg_len
                        }
                    } else if same_lane {
                        q - seg_len + index - 1
                    } else if index == 0 {
                        q - seg_len - 1
                    } else {
                        q - seg_len
                    };

                    let rel_x = (j1 * j1) >> 32;
                    let rel_y = ((ref_area_size as u64) * rel_x) >> 32;
                    let rel_pos = (ref_area_size as u64) - 1 - rel_y;
                    let start_pos = if pass != 0 && slice != 3 {
                        (slice_usize + 1) * seg_len
                    } else {
                        0
                    };
                    let ref_offset = (start_pos + rel_pos as usize) % q;

                    let mut next_block = compress_g(
                        &mem[lane * q + prev_offset],
                        &mem[ref_lane * q + ref_offset],
                    );
                    if pass > 0 {
                        next_block.xor_with(&mem[lane * q + curr_offset]);
                    }
                    mem[lane * q + curr_offset] = next_block;
                }
            }
        }
    }

    let mut final_block = mem[q - 1].clone();
    for lane in 1..p {
        final_block.xor_with(&mem[lane * q + q - 1]);
    }
    Ok(blake2b_long(params.tag_len, &final_block.to_bytes()))
}

/// Encodes an Argon2id hash into standard PHC string format (`$argon2id$v=19$m=...,t=...,p=...$salt$hash`).
///
/// # Errors
///
/// Returns [`ErrorCode::ValidationError`] if parameters violate RFC 9106 bounds.
pub fn argon2id_phc_encode(
    password: &[u8],
    salt: &[u8],
    params: Argon2Params,
) -> GapResult<String> {
    let digest = argon2id_hash(password, salt, params)?;
    let salt_b64 = base64_encode(salt).trim_end_matches('=').to_owned();
    let hash_b64 = base64_encode(&digest).trim_end_matches('=').to_owned();
    Ok(format!(
        "$argon2id$v=19$m={},t={},p={}${salt_b64}${hash_b64}",
        params.m_cost_kib, params.t_cost, params.p_cost
    ))
}

/// Verifies `password` against an encoded PHC `$argon2id$v=19$...` string in constant time.
///
/// # Errors
///
/// Returns [`ErrorCode::ValidationError`] if `phc` is malformed or uses unsupported parameters.
pub fn argon2id_phc_verify(password: &[u8], phc: &str) -> GapResult<bool> {
    let parts: Vec<&str> = phc.split('$').collect();
    if parts.len() != 6 || !parts[0].is_empty() || parts[1] != "argon2id" || parts[2] != "v=19" {
        return Err(GapError::new(
            ErrorCode::ValidationError,
            "malformed Argon2id PHC string",
        ));
    }
    let mut m_cost = None;
    let mut t_cost = None;
    let mut p_cost = None;
    for kv in parts[3].split(',') {
        let Some((k, v)) = kv.split_once('=') else {
            return Err(GapError::new(
                ErrorCode::ValidationError,
                "invalid Argon2id parameter pair",
            ));
        };
        let parsed: u32 = v.parse().map_err(|_| {
            GapError::new(ErrorCode::ValidationError, "invalid Argon2id numeric param")
        })?;
        match k {
            "m" => m_cost = Some(parsed),
            "t" => t_cost = Some(parsed),
            "p" => p_cost = Some(parsed),
            _ => {
                return Err(GapError::new(
                    ErrorCode::ValidationError,
                    "unknown Argon2id PHC parameter",
                ));
            }
        }
    }
    let salt = decode_unpadded_std_b64(parts[4])?;
    let expected = decode_unpadded_std_b64(parts[5])?;
    let params = Argon2Params {
        m_cost_kib: m_cost.ok_or_else(|| {
            GapError::new(ErrorCode::ValidationError, "missing m parameter in PHC")
        })?,
        t_cost: t_cost.ok_or_else(|| {
            GapError::new(ErrorCode::ValidationError, "missing t parameter in PHC")
        })?,
        p_cost: p_cost.ok_or_else(|| {
            GapError::new(ErrorCode::ValidationError, "missing p parameter in PHC")
        })?,
        tag_len: expected.len(),
    };
    let actual = argon2id_hash(password, &salt, params)?;
    Ok(ct_eq(&actual, &expected))
}

fn decode_unpadded_std_b64(raw: &str) -> GapResult<Vec<u8>> {
    let padded = match raw.len() % 4 {
        0 => raw.to_owned(),
        2 => format!("{raw}=="),
        3 => format!("{raw}="),
        _ => {
            return Err(GapError::new(
                ErrorCode::ValidationError,
                "invalid unpadded base64 length",
            ));
        }
    };
    base64_decode(&padded)
}
