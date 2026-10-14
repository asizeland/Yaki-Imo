#!/bin/sh
# e2e/ports-inc.sh — fast inner loop for ports development: rebuild the
# repo from the CURRENT ports/ tree and install extra packages into the
# root left behind by the last full e2e/ports.sh run (already-built,
# unchanged packages stay installed; only new/changed ones compile).
#
# Usage:  sh e2e/ports-inc.sh <pkg> [pkg...]
#
# Requires a completed e2e/ports.sh run at /tmp/pkg-e2e-ports (for the
# seeded toolchain farm and trust setup). Always prove a from-scratch
# build with the full e2e/ports.sh before committing — this script
# skips every smoke test and the clean-room guarantees.
set -eu

if [ -z "${E2E_INNER:-}" ]; then
    exec nix shell \
        nixpkgs#gcc nixpkgs#perl nixpkgs#bubblewrap nixpkgs#python3 \
        nixpkgs#coreutils nixpkgs#gnutar nixpkgs#gzip nixpkgs#gnumake \
        nixpkgs#sqlite nixpkgs#binutils nixpkgs#gnused nixpkgs#gnugrep \
        nixpkgs#gawk nixpkgs#diffutils nixpkgs#curl nixpkgs#xz \
        nixpkgs#findutils nixpkgs#util-linux nixpkgs#bison nixpkgs#m4 \
        nixpkgs#gettext nixpkgs#flex nixpkgs#gperf \
        -c env E2E_INNER=1 sh "$0" "$@"
fi

[ $# -ge 1 ] || { echo "usage: $0 <pkg> [pkg...]" >&2; exit 2; }

WS=$(cd "$(dirname "$0")/.." && pwd)
cd "$WS"

E2E=/tmp/pkg-e2e-ports
ROOT=$E2E/root
REPO=$E2E/repo
DIST=/tmp/pkg-ports-dist
PORT=8784
BASE=http://127.0.0.1:$PORT

export PKG_ROOT=$ROOT
export PATH="$E2E/shim:$PATH"

[ -d "$ROOT" ] || { echo "no $ROOT — run e2e/ports.sh first" >&2; exit 1; }

cargo build --offline -q -p pkg-cli
cargo build --offline -q -p pkg-repo --example ports2repo
PKG=$WS/target/debug/pkg-cli
PORTS2REPO=$WS/target/debug/examples/ports2repo

# Refresh the toolchain farm (ports.sh built it; new farm tools added
# since the last full run must appear here too). Same rules as
# e2e/ports.sh: type -P skips shell builtins, readlink -f resolves to
# the real /nix/store path so links don't dangle in the sandbox.
TOOLBIN=$E2E/toolbin
for tool in env sh bash ar ranlib make tar gzip xz sed grep \
            awk gawk cut diff cmp find xargs uname install cp mv mkdir ls pwd \
            ln rm cat chmod touch basename dirname sort uniq wc tr head \
            tail ld as nm readelf objcopy strip sleep date expr printf \
            test comm m4 od id bison python3 echo true false readlink \
            msgfmt objdump perl rmdir mktemp flex gperf; do
    p=$(command -v "$tool" 2>/dev/null || true)
    case "$p" in
        /*) ;;
        *) p=$(type -P "$tool" 2>/dev/null || true) ;;
    esac
    [ -n "$p" ] && ln -sf "$(readlink -f "$p")" "$TOOLBIN/$tool" || true
done

sh ports/fetch-distfiles.sh "$DIST" >/dev/null

rm -rf "$REPO"
mkdir -p "$REPO"
"$PORTS2REPO" "$REPO" "$BASE" --distfiles "$DIST" "$WS/ports" >/dev/null
sh ports/install.sh "$REPO" >/dev/null

python3 -m http.server "$PORT" --directory "$REPO" >/dev/null 2>&1 &
HTTP_PID=$!
trap 'kill $HTTP_PID 2>/dev/null || true' EXIT
sleep 1

"$PKG" sync --force >/dev/null 2>&1
exec "$PKG" install -y "$@"
