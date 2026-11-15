//! Minimal pure-`std` RFC 8259 JSON parser and serializer for `.safetensors` headers, `vocab.json`,
//! and golden fixture vectors (`WP-M5-02`).

use std::collections::BTreeMap;
use std::fmt::Write as _;

use gap_core::error::{ErrorCode, GapError, GapResult};

const MAX_JSON_DEPTH: usize = 64;

/// Parsed JSON value tree.
#[derive(Debug, Clone, PartialEq)]
pub enum JsonValue {
    /// JSON `null`.
    Null,
    /// JSON boolean.
    Bool(bool),
    /// JSON number stored as finite `f64`.
    Number(f64),
    /// JSON UTF-8 string.
    String(String),
    /// JSON ordered array.
    Array(Vec<JsonValue>),
    /// JSON object with deterministic `BTreeMap` key order.
    Object(BTreeMap<String, JsonValue>),
}

impl JsonValue {
    /// Returns string slice if `Self::String`.
    #[must_use]
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::String(s) => Some(s.as_str()),
            _ => None,
        }
    }

    /// Returns finite `f64` if `Self::Number`.
    #[must_use]
    pub const fn as_f64(&self) -> Option<f64> {
        match self {
            Self::Number(n) if n.is_finite() => Some(*n),
            _ => None,
        }
    }

    /// Returns exact non-negative integer `usize` if `Self::Number` represents one cleanly.
    #[must_use]
    pub fn as_usize(&self) -> Option<usize> {
        let n = self.as_f64()?;
        if n < 0.0 || n.fract() != 0.0 || n > (usize::MAX as f64) {
            return None;
        }
        Some(n as usize)
    }

    /// Returns slice if `Self::Array`.
    #[must_use]
    pub fn as_array(&self) -> Option<&[Self]> {
        match self {
            Self::Array(items) => Some(items.as_slice()),
            _ => None,
        }
    }

    /// Returns map if `Self::Object`.
    #[must_use]
    pub const fn as_object(&self) -> Option<&BTreeMap<String, Self>> {
        match self {
            Self::Object(map) => Some(map),
            _ => None,
        }
    }

    /// Looks up field `key` when `self` is `Self::Object`.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&Self> {
        self.as_object().and_then(|m| m.get(key))
    }
}

/// Parses a complete UTF-8 JSON document up to `64` (`MAX_JSON_DEPTH`) nesting levels.
///
/// # Errors
///
/// Returns [`GapError`] on malformed syntax, trailing junk, non-finite numbers, or depth overflow.
pub fn parse_json(input: &str) -> GapResult<JsonValue> {
    let mut parser = Parser {
        bytes: input.as_bytes(),
        pos: 0,
    };
    let val = parser.parse_value(0)?;
    parser.skip_ws();
    if parser.pos != parser.bytes.len() {
        return Err(GapError::new(
            ErrorCode::ValidationError,
            format!("unexpected trailing JSON content at byte {}", parser.pos),
        ));
    }
    Ok(val)
}

/// Escapes `raw` for inclusion inside a JSON string literal (without surrounding quotes).
#[must_use]
pub fn escape_json_str(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for ch in raw.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out
}

struct Parser<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Parser<'a> {
    fn skip_ws(&mut self) {
        while let Some(&b) = self.bytes.get(self.pos) {
            if matches!(b, b' ' | b'\n' | b'\r' | b'\t') {
                self.pos += 1;
            } else {
                break;
            }
        }
    }

    fn err(&self, msg: impl Into<String>) -> GapError {
        GapError::new(ErrorCode::ValidationError, msg)
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn next_byte(&mut self) -> GapResult<u8> {
        let Some(&b) = self.bytes.get(self.pos) else {
            return Err(self.err("unexpected end of JSON input"));
        };
        self.pos += 1;
        Ok(b)
    }

    fn parse_value(&mut self, depth: usize) -> GapResult<JsonValue> {
        if depth > MAX_JSON_DEPTH {
            return Err(self.err("JSON nesting depth exceeded 64"));
        }
        self.skip_ws();
        let Some(b) = self.peek() else {
            return Err(self.err("unexpected end of JSON input"));
        };
        match b {
            b'{' => self.parse_object(depth + 1),
            b'[' => self.parse_array(depth + 1),
            b'"' => Ok(JsonValue::String(self.parse_string()?)),
            b't' => self.consume_literal(b"true", JsonValue::Bool(true)),
            b'f' => self.consume_literal(b"false", JsonValue::Bool(false)),
            b'n' => self.consume_literal(b"null", JsonValue::Null),
            b'-' | b'0'..=b'9' => self.parse_number(),
            other => Err(self.err(format!(
                "unexpected byte `{other}` while parsing JSON at offset {}",
                self.pos
            ))),
        }
    }

    fn consume_literal(&mut self, expected: &[u8], val: JsonValue) -> GapResult<JsonValue> {
        let end = self.pos.saturating_add(expected.len());
        let Some(slice) = self.bytes.get(self.pos..end) else {
            return Err(self.err("unexpected EOF in JSON literal"));
        };
        if slice != expected {
            return Err(self.err(format!("invalid JSON literal at offset {}", self.pos)));
        }
        self.pos = end;
        Ok(val)
    }

    fn parse_object(&mut self, depth: usize) -> GapResult<JsonValue> {
        self.pos += 1; // '{'
        self.skip_ws();
        let mut map = BTreeMap::new();
        if self.peek() == Some(b'}') {
            self.pos += 1;
            return Ok(JsonValue::Object(map));
        }
        loop {
            self.skip_ws();
            if self.peek() != Some(b'"') {
                return Err(self.err(format!("expected object string key at offset {}", self.pos)));
            }
            let key = self.parse_string()?;
            self.skip_ws();
            if self.next_byte()? != b':' {
                return Err(self.err(format!("expected `:` at offset {}", self.pos)));
            }
            let val = self.parse_value(depth)?;
            map.insert(key, val);
            self.skip_ws();
            match self.next_byte()? {
                b',' => {}
                b'}' => return Ok(JsonValue::Object(map)),
                _ => {
                    return Err(self.err(format!(
                        "expected `,` or `}}` in JSON object near offset {}",
                        self.pos
                    )));
                }
            }
        }
    }

    fn parse_array(&mut self, depth: usize) -> GapResult<JsonValue> {
        self.pos += 1; // '['
        self.skip_ws();
        let mut items = Vec::new();
        if self.peek() == Some(b']') {
            self.pos += 1;
            return Ok(JsonValue::Array(items));
        }
        loop {
            let val = self.parse_value(depth)?;
            items.push(val);
            self.skip_ws();
            match self.next_byte()? {
                b',' => {}
                b']' => return Ok(JsonValue::Array(items)),
                _ => {
                    return Err(self.err(format!(
                        "expected `,` or `]` in JSON array near offset {}",
                        self.pos
                    )));
                }
            }
        }
    }

    fn parse_string(&mut self) -> GapResult<String> {
        if self.next_byte()? != b'"' {
            return Err(self.err("expected opening `\"`"));
        }
        let mut out = String::new();
        loop {
            let start = self.pos;
            while let Some(&b) = self.bytes.get(self.pos) {
                if b == b'"' || b == b'\\' || b < 0x20 {
                    break;
                }
                self.pos += 1;
            }
            if self.pos > start {
                let segment = std::str::from_utf8(&self.bytes[start..self.pos])
                    .map_err(|e| self.err(format!("invalid UTF-8 in JSON string: {e}")))?;
                out.push_str(segment);
            }
            let b = self.next_byte()?;
            match b {
                b'"' => return Ok(out),
                b'\\' => {
                    let esc = self.next_byte()?;
                    match esc {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'b' => out.push('\u{0008}'),
                        b'f' => out.push('\u{000C}'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => {
                            let code1 = self.parse_hex4()?;
                            if (0xD800..=0xDBFF).contains(&code1) {
                                // UTF-16 high surrogate: expect `\uDC00..\uDFFF`.
                                if self.next_byte()? != b'\\' || self.next_byte()? != b'u' {
                                    return Err(self.err("unpaired UTF-16 high surrogate in JSON"));
                                }
                                let code2 = self.parse_hex4()?;
                                if !(0xDC00..=0xDFFF).contains(&code2) {
                                    return Err(self.err("invalid UTF-16 low surrogate in JSON"));
                                }
                                let cp = 0x10000
                                    + (((u32::from(code1) - 0xD800) << 10)
                                        | (u32::from(code2) - 0xDC00));
                                let ch = char::from_u32(cp)
                                    .ok_or_else(|| self.err("invalid Unicode code point"))?;
                                out.push(ch);
                            } else {
                                let ch = char::from_u32(u32::from(code1))
                                    .ok_or_else(|| self.err("invalid Unicode escape in JSON"))?;
                                out.push(ch);
                            }
                        }
                        _ => return Err(self.err("invalid escape sequence in JSON string")),
                    }
                }
                _ => return Err(self.err("unescaped control char in JSON string")),
            }
        }
    }

    fn parse_hex4(&mut self) -> GapResult<u16> {
        let mut val: u16 = 0;
        for _ in 0..4 {
            let b = self.next_byte()?;
            let d = match b {
                b'0'..=b'9' => u16::from(b - b'0'),
                b'a'..=b'f' => u16::from(b - b'a' + 10),
                b'A'..=b'F' => u16::from(b - b'A' + 10),
                _ => return Err(self.err("invalid hex digit in JSON `\\u` escape")),
            };
            val = (val << 4) | d;
        }
        Ok(val)
    }

    fn parse_number(&mut self) -> GapResult<JsonValue> {
        let start = self.pos;
        if self.peek() == Some(b'-') {
            self.pos += 1;
        }
        let digit_start = self.pos;
        match self.peek() {
            Some(b'0') => self.pos += 1,
            Some(b'1'..=b'9') => {
                while let Some(b) = self.peek() {
                    if b.is_ascii_digit() {
                        self.pos += 1;
                    } else {
                        break;
                    }
                }
            }
            _ => return Err(self.err(format!("invalid number near byte {start}"))),
        }
        if self.pos == digit_start {
            return Err(self.err(format!("missing digits in number near byte {start}")));
        }
        if self.peek() == Some(b'.') {
            self.pos += 1;
            let frac_start = self.pos;
            while let Some(b) = self.peek() {
                if b.is_ascii_digit() {
                    self.pos += 1;
                } else {
                    break;
                }
            }
            if self.pos == frac_start {
                return Err(self.err("empty fractional part in JSON number"));
            }
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            self.pos += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.pos += 1;
            }
            let exp_start = self.pos;
            while let Some(b) = self.peek() {
                if b.is_ascii_digit() {
                    self.pos += 1;
                } else {
                    break;
                }
            }
            if self.pos == exp_start {
                return Err(self.err("empty exponent in JSON number"));
            }
        }
        let raw = std::str::from_utf8(&self.bytes[start..self.pos])
            .map_err(|e| self.err(format!("invalid UTF-8 in number: {e}")))?;
        let parsed: f64 = raw
            .parse()
            .map_err(|e| self.err(format!("invalid JSON number `{raw}`: {e}")))?;
        if !parsed.is_finite() {
            return Err(self.err(format!("non-finite JSON number `{raw}`")));
        }
        Ok(JsonValue::Number(parsed))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn json_parser_handles_nested_objects_escapes_and_numbers() {
        let doc = r#"{"ok": true, "vals": [-1.25e2, 0, 42], "text": "a\"b\u0020\uD83D\uDE80"}"#;
        let parsed = parse_json(doc).expect("parse json");
        assert_eq!(parsed.get("ok"), Some(&JsonValue::Bool(true)));
        let vals = parsed
            .get("vals")
            .and_then(JsonValue::as_array)
            .expect("vals");
        assert_eq!(vals.len(), 3);
        assert!((vals[0].as_f64().unwrap() + 125.0).abs() < 1e-9);
        assert_eq!(vals[2].as_usize(), Some(42));
        assert_eq!(
            parsed.get("text").and_then(JsonValue::as_str),
            Some("a\"b 🚀")
        );
    }
}
