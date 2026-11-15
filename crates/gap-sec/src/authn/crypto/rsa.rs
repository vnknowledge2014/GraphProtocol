//! Pure-Rust RS256 (`RSASSA-PKCS1-v1_5` with SHA-256) public signature verification (`WP-M6-01`).

use gap_core::error::{ErrorCode, GapError, GapResult};

use super::hash::{ct_eq, sha256};

/// Minimum allowed RSA modulus size in bits (`NIST SP 800-131A` / `SEC-001`).
pub const MIN_RSA_MODULUS_BITS: usize = 2048;

/// DER `DigestInfo` prefix for SHA-256 (`RFC 8017` §9.2 Notes 1).
const SHA256_DIGEST_INFO_PREFIX: [u8; 19] = [
    0x30, 0x31, 0x30, 0x0d, 0x06, 0x09, 0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x01, 0x05,
    0x00, 0x04, 0x20,
];

/// Unsigned big integer stored as little-endian 32-bit limbs.
#[derive(Clone, Debug, PartialEq, Eq)]
struct BigUint(Vec<u32>);

impl BigUint {
    fn from_be_bytes(bytes: &[u8]) -> Self {
        let mut limbs = Vec::with_capacity(bytes.len().div_ceil(4));
        for chunk in bytes.rchunks(4) {
            let mut buf = [0_u8; 4];
            let offset = 4 - chunk.len();
            buf[offset..].copy_from_slice(chunk);
            limbs.push(u32::from_be_bytes(buf));
        }
        let mut out = Self(limbs);
        out.trim();
        out
    }

    fn to_be_bytes_padded(&self, len: usize) -> Vec<u8> {
        let mut out = vec![0_u8; len];
        for (idx, &limb) in self.0.iter().enumerate() {
            let be = limb.to_be_bytes();
            for (b_i, &b) in be.iter().enumerate() {
                let rev_byte = idx * 4 + (3 - b_i);
                if rev_byte < len {
                    out[len - 1 - rev_byte] = b;
                }
            }
        }
        out
    }

    fn trim(&mut self) {
        while matches!(self.0.last(), Some(0)) {
            self.0.pop();
        }
    }

    fn is_zero(&self) -> bool {
        self.0.is_empty()
    }

    fn bit_len(&self) -> usize {
        let Some(&top) = self.0.last() else {
            return 0;
        };
        (self.0.len() - 1) * 32 + (32 - top.leading_zeros() as usize)
    }

    fn cmp_val(&self, rhs: &Self) -> std::cmp::Ordering {
        if self.0.len() != rhs.0.len() {
            return self.0.len().cmp(&rhs.0.len());
        }
        for (&a, &b) in self.0.iter().rev().zip(rhs.0.iter().rev()) {
            if a != b {
                return a.cmp(&b);
            }
        }
        std::cmp::Ordering::Equal
    }

    fn shl1_add_bit(&mut self, bit: u32) {
        let mut carry = bit & 1;
        for limb in &mut self.0 {
            let next_carry = *limb >> 31;
            *limb = (*limb << 1) | carry;
            carry = next_carry;
        }
        if carry != 0 {
            self.0.push(carry);
        }
    }

    fn sub_in_place(&mut self, rhs: &Self) {
        let mut borrow = 0_u64;
        for i in 0..self.0.len() {
            let a = u64::from(self.0[i]);
            let b = rhs.0.get(i).copied().map_or(0, u64::from) + borrow;
            if a >= b {
                self.0[i] = (a - b) as u32;
                borrow = 0;
            } else {
                self.0[i] = ((1_u64 << 32) + a - b) as u32;
                borrow = 1;
            }
        }
        self.trim();
    }

    fn mul(&self, rhs: &Self) -> Self {
        if self.is_zero() || rhs.is_zero() {
            return Self(Vec::new());
        }
        let mut acc = vec![0_u32; self.0.len() + rhs.0.len()];
        for (i, &a) in self.0.iter().enumerate() {
            let mut carry = 0_u64;
            for (j, &b) in rhs.0.iter().enumerate() {
                let prod = u64::from(a) * u64::from(b) + u64::from(acc[i + j]) + carry;
                acc[i + j] = (prod & 0xffff_ffff) as u32;
                carry = prod >> 32;
            }
            acc[i + rhs.0.len()] = (u64::from(acc[i + rhs.0.len()]) + carry) as u32;
        }
        let mut out = Self(acc);
        out.trim();
        out
    }

    fn rem(&self, modulus: &Self) -> Self {
        if self.cmp_val(modulus) == std::cmp::Ordering::Less {
            return self.clone();
        }
        let mut r = Self(Vec::new());
        for &limb in self.0.iter().rev() {
            for bit in (0..32).rev() {
                r.shl1_add_bit((limb >> bit) & 1);
                if r.cmp_val(modulus) != std::cmp::Ordering::Less {
                    r.sub_in_place(modulus);
                }
            }
        }
        r
    }

    fn modpow(base: &Self, exp: &Self, modulus: &Self) -> Self {
        let mut result = Self(vec![1]);
        let mut cur_base = base.rem(modulus);
        for (idx, &limb) in exp.0.iter().enumerate() {
            let max_bit = if idx + 1 == exp.0.len() {
                32 - limb.leading_zeros() as usize
            } else {
                32
            };
            for bit in 0..max_bit {
                if ((limb >> bit) & 1) == 1 {
                    result = result.mul(&cur_base).rem(modulus);
                }
                if idx + 1 < exp.0.len() || bit + 1 < max_bit {
                    cur_base = cur_base.mul(&cur_base).rem(modulus);
                }
            }
        }
        result
    }
}

/// Verifies an `RS256` (`RSASSA-PKCS1-v1_5` with `SHA-256`) signature using public `(n, e)`.
///
/// Rejects RSA moduli smaller than [`MIN_RSA_MODULUS_BITS`] (`2048` bits) or degenerate
/// exponents (`e < 3` or even).
///
/// # Errors
///
/// Returns [`ErrorCode::ValidationError`] on weak/malformed RSA parameters or
/// [`ErrorCode::Unauthenticated`] if signature verification fails.
pub fn rsa_rs256_verify(n_be: &[u8], e_be: &[u8], message: &[u8], sig: &[u8]) -> GapResult<()> {
    let n = BigUint::from_be_bytes(n_be);
    if n.bit_len() < MIN_RSA_MODULUS_BITS {
        return Err(GapError::new(
            ErrorCode::ValidationError,
            "RSA modulus below minimum 2048 bits",
        ));
    }
    let e = BigUint::from_be_bytes(e_be);
    if e.0.is_empty() || (e.0[0] & 1) == 0 || (e.0.len() == 1 && e.0[0] < 3) {
        return Err(GapError::new(
            ErrorCode::ValidationError,
            "invalid RSA public exponent",
        ));
    }
    let k = n.bit_len().div_ceil(8);
    if sig.len() != k {
        return Err(GapError::new(
            ErrorCode::Unauthenticated,
            "RSA signature length mismatch",
        ));
    }
    let s = BigUint::from_be_bytes(sig);
    if s.cmp_val(&n) != std::cmp::Ordering::Less {
        return Err(GapError::new(
            ErrorCode::Unauthenticated,
            "RSA signature integer >= modulus",
        ));
    }

    let em_int = BigUint::modpow(&s, &e, &n);
    let em = em_int.to_be_bytes_padded(k);

    let digest = sha256(message);
    let t_len = SHA256_DIGEST_INFO_PREFIX.len() + digest.len();
    if k < t_len + 11 {
        return Err(GapError::new(
            ErrorCode::Unauthenticated,
            "RSA modulus too short for PKCS#1 v1.5",
        ));
    }
    let ps_len = k - t_len - 3;
    let mut expected = Vec::with_capacity(k);
    expected.push(0x00);
    expected.push(0x01);
    expected.resize(2 + ps_len, 0xff);
    expected.push(0x00);
    expected.extend_from_slice(&SHA256_DIGEST_INFO_PREFIX);
    expected.extend_from_slice(&digest);

    if ct_eq(&em, &expected) {
        Ok(())
    } else {
        Err(GapError::new(
            ErrorCode::Unauthenticated,
            "RS256 signature verification failed",
        ))
    }
}
