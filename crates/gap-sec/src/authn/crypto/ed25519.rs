//! Pure-Rust RFC 8032 Ed25519 signing and canonical verification (`WP-M6-01`).

use gap_core::error::{ErrorCode, GapError, GapResult};

use super::hash::{ct_eq, sha512};

const MASK51: u64 = (1_u64 << 51) - 1;
const P_LIMBS: [u64; 5] = [
    0x0007_ffff_ffff_ffed,
    0x0007_ffff_ffff_ffff,
    0x0007_ffff_ffff_ffff,
    0x0007_ffff_ffff_ffff,
    0x0007_ffff_ffff_ffff,
];

/// Canonical group order `L = 2^252 + 27742317777372353535851937790883648493` in LE bytes.
const L_BYTES: [u8; 32] = [
    0xed, 0xd3, 0xf5, 0x5c, 0x1a, 0x63, 0x12, 0x58, 0xd6, 0x9c, 0xf7, 0xa2, 0xde, 0xf9, 0xde, 0x14,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10,
];

#[derive(Clone, Copy)]
struct Fe([u64; 5]);

impl Fe {
    const ZERO: Self = Self([0, 0, 0, 0, 0]);
    const ONE: Self = Self([1, 0, 0, 0, 0]);

    const fn from_u64(v: u64) -> Self {
        Self([v & MASK51, v >> 51, 0, 0, 0])
    }

    fn from_bytes_unmasked(s: &[u8; 32]) -> Self {
        let lo = u64::from_le_bytes([s[0], s[1], s[2], s[3], s[4], s[5], s[6], s[7]]);
        let w1 = u64::from_le_bytes([s[6], s[7], s[8], s[9], s[10], s[11], s[12], s[13]]);
        let w2 = u64::from_le_bytes([s[12], s[13], s[14], s[15], s[16], s[17], s[18], s[19]]);
        let w3 = u64::from_le_bytes([s[19], s[20], s[21], s[22], s[23], s[24], s[25], s[26]]);
        let w4 = u64::from_le_bytes([s[24], s[25], s[26], s[27], s[28], s[29], s[30], s[31]]);
        Self([
            lo & MASK51,
            (w1 >> 3) & MASK51,
            (w2 >> 6) & MASK51,
            (w3 >> 1) & MASK51,
            (w4 >> 12) & ((1_u64 << 52) - 1),
        ])
    }

    fn from_bytes(s: &[u8; 32]) -> Self {
        let mut fe = Self::from_bytes_unmasked(s);
        fe.0[4] &= MASK51;
        fe
    }

    fn reduce_limbs(mut h: [u64; 5]) -> Self {
        for _ in 0..2 {
            let c0 = h[0] >> 51;
            h[0] &= MASK51;
            h[1] += c0;
            let c1 = h[1] >> 51;
            h[1] &= MASK51;
            h[2] += c1;
            let c2 = h[2] >> 51;
            h[2] &= MASK51;
            h[3] += c2;
            let c3 = h[3] >> 51;
            h[3] &= MASK51;
            h[4] += c3;
            let c4 = h[4] >> 51;
            h[4] &= MASK51;
            h[0] += c4 * 19;
        }
        Self(h)
    }

    fn to_bytes(self) -> [u8; 32] {
        let mut h = Self::reduce_limbs(self.0).0;
        let mut q = (h[0] + 19) >> 51;
        q = (h[1] + q) >> 51;
        q = (h[2] + q) >> 51;
        q = (h[3] + q) >> 51;
        q = (h[4] + q) >> 51;

        h[0] += 19 * q;
        let c0 = h[0] >> 51;
        h[0] &= MASK51;
        h[1] += c0;
        let c1 = h[1] >> 51;
        h[1] &= MASK51;
        h[2] += c1;
        let c2 = h[2] >> 51;
        h[2] &= MASK51;
        h[3] += c2;
        let c3 = h[3] >> 51;
        h[3] &= MASK51;
        h[4] += c3;
        h[4] &= MASK51;

        let mut exact = [0_u8; 32];
        for (idx, word) in h.iter().enumerate() {
            let bit_off = idx * 51;
            for bit in 0..51 {
                if ((word >> bit) & 1) == 1 {
                    let abs_bit = bit_off + bit;
                    exact[abs_bit / 8] |= 1_u8 << (abs_bit % 8);
                }
            }
        }
        exact
    }

    fn add(self, rhs: Self) -> Self {
        Self::reduce_limbs([
            self.0[0] + rhs.0[0],
            self.0[1] + rhs.0[1],
            self.0[2] + rhs.0[2],
            self.0[3] + rhs.0[3],
            self.0[4] + rhs.0[4],
        ])
    }

    fn sub(self, rhs: Self) -> Self {
        let a = Self::reduce_limbs(self.0).0;
        let b = Self::reduce_limbs(rhs.0).0;
        Self::reduce_limbs([
            a[0] + 4 * P_LIMBS[0] - b[0],
            a[1] + 4 * P_LIMBS[1] - b[1],
            a[2] + 4 * P_LIMBS[2] - b[2],
            a[3] + 4 * P_LIMBS[3] - b[3],
            a[4] + 4 * P_LIMBS[4] - b[4],
        ])
    }

    fn neg(self) -> Self {
        Self::ZERO.sub(self)
    }

    fn mul(self, rhs: Self) -> Self {
        let a0 = u128::from(self.0[0]);
        let a1 = u128::from(self.0[1]);
        let a2 = u128::from(self.0[2]);
        let a3 = u128::from(self.0[3]);
        let a4 = u128::from(self.0[4]);

        let b0 = u128::from(rhs.0[0]);
        let b1 = u128::from(rhs.0[1]);
        let b2 = u128::from(rhs.0[2]);
        let b3 = u128::from(rhs.0[3]);
        let b4 = u128::from(rhs.0[4]);

        let b1_19 = b1 * 19;
        let b2_19 = b2 * 19;
        let b3_19 = b3 * 19;
        let b4_19 = b4 * 19;

        let mut t0 = a0 * b0 + a1 * b4_19 + a2 * b3_19 + a3 * b2_19 + a4 * b1_19;
        let mut t1 = a0 * b1 + a1 * b0 + a2 * b4_19 + a3 * b3_19 + a4 * b2_19;
        let mut t2 = a0 * b2 + a1 * b1 + a2 * b0 + a3 * b4_19 + a4 * b3_19;
        let mut t3 = a0 * b3 + a1 * b2 + a2 * b1 + a3 * b0 + a4 * b4_19;
        let mut t4 = a0 * b4 + a1 * b3 + a2 * b2 + a3 * b1 + a4 * b0;

        let mask = u128::from(MASK51);
        for _ in 0..2 {
            let c0 = t0 >> 51;
            t0 &= mask;
            t1 += c0;
            let c1 = t1 >> 51;
            t1 &= mask;
            t2 += c1;
            let c2 = t2 >> 51;
            t2 &= mask;
            t3 += c2;
            let c3 = t3 >> 51;
            t3 &= mask;
            t4 += c3;
            let c4 = t4 >> 51;
            t4 &= mask;
            t0 += c4 * 19;
        }
        Self([t0 as u64, t1 as u64, t2 as u64, t3 as u64, t4 as u64])
    }

    fn sq(self) -> Self {
        self.mul(self)
    }

    fn sq_n(self, n: usize) -> Self {
        let mut r = self;
        for _ in 0..n {
            r = r.sq();
        }
        r
    }

    fn pow_22523(self) -> Self {
        let t0 = self.sq();
        let t1 = t0.sq().sq();
        let t2 = self.mul(t1);
        let t3 = t0.mul(t2);
        let t4 = t3.sq();
        let x31 = t2.mul(t4);
        let t10 = x31.sq_n(5).mul(x31);
        let t20 = t10.sq_n(10).mul(t10);
        let t40 = t20.sq_n(20).mul(t20);
        let t50 = t40.sq_n(10).mul(t10);
        let t100 = t50.sq_n(50).mul(t50);
        let t200 = t100.sq_n(100).mul(t100);
        let t250 = t200.sq_n(50).mul(t50);
        t250.sq_n(2).mul(self)
    }

    fn invert(self) -> Self {
        // Compute x^(p - 2) where p - 2 = 2^255 - 21 = 8 * (2^252 - 3) + 3.
        let x3 = self.sq().mul(self);
        let base = self.pow_22523();
        let e3 = base.sq().sq().sq();
        e3.mul(x3)
    }

    fn is_zero(self) -> bool {
        ct_eq(&self.to_bytes(), &[0_u8; 32])
    }

    fn is_negative(self) -> bool {
        (self.to_bytes()[0] & 1) == 1
    }

    fn eq_mod_p(self, rhs: Self) -> bool {
        ct_eq(&self.to_bytes(), &rhs.to_bytes())
    }
}

fn curve_d() -> Fe {
    let num = Fe::from_u64(121_665).neg();
    let den = Fe::from_u64(121_666).invert();
    num.mul(den)
}

fn sqrt_minus_one() -> Fe {
    // 2^((p-1)/4) = 2^(2*(2^252-3) + 1) = (2^pow_22523)^2 * 2
    let two = Fe::from_u64(2);
    let r = two.pow_22523();
    r.sq().mul(two)
}

#[derive(Clone, Copy)]
struct Point {
    x: Fe,
    y: Fe,
    z: Fe,
    t: Fe,
}

impl Point {
    const IDENTITY: Self = Self {
        x: Fe::ZERO,
        y: Fe::ONE,
        z: Fe::ONE,
        t: Fe::ZERO,
    };

    fn add(self, q: Self, d2: Fe) -> Self {
        let a = self.y.sub(self.x).mul(q.y.sub(q.x));
        let b = self.y.add(self.x).mul(q.y.add(q.x));
        let c = self.t.mul(d2).mul(q.t);
        let d = self.z.mul(q.z).mul(Fe::from_u64(2));
        let e = b.sub(a);
        let f = d.sub(c);
        let g = d.add(c);
        let h = b.add(a);
        Self {
            x: e.mul(f),
            y: g.mul(h),
            z: f.mul(g),
            t: e.mul(h),
        }
    }

    fn double(self, d2: Fe) -> Self {
        self.add(self, d2)
    }

    fn scalar_mul(self, scalar: &[u8; 32]) -> Self {
        let d = curve_d();
        let d2 = d.add(d);
        let mut r = Self::IDENTITY;
        let mut q = self;
        for &byte in scalar {
            for bit in 0..8 {
                if ((byte >> bit) & 1) == 1 {
                    r = r.add(q, d2);
                }
                q = q.double(d2);
            }
        }
        r
    }

    fn encode(self) -> [u8; 32] {
        let zi = self.z.invert();
        let x = self.x.mul(zi);
        let y = self.y.mul(zi);
        let mut out = y.to_bytes();
        if x.is_negative() {
            out[31] |= 0x80;
        }
        out
    }

    fn decode(bytes: &[u8; 32]) -> GapResult<Self> {
        let sign = (bytes[31] >> 7) == 1;
        let mut y_bytes = *bytes;
        y_bytes[31] &= 0x7f;
        let y = Fe::from_bytes(&y_bytes);
        if !ct_eq(&y.to_bytes(), &y_bytes) {
            return Err(GapError::new(
                ErrorCode::ValidationError,
                "non-canonical Ed25519 y coordinate",
            ));
        }

        let d = curve_d();
        let y2 = y.sq();
        let u = y2.sub(Fe::ONE);
        let v = d.mul(y2).add(Fe::ONE);
        let v3 = v.sq().mul(v);
        let v7 = v3.sq().mul(v);
        let mut x = u.mul(v3).mul(u.mul(v7).pow_22523());

        let vx2 = v.mul(x.sq());
        if !vx2.eq_mod_p(u) {
            x = x.mul(sqrt_minus_one());
            if !v.mul(x.sq()).eq_mod_p(u) {
                return Err(GapError::new(
                    ErrorCode::ValidationError,
                    "invalid Ed25519 point compression",
                ));
            }
        }

        if x.is_zero() && sign {
            return Err(GapError::new(
                ErrorCode::ValidationError,
                "non-canonical Ed25519 sign bit on zero x",
            ));
        }
        if x.is_negative() != sign {
            x = x.neg();
        }

        Ok(Self {
            x,
            y,
            z: Fe::ONE,
            t: x.mul(y),
        })
    }

    fn eq_projective(self, other: Self) -> bool {
        self.x.mul(other.z).eq_mod_p(other.x.mul(self.z))
            && self.y.mul(other.z).eq_mod_p(other.y.mul(self.z))
    }
}

fn basepoint() -> GapResult<Point> {
    let by = Fe::from_u64(4).mul(Fe::from_u64(5).invert());
    let mut b_bytes = by.to_bytes();
    b_bytes[31] &= 0x7f;
    Point::decode(&b_bytes)
}

fn is_canonical_scalar(s: &[u8; 32]) -> bool {
    for i in (0..32).rev() {
        if s[i] < L_BYTES[i] {
            return true;
        }
        if s[i] > L_BYTES[i] {
            return false;
        }
    }
    false
}

fn scalar_reduce_64(bytes: &[u8; 64]) -> [u8; 32] {
    // Shift-and-subtract reduction modulo L over 512 bits in little-endian representation.
    let mut acc = [0_u8; 33];
    let mut l_ext = [0_u8; 33];
    l_ext[..32].copy_from_slice(&L_BYTES);

    for byte_idx in (0..64).rev() {
        let byte = bytes[byte_idx];
        for bit_idx in (0..8).rev() {
            let bit = (byte >> bit_idx) & 1;
            let mut carry = bit;
            for limb in &mut acc {
                let next = (*limb << 1) | carry;
                carry = *limb >> 7;
                *limb = next;
            }
            if ge_33(&acc, &l_ext) {
                sub_in_place_33(&mut acc, &l_ext);
            }
        }
    }
    let mut out = [0_u8; 32];
    out.copy_from_slice(&acc[..32]);
    out
}

fn ge_33(a: &[u8; 33], b: &[u8; 33]) -> bool {
    for i in (0..33).rev() {
        if a[i] > b[i] {
            return true;
        }
        if a[i] < b[i] {
            return false;
        }
    }
    true
}

fn sub_in_place_33(a: &mut [u8; 33], b: &[u8; 33]) {
    let mut borrow = 0_u16;
    for i in 0..33 {
        let av = u16::from(a[i]);
        let bv = u16::from(b[i]) + borrow;
        if av >= bv {
            a[i] = (av - bv) as u8;
            borrow = 0;
        } else {
            a[i] = (av + 256 - bv) as u8;
            borrow = 1;
        }
    }
}

fn scalar_mul_add_mod_l(a: &[u8; 32], b: &[u8; 32], c: &[u8; 32]) -> [u8; 32] {
    let mut prod = [0_u16; 64];
    for i in 0..32 {
        let mut carry = 0_u16;
        for j in 0..32 {
            let v = prod[i + j] + u16::from(a[i]) * u16::from(b[j]) + carry;
            prod[i + j] = v & 0xff;
            carry = v >> 8;
        }
        prod[i + 32] += carry;
    }
    let mut carry = 0_u16;
    for i in 0..64 {
        let c_byte = if i < 32 { u16::from(c[i]) } else { 0 };
        let v = prod[i] + c_byte + carry;
        prod[i] = v & 0xff;
        carry = v >> 8;
    }
    let mut wide = [0_u8; 64];
    for (dst, src) in wide.iter_mut().zip(prod.iter()) {
        *dst = *src as u8;
    }
    scalar_reduce_64(&wide)
}

fn clamp_scalar(hash_prefix: &[u8]) -> [u8; 32] {
    let mut s = [0_u8; 32];
    s.copy_from_slice(&hash_prefix[..32]);
    s[0] &= 248;
    s[31] &= 127;
    s[31] |= 64;
    s
}

/// Pure-Rust RFC 8032 Ed25519 private signing key backed by a 32-byte seed.
#[derive(Clone)]
pub struct Ed25519SigningKey {
    seed: [u8; 32],
    a_scalar: [u8; 32],
    prefix: [u8; 32],
    public_bytes: [u8; 32],
}

fn scrub_32(buf: &mut [u8; 32]) {
    for b in buf.iter_mut() {
        *b = 0;
    }
    std::hint::black_box(buf);
}

impl Drop for Ed25519SigningKey {
    fn drop(&mut self) {
        scrub_32(&mut self.seed);
        scrub_32(&mut self.a_scalar);
        scrub_32(&mut self.prefix);
    }
}

impl Ed25519SigningKey {
    /// Derives an Ed25519 keypair from a 32-byte seed per RFC 8032 §5.1.5.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorCode::Internal`] only if curve basepoint derivation fails.
    pub fn from_seed(seed: [u8; 32]) -> GapResult<Self> {
        let h = sha512(&seed);
        let a_scalar = clamp_scalar(&h[..32]);
        let mut prefix = [0_u8; 32];
        prefix.copy_from_slice(&h[32..]);
        let public_bytes = basepoint()?.scalar_mul(&a_scalar).encode();
        Ok(Self {
            seed,
            a_scalar,
            prefix,
            public_bytes,
        })
    }

    /// Returns the corresponding [`Ed25519VerifyingKey`].
    ///
    /// # Errors
    ///
    /// Returns [`ErrorCode::ValidationError`] if public key decoding fails.
    pub fn verifying_key(&self) -> GapResult<Ed25519VerifyingKey> {
        Ed25519VerifyingKey::from_bytes(&self.public_bytes)
    }

    /// Signs `message` deterministically according to RFC 8032 §5.1.6.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorCode::Internal`] if basepoint computation fails.
    pub fn sign(&self, message: &[u8]) -> GapResult<[u8; 64]> {
        let mut r_in = Vec::with_capacity(32 + message.len());
        r_in.extend_from_slice(&self.prefix);
        r_in.extend_from_slice(message);
        let r_hash = sha512(&r_in);
        let r = scalar_reduce_64(&r_hash);
        let r_point = basepoint()?.scalar_mul(&r);
        let r_bytes = r_point.encode();

        let mut k_in = Vec::with_capacity(64 + message.len());
        k_in.extend_from_slice(&r_bytes);
        k_in.extend_from_slice(&self.public_bytes);
        k_in.extend_from_slice(message);
        let k_hash = sha512(&k_in);
        let k = scalar_reduce_64(&k_hash);

        let s = scalar_mul_add_mod_l(&k, &self.a_scalar, &r);
        let mut sig = [0_u8; 64];
        sig[..32].copy_from_slice(&r_bytes);
        sig[32..].copy_from_slice(&s);
        Ok(sig)
    }
}

/// Pure-Rust RFC 8032 Ed25519 verifying key (`32`-byte compressed curve point).
#[derive(Clone)]
pub struct Ed25519VerifyingKey {
    bytes: [u8; 32],
    point: Point,
}

impl std::fmt::Debug for Ed25519VerifyingKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Ed25519VerifyingKey")
            .field("public_key_hex", &super::hash::hex_encode(&self.bytes))
            .finish()
    }
}

impl PartialEq for Ed25519VerifyingKey {
    fn eq(&self, other: &Self) -> bool {
        ct_eq(&self.bytes, &other.bytes)
    }
}

impl Eq for Ed25519VerifyingKey {}

impl std::fmt::Debug for Ed25519SigningKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Ed25519SigningKey")
            .field(
                "public_key_hex",
                &super::hash::hex_encode(&self.public_bytes),
            )
            .field("seed", &"[REDACTED]")
            .finish()
    }
}

impl Ed25519VerifyingKey {
    /// Decodes a 32-byte compressed Ed25519 public key and validates it lies on the curve.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorCode::ValidationError`] if decoding or canonicality fails.
    pub fn from_bytes(bytes: &[u8]) -> GapResult<Self> {
        let arr: [u8; 32] = bytes.try_into().map_err(|_| {
            GapError::new(
                ErrorCode::ValidationError,
                "Ed25519 public key must be 32 bytes",
            )
        })?;
        let point = Point::decode(&arr)?;
        Ok(Self { bytes: arr, point })
    }

    /// Returns the raw 32-byte compressed public key.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.bytes
    }

    /// Strictly verifies `signature` over `message` (`s < L`, canonical points, `[s]B == R + [k]A`).
    ///
    /// # Errors
    ///
    /// Returns [`ErrorCode::Unauthenticated`] if the signature is malformed or invalid.
    pub fn verify(&self, message: &[u8], signature: &[u8]) -> GapResult<()> {
        if signature.len() != 64 {
            return Err(GapError::new(
                ErrorCode::Unauthenticated,
                "Ed25519 signature must be 64 bytes",
            ));
        }
        let mut r_bytes = [0_u8; 32];
        let mut s_bytes = [0_u8; 32];
        r_bytes.copy_from_slice(&signature[..32]);
        s_bytes.copy_from_slice(&signature[32..]);
        if !is_canonical_scalar(&s_bytes) {
            return Err(GapError::new(
                ErrorCode::Unauthenticated,
                "non-canonical Ed25519 signature scalar",
            ));
        }
        let r_point = Point::decode(&r_bytes)
            .map_err(|_| GapError::new(ErrorCode::Unauthenticated, "invalid Ed25519 R point"))?;

        let mut k_in = Vec::with_capacity(64 + message.len());
        k_in.extend_from_slice(&r_bytes);
        k_in.extend_from_slice(&self.bytes);
        k_in.extend_from_slice(message);
        let k = scalar_reduce_64(&sha512(&k_in));

        let lhs = basepoint()?.scalar_mul(&s_bytes);
        let d = curve_d();
        let rhs = r_point.add(self.point.scalar_mul(&k), d.add(d));
        if lhs.eq_projective(rhs) {
            Ok(())
        } else {
            Err(GapError::new(
                ErrorCode::Unauthenticated,
                "Ed25519 signature verification failed",
            ))
        }
    }
}
