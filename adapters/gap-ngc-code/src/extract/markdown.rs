//! Markdown documentation, heading hierarchy, concept, and symbol-link extractor (`ING-060`, `ING-061`, `ING-063`).

use std::collections::BTreeSet;
use std::sync::OnceLock;

use regex::Regex;

use super::{ExtractedFileGraph, ExtractedNode, ExtractedRef, slugify};
use crate::model::{
    DocKind, EDGE_DOCUMENTS, EDGE_REFERENCES, EDGE_SUPERSEDES, Language, NODE_CONCEPT, NODE_DOC,
};

macro_rules! lazy_re {
    ($pat:literal) => {{
        static RE: OnceLock<Option<Regex>> = OnceLock::new();
        RE.get_or_init(|| Regex::new($pat).ok()).as_ref()
    }};
}

fn classify_doc_kind(rel_path: &str) -> DocKind {
    let lower = rel_path.to_ascii_lowercase();
    if lower.contains("readme") {
        DocKind::Readme
    } else if lower.contains("adr") {
        DocKind::Adr
    } else if lower.contains("api") || lower.contains("spec") {
        DocKind::Api
    } else {
        DocKind::MarkdownSection
    }
}

/// Extracts top-level document nodes, nested `#` headings (`CONTAINS`), domain `Concept`s for ADRs,
/// inline code identifier mentions (`DOCUMENTS`), and relative markdown links (`REFERENCES`) (`ING-060`).
pub(crate) fn extract_markdown(rel_path: &str, src: &str) -> ExtractedFileGraph {
    let lines: Vec<&str> = src.lines().collect();
    let doc_kind = classify_doc_kind(rel_path);

    let first_heading = lines.iter().find_map(|l| {
        let t = l.trim();
        t.strip_prefix("# ").map(str::trim)
    });
    let first_prose = lines.iter().find_map(|l| {
        let t = l.trim();
        (!t.is_empty() && !t.starts_with('#') && !t.starts_with("```")).then_some(t)
    });

    let root_label = first_heading.map_or_else(|| rel_path.to_owned(), ToOwned::to_owned);
    let root_key = String::from("doc:root");
    let mut nodes = vec![ExtractedNode {
        local_key: root_key.clone(),
        node_type: NODE_DOC,
        label: root_label.clone(),
        symbol_kind: None,
        doc_kind: Some(doc_kind),
        parent_local_key: None,
        signature: root_label.clone(),
        doc_comment: first_prose.map(ToOwned::to_owned),
        start_line: 1,
        calls: Vec::new(),
        implements: Vec::new(),
        extends: Vec::new(),
    }];

    let mut refs = Vec::new();
    if matches!(doc_kind, DocKind::Adr) {
        let concept_key = format!("concept:{}", slugify(&root_label));
        nodes.push(ExtractedNode {
            local_key: concept_key.clone(),
            node_type: NODE_CONCEPT,
            label: root_label.clone(),
            symbol_kind: None,
            doc_kind: None,
            parent_local_key: Some(root_key.clone()),
            signature: format!("Concept: {root_label}"),
            doc_comment: first_prose.map(ToOwned::to_owned),
            start_line: 1,
            calls: Vec::new(),
            implements: Vec::new(),
            extends: Vec::new(),
        });
        refs.push(ExtractedRef {
            from_local_key: Some(root_key.clone()),
            edge_type: EDGE_DOCUMENTS,
            target: concept_key,
            exact_syntax: true,
        });
    }

    let heading_re = lazy_re!(r"^(#{1,6})\s+(.+)$");
    let code_re = lazy_re!(r"`([A-Za-z_][A-Za-z0-9_.:]*)`");
    let link_re = lazy_re!(r"\[[^\]]+\]\(([^)#\s][^)#]*)\)");

    let mut in_fence = false;
    let mut section_stack: Vec<(usize, String)> = Vec::new();
    let mut seen_code_refs = BTreeSet::new();
    let mut seen_link_refs = BTreeSet::new();

    for (idx, raw_line) in lines.iter().enumerate() {
        let trimmed = raw_line.trim();
        if trimmed.starts_with("```") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            continue;
        }

        let active_key = section_stack
            .last()
            .map_or_else(|| root_key.clone(), |(_, k)| k.clone());

        if let Some(re) = heading_re
            && let Some(caps) = re.captures(trimmed)
        {
            let level = caps[1].len();
            let title = caps[2].trim().trim_end_matches('#').trim().to_owned();
            while section_stack.last().is_some_and(|(lvl, _)| *lvl >= level) {
                section_stack.pop();
            }
            if level == 1 && section_stack.is_empty() && first_heading == Some(title.as_str()) {
                section_stack.push((1, root_key.clone()));
                continue;
            }
            let parent_key = section_stack
                .last()
                .map_or_else(|| root_key.clone(), |(_, k)| k.clone());
            let sec_key = format!("doc:sec:{}:{}", idx + 1, slugify(&title));
            let body_prose = lines.iter().skip(idx + 1).find_map(|l| {
                let p = l.trim();
                (!p.is_empty() && !p.starts_with('#') && !p.starts_with("```")).then_some(p)
            });
            nodes.push(ExtractedNode {
                local_key: sec_key.clone(),
                node_type: NODE_DOC,
                label: title.clone(),
                symbol_kind: None,
                doc_kind: Some(DocKind::MarkdownSection),
                parent_local_key: Some(parent_key),
                signature: title,
                doc_comment: body_prose.map(ToOwned::to_owned),
                start_line: idx + 1,
                calls: Vec::new(),
                implements: Vec::new(),
                extends: Vec::new(),
            });
            section_stack.push((level, sec_key));
            continue;
        }

        if let Some(re) = code_re {
            for caps in re.captures_iter(trimmed) {
                let sym = caps[1].to_owned();
                if seen_code_refs.insert((active_key.clone(), sym.clone())) {
                    refs.push(ExtractedRef {
                        from_local_key: Some(active_key.clone()),
                        edge_type: EDGE_DOCUMENTS,
                        target: sym,
                        exact_syntax: false,
                    });
                }
            }
        }

        if trimmed.len() > 11
            && trimmed
                .get(..11)
                .is_some_and(|prefix| prefix.eq_ignore_ascii_case("supersedes:"))
        {
            let rest = trimmed[11..].trim();
            let target = if let Some(start) = rest.find("](")
                && let Some(end) = rest[start + 2..].find(')')
            {
                rest[start + 2..start + 2 + end].trim()
            } else {
                rest.trim_matches('`')
            };
            if !target.is_empty() {
                refs.push(ExtractedRef {
                    from_local_key: Some(root_key.clone()),
                    edge_type: EDGE_SUPERSEDES,
                    target: target.to_owned(),
                    exact_syntax: true,
                });
            }
        }

        if let Some(re) = link_re {
            for caps in re.captures_iter(trimmed) {
                let target = caps[1].trim().to_owned();
                if !target.starts_with("http://")
                    && !target.starts_with("https://")
                    && seen_link_refs.insert((active_key.clone(), target.clone()))
                {
                    refs.push(ExtractedRef {
                        from_local_key: Some(active_key.clone()),
                        edge_type: EDGE_REFERENCES,
                        target,
                        exact_syntax: true,
                    });
                }
            }
        }
    }

    ExtractedFileGraph {
        rel_path: rel_path.to_owned(),
        language: Language::Markdown,
        grammar_version: Language::Markdown.grammar_version(),
        nodes,
        refs,
    }
}
