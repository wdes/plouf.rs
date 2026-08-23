//! `phpcs.xml` / `phpstan.neon` -> the "standard files" they activate.
//!
//! A linter config is an entry point: it turns on sniff/rule classes that no
//! application code ever calls, so plouf's `missing` flags them as dead. Scanning
//! the config links it to each in-repo file/class it references via `configures`
//! edges, so `callers <Sniff>` shows the config that enables it and the class
//! stops reading as unreferenced.

use std::path::Path;

use ignore::WalkBuilder;

use crate::model::RawEdge;
use crate::php::dequalify;

/// Scan every `phpcs.xml`(`.dist`) / `phpstan.neon`(`.dist`) under `root`,
/// emitting `configures` edges from the config file to the sniff files
/// (phpcs `<rule ref="./X.php">`) and rule/service classes (phpstan
/// `- Vendor\...\FooRule`) it activates.
pub fn scan(root: &Path) -> Vec<RawEdge> {
    let mut edges = Vec::new();
    for result in WalkBuilder::new(root).hidden(false).parents(true).build() {
        let Ok(entry) = result else { continue };
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else { continue };
        let is_phpcs = name == "phpcs.xml" || name == "phpcs.xml.dist";
        let is_phpstan = name == "phpstan.neon" || name == "phpstan.neon.dist";
        if !is_phpcs && !is_phpstan {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(path) else { continue };
        let rel = path.strip_prefix(root).unwrap_or(path).to_string_lossy().replace('\\', "/");
        let dir = rel.rsplit_once('/').map_or("", |(d, _)| d);
        if is_phpcs {
            scan_phpcs(&rel, dir, &text, &mut edges);
        } else {
            scan_phpstan(&rel, dir, &text, &mut edges);
        }
    }
    edges
}

/// phpcs `<rule ref="./path/to/Sniff.php"/>`: a `.php` ref is an in-repo sniff
/// file -- resolve it against the config's dir and emit a `configures` edge. A
/// dotted `Generic.Files.LineLength` ref is an external standard, and is skipped.
fn scan_phpcs(source: &str, dir: &str, text: &str, edges: &mut Vec<RawEdge>) {
    for value in ref_values(text) {
        if Path::new(&value).extension().is_some_and(|e| e.eq_ignore_ascii_case("php")) {
            edges.push(RawEdge::named(source.to_string(), "configures", join_relative(dir, &value)));
        }
    }
}

/// phpstan neon list items: a `- Vendor\...\FooRule` FQCN (a `rules:` or
/// `services:` entry) links to its class by bare name; a `- x.neon` include
/// links to that neon file. Indentation is irrelevant to the linkage, so a flat
/// line scan suffices.
fn scan_phpstan(source: &str, dir: &str, text: &str, edges: &mut Vec<RawEdge>) {
    for line in text.lines() {
        let Some(item) = line.trim().strip_prefix("- ") else { continue };
        let value = item.trim().trim_matches(|c| c == '"' || c == '\'');
        if value.contains('\\') {
            edges.push(RawEdge::named(source.to_string(), "configures", dequalify(value)));
        } else if Path::new(value).extension().is_some_and(|e| e.eq_ignore_ascii_case("neon")) {
            edges.push(RawEdge::named(source.to_string(), "configures", join_relative(dir, value)));
        }
    }
}

/// Every `ref="..."` / `ref='...'` attribute value in an XML text.
fn ref_values(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(pos) = text[from..].find("ref=") {
        let at = from + pos + "ref=".len();
        from = at;
        let rest = &text[at..];
        let Some(q) = rest.chars().next() else { continue };
        if q != '"' && q != '\'' {
            continue;
        }
        let body = &rest[1..];
        if let Some(end) = body.find(q) {
            out.push(body[..end].to_string());
            from = at + 1 + end;
        }
    }
    out
}

/// Resolve a `./x`/`../x` config-relative path against the config file's dir.
fn join_relative(dir: &str, spec: &str) -> String {
    let mut parts: Vec<&str> = if dir.is_empty() { Vec::new() } else { dir.split('/').collect() };
    for seg in spec.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            other => parts.push(other),
        }
    }
    parts.join("/")
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::{scan_phpcs, scan_phpstan};
    use crate::model::RawEdge;

    fn targets(edges: &[RawEdge]) -> Vec<String> {
        edges.iter().filter(|e| e.relation == "configures").filter_map(|e| e.name.clone()).collect()
    }

    #[test]
    fn phpcs_links_sniff_files_and_skips_external() {
        let xml = "<ruleset>\n  <rule ref=\"Generic.Files.LineLength\"/>\n  <rule ref=\"./coding-standard/Sniffs/Foo/BarSniff.php\"/>\n</ruleset>";
        let mut edges = Vec::new();
        scan_phpcs("phpcs.xml", "", xml, &mut edges);
        let t = targets(&edges);
        assert_eq!(t, vec!["coding-standard/Sniffs/Foo/BarSniff.php"]); // external dotted ref skipped
    }

    #[test]
    fn scan_walks_and_links_both_config_kinds() {
        use std::fs;
        let root = std::env::temp_dir().join("plouf_config_scan");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("standard/Sniffs")).unwrap();
        fs::write(root.join("standard/Sniffs/FooSniff.php"), "<?php class FooSniff {}").unwrap();
        fs::write(root.join("phpcs.xml"), "<ruleset><rule ref=\"./standard/Sniffs/FooSniff.php\"/><rule ref=\"Generic.X\"/></ruleset>").unwrap();
        fs::write(root.join("phpstan.neon"), "rules:\n    - App\\Rules\\BarRule\n").unwrap();
        let edges = super::scan(&root);
        let t = targets(&edges);
        assert!(t.contains(&"standard/Sniffs/FooSniff.php".to_string()));
        assert!(t.contains(&"BarRule".to_string()));
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn join_relative_and_ref_values_edge_cases() {
        assert_eq!(super::join_relative("a/b", "../c.php"), "a/c.php"); // `..` pops a dir
        assert_eq!(super::join_relative("", "./x.php"), "x.php"); // root config
        // single- and double-quoted refs captured; a bare (unquoted) ref= skipped.
        let refs = super::ref_values("<rule ref='a.php'/><rule ref=\"b.php\"/><x ref=nope/>");
        assert_eq!(refs, vec!["a.php", "b.php"]);
    }

    #[test]
    fn phpstan_links_rule_classes_and_neon_includes() {
        let neon = "includes:\n    - phpstan-baseline.neon\nrules:\n    - Acme\\PHPStan\\Rules\\NoDynamicPropertyRule\n    - 'Acme\\PHPStan\\Rules\\OpenApiSyncRule'";
        let mut edges = Vec::new();
        scan_phpstan("phpstan.neon", "", neon, &mut edges);
        let t = targets(&edges);
        assert!(t.contains(&"NoDynamicPropertyRule".to_string())); // FQCN -> bare class name
        assert!(t.contains(&"OpenApiSyncRule".to_string())); // quoted FQCN too
        assert!(t.contains(&"phpstan-baseline.neon".to_string())); // include -> the neon file
    }
}
