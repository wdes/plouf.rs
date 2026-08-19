//! End-to-end CLI tests: build the graph for the fixture tree, then exercise
//! every query verb (and the DB-schema verbs) through the built binary. This
//! drives `main.rs`, `format.rs`, the extractors, `resolve.rs`, `query.rs`, and
//! `schema.rs` together the way a user does.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;
use std::process::Command;

const BIN: &str = env!("CARGO_BIN_EXE_plouf-rs");
const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");
const SCHEMA: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/schema.json");

fn out_dir() -> PathBuf {
    // Per-process dir so parallel test binaries never collide.
    std::env::temp_dir().join(format!("plouf_cli_{}", std::process::id()))
}

/// Run the binary from the fixtures dir (so relative graph paths resolve for
/// `sig`/`body`). Returns (stdout, stderr, success).
fn run(args: &[&str]) -> (String, String, bool) {
    let output = Command::new(BIN).args(args).current_dir(FIXTURES).output().unwrap();
    (
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
        output.status.success(),
    )
}

#[test]
fn indexes_and_answers_every_verb() {
    let out = out_dir();
    let out_s = out.to_str().unwrap();

    // Build the graph (default parallel path). The banner goes to stderr.
    let (_, err, ok) = run(&["index", ".", "--out", out_s]);
    assert!(ok, "index failed: {err}");
    assert!(err.contains("nodes") && err.contains("lang:"), "banner: {err}");

    // Sequential single-threaded path (the other branch of extract_all).
    let seq = Command::new(BIN)
        .args(["index", ".", "--out", out_s])
        .current_dir(FIXTURES)
        .env("PLOUF_THREADS", "1")
        .output()
        .unwrap();
    assert!(seq.status.success());

    // find: a symbol by name.
    let (o, _, ok) = run(&["find", "Company", "--out", out_s]);
    assert!(ok && o.contains("Company.php#Company"), "find: {o}");

    // sig / body slice the source span (needs the fixtures cwd).
    let (o, _, ok) = run(&["sig", "Company.invoices", "--out", out_s]);
    assert!(ok && o.contains("function invoices"), "sig: {o}");
    let (o, _, ok) = run(&["body", "helper", "--out", out_s]);
    assert!(ok && o.contains("strlen"), "body: {o}");
    // A whole-unit kind (Vue component) has no signature line to slice.
    let (o, _, ok) = run(&["sig", "App", "--out", out_s]);
    assert!(ok && o.contains("App"), "sig component: {o}");

    // callers: Eloquent relations + PHPUnit covers into the Company class.
    let (o, _, ok) = run(&["callers", "Company", "--out", out_s]);
    assert!(ok, "callers err");
    assert!(o.contains("belongsTo") && o.contains("covers"), "callers Company: {o}");

    // callers of the shared table node: the model + its migration.
    let (o, _, _) = run(&["callers", "table:companies", "--out", out_s]);
    assert!(o.contains("table\t") && o.contains("migrates\t"), "callers table: {o}");

    // uses: an exact translation key, then a substring fallback.
    let (o, _, ok) = run(&["uses", "company.label", "--out", out_s]);
    assert!(ok && o.contains("Company.php"), "uses exact: {o}");
    let (o, _, _) = run(&["uses", "invoice.", "--out", out_s]);
    assert!(o.contains("invoice.title") && o.contains("invoice.total"), "uses substring: {o}");
    // A key nothing uses prints nothing but still succeeds.
    let (o, _, ok) = run(&["uses", "no.such.key.anywhere", "--out", out_s]);
    assert!(ok && o.trim().is_empty(), "uses empty: {o:?}");

    // missing: the gaps report runs clean.
    let (_, _, ok) = run(&["missing", "--out", out_s]);
    assert!(ok, "missing failed");

    // Error paths: an unknown symbol, and an ambiguous one.
    let (_, _, ok) = run(&["body", "definitely_not_here", "--out", out_s]);
    assert!(!ok, "unknown symbol should fail");
    let (_, _, ok) = run(&["body", "label", "--out", out_s]); // Company.label + Named.label
    assert!(!ok, "ambiguous symbol should fail");

    // DB-schema verbs.
    let (o, _, ok) = run(&["tables", "--schema", SCHEMA]);
    assert!(ok && o.contains("companies") && o.contains("invoices"), "tables: {o}");
    let (o, _, ok) = run(&["table", "companies", "--schema", SCHEMA]);
    assert!(ok && o.contains("id: bigint") && o.contains("referenced by"), "table companies: {o}");
    let (o, _, ok) = run(&["table", "invoices", "--schema", SCHEMA]);
    assert!(ok && o.contains("references:"), "table invoices: {o}");
    let (_, _, ok) = run(&["table", "no_such_table", "--schema", SCHEMA]);
    assert!(!ok, "unknown table should fail");
    let (_, _, ok) = run(&["tables", "--schema", "/no/such/schema.json"]);
    assert!(!ok, "missing schema file should fail");

    // --version prints the crate version + build hash.
    let (o, _, ok) = run(&["--version"]);
    assert!(ok && o.contains("plouf-rs v"), "version: {o}");

    std::fs::remove_dir_all(&out).ok();
}
