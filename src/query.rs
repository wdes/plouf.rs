//! Query layer over a built `wiring.json`: word search, signature/body slicing
//! (via the byte spans stored on each node), and a gaps report. Paths in the
//! graph are relative to the indexed root, so run queries from that same root.

use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::Path;

use serde_json::Value;

/// A node as read back from `wiring.json`.
struct NodeRec {
    id: String,
    name: String,
    kind: String,
    path: String,
    start: usize,
    end: usize,
}

/// An edge as read back from `wiring.json`.
struct EdgeRec {
    source: String,
    target: String,
    relation: String,
}

/// The loaded graph.
struct Graph {
    nodes: Vec<NodeRec>,
    edges: Vec<EdgeRec>,
}

fn node_rec(v: &Value) -> Option<NodeRec> {
    Some(NodeRec {
        id: v["id"].as_str()?.to_string(),
        name: v["name"].as_str()?.to_string(),
        kind: v["kind"].as_str()?.to_string(),
        path: v["path"].as_str()?.to_string(),
        start: usize::try_from(v["start"].as_u64().unwrap_or(0)).unwrap_or(0),
        end: usize::try_from(v["end"].as_u64().unwrap_or(0)).unwrap_or(0),
    })
}

fn edge_rec(v: &Value) -> Option<EdgeRec> {
    Some(EdgeRec {
        source: v["source"].as_str()?.to_string(),
        target: v["target"].as_str()?.to_string(),
        relation: v["relation"].as_str()?.to_string(),
    })
}

fn load(out: &str) -> Result<Graph, io::Error> {
    let path = format!("{out}/.graph/wiring.json");
    let text = fs::read_to_string(&path)?;
    let v: Value = serde_json::from_str(&text).map_err(io::Error::other)?;
    let nodes = v["nodes"].as_array().map(|a| a.iter().filter_map(node_rec).collect()).unwrap_or_default();
    let edges = v["edges"].as_array().map(|a| a.iter().filter_map(edge_rec).collect()).unwrap_or_default();
    Ok(Graph { nodes, edges })
}

/// Resolve a symbol string to one node: exact id, else a unique `#name` /
/// `.name` / bare-name match. Ambiguous or missing is an error.
fn resolve<'a>(graph: &'a Graph, symbol: &str) -> Result<&'a NodeRec, io::Error> {
    if let Some(n) = graph.nodes.iter().find(|n| n.id == symbol) {
        return Ok(n);
    }
    let hash = format!("#{symbol}");
    let dot = format!(".{symbol}");
    let matches: Vec<&NodeRec> = graph
        .nodes
        .iter()
        .filter(|n| n.kind != "file")
        .filter(|n| n.name == symbol || n.id.ends_with(&hash) || n.id.ends_with(&dot))
        .collect();
    match matches.as_slice() {
        [one] => Ok(one),
        [] => Err(io::Error::new(io::ErrorKind::NotFound, format!("no symbol matches '{symbol}'"))),
        many => {
            let list = many.iter().map(|n| n.id.as_str()).collect::<Vec<_>>().join("\n  ");
            Err(io::Error::new(io::ErrorKind::InvalidInput, format!("ambiguous '{symbol}':\n  {list}")))
        }
    }
}

/// The source text a node spans (the whole file for `file` nodes; the extracted
/// `<script>` for `.vue`, matching how spans were recorded).
fn source_slice(node: &NodeRec) -> Result<String, io::Error> {
    let code = fs::read_to_string(&node.path)?;
    if node.kind == "file" {
        return Ok(code);
    }
    let is_vue = Path::new(&node.path).extension().is_some_and(|e| e.eq_ignore_ascii_case("vue"));
    let text = if is_vue { crate::js::vue_script(&code) } else { code };
    let bytes = text.as_bytes();
    let start = node.start.min(bytes.len());
    let end = node.end.max(node.start).min(bytes.len());
    Ok(String::from_utf8_lossy(&bytes[start..end]).into_owned())
}

/// Print one line to stdout, exiting cleanly if the reader has gone away -- a
/// `plouf-rs find ... | head` closes the pipe, and a broken-pipe write must not
/// panic the whole query (Rust ignores SIGPIPE, so the write errors instead).
pub fn emit(args: std::fmt::Arguments) {
    use std::io::Write;
    if writeln!(std::io::stdout(), "{args}").is_err() {
        std::process::exit(0);
    }
}

/// List symbols whose id or name contains `term` (case-insensitive).
pub fn find(out: &str, term: &str) -> Result<(), io::Error> {
    let graph = load(out)?;
    let needle = term.to_lowercase();
    let mut hits: Vec<&NodeRec> = graph
        .nodes
        .iter()
        .filter(|n| n.kind != "file")
        .filter(|n| n.id.to_lowercase().contains(&needle) || n.name.to_lowercase().contains(&needle))
        .collect();
    hits.sort_by(|a, b| a.id.cmp(&b.id));
    for n in hits {
        emit(format_args!("{}\t{}", n.kind, n.id));
    }
    Ok(())
}

/// Print a symbol's declaration line (source up to its opening `{` or `;`).
pub fn signature(out: &str, symbol: &str) -> Result<(), io::Error> {
    let graph = load(out)?;
    let node = resolve(&graph, symbol)?;
    // Whole-unit kinds have no declaration line to slice; name them instead.
    if matches!(node.kind.as_str(), "component" | "file") {
        emit(format_args!("{} {}", node.kind, node.name));
        return Ok(());
    }
    let body = source_slice(node)?;
    let cut = body.find('{').or_else(|| body.find(';')).unwrap_or(body.len());
    emit(format_args!("{}", body[..cut].trim()));
    Ok(())
}

/// Print a symbol's full source body.
pub fn body(out: &str, symbol: &str) -> Result<(), io::Error> {
    let graph = load(out)?;
    let node = resolve(&graph, symbol)?;
    emit(format_args!("{}", source_slice(node)?));
    Ok(())
}

/// A "reference" edge for `callers`/`missing`: a call, import, heritage, Blade
/// include, Eloquent relation, or a model/migration table link.
fn is_reference(relation: &str) -> bool {
    matches!(
        relation,
        "calls"
            | "imports"
            | "extends"
            | "implements"
            | "includes"
            | "covers"
            | "table"
            | "migrates"
            | "uses-table"
            | "visits"
            | "renders"
            | "requires"
            | "defines-fn"
            | "uses-fn"
            | "defines-pipe"
            | "uses-pipe"
            | "routes-to"
            | "serves"
            | "export-ignores"
            | "configures"
            | "checks-permission"
            | "raises-trigger"
            | "handles-trigger"
            | "fires-hook"
            | "handles-hook"
            | "declares-module"
            | "relates-to"
            | "depends-on"
            | "requires-role"
            | "dol-requires"
    ) || crate::laravel::relation_kind(relation).is_some()
}

/// List what references a symbol: every reference edge that targets it, as
/// `relation<TAB>source` (the blast radius).
pub fn callers(out: &str, symbol: &str) -> Result<(), io::Error> {
    let graph = load(out)?;
    let node = resolve(&graph, symbol)?;
    let mut hits: Vec<String> = graph
        .edges
        .iter()
        .filter(|e| e.target == node.id)
        .filter(|e| is_reference(&e.relation))
        .map(|e| format!("{}\t{}", e.relation, e.source))
        .collect();
    hits.sort();
    hits.dedup();
    for h in hits {
        emit(format_args!("{h}"));
    }
    Ok(())
}

/// List files that use a translation `key`, from the `.graph/lang.json` sidecar
/// (kept out of `wiring.json` because there can be thousands). Prints
/// `key<TAB>file` per usage. Tries an exact key first; if none, falls back to a
/// case-insensitive substring match over every key (discovery).
pub fn uses(out: &str, key: &str) -> Result<(), io::Error> {
    let path = format!("{out}/.graph/lang.json");
    let text = fs::read_to_string(&path)?;
    let v: Value = serde_json::from_str(&text).map_err(io::Error::other)?;
    let obj = v.as_object().ok_or_else(|| io::Error::other("lang.json: not an object"))?;

    let files_of = |k: &str| -> Vec<String> {
        obj.get(k)
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(|f| f.as_str().map(str::to_string)).collect())
            .unwrap_or_default()
    };

    let mut hits: Vec<String> = files_of(key).into_iter().map(|f| format!("{key}\t{f}")).collect();
    if hits.is_empty() {
        let needle = key.to_lowercase();
        for (k, v) in obj {
            if k.to_lowercase().contains(&needle) {
                if let Some(arr) = v.as_array() {
                    for f in arr.iter().filter_map(Value::as_str) {
                        hits.push(format!("{k}\t{f}"));
                    }
                }
            }
        }
    }
    hits.sort();
    hits.dedup();
    for h in hits {
        emit(format_args!("{h}"));
    }
    Ok(())
}

/// File extensions whose nodes legitimately hold no code symbols, so an empty
/// one is not a gap: templates and non-code assets.
const NO_SYMBOL_EXTS: [&str; 6] = ["html", "json", "css", "scss", "sass", "svg"];

/// Report graph gaps: symbols nothing references, edges that never resolved to a
/// node, and files that parsed to nothing.
pub fn missing(out: &str) -> Result<(), io::Error> {
    let graph = load(out)?;
    let ids: HashSet<&str> = graph.nodes.iter().map(|n| n.id.as_str()).collect();

    let mut referenced: HashSet<&str> = HashSet::new();
    for e in &graph.edges {
        if e.relation != "contains" && ids.contains(e.target.as_str()) {
            referenced.insert(e.target.as_str());
        }
    }

    let unreferenced: Vec<&NodeRec> = graph
        .nodes
        .iter()
        // `route:` join nodes point OUT to their controller (`serves`); nothing
        // targets them unless an e2e/router does, so they are not "unreferenced".
        // The Dolibarr join nodes (module/permission/trigger/hook) are the same:
        // fan-in/out hubs, not symbols that should read as dead.
        .filter(|n| {
            !matches!(
                n.kind.as_str(),
                "file" | "component" | "route" | "module" | "permission" | "trigger" | "hook" | "role"
            )
        })
        .filter(|n| !referenced.contains(n.id.as_str()))
        .collect();

    let mut unresolved: Vec<&EdgeRec> = graph
        .edges
        .iter()
        .filter(|e| is_reference(&e.relation))
        .filter(|e| !ids.contains(e.target.as_str()))
        // Drop references that point OUTSIDE the repo -- expected, not gaps:
        //  - an external import: a non-relative `vue` / `Illuminate\...` /
        //    built-in specifier (a relative `./x` that misses IS a real gap);
        //  - heritage (`extends`/`implements`) to a base not in the repo, which
        //    is necessarily a framework/vendor base since PHP needs the parent
        //    to exist.
        // What survives is genuinely-internal broken links.
        .filter(|e| match e.relation.as_str() {
            "imports" => e.target.starts_with('.'),
            // Heritage / `$fields` relations / module deps point at a
            // base/related/dependency class usually outside the indexed tree; a
            // `dol_include_once` of another module's file is likewise expected to
            // be unresolvable in a standalone index. None are gaps.
            "extends" | "implements" | "relates-to" | "depends-on" | "dol-requires" => false,
            _ => true,
        })
        // A target under `vendor/` is an out-of-repo dependency (Composer's
        // gitignored tree) -- a `require vendor/autoload.php` / a phpstan
        // `includes:` of a vendor extension, not a gap.
        .filter(|e| !e.target.contains("vendor/"))
        .collect();
    unresolved.sort_by(|a, b| a.target.cmp(&b.target));

    let has_children: HashSet<&str> =
        graph.edges.iter().filter(|e| e.relation == "contains").map(|e| e.source.as_str()).collect();
    // Files whose whole purpose is to `return` a value at file scope (a
    // `config/*.php` array, `bootstrap/app.php`): they declare no symbols by
    // design, so they are not "empty/broken" -- exclude them from the report.
    let returns_value: HashSet<&str> =
        graph.edges.iter().filter(|e| e.relation == "returns").map(|e| e.source.as_str()).collect();
    let empty_files: Vec<&NodeRec> = graph
        .nodes
        .iter()
        .filter(|n| n.kind == "file" && !has_children.contains(n.id.as_str()))
        .filter(|n| !returns_value.contains(n.id.as_str()))
        // Template + asset files legitimately hold no code symbols -- not a gap.
        .filter(|n| {
            let ext = Path::new(&n.path).extension().and_then(|e| e.to_str()).unwrap_or("");
            !NO_SYMBOL_EXTS.iter().any(|a| a.eq_ignore_ascii_case(ext)) && !n.path.ends_with(".blade.php")
        })
        .collect();

    report("unreferenced symbols (never called/imported/extended)", unreferenced.iter().map(|n| n.id.clone()));
    report("unresolved edges (target not in graph)", unresolved.iter().map(|e| format!("{} -> {} ({})", e.source, e.target, e.relation)));
    report("empty files (parsed to nothing)", empty_files.iter().map(|n| n.id.clone()));
    Ok(())
}

/// Print a titled count plus a capped sample of the items.
fn report(title: &str, items: impl Iterator<Item = String>) {
    const SAMPLE: usize = 20;
    let all: Vec<String> = items.collect();
    emit(format_args!("{}: {}", title, all.len()));
    for line in all.iter().take(SAMPLE) {
        emit(format_args!("  {line}"));
    }
    if all.len() > SAMPLE {
        emit(format_args!("  ... and {} more", all.len() - SAMPLE));
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::{resolve, source_slice, Graph, NodeRec};

    fn node(id: &str, name: &str, kind: &str, path: &str, start: usize, end: usize) -> NodeRec {
        NodeRec {
            id: id.to_string(),
            name: name.to_string(),
            kind: kind.to_string(),
            path: path.to_string(),
            start,
            end,
        }
    }

    #[test]
    fn resolves_exact_bare_ambiguous_and_missing() {
        let graph = Graph {
            nodes: vec![
                node("a.php#Foo", "Foo", "class", "a.php", 0, 0),
                node("a.php#Foo.bar", "bar", "method", "a.php", 0, 0),
                node("b.php#Foo.bar", "bar", "method", "b.php", 0, 0),
            ],
            edges: vec![],
        };
        assert_eq!(resolve(&graph, "a.php#Foo.bar").unwrap().id, "a.php#Foo.bar");
        assert_eq!(resolve(&graph, "Foo").unwrap().id, "a.php#Foo");
        assert!(resolve(&graph, "bar").is_err()); // ambiguous
        assert!(resolve(&graph, "nope").is_err()); // missing
    }

    #[test]
    fn slices_a_symbol_span_from_source() {
        let path = std::env::temp_dir().join("plouf_query_span.php");
        std::fs::write(&path, "<?php function foo(): int { return 1; }").unwrap();
        let p = path.to_str().unwrap();
        let n = node("x#foo", "foo", "function", p, 6, 100);
        assert!(source_slice(&n).unwrap().starts_with("function foo"));
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn file_kind_slice_returns_whole_file() {
        let path = std::env::temp_dir().join("plouf_query_whole.php");
        std::fs::write(&path, "<?php // whole-file-body").unwrap();
        let p = path.to_str().unwrap();
        let n = node(p, "plouf_query_whole.php", "file", p, 0, 0);
        assert!(source_slice(&n).unwrap().contains("whole-file-body"));
        std::fs::remove_file(&path).ok();
    }
}
