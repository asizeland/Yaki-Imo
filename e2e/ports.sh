#!/bin/sh
# e2e/ports.sh — build the whole seed ports tree through the real
# toolchain: fetch distfiles, turn ports/ into a signed repo with
# ports2repo, sync, install every package, smoke-test each, then
# exercise option-driven dependencies (lua[readline]) and an
# abi_affecting=false option flip (sqlite[fts5]).
#
# Usage:  sh e2e/ports.sh        (from the workspace root)
#
# Requires network access for the one-time distfile fetches (cached in
# /tmp/pkg-ports-dist afterwards). Discord is served but not installed
# here — e2e/real-binary.sh covers the binary path.

set -eu

if [ -z "${E2E_INNER:-}" ]; then
    exec nix shell \
        nixpkgs#gcc nixpkgs#perl nixpkgs#bubblewrap nixpkgs#python3 \
        nixpkgs#coreutils nixpkgs#gnutar nixpkgs#gzip nixpkgs#gnumake \
        nixpkgs#sqlite nixpkgs#binutils nixpkgs#gnused nixpkgs#gnugrep \
        nixpkgs#gawk nixpkgs#diffutils nixpkgs#curl nixpkgs#xz \
        nixpkgs#findutils nixpkgs#util-linux \
        -c env E2E_INNER=1 sh "$0" "$@"
fi

WS=$(cd "$(dirname "$0")/.." && pwd)
cd "$WS"

E2E=/tmp/pkg-e2e-ports
ROOT=$E2E/root
REPO=$E2E/repo
TOOLBIN=$E2E/toolbin
DIST=/tmp/pkg-ports-dist
PORT=8784
BASE=http://127.0.0.1:$PORT

export PKG_ROOT=$ROOT

pass() { echo "PASS: $1"; }
fail() { echo "FAIL: $1" >&2; exit 1; }

echo "== building workspace =="
cargo build --offline -q -p pkg-cli
cargo build --offline -q -p pkg-repo --example ports2repo
PKG=$WS/target/debug/pkg-cli
PORTS2REPO=$WS/target/debug/examples/ports2repo
REAL_BWRAP=$(command -v bwrap)

rm -rf "$E2E"
mkdir -p "$ROOT" "$REPO" "$TOOLBIN" "$E2E/shim" "$E2E/home" "$DIST"

# ---------------------------------------------------------------
# 1. Distfiles (tree-pinned URLs + hashes).
# ---------------------------------------------------------------
sh ports/fetch-distfiles.sh "$DIST" >/dev/null
pass "distfiles fetched and hash-verified"

# ---------------------------------------------------------------
# 2. Host-glibc overlay recipe (see e2e/run.sh for why).
# ---------------------------------------------------------------
LIBC_PATH=$(ldd /bin/sh | awk '/libc\.so/{print $3; exit}')
LD_PATH=$(ldd /bin/sh | awk '/ld-linux/{print $1; exit}')
GLIBC_VER=$(ldd --version | head -1 | awk '{print $NF}')
echo "host glibc: $GLIBC_VER"

mkdir -p "$E2E/overlay/sys-libs/glibc"
cat > "$E2E/overlay/sys-libs/glibc/pkg-meta.toml" <<EOF
name = "glibc"
version = "$GLIBC_VER"
category = "sys-libs"
EOF
cat > "$E2E/overlay/sys-libs/glibc/build" <<EOF
cp $LIBC_PATH libc.so.6
cp $LD_PATH ld-linux-x86-64.so.2
# Programs linking -lm/-ldl/-lpthread/-lrt need those sonames provided
# by the glibc package (modern glibc ships them as stub DSOs).
for lib in libm.so.6 libdl.so.2 libpthread.so.0 librt.so.1; do
    p=\$(dirname $LIBC_PATH)/\$lib
    [ -e "\$p" ] && cp "\$p" "\$lib"
done
EOF
cat > "$E2E/overlay/sys-libs/glibc/install" <<'EOF'
mkdir -p "$PKGDIR/usr/lib" "$PKGDIR/lib64"
cp libc.so.6 "$PKGDIR/usr/lib/"
cp ld-linux-x86-64.so.2 "$PKGDIR/lib64/"
for lib in libm.so.6 libdl.so.2 libpthread.so.0 librt.so.1; do
    [ -e "$lib" ] && cp "$lib" "$PKGDIR/usr/lib/"
done
EOF

# ---------------------------------------------------------------
# 3. ports tree + overlay -> signed repo; install recipes.
# ---------------------------------------------------------------
"$PORTS2REPO" "$REPO" "$BASE" --distfiles "$DIST" \
    "$WS/ports" "$E2E/overlay" >/dev/null
pass "ports2repo built signed repo from the tree"

mkdir -p "$ROOT/etc/pkg"
printf 'mirrors = [\n  "%s",\n]\n' "$BASE" > "$ROOT/etc/pkg/mirrors.toml"
sh ports/install.sh "$REPO" >/dev/null
pass "recipes installed into \$PKG_ROOT"

# ---------------------------------------------------------------
# 4. Toolchain farm + bwrap shim (host-quirk adaptations, as run.sh).
# ---------------------------------------------------------------
# readlink -f: farm links must point at the real /nix/store path — a
# symlink into /run/current-system would dangle inside the sandbox.
for tool in env sh bash cc gcc g++ ar ranlib make tar gzip xz sed grep \
            awk cut diff cmp find xargs uname install cp mv mkdir ls pwd \
            ln rm cat chmod touch basename dirname sort uniq wc tr head \
            tail ld as nm readelf objcopy strip sleep date expr printf \
            test comm m4 od id; do
    p=$(command -v "$tool" 2>/dev/null) && ln -sf "$(readlink -f "$p")" "$TOOLBIN/$tool"
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
# 6. Install the tree (readline deliberately absent — the lua option
#    test below pulls it in via add_deps).
# ---------------------------------------------------------------
R=$ROOT/usr/bin
RLIBS=$ROOT/usr/lib
if ! "$PKG" install -y zlib bzip2 ncurses sqlite lua less tty-solitaire \
        man-pages >"$E2E/install.log" 2>&1; then
    echo "---- install output ----" >&2
    tail -40 "$E2E/install.log" >&2
    fail "pkg install of the ports tree"
fi
pass "installed the whole tree (8 packages + glibc)"

# --- smoke tests -------------------------------------------------
[ -e "$RLIBS/libz.so.1.3.1" ] || fail "zlib lib activated"
pass "zlib: libz.so.1.3.1 in live tree"

[ "$(echo yakimo | "$R/bzip2" | "$R/bzip2" -d)" = "yakimo" ] \
    || fail "bzip2 round-trip"
pass "bzip2: compress/decompress round-trip"

[ -d "$ROOT/usr/share/terminfo/x" ] && [ -e "$RLIBS/libncursesw.so.6" ] \
    || fail "ncurses terminfo + libncursesw"
pass "ncurses: terminfo database + wide shared lib"

[ "$(LD_LIBRARY_PATH=$RLIBS "$R/sqlite3" :memory: 'select sqlite_version();')" = "3.45.3" ] \
    || fail "sqlite3 runs"
pass "sqlite: sqlite3 runs, reports 3.45.3"

# fts5 defaults ON (declared default = true).
LD_LIBRARY_PATH=$RLIBS "$R/sqlite3" :memory: \
    'create virtual table t using fts5(x);' || fail "sqlite fts5 on by default"
pass "sqlite: fts5 active at the default selection"

[ "$(LD_LIBRARY_PATH=$RLIBS "$R/lua" -e 'print(2+2)')" = "4" ] \
    || fail "lua runs"
pass "lua: interpreter runs"

LD_LIBRARY_PATH=$RLIBS "$R/less" --version >/dev/null 2>&1 \
    || fail "less --version"
pass "less: runs (linked against pkg-built ncurses)"

# tty-solitaire is a curses game (binary: ttysolitaire): run it on a
# pty against OUR terminfo; surviving until the timeout (124) proves
# the ncurses link works.
if timeout 8 script -qec \
    "env TERM=xterm TERMINFO=$ROOT/usr/share/terminfo HOME=$E2E/home LD_LIBRARY_PATH=$RLIBS $R/ttysolitaire" \
    /dev/null >/dev/null 2>&1; then
    : # exited cleanly before the timeout — also fine
else
    [ $? -eq 124 ] || fail "ttysolitaire died immediately (link problem?)"
fi
pass "tty-solitaire: initializes curses on a pty"

# (find -L: the live tree is symlinks into the store.)
n=$(find -L "$ROOT/usr/share/man" -type f | wc -l)
[ "$n" -gt 1000 ] || fail "man-pages installed ($n files)"
pass "man-pages: $n manual pages installed (data-only package)"

"$PKG" db check >/dev/null 2>&1 || fail "db check after tree install"
pass "db check clean"

# ---------------------------------------------------------------
# 7. Option-driven dependency: lua[opt:readline=1] pulls readline.
# ---------------------------------------------------------------
"$PKG" install -y 'lua[opt:readline=1]' >"$E2E/lua-rl.log" 2>&1 \
    || { tail -20 "$E2E/lua-rl.log" >&2; fail "lua[opt:readline=1]"; }
[ -e "$RLIBS/libreadline.so.8" ] || fail "readline built via add_deps"
pass "lua[readline]: add_deps pulled readline into the transaction"

LUABIN=$(readlink -f "$R/lua")
readelf -d "$LUABIN" | grep -q "NEEDED.*libreadline" \
    || fail "lua links libreadline"
pass "lua[readline]: rebuilt lua links libreadline (identity changed)"

# ---------------------------------------------------------------
# 8. abi_affecting=false flip: sqlite fts5 off = new identity, no
#    cascade.
# ---------------------------------------------------------------
"$PKG" install -y sqlite --opt fts5=0 >/dev/null 2>&1 \
    || fail "sqlite --opt fts5=0"
if LD_LIBRARY_PATH=$RLIBS "$R/sqlite3" :memory: \
        'create virtual table t using fts5(x);' 2>/dev/null; then
    fail "fts5 still present after --opt fts5=0"
fi
pass "sqlite: --opt fts5=0 rebuilt without fts5"
ls -d "$ROOT"/usr/lib/pkg/.store/sqlite-*+p* >/dev/null 2>&1 \
    || fail "fts5=0 identity carries a +p tag"
pass "sqlite: fts5=0 earned a +p opt_tag identity"

"$PKG" db check >/dev/null 2>&1 || fail "final db check"
pass "final db check clean"

echo
echo "PORTS E2E PASSED"
