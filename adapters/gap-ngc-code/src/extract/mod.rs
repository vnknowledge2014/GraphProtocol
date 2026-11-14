//! Multi-language code and Markdown AST/structural extractors (`ING-060`, `ING-061`, `ING-063`).

mod code;
mod markdown;

use std::collections::BTreeSet;
use std::path::Path;

use crate::model::{DocKind, Language, SymbolKind};

/// Raw extracted declaration node scoped to a single source or documentation file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtractedNode {
    /// Local key relative to the file (unique within file).
    pub local_key: String,
    /// Target `gap-store` category (`Module`, `Symbol`, `Doc`, `Test`, `Concept`).
    pub node_type: &'static str,
    /// Human-readable symbol/module/section label.
    pub label: String,
    /// Optional symbol sub-kind (`function`, `method`, `struct`, ...).
    pub symbol_kind: Option<SymbolKind>,
    /// Optional doc sub-kind (`readme`, `adr`, `api`, `comment`, `markdown_section`).
    pub doc_kind: Option<DocKind>,
    /// Parent local key within the same file (e.g. struct/class for a method, or parent heading section).
    pub parent_local_key: Option<String>,
    /// Optional normalized signature or heading used by [`crate::model::summarize_text`].
    pub signature: String,
    /// Optional extracted first doc-comment / docstring block.
    pub doc_comment: Option<String>,
    /// Start 1-based source line number.
    pub start_line: usize,
    /// Outgoing call identifiers found inside this symbol or test body.
    pub calls: Vec<String>,
    /// Implemented traits / interfaces declared directly (`IMPLEMENTS`).
    pub implements: Vec<String>,
    /// Extended base classes / interfaces declared directly (`EXTENDS`).
    pub extends: Vec<String>,
}

/// Raw extracted file- or symbol-level edge reference resolved across the repository.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtractedRef {
    /// Local key of source node inside this file (`None` means the `File` node itself).
    pub from_local_key: Option<String>,
    /// Edge predicate (`IMPORTS`, `DEPENDS_ON`, `DOCUMENTS`, `REFERENCES`, ...).
    pub edge_type: &'static str,
    /// Target identifier, module name, path, or concept slug.
    pub target: String,
    /// Whether this reference comes from explicit syntax (`EXACT`) vs inline heuristic match.
    pub exact_syntax: bool,
}

/// Complete extraction result for a single healthy file (`ING-060`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtractedFileGraph {
    /// Repository-relative POSIX path.
    pub rel_path: String,
    /// Detected language.
    pub language: Language,
    /// Pinned grammar version string (`ING-061`).
    pub grammar_version: &'static str,
    /// Extracted nodes declared in this file.
    pub nodes: Vec<ExtractedNode>,
    /// Extracted cross-file / import / doc references.
    pub refs: Vec<ExtractedRef>,
}

/// Validates UTF-8 source integrity and balanced lexical/block structure so malformed files fail
/// cleanly without aborting sibling files (`ING-061`).
pub fn extract_file(
    rel_path: &str,
    lang: Language,
    raw_bytes: &[u8],
) -> Result<ExtractedFileGraph, String> {
    if raw_bytes.contains(&0) {
        return Err("binary NUL byte encountered in source file".to_owned());
    }
    let src = std::str::from_utf8(raw_bytes)
        .map_err(|err| format!("invalid UTF-8 at byte {}: {err}", err.valid_up_to()))?;

    validate_balanced_structure(src, lang)?;

    let mut graph = match lang {
        Language::Markdown => markdown::extract_markdown(rel_path, src),
        Language::Rust => code::extract_rust(rel_path, src),
        Language::TypeScript | Language::JavaScript => code::extract_ts_js(rel_path, lang, src),
        Language::Python => code::extract_python(rel_path, src),
        Language::Go => code::extract_go(rel_path, src),
        Language::Java => code::extract_java(rel_path, src),
    };

    graph.nodes.sort_by(|a, b| {
        a.start_line
            .cmp(&b.start_line)
            .then_with(|| a.local_key.cmp(&b.local_key))
    });
    Ok(graph)
}

/// Sanitizes a string segment for deterministic node IDs.
#[must_use]
pub fn slugify(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut prev_underscore = false;
    for ch in input.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            prev_underscore = false;
        } else if !prev_underscore && !out.is_empty() {
            out.push('_');
            prev_underscore = true;
        }
    }
    while out.ends_with('_') {
        out.pop();
    }
    if out.is_empty() {
        "root".to_owned()
    } else {
        out
    }
}

/// Computes module name from a repository-relative file path when no explicit `package`/`mod` header exists.
#[must_use]
pub fn default_module_name(rel_path: &str) -> String {
    let p = Path::new(rel_path);
    p.file_stem()
        .and_then(|s| s.to_str())
        .map_or_else(|| "module".to_owned(), ToOwned::to_owned)
}

pub(crate) fn validate_balanced_structure(src: &str, lang: Language) -> Result<(), String> {
    if matches!(lang, Language::Markdown) {
        return Ok(());
    }

    let bytes = src.as_bytes();
    let mut i = 0usize;
    let mut brace_depth: i32 = 0;
    let mut paren_depth: i32 = 0;
    let mut bracket_depth: i32 = 0;
    let uses_braces = !matches!(lang, Language::Python);
    let uses_hash_comments = matches!(lang, Language::Python);
    let mut line_no = 1usize;

    while i < bytes.len() {
        let b = bytes[i];
        if b == b'\n' {
            line_no += 1;
            i += 1;
            continue;
        }

        // Line comments: `#` in Python or `//` in C-family/Rust/TS/JS/Go/Java
        if uses_hash_comments && b == b'#' {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if !uses_hash_comments && b == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }

        // Block comments `/* ... */`
        if !uses_hash_comments && b == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'*' {
            i += 2;
            let mut closed = false;
            while i + 1 < bytes.len() {
                if bytes[i] == b'\n' {
                    line_no += 1;
                }
                if bytes[i] == b'*' && bytes[i + 1] == b'/' {
                    i += 2;
                    closed = true;
                    break;
                }
                i += 1;
            }
            if !closed {
                return Err(format!(
                    "unclosed block comment starting near line {line_no}"
                ));
            }
            continue;
        }

        // Python triple-quoted docstrings/strings
        if uses_hash_comments
            && (b == b'"' || b == b'\'')
            && i + 2 < bytes.len()
            && bytes[i + 1] == b
            && bytes[i + 2] == b
        {
            let q = b;
            i += 3;
            let mut closed = false;
            while i + 2 < bytes.len() {
                if bytes[i] == b'\n' {
                    line_no += 1;
                }
                if bytes[i] == b'\\' {
                    i += 2;
                    continue;
                }
                if bytes[i] == q && bytes[i + 1] == q && bytes[i + 2] == q {
                    i += 3;
                    closed = true;
                    break;
                }
                i += 1;
            }
            if !closed {
                return Err(format!("unclosed triple-quoted string near line {line_no}"));
            }
            continue;
        }

        // Go raw backtick string / JS template string
        if b == b'`'
            && matches!(
                lang,
                Language::Go | Language::JavaScript | Language::TypeScript
            )
        {
            i += 1;
            let mut closed = false;
            while i < bytes.len() {
                if bytes[i] == b'\n' {
                    line_no += 1;
                }
                if bytes[i] == b'\\' && matches!(lang, Language::JavaScript | Language::TypeScript)
                {
                    i += 2;
                    continue;
                }
                if bytes[i] == b'`' {
                    i += 1;
                    closed = true;
                    break;
                }
                i += 1;
            }
            if !closed {
                return Err(format!("unclosed backtick literal near line {line_no}"));
            }
            continue;
        }

        // Standard single-line strings `"..."` (and `'...'` in non-Rust or char literals)
        if b == b'"' || (b == b'\'' && !matches!(lang, Language::Rust)) {
            let quote = b;
            i += 1;
            let mut closed = false;
            while i < bytes.len() {
                if bytes[i] == b'\n' {
                    break;
                }
                if bytes[i] == b'\\' {
                    i += 2;
                    continue;
                }
                if bytes[i] == quote {
                    i += 1;
                    closed = true;
                    break;
                }
                i += 1;
            }
            if !closed {
                return Err(format!("unterminated string literal at line {line_no}"));
            }
            continue;
        }

        // Rust char literal vs lifetime `'a`
        if b == b'\'' && matches!(lang, Language::Rust) {
            if i + 2 < bytes.len() && bytes[i + 1] == b'\\' {
                if let Some(pos) = bytes[i + 2..]
                    .iter()
                    .position(|&c| c == b'\'' || c == b'\n')
                    && bytes[i + 2 + pos] == b'\''
                {
                    i += 2 + pos + 1;
                    continue;
                }
            } else if i + 2 < bytes.len() && bytes[i + 2] == b'\'' {
                i += 3;
                continue;
            }
            i += 1;
            continue;
        }

        match b {
            b'{' => brace_depth += 1,
            b'}' => {
                brace_depth -= 1;
                if brace_depth < 0 {
                    return Err(format!("unexpected closing `}}` at line {line_no}"));
                }
            }
            b'(' => paren_depth += 1,
            b')' => {
                paren_depth -= 1;
                if paren_depth < 0 {
                    return Err(format!("unexpected closing `)` at line {line_no}"));
                }
            }
            b'[' => bracket_depth += 1,
            b']' => {
                bracket_depth -= 1;
                if bracket_depth < 0 {
                    return Err(format!("unexpected closing `]` at line {line_no}"));
                }
            }
            _ => {}
        }
        i += 1;
    }

    if uses_braces && brace_depth != 0 {
        return Err(format!("unbalanced braces: {brace_depth} unclosed `{{`"));
    }
    if paren_depth != 0 {
        return Err(format!(
            "unbalanced parentheses: {paren_depth} unclosed `(`"
        ));
    }
    if bracket_depth != 0 {
        return Err(format!("unbalanced brackets: {bracket_depth} unclosed `[`"));
    }

    Ok(())
}

/// Scans an indented/braced code slice for outgoing call identifiers (`foo(...)` / `obj.bar(...)`),
/// filtering out language keywords and the caller's own name.
pub(crate) fn extract_call_targets(body_lines: &[&str], self_name: &str) -> Vec<String> {
    let mut found = BTreeSet::new();
    for line in body_lines {
        let trimmed = line.trim();
        if trimmed.starts_with("//") || trimmed.starts_with('#') || trimmed.starts_with('*') {
            continue;
        }
        // Strip quoted segments naively per line so identifier-like strings don't become fake calls.
        let cleaned = strip_line_strings_and_comments(line);
        let bytes = cleaned.as_bytes();
        let mut idx = 0usize;
        while idx < bytes.len() {
            if bytes[idx] == b'(' && idx > 0 {
                let mut end = idx;
                while end > 0 && bytes[end - 1].is_ascii_whitespace() {
                    end -= 1;
                }
                let mut start = end;
                while start > 0
                    && (bytes[start - 1].is_ascii_alphanumeric() || bytes[start - 1] == b'_')
                {
                    start -= 1;
                }
                if start < end {
                    let cand = &cleaned[start..end];
                    if !is_non_call_keyword(cand)
                        && cand != self_name
                        && !cand.chars().all(|c| c.is_ascii_digit())
                    {
                        found.insert(cand.to_owned());
                    }
                }
            }
            idx += 1;
        }
    }
    found.into_iter().collect()
}

fn strip_line_strings_and_comments(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars().peekable();
    let mut in_quote: Option<char> = None;
    let mut escaped = false;

    while let Some(ch) = chars.next() {
        if let Some(q) = in_quote {
            if escaped {
                escaped = false;
                continue;
            }
            if ch == '\\' {
                escaped = true;
                continue;
            }
            if ch == q {
                in_quote = None;
            }
            out.push(' ');
            continue;
        }
        if ch == '/' && chars.peek() == Some(&'/') {
            break;
        }
        if ch == '"' || ch == '\'' || ch == '`' {
            in_quote = Some(ch);
            out.push(' ');
        } else {
            out.push(ch);
        }
    }
    out
}

fn is_non_call_keyword(ident: &str) -> bool {
    matches!(
        ident,
        "if" | "else"
            | "for"
            | "while"
            | "loop"
            | "match"
            | "switch"
            | "case"
            | "catch"
            | "try"
            | "return"
            | "sizeof"
            | "typeof"
            | "fn"
            | "def"
            | "func"
            | "function"
            | "class"
            | "struct"
            | "enum"
            | "trait"
            | "interface"
            | "new"
            | "Some"
            | "Ok"
            | "Err"
            | "len"
            | "make"
            | "append"
            | "print"
            | "println"
            | "super"
            | "Self"
            | "self"
            | "async"
            | "await"
            | "test"
    )
}
