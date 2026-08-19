//! Translation-key usage scan: a uniform textual pass over any source (PHP,
//! JS/TS/Vue, Blade) that captures translation-function calls carrying a
//! string-literal key, emitting `uses-lang` edges from the file. Text rather
//! than AST on purpose -- it covers Vue `<template>` `$t(...)` and phpMyAdmin
//! gettext that the language parsers never see, with one code path for every
//! surface. The target of a `uses-lang` edge is the raw key (never resolved to
//! a node, like an external call name); the `uses` query verb reads them back.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;

use crate::model::RawEdge;

/// The translation functions we recognise, each with the argument index that
/// holds the key. `0` = first string arg (the common case: Laravel `__`/`trans`
/// /`trans_choice`, Vue `t`/`$t`/`tc`/`te`, gettext `_gettext`/`_ngettext`).
/// `_pgettext(context, message)` keeps the key in its second arg.
const FNS: &[(&str, usize)] = &[
    ("__", 0),
    ("trans", 0),
    ("trans_choice", 0),
    ("_gettext", 0),
    ("_ngettext", 0),
    ("_pgettext", 1),
    ("t", 0),
    ("tc", 0),
    ("te", 0),
    ("$t", 0),
    ("$tc", 0),
    ("$te", 0),
    ("instant", 0), // Angular ngx-translate: `TranslateService.instant('KEY')`
];

/// Pipe names whose left operand is a translation key: Angular ngx-translate's
/// `'KEY' | translate` and transloco's `'KEY' | transloco`.
const PIPES: &[&str] = &["translate", "transloco"];

/// Identifier byte: ASCII alphanumeric, `_`, or `$` (so `$t` and `__` read as
/// single tokens and word boundaries are exact).
const fn is_ident(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'$'
}

fn skip_ws(bytes: &[u8], mut i: usize) -> usize {
    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
        i += 1;
    }
    i
}

/// Read a single/double-quoted string literal whose opening quote is at `i`.
/// Returns the decoded inner text and the index just past the closing quote.
/// Escapes keep the escaped char verbatim (`\'` -> `'`) -- enough for keys.
fn read_string(bytes: &[u8], i: usize) -> Option<(String, usize)> {
    let quote = *bytes.get(i)?;
    if quote != b'\'' && quote != b'"' {
        return None;
    }
    let mut j = i + 1;
    let mut buf: Vec<u8> = Vec::new();
    while j < bytes.len() {
        match bytes[j] {
            b'\\' => {
                let next = *bytes.get(j + 1)?;
                buf.push(next);
                j += 2;
            }
            c if c == quote => return Some((String::from_utf8_lossy(&buf).into_owned(), j + 1)),
            c => {
                buf.push(c);
                j += 1;
            }
        }
    }
    None
}

/// The string literal at argument `index` (0-based), where `after_paren` points
/// just past the call's `(`. Every argument up to and including the target must
/// be a string literal (true for the functions we scan); anything else bails.
fn key_arg(bytes: &[u8], after_paren: usize, index: usize) -> Option<String> {
    let mut i = skip_ws(bytes, after_paren);
    let mut current = 0;
    loop {
        let (val, next) = read_string(bytes, i)?;
        if current == index {
            return Some(val);
        }
        i = skip_ws(bytes, next);
        if bytes.get(i) != Some(&b',') {
            return None;
        }
        i = skip_ws(bytes, i + 1);
        current += 1;
    }
}

/// Scan `code` for translation-key usages, emitting a `uses-lang` edge from
/// `source_id` (the file id) per captured key. Two forms: the call syntax
/// `name('KEY', ...)` and the Angular template pipe `'KEY' | translate`.
pub fn scan(source_id: &str, code: &str) -> Vec<RawEdge> {
    let mut out = Vec::new();
    scan_calls(source_id, code, &mut out);
    scan_pipe(source_id, code, &mut out);
    out
}

/// Call form: `name('KEY', ...)` for the `FNS` set. Whole-identifier-token
/// matching keeps `__` from firing on `__construct` and `trans` on
/// `trans_choice`.
fn scan_calls(source_id: &str, code: &str, out: &mut Vec<RawEdge>) {
    let bytes = code.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if !is_ident(bytes[i]) || (i > 0 && is_ident(bytes[i - 1])) {
            i += 1;
            continue;
        }
        let start = i;
        while i < bytes.len() && is_ident(bytes[i]) {
            i += 1;
        }
        let token = &code[start..i];
        let Some(&(_, key_index)) = FNS.iter().find(|(n, _)| *n == token) else {
            continue;
        };
        let j = skip_ws(bytes, i);
        if bytes.get(j) != Some(&b'(') {
            continue;
        }
        if let Some(key) = key_arg(bytes, j + 1, key_index) {
            out.push(RawEdge::named(source_id.to_string(), "uses-lang", key));
        }
    }
}

/// Pipe form: a string literal followed by `| translate` / `| transloco`
/// (Angular ngx-translate / transloco templates). Captures the string as the
/// key; a `||` (logical or) is skipped.
fn scan_pipe(source_id: &str, code: &str, out: &mut Vec<RawEdge>) {
    let bytes = code.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'\'' && bytes[i] != b'"' {
            i += 1;
            continue;
        }
        let Some((key, next)) = read_string(bytes, i) else {
            i += 1;
            continue;
        };
        // HTML attribute binding `[x]="'KEY' | translate"` nests the single-quoted
        // key inside a double-quoted string; scan the contents too so it is caught.
        if next > i + 2 {
            scan_pipe(source_id, &code[i + 1..next - 1], out);
        }
        i = next;
        let j = skip_ws(bytes, next);
        if bytes.get(j) != Some(&b'|') || bytes.get(j + 1) == Some(&b'|') {
            continue;
        }
        let start = skip_ws(bytes, j + 1);
        let mut e = start;
        while e < bytes.len() && is_ident(bytes[e]) {
            e += 1;
        }
        let pipe = &code[start..e];
        if PIPES.contains(&pipe) {
            out.push(RawEdge::named(source_id.to_string(), "uses-lang", key));
        }
    }
}

/// Stream the translation-key index to `w` as a JSON object
/// `{"<key>": ["<file>", ...], ...}` -- keys and files both sorted/unique. This
/// is a sidecar (`.graph/lang.json`), kept out of `wiring.json`: a gettext app
/// has thousands of keys, and only the `uses` verb needs them. `usages` are
/// `(file, key)` pairs. Returns the number of distinct keys (for the banner).
pub fn write_index<W: Write>(w: &mut W, usages: &[(String, String)]) -> std::io::Result<usize> {
    let mut map: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    for (file, key) in usages {
        map.entry(key).or_default().insert(file);
    }
    w.write_all(b"{")?;
    for (i, (key, files)) in map.iter().enumerate() {
        if i > 0 {
            w.write_all(b",")?;
        }
        serde_json::to_writer(&mut *w, key).map_err(std::io::Error::other)?;
        w.write_all(b":")?;
        let files_vec: Vec<&str> = files.iter().copied().collect();
        serde_json::to_writer(&mut *w, &files_vec).map_err(std::io::Error::other)?;
    }
    w.write_all(b"}")?;
    Ok(map.len())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::{scan, write_index};

    fn keys(code: &str) -> Vec<String> {
        scan("f", code).into_iter().filter_map(|e| e.name).collect()
    }

    #[test]
    fn captures_php_translation_calls() {
        let code = "<?php echo __('invoice.title'); trans_choice('invoice.lines', 2); trans(\"a.b\");";
        let k = keys(code);
        assert!(k.contains(&"invoice.title".to_string()));
        assert!(k.contains(&"invoice.lines".to_string()));
        assert!(k.contains(&"a.b".to_string()));
    }

    #[test]
    fn captures_gettext_and_pgettext_second_arg() {
        let code = "<?php _gettext('Hello'); _pgettext('menu', 'Open');";
        let k = keys(code);
        assert!(k.contains(&"Hello".to_string()));
        assert!(k.contains(&"Open".to_string())); // pgettext key is the 2nd arg
        assert!(!k.contains(&"menu".to_string())); // ...not the context
    }

    #[test]
    fn captures_vue_and_ts_i18n_calls() {
        let code = "const s = t('nav.home'); $t('nav.away'); i18n.global.t('deep.key');";
        let k = keys(code);
        assert!(k.contains(&"nav.home".to_string()));
        assert!(k.contains(&"nav.away".to_string()));
        assert!(k.contains(&"deep.key".to_string()));
    }

    #[test]
    fn ignores_lookalike_identifiers_and_nonliteral_args() {
        // __construct is not __, trans_choice is not trans, sort(...) is not t(...)
        let code = "<?php function __construct() {} sort($items); $x = translate($v); t($dynamic);";
        assert!(keys(code).is_empty());
    }

    #[test]
    fn handles_spaces_and_escaped_quotes() {
        let code = "__(  'a.b'  ); __('it\\'s')";
        let k = keys(code);
        assert!(k.contains(&"a.b".to_string()));
        assert!(k.contains(&"it's".to_string()));
    }

    #[test]
    fn captures_angular_ngx_translate_forms() {
        // TS service call + HTML pipe (both interpolation and binding).
        let code = "this.translate.instant('nav.home'); tpl = `{{ 'nav.away' | translate }} [x]=\"'form.ok' | translate\"`;";
        let k = keys(code);
        assert!(k.contains(&"nav.home".to_string())); // .instant('KEY')
        assert!(k.contains(&"nav.away".to_string())); // 'KEY' | translate (interpolation)
        assert!(k.contains(&"form.ok".to_string())); // 'KEY' | translate (binding)
    }

    #[test]
    fn pipe_ignores_logical_or_and_other_pipes() {
        let code = "a = 'x' || 'y'; b = 'z' | uppercase;";
        assert!(keys(code).is_empty());
    }

    #[test]
    fn writes_grouped_sorted_index() {
        let usages = vec![
            ("b.php".to_string(), "k".to_string()),
            ("a.php".to_string(), "k".to_string()),
            ("a.php".to_string(), "k".to_string()), // dup collapses
        ];
        let mut buf: Vec<u8> = Vec::new();
        let count = write_index(&mut buf, &usages).unwrap();
        assert_eq!(count, 1);
        assert_eq!(String::from_utf8(buf).unwrap(), r#"{"k":["a.php","b.php"]}"#);
    }
}
