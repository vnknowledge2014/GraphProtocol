//! Safe `.safetensors` mmap parser, serializer, and IEEE-754 `f16`/`f32` conversion (`WP-M5-02`).

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::Path;

use gap_core::error::{ErrorCode, GapError, GapResult};

use crate::candle::json::{JsonValue, escape_json_str, parse_json};
use crate::candle::mmap::MmapBytes;
use crate::candle::sha256::sha256_hex;

/// Maximum permitted `.safetensors` header size (16 MiB).
const MAX_HEADER_BYTES: usize = 16 * 1024 * 1024;

/// Floating-point storage precision supported by `LayaCandle` (`ADR-005`, `WP-M5-02`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Precision {
    /// IEEE-754 single precision (`"F32"`, 4 bytes per scalar).
    F32,
    /// IEEE-754 half precision (`"F16"`, 2 bytes per scalar).
    F16,
}

impl Precision {
    /// Short lowercase identifier used in `model_id` strings.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::F32 => "f32",
            Self::F16 => "f16",
        }
    }

    /// Canonical `.safetensors` dtype string.
    #[must_use]
    pub const fn dtype_str(self) -> &'static str {
        match self {
            Self::F32 => "F32",
            Self::F16 => "F16",
        }
    }

    /// Bytes per scalar element.
    #[must_use]
    pub const fn elem_bytes(self) -> usize {
        match self {
            Self::F32 => 4,
            Self::F16 => 2,
        }
    }
}

/// Converts IEEE-754 binary16 bits (`u16`) to `f32`.
#[must_use]
pub fn f16_bits_to_f32(bits: u16) -> f32 {
    let sign = u32::from(bits & 0x8000) << 16;
    let exp = (bits >> 10) & 0x1F;
    let mant = u32::from(bits & 0x03FF);

    let out_bits = if exp == 0 {
        if mant == 0 {
            sign
        } else {
            // Normalize subnormal binary16 mantissa into IEEE-754 binary32.
            let mut m = mant;
            let mut e: i32 = -14;
            while (m & 0x0400) == 0 {
                m <<= 1;
                e -= 1;
            }
            m &= 0x03FF;
            let f32_exp = u32::try_from(e + 127).unwrap_or(0);
            sign | (f32_exp << 23) | (m << 13)
        }
    } else if exp == 0x1F {
        sign | 0x7F80_0000 | (mant << 13)
    } else {
        let f32_exp = u32::from(exp) + (127 - 15);
        sign | (f32_exp << 23) | (mant << 13)
    };
    f32::from_bits(out_bits)
}

/// Converts `f32` to IEEE-754 binary16 bits (`u16`) using round-to-nearest-even.
#[must_use]
pub fn f32_to_f16_bits(val: f32) -> u16 {
    let bits = val.to_bits();
    let sign = ((bits >> 16) & 0x8000) as u16;
    let exp = ((bits >> 23) & 0xFF) as i32;
    let mant = bits & 0x007F_FFFF;

    if exp == 255 {
        if mant == 0 {
            return sign | 0x7C00;
        }
        let nan_payload = ((mant >> 13) as u16) | 1;
        return sign | 0x7C00 | (nan_payload & 0x03FF);
    }

    let unbiased = exp - 127;
    if unbiased > 15 {
        // Overflow to signed infinity.
        return sign | 0x7C00;
    }
    if unbiased < -24 {
        // Underflow to signed zero.
        return sign;
    }
    if unbiased < -14 {
        // Subnormal binary16 with round-to-nearest-even.
        let full_mant = mant | 0x0080_0000;
        let shift = u32::try_from(-1 - unbiased).unwrap_or(24);
        let mant16 = (full_mant >> shift) as u16;
        let rem_mask = (1u32 << shift) - 1;
        let rem = full_mant & rem_mask;
        let halfway = 1u32 << (shift - 1);
        let round_up = rem > halfway || (rem == halfway && (mant16 & 1) == 1);
        return sign | mant16.saturating_add(u16::from(round_up));
    }

    let h_exp = u16::try_from(unbiased + 15).unwrap_or(0);
    let mant16 = (mant >> 13) as u16;
    let rem = mant & 0x1FFF;
    let round_up = rem > 0x1000 || (rem == 0x1000 && (mant16 & 1) == 1);
    let combined = (h_exp << 10) | mant16;
    sign | combined.saturating_add(u16::from(round_up))
}

/// Tensor weight storage preserving exact `F32` or quantized `F16` backing representations.
#[derive(Debug, Clone, PartialEq)]
pub enum TensorData {
    /// Unquantized 32-bit floats.
    F32(Vec<f32>),
    /// IEEE-754 binary16 scalars stored as raw `u16` bits.
    F16(Vec<u16>),
}

impl TensorData {
    /// Returns the storage precision of this tensor.
    #[must_use]
    pub const fn precision(&self) -> Precision {
        match self {
            Self::F32(_) => Precision::F32,
            Self::F16(_) => Precision::F16,
        }
    }

    /// Returns the number of scalar elements.
    #[must_use]
    pub fn len(&self) -> usize {
        match self {
            Self::F32(v) => v.len(),
            Self::F16(v) => v.len(),
        }
    }

    /// Returns `true` if empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Reads scalar at `index` as `f32`.
    #[must_use]
    pub fn get_f32(&self, index: usize) -> Option<f32> {
        match self {
            Self::F32(v) => v.get(index).copied(),
            Self::F16(v) => v.get(index).copied().map(f16_bits_to_f32),
        }
    }

    /// Decodes all elements into `f32`.
    #[must_use]
    pub fn to_vec_f32(&self) -> Vec<f32> {
        match self {
            Self::F32(v) => v.clone(),
            Self::F16(v) => v.iter().copied().map(f16_bits_to_f32).collect(),
        }
    }
}

/// Decoded tensor entry paired with its validated shape.
#[derive(Debug, Clone, PartialEq)]
pub struct LoadedTensor {
    /// Tensor dimensions.
    pub shape: Vec<usize>,
    /// Underlying scalar storage.
    pub data: TensorData,
}

impl LoadedTensor {
    /// Returns the storage precision of this tensor.
    #[must_use]
    pub const fn precision(&self) -> Precision {
        match &self.data {
            TensorData::F32(_) => Precision::F32,
            TensorData::F16(_) => Precision::F16,
        }
    }
}

/// Validated `.safetensors` archive loaded via POSIX read-only `mmap` (or raw bytes in tests).
#[derive(Debug, Clone)]
pub struct SafeTensorsArchive {
    /// Full-file SHA-256 hex digest.
    pub file_sha256: String,
    /// Map from tensor name to validated shape & weights.
    pub tensors: BTreeMap<String, LoadedTensor>,
}

impl SafeTensorsArchive {
    /// Opens `path` using [`MmapBytes`], validates header and offsets, and decodes tensors.
    ///
    /// # Errors
    ///
    /// Returns [`GapError`] with [`ErrorCode::DiscoveryDegraded`] on I/O, malformed header, out-of-bounds
    /// offset, unsupported dtype, or non-finite weight values.
    pub fn load_mmap(path: &Path) -> GapResult<Self> {
        let mapped = MmapBytes::open(path)?;
        Self::from_bytes(mapped.as_slice())
    }

    /// Parses and validates a `.safetensors` byte buffer.
    ///
    /// # Errors
    ///
    /// Returns [`GapError`] with [`ErrorCode::DiscoveryDegraded`] on malformed header, truncated payload,
    /// invalid tensor bounds, or non-finite scalar weights.
    pub fn from_bytes(bytes: &[u8]) -> GapResult<Self> {
        if bytes.len() < 8 {
            return Err(GapError::new(
                ErrorCode::DiscoveryDegraded,
                "corrupt safetensors: fewer than 8 header length bytes",
            ));
        }
        let header_len_u64 = u64::from_le_bytes([
            bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
        ]);
        let header_len = usize::try_from(header_len_u64).map_err(|_| {
            GapError::new(
                ErrorCode::DiscoveryDegraded,
                "corrupt safetensors: header length overflows usize",
            )
        })?;
        if header_len == 0 || header_len > MAX_HEADER_BYTES {
            return Err(GapError::new(
                ErrorCode::DiscoveryDegraded,
                format!("corrupt safetensors: invalid header length {header_len}"),
            ));
        }
        let data_start = 8usize.checked_add(header_len).ok_or_else(|| {
            GapError::new(
                ErrorCode::DiscoveryDegraded,
                "corrupt safetensors: header length arithmetic overflow",
            )
        })?;
        if data_start > bytes.len() {
            return Err(GapError::new(
                ErrorCode::DiscoveryDegraded,
                format!(
                    "truncated safetensors: header ends at {data_start}, file has {} bytes",
                    bytes.len()
                ),
            ));
        }
        let header_str = std::str::from_utf8(&bytes[8..data_start]).map_err(|err| {
            GapError::new(
                ErrorCode::DiscoveryDegraded,
                format!("corrupt safetensors header UTF-8: {err}"),
            )
        })?;
        let header_json = parse_json(header_str).map_err(|err| {
            GapError::new(
                ErrorCode::DiscoveryDegraded,
                format!("corrupt safetensors JSON header: {err}"),
            )
        })?;
        let Some(root_obj) = header_json.as_object() else {
            return Err(GapError::new(
                ErrorCode::DiscoveryDegraded,
                "corrupt safetensors: header root must be a JSON object",
            ));
        };

        let payload = &bytes[data_start..];
        let mut tensors = BTreeMap::new();

        for (name, meta_val) in root_obj {
            if name == "__metadata__" {
                continue;
            }
            let Some(meta) = meta_val.as_object() else {
                return Err(GapError::new(
                    ErrorCode::DiscoveryDegraded,
                    format!("corrupt safetensors tensor `{name}` entry is not an object"),
                ));
            };
            let dtype = meta
                .get("dtype")
                .and_then(JsonValue::as_str)
                .ok_or_else(|| {
                    GapError::new(
                        ErrorCode::DiscoveryDegraded,
                        format!("corrupt safetensors tensor `{name}` missing string dtype"),
                    )
                })?;
            let precision = match dtype {
                "F32" => Precision::F32,
                "F16" => Precision::F16,
                other => {
                    return Err(GapError::new(
                        ErrorCode::DiscoveryDegraded,
                        format!("unsupported safetensors dtype `{other}` for `{name}`"),
                    ));
                }
            };

            let shape_arr = meta
                .get("shape")
                .and_then(JsonValue::as_array)
                .ok_or_else(|| {
                    GapError::new(
                        ErrorCode::DiscoveryDegraded,
                        format!("corrupt safetensors tensor `{name}` missing shape array"),
                    )
                })?;
            let mut shape = Vec::with_capacity(shape_arr.len());
            let mut elem_count: usize = 1;
            for dim_val in shape_arr {
                let dim = dim_val.as_usize().ok_or_else(|| {
                    GapError::new(
                        ErrorCode::DiscoveryDegraded,
                        format!("corrupt safetensors tensor `{name}` has invalid dimension"),
                    )
                })?;
                elem_count = elem_count.checked_mul(dim).ok_or_else(|| {
                    GapError::new(
                        ErrorCode::DiscoveryDegraded,
                        format!("corrupt safetensors tensor `{name}` shape overflow"),
                    )
                })?;
                shape.push(dim);
            }
            let expected_bytes =
                elem_count
                    .checked_mul(precision.elem_bytes())
                    .ok_or_else(|| {
                        GapError::new(
                            ErrorCode::DiscoveryDegraded,
                            format!("corrupt safetensors tensor `{name}` byte size overflow"),
                        )
                    })?;

            let offsets_arr = meta
                .get("data_offsets")
                .and_then(JsonValue::as_array)
                .ok_or_else(|| {
                    GapError::new(
                        ErrorCode::DiscoveryDegraded,
                        format!("corrupt safetensors tensor `{name}` missing data_offsets"),
                    )
                })?;
            if offsets_arr.len() != 2 {
                return Err(GapError::new(
                    ErrorCode::DiscoveryDegraded,
                    format!("corrupt safetensors tensor `{name}` data_offsets must have len 2"),
                ));
            }
            let begin = offsets_arr[0].as_usize().ok_or_else(|| {
                GapError::new(
                    ErrorCode::DiscoveryDegraded,
                    format!("corrupt safetensors tensor `{name}` begin offset invalid"),
                )
            })?;
            let end = offsets_arr[1].as_usize().ok_or_else(|| {
                GapError::new(
                    ErrorCode::DiscoveryDegraded,
                    format!("corrupt safetensors tensor `{name}` end offset invalid"),
                )
            })?;
            if begin > end || end > payload.len() || (end - begin) != expected_bytes {
                return Err(GapError::new(
                    ErrorCode::DiscoveryDegraded,
                    format!(
                        "corrupt safetensors tensor `{name}` out-of-bounds or mismatched byte span [{begin}, {end}) vs expected {expected_bytes} within payload {}",
                        payload.len()
                    ),
                ));
            }

            let raw_slice = &payload[begin..end];
            let data = match precision {
                Precision::F32 => {
                    let mut out = Vec::with_capacity(elem_count);
                    let (chunks, _) = raw_slice.as_chunks::<4>();
                    for chunk in chunks {
                        let f = f32::from_le_bytes(*chunk);
                        if !f.is_finite() {
                            return Err(GapError::new(
                                ErrorCode::DiscoveryDegraded,
                                format!("non-finite F32 weight detected in tensor `{name}`"),
                            ));
                        }
                        out.push(f);
                    }
                    TensorData::F32(out)
                }
                Precision::F16 => {
                    let mut out = Vec::with_capacity(elem_count);
                    let (chunks, _) = raw_slice.as_chunks::<2>();
                    for chunk in chunks {
                        let bits = u16::from_le_bytes(*chunk);
                        let f = f16_bits_to_f32(bits);
                        if !f.is_finite() {
                            return Err(GapError::new(
                                ErrorCode::DiscoveryDegraded,
                                format!("non-finite F16 weight detected in tensor `{name}`"),
                            ));
                        }
                        out.push(bits);
                    }
                    TensorData::F16(out)
                }
            };

            tensors.insert(name.clone(), LoadedTensor { shape, data });
        }

        if tensors.is_empty() {
            return Err(GapError::new(
                ErrorCode::DiscoveryDegraded,
                "corrupt safetensors: archive contains no tensors",
            ));
        }

        Ok(Self {
            file_sha256: sha256_hex(bytes),
            tensors,
        })
    }

    /// Borrows tensor `name` or returns a typed `DiscoveryDegraded` error.
    pub fn tensor(&self, name: &str) -> GapResult<&LoadedTensor> {
        self.tensors.get(name).ok_or_else(|| {
            GapError::new(
                ErrorCode::DiscoveryDegraded,
                format!("missing tensor `{name}` in S1 safetensors checkpoint"),
            )
        })
    }
}

/// Serializes named tensors into a canonical `.safetensors` byte stream (`F32` or `F16`).
///
/// # Errors
///
/// Returns [`GapError`] if any tensor's slice length mismatches its shape product or contains NaN/Inf.
pub fn serialize_safetensors(
    tensors: &[(&str, &[usize], &[f32])],
    precision: Precision,
) -> GapResult<Vec<u8>> {
    let mut header_entries = Vec::with_capacity(tensors.len());
    let mut data_buf = Vec::new();

    for &(name, shape, values) in tensors {
        let mut expected: usize = 1;
        for &d in shape {
            expected = expected.checked_mul(d).ok_or_else(|| {
                GapError::new(
                    ErrorCode::ValidationError,
                    format!("shape overflow serializing tensor `{name}`"),
                )
            })?;
        }
        if expected != values.len() {
            return Err(GapError::new(
                ErrorCode::ValidationError,
                format!(
                    "tensor `{name}` slice len {} != shape product {expected}",
                    values.len()
                ),
            ));
        }

        let begin = data_buf.len();
        match precision {
            Precision::F32 => {
                for &v in values {
                    if !v.is_finite() {
                        return Err(GapError::new(
                            ErrorCode::ValidationError,
                            format!("non-finite weight in tensor `{name}`"),
                        ));
                    }
                    data_buf.extend_from_slice(&v.to_le_bytes());
                }
            }
            Precision::F16 => {
                for &v in values {
                    if !v.is_finite() {
                        return Err(GapError::new(
                            ErrorCode::ValidationError,
                            format!("non-finite weight in tensor `{name}`"),
                        ));
                    }
                    let bits = f32_to_f16_bits(v);
                    if !f16_bits_to_f32(bits).is_finite() {
                        return Err(GapError::new(
                            ErrorCode::ValidationError,
                            format!("f16 overflow converting weight `{v}` in tensor `{name}`"),
                        ));
                    }
                    data_buf.extend_from_slice(&bits.to_le_bytes());
                }
            }
        }
        let end = data_buf.len();

        let mut shape_str = String::new();
        for (idx, &d) in shape.iter().enumerate() {
            if idx > 0 {
                shape_str.push(',');
            }
            let _ = write!(shape_str, "{d}");
        }
        header_entries.push(format!(
            "\"{}\":{{\"dtype\":\"{}\",\"shape\":[{shape_str}],\"data_offsets\":[{begin},{end}]}}",
            escape_json_str(name),
            precision.dtype_str()
        ));
    }

    let mut header_json = format!("{{{}}}", header_entries.join(","));
    while (header_json.len() % 8) != 0 {
        header_json.push(' ');
    }
    let header_bytes = header_json.as_bytes();
    let header_len_u64 = header_bytes.len() as u64;

    let mut out = Vec::with_capacity(8 + header_bytes.len() + data_buf.len());
    out.extend_from_slice(&header_len_u64.to_le_bytes());
    out.extend_from_slice(header_bytes);
    out.extend_from_slice(&data_buf);
    Ok(out)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn f16_roundtrip_preserves_normal_subnormal_and_zero() {
        let samples = [0.0f32, -0.0, 1.0, -2.5, 0.00012207031, 65504.0];
        for s in samples {
            let bits = f32_to_f16_bits(s);
            let back = f16_bits_to_f32(bits);
            let tol = if s.abs() < 1e-3 { 1e-5 } else { s.abs() * 2e-3 };
            assert!((back - s).abs() <= tol, "sample={s} back={back}");
        }
    }

    #[test]
    fn safetensors_serialize_and_parse_roundtrip_f32_and_f16() {
        let weights = [0.25f32, -0.5, 1.5, -2.0];
        let f32_bytes =
            serialize_safetensors(&[("w", &[2, 2], &weights)], Precision::F32).expect("serialize");
        let f32_arch = SafeTensorsArchive::from_bytes(&f32_bytes).expect("parse f32");
        assert_eq!(f32_arch.tensor("w").unwrap().data.to_vec_f32(), weights);

        let f16_bytes =
            serialize_safetensors(&[("w", &[2, 2], &weights)], Precision::F16).expect("serialize");
        let f16_arch = SafeTensorsArchive::from_bytes(&f16_bytes).expect("parse f16");
        assert_eq!(f16_arch.tensor("w").unwrap().precision(), Precision::F16);
        assert_eq!(f16_arch.tensor("w").unwrap().data.to_vec_f32(), weights);
    }
}
