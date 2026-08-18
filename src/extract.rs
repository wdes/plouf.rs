//! The per-file AST walk: turns a parsed `Program` into nodes + raw edges,
//! tracking scope, class stack, and `$var -> class` bindings for receiver-typed
//! call resolution.

use std::collections::{HashMap, HashSet};

use mago_syntax::cst::cst::{
    ArrowFunction, Class, Closure, Enum, Expression, Function, FunctionLikeParameterList,
    Interface, Method, MethodCall, StaticMethodCall, Trait, TraitUse, Use, UseItems,
};
use mago_span::HasSpan;
use mago_syntax::cst::Program;
use mago_syntax::walker::Walker;

use crate::ast::{bytes, callee_name, hint_class, ident_full, ident_name, selector_name, var_name};
use crate::model::{Node, RawEdge};

/// Accumulator threaded through the walk (the walker itself is stateless).
pub struct Ctx {
    pub rel: String,
    pub nodes: Vec<Node>,
    pub edges: Vec<RawEdge>,
    scope: Vec<String>,
    class_stack: Vec<String>,
    minted: HashSet<String>,
    bindings: Vec<HashMap<String, String>>,
    pending_closure_name: Option<String>,
}

impl Ctx {
    pub fn new(rel: String) -> Self {
        let mut minted = HashSet::new();
        minted.insert(rel.clone());
        Self {
            rel,
            nodes: Vec::new(),
            edges: Vec::new(),
            scope: Vec::new(),
            class_stack: Vec::new(),
            minted,
            bindings: Vec::new(),
            pending_closure_name: None,
        }
    }

    /// Seed the file node before walking (file body is read wholesale, so its
    /// span is left at 0..0).
    pub fn push_file(&mut self, name: String) {
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
        self.scope.push(id);
    }

    fn leave_class_like(&mut self) {
        self.scope.pop();
        self.class_stack.pop();
    }

    fn closure_id(&mut self, start: u32, end: u32) -> (String, String) {
        let name = self.pending_closure_name.take().unwrap_or_else(|| "{closure}".to_string());
        let id = self.push_node(format!("{}.{}", self.cur(), name), name.clone(), "function", start, end);
        (id, name)
    }
}

/// The stateless walker; all state lives in [`Ctx`].
pub struct Ext;

impl Ext {
    /// Walk a parsed program, filling `ctx`.
    pub fn run(program: &Program, ctx: &mut Ctx) {
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
    use super::{Ctx, Ext};
    use crate::model::{Node, RawEdge};
    use mago_allocator::LocalArena;
    use mago_database::file::File;
    use mago_syntax::parser::parse_file;
    use std::borrow::Cow;

    fn extract(rel: &str, code: &str) -> (Vec<Node>, Vec<RawEdge>) {
        let arena = LocalArena::new();
        let file =
            File::ephemeral(Cow::Owned(rel.to_string().into_bytes()), Cow::Owned(code.to_string().into_bytes()));
        let program = parse_file(&arena, &file);
        let mut ctx = Ctx::new(rel.to_string());
        ctx.push_file("f.php".to_string());
        Ext::run(program, &mut ctx);
        (ctx.nodes, ctx.edges)
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
}
