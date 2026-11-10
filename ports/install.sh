#!/bin/sh
# ports/install.sh — install the repo-built recipe tree into a root.
#
# Usage:  sh ports/install.sh <repo-dir> [root]
#
# <repo-dir> is the output of ports2repo (it carries recipes/<name>/
# with .recipe-commit filled in). [root] defaults to $PKG_ROOT, or /
# for a real system. After installing recipes, point
# <root>/etc/pkg/mirrors.toml at the served repo and:
#
#   pkg key trust-root <repo>/root.json
#   pkg sync
set -eu

REPO=${1:?usage: install.sh <repo-dir> [root]}
ROOT=${2:-${PKG_ROOT:-/}}

[ -d "$REPO/recipes" ] || { echo "$REPO has no recipes/ (run ports2repo first)" >&2; exit 1; }
mkdir -p "$ROOT/var/lib/pkg/recipes"
for recipe in "$REPO"/recipes/*; do
    name=${recipe##*/}
    dest=$ROOT/var/lib/pkg/recipes/$name
    mkdir -p "$dest"
    # A migration from external phases/source pins to pkg-meta.toml must
    # not leave the old definitions active in the installed checkout.
    for file in source.toml prepare build check install; do
        rm -f "$dest/$file"
    done
    cp -r "$recipe"/. "$dest/"
done
echo "installed $(ls "$REPO/recipes" | wc -l) recipes into $ROOT/var/lib/pkg/recipes"
