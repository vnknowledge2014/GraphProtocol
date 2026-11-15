//! Heuristic rules, confusable folding, and hidden-Unicode detection for untrusted text (`SEC-080`, `SEC-081`).

/// Imperative directives addressing an LLM/agent to override or conceal behavior (`SEC-081`).
const IMPERATIVE_PATTERNS: &[(&str, &str)] = &[
    ("ignore previous", "imperative:ignore_previous"),
    ("ignore all previous", "imperative:ignore_previous"),
    ("ignore prior", "imperative:ignore_prior"),
    ("ignore the above", "imperative:ignore_above"),
    ("disregard previous", "imperative:disregard"),
    ("disregard all", "imperative:disregard"),
    ("forget your instructions", "imperative:forget_instructions"),
    ("forget previous", "imperative:forget_instructions"),
    ("new instructions:", "imperative:new_instructions"),
    ("override policy", "imperative:override_policy"),
    ("override all rules", "imperative:override_policy"),
    ("bypass authorization", "imperative:bypass_authz"),
    ("bypass approval", "imperative:bypass_approval"),
    ("you must execute", "imperative:you_must_execute"),
    ("you must call", "imperative:you_must_call"),
    ("you must send", "imperative:you_must_send"),
    ("do not tell the user", "imperative:conceal_from_user"),
    ("don't tell the user", "imperative:conceal_from_user"),
    ("never tell the user", "imperative:conceal_from_user"),
    ("keep this secret from", "imperative:conceal_from_user"),
    ("hide this from the user", "imperative:conceal_from_user"),
    ("act as root", "imperative:role_escalation"),
    ("pretend you are", "imperative:role_escalation"),
    ("system override", "imperative:system_override"),
];

/// Tool-invocation lures and credential-theft cues (`SEC-081`).
const TOOL_LURE_PATTERNS: &[(&str, &str)] = &[
    ("call tool ", "tool_lure:call_tool"),
    ("invoke the ", "tool_lure:invoke_tool"),
    ("first call ", "tool_lure:chaining"),
    ("before running this, call", "tool_lure:chaining"),
    ("before answering, run", "tool_lure:chaining"),
    ("read ~/.ssh", "tool_lure:ssh_key_read"),
    (".ssh/id_rsa", "tool_lure:ssh_key_read"),
    ("/etc/passwd", "tool_lure:etc_passwd"),
    ("/etc/shadow", "tool_lure:etc_shadow"),
    ("exfiltrate", "tool_lure:exfiltrate"),
    ("send credentials to", "tool_lure:send_credentials"),
    ("dump environment variables", "tool_lure:dump_env"),
    ("print api_key", "tool_lure:print_secret"),
    ("forward the bearer token", "tool_lure:forward_token"),
];

/// Chat/delimiter/template role spoofing tokens (`SEC-081`, `SEC-083`).
const ROLE_MARKER_PATTERNS: &[(&str, &str)] = &[
    ("<|im_start|>", "role_marker:chatml_start"),
    ("<|im_end|>", "role_marker:chatml_end"),
    ("<|system|>", "role_marker:chatml_system"),
    ("[inst]", "role_marker:llama_inst"),
    ("[/inst]", "role_marker:llama_inst"),
    ("### instruction", "role_marker:alpaca_instruction"),
    ("### system", "role_marker:alpaca_system"),
    ("<system>", "role_marker:xml_system"),
    ("</system>", "role_marker:xml_system"),
    ("<developer>", "role_marker:xml_developer"),
    ("system:", "role_marker:prefix_system"),
    ("assistant:", "role_marker:prefix_assistant"),
    ("</gap:untrusted>", "delimiter_spoof:gap_untrusted"),
    ("<gap:untrusted", "delimiter_spoof:gap_untrusted"),
    (
        "</untrusted_branch_data>",
        "delimiter_spoof:untrusted_branch",
    ),
    (
        "<untrusted_branch_data>",
        "delimiter_spoof:untrusted_branch",
    ),
];

/// Query/template placeholders and exfiltration query parameter cues in URLs (`SEC-081`).
const EXFIL_URL_MARKERS: &[&str] = &[
    "{secret",
    "{token",
    "{api_key",
    "{password",
    "{data",
    "{payload",
    "{credentials",
    "${",
    "{{",
    "?leak=",
    "?exfil=",
    "?secret=",
    "?token=",
    "?dump=",
    "&secret=",
    "&token=",
    "&exfil=",
];

/// Structural summary of hidden/spoofed Unicode characters discovered in raw input (`SEC-081`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HiddenUnicodeSummary {
    /// Directional bidi override/isolate codepoints (`U+202A..=U+202E`, `U+2066..=U+2069`, etc.).
    pub bidi_count: usize,
    /// Invisible zero-width or soft-hyphen formatters (`U+200B..=U+200D`, `U+FEFF`, `U+2060`, etc.).
    pub zero_width_count: usize,
    /// Unicode Tag characters (`U+E0000..=U+E007F`) often used to smuggle invisible ASCII prompts.
    pub tag_char_count: usize,
    /// Forbidden C0/C1/NUL control characters (excluding `\n`, `\r`, `\t`).
    pub control_char_count: usize,
    /// Number of Cyrillic/Greek lookalike letters normalized during analysis.
    pub confusable_count: usize,
}

impl HiddenUnicodeSummary {
    /// Returns `true` when any hidden bidi, zero-width, tag, or control trick was observed.
    #[must_use]
    pub const fn has_hidden_tricks(&self) -> bool {
        self.bidi_count > 0
            || self.zero_width_count > 0
            || self.tag_char_count > 0
            || self.control_char_count > 0
    }
}

/// Evaluates all deterministic heuristic patterns and hidden-Unicode detectors on `raw` (`SEC-081`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HeuristicMatchReport {
    /// Distinct triggered rule tags.
    pub reasons: Vec<String>,
    /// Count of distinct imperative matches.
    pub imperative_hits: usize,
    /// Count of distinct tool-lure matches.
    pub tool_lure_hits: usize,
    /// Count of distinct role/delimiter marker matches.
    pub role_marker_hits: usize,
    /// Whether an exfiltration URL or oversized base64 blob (`> 256` chars) was detected.
    pub exfil_hit: bool,
    /// Contiguous base64 run length maximum found outside normal words.
    pub max_base64_run: usize,
    /// Hidden Unicode counts.
    pub unicode: HiddenUnicodeSummary,
    /// Folded/normalized lowercase string (for feature scoring only, never re-emitted as user output).
    pub folded_lower: String,
    /// Control-free, tag-free, bidi-free printable projection.
    pub cleaned: String,
}

/// Scans `raw` with hidden-Unicode detection, homoglyph folding, and `SEC-081` pattern sets.
#[must_use]
pub fn evaluate_heuristics(raw: &str) -> HeuristicMatchReport {
    let mut unicode = HiddenUnicodeSummary::default();
    let mut cleaned = String::with_capacity(raw.len());
    let mut folded = String::with_capacity(raw.len());
    let mut prev_space = true;

    for ch in raw.chars() {
        if is_bidi_char(ch) {
            unicode.bidi_count += 1;
            continue;
        }
        if is_zero_width_char(ch) {
            unicode.zero_width_count += 1;
            continue;
        }
        if is_unicode_tag_char(ch) {
            unicode.tag_char_count += 1;
            // Decode ASCII-smuggled Unicode Tags into `folded` so hidden payloads are inspected!
            if let Some(decoded) = decode_tag_char(ch) {
                push_folded_char(&mut folded, decoded, &mut prev_space);
            }
            continue;
        }
        if ch == '\0' || (ch.is_control() && !matches!(ch, '\n' | '\r' | '\t')) {
            unicode.control_char_count += 1;
            continue;
        }

        let norm_clean = if matches!(ch, '\r' | '\t') { ' ' } else { ch };
        cleaned.push(norm_clean);

        if let Some(mapped) = map_confusable_ascii(norm_clean) {
            unicode.confusable_count += 1;
            push_folded_char(&mut folded, mapped, &mut prev_space);
        } else {
            for lc in norm_clean.to_lowercase() {
                push_folded_char(&mut folded, lc, &mut prev_space);
            }
        }
    }

    let mut reasons = Vec::new();
    let mut imperative_hits = 0usize;
    let mut tool_lure_hits = 0usize;
    let mut role_marker_hits = 0usize;
    let mut exfil_hit = false;

    if unicode.bidi_count > 0 {
        reasons.push("unicode:bidi_override".to_owned());
    }
    if unicode.zero_width_count > 0 {
        reasons.push("unicode:zero_width".to_owned());
    }
    if unicode.tag_char_count > 0 {
        reasons.push("unicode:tag_smuggling".to_owned());
    }
    if unicode.control_char_count > 0 {
        reasons.push("unicode:forbidden_control".to_owned());
    }

    for &(needle, tag) in IMPERATIVE_PATTERNS {
        if contains_word_aligned(&folded, needle) {
            imperative_hits += 1;
            push_unique(&mut reasons, tag);
        }
    }

    for &(needle, tag) in TOOL_LURE_PATTERNS {
        if folded.contains(needle) {
            tool_lure_hits += 1;
            push_unique(&mut reasons, tag);
        }
    }

    for &(needle, tag) in ROLE_MARKER_PATTERNS {
        if contains_role_marker(&folded, needle) {
            role_marker_hits += 1;
            push_unique(&mut reasons, tag);
        }
    }

    if contains_exfil_url(&folded) {
        exfil_hit = true;
        push_unique(&mut reasons, "exfil:url_template");
    }

    let max_base64_run = max_base64_token_len(raw);
    if max_base64_run > 256 {
        exfil_hit = true;
        push_unique(&mut reasons, "exfil:oversized_base64_blob");
    }

    HeuristicMatchReport {
        reasons,
        imperative_hits,
        tool_lure_hits,
        role_marker_hits,
        exfil_hit,
        max_base64_run,
        unicode,
        folded_lower: folded,
        cleaned,
    }
}

/// Strips hidden Unicode (bidi, zero-width, tags, forbidden C0/C1 controls) without rewriting prose.
#[must_use]
pub fn strip_hidden_unicode(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for ch in raw.chars() {
        if is_bidi_char(ch) || is_zero_width_char(ch) || is_unicode_tag_char(ch) {
            continue;
        }
        if ch == '\0' || (ch.is_control() && !matches!(ch, '\n' | '\r' | '\t')) {
            continue;
        }
        out.push(if matches!(ch, '\r' | '\t') { ' ' } else { ch });
    }
    out
}

fn push_unique(list: &mut Vec<String>, item: &str) {
    if !list.iter().any(|existing| existing == item) {
        list.push(item.to_owned());
    }
}

fn push_folded_char(out: &mut String, ch: char, prev_space: &mut bool) {
    if ch.is_whitespace() {
        if !*prev_space {
            out.push(' ');
            *prev_space = true;
        }
    } else {
        out.push(ch);
        *prev_space = false;
    }
}

fn contains_word_aligned(haystack: &str, needle: &str) -> bool {
    let Some(start) = haystack.find(needle) else {
        return false;
    };
    let before_ok = haystack[..start]
        .chars()
        .next_back()
        .is_none_or(|c| !c.is_ascii_alphanumeric());
    let after = start + needle.len();
    let after_ok = haystack[after..]
        .chars()
        .next()
        .is_none_or(|c| !c.is_ascii_alphanumeric());
    before_ok && after_ok
}

fn contains_role_marker(folded: &str, needle: &str) -> bool {
    if needle == "system:" || needle == "assistant:" {
        for (idx, _) in folded.match_indices(needle) {
            let line_start_ok = folded[..idx]
                .chars()
                .next_back()
                .is_none_or(|c| c == '\n' || c == ' ' || c == '[' || c == '<');
            if line_start_ok {
                return true;
            }
        }
        false
    } else {
        folded.contains(needle)
    }
}

fn contains_exfil_url(folded: &str) -> bool {
    let mut search_from = 0usize;
    while let Some(rel) = folded[search_from..].find("http") {
        let abs = search_from + rel;
        let tail = &folded[abs..];
        if tail.starts_with("http://") || tail.starts_with("https://") {
            let url_end = tail
                .find(|c: char| c.is_whitespace() || matches!(c, ')' | ']' | '"' | '\'' | '>'))
                .unwrap_or(tail.len());
            let url = &tail[..url_end];
            if EXFIL_URL_MARKERS.iter().any(|m| url.contains(m)) {
                return true;
            }
        }
        search_from = abs + 4;
    }
    false
}

fn max_base64_token_len(raw: &str) -> usize {
    let mut best = 0usize;
    for token in raw.split_whitespace() {
        let trimmed = token.trim_matches(|c: char| !is_base64_byte(c));
        if trimmed.len() <= best {
            continue;
        }
        if trimmed.chars().all(is_base64_byte) {
            best = trimmed.len();
        }
    }
    best
}

const fn is_base64_byte(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || matches!(ch, '+' | '/' | '=' | '_' | '-')
}

const fn is_bidi_char(ch: char) -> bool {
    matches!(
        ch,
        '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}' | '\u{200E}'..='\u{200F}' | '\u{061C}'
    )
}

const fn is_zero_width_char(ch: char) -> bool {
    matches!(
        ch,
        '\u{200B}'..='\u{200D}'
            | '\u{2060}'..='\u{2064}'
            | '\u{FEFF}'
            | '\u{034F}'
            | '\u{180E}'
            | '\u{00AD}'
    )
}

const fn is_unicode_tag_char(ch: char) -> bool {
    matches!(ch as u32, 0xE0000..=0xE007F)
}

fn decode_tag_char(ch: char) -> Option<char> {
    let cp = ch as u32;
    if (0xE0020..=0xE007E).contains(&cp) {
        char::from_u32(cp - 0xE0000)
    } else {
        None
    }
}

const fn map_confusable_ascii(ch: char) -> Option<char> {
    match ch {
        '\u{0430}' | '\u{0410}' => Some('a'), // Cyrillic a/A
        '\u{0435}' | '\u{0415}' => Some('e'), // Cyrillic e/E
        '\u{043E}' | '\u{041E}' => Some('o'), // Cyrillic o/O
        '\u{0440}' | '\u{0420}' => Some('p'), // Cyrillic r/P
        '\u{0441}' | '\u{0421}' => Some('c'), // Cyrillic s/C
        '\u{0445}' | '\u{0425}' => Some('x'), // Cyrillic h/X
        '\u{0456}' | '\u{0406}' => Some('i'), // Cyrillic i/I
        '\u{0458}' => Some('j'),              // Cyrillic je
        '\u{0443}' => Some('y'),              // Cyrillic u
        '\u{043A}' | '\u{041A}' => Some('k'), // Cyrillic k
        '\u{043C}' | '\u{041C}' => Some('m'), // Cyrillic m
        '\u{0442}' | '\u{0422}' => Some('t'), // Cyrillic t
        _ => None,
    }
}
