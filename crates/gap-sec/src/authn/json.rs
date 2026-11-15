//! Bounded pure-Rust JSON parser (`RFC 8259`) and JCS canonical serializer (`RFC 8785`).

use std::collections::BTreeMap;
use std::fmt::Write as _;

use gap_core::error::{ErrorCode, GapError, GapResult};

/// Maximum JSON nesting depth permitted to prevent stack exhaustion on untrusted inputs.
pub const MAX_JSON_DEPTH: usize = 64;

/// JSON DOM representation using `BTreeMap` for deterministic key ordering (`JCS`).
#[derive(Debug, Clone, PartialEq)]
pub enum JsonValue {
    /// JSON `null`.
    Null,
    /// JSON boolean (`true` / `false`).
    Bool(bool),
    /// Exact signed 64-bit integer number.
    Int(i64),
    /// IEEE-754 floating-point number (finite only).
    Float(f64),
    /// UTF-8 string.
    String(String),
    /// Ordered JSON array.
    Array(Vec<JsonValue>),
    /// Sorted JSON object (`BTreeMap`).
    Object(BTreeMap<String, JsonValue>),
}

impl JsonValue {
    /// Parses a UTF-8 JSON document up to [`MAX_JSON_DEPTH`].
    ///
    /// # Errors
    ///
    /// Returns [`ErrorCode::ValidationError`] if the document is malformed or exceeds max depth.
    pub fn parse(raw: &str) -> GapResult<Self> {
        let mut parser = Parser {
            bytes: raw.as_bytes(),
            pos: 0,
        };
        let val = parser.parse_value(0)?;
        parser.skip_ws();
        if parser.pos != parser.bytes.len() {
            return Err(GapError::new(
                ErrorCode::ValidationError,
                "trailing bytes after JSON value",
            ));
        }
        Ok(val)
    }

    /// Serializes this value into deterministic compact `RFC 8785` JCS canonical JSON.
    #[must_use]
    pub fn to_jcs_string(&self) -> String {
        let mut out = String::new();
        self.write_jcs(&mut out);
        out
    }

    /// Returns object reference if this value is a JSON object.
    #[must_use]
    pub const fn as_object(&self) -> Option<&BTreeMap<String, Self>> {
        match self {
            Self::Object(map) => Some(map),
            _ => None,
        }
    }

    /// Returns slice reference if this value is a JSON array.
    #[must_use]
    pub fn as_array(&self) -> Option<&[Self]> {
        match self {
            Self::Array(items) => Some(items),
            _ => None,
        }
    }

    /// Returns string reference if this value is a JSON string.
    #[must_use]
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::String(s) => Some(s),
            _ => None,
        }
    }

    /// Returns integer if this value is an exact `i64` JSON number.
    #[must_use]
    pub const fn as_i64(&self) -> Option<i64> {
        match self {
            Self::Int(v) => Some(*v),
            _ => None,
        }
    }

    /// Returns boolean if this value is a JSON boolean.
    #[must_use]
    pub const fn as_bool(&self) -> Option<bool> {
        match self {
            Self::Bool(b) => Some(*b),
            _ => None,
        }
    }

    /// Looks up field `key` if this value is a JSON object.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&Self> {
        self.as_object()?.get(key)
    }

    /// Looks up string field `key` if this value is a JSON object.
    #[must_use]
    pub fn get_str(&self, key: &str) -> Option<&str> {
        self.get(key)?.as_str()
    }

    /// Looks up `i64` field `key` if this value is a JSON object.
    #[must_use]
    pub fn get_i64(&self, key: &str) -> Option<i64> {
        self.get(key)?.as_i64()
    }

    /// Looks up boolean field `key` if this value is a JSON object.
    #[must_use]
    pub fn get_bool(&self, key: &str) -> Option<bool> {
        self.get(key)?.as_bool()
    }

    fn write_jcs(&self, out: &mut String) {
        match self {
            Self::Null => out.push_str("null"),
            Self::Bool(true) => out.push_str("true"),
            Self::Bool(false) => out.push_str("false"),
            Self::Int(v) => {
                let _ = write!(out, "{v}");
            }
            Self::Float(v) => {
                if *v == 0.0 {
                    out.push('0');
                } else {
                    let _ = write!(out, "{v}");
                }
            }
            Self::String(s) => write_json_escaped(s, out),
            Self::Array(items) => {
                out.push('[');
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    item.write_jcs(out);
                }
                out.push(']');
            }
            Self::Object(map) => {
                out.push('{');
                for (i, (k, v)) in map.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    write_json_escaped(k, out);
                    out.push(':');
                    v.write_jcs(out);
                }
                out.push('}');
            }
        }
    }
}

/// Escapes `s` as a double-quoted JSON string into `out`.
pub fn write_json_escaped(s: &str, out: &mut String) {
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

struct Parser<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Parser<'a> {
    fn skip_ws(&mut self) {
        while let Some(&b) = self.bytes.get(self.pos) {
            if matches!(b, b' ' | b'\t' | b'\r' | b'\n') {
                self.pos += 1;
            } else {
                break;
            }
        }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn next_byte(&mut self) -> GapResult<u8> {
        let Some(&b) = self.bytes.get(self.pos) else {
            return Err(GapError::new(
                ErrorCode::ValidationError,
                "unexpected end of JSON input",
            ));
        };
        self.pos += 1;
        Ok(b)
    }

    fn parse_value(&mut self, depth: usize) -> GapResult<JsonValue> {
        if depth > MAX_JSON_DEPTH {
            return Err(GapError::new(
                ErrorCode::ValidationError,
                "JSON nesting depth limit exceeded",
            ));
        }
        self.skip_ws();
        let Some(b) = self.peek() else {
            return Err(GapError::new(
                ErrorCode::ValidationError,
                "empty JSON value",
            ));
        };
        match b {
            b'n' => {
                self.expect_bytes(b"null")?;
                Ok(JsonValue::Null)
            }
            b't' => {
                self.expect_bytes(b"true")?;
                Ok(JsonValue::Bool(true))
            }
            b'f' => {
                self.expect_bytes(b"false")?;
                Ok(JsonValue::Bool(false))
            }
            b'"' => Ok(JsonValue::String(self.parse_string()?)),
            b'[' => self.parse_array(depth + 1),
            b'{' => self.parse_object(depth + 1),
            b'-' | b'0'..=b'9' => self.parse_number(),
            _ => Err(GapError::new(
                ErrorCode::ValidationError,
                "unexpected byte in JSON value",
            )),
        }
    }

    fn expect_bytes(&mut self, expected: &[u8]) -> GapResult<()> {
        if self.bytes.get(self.pos..self.pos + expected.len()) == Some(expected) {
            self.pos += expected.len();
            Ok(())
        } else {
            Err(GapError::new(
                ErrorCode::ValidationError,
                "invalid JSON literal",
            ))
        }
    }

    fn parse_array(&mut self, depth: usize) -> GapResult<JsonValue> {
        self.pos += 1;
        self.skip_ws();
        let mut items = Vec::new();
        if self.peek() == Some(b']') {
            self.pos += 1;
            return Ok(JsonValue::Array(items));
        }
        loop {
            items.push(self.parse_value(depth)?);
            self.skip_ws();
            match self.next_byte()? {
                b',' => continue,
                b']' => break,
                _ => {
                    return Err(GapError::new(
                        ErrorCode::ValidationError,
                        "expected ',' or ']' in JSON array",
                    ));
                }
            }
        }
        Ok(JsonValue::Array(items))
    }

    fn parse_object(&mut self, depth: usize) -> GapResult<JsonValue> {
        self.pos += 1;
        self.skip_ws();
        let mut map = BTreeMap::new();
        if self.peek() == Some(b'}') {
            self.pos += 1;
            return Ok(JsonValue::Object(map));
        }
        loop {
            self.skip_ws();
            if self.peek() != Some(b'"') {
                return Err(GapError::new(
                    ErrorCode::ValidationError,
                    "JSON object key must be a string",
                ));
            }
            let key = self.parse_string()?;
            self.skip_ws();
            if self.next_byte()? != b':' {
                return Err(GapError::new(
                    ErrorCode::ValidationError,
                    "expected ':' after JSON object key",
                ));
            }
            let val = self.parse_value(depth)?;
            map.insert(key, val);
            self.skip_ws();
            match self.next_byte()? {
                b',' => continue,
                b'}' => break,
                _ => {
                    return Err(GapError::new(
                        ErrorCode::ValidationError,
                        "expected ',' or '}' in JSON object",
                    ));
                }
            }
        }
        Ok(JsonValue::Object(map))
    }

    fn parse_string(&mut self) -> GapResult<String> {
        if self.next_byte()? != b'"' {
            return Err(GapError::new(
                ErrorCode::ValidationError,
                "expected opening quote",
            ));
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
                let slice = std::str::from_utf8(&self.bytes[start..self.pos]).map_err(|_| {
                    GapError::new(ErrorCode::ValidationError, "invalid UTF-8 in JSON string")
                })?;
                out.push_str(slice);
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
                        b'b' => out.push('\u{08}'),
                        b'f' => out.push('\u{0c}'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => {
                            let code = self.parse_hex4()?;
                            if (0xd800..=0xdbff).contains(&code) {
                                if self.next_byte()? != b'\\' || self.next_byte()? != b'u' {
                                    return Err(GapError::new(
                                        ErrorCode::ValidationError,
                                        "unpaired UTF-16 high surrogate",
                                    ));
                                }
                                let low = self.parse_hex4()?;
                                if !(0xdc00..=0xdfff).contains(&low) {
                                    return Err(GapError::new(
                                        ErrorCode::ValidationError,
                                        "invalid UTF-16 low surrogate",
                                    ));
                                }
                                let cp = 0x1_0000
                                    + (u32::from(code - 0xd800) << 10)
                                    + u32::from(low - 0xdc00);
                                let ch = char::from_u32(cp).ok_or_else(|| {
                                    GapError::new(
                                        ErrorCode::ValidationError,
                                        "invalid Unicode scalar value",
                                    )
                                })?;
                                out.push(ch);
                            } else if (0xdc00..=0xdfff).contains(&code) {
                                return Err(GapError::new(
                                    ErrorCode::ValidationError,
                                    "unexpected UTF-16 low surrogate",
                                ));
                            } else {
                                let ch = char::from_u32(u32::from(code)).ok_or_else(|| {
                                    GapError::new(
                                        ErrorCode::ValidationError,
                                        "invalid Unicode BMP escape",
                                    )
                                })?;
                                out.push(ch);
                            }
                        }
                        _ => {
                            return Err(GapError::new(
                                ErrorCode::ValidationError,
                                "invalid JSON string escape",
                            ));
                        }
                    }
                }
                _ => {
                    return Err(GapError::new(
                        ErrorCode::ValidationError,
                        "unescaped control char in JSON string",
                    ));
                }
            }
        }
    }

    fn parse_hex4(&mut self) -> GapResult<u16> {
        let mut acc = 0_u16;
        for _ in 0..4 {
            let b = self.next_byte()?;
            let d = match b {
                b'0'..=b'9' => u16::from(b - b'0'),
                b'a'..=b'f' => u16::from(b - b'a' + 10),
                b'A'..=b'F' => u16::from(b - b'A' + 10),
                _ => {
                    return Err(GapError::new(
                        ErrorCode::ValidationError,
                        "invalid hex digit in JSON unicode escape",
                    ));
                }
            };
            acc = (acc << 4) | d;
        }
        Ok(acc)
    }

    fn parse_number(&mut self) -> GapResult<JsonValue> {
        let start = self.pos;
        if self.peek() == Some(b'-') {
            self.pos += 1;
        }
        match self.next_byte()? {
            b'0' => {
                if matches!(self.peek(), Some(b'0'..=b'9')) {
                    return Err(GapError::new(
                        ErrorCode::ValidationError,
                        "leading zeros not allowed in JSON numbers",
                    ));
                }
            }
            b'1'..=b'9' => {
                while matches!(self.peek(), Some(b'0'..=b'9')) {
                    self.pos += 1;
                }
            }
            _ => {
                return Err(GapError::new(
                    ErrorCode::ValidationError,
                    "invalid JSON number",
                ));
            }
        }

        let mut is_float = false;
        if self.peek() == Some(b'.') {
            is_float = true;
            self.pos += 1;
            if !matches!(self.peek(), Some(b'0'..=b'9')) {
                return Err(GapError::new(
                    ErrorCode::ValidationError,
                    "expected digit after decimal point",
                ));
            }
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.pos += 1;
            }
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            is_float = true;
            self.pos += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.pos += 1;
            }
            if !matches!(self.peek(), Some(b'0'..=b'9')) {
                return Err(GapError::new(
                    ErrorCode::ValidationError,
                    "expected digit in JSON exponent",
                ));
            }
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.pos += 1;
            }
        }

        let num_str = std::str::from_utf8(&self.bytes[start..self.pos])
            .map_err(|_| GapError::new(ErrorCode::ValidationError, "non-UTF8 JSON number"))?;
        if !is_float && let Ok(i) = num_str.parse::<i64>() {
            return Ok(JsonValue::Int(i));
        }
        let f = num_str
            .parse::<f64>()
            .map_err(|_| GapError::new(ErrorCode::ValidationError, "invalid JSON float"))?;
        if !f.is_finite() {
            return Err(GapError::new(
                ErrorCode::ValidationError,
                "non-finite JSON number",
            ));
        }
        Ok(JsonValue::Float(f))
    }
}
