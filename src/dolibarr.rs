//! Dolibarr ERP integration -- recognise the framework's extension points so the
//! graph reasons about a Dolibarr module the way `laravel` does a Laravel app.
//!
//! Phase 1 (framework recognition), all textual passes over PHP source:
//! - **module descriptors** (`mod<Name> extends DolibarrModules`) -> a
//!   `module:<rights_class>` node the rest of the graph keys to;
//! - **permissions** (`$user->hasRight('module','l1'[,'l2'])`) -> `right:` nodes
//!   + `checks-permission` edges;
//! - **triggers** (`call_trigger('X')` raise, `DolibarrTriggers::runTrigger`
//!   `switch ($action)` handle) -> `trigger:` nodes + `raises`/`handles` edges
//!   (Dolibarr's model events);
//! - **hooks** (`$hookmanager->executeHooks('X')` fire, an `actions_<module>`
//!   class method handle) -> `hook:` nodes + `fires`/`handles` edges;
//! - **objects** (`extends CommonObject` with `$table_element`) -> a `table:`
//!   link, joining the object to its migration/SQL and to raw-SQL usages.
//!
//! Table links from Dolibarr's hand-written SQL are covered by
//! [`crate::laravel::scan_raw_sql_tables`] (shared with Laravel raw queries).

use std::collections::HashSet;

use crate::model::{Node, RawEdge};

/// Curated Dolibarr hook methods. An `actions_<module>.class.php` handler class
/// implements a subset of these; a method with one of these names is a hook
/// handler for the same-named hook, so it links to `hook:<name>`.
const HOOK_METHODS: &[&str] = &[
    "doActions",
    "doMassActions",
    "addMoreMassActions",
    "addMoreActionsButtons",
    "formObjectOptions",
    "formConfirm",
    "formAddObjectLine",
    "printObjectLine",
    "printOriginObjectLine",
    "printFieldPreListTitle",
    "printCommonFooter",
    "printLeftBlock",
    "defineColumnField",
    "insertExtraFields",
    "insertExtraHeader",
    "insertExtraFooter",
    "addHtmlHeader",
    "showInputField",
    "showOutputField",
    "getNomUrl",
    "formBuilddocOptions",
    "restrictedArea",
];

/// Whether `name` is a Dolibarr hook a handler class implements: a known hook
/// name, or a member of a hook family -- `pdf_*` document hooks, `dashboard*`
/// widgets, `printField*` list-column hooks, or the login hooks (all fired by
/// core, so their handlers would otherwise read as dead).
fn is_hook_method(name: &str) -> bool {
    HOOK_METHODS.contains(&name)
        || name.starts_with("pdf_")
        || name.starts_with("dashboard")
        || name.starts_with("printField")
        || matches!(
            name,
            "afterLogin"
                | "afterLoginFailed"
                | "beforeLoginAuthentication"
                | "beforePDFCreation"
                | "afterPDFCreation"
                | "createFrom"
                | "formButtonsList"
                | "getnomurltooltip"
        )
}

/// Run every Dolibarr scanner over one PHP file, appending to `nodes`/`edges`.
pub fn scan(rel: &str, base: &str, code: &str, nodes: &mut Vec<Node>, edges: &mut Vec<RawEdge>) {
    scan_module(rel, code, nodes, edges);
    scan_permissions(rel, code, nodes, edges);
    scan_triggers(rel, code, nodes, edges);
    scan_hooks(rel, base, code, nodes, edges);
    scan_common_object(rel, code, nodes, edges);
    scan_object_fields(rel, code, nodes, edges);
    scan_api_routes(rel, base, code, nodes, edges);
    scan_includes(rel, code, edges);
}

/// `mod<Name> extends DolibarrModules`: mint a `module:<rights_class>` node (the
/// key the graph ties permissions/hooks/triggers to) plus a `declares-module`
/// edge from the descriptor file.
fn scan_module(rel: &str, code: &str, nodes: &mut Vec<Node>, edges: &mut Vec<RawEdge>) {
    if !code.contains("DolibarrModules") {
        return;
    }
    let Some(module) = assigned_string(code, "rights_class") else {
        return;
    };
    nodes.push(node(rel, "module", &module));
    edges.push(RawEdge::named(rel.to_string(), "declares-module", module));
    // `$this->depends = array('modProduct', 'modStock')` -> a `depends-on` edge
    // to each required module's descriptor class.
    for dep in assigned_array_strings(code, "depends") {
        edges.push(RawEdge::named(rel.to_string(), "depends-on", dep));
    }
    // A `cronjobs` entry (`'objectname' => 'X', 'method' => 'doJob'`) -> a
    // `schedules` edge to the cron target class, so it is not read as dead code.
    for target in array_key_values(code, "objectname") {
        edges.push(RawEdge::named(rel.to_string(), "schedules", target));
    }
}

/// Permission checks, both forms Dolibarr uses. The modern
/// `$user->hasRight('module','level1'[,'level2'])` (a string-literal module is
/// required; a dynamic `hasRight($m, ...)` is skipped) and the legacy
/// `$user->rights->module->level1[->level2]` property access. Both yield a
/// `right:<module>.<l1>[.<l2>]` node + a `checks-permission` edge.
fn scan_permissions(rel: &str, code: &str, nodes: &mut Vec<Node>, edges: &mut Vec<RawEdge>) {
    let bytes = code.as_bytes();
    let mut minted = HashSet::new();
    for_each_call(code, "hasRight", |args| {
        let parts = split_args(args);
        let (Some(module), Some(l1)) =
            (parts.first().and_then(|a| string_literal(a)), parts.get(1).and_then(|a| string_literal(a)))
        else {
            return;
        };
        let mut name = format!("{module}.{l1}");
        if let Some(l2) = parts.get(2).and_then(|a| string_literal(a)) {
            name.push('.');
            name.push_str(&l2);
        }
        emit_permission(rel, &name, &mut minted, nodes, edges);
    });
    let mut from = 0;
    while let Some(pos) = code[from..].find("rights->") {
        let at = from + pos;
        from = at + "rights->".len();
        // Only the `$user->rights->` chain (always preceded by `->`), never a
        // bare `$rights->` local variable.
        if !(at >= 2 && &code[at - 2..at] == "->") {
            continue;
        }
        let segs = property_chain(bytes, code, at + "rights->".len(), 3);
        if segs.len() >= 2 {
            emit_permission(rel, &segs.join("."), &mut minted, nodes, edges);
        }
    }
}

/// Emit a `checks-permission` edge to `right:<name>` and mint the node once.
fn emit_permission(
    rel: &str,
    name: &str,
    minted: &mut HashSet<String>,
    nodes: &mut Vec<Node>,
    edges: &mut Vec<RawEdge>,
) {
    edges.push(RawEdge::named(rel.to_string(), "checks-permission", name.to_string()));
    mint(minted, nodes, node(rel, "permission", name));
}

/// `$fields` type sub-DSL on a `CommonObject`: `integer:Class:path/to.php[:...]`
/// is an implicit belongs-to -> a `relates-to` edge to the class; `sellist:Table`
/// / `chkbxlst:Table` is a dictionary lookup -> a `uses-table` edge. A plain
/// `integer` (no relation) or a dynamic type is skipped.
fn scan_object_fields(rel: &str, code: &str, nodes: &mut Vec<Node>, edges: &mut Vec<RawEdge>) {
    if !code.contains("$fields") {
        return;
    }
    let mut minted = HashSet::new();
    each_quoted(code, |lit| {
        let mut parts = lit.split(':');
        match parts.next() {
            Some("integer") => {
                if let Some(class) = parts.next().map(crate::php::dequalify) {
                    if is_field_class(&class) {
                        edges.push(RawEdge::named(rel.to_string(), "relates-to", class));
                    }
                }
            }
            Some("sellist" | "chkbxlst") => {
                if let Some(raw) = parts.next() {
                    let table = raw.strip_prefix("llx_").unwrap_or(raw);
                    if !table.is_empty() {
                        edges.push(RawEdge::named(rel.to_string(), "uses-table", table.to_string()));
                        mint(&mut minted, nodes, Node {
                            id: format!("table:{table}"),
                            name: table.to_string(),
                            kind: "table",
                            path: rel.to_string(),
                            start: 0,
                            end: 0,
                        });
                    }
                }
            }
            _ => {}
        }
    });
}

/// Triggers -- Dolibarr's model events. `call_trigger('X', ...)` raises event
/// `trigger:X`; a `DolibarrTriggers` subclass' `switch ($action)` handles the
/// upper-case action names it cases on. (Nested `switch ($object->element)` on
/// lower-case element names is ignored by the upper-case filter.)
fn scan_triggers(rel: &str, code: &str, nodes: &mut Vec<Node>, edges: &mut Vec<RawEdge>) {
    let mut minted = HashSet::new();
    for_each_call(code, "call_trigger", |args| {
        let Some(action) = split_args(args).first().and_then(|a| string_literal(a)) else {
            return;
        };
        if !is_trigger_action(&action) {
            return;
        }
        edges.push(RawEdge::named(rel.to_string(), "raises-trigger", action.clone()));
        mint(&mut minted, nodes, node(rel, "trigger", &action));
    });
    if code.contains("DolibarrTriggers") {
        for action in switch_case_strings(code).into_iter().filter(|s| is_trigger_action(s)) {
            edges.push(RawEdge::named(rel.to_string(), "handles-trigger", action.clone()));
            mint(&mut minted, nodes, node(rel, "trigger", &action));
        }
    }
}

/// Hooks -- Dolibarr's filter chain. `$hookmanager->executeHooks('X', ...)`
/// fires hook `hook:X`; an `actions_<module>.class.php` handler class' methods
/// named like a known Dolibarr hook handle the same-named hook.
fn scan_hooks(rel: &str, base: &str, code: &str, nodes: &mut Vec<Node>, edges: &mut Vec<RawEdge>) {
    let mut minted = HashSet::new();
    for_each_call(code, "executeHooks", |args| {
        let Some(method) = split_args(args).first().and_then(|a| string_literal(a)) else {
            return;
        };
        edges.push(RawEdge::named(rel.to_string(), "fires-hook", method.clone()));
        mint(&mut minted, nodes, node(rel, "hook", &method));
    });
    // A hook handler class lives at actions_<module>.class.php (or extends the
    // core CommonHookActions base). Its methods named like a Dolibarr hook -- a
    // known name or a hook family (`pdf_*`, `dashboard*`, `printField*`, login) --
    // handle that hook, including hooks fired only by core (e.g. afterLogin).
    if base.starts_with("actions_") || code.contains("CommonHookActions") {
        for method in defined_methods(code).into_iter().filter(|m| is_hook_method(m)) {
            edges.push(RawEdge::named(rel.to_string(), "handles-hook", method.clone()));
            mint(&mut minted, nodes, node(rel, "hook", &method));
        }
    }
}

/// A `CommonObject` (or `CommonObjectLine`) subclass with `$table_element = 'x'`
/// -> a `table` edge to `table:x` (and mints the node), joining the object to its
/// migration/SQL definition and to raw-SQL usages of the same table.
fn scan_common_object(rel: &str, code: &str, nodes: &mut Vec<Node>, edges: &mut Vec<RawEdge>) {
    if !code.contains("extends CommonObject") {
        return;
    }
    let Some(table) = assigned_string(code, "table_element") else {
        return;
    };
    edges.push(RawEdge::named(rel.to_string(), "table", table.clone()));
    nodes.push(Node {
        id: format!("table:{table}"),
        name: table,
        kind: "table",
        path: rel.to_string(),
        start: 0,
        end: 0,
    });
}

/// Dolibarr REST API (`Luracast` `Restler`) route auto-wiring in an
/// `api_<name>.class.php` class extending `DolibarrApi`. The route base path is
/// `strtolower(ClassName)`. A public method is routed either explicitly, from a
/// `PHPDoc` `@url <VERB> <path>` tag (one route per tag), or by convention when it
/// has none: a method named exactly after an HTTP verb (`get`/`post`/`put`/
/// `delete`/...) or `index` maps to that verb at the resource root, with each
/// required parameter appended as a `/{param}` path segment. Each route mints a
/// shared `route:<path>` node + a `serves` edge to the API class, exactly like
/// the Laravel/attribute route nodes, so `find route:` and `callers <ApiClass>`
/// span them all.
fn scan_api_routes(rel: &str, base: &str, code: &str, nodes: &mut Vec<Node>, edges: &mut Vec<RawEdge>) {
    if !(base.starts_with("api_") && code.contains("extends DolibarrApi")) {
        return;
    }
    let Some(class) = api_class_name(code) else {
        return;
    };
    let base_path = class.to_lowercase();
    let bytes = code.as_bytes();
    let mut minted = HashSet::new();
    // Layer A role gate: `@class DolibarrApiAccess {@requires user,external}` on
    // the class -> a `requires-role` edge to each `role:<name>` node.
    for role in api_required_roles(code) {
        edges.push(RawEdge::named(rel.to_string(), "requires-role", role.clone()));
        mint(&mut minted, nodes, Node {
            id: format!("role:{role}"),
            name: role,
            kind: "role",
            path: rel.to_string(),
            start: 0,
            end: 0,
        });
    }
    let mut from = 0;
    while let Some(pos) = code[from..].find("function") {
        let at = from + pos;
        from = at + "function".len();
        if at > 0 && is_ident(bytes[at - 1]) {
            continue;
        }
        // A private method is not exposed as a route.
        if code[..at].trim_end().rsplit(char::is_whitespace).next() == Some("private") {
            continue;
        }
        let mut i = at + "function".len();
        while matches!(bytes.get(i), Some(b) if b.is_ascii_whitespace()) {
            i += 1;
        }
        let name_start = i;
        while matches!(bytes.get(i), Some(b) if is_ident(*b)) {
            i += 1;
        }
        let name = &code[name_start..i];
        if name.is_empty() || name.starts_with('_') {
            continue;
        }
        while matches!(bytes.get(i), Some(b) if b.is_ascii_whitespace()) {
            i += 1;
        }
        if bytes.get(i) != Some(&b'(') {
            continue;
        }
        let Some(close) = matching_paren(bytes, i) else {
            continue;
        };
        let params = &code[i + 1..close];
        from = close;

        // A route serves the specific implementing method node, so the endpoint
        // method is not read as dead and `callers route:<path>` names the handler.
        let method_id = format!("{rel}#{class}.{name}");
        let urls = url_tags(preceding_docblock(code, at));
        if !urls.is_empty() {
            for path in urls {
                let route = normalize_route(&format!("{base_path}/{path}"));
                mint_route(rel, &route, &method_id, &mut minted, nodes, edges);
            }
        } else if auto_route_verb(name).is_some() {
            let mut route = base_path.clone();
            for p in required_params(params) {
                route.push_str("/{");
                route.push_str(&p);
                route.push('}');
            }
            mint_route(rel, &normalize_route(&route), &method_id, &mut minted, nodes, edges);
        }
    }
}

/// The class name in `class <Name> extends DolibarrApi`.
fn api_class_name(code: &str) -> Option<String> {
    let pos = code.find("extends DolibarrApi")?;
    let head = code[..pos].trim_end();
    let start = head.rfind(|c: char| !(c.is_ascii_alphanumeric() || c == '_')).map_or(0, |i| i + 1);
    (start < head.len()).then(|| head[start..].to_string())
}

/// The `/** ... */` docblock immediately preceding the `function` at `func_at`
/// (only whitespace / visibility modifiers may sit between), else `""`.
fn preceding_docblock(code: &str, func_at: usize) -> &str {
    let head = code[..func_at].trim_end();
    let Some(end) = head.rfind("*/") else {
        return "";
    };
    let between = &code[end + 2..func_at];
    let only_modifiers = between
        .split_whitespace()
        .all(|w| matches!(w, "public" | "protected" | "private" | "static" | "final" | "abstract"));
    if only_modifiers {
        if let Some(start) = head[..end].rfind("/**") {
            return &code[start..end + 2];
        }
    }
    ""
}

/// Every `@url <VERB> <path>` tag's path (prefix `/` stripped) in a docblock.
fn url_tags(doc: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(pos) = doc[from..].find("@url") {
        from = from + pos + "@url".len();
        let rest = doc[from..].trim_start();
        let verb_end = rest.find(char::is_whitespace).unwrap_or(rest.len());
        if !is_http_verb(&rest[..verb_end]) {
            continue;
        }
        let after = rest[verb_end..].trim_start();
        let path_end = after.find(char::is_whitespace).unwrap_or(after.len());
        out.push(after[..path_end].trim_start_matches('/').to_string());
    }
    out
}

fn is_http_verb(v: &str) -> bool {
    matches!(v, "GET" | "POST" | "PUT" | "PATCH" | "DELETE" | "HEAD" | "OPTIONS")
}

/// The HTTP verb a method name auto-routes to when it carries no `@url`: a name
/// that is exactly an HTTP verb, or `index` (the collection root).
fn auto_route_verb(name: &str) -> Option<&'static str> {
    match name {
        "get" | "index" => Some("GET"),
        "post" => Some("POST"),
        "put" => Some("PUT"),
        "patch" => Some("PATCH"),
        "delete" => Some("DELETE"),
        "head" => Some("HEAD"),
        "options" => Some("OPTIONS"),
        _ => None,
    }
}

/// The names of the required parameters (those without a default `=`) in a
/// method's parameter list, in order -- each becomes a `/{param}` path segment.
fn required_params(params: &str) -> Vec<String> {
    split_args(params)
        .into_iter()
        .filter(|p| !p.is_empty() && !p.contains('='))
        .filter_map(|p| {
            let dollar = p.find('$')?;
            let after = &p[dollar + 1..];
            let end = after.find(|c: char| !(c.is_ascii_alphanumeric() || c == '_')).unwrap_or(after.len());
            (end > 0).then(|| after[..end].to_string())
        })
        .collect()
}

/// A single leading slash, no trailing slash.
fn normalize_route(path: &str) -> String {
    format!("/{}", path.trim_matches('/'))
}

/// Mint a shared `route:<path>` node (once) + a `serves` edge to `target` (the
/// implementing method's node id).
fn mint_route(
    rel: &str,
    route: &str,
    target: &str,
    minted: &mut HashSet<String>,
    nodes: &mut Vec<Node>,
    edges: &mut Vec<RawEdge>,
) {
    let id = format!("route:{route}");
    if minted.insert(id.clone()) {
        nodes.push(Node { id: id.clone(), name: route.to_string(), kind: "route", path: rel.to_string(), start: 0, end: 0 });
    }
    edges.push(RawEdge::named(id, "serves", target.to_string()));
}

/// Dolibarr SQL install files (`sql/llx_*.sql`, `.key.sql`): a `CREATE TABLE` or
/// `ALTER TABLE llx_<name>` (case-insensitive) mints/links the shared
/// `table:<name>` node via a `migrates` edge, joining the DDL to the object and
/// raw-SQL usages -- the Dolibarr analog of a Laravel migration. The `llx_`
/// prefix is stripped; a dynamic/quoted name is skipped.
pub fn scan_sql_ddl(rel: &str, code: &str, nodes: &mut Vec<Node>, edges: &mut Vec<RawEdge>) {
    let lower = code.to_ascii_lowercase();
    let bytes = code.as_bytes();
    let mut minted = HashSet::new();
    for verb in ["create table", "alter table"] {
        let mut from = 0;
        while let Some(pos) = lower[from..].find(verb) {
            let at = from + pos;
            from = at + verb.len();
            let mut i = at + verb.len();
            while matches!(bytes.get(i), Some(b) if b.is_ascii_whitespace()) {
                i += 1;
            }
            if lower[i..].starts_with("if not exists") {
                i += "if not exists".len();
                while matches!(bytes.get(i), Some(b) if b.is_ascii_whitespace()) {
                    i += 1;
                }
            }
            let Some(table) = sql_table_name(&code[i..]) else {
                continue;
            };
            edges.push(RawEdge::named(rel.to_string(), "migrates", table.clone()));
            mint(&mut minted, nodes, Node {
                id: format!("table:{table}"),
                name: table,
                kind: "table",
                path: rel.to_string(),
                start: 0,
                end: 0,
            });
        }
    }
}

/// A bare SQL table identifier at the start of `s` with a leading `llx_` stripped;
/// `None` for a quoted/backticked/dynamic name.
fn sql_table_name(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    if !matches!(bytes.first(), Some(b) if b.is_ascii_alphabetic() || *b == b'_') {
        return None;
    }
    let mut i = 0;
    while matches!(bytes.get(i), Some(b) if is_ident(*b)) {
        i += 1;
    }
    let name = s[..i].strip_prefix("llx_").unwrap_or(&s[..i]);
    (!name.is_empty()).then(|| name.to_string())
}

/// `dol_include_once('/module/class/x.class.php')` -> a `dol-requires` file
/// dependency edge (a module-relative include, resolved against the doc-root).
/// A dynamic path is skipped.
fn scan_includes(rel: &str, code: &str, edges: &mut Vec<RawEdge>) {
    for_each_call(code, "dol_include_once", |args| {
        if let Some(path) = split_args(args).first().and_then(|a| string_literal(a)) {
            edges.push(RawEdge::named(rel.to_string(), "dol-requires", path));
        }
    });
}

/// A Dolibarr `langs/<locale>/<domain>.lang` file: each `Key = Value` line
/// defines a translation key. Emitted as `uses-lang` edges so the key index
/// (`lang.json`, read by the `uses` verb) records where a key is defined, next
/// to where it is used. `#`-comment and blank lines are ignored.
pub fn scan_lang_file(rel: &str, code: &str) -> Vec<RawEdge> {
    let mut out = Vec::new();
    for line in code.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((key, _)) = line.split_once('=') {
            let key = key.trim();
            if !key.is_empty() && key.bytes().all(|b| is_ident(b) || b == b'.' || b == b'-') {
                out.push(RawEdge::named(rel.to_string(), "uses-lang", key.to_string()));
            }
        }
    }
    out
}

// --- helpers ---------------------------------------------------------------

/// A `kind:name` join node (`module:`/`right:`/`trigger:`/`hook:`).
fn node(rel: &str, kind: &'static str, name: &str) -> Node {
    let prefix = match kind {
        "permission" => "right",
        other => other,
    };
    Node { id: format!("{prefix}:{name}"), name: name.to_string(), kind, path: rel.to_string(), start: 0, end: 0 }
}

/// Push `n` only if its id has not been minted in this file yet.
fn mint(minted: &mut HashSet<String>, nodes: &mut Vec<Node>, n: Node) {
    if minted.insert(n.id.clone()) {
        nodes.push(n);
    }
}

const fn is_ident(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// A trigger action name: `ALL_CAPS_WITH_UNDERSCORES` (e.g. `BILL_VALIDATE`),
/// which distinguishes it from a lower-case `$object->element` switch value.
fn is_trigger_action(s: &str) -> bool {
    !s.is_empty()
        && s.bytes().next().is_some_and(|b| b.is_ascii_uppercase())
        && s.bytes().all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
}

/// Index of the `)` matching the `(` at `open`, respecting nested parens and
/// single/double-quoted strings. `None` if unbalanced.
const fn matching_paren(bytes: &[u8], open: usize) -> Option<usize> {
    let mut depth = 0i32;
    let mut i = open;
    while i < bytes.len() {
        match bytes[i] {
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            q @ (b'\'' | b'"') => i = skip_string(bytes, i + 1, q),
            _ => {}
        }
        i += 1;
    }
    None
}

/// Index of the closing quote `q` (starting at `i`, just past the opener),
/// honouring backslash escapes; the byte index of the closing quote.
const fn skip_string(bytes: &[u8], mut i: usize, q: u8) -> usize {
    while i < bytes.len() && bytes[i] != q {
        if bytes[i] == b'\\' {
            i += 1;
        }
        i += 1;
    }
    i
}

/// Call `f` with the argument slice (between the parens) of every `needle(...)`
/// call, boundary-checked so `needle` is not the tail of a longer identifier.
fn for_each_call(code: &str, needle: &str, mut f: impl FnMut(&str)) {
    let bytes = code.as_bytes();
    let mut from = 0;
    while let Some(pos) = code[from..].find(needle) {
        let at = from + pos;
        from = at + needle.len();
        if at > 0 && is_ident(bytes[at - 1]) {
            continue;
        }
        let mut i = at + needle.len();
        while matches!(bytes.get(i), Some(b) if b.is_ascii_whitespace()) {
            i += 1;
        }
        if bytes.get(i) != Some(&b'(') {
            continue;
        }
        if let Some(close) = matching_paren(bytes, i) {
            f(&code[i + 1..close]);
            from = close;
        }
    }
}

/// Split a call's argument slice on top-level commas (ignoring commas nested in
/// parens/brackets or inside string literals).
fn split_args(args: &str) -> Vec<&str> {
    let bytes = args.as_bytes();
    let mut out = Vec::new();
    let (mut start, mut depth, mut i) = (0usize, 0i32, 0usize);
    while i < bytes.len() {
        match bytes[i] {
            b'(' | b'[' => depth += 1,
            b')' | b']' => depth -= 1,
            q @ (b'\'' | b'"') => i = skip_string(bytes, i + 1, q),
            b',' if depth == 0 => {
                out.push(args[start..i].trim());
                start = i + 1;
            }
            _ => {}
        }
        i += 1;
    }
    out.push(args[start..].trim());
    out
}

/// The value of `arg` if it is a single quoted string literal with no trailing
/// concatenation, else `None` (a `$var`, a `X::class`, a `'a'.$b`, a constant).
fn string_literal(arg: &str) -> Option<String> {
    let bytes = arg.as_bytes();
    let q = *bytes.first()?;
    if q != b'\'' && q != b'"' {
        return None;
    }
    let end = skip_string(bytes, 1, q);
    // The closing quote must be the last byte -- otherwise it is a concatenation.
    if end != bytes.len() - 1 {
        return None;
    }
    Some(unescape(&arg[1..end]))
}

/// The first single/double-quoted string literal anywhere in `s` (its content).
fn first_quoted(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'\'' || b == b'"' {
            let end = skip_string(bytes, i + 1, b);
            return (end < bytes.len()).then(|| unescape(&s[i + 1..end]));
        }
        i += 1;
    }
    None
}

/// Drop backslash escapes from a string-literal body (names are ASCII).
fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            if let Some(next) = chars.next() {
                out.push(next);
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// The string value assigned to property `prop` (`$this->prop = '...'` or
/// `public $prop = '...'`), if any. Boundary-checked so `prop` is not the head
/// of a longer name (`table_element` does not match `table_element_line`).
fn assigned_string(code: &str, prop: &str) -> Option<String> {
    let bytes = code.as_bytes();
    let mut from = 0;
    while let Some(pos) = code[from..].find(prop) {
        let at = from + pos;
        from = at + prop.len();
        let before_ok = at == 0 || !is_ident(bytes[at - 1]);
        let after_ok = !matches!(bytes.get(at + prop.len()), Some(b) if is_ident(*b));
        if !(before_ok && after_ok) {
            continue;
        }
        let rest = &code[at + prop.len()..];
        let Some(eq) = rest.find('=') else { continue };
        let seg_end = rest[eq..].find(';').map_or(rest.len(), |s| eq + s);
        if let Some(value) = first_quoted(&rest[eq..seg_end]) {
            return Some(value);
        }
    }
    None
}

/// Every `case '...':` / `case "...":` string literal in `code`.
fn switch_case_strings(code: &str) -> Vec<String> {
    let mut out = Vec::new();
    let bytes = code.as_bytes();
    let mut from = 0;
    while let Some(pos) = code[from..].find("case") {
        let at = from + pos;
        from = at + "case".len();
        let before_ok = at == 0 || !is_ident(bytes[at - 1]);
        let after_ok = matches!(bytes.get(at + 4), Some(b) if b.is_ascii_whitespace());
        if !(before_ok && after_ok) {
            continue;
        }
        let mut i = at + 4;
        while matches!(bytes.get(i), Some(b) if b.is_ascii_whitespace()) {
            i += 1;
        }
        if let Some(&b) = bytes.get(i) {
            if b == b'\'' || b == b'"' {
                let end = skip_string(bytes, i + 1, b);
                if end < bytes.len() {
                    out.push(unescape(&code[i + 1..end]));
                }
            }
        }
    }
    out
}

/// Every method/function name declared in `code` (`function <name>(`).
fn defined_methods(code: &str) -> Vec<String> {
    let bytes = code.as_bytes();
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(pos) = code[from..].find("function") {
        let at = from + pos;
        from = at + "function".len();
        let before_ok = at == 0 || !is_ident(bytes[at - 1]);
        if !before_ok {
            continue;
        }
        let mut i = at + "function".len();
        while matches!(bytes.get(i), Some(b) if b.is_ascii_whitespace()) {
            i += 1;
        }
        let start = i;
        while matches!(bytes.get(i), Some(b) if is_ident(*b)) {
            i += 1;
        }
        if i > start {
            out.push(code[start..i].to_string());
        }
    }
    out
}

/// The identifiers of a `->a->b->c` property chain starting at byte `i` (just
/// past `rights->`), up to `max` segments.
fn property_chain<'a>(bytes: &[u8], code: &'a str, mut i: usize, max: usize) -> Vec<&'a str> {
    let mut segs = Vec::new();
    while segs.len() < max {
        let start = i;
        while matches!(bytes.get(i), Some(b) if is_ident(*b)) {
            i += 1;
        }
        if i == start {
            break;
        }
        segs.push(&code[start..i]);
        if code[i..].starts_with("->") {
            i += 2;
        } else {
            break;
        }
    }
    segs
}

/// Call `f` with the content of every single/double-quoted string literal.
fn each_quoted(code: &str, mut f: impl FnMut(&str)) {
    let bytes = code.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\'' || bytes[i] == b'"' {
            let end = skip_string(bytes, i + 1, bytes[i]);
            if end >= bytes.len() {
                break;
            }
            f(&code[i + 1..end]);
            i = end + 1;
        } else {
            i += 1;
        }
    }
}

/// Every quoted string in the array assigned to property `prop`
/// (`$this->prop = array('a', 'b')` / `= ['a', 'b']`), boundary-checked like
/// [`assigned_string`].
fn assigned_array_strings(code: &str, prop: &str) -> Vec<String> {
    let bytes = code.as_bytes();
    let mut from = 0;
    while let Some(pos) = code[from..].find(prop) {
        let at = from + pos;
        from = at + prop.len();
        let before_ok = at == 0 || !is_ident(bytes[at - 1]);
        let after_ok = !matches!(bytes.get(at + prop.len()), Some(b) if is_ident(*b));
        if !(before_ok && after_ok) {
            continue;
        }
        let rest = &code[at + prop.len()..];
        let Some(eq) = rest.find('=') else { continue };
        let seg_end = rest[eq..].find(';').map_or(rest.len(), |s| eq + s);
        let mut out = Vec::new();
        each_quoted(&rest[eq..seg_end], |s| out.push(s.to_string()));
        if !out.is_empty() {
            return out;
        }
    }
    Vec::new()
}

/// The string values of every `'<key>' => '<value>'` array entry in `code` (an
/// array key immediately followed by `=> '...'`). Used to pull `objectname`
/// targets out of a descriptor's `cronjobs` array.
fn array_key_values(code: &str, key: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(pos) = code[from..].find(key) {
        let at = from + pos;
        from = at + key.len();
        let rest = &code[at + key.len()..];
        // The key and its `=>` must be adjacent (an array key, not a stray word).
        let Some(arrow) = rest.find("=>") else { continue };
        if arrow > 4 {
            continue;
        }
        let seg = &rest[arrow + 2..rest.len().min(arrow + 2 + 80)];
        if let Some(value) = first_quoted(seg) {
            out.push(value);
        }
    }
    out
}

/// The roles in a `@class DolibarrApiAccess {@requires user,external}` class tag.
/// Empty unless the tag's class key is literally `DolibarrApiAccess` (matching
/// Dolibarr's own `verifyAccess`, which ignores any other key).
fn api_required_roles(code: &str) -> Vec<String> {
    let Some(pos) = code.find("DolibarrApiAccess") else {
        return Vec::new();
    };
    let rest = &code[pos..];
    let Some(rq) = rest.find("{@requires") else {
        return Vec::new();
    };
    let after = &rest[rq + "{@requires".len()..];
    let end = after.find('}').unwrap_or(after.len());
    after[..end].split(',').map(str::trim).filter(|s| !s.is_empty()).map(str::to_string).collect()
}

/// Whether `s` looks like a class name (upper-case initial, identifier chars) --
/// the relation target in an `integer:Class:...` field type.
fn is_field_class(s: &str) -> bool {
    s.chars().next().is_some_and(|c| c.is_ascii_uppercase())
        && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::scan;
    use crate::model::{Node, RawEdge};

    fn run(rel: &str, base: &str, code: &str) -> (Vec<Node>, Vec<RawEdge>) {
        let mut nodes = Vec::new();
        let mut edges = Vec::new();
        scan(rel, base, code, &mut nodes, &mut edges);
        (nodes, edges)
    }

    fn edge_targets<'a>(edges: &'a [RawEdge], relation: &str) -> Vec<&'a str> {
        edges.iter().filter(|e| e.relation == relation).filter_map(|e| e.name.as_deref()).collect()
    }

    #[test]
    fn permissions_from_has_right_with_two_and_three_levels() {
        let code = r"<?php
            if (!$user->hasRight('inventairerapide', 'stock', 'read')) {
                throw new RestException(403);
            }
            if (!DolibarrApiAccess::$user->hasRight('produit', 'lire')) {
                accessforbidden();
            }
            // dynamic module -> skipped
            if (!$user->hasRight($module, 'read')) {
                return;
            }
        ";
        let (nodes, edges) = run("acme/api.php", "api.php", code);
        let perms = edge_targets(&edges, "checks-permission");
        assert!(perms.contains(&"inventairerapide.stock.read"), "three-level right");
        assert!(perms.contains(&"produit.lire"), "two-level right");
        assert_eq!(perms.len(), 2, "the dynamic module call is skipped");
        assert!(nodes.iter().any(|n| n.kind == "permission" && n.id == "right:produit.lire"));
    }

    #[test]
    fn permissions_from_legacy_rights_property() {
        let code = r"<?php
            if ($user->rights->widgetshop->read) {
                // two-level property form
            }
            if (empty($user->rights->stock->import->create)) {
                accessforbidden();
            }
            // a bare $rights local variable must NOT be read as a permission
            if ($rights->something->else) {
                return;
            }
        ";
        let (nodes, edges) = run("acme/widget_list.php", "widget_list.php", code);
        let perms = edge_targets(&edges, "checks-permission");
        assert!(perms.contains(&"widgetshop.read"), "two-level property right");
        assert!(perms.contains(&"stock.import.create"), "three-level property right");
        assert!(!perms.contains(&"something.else"), "a bare $rights var is not a permission");
        assert!(nodes.iter().any(|n| n.kind == "permission" && n.id == "right:stock.import.create"));
    }

    #[test]
    fn object_fields_link_related_class_and_dictionary_table() {
        let code = r"<?php
            class Invoice extends CommonObject
            {
                public $table_element = 'facture';
                public $fields = array(
                    'rowid'   => array('type' => 'integer'),
                    'fk_soc'  => array('type' => 'integer:Societe:societe/class/societe.class.php:1:filter'),
                    'country' => array('type' => 'sellist:c_country:label:rowid'),
                );
            }
        ";
        let (nodes, edges) = run("acme/class/invoice.class.php", "invoice.class.php", code);
        // `integer:Class:path` -> a relates-to edge to the class (a belongs-to).
        assert!(edge_targets(&edges, "relates-to").contains(&"Societe"), "FK field relation");
        // a plain `integer` type carries no relation.
        assert!(!edge_targets(&edges, "relates-to").contains(&"integer"));
        // `sellist:Table` -> a uses-table edge to the dictionary table.
        assert!(edge_targets(&edges, "uses-table").contains(&"c_country"), "sellist table");
        assert!(nodes.iter().any(|n| n.kind == "table" && n.id == "table:c_country"));
    }

    #[test]
    fn triggers_raise_from_call_and_handle_from_switch() {
        let emitter = r"<?php
            class Widget extends CommonObject
            {
                public function validate($user)
                {
                    $this->call_trigger('WIDGET_VALIDATE', $user);
                }
            }
        ";
        let (_, emit_edges) = run("acme/class/widget.class.php", "widget.class.php", emitter);
        assert!(edge_targets(&emit_edges, "raises-trigger").contains(&"WIDGET_VALIDATE"));

        let handler = r"<?php
            class InterfaceAcme extends DolibarrTriggers
            {
                public function runTrigger($action, $object, User $user, Translate $langs, Conf $conf)
                {
                    switch ($action) {
                        case 'BILL_VALIDATE':
                        case 'LINEBILL_INSERT':
                            // a nested switch on the element must NOT be a trigger
                            switch ($object->element) {
                                case 'facturedet':
                                    break;
                            }
                            break;
                    }
                    return 0;
                }
            }
        ";
        let (nodes, handle_edges) = run("acme/core/triggers/interface_99_modAcme_Acme.class.php", "interface_99_modAcme_Acme.class.php", handler);
        let hits = edge_targets(&handle_edges, "handles-trigger");
        assert!(hits.contains(&"BILL_VALIDATE"));
        assert!(hits.contains(&"LINEBILL_INSERT"));
        assert!(!hits.contains(&"facturedet"), "lower-case element case is not a trigger");
        assert!(nodes.iter().any(|n| n.kind == "trigger" && n.id == "trigger:BILL_VALIDATE"));
    }

    #[test]
    fn hooks_fire_from_executehooks_and_handle_from_actions_class() {
        let firer = r"<?php
            $parameters = array('id' => $object->id);
            $reshook = $hookmanager->executeHooks('formObjectOptions', $parameters, $object, $action);
        ";
        let (_, fire_edges) = run("acme/acme_card.php", "acme_card.php", firer);
        assert!(edge_targets(&fire_edges, "fires-hook").contains(&"formObjectOptions"));

        let handler = r"<?php
            class ActionsAcme
            {
                public function formObjectOptions($parameters, &$object, &$action, $hookmanager)
                {
                    return 0;
                }

                public function computeSomethingPrivate($value)
                {
                    return $value * 2;
                }
            }
        ";
        let (nodes, handle_edges) = run("acme/class/actions_acme.class.php", "actions_acme.class.php", handler);
        let hits = edge_targets(&handle_edges, "handles-hook");
        assert!(hits.contains(&"formObjectOptions"), "a known hook method handles the hook");
        assert!(!hits.contains(&"computeSomethingPrivate"), "a plain helper is not a hook");
        assert!(nodes.iter().any(|n| n.kind == "hook" && n.id == "hook:formObjectOptions"));
    }

    #[test]
    fn hook_handler_recognises_pdf_login_and_family_methods() {
        let code = r"<?php
            class ActionsX extends CommonHookActions
            {
                public function formObjectOptions($parameters, &$object, &$action) { return 0; }
                public function pdf_writelinedesc($parameters, &$pdf) { return 0; }
                public function afterLogin($parameters, &$user) { return 0; }
                public function dashboardWidgets($parameters) { return 0; }
                public function computeStuff($value) { return $value + 1; }
            }
        ";
        let (_, edges) = run("acme/class/actions_x.class.php", "actions_x.class.php", code);
        let hooks = edge_targets(&edges, "handles-hook");
        assert!(hooks.contains(&"formObjectOptions"), "curated hook");
        assert!(hooks.contains(&"pdf_writelinedesc"), "pdf_ document-hook family");
        assert!(hooks.contains(&"afterLogin"), "core-fired login hook");
        assert!(hooks.contains(&"dashboardWidgets"), "dashboard widget family");
        assert!(!hooks.contains(&"computeStuff"), "a plain helper is not a hook");
    }

    #[test]
    fn cronjobs_schedule_the_target_class() {
        let code = r"<?php
            class modWidgetshop extends DolibarrModules
            {
                public function __construct($db)
                {
                    $this->rights_class = 'widgetshop';
                    $this->cronjobs = array(
                        0 => array(
                            'label' => 'Nightly sync',
                            'jobtype' => 'method',
                            'class' => '/widgetshop/class/batchwidget.class.php',
                            'objectname' => 'BatchWidget',
                            'method' => 'doScheduledJob',
                            'frequency' => 1,
                        ),
                    );
                }
            }
        ";
        let (_, edges) = run("widgetshop/core/modules/modWidgetshop.class.php", "modWidgetshop.class.php", code);
        assert!(edge_targets(&edges, "schedules").contains(&"BatchWidget"), "cron target class linked");
    }

    #[test]
    fn common_object_links_its_table_element() {
        let code = r"<?php
            class AcmeWidget extends CommonObject
            {
                public $element = 'acmewidget';
                public $table_element = 'acme_widget';
                public $table_element_line = 'acme_widget_line';
            }
        ";
        let (nodes, edges) = run("acme/class/acmewidget.class.php", "acmewidget.class.php", code);
        // table_element resolves; the longer table_element_line must not shadow it.
        assert_eq!(edge_targets(&edges, "table"), vec!["acme_widget"]);
        assert!(nodes.iter().any(|n| n.kind == "table" && n.id == "table:acme_widget"));
    }

    #[test]
    fn module_descriptor_mints_a_module_node() {
        let code = r"<?php
            class modAcme extends DolibarrModules
            {
                public function __construct($db)
                {
                    $this->numero = 436150;
                    $this->rights_class = 'acme';
                    $this->family = 'products';
                    $this->depends = array('modProduct', 'modStock');
                }
            }
        ";
        let (nodes, edges) = run("acme/core/modules/modAcme.class.php", "modAcme.class.php", code);
        assert!(nodes.iter().any(|n| n.kind == "module" && n.id == "module:acme"));
        assert_eq!(edge_targets(&edges, "declares-module"), vec!["acme"]);
        // depends -> a dependency edge per required module descriptor class.
        let deps = edge_targets(&edges, "depends-on");
        assert!(deps.contains(&"modProduct") && deps.contains(&"modStock"), "module deps: {deps:?}");
    }

    #[test]
    fn api_routes_from_url_tag_and_auto_routing() {
        let code = r"<?php
            /**
             * @access protected
             * @class DolibarrApiAccess {@requires user,external}
             */
            class Products extends DolibarrApi
            {
                /**
                 * Get a product by ref
                 *
                 * @param  string $ref  Reference
                 * @url GET ref/{ref}
                 */
                public function getByRef($ref)
                {
                    return $this->_fetch(0, $ref);
                }

                public function get($id, $includestock = 0)
                {
                    return $this->_fetch($id);
                }

                public function index($sortfield = 't.ref')
                {
                    return array();
                }

                public function post($request_data = null)
                {
                    return 1;
                }

                public function delete($id)
                {
                    return 1;
                }

                private function _fetch($id, $ref = '')
                {
                    return null;
                }
            }
        ";
        let (nodes, edges) = run("product/class/api_products.class.php", "api_products.class.php", code);
        let routes: std::collections::HashSet<&str> =
            nodes.iter().filter(|n| n.kind == "route").map(|n| n.id.as_str()).collect();
        assert!(routes.contains("route:/products/ref/{ref}"), "explicit @url route");
        assert!(routes.contains("route:/products/{id}"), "auto GET/DELETE with a required id segment");
        assert!(routes.contains("route:/products"), "auto index/post at the resource root");
        assert!(!routes.iter().any(|r| r.contains("fetch")), "the private _fetch is not a route");
        // Every route serves the implementing method node (not just the class).
        let serves = edge_targets(&edges, "serves");
        assert!(!serves.is_empty(), "routes serve something");
        assert!(serves.iter().all(|t| t.contains("#Products.")), "serves the method: {serves:?}");
        assert!(serves.iter().any(|t| t.ends_with("#Products.getByRef")), "explicit @url -> getByRef");
        // Layer A role gate from the class `@requires` tag.
        let roles = edge_targets(&edges, "requires-role");
        assert!(roles.contains(&"user") && roles.contains(&"external"), "api roles: {roles:?}");
        assert!(nodes.iter().any(|n| n.kind == "role" && n.id == "role:user"));
    }

    #[test]
    fn dol_include_once_emits_a_requires_edge() {
        let code = r"<?php
            dol_include_once('/acme/class/widget.class.php');
            dol_include_once($dynamicPath);
        ";
        let (_, edges) = run("acme/widget_card.php", "widget_card.php", code);
        let reqs = edge_targets(&edges, "dol-requires");
        assert_eq!(reqs, vec!["/acme/class/widget.class.php"], "only the static include");
    }

    #[test]
    fn lang_file_defines_keys_as_uses_lang() {
        let lang = "# widgetshop language file\n\
            WidgetLabel = Widget\n\
            Widget.status.draft = Draft\n\
            \n\
            EmptyValueKept =\n";
        let edges = super::scan_lang_file("langs/en_US/widgetshop.lang", lang);
        let keys: Vec<&str> = edges.iter().filter(|e| e.relation == "uses-lang").filter_map(|e| e.name.as_deref()).collect();
        assert!(keys.contains(&"WidgetLabel"));
        assert!(keys.contains(&"Widget.status.draft"), "dotted keys kept");
        assert!(keys.contains(&"EmptyValueKept"), "a key with an empty value still defines the key");
        // the comment and blank line define nothing.
        assert_eq!(keys.len(), 3);
    }

    #[test]
    fn sql_ddl_links_created_and_altered_tables() {
        let sql = r"
            CREATE TABLE llx_widgetshop_widget (
                rowid integer AUTO_INCREMENT PRIMARY KEY,
                ref   varchar(128) NOT NULL
            );
            ALTER TABLE llx_widgetshop_widget ADD INDEX idx_ref (ref);
            create table if not exists llx_widgetshop_line (fk_widget integer);
        ";
        let mut nodes = Vec::new();
        let mut edges = Vec::new();
        super::scan_sql_ddl("widgetshop/sql/llx_widgetshop_widget.sql", sql, &mut nodes, &mut edges);
        let tables: std::collections::HashSet<&str> =
            edges.iter().filter(|e| e.relation == "migrates").filter_map(|e| e.name.as_deref()).collect();
        // CREATE and ALTER on the same table both link it; llx_ prefix stripped.
        assert!(tables.contains("widgetshop_widget"), "CREATE + ALTER");
        assert!(tables.contains("widgetshop_line"), "lower-case create table if not exists");
        assert!(nodes.iter().any(|n| n.kind == "table" && n.id == "table:widgetshop_widget"));
    }

    #[test]
    fn non_dolibarr_php_yields_nothing() {
        let code = r"<?php
            class PlainService
            {
                public function handle(array $items): int
                {
                    return count($items);
                }
            }
        ";
        let (nodes, edges) = run("app/Service.php", "Service.php", code);
        assert!(nodes.is_empty(), "no Dolibarr idioms -> no nodes");
        assert!(edges.is_empty());
    }
}
