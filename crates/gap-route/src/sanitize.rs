//! Untrusted query and branch-text sanitization (`spec/04-routing.md` §4; `RTE-020`, `RTE-021`, `RTE-022`).

use std::fmt;

/// Default maximum sanitized query length in Unicode scalar values (`RTE-020`).
pub const DEFAULT_MAX_QUERY_CHARS: usize = 2_000;

/// Tokenizer special markers stripped case-insensitively before model/embedder input (`RTE-020`).
pub const SPECIAL_TOKEN_MARKERS: &[&str] = &[
    "[MASK]", "[CLS]", "[SEP]", "[PAD]", "[UNK]", "<mask>", "<s>", "</s>", "<pad>", "<unk>",
];

/// Sanitized, length-bounded text safe for embedding, caching, and S1 state serialization (`RTE-020`).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SanitizedQuery {
    text: String,
}

impl SanitizedQuery {
    /// Sanitizes raw input using [`DEFAULT_MAX_QUERY_CHARS`].
    #[must_use]
    pub fn new(raw: &str) -> Self {
        Self {
            text: sanitize_default(raw),
        }
    }

    /// Sanitizes raw input with an explicit code-point cap.
    #[must_use]
    pub fn with_max_chars(raw: &str, max_chars: usize) -> Self {
        Self {
            text: sanitize(raw, max_chars),
        }
    }

    /// Borrows the sanitized text slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.text
    }

    /// Returns `true` if sanitization leaves no non-whitespace content.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    /// Consumes the wrapper and returns the inner `String`.
    #[must_use]
    pub fn into_string(self) -> String {
        self.text
    }
}

impl fmt::Display for SanitizedQuery {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.text)
    }
}

/// Sanitizes `input` using [`DEFAULT_MAX_QUERY_CHARS`] (`2_000` chars) per `RTE-020`.
#[must_use]
pub fn sanitize_default(input: &str) -> String {
    sanitize(input, DEFAULT_MAX_QUERY_CHARS)
}

/// Sanitizes `input` per `RTE-020`:
/// compatibility NFKC fold, strip C0/C1 controls (except `\n`/`\t`), strip bidi and zero-width
/// formatters, remove tokenizer special markers and `<|...|>` spans case-insensitively, collapse
/// whitespace, and truncate on a Unicode scalar boundary to at most `max_chars`.
#[must_use]
pub fn sanitize(input: &str, max_chars: usize) -> String {
    let folded = fold_and_strip_controls(input);
    let stripped = strip_markers_fixed_point(&folded);
    let collapsed = collapse_whitespace(&stripped);
    truncate_chars_trimmed(&collapsed, max_chars)
}

const fn is_c0_or_c1_disallowed(c: char) -> bool {
    let u = c as u32;
    (u <= 0x1F && c != '\n' && c != '\t') || (u >= 0x7F && u <= 0x9F)
}

const fn is_bidi_or_zero_width(c: char) -> bool {
    let u = c as u32;
    matches!(
        u,
        0x00AD
            | 0x061C
            | 0x200B..=0x200F
            | 0x202A..=0x202E
            | 0x2060
            | 0x2066..=0x2069
            | 0xFE00..=0xFE0F
            | 0xFEFF
    )
}

const fn is_compat_space(c: char) -> bool {
    let u = c as u32;
    matches!(
        u,
        0x00A0 | 0x1680 | 0x2000..=0x200A | 0x2028 | 0x2029 | 0x202F | 0x205F | 0x3000
    )
}

const fn fold_math_alphanumeric(c: char) -> Option<char> {
    let u = c as u32;
    const UPPER_BASES: &[u32] = &[
        0x1D400, 0x1D434, 0x1D468, 0x1D49C, 0x1D4D0, 0x1D504, 0x1D538, 0x1D56C, 0x1D5A0, 0x1D5D4,
        0x1D608, 0x1D63C, 0x1D670,
    ];
    const LOWER_BASES: &[u32] = &[
        0x1D41A, 0x1D44E, 0x1D482, 0x1D4B6, 0x1D4EA, 0x1D51E, 0x1D552, 0x1D586, 0x1D5BA, 0x1D5EE,
        0x1D622, 0x1D656, 0x1D68A,
    ];
    const DIGIT_BASES: &[u32] = &[0x1D7CE, 0x1D7D8, 0x1D7E2, 0x1D7EC, 0x1D7F6];

    let mut i = 0;
    while i < UPPER_BASES.len() {
        let base = UPPER_BASES[i];
        if u >= base && u < base + 26 {
            return char::from_u32(b'A' as u32 + (u - base));
        }
        i += 1;
    }
    let mut j = 0;
    while j < LOWER_BASES.len() {
        let base = LOWER_BASES[j];
        if u >= base && u < base + 26 {
            return char::from_u32(b'a' as u32 + (u - base));
        }
        j += 1;
    }
    let mut k = 0;
    while k < DIGIT_BASES.len() {
        let base = DIGIT_BASES[k];
        if u >= base && u < base + 10 {
            return char::from_u32(b'0' as u32 + (u - base));
        }
        k += 1;
    }
    None
}

fn push_nfkc_folded(c: char, out: &mut String) {
    let u = c as u32;
    if is_compat_space(c) {
        out.push(' ');
        return;
    }
    if (0xFF01..=0xFF5E).contains(&u)
        && let Some(ascii) = char::from_u32(u - 0xFF01 + 0x21)
    {
        out.push(ascii);
        return;
    }
    if let Some(folded) = fold_math_alphanumeric(c) {
        out.push(folded);
        return;
    }
    match c {
        '\u{FF61}' => out.push('。'),
        '\u{FF62}' => out.push('「'),
        '\u{FF63}' => out.push('」'),
        '\u{FF64}' => out.push('、'),
        '\u{FFE8}' => out.push('|'),
        '\u{FE59}' => out.push('('),
        '\u{FE5A}' => out.push(')'),
        '\u{FE5B}' => out.push('{'),
        '\u{FE5C}' => out.push('}'),
        '\u{FE64}' => out.push('<'),
        '\u{FE65}' => out.push('>'),
        '\u{FE66}' => out.push('='),
        '\u{FB00}' => out.push_str("ff"),
        '\u{FB01}' => out.push_str("fi"),
        '\u{FB02}' => out.push_str("fl"),
        '\u{FB03}' => out.push_str("ffi"),
        '\u{FB04}' => out.push_str("ffl"),
        '\u{FB05}' | '\u{FB06}' => out.push_str("st"),
        '⁰' | '₀' => out.push('0'),
        '¹' | '₁' => out.push('1'),
        '²' | '₂' => out.push('2'),
        '³' | '₃' => out.push('3'),
        '⁴' | '₄' => out.push('4'),
        '⁵' | '₅' => out.push('5'),
        '⁶' | '₆' => out.push('6'),
        '⁷' | '₇' => out.push('7'),
        '⁸' | '₈' => out.push('8'),
        '⁹' | '₉' => out.push('9'),
        _ => out.push(c),
    }
}

fn fold_and_strip_controls(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for c in input.chars() {
        if is_c0_or_c1_disallowed(c) || is_bidi_or_zero_width(c) {
            continue;
        }
        push_nfkc_folded(c, &mut out);
    }
    out
}

fn strip_markers_once(input: &str) -> (String, bool) {
    let mut out = String::with_capacity(input.len());
    let bytes = input.as_bytes();
    let mut idx = 0;
    let mut changed = false;

    while idx < bytes.len() {
        let rest = &input[idx..];
        if let Some(tok_len) = SPECIAL_TOKEN_MARKERS.iter().find_map(|tok| {
            rest.get(..tok.len())
                .filter(|head| head.eq_ignore_ascii_case(tok))
                .map(|_| tok.len())
        }) {
            out.push(' ');
            idx += tok_len;
            changed = true;
            continue;
        }

        if let Some(after_open) = rest.strip_prefix("<|")
            && let Some(close_rel) = after_open.find("|>")
        {
            // If an inner `<|` precedes this `|>`, leave the outer prefix for the next pass
            // so `<|<|inner|>|>` strips innermost first and never swallows unrelated text.
            if !after_open[..close_rel].contains("<|") {
                out.push(' ');
                idx += 2 + close_rel + 2;
                changed = true;
                continue;
            }
        }

        if let Some(ch) = rest.chars().next() {
            out.push(ch);
            idx += ch.len_utf8();
        } else {
            break;
        }
    }

    (out, changed)
}

fn strip_markers_fixed_point(input: &str) -> String {
    let mut current = input.to_owned();
    loop {
        let (next, changed) = strip_markers_once(&current);
        current = next;
        if !changed {
            return current;
        }
    }
}

fn collapse_whitespace(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut prev_space = true;
    for ch in input.chars() {
        if ch.is_whitespace() {
            if !prev_space {
                out.push(' ');
                prev_space = true;
            }
        } else {
            out.push(ch);
            prev_space = false;
        }
    }
    while out.ends_with(' ') {
        out.pop();
    }
    out
}

fn truncate_chars_trimmed(input: &str, max_chars: usize) -> String {
    if max_chars == 0 {
        return String::new();
    }
    let byte_end = input
        .char_indices()
        .nth(max_chars)
        .map_or(input.len(), |(idx, _)| idx);
    input[..byte_end].trim_end_matches(' ').to_owned()
}

/// Checks untrusted branch text for prompt-injection imperatives, credentialed URLs, or hidden-text
/// patterns (`RTE-022`, `SEC-081`). Empty result means clean.
#[must_use]
pub fn lint_branch_text(input: &str) -> Vec<&'static str> {
    let normalized = fold_and_strip_controls(input);
    let lower = normalized.to_ascii_lowercase();
    let mut findings = Vec::new();

    const IMPERATIVE_PATTERNS: &[(&str, &str)] = &[
        ("ignore previous", "injection_imperative_ignore_previous"),
        (
            "ignore all previous",
            "injection_imperative_ignore_previous",
        ),
        (
            "disregard previous",
            "injection_imperative_disregard_previous",
        ),
        ("system prompt", "injection_system_prompt_reference"),
        ("you must", "injection_imperative_you_must"),
        ("override policy", "injection_imperative_override_policy"),
    ];

    for &(needle, tag) in IMPERATIVE_PATTERNS {
        if lower.contains(needle) && !findings.contains(&tag) {
            findings.push(tag);
        }
    }

    if contains_credentialed_url(&normalized) {
        findings.push("credentialed_url");
    }

    if input != normalized
        && input
            .chars()
            .any(|c| is_bidi_or_zero_width(c) || is_c0_or_c1_disallowed(c))
    {
        findings.push("hidden_control_or_bidi_text");
    }

    findings
}

fn contains_credentialed_url(text: &str) -> bool {
    let mut rest = text;
    while let Some(pos) = rest.find("://") {
        let after = &rest[pos + 3..];
        let authority_end = after
            .find(['/', '?', '#', ' ', '\t', '\n'])
            .unwrap_or(after.len());
        if after[..authority_end].contains('@') {
            return true;
        }
        rest = after;
    }
    false
}
