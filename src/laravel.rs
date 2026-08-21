//! Laravel-specific extraction over PHP. All Laravel/Eloquent knowledge lives
//! here; the generic PHP walker in `php.rs` calls into it. Covers:
//!
//! * **Eloquent relations** -- `$this->belongsTo(Related::class)` and friends,
//!   labelled by kind (the method name).
//! * **Model <-> table <-> migration** -- a shared `table:<name>` node joins a
//!   model (explicit `$table` or the snake-case-plural convention), a migration
//!   (`Schema::create/table('x')`), and query-builder usage (`DB::table('x')`).

use std::collections::HashSet;

use crate::model::{Node, RawEdge};
use crate::php::dequalify;

/// Eloquent relation methods: a `$this->belongsTo(Related::class)` call yields a
/// model-class -> related-class edge, resolved by unique class name (like
/// heritage). The method name doubles as the `&'static` edge relation so the
/// graph records which kind of relation it is.
pub const RELATION_KINDS: &[&str] = &[
    "belongsTo", "hasMany", "hasOne", "belongsToMany", "morphTo", "morphMany", "morphOne",
    "morphToMany", "morphedByMany", "hasManyThrough", "hasOneThrough",
];

/// The `&'static` relation label for an Eloquent relation method, if it is one.
pub fn relation_kind(method: &str) -> Option<&'static str> {
    RELATION_KINDS.iter().copied().find(|&r| r == method)
}

/// The related model named by a relation call: the `X` of the first `X::class`,
/// else a quoted class-string first argument. De-qualified to the bare name.
pub fn related_model(call_src: &str) -> Option<String> {
    if let Some(pos) = call_src.find("::class") {
        let before = &call_src[..pos];
        let start = before.rfind(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '\\')).map_or(0, |i| i + 1);
        let ident = &before[start..];
        if !ident.is_empty() {
            return Some(dequalify(ident));
        }
    }
    first_string_literal(call_src).map(|s| dequalify(&s))
}

/// The table an Eloquent model maps to: an explicit `$table = '...'`, else the
/// Laravel convention for a class that looks like a model, else `None`.
pub fn model_table(name: &str, extends: &[String], body: &str) -> Option<String> {
    if let Some(t) = table_from_body(body) {
        return Some(t);
    }
    is_model_extends(extends).then(|| convention_table(name))
}

/// Does the class extend an Eloquent base (`Model`, `Authenticatable`, `Pivot`,
/// or a `*Model` base class)?
fn is_model_extends(extends: &[String]) -> bool {
    extends
        .iter()
        .any(|e| matches!(e.as_str(), "Model" | "Authenticatable" | "Pivot" | "MorphPivot") || e.ends_with("Model"))
}

/// An explicit `$table = '...'` property value in the class body, if any.
fn table_from_body(body: &str) -> Option<String> {
    let mut from = 0;
    while let Some(pos) = body[from..].find("$table") {
        let at = from + pos;
        from = at + "$table".len();
        let rest = body[from..].trim_start();
        if let Some(after_eq) = rest.strip_prefix('=') {
            let after_eq = after_eq.trim_start();
            if after_eq.starts_with('\'') || after_eq.starts_with('"') {
                return first_string_literal(after_eq);
            }
        }
    }
    None
}

/// The Laravel table name for a model class: `snake_case(pluralize(ClassName))`.
fn convention_table(class: &str) -> String {
    snake_case(&pluralize(class))
}

/// English pluralization covering the common cases (`Company` -> `Companies`,
/// `Address` -> `Addresses`, `InvoiceLine` -> `InvoiceLines`, and the Latin
/// `-is` -> `-es`: `Axis` -> `Axes`, `Analysis` -> `Analyses`).
fn pluralize(word: &str) -> String {
    let lower = word.to_ascii_lowercase();
    if lower.ends_with("is") && word.len() > 2 {
        return format!("{}es", &word[..word.len() - 2]);
    }
    if lower.ends_with('s') || lower.ends_with('x') || lower.ends_with('z') || lower.ends_with("ch") || lower.ends_with("sh") {
        return format!("{word}es");
    }
    if lower.ends_with('y') {
        let prev = word.chars().rev().nth(1);
        let is_vowel = matches!(prev, Some('a' | 'e' | 'i' | 'o' | 'u'));
        if !is_vowel {
            return format!("{}ies", &word[..word.len() - 1]);
        }
    }
    format!("{word}s")
}

/// `PascalCase`/`camelCase` -> `snake_case` (`InvoiceLine` -> `invoice_line`).
fn snake_case(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 4);
    for (i, c) in s.char_indices() {
        if c.is_ascii_uppercase() {
            if i > 0 {
                out.push('_');
            }
            out.push(c.to_ascii_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

/// The first single/double-quoted string literal in `s` (inter-quote content).
fn first_string_literal(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let q = bytes[i];
        if q == b'\'' || q == b'"' {
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

/// Scan a file for table references and emit file -> `table:<name>` edges (plus
/// the shared table node): `Schema::create/table/...('x')` (a migration defining
/// a table, relation `migrates`) and `DB::table('x')` (query-builder usage,
/// relation `uses-table`). Both join to the model of the same table.
pub fn scan_tables(rel: &str, code: &str, nodes: &mut Vec<Node>, edges: &mut Vec<RawEdge>) {
    let mut minted: HashSet<String> = HashSet::new();
    for method in ["create", "table", "rename", "drop", "dropIfExists"] {
        scan_static_string_arg(rel, code, &format!("Schema::{method}"), "migrates", nodes, edges, &mut minted);
    }
    scan_static_string_arg(rel, code, "DB::table", "uses-table", nodes, edges, &mut minted);
}

/// For every `needle(<string>, ...)` in `code`, emit a `rel -[relation]-> table`
/// edge and (once per file) the `table:<name>` node. Only fires when the first
/// argument is a string literal (a dynamic `DB::table($x)` is skipped).
fn scan_static_string_arg(
    rel: &str,
    code: &str,
    needle: &str,
    relation: &'static str,
    nodes: &mut Vec<Node>,
    edges: &mut Vec<RawEdge>,
    minted: &mut HashSet<String>,
) {
    let bytes = code.as_bytes();
    let mut from = 0;
    while let Some(pos) = code[from..].find(needle) {
        let at = from + pos;
        from = at + needle.len();
        // A preceding identifier char means this is a longer name, not our call.
        if at > 0 {
            let prev = bytes[at - 1];
            if prev.is_ascii_alphanumeric() || prev == b'_' {
                continue;
            }
        }
        let mut i = at + needle.len();
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if bytes.get(i) != Some(&b'(') {
            continue;
        }
        let mut k = i + 1;
        while k < bytes.len() && bytes[k].is_ascii_whitespace() {
            k += 1;
        }
        if !matches!(bytes.get(k), Some(b'\'' | b'"')) {
            continue;
        }
        if let Some(table) = first_string_literal(&code[k..]) {
            edges.push(RawEdge::named(rel.to_string(), relation, table.clone()));
            if minted.insert(table.clone()) {
                nodes.push(Node { id: format!("table:{table}"), name: table.clone(), kind: "table", path: rel.to_string(), start: 0, end: 0 });
            }
        }
    }
}

/// Recognised `Route::<x>(...)` methods that wire a controller: the HTTP verbs
/// (an action second argument), the resource/singleton family (a controller
/// second argument), and `controller(...)` (a group's controller).
const ROUTE_METHODS: &[&str] = &[
    "get", "post", "put", "patch", "delete", "any", "options", "match", "resource", "apiResource",
    "resources", "apiResources", "singleton", "apiSingleton", "controller",
];

/// The resource-family `Route::<x>(...)` methods, whose argument list may bind
/// several controllers at once -- the plural `resources`/`apiResources` array
/// form `['n' => C::class, ...]`, or a singular binding `('n', C::class)`. Every
/// `X::class` in the call is a controller, so these are scanned for ALL of them
/// (the HTTP-verb and `controller(...)` calls name a single controller instead).
const RESOURCE_METHODS: &[&str] =
    &["resource", "apiResource", "resources", "apiResources", "singleton", "apiSingleton"];

/// Scan a Laravel route file for controller references and emit a `routes-to`
/// edge from the file to each wired controller CLASS (de-qualified bare name).
/// Every controller reference in a route definition takes one of two shapes,
/// both captured from the argument list of a recognised `Route::<x>(...)` call:
///
/// * a `Controller::class` constant -- in an action array `[C::class, 'method']`,
///   a lone invokable `[C::class]`, a `Route::resource('n', C::class)` /
///   `apiResource` / `singleton` / `apiSingleton` binding, or a
///   `Route::controller(C::class)` group.
/// * a string action `'C@method'` / `'App\Http\Controllers\C@method'`.
///
/// Only literal references are captured; a closure/arrow action or a dynamic
/// (`$var`) reference yields nothing. The class is the target (the specific
/// method is not resolved), later resolved by unique class name like heritage.
pub fn scan_routes(rel: &str, code: &str, nodes: &mut Vec<Node>, edges: &mut Vec<RawEdge>) {
    const NEEDLE: &str = "Route::";
    let bytes = code.as_bytes();
    let mut minted: HashSet<String> = HashSet::new();
    let mut from = 0;
    while let Some(pos) = code[from..].find(NEEDLE) {
        let at = from + pos;
        from = at + NEEDLE.len();
        // A preceding identifier char means this is a longer name, not `Route`
        // (a leading `\` of a fully-qualified `\...\Route::get` is fine).
        if at > 0 {
            let prev = bytes[at - 1];
            if prev.is_ascii_alphanumeric() || prev == b'_' {
                continue;
            }
        }
        // Read the method identifier that follows `Route::` and gate on it.
        let mut m = from;
        while m < bytes.len() && (bytes[m].is_ascii_alphanumeric() || bytes[m] == b'_') {
            m += 1;
        }
        let method = &code[from..m];
        if !ROUTE_METHODS.contains(&method) {
            continue;
        }
        // The next non-space byte must open the call's argument list.
        let mut i = m;
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if bytes.get(i) != Some(&b'(') {
            continue;
        }
        let Some(close) = matching_paren(bytes, i) else { continue };
        let args = &code[i + 1..close];
        // The resource family may bind many controllers in one call (the plural
        // array form especially); every other verb names a single controller.
        if RESOURCE_METHODS.contains(&method) {
            let controllers = every_class_const(args);
            // A single-controller resource gets one base `route:<name>` node; the
            // plural array form binds several controllers to distinct paths, so
            // that path is ambiguous -- keep the controller edges only.
            if let [only] = controllers.as_slice() {
                mint_route(rel, args, only, nodes, edges, &mut minted);
            }
            for controller in controllers {
                push_controller_edge(rel, controller, edges);
            }
        } else if let Some(controller) = controller_ref(args) {
            mint_route(rel, args, &controller, nodes, edges, &mut minted);
            push_controller_edge(rel, controller, edges);
        }
    }
}

/// Mint the shared `route:<path>` node for a file-based route from its first
/// string argument (`Route::get('/x', ...)` -> `route:/x`) plus a `serves` edge
/// to its controller -- the SAME node/edge shape attribute routing emits, so
/// `find route:` lists file-based routes too. No-op when the controller is the
/// base `Controller`, or the first argument is not a string literal (a variable
/// path / a closure route with no controller).
fn mint_route(rel: &str, args: &str, controller: &str, nodes: &mut Vec<Node>, edges: &mut Vec<RawEdge>, minted: &mut HashSet<String>) {
    if controller == "Controller" {
        return;
    }
    let Some(path) = first_string_literal(args) else { return };
    let route = normalize_route(&path);
    let route_id = format!("route:{route}");
    if minted.insert(route_id.clone()) {
        nodes.push(Node { id: route_id.clone(), name: route, kind: "route", path: rel.to_string(), start: 0, end: 0 });
    }
    edges.push(RawEdge::named(route_id, "serves", controller.to_string()));
}

/// Emit a `routes-to` edge for a wired controller, skipping the base
/// `Controller` (`app/Http/Controllers/Controller.php`): a `Controller::class`
/// reference or a group's base is not a route action, so it is not a target.
fn push_controller_edge(rel: &str, controller: String, edges: &mut Vec<RawEdge>) {
    if controller == "Controller" {
        return;
    }
    edges.push(RawEdge::named(rel.to_string(), "routes-to", controller));
}

/// Every `X::class` in an argument slice, de-qualified to the bare name, in
/// source order. Used for the resource family, where one call can bind several
/// controllers (`Route::apiResources(['a' => A::class, 'b' => B::class])`).
fn every_class_const(args: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(pos) = args[from..].find("::class") {
        let at = from + pos;
        from = at + "::class".len();
        let before = &args[..at];
        let start = before.rfind(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '\\')).map_or(0, |i| i + 1);
        let ident = &before[start..];
        if !ident.is_empty() {
            out.push(dequalify(ident));
        }
    }
    out
}

/// Route-defining PHP 8 attributes on controller classes/methods. `Route` takes
/// an explicit path (phpMyAdmin's `#[Route('/x', ['GET'])]`, Symfony's `#[Route(
/// path: '/x', methods: [...])]`); the verb attributes (`#[Get('/x')]`,
/// `#[Post(...)]`, ...) carry the path as their first argument. These are the
/// BARE (unqualified) spellings; a namespaced `OpenAPI` operation attribute
/// (`#[OA\Post(path: '/x')]`) is handled separately via [`OA_ROUTE_VERBS`].
const ROUTE_ATTRS: &[&str] =
    &["Route", "Get", "Post", "Put", "Patch", "Delete", "Options", "Any"];

/// Swagger-PHP operation attributes (`#[OA\Get(path: '/x')]`, ...): a NAMESPACED
/// verb whose `path:` argument documents a real HTTP route. Matched by trailing
/// segment, so any alias works (`OA\Post`, `OpenApi\Attributes\Post`). The
/// `path:` requirement keeps non-route `OpenAPI` attributes (`OA\Schema`,
/// `OA\Response`, ...) out.
const OA_ROUTE_VERBS: &[&str] = &["Get", "Post", "Put", "Patch", "Delete", "Head", "Options"];

/// Scan a controller file for route-defining PHP attributes and, for each, emit
/// a `route:<path>` node (kind `route`, id `route:<path>` -- the SAME join node
/// the e2e/router scanners emit, so `callers route:/x` reaches every surface
/// that touches the path) plus a `serves` edge from that node to the enclosing
/// controller. The controller is the file's class (resolved to its node by
/// unique name), else the file itself. `route:` nodes are de-duplicated per file.
pub fn scan_route_attributes(rel: &str, code: &str, nodes: &mut Vec<Node>, edges: &mut Vec<RawEdge>) {
    if !code.contains("#[") {
        return;
    }
    let target = file_class_name(code).unwrap_or_else(|| rel.to_string());
    let bytes = code.as_bytes();
    let mut minted: HashSet<String> = HashSet::new();
    let mut from = 0;
    while let Some(pos) = code[from..].find("#[") {
        let at = from + pos;
        from = at + 2;
        let Some((attr, open)) = attribute_head(bytes, code, from) else { continue };
        let Some(close) = matching_paren(bytes, open) else { continue };
        let args = &code[open + 1..close];
        // A bare routing attribute (`#[Route('/x')]`, `#[Get('/x')]`) takes its
        // path positionally or as `path:`. A namespaced OpenAPI operation
        // attribute (`#[OA\Post(path: '/x')]`) names the same route via `path:`.
        // Anything else (`#[OA\Schema]`, a plain annotation) is not a route.
        let path = if !attr.contains('\\') && ROUTE_ATTRS.contains(&attr) {
            attribute_path(args)
        } else if attr.contains('\\') && OA_ROUTE_VERBS.contains(&attr.rsplit('\\').next().unwrap_or(attr)) {
            named_arg_string(args, "path")
        } else {
            None
        };
        let Some(path) = path else { continue };
        let route = normalize_route(&path);
        let route_id = format!("route:{route}");
        if minted.insert(route_id.clone()) {
            nodes.push(Node { id: route_id.clone(), name: route, kind: "route", path: rel.to_string(), start: 0, end: 0 });
        }
        edges.push(RawEdge::named(route_id, "serves", target.clone()));
    }
}

/// Parse an attribute head starting just after `#[`: an optional single leading
/// `\`, a bare identifier, then (after optional whitespace) the `(` that opens
/// its argument list. Returns the identifier and the `(` index. A namespaced
/// name (`OA\Get`, `Symfony\...\Route`) is rejected -- only the unqualified
/// spelling is a route -- as is an attribute with no argument list.
fn attribute_head<'a>(bytes: &[u8], code: &'a str, after_hash: usize) -> Option<(&'a str, usize)> {
    let mut i = after_hash;
    if bytes.get(i) == Some(&b'\\') {
        i += 1;
    }
    let start = i;
    // A possibly-namespaced identifier: `Get`, `OA\Post`, `Symfony\...\Route`.
    // The caller decides which namespaces (only OpenAPI verbs) count as routes.
    while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_' || bytes[i] == b'\\') {
        i += 1;
    }
    if i == start {
        return None;
    }
    let name = code.get(start..i)?;
    let mut j = i;
    while j < bytes.len() && bytes[j].is_ascii_whitespace() {
        j += 1;
    }
    match bytes.get(j) {
        Some(b'(') => Some((name, j)),
        _ => None,
    }
}

/// The path a route attribute defines: an explicit `path:` named argument wins
/// (so `#[Route(methods: [...], path: '/x')]` is read correctly), otherwise the
/// first string literal (the positional path of `#[Route('/x', ...)]` /
/// `#[Get('/x')]`).
fn attribute_path(args: &str) -> Option<String> {
    named_arg_string(args, "path").or_else(|| first_string_literal(args))
}

/// The string literal value of a `name:` named argument in an argument slice,
/// requiring the `name` token to sit at an argument boundary (start, or after a
/// `(`/`,`) so a substring like `xpath:` is not mistaken for `path:`.
fn named_arg_string(args: &str, name: &str) -> Option<String> {
    let needle = format!("{name}:");
    let bytes = args.as_bytes();
    let mut from = 0;
    while let Some(pos) = args[from..].find(&needle) {
        let at = from + pos;
        from = at + needle.len();
        let boundary = at == 0 || {
            let prev = bytes[at - 1];
            !(prev.is_ascii_alphanumeric() || prev == b'_')
        };
        if boundary {
            return first_string_literal(&args[from..]);
        }
    }
    None
}

/// The name of the file's class (its route-attribute target). The first `class`
/// keyword followed by an identifier; `Route::class` and the like are ignored
/// (a `::class` constant is not the `class Name {` declaration).
fn file_class_name(code: &str) -> Option<String> {
    let bytes = code.as_bytes();
    let mut from = 0;
    while let Some(pos) = code[from..].find("class") {
        let at = from + pos;
        from = at + "class".len();
        let prev_ok = at == 0 || {
            let p = bytes[at - 1];
            !(p.is_ascii_alphanumeric() || p == b'_')
        };
        if !prev_ok {
            continue;
        }
        let mut i = at + "class".len();
        let ws_start = i;
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i == ws_start {
            continue; // `class` must be followed by whitespace, not `::`/`(`.
        }
        let name_start = i;
        while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
            i += 1;
        }
        if i > name_start {
            return code.get(name_start..i).map(str::to_string);
        }
    }
    None
}

/// Normalise a route path to a leading `/` (an empty path becomes `/`), matching
/// the `route:<path>` node id convention the router/e2e scanners use.
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

/// The byte index of the `)` matching the `(` at `open`, skipping over
/// single/double-quoted string contents; `None` if the source is unbalanced.
fn matching_paren(bytes: &[u8], open: usize) -> Option<usize> {
    let mut depth = 0i32;
    let mut quote: Option<u8> = None;
    let mut i = open;
    while i < bytes.len() {
        let b = bytes[i];
        if let Some(q) = quote {
            if b == b'\\' {
                i += 2;
                continue;
            }
            if b == q {
                quote = None;
            }
        } else {
            match b {
                b'\'' | b'"' => quote = Some(b),
                b'(' => depth += 1,
                b')' => {
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

/// The controller class referenced in a route call's argument list: the
/// identifier before the first `X::class`, else the class part of the first
/// `'Class@method'` string action. De-qualified to the bare name.
fn controller_ref(args: &str) -> Option<String> {
    if let Some(pos) = args.find("::class") {
        let before = &args[..pos];
        let start = before.rfind(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '\\')).map_or(0, |i| i + 1);
        let ident = &before[start..];
        if !ident.is_empty() {
            return Some(dequalify(ident));
        }
    }
    controller_from_string_action(args)
}

/// The class part of the first `'Class@method'` string action in `args`, if any.
/// Both parts must be well-formed (class chars `[A-Za-z0-9_\\]`, method chars
/// `[A-Za-z0-9_]`) so non-action strings (a path, an email) are not mistaken
/// for one.
fn controller_from_string_action(args: &str) -> Option<String> {
    let bytes = args.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let q = bytes[i];
        if q == b'\'' || q == b'"' {
            let mut j = i + 1;
            while j < bytes.len() && bytes[j] != q {
                if bytes[j] == b'\\' {
                    j += 1;
                }
                j += 1;
            }
            let content = args.get(i + 1..j.min(bytes.len())).unwrap_or("");
            if let Some((class, method)) = content.split_once('@') {
                let class_ok = !class.is_empty() && class.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '\\');
                let method_ok = !method.is_empty() && method.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
                if class_ok && method_ok {
                    return Some(dequalify(class));
                }
            }
            i = j + 1;
        } else {
            i += 1;
        }
    }
    None
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::{convention_table, related_model, scan_route_attributes, scan_routes, scan_tables};
    use crate::model::{Node, RawEdge};

    #[test]
    fn convention_pluralizer_cases() {
        assert_eq!(convention_table("Company"), "companies");
        assert_eq!(convention_table("Address"), "addresses");
        assert_eq!(convention_table("InvoiceLine"), "invoice_lines");
        assert_eq!(convention_table("User"), "users");
        assert_eq!(convention_table("Category"), "categories");
        // Latin -is -> -es, e.g. a `VariantAxis` model maps to `variant_axes`.
        assert_eq!(convention_table("VariantAxis"), "variant_axes");
        assert_eq!(convention_table("Analysis"), "analyses");
    }

    #[test]
    fn related_model_from_class_const_or_string() {
        assert_eq!(related_model("$this->belongsTo(Company::class)").as_deref(), Some("Company"));
        assert_eq!(related_model("$this->hasMany(\\App\\Models\\Line::class, 'fk')").as_deref(), Some("Line"));
        assert_eq!(related_model("$this->belongsTo('App\\Models\\User')").as_deref(), Some("User"));
        assert_eq!(related_model("$this->morphTo()"), None);
    }

    #[test]
    fn model_table_resolution_edges() {
        // Not a model base + no $table -> nothing.
        assert_eq!(super::model_table("PriceService", &["SomeBase".to_string()], "class body"), None);
        // Explicit $table wins regardless of the base class.
        assert_eq!(super::model_table("X", &[], "protected $table = 'widgets';").as_deref(), Some("widgets"));
        // A `$table` that is not a string assignment is ignored.
        assert_eq!(super::model_table("X", &[], "$table->string('name');"), None);
        // A model base with no explicit $table falls back to the convention.
        assert_eq!(super::model_table("Company", &["Model".to_string()], "").as_deref(), Some("companies"));
    }

    #[test]
    fn scan_tables_skips_dynamic_table_names() {
        let mut nodes: Vec<Node> = Vec::new();
        let mut edges: Vec<RawEdge> = Vec::new();
        super::scan_tables("f.php", "<?php DB::table($name)->get(); Schema::create($t);", &mut nodes, &mut edges);
        assert!(edges.is_empty() && nodes.is_empty());
    }

    #[test]
    fn scan_tables_boundary_and_no_paren() {
        let mut nodes: Vec<Node> = Vec::new();
        let mut edges: Vec<RawEdge> = Vec::new();
        // `myDB::table` is a longer name (preceding ident char) -> ignored;
        // `Schema::create` with no `(` after -> ignored.
        super::scan_tables("f.php", "<?php myDB::table('x'); Schema::create ; DB::table  ('spaced');", &mut nodes, &mut edges);
        assert!(!edges.iter().any(|e| e.name.as_deref() == Some("x")));
        assert!(edges.iter().any(|e| e.relation == "uses-table" && e.name.as_deref() == Some("spaced")));
    }

    #[test]
    fn scan_routes_links_route_file_to_controllers() {
        let names = |code: &str| -> Vec<String> {
            let mut nodes: Vec<Node> = Vec::new();
            let mut edges: Vec<RawEdge> = Vec::new();
            scan_routes("routes/web.php", code, &mut nodes, &mut edges);
            edges.iter().filter(|e| e.relation == "routes-to").filter_map(|e| e.name.clone()).collect()
        };
        // Action array [Controller::class, 'method'].
        assert_eq!(names("<?php Route::get('/u', [UserController::class, 'index']);"), vec!["UserController"]);
        // Resource with a fully-qualified controller -> de-qualified.
        assert_eq!(
            names("<?php Route::resource('users', \\App\\Http\\Controllers\\PhotoController::class);"),
            vec!["PhotoController"]
        );
        // String action 'Controller@method'.
        assert_eq!(names("<?php Route::get('/home', 'HomeController@show');"), vec!["HomeController"]);
        // Group controller Route::controller(Controller::class).
        assert_eq!(
            names("<?php Route::controller(AdminController::class)->group(function () {});"),
            vec!["AdminController"]
        );
        // Invokable single-element array [Controller::class].
        assert_eq!(names("<?php Route::post('/x', [InvokableController::class]);"), vec!["InvokableController"]);
        // A closure action wires no controller.
        assert!(names("<?php Route::get('/x', fn () => 1);").is_empty());
    }

    #[test]
    fn scan_routes_mints_route_path_nodes_and_serves() {
        let mut nodes: Vec<Node> = Vec::new();
        let mut edges: Vec<RawEdge> = Vec::new();
        scan_routes(
            "routes/api.php",
            "<?php Route::get('/users', [UserController::class, 'index']); Route::post('companies', 'CompanyController@store');",
            &mut nodes,
            &mut edges,
        );
        let routes: Vec<String> = nodes.iter().filter(|n| n.kind == "route").map(|n| n.name.clone()).collect();
        assert!(routes.contains(&"/users".to_string()));
        assert!(routes.contains(&"/companies".to_string())); // a path with no leading slash is normalised
        assert!(edges.iter().any(|e| e.relation == "serves"
            && e.source == "route:/users"
            && e.name.as_deref() == Some("UserController")));
    }

    #[test]
    fn scan_routes_plural_resource_array_and_base_controller() {
        let mut nodes: Vec<Node> = Vec::new();
        let mut edges: Vec<RawEdge> = Vec::new();
        // The PLURAL array form binds several controllers in ONE call: every
        // `X::class` in it is a target, not just the first.
        let code = "<?php Route::apiResources([\n    'animals' => AnimalController::class,\n    'workflows' => \\App\\Http\\Controllers\\WorkflowController::class,\n    'tours' => TourController::class,\n]);";
        scan_routes("routes/api.php", code, &mut nodes, &mut edges);
        let targets: Vec<String> =
            edges.iter().filter(|e| e.relation == "routes-to").filter_map(|e| e.name.clone()).collect();
        assert!(targets.contains(&"AnimalController".to_string()));
        assert!(targets.contains(&"WorkflowController".to_string()));
        assert!(targets.contains(&"TourController".to_string()));
        assert_eq!(targets.len(), 3);

        // The singular `resources`/`apiResources` still work, and the base
        // `Controller` (a `Controller::class` reference / group base) is skipped.
        let mut n2: Vec<Node> = Vec::new();
        let mut e2: Vec<RawEdge> = Vec::new();
        scan_routes("routes/web.php", "<?php Route::resources(['photos' => PhotoController::class]); Route::get('/x', [Controller::class, 'i']);", &mut n2, &mut e2);
        let t2: Vec<String> = e2.iter().filter(|e| e.relation == "routes-to").filter_map(|e| e.name.clone()).collect();
        assert_eq!(t2, vec!["PhotoController"]);
        assert!(!t2.iter().any(|c| c == "Controller"));
    }

    #[test]
    fn scan_route_attributes_positional_named_and_verbs() {
        let attrs = |code: &str| -> (Vec<Node>, Vec<RawEdge>) {
            let mut nodes: Vec<Node> = Vec::new();
            let mut edges: Vec<RawEdge> = Vec::new();
            scan_route_attributes("src/Controllers/FooController.php", code, &mut nodes, &mut edges);
            (nodes, edges)
        };
        let route = |nodes: &[Node]| -> Vec<String> {
            nodes.iter().filter(|n| n.kind == "route").map(|n| n.name.clone()).collect()
        };

        // phpMyAdmin positional form on the class, path first, methods array.
        let (n, e) = attrs("<?php\n#[Route('/check-relations', ['GET', 'POST'])]\nfinal class CheckRelationsController {}");
        assert_eq!(route(&n), vec!["/check-relations"]);
        assert!(e.iter().any(|x| x.relation == "serves"
            && x.source == "route:/check-relations"
            && x.name.as_deref() == Some("CheckRelationsController")));

        // Symfony named args, path reordered after methods -> still the path.
        let (n, _) = attrs("<?php\n#[Route(methods: ['GET'], path: '/x/y')]\nclass FooController {}");
        assert_eq!(route(&n), vec!["/x/y"]);

        // Verb attributes; a path with no leading slash is normalised.
        let (n, _) = attrs("<?php\n#[Get('/a')]\n#[Post('b')]\nclass FooController {}");
        let routes = route(&n);
        assert!(routes.contains(&"/a".to_string()));
        assert!(routes.contains(&"/b".to_string()));
    }

    #[test]
    fn scan_route_attributes_captures_openapi_verbs_and_dedupes() {
        let mut nodes: Vec<Node> = Vec::new();
        let mut edges: Vec<RawEdge> = Vec::new();
        // `#[OA\Post(path: '/x')]` is a Swagger-PHP operation attribute -> a real
        // route; `#[OA\Schema(...)]` is not a verb, so it is ignored; the same
        // path twice mints one node.
        let code = "<?php\n#[OA\\Post(path: '/admin/authenticate', tags: ['auth'])]\n#[OA\\Schema(schema: 'Foo')]\n#[Get('/dup')]\n#[Post('/dup')]\nclass FooController {}";
        scan_route_attributes("src/Controllers/FooController.php", code, &mut nodes, &mut edges);
        let routes: Vec<String> = nodes.iter().filter(|n| n.kind == "route").map(|n| n.name.clone()).collect();
        assert!(routes.contains(&"/admin/authenticate".to_string())); // OpenAPI verb captured
        assert!(routes.contains(&"/dup".to_string()));
        assert_eq!(routes.iter().filter(|r| *r == "/dup").count(), 1); // de-duplicated
        assert!(!routes.iter().any(|r| r == "/Foo")); // OA\Schema is not a route
        assert!(edges.iter().any(|e| e.relation == "serves"
            && e.source == "route:/admin/authenticate"
            && e.name.as_deref() == Some("FooController")));
    }

    #[test]
    fn scan_tables_migrations_and_query_builder() {
        let code = "<?php\nSchema::create('companies', function ($t) { $t->string('name'); });\nDB::table('users')->where('id', 1);";
        let mut nodes: Vec<Node> = Vec::new();
        let mut edges: Vec<RawEdge> = Vec::new();
        scan_tables("f.php", code, &mut nodes, &mut edges);
        let named = |rel: &str, name: &str| edges.iter().any(|e| e.relation == rel && e.name.as_deref() == Some(name));
        assert!(named("migrates", "companies"));
        assert!(named("uses-table", "users"));
        assert!(!named("migrates", "name")); // a column, not a table
        assert!(nodes.iter().any(|n| n.kind == "table" && n.name == "companies"));
        assert!(nodes.iter().any(|n| n.kind == "table" && n.name == "users"));
    }
}
