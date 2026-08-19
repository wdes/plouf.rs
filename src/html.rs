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

/// A `file` node for the template plus its translation-key usages.
pub fn extract(rel: &str, base: &str, code: &str) -> (Vec<Node>, Vec<RawEdge>) {
    let node = Node { id: rel.to_string(), name: base.to_string(), kind: "file", path: rel.to_string(), start: 0, end: 0 };
    (vec![node], crate::lang::scan(rel, code))
}
