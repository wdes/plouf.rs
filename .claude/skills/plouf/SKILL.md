---
name: plouf
description: >
  Query a repository code-graph (plouf.rs) instead of grepping. One index over
  the whole repo -- PHP (Mago) + JS/TS/Vue (oxc): find where a symbol lives, get
  its signature or full body, list its callers (blast radius), report graph gaps,
  and read DB-table structure (columns + foreign keys). Use when orienting in the
  codebase, locating X, tracing who calls/extends/imports X, or checking a
  table's schema, or when invoked as /plouf.
allowed-tools: Bash(plouf-rs *) Read
---

plouf.rs builds a code-graph (`wiring.json`) over the whole repo -- PHP (Mago)
and JS/TS/Vue (oxc) -- then answers queries over it. Prefer it over blind
grepping: it resolves calls, heritage, and imports, and slices exact source
spans, across both languages in one index.

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

## 2. Query it

```bash
plouf-rs find CompanyController          # symbols whose id/name matches
plouf-rs sig Company.getId               # a symbol's declaration line
plouf-rs body InvoiceConverter.convert   # full source of a fn/method/class/enum
plouf-rs callers BaseRequest             # who references it (calls/imports/extends/includes)
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

## 3. DB schema

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
