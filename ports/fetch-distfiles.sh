#!/bin/sh
# ports/fetch-distfiles.sh — download every source pinned by the tree
# into a distfiles directory, verifying the tree-pinned SHA-256.
#
# Usage:  sh ports/fetch-distfiles.sh <distfiles-dir>
#
# Idempotent: files already present with a matching hash are kept.
set -eu

PORTS=$(cd "$(dirname "$0")" && pwd)
DIST=${1:?usage: fetch-distfiles.sh <distfiles-dir>}
mkdir -p "$DIST"

# Extract url/sha256 pairs from every source.toml in the tree.
find "$PORTS" -name source.toml | while read -r f; do
    url=$(sed -n 's/^url = "\(.*\)"/\1/p' "$f" | head -1)
    sha=$(sed -n 's/^sha256 = "\(.*\)"/\1/p' "$f" | head -1)
    fname=$(sed -n 's/^filename = "\(.*\)"/\1/p' "$f" | head -1)
    [ -n "$url" ] && [ -n "$sha" ] || continue
    file=${fname:-${url##*/}}
    out=$DIST/$file
    if [ -f "$out" ] && echo "$sha  $out" | sha256sum -c - >/dev/null 2>&1; then
        echo "cached: $file"
        continue
    fi
    echo "fetch:  $file"
    # Retry with backoff: a transient reset/stall must not abort the
    # whole run (the sha256 pin still guards content correctness).
    attempt=1
    until curl -fsSL --max-time 600 -o "$out" "$url" \
        && echo "$sha  $out" | sha256sum -c - >/dev/null; do
        attempt=$((attempt + 1))
        [ "$attempt" -le 4 ] || { echo "FAILED after 4 attempts: $file" >&2; exit 1; }
        rm -f "$out"
        echo "retry $attempt: $file" >&2
        sleep $((attempt * 2))
    done
done
