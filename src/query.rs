//! Query layer over a built `wiring.json`: word search, signature/body slicing
//! (via the byte spans stored on each node), and a gaps report. Paths in the
//! graph are relative to the indexed root, so run queries from that same root.

use std::collections::{HashMap, HashSet};
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
        [] => {
            // A bare miss wastes agent turns (guessing invented names); point at
            // the nearest real symbols instead of a dead-end "no match".
            let hints = suggest(graph, symbol);
            let msg = if hints.is_empty() {
                format!("no symbol matches '{symbol}'")
            } else {
                format!("no match for '{symbol}' -- did you mean: {}?", hints.join(", "))
            };
            Err(io::Error::new(io::ErrorKind::NotFound, msg))
        }
        many => {
            let list = many.iter().map(|n| n.id.as_str()).collect::<Vec<_>>().join("\n  ");
            Err(io::Error::new(io::ErrorKind::InvalidInput, format!("ambiguous '{symbol}':\n  {list}")))
        }
    }
}

/// How many "did you mean" candidates to offer on a miss.
const SUGGEST_LIMIT: usize = 5;

/// Levenshtein edit distance (iterative two-row), for the fuzzy fallback.
fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut curr: Vec<usize> = vec![0; b.len() + 1];
    for (i, ca) in a.iter().enumerate() {
        curr[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let cost = usize::from(ca != cb);
            curr[j + 1] = (prev[j + 1] + 1).min(curr[j] + 1).min(prev[j] + cost);
        }
        std::mem::swap(&mut prev, &mut curr);
    }
    prev[b.len()]
}

/// The closest existing symbol names to `symbol`, for a "did you mean": every
/// case-insensitive substring match first (tightest -- shortest -- name wins),
/// then a small edit-distance fallback. Capped at `SUGGEST_LIMIT`.
fn suggest(graph: &Graph, symbol: &str) -> Vec<String> {
    let needle = symbol.to_lowercase();
    let mut names: Vec<&str> =
        graph.nodes.iter().filter(|n| n.kind != "file").map(|n| n.name.as_str()).collect();
    names.sort_unstable();
    names.dedup();

    let mut subs: Vec<&str> = names
        .iter()
        .copied()
        .filter(|n| {
            let low = n.to_lowercase();
            low.contains(&needle) || needle.contains(&low)
        })
        .collect();
    subs.sort_by_key(|n| (n.len(), *n));

    // Edit-distance fallback over the names that did not substring-match, kept to
    // near neighbours so a wild query does not surface unrelated symbols.
    let mut rest: Vec<(usize, &str)> = names
        .iter()
        .copied()
        .filter(|n| !subs.contains(n))
        .map(|n| (levenshtein(&needle, &n.to_lowercase()), n))
        .filter(|(d, _)| *d <= 6)
        .collect();
    rest.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(b.1)));

    let mut out: Vec<String> = Vec::new();
    for n in subs.into_iter().chain(rest.into_iter().map(|(_, n)| n)) {
        out.push(n.to_string());
        if out.len() >= SUGGEST_LIMIT {
            break;
        }
    }
    out
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
    if hits.is_empty() {
        // Nothing matched the term -- offer the nearest names rather than leaving
        // the caller to guess (the discovery unblock).
        let nearest = suggest(&graph, term);
        if !nearest.is_empty() {
            emit(format_args!("no match for '{term}' -- did you mean: {}?", nearest.join(", ")));
        }
        return Ok(());
    }
    for n in hits {
        emit(format_args!("{}\t{}", n.kind, n.id));
    }
    Ok(())
}

/// A symbol's declaration line (source up to its opening `{` or `;`). Whole-unit
/// kinds (a Vue `component`, a `file`) have no line to slice, so they are named.
fn signature_line(node: &NodeRec) -> Result<String, io::Error> {
    if matches!(node.kind.as_str(), "component" | "file") {
        return Ok(format!("{} {}", node.kind, node.name));
    }
    let body = source_slice(node)?;
    let cut = body.find('{').or_else(|| body.find(';')).unwrap_or(body.len());
    Ok(body[..cut].trim().to_string())
}

/// Print a symbol's declaration line (source up to its opening `{` or `;`).
pub fn signature(out: &str, symbol: &str) -> Result<(), io::Error> {
    let graph = load(out)?;
    let node = resolve(&graph, symbol)?;
    emit(format_args!("{}", signature_line(node)?));
    Ok(())
}

/// Line budget for a truncated `body`: enough to read a normal method, short
/// enough to keep a huge one from flooding a token-constrained agent.
const BODY_MAX_LINES: usize = 40;

/// Tighter budget for the body shown inside `orient`, where four other sections
/// share the same output.
const ORIENT_BODY_LINES: usize = 20;

/// `text` capped at `budget` lines, with a `... (N more lines)` marker when it
/// overflows. Under the budget the text is returned verbatim.
fn truncate_body(text: &str, budget: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    if lines.len() <= budget {
        return text.to_string();
    }
    let more = lines.len() - budget;
    format!("{}\n... ({more} more lines)", lines[..budget].join("\n"))
}

/// Print a symbol's source body (signature line first, as sliced). Truncated to
/// `BODY_MAX_LINES` unless `full` is set -- token economy for large bodies.
pub fn body(out: &str, symbol: &str, full: bool) -> Result<(), io::Error> {
    let graph = load(out)?;
    let node = resolve(&graph, symbol)?;
    let src = source_slice(node)?;
    if full {
        emit(format_args!("{src}"));
    } else {
        emit(format_args!("{}", truncate_body(&src, BODY_MAX_LINES)));
    }
    Ok(())
}

/// The smallest node whose byte span contains `offset` (a method beats its
/// enclosing class), among the candidates for one file.
fn smallest_containing<'a>(nodes: &[&'a NodeRec], offset: usize) -> Option<&'a NodeRec> {
    nodes
        .iter()
        .copied()
        .filter(|n| offset >= n.start && offset < n.end)
        .min_by_key(|n| n.end.saturating_sub(n.start))
}

/// Case-insensitive content search over every indexed file -- code, comments AND
/// string literals, not just symbol names. Each file is re-read from disk (like
/// `source_slice`), scanned line by line, and every hit is mapped back to the
/// smallest node whose span contains it (else the file), printed as
/// `<kind><TAB><id><TAB><line>: <trimmed matching line>`. This is the discovery
/// unblock: a caller with only task vocabulary ("bullet", "<ul>") finds the
/// class that owns the concept even without knowing its name.
pub fn grep(out: &str, text: &str) -> Result<(), io::Error> {
    let graph = load(out)?;
    let needle = text.to_lowercase();

    // path -> its non-file nodes, for span containment.
    let mut by_path: HashMap<&str, Vec<&NodeRec>> = HashMap::new();
    for n in &graph.nodes {
        if n.kind != "file" {
            by_path.entry(n.path.as_str()).or_default().push(n);
        }
    }

    let mut lines: Vec<String> = Vec::new();
    for file in graph.nodes.iter().filter(|n| n.kind == "file") {
        let Ok(code) = fs::read_to_string(&file.path) else { continue };
        // Search the SAME text the spans index into (the `<script>` for `.vue`),
        // so a hit's byte offset maps onto a node correctly.
        let is_vue = Path::new(&file.path).extension().is_some_and(|e| e.eq_ignore_ascii_case("vue"));
        let haystack = if is_vue { crate::js::vue_script(&code) } else { code };
        let empty: Vec<&NodeRec> = Vec::new();
        let nodes = by_path.get(file.path.as_str()).unwrap_or(&empty);

        let mut offset = 0usize;
        for (lineno, line) in haystack.split_inclusive('\n').enumerate() {
            if let Some(col) = line.to_lowercase().find(&needle) {
                let owner = smallest_containing(nodes, offset + col).unwrap_or(file);
                lines.push(format!("{}\t{}\t{}: {}", owner.kind, owner.id, lineno + 1, line.trim()));
            }
            offset += line.len();
        }
    }
    lines.sort();
    lines.dedup();
    for l in lines {
        emit(format_args!("{l}"));
    }
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

/// Every reference edge that targets `node`, as `relation<TAB>source` (the blast
/// radius), sorted and deduplicated.
fn collect_callers(graph: &Graph, node: &NodeRec) -> Vec<String> {
    let mut hits: Vec<String> = graph
        .edges
        .iter()
        .filter(|e| e.target == node.id)
        .filter(|e| is_reference(&e.relation))
        .map(|e| format!("{}\t{}", e.relation, e.source))
        .collect();
    hits.sort();
    hits.dedup();
    hits
}

/// List what references a symbol: every reference edge that targets it, as
/// `relation<TAB>source` (the blast radius).
pub fn callers(out: &str, symbol: &str) -> Result<(), io::Error> {
    let graph = load(out)?;
    let node = resolve(&graph, symbol)?;
    for h in collect_callers(&graph, node) {
        emit(format_args!("{h}"));
    }
    Ok(())
}

/// The base name for twin matching: the segment before the first `.` (so a JS
/// `home.component` and its bare `home` sibling share a base).
fn base_name(name: &str) -> &str {
    name.split('.').next().unwrap_or(name)
}

/// The cross-file / cross-language same-name symbol(s): nodes sharing `node`'s
/// name (or base name) but living in a DIFFERENT file -- a PHP method and its
/// JS/TS port, the same concept expressed twice.
fn collect_twins<'a>(graph: &'a Graph, node: &NodeRec) -> Vec<&'a NodeRec> {
    let base = base_name(&node.name);
    let mut hits: Vec<&NodeRec> = graph
        .nodes
        .iter()
        .filter(|n| n.kind != "file")
        .filter(|n| n.path != node.path)
        .filter(|n| n.name == node.name || base_name(&n.name) == base)
        .collect();
    hits.sort_by(|a, b| a.id.cmp(&b.id));
    hits.dedup_by(|a, b| a.id == b.id);
    hits
}

/// List a symbol's cross-file / cross-language twin(s) as `<kind><TAB><id>`.
pub fn twin(out: &str, symbol: &str) -> Result<(), io::Error> {
    let graph = load(out)?;
    let node = resolve(&graph, symbol)?;
    for n in collect_twins(&graph, node) {
        emit(format_args!("{}\t{}", n.kind, n.id));
    }
    Ok(())
}

/// The tests that cover a symbol: every `covers` (`PHPUnit` `@covers`) edge into it,
/// plus any reference edge whose source lives in a `test`/`spec` path -- as
/// `relation<TAB>source`, sorted and deduplicated. Spec-by-example a caller would
/// otherwise never find.
fn collect_tests(graph: &Graph, node: &NodeRec) -> Vec<String> {
    let mut hits: Vec<String> = graph
        .edges
        .iter()
        .filter(|e| e.target == node.id)
        .filter(|e| {
            if e.relation == "covers" {
                return true;
            }
            let src_path = e.source.split('#').next().unwrap_or(&e.source).to_lowercase();
            is_reference(&e.relation) && (src_path.contains("test") || src_path.contains("spec"))
        })
        .map(|e| format!("{}\t{}", e.relation, e.source))
        .collect();
    hits.sort();
    hits.dedup();
    hits
}

/// List the tests covering a symbol as `relation<TAB>source`.
pub fn tests(out: &str, symbol: &str) -> Result<(), io::Error> {
    let graph = load(out)?;
    let node = resolve(&graph, symbol)?;
    for h in collect_tests(&graph, node) {
        emit(format_args!("{h}"));
    }
    Ok(())
}

/// One compact, token-budgeted orientation shot for a symbol: its signature, a
/// truncated body, the covering tests, the cross-language twin, and the callers
/// -- one command instead of four or five round-trips. When `arg` does not
/// resolve to a symbol it is treated as a keyword and content-searched (`grep`),
/// so a caller that only knows the concept still gets a foothold.
pub fn orient(out: &str, arg: &str) -> Result<(), io::Error> {
    let graph = load(out)?;
    let Ok(node) = resolve(&graph, arg) else {
        emit(format_args!("# grep '{arg}' (no symbol named that)"));
        return grep(out, arg);
    };

    emit(format_args!("# {} {}", node.kind, node.id));
    emit(format_args!("{}", signature_line(node)?));

    let src = source_slice(node)?;
    emit(format_args!("# body"));
    emit(format_args!("{}", truncate_body(&src, ORIENT_BODY_LINES)));

    let section = |title: &str, items: &[String]| {
        if !items.is_empty() {
            emit(format_args!("# {title}"));
            for it in items {
                emit(format_args!("{it}"));
            }
        }
    };
    section("tests", &collect_tests(&graph, node));
    let twins: Vec<String> = collect_twins(&graph, node).iter().map(|n| format!("{}\t{}", n.kind, n.id)).collect();
    section("twin", &twins);
    section("callers", &collect_callers(&graph, node));
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
    use super::{
        base_name, collect_tests, collect_twins, levenshtein, resolve, smallest_containing,
        source_slice, suggest, truncate_body, EdgeRec, Graph, NodeRec,
    };

    fn edge(source: &str, target: &str, relation: &str) -> EdgeRec {
        EdgeRec { source: source.to_string(), target: target.to_string(), relation: relation.to_string() }
    }

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

    #[test]
    fn levenshtein_counts_single_edits() {
        assert_eq!(levenshtein("kitten", "sitting"), 3);
        assert_eq!(levenshtein("Company", "Compaany"), 1);
        assert_eq!(levenshtein("same", "same"), 0);
    }

    #[test]
    fn suggest_prefers_substring_then_edit_distance() {
        let graph = Graph {
            nodes: vec![
                node("a.php#Company", "Company", "class", "a.php", 0, 0),
                node("a.php#CompanyTest", "CompanyTest", "class", "a.php", 0, 0),
                node("a.php#Widget", "Widget", "class", "a.php", 0, 0),
                node("a.php", "a.php", "file", "a.php", 0, 0),
            ],
            edges: vec![],
        };
        // A near-miss on "Compaany" surfaces the substring family first, and the
        // tightest (shortest) name leads.
        let hits = suggest(&graph, "Compaany");
        assert_eq!(hits.first().map(String::as_str), Some("Company"));
        assert!(hits.contains(&"CompanyTest".to_string()));
        // Never suggest a `file` node.
        assert!(!hits.iter().any(|h| h == "a.php"));
    }

    #[test]
    fn truncate_body_caps_and_marks_overflow() {
        let short = "line1\nline2\nline3";
        assert_eq!(truncate_body(short, 40), short); // under budget: verbatim

        let long: String = (1..=10).map(|i| format!("line{i}")).collect::<Vec<_>>().join("\n");
        let cut = truncate_body(&long, 4);
        assert!(cut.starts_with("line1\nline2\nline3\nline4"));
        assert!(cut.contains("... (6 more lines)"));
        assert!(!cut.contains("line5"));
    }

    #[test]
    fn smallest_containing_prefers_the_tighter_span() {
        let class = node("a.php#C", "C", "class", "a.php", 0, 100);
        let method = node("a.php#C.m", "m", "method", "a.php", 20, 60);
        let candidates = vec![&class, &method];
        // An offset inside both spans resolves to the method (the smaller span).
        assert_eq!(smallest_containing(&candidates, 30).map(|n| n.id.as_str()), Some("a.php#C.m"));
        // An offset only inside the class resolves to the class.
        assert_eq!(smallest_containing(&candidates, 5).map(|n| n.id.as_str()), Some("a.php#C"));
        // Outside every span: nothing.
        assert!(smallest_containing(&candidates, 200).is_none());
    }

    #[test]
    fn twins_are_same_name_in_a_different_file() {
        let graph = Graph {
            nodes: vec![
                node("app/Markup.php#Markup", "Markup", "class", "app/Markup.php", 0, 0),
                node("js/markup.ts#Markup", "Markup", "class", "js/markup.ts", 0, 0),
                node("app/Markup.php#Markup.render", "render", "method", "app/Markup.php", 0, 0),
            ],
            edges: vec![],
        };
        let php = resolve(&graph, "app/Markup.php#Markup").unwrap();
        let twins = collect_twins(&graph, php);
        // The JS port, and only it -- not the same-file `render` method.
        assert_eq!(twins.iter().map(|n| n.id.as_str()).collect::<Vec<_>>(), vec!["js/markup.ts#Markup"]);
    }

    #[test]
    fn base_name_strips_the_dotted_suffix() {
        assert_eq!(base_name("home.component"), "home");
        assert_eq!(base_name("plain"), "plain");
    }

    #[test]
    fn tests_collect_covers_and_testish_references() {
        let graph = Graph {
            nodes: vec![
                node("app/C.php#C", "C", "class", "app/C.php", 0, 0),
                node("tests/CTest.php#CTest", "CTest", "class", "tests/CTest.php", 0, 0),
                node("app/Other.php#Other", "Other", "class", "app/Other.php", 0, 0),
            ],
            edges: vec![
                edge("tests/CTest.php", "app/C.php#C", "covers"),
                edge("tests/CTest.php#CTest.it_works", "app/C.php#C", "calls"),
                // A non-test caller must NOT show up under `tests`.
                edge("app/Other.php#Other.use", "app/C.php#C", "calls"),
            ],
        };
        let node_c = resolve(&graph, "app/C.php#C").unwrap();
        let hits = collect_tests(&graph, node_c);
        assert!(hits.iter().any(|h| h == "covers\ttests/CTest.php"));
        assert!(hits.iter().any(|h| h == "calls\ttests/CTest.php#CTest.it_works"));
        assert!(!hits.iter().any(|h| h.contains("Other")));
    }
}
