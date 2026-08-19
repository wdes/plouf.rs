# plouf.rs

A fast, deterministic code-graph for a polyglot repository, written in Rust. It
walks the source tree, emits a `wiring.json` (nodes + resolved edges), and
answers queries over it -- so tools and agents navigate a codebase instead of
grepping it.

- **PHP** via [Mago](https://github.com/carthage-software/mago)
- **JS / TS / Vue** via [oxc](https://oxc.rs) (`.vue` = its `<script>` blocks)

One graph spans backend and frontend.

## Install

Download the `.deb` from the [releases](https://github.com/wdes/plouf.rs/releases)
and install it with apt (resolves dependencies and puts `plouf-rs` on your
`PATH` at `/usr/bin/plouf-rs`):

```sh
curl -fsSL -o /tmp/plouf-rs.deb https://github.com/wdes/plouf.rs/releases/download/v0.1.0/plouf-rs_0.1.0-1_amd64.deb
sudo apt install /tmp/plouf-rs.deb
```

The package (next version) also drops the `/plouf` agent skill ([SKILL.md](.claude/skills/plouf/SKILL.md)) under
`/usr/share/doc/plouf-rs/skill/` -- copy it into a project's `.claude/skills/` to use it.

## Build

Or build from source:

```sh
cargo build --release
```

Needs a recent Rust toolchain (Mago requires rustc >= 1.97).

## Index

```sh
plouf-rs index . --out build/out
```

Writes `build/out/.graph/wiring.json` plus a tiny `stats.json`. Extraction is
parallel; `PLOUF_THREADS=1` forces sequential for the lowest peak RSS, higher
values trade memory for speed.

## Query

Run these from the directory you indexed (paths in the graph are relative):

```sh
plouf-rs find CompanyController   # symbols whose id/name matches
plouf-rs sig Company.getId        # a symbol's declaration line
plouf-rs body Foo.convert         # full source (fn/class/enum/...)
plouf-rs callers BaseRequest      # who references it (blast radius)
plouf-rs missing                  # gaps: unreferenced/unresolved/empty
```

A bare name resolves when unique; otherwise the candidates are listed. `--out`
defaults to `build/plouf-rs-out`; pass the directory you indexed into.

## DB schema (optional)

`plouf` reads a JSON **any tool can produce** -- `{tables: [{name, columns}],
foreignKeys: [...]}` -- so a project can feed its live schema in:

```sh
plouf-rs tables --schema schema.json
plouf-rs table companies --schema schema.json
```

## Model

- **Nodes**: `file`, `class`, `interface`, `trait`, `enum`, `function`,
  `method`, `component` (one per Vue SFC). Ids are `path` for files and
  `path#Symbol` / `path#Class.method` for the rest, each carrying a byte span
  so `sig` / `body` slice the source without re-parsing.
- **Edges**: `contains`, `imports` (JS relative specifiers resolve to files),
  `extends` / `implements`, `calls` (resolved via a typed receiver -- `$this` /
  `this`, typed params, `new X()` / annotated locals -- then the extends chain,
  else a unique-name fallback).

## Tests

```sh
cargo test
```

## License

[MPL-2.0](LICENSE).
