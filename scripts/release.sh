#!/bin/sh
# Bump plouf.rs to a new version in every place it is pinned, in one shot, so a
# release never ships a stale version string -- least of all the /plouf skill
# that cargo-deb bakes into the .deb (it has drifted before).
#
# Kept in sync:
#   - Cargo.toml                       the [package] version
#   - Cargo.lock                       the plouf-rs package entry
#   - README.md                        the .deb download URL (path + filename)
#   - .claude/skills/plouf/SKILL.md    the .deb download URL packaged in the .deb
#
# Usage:  scripts/release.sh 0.3.0     (a leading v is fine: v0.3.0)
# After:  cargo check (verify the lock), review `git diff`, commit, tag v0.3.0.
set -eu

new="${1:-}"
if [ -z "$new" ]; then
    echo "usage: scripts/release.sh <version>   e.g. scripts/release.sh 0.3.0" >&2
    exit 2
fi
new="${new#v}"

root="$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)"
cd "$root"

old="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)"
if [ -z "$old" ]; then
    echo "could not read the current version from Cargo.toml" >&2
    exit 1
fi

echo "bumping $old -> $new"

# 1. Cargo.toml: the [package] version. Line-anchored, so no dependency's
#    inline version = "..." is touched.
sed -i.bak 's/^version = "[^"]*"/version = "'"$new"'"/' Cargo.toml

# 2. Cargo.lock: the plouf-rs package's own version (the line right after its
#    name). Every dependency entry is left alone.
sed -i.bak '/^name = "plouf-rs"$/{n;s/^version = "[^"]*"/version = "'"$new"'"/;}' Cargo.lock

# 3. README + the skill baked into the .deb: rewrite the .deb download URL to
#    $new whatever it currently pins. The skill has lagged the real version
#    before, and that is exactly what ships stale inside the package.
for f in README.md .claude/skills/plouf/SKILL.md; do
    sed -i.bak \
        -e 's#releases/download/v[0-9][0-9.]*/#releases/download/v'"$new"'/#g' \
        -e 's#plouf-rs_[0-9][0-9.]*-1_amd64\.deb#plouf-rs_'"$new"'-1_amd64.deb#g' \
        "$f"
done

rm -f Cargo.toml.bak Cargo.lock.bak README.md.bak .claude/skills/plouf/SKILL.md.bak

echo "bumped to $new. next: cargo check, review 'git diff', commit, then tag v$new"
