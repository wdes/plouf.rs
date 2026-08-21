//! Route -> page linking for Vue Router + Angular Router configs (TS/JS). A route
//! object `{ path: '/x', component: Foo }` (or `loadComponent` / `loadChildren:
//! () => import('...')`) links a `route:<path>` node to the page component's
//! file. `route:<path>` is the same join node bbscript emits, so
//! `callers route:/x` reaches both the e2e scenarios that visit a route and the
//! component that serves it -- closing scenario -> route -> source.
//!
//! Text-scanned (not AST): route objects are frequently multi-line, and the two
//! router frameworks share the `path:` / `component:` shape, so one scan covers
//! both. Runs only on files that look like a router config.

use std::collections::{HashMap, HashSet};

use crate::model::{Node, RawEdge};

/// Scan a JS/TS file for router route definitions, emitting `route:<path>` nodes
/// and `renders` edges to each route's component file.
pub fn scan(rel: &str, code: &str) -> (Vec<Node>, Vec<RawEdge>) {
    let mut nodes = Vec::new();
    let mut edges = Vec::new();
    // Cheap gate: only router-config-shaped files pay for the brace scan.
    if !code.contains("path:") || !(code.contains("component") || code.contains("loadChildren")) {
        return (nodes, edges);
    }

    let aliases = import_aliases(code);
    let bytes = code.as_bytes();
    let mut minted: HashSet<String> = HashSet::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'{' {
            i += 1;
            continue;
        }
        let Some(end) = matching_brace(bytes, i) else { break };
        // Every `{` is inspected (not skipped), so nested `children` routes are
        // found too; each object uses its own first `path:` + component.
        if let Some((path, spec)) = route_entry(&code[i + 1..end], &aliases) {
            let route = normalize_route(&path);
            if is_route_path(&route) {
                let target = join_relative(rel, &spec);
                let route_id = format!("route:{route}");
                if minted.insert(route_id.clone()) {
                    nodes.push(Node { id: route_id.clone(), name: route, kind: "route", path: rel.to_string(), start: 0, end: 0 });
                }
                edges.push(RawEdge::named(route_id, "renders", target));
            }
        }
        i += 1;
    }
    (nodes, edges)
}

/// A map of local identifier -> import specifier, from `const X = () =>
/// import('spec')` lazy consts and static `import X from 'spec'` declarations.
fn import_aliases(code: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for (idx, _) in code.match_indices("const ") {
        let after = &code[idx + "const ".len()..];
        let name: String = after.chars().take_while(|c| is_ident_char(*c)).collect();
        if name.is_empty() {
            continue;
        }
        let stmt = &after[..after.find([';', '\n']).unwrap_or(after.len())];
        if let Some(spec) = import_call_spec(stmt) {
            map.insert(name, spec);
        }
    }
    for (idx, _) in code.match_indices("import ") {
        let after = &code[idx + "import ".len()..];
        let line = &after[..after.find([';', '\n']).unwrap_or(after.len())];
        let Some(from_pos) = line.find(" from ") else { continue };
        let Some(spec) = first_string(&line[from_pos + " from ".len()..]) else { continue };
        for local in import_locals(&line[..from_pos]) {
            map.insert(local, spec.clone());
        }
    }
    map
}

/// The local names an import clause binds (`Foo`, `{ A, B as C }`, `Foo, { A }`).
fn import_locals(clause: &str) -> Vec<String> {
    let mut out = Vec::new();
    let (default_part, named_part) = match clause.split_once('{') {
        Some((d, n)) => (d, n.split_once('}').map_or(n, |(inner, _)| inner)),
        None => (clause, ""),
    };
    let default: String = default_part.trim().trim_end_matches(',').trim().chars().take_while(|c| is_ident_char(*c)).collect();
    if !default.is_empty() && default != "type" {
        out.push(default);
    }
    for part in named_part.split(',') {
        let name = part.rsplit(" as ").next().unwrap_or(part).trim();
        let local: String = name.chars().take_while(|c| is_ident_char(*c)).collect();
        if !local.is_empty() {
            out.push(local);
        }
    }
    out
}

/// `(path, component-spec)` for a route object, or `None` if it is not one. The
/// component spec is a dynamic-import string or an aliased identifier's import.
fn route_entry(obj: &str, aliases: &HashMap<String, String>) -> Option<(String, String)> {
    let path = prop_string(obj, "path")?;
    for key in ["component", "loadComponent", "loadChildren", "element"] {
        let Some(value) = prop_value(obj, key) else { continue };
        if let Some(spec) = import_call_spec(value) {
            return Some((path, spec));
        }
        let ident = leading_ident(value);
        if let Some(spec) = aliases.get(&ident) {
            return Some((path, spec.clone()));
        }
    }
    None
}

/// The `import('spec')` specifier appearing anywhere in `s`, if any.
fn import_call_spec(s: &str) -> Option<String> {
    let at = s.find("import(")?;
    first_string(&s[at + "import(".len()..])
}

/// The string value of the `key:` property in an object body (`path`).
fn prop_string(obj: &str, key: &str) -> Option<String> {
    first_string(prop_value(obj, key)?)
}

/// The raw value text of the `key:` property -- from after its colon up to the
/// next top-level comma (quotes/brackets/braces/parens respected).
fn prop_value<'a>(obj: &'a str, key: &str) -> Option<&'a str> {
    let bytes = obj.as_bytes();
    let mut from = 0;
    while let Some(rel_pos) = obj[from..].find(key) {
        let at = from + rel_pos;
        from = at + key.len();
        // Key boundary: not preceded by an identifier char.
        if at > 0 && is_ident_byte(bytes[at - 1]) {
            continue;
        }
        let mut j = at + key.len();
        while j < bytes.len() && bytes[j].is_ascii_whitespace() {
            j += 1;
        }
        if bytes.get(j) != Some(&b':') {
            continue;
        }
        j += 1;
        let start = j;
        let mut depth = 0i32;
        let mut quote: Option<u8> = None;
        let mut esc = false;
        while j < bytes.len() {
            let c = bytes[j];
            if let Some(q) = quote {
                if esc {
                    esc = false;
                } else if c == b'\\' {
                    esc = true;
                } else if c == q {
                    quote = None;
                }
            } else {
                match c {
                    b'\'' | b'"' | b'`' => quote = Some(c),
                    b'(' | b'[' | b'{' => depth += 1,
                    b')' | b']' | b'}' => depth -= 1,
                    b',' if depth == 0 => break,
                    _ => {}
                }
            }
            j += 1;
        }
        return Some(obj[start..j].trim());
    }
    None
}

/// The leading identifier of a value (`LoginPage` from `LoginPage`).
fn leading_ident(value: &str) -> String {
    value.trim().chars().take_while(|c| is_ident_char(*c)).collect()
}

/// The first single/double/back-quoted string literal in `s`.
fn first_string(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let q = bytes[i];
        if q == b'\'' || q == b'"' || q == b'`' {
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

/// The byte index of the `}` matching the `{` at `open`, quote-aware.
fn matching_brace(bytes: &[u8], open: usize) -> Option<usize> {
    let mut depth = 0i32;
    let mut quote: Option<u8> = None;
    let mut esc = false;
    let mut i = open;
    while i < bytes.len() {
        let c = bytes[i];
        if let Some(q) = quote {
            if esc {
                esc = false;
            } else if c == b'\\' {
                esc = true;
            } else if c == q {
                quote = None;
            }
        } else {
            match c {
                b'\'' | b'"' | b'`' => quote = Some(c),
                b'{' => depth += 1,
                b'}' => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(i);
                    }
                }
                _ => {}
            }
        }
        i += 1;
    }
    None
}

/// A real route path, not a JS regex literal / validation pattern that happens
/// to share the `path:` shape (`/\.pdf$/`, `/100[.,]00/`): reject regex anchors
/// and character classes, which never appear in an actual route path.
fn is_route_path(path: &str) -> bool {
    !path.contains(['$', '^', '[', ']'])
}

/// Ensure a leading `/` (Angular paths are relative, `''` is the root).
fn normalize_route(path: &str) -> String {
    if path.is_empty() {
        return "/".to_string();
    }
    if path.starts_with('/') {
        path.to_string()
    } else {
        format!("/{path}")
    }
}

/// Resolve a relative import specifier against the importing file's directory
/// (`resources/js/router/index.ts` + `../pages/X.vue` -> `resources/js/pages/X.vue`).
/// Non-relative specifiers (bare packages, `@/` aliases) are kept as-is.
fn join_relative(base_file: &str, spec: &str) -> String {
    if !spec.starts_with('.') {
        return spec.to_string();
    }
    let dir = base_file.rsplit_once('/').map_or("", |(d, _)| d);
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

const fn is_ident_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '$'
}

const fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'$'
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::scan;

    #[test]
    fn links_vue_lazy_const_routes_to_pages() {
        let code = "const LoginPage = () => import('../pages/LoginPage.vue');\nconst HomePage = () => import('../pages/HomePage.vue');\nconst routes = [\n  { path: '/', name: 'home', component: HomePage, meta: { a: 1 } },\n  { path: '/login', name: 'login', component: LoginPage },\n];";
        let (nodes, edges) = scan("resources/js/router/index.ts", code);
        assert!(nodes.iter().any(|n| n.kind == "route" && n.name == "/login"));
        assert!(nodes.iter().any(|n| n.kind == "route" && n.name == "/"));
        // `../pages/LoginPage.vue` resolves against the router dir.
        assert!(edges.iter().any(|e| e.relation == "renders"
            && e.source == "route:/login"
            && e.name.as_deref() == Some("resources/js/pages/LoginPage.vue")));
    }

    #[test]
    fn links_angular_component_and_lazy_routes() {
        // Static component import + a relative path (no leading slash), and a lazy
        // loadComponent with an inline dynamic import.
        let code = "import { HomeComponent } from './home/home.component';\nconst routes = [\n  { path: 'home', component: HomeComponent },\n  { path: 'admin', loadComponent: () => import('./admin/admin.component').then(m => m.AdminComponent) },\n];";
        let (nodes, edges) = scan("src/app/app.routes.ts", code);
        assert!(nodes.iter().any(|n| n.kind == "route" && n.name == "/home"));
        assert!(nodes.iter().any(|n| n.kind == "route" && n.name == "/admin"));
        assert!(edges.iter().any(|e| e.name.as_deref() == Some("src/app/home/home.component")));
        assert!(edges.iter().any(|e| e.name.as_deref() == Some("src/app/admin/admin.component")));
    }

    #[test]
    fn ignores_non_router_files() {
        let (nodes, edges) = scan("a.ts", "export const x = { path: 'nope' };");
        assert!(nodes.is_empty() && edges.is_empty()); // has path: but no component/loadChildren
    }

    #[test]
    fn covers_router_edge_cases() {
        // empty path -> '/', a non-relative (`@/`) import kept raw, `import type`
        // skipped, and a relative loadChildren joined.
        let code = "import type { T } from './t';\nconst routes = [\n  { path: '', component: () => import('@/pages/Root') },\n  { path: 'x', loadChildren: () => import('./x.module') },\n];";
        let (nodes, edges) = scan("src/app.routes.ts", code);
        assert!(nodes.iter().any(|n| n.kind == "route" && n.name == "/")); // '' -> root
        assert!(edges.iter().any(|e| e.name.as_deref() == Some("@/pages/Root"))); // non-relative kept raw
        assert!(edges.iter().any(|e| e.name.as_deref() == Some("src/x.module"))); // relative joined
    }

    #[test]
    fn unbalanced_brace_does_not_panic() {
        // Passes the gate (path: + component) but the object never closes.
        let (nodes, _) = scan("r.ts", "const routes = [ { path: '/a', component: A ");
        assert!(nodes.is_empty());
    }
}
