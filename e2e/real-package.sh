#!/bin/sh
# e2e/real-package.sh — end-to-end run building a REAL upstream package
# (zlib 1.3.1, fetched from zlib.net and hash-pinned) through the whole
# pkg toolchain: signed repo, sandboxed build, install, and a consumer
# package linked against it via the stable /pkg/deps/<name> alias.
#
# Usage:  sh e2e/real-package.sh        (from the workspace root)
#
# Requires network access for the one-time tarball fetch (cached under
# /tmp/pkg-e2e-real-cache afterwards). Everything else mirrors
# e2e/run.sh's host-quirk adaptations.

set -eu

if [ -z "${E2E_INNER:-}" ]; then
    exec nix shell \
        nixpkgs#gcc nixpkgs#perl nixpkgs#bubblewrap nixpkgs#python3 \
        nixpkgs#coreutils nixpkgs#gnutar nixpkgs#gzip nixpkgs#gnumake \
        nixpkgs#sqlite nixpkgs#binutils nixpkgs#gnused nixpkgs#gnugrep \
        nixpkgs#gawk nixpkgs#diffutils nixpkgs#curl \
        -c env E2E_INNER=1 sh "$0" "$@"
fi

WS=$(cd "$(dirname "$0")/.." && pwd)
cd "$WS"

E2E=/tmp/pkg-e2e-real
CACHE=/tmp/pkg-e2e-real-cache
ROOT=$E2E/root
REPO=$E2E/repo
SRC=$E2E/src
TOOLBIN=$E2E/toolbin
PORT=8778
BASE=http://127.0.0.1:$PORT

ZLIB_VER=1.3.1
ZLIB_SHA256=9a93b2b7dfdac77ceba5a558a580e74667dd6fede4585b91eefb60f03b72df23

export PKG_ROOT=$ROOT

pass() { echo "PASS: $1"; }
fail() { echo "FAIL: $1" >&2; exit 1; }

echo "== building workspace =="
cargo build --offline -q -p pkg-cli
cargo build --offline -q -p pkg-repo --example mkrepo
PKG=$WS/target/debug/pkg-cli
MKREPO=$WS/target/debug/examples/mkrepo
REAL_BWRAP=$(command -v bwrap)

rm -rf "$E2E"
mkdir -p "$ROOT" "$REPO" "$SRC" "$TOOLBIN" "$E2E/shim" "$CACHE"

# ---------------------------------------------------------------
# 1. Fetch + hash-pin the real upstream source.
# ---------------------------------------------------------------
TARBALL=$CACHE/zlib-$ZLIB_VER.tar.gz
if ! [ -f "$TARBALL" ] || \
   ! echo "$ZLIB_SHA256  $TARBALL" | sha256sum -c - >/dev/null 2>&1; then
    echo "fetching zlib $ZLIB_VER from zlib.net"
    curl -sL --max-time 300 -o "$TARBALL" \
        "https://zlib.net/fossils/zlib-$ZLIB_VER.tar.gz"
fi
echo "$ZLIB_SHA256  $TARBALL" | sha256sum -c - >/dev/null \
    || fail "zlib tarball sha256 mismatch"
pass "zlib $ZLIB_VER tarball fetched, sha256 pinned"

# A consumer package that links against the installed zlib.
mkdir -p "$SRC/zconsumer-1"
cat > "$SRC/zconsumer-1/main.c" <<'EOF'
#include <stdio.h>
#include <string.h>
#include <zlib.h>
int main(void) {
    const char *msg = "yakimo real-package e2e";
    unsigned char buf[256];
    unsigned long n = sizeof buf;
    if (compress(buf, &n, (const unsigned char *)msg, strlen(msg)) != Z_OK)
        return 1;
    printf("zlib %s compressed %zu -> %lu\n", zlibVersion(), strlen(msg), n);
    return 0;
}
EOF
tar -czf "$SRC/zconsumer-1.tar.gz" -C "$SRC" zconsumer-1

# Host glibc, as seen through ldd (the build gate requires libc.so.6
# to come from a declared dependency — see e2e/run.sh).
LIBC_PATH=$(ldd /bin/sh | awk '/libc\.so/{print $3; exit}')
LD_PATH=$(ldd /bin/sh | awk '/ld-linux/{print $1; exit}')
GLIBC_VER=$(ldd --version | head -1 | awk '{print $NF}')
echo "host glibc: $GLIBC_VER ($LIBC_PATH)"

# ---------------------------------------------------------------
# 2. Signed test repo carrying the real zlib source.
# ---------------------------------------------------------------
"$MKREPO" "$REPO" "$BASE" \
    "glibc:$GLIBC_VER:e2e0001:false:" \
    "zlib:$ZLIB_VER:e2e0001:false:$TARBALL" \
    "zconsumer:1:e2e0001:false:$SRC/zconsumer-1.tar.gz" >/dev/null
pass "mkrepo fabricated signed repo with real zlib source"

# ---------------------------------------------------------------
# 3. Recipes.
# ---------------------------------------------------------------
for p in glibc zlib zconsumer; do
    mkdir -p "$ROOT/var/lib/pkg/recipes/$p"
    echo "e2e0001" > "$ROOT/var/lib/pkg/recipes/$p/.recipe-commit"
done

cat > "$ROOT/var/lib/pkg/recipes/glibc/pkg-meta.toml" <<EOF
name = "glibc"
version = "$GLIBC_VER"
category = "libs"
EOF
cat > "$ROOT/var/lib/pkg/recipes/glibc/build" <<EOF
cp $LIBC_PATH libc.so.6
cp $LD_PATH ld-linux-x86-64.so.2
EOF
cat > "$ROOT/var/lib/pkg/recipes/glibc/install" <<'EOF'
mkdir -p "$PKGDIR/usr/lib" "$PKGDIR/lib64"
cp libc.so.6 "$PKGDIR/usr/lib/"
cp ld-linux-x86-64.so.2 "$PKGDIR/lib64/"
EOF

cat > "$ROOT/var/lib/pkg/recipes/zlib/pkg-meta.toml" <<EOF
name = "zlib"
version = "$ZLIB_VER"
category = "libs"

[dependencies]
runtime = ["glibc"]
EOF
cat > "$ROOT/var/lib/pkg/recipes/zlib/build" <<EOF
tar xzf zlib-$ZLIB_VER.tar.gz
cd zlib-$ZLIB_VER
./configure --prefix=/usr
make -j1
EOF
cat > "$ROOT/var/lib/pkg/recipes/zlib/install" <<EOF
make -C zlib-$ZLIB_VER DESTDIR="\$PKGDIR" install
EOF

cat > "$ROOT/var/lib/pkg/recipes/zconsumer/pkg-meta.toml" <<'EOF'
name = "zconsumer"
version = "1"
category = "app"

[dependencies]
link = ["zlib>=1"]
runtime = ["glibc", "zlib>=1"]
EOF
cat > "$ROOT/var/lib/pkg/recipes/zconsumer/build" <<'EOF'
tar xzf zconsumer-1.tar.gz
cd zconsumer-1
cc -O2 -o zconsumer main.c -I /pkg/deps/zlib/usr/include \
   -L /pkg/deps/zlib/usr/lib -lz -Wl,-rpath,/usr/lib
EOF
cat > "$ROOT/var/lib/pkg/recipes/zconsumer/install" <<'EOF'
mkdir -p "$PKGDIR/usr/bin"
cp zconsumer-1/zconsumer "$PKGDIR/usr/bin/"
EOF

# ---------------------------------------------------------------
# 4. Config + toolchain farm + bwrap shim (see e2e/run.sh).
# ---------------------------------------------------------------
mkdir -p "$ROOT/etc/pkg"
printf 'mirrors = [\n  "%s",\n]\n' "$BASE" > "$ROOT/etc/pkg/mirrors.toml"

for tool in env sh bash cc gcc ar ranlib make tar gzip sed grep awk cut \
            diff find xargs uname install cp mv mkdir ln rm cat chmod \
            touch basename dirname sort uniq wc tr head tail ld as nm \
            readelf objcopy strip sleep date expr printf test; do
    p=$(command -v "$tool" 2>/dev/null) && ln -sf "$p" "$TOOLBIN/$tool"
done

cat > "$E2E/shim/bwrap" <<EOF
#!/bin/sh
filtered() {
    while [ "\$#" -gt 0 ]; do
        case "\$1" in
            --ro-bind|--bind)
                if [ -e "\$2" ]; then
                    printf '%s\0%s\0%s\0' "\$1" "\$2" "\$3"
                else
                    echo "bwrap-shim: skipping missing bind source \$2" >&2
                fi
                shift 3 ;;
            --)
                printf '%s\0' --ro-bind /nix/store /nix/store
                printf '%s\0' --ro-bind "$TOOLBIN" /usr/bin
                printf '%s\0' "\$@"
                break ;;
            *) printf '%s\0' "\$1"; shift ;;
        esac
    done
}
filtered "\$@" | xargs -0 sh -c 'exec "\$0" "\$@"' "$REAL_BWRAP"
EOF
chmod +x "$E2E/shim/bwrap"
export PATH="$E2E/shim:$PATH"

# ---------------------------------------------------------------
# 5. Serve, trust, sync.
# ---------------------------------------------------------------
python3 -m http.server "$PORT" --directory "$REPO" >/dev/null 2>&1 &
HTTP_PID=$!
trap 'kill $HTTP_PID 2>/dev/null || true' EXIT
sleep 1

echo yes | "$PKG" key trust-root "$REPO/root.json" >/dev/null
"$PKG" sync >/dev/null
pass "trust bootstrap + sync"

# ---------------------------------------------------------------
# 6. Build + install the real package and its consumer.
# ---------------------------------------------------------------
if ! "$PKG" install -y zconsumer >"$E2E/install.log" 2>&1; then
    echo "---- zconsumer install output ----" >&2
    cat "$E2E/install.log" >&2
    fail "pkg install zconsumer (pulls zlib)"
fi
pass "pkg install zconsumer resolved + built zlib $ZLIB_VER"

[ -e "$ROOT/usr/lib/libz.so.$ZLIB_VER" ] || fail "zlib activated into live tree"
pass "libz.so.$ZLIB_VER activated into live tree"
[ -f "$ROOT/usr/include/zlib.h" ] || fail "zlib headers installed"
pass "zlib headers installed"

out=$(LD_LIBRARY_PATH="$ROOT/usr/lib" "$ROOT/usr/bin/zconsumer") \
    || fail "zconsumer runs"
[ "$out" = "zlib $ZLIB_VER compressed 23 -> 31" ] \
    || { echo "got: $out" >&2; fail "zconsumer output"; }
pass "zconsumer compresses through the pkg-built zlib ($out)"

"$PKG" db check >/dev/null && pass "pkg db check" || fail "pkg db check"

# The real build must record real provenance too.
hfp=$(sqlite3 "$ROOT/var/lib/pkg/db/pkg.sqlite" \
    "SELECT li.header_fingerprint FROM link_interface li JOIN packages p ON p.id=li.package_id WHERE p.name='zlib'")
[ -n "$hfp" ] || fail "zlib header_fingerprint recorded"
pass "zlib header_fingerprint recorded (ships zlib.h)"

echo
echo "REAL-PACKAGE E2E PASSED"
