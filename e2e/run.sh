#!/bin/sh
# e2e/run.sh — end-to-end smoke run of the whole pkg toolchain against a
# throwaway root (PKG_ROOT) and a fabricated, validly-signed test repo.
#
# Usage:  sh e2e/run.sh        (from the workspace root)
#
# The script re-enters itself inside `nix shell` (this host has no system
# toolchain). Everything lives under /tmp/pkg-e2e; the host system is
# untouched EXCEPT that the sandbox's bwrap invocation needs unprivileged
# user namespaces (enabled by default on NixOS).
#
# Host-quirk adaptations (all inside the sandbox shim, host untouched):
#  - a bwrap shim drops bind sources that don't exist here (/lib) and
#    adds RO binds of /nix/store (so the host /bin/sh symlink resolves)
#    and of a symlink farm of build tools at the toolchain path and
#    /usr/bin (NixOS has no /usr/bin toolset).

set -eu

if [ -z "${E2E_INNER:-}" ]; then
    exec nix shell \
        nixpkgs#gcc nixpkgs#perl nixpkgs#bubblewrap nixpkgs#python3 \
        nixpkgs#coreutils nixpkgs#gnutar nixpkgs#gzip nixpkgs#gnumake \
        nixpkgs#sqlite \
        -c env E2E_INNER=1 sh "$0" "$@"
fi

WS=$(cd "$(dirname "$0")/.." && pwd)
cd "$WS"

E2E=/tmp/pkg-e2e
ROOT=$E2E/root
REPO=$E2E/repo
SRC=$E2E/src
TOOLBIN=$E2E/toolbin
PORT=8777
BASE=http://127.0.0.1:$PORT

export PKG_ROOT=$ROOT

pass() { echo "PASS: $1"; }
fail() { echo "FAIL: $1" >&2; exit 1; }
check() { # check <desc> <command...>
    desc=$1; shift
    if "$@" >/dev/null 2>&1; then pass "$desc"; else fail "$desc"; fi
}

echo "== building workspace =="
cargo build --offline -q -p pkg-cli
cargo build --offline -q -p pkg-repo --example mkrepo
PKG=$WS/target/debug/pkg-cli
MKREPO=$WS/target/debug/examples/mkrepo
REAL_BWRAP=$(command -v bwrap)

rm -rf "$E2E"
mkdir -p "$ROOT" "$REPO" "$SRC" "$TOOLBIN" "$E2E/shim"

# ---------------------------------------------------------------
# 1. Sources: libfoo (a shared library) and hello (a C program that
#    runtime-depends on libfoo). glibc is a wrapper package around the
#    host libc — the build gate (v12 §9) requires libc.so.6 to be
#    provided by a declared dependency.
# ---------------------------------------------------------------
mkdir -p "$SRC/libfoo-1.0" "$SRC/hello-1.0"
cat > "$SRC/libfoo-1.0/foo.c" <<'EOF'
int foo_answer(void) { return 42; }
EOF
cat > "$SRC/hello-1.0/hello.c" <<'EOF'
#include <stdio.h>
extern int foo_answer(void);
int main(void) { printf("hello from pkg e2e: %d\n", foo_answer()); return 0; }
EOF
tar -czf "$SRC/libfoo-1.0.tar.gz" -C "$SRC" libfoo-1.0
tar -czf "$SRC/hello-1.0.tar.gz" -C "$SRC" hello-1.0

# Host glibc, as seen through ldd.
LIBC_PATH=$(ldd /bin/sh | awk '/libc\.so/{print $3; exit}')
LD_PATH=$(ldd /bin/sh | awk '/ld-linux/{print $1; exit}')
GLIBC_VER=$(ldd --version | head -1 | awk '{print $NF}')
echo "host glibc: $GLIBC_VER ($LIBC_PATH)"

# ---------------------------------------------------------------
# 2. Signed test repo.
# ---------------------------------------------------------------
"$MKREPO" "$REPO" "$BASE" \
    "glibc:$GLIBC_VER:e2e0001:false:" \
    "libfoo:1:e2e0001:false:$SRC/libfoo-1.0.tar.gz" \
    "hello:1:e2e0001:false:$SRC/hello-1.0.tar.gz" >/dev/null
pass "mkrepo fabricated signed repo"

# ---------------------------------------------------------------
# 3. Recipes checked out under the test root.
# ---------------------------------------------------------------
for p in glibc libfoo hello; do
    mkdir -p "$ROOT/var/lib/pkg/recipes/$p"
    echo "e2e0001" > "$ROOT/var/lib/pkg/recipes/$p/.recipe-commit"
done

cat > "$ROOT/var/lib/pkg/recipes/glibc/pkg-meta.toml" <<EOF
name = "glibc"
version = "$GLIBC_VER"
category = "libs"
EOF
cat > "$ROOT/var/lib/pkg/recipes/glibc/build" <<EOF
# Wrapper recipe: re-packages the host libc the toolchain links
# against (the e2e sandbox exposes /nix/store read-only).
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
mkdir -p "$PKGDIR/usr/lib"
cp libfoo-1.0/libfoo.so.1.0 "$PKGDIR/usr/lib/"
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
cat > "$ROOT/var/lib/pkg/recipes/hello/build" <<EOF
tar xzf hello-1.0.tar.gz
cd hello-1.0
# Dep store dirs are RO-bound into the sandbox at their real paths
# (PKG_ROOT-prefixed on this host).
LIBFOO=\$(echo $ROOT/usr/lib/pkg/.store/libfoo-*/usr/lib)
echo "linking against: \$LIBFOO"
cc -O2 -o hello hello.c -L "\$LIBFOO" -lfoo -Wl,-rpath,/usr/lib
EOF
cat > "$ROOT/var/lib/pkg/recipes/hello/install" <<'EOF'
mkdir -p "$PKGDIR/usr/bin"
cp hello-1.0/hello "$PKGDIR/usr/bin/"
EOF

# ---------------------------------------------------------------
# 4. Config: mirrors pointing at the local server.
# ---------------------------------------------------------------
mkdir -p "$ROOT/etc/pkg"
printf 'mirrors = [\n  "%s",\n]\n' "$BASE" > "$ROOT/etc/pkg/mirrors.toml"

# ---------------------------------------------------------------
# 5. Toolchain symlink farm + bwrap shim (host-quirk adaptation).
# ---------------------------------------------------------------
for tool in env sh bash cc gcc make tar gzip sed grep find xargs uname \
            install cp mv mkdir ln rm cat chmod touch basename dirname \
            sort uniq wc tr head tail ld as nm readelf objcopy strip \
            sleep date expr printf test; do
    p=$(command -v "$tool" 2>/dev/null) && ln -sf "$p" "$TOOLBIN/$tool"
done

cat > "$E2E/shim/bwrap" <<EOF
#!/bin/sh
# e2e bwrap shim: skip bind sources missing on this host; expose the
# nix store and the e2e toolchain overlay inside the sandbox.
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
# 6. Serve the repo over HTTP.
# ---------------------------------------------------------------
python3 -m http.server "$PORT" --directory "$REPO" >/dev/null 2>&1 &
HTTP_PID=$!
trap 'kill $HTTP_PID 2>/dev/null || true' EXIT
sleep 1

# ---------------------------------------------------------------
# 7. Trust bootstrap + sync.
# ---------------------------------------------------------------
echo yes | "$PKG" key trust-root "$REPO/root.json" >/dev/null
pass "key trust-root imported fabricated root"

"$PKG" sync >/dev/null
[ -f "$ROOT/var/cache/pkg/index/index.json" ] || fail "sync cached index"
pass "pkg sync fetched + verified signed index"

"$PKG" sync --status | grep -q "index version: 1" && pass "sync --status" || fail "sync --status"

# ---------------------------------------------------------------
# 8. Tamper rejection: corrupt the served index, force re-sync.
# ---------------------------------------------------------------
cp "$REPO/index.json" "$E2E/index.json.good"
printf 'tampered' | dd of="$REPO/index.json" bs=1 seek=20 conv=notrunc 2>/dev/null
if "$PKG" sync --force >/dev/null 2>&1; then
    fail "tampered index was accepted"
else
    pass "tampered index rejected"
fi
cp "$E2E/index.json.good" "$REPO/index.json"
"$PKG" sync --force >/dev/null
pass "re-sync after restore"

# ---------------------------------------------------------------
# 9. Install glibc, libfoo, then hello (dep resolution).
# ---------------------------------------------------------------
"$PKG" install -y glibc >/dev/null 2>&1 || {
    echo "---- glibc install output ----" >&2
    "$PKG" install -y glibc >&2 || true
    fail "pkg install glibc"
}
pass "pkg install glibc (host-libc wrapper)"

"$PKG" install -y libfoo >/dev/null 2>&1 || {
    echo "---- libfoo install output ----" >&2
    "$PKG" install -y libfoo >&2 || true
    fail "pkg install libfoo"
}
pass "pkg install libfoo (source build in sandbox)"

ls "$ROOT"/usr/lib/pkg/.store/*/usr/lib/libfoo.so.1.0 >/dev/null 2>&1 || fail "libfoo store artifact"
pass "libfoo store artifact present"
[ -e "$ROOT/usr/lib/libfoo.so.1.0" ] || fail "libfoo activated symlink"
pass "libfoo activated into live tree"

"$PKG" install -y hello >/dev/null 2>&1 || fail "pkg install hello"
pass "pkg install hello (dependency resolved)"
out=$(LD_LIBRARY_PATH="$ROOT/usr/lib" "$ROOT/usr/bin/hello") \
    && [ "$out" = "hello from pkg e2e: 42" ] || fail "installed hello runs"
pass "installed hello binary runs (linked against libfoo)"

"$PKG" info hello >/dev/null && pass "pkg info hello" || fail "pkg info hello"
"$PKG" db check >/dev/null && pass "pkg db check" || fail "pkg db check"

# ---------------------------------------------------------------
# 10. Cache: rebuilding the identical identity is a cache hit.
# ---------------------------------------------------------------
out=$("$PKG" build hello 2>&1) || fail "pkg build hello (cache)"
echo "$out" | grep -q "1 from cache" && pass "rebuild of identical identity is a cache hit" \
    || { echo "$out" >&2; fail "cache hit"; }

# ---------------------------------------------------------------
# 11. Upgrade cascade: bump libfoo 1.0 -> 1.1 (interface change
#     rebuilds the dependent hello).
# ---------------------------------------------------------------
mkdir -p "$SRC/libfoo-1.1"
cat > "$SRC/libfoo-1.1/foo.c" <<'EOF'
int foo_answer(void) { return 43; }
int foo_extra(void) { return 1; }
EOF
tar -czf "$SRC/libfoo-1.1.tar.gz" -C "$SRC" libfoo-1.1

sed -i 's/^version = "1"$/version = "1.1"/' "$ROOT/var/lib/pkg/recipes/libfoo/pkg-meta.toml"
sed -i 's/libfoo-1\.0/libfoo-1.1/g' "$ROOT/var/lib/pkg/recipes/libfoo/build" \
    "$ROOT/var/lib/pkg/recipes/libfoo/install"
echo "e2e0002" > "$ROOT/var/lib/pkg/recipes/libfoo/.recipe-commit"

"$MKREPO" "$REPO" "$BASE" \
    "glibc:$GLIBC_VER:e2e0001:false:" \
    "libfoo:1.1:e2e0002:false:$SRC/libfoo-1.1.tar.gz" \
    "hello:1:e2e0001:false:$SRC/hello-1.0.tar.gz" >/dev/null
"$PKG" sync --force >/dev/null
"$PKG" upgrade -y libfoo >/dev/null 2>&1 || fail "pkg upgrade libfoo"
ls "$ROOT"/usr/lib/pkg/.store/*/usr/lib/libfoo.so.1.0 >/dev/null 2>&1 || fail "libfoo 1.1 store artifact"
pass "pkg upgrade libfoo 1.0 -> 1.1"
# The cascade must have rebuilt hello against the new interface:
# foo_answer now returns 43.
out=$(LD_LIBRARY_PATH="$ROOT/usr/lib" "$ROOT/usr/bin/hello") \
    && [ "$out" = "hello from pkg e2e: 43" ] || { echo "got: $out" >&2; fail "cascade rebuilt hello"; }
pass "cascade rebuilt hello against libfoo 1.1"

# ---------------------------------------------------------------
# 12. Security: plant an advisory for hello, re-sync, audit.
# ---------------------------------------------------------------
cat > "$E2E/advisory.json" <<'EOF'
{"format_version": 1, "advisories": [
  {"id": "E2E-2026-0001", "class": "vulnerability", "severity": "high",
   "package": "hello", "affected": "*", "fixed": null,
   "summary": "e2e planted advisory", "references": []}
]}
EOF
"$MKREPO" "$REPO" "$BASE" \
    "glibc:$GLIBC_VER:e2e0001:false:" \
    "libfoo:1.1:e2e0002:false:$SRC/libfoo-1.1.tar.gz" \
    "hello:1:e2e0001:false:$SRC/hello-1.0.tar.gz" \
    --advisory "$E2E/advisory.json" >/dev/null
"$PKG" sync --force >/dev/null
if "$PKG" audit 2>&1 | grep -q "E2E-2026-0001"; then
    pass "pkg audit finds planted advisory for installed hello"
else
    fail "pkg audit"
fi

echo
echo "ALL E2E CHECKS PASSED"
