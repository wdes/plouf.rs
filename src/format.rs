//! The format abstraction. Each source format is one module (`php`, `js`,
//! `blade`, `html`) exposing a unit struct that implements [`Format`];
//! [`extract`] reads a file and routes it to the first format that matches.
//! Cross-format concerns (translation-key scanning in `lang`, DB schema in
//! `schema`) are not formats.

use std::path::Path;

use crate::model::{Node, RawEdge};

/// A source format plouf can extract into the node/edge model.
pub trait Format {
    /// Does this format handle a file named `base` with lowercased `ext`?
    fn matches(&self, base: &str, ext: &str) -> bool;
    /// Extract already-read `code` (`rel` = repo-relative path, `base` = file
    /// name) into nodes + raw edges.
    fn extract(&self, rel: &str, base: &str, code: &str) -> (Vec<Node>, Vec<RawEdge>);
}

/// Registered formats, in priority order. Blade precedes PHP because a
/// `*.blade.php` file has extension `php` but must not go to Mago.
const FORMATS: &[&dyn Format] =
    &[&crate::blade::Blade, &crate::php::Php, &crate::js::Js, &crate::html::Html];

/// Extensions collected for extraction -- the union across formats, used as the
/// directory-walk filter.
pub const SOURCE_EXTS: [&str; 11] =
    ["php", "ts", "tsx", "mts", "cts", "js", "jsx", "mjs", "cjs", "vue", "html"];

/// Read one file and route it to the first matching format. Returns empty on a
/// read error or when no format matches.
pub fn extract(root: &Path, path: &Path) -> (Vec<Node>, Vec<RawEdge>) {
    let Ok(code) = std::fs::read_to_string(path) else {
        return (Vec::new(), Vec::new());
    };
    let rel = path.strip_prefix(root).unwrap_or(path).to_string_lossy().replace('\\', "/");
    let base = path.file_name().and_then(|n| n.to_str()).unwrap_or(&rel).to_string();
    let ext = path.extension().and_then(|x| x.to_str()).unwrap_or("");

    for format in FORMATS {
        if format.matches(&base, ext) {
            return format.extract(&rel, &base, &code);
        }
    }
    (Vec::new(), Vec::new())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use std::path::Path;

    #[test]
    fn unreadable_file_yields_empty() {
        let (nodes, edges) = super::extract(Path::new("/"), Path::new("/no/such/plouf/file.php"));
        assert!(nodes.is_empty() && edges.is_empty());
    }

    #[test]
    fn no_matching_format_yields_empty() {
        // A `.rs` file is collected by nothing -- dispatch returns empty.
        let dir = std::env::temp_dir();
        let f = dir.join("plouf_fmt_probe.rs");
        std::fs::write(&f, "fn main() {}").unwrap();
        let (nodes, edges) = super::extract(&dir, &f);
        assert!(nodes.is_empty() && edges.is_empty());
        std::fs::remove_file(&f).ok();
    }
}
