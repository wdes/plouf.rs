//! Whole-tree edge resolution: turns raw name/receiver refs into node ids,
//! the whole-tree resolver (contains passthrough, imports -> file,
//! heritage keeps unresolved names, calls via receiver-type + extends chain).
//! The index borrows string slices from the nodes/edges rather than cloning
//! them, so building it allocates nothing per symbol.

use std::collections::{HashMap, HashSet};

use crate::model::{Node, RawEdge, ResolvedEdge};

/// The class segment of a symbol id (`path#Class.method` -> `Class`;
/// `path#Class` -> `Class`).
pub fn class_of(id: &str) -> Option<&str> {
    let after = id.split('#').nth(1)?;
    Some(after.rsplit_once('.').map_or(after, |(c, _)| c))
}

/// Name/receiver indices, all borrowing slices from the nodes/edges they index.
struct Index<'a> {
    by_name: HashMap<&'a str, Vec<&'a str>>,
    path_by_name: HashMap<&'a str, Vec<&'a str>>,
    owner: HashMap<&'a str, HashMap<&'a str, Vec<&'a str>>>, // class -> method -> ids
    parents: HashMap<&'a str, Vec<&'a str>>,                 // class name -> base names
    files: HashSet<&'a str>,                                 // every file node's rel path
}

impl<'a> Index<'a> {
    fn build(nodes: &'a [Node], edges: &'a [RawEdge]) -> Self {
        let mut by_name: HashMap<&str, Vec<&str>> = HashMap::new();
        let mut path_by_name: HashMap<&str, Vec<&str>> = HashMap::new();
        let mut owner: HashMap<&str, HashMap<&str, Vec<&str>>> = HashMap::new();
        let mut files: HashSet<&str> = HashSet::new();
        for n in nodes {
            match n.kind {
                "method" => {
                    if let Some(c) = class_of(&n.id) {
                        owner.entry(c).or_default().entry(n.name.as_str()).or_default().push(&n.id);
                    }
                }
                "file" => {
                    files.insert(&n.path);
                }
                _ => {
                    by_name.entry(n.name.as_str()).or_default().push(&n.id);
                    path_by_name.entry(n.name.as_str()).or_default().push(&n.path);
                }
            }
        }
        let mut parents: HashMap<&str, Vec<&str>> = HashMap::new();
        for e in edges {
            if e.relation == "extends" {
                if let (Some(own), Some(p)) = (e.source.split('#').nth(1), &e.name) {
                    parents.entry(own).or_default().push(p.as_str());
                }
            }
        }
        Self { by_name, path_by_name, owner, parents, files }
    }

    fn unique(map: &HashMap<&'a str, Vec<&'a str>>, key: &str) -> Option<&'a str> {
        map.get(key).filter(|v| v.len() == 1).map(|v| v[0])
    }

    /// `recv.method`, walking the extends chain (cycle-guarded).
    fn resolve_member(&self, recv: &str, method: &str, seen: &mut HashSet<String>) -> Option<&'a str> {
        if !seen.insert(recv.to_string()) {
            return None;
        }
        if let Some(methods) = self.owner.get(recv) {
            if let Some(ids) = methods.get(method) {
                if ids.len() == 1 {
                    return Some(ids[0]);
                }
            }
        }
        self.parents.get(recv).into_iter().flatten().find_map(|p| self.resolve_member(p, method, seen))
    }
}

/// Resolve every raw edge to a deduplicated `ResolvedEdge` list.
pub fn resolve(nodes: &[Node], edges: &[RawEdge]) -> Vec<ResolvedEdge> {
    let idx = Index::build(nodes, edges);
    let mut out = Vec::new();
    let mut seen: HashSet<(&str, &'static str, String)> = HashSet::new();

    for e in edges {
        let name = e.name.as_deref().unwrap_or("");
        let target = match e.relation {
            "contains" => e.target_id.clone(),
            "imports" if name.starts_with('.') => {
                Some(resolve_relative(&e.source, name, &idx.files).unwrap_or_else(|| name.to_string()))
            }
            "imports" => {
                let last = name.rsplit('\\').next().unwrap_or(name);
                Some(Index::unique(&idx.path_by_name, last).map_or_else(|| name.to_string(), str::to_string))
            }
            // Heritage, PHPUnit covers, + Eloquent relations resolve to a
            // class/function node by unique name.
            "extends" | "implements" | "covers" => {
                Some(Index::unique(&idx.by_name, name).map_or_else(|| name.to_string(), str::to_string))
            }
            rel if crate::laravel::relation_kind(rel).is_some() => {
                Some(Index::unique(&idx.by_name, name).map_or_else(|| name.to_string(), str::to_string))
            }
            // Model/migration/query-builder -> the shared `table:<name>` node.
            "table" | "migrates" | "uses-table" => Some(format!("table:{name}")),
            // An e2e scenario -> the `route:<path>` node it opens.
            "visits" => Some(format!("route:{name}")),
            // A route -> the page-component file it renders (extension inferred).
            "renders" => Some(match_component_file(name, &idx.files).unwrap_or_else(|| name.to_string())),
            // PHP require/include -> the included file (relative to the includer).
            "requires" => Some(resolve_relative(&e.source, name, &idx.files).unwrap_or_else(|| name.to_string())),
            "calls" => resolve_call(&idx, e, name),
            "includes" => Some(resolve_view(name, &idx.files).unwrap_or_else(|| name.to_string())),
            _ => None,
        };
        if let Some(t) = target {
            if seen.insert((e.source.as_str(), e.relation, t.clone())) {
                out.push(ResolvedEdge { source: e.source.clone(), target: t, relation: e.relation });
            }
        }
    }
    out
}

/// Resolve a relative JS/TS import specifier (`./x`, `../y/z`) against the
/// importing file's directory to a known file node, trying the usual extension
/// and `index.*` candidates. Returns `None` when nothing matches.
fn resolve_relative(importer: &str, spec: &str, files: &HashSet<&str>) -> Option<String> {
    const EXTS: [&str; 10] =
        ["", ".ts", ".tsx", ".js", ".jsx", ".vue", ".mts", ".cts", ".mjs", ".cjs"];
    const INDEXES: [&str; 5] =
        ["/index.ts", "/index.tsx", "/index.js", "/index.vue", "/index.mjs"];

    let dir = importer.rsplit_once('/').map_or("", |(d, _)| d);
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
    let joined = parts.join("/");

    for ext in EXTS {
        let candidate = format!("{joined}{ext}");
        if files.contains(candidate.as_str()) {
            return Some(candidate);
        }
    }
    for index in INDEXES {
        let candidate = format!("{joined}{index}");
        if files.contains(candidate.as_str()) {
            return Some(candidate);
        }
    }
    None
}

/// Resolve a view name to a template file node by unique path-suffix match:
/// Blade dotted `layouts.app` -> `.../layouts/app.blade.php`, or Twig slashed
/// `database/row` -> `.../database/row.twig`. Keeps the raw name (via the
/// caller's fallback) when it is ambiguous or unknown -- e.g. namespaced/kebab
/// component tags (`mail::message`, `input-group`).
fn resolve_view(name: &str, files: &HashSet<&str>) -> Option<String> {
    let base = name.replace('.', "/");
    for ext in [".blade.php", ".twig"] {
        let suffix = format!("{base}{ext}");
        if files.contains(suffix.as_str()) {
            return Some(suffix);
        }
        let tail = format!("/{suffix}");
        let mut hits = files.iter().copied().filter(|f| f.ends_with(&tail));
        if let Some(first) = hits.next() {
            if hits.next().is_none() {
                return Some(first.to_string());
            }
        }
    }
    None
}

/// Match an already-joined component path to a file node, inferring the
/// extension (`src/app/home/home.component` -> `...home.component.ts`). An exact
/// path (a spec that already carried `.vue`) matches directly.
fn match_component_file(base: &str, files: &HashSet<&str>) -> Option<String> {
    const EXTS: [&str; 6] = [".ts", ".tsx", ".js", ".jsx", ".vue", ".mts"];
    if files.contains(base) {
        return Some(base.to_string());
    }
    EXTS.iter().map(|ext| format!("{base}{ext}")).find(|c| files.contains(c.as_str()))
}

fn resolve_call(idx: &Index, e: &RawEdge, name: &str) -> Option<String> {
    e.recv_type.as_ref().map_or_else(
        || {
            if e.via_member {
                None // no known receiver -> drop, like JS
            } else {
                Index::unique(&idx.by_name, name).map(str::to_string)
            }
        },
        |recv| idx.resolve_member(recv, name, &mut HashSet::new()).map(str::to_string),
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::{class_of, resolve};
    use crate::model::{Node, RawEdge};

    fn node(id: &str, name: &str, kind: &'static str) -> Node {
        let path = id.split('#').next().unwrap_or(id).to_string();
        Node { id: id.to_string(), name: name.to_string(), kind, path, start: 0, end: 0 }
    }

    fn call_target<'a>(edges: &'a [crate::model::ResolvedEdge], source: &str) -> Option<&'a str> {
        edges.iter().find(|e| e.relation == "calls" && e.source == source).map(|e| e.target.as_str())
    }

    #[test]
    fn class_of_extracts_class_segment() {
        assert_eq!(class_of("app/Foo.php#Foo.bar"), Some("Foo"));
        assert_eq!(class_of("app/Foo.php#Foo"), Some("Foo"));
        assert_eq!(class_of("app/Foo.php"), None);
    }

    #[test]
    fn resolves_unique_function_call_by_name() {
        let nodes = vec![
            node("a.php#Foo", "Foo", "class"),
            node("a.php#Foo.bar", "bar", "method"),
            node("b.php#baz", "baz", "function"),
        ];
        let edges = vec![RawEdge::call("a.php#Foo.bar".to_string(), "baz".to_string(), false, None)];
        let resolved = resolve(&nodes, &edges);
        assert_eq!(call_target(&resolved, "a.php#Foo.bar"), Some("b.php#baz"));
    }

    #[test]
    fn resolves_receiver_typed_method_call() {
        let nodes = vec![
            node("a.php#Foo", "Foo", "class"),
            node("a.php#Foo.bar", "bar", "method"),
            node("a.php#Foo.qux", "qux", "method"),
        ];
        let edges =
            vec![RawEdge::call("a.php#Foo.bar".to_string(), "qux".to_string(), true, Some("Foo".to_string()))];
        let resolved = resolve(&nodes, &edges);
        assert_eq!(call_target(&resolved, "a.php#Foo.bar"), Some("a.php#Foo.qux"));
    }

    #[test]
    fn drops_unresolved_member_call() {
        let nodes = vec![node("a.php#Foo.bar", "bar", "method")];
        let edges = vec![RawEdge::call("a.php#Foo.bar".to_string(), "unknown".to_string(), true, None)];
        let resolved = resolve(&nodes, &edges);
        assert!(resolved.iter().all(|e| e.relation != "calls"));
    }

    #[test]
    fn resolves_method_through_extends_chain() {
        let nodes = vec![
            node("a.php#Child", "Child", "class"),
            node("a.php#Child.f", "f", "method"),
            node("b.php#Base", "Base", "class"),
            node("b.php#Base.m", "m", "method"),
        ];
        let edges = vec![
            RawEdge::named("a.php#Child".to_string(), "extends", "Base".to_string()),
            RawEdge::call("a.php#Child.f".to_string(), "m".to_string(), true, Some("Child".to_string())),
        ];
        let resolved = resolve(&nodes, &edges);
        assert_eq!(call_target(&resolved, "a.php#Child.f"), Some("b.php#Base.m"));
    }

    #[test]
    fn resolves_heritage_by_unique_name() {
        let nodes = vec![node("a.php#Child", "Child", "class"), node("b.php#Base", "Base", "class")];
        let edges = vec![RawEdge::named("a.php#Child".to_string(), "extends", "Base".to_string())];
        let resolved = resolve(&nodes, &edges);
        let target = resolved.iter().find(|e| e.relation == "extends").map(|e| e.target.as_str());
        assert_eq!(target, Some("b.php#Base"));
    }

    #[test]
    fn resolves_relative_js_import_to_file() {
        let nodes = vec![node("resources/js/a.ts", "a.ts", "file"), node("resources/js/b.ts", "b.ts", "file")];
        let edges = vec![RawEdge::named("resources/js/a.ts".to_string(), "imports", "./b".to_string())];
        let resolved = resolve(&nodes, &edges);
        let target = resolved.iter().find(|e| e.relation == "imports").map(|e| e.target.as_str());
        assert_eq!(target, Some("resources/js/b.ts"));
    }

    #[test]
    fn keeps_unresolved_bare_import_name() {
        let nodes = vec![node("resources/js/a.ts", "a.ts", "file")];
        let edges = vec![RawEdge::named("resources/js/a.ts".to_string(), "imports", "vue".to_string())];
        let resolved = resolve(&nodes, &edges);
        let target = resolved.iter().find(|e| e.relation == "imports").map(|e| e.target.as_str());
        assert_eq!(target, Some("vue"));
    }

    #[test]
    fn resolves_relative_index_blade_include_and_keeps_unresolved() {
        // relative import resolving through `/index.*`
        let nodes = vec![node("a/b.ts", "b.ts", "file"), node("a/x/index.ts", "index.ts", "file")];
        let edges = vec![RawEdge::named("a/b.ts".to_string(), "imports", "./x".to_string())];
        let r = resolve(&nodes, &edges);
        assert_eq!(r.iter().find(|e| e.relation == "imports").map(|e| e.target.as_str()), Some("a/x/index.ts"));

        // a dotted Blade view resolves to its `.blade.php` file
        let nodes = vec![node("resources/views/layouts/app.blade.php", "app.blade.php", "file")];
        let edges = vec![RawEdge::named("v.blade.php".to_string(), "includes", "layouts.app".to_string())];
        let r = resolve(&nodes, &edges);
        assert_eq!(
            r.iter().find(|e| e.relation == "includes").map(|e| e.target.as_str()),
            Some("resources/views/layouts/app.blade.php")
        );

        // an unresolvable relative import keeps the raw specifier
        let nodes = vec![node("a/b.ts", "b.ts", "file")];
        let edges = vec![RawEdge::named("a/b.ts".to_string(), "imports", "./nope".to_string())];
        let r = resolve(&nodes, &edges);
        assert_eq!(r.iter().find(|e| e.relation == "imports").map(|e| e.target.as_str()), Some("./nope"));
    }

    #[test]
    fn resolves_eloquent_relation_and_table_edges() {
        let nodes = vec![
            node("app/Invoice.php#Invoice", "Invoice", "class"),
            node("app/Company.php#Company", "Company", "class"),
            node("table:companies", "companies", "table"),
        ];
        let edges = vec![
            RawEdge::named("app/Invoice.php#Invoice".to_string(), "belongsTo", "Company".to_string()),
            RawEdge::named("app/Company.php#Company".to_string(), "table", "companies".to_string()),
            RawEdge::named("db/m.php".to_string(), "migrates", "companies".to_string()),
        ];
        let r = resolve(&nodes, &edges);
        assert!(r.iter().any(|e| e.relation == "belongsTo" && e.target == "app/Company.php#Company"));
        assert!(r.iter().any(|e| e.relation == "table" && e.target == "table:companies"));
        assert!(r.iter().any(|e| e.relation == "migrates" && e.target == "table:companies"));
    }

    #[test]
    fn dedupes_identical_edges() {
        let nodes = vec![node("b.php#baz", "baz", "function")];
        let edges = vec![
            RawEdge::call("a.php".to_string(), "baz".to_string(), false, None),
            RawEdge::call("a.php".to_string(), "baz".to_string(), false, None),
        ];
        let resolved = resolve(&nodes, &edges);
        assert_eq!(resolved.iter().filter(|e| e.relation == "calls").count(), 1);
    }
}
