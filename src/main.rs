//! plouf.rs -- a Rust code-graph for PHP (Mago) + JS/TS/Vue (oxc). Walks every
//! source file under a root, emits a node/edge model, resolves it against a
//! whole-tree index, and writes a `wiring.json` graph.

mod bbscript;
mod blade;
mod config;
mod format;
mod gitattributes;
mod html;
mod js;
mod lang;
mod laravel;
mod model;
mod php;
mod query;
mod resolve;
mod router;
mod schema;
mod twig;

use std::collections::{HashMap, HashSet};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Instant;

use ignore::WalkBuilder;
use rayon::prelude::*;

use clap::Parser;
use serde_json::json;

/// Version string for `--version`: the crate version plus the build hash
/// (`BUILD_HASH`, from `build.rs` -- a short git hash or a compile timestamp).
const VERSION: &str = concat!("v", env!("CARGO_PKG_VERSION"), " (", env!("BUILD_HASH"), ")");

/// A code-graph for PHP (Mago) + JS/TS/Vue (oxc): `index` builds
/// it, the other subcommands query it.
#[derive(Parser)]
#[command(name = "plouf-rs", version = VERSION, about = "Code-graph (wiring.json) for PHP + JS/TS/Vue")]
struct Cli {
    /// Graph directory: `index` writes `<out>/.graph/wiring.json`; queries read it.
    #[arg(short, long, default_value = "build/plouf-rs-out", global = true)]
    out: String,
    #[command(subcommand)]
    cmd: Cmd,
}

/// The subcommands: one builder + the query verbs over the built graph.
#[derive(clap::Subcommand)]
enum Cmd {
    /// Build the wiring.json graph for a source tree.
    Index {
        /// Root directory to scan (honours `.gitignore`).
        #[arg(default_value = ".")]
        root: String,
    },
    /// List symbols whose id or name contains TERM (case-insensitive).
    Find { term: String },
    /// Print the declaration (signature) line of a symbol.
    Sig { symbol: String },
    /// Print the full source body of a symbol.
    Body { symbol: String },
    /// List what references a symbol (calls/imports/extends/implements).
    Callers { symbol: String },
    /// List files that use a translation KEY (exact, else substring), across
    /// PHP/Vue/Blade -- read from the `.graph/lang.json` sidecar.
    Uses { key: String },
    /// Report graph gaps: unreferenced symbols, unresolved edges, empty files.
    Missing,
    /// List DB tables from a schema JSON (`php artisan schema:svg --format=json`).
    Tables {
        #[arg(long, default_value = "build/schema.json")]
        schema: String,
    },
    /// Print a DB table's columns + foreign keys from a schema JSON.
    Table {
        /// Table name.
        name: String,
        #[arg(long, default_value = "build/schema.json")]
        schema: String,
    },
}

use crate::model::{Node, RawEdge};

/// Collect source files under `root`, honouring `.gitignore` (and parent
/// gitignores) via the `ignore` crate -- gitignore-aware. The extensions come
/// from the format registry (`format::SOURCE_EXTS`).
fn collect_sources(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for result in WalkBuilder::new(root).hidden(false).parents(true).build() {
        let Ok(entry) = result else { continue };
        let path = entry.path();
        let ext = path.extension().and_then(|x| x.to_str()).unwrap_or("");
        if path.is_file() && format::SOURCE_EXTS.contains(&ext) {
            out.push(path.to_path_buf());
        }
    }
    out.sort();
    out
}

/// Extract every file into one node/edge set, in parallel over a bounded pool.
/// Threads default to `min(cores, 4)`; `PLOUF_THREADS` overrides (`1` =
/// sequential, lowest peak RSS; higher trades memory for speed).
fn extract_all(root: &Path, files: &[PathBuf]) -> Result<(Vec<Node>, Vec<RawEdge>), std::io::Error> {
    let threads = std::env::var("PLOUF_THREADS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|&n| n > 0)
        .unwrap_or_else(|| std::thread::available_parallelism().map_or(4, |n| n.get().min(4)));
    let mut nodes: Vec<Node> = Vec::new();
    let mut edges: Vec<RawEdge> = Vec::new();

    // Single-threaded: extend directly, one arena alive at a time -- the lowest
    // possible peak RSS.
    if threads == 1 {
        for f in files {
            let (mut fnodes, mut fedges) = format::extract(root, f);
            nodes.append(&mut fnodes);
            edges.append(&mut fedges);
        }
        return Ok((nodes, edges));
    }

    let pool = rayon::ThreadPoolBuilder::new().num_threads(threads).build().map_err(std::io::Error::other)?;
    // fold into per-thread accumulators (each file's arena is freed as soon as
    // its nodes/edges are appended) then reduce -- so only the growing graph +
    // a few live arenas are ever resident, not every file's result at once.
    // Node/edge order follows work-stealing, but the output is a gitignored
    // graph queried by id, so order is irrelevant.
    let (fold_nodes, fold_edges) = pool.install(|| {
        files
            .par_iter()
            .fold(
                || (Vec::new(), Vec::new()),
                |mut acc: (Vec<Node>, Vec<RawEdge>), f| {
                    let (mut fnodes, mut fedges) = format::extract(root, f);
                    acc.0.append(&mut fnodes);
                    acc.1.append(&mut fedges);
                    acc
                },
            )
            .reduce(
                || (Vec::new(), Vec::new()),
                |mut a, mut b| {
                    a.0.append(&mut b.0);
                    a.1.append(&mut b.1);
                    a
                },
            )
    });
    nodes = fold_nodes;
    edges = fold_edges;
    Ok((nodes, edges))
}

fn kind_histogram(nodes: &[Node]) -> Vec<(&'static str, usize)> {
    let mut kinds: HashMap<&'static str, usize> = HashMap::new();
    for n in nodes {
        *kinds.entry(n.kind).or_default() += 1;
    }
    let mut kv: Vec<_> = kinds.into_iter().collect();
    kv.sort_by_key(|(_, c)| std::cmp::Reverse(*c));
    kv
}

/// A run's summary, for the banner.
struct Summary {
    files: usize,
    nodes: usize,
    edges: usize,
    lang_keys: usize,
    lang_usages: usize,
    kinds: Vec<(&'static str, usize)>,
    elapsed_ms: u128,
}

fn run(root: &str, out_dir: &str) -> Result<Summary, std::io::Error> {
    let start = Instant::now();
    let root_path = PathBuf::from(root);
    let files = collect_sources(&root_path);

    // Each file is parsed with its own arena, so extraction is embarrassingly
    // parallel; rayon's collect preserves input (sorted) order, keeping the
    // output deterministic. Threads are capped (default: cores, max 4) so only a
    // few large parse arenas are alive at once -- the CPU win without an
    // all-cores memory spike. `PLOUF_THREADS=1` forces sequential for the
    // lowest peak RSS; higher values trade memory for speed.
    let (mut nodes, mut edges) = extract_all(&root_path, &files)?;

    // `.gitattributes` export-ignore mapping: link each pattern to the file or
    // folder it names. Appended before dedup so a pattern that names an existing
    // source file collapses onto that file node rather than a redundant `path`.
    let (ga_nodes, ga_edges) = gitattributes::scan(&root_path);
    nodes.extend(ga_nodes);
    edges.extend(ga_edges);

    // Linter configs (phpcs.xml / phpstan.neon) -> the sniff/rule files they
    // activate, so those entry-point classes stop reading as unreferenced.
    edges.extend(config::scan(&root_path));

    // Collapse cross-file duplicate nodes. Only shared `table:<name>` ids recur
    // (emitted by every model + migration of that table); everything else is
    // already unique (ids carry the file path).
    let mut seen_ids: HashSet<String> = HashSet::new();
    nodes.retain(|n| seen_ids.insert(n.id.clone()));

    // Translation-key usages can be numerous (a gettext app has thousands) and
    // would bloat the graph everyone loads, so they never enter `wiring.json`:
    // drain them into `(file, key)` pairs for their own `lang.json` sidecar.
    let mut lang_usages: Vec<(String, String)> = Vec::new();
    edges.retain(|e| {
        if e.relation == "uses-lang" {
            if let Some(key) = &e.name {
                lang_usages.push((e.source.clone(), key.clone()));
            }
            return false;
        }
        true
    });

    let resolved = resolve::resolve(&nodes, &edges);
    drop(edges); // raw edges are consumed; free them before serializing

    let graph_dir = format!("{out_dir}/.graph");
    std::fs::create_dir_all(&graph_dir)?;
    let file = std::fs::File::create(format!("{graph_dir}/wiring.json"))?;
    let mut writer = BufWriter::new(file);
    write_wiring(&mut writer, &nodes, &resolved)?;
    writer.flush()?;

    // The translation-key index, streamed to its own sidecar so `wiring.json`
    // stays lean and only the `uses` verb pays to load it.
    let lang_file = std::fs::File::create(format!("{graph_dir}/lang.json"))?;
    let mut lang_writer = BufWriter::new(lang_file);
    let lang_keys = lang::write_index(&mut lang_writer, &lang_usages)?;
    lang_writer.flush()?;

    // A tiny sidecar the status line (and other tooling) can read without
    // parsing the multi-MB wiring.json on every render.
    let stats = json!({"files": files.len(), "nodes": nodes.len(), "edges": resolved.len(), "lang_keys": lang_keys, "lang_usages": lang_usages.len()});
    std::fs::write(format!("{graph_dir}/stats.json"), serde_json::to_string(&stats).map_err(std::io::Error::other)?)?;

    Ok(Summary {
        files: files.len(),
        nodes: nodes.len(),
        edges: resolved.len(),
        lang_keys,
        lang_usages: lang_usages.len(),
        kinds: kind_histogram(&nodes),
        elapsed_ms: start.elapsed().as_millis(),
    })
}

/// Stream the wiring graph to `w` one node/edge at a time -- each item is a
/// transient `Value` (dropped immediately), so the whole graph is never
/// materialized as one JSON tree or one output string.
fn write_wiring<W: Write>(w: &mut W, nodes: &[Node], edges: &[model::ResolvedEdge]) -> std::io::Result<()> {
    w.write_all(br#"{"meta":{"tool":"plouf-rs","lang":"php+js"},"nodes":["#)?;
    for (i, n) in nodes.iter().enumerate() {
        if i > 0 {
            w.write_all(b",")?;
        }
        let v = json!({"id": n.id, "name": n.name, "kind": n.kind, "path": n.path, "start": n.start, "end": n.end, "exported": true});
        serde_json::to_writer(&mut *w, &v).map_err(std::io::Error::other)?;
    }
    w.write_all(br#"],"edges":["#)?;
    for (i, e) in edges.iter().enumerate() {
        if i > 0 {
            w.write_all(b",")?;
        }
        let v = json!({"source": e.source, "target": e.target, "relation": e.relation});
        serde_json::to_writer(&mut *w, &v).map_err(std::io::Error::other)?;
    }
    w.write_all(b"]}")?;
    Ok(())
}

/// Peak resident set size in KiB (Linux `VmHWM`), for the index banner.
fn peak_rss_kb() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    status.lines().find_map(|line| {
        line.strip_prefix("VmHWM:").and_then(|rest| rest.split_whitespace().next()?.parse().ok())
    })
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match cli.cmd {
        Cmd::Index { root } => run(&root, &cli.out).map(|s| {
            eprintln!("plouf-rs: {} files, {} nodes, {} edges in {} ms", s.files, s.nodes, s.edges, s.elapsed_ms);
            eprintln!("kinds: {:?}", s.kinds);
            eprintln!("lang: {} keys, {} usages", s.lang_keys, s.lang_usages);
            if let Some(kb) = peak_rss_kb() {
                eprintln!("peak RSS: {} MiB", kb / 1024);
            }
        }),
        Cmd::Find { term } => query::find(&cli.out, &term),
        Cmd::Sig { symbol } => query::signature(&cli.out, &symbol),
        Cmd::Body { symbol } => query::body(&cli.out, &symbol),
        Cmd::Callers { symbol } => query::callers(&cli.out, &symbol),
        Cmd::Uses { key } => query::uses(&cli.out, &key),
        Cmd::Missing => query::missing(&cli.out),
        Cmd::Tables { schema } => schema::list_tables(&schema),
        Cmd::Table { name, schema } => schema::table(&schema, &name),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("plouf-rs: {e}");
            ExitCode::FAILURE
        }
    }
}
