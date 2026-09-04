#!/bin/sh
# Cut a plouf.rs release end to end, then re-open development.
#
#   scripts/release.sh 0.4.0        (a leading v is fine: v0.4.0)
#
# Steps, in order:
#   1. bump every pinned version to X.Y.Z (drop the -dev suffix),
#   2. commit + push main,
#   3. create a signed, annotated tag vX.Y.Z and push it (this fires
#      .github/workflows/release.yml, which builds the DRAFT release),
#   4. ALWAYS re-open development on the next PATCH: bump to X.Y.(Z+1)-dev,
#      commit + push main.
#
# The release version itself is your choice (pass 0.4.0 for a minor, 1.0.0 for a
# major); only the post-release dev bump is fixed at patch+1. Requires a clean
# tree on main and a signing key (git config user.signingkey).
#
# Versions are kept in sync across:
#   - Cargo.toml                       the [package] version
#   - Cargo.lock                       the plouf-rs package entry
#   - README.md                        the .deb download URL (path + filename)
#   - .claude/skills/plouf/SKILL.md    the .deb download URL baked into the .deb
set -eu

new="${1:-}"
if [ -z "$new" ]; then
    echo "usage: scripts/release.sh <version>   e.g. scripts/release.sh 0.4.0" >&2
    exit 2
fi
new="${new#v}"

root="$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)"
cd "$root"

# Rewrite every pinned version string to $1.
bump() {
    v="$1"
    # Cargo.toml: the [package] version (line-anchored; no dependency touched).
    sed -i.bak 's/^version = "[^"]*"/version = "'"$v"'"/' Cargo.toml
    # Cargo.lock: the plouf-rs package's own version (line after its name).
    sed -i.bak '/^name = "plouf-rs"$/{n;s/^version = "[^"]*"/version = "'"$v"'"/;}' Cargo.lock
    # README + the skill baked into the .deb: rewrite the .deb download URL.
    for f in README.md .claude/skills/plouf/SKILL.md; do
        sed -i.bak \
            -e 's#releases/download/v[0-9][0-9.]*/#releases/download/v'"$v"'/#g' \
            -e 's#plouf-rs_[0-9][0-9.]*-1_amd64\.deb#plouf-rs_'"$v"'-1_amd64.deb#g' \
            "$f"
    done
    rm -f Cargo.toml.bak Cargo.lock.bak README.md.bak .claude/skills/plouf/SKILL.md.bak
}

# X.Y.Z -> X.Y.(Z+1)-dev  (any -suffix on the patch is stripped first).
next_dev() {
    major="${1%%.*}"
    rest="${1#*.}"
    minor="${rest%%.*}"
    patch="${rest#*.}"
    patch="${patch%%-*}"
    echo "${major}.${minor}.$((patch + 1))-dev"
}

# Guard: a clean tree on main, so the release commits carry only the bump.
branch="$(git rev-parse --abbrev-ref HEAD)"
[ "$branch" = "main" ] || { echo "not on main (on $branch)" >&2; exit 1; }
git diff --quiet && git diff --cached --quiet || { echo "working tree not clean" >&2; exit 1; }

files="Cargo.toml Cargo.lock README.md .claude/skills/plouf/SKILL.md"

echo "==> releasing v$new"
bump "$new"
git commit -m "chore: release v$new" -- $files
git push origin main
git tag -a -s -m "v$new" "v$new"
git push origin "v$new"

dev="$(next_dev "$new")"
echo "==> re-opening development on $dev"
bump "$dev"
git commit -m "chore: $dev" -- $files
git push origin main

echo "released v$new (draft build running); main is now on $dev"
