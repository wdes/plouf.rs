//! `.gitattributes` `export-ignore` mapping.
//!
//! `git archive` (the GitHub "source tarball") drops every path marked
//! `export-ignore`, but that list rots as files are renamed or removed and
//! nobody updates the attribute file. Scanning `.gitattributes` links each
//! `export-ignore` pattern to the file or folder it currently names, so
//! `callers <path>` shows what ignores it and `missing` reports a pattern whose
//! target no longer exists -- a stale entry that silently ships junk in (or
//! omits wanted files from) the tarball.

use std::path::Path;

use ignore::WalkBuilder;

use crate::model::{Node, RawEdge};

/// Scan every `.gitattributes` under `root` for `export-ignore` patterns.
///
/// For each pattern we emit an `export-ignores` edge from the `.gitattributes`
/// file to the path it names; when that path exists on disk we also emit a
/// lightweight `path` node so the edge resolves (and `callers` can list it). A
/// pattern whose target is gone emits only the edge -- it stays unresolved, and
/// `missing` surfaces it as a stale entry.
pub fn scan(root: &Path) -> (Vec<Node>, Vec<RawEdge>) {
    let mut nodes = Vec::new();
    let mut edges = Vec::new();
    for result in WalkBuilder::new(root).hidden(false).parents(true).build() {
        let Ok(entry) = result else { continue };
        let path = entry.path();
        if !path.is_file() || path.file_name().and_then(|n| n.to_str()) != Some(".gitattributes") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(path) else { continue };
        let rel = path.strip_prefix(root).unwrap_or(path).to_string_lossy().replace('\\', "/");
        // Patterns are relative to the `.gitattributes` file's own directory.
        let dir = rel.rsplit_once('/').map_or("", |(d, _)| d);
        scan_file(root, &rel, dir, &text, &mut nodes, &mut edges);
    }
    (nodes, edges)
}

/// Parse one `.gitattributes` file's `text`, appending nodes/edges for every
/// line that sets the `export-ignore` attribute.
fn scan_file(root: &Path, source: &str, dir: &str, text: &str, nodes: &mut Vec<Node>, edges: &mut Vec<RawEdge>) {
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut tokens = line.split_whitespace();
        let Some(pattern) = tokens.next() else { continue };
        // Set only: `export-ignore`. `-export-ignore` (unset) and a macro
        // definition are not a mapping.
        if !tokens.any(|a| a == "export-ignore") {
            continue;
        }
        let Some(rel) = pattern_path(dir, pattern) else { continue };
        // A glob names no single path, so its existence can't be checked: record
        // the mapping (with a node, never flagged stale) but do not touch disk.
        let is_glob = pattern.contains(['*', '?', '[']);
        if is_glob || root.join(&rel).exists() {
            let name = rel.rsplit_once('/').map_or(rel.as_str(), |(_, b)| b).to_string();
            nodes.push(Node { id: rel.clone(), name, kind: "path", path: rel.clone(), start: 0, end: 0 });
        }
        edges.push(RawEdge::named(source.to_string(), "export-ignores", rel));
    }
}

/// Resolve a `.gitattributes` pattern to a repo-relative path: strip surrounding
/// quotes, the leading `/` (anchor) and trailing `/` (directory marker), then
/// prefix the `.gitattributes` file's own directory. Returns `None` for an
/// empty, negated (`!`), or up-tree (`..`) pattern -- none names a path we map.
fn pattern_path(dir: &str, pattern: &str) -> Option<String> {
    if pattern.starts_with('!') {
        return None;
    }
    let unquoted = pattern.trim_matches('"').replace('\\', "/");
    let trimmed = unquoted.trim_start_matches('/').trim_end_matches('/');
    if trimmed.is_empty() || trimmed.starts_with("..") {
        return None;
    }
    Some(if dir.is_empty() { trimmed.to_string() } else { format!("{dir}/{trimmed}") })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::scan;
    use std::fs;
    use std::path::PathBuf;

    /// A fresh, empty temp dir (removing any leftover from a prior run).
    fn tmp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(name);
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn maps_existing_paths_and_flags_stale() {
        let root = tmp("plouf_ga_basic");
        fs::create_dir_all(root.join(".github")).unwrap();
        fs::write(root.join(".github/x.yml"), "").unwrap();
        fs::write(root.join("keep.txt"), "").unwrap();
        fs::write(
            root.join(".gitattributes"),
            "# a comment\n\n/.github/ export-ignore\nkeep.txt export-ignore\ngone.txt export-ignore\nnormal.txt text\n",
        )
        .unwrap();

        let (nodes, edges) = scan(&root);

        // One export-ignore edge per set line, all from the .gitattributes file.
        let mapped: Vec<&str> =
            edges.iter().filter(|e| e.relation == "export-ignores").filter_map(|e| e.name.as_deref()).collect();
        assert_eq!(mapped.len(), 3);
        assert!(edges.iter().filter(|e| e.relation == "export-ignores").all(|e| e.source == ".gitattributes"));
        assert!(mapped.contains(&".github") && mapped.contains(&"keep.txt") && mapped.contains(&"gone.txt"));

        // A node only for the paths that exist (the stale one stays unresolved).
        let ids: Vec<&str> = nodes.iter().map(|n| n.id.as_str()).collect();
        assert!(ids.contains(&".github") && ids.contains(&"keep.txt"));
        assert!(!ids.contains(&"gone.txt"));
        assert!(nodes.iter().all(|n| n.kind == "path"));

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn honours_unset_glob_and_nested_dir() {
        let root = tmp("plouf_ga_nested");
        fs::create_dir_all(root.join("sub")).unwrap();
        fs::write(root.join("sub/a.php"), "").unwrap();
        // Nested file: its pattern is relative to `sub/`.
        fs::write(root.join("sub/.gitattributes"), "a.php export-ignore\n").unwrap();
        // Root: a glob (recorded, never existence-checked) and an unset line.
        fs::write(root.join(".gitattributes"), "*.dist export-ignore\nfoo -export-ignore\n").unwrap();

        let (nodes, edges) = scan(&root);

        let mapped: Vec<&str> =
            edges.iter().filter(|e| e.relation == "export-ignores").filter_map(|e| e.name.as_deref()).collect();
        assert!(mapped.contains(&"sub/a.php")); // prefixed with the nested dir
        assert!(mapped.contains(&"*.dist")); // glob kept
        assert!(!mapped.contains(&"foo")); // unset line dropped

        let ids: Vec<&str> = nodes.iter().map(|n| n.id.as_str()).collect();
        assert!(ids.contains(&"sub/a.php")); // existing file -> node
        assert!(ids.contains(&"*.dist")); // glob -> node (assumed to match)
    }

    #[test]
    fn rejects_negated_uptree_and_empty_patterns() {
        let root = tmp("plouf_ga_reject");
        fs::write(root.join(".gitattributes"), "!keep export-ignore\n../escape export-ignore\n/ export-ignore\n")
            .unwrap();
        let (nodes, edges) = scan(&root);
        assert!(edges.is_empty() && nodes.is_empty());
        fs::remove_dir_all(&root).ok();
    }
}
