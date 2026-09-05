#!/usr/bin/env python3
"""Surgically strip everything plouf.rs can explain from an indexed tree, then
report the residue -- the code plouf could NOT wire.

This is the dogfooding tool behind the framework-understanding work: it deletes
every symbol that has an incoming link (is called / imported / extended / ...)
plus every class whose members are all used, and shows what survives. A residue
that clusters by method name (`loadBox`, `getNextValue`, `write_file`, ...) is a
map of the extractor's remaining blind spots -- each cluster is a framework
dispatch convention worth teaching plouf, not random dead code.

Usage:
    scripts/residue.py <graph-out-dir> <source-root> [--write]

    <graph-out-dir>  the dir passed to `plouf-rs index --out` (holds
                     .graph/wiring.json)
    <source-root>    the tree that was indexed (paths in the graph are relative
                     to it)
    --write          ALSO physically delete the explained spans from the files
                     under <source-root>, so you can browse the residue as real
                     code. DESTRUCTIVE -- only ever point this at a throwaway
                     copy. Omit it for a read-only report.

Example (as used in development):
    rsync -a --exclude includes/ .../dolibarr/htdocs/ /tmp/dolib/htdocs/
    plouf-rs index /tmp/dolib/htdocs --out /tmp/dolib-out
    scripts/residue.py /tmp/dolib-out /tmp/dolib/htdocs --write
"""
import argparse
import json
import os
import sys
from collections import Counter, defaultdict

CONTAINER = {"class", "trait", "enum"}
SYMBOL = {"function", "method", "class", "interface", "trait", "enum"}


def load_graph(out_dir):
    path = os.path.join(out_dir, ".graph", "wiring.json")
    with open(path) as fh:
        return json.load(fh)


def merge(spans):
    """Merge overlapping (start, end) byte ranges."""
    out = []
    for s, e in sorted(spans):
        if out and s <= out[-1][1]:
            out[-1] = (out[-1][0], max(out[-1][1], e))
        else:
            out.append((s, e))
    return out


def analyse(graph):
    nodes = {n["id"]: n for n in graph["nodes"]}
    referenced = set()                      # target of any non-contains edge
    children = defaultdict(list)            # container id -> child ids
    for e in graph["edges"]:
        if e["relation"] == "contains":
            children[e["source"]].append(e["target"])
        else:
            referenced.add(e["target"])

    def methods_of(cid):
        return [c for c in children.get(cid, []) if nodes.get(c, {}).get("kind") == "method"]

    del_spans = defaultdict(list)           # path -> [(start, end)]
    deleted = set()                         # symbol ids removed (explained)

    def drop(node):
        del_spans[node["path"]].append((node["start"], node["end"]))
        deleted.add(node["id"])

    for i, n in nodes.items():
        kind = n["kind"]
        if kind not in SYMBOL:
            continue
        if kind in CONTAINER:
            members = methods_of(i)
            if i in referenced and all(m in referenced for m in members):
                drop(n)                      # whole class is accounted for
                deleted.update(members)      # its methods are subsumed
            else:
                for m in members:
                    if m in referenced:
                        drop(nodes[m])
        elif kind == "interface":
            if i in referenced:              # method decls are contracts, not calls
                drop(n)
                deleted.update(methods_of(i))
        elif kind == "function" and i in referenced:
            drop(n)

    kept = [n for i, n in nodes.items() if n["kind"] in SYMBOL and i not in deleted]
    return nodes, del_spans, deleted, kept


def prune_files(root, del_spans):
    for path, spans in del_spans.items():
        fp = os.path.join(root, path)
        try:
            data = open(fp, "rb").read()
        except FileNotFoundError:
            continue
        out = bytearray()
        prev = 0
        for s, e in merge(spans):
            out += data[prev:s]
            prev = e
        out += data[prev:]
        open(fp, "wb").write(out)


def report(nodes, deleted, kept):
    total = sum(1 for n in nodes.values() if n["kind"] in SYMBOL)
    print("=" * 70)
    print(f"SYMBOLS: total {total} | explained/pruned {len(deleted)} | residue kept {len(kept)}")
    print("=" * 70)

    print("\nRESIDUE by kind:")
    for kind, c in Counter(n["kind"] for n in kept).most_common():
        print(f"  {kind:<10} {c}")

    print("\nRESIDUE by top-level dir:")
    for d, c in Counter(n["path"].split("/")[0] for n in kept).most_common(25):
        print(f"  {d:<22} {c}")

    print(f"\nSource files with residue: {len(set(n['path'] for n in kept))}")

    print("\nRESIDUE symbol-name frequency (top 40 -- clusters reveal gaps):")
    names = Counter(n["name"] for n in kept if n["kind"] in ("method", "function"))
    for name, c in names.most_common(40):
        print(f"  {name:<32} {c}")


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("out_dir", help="the dir passed to `plouf-rs index --out`")
    ap.add_argument("source_root", help="the tree that was indexed")
    ap.add_argument("--write", action="store_true", help="DESTRUCTIVE: prune the files in place (use a throwaway copy)")
    args = ap.parse_args()

    graph = load_graph(args.out_dir)
    nodes, del_spans, deleted, kept = analyse(graph)
    if args.write:
        prune_files(args.source_root, del_spans)
        print(f"pruned {sum(len(v) for v in del_spans.values())} explained spans "
              f"across {len(del_spans)} files under {args.source_root}\n")
    report(nodes, deleted, kept)


if __name__ == "__main__":
    sys.exit(main())
