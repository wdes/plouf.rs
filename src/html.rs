//! HTML format: e.g. Angular external templates. HTML holds no code symbols, so
//! this emits a single `file` node plus translation-key usages (the
//! `| translate` pipe) from the shared scanner.

use crate::format::Format;
use crate::model::{Node, RawEdge};

/// The HTML format: routes every `*.html`.
pub struct Html;

impl Format for Html {
    fn matches(&self, _base: &str, ext: &str) -> bool {
        ext == "html"
    }

    fn extract(&self, rel: &str, base: &str, code: &str) -> (Vec<Node>, Vec<RawEdge>) {
        extract(rel, base, code)
    }
}

/// A `file` node for the template plus its translation-key + pipe usages.
pub fn extract(rel: &str, base: &str, code: &str) -> (Vec<Node>, Vec<RawEdge>) {
    let node = Node { id: rel.to_string(), name: base.to_string(), kind: "file", path: rel.to_string(), start: 0, end: 0 };
    let mut edges = crate::lang::scan(rel, code);
    scan_pipes(rel, code, &mut edges);
    (vec![node], edges)
}

/// Emit a `uses-pipe` edge for each Angular template pipe use `| pipeName`
/// (`{{ x | myPipe:arg }}`). Candidates: only names a class registered with
/// `@Pipe({ name })` survive resolution, so built-in pipes (`date`, `async`,
/// `json`, ...) drop out without an exclude-list.
fn scan_pipes(rel: &str, code: &str, edges: &mut Vec<RawEdge>) {
    let bytes = code.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        // a single `|` (not `||`)
        if bytes[i] != b'|' || bytes.get(i + 1) == Some(&b'|') || (i > 0 && bytes[i - 1] == b'|') {
            i += 1;
            continue;
        }
        let mut j = i + 1;
        while j < bytes.len() && bytes[j].is_ascii_whitespace() {
            j += 1;
        }
        let start = j;
        while j < bytes.len() && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_') {
            j += 1;
        }
        if j > start {
            edges.push(RawEdge::named(rel.to_string(), "uses-pipe", code[start..j].to_string()));
        }
        i = j.max(i + 1);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::extract;

    #[test]
    fn captures_pipe_usages_and_skips_logical_or() {
        let code = "<p>{{ createdAt | timeAgo }}</p>\n<b>{{ n | number:'1.0-2' }}</b>\n<i *ngIf=\"a || b\">x</i>";
        let (_, edges) = extract("src/app/x.html", "x.html", code);
        assert!(edges.iter().any(|e| e.relation == "uses-pipe" && e.name.as_deref() == Some("timeAgo")));
        assert!(edges.iter().any(|e| e.relation == "uses-pipe" && e.name.as_deref() == Some("number")));
        assert!(!edges.iter().any(|e| e.relation == "uses-pipe" && e.name.as_deref() == Some("b")));
    }
}
