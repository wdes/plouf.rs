//! PHP format: parse with Mago, walk the CST into the node/edge model, and scan
//! translation keys. All PHP-specific code lives here -- the AST helpers, the
//! walker, and the `Format` entry point.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};

use mago_allocator::LocalArena;
use mago_database::file::File;
use mago_span::HasSpan;
use mago_syntax::cst::cst::{
    Access, ArrowFunction, Class, ClassConstantAccess, ClassLikeMember, ClassLikeMemberSelector, Closure, Enum,
    Expression, Function, FunctionLikeParameterList, Hint, Identifier, IncludeConstruct, IncludeOnceConstruct,
    FunctionPartialApplication, Instantiation, Interface, Method, MethodCall,
    MethodPartialApplication, Property, PropertyItem, RequireConstruct, RequireOnceConstruct, Return,
    StaticMethodCall, StaticMethodPartialApplication, Trait, TraitUse, Use, UseItems, Variable,
};
use mago_syntax::cst::Sequence;
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
    // Laravel table references (`Schema::create('x')`, `DB::table('x')`), scanned
    // from the raw source; join to model `table` edges via the shared `table:` node.
    crate::laravel::scan_tables(rel, code, &mut nodes, &mut edges);
    // Raw-SQL table usages (`FROM`/`JOIN`/`INTO`/`UPDATE <table>`) -> `uses-table`
    // edges, so a hand-written query joins the model/migration at `table:<name>`.
    crate::laravel::scan_raw_sql_tables(rel, code, &mut nodes, &mut edges);
    // Data migrations write rows via an Eloquent model (not Schema/DB::table);
    // link them to the model's table so `callers table:x` sees the seeders.
    crate::laravel::scan_data_migrations(rel, code, &mut edges);
    // Laravel route files (`routes/web.php`, ...) wire controllers via `Route::`
    // calls; link the file to each controller and mint a `route:<path>` node.
    crate::laravel::scan_routes(rel, code, &mut nodes, &mut edges);
    // Attribute routing (`#[Route('/x', ...)]`, `#[Get('/x')]`, ... on a
    // controller): emit a `route:<path>` node + a `serves` edge to the class.
    crate::laravel::scan_route_attributes(rel, code, &mut nodes, &mut edges);
    scan_covers(rel, code, &mut edges);
    // `require`/`include` edges are emitted from the CST during the walk above
    // (see `walk_in_require_construct` &co) -- a keyword in a comment or string
    // is not a construct, so it can never masquerade as a real include.
    scan_twig_functions(rel, code, &mut nodes, &mut edges);
    // Dolibarr extension points: module descriptors, permissions, triggers,
    // hooks, and CommonObject -> table links.
    crate::dolibarr::scan(rel, base, code, &mut nodes, &mut edges);
    edges.extend(crate::lang::scan(rel, code));
    (nodes, edges)
}

// --- AST helpers -----------------------------------------------------------

/// A byte-slice AST value as an owned `String` (lossy on invalid UTF-8).
fn bytes(v: &[u8]) -> String {
    String::from_utf8_lossy(v).into_owned()
}

/// The trailing segment of a `\`-qualified name (`App\Models\User` -> `User`).
pub fn dequalify(name: &str) -> String {
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

/// The property name of a `$this->prop` access (handles `?->` too), so a
/// `$this->prop->method()` receiver can be resolved through the property's
/// declared type. `None` for any other object expression.
fn this_prop_name(expr: &Expression) -> Option<String> {
    let (object, property) = match expr {
        Expression::Access(Access::Property(p)) => (p.object, &p.property),
        Expression::Access(Access::NullSafeProperty(p)) => (p.object, &p.property),
        _ => return None,
    };
    (var_name(object).as_deref() == Some("$this")).then(|| selector_name(property)).flatten()
}

/// Collect the declared class types of a class-like's typed instance properties,
/// keyed by bare property name (`prop` for `public Foo $prop;`), including
/// constructor-promoted params (`__construct(private Foo $prop)`). Untyped or
/// non-class-typed properties are skipped -- they carry no receiver we can wire.
fn collect_prop_types(members: &Sequence<ClassLikeMember>) -> HashMap<String, String> {
    let mut types = HashMap::new();
    for member in members {
        match member {
            ClassLikeMember::Property(prop) => {
                let (hint, items): (_, Vec<&PropertyItem>) = match prop {
                    Property::Plain(p) => (p.hint.as_ref(), p.items.iter().collect()),
                    Property::Hooked(p) => (p.hint.as_ref(), vec![&p.item]),
                };
                if let Some(ty) = hint.and_then(hint_class) {
                    for item in items {
                        let var = match item {
                            PropertyItem::Abstract(a) => a.variable.name,
                            PropertyItem::Concrete(c) => c.variable.name,
                        };
                        types.insert(bytes(var).trim_start_matches('$').to_string(), ty.clone());
                    }
                }
            }
            // Constructor property promotion: a param with a visibility/readonly
            // modifier declares an instance property of the same name.
            ClassLikeMember::Method(m) if bytes(m.name.value) == "__construct" => {
                for p in m.parameter_list.parameters.iter() {
                    if p.modifiers.is_empty() {
                        continue;
                    }
                    if let Some(ty) = p.hint.as_ref().and_then(hint_class) {
                        types.insert(bytes(p.variable.name).trim_start_matches('$').to_string(), ty);
                    }
                }
            }
            _ => {}
        }
    }
    types
}

/// Emit a `requires` file-dependency edge for one `require`/`include` construct,
/// given its path expression `value`. The path is resolved relative to the
/// including file (like a JS relative import): `__DIR__`/`dirname(__FILE__)`
/// anchor to the file's dir; a plain `'x.php'` is treated as relative too. A
/// dynamic path (a `$var`) or an absolute filesystem path is skipped. Because
/// `value` is the path expression *only* (not the rest of the line), and only
/// real `require`/`include` constructs reach here, a keyword inside a comment or
/// string literal can no longer mint a spurious edge.
fn require_edge(value: &Expression<'_>, ctx: &mut Ctx) {
    let slice = span(&ctx.source, value.start_offset(), value.end_offset());
    if let Some(spec) = require_spec(slice) {
        ctx.edges.push(RawEdge::named(ctx.rel.clone(), "requires", spec));
    }
}

/// Scan custom Twig function/filter registrations (`new TwigFunction('name',
/// ...)`, `new TwigFilter('name', ...)`, `->addFunction('name', ...)`,
/// `->addFilter('name', ...)`) and emit a `twigfn:<name>` node + a `defines-fn`
/// edge from the file. A `.twig` template's `{{ name(...) }}` call resolves to
/// this node, so `callers twigfn:foo` lists the templates that use it and the
/// file that registers it.
fn scan_twig_functions(rel: &str, code: &str, nodes: &mut Vec<Node>, edges: &mut Vec<RawEdge>) {
    let mut minted: HashSet<String> = HashSet::new();
    for needle in ["TwigFunction(", "TwigFilter(", "addFunction(", "addFilter("] {
        let mut from = 0;
        while let Some(pos) = code[from..].find(needle) {
            let at = from + pos + needle.len();
            from = at;
            let window = &code[at..code.len().min(at + 120)];
            if let Some(name) = first_string(window) {
                let id = format!("twigfn:{name}");
                if minted.insert(id.clone()) {
                    nodes.push(Node { id, name: name.clone(), kind: "twig-function", path: rel.to_string(), start: 0, end: 0 });
                }
                edges.push(RawEdge::named(rel.to_string(), "defines-fn", name));
            }
        }
    }
}

/// The relative specifier a `require`/`include` statement points at, or `None`
/// for a dynamic (`$var`) or absolute-filesystem path.
fn require_spec(stmt: &str) -> Option<String> {
    if stmt.contains('$') {
        return None; // dynamic base -> not statically resolvable
    }
    let s = first_string(stmt)?;
    let anchored = stmt.contains("__DIR__") || stmt.contains("__FILE__") || stmt.contains("dirname");
    let spec = if let Some(rest) = s.strip_prefix('/') {
        if anchored {
            format!("./{rest}") // `__DIR__ . '/../x.php'` -> `./../x.php`
        } else {
            return None; // absolute filesystem path, not in the repo
        }
    } else if s.starts_with('.') {
        s
    } else {
        format!("./{s}") // `'helpers.php'` -> `./helpers.php`
    };
    // require/include always load a PHP file; a non-`.php` spec is a false match
    // -- the keyword sat inside a comment/string, or the arg is a dynamic build
    // -- so drop it (this kills `]`, `)]`, prose fragments, ...).
    std::path::Path::new(&spec).extension().is_some_and(|e| e.eq_ignore_ascii_case("php")).then_some(spec)
}

/// The first single/double-quoted string literal in `s` (inter-quote content).
fn first_string(s: &str) -> Option<String> {
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

/// A byte range of `src` as a `&str`, clamped to valid bounds. Used to hand a
/// node's source span to the Laravel helpers for text extraction.
fn span(src: &str, start: u32, end: u32) -> &str {
    let s = (start as usize).min(src.len());
    let e = (end as usize).max(s).min(src.len());
    src.get(s..e).unwrap_or("")
}

/// Scan `PHPUnit` coverage declarations and emit a `covers` edge from the test
/// file to each covered class/function (resolved by unique name). Both the
/// modern attributes (`#[CoversClass(X::class)]`, `#[CoversFunction('fn')]`,
/// `#[CoversMethod(X::class, 'm')]`) and legacy docblocks (`@covers X`,
/// `@coversDefaultClass X`) are recognised -- so `callers X` lists its tests.
/// A syntactically valid (possibly namespaced) PHP class name -- no code
/// punctuation. Used to reject fragments a loose attribute scan can pick up.
fn is_class_name(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '\\')
}

fn scan_covers(rel: &str, code: &str, edges: &mut Vec<RawEdge>) {
    for tag in ["#[CoversClass", "#[CoversFunction", "#[CoversMethod"] {
        let mut from = 0;
        while let Some(pos) = code[from..].find(tag) {
            let at = from + pos;
            from = at + tag.len();
            let window = &code[at..code.len().min(at + 256)];
            if let Some(end) = window.find(')') {
                if let Some(target) = crate::laravel::related_model(&window[..end]) {
                    // Guard against a stray string literal in the attribute args
                    // (e.g. a rule test whose subject is `'->default('`): a covered
                    // target must be a class name, not a code fragment.
                    if is_class_name(&target) {
                        edges.push(RawEdge::named(rel.to_string(), "covers", target));
                    }
                }
            }
        }
    }
    for tag in ["@covers ", "@coversDefaultClass "] {
        let mut from = 0;
        while let Some(pos) = code[from..].find(tag) {
            let at = from + pos + tag.len();
            from = at;
            let token: String = code[at..]
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '\\' || *c == ':')
                .collect();
            let class = token.split("::").next().unwrap_or(&token);
            let name = dequalify(class.trim_start_matches('\\'));
            if !name.is_empty() {
                edges.push(RawEdge::named(rel.to_string(), "covers", name));
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
    /// The current class's base-class name (for `parent::`), one per class scope.
    parent_stack: Vec<Option<String>>,
    /// Each class scope's typed instance properties (`prop` -> class), so a
    /// `$this->prop->method()` receiver resolves to the property's type.
    prop_types: Vec<HashMap<String, String>>,
    minted: HashSet<String>,
    bindings: Vec<HashMap<String, String>>,
    pending_closure_name: Option<String>,
    /// Whether a file-scope `return` has already been marked (a config/manifest
    /// file `<?php return [...];`), so the marker edge is pushed at most once.
    returned: bool,
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
            parent_stack: Vec::new(),
            prop_types: Vec::new(),
            minted,
            bindings: Vec::new(),
            pending_closure_name: None,
            returned: false,
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

    fn enter_class_like(&mut self, name: String, id: String, parent: Option<String>, props: HashMap<String, String>) {
        self.class_stack.push(name);
        self.class_ids.push(id.clone());
        self.parent_stack.push(parent);
        self.prop_types.push(props);
        self.scope.push(id);
    }

    fn leave_class_like(&mut self) {
        self.scope.pop();
        self.prop_types.pop();
        self.parent_stack.pop();
        self.class_ids.pop();
        self.class_stack.pop();
    }

    /// The declared class type of the current class's `$this->prop`, if typed.
    fn prop_type(&self, prop: &str) -> Option<String> {
        self.prop_types.last().and_then(|m| m.get(prop).cloned())
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
        if let Some(table) =
            crate::laravel::model_table(&name, &extends_names, span(&ctx.source, node.start_offset(), node.end_offset()))
        {
            ctx.link_table(&id, &table, "table");
        }
        // The first extended class is the `parent::` target for this class scope.
        ctx.enter_class_like(name, id, extends_names.first().cloned(), collect_prop_types(&node.members));
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
        ctx.enter_class_like(name, id, None, HashMap::new());
    }
    fn walk_out_interface(&self, _n: &'ast Interface<'arena>, ctx: &mut Ctx) {
        ctx.leave_class_like();
    }

    fn walk_in_trait(&self, node: &'ast Trait<'arena>, ctx: &mut Ctx) {
        let name = bytes(node.name.value);
        let id = ctx.push_node(format!("{}#{}", ctx.rel, name), name.clone(), "trait", node.start_offset(), node.end_offset());
        ctx.enter_class_like(name, id, None, collect_prop_types(&node.members));
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
        ctx.enter_class_like(name, id, None, collect_prop_types(&node.members));
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
            if let Some(kind) = crate::laravel::relation_kind(&m) {
                if let Some(class_id) = ctx.class_ids.last().cloned() {
                    if let Some(related) = crate::laravel::related_model(span(&ctx.source, node.start_offset(), node.end_offset())) {
                        ctx.edges.push(RawEdge::named(class_id, kind, related));
                        return;
                    }
                }
            }
        }
        let recv = match var_name(node.object) {
            Some(v) if v == "$this" => ctx.class_stack.last().cloned(),
            Some(v) => ctx.lookup(&v),
            // `$this->prop->method()` -> the property's declared class type.
            None => this_prop_name(node.object).and_then(|p| ctx.prop_type(&p)),
        };
        let src = ctx.cur();
        ctx.edges.push(RawEdge::call(src, m, true, recv));
    }

    fn walk_in_static_method_call(&self, node: &'ast StaticMethodCall<'arena>, ctx: &mut Ctx) {
        let Some(m) = selector_name(&node.method) else { return };
        let recv = match node.class {
            Expression::Identifier(id) => Some(ident_name(id)),
            // `self::`/`static::` target the current class; `parent::` targets the
            // base class -- resolving it against the current class would find the
            // overriding method itself (a self-loop), so use the parent's name.
            Expression::Self_(_) | Expression::Static(_) => ctx.class_stack.last().cloned(),
            Expression::Parent(_) => ctx.parent_stack.last().cloned().flatten(),
            _ => None,
        };
        let src = ctx.cur();
        ctx.edges.push(RawEdge::call(src, m, true, recv));
    }

    // First-class callable syntax (`foo(...)`, `$this->m(...)`, `Class::m(...)`)
    // references the callable without invoking it -- emit the same `calls` edge as
    // a real call so a method only ever passed as a callable is not read as dead.
    fn walk_in_function_partial_application(&self, node: &'ast FunctionPartialApplication<'arena>, ctx: &mut Ctx) {
        if let Some(n) = callee_name(node.function) {
            let src = ctx.cur();
            ctx.edges.push(RawEdge::call(src, n, false, None));
        }
    }

    fn walk_in_method_partial_application(&self, node: &'ast MethodPartialApplication<'arena>, ctx: &mut Ctx) {
        let Some(m) = selector_name(&node.method) else { return };
        let recv = match var_name(node.object) {
            Some(v) if v == "$this" => ctx.class_stack.last().cloned(),
            Some(v) => ctx.lookup(&v),
            None => this_prop_name(node.object).and_then(|p| ctx.prop_type(&p)),
        };
        let src = ctx.cur();
        ctx.edges.push(RawEdge::call(src, m, true, recv));
    }

    fn walk_in_static_method_partial_application(&self, node: &'ast StaticMethodPartialApplication<'arena>, ctx: &mut Ctx) {
        let Some(m) = selector_name(&node.method) else { return };
        let recv = match node.class {
            Expression::Identifier(id) => Some(ident_name(id)),
            Expression::Self_(_) | Expression::Static(_) => ctx.class_stack.last().cloned(),
            Expression::Parent(_) => ctx.parent_stack.last().cloned().flatten(),
            _ => None,
        };
        let src = ctx.cur();
        ctx.edges.push(RawEdge::call(src, m, true, recv));
    }

    // `Class::CONST` / `Enum::Case` / `Class::class` -> a `uses-const` edge to the
    // class, so a constant/enum-case registry (route names, status enums) that is
    // only ever read through its members is not read as dead. `self::`/`static::`
    // (a self-reference) and a dynamic class are skipped; `parent::` targets the base.
    fn walk_in_class_constant_access(&self, node: &'ast ClassConstantAccess<'arena>, ctx: &mut Ctx) {
        let class = match node.class {
            Expression::Identifier(id) => Some(ident_name(id)),
            Expression::Parent(_) => ctx.parent_stack.last().cloned().flatten(),
            _ => None,
        };
        if let Some(class) = class {
            let src = ctx.cur();
            ctx.edges.push(RawEdge::named(src, "uses-const", class));
        }
    }

    // `new X(...)` -> an `instantiates` edge to class `X`, so a class only ever
    // constructed (DTOs, controllers, models) is not read as dead. `new $var()` /
    // `new (expr)()` is dynamic and skipped; `new self/static()` targets the class.
    fn walk_in_instantiation(&self, node: &'ast Instantiation<'arena>, ctx: &mut Ctx) {
        let class = match node.class {
            Expression::Identifier(id) => Some(ident_name(id)),
            Expression::Self_(_) | Expression::Static(_) => ctx.class_stack.last().cloned(),
            _ => None,
        };
        if let Some(class) = class {
            let src = ctx.cur();
            ctx.edges.push(RawEdge::named(src, "instantiates", class));
        }
    }

    // `require` / `include` (and their `_once` forms) as file-dependency edges.
    // Sourced from the CST so a keyword sitting in a comment / string / longer
    // identifier is never mistaken for a real include (the old byte-scanner's
    // false-positive class -- a keyword in a comment, a string, or a `PARAM_X`
    // const / `$xInclude` variable name).
    fn walk_in_require_construct(&self, node: &'ast RequireConstruct<'arena>, ctx: &mut Ctx) {
        require_edge(node.value, ctx);
    }
    fn walk_in_require_once_construct(&self, node: &'ast RequireOnceConstruct<'arena>, ctx: &mut Ctx) {
        require_edge(node.value, ctx);
    }
    fn walk_in_include_construct(&self, node: &'ast IncludeConstruct<'arena>, ctx: &mut Ctx) {
        require_edge(node.value, ctx);
    }
    fn walk_in_include_once_construct(&self, node: &'ast IncludeOnceConstruct<'arena>, ctx: &mut Ctx) {
        require_edge(node.value, ctx);
    }

    // A file-scope `return` (empty scope stack) marks a config/manifest file --
    // `config/*.php` returning an array, `bootstrap/app.php` returning the built
    // app. Such a file declares no reusable symbols by design, so `missing` must
    // not flag it as an empty/broken file. Recorded as a `returns` self-edge.
    fn walk_in_return(&self, _node: &'ast Return<'arena>, ctx: &mut Ctx) {
        if ctx.scope.is_empty() && !ctx.returned {
            ctx.returned = true;
            ctx.edges.push(RawEdge::named(ctx.rel.clone(), "returns", ctx.rel.clone()));
        }
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
    fn is_class_name_rejects_code_fragments() {
        assert!(super::is_class_name("App\\Models\\Foo"));
        assert!(super::is_class_name("FooRule"));
        assert!(!super::is_class_name("->default(")); // the covers false-positive
        assert!(!super::is_class_name("foo()"));
        assert!(!super::is_class_name(""));
    }

    #[test]
    fn require_spec_keeps_only_php_targets() {
        // require/include always load a PHP file; garbage / non-.php is dropped.
        assert_eq!(super::require_spec(" 'helpers.php'").as_deref(), Some("./helpers.php"));
        assert_eq!(super::require_spec(" __DIR__ . '/../app.php'").as_deref(), Some("./../app.php"));
        assert_eq!(super::require_spec(" ']'"), None);
        assert_eq!(super::require_spec(" '../assets/x.css'"), None); // not a PHP include
        assert_eq!(super::require_spec(" $dynamic . 'x.php'"), None); // dynamic base
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
    fn registers_custom_twig_functions() {
        let code = "<?php\nclass Ext {\n  public function getFunctions() {\n    return [\n      new TwigFunction('getIcon', [Util::class, 'getIcon']),\n      new TwigFilter('formatBytes', 'format_bytes'),\n    ];\n  }\n}";
        let (nodes, edges) = extract("app/TwigExt.php", code);
        assert!(nodes.iter().any(|n| n.kind == "twig-function" && n.name == "getIcon"));
        assert!(nodes.iter().any(|n| n.kind == "twig-function" && n.name == "formatBytes"));
        assert!(has_named(&edges, "defines-fn", "getIcon"));
        assert!(has_named(&edges, "defines-fn", "formatBytes"));
    }

    #[test]
    fn links_require_include_file_dependencies() {
        let code = "<?php\nrequire __DIR__ . '/../bootstrap/app.php';\ninclude_once 'helpers.php';\nrequire_once dirname(__FILE__) . '/config.php';\nrequire $dynamic . '/x.php';\ninclude '/etc/abs.php';";
        let (_, edges) = extract("app/kernel.php", code);
        assert!(has_named(&edges, "requires", "./../bootstrap/app.php"));
        assert!(has_named(&edges, "requires", "./helpers.php"));
        assert!(has_named(&edges, "requires", "./config.php"));
        // dynamic ($var) and absolute filesystem paths are skipped
        assert!(!edges.iter().any(|e| e.relation == "requires" && e.name.as_deref() == Some("./x.php")));
        assert!(!edges.iter().any(|e| e.relation == "requires" && e.name.as_deref().is_some_and(|n| n.contains("abs"))));
    }

    #[test]
    fn require_keyword_in_comment_string_or_identifier_is_not_an_edge() {
        // The old byte-scanner matched `require`/`include` anywhere in the source
        // -- inside comments, string literals, and const/var names -- and grabbed
        // the next `.php` string as a bogus dependency. The CST walker sees only
        // real constructs, so none of these lines produce a `requires` edge, yet
        // the genuine `require` on the last line still does.
        let code = "<?php\n\
            // TODO: this require in a comment must be ignored, see config/foo.php\n\
            $flag = 'skip require-dev when building bar.php';\n\
            const PARAM_INCLUDE = 'include';\n\
            $includeParameters = ['a.php'];\n\
            require __DIR__ . '/real.php';\n";
        let (_, edges) = extract("app/Foo.php", code);
        let reqs: Vec<&str> = edges
            .iter()
            .filter(|e| e.relation == "requires")
            .filter_map(|e| e.name.as_deref())
            .collect();
        assert_eq!(reqs, vec!["./real.php"], "only the real construct is a dependency");
    }

    #[test]
    fn marks_file_scope_return_as_config_not_empty() {
        // A config/manifest file returns a value at file scope -> `returns` marker.
        let (_, cfg) = extract("config/app.php", "<?php\nreturn [\n  'name' => 'X',\n  'debug' => false,\n];");
        assert!(has_named(&cfg, "returns", "config/app.php"), "config file gets a returns marker");

        // A return *inside* a method is not a file-scope return -> no marker.
        let (_, cls) = extract("app/Foo.php", "<?php\nclass Foo {\n  public function m(): int { return 1; }\n}");
        assert!(!cls.iter().any(|e| e.relation == "returns"), "a method return is not a config marker");
    }

    #[test]
    fn covers_closures_statics_and_duplicate_names() {
        let code = r"<?php
            function a() {}
            function a() {}
            class Base { public function x() {} }
            class P extends Base {
                public function m() { return parent::x(); }
                public function n() {
                    $f = function () { return 1; };
                    $g = fn () => 2;
                    return static::y();
                }
            }
        ";
        let (nodes, edges) = extract("d.php", code);
        assert_eq!(nodes.iter().filter(|n| n.name == "a" && n.kind == "function").count(), 2); // id collision -> ~2
        assert!(has_call(&edges, "x", Some("Base")), "parent:: targets the base class, not self");
        assert!(has_call(&edges, "y", Some("P")), "static:: targets the current class");
        assert!(nodes.iter().any(|n| n.name == "f")); // closure named from its `$f =` assignment
    }

    #[test]
    fn new_expression_links_the_instantiated_class() {
        let code = r"<?php
            class WidgetDto {}
            class Factory {
                public function build($cls) {
                    $a = new WidgetDto();
                    $b = new $cls();
                    return $a;
                }
            }
        ";
        let (_, edges) = extract("f.php", code);
        let inst: Vec<&str> =
            edges.iter().filter(|e| e.relation == "instantiates").filter_map(|e| e.name.as_deref()).collect();
        assert!(inst.contains(&"WidgetDto"), "new WidgetDto() -> instantiates the class");
        assert!(!inst.iter().any(|n| n.contains("cls")), "new $cls() is dynamic -> skipped");
    }

    #[test]
    fn links_phpunit_covers_to_targets() {
        let code = "<?php\n#[CoversClass(Invoice::class)]\n#[CoversFunction('array_flatten')]\n/**\n * @coversDefaultClass \\App\\Services\\Billing\n */\nclass InvoiceTest extends TestCase {}";
        let (_, edges) = extract("tests/Feature/InvoiceTest.php", code);
        assert!(has_named(&edges, "covers", "Invoice")); // CoversClass(X::class)
        assert!(has_named(&edges, "covers", "array_flatten")); // CoversFunction('fn')
        assert!(has_named(&edges, "covers", "Billing")); // @coversDefaultClass (de-qualified)
    }

    #[test]
    fn class_constant_access_links_the_owning_class() {
        // A route/enum registry read only through its members must not look dead.
        let code = r"<?php
            enum Status { case Open; }
            class Route { const IMPORT = 'import'; }
            class Svc {
                public function run() {
                    $a = Route::IMPORT;
                    $b = Status::Open;
                    $c = Route::class;
                    return [$a, $b, $c];
                }
            }
        ";
        let (_, edges) = extract("f.php", code);
        let used: Vec<&str> =
            edges.iter().filter(|e| e.relation == "uses-const").filter_map(|e| e.name.as_deref()).collect();
        assert!(used.contains(&"Route"), "Route::IMPORT / Route::class -> uses-const Route");
        assert!(used.contains(&"Status"), "Status::Open -> uses-const Status");
    }

    #[test]
    fn typed_property_receiver_resolves_the_method_owner() {
        // `$this->prop->method()` resolves through the property's declared type --
        // from a plain declaration, a promoted constructor param, or a `public`
        // property. An untyped property carries no receiver.
        let code = r"<?php
            class Db { public function query($s) {} }
            class Mailer { public function send() {} }
            class Logger { public function info($m) {} }
            class Service {
                private Db $db;
                public Mailer $mail;
                private $untyped;
                public function __construct(private Logger $log) {}
                public function run() {
                    $this->db->query('x');
                    $this->mail->send();
                    $this->log->info('y');
                    $this->untyped->whatever();
                }
            }
        ";
        let (_, edges) = extract("a.php", code);
        assert!(has_call(&edges, "query", Some("Db")), "declared `private Db $db`");
        assert!(has_call(&edges, "send", Some("Mailer")), "`public Mailer $mail`");
        assert!(has_call(&edges, "info", Some("Logger")), "promoted `private Logger $log`");
        assert!(has_call(&edges, "whatever", None), "untyped property -> no receiver");
    }

    #[test]
    fn first_class_callables_emit_the_same_call_edge() {
        // `foo(...)` / `$this->m(...)` / `Class::m(...)` reference a callable without
        // invoking it -- a method only ever passed this way must not look dead.
        let code = r"<?php
            class Base { public function help() {} }
            class Svc extends Base {
                public function run() {
                    $fn = foo(...);
                    $m = $this->scale(...);
                    $s = self::help(...);
                    $st = Base::help(...);
                    return [$fn, $m, $s, $st];
                }
                public function scale() {}
            }
            function foo() {}
        ";
        let (_, edges) = extract("f.php", code);
        assert!(has_call(&edges, "foo", None), "foo(...) -> calls foo");
        assert!(has_call(&edges, "scale", Some("Svc")), "$this->scale(...) -> calls Svc::scale");
        assert!(has_call(&edges, "help", Some("Svc")), "self::help(...) targets the current class");
        assert!(has_call(&edges, "help", Some("Base")), "Base::help(...) targets the named class");
    }
}
