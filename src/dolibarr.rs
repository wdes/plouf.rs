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

/// Run every Dolibarr scanner over one PHP file, appending to `nodes`/`edges`.
pub fn scan(rel: &str, base: &str, code: &str, nodes: &mut Vec<Node>, edges: &mut Vec<RawEdge>) {
    scan_module(rel, code, nodes, edges);
    scan_permissions(rel, code, nodes, edges);
    scan_triggers(rel, code, nodes, edges);
    scan_hooks(rel, base, code, nodes, edges);
    scan_common_object(rel, code, nodes, edges);
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
}

/// `$user->hasRight('module','level1'[,'level2'])` -> a
/// `right:<module>.<l1>[.<l2>]` node + a `checks-permission` edge. The module
/// (first argument) must be a string literal; a dynamic `hasRight($m, ...)` is
/// skipped.
fn scan_permissions(rel: &str, code: &str, nodes: &mut Vec<Node>, edges: &mut Vec<RawEdge>) {
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
        edges.push(RawEdge::named(rel.to_string(), "checks-permission", name.clone()));
        mint(&mut minted, nodes, node(rel, "permission", &name));
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
    // core CommonHookActions base). Its methods named like a known hook handle it.
    if base.starts_with("actions_") || code.contains("CommonHookActions") {
        for method in defined_methods(code).into_iter().filter(|m| HOOK_METHODS.contains(&m.as_str())) {
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
fn matching_paren(bytes: &[u8], open: usize) -> Option<usize> {
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
fn skip_string(bytes: &[u8], mut i: usize, q: u8) -> usize {
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
                }
            }
        ";
        let (nodes, edges) = run("acme/core/modules/modAcme.class.php", "modAcme.class.php", code);
        assert!(nodes.iter().any(|n| n.kind == "module" && n.id == "module:acme"));
        assert_eq!(edge_targets(&edges, "declares-module"), vec!["acme"]);
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
