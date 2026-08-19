//! JS/TS extractor: parses with oxc and emits the same node/edge model as
//! the PHP side. Uses the `Visit` trait's `enter_node`/`leave_node` hooks with a
//! scope stack (containment), a class stack (`this` receiver), and per-function
//! type bindings (typed params + `new`/annotated locals) for member-call
//! receiver resolution -- the JS analogue of the PHP `extract` module.

use std::collections::{HashMap, HashSet};

use oxc::allocator::Allocator;
use oxc::ast::ast::{
    Argument, BindingPattern, CallExpression, Class, ExportDefaultDeclarationKind, Expression,
    FormalParameters, Function, ImportDeclaration, ObjectExpression, ObjectPropertyKind, Program,
    PropertyKey, Statement, TSEnumDeclaration, TSInterfaceDeclaration, TSType, TSTypeAnnotation,
    TSTypeName,
};
use oxc::ast::AstKind;
use oxc::ast_visit::Visit;
use oxc::parser::Parser;
use oxc::span::SourceType;

use crate::model::{Node, RawEdge};

/// Parse one JS/TS source and return its nodes + raw edges (owned; the arena is
/// dropped on return). `rel` is the repo-relative path, `base` the file name.
pub fn extract(rel: &str, base: &str, code: &str, source_type: SourceType) -> (Vec<Node>, Vec<RawEdge>) {
    let allocator = Allocator::default();
    let ret = Parser::new(&allocator, code, source_type).parse();

    let mut ext = JsExt::new(rel);
    ext.nodes.push(Node { id: rel.to_string(), name: base.to_string(), kind: "file", path: rel.to_string(), start: 0, end: 0 });
    ext.minted.insert(rel.to_string());
    ext.visit_program(&ret.program);
    ext.edges.extend(crate::lang::scan(rel, code));
    (ext.nodes, ext.edges)
}

/// Parse a Vue SFC: emit a `component` node (named by `defineOptions` /
/// `defineComponent` / `export default { name }`, else the file stem) that
/// contains the `<script>` setup symbols, then extract the script like a module.
pub fn extract_vue(rel: &str, base: &str, code: &str) -> (Vec<Node>, Vec<RawEdge>) {
    let script = vue_script(code);
    let allocator = Allocator::default();
    let ret = Parser::new(&allocator, &script, SourceType::tsx()).parse();
    let name = detect_component_name(&ret.program).unwrap_or_else(|| component_stem(base));

    let mut ext = JsExt::new(rel);
    ext.nodes.push(Node { id: rel.to_string(), name: base.to_string(), kind: "file", path: rel.to_string(), start: 0, end: 0 });
    ext.minted.insert(rel.to_string());

    let comp_id = format!("{rel}#{name}");
    let script_end = u32::try_from(script.len()).unwrap_or(u32::MAX);
    ext.mint(&comp_id, &name, "component", 0, script_end);
    ext.contains(&comp_id);
    ext.scope.push(comp_id);

    ext.visit_program(&ret.program);
    // Scan the FULL SFC (not just the extracted <script>) so template `$t(...)`
    // usages are captured too.
    ext.edges.extend(crate::lang::scan(rel, code));
    (ext.nodes, ext.edges)
}

/// The component name a Vue SFC declares explicitly, if any.
fn detect_component_name(program: &Program) -> Option<String> {
    program.body.iter().find_map(|stmt| match stmt {
        Statement::ExportDefaultDeclaration(ed) => name_from_default(&ed.declaration),
        Statement::ExpressionStatement(es) => name_from_define(&es.expression),
        _ => None,
    })
}

/// A name from `export default { name }` or `export default defineComponent({ name })`.
fn name_from_default(kind: &ExportDefaultDeclarationKind) -> Option<String> {
    match kind {
        ExportDefaultDeclarationKind::ObjectExpression(o) => name_property(o),
        ExportDefaultDeclarationKind::CallExpression(c) => name_from_call(c),
        _ => None,
    }
}

/// A name from a top-level `defineOptions({ name })` / `defineComponent({ name })`.
fn name_from_define(e: &Expression) -> Option<String> {
    match peel(e) {
        Expression::CallExpression(c) => name_from_call(c),
        _ => None,
    }
}

/// A `name` string from a `define*({...})` call's first object argument.
fn name_from_call(c: &CallExpression) -> Option<String> {
    let is_component_macro = matches!(
        peel(&c.callee),
        Expression::Identifier(id)
            if matches!(id.name.as_str(), "defineComponent" | "defineOptions" | "defineNuxtComponent")
    );
    if !is_component_macro {
        return None;
    }
    match c.arguments.first() {
        Some(Argument::ObjectExpression(o)) => name_property(o),
        _ => None,
    }
}

/// The value of a `name: "..."` string property in an object literal.
fn name_property(o: &ObjectExpression) -> Option<String> {
    string_property(o, "name")
}

/// The value of a `<key>: "..."` string property in an object literal.
fn string_property(o: &ObjectExpression, key: &str) -> Option<String> {
    o.properties.iter().find_map(|p| {
        let ObjectPropertyKind::ObjectProperty(prop) = p else {
            return None;
        };
        let PropertyKey::StaticIdentifier(k) = &prop.key else {
            return None;
        };
        if k.name.as_str() != key {
            return None;
        }
        match peel(&prop.value) {
            Expression::StringLiteral(s) => Some(s.value.as_str().to_string()),
            _ => None,
        }
    })
}

/// The name a class decorator applies, e.g. `@Component({...})` -> `Component`,
/// `@Injectable()` -> `Injectable`.
fn decorator_name(e: &Expression) -> Option<String> {
    match peel(e) {
        Expression::CallExpression(c) => callee_name(&c.callee),
        other => callee_name(other),
    }
}

/// The `selector: '...'` of an Angular `@Component({...})` decorator, if present.
fn component_selector(c: &Class) -> Option<String> {
    c.decorators.iter().find_map(|d| {
        let Expression::CallExpression(call) = peel(&d.expression) else {
            return None;
        };
        if callee_name(&call.callee).as_deref() != Some("Component") {
            return None;
        }
        match call.arguments.first() {
            Some(Argument::ObjectExpression(o)) => string_property(o, "selector"),
            _ => None,
        }
    })
}

/// The component name a `.vue` file implies from its name (`Foo.vue` -> `Foo`).
fn component_stem(base: &str) -> String {
    base.strip_suffix(".vue").unwrap_or(base).to_string()
}

/// Extract the concatenated `<script>` / `<script setup>` bodies of a Vue SFC,
/// so the TS/JS inside a single-file component is parsed like any other module.
pub fn vue_script(code: &str) -> String {
    let mut out = String::new();
    let mut rest = code;
    while let Some(open) = rest.find("<script") {
        let after_tag = &rest[open..];
        let Some(gt) = after_tag.find('>') else { break };
        let body_start = open + gt + 1;
        let Some(close_rel) = rest[body_start..].find("</script>") else { break };
        out.push_str(&rest[body_start..body_start + close_rel]);
        out.push('\n');
        rest = &rest[body_start + close_rel + "</script>".len()..];
    }
    out
}

/// Walker state: owned buffers plus the scope / class / binding stacks.
struct JsExt {
    rel: String,
    file_id: String,
    nodes: Vec<Node>,
    edges: Vec<RawEdge>,
    minted: HashSet<String>,
    scope: Vec<String>,
    class_stack: Vec<String>,
    bindings: Vec<HashMap<String, String>>,
    fn_pushed: Vec<bool>,
    class_pushed: Vec<bool>,
    pending_method: Option<String>,
}

impl JsExt {
    fn new(rel: &str) -> Self {
        Self {
            rel: rel.to_string(),
            file_id: rel.to_string(),
            nodes: Vec::new(),
            edges: Vec::new(),
            minted: HashSet::new(),
            scope: vec![rel.to_string()],
            class_stack: Vec::new(),
            bindings: vec![HashMap::new()],
            fn_pushed: Vec::new(),
            class_pushed: Vec::new(),
            pending_method: None,
        }
    }

    fn mint(&mut self, id: &str, name: &str, kind: &'static str, start: u32, end: u32) {
        if self.minted.insert(id.to_string()) {
            self.nodes.push(Node { id: id.to_string(), name: name.to_string(), kind, path: self.rel.clone(), start, end });
        }
    }

    fn contains(&mut self, child: &str) {
        if let Some(parent) = self.scope.last() {
            self.edges.push(RawEdge::contains(parent.clone(), child.to_string()));
        }
    }

    fn lookup(&self, name: &str) -> Option<String> {
        self.bindings.iter().rev().find_map(|frame| frame.get(name).cloned())
    }

    fn enter_class(&mut self, c: &Class) {
        let Some(id_node) = &c.id else {
            self.class_pushed.push(false);
            return;
        };
        let name = id_node.name.as_str().to_string();
        let id = format!("{}#{}", self.rel, name);
        // Angular: a class decorated with `@Component({...})` is a component, not
        // a plain class. Emit it as `component` and, when the decorator carries a
        // `selector: '...'`, an extra `component` node named by the selector so
        // `find app-foo` locates it too.
        let is_component = c.decorators.iter().any(|d| decorator_name(&d.expression).as_deref() == Some("Component"));
        let kind = if is_component { "component" } else { "class" };
        self.mint(&id, &name, kind, c.span.start, c.span.end);
        self.contains(&id);
        if is_component {
            if let Some(selector) = component_selector(c) {
                let sel_id = format!("{}#{}", self.rel, selector);
                self.mint(&sel_id, &selector, "component", c.span.start, c.span.end);
                self.contains(&sel_id);
            }
        }
        if let Some(heritage) = &c.heritage {
            if let Some(sup) = callee_name(&heritage.expression) {
                self.edges.push(RawEdge::named(id.clone(), "extends", sup));
            }
        }
        for implemented in &c.implements {
            if let Some(n) = type_name(&implemented.expression) {
                self.edges.push(RawEdge::named(id.clone(), "implements", n));
            }
        }
        self.scope.push(id);
        self.class_stack.push(name);
        self.bindings.push(HashMap::new());
        self.class_pushed.push(true);
    }

    fn enter_function(&mut self, f: &Function) {
        if let Some(method) = self.pending_method.take() {
            let class = self.class_stack.last().cloned().unwrap_or_default();
            let id = format!("{}#{}.{}", self.rel, class, method);
            self.mint(&id, &method, "method", f.span.start, f.span.end);
            self.contains(&id);
            self.open_fn_scope(id, f);
        } else if let Some(id_node) = &f.id {
            let name = id_node.name.as_str().to_string();
            let id = format!("{}#{}", self.rel, name);
            self.mint(&id, &name, "function", f.span.start, f.span.end);
            self.contains(&id);
            self.open_fn_scope(id, f);
        } else {
            self.fn_pushed.push(false);
        }
    }

    fn open_fn_scope(&mut self, id: String, f: &Function) {
        self.scope.push(id);
        let mut frame = HashMap::new();
        seed_params(&mut frame, &f.params);
        self.bindings.push(frame);
        self.fn_pushed.push(true);
    }

    fn enter_interface(&mut self, i: &TSInterfaceDeclaration) {
        let name = i.id.name.as_str().to_string();
        let id = format!("{}#{}", self.rel, name);
        self.mint(&id, &name, "interface", i.span.start, i.span.end);
        self.contains(&id);
        for heritage in &i.extends {
            if let Some(n) = type_name(&heritage.type_name) {
                self.edges.push(RawEdge::named(id.clone(), "extends", n));
            }
        }
    }

    fn enter_enum(&mut self, e: &TSEnumDeclaration) {
        let name = e.id.name.as_str().to_string();
        let id = format!("{}#{}", self.rel, name);
        self.mint(&id, &name, "enum", e.span.start, e.span.end);
        self.contains(&id);
    }

    fn capture_binding(&mut self, name: &str, ty: String) {
        if let Some(frame) = self.bindings.last_mut() {
            frame.insert(name.to_string(), ty);
        }
    }

    fn capture_declarator(&mut self, d: &oxc::ast::ast::VariableDeclarator) {
        let BindingPattern::BindingIdentifier(bi) = &d.id else {
            return;
        };
        let Some(ty) = declarator_type(d) else {
            return;
        };
        self.capture_binding(bi.name.as_str(), ty);
    }

    fn record_call(&mut self, callee: &Expression) {
        let Some((name, via_member, recv)) = self.analyze_callee(callee) else {
            return;
        };
        let source = self.scope.last().cloned().unwrap_or_else(|| self.file_id.clone());
        self.edges.push(RawEdge::call(source, name, via_member, recv));
    }

    fn analyze_callee(&self, callee: &Expression) -> Option<(String, bool, Option<String>)> {
        match peel(callee) {
            Expression::Identifier(id) => Some((id.name.as_str().to_string(), false, None)),
            Expression::StaticMemberExpression(m) => {
                Some((m.property.name.as_str().to_string(), true, self.recv_type(&m.object)))
            }
            _ => None,
        }
    }

    fn recv_type(&self, object: &Expression) -> Option<String> {
        match peel(object) {
            Expression::ThisExpression(_) => self.class_stack.last().cloned(),
            Expression::Identifier(id) => self.lookup(id.name.as_str()),
            _ => None,
        }
    }

    fn close_class(&mut self) {
        if self.class_pushed.pop() == Some(true) {
            self.scope.pop();
            self.class_stack.pop();
            self.bindings.pop();
        }
    }

    fn close_function(&mut self) {
        if self.fn_pushed.pop() == Some(true) {
            self.scope.pop();
            self.bindings.pop();
        }
    }
}

impl<'a> Visit<'a> for JsExt {
    fn enter_node(&mut self, kind: AstKind<'a>) {
        match kind {
            AstKind::Class(c) => self.enter_class(c),
            AstKind::Function(f) => self.enter_function(f),
            AstKind::MethodDefinition(md) => self.pending_method = property_key(&md.key),
            AstKind::TSInterfaceDeclaration(i) => self.enter_interface(i),
            AstKind::TSEnumDeclaration(e) => self.enter_enum(e),
            AstKind::ImportDeclaration(imp) => self.record_import(imp),
            AstKind::VariableDeclarator(d) => self.capture_declarator(d),
            AstKind::CallExpression(call) => self.record_call(&call.callee),
            _ => {}
        }
    }

    fn leave_node(&mut self, kind: AstKind<'a>) {
        match kind {
            AstKind::Class(_) => self.close_class(),
            AstKind::Function(_) => self.close_function(),
            _ => {}
        }
    }
}

impl JsExt {
    fn record_import(&mut self, imp: &ImportDeclaration) {
        let source = imp.source.value.as_str().to_string();
        self.edges.push(RawEdge::named(self.file_id.clone(), "imports", source));
    }
}

/// Peel parenthesized / TS wrapper expressions to the underlying value.
fn peel<'a, 'b>(e: &'b Expression<'a>) -> &'b Expression<'a> {
    match e {
        Expression::ParenthesizedExpression(p) => peel(&p.expression),
        Expression::TSAsExpression(t) => peel(&t.expression),
        Expression::TSSatisfiesExpression(t) => peel(&t.expression),
        Expression::TSNonNullExpression(t) => peel(&t.expression),
        _ => e,
    }
}

/// The bare identifier a callee/heritage expression names, if any.
fn callee_name(e: &Expression) -> Option<String> {
    match peel(e) {
        Expression::Identifier(id) => Some(id.name.as_str().to_string()),
        Expression::StaticMemberExpression(m) => Some(m.property.name.as_str().to_string()),
        _ => None,
    }
}

/// The trailing name of a TS type reference (`A.B.C` -> `C`).
fn type_name(n: &TSTypeName) -> Option<String> {
    match n {
        TSTypeName::IdentifierReference(id) => Some(id.name.as_str().to_string()),
        TSTypeName::QualifiedName(q) => Some(q.right.name.as_str().to_string()),
        TSTypeName::ThisExpression(_) => None,
    }
}

/// The referenced type of a `: T` annotation, when `T` is a plain reference.
fn annotation_type(a: &TSTypeAnnotation) -> Option<String> {
    match &a.type_annotation {
        TSType::TSTypeReference(r) => type_name(&r.type_name),
        _ => None,
    }
}

/// A declarator's inferred type: its annotation, else `new X()`'s class.
fn declarator_type(d: &oxc::ast::ast::VariableDeclarator) -> Option<String> {
    if let Some(ann) = &d.type_annotation {
        if let Some(ty) = annotation_type(ann) {
            return Some(ty);
        }
    }
    match d.init.as_ref().map(peel) {
        Some(Expression::NewExpression(n)) => callee_name(&n.callee),
        _ => None,
    }
}

/// The plain name of a method key (identifier or private field).
fn property_key(k: &PropertyKey) -> Option<String> {
    match k {
        PropertyKey::StaticIdentifier(id) => Some(id.name.as_str().to_string()),
        PropertyKey::PrivateIdentifier(id) => Some(id.name.as_str().to_string()),
        _ => None,
    }
}

/// Seed a binding frame with typed parameters (`(x: Foo)` -> `x: Foo`).
fn seed_params(frame: &mut HashMap<String, String>, params: &FormalParameters) {
    for p in &params.items {
        if let BindingPattern::BindingIdentifier(bi) = &p.pattern {
            if let Some(ann) = &p.type_annotation {
                if let Some(ty) = annotation_type(ann) {
                    frame.insert(bi.name.as_str().to_string(), ty);
                }
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::{extract, extract_vue, vue_script};
    use crate::model::{Node, RawEdge};
    use oxc::span::SourceType;

    fn names<'a>(nodes: &'a [Node], kind: &str) -> Vec<&'a str> {
        nodes.iter().filter(|n| n.kind == kind).map(|n| n.name.as_str()).collect()
    }

    fn has_call(edges: &[RawEdge], name: &str, recv: Option<&str>) -> bool {
        edges
            .iter()
            .any(|e| e.relation == "calls" && e.name.as_deref() == Some(name) && e.recv_type.as_deref() == recv)
    }

    #[test]
    fn extracts_class_method_function_and_heritage() {
        let code = r#"
            export class Widget extends Base implements Greeter {
                greet(): string { return this.render(); }
                render(): string { return "x"; }
            }
            export function make(): Widget { return new Widget(); }
        "#;
        let (nodes, edges) = extract("a.ts", "a.ts", code, SourceType::ts());
        assert!(names(&nodes, "class").contains(&"Widget"));
        assert!(names(&nodes, "method").contains(&"greet"));
        assert!(names(&nodes, "function").contains(&"make"));
        assert!(edges.iter().any(|e| e.relation == "extends" && e.name.as_deref() == Some("Base")));
        assert!(edges.iter().any(|e| e.relation == "implements" && e.name.as_deref() == Some("Greeter")));
        assert!(has_call(&edges, "render", Some("Widget")));
    }

    #[test]
    fn detects_angular_component_and_selector() {
        let code = "@Component({ selector: 'app-foo', templateUrl: './foo.html' })\nexport class FooComponent { ngOnInit(): void {} }";
        let (nodes, _) = extract("src/app/foo.component.ts", "foo.component.ts", code, SourceType::ts());
        assert!(nodes.iter().any(|n| n.kind == "component" && n.name == "FooComponent"));
        assert!(nodes.iter().any(|n| n.kind == "component" && n.name == "app-foo"));
        assert!(names(&nodes, "method").contains(&"ngOnInit"));
        // A plain class stays a class.
        let (plain, _) = extract("b.ts", "b.ts", "export class Bar {}", SourceType::ts());
        assert!(names(&plain, "class").contains(&"Bar"));
    }

    #[test]
    fn extracts_interface_and_enum() {
        let code = "export interface I extends J {} export enum Color { Red, Green }";
        let (nodes, _) = extract("a.ts", "a.ts", code, SourceType::ts());
        assert!(names(&nodes, "interface").contains(&"I"));
        assert!(names(&nodes, "enum").contains(&"Color"));
    }

    #[test]
    fn emits_relative_import_edges() {
        let code = "import { Foo } from './foo'; import Bar from '../bar';";
        let (_, edges) = extract("a.ts", "a.ts", code, SourceType::ts());
        let imports: Vec<&str> =
            edges.iter().filter(|e| e.relation == "imports").filter_map(|e| e.name.as_deref()).collect();
        assert!(imports.contains(&"./foo"));
        assert!(imports.contains(&"../bar"));
    }

    #[test]
    fn resolves_typed_param_receiver() {
        let code = "export function use(x: Foo): void { x.bar(); }";
        let (_, edges) = extract("a.ts", "a.ts", code, SourceType::ts());
        assert!(has_call(&edges, "bar", Some("Foo")));
    }

    #[test]
    fn vue_script_extracts_only_the_script_block() {
        let sfc = "<template><div/></template>\n<script setup lang=\"ts\">\nfunction handle() {}\n</script>\n";
        let script = vue_script(sfc);
        assert!(script.contains("function handle"));
        assert!(!script.contains("template"));
    }

    #[test]
    fn extract_vue_emits_a_named_component_node() {
        let sfc = "<script setup lang=\"ts\">\nfunction helper() {}\n</script>\n";
        let (nodes, _) = extract_vue("resources/js/AppComponent.vue", "AppComponent.vue", sfc);
        assert!(nodes.iter().any(|n| n.kind == "component" && n.name == "AppComponent"));
        assert!(names(&nodes, "function").contains(&"helper"));
    }

    #[test]
    fn extract_vue_prefers_define_options_name() {
        let sfc = "<script setup lang=\"ts\">\ndefineOptions({ name: \"CustomName\" });\n</script>\n";
        let (nodes, _) = extract_vue("resources/js/Foo.vue", "Foo.vue", sfc);
        assert!(nodes.iter().any(|n| n.kind == "component" && n.name == "CustomName"));
    }
}
