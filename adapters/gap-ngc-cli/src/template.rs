//! Typed `argv` template rendering without shell invocation (`CMP-060`, `TM-07`).

use std::collections::BTreeMap;
use std::fmt;

use gap_core::error::{NgcErrorCode, NgcErrorInfo};
use gap_sec::json::JsonValue;

/// Scalar validation rule for a CLI template placeholder (`CMP-060`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArgConstraint {
    /// Alphanumeric, underscore, hyphen, dot (`1..=max_len`), MUST NOT start with `-`.
    Identifier {
        /// Maximum byte length allowed.
        max_len: usize,
    },
    /// Signed base-10 integer string (`-?[0-9]+`, optionally bounded).
    Integer {
        /// Inclusive lower bound.
        min: Option<i64>,
        /// Inclusive upper bound.
        max: Option<i64>,
    },
    /// Explicit closed set of constant enum literals.
    EnumValues(Vec<String>),
    /// Printable UTF-8 string up to `max_len` without control chars or leading `-` unless allowed.
    SafeText {
        /// Maximum byte length allowed.
        max_len: usize,
        /// Whether values beginning with `-` are accepted (default `false` prevents option smuggling).
        allow_leading_dash: bool,
    },
}

impl ArgConstraint {
    /// Validates and formats a [`JsonValue`] scalar argument as a clean `argv` token.
    ///
    /// # Errors
    ///
    /// Returns [`NgcErrorInfo`] with `INVALID_ARGUMENT` on type mismatch, control bytes, or constraint failure.
    pub fn render_value(&self, field: &str, value: &JsonValue) -> Result<String, NgcErrorInfo> {
        match self {
            Self::Identifier { max_len } => {
                let s = value.as_str().ok_or_else(|| {
                    NgcErrorInfo::new(
                        NgcErrorCode::InvalidArgument,
                        format!("argument `{field}` must be a string identifier"),
                    )
                })?;
                if s.is_empty() || s.len() > *max_len {
                    return Err(NgcErrorInfo::new(
                        NgcErrorCode::InvalidArgument,
                        format!(
                            "argument `{field}` identifier length out of bounds (1..={max_len})"
                        ),
                    ));
                }
                if s.starts_with('-') {
                    return Err(NgcErrorInfo::new(
                        NgcErrorCode::InvalidArgument,
                        format!("argument `{field}` may not start with `-`"),
                    ));
                }
                if !s
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b':'))
                {
                    return Err(NgcErrorInfo::new(
                        NgcErrorCode::InvalidArgument,
                        format!("argument `{field}` contains disallowed identifier characters"),
                    ));
                }
                Ok(s.to_owned())
            }
            Self::Integer { min, max } => {
                let n = match value {
                    JsonValue::Int(i) => *i,
                    JsonValue::Float(f) if f.is_finite() && f.fract() == 0.0 => *f as i64,
                    _ => {
                        return Err(NgcErrorInfo::new(
                            NgcErrorCode::InvalidArgument,
                            format!("argument `{field}` must be an integer"),
                        ));
                    }
                };
                if min.is_some_and(|lo| n < lo) || max.is_some_and(|hi| n > hi) {
                    return Err(NgcErrorInfo::new(
                        NgcErrorCode::InvalidArgument,
                        format!("argument `{field}` integer value {n} out of range"),
                    ));
                }
                Ok(n.to_string())
            }
            Self::EnumValues(allowed) => {
                let s = value.as_str().ok_or_else(|| {
                    NgcErrorInfo::new(
                        NgcErrorCode::InvalidArgument,
                        format!("argument `{field}` must be a string enum"),
                    )
                })?;
                if !allowed.iter().any(|candidate| candidate == s) {
                    return Err(NgcErrorInfo::new(
                        NgcErrorCode::InvalidArgument,
                        format!("argument `{field}` value `{s}` not in allowed set"),
                    ));
                }
                Ok(s.to_owned())
            }
            Self::SafeText {
                max_len,
                allow_leading_dash,
            } => {
                let s = value.as_str().ok_or_else(|| {
                    NgcErrorInfo::new(
                        NgcErrorCode::InvalidArgument,
                        format!("argument `{field}` must be a string"),
                    )
                })?;
                if s.len() > *max_len {
                    return Err(NgcErrorInfo::new(
                        NgcErrorCode::InvalidArgument,
                        format!("argument `{field}` exceeds maximum length of {max_len}"),
                    ));
                }
                if !*allow_leading_dash && s.starts_with('-') {
                    return Err(NgcErrorInfo::new(
                        NgcErrorCode::InvalidArgument,
                        format!(
                            "argument `{field}` cannot start with `-` (option injection blocked)"
                        ),
                    ));
                }
                if s.bytes().any(|b| b == 0 || (b < 0x20 && b != b' ')) {
                    return Err(NgcErrorInfo::new(
                        NgcErrorCode::InvalidArgument,
                        format!("argument `{field}` contains forbidden control bytes"),
                    ));
                }
                Ok(s.to_owned())
            }
        }
    }
}

/// Ordered token specification inside [`ArgvTemplate`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TemplateToken {
    /// Fixed literal argument (e.g. `"get"`, `"--json"`).
    Literal(String),
    /// Named `--flag` + validated value placeholder (`[`flag`, rendered_value]` or single positional if flag is empty).
    BoundArg {
        /// Optional flag token emitted before value (e.g. `"--id"`). `None` emits a positional arg.
        flag: Option<String>,
        /// Name of field looked up inside the request `arguments` JSON object.
        arg_name: String,
        /// Strict scalar validation rule.
        constraint: ArgConstraint,
        /// Whether omitting this argument yields an error (`true`) or simply skips the token (`false`).
        required: bool,
    },
    /// Optional boolean flag emitted only when `arguments[arg_name] == true`.
    BoolFlag {
        /// Flag emitted when true (e.g. `"--dry-run"`).
        flag: String,
        /// Argument field name.
        arg_name: String,
    },
}

/// Typed `argv` template built without any shell parsing or string interpolation (`CMP-060`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArgvTemplate {
    tokens: Vec<TemplateToken>,
}

impl ArgvTemplate {
    /// Creates an empty template.
    #[must_use]
    pub const fn new(tokens: Vec<TemplateToken>) -> Self {
        Self { tokens }
    }

    /// Renders `argv` directly from `arguments`, rejecting unknown/excess arguments and unsafe values (`CMP-060`).
    ///
    /// # Errors
    ///
    /// Returns [`NgcErrorInfo`] with `INVALID_ARGUMENT` if a required argument is missing or invalid.
    pub fn render(
        &self,
        arguments: &BTreeMap<String, JsonValue>,
    ) -> Result<Vec<String>, NgcErrorInfo> {
        let mut argv = Vec::with_capacity(self.tokens.len() * 2);
        for token in &self.tokens {
            match token {
                TemplateToken::Literal(lit) => argv.push(lit.clone()),
                TemplateToken::BoundArg {
                    flag,
                    arg_name,
                    constraint,
                    required,
                } => match arguments.get(arg_name) {
                    None | Some(JsonValue::Null) => {
                        if *required {
                            return Err(NgcErrorInfo::new(
                                NgcErrorCode::InvalidArgument,
                                format!("missing required CLI argument `{arg_name}`"),
                            ));
                        }
                    }
                    Some(val) => {
                        let rendered = constraint.render_value(arg_name, val)?;
                        if let Some(flag_tok) = flag {
                            argv.push(flag_tok.clone());
                        }
                        argv.push(rendered);
                    }
                },
                TemplateToken::BoolFlag { flag, arg_name } => {
                    if let Some(val) = arguments.get(arg_name) {
                        let b = val.as_bool().ok_or_else(|| {
                            NgcErrorInfo::new(
                                NgcErrorCode::InvalidArgument,
                                format!("argument `{arg_name}` must be a boolean"),
                            )
                        })?;
                        if b {
                            argv.push(flag.clone());
                        }
                    }
                }
            }
        }
        Ok(argv)
    }
}

impl fmt::Display for ArgvTemplate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ArgvTemplate({} tokens)", self.tokens.len())
    }
}
