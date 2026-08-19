//! Twig template extraction (`*.twig`). Hand-scanned like Blade: template tags
//! (`{% extends %}` / `{% include %}` / `{% embed %}` / `{% import %}` /
//! `{% from %}` / `{% use %}`) emit `includes` edges to the referenced template,
//! and translations (`{% trans %}...{% endtrans %}` blocks and the `|trans`
//! filter) emit `uses-lang`. Whitespace-control markers (`{%- ... -%}`) are
//! tolerated.

use crate::format::Format;
use crate::model::{Node, RawEdge};

/// The Twig format: routes every `*.twig`.
pub struct Twig;

impl Format for Twig {
    fn matches(&self, _base: &str, ext: &str) -> bool {
        ext == "twig"
    }

    fn extract(&self, rel: &str, base: &str, code: &str) -> (Vec<Node>, Vec<RawEdge>) {
        extract(rel, base, code)
    }
}

/// A `file` node plus `includes` / `uses-lang` edges for a Twig template.
pub fn extract(rel: &str, base: &str, code: &str) -> (Vec<Node>, Vec<RawEdge>) {
    let nodes = vec![Node { id: rel.to_string(), name: base.to_string(), kind: "file", path: rel.to_string(), start: 0, end: 0 }];
    let mut edges = Vec::new();
    scan_tags(code, rel, &mut edges);
    scan_trans_blocks(code, rel, &mut edges);
    // The `|trans` filter (`'key'|trans`) is caught by the shared scanner.
    edges.extend(crate::lang::scan(rel, code));
    (nodes, edges)
}

/// Emit an `includes` edge for each template-reference tag's first quoted name.
fn scan_tags(code: &str, rel: &str, edges: &mut Vec<RawEdge>) {
    for (idx, _) in code.match_indices("{%") {
        let after = &code[idx + 2..];
        let Some(close) = after.find("%}") else { continue };
        let tag = after[..close].trim().trim_start_matches('-').trim();
        let keyword = tag.split_whitespace().next().unwrap_or("");
        if matches!(keyword, "extends" | "include" | "embed" | "import" | "from" | "use") {
            if let Some(name) = first_string(tag) {
                edges.push(RawEdge::named(rel.to_string(), "includes", name));
            }
        }
    }
}

/// Emit a `uses-lang` edge for each `{% trans %}...{% endtrans %}` block, keyed
/// by the (trimmed) inner text -- the gettext msgid.
fn scan_trans_blocks(code: &str, rel: &str, edges: &mut Vec<RawEdge>) {
    let mut from = 0;
    while let Some(rel_open) = code[from..].find("{%") {
        let open = from + rel_open;
        let Some(close_rel) = code[open + 2..].find("%}") else { break };
        let close = open + 2 + close_rel;
        let keyword = code[open + 2..close].trim_start().trim_start_matches(['-', ' ']).split_whitespace().next().unwrap_or("");
        if keyword != "trans" {
            from = close + 2;
            continue;
        }
        let body_start = close + 2;
        let Some(end_rel) = code[body_start..].find("endtrans") else { break };
        let end_abs = body_start + end_rel;
        // The msgid ends just before the `{%` (or `{%-`) that opens `endtrans`.
        let tag_open = code[body_start..end_abs].rfind("{%").map_or(end_abs, |p| body_start + p);
        let text = code[body_start..tag_open].trim();
        if !text.is_empty() {
            edges.push(RawEdge::named(rel.to_string(), "uses-lang", text.to_string()));
        }
        from = end_abs + "endtrans".len();
    }
}

/// The first single/double-quoted string literal in `s`.
fn first_string(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let q = bytes[i];
        if q == b'\'' || q == b'"' {
            let mut j = i + 1;
            while j < bytes.len() && bytes[j] != q {
                if bytes[j] == b'\\' {
                    j += 1;
                }
                j += 1;
            }
            return Some(String::from_utf8_lossy(&bytes[i + 1..j.min(bytes.len())]).into_owned());
        }
        i += 1;
    }
    None
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::extract;
    use crate::model::RawEdge;

    fn has(edges: &[RawEdge], relation: &str, name: &str) -> bool {
        edges.iter().any(|e| e.relation == relation && e.name.as_deref() == Some(name))
    }

    #[test]
    fn captures_includes_extends_and_translations() {
        let code = "{% extends 'layout.twig' %}\n{%- include 'database/structure/row' with {'x': y} only -%}\n{% import 'macros.twig' as m %}\n<p>{% trans %}Hello world{% endtrans %}</p>\n<span>{{ 'Save'|trans }}</span>\n{%- trans -%}Spaced{%- endtrans -%}";
        let (nodes, edges) = extract("templates/table.twig", "table.twig", code);
        assert_eq!(nodes.len(), 1);
        assert!(has(&edges, "includes", "layout.twig"));
        assert!(has(&edges, "includes", "database/structure/row"));
        assert!(has(&edges, "includes", "macros.twig"));
        assert!(has(&edges, "uses-lang", "Hello world")); // {% trans %} block
        assert!(has(&edges, "uses-lang", "Save")); // |trans filter (via lang::scan)
        assert!(has(&edges, "uses-lang", "Spaced")); // whitespace-control block
    }

    #[test]
    fn ignores_non_reference_tags() {
        let code = "{% if x %}{% set a = 'b' %}{% endif %}\n{{ value }}";
        let (_, edges) = extract("t.twig", "t.twig", code);
        assert!(edges.is_empty());
    }
}
