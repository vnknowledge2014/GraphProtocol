//! Log, trace, error, metric, and Enricher-input redaction layer (`SEC-041`, `SEC-042`, `INV-15`, `TM-18`).

use std::collections::BTreeMap;
use std::sync::RwLock;

use super::secret::SecretString;

/// Canonical replacement token emitted whenever secret material is scrubbed (`SEC-041`).
pub const REDACTED_TOKEN: &str = "***";

const SENSITIVE_KEY_NAMES: &[&str] = &[
    "authorization",
    "proxy-authorization",
    "cookie",
    "set-cookie",
    "x-api-key",
    "api_key",
    "apikey",
    "api-key",
    "access_token",
    "refresh_token",
    "id_token",
    "client_secret",
    "private_key",
    "password",
    "passwd",
    "secret",
    "secret_key",
    "token",
    "bearer_token",
];

const PEM_BEGIN_MARKERS: &[&str] = &[
    "-----BEGIN PRIVATE KEY-----",
    "-----BEGIN RSA PRIVATE KEY-----",
    "-----BEGIN EC PRIVATE KEY-----",
    "-----BEGIN OPENSSH PRIVATE KEY-----",
    "-----BEGIN ED25519 PRIVATE KEY-----",
    "-----BEGIN ENCRYPTED PRIVATE KEY-----",
];

const TOKEN_PREFIXES: &[(&str, usize)] = &[
    ("AKIA", 20),
    ("ASIA", 20),
    ("ghp_", 16),
    ("github_pat_", 20),
    ("gho_", 16),
    ("ghu_", 16),
    ("sk-", 16),
    ("sk_", 16),
    ("xoxb-", 16),
    ("xoxa-", 16),
    ("xoxp-", 16),
    ("xoxr-", 16),
    ("xoxs-", 16),
    ("AIza", 24),
];

/// Stateful + pattern-based secret redactor protecting logs, traces, errors, metrics, and
/// model inputs (`SEC-041`, `SEC-042`, `INV-15`).
#[derive(Default)]
pub struct Redactor {
    registered_secrets: RwLock<Vec<SecretString>>,
}

impl std::fmt::Debug for Redactor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let count = self.registered_secrets.read().map_or(0, |g| g.len());
        f.debug_struct("Redactor")
            .field("registered_secret_count", &count)
            .finish()
    }
}

impl Redactor {
    /// Creates an empty [`Redactor`] with built-in structural secret detectors enabled.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers an exact secret string (such as a loaded vault credential or canary) so every
    /// occurrence is masked even if it does not match a standard prefix (`SEC-041`, `INV-15`).
    pub fn register_secret(&self, secret: &SecretString) {
        let raw = secret.expose_secret();
        if raw.is_empty() {
            return;
        }
        if let Ok(mut guard) = self.registered_secrets.write()
            && !guard.iter().any(|existing| existing.ct_equals(raw))
        {
            guard.push(secret.duplicate());
            guard.sort_by_key(|a| std::cmp::Reverse(a.expose_secret().len()));
        }
    }

    /// Scrubs registered secrets and pattern-matched credentials from free text (`SEC-041`).
    #[must_use]
    pub fn redact_text(&self, input: &str) -> String {
        let mut s = input.to_owned();
        if let Ok(guard) = self.registered_secrets.read() {
            for sec in guard.iter() {
                let needle = sec.expose_secret();
                if !needle.is_empty() && s.contains(needle) {
                    s = s.replace(needle, REDACTED_TOKEN);
                }
            }
        }
        s = scrub_pem_blocks(&s);
        s = scrub_bearer_tokens(&s);
        s = scrub_url_userinfo(&s);
        s = scrub_jwts(&s);
        s = scrub_known_token_prefixes(&s);
        scrub_kv_assignments(&s)
    }

    /// Scrubs a single log line prior to emission (`SEC-041`, `INV-15`).
    #[must_use]
    pub fn redact_log(&self, line: &str) -> String {
        self.redact_text(line)
    }

    /// Scrubs an error message prior to wrapping in `GapError` or RPC responses (`SEC-041`, `INV-15`).
    #[must_use]
    pub fn redact_error_message(&self, msg: &str) -> String {
        self.redact_text(msg)
    }

    /// Scrubs OpenTelemetry span attributes: masks sensitive keys unconditionally and scrubs
    /// all attribute values through [`Self::redact_text`] (`SEC-041`, `INV-15`).
    #[must_use]
    pub fn redact_trace_attributes(
        &self,
        attrs: &BTreeMap<String, String>,
    ) -> BTreeMap<String, String> {
        self.redact_map(attrs)
    }

    /// Scrubs metric label maps (`SEC-041`, `INV-15`).
    #[must_use]
    pub fn redact_metric_labels(
        &self,
        labels: &BTreeMap<String, String>,
    ) -> BTreeMap<String, String> {
        self.redact_map(labels)
    }

    /// Sanitizes and UTF-8-boundary caps untrusted text before sending to Enricher / model
    /// providers (`SEC-042`, `INV-15`).
    #[must_use]
    pub fn sanitize_enricher_input(&self, raw: &str, max_bytes: usize) -> String {
        let redacted = self.redact_text(raw);
        if redacted.len() <= max_bytes {
            return redacted;
        }
        let mut end = max_bytes;
        while end > 0 && !redacted.is_char_boundary(end) {
            end -= 1;
        }
        redacted[..end].to_owned()
    }

    fn redact_map(&self, input: &BTreeMap<String, String>) -> BTreeMap<String, String> {
        let mut out = BTreeMap::new();
        for (k, v) in input {
            if is_sensitive_key(k) {
                out.insert(k.clone(), REDACTED_TOKEN.to_owned());
            } else {
                out.insert(k.clone(), self.redact_text(v));
            }
        }
        out
    }
}

fn is_sensitive_key(key: &str) -> bool {
    let lower = key.trim().to_ascii_lowercase();
    SENSITIVE_KEY_NAMES
        .iter()
        .any(|&cand| lower == cand || lower.ends_with(cand))
}

fn scrub_pem_blocks(input: &str) -> String {
    let mut out = input.to_owned();
    for begin in PEM_BEGIN_MARKERS {
        while let Some(start) = out.find(begin) {
            let after_begin = start + begin.len();
            let replacement_end = if let Some(end_rel) = out[after_begin..].find("-----END ") {
                let end_marker_start = after_begin + end_rel;
                if let Some(close_rel) = out[end_marker_start + 9..].find("-----") {
                    end_marker_start + 9 + close_rel + 5
                } else {
                    out.len()
                }
            } else {
                out.len()
            };
            let mut next = String::with_capacity(out.len());
            next.push_str(&out[..start]);
            next.push_str(REDACTED_TOKEN);
            next.push_str(&out[replacement_end..]);
            out = next;
        }
    }
    out
}

fn is_token_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'+' | b'/' | b'=')
}

fn scrub_bearer_tokens(input: &str) -> String {
    let bytes = input.as_bytes();
    let lower = input.to_ascii_lowercase();
    let lower_bytes = lower.as_bytes();
    let mut out = String::with_capacity(input.len());
    let mut i = 0;

    while i < bytes.len() {
        if lower_bytes[i..].starts_with(b"bearer ")
            && (i == 0 || !bytes[i - 1].is_ascii_alphanumeric())
        {
            out.push_str(&input[i..i + 7]);
            let mut j = i + 7;
            while j < bytes.len() && bytes[j] == b' ' {
                out.push(' ');
                j += 1;
            }
            let tok_start = j;
            while j < bytes.len() && is_token_char(bytes[j]) {
                j += 1;
            }
            if j > tok_start {
                out.push_str(REDACTED_TOKEN);
                i = j;
                continue;
            }
            i += 7;
            continue;
        }
        if let Some(ch) = input[i..].chars().next() {
            out.push(ch);
            i += ch.len_utf8();
        } else {
            break;
        }
    }
    out
}

fn scrub_url_userinfo(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;

    while let Some(scheme_pos) = rest.find("://") {
        let prefix_end = scheme_pos + 3;
        out.push_str(&rest[..prefix_end]);
        let after_scheme = &rest[prefix_end..];
        let auth_end = after_scheme
            .find(['/', '?', '#', ' ', '\t', '\r', '\n', '"', '\''])
            .unwrap_or(after_scheme.len());
        let authority = &after_scheme[..auth_end];
        if let Some(at_pos) = authority.rfind('@') {
            out.push_str(REDACTED_TOKEN);
            out.push_str(&authority[at_pos..]);
            rest = &after_scheme[auth_end..];
        } else {
            out.push_str(authority);
            rest = &after_scheme[auth_end..];
        }
    }
    out.push_str(rest);
    out
}

fn is_b64url_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'-' || b == b'_'
}

fn scrub_jwts(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = String::with_capacity(input.len());
    let mut i = 0;

    while i < bytes.len() {
        if bytes[i..].starts_with(b"eyJ") && (i == 0 || !is_b64url_char(bytes[i - 1])) {
            let mut j = i;
            while j < bytes.len() && (is_b64url_char(bytes[j]) || bytes[j] == b'.') {
                j += 1;
            }
            let cand = &input[i..j];
            let mut parts = cand.split('.');
            if let (Some(h), Some(p), Some(sig), None) =
                (parts.next(), parts.next(), parts.next(), parts.next())
                && h.len() >= 4
                && p.starts_with("eyJ")
                && p.len() >= 4
                && !sig.is_empty()
            {
                out.push_str(REDACTED_TOKEN);
                i = j;
                continue;
            }
        }
        if let Some(ch) = input[i..].chars().next() {
            out.push(ch);
            i += ch.len_utf8();
        } else {
            break;
        }
    }
    out
}

fn scrub_known_token_prefixes(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = String::with_capacity(input.len());
    let mut i = 0;

    while i < bytes.len() {
        let mut matched_len = None;
        for &(prefix, min_len) in TOKEN_PREFIXES {
            let pbytes = prefix.as_bytes();
            if bytes[i..].starts_with(pbytes)
                && (i == 0 || (!bytes[i - 1].is_ascii_alphanumeric() && bytes[i - 1] != b'_'))
            {
                let mut j = i + pbytes.len();
                while j < bytes.len() && is_token_char(bytes[j]) && bytes[j] != b'.' {
                    j += 1;
                }
                if j - i >= min_len {
                    matched_len = Some(j - i);
                    break;
                }
            }
        }
        if let Some(len) = matched_len {
            out.push_str(REDACTED_TOKEN);
            i += len;
            continue;
        }
        if let Some(ch) = input[i..].chars().next() {
            out.push(ch);
            i += ch.len_utf8();
        } else {
            break;
        }
    }
    out
}

fn scrub_kv_assignments(input: &str) -> String {
    let lower = input.to_ascii_lowercase();
    let bytes = input.as_bytes();
    let lower_bytes = lower.as_bytes();
    let mut out = String::with_capacity(input.len());
    let mut i = 0;

    while i < bytes.len() {
        let mut found_key_end = None;
        for &key in SENSITIVE_KEY_NAMES {
            let kb = key.as_bytes();
            if lower_bytes[i..].starts_with(kb)
                && (i == 0 || (!bytes[i - 1].is_ascii_alphanumeric() && bytes[i - 1] != b'_'))
            {
                let after = i + kb.len();
                let sep_start = if bytes.get(after) == Some(&b'"') {
                    after + 1
                } else {
                    after
                };
                if matches!(bytes.get(sep_start), Some(b'=' | b':')) {
                    found_key_end = Some(sep_start);
                    break;
                }
            }
        }

        if let Some(sep_idx) = found_key_end {
            out.push_str(&input[i..=sep_idx]);
            let mut j = sep_idx + 1;
            while j < bytes.len() && matches!(bytes[j], b' ' | b'\t') {
                out.push(bytes[j] as char);
                j += 1;
            }
            if j < bytes.len() && bytes[j] == b'"' {
                out.push('"');
                j += 1;
                while j < bytes.len() && bytes[j] != b'"' {
                    j += 1;
                }
                out.push_str(REDACTED_TOKEN);
                if j < bytes.len() && bytes[j] == b'"' {
                    out.push('"');
                    j += 1;
                }
                i = j;
                continue;
            }
            while j < bytes.len()
                && !matches!(
                    bytes[j],
                    b' ' | b'\t' | b'\r' | b'\n' | b',' | b';' | b'&' | b')' | b']' | b'}'
                )
            {
                j += 1;
            }
            if j > sep_idx + 1 {
                out.push_str(REDACTED_TOKEN);
                i = j;
                continue;
            }
        }

        if let Some(ch) = input[i..].chars().next() {
            out.push(ch);
            i += ch.len_utf8();
        } else {
            break;
        }
    }
    out
}
