//! PHP format: parse with Mago, walk the CST into the node/edge model, and scan
//! translation keys. All PHP-specific code lives here -- the AST helpers, the
//! walker, and the `Format` entry point.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};

use mago_allocator::LocalArena;
use mago_database::file::File;
use mago_span::HasSpan;
use mago_syntax::cst::cst::{
    ArrowFunction, Class, ClassLikeMemberSelector, Closure, Enum, Expression, Function,
    FunctionLikeParameterList, Hint, Identifier, Interface, Method, MethodCall, StaticMethodCall,
    Trait, TraitUse, Use, UseItems, Variable,
};
use mago_syntax::cst::Program;
use mago_syntax::parser::parse_file;
use mago_syntax::walker::Walker;

use crate::format::Format;
use crate::model::{Node, RawEdge};

/// The PHP format: routes every `*.php` that is not a Blade template.
pub struct Php;

impl Format for Php {
    // Blade precedes PHP in the registry, so a `*.blade.php` never reaches here.
    fn matches(&self, _base: &str, ext: &str) -> bool {
        ext == "php"
    }

    fn extract(&self, rel: &str, base: &str, code: &str) -> (Vec<Node>, Vec<RawEdge>) {
        extract(rel, base, code)
    }
}

/// Parse one PHP file with Mago and return its nodes + raw edges (plus
/// translation-key usages from the shared scanner).
pub fn extract(rel: &str, base: &str, code: &str) -> (Vec<Node>, Vec<RawEdge>) {
    let arena = LocalArena::new();
    let file = File::ephemeral(Cow::Owned(rel.to_string().into_bytes()), Cow::Owned(code.to_string().into_bytes()));
    let program = parse_file(&arena, &file);

    let mut ctx = Ctx::new(rel.to_string(), code.to_string());
    ctx.push_file(base.to_string());
    Ext::run(program, &mut ctx);
    let mut nodes = ctx.nodes;
    let mut edges = ctx.edges;
    // Migration <-> table links (`Schema::create('x')`), scanned from the raw
    // source; joins to model `table` edges through the shared `table:` node.
    scan_schema(rel, code, &mut nodes, &mut edges);
    edges.extend(crate::lang::scan(rel, code));
    (nodes, edges)
}

// --- AST helpers -----------------------------------------------------------

/// A byte-slice AST value as an owned `String` (lossy on invalid UTF-8).
fn bytes(v: &[u8]) -> String {
    String::from_utf8_lossy(v).into_owned()
}

/// The trailing segment of a `\`-qualified name (`App\Models\User` -> `User`).
fn dequalify(name: &str) -> String {
    name.rsplit('\\').next().unwrap_or(name).to_string()
}

/// The full (leading-`\`-trimmed) text of an identifier.
fn ident_full(id: &Identifier) -> String {
    let raw = match id {
        Identifier::Local(l) => bytes(l.value),
        Identifier::Qualified(q) => bytes(q.value),
        Identifier::FullyQualified(f) => bytes(f.value),
    };
    raw.trim_start_matches('\\').to_string()
}

/// The bare (de-qualified) name of an identifier.
fn ident_name(id: &Identifier) -> String {
    dequalify(&ident_full(id))
}

/// The single class named by a type hint, if any (unwraps `?T`/`(T)`; `None`
/// for unions, primitives, `array`, `void`, ...).
fn hint_class(h: &Hint) -> Option<String> {
    match h {
        Hint::Identifier(id) => Some(ident_name(id)),
        Hint::Nullable(n) => hint_class(n.hint),
        Hint::Parenthesized(p) => hint_class(p.hint),
        _ => None,
    }
}

/// The member name of a `->m`/`::m` selector, when it's a plain identifier.
fn selector_name(sel: &ClassLikeMemberSelector) -> Option<String> {
    match sel {
        ClassLikeMemberSelector::Identifier(id) => Some(bytes(id.value)),
        _ => None,
    }
}

/// The callee name of a `foo()` call, when it's a plain identifier.
fn callee_name(expr: &Expression) -> Option<String> {
    match expr {
        Expression::Identifier(id) => Some(ident_name(id)),
        _ => None,
    }
}

/// The `$var` text of a direct-variable expression (e.g. the `->` receiver).
fn var_name(expr: &Expression) -> Option<String> {
    match expr {
        Expression::Variable(Variable::Direct(dv)) => Some(bytes(dv.name)),
        _ => None,
    }
}

// --- Eloquent + migration helpers (text over source spans) -----------------

/// A byte range of `src` as a `&str`, clamped to valid bounds.
fn span(src: &str, start: u32, end: u32) -> &str {
    let s = (start as usize).min(src.len());
    let e = (end as usize).max(s).min(src.len());
    src.get(s..e).unwrap_or("")
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

/// The related model named by a relation call: the `X` of the first `X::class`,
/// else a quoted class-string first argument. De-qualified to the bare name.
fn related_model(call_src: &str) -> Option<String> {
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
fn model_table(name: &str, extends: &[String], body: &str) -> Option<String> {
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
/// `Address` -> `Addresses`, `InvoiceLine` -> `InvoiceLines`).
fn pluralize(word: &str) -> String {
    let lower = word.to_ascii_lowercase();
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

/// Scan migration `Schema::create/table/rename/drop/dropIfExists('x', ...)` calls
/// and emit a file -> `table:<x>` `migrates` edge (+ the shared table node) for
/// each. This is what links a migration back to the model of the same table.
fn scan_schema(rel: &str, code: &str, nodes: &mut Vec<Node>, edges: &mut Vec<RawEdge>) {
    const METHODS: [&str; 5] = ["create", "table", "rename", "drop", "dropIfExists"];
    let bytes = code.as_bytes();
    let mut minted: HashSet<String> = HashSet::new();
    for m in METHODS {
        let needle = format!("Schema::{m}");
        let mut from = 0;
        while let Some(pos) = code[from..].find(&needle) {
            let at = from + pos;
            from = at + needle.len();
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
            // The table is the first argument, and only when it is a string
            // literal (a dynamic `Schema::create($name, ...)` is skipped).
            if !matches!(bytes.get(k), Some(b'\'' | b'"')) {
                continue;
            }
            if let Some(table) = first_string_literal(&code[k..]) {
                edges.push(RawEdge::named(rel.to_string(), "migrates", table.clone()));
                if minted.insert(table.clone()) {
                    nodes.push(Node { id: format!("table:{table}"), name: table.clone(), kind: "table", path: rel.to_string(), start: 0, end: 0 });
                }
            }
        }
    }
}

// --- the walk --------------------------------------------------------------

/// Accumulator threaded through the walk (the walker itself is stateless).
struct Ctx {
    rel: String,
    source: String,
    nodes: Vec<Node>,
    edges: Vec<RawEdge>,
    scope: Vec<String>,
    class_stack: Vec<String>,
    class_ids: Vec<String>,
    minted: HashSet<String>,
    bindings: Vec<HashMap<String, String>>,
    pending_closure_name: Option<String>,
}

impl Ctx {
    fn new(rel: String, source: String) -> Self {
        let mut minted = HashSet::new();
        minted.insert(rel.clone());
        Self {
            rel,
            source,
            nodes: Vec::new(),
            edges: Vec::new(),
            scope: Vec::new(),
            class_stack: Vec::new(),
            class_ids: Vec::new(),
            minted,
            bindings: Vec::new(),
            pending_closure_name: None,
        }
    }

    /// Seed the file node before walking (file body is read wholesale, so its
    /// span is left at 0..0).
    fn push_file(&mut self, name: String) {
        self.nodes.push(Node { id: self.rel.clone(), name, kind: "file", path: self.rel.clone(), start: 0, end: 0 });
    }

    fn cur(&self) -> String {
        self.scope.last().cloned().unwrap_or_else(|| self.rel.clone())
    }

    /// Collision-proof id (`~2`/`~3`/...).
    fn mint(&mut self, base: String) -> String {
        if self.minted.insert(base.clone()) {
            return base;
        }
        let mut k = 2u32;
        loop {
            let cand = format!("{base}~{k}");
            if self.minted.insert(cand.clone()) {
                return cand;
            }
            k += 1;
        }
    }

    fn push_node(&mut self, base: String, name: String, kind: &'static str, start: u32, end: u32) -> String {
        let id = self.mint(base);
        self.edges.push(RawEdge::contains(self.cur(), id.clone()));
        self.nodes.push(Node { id: id.clone(), name, kind, path: self.rel.clone(), start, end });
        id
    }

    fn heritage(&mut self, class_id: &str, types: Vec<String>, relation: &'static str) {
        for t in types {
            self.edges.push(RawEdge::named(class_id.to_string(), relation, t));
        }
    }

    /// Emit a `table:<name>` node (once per file) and an edge from `source_id` to
    /// it. The shared node id is what joins models and migrations; cross-file
    /// duplicates are collapsed when the graph is assembled.
    fn link_table(&mut self, source_id: &str, table: &str, relation: &'static str) {
        let node_id = format!("table:{table}");
        if self.minted.insert(node_id.clone()) {
            self.nodes.push(Node { id: node_id, name: table.to_string(), kind: "table", path: self.rel.clone(), start: 0, end: 0 });
        }
        self.edges.push(RawEdge::named(source_id.to_string(), relation, table.to_string()));
    }

    fn bind(&mut self, var: String, ty: String) {
        if let Some(m) = self.bindings.last_mut() {
            m.insert(var, ty);
        }
    }

    /// Innermost-first, so a closure sees its enclosing function's typed vars.
    fn lookup(&self, var: &str) -> Option<String> {
        self.bindings.iter().rev().find_map(|m| m.get(var).cloned())
    }

    fn bind_params(&mut self, params: &FunctionLikeParameterList) {
        for p in params.parameters.iter() {
            if let Some(ty) = p.hint.as_ref().and_then(hint_class) {
                self.bind(bytes(p.variable.name), ty);
            }
        }
    }

    fn enter_fn_like(&mut self, id: String, params: &FunctionLikeParameterList) {
        self.scope.push(id);
        self.bindings.push(HashMap::new());
        self.bind_params(params);
    }

    fn leave_fn_like(&mut self) {
        self.scope.pop();
        self.bindings.pop();
    }

    fn enter_class_like(&mut self, name: String, id: String) {
        self.class_stack.push(name);
        self.class_ids.push(id.clone());
        self.scope.push(id);
    }

    fn leave_class_like(&mut self) {
        self.scope.pop();
        self.class_ids.pop();
        self.class_stack.pop();
    }

    fn closure_id(&mut self, start: u32, end: u32) -> (String, String) {
        let name = self.pending_closure_name.take().unwrap_or_else(|| "{closure}".to_string());
        let id = self.push_node(format!("{}.{}", self.cur(), name), name.clone(), "function", start, end);
        (id, name)
    }
}

/// The stateless walker; all state lives in [`Ctx`].
struct Ext;

impl Ext {
    /// Walk a parsed program, filling `ctx`.
    fn run(program: &Program, ctx: &mut Ctx) {
        Self.walk_program(program, ctx);
    }
}

impl<'ast, 'arena> Walker<'ast, 'arena, Ctx> for Ext {
    fn walk_in_class(&self, node: &'ast Class<'arena>, ctx: &mut Ctx) {
        let name = bytes(node.name.value);
        let id = ctx.push_node(format!("{}#{}", ctx.rel, name), name.clone(), "class", node.start_offset(), node.end_offset());
        if let Some(ext) = &node.extends {
            ctx.heritage(&id, ext.types.iter().map(ident_name).collect(), "extends");
        }
        if let Some(imp) = &node.implements {
            ctx.heritage(&id, imp.types.iter().map(ident_name).collect(), "implements");
        }
        // Eloquent model -> table link: explicit `$table = '...'`, else the
        // Laravel snake_case-plural convention (for classes that look like models).
        let extends_names: Vec<String> =
            node.extends.as_ref().map(|e| e.types.iter().map(ident_name).collect()).unwrap_or_default();
        if let Some(table) = model_table(&name, &extends_names, span(&ctx.source, node.start_offset(), node.end_offset())) {
            ctx.link_table(&id, &table, "table");
        }
        ctx.enter_class_like(name, id);
    }
    fn walk_out_class(&self, _n: &'ast Class<'arena>, ctx: &mut Ctx) {
        ctx.leave_class_like();
    }

    fn walk_in_interface(&self, node: &'ast Interface<'arena>, ctx: &mut Ctx) {
        let name = bytes(node.name.value);
        let id = ctx.push_node(format!("{}#{}", ctx.rel, name), name.clone(), "interface", node.start_offset(), node.end_offset());
        if let Some(ext) = &node.extends {
            ctx.heritage(&id, ext.types.iter().map(ident_name).collect(), "extends");
        }
        ctx.enter_class_like(name, id);
    }
    fn walk_out_interface(&self, _n: &'ast Interface<'arena>, ctx: &mut Ctx) {
        ctx.leave_class_like();
    }

    fn walk_in_trait(&self, node: &'ast Trait<'arena>, ctx: &mut Ctx) {
        let name = bytes(node.name.value);
        let id = ctx.push_node(format!("{}#{}", ctx.rel, name), name.clone(), "trait", node.start_offset(), node.end_offset());
        ctx.enter_class_like(name, id);
    }
    fn walk_out_trait(&self, _n: &'ast Trait<'arena>, ctx: &mut Ctx) {
        ctx.leave_class_like();
    }

    fn walk_in_enum(&self, node: &'ast Enum<'arena>, ctx: &mut Ctx) {
        let name = bytes(node.name.value);
        let id = ctx.push_node(format!("{}#{}", ctx.rel, name), name.clone(), "enum", node.start_offset(), node.end_offset());
        if let Some(imp) = &node.implements {
            ctx.heritage(&id, imp.types.iter().map(ident_name).collect(), "implements");
        }
        ctx.enter_class_like(name, id);
    }
    fn walk_out_enum(&self, _n: &'ast Enum<'arena>, ctx: &mut Ctx) {
        ctx.leave_class_like();
    }

    fn walk_in_function(&self, node: &'ast Function<'arena>, ctx: &mut Ctx) {
        let name = bytes(node.name.value);
        let id = ctx.push_node(format!("{}#{}", ctx.rel, name), name, "function", node.start_offset(), node.end_offset());
        ctx.enter_fn_like(id, &node.parameter_list);
    }
    fn walk_out_function(&self, _n: &'ast Function<'arena>, ctx: &mut Ctx) {
        ctx.leave_fn_like();
    }

    fn walk_in_method(&self, node: &'ast Method<'arena>, ctx: &mut Ctx) {
        let name = bytes(node.name.value);
        let cls = ctx.class_stack.last().cloned().unwrap_or_default();
        let id = ctx.push_node(format!("{}#{cls}.{name}", ctx.rel), name, "method", node.start_offset(), node.end_offset());
        ctx.enter_fn_like(id, &node.parameter_list);
    }
    fn walk_out_method(&self, _n: &'ast Method<'arena>, ctx: &mut Ctx) {
        ctx.leave_fn_like();
    }

    fn walk_in_assignment(&self, node: &'ast mago_syntax::cst::cst::Assignment<'arena>, ctx: &mut Ctx) {
        if matches!(node.rhs, Expression::Closure(_) | Expression::ArrowFunction(_)) {
            if let Some(v) = var_name(node.lhs) {
                ctx.pending_closure_name = Some(v.trim_start_matches('$').to_string());
            }
        }
    }

    fn walk_in_closure(&self, node: &'ast Closure<'arena>, ctx: &mut Ctx) {
        let (id, _name) = ctx.closure_id(node.start_offset(), node.end_offset());
        ctx.enter_fn_like(id, &node.parameter_list);
    }
    fn walk_out_closure(&self, _n: &'ast Closure<'arena>, ctx: &mut Ctx) {
        ctx.leave_fn_like();
    }

    fn walk_in_arrow_function(&self, node: &'ast ArrowFunction<'arena>, ctx: &mut Ctx) {
        let (id, _name) = ctx.closure_id(node.start_offset(), node.end_offset());
        ctx.enter_fn_like(id, &node.parameter_list);
    }
    fn walk_out_arrow_function(&self, _n: &'ast ArrowFunction<'arena>, ctx: &mut Ctx) {
        ctx.leave_fn_like();
    }

    fn walk_in_use(&self, node: &'ast Use<'arena>, ctx: &mut Ctx) {
        if let UseItems::Sequence(seq) = &node.items {
            for it in seq.items.iter() {
                let full = ident_full(&it.name);
                ctx.edges.push(RawEdge::named(ctx.rel.clone(), "imports", full));
            }
        }
    }

    fn walk_in_trait_use(&self, node: &'ast TraitUse<'arena>, ctx: &mut Ctx) {
        if let Some(cls) = ctx.scope.last().cloned() {
            ctx.heritage(&cls, node.trait_names.iter().map(ident_name).collect(), "implements");
        }
    }

    fn walk_in_function_call(&self, node: &'ast mago_syntax::cst::cst::FunctionCall<'arena>, ctx: &mut Ctx) {
        if let Some(n) = callee_name(node.function) {
            let src = ctx.cur();
            ctx.edges.push(RawEdge::call(src, n, false, None));
        }
    }

    fn walk_in_method_call(&self, node: &'ast MethodCall<'arena>, ctx: &mut Ctx) {
        let Some(m) = selector_name(&node.method) else { return };
        // Eloquent relation: `$this->belongsTo(Related::class)` -> a
        // model-class -> related-class edge labelled by the relation kind.
        if var_name(node.object).as_deref() == Some("$this") {
            if let Some(kind) = crate::model::relation_kind(&m) {
                if let Some(class_id) = ctx.class_ids.last().cloned() {
                    if let Some(related) = related_model(span(&ctx.source, node.start_offset(), node.end_offset())) {
                        ctx.edges.push(RawEdge::named(class_id, kind, related));
                        return;
                    }
                }
            }
        }
        let recv = match var_name(node.object) {
            Some(v) if v == "$this" => ctx.class_stack.last().cloned(),
            Some(v) => ctx.lookup(&v),
            None => None,
        };
        let src = ctx.cur();
        ctx.edges.push(RawEdge::call(src, m, true, recv));
    }

    fn walk_in_static_method_call(&self, node: &'ast StaticMethodCall<'arena>, ctx: &mut Ctx) {
        let Some(m) = selector_name(&node.method) else { return };
        let recv = match node.class {
            Expression::Identifier(id) => Some(ident_name(id)),
            Expression::Self_(_) | Expression::Static(_) | Expression::Parent(_) => ctx.class_stack.last().cloned(),
            _ => None,
        };
        let src = ctx.cur();
        ctx.edges.push(RawEdge::call(src, m, true, recv));
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use crate::model::{Node, RawEdge};

    fn extract(rel: &str, code: &str) -> (Vec<Node>, Vec<RawEdge>) {
        super::extract(rel, "f.php", code)
    }

    fn has_call(edges: &[RawEdge], name: &str, recv: Option<&str>) -> bool {
        edges
            .iter()
            .any(|e| e.relation == "calls" && e.name.as_deref() == Some(name) && e.recv_type.as_deref() == recv)
    }

    #[test]
    fn extracts_class_methods_and_heritage() {
        let code = "<?php\nclass Foo extends Bar implements Baz {\n    public function m(): int { return $this->n(); }\n    public function n(): int { return 1; }\n}";
        let (nodes, edges) = extract("a.php", code);
        assert!(nodes.iter().any(|n| n.kind == "class" && n.name == "Foo"));
        assert!(nodes.iter().any(|n| n.kind == "method" && n.name == "m"));
        assert!(edges.iter().any(|e| e.relation == "extends" && e.name.as_deref() == Some("Bar")));
        assert!(edges.iter().any(|e| e.relation == "implements" && e.name.as_deref() == Some("Baz")));
        assert!(has_call(&edges, "n", Some("Foo")));
    }

    #[test]
    fn extracts_enum_with_const_member() {
        // An enum with a const member (a shape some tree-sitter-php grammars drop).
        let code = "<?php\nenum Color: string {\n    case Red = 'r';\n    private const MAP = ['r' => 1];\n    public function label(): int { return self::MAP[$this->value] ?? 0; }\n}";
        let (nodes, _) = extract("a.php", code);
        assert!(nodes.iter().any(|n| n.kind == "enum" && n.name == "Color"));
        assert!(nodes.iter().any(|n| n.kind == "method" && n.name == "label"));
    }

    #[test]
    fn extracts_function_and_use_import() {
        let code = "<?php\nuse App\\Models\\User;\nfunction helper(): void { strlen('x'); }";
        let (nodes, edges) = extract("a.php", code);
        assert!(nodes.iter().any(|n| n.kind == "function" && n.name == "helper"));
        assert!(edges.iter().any(|e| e.relation == "imports" && e.name.as_deref() == Some("App\\Models\\User")));
    }

    #[test]
    fn binds_typed_param_for_member_call_receiver() {
        let code = "<?php\nfunction f(Foo $x): void { $x->bar(); }";
        let (_, edges) = extract("a.php", code);
        assert!(has_call(&edges, "bar", Some("Foo")));
    }

    #[test]
    fn records_byte_spans_on_symbols() {
        let code = "<?php\nfunction foo(): void {}";
        let (nodes, _) = extract("a.php", code);
        let foo = nodes.iter().find(|n| n.name == "foo").unwrap();
        assert!(foo.end > foo.start);
    }

    fn has_named(edges: &[RawEdge], relation: &str, name: &str) -> bool {
        edges.iter().any(|e| e.relation == relation && e.name.as_deref() == Some(name))
    }

    #[test]
    fn extracts_eloquent_relations_with_kind() {
        let code = "<?php\nclass Invoice extends Model {\n    public function company() { return $this->belongsTo(Company::class); }\n    public function lines() { return $this->hasMany(InvoiceLine::class, 'invoice_id'); }\n    public function tags() { return $this->belongsToMany(Tag::class); }\n}";
        let (_, edges) = extract("app/Models/Invoice.php", code);
        assert!(has_named(&edges, "belongsTo", "Company"));
        assert!(has_named(&edges, "hasMany", "InvoiceLine"));
        assert!(has_named(&edges, "belongsToMany", "Tag"));
    }

    #[test]
    fn links_model_to_table_explicit_and_by_convention() {
        // Explicit $table wins.
        let (nodes, edges) = extract("app/Models/Foo.php", "<?php\nclass Foo extends Model {\n    protected $table = 'custom_foo';\n}");
        assert!(nodes.iter().any(|n| n.kind == "table" && n.name == "custom_foo"));
        assert!(has_named(&edges, "table", "custom_foo"));
        // Convention: Company -> companies, InvoiceLine -> invoice_lines, Category -> categories.
        let (_, e1) = extract("m.php", "<?php\nclass Company extends Model {}");
        assert!(has_named(&e1, "table", "companies"));
        let (_, e2) = extract("m.php", "<?php\nclass InvoiceLine extends Model {}");
        assert!(has_named(&e2, "table", "invoice_lines"));
        let (_, e3) = extract("m.php", "<?php\nclass Category extends Authenticatable {}");
        assert!(has_named(&e3, "table", "categories"));
        // A non-model class gets no table link.
        let (_, e4) = extract("s.php", "<?php\nclass PriceService { public function go() {} }");
        assert!(!e4.iter().any(|e| e.relation == "table"));
    }

    #[test]
    fn links_migration_to_table_via_schema_calls() {
        let code = "<?php\nreturn new class extends Migration {\n    public function up(): void {\n        Schema::create('companies', function (Blueprint $table) { $table->string('name'); });\n        Schema::table('users', function (Blueprint $table) { $table->string('email'); });\n    }\n};";
        let (nodes, edges) = extract("database/migrations/2024_create.php", code);
        assert!(has_named(&edges, "migrates", "companies"));
        assert!(has_named(&edges, "migrates", "users"));
        assert!(nodes.iter().any(|n| n.kind == "table" && n.name == "companies"));
        // The column name 'name' must NOT be mistaken for a table.
        assert!(!has_named(&edges, "migrates", "name"));
    }

    #[test]
    fn convention_pluralizer_cases() {
        assert_eq!(super::convention_table("Company"), "companies");
        assert_eq!(super::convention_table("Address"), "addresses");
        assert_eq!(super::convention_table("InvoiceLine"), "invoice_lines");
        assert_eq!(super::convention_table("User"), "users");
        assert_eq!(super::convention_table("Category"), "categories");
    }
}
