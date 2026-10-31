//! NGC -> GAP error translation (`NGC-060`, `NGC-063`) and untrusted connector message
//! sanitisation (`NGC-061`, `GQL-072`, `INV-15`).

use gap_core::error::{
    ErrorCode, GapError, MAX_ERROR_MESSAGE_CHARS, NgcErrorCode, NgcErrorInfo, PathSegment,
    RetryInfo, clamp_error_message,
};
use gap_core::secret::REDACTED_MARKER;
use serde::{Deserialize, Serialize};

/// Structured `extensions` object on a [`GraphqlError`] (`GQL-051`, `spec/02-gap-ql.md` §10).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GraphqlErrorExtensions {
    /// Stable GAP error code (`GAP_*`).
    pub code: ErrorCode,
    /// Optional structured retry hint (`retryable`, `afterMs`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry: Option<RetryInfo>,
}

/// GraphQL execution error entry serialized inside `ExecResponse.errors` (`GQL-051`, `Brief §3.2`, `§3.4`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GraphqlError {
    /// Sanitised, clamped (`<= 300` Unicode scalar values) diagnostic message (`GQL-072`, `INV-15`).
    pub message: String,
    /// GraphQL response path where the failure occurred (`["orders", 0, "sku"]`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<Vec<PathSegment>>,
    /// GAP error code and retry metadata.
    pub extensions: GraphqlErrorExtensions,
}

impl GraphqlError {
    /// Constructs a [`GraphqlError`] with clamped message and default retry metadata for `code`.
    #[must_use]
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        let _ = MAX_ERROR_MESSAGE_CHARS;
        Self {
            message: clamp_error_message(message),
            path: None,
            extensions: GraphqlErrorExtensions {
                code,
                retry: Some(RetryInfo {
                    retryable: code.default_retryable(),
                    after_ms: None,
                }),
            },
        }
    }

    /// Attaches a GraphQL response `path`.
    #[must_use]
    pub fn with_path(mut self, path: Vec<PathSegment>) -> Self {
        self.path = Some(path);
        self
    }

    /// Overrides `extensions.retry`.
    #[must_use]
    pub const fn with_retry(mut self, retry: RetryInfo) -> Self {
        self.extensions.retry = Some(retry);
        self
    }

    /// Returns the GAP [`ErrorCode`].
    #[must_use]
    pub const fn code(&self) -> ErrorCode {
        self.extensions.code
    }
}

impl From<GapError> for GraphqlError {
    fn from(err: GapError) -> Self {
        Self {
            message: err.message().to_owned(),
            path: err.path,
            extensions: GraphqlErrorExtensions {
                code: err.extensions.code,
                retry: Some(err.extensions.retry),
            },
        }
    }
}

impl From<GraphqlError> for GapError {
    fn from(err: GraphqlError) -> Self {
        let mut out = Self::new(err.extensions.code, err.message);
        if let Some(retry) = err.extensions.retry {
            out = out.with_retry(retry.retryable, retry.after_ms);
        }
        if let Some(path) = err.path {
            out = out.with_path(path);
        }
        out
    }
}

fn strip_ansi_sequences(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            if chars.peek() == Some(&'[') {
                let _ = chars.next();
                for esc_c in chars.by_ref() {
                    if ('@'..='~').contains(&esc_c) {
                        break;
                    }
                }
            } else {
                let _ = chars.next();
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn is_stack_trace_line(trimmed: &str) -> bool {
    let lower = trimmed.to_ascii_lowercase();
    if lower.starts_with("stack backtrace:")
        || lower.starts_with("backtrace:")
        || lower.starts_with("traceback (most recent call last):")
        || lower.starts_with("panicked at ")
        || lower.starts_with("at /")
        || lower.starts_with("at ")
    {
        return true;
    }
    // Rust backtrace numbered frames such as `0: crate::module::fn`
    if let Some((num_part, _rest)) = trimmed.split_once(':')
        && !num_part.is_empty()
        && num_part.chars().all(|c| c.is_ascii_digit())
    {
        return true;
    }
    false
}

fn strip_stack_traces_and_controls(input: &str) -> String {
    let no_ansi = strip_ansi_sequences(input);
    let mut kept_lines = Vec::new();
    for line in no_ansi.lines() {
        let cleaned_line: String = line
            .chars()
            .map(|c| if c.is_control() { ' ' } else { c })
            .collect();
        let trimmed = cleaned_line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if is_stack_trace_line(trimmed) {
            break;
        }
        kept_lines.push(trimmed.to_owned());
    }

    let joined = kept_lines.join(" ");
    let mut collapsed = String::with_capacity(joined.len());
    let mut prev_space = false;
    for c in joined.chars() {
        if c.is_whitespace() || c.is_control() {
            if !prev_space && !collapsed.is_empty() {
                collapsed.push(' ');
                prev_space = true;
            }
        } else {
            collapsed.push(c);
            prev_space = false;
        }
    }
    collapsed.trim().to_owned()
}

fn redact_uri_userinfo(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(scheme_pos) = rest.find("://") {
        let prefix_end = scheme_pos + 3;
        out.push_str(&rest[..prefix_end]);
        let after_scheme = &rest[prefix_end..];
        let authority_end = after_scheme
            .find(|c: char| ['/', '?', '#', ' ', ';', ',', ')', ']', '"', '\''].contains(&c))
            .unwrap_or(after_scheme.len());
        let authority = &after_scheme[..authority_end];
        if let Some(at_pos) = authority.rfind('@') {
            out.push_str(REDACTED_MARKER);
            out.push('@');
            out.push_str(&authority[at_pos + 1..]);
        } else {
            out.push_str(authority);
        }
        rest = &after_scheme[authority_end..];
    }
    out.push_str(rest);
    out
}

fn is_sensitive_key_name(key: &str) -> bool {
    let lower = key
        .trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '_' && c != '-')
        .to_ascii_lowercase();
    if lower.is_empty() {
        return false;
    }
    [
        "secret",
        "token",
        "password",
        "passwd",
        "pwd",
        "apikey",
        "api_key",
        "api-key",
        "access_key",
        "private_key",
        "client_secret",
        "authorization",
        "cookie",
        "credential",
        "bearer",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

fn has_well_known_secret_prefix(token: &str) -> bool {
    let clean = token.trim_matches(|c: char| [',', ';', ')', ']', '"', '\''].contains(&c));
    if clean.starts_with("sk-") && clean.len() >= 12 {
        return true;
    }
    for prefix in [
        "ghp_",
        "gho_",
        "ghs_",
        "ghu_",
        "github_pat_",
        "glpat-",
        "xoxb-",
        "xoxp-",
        "xoxa-",
        "xoxr-",
        "xoxs-",
        "AKIA",
        "ASIA",
    ] {
        if clean.starts_with(prefix) && clean.len() >= prefix.len() + 8 {
            return true;
        }
    }
    // Compact JWT (`eyJ... . eyJ... . sig`)
    if clean.starts_with("eyJ") && clean.matches('.').count() >= 2 && clean.len() >= 20 {
        return true;
    }
    false
}

fn split_trailing_punct(word: &str) -> (&str, &str) {
    let trimmed = word.trim_end_matches([',', ';', ')', ']']);
    (&word[..trimmed.len()], &word[trimmed.len()..])
}

fn scrub_credential_tokens(input: &str) -> String {
    if let Some(pem_idx) = input.find("-----BEGIN ") {
        let head = input[..pem_idx].trim_end();
        return if head.is_empty() {
            REDACTED_MARKER.to_owned()
        } else {
            format!("{head} {REDACTED_MARKER}")
        };
    }

    let words: Vec<&str> = input.split(' ').filter(|w| !w.is_empty()).collect();
    let mut out: Vec<String> = Vec::with_capacity(words.len());
    let mut redact_next = false;

    for word in words {
        let (core, trail) = split_trailing_punct(word);
        let lower_core = core.to_ascii_lowercase();
        let is_auth_scheme = matches!(
            lower_core.as_str(),
            "bearer" | "basic" | "apikey" | "digest" | "token"
        );

        if redact_next {
            if is_auth_scheme && trail.is_empty() {
                out.push(word.to_owned());
                // Keep `redact_next = true` so `Authorization: Bearer <token>` redacts `<token>`!
                continue;
            }
            out.push(format!("{REDACTED_MARKER}{trail}"));
            redact_next = false;
            continue;
        }

        if is_auth_scheme {
            out.push(word.to_owned());
            redact_next = true;
            continue;
        }

        if let Some(key_colon) = core.strip_suffix(':')
            && is_sensitive_key_name(key_colon)
        {
            out.push(word.to_owned());
            redact_next = true;
            continue;
        }

        if let Some((lhs, rhs)) = core.split_once('=') {
            if is_sensitive_key_name(lhs) {
                out.push(format!("{lhs}={REDACTED_MARKER}{trail}"));
                continue;
            }
            if has_well_known_secret_prefix(rhs) {
                out.push(format!("{lhs}={REDACTED_MARKER}{trail}"));
                continue;
            }
        }

        if let Some((lhs, rhs)) = core.split_once(':')
            && !rhs.starts_with("//")
            && is_sensitive_key_name(lhs)
        {
            out.push(format!("{lhs}:{REDACTED_MARKER}{trail}"));
            continue;
        }

        if has_well_known_secret_prefix(core) {
            out.push(format!("{REDACTED_MARKER}{trail}"));
            continue;
        }

        out.push(word.to_owned());
    }

    out.join(" ")
}

/// Sanitises untrusted connector error text (`NGC-061`, `GQL-072`, `INV-15`): strips control
/// characters, ANSI escapes, and stack traces; redacts `extra_secrets` and credential patterns;
/// and clamps to `<= 300` Unicode scalar values.
#[must_use]
pub fn sanitize_connector_message(raw: &str, extra_secrets: &[&str]) -> String {
    let mut text = strip_stack_traces_and_controls(raw);
    for &secret in extra_secrets {
        let trimmed = secret.trim();
        if !trimmed.is_empty() {
            text = text.replace(trimmed, REDACTED_MARKER);
        }
    }
    text = redact_uri_userinfo(&text);
    text = scrub_credential_tokens(&text);
    if text.is_empty() {
        return "branch error".to_owned();
    }
    clamp_error_message(text)
}

/// Maps an [`NgcErrorInfo`] from a connector to a [`GraphqlError`] per `NGC-060` and `NGC-063`,
/// dropping `details` and sanitising `message` (`INV-15`).
#[must_use]
pub fn map_ngc_error(
    ngc: &NgcErrorInfo,
    path: Option<Vec<PathSegment>>,
    extra_secrets: &[&str],
) -> GraphqlError {
    let (code, retryable) = match ngc.code {
        NgcErrorCode::Unspecified
        | NgcErrorCode::NotFound
        | NgcErrorCode::FailedPrecondition
        | NgcErrorCode::Unavailable
        | NgcErrorCode::Internal
        | NgcErrorCode::UpstreamError => (ErrorCode::BranchError, ngc.retryable),
        NgcErrorCode::InvalidArgument => (ErrorCode::ValidationError, ngc.retryable),
        NgcErrorCode::PermissionDenied => (ErrorCode::Forbidden, false),
        NgcErrorCode::Unauthenticated => (ErrorCode::Unauthenticated, ngc.retryable),
        NgcErrorCode::ResourceExhausted => (ErrorCode::RateLimited, true),
        NgcErrorCode::DeadlineExceeded => (ErrorCode::BranchTimeout, ngc.retryable),
        NgcErrorCode::Cancelled => (ErrorCode::Cancelled, ngc.retryable),
        NgcErrorCode::Unimplemented => (ErrorCode::UnsupportedFeature, ngc.retryable),
        NgcErrorCode::Conflict => (ErrorCode::IdempotencyConflict, ngc.retryable),
    };

    let after_ms = if retryable && ngc.retry_after_ms > 0 {
        Some(ngc.retry_after_ms)
    } else {
        None
    };

    let sanitised = sanitize_connector_message(ngc.message(), extra_secrets);
    let mut err = GraphqlError::new(code, sanitised).with_retry(RetryInfo {
        retryable,
        after_ms,
    });
    if let Some(p) = path {
        err = err.with_path(p);
    }
    err
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    // spec: INV-15, NGC-061, NGC-063
    #[test]
    fn sanitiser_strips_controls_backtraces_and_credentials_while_keeping_clean_text() {
        assert_eq!(
            sanitize_connector_message("row lookup failed on index 2", &[]),
            "row lookup failed on index 2"
        );

        let jwt = "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.SflKxwRJSMeKKF2QT4fwpMeJf36POk6yJV_adQssw5c";
        let leaked = sanitize_connector_message(
            &format!("token={jwt} ghp_1234567890abcdef AKIA1234567890ABCDEF"),
            &[],
        );
        assert!(!leaked.contains("eyJ"));
        assert!(!leaked.contains("ghp_"));
        assert!(!leaked.contains("AKIA"));
    }
}
