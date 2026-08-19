//! Blade template extraction (`*.blade.php`).
//!
//! Blade is NOT AST-parsed by Mago (its `@directive` / `{{ }}` / `<x-...>`
//! syntax is not valid PHP, so the parser would choke). Instead we scan the
//! raw text by hand -- comment/verbatim stripping followed by a few small,
//! quote/paren-aware passes -- and emit exactly two graph relations:
//!
//! * `"includes"` -- a reference to another Blade view by its dotted name
//!   (the whole `@include*` / `@each` / `@extends` / `@component` / `<x-...>`
//!   family), so "who references view X" is answerable with one query.
//! * `"uses-lang"` -- a translation-key usage (`@lang` / `@choice` and the
//!   `__` / `trans` / `trans_choice` helpers anywhere in the text).
//!
//! Targets are kept RAW (the dotted view name / the translation key); the
//! dotted-name -> file-path resolution happens later in `resolve.rs`. Only a
//! captured STRING LITERAL (single or double quoted) produces an edge; a purely
//! dynamic reference such as `@include($var)` is skipped.

use crate::format::Format;
use crate::model::{Node, RawEdge};

/// The Blade format: routes every `*.blade.php`. Registered before PHP so these
/// templates never reach Mago (which chokes on `@directive` / `{{ }}`).
pub struct Blade;

impl Format for Blade {
    fn matches(&self, base: &str, _ext: &str) -> bool {
        base.ends_with(".blade.php")
    }

    fn extract(&self, rel: &str, base: &str, code: &str) -> (Vec<Node>, Vec<RawEdge>) {
        extract(rel, base, code)
    }
}

/// Extract a Blade template (`*.blade.php`) into nodes + raw edges.
///
/// Emits exactly one `file` node (id = `rel`, span `0..0`, matching the shape
/// `Ctx::push_file` seeds for PHP files) plus `includes` / `uses-lang` edges
/// whose `source` is the file id.
pub fn extract(rel: &str, base: &str, code: &str) -> (Vec<Node>, Vec<RawEdge>) {
    let mut nodes = Vec::new();
    let mut edges = Vec::new();
    nodes.push(Node {
        id: rel.to_string(),
        name: base.to_string(),
        kind: "file",
        path: rel.to_string(),
        start: 0,
        end: 0,
    });

    // Drop `{{-- --}}` comments and `@verbatim ... @endverbatim` blocks first:
    // both suppress directive/echo processing, so nothing inside them is a real
    // reference (e.g. `{{-- @include('x') --}}` must NOT emit an edge).
    let cleaned = strip_between(code, "{{--", "--}}");
    let cleaned = strip_between(&cleaned, "@verbatim", "@endverbatim");

    scan_directives(&cleaned, rel, &mut edges);
    scan_components(&cleaned, rel, &mut edges);
    scan_translations(&cleaned, rel, &mut edges);

    (nodes, edges)
}

/// Remove every `open ... close` region from `src` (non-nesting; an unterminated
/// `open` drops the remainder). Both delimiters are ASCII, so all slice indices
/// land on char boundaries.
fn strip_between(src: &str, open: &str, close: &str) -> String {
    let mut out = String::with_capacity(src.len());
    let mut rest = src;
    while let Some(i) = rest.find(open) {
        out.push_str(&rest[..i]);
        let after_open = &rest[i + open.len()..];
        let Some(j) = after_open.find(close) else {
            rest = "";
            break;
        };
        rest = &after_open[j + close.len()..];
    }
    out.push_str(rest);
    out
}

/// The byte index of the `)` matching the `(` at `open`, honouring nested parens
/// and single/double quoted string literals (with backslash escapes). None if
/// unbalanced.
fn matching_paren(bytes: &[u8], open: usize) -> Option<usize> {
    let mut depth: i32 = 0;
    let mut quote: Option<u8> = None;
    let mut escaped = false;
    let mut i = open;
    while i < bytes.len() {
        let c = bytes[i];
        if let Some(q) = quote {
            if escaped {
                escaped = false;
            } else if c == b'\\' {
                escaped = true;
            } else if c == q {
                quote = None;
            }
        } else {
            match c {
                b'\'' | b'"' => quote = Some(c),
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

/// The first single/double-quoted string literal in `s`, unescaped-quote content,
/// or None when there is none.
fn first_string(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        if c == b'\'' || c == b'"' {
            return read_literal(s, i, c);
        }
        i += 1;
    }
    None
}

/// Every string literal in `s`, in order (used for `@includeFirst`'s array of
/// views).
fn all_strings(s: &str) -> Vec<String> {
    let bytes = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        if c == b'\'' || c == b'"' {
            match read_literal(s, i, c) {
                // `read_literal` returns the raw inter-quote slice, so the
                // closing quote sits at `i + 1 + lit.len()`; step just past it.
                Some(lit) => {
                    i += lit.len() + 2;
                    out.push(lit);
                    continue;
                }
                None => break,
            }
        }
        i += 1;
    }
    out
}

/// Read a quoted literal whose opening quote (char `q`) is the byte at `open`,
/// returning the raw slice between the quotes. None if unterminated. Both quotes
/// are ASCII, so the returned slice is a valid substring.
fn read_literal(s: &str, open: usize, q: u8) -> Option<String> {
    let bytes = s.as_bytes();
    let mut j = open + 1;
    let mut escaped = false;
    while j < bytes.len() {
        let c = bytes[j];
        if escaped {
            escaped = false;
        } else if c == b'\\' {
            escaped = true;
        } else if c == q {
            return Some(s[open + 1..j].to_string());
        }
        j += 1;
    }
    None
}

/// Split `s` into top-level comma-separated arguments, respecting quotes and
/// nested `()`/`[]`/`{}`. Commas are ASCII, so each returned slice is valid.
fn split_args(s: &str) -> Vec<&str> {
    let bytes = s.as_bytes();
    let mut parts = Vec::new();
    let mut depth: i32 = 0;
    let mut quote: Option<u8> = None;
    let mut escaped = false;
    let mut start = 0;
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        if let Some(qc) = quote {
            if escaped {
                escaped = false;
            } else if c == b'\\' {
                escaped = true;
            } else if c == qc {
                quote = None;
            }
        } else {
            match c {
                b'\'' | b'"' => quote = Some(c),
                b'(' | b'[' | b'{' => depth += 1,
                b')' | b']' | b'}' => depth -= 1,
                b',' if depth == 0 => {
                    parts.push(&s[start..i]);
                    start = i + 1;
                }
                _ => {}
            }
        }
        i += 1;
    }
    parts.push(&s[start..]);
    parts
}

/// Is `b` an ASCII whitespace byte (space / tab / CR / LF)?
const fn is_ws(b: u8) -> bool {
    b == b' ' || b == b'\t' || b == b'\n' || b == b'\r'
}

/// Scan `@directive(...)` occurrences and emit `includes` / `uses-lang` edges
/// for the view-reference and translation directive families.
fn scan_directives(text: &str, rel: &str, edges: &mut Vec<RawEdge>) {
    let bytes = text.as_bytes();
    let n = bytes.len();
    let mut i = 0;
    while i < n {
        if bytes[i] != b'@' {
            i += 1;
            continue;
        }
        // `@@directive` is an escaped literal `@directive`, not a directive.
        if i > 0 && bytes[i - 1] == b'@' {
            i += 1;
            continue;
        }
        // Directive name: ASCII letters only.
        let mut j = i + 1;
        while j < n && bytes[j].is_ascii_alphabetic() {
            j += 1;
        }
        if j == i + 1 {
            i += 1;
            continue;
        }
        let name = &text[i + 1..j];
        // Optional whitespace then a `(` opens the argument list.
        let mut k = j;
        while k < n && is_ws(bytes[k]) {
            k += 1;
        }
        if k < n && bytes[k] == b'(' {
            if let Some(end) = matching_paren(bytes, k) {
                let args = &text[k + 1..end];
                handle_directive(name, args, rel, edges);
                i = end + 1;
                continue;
            }
        }
        i += 1;
    }
}

/// Emit the edge(s) for one directive, or nothing for a directive that
/// references neither a view nor a translation key.
fn handle_directive(name: &str, args: &str, rel: &str, edges: &mut Vec<RawEdge>) {
    match name {
        // View references: the view is the first string literal.
        // `@extends` is layout inheritance -- same `includes` relation so one
        // query covers every view reference. `@livewire('name')` references a
        // Livewire component by its registered name.
        "include" | "includeIf" | "extends" | "component" | "each" | "livewire" => {
            if let Some(view) = first_string(args) {
                edges.push(RawEdge::named(rel.to_string(), "includes", view));
            }
        }
        // Conditional includes: `@includeWhen($bool, 'view', $data)` /
        // `@includeUnless($bool, 'view', $data)` -- the view is the SECOND
        // argument, so read the string literal there (avoids a string inside
        // the boolean condition being mistaken for the view).
        "includeWhen" | "includeUnless" => {
            let parts = split_args(args);
            if let Some(second) = parts.get(1) {
                if let Some(view) = first_string(second) {
                    edges.push(RawEdge::named(rel.to_string(), "includes", view));
                }
            }
        }
        // `@includeFirst(['a', 'b'], $data)` -- every view in the first array
        // argument (the second arg is data, so scope to the array).
        "includeFirst" => {
            let parts = split_args(args);
            let scope = parts.first().copied().unwrap_or(args);
            for view in all_strings(scope) {
                edges.push(RawEdge::named(rel.to_string(), "includes", view));
            }
        }
        // Translation directives: the key is the first string literal.
        "lang" | "choice" => {
            if let Some(key) = first_string(args) {
                edges.push(RawEdge::named(rel.to_string(), "uses-lang", key));
            }
        }
        _ => {}
    }
}

/// Scan `<x-...>` component tags and emit an `includes` edge with the dotted
/// component/view name (`<x-foo.bar>` -> `foo.bar`, `<x-foo />` -> `foo`).
/// Skips `<x-slot>` / `<x-slot:name>` (a slot, not a view) and
/// `<x-dynamic-component>` (a dynamic reference with no literal name).
fn scan_components(text: &str, rel: &str, edges: &mut Vec<RawEdge>) {
    for (idx, _) in text.match_indices("<x-") {
        let rest = &text[idx + 3..];
        let name: String = rest
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '.' || *c == '_' || *c == '-' || *c == ':')
            .collect();
        if name.is_empty() {
            continue;
        }
        if name == "slot" || name.starts_with("slot:") || name == "dynamic-component" {
            continue;
        }
        edges.push(RawEdge::named(rel.to_string(), "includes", name));
    }
    // Livewire tags: `<livewire:foo-bar />` renders the Livewire component
    // `foo-bar` (bladestan resolves these too). The name is a `:`-prefixed,
    // kebab/dotted identifier.
    for (idx, _) in text.match_indices("<livewire:") {
        let rest = &text[idx + "<livewire:".len()..];
        let name: String =
            rest.chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '.' || *c == '_' || *c == '-').collect();
        if !name.is_empty() {
            edges.push(RawEdge::named(rel.to_string(), "includes", name));
        }
    }
}

/// Scan the `__` / `trans` / `trans_choice` translation helpers anywhere in the
/// text and emit a `uses-lang` edge for each call with a first string-literal
/// argument. `trans_choice` is scanned before `trans` so it is not double-counted
/// (the `trans(` boundary check already excludes it, but order keeps intent
/// clear).
fn scan_translations(text: &str, rel: &str, edges: &mut Vec<RawEdge>) {
    scan_call(text, "trans_choice", rel, edges);
    scan_call(text, "trans", rel, edges);
    scan_call(text, "__", rel, edges);
}

/// Emit a `uses-lang` edge for every `fname(<string>, ...)` call in `text`. A
/// preceding identifier character (`[A-Za-z0-9_$\]`) rules out a longer name
/// (`mytrans(`, `->__construct(`, ...); `trans(` before `_` never matches
/// `trans_choice`).
fn scan_call(text: &str, fname: &str, rel: &str, edges: &mut Vec<RawEdge>) {
    let bytes = text.as_bytes();
    for (idx, _) in text.match_indices(fname) {
        if idx > 0 {
            let prev = bytes[idx - 1];
            if prev.is_ascii_alphanumeric() || prev == b'_' || prev == b'$' || prev == b'\\' {
                continue;
            }
        }
        let mut k = idx + fname.len();
        while k < bytes.len() && is_ws(bytes[k]) {
            k += 1;
        }
        if k < bytes.len() && bytes[k] == b'(' {
            if let Some(end) = matching_paren(bytes, k) {
                let args = &text[k + 1..end];
                if let Some(key) = first_string(args) {
                    edges.push(RawEdge::named(rel.to_string(), "uses-lang", key));
                }
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::extract;
    use crate::model::RawEdge;

    fn has(edges: &[RawEdge], relation: &str, name: &str) -> bool {
        edges
            .iter()
            .any(|e| e.relation == relation && e.name.as_deref() == Some(name))
    }

    fn count(edges: &[RawEdge], relation: &str) -> usize {
        edges.iter().filter(|e| e.relation == relation).count()
    }

    #[test]
    fn emits_exactly_one_file_node() {
        let (nodes, _) = extract("resources/views/x.blade.php", "x.blade.php", "<p>hi</p>");
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0].kind, "file");
        assert_eq!(nodes[0].id, "resources/views/x.blade.php");
        assert_eq!(nodes[0].name, "x.blade.php");
        assert_eq!(nodes[0].start, 0);
        assert_eq!(nodes[0].end, 0);
    }

    #[test]
    fn captures_extends_include_and_each_views() {
        let code = "@extends('layouts.app')\n@include('partials.header')\n@each('rows.item', $rows, 'row')";
        let (_, edges) = extract("v.blade.php", "v.blade.php", code);
        assert!(has(&edges, "includes", "layouts.app"));
        assert!(has(&edges, "includes", "partials.header"));
        assert!(has(&edges, "includes", "rows.item"));
    }

    #[test]
    fn maps_component_tag_to_dotted_name() {
        let code = "<x-foo.bar class=\"a\" :x=\"$y\" />\n<x-alert>msg</x-alert>";
        let (_, edges) = extract("v.blade.php", "v.blade.php", code);
        assert!(has(&edges, "includes", "foo.bar"));
        assert!(has(&edges, "includes", "alert"));
    }

    #[test]
    fn captures_livewire_tags_and_directive() {
        let code = "<livewire:user.profile :id=\"$id\" />\n<livewire:nav-bar />\n@livewire('admin.dashboard', ['x' => 1])";
        let (_, edges) = extract("v.blade.php", "v.blade.php", code);
        assert!(has(&edges, "includes", "user.profile"));
        assert!(has(&edges, "includes", "nav-bar"));
        assert!(has(&edges, "includes", "admin.dashboard"));
    }

    #[test]
    fn keeps_namespaced_component_name_raw() {
        let code = "<x-mail::message>Body</x-mail::message>";
        let (_, edges) = extract("v.blade.php", "v.blade.php", code);
        assert!(has(&edges, "includes", "mail::message"));
    }

    #[test]
    fn ignores_slot_and_dynamic_component_tags() {
        let code = "<x-slot:title>T</x-slot:title>\n<x-slot name=\"body\"></x-slot>\n<x-dynamic-component :component=\"$c\" />";
        let (_, edges) = extract("v.blade.php", "v.blade.php", code);
        assert_eq!(count(&edges, "includes"), 0);
    }

    #[test]
    fn captures_lang_choice_and_helper_keys() {
        let code =
            "@lang('messages.welcome')\n@choice('messages.apples', $n)\n<p>{{ __('nav.home') }}</p>\n{!! trans('nav.about') !!}\n{{ trans_choice('items.count', $n) }}";
        let (_, edges) = extract("v.blade.php", "v.blade.php", code);
        assert!(has(&edges, "uses-lang", "messages.welcome"));
        assert!(has(&edges, "uses-lang", "messages.apples"));
        assert!(has(&edges, "uses-lang", "nav.home"));
        assert!(has(&edges, "uses-lang", "nav.about"));
        assert!(has(&edges, "uses-lang", "items.count"));
    }

    #[test]
    fn helper_in_html_attribute_is_captured() {
        let code = "<input placeholder=\"{{ __('form.name') }}\" value=\"{{ trans('form.value') }}\">";
        let (_, edges) = extract("v.blade.php", "v.blade.php", code);
        assert!(has(&edges, "uses-lang", "form.name"));
        assert!(has(&edges, "uses-lang", "form.value"));
    }

    #[test]
    fn handles_multiline_and_spacey_directives() {
        let code = "@include (\n    'partials.footer',\n    ['k' => $v]\n)\n@lang( 'messages.bye' )";
        let (_, edges) = extract("v.blade.php", "v.blade.php", code);
        assert!(has(&edges, "includes", "partials.footer"));
        assert!(has(&edges, "uses-lang", "messages.bye"));
    }

    #[test]
    fn include_when_and_unless_take_the_second_argument_view() {
        let code = "@includeWhen($ok, 'alerts.ok', ['x' => 'ignored'])\n@includeUnless($bad, 'alerts.bad')";
        let (_, edges) = extract("v.blade.php", "v.blade.php", code);
        assert!(has(&edges, "includes", "alerts.ok"));
        assert!(has(&edges, "includes", "alerts.bad"));
        assert!(!has(&edges, "includes", "ignored"));
    }

    #[test]
    fn include_first_captures_every_view_in_the_array() {
        let code = "@includeFirst(['custom.admin', 'admin.index'], ['status' => 'ok'])";
        let (_, edges) = extract("v.blade.php", "v.blade.php", code);
        assert!(has(&edges, "includes", "custom.admin"));
        assert!(has(&edges, "includes", "admin.index"));
        assert!(!has(&edges, "includes", "ok"));
    }

    #[test]
    fn include_if_and_component_are_captured() {
        let code = "@includeIf('maybe.here', $data)\n@component('mail.message')\n@endcomponent";
        let (_, edges) = extract("v.blade.php", "v.blade.php", code);
        assert!(has(&edges, "includes", "maybe.here"));
        assert!(has(&edges, "includes", "mail.message"));
    }

    #[test]
    fn dynamic_include_emits_no_edge() {
        let code = "@include($view)\n@include ( $another )";
        let (_, edges) = extract("v.blade.php", "v.blade.php", code);
        assert_eq!(count(&edges, "includes"), 0);
    }

    #[test]
    fn commented_directive_emits_no_edge() {
        let code = "{{-- @include('ignored.partial') and {{ __('ignored.key') }} --}}\n@include('real.partial')";
        let (_, edges) = extract("v.blade.php", "v.blade.php", code);
        assert!(has(&edges, "includes", "real.partial"));
        assert!(!has(&edges, "includes", "ignored.partial"));
        assert!(!has(&edges, "uses-lang", "ignored.key"));
    }

    #[test]
    fn verbatim_block_is_not_scanned() {
        let code = "@verbatim\n@include('literal.text')\n{{ __('literal.key') }}\n@endverbatim\n@include('kept.view')";
        let (_, edges) = extract("v.blade.php", "v.blade.php", code);
        assert!(has(&edges, "includes", "kept.view"));
        assert!(!has(&edges, "includes", "literal.text"));
        assert!(!has(&edges, "uses-lang", "literal.key"));
    }

    #[test]
    fn escaped_directive_is_ignored() {
        let code = "@@include('shown.literally')\n@include('processed.view')";
        let (_, edges) = extract("v.blade.php", "v.blade.php", code);
        assert!(has(&edges, "includes", "processed.view"));
        assert!(!has(&edges, "includes", "shown.literally"));
    }

    #[test]
    fn control_flow_and_asset_directives_emit_nothing() {
        let code = "@if ($x) @foreach ($xs as $y) @endforeach @endif\n@csrf @method('PUT')\n@section('content') @yield('sidebar') @stop\n@push('scripts') @endpush\n@vite(['resources/js/app.ts'])\n@php $z = __('php.key'); @endphp";
        let (_, edges) = extract("v.blade.php", "v.blade.php", code);
        assert_eq!(count(&edges, "includes"), 0);
        // The helper inside a @php block is live PHP, so its key IS captured;
        // @method/@section/@yield/@push/@vite names are not views/keys.
        assert!(has(&edges, "uses-lang", "php.key"));
        assert_eq!(count(&edges, "uses-lang"), 1);
    }

    #[test]
    fn css_at_rules_do_not_false_match() {
        let code = "<style>@media (max-width: 600px) { a { transition: none; } }\n@page { margin: 1mm; }</style>\n@include('real.one')";
        let (_, edges) = extract("v.blade.php", "v.blade.php", code);
        assert!(has(&edges, "includes", "real.one"));
        assert_eq!(count(&edges, "includes"), 1);
        assert_eq!(count(&edges, "uses-lang"), 0);
    }
}
