#!/bin/sh
# ports/watch.sh — poll upstreams for newer releases of every recipe
# carrying a watch.toml, and optionally bump the tree to match.
#
# Design note (why this exists instead of a "latest" version grammar in
# recipes): Yakimo recipes always pin an exact version plus a SHA-256 of
# the distfile — that pin is what makes builds reproducible and what the
# attestation chain signs. A floating "latest" pointer resolved by the
# client would defeat both. What can be automated is the *bump* itself:
# this script is the repo-side half of that (Debian's uscan plays the
# same role for debian/watch files). It queries each recipe's upstream,
# reports which recipes are behind, and with --apply rewrites
# pkg-meta.toml + source.toml to the newest matching release, fetching
# the new distfile to compute its hash. The bump then flows through the
# normal review/attestation path like any other tree change.
#
# Usage:
#   ports/watch.sh            report only
#   ports/watch.sh --apply    bump outdated recipes in place
#
# Requires: curl, git, sed, sort -V, sha256sum, awk.
set -eu

PORTS=$(CDPATH= cd -- "$(dirname "$0")" && pwd)
APPLY=0
[ "${1:-}" = "--apply" ] && { APPLY=1; shift; }
[ $# -eq 0 ] || { echo "usage: $0 [--apply]" >&2; exit 2; }

for tool in curl git sed awk sha256sum sort; do
    command -v "$tool" >/dev/null 2>&1 || {
        echo "watch.sh: required tool '$tool' not found" >&2
        exit 1
    }
done

# field <key> <file> — prints the value of a flat `key = "..."` or
# `key = '...'` TOML assignment. watch.toml files are deliberately kept
# flat (no sections) so this stays a one-liner; regexes use TOML literal
# strings (single quotes) to avoid double backslash escaping.
field() {
    sed -n -e "s/^$1 = \"\(.*\)\"[[:space:]]*\$/\1/p" \
           -e "s/^$1 = '\(.*\)'[[:space:]]*\$/\1/p" "$2" | head -n 1
}

# latest_for <recipe-dir> — prints the newest upstream version matching
# the recipe's watch.toml stable filter.
latest_for() {
    dir=$1
    type=$(field type "$dir/watch.toml")
    url=$(field url "$dir/watch.toml")
    stable=$(field stable_regex "$dir/watch.toml")
    stable=${stable:-.}
    case $type in
    git-tags)
        # git ls-remote lists `refs/tags/<tag>` plus `<tag>^{}` deref
        # lines for annotated tags; strip both decorations.
        prefix=$(field tag_prefix "$dir/watch.toml")
        git ls-remote --tags "$url" \
            | sed -n 's|.*refs/tags/||p' \
            | sed 's/\^{}//' \
            | sort -u \
            | sed "s/^$prefix//" \
            | grep -E "$stable" \
            | sort -V \
            | tail -n 1
        ;;
    release-regex)
        # Scrape a directory listing / download page: tokenize on the
        # characters that delimit hrefs, extract capture group 1 of the
        # recipe's regex from each token.
        regex=$(field regex "$dir/watch.toml")
        curl -fsSL --retry 3 --retry-all-errors "$url" \
            | tr '"<> ' '\n' \
            | sed -n -E "s/^.*$regex.*$/\1/p" \
            | grep -E "$stable" \
            | sort -uV \
            | tail -n 1
        ;;
    *)
        echo "watch.sh: $dir/watch.toml: unknown type '$type'" >&2
        return 1
        ;;
    esac
}

# apply_bump <recipe-dir> <new-version> — fetch the new distfile, hash
# it, and rewrite pkg-meta.toml + source.toml.
apply_bump() {
    dir=$1 new=$2
    template=$(field url_template "$dir/watch.toml")
    if [ -z "$template" ]; then
        echo "  no url_template in watch.toml; cannot --apply" >&2
        return 1
    fi
    url=$(printf '%s' "$template" | sed "s/{version}/$new/g")
    tmp=$(mktemp)
    curl -fsSL "$url" -o "$tmp"
    sum=$(sha256sum "$tmp" | awk '{print $1}')
    rm -f "$tmp"

    fname=$(field filename_template "$dir/watch.toml")
    if [ -n "$fname" ]; then
        fname=$(printf '%s' "$fname" | sed "s/{version}/$new/g")
    fi

    sed -i "s/^version = .*/version = \"$new\"/" "$dir/pkg-meta.toml"
    {
        echo '[[source]]'
        echo "url = \"$url\""
        echo "sha256 = \"$sum\""
        [ -n "$fname" ] && echo "filename = \"$fname\""
    } > "$dir/source.toml"
}

status=0
for wt in "$PORTS"/*/*/watch.toml; do
    [ -e "$wt" ] || continue
    dir=$(dirname "$wt")
    name=$(field name "$dir/pkg-meta.toml")
    cur=$(field version "$dir/pkg-meta.toml")
    if ! latest=$(latest_for "$dir"); then
        echo "$name $cur: watch query FAILED" >&2
        status=1
        continue
    fi
    if [ -z "$latest" ]; then
        echo "$name $cur: no upstream version matched the stable filter" >&2
        status=1
        continue
    fi
    if [ "$latest" = "$cur" ]; then
        echo "$name $cur (up to date)"
    else
        echo "$name $cur -> $latest"
        if [ "$APPLY" = 1 ]; then
            apply_bump "$dir" "$latest"
            echo "  bumped"
        fi
    fi
done
exit "$status"
