//! RFC 8785 JSON Canonicalization Scheme (JCS) serializer (`spec/00-conventions.md` §7, `NGC-011`).
//!
//! Produces deterministic UTF-8 JSON with:
//! - ECMAScript `Number::toString` formatting for IEEE-754 binary64 numbers (`-0.0` -> `"0"`,
//!   non-finite rejected with [`CanonicalError::NonFiniteNumber`]),
//! - JCS minimal string escaping (`\b`, `\t`, `\n`, `\f`, `\r`, `\"`, `\\`, `\u00xx` lowercase hex
//!   for remaining `U+0000..=U+001F`, raw UTF-8 for `U+0020..=U+10FFFF`, `/` unescaped),
//! - lexicographic object property ordering by UTF-16 code units,
//! - zero insignificant whitespace, using an explicit work stack so deeply nested JSON cannot
//!   overflow the call stack.

use std::cmp::Ordering;
use std::fmt::Write as _;

use serde_json::Value;
use thiserror::Error;

/// Failure returned when canonicalizing a JSON value or IEEE-754 number under RFC 8785.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum CanonicalError {
    /// RFC 8785 §3.2.2.3 forbids `NaN`, `+Infinity`, `-Infinity`, and numbers outside binary64.
    #[error("RFC 8785 canonical JSON rejects non-finite IEEE-754 number")]
    NonFiniteNumber,
}

/// Compares two UTF-8 strings by their UTF-16 code-unit sequences per RFC 8785 §3.2.3.
#[inline]
#[must_use]
pub fn cmp_utf16_code_units(a: &str, b: &str) -> Ordering {
    a.encode_utf16().cmp(b.encode_utf16())
}

fn append_canonical_string(value: &str, out: &mut String) {
    const HEX_LOWER: &[u8; 16] = b"0123456789abcdef";

    out.push('"');
    for ch in value.chars() {
        match ch {
            '\u{0008}' => out.push_str("\\b"),
            '\u{0009}' => out.push_str("\\t"),
            '\u{000A}' => out.push_str("\\n"),
            '\u{000C}' => out.push_str("\\f"),
            '\u{000D}' => out.push_str("\\r"),
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{0000}'..='\u{001F}' => {
                let b = ch as u8;
                out.push_str("\\u00");
                out.push(char::from(HEX_LOWER[usize::from(b >> 4)]));
                out.push(char::from(HEX_LOWER[usize::from(b & 0x0f)]));
            }
            _ => out.push(ch),
        }
    }
    out.push('"');
}

fn extract_ryu_decimal(abs: f64) -> Result<(String, i32), CanonicalError> {
    // Delegate binary64 -> shortest round-to-even decimal conversion to `serde_json` (`ryu::d2d`),
    // which implements exact IEEE-754 / ECMA-262 even-digit tie-breaking (unlike `core::fmt`'s
    // Grisu3/Dragon4 fallback on midpoint ties such as `0x430c_6bf5_2634_0002`).
    let num = serde_json::Number::from_f64(abs).ok_or(CanonicalError::NonFiniteNumber)?;
    let ryu_str = serde_json::to_string(&num).map_err(|_| CanonicalError::NonFiniteNumber)?;

    if let Some((mantissa, exp_str)) = ryu_str.split_once('e') {
        let exp: i32 = exp_str
            .parse()
            .map_err(|_| CanonicalError::NonFiniteNumber)?;
        let mut digits = String::with_capacity(17);
        for ch in mantissa.chars() {
            if ch.is_ascii_digit() {
                digits.push(ch);
            }
        }
        while digits.len() > 1 && digits.ends_with('0') {
            digits.pop();
        }
        if digits.is_empty() {
            return Err(CanonicalError::NonFiniteNumber);
        }
        Ok((digits, exp.saturating_add(1)))
    } else if let Some((int_part, frac_part)) = ryu_str.split_once('.') {
        if int_part == "0" {
            let leading_zeros = frac_part.bytes().take_while(|&b| b == b'0').count();
            let sig = frac_part[leading_zeros..].trim_end_matches('0');
            if sig.is_empty() {
                return Err(CanonicalError::NonFiniteNumber);
            }
            let z_i32 =
                i32::try_from(leading_zeros).map_err(|_| CanonicalError::NonFiniteNumber)?;
            Ok((sig.to_owned(), -z_i32))
        } else {
            let n = i32::try_from(int_part.len()).map_err(|_| CanonicalError::NonFiniteNumber)?;
            let mut digits = String::with_capacity(int_part.len() + frac_part.len());
            digits.push_str(int_part);
            if frac_part != "0" {
                digits.push_str(frac_part);
            }
            while digits.len() > 1 && digits.ends_with('0') {
                digits.pop();
            }
            Ok((digits, n))
        }
    } else {
        let n = i32::try_from(ryu_str.len()).map_err(|_| CanonicalError::NonFiniteNumber)?;
        let digits = ryu_str.trim_end_matches('0');
        if digits.is_empty() {
            return Err(CanonicalError::NonFiniteNumber);
        }
        Ok((digits.to_owned(), n))
    }
}

fn append_canonical_number(value: f64, out: &mut String) -> Result<(), CanonicalError> {
    if !value.is_finite() {
        return Err(CanonicalError::NonFiniteNumber);
    }
    if value == 0.0 {
        out.push('0');
        return Ok(());
    }

    let abs = if value.is_sign_negative() {
        out.push('-');
        -value
    } else {
        value
    };

    let (digits, n) = extract_ryu_decimal(abs)?;
    let k = digits.len();
    let k_i32 = i32::try_from(k).map_err(|_| CanonicalError::NonFiniteNumber)?;
    let exp = n.saturating_sub(1);

    if k_i32 <= n && n <= 21 {
        // ECMA-262 step 6: integer without decimal point, padded with trailing zeros.
        out.push_str(&digits);
        let zeros = usize::try_from(n - k_i32).unwrap_or(0);
        for _ in 0..zeros {
            out.push('0');
        }
    } else if 0 < n && n <= 21 {
        // ECMA-262 step 7: fixed-point with `n` integer digits and `k - n` fractional digits.
        let split_at = usize::try_from(n).unwrap_or(0);
        out.push_str(&digits[..split_at]);
        out.push('.');
        out.push_str(&digits[split_at..]);
    } else if -6 < n && n <= 0 {
        // ECMA-262 step 8: `0.` followed by `-n` leading fractional zeros and `s`.
        out.push_str("0.");
        let zeros = usize::try_from(-n).unwrap_or(0);
        for _ in 0..zeros {
            out.push('0');
        }
        out.push_str(&digits);
    } else {
        // ECMA-262 steps 9-10: scientific notation `<d>[.<frac>]e<+|-><|exp|>`.
        out.push_str(&digits[..1]);
        if k > 1 {
            out.push('.');
            out.push_str(&digits[1..]);
        }
        out.push('e');
        if exp >= 0 {
            out.push('+');
        } else {
            out.push('-');
        }
        let _ = write!(out, "{}", exp.unsigned_abs());
    }

    Ok(())
}

/// Formats a single IEEE-754 `f64` according to RFC 8785 §3.2.2.3 / ECMA-262 `Number::toString`.
///
/// # Errors
///
/// Returns [`CanonicalError::NonFiniteNumber`] if `value` is `NaN` or infinite.
pub fn canonical_number(value: f64) -> Result<String, CanonicalError> {
    let mut out = String::with_capacity(24);
    append_canonical_number(value, &mut out)?;
    Ok(out)
}

/// Formats a string literal (including surrounding quotes) per RFC 8785 §3.2.2.2.
#[must_use]
pub fn canonical_string(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    append_canonical_string(value, &mut out);
    out
}

enum Work<'a> {
    Value(&'a Value),
    Key(&'a str),
    Literal(&'static str),
}

/// Serializes `value` into RFC 8785 canonical JSON (`spec/00-conventions.md` §7, `NGC-011`).
///
/// # Errors
///
/// Returns [`CanonicalError::NonFiniteNumber`] if any number in `value` cannot be represented as a
/// finite IEEE-754 binary64 float.
pub fn canonical_json(value: &Value) -> Result<String, CanonicalError> {
    let mut out = String::with_capacity(64);
    let mut stack = vec![Work::Value(value)];

    while let Some(item) = stack.pop() {
        match item {
            Work::Literal(lit) => out.push_str(lit),
            Work::Key(key) => append_canonical_string(key, &mut out),
            Work::Value(val) => match val {
                Value::Null => out.push_str("null"),
                Value::Bool(true) => out.push_str("true"),
                Value::Bool(false) => out.push_str("false"),
                Value::Number(num) => {
                    let f = num.as_f64().ok_or(CanonicalError::NonFiniteNumber)?;
                    append_canonical_number(f, &mut out)?;
                }
                Value::String(s) => append_canonical_string(s, &mut out),
                Value::Array(items) => {
                    if items.is_empty() {
                        out.push_str("[]");
                    } else {
                        stack.push(Work::Literal("]"));
                        for (idx, elem) in items.iter().enumerate().rev() {
                            stack.push(Work::Value(elem));
                            if idx > 0 {
                                stack.push(Work::Literal(","));
                            }
                        }
                        stack.push(Work::Literal("["));
                    }
                }
                Value::Object(map) => {
                    if map.is_empty() {
                        out.push_str("{}");
                    } else {
                        let mut entries: Vec<(&str, &Value)> =
                            map.iter().map(|(k, v)| (k.as_str(), v)).collect();
                        entries.sort_by(|(ka, _), (kb, _)| cmp_utf16_code_units(ka, kb));

                        stack.push(Work::Literal("}"));
                        for (idx, (k, v)) in entries.into_iter().enumerate().rev() {
                            stack.push(Work::Value(v));
                            stack.push(Work::Literal(":"));
                            stack.push(Work::Key(k));
                            if idx > 0 {
                                stack.push(Work::Literal(","));
                            }
                        }
                        stack.push(Work::Literal("{"));
                    }
                }
            },
        }
    }

    Ok(out)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use proptest::prelude::*;
    use serde_json::{Value, json};

    use super::*;

    // spec: NGC-011
    #[test]
    fn canonical_number_matches_ecmascript_table_of_known_doubles() {
        let cases: &[(f64, &str)] = &[
            (0.0, "0"),
            (-0.0, "0"),
            (1.0, "1"),
            (-1.0, "-1"),
            (2.0, "2"),
            (0.5, "0.5"),
            (-0.5, "-0.5"),
            (0.25, "0.25"),
            (0.1, "0.1"),
            (0.2, "0.2"),
            (0.1 + 0.2, "0.30000000000000004"),
            (0.3, "0.3"),
            (1.5, "1.5"),
            (10.0, "10"),
            (100.0, "100"),
            (1000.5, "1000.5"),
            (-12345.6789, "-12345.6789"),
            // Powers of ten around the 1e-6 / 1e-7 fixed-to-exponential boundary (n = -5 vs n = -6)
            (1e-1, "0.1"),
            (1e-2, "0.01"),
            (1e-3, "0.001"),
            (1e-4, "0.0001"),
            (1e-5, "0.00001"),
            (1e-6, "0.000001"),
            (9.999_999_999_999_997e-7, "9.999999999999997e-7"),
            (1e-7, "1e-7"),
            (-1e-6, "-0.000001"),
            (-1e-7, "-1e-7"),
            (1.2345e-7, "1.2345e-7"),
            (1e-20, "1e-20"),
            (1e-308, "1e-308"),
            // Smallest positive normal and subnormal IEEE-754 binary64 values
            (f64::MIN_POSITIVE, "2.2250738585072014e-308"),
            (5e-324, "5e-324"),
            (-5e-324, "-5e-324"),
            (f64::from_bits(2), "1e-323"),
            // Powers of ten around the 1e20 / 1e21 integer-to-exponential boundary (n = 21 vs n = 22)
            (1e15, "1000000000000000"),
            (1e18, "1000000000000000000"),
            (1e19, "10000000000000000000"),
            (1e20, "100000000000000000000"),
            (99_999_999_999_999_980_000.0, "99999999999999980000"),
            (999_999_999_999_999_900_000.0, "999999999999999900000"),
            (1e21, "1e+21"),
            (-1e20, "-100000000000000000000"),
            (-1e21, "-1e+21"),
            (1.5e21, "1.5e+21"),
            (1e30, "1e+30"),
            // Largest finite IEEE-754 binary64 magnitudes
            (1.797_693_134_862_315_7e308, "1.7976931348623157e+308"),
            (-1.797_693_134_862_315_7e308, "-1.7976931348623157e+308"),
            // RFC 8785 Appendix B & IEEE-754 exact safe integer boundary (2^53 - 1 and 2^53)
            (f64::from_bits(0x41b3_de43_5555_5555), "333333333.3333333"),
            (333_333_333.333_333_3, "333333333.3333333"),
            (4.50, "4.5"),
            (2e-3, "0.002"),
            (0.000_000_1, "1e-7"),
            (9_007_199_254_740_990.0, "9007199254740990"),
            (9_007_199_254_740_991.0, "9007199254740991"),
            (9_007_199_254_740_992.0, "9007199254740992"),
            (-9_007_199_254_740_991.0, "-9007199254740991"),
            (-9_007_199_254_740_992.0, "-9007199254740992"),
            // Midpoint even-digit tie-breaker (`1000000000000000.25` rounds to `.2`, not `.3`)
            (f64::from_bits(0x430c_6bf5_2634_0002), "1000000000000000.2"),
        ];
        assert!(cases.len() >= 40);
        for &(input, expected) in cases {
            let actual = canonical_number(input)
                .unwrap_or_else(|e| panic!("canonical_number({input:?}) failed: {e}"));
            assert_eq!(actual, expected, "mismatch for binary64 {input:?}");
        }
    }

    // spec: NGC-011
    #[test]
    fn canonical_number_rejects_nan_and_infinities() {
        assert_eq!(
            canonical_number(f64::NAN),
            Err(CanonicalError::NonFiniteNumber)
        );
        assert_eq!(
            canonical_number(-f64::NAN),
            Err(CanonicalError::NonFiniteNumber)
        );
        assert_eq!(
            canonical_number(f64::INFINITY),
            Err(CanonicalError::NonFiniteNumber)
        );
        assert_eq!(
            canonical_number(f64::NEG_INFINITY),
            Err(CanonicalError::NonFiniteNumber)
        );
    }

    // spec: NGC-011
    #[test]
    fn canonical_json_sorts_object_keys_by_utf16_code_units_not_utf8_bytes() {
        // Supplementary plane U+10000 encodes to UTF-16 [0xD800, 0xDC00] (UTF-8: F0 90 80 80),
        // whereas BMP U+E000 encodes to UTF-16 [0xE000] (UTF-8: EE 80 80).
        // Under UTF-16 code-unit order: U+10000 (0xD800) < U+E000 (0xE000).
        // Under UTF-8 byte order: U+E000 (0xEE) < U+10000 (0xF0).
        let supplementary = "\u{10000}";
        let bmp_above_surrogates = "\u{E000}";
        let v = json!({
            bmp_above_surrogates: 2,
            supplementary: 1,
        });
        let expected = format!("{{\"{supplementary}\":1,\"{bmp_above_surrogates}\":2}}");
        assert_eq!(canonical_json(&v).expect("canonical"), expected);

        // RFC 8785 §3.2.3 property sorting vector (U+1F600 grinning face [0xD83D, 0xDE00] before U+FB33 [0xFB33]).
        let rfc_sorting = json!({
            "\u{20ac}": "Euro Sign",
            "\r": "Carriage Return",
            "\u{fb33}": "Hebrew Letter Dalet With Dagesh",
            "1": "One",
            "\u{1f600}": "Emoji: Grinning Face",
            "\u{0080}": "Control",
            "\u{00f6}": "Latin Small Letter O With Diaeresis",
        });
        let rfc_expected = concat!(
            "{\"\\r\":\"Carriage Return\",",
            "\"1\":\"One\",",
            "\"\u{0080}\":\"Control\",",
            "\"\u{00f6}\":\"Latin Small Letter O With Diaeresis\",",
            "\"\u{20ac}\":\"Euro Sign\",",
            "\"\u{1f600}\":\"Emoji: Grinning Face\",",
            "\"\u{fb33}\":\"Hebrew Letter Dalet With Dagesh\"}"
        );
        assert_eq!(
            canonical_json(&rfc_sorting).expect("canonical"),
            rfc_expected
        );
    }

    // spec: NGC-011
    #[test]
    fn canonical_string_escapes_jcs_control_chars_quotes_backslash_and_leaves_slash_and_utf8_raw() {
        assert_eq!(canonical_string(""), "\"\"");
        assert_eq!(
            canonical_string("\u{0008}\t\n\u{000C}\r\"\\/"),
            "\"\\b\\t\\n\\f\\r\\\"\\\\/\""
        );
        assert_eq!(
            canonical_string("\u{0000}\u{0007}\u{000B}\u{000E}\u{000F}\u{001F}"),
            "\"\\u0000\\u0007\\u000b\\u000e\\u000f\\u001f\""
        );
        // DEL (U+007F) and non-ASCII scalars are >= U+0020 and MUST remain raw UTF-8.
        assert_eq!(canonical_string("A\u{007F}€😀"), "\"A\u{007F}€😀\"");
    }

    // spec: NGC-011
    #[test]
    fn canonical_json_handles_negative_zero_large_integers_and_nested_structures() {
        let neg_zero_val: Value =
            serde_json::from_str("[-0.0, -0, 1.0, 1e21, 1e-7]").expect("parse json numbers");
        assert_eq!(
            canonical_json(&neg_zero_val).expect("canonical"),
            "[0,0,1,1e+21,1e-7]"
        );

        let nested = json!({
            "numbers": [333_333_333.333_333_3_f64, 1e30, 4.50, 2e-3, 0.000_000_1],
            "string": "\u{20ac}$\u{000F}\nA'B\"\\\\\"/",
            "literals": [null, true, false],
            "safe_ints": [9_007_199_254_740_991_i64, -9_007_199_254_740_991_i64, 9_007_199_254_740_992_u64],
            "nested": { "z": [], "a": {} }
        });
        let expected = concat!(
            "{",
            "\"literals\":[null,true,false],",
            "\"nested\":{\"a\":{},\"z\":[]},",
            "\"numbers\":[333333333.3333333,1e+30,4.5,0.002,1e-7],",
            "\"safe_ints\":[9007199254740991,-9007199254740991,9007199254740992],",
            "\"string\":\"€$\\u000f\\nA'B\\\"\\\\\\\\\\\"/\"",
            "}"
        );
        assert_eq!(canonical_json(&nested).expect("canonical"), expected);
    }

    proptest! {
        #![proptest_config(ProptestConfig {
            cases: 512,
            failure_persistence: None,
            ..ProptestConfig::default()
        })]

        // spec: NGC-011
        #[test]
        fn prop_canonical_number_roundtrips_every_finite_f64_without_whitespace(bits in any::<u64>()) {
            let x = f64::from_bits(bits);
            if x.is_finite() {
                let s = canonical_number(x).expect("finite f64 must canonicalize");
                prop_assert!(!s.bytes().any(|b| b.is_ascii_whitespace()));
                if x == 0.0 {
                    prop_assert_eq!(s, "0");
                } else {
                    let parsed: f64 = s.parse().expect("canonical number must parse as f64");
                    prop_assert_eq!(parsed.to_bits(), x.to_bits());
                }
            } else {
                prop_assert_eq!(canonical_number(x), Err(CanonicalError::NonFiniteNumber));
            }
        }
    }
}
