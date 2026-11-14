//! Bottom-k blake3 column sketch (`NGC-050`, `CMP-050`).

use std::collections::BTreeSet;

use gap_core::error::{NgcErrorCode, NgcErrorInfo};
use gap_proto::ngc::v1::SketchResponse;

use crate::driver::{SqlRow, SqlValue};

pub const MAX_SAMPLE: u32 = 100_000;
pub const DEFAULT_K: u32 = 1024;

/// Computes column summary statistics and salted bottom-k hashes (`NGC-050`).
///
/// The caller should request `sample_size + 1` rows from the SQL driver so `exact` can distinguish
/// a domain that fit within `sample_size` (`rows.len() <= sample_cap`) from one that exceeded it.
pub fn compute(
    rows: &[SqlRow],
    field_path: &str,
    salt: &[u8],
    bottom_k: u32,
    sample_size: u32,
) -> Result<SketchResponse, NgcErrorInfo> {
    if bottom_k == 0 {
        return Err(NgcErrorInfo::new(
            NgcErrorCode::InvalidArgument,
            "bottom_k must be >= 1",
        ));
    }
    let k = bottom_k as usize;
    let cap = sample_size.clamp(1, MAX_SAMPLE) as usize;
    let exact = rows.len() <= cap;

    let mut sample_count: u32 = 0;
    let mut null_count: u32 = 0;
    let mut all_hashes: BTreeSet<u64> = BTreeSet::new();
    let mut value_kind = "other".to_owned();

    for row in rows.iter().take(cap) {
        sample_count = sample_count.saturating_add(1);
        let Some(val) = row.get(field_path) else {
            null_count = null_count.saturating_add(1);
            continue;
        };

        let canonical = match val {
            SqlValue::Null => {
                null_count = null_count.saturating_add(1);
                continue;
            }
            SqlValue::Bool(b) => {
                value_kind = "other".to_owned();
                b.to_string()
            }
            SqlValue::Integer(n) => {
                value_kind = "integer".to_owned();
                n.to_string()
            }
            SqlValue::Real(f) => {
                value_kind = "float".to_owned();
                let norm = if *f == 0.0 { 0.0 } else { *f };
                format!("{norm}")
            }
            SqlValue::Text(s) => {
                value_kind = if looks_like_uuid(s) {
                    "uuid".to_owned()
                } else {
                    "string".to_owned()
                };
                s.clone()
            }
            SqlValue::Blob(b) => {
                value_kind = "other".to_owned();
                b.iter().map(|x| format!("{x:02x}")).collect()
            }
        };

        all_hashes.insert(blake3_truncated(salt, canonical.as_bytes()));
    }

    let distinct_estimate = all_hashes.len() as u64;
    let non_null = sample_count.saturating_sub(null_count);
    let null_ratio = if sample_count == 0 {
        0.0
    } else {
        f64::from(null_count) / f64::from(sample_count)
    };
    let uniqueness_ratio = if non_null == 0 {
        0.0
    } else {
        distinct_estimate as f64 / f64::from(non_null)
    };
    let bottom_k_hashes: Vec<u64> = all_hashes.into_iter().take(k).collect();

    Ok(SketchResponse {
        distinct_estimate,
        null_ratio,
        uniqueness_ratio,
        bottom_k_hashes,
        sample_count,
        value_kind,
        exact,
    })
}

fn blake3_truncated(salt: &[u8], value: &[u8]) -> u64 {
    let mut h = blake3::Hasher::new();
    h.update(salt);
    h.update(value);
    let b = h.finalize();
    let bytes = b.as_bytes();
    u64::from_le_bytes([
        bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
    ])
}

fn looks_like_uuid(s: &str) -> bool {
    s.len() == 36
        && s.chars().enumerate().all(|(i, c)| {
            if i == 8 || i == 13 || i == 18 || i == 23 {
                c == '-'
            } else {
                c.is_ascii_hexdigit()
            }
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn row_of(col: &str, val: SqlValue) -> SqlRow {
        let mut m = BTreeMap::new();
        m.insert(col.to_owned(), val);
        m
    }

    // spec: NGC-050
    #[test]
    fn sketch_computes_sorted_salted_hashes_and_exact_flag() -> Result<(), NgcErrorInfo> {
        let rows = vec![
            row_of("id", SqlValue::Integer(1)),
            row_of("id", SqlValue::Integer(2)),
            row_of("id", SqlValue::Integer(2)),
            row_of("id", SqlValue::Null),
        ];

        // With cap=5 and 4 rows returned (`rows.len() <= cap`), domain fit inside sample -> exact=true.
        let exact_resp = compute(&rows, "id", b"salt-a", 2, 5)?;
        assert!(exact_resp.exact);
        assert_eq!(exact_resp.sample_count, 4);
        assert_eq!(exact_resp.distinct_estimate, 2);
        assert!((exact_resp.null_ratio - 0.25).abs() < 1e-9);
        assert_eq!(exact_resp.value_kind, "integer");
        assert_eq!(exact_resp.bottom_k_hashes.len(), 2);
        assert!(exact_resp.bottom_k_hashes[0] < exact_resp.bottom_k_hashes[1]);

        // Different salt changes hashes deterministically.
        let other_salt = compute(&rows, "id", b"salt-b", 2, 5)?;
        assert_ne!(exact_resp.bottom_k_hashes, other_salt.bottom_k_hashes);

        // With cap=3 and 4 rows supplied (`cap + 1`), sample_count=3 and exact=false.
        let truncated_resp = compute(&rows, "id", b"salt-a", 10, 3)?;
        assert!(!truncated_resp.exact);
        assert_eq!(truncated_resp.sample_count, 3);
        Ok(())
    }

    // spec: NGC-050
    #[test]
    fn sketch_normalizes_negative_zero_real_and_sets_float_kind() -> Result<(), NgcErrorInfo> {
        let a = vec![row_of("score", SqlValue::Real(-0.0))];
        let b = vec![row_of("score", SqlValue::Real(0.0))];
        let ra = compute(&a, "score", b"s", 10, 10)?;
        let rb = compute(&b, "score", b"s", 10, 10)?;
        assert_eq!(ra.value_kind, "float");
        assert_eq!(ra.bottom_k_hashes, rb.bottom_k_hashes);
        Ok(())
    }
}
