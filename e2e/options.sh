#!/bin/sh
# e2e/options.sh — build options and store-GC/undo interplay:
#
#   - a recipe-declared option builds a distinct identity
#     (`+p<opt>` store dir), changes the artifact's behavior, and the
#     superseded variant's store dir is retained for undo;
#   - `pkg clean` keeps generation-pinned dirs (undo survives it);
#   - `pkg clean --garbage` releases the generation hold (with a
#     warning), and undo then fails honestly as non-restorable.
#
# Usage:  sh e2e/options.sh        (from the workspace root)
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

E2E=/tmp/pkg-e2e-opts
ROOT=$E2E/root
REPO=$E2E/repo
SRC=$E2E/src
TOOLBIN=$E2E/toolbin
PORT=8782
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
# 1. Sources: greeter changes its output with the `loud` option.
# ---------------------------------------------------------------
mkdir -p "$SRC/greeter-1"
cat > "$SRC/greeter-1/main.c" <<'EOF'
#include <stdio.h>
int main(void) {
#ifdef LOUD
    puts("GREETING FROM PKG E2E");
#else
    puts("greeting from pkg e2e");
#endif
    return 0;
}
EOF
tar -czf "$SRC/greeter-1.tar.gz" -C "$SRC" greeter-1

LIBC_PATH=$(ldd /bin/sh | awk '/libc\.so/{print $3; exit}')
LD_PATH=$(ldd /bin/sh | awk '/ld-linux/{print $1; exit}')
GLIBC_VER=$(ldd --version | head -1 | awk '{print $NF}')
echo "host glibc: $GLIBC_VER"

"$MKREPO" "$REPO" "$BASE" \
    "glibc:$GLIBC_VER:e2e0001:false:" \
    "greeter:1:e2e0001:false:$SRC/greeter-1.tar.gz" >/dev/null
pass "mkrepo fabricated"

# ---------------------------------------------------------------
# 2. Recipes.
# ---------------------------------------------------------------
for p in glibc greeter; do
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

cat > "$ROOT/var/lib/pkg/recipes/greeter/pkg-meta.toml" <<'EOF'
name = "greeter"
version = "1"
category = "app"

[dependencies]
runtime = ["glibc"]

[options.loud]
default = false
description = "SHOUT the greeting"
EOF
cat > "$ROOT/var/lib/pkg/recipes/greeter/build" <<'EOF'
tar xzf greeter-1.tar.gz
cd greeter-1
if [ "$PKG_OPT_LOUD" = "1" ]; then DEF=-DLOUD; else DEF=; fi
cc -O2 -o greeter main.c $DEF
EOF
cat > "$ROOT/var/lib/pkg/recipes/greeter/install" <<'EOF'
mkdir -p "$PKGDIR/usr/bin"
cp greeter-1/greeter "$PKGDIR/usr/bin/"
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
trap 'kill $HTTP_PID 2>/dev/null || true' EXIT
sleep 1

echo yes | "$PKG" key trust-root "$REPO/root.json" >/dev/null
"$PKG" sync >/dev/null
pass "trust bootstrap + sync"

# ---------------------------------------------------------------
# 4. Default build, then the loud variant: distinct identities.
# ---------------------------------------------------------------
"$PKG" install -y glibc >/dev/null 2>&1 || fail "install glibc"
"$PKG" install -y greeter >/dev/null 2>&1 || fail "install greeter (default opts)"
[ "$("$ROOT/usr/bin/greeter")" = "greeting from pkg e2e" ] || fail "default greeter output"
pass "default greeter build is quiet"

DEFAULT_DIR=$(echo "$ROOT"/usr/lib/pkg/.store/greeter-1+*)
case "$DEFAULT_DIR" in
    *+p*) fail "default build should not carry an opt tag" ;;
esac
pass "default store dir has no +p opt suffix"

"$PKG" install -y greeter --opt loud=1 >/dev/null 2>&1 || {
    echo "---- loud install output ----" >&2
    "$PKG" install -y greeter --opt loud=1 >&2 || true
    fail "install greeter --opt loud=1"
}
[ "$("$ROOT/usr/bin/greeter")" = "GREETING FROM PKG E2E" ] || fail "loud greeter output"
pass "--opt loud=1 rebuilt greeter (PKG_OPT_LOUD reached the sandbox)"

ls -d "$ROOT"/usr/lib/pkg/.store/greeter-1+*+p* >/dev/null 2>&1 \
    || fail "loud variant store dir carries a +p opt tag"
pass "loud variant lives under a distinct +p-tagged identity"

# The superseded default build is retained (generation-pinned) and
# db check recognizes both.
[ -d "$DEFAULT_DIR" ] || fail "default variant pruned too early"
"$PKG" db check >/dev/null 2>&1 || fail "db check with two greeter identities"
pass "superseded variant retained; db check recognizes both"

# ---------------------------------------------------------------
# 5. pkg clean keeps generation-pinned dirs; undo still works.
# ---------------------------------------------------------------
"$PKG" clean >/dev/null 2>&1 || fail "pkg clean"
[ -d "$DEFAULT_DIR" ] || fail "pkg clean pruned a generation-pinned dir"
pass "pkg clean keeps generation-pinned dirs"

"$PKG" undo -y >/dev/null 2>&1 || fail "pkg undo to quiet greeter"
[ "$("$ROOT/usr/bin/greeter")" = "greeting from pkg e2e" ] || fail "undo restored quiet greeter"
pass "undo after clean restored the quiet build"

# ---------------------------------------------------------------
# 6. --garbage releases the hold; undo then fails honestly.
# ---------------------------------------------------------------
"$PKG" install -y greeter --opt loud=1 >/dev/null 2>&1 || fail "reinstall loud greeter"
"$PKG" clean --garbage >/dev/null 2>"$E2E/garbage.err" || fail "pkg clean --garbage"
grep -q "non-restorable" "$E2E/garbage.err" \
    && pass "--garbage warns which generations become non-restorable" \
    || fail "--garbage warning"
[ ! -d "$DEFAULT_DIR" ] || fail "--garbage did not prune the old variant"
pass "--garbage pruned the superseded variant"

if "$PKG" undo -y >/dev/null 2>&1; then
    fail "undo succeeded despite pruned store dirs"
else
    "$PKG" undo -y 2>&1 | grep -q "no longer restorable" \
        && pass "undo after --garbage fails as non-restorable" \
        || fail "undo failure should name restorability"
fi

# The live system is untouched by the failed undo.
[ "$("$ROOT/usr/bin/greeter")" = "GREETING FROM PKG E2E" ] \
    && pass "failed undo left the live tree untouched" \
    || fail "live tree after failed undo"
"$PKG" db check >/dev/null 2>&1 || fail "final db check"
pass "final db check clean"

# ---------------------------------------------------------------
# 7. Parallelism control: [build] jobs in config.toml + --jobs.
# ---------------------------------------------------------------
printf '[build]\njobs = 2\nmax_pkg_jobs = 2\n' > "$ROOT/etc/pkg/config.toml"
# Reinstall the default (quiet) variant — its store dir was pruned by
# --garbage, so this also exercises rebuild-after-GC.
"$PKG" install -y greeter --opt loud=0 >/dev/null 2>&1 || fail "install with [build] jobs config"
[ "$("$ROOT/usr/bin/greeter")" = "greeting from pkg e2e" ] || fail "quiet greeter after jobs config"
pass "[build] jobs/max_pkg_jobs config accepted"

"$PKG" install -y greeter --opt loud=1 --jobs 1 >/dev/null 2>&1 || fail "install with --jobs 1"
[ "$("$ROOT/usr/bin/greeter")" = "GREETING FROM PKG E2E" ] || fail "loud greeter after --jobs 1"
pass "--jobs flag overrides config"

printf '[build]\njobs = 0\n' > "$ROOT/etc/pkg/config.toml"
if "$PKG" install -y greeter >/dev/null 2>"$E2E/jobs.err"; then
    fail "jobs = 0 accepted"
else
    grep -q "jobs" "$E2E/jobs.err" \
        && pass "invalid [build] jobs is a hard error" \
        || fail "jobs=0 error should name the setting"
fi
rm -f "$ROOT/etc/pkg/config.toml"
"$PKG" db check >/dev/null 2>&1 || fail "db check after jobs tests"
pass "db check clean after jobs tests"

echo
echo "OPTIONS E2E PASSED"
