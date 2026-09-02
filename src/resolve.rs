//! Whole-tree edge resolution: turns raw name/receiver refs into node ids,
//! the whole-tree resolver (contains passthrough, imports -> file,
//! heritage keeps unresolved names, calls via receiver-type + extends chain).
//! The index borrows string slices from the nodes/edges rather than cloning
//! them, so building it allocates nothing per symbol.

use std::collections::{HashMap, HashSet};

use crate::model::{Node, RawEdge, ResolvedEdge};

/// A Laravel local-scope method name for a query call: `active` -> `scopeActive`.
/// Only used as a fallback after the plain method name misses, and the caller
/// confirms `scope<Method>` exists before mapping (proof required).
fn scope_name(method: &str) -> String {
    let mut chars = method.chars();
    chars.next().map_or_else(String::new, |first| format!("scope{}{}", first.to_ascii_uppercase(), chars.as_str()))
}

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
    method_by_name: HashMap<&'a str, Vec<&'a str>>,          // method name -> ids (for scopes)
    parents: HashMap<&'a str, Vec<&'a str>>,                 // class name -> base names
    files: HashSet<&'a str>,                                 // every file node's rel path
    uses: HashMap<&'a str, HashMap<&'a str, &'a str>>,       // file -> (short name -> FQCN)
}

impl<'a> Index<'a> {
    fn build(nodes: &'a [Node], edges: &'a [RawEdge]) -> Self {
        let mut by_name: HashMap<&str, Vec<&str>> = HashMap::new();
        let mut path_by_name: HashMap<&str, Vec<&str>> = HashMap::new();
        let mut owner: HashMap<&str, HashMap<&str, Vec<&str>>> = HashMap::new();
        let mut method_by_name: HashMap<&str, Vec<&str>> = HashMap::new();
        let mut files: HashSet<&str> = HashSet::new();
        for n in nodes {
            match n.kind {
                "method" => {
                    if let Some(c) = class_of(&n.id) {
                        owner.entry(c).or_default().entry(n.name.as_str()).or_default().push(&n.id);
                    }
                    method_by_name.entry(n.name.as_str()).or_default().push(&n.id);
                }
                "file" => {
                    files.insert(&n.path);
                }
                // `.gitattributes` export-ignore targets: resolved by exact path
                // id, never by name -- keep them out of the name index.
                "path" => {}
                _ => {
                    by_name.entry(n.name.as_str()).or_default().push(&n.id);
                    path_by_name.entry(n.name.as_str()).or_default().push(&n.path);
                }
            }
        }
        let mut parents: HashMap<&str, Vec<&str>> = HashMap::new();
        let mut uses: HashMap<&str, HashMap<&str, &str>> = HashMap::new();
        for e in edges {
            if e.relation == "extends" {
                if let (Some(own), Some(p)) = (e.source.split('#').nth(1), &e.name) {
                    parents.entry(own).or_default().push(p.as_str());
                }
            }
            // A PHP `use A\B\Foo;` -> the file's short-name -> FQCN map, used to
            // disambiguate a class reference when the short name collides.
            if e.relation == "imports" {
                if let Some(fqcn) = &e.name {
                    if fqcn.contains('\\') {
                        let short = fqcn.rsplit('\\').next().unwrap_or(fqcn);
                        uses.entry(e.source.as_str()).or_default().insert(short, fqcn.as_str());
                    }
                }
            }
        }
        Self { by_name, path_by_name, owner, method_by_name, parents, files, uses }
    }

    fn unique(map: &HashMap<&'a str, Vec<&'a str>>, key: &str) -> Option<&'a str> {
        map.get(key).filter(|v| v.len() == 1).map(|v| v[0])
    }

    /// A bare class name referenced from `source` -> a node id. A unique name
    /// wins; on a short-name collision the source file's `use` import gives the
    /// FQCN, and the candidate whose path matches the namespace is chosen --
    /// `App\Models\Address` picks `app/Models/Address.php`, not the
    /// `OpenApi\Schemas\Address` twin. None when it cannot be pinned down.
    fn resolve_named(&self, source: &str, name: &str) -> Option<&'a str> {
        let candidates = self.by_name.get(name)?;
        if let [only] = candidates.as_slice() {
            return Some(only);
        }
        // A relation/heritage edge is sourced from the class node (`file#Class`);
        // `use` imports and namespaces key off the file.
        let file = source.split('#').next().unwrap_or(source);
        // 1. An explicit `use A\B\Name` wins: the candidate whose path matches
        //    that FQCN (`App\Models\Address` -> `app/Models/Address.php`).
        if let Some(fqcn) = self.uses.get(file).and_then(|m| m.get(name)) {
            let want = format!("{}.php", fqcn.replace('\\', "/").to_lowercase());
            if let Some(hit) =
                candidates.iter().copied().find(|id| id.split('#').next().unwrap_or(id).to_lowercase().ends_with(&want))
            {
                return Some(hit);
            }
        }
        // 2. No import -> a same-namespace reference: PHP resolves a bare name in
        //    the current namespace first, so prefer the candidate in the SAME
        //    directory as the referencing file.
        let dir = file.rsplit_once('/').map_or("", |(d, _)| d);
        candidates.iter().copied().find(|id| id.split('#').next().unwrap_or(id).rsplit_once('/').map_or("", |(d, _)| d) == dir)
    }

    /// `recv.method`, walking the extends chain (cycle-guarded). Falls back to a
    /// Laravel local scope (`->active()` -> `scopeActive`) but ONLY when that
    /// `scope<Method>` method actually exists on `recv` -- never a blind rename,
    /// so a plain call that has no matching scope is unaffected.
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
            let scoped = scope_name(method);
            if let Some(ids) = methods.get(scoped.as_str()) {
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
    let ids: HashSet<&str> = nodes.iter().map(|n| n.id.as_str()).collect();
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
            // Heritage, PHPUnit covers, + a route file -> the controller class it
            // wires, all resolve to a class/function node by unique name.
            "extends" | "implements" | "covers" | "routes-to" => {
                Some(idx.resolve_named(&e.source, name).map_or_else(|| name.to_string(), str::to_string))
            }
            // A `route:<path>` node -> what serves it. A Dolibarr API route names
            // the implementing method node directly (an exact id); an attribute
            // route names the controller class, resolved by unique name (else kept
            // raw -- a bare name resolves to the class, a file path to the file).
            "serves" if ids.contains(name) => Some(name.to_string()),
            "serves" => Some(idx.resolve_named(&e.source, name).map_or_else(|| name.to_string(), str::to_string)),
            // A `.gitattributes` export-ignore pattern -> the path it names (a
            // source file node, a `path` node when the target exists, else kept
            // raw -- unresolved, so `missing` flags it as a stale entry).
            "export-ignores" => Some(name.to_string()),
            // A linter config -> the file/class it activates: a phpcs sniff file
            // (an in-repo path) resolves directly; a phpstan rule class resolves
            // by unique bare name; anything else is kept raw.
            "configures" => Some(if idx.files.contains(name) {
                name.to_string()
            } else {
                Index::unique(&idx.by_name, name).map_or_else(|| name.to_string(), str::to_string)
            }),
            rel if crate::laravel::relation_kind(rel).is_some() => {
                if matches!(name, "self" | "static") {
                    // A self-referential relation (`belongsTo(self::class)`) points
                    // back at the model that declares it (the edge's own source).
                    Some(e.source.clone())
                } else {
                    Some(idx.resolve_named(&e.source, name).map_or_else(|| name.to_string(), str::to_string))
                }
            }
            // Model/migration/query-builder -> the shared `table:<name>` node.
            "table" | "migrates" | "uses-table" => Some(format!("table:{name}")),
            // An e2e scenario -> the `route:<path>` node it opens.
            "visits" => Some(format!("route:{name}")),
            // A route -> the page-component file it renders (extension inferred).
            "renders" => Some(match_component_file(name, &idx.files).unwrap_or_else(|| name.to_string())),
            // PHP require/include -> the included file (relative to the includer).
            "requires" => Some(resolve_relative(&e.source, name, &idx.files).unwrap_or_else(|| name.to_string())),
            // A file-scope `return` marker (config/manifest file) -> the file
            // itself, so `missing` can tell "returns a value" from "empty/broken".
            "returns" => Some(e.source.clone()),
            // Dolibarr extension points -> their shared join nodes.
            "checks-permission" => Some(format!("right:{name}")),
            "raises-trigger" | "handles-trigger" => Some(format!("trigger:{name}")),
            "fires-hook" | "handles-hook" => Some(format!("hook:{name}")),
            "declares-module" => Some(format!("module:{name}")),
            // A `$fields` `integer:Class:...` FK, or a descriptor `depends`
            // module -> the related/dependency class by name.
            "relates-to" | "depends-on" => {
                Some(idx.resolve_named(&e.source, name).map_or_else(|| name.to_string(), str::to_string))
            }
            // An API class' `@requires` role gate -> the shared `role:<name>` node.
            "requires-role" => Some(format!("role:{name}")),
            // `dol_include_once('/module/...')` -> the included file, resolved
            // against the doc-root (try the path, then with its first segment --
            // the module dir -- stripped, since a module is often indexed at its
            // own root).
            "dol-requires" => Some(resolve_dol_include(name, &idx.files)),
            // A PHP file registers a custom Twig function -> its `twigfn:` node.
            "defines-fn" => Some(format!("twigfn:{name}")),
            // A Twig template calls one -> the node, but only if it was actually
            // registered (drops built-ins, filters, and keyword calls).
            "uses-fn" => {
                let id = format!("twigfn:{name}");
                ids.contains(id.as_str()).then_some(id)
            }
            // An Angular class registers a custom pipe -> its `pipe:` node.
            "defines-pipe" => Some(format!("pipe:{name}")),
            // A template `| pipe` use -> the node, only if a class registered it.
            "uses-pipe" => {
                let id = format!("pipe:{name}");
                ids.contains(id.as_str()).then_some(id)
            }
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

/// Resolve a Dolibarr `dol_include_once('/module/path.php')` include. The path
/// is doc-root-absolute; try it verbatim (leading `/` stripped), then with the
/// leading module-directory segment removed -- a module is often indexed at its
/// own root, so `/mymod/class/x.php` lands at `class/x.php`. Kept raw (thus
/// unresolved, but not reported by `missing`) when neither matches.
fn resolve_dol_include(name: &str, files: &HashSet<&str>) -> String {
    let path = name.trim_start_matches('/');
    if files.contains(path) {
        return path.to_string();
    }
    if let Some((_module, rest)) = path.split_once('/') {
        if files.contains(rest) {
            return rest.to_string();
        }
    }
    name.to_string()
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
    // ESM/TS convention: `import './x.js'` refers to `x.ts` (the source the `.js`
    // was emitted from). Swap the written JS extension for its TS/Vue twin.
    for js in [".js", ".jsx", ".mjs", ".cjs"] {
        if let Some(stem) = joined.strip_suffix(js) {
            for ts in [".ts", ".tsx", ".mts", ".cts", ".vue"] {
                let candidate = format!("{stem}{ts}");
                if files.contains(candidate.as_str()) {
                    return Some(candidate);
                }
            }
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
                // No known receiver (a query-builder chain, `Model::query()->scope()`,
                // etc.). A Laravel scope call (`->active()`) still resolves when
                // EXACTLY ONE `scopeActive` method exists anywhere -- proof (the
                // scope method must exist) + uniqueness, never a blind rename.
                Index::unique(&idx.method_by_name, &scope_name(name)).map(str::to_string)
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
    fn resolves_laravel_scope_only_with_proof() {
        let nodes = vec![
            node("app/User.php#User", "User", "class"),
            node("app/User.php#User.scopeActive", "scopeActive", "method"),
        ];
        // ->active() on a User receiver maps to scopeActive -- the scope exists.
        let hit = vec![RawEdge::call("a.php#X.f".to_string(), "active".to_string(), true, Some("User".to_string()))];
        assert_eq!(call_target(&resolve(&nodes, &hit), "a.php#X.f"), Some("app/User.php#User.scopeActive"));
        // ->missing() has NO scopeMissing method -> dropped, never a blind rename.
        let miss = vec![RawEdge::call("a.php#X.f".to_string(), "missing".to_string(), true, Some("User".to_string()))];
        assert!(resolve(&nodes, &miss).iter().all(|e| e.relation != "calls"));
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
    fn resolves_js_specifier_to_ts_twin() {
        // `import './api/client.js'` hits client.ts -- the ESM/TS convention where
        // the written `.js` extension refers to the `.ts` source.
        let nodes = vec![node("resources/js/a.ts", "a.ts", "file"), node("resources/js/api/client.ts", "client.ts", "file")];
        let edges = vec![RawEdge::named("resources/js/a.ts".to_string(), "imports", "./api/client.js".to_string())];
        let r = resolve(&nodes, &edges);
        let target = r.iter().find(|e| e.relation == "imports").map(|e| e.target.as_str());
        assert_eq!(target, Some("resources/js/api/client.ts"));
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
    fn resolves_self_referential_relation_to_its_model() {
        let nodes = vec![node("app/Models/Step.php#Step", "Step", "class")];
        // `belongsTo(self::class)` on Step points back at Step.
        let edges = vec![RawEdge::named("app/Models/Step.php#Step".to_string(), "belongsTo", "self".to_string())];
        let r = resolve(&nodes, &edges);
        assert!(r.iter().any(|e| e.relation == "belongsTo" && e.target == "app/Models/Step.php#Step"));
    }

    #[test]
    fn fqcn_use_disambiguates_short_name_collision() {
        let nodes = vec![
            node("app/Models/Address.php#Address", "Address", "class"),
            node("app/OpenApi/Schemas/Address.php#Address", "Address", "class"),
            node("app/Models/Company.php#Company", "Company", "class"),
        ];
        // The importing file's `use App\Models\Address` picks the model, not the
        // OpenAPI-schema twin, for its `belongsTo(Address::class)`.
        let edges = vec![
            // `use` imports are file-sourced; the relation is class-node-sourced.
            RawEdge::named("app/Models/Company.php".to_string(), "imports", "App\\Models\\Address".to_string()),
            RawEdge::named("app/Models/Company.php#Company".to_string(), "belongsTo", "Address".to_string()),
        ];
        let r = resolve(&nodes, &edges);
        assert!(r.iter().any(|e| e.relation == "belongsTo" && e.target == "app/Models/Address.php#Address"));

        // Same-namespace reference with NO use import: the candidate in the same
        // directory as the referencing file wins (current namespace resolved first).
        let edges3 = vec![RawEdge::named("app/Models/Contact.php#Contact".to_string(), "belongsTo", "Address".to_string())];
        let r3 = resolve(&nodes, &edges3);
        assert!(r3.iter().any(|e| e.relation == "belongsTo" && e.target == "app/Models/Address.php#Address"));

        // Neither a use nor a same-dir candidate: an ambiguous name stays raw.
        let edges2 = vec![RawEdge::named("app/x.php#X".to_string(), "belongsTo", "Address".to_string())];
        let r2 = resolve(&nodes, &edges2);
        assert!(r2.iter().any(|e| e.relation == "belongsTo" && e.target == "Address"));
    }

    #[test]
    fn resolves_export_ignore_to_named_path() {
        let nodes = vec![node(".github", ".github", "path")];
        let edges = vec![
            RawEdge::named(".gitattributes".to_string(), "export-ignores", ".github".to_string()),
            RawEdge::named(".gitattributes".to_string(), "export-ignores", "gone.txt".to_string()),
        ];
        let r = resolve(&nodes, &edges);
        let targets: Vec<&str> =
            r.iter().filter(|e| e.relation == "export-ignores").map(|e| e.target.as_str()).collect();
        assert!(targets.contains(&".github")); // existing path -> resolves to the node
        assert!(targets.contains(&"gone.txt")); // stale -> kept raw, so `missing` flags it
    }

    #[test]
    fn resolves_configures_to_sniff_file_and_rule_class() {
        let nodes = vec![
            node("standard/FooSniff.php", "FooSniff.php", "file"),
            node("standard/BarRule.php#BarRule", "BarRule", "class"),
        ];
        let edges = vec![
            // a phpcs sniff ref (an in-repo path) and a phpstan rule class (by name)
            RawEdge::named("phpcs.xml".to_string(), "configures", "standard/FooSniff.php".to_string()),
            RawEdge::named("phpstan.neon".to_string(), "configures", "BarRule".to_string()),
        ];
        let r = resolve(&nodes, &edges);
        assert!(r.iter().any(|e| e.relation == "configures" && e.target == "standard/FooSniff.php"));
        assert!(r.iter().any(|e| e.relation == "configures" && e.target == "standard/BarRule.php#BarRule"));
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
