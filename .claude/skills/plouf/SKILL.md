---
name: plouf
description: >
  Query a repository code-graph (plouf.rs) instead of grepping. One index over
  the whole repo -- PHP (Mago) + JS/TS/Vue (oxc): find where a symbol lives, get
  its signature or full body, list its callers (blast radius), report graph gaps,
  and read DB-table structure (columns + foreign keys). Use when orienting in the
  codebase, locating X, tracing who calls/extends/imports X, or checking a
  table's schema, or when invoked as /plouf.
allowed-tools: Bash(plouf-rs *) Read Edit Write
---

plouf.rs builds a code-graph (`wiring.json`) over the whole repo -- PHP (Mago)
and JS/TS/Vue (oxc) -- then answers queries over it. Prefer it over blind
grepping: it resolves calls, heritage, and imports, and slices exact source
spans, across both languages in one index.

## When to set up -- do this automatically, do not ask

The first time you use plouf in a repo (and whenever invoked as `/plouf`), get
both of these in place proactively -- do not ask permission, just do it:

1. **Build the index if it is missing.** If `build/plouf-rs-out/.graph/wiring.json`
   does not exist, run step 1 below. Re-index after large edits; queries always
   read the last index, so a stale one gives stale answers.
2. **Add the status line if it is not configured.** Run step 2 below so the user
   can see, at a glance, that plouf is indexed and being used.

Then, for a bare `/plouf` with no target, print a `missing` overview and the
example queries below -- do not just ask what to run.

## 0. Setup (once) -- if `plouf-rs` is not installed

Download the `.deb` from the releases and install it:

```bash
curl -fsSL -o /tmp/plouf-rs.deb https://github.com/wdes/plouf.rs/releases/download/v0.2.0/plouf-rs_0.2.0-1_amd64.deb
sudo apt install /tmp/plouf-rs.deb
```

This installs the binary to `/usr/bin/plouf-rs` (on your `PATH`) and drops this
skill under `/usr/share/doc/plouf-rs/skill/`.

## 1. Build the index (once per session, or after large changes)

```bash
plouf-rs index . --out build/plouf-rs-out
```

## 2. Add the status line (once per repo)

The binary also renders a Claude Code status line: `plouf-rs statusline` reads the
harness context on stdin and prints the graph size, model, cwd, context tokens,
and **how long since plouf-rs was last queried** -- a growing `[..ago]` age, or
`[unused]`, is a visible cue that you have stopped reaching for the graph:

```
plouf 17508n/42638e [3m ago] | Opus 4.8 | myrepo | ctx 45k
```

Wire it into the project's `.claude/settings.json` if it has no `statusLine`
yet. **Merge** the key -- read the file first and preserve every other key; only
create the file if it does not exist:

```json
{
  "statusLine": { "type": "command", "command": "plouf-rs statusline" }
}
```

(Needs a `plouf-rs` that ships the `statusline` subcommand; `plouf-rs statusline
</dev/null` prints a line if so. It falls back to `[unused]` / `(no index)` until
the first index exists.)

## 3. Query it

```bash
plouf-rs find CompanyController          # symbols whose id/name matches
plouf-rs sig Company.getId               # a symbol's declaration line
plouf-rs body InvoiceConverter.convert   # full source of a fn/method/class/enum
plouf-rs callers BaseRequest             # who references it (calls/imports/extends/includes)
plouf-rs find route:                     # list every route (Laravel/attribute/OpenAPI)
plouf-rs uses invoice.title              # files using a translation key (PHP/Vue/Blade)
plouf-rs missing                         # gaps: unreferenced, unresolved, empty files
```

`uses` takes an exact translation key (Laravel `__`/`trans`/`trans_choice`, Vue
`$t`/`t`, Angular ngx-translate `.instant(...)` + the `| translate` pipe, gettext)
or, failing that, a case-insensitive substring -- handy for finding every surface
that references a key, including Blade templates, `.vue` `<template>` `$t(...)`,
and Angular `.html` templates.

A bare name works when unique; otherwise the candidates are listed -- copy a full
id (`path#Class.method`) to disambiguate.

`callers` also surfaces **Eloquent relations** (`belongsTo`/`hasMany`/... edges,
labelled by kind) and the **model <-> table <-> migration** join: run
`plouf-rs callers table:<name>` (e.g. `table:companies`) to list the model that
maps to a table and every migration that touches it.

**Routes** are shared `route:<path>` nodes: `plouf-rs find route:` lists every
route -- Laravel file-based (`Route::get('/x', [Ctrl, 'm'])`), PHP attribute
(`#[Route]`/`#[Get]`), and Swagger-PHP OpenAPI (`#[OA\Post(path: '/x')]`). Run
`plouf-rs callers <Controller>` for the route files (`routes-to`) and paths
(`serves`) that wire it, or `plouf-rs callers route:/x` for what navigates to a
route.

## 4. DB schema

Feed a JSON your project can produce (`{tables: [{name, columns}], foreignKeys:
[...]}`):

```bash
plouf-rs tables --schema schema.json           # list tables
plouf-rs table companies --schema schema.json  # columns + foreign keys
```

## Notes

- Run queries from the repo root you indexed -- paths in the graph are relative.
- Local dev tool, gitignored output; CI never runs it.
- Full node/edge model + roadmap: <https://github.com/wdes/plouf.rs>.
