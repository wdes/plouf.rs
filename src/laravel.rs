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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::{convention_table, related_model, scan_tables};
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
