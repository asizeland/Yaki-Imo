#!/bin/sh
# e2e/dkms.sh — transaction-driven DKMS rebuilds against fabricated kernels.
set -eu

if [ -z "${E2E_INNER:-}" ]; then
    exec nix shell \
        nixpkgs#gcc nixpkgs#cargo nixpkgs#rustc nixpkgs#bubblewrap nixpkgs#python3 \
        nixpkgs#coreutils nixpkgs#gnutar nixpkgs#gzip nixpkgs#gnumake \
        nixpkgs#sqlite nixpkgs#binutils nixpkgs#gnused nixpkgs#gnugrep \
        nixpkgs#gawk nixpkgs#diffutils nixpkgs#kmod \
        -c env E2E_INNER=1 sh "$0" "$@"
fi

WS=$(cd "$(dirname "$0")/.." && pwd)
cd "$WS"
E2E=/tmp/pkg-e2e-dkms
ROOT=$E2E/root
REPO=$E2E/repo
TOOLBIN=$E2E/toolbin
PORT=8786
BASE=http://127.0.0.1:$PORT
export PKG_ROOT=$ROOT

pass() { echo "PASS: $1"; }
fail() { echo "FAIL: $1" >&2; exit 1; }

cargo build --offline -q -p pkg-cli
cargo build --offline -q -p pkg-repo --example mkrepo
PKG=$WS/target/debug/pkg-cli
MKREPO=$WS/target/debug/examples/mkrepo
REAL_BWRAP=$(command -v bwrap)

rm -rf "$E2E"
mkdir -p "$ROOT" "$REPO" "$TOOLBIN" "$E2E/shim"
"$MKREPO" "$REPO" "$BASE" \
    "dkms-tool:1:e2e0001:false:" \
    "kernel-a:1:e2e0001:false:" \
    "kernel-b:1:e2e0001:false:" \
    "kernel-c:1:e2e0001:false:" \
    "example-module:1:e2e0001:false:" >/dev/null

for package in dkms-tool kernel-a kernel-b kernel-c example-module; do
    dir="$ROOT/var/lib/pkg/recipes/$package"
    mkdir -p "$dir"
    echo e2e0001 > "$dir/.recipe-commit"
done

cat > "$ROOT/var/lib/pkg/recipes/dkms-tool/pkg-meta.toml" <<'EOF'
name = "dkms-tool"
version = "1"
category = "sys-devel"
[build]
binary = true
EOF
cat > "$ROOT/var/lib/pkg/recipes/dkms-tool/install" <<'EOF'
mkdir -p "$PKGDIR/usr/sbin"
cat > "$PKGDIR/usr/sbin/dkms" <<'FAKE'
#!/bin/bash
action= state= sources= installtree= module= version= kernel= jobs=
while [ "$#" -gt 0 ]; do
    case "$1" in
        add|build|install|remove) action=$1 ;;
        --dkmstree) state=$2; shift ;;
        --sourcetree) sources=$2; shift ;;
        --installtree) installtree=$2; shift ;;
        -m) module=$2; shift ;;
        -v) version=$2; shift ;;
        -k) kernel=$2; shift ;;
        -j) jobs=$2; shift ;;
    esac
    shift
done
printf '%s %s %s %s\n' "$action" "$module" "$version" "$kernel" >> "$state/calls"
case "$action" in
    add)
        mkdir -p "$state/$module/$version"
        ln -s "$sources/$module-$version" "$state/$module/$version/source"
        ;;
    build)
        printf '%s\n' "$jobs" > "$state/jobs-$kernel"
        [ "$kernel" != 6.3-test ] || { echo "intentional DKMS failure"; exit 1; }
        ;;
    install)
        mkdir -p "$installtree/$kernel/extra"
        printf 'module for %s\n' "$kernel" > "$installtree/$kernel/extra/example.ko"
        ;;
esac
FAKE
chmod +x "$PKGDIR/usr/sbin/dkms"
EOF

for item in kernel-a:6.1-test kernel-b:6.2-test kernel-c:6.3-test; do
    package=${item%%:*}
    release=${item#*:}
    cat > "$ROOT/var/lib/pkg/recipes/$package/pkg-meta.toml" <<EOF
name = "$package"
version = "1"
category = "sys-kernel"
provides = ["virtual/kernel"]
[build]
binary = true
EOF
    cat > "$ROOT/var/lib/pkg/recipes/$package/install" <<EOF
mkdir -p "\$PKGDIR/usr/lib/modules/$release/build"
printf 'all:\n\t@true\n' > "\$PKGDIR/usr/lib/modules/$release/build/Makefile"
EOF
done

cat > "$ROOT/var/lib/pkg/recipes/example-module/pkg-meta.toml" <<'EOF'
name = "example-module"
version = "1"
category = "x11-drivers"
[dependencies]
runtime = ["dkms-tool"]
[build]
binary = true
[dkms]
name = "example"
version = "1"
EOF
cat > "$ROOT/var/lib/pkg/recipes/example-module/install" <<'EOF'
mkdir -p "$PKGDIR/usr/src/example-1"
printf 'PACKAGE_NAME="example"\nPACKAGE_VERSION="1"\n' > "$PKGDIR/usr/src/example-1/dkms.conf"
EOF

mkdir -p "$ROOT/etc/pkg"
printf 'mirrors = [\n  "%s",\n]\n' "$BASE" > "$ROOT/etc/pkg/mirrors.toml"
printf '[build]\njobs = 1\nmax_pkg_jobs = 7\n' > "$ROOT/etc/pkg/config.toml"
for tool in env sh bash make tar gzip sed grep awk find xargs uname install cp mv mkdir \
            ln rm cat chmod touch basename dirname sort uniq wc tr head tail sleep \
            date expr printf test depmod id readlink mktemp rmdir; do
    p=$(command -v "$tool" 2>/dev/null || true)
    case "$p" in /*) ln -sf "$(readlink -f "$p")" "$TOOLBIN/$tool" ;; esac
done

cat > "$E2E/shim/bwrap" <<EOF
#!/bin/sh
filtered() {
    while [ "\$#" -gt 0 ]; do
        case "\$1" in
            --ro-bind|--bind)
                if [ -e "\$2" ]; then
                    printf '%s\0%s\0%s\0' "\$1" "\$2" "\$3"
                fi
                shift 3 ;;
            --)
                if [ -d /nix/store ]; then
                    printf '%s\0' --ro-bind /nix/store /nix/store
                    printf '%s\0' --ro-bind "$TOOLBIN" /usr/bin
                fi
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

"$PKG" install -y kernel-a example-module >"$E2E/initial.log" 2>&1 \
    || { cat "$E2E/initial.log" >&2; fail "initial DKMS install"; }
[ -s "$ROOT/usr/lib/modules/6.1-test/extra/example.ko" ] \
    || fail "module installed for first kernel"
grep -q '^build example 1 6.1-test$' "$ROOT/var/lib/dkms/calls" \
    || fail "real transaction invoked DKMS build"
[ "$(cat "$ROOT/var/lib/dkms/jobs-6.1-test")" = 7 ] \
    || fail "max_pkg_jobs did not reach DKMS"
[ ! -e "$ROOT/.pkg-recipe-manifest.toml" ] \
    || fail "installed recipe sidecar leaked into the FHS root"
find "$ROOT/usr/lib/pkg/.store" \
    -path '*/example-module-1+*/.pkg-recipe-manifest.toml' | grep -q . \
    || fail "installed recipe metadata not retained"
pass "driver installation builds its module for the installed kernel"

"$PKG" install -y kernel-b >"$E2E/new-kernel.log" 2>&1 \
    || { cat "$E2E/new-kernel.log" >&2; fail "new kernel install"; }
[ -s "$ROOT/usr/lib/modules/6.2-test/extra/example.ko" ] \
    || fail "module installed for new kernel"
[ "$(grep -c '^build example 1 6.1-test$' "$ROOT/var/lib/dkms/calls")" = 1 ] \
    || fail "existing kernel was rebuilt unnecessarily"
pass "new kernel rebuilds the installed driver without rebuilding old kernels"

"$PKG" install -y kernel-c >"$E2E/failure.log" 2>&1 \
    || { cat "$E2E/failure.log" >&2; fail "DKMS failure aborted kernel installation"; }
[ -s "$ROOT/usr/lib/modules/6.3-test/build/Makefile" ] || fail "kernel-c not committed"
[ ! -e "$ROOT/usr/lib/modules/6.3-test/extra/example.ko" ] || fail "failed module was installed"
grep -q 'warning: DKMS rebuild failed for example-module against kernel 6.3-test' \
    "$E2E/failure.log" || fail "failure warning missing"
grep -q 'intentional DKMS failure' "$ROOT/var/log/pkg/dkms-example-module-6.3-test.log" \
    || fail "persistent DKMS log missing compiler output"
pass "failed module build warns, logs, and leaves the new kernel committed"

"$PKG" db check >/dev/null
pass "database remains consistent after DKMS transactions"
echo "DKMS E2E PASSED"
