#!/bin/sh
# e2e/lifecycle.sh — end-to-end run of the installed-system lifecycle
# against a throwaway root: removal protection + autoremove, tamper
# detection, upgrade + undo rollback, signed-index rollback rejection,
# source-tamper rejection, and mirror failover.
#
# Usage:  sh e2e/lifecycle.sh        (from the workspace root)
#
# Same host-quirk adaptations as e2e/run.sh (see its header).

set -eu

if [ -z "${E2E_INNER:-}" ]; then
    exec nix shell \
        nixpkgs#gcc nixpkgs#perl nixpkgs#bubblewrap nixpkgs#python3 \
        nixpkgs#coreutils nixpkgs#gnutar nixpkgs#gzip nixpkgs#gnumake \
        nixpkgs#sqlite nixpkgs#binutils nixpkgs#gnused nixpkgs#gnugrep \
        nixpkgs#gawk nixpkgs#diffutils \
        -c env E2E_INNER=1 sh "$0" "$@"
fi

WS=$(cd "$(dirname "$0")/.." && pwd)
cd "$WS"

E2E=/tmp/pkg-e2e-life
ROOT=$E2E/root
REPO=$E2E/repo
SRC=$E2E/src
TOOLBIN=$E2E/toolbin
PORT=8780
BASE=http://127.0.0.1:$PORT

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
mkdir -p "$ROOT" "$REPO" "$SRC" "$TOOLBIN" "$E2E/shim"

# ---------------------------------------------------------------
# 1. Sources: libfoo (shared lib), hello (links it), tampkg (target
#    for the source-tamper test later).
# ---------------------------------------------------------------
mkdir -p "$SRC/libfoo-1.0" "$SRC/hello-1.0" "$SRC/tampkg-1"
cat > "$SRC/libfoo-1.0/foo.c" <<'EOF'
int foo_answer(void) { return 42; }
EOF
cat > "$SRC/libfoo-1.0/foo.h" <<'EOF'
#ifndef FOO_H
#define FOO_H
int foo_answer(void);
#endif
EOF
cat > "$SRC/hello-1.0/hello.c" <<'EOF'
#include <stdio.h>
#include <foo.h>
int main(void) { printf("hello from pkg e2e: %d\n", foo_answer()); return 0; }
EOF
printf '#include <stdio.h>\nint main(void){puts("tampkg");return 0;}\n' \
    > "$SRC/tampkg-1/main.c"
for t in libfoo-1.0 hello-1.0 tampkg-1; do
    tar -czf "$SRC/$t.tar.gz" -C "$SRC" "$t"
done
cp "$SRC/tampkg-1.tar.gz" "$E2E/tampkg-1.tar.gz.good"

LIBC_PATH=$(ldd /bin/sh | awk '/libc\.so/{print $3; exit}')
LD_PATH=$(ldd /bin/sh | awk '/ld-linux/{print $1; exit}')
GLIBC_VER=$(ldd --version | head -1 | awk '{print $NF}')
echo "host glibc: $GLIBC_VER"

mkrepo_v1() {
    "$MKREPO" "$REPO" "$BASE" \
        "glibc:$GLIBC_VER:e2e0001:false:" \
        "libfoo:1:e2e0001:false:$SRC/libfoo-1.0.tar.gz" \
        "hello:1:e2e0001:false:$SRC/hello-1.0.tar.gz" >/dev/null
}
mkrepo_v1
pass "mkrepo v1 fabricated"

# ---------------------------------------------------------------
# 2. Recipes (same shape as e2e/run.sh; hello via /pkg/deps alias).
# ---------------------------------------------------------------
for p in glibc libfoo hello tampkg; do
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

cat > "$ROOT/var/lib/pkg/recipes/libfoo/pkg-meta.toml" <<'EOF'
name = "libfoo"
version = "1"
category = "libs"

[dependencies]
runtime = ["glibc"]
EOF
cat > "$ROOT/var/lib/pkg/recipes/libfoo/build" <<'EOF'
tar xzf libfoo-1.0.tar.gz
cd libfoo-1.0
cc -O2 -fPIC -shared -Wl,-soname,libfoo.so.1 -o libfoo.so.1.0 foo.c
EOF
cat > "$ROOT/var/lib/pkg/recipes/libfoo/install" <<'EOF'
mkdir -p "$PKGDIR/usr/lib" "$PKGDIR/usr/include"
cp libfoo-1.0/libfoo.so.1.0 "$PKGDIR/usr/lib/"
cp libfoo-1.0/foo.h "$PKGDIR/usr/include/"
ln -s libfoo.so.1.0 "$PKGDIR/usr/lib/libfoo.so.1"
ln -s libfoo.so.1.0 "$PKGDIR/usr/lib/libfoo.so"
EOF

cat > "$ROOT/var/lib/pkg/recipes/hello/pkg-meta.toml" <<'EOF'
name = "hello"
version = "1"
category = "app"

[dependencies]
link = ["libfoo>=1"]
runtime = ["glibc", "libfoo>=1"]
EOF
cat > "$ROOT/var/lib/pkg/recipes/hello/build" <<'EOF'
tar xzf hello-1.0.tar.gz
cd hello-1.0
cc -O2 -o hello hello.c -I /pkg/deps/libfoo/usr/include \
   -L /pkg/deps/libfoo/usr/lib -lfoo -Wl,-rpath,/usr/lib
EOF
cat > "$ROOT/var/lib/pkg/recipes/hello/install" <<'EOF'
mkdir -p "$PKGDIR/usr/bin"
cp hello-1.0/hello "$PKGDIR/usr/bin/"
EOF

cat > "$ROOT/var/lib/pkg/recipes/tampkg/pkg-meta.toml" <<'EOF'
name = "tampkg"
version = "1"
category = "app"

[dependencies]
runtime = ["glibc"]
EOF
cat > "$ROOT/var/lib/pkg/recipes/tampkg/build" <<'EOF'
tar xzf tampkg-1.tar.gz
cd tampkg-1
cc -O2 -o tampkg main.c
EOF
cat > "$ROOT/var/lib/pkg/recipes/tampkg/install" <<'EOF'
mkdir -p "$PKGDIR/usr/bin"
cp tampkg-1/tampkg "$PKGDIR/usr/bin/"
EOF

# ---------------------------------------------------------------
# 3. Config, toolchain farm, bwrap shim, server, trust, sync.
# ---------------------------------------------------------------
mkdir -p "$ROOT/etc/pkg"
printf 'mirrors = [\n  "%s",\n]\n' "$BASE" > "$ROOT/etc/pkg/mirrors.toml"

for tool in env sh bash cc gcc make tar gzip sed grep awk find xargs \
            uname install cp mv mkdir ln rm cat chmod touch basename \
            dirname sort uniq wc tr head tail ld as nm readelf objcopy \
            strip sleep date expr printf test; do
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

python3 -m http.server "$PORT" --directory "$REPO" >/dev/null 2>&1 &
HTTP_PID=$!
HTTP2_PID=""
trap 'kill $HTTP_PID $HTTP2_PID 2>/dev/null || true' EXIT
sleep 1

echo yes | "$PKG" key trust-root "$REPO/root.json" >/dev/null
"$PKG" sync >/dev/null
pass "trust bootstrap + sync"

# ---------------------------------------------------------------
# 4. Removal protection + autoremove (§16.6).
# ---------------------------------------------------------------
"$PKG" install -y glibc >/dev/null 2>&1 || fail "install glibc"
"$PKG" install -y hello >/dev/null 2>&1 || fail "install hello (pulls libfoo)"
out=$(LD_LIBRARY_PATH="$ROOT/usr/lib" "$ROOT/usr/bin/hello") \
    && [ "$out" = "hello from pkg e2e: 42" ] || fail "hello runs"
pass "glibc explicit + hello installed (libfoo as a dependency)"

if "$PKG" remove -y libfoo >/dev/null 2>&1; then
    fail "remove of a needed library was allowed"
else
    "$PKG" remove -y libfoo 2>&1 | grep -qi "hello" \
        && pass "remove libfoo refused (reverse dependent hello named)" \
        || fail "remove refusal should name the dependent"
fi

"$PKG" remove -y hello >/dev/null 2>&1 || fail "pkg remove hello"
[ ! -e "$ROOT/usr/bin/hello" ] || fail "hello symlink still present"
pass "hello removed from the live tree"

"$PKG" autoremove -y >/dev/null 2>&1 || fail "pkg autoremove"
[ ! -e "$ROOT/usr/lib/libfoo.so.1.0" ] || fail "orphaned libfoo still present"
[ -e "$ROOT/usr/lib/libc.so.6" ] || fail "explicitly-installed glibc was autoremoved"
pass "autoremove dropped orphaned libfoo, kept explicit glibc"

# ---------------------------------------------------------------
# 5. Tamper detection: corrupt a store file, db check must notice.
# ---------------------------------------------------------------
"$PKG" install -y hello >/dev/null 2>&1 || fail "reinstall hello"
STORE_LIB=$(echo "$ROOT"/usr/lib/pkg/.store/libfoo-*/usr/lib/libfoo.so.1.0)
cp "$STORE_LIB" "$E2E/libfoo.so.1.0.good"
printf 'tampered' >> "$STORE_LIB"
if "$PKG" db check >/dev/null 2>&1; then
    fail "db check accepted a tampered store file"
else
    pass "db check rejects a tampered store file"
fi
cp "$E2E/libfoo.so.1.0.good" "$STORE_LIB"
"$PKG" db check >/dev/null 2>&1 || fail "db check after restore"
pass "db check clean after restore"

# ---------------------------------------------------------------
# 6. Upgrade + undo (Issue 5): bump libfoo, then roll back.
# ---------------------------------------------------------------
mkdir -p "$SRC/libfoo-1.1"
cat > "$SRC/libfoo-1.1/foo.c" <<'EOF'
int foo_answer(void) { return 43; }
int foo_extra(void) { return 1; }
EOF
cat > "$SRC/libfoo-1.1/foo.h" <<'EOF'
#ifndef FOO_H
#define FOO_H
int foo_answer(void);
int foo_extra(void);
#endif
EOF
tar -czf "$SRC/libfoo-1.1.tar.gz" -C "$SRC" libfoo-1.1
sed -i 's/^version = "1"$/version = "1.1"/' "$ROOT/var/lib/pkg/recipes/libfoo/pkg-meta.toml"
sed -i 's/libfoo-1\.0/libfoo-1.1/g' "$ROOT/var/lib/pkg/recipes/libfoo/build" \
    "$ROOT/var/lib/pkg/recipes/libfoo/install"
echo "e2e0002" > "$ROOT/var/lib/pkg/recipes/libfoo/.recipe-commit"

"$MKREPO" "$REPO" "$BASE" --index-version 2 \
    "glibc:$GLIBC_VER:e2e0001:false:" \
    "libfoo:1.1:e2e0002:false:$SRC/libfoo-1.1.tar.gz" \
    "hello:1:e2e0001:false:$SRC/hello-1.0.tar.gz" >/dev/null
"$PKG" sync --force >/dev/null
"$PKG" upgrade -y libfoo >/dev/null 2>&1 || fail "pkg upgrade libfoo"
out=$(LD_LIBRARY_PATH="$ROOT/usr/lib" "$ROOT/usr/bin/hello") \
    && [ "$out" = "hello from pkg e2e: 43" ] || fail "upgrade rebuilt hello"
pass "upgrade to libfoo 1.1 cascaded (hello prints 43)"

"$PKG" history >/dev/null 2>&1 || fail "pkg history"
pass "pkg history lists generations"

# The upgrade cascade recorded two generations (libfoo, then the hello
# rebuild) sharing one cascade_id — undo rolls a cascade back as a
# unit (§4.2.5/Issue 5), so ONE undo restores the pre-upgrade state.
"$PKG" undo -y >/dev/null 2>&1 || fail "pkg undo"
out=$(LD_LIBRARY_PATH="$ROOT/usr/lib" "$ROOT/usr/bin/hello") \
    && [ "$out" = "hello from pkg e2e: 42" ] || { echo "got: $out" >&2; fail "undo restored pre-upgrade state"; }
pass "undo rolled the whole cascade back (hello prints 42)"

# ---------------------------------------------------------------
# 7. Index rollback rejection: re-serve an OLDER index_version.
# ---------------------------------------------------------------
mkrepo_v1  # index_version 1 < cached 2
if "$PKG" sync --force >/dev/null 2>&1; then
    fail "older index_version accepted (rollback!)"
else
    pass "older index_version rejected (rollback protection)"
fi
"$MKREPO" "$REPO" "$BASE" --index-version 2 \
    "glibc:$GLIBC_VER:e2e0001:false:" \
    "libfoo:1.1:e2e0002:false:$SRC/libfoo-1.1.tar.gz" \
    "hello:1:e2e0001:false:$SRC/hello-1.0.tar.gz" \
    "tampkg:1:e2e0001:false:$SRC/tampkg-1.tar.gz" >/dev/null
"$PKG" sync --force >/dev/null || fail "re-sync at cached version"
pass "re-sync at equal index_version accepted"

# ---------------------------------------------------------------
# 8. Source tamper: corrupt the SERVED tarball; the fetch-time hash
#    check against the signed manifest must reject it.
# ---------------------------------------------------------------
printf 'evil' > "$REPO/sources/tampkg-1.tar.gz"
if "$PKG" install -y tampkg >/dev/null 2>&1; then
    fail "tampered source tarball was built"
else
    pass "tampered source tarball rejected at fetch verification"
fi
cp "$E2E/tampkg-1.tar.gz.good" "$REPO/sources/tampkg-1.tar.gz"
"$PKG" install -y tampkg >/dev/null 2>&1 || fail "tampkg installs after restore"
[ -x "$ROOT/usr/bin/tampkg" ] || fail "tampkg activated"
pass "pristine source builds and installs after restore"

# ---------------------------------------------------------------
# 9. Mirror failover: first mirror dead, second serves the repo.
# ---------------------------------------------------------------
python3 -m http.server $((PORT + 1)) --directory "$REPO" >/dev/null 2>&1 &
HTTP2_PID=$!
sleep 1
printf 'mirrors = [\n  "http://127.0.0.1:9/dead",\n  "http://127.0.0.1:%s",\n]\n' \
    $((PORT + 1)) > "$ROOT/etc/pkg/mirrors.toml"
"$PKG" sync --force >/dev/null 2>&1 || fail "sync via fallback mirror"
pass "sync fails over to the second mirror"

"$PKG" db check >/dev/null 2>&1 || fail "final db check"
pass "final db check clean"

echo
echo "LIFECYCLE E2E PASSED"
