//! Language-specific code AST/structural extractors for Rust, TypeScript/JavaScript,
//! Python, Go, and Java (`ING-060`, `ING-061`).

use std::sync::OnceLock;

use regex::Regex;

use super::{
    ExtractedFileGraph, ExtractedNode, ExtractedRef, default_module_name, extract_call_targets,
};
use crate::model::{
    EDGE_DEPENDS_ON, EDGE_IMPORTS, Language, NODE_MODULE, NODE_SYMBOL, NODE_TEST, SymbolKind,
};

macro_rules! lazy_re {
    ($pat:literal) => {{
        static RE: OnceLock<Option<Regex>> = OnceLock::new();
        RE.get_or_init(|| Regex::new($pat).ok()).as_ref()
    }};
}

fn find_brace_block_end(lines: &[&str], start_idx: usize) -> usize {
    let mut depth = 0i32;
    let mut opened = false;
    for (offset, line) in lines[start_idx..].iter().enumerate() {
        let mut in_str = false;
        let mut escaped = false;
        let chars: Vec<char> = line.chars().collect();
        let mut k = 0usize;
        while k < chars.len() {
            let ch = chars[k];
            if in_str {
                if escaped {
                    escaped = false;
                } else if ch == '\\' {
                    escaped = true;
                } else if ch == '"' || ch == '\'' || ch == '`' {
                    in_str = false;
                }
                k += 1;
                continue;
            }
            if ch == '/' && k + 1 < chars.len() && chars[k + 1] == '/' {
                break;
            }
            if ch == '"' || ch == '`' {
                in_str = true;
            } else if ch == '{' {
                depth += 1;
                opened = true;
            } else if ch == '}' {
                depth -= 1;
                if opened && depth <= 0 {
                    return start_idx + offset;
                }
            }
            k += 1;
        }
        if !opened && offset >= 3 {
            return start_idx + offset;
        }
    }
    lines.len().saturating_sub(1)
}

fn leading_indent(line: &str) -> usize {
    line.chars().take_while(|c| *c == ' ' || *c == '\t').count()
}

fn find_python_block_end(lines: &[&str], start_idx: usize) -> usize {
    let header_indent = leading_indent(lines[start_idx]);
    let mut end = start_idx;
    for (offset, line) in lines[start_idx + 1..].iter().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if leading_indent(line) <= header_indent {
            return end;
        }
        end = start_idx + 1 + offset;
    }
    end
}

fn collect_slash_doc_comment(lines: &[&str], decl_line: usize) -> Option<String> {
    if decl_line == 0 {
        return None;
    }
    let mut collected = Vec::new();
    let mut idx = decl_line;
    let mut in_block_comment = false;

    while idx > 0 {
        idx -= 1;
        let trimmed = lines[idx].trim();
        if in_block_comment {
            collected.push(trimmed.to_owned());
            if trimmed.starts_with("/*") {
                break;
            }
            continue;
        }
        if trimmed.starts_with("#[") || trimmed.starts_with('@') {
            continue;
        }
        if trimmed.starts_with("///") || trimmed.starts_with("//") {
            collected.push(trimmed.to_owned());
            continue;
        }
        if trimmed.ends_with("*/") {
            in_block_comment = true;
            collected.push(trimmed.to_owned());
            if trimmed.starts_with("/*") {
                break;
            }
            continue;
        }
        break;
    }

    if collected.is_empty() {
        None
    } else {
        collected.reverse();
        Some(collected.join("\n"))
    }
}

fn collect_module_slash_banner(lines: &[&str]) -> Option<String> {
    let mut collected = Vec::new();
    for line in lines {
        let trimmed = line.trim();
        if trimmed.starts_with("//!") || trimmed.starts_with("///") || trimmed.starts_with('#') {
            collected.push(trimmed.to_owned());
        } else if !trimmed.is_empty() {
            break;
        }
    }
    if collected.is_empty() {
        None
    } else {
        Some(collected.join("\n"))
    }
}

fn collect_python_docstring(lines: &[&str], header_line: usize) -> Option<String> {
    for line in lines.iter().skip(header_line + 1) {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if trimmed.starts_with("\"\"\"") || trimmed.starts_with("'''") {
            return Some(trimmed.to_owned());
        }
        break;
    }
    None
}

fn push_module_node(
    nodes: &mut Vec<ExtractedNode>,
    mod_name: String,
    doc_comment: Option<String>,
    sig: String,
) -> String {
    let local_key = format!("mod:{mod_name}");
    nodes.push(ExtractedNode {
        local_key: local_key.clone(),
        node_type: NODE_MODULE,
        label: mod_name,
        symbol_kind: None,
        doc_kind: None,
        parent_local_key: None,
        signature: sig,
        doc_comment,
        start_line: 1,
        calls: Vec::new(),
        implements: Vec::new(),
        extends: Vec::new(),
    });
    local_key
}

/// Extracts Rust modules, uses, structs, enums, traits, consts, type aliases, impls, functions,
/// methods, and `#[test]` / `#[tokio::test]` cases (`ING-060`, `ING-061`).
pub(crate) fn extract_rust(rel_path: &str, src: &str) -> ExtractedFileGraph {
    let lines: Vec<&str> = src.lines().collect();
    let mod_name = default_module_name(rel_path);
    let mut nodes = Vec::new();
    let mut refs = Vec::new();
    let mod_key = push_module_node(
        &mut nodes,
        mod_name.clone(),
        collect_module_slash_banner(&lines),
        format!("mod {mod_name}"),
    );

    let use_re = lazy_re!(r"^(?:pub(?:\([^)]*\))?\s+)?use\s+([^;]+);");
    let mod_decl_re = lazy_re!(r"^(?:pub(?:\([^)]*\))?\s+)?mod\s+([A-Za-z_][A-Za-z0-9_]*)\s*[;{]");
    let struct_re = lazy_re!(r"^(?:pub(?:\([^)]*\))?\s+)?struct\s+([A-Za-z_][A-Za-z0-9_]*)");
    let enum_re = lazy_re!(r"^(?:pub(?:\([^)]*\))?\s+)?enum\s+([A-Za-z_][A-Za-z0-9_]*)");
    let trait_re = lazy_re!(r"^(?:pub(?:\([^)]*\))?\s+)?trait\s+([A-Za-z_][A-Za-z0-9_]*)");
    let type_re = lazy_re!(r"^(?:pub(?:\([^)]*\))?\s+)?type\s+([A-Za-z_][A-Za-z0-9_]*)");
    let const_re = lazy_re!(r"^(?:pub(?:\([^)]*\))?\s+)?const\s+([A-Za-z_][A-Za-z0-9_]*)\s*:");
    let fn_re = lazy_re!(
        r"^(?:pub(?:\([^)]*\))?\s+)?(?:async\s+)?(?:unsafe\s+)?fn\s+([A-Za-z_][A-Za-z0-9_]*)"
    );
    let impl_re = lazy_re!(
        r"^impl(?:<[^>]*>)?\s+(?:([A-Za-z_][A-Za-z0-9_:]*)(?:<[^>]*>)?\s+for\s+)?([A-Za-z_][A-Za-z0-9_]*)"
    );

    let mut pending_test = false;
    let mut impl_stack: Vec<(usize, String, Option<String>)> = Vec::new();

    for (idx, raw_line) in lines.iter().enumerate() {
        while impl_stack.last().is_some_and(|(end, _, _)| idx > *end) {
            impl_stack.pop();
        }
        let trimmed = raw_line.trim();
        if trimmed == "#[test]" || trimmed.starts_with("#[tokio::test") {
            pending_test = true;
            continue;
        }
        if trimmed.starts_with("//") {
            continue;
        }

        if let Some(re) = use_re
            && let Some(caps) = re.captures(trimmed)
        {
            let target = caps[1].trim().to_owned();
            refs.push(ExtractedRef {
                from_local_key: Some(mod_key.clone()),
                edge_type: EDGE_IMPORTS,
                target: target.clone(),
                exact_syntax: true,
            });
            let dep_root = target.split("::").next().unwrap_or(&target).to_owned();
            if !matches!(dep_root.as_str(), "crate" | "self" | "super" | "std") {
                refs.push(ExtractedRef {
                    from_local_key: None,
                    edge_type: EDGE_DEPENDS_ON,
                    target: dep_root,
                    exact_syntax: true,
                });
            }
            continue;
        }

        if let Some(re) = mod_decl_re
            && let Some(caps) = re.captures(trimmed)
        {
            let submod = caps[1].to_owned();
            nodes.push(ExtractedNode {
                local_key: format!("submod:{submod}"),
                node_type: NODE_MODULE,
                label: submod,
                symbol_kind: None,
                doc_kind: None,
                parent_local_key: Some(mod_key.clone()),
                signature: trimmed.trim_end_matches('{').trim().to_owned(),
                doc_comment: collect_slash_doc_comment(&lines, idx),
                start_line: idx + 1,
                calls: Vec::new(),
                implements: Vec::new(),
                extends: Vec::new(),
            });
            continue;
        }

        if let Some(re) = impl_re
            && let Some(caps) = re.captures(trimmed)
            && trimmed.contains('{')
        {
            let end = find_brace_block_end(&lines, idx);
            let target_ty = caps[2].to_owned();
            let trait_name = caps.get(1).map(|m| m.as_str().to_owned());
            if let Some(ref tr) = trait_name {
                // Attach IMPLEMENTS on the struct if declared in this file, or emit reference
                if let Some(existing) = nodes.iter_mut().find(|n| n.label == target_ty) {
                    existing.implements.push(tr.clone());
                } else {
                    refs.push(ExtractedRef {
                        from_local_key: Some(mod_key.clone()),
                        edge_type: crate::model::EDGE_IMPLEMENTS,
                        target: tr.clone(),
                        exact_syntax: true,
                    });
                }
            }
            impl_stack.push((end, target_ty, trait_name));
            continue;
        }

        for (re_opt, kind) in [
            (struct_re, SymbolKind::Struct),
            (enum_re, SymbolKind::Enum),
            (trait_re, SymbolKind::Trait),
            (type_re, SymbolKind::Type),
            (const_re, SymbolKind::Const),
        ] {
            if let Some(re) = re_opt
                && let Some(caps) = re.captures(trimmed)
            {
                let name = caps[1].to_owned();
                let mut parent = mod_key.clone();
                if let Some((_, ty, _)) = impl_stack.last() {
                    parent = format!("sym:{ty}");
                }
                nodes.push(ExtractedNode {
                    local_key: format!("sym:{name}"),
                    node_type: NODE_SYMBOL,
                    label: name,
                    symbol_kind: Some(kind),
                    doc_kind: None,
                    parent_local_key: Some(parent),
                    signature: trimmed.trim_end_matches('{').trim().to_owned(),
                    doc_comment: collect_slash_doc_comment(&lines, idx),
                    start_line: idx + 1,
                    calls: Vec::new(),
                    implements: Vec::new(),
                    extends: Vec::new(),
                });
                break;
            }
        }

        if let Some(re) = fn_re
            && let Some(caps) = re.captures(trimmed)
        {
            let name = caps[1].to_owned();
            let end = find_brace_block_end(&lines, idx);
            let calls = extract_call_targets(&lines[idx..=end], &name);
            let is_test = pending_test;
            pending_test = false;

            let (node_type, symbol_kind, parent_local_key, local_key) = if is_test {
                (
                    NODE_TEST,
                    None,
                    Some(mod_key.clone()),
                    format!("test:{name}"),
                )
            } else if let Some((_, owner_ty, _)) = impl_stack.last() {
                (
                    NODE_SYMBOL,
                    Some(SymbolKind::Method),
                    Some(format!("sym:{owner_ty}")),
                    format!("sym:{owner_ty}.{name}"),
                )
            } else {
                (
                    NODE_SYMBOL,
                    Some(SymbolKind::Function),
                    Some(mod_key.clone()),
                    format!("sym:{name}"),
                )
            };

            nodes.push(ExtractedNode {
                local_key,
                node_type,
                label: name,
                symbol_kind,
                doc_kind: None,
                parent_local_key,
                signature: trimmed.trim_end_matches('{').trim().to_owned(),
                doc_comment: collect_slash_doc_comment(&lines, idx),
                start_line: idx + 1,
                calls,
                implements: Vec::new(),
                extends: Vec::new(),
            });
            continue;
        }

        if !trimmed.starts_with('#') {
            pending_test = false;
        }
    }

    ExtractedFileGraph {
        rel_path: rel_path.to_owned(),
        language: Language::Rust,
        grammar_version: Language::Rust.grammar_version(),
        nodes,
        refs,
    }
}

/// Extracts TypeScript and JavaScript modules, imports, classes, interfaces, types, functions,
/// methods, and `test`/`it` cases (`ING-060`, `ING-061`).
pub(crate) fn extract_ts_js(rel_path: &str, lang: Language, src: &str) -> ExtractedFileGraph {
    let lines: Vec<&str> = src.lines().collect();
    let mod_name = default_module_name(rel_path);
    let mut nodes = Vec::new();
    let mut refs = Vec::new();
    let mod_key = push_module_node(
        &mut nodes,
        mod_name.clone(),
        collect_module_slash_banner(&lines),
        format!("module {mod_name}"),
    );

    let import_re = lazy_re!(r#"^import\s+.*from\s+["']([^"']+)["']"#);
    let class_re = lazy_re!(
        r"^(?:export\s+(?:default\s+)?)?class\s+([A-Za-z_$][A-Za-z0-9_$]*)(?:\s+extends\s+([A-Za-z_$][A-Za-z0-9_$.]*))?(?:\s+implements\s+([A-Za-z0-9_$,.\s]+))?"
    );
    let iface_re = lazy_re!(
        r"^(?:export\s+)?interface\s+([A-Za-z_$][A-Za-z0-9_$]*)(?:\s+extends\s+([A-Za-z0-9_$,.\s]+))?"
    );
    let type_re = lazy_re!(r"^(?:export\s+)?type\s+([A-Za-z_$][A-Za-z0-9_$]*)\s*=");
    let fn_re = lazy_re!(
        r"^(?:export\s+(?:default\s+)?)?(?:async\s+)?function\s+([A-Za-z_$][A-Za-z0-9_$]*)"
    );
    let method_re = lazy_re!(
        r"^(?:public\s+|private\s+|protected\s+)?(?:static\s+)?(?:async\s+)?([A-Za-z_$][A-Za-z0-9_$]*)\s*\([^)]*\)\s*(?::\s*[^{]+)?\s*\{"
    );
    let test_re = lazy_re!(r#"^(?:test|it)\s*\(\s*["']([^"']+)["']"#);

    let mut class_stack: Vec<(usize, String)> = Vec::new();

    for (idx, raw_line) in lines.iter().enumerate() {
        while class_stack.last().is_some_and(|(end, _)| idx > *end) {
            class_stack.pop();
        }
        let trimmed = raw_line.trim();
        if trimmed.starts_with("//") || trimmed.starts_with('*') {
            continue;
        }

        if let Some(re) = import_re
            && let Some(caps) = re.captures(trimmed)
        {
            let target = caps[1].to_owned();
            refs.push(ExtractedRef {
                from_local_key: Some(mod_key.clone()),
                edge_type: EDGE_IMPORTS,
                target,
                exact_syntax: true,
            });
            continue;
        }

        if let Some(re) = test_re
            && let Some(caps) = re.captures(trimmed)
        {
            let name = caps[1].to_owned();
            let end = find_brace_block_end(&lines, idx);
            let calls = extract_call_targets(&lines[idx..=end], &name);
            nodes.push(ExtractedNode {
                local_key: format!("test:{}", super::slugify(&name)),
                node_type: NODE_TEST,
                label: name,
                symbol_kind: None,
                doc_kind: None,
                parent_local_key: Some(mod_key.clone()),
                signature: trimmed.trim_end_matches('{').trim().to_owned(),
                doc_comment: collect_slash_doc_comment(&lines, idx),
                start_line: idx + 1,
                calls,
                implements: Vec::new(),
                extends: Vec::new(),
            });
            continue;
        }

        if let Some(re) = class_re
            && let Some(caps) = re.captures(trimmed)
        {
            let name = caps[1].to_owned();
            let extends = caps
                .get(2)
                .map(|m| vec![m.as_str().trim().to_owned()])
                .unwrap_or_default();
            let implements = caps
                .get(3)
                .map(|m| {
                    m.as_str()
                        .split(',')
                        .map(|s| s.trim().trim_end_matches('{').trim().to_owned())
                        .filter(|s| !s.is_empty())
                        .collect()
                })
                .unwrap_or_default();
            let end = find_brace_block_end(&lines, idx);
            nodes.push(ExtractedNode {
                local_key: format!("sym:{name}"),
                node_type: NODE_SYMBOL,
                label: name.clone(),
                symbol_kind: Some(SymbolKind::Class),
                doc_kind: None,
                parent_local_key: Some(mod_key.clone()),
                signature: trimmed.trim_end_matches('{').trim().to_owned(),
                doc_comment: collect_slash_doc_comment(&lines, idx),
                start_line: idx + 1,
                calls: Vec::new(),
                implements,
                extends,
            });
            class_stack.push((end, name));
            continue;
        }

        if let Some(re) = iface_re
            && let Some(caps) = re.captures(trimmed)
        {
            let name = caps[1].to_owned();
            let extends = caps
                .get(2)
                .map(|m| {
                    m.as_str()
                        .split(',')
                        .map(|s| s.trim().trim_end_matches('{').trim().to_owned())
                        .filter(|s| !s.is_empty())
                        .collect()
                })
                .unwrap_or_default();
            nodes.push(ExtractedNode {
                local_key: format!("sym:{name}"),
                node_type: NODE_SYMBOL,
                label: name,
                symbol_kind: Some(SymbolKind::Interface),
                doc_kind: None,
                parent_local_key: Some(mod_key.clone()),
                signature: trimmed.trim_end_matches('{').trim().to_owned(),
                doc_comment: collect_slash_doc_comment(&lines, idx),
                start_line: idx + 1,
                calls: Vec::new(),
                implements: Vec::new(),
                extends,
            });
            continue;
        }

        if let Some(re) = type_re
            && let Some(caps) = re.captures(trimmed)
        {
            let name = caps[1].to_owned();
            nodes.push(ExtractedNode {
                local_key: format!("sym:{name}"),
                node_type: NODE_SYMBOL,
                label: name,
                symbol_kind: Some(SymbolKind::Type),
                doc_kind: None,
                parent_local_key: Some(mod_key.clone()),
                signature: trimmed.trim_end_matches(';').trim().to_owned(),
                doc_comment: collect_slash_doc_comment(&lines, idx),
                start_line: idx + 1,
                calls: Vec::new(),
                implements: Vec::new(),
                extends: Vec::new(),
            });
            continue;
        }

        if let Some(re) = fn_re
            && let Some(caps) = re.captures(trimmed)
        {
            let name = caps[1].to_owned();
            let end = find_brace_block_end(&lines, idx);
            let calls = extract_call_targets(&lines[idx..=end], &name);
            nodes.push(ExtractedNode {
                local_key: format!("sym:{name}"),
                node_type: NODE_SYMBOL,
                label: name,
                symbol_kind: Some(SymbolKind::Function),
                doc_kind: None,
                parent_local_key: Some(mod_key.clone()),
                signature: trimmed.trim_end_matches('{').trim().to_owned(),
                doc_comment: collect_slash_doc_comment(&lines, idx),
                start_line: idx + 1,
                calls,
                implements: Vec::new(),
                extends: Vec::new(),
            });
            continue;
        }

        if let Some((_, owner_cls)) = class_stack.last()
            && let Some(re) = method_re
            && let Some(caps) = re.captures(trimmed)
        {
            let name = caps[1].to_owned();
            if !matches!(name.as_str(), "if" | "for" | "while" | "switch" | "catch") {
                let end = find_brace_block_end(&lines, idx);
                let calls = extract_call_targets(&lines[idx..=end], &name);
                nodes.push(ExtractedNode {
                    local_key: format!("sym:{owner_cls}.{name}"),
                    node_type: NODE_SYMBOL,
                    label: name,
                    symbol_kind: Some(SymbolKind::Method),
                    doc_kind: None,
                    parent_local_key: Some(format!("sym:{owner_cls}")),
                    signature: trimmed.trim_end_matches('{').trim().to_owned(),
                    doc_comment: collect_slash_doc_comment(&lines, idx),
                    start_line: idx + 1,
                    calls,
                    implements: Vec::new(),
                    extends: Vec::new(),
                });
            }
        }
    }

    ExtractedFileGraph {
        rel_path: rel_path.to_owned(),
        language: lang,
        grammar_version: lang.grammar_version(),
        nodes,
        refs,
    }
}

/// Extracts Python imports, classes, base inheritance, functions, methods, docstrings, and `test_*` (`ING-060`, `ING-061`).
pub(crate) fn extract_python(rel_path: &str, src: &str) -> ExtractedFileGraph {
    let lines: Vec<&str> = src.lines().collect();
    let mod_name = default_module_name(rel_path);
    let mut nodes = Vec::new();
    let mut refs = Vec::new();
    let mod_key = push_module_node(
        &mut nodes,
        mod_name.clone(),
        collect_module_slash_banner(&lines),
        format!("module {mod_name}"),
    );

    let import_re = lazy_re!(r"^import\s+([A-Za-z0-9_.,\s]+)");
    let from_re = lazy_re!(r"^from\s+([A-Za-z0-9_.]+)\s+import\s+");
    let class_re = lazy_re!(r"^class\s+([A-Za-z_][A-Za-z0-9_]*)(?:\(([^)]*)\))?\s*:");
    let def_re = lazy_re!(r"^(?:async\s+)?def\s+([A-Za-z_][A-Za-z0-9_]*)\s*\(");

    let mut class_stack: Vec<(usize, String, usize)> = Vec::new();

    for (idx, raw_line) in lines.iter().enumerate() {
        while class_stack.last().is_some_and(|(end, _, _)| idx > *end) {
            class_stack.pop();
        }
        let trimmed = raw_line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }

        if let Some(re) = from_re
            && let Some(caps) = re.captures(trimmed)
        {
            refs.push(ExtractedRef {
                from_local_key: Some(mod_key.clone()),
                edge_type: EDGE_IMPORTS,
                target: caps[1].to_owned(),
                exact_syntax: true,
            });
            continue;
        }
        if let Some(re) = import_re
            && let Some(caps) = re.captures(trimmed)
        {
            for item in caps[1].split(',') {
                let target = item.split(" as ").next().unwrap_or("").trim();
                if !target.is_empty() {
                    refs.push(ExtractedRef {
                        from_local_key: Some(mod_key.clone()),
                        edge_type: EDGE_IMPORTS,
                        target: target.to_owned(),
                        exact_syntax: true,
                    });
                }
            }
            continue;
        }

        if let Some(re) = class_re
            && let Some(caps) = re.captures(trimmed)
        {
            let name = caps[1].to_owned();
            let extends = caps
                .get(2)
                .map(|m| {
                    m.as_str()
                        .split(',')
                        .map(|s| s.trim().to_owned())
                        .filter(|s| !s.is_empty() && s != "object")
                        .collect()
                })
                .unwrap_or_default();
            let end = find_python_block_end(&lines, idx);
            nodes.push(ExtractedNode {
                local_key: format!("sym:{name}"),
                node_type: NODE_SYMBOL,
                label: name.clone(),
                symbol_kind: Some(SymbolKind::Class),
                doc_kind: None,
                parent_local_key: Some(mod_key.clone()),
                signature: trimmed.trim_end_matches(':').trim().to_owned(),
                doc_comment: collect_python_docstring(&lines, idx),
                start_line: idx + 1,
                calls: Vec::new(),
                implements: Vec::new(),
                extends,
            });
            class_stack.push((end, name, leading_indent(raw_line)));
            continue;
        }

        if let Some(re) = def_re
            && let Some(caps) = re.captures(trimmed)
        {
            let name = caps[1].to_owned();
            let end = find_python_block_end(&lines, idx);
            let calls = extract_call_targets(&lines[idx..=end], &name);
            let cur_indent = leading_indent(raw_line);
            let is_test = name.starts_with("test_");
            let doc_comment = collect_python_docstring(&lines, idx);

            if is_test {
                nodes.push(ExtractedNode {
                    local_key: format!("test:{name}"),
                    node_type: NODE_TEST,
                    label: name,
                    symbol_kind: None,
                    doc_kind: None,
                    parent_local_key: Some(mod_key.clone()),
                    signature: trimmed.trim_end_matches(':').trim().to_owned(),
                    doc_comment,
                    start_line: idx + 1,
                    calls,
                    implements: Vec::new(),
                    extends: Vec::new(),
                });
            } else if let Some((_, owner_cls, cls_indent)) = class_stack.last()
                && cur_indent > *cls_indent
            {
                nodes.push(ExtractedNode {
                    local_key: format!("sym:{owner_cls}.{name}"),
                    node_type: NODE_SYMBOL,
                    label: name,
                    symbol_kind: Some(SymbolKind::Method),
                    doc_kind: None,
                    parent_local_key: Some(format!("sym:{owner_cls}")),
                    signature: trimmed.trim_end_matches(':').trim().to_owned(),
                    doc_comment,
                    start_line: idx + 1,
                    calls,
                    implements: Vec::new(),
                    extends: Vec::new(),
                });
            } else {
                nodes.push(ExtractedNode {
                    local_key: format!("sym:{name}"),
                    node_type: NODE_SYMBOL,
                    label: name,
                    symbol_kind: Some(SymbolKind::Function),
                    doc_kind: None,
                    parent_local_key: Some(mod_key.clone()),
                    signature: trimmed.trim_end_matches(':').trim().to_owned(),
                    doc_comment,
                    start_line: idx + 1,
                    calls,
                    implements: Vec::new(),
                    extends: Vec::new(),
                });
            }
        }
    }

    ExtractedFileGraph {
        rel_path: rel_path.to_owned(),
        language: Language::Python,
        grammar_version: Language::Python.grammar_version(),
        nodes,
        refs,
    }
}

/// Extracts Go packages, imports, structs, interfaces, functions, receiver methods, and `Test*` (`ING-060`, `ING-061`).
pub(crate) fn extract_go(rel_path: &str, src: &str) -> ExtractedFileGraph {
    let lines: Vec<&str> = src.lines().collect();
    let pkg_re = lazy_re!(r"^package\s+([A-Za-z_][A-Za-z0-9_]*)");
    let pkg_name = lines
        .iter()
        .find_map(|l| {
            pkg_re
                .and_then(|re| re.captures(l.trim()))
                .map(|c| c[1].to_owned())
        })
        .unwrap_or_else(|| default_module_name(rel_path));

    let mut nodes = Vec::new();
    let mut refs = Vec::new();
    let mod_key = push_module_node(
        &mut nodes,
        pkg_name.clone(),
        collect_module_slash_banner(&lines),
        format!("package {pkg_name}"),
    );

    let import_single_re = lazy_re!(r#"^import\s+(?:[A-Za-z_][A-Za-z0-9_]*\s+)?"([^"]+)""#);
    let import_item_re = lazy_re!(r#"^(?:[A-Za-z_][A-Za-z0-9_]*\s+)?"([^"]+)""#);
    let type_struct_re = lazy_re!(r"^type\s+([A-Za-z_][A-Za-z0-9_]*)\s+struct\b");
    let type_iface_re = lazy_re!(r"^type\s+([A-Za-z_][A-Za-z0-9_]*)\s+interface\b");
    let receiver_fn_re = lazy_re!(
        r"^func\s+\(\s*[A-Za-z_][A-Za-z0-9_]*\s+\*?([A-Za-z_][A-Za-z0-9_]*)\s*\)\s+([A-Za-z_][A-Za-z0-9_]*)\s*\("
    );
    let func_re = lazy_re!(r"^func\s+([A-Za-z_][A-Za-z0-9_]*)\s*\(");

    let mut in_import_block = false;
    for (idx, raw_line) in lines.iter().enumerate() {
        let trimmed = raw_line.trim();
        if trimmed.starts_with("//") {
            continue;
        }
        if trimmed == "import (" {
            in_import_block = true;
            continue;
        }
        if in_import_block {
            if trimmed == ")" {
                in_import_block = false;
                continue;
            }
            if let Some(re) = import_item_re
                && let Some(caps) = re.captures(trimmed)
            {
                refs.push(ExtractedRef {
                    from_local_key: Some(mod_key.clone()),
                    edge_type: EDGE_IMPORTS,
                    target: caps[1].to_owned(),
                    exact_syntax: true,
                });
            }
            continue;
        }

        if let Some(re) = import_single_re
            && let Some(caps) = re.captures(trimmed)
        {
            refs.push(ExtractedRef {
                from_local_key: Some(mod_key.clone()),
                edge_type: EDGE_IMPORTS,
                target: caps[1].to_owned(),
                exact_syntax: true,
            });
            continue;
        }

        if let Some(re) = type_struct_re
            && let Some(caps) = re.captures(trimmed)
        {
            let name = caps[1].to_owned();
            nodes.push(ExtractedNode {
                local_key: format!("sym:{name}"),
                node_type: NODE_SYMBOL,
                label: name,
                symbol_kind: Some(SymbolKind::Struct),
                doc_kind: None,
                parent_local_key: Some(mod_key.clone()),
                signature: trimmed.trim_end_matches('{').trim().to_owned(),
                doc_comment: collect_slash_doc_comment(&lines, idx),
                start_line: idx + 1,
                calls: Vec::new(),
                implements: Vec::new(),
                extends: Vec::new(),
            });
            continue;
        }

        if let Some(re) = type_iface_re
            && let Some(caps) = re.captures(trimmed)
        {
            let name = caps[1].to_owned();
            nodes.push(ExtractedNode {
                local_key: format!("sym:{name}"),
                node_type: NODE_SYMBOL,
                label: name,
                symbol_kind: Some(SymbolKind::Interface),
                doc_kind: None,
                parent_local_key: Some(mod_key.clone()),
                signature: trimmed.trim_end_matches('{').trim().to_owned(),
                doc_comment: collect_slash_doc_comment(&lines, idx),
                start_line: idx + 1,
                calls: Vec::new(),
                implements: Vec::new(),
                extends: Vec::new(),
            });
            continue;
        }

        if let Some(re) = receiver_fn_re
            && let Some(caps) = re.captures(trimmed)
        {
            let owner = caps[1].to_owned();
            let name = caps[2].to_owned();
            let end = find_brace_block_end(&lines, idx);
            let calls = extract_call_targets(&lines[idx..=end], &name);
            nodes.push(ExtractedNode {
                local_key: format!("sym:{owner}.{name}"),
                node_type: NODE_SYMBOL,
                label: name,
                symbol_kind: Some(SymbolKind::Method),
                doc_kind: None,
                parent_local_key: Some(format!("sym:{owner}")),
                signature: trimmed.trim_end_matches('{').trim().to_owned(),
                doc_comment: collect_slash_doc_comment(&lines, idx),
                start_line: idx + 1,
                calls,
                implements: Vec::new(),
                extends: Vec::new(),
            });
            continue;
        }

        if let Some(re) = func_re
            && let Some(caps) = re.captures(trimmed)
        {
            let name = caps[1].to_owned();
            let end = find_brace_block_end(&lines, idx);
            let calls = extract_call_targets(&lines[idx..=end], &name);
            let is_test = name.starts_with("Test");
            nodes.push(ExtractedNode {
                local_key: if is_test {
                    format!("test:{name}")
                } else {
                    format!("sym:{name}")
                },
                node_type: if is_test { NODE_TEST } else { NODE_SYMBOL },
                label: name,
                symbol_kind: (!is_test).then_some(SymbolKind::Function),
                doc_kind: None,
                parent_local_key: Some(mod_key.clone()),
                signature: trimmed.trim_end_matches('{').trim().to_owned(),
                doc_comment: collect_slash_doc_comment(&lines, idx),
                start_line: idx + 1,
                calls,
                implements: Vec::new(),
                extends: Vec::new(),
            });
        }
    }

    ExtractedFileGraph {
        rel_path: rel_path.to_owned(),
        language: Language::Go,
        grammar_version: Language::Go.grammar_version(),
        nodes,
        refs,
    }
}

/// Extracts Java packages, imports, classes, interfaces, enums, methods, and `@Test` cases (`ING-060`, `ING-061`).
pub(crate) fn extract_java(rel_path: &str, src: &str) -> ExtractedFileGraph {
    let lines: Vec<&str> = src.lines().collect();
    let pkg_re = lazy_re!(r"^package\s+([A-Za-z0-9_.]+)\s*;");
    let pkg_name = lines
        .iter()
        .find_map(|l| {
            pkg_re
                .and_then(|re| re.captures(l.trim()))
                .map(|c| c[1].to_owned())
        })
        .unwrap_or_else(|| default_module_name(rel_path));

    let mut nodes = Vec::new();
    let mut refs = Vec::new();
    let mod_key = push_module_node(
        &mut nodes,
        pkg_name.clone(),
        collect_module_slash_banner(&lines),
        format!("package {pkg_name}"),
    );

    let import_re = lazy_re!(r"^import\s+(?:static\s+)?([A-Za-z0-9_.*]+)\s*;");
    let class_re = lazy_re!(
        r"^(?:public\s+|private\s+|protected\s+)?(?:abstract\s+|final\s+)?class\s+([A-Za-z_][A-Za-z0-9_]*)(?:\s+extends\s+([A-Za-z0-9_.]+))?(?:\s+implements\s+([A-Za-z0-9_.,\s]+))?"
    );
    let iface_re = lazy_re!(
        r"^(?:public\s+|private\s+|protected\s+)?interface\s+([A-Za-z_][A-Za-z0-9_]*)(?:\s+extends\s+([A-Za-z0-9_.,\s]+))?"
    );
    let enum_re =
        lazy_re!(r"^(?:public\s+|private\s+|protected\s+)?enum\s+([A-Za-z_][A-Za-z0-9_]*)");
    let method_re = lazy_re!(
        r"^(?:public|private|protected)\s+(?:static\s+|final\s+|synchronized\s+)*[A-Za-z0-9_<>\[\].,\s]+\s+([A-Za-z_][A-Za-z0-9_]*)\s*\([^)]*\)"
    );

    let mut pending_test = false;
    let mut class_stack: Vec<(usize, String)> = Vec::new();

    for (idx, raw_line) in lines.iter().enumerate() {
        while class_stack.last().is_some_and(|(end, _)| idx > *end) {
            class_stack.pop();
        }
        let trimmed = raw_line.trim();
        if trimmed == "@Test" || trimmed.starts_with("@Test(") {
            pending_test = true;
            continue;
        }
        if trimmed.starts_with("//") || trimmed.starts_with('*') || trimmed.starts_with("/*") {
            continue;
        }

        if let Some(re) = import_re
            && let Some(caps) = re.captures(trimmed)
        {
            refs.push(ExtractedRef {
                from_local_key: Some(mod_key.clone()),
                edge_type: EDGE_IMPORTS,
                target: caps[1].to_owned(),
                exact_syntax: true,
            });
            continue;
        }

        if let Some(re) = class_re
            && let Some(caps) = re.captures(trimmed)
        {
            let name = caps[1].to_owned();
            let extends = caps
                .get(2)
                .map(|m| vec![m.as_str().trim().to_owned()])
                .unwrap_or_default();
            let implements = caps
                .get(3)
                .map(|m| {
                    m.as_str()
                        .split(',')
                        .map(|s| s.trim().trim_end_matches('{').trim().to_owned())
                        .filter(|s| !s.is_empty())
                        .collect()
                })
                .unwrap_or_default();
            let end = find_brace_block_end(&lines, idx);
            nodes.push(ExtractedNode {
                local_key: format!("sym:{name}"),
                node_type: NODE_SYMBOL,
                label: name.clone(),
                symbol_kind: Some(SymbolKind::Class),
                doc_kind: None,
                parent_local_key: Some(mod_key.clone()),
                signature: trimmed.trim_end_matches('{').trim().to_owned(),
                doc_comment: collect_slash_doc_comment(&lines, idx),
                start_line: idx + 1,
                calls: Vec::new(),
                implements,
                extends,
            });
            class_stack.push((end, name));
            continue;
        }

        if let Some(re) = iface_re
            && let Some(caps) = re.captures(trimmed)
        {
            let name = caps[1].to_owned();
            let extends = caps
                .get(2)
                .map(|m| {
                    m.as_str()
                        .split(',')
                        .map(|s| s.trim().trim_end_matches('{').trim().to_owned())
                        .filter(|s| !s.is_empty())
                        .collect()
                })
                .unwrap_or_default();
            nodes.push(ExtractedNode {
                local_key: format!("sym:{name}"),
                node_type: NODE_SYMBOL,
                label: name,
                symbol_kind: Some(SymbolKind::Interface),
                doc_kind: None,
                parent_local_key: Some(mod_key.clone()),
                signature: trimmed.trim_end_matches('{').trim().to_owned(),
                doc_comment: collect_slash_doc_comment(&lines, idx),
                start_line: idx + 1,
                calls: Vec::new(),
                implements: Vec::new(),
                extends,
            });
            continue;
        }

        if let Some(re) = enum_re
            && let Some(caps) = re.captures(trimmed)
        {
            let name = caps[1].to_owned();
            nodes.push(ExtractedNode {
                local_key: format!("sym:{name}"),
                node_type: NODE_SYMBOL,
                label: name,
                symbol_kind: Some(SymbolKind::Enum),
                doc_kind: None,
                parent_local_key: Some(mod_key.clone()),
                signature: trimmed.trim_end_matches('{').trim().to_owned(),
                doc_comment: collect_slash_doc_comment(&lines, idx),
                start_line: idx + 1,
                calls: Vec::new(),
                implements: Vec::new(),
                extends: Vec::new(),
            });
            continue;
        }

        if let Some((_, owner_cls)) = class_stack.last()
            && let Some(re) = method_re
            && let Some(caps) = re.captures(trimmed)
            && (trimmed.ends_with('{') || trimmed.contains('{'))
        {
            let name = caps[1].to_owned();
            let end = find_brace_block_end(&lines, idx);
            let calls = extract_call_targets(&lines[idx..=end], &name);
            let is_test = pending_test || name.starts_with("test");
            pending_test = false;
            nodes.push(ExtractedNode {
                local_key: if is_test {
                    format!("test:{owner_cls}.{name}")
                } else {
                    format!("sym:{owner_cls}.{name}")
                },
                node_type: if is_test { NODE_TEST } else { NODE_SYMBOL },
                label: name,
                symbol_kind: (!is_test).then_some(SymbolKind::Method),
                doc_kind: None,
                parent_local_key: Some(format!("sym:{owner_cls}")),
                signature: trimmed.trim_end_matches('{').trim().to_owned(),
                doc_comment: collect_slash_doc_comment(&lines, idx),
                start_line: idx + 1,
                calls,
                implements: Vec::new(),
                extends: Vec::new(),
            });
            continue;
        }

        if !trimmed.starts_with('@') {
            pending_test = false;
        }
    }

    ExtractedFileGraph {
        rel_path: rel_path.to_owned(),
        language: Language::Java,
        grammar_version: Language::Java.grammar_version(),
        nodes,
        refs,
    }
}
