#!/bin/sh
# Build the x86_64 Yaki Imo live-test ISO without installing host packages.
set -eu

if [ -z "${YAKIMO_BUILD_INNER:-}" ]; then
    exec nix shell \
        nixpkgs#coreutils nixpkgs#findutils nixpkgs#gnused nixpkgs#curl \
        -c env YAKIMO_BUILD_INNER=1 sh "$0" "$@"
fi

LIVE=$(cd "$(dirname "$0")" && pwd)
WS=$(cd "$LIVE/.." && pwd)
STAGE=$LIVE/.build
PORTS_STAGE=$STAGE/ports
DIST=${YAKIMO_DISTFILES:-/tmp/pkg-ports-dist}

mkdir -p "$DIST"
DIST=$(cd "$DIST" && pwd)
rm -rf "$PORTS_STAGE"
mkdir -p "$PORTS_STAGE"

while IFS= read -r port; do
    [ -n "$port" ] || continue
    mkdir -p "$PORTS_STAGE/$(dirname "$port")"
    cp -R "$WS/ports/$port" "$PORTS_STAGE/$port"
done < "$LIVE/ports.txt"

# The fetch helper discovers sources relative to itself. Giving the staged
# tree a copy keeps the ISO's source set limited to ports.txt.
cp "$WS/ports/fetch-distfiles.sh" "$PORTS_STAGE/fetch-distfiles.sh"
sh "$PORTS_STAGE/fetch-distfiles.sh" "$DIST"

nix-build "$LIVE" --arg distfiles "$DIST" -A iso -o "$LIVE/result"
ISO=$(find "$LIVE/result/iso" -maxdepth 1 -type f -name '*.iso' -print -quit)
[ -n "$ISO" ] && [ -f "$ISO" ] || {
    echo "nix-build completed without producing an ISO" >&2
    exit 1
}
ls -lh "$ISO"
sha256sum "$ISO"
