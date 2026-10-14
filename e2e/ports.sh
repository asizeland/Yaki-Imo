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
        nixpkgs#findutils nixpkgs#util-linux nixpkgs#bison nixpkgs#m4 \
        nixpkgs#gettext nixpkgs#flex nixpkgs#gperf \
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
# 2. Seed loader (host-quirk adaptation): binaries linked in the
#    sandbox carry the PACKAGED interpreter (/lib64/ld-linux, stamped
#    by the cc wrappers below) — but build-time helper programs
#    (configure conftests, ncurses' tic) must run in the sandbox
#    before glibc is installed. On a normal FHS host the loader is
#    simply there; here we bind the host's loader at /lib64. It
#    resolves its own libc from /nix/store (compiled-in default path),
#    which the shim binds RO for builds. The installed system never
#    sees this: its /lib64/ld-linux is the packaged glibc's.
# ---------------------------------------------------------------
SEED=$E2E/seed
mkdir -p "$SEED/lib64"
LD_PATH=$(ldd /bin/sh | awk '/ld-linux/{print $1; exit}')
cp "$(readlink -f "$LD_PATH")" "$SEED/lib64/ld-linux-x86-64.so.2"

# ---------------------------------------------------------------
# 3. ports tree -> signed repo; install recipes.
# ---------------------------------------------------------------
"$PORTS2REPO" "$REPO" "$BASE" --distfiles "$DIST" \
    "$WS/ports" >/dev/null
pass "ports2repo built signed repo from the tree"

mkdir -p "$ROOT/etc/pkg"
printf 'mirrors = [\n  "%s",\n]\n' "$BASE" > "$ROOT/etc/pkg/mirrors.toml"
# glibc at the default max_pkg_jobs=4 takes a while; open the taps.
printf '[build]\njobs = %s\nmax_pkg_jobs = %s\n' "$(nproc)" "$(nproc)" \
    > "$ROOT/etc/pkg/config.toml"
sh ports/install.sh "$REPO" >/dev/null
pass "recipes installed into \$PKG_ROOT"

# ---------------------------------------------------------------
# 4. Toolchain farm + bwrap shim (host-quirk adaptations, as run.sh).
# ---------------------------------------------------------------
# readlink -f: farm links must point at the real /nix/store path — a
# symlink into /run/current-system would dangle inside the sandbox.
for tool in env sh bash ar ranlib make tar gzip xz sed grep \
            awk gawk cut diff cmp find xargs uname install cp mv mkdir ls pwd \
            ln rm cat chmod touch basename dirname sort uniq wc tr head \
            tail ld as nm readelf objcopy strip sleep date expr printf \
            test comm m4 od id bison python3 echo true false readlink \
            msgfmt objdump perl rmdir mktemp flex gperf; do
    # command -v reports shell builtins (echo, test, printf) as bare
    # names, which would produce dangling farm links — and make execs
    # simple commands DIRECTLY (no shell), so a builtin is not good
    # enough. type -P skips builtins and finds the PATH binary.
    p=$(command -v "$tool" 2>/dev/null || true)
    case "$p" in
        /*) ;;
        *) p=$(type -P "$tool" 2>/dev/null || true) ;;
    esac
    [ -n "$p" ] && ln -sf "$(readlink -f "$p")" "$TOOLBIN/$tool" || true
done

# cc/gcc/g++ are wrapper scripts, not links: this host's gcc burns a
# /nix/store interpreter into every binary, which would make the
# installed root depend on /nix/store forever. Stamp the PACKAGED
# loader instead, on final links only (compile/assemble/shared-object
# invocations embed no interpreter). On a normal FHS host the system
# gcc already targets /lib64/ld-linux and none of this exists.
for cc_tool in cc gcc g++; do
    real=$(readlink -f "$(command -v "$cc_tool")")
    cat > "$TOOLBIN/$cc_tool" <<EOF
#!/bin/sh
case " \$* " in
    *" -c "*|*" -E "*|*" -S "*|*" -shared "*|*" -r "*|*" -print-"*) exec $real "\$@" ;;
esac
exec $real "\$@" -Wl,--dynamic-linker=/lib64/ld-linux-x86-64.so.2
EOF
    chmod +x "$TOOLBIN/$cc_tool"
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
                printf '%s\0%s\0%s\0' --ro-bind "$SEED/lib64" /lib64
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
# Binaries carry the packaged interpreter (/lib64/ld-linux), which does
# not exist on this host — run host-side smoke tests through the
# packaged loader explicitly. This doubles as the first proof that the
# pkg-built glibc actually works. (glibc's elf/Makefile installs the
# loader itself to /lib64; slibdir=/usr/lib covers the libraries.)
LOADER="$ROOT/lib64/ld-linux-x86-64.so.2 --library-path $RLIBS"
if ! "$PKG" install -y zlib bzip2 ncurses sqlite lua less tty-solitaire \
        man-pages pkgconf openssl expat ca-certificates curl git \
        >"$E2E/install.log" 2>&1; then
    echo "---- install output ----" >&2
    tail -40 "$E2E/install.log" >&2
    fail "pkg install of the ports tree"
fi
pass "installed the tree ($(ls "$ROOT/var/lib/pkg/recipes" | wc -l) recipes; tree subset + git chain)"

# --- smoke tests -------------------------------------------------
[ -x "$ROOT/lib64/ld-linux-x86-64.so.2" ] && [ -e "$RLIBS/libc.so.6" ] \
    || fail "glibc: loader + libc activated"
[ "$($ROOT/lib64/ld-linux-x86-64.so.2 --version | head -1 | awk '{print $NF}')" = "2.42." ] \
    || fail "glibc: packaged loader reports 2.42"
pass "glibc: built from source, loader runs (2.42)"

[ -e "$ROOT/usr/include/linux/unistd.h" ] || fail "linux-headers activated"
pass "linux-headers: UAPI headers in live tree"

[ -e "$RLIBS/libz.so.1.3.1" ] || fail "zlib lib activated"
pass "zlib: libz.so.1.3.1 in live tree"

[ "$(echo yakimo | $LOADER "$R/bzip2" | $LOADER "$R/bzip2" -d)" = "yakimo" ] \
    || fail "bzip2 round-trip"
pass "bzip2: compress/decompress round-trip"

[ -d "$ROOT/usr/share/terminfo/x" ] && [ -e "$RLIBS/libncursesw.so.6" ] \
    || fail "ncurses terminfo + libncursesw"
pass "ncurses: terminfo database + wide shared lib"

[ "$($LOADER "$R/sqlite3" :memory: 'select sqlite_version();')" = "3.45.3" ] \
    || fail "sqlite3 runs"
pass "sqlite: sqlite3 runs, reports 3.45.3"

# fts5 defaults ON (declared default = true).
$LOADER "$R/sqlite3" :memory: \
    'create virtual table t using fts5(x);' || fail "sqlite fts5 on by default"
pass "sqlite: fts5 active at the default selection"

[ "$($LOADER "$R/lua" -e 'print(2+2)')" = "4" ] \
    || fail "lua runs"
pass "lua: interpreter runs"

$LOADER "$R/less" --version >/dev/null 2>&1 \
    || fail "less --version"
pass "less: runs (linked against pkg-built ncurses)"

# tty-solitaire is a curses game (binary: ttysolitaire): run it on a
# pty against OUR terminfo; surviving until the timeout (124) proves
# the ncurses link works.
if timeout 8 script -qec \
    "env TERM=xterm TERMINFO=$ROOT/usr/share/terminfo HOME=$E2E/home $LOADER $R/ttysolitaire" \
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

# --- batch 1: the git chain ------------------------------------------
[ "$($LOADER "$R/pkgconf" --version)" = "2.3.0" ] || fail "pkgconf runs"
pass "pkgconf: runs (2.3.0)"

$LOADER "$R/openssl" version | grep -q "^OpenSSL 3\.5\.2 " || fail "openssl runs"
pass "openssl: runs (3.5.2)"

$LOADER "$R/curl" --version | head -1 | grep -q "OpenSSL/3.5.2 zlib/1.3.1" \
    || fail "curl links packaged openssl+zlib"
pass "curl: linked against pkg-built openssl + zlib"

# Real TLS end to end: packaged curl + packaged openssl + the packaged
# CA bundle verifying a live host.
[ "$($LOADER "$R/curl" -sS --cacert "$ROOT/etc/ssl/certs/ca-certificates.crt" \
        -o /dev/null -w '%{http_code}' https://example.com)" = "200" ] \
    || fail "curl https fetch through packaged CA bundle"
pass "curl: https fetch verified against the packaged CA bundle"

GITHOME=$E2E/home
[ "$(HOME=$GITHOME $LOADER "$R/git" --version)" = "git version 2.50.1" ] \
    || fail "git runs"
rm -rf "$E2E/git-smoke" && mkdir -p "$E2E/git-smoke"
( cd "$E2E/git-smoke" && HOME=$GITHOME $LOADER "$R/git" init -q . \
    && HOME=$GITHOME $LOADER "$R/git" -c user.email=t@t -c user.name=t \
        commit -q --allow-empty -m init \
    && HOME=$GITHOME $LOADER "$R/git" log --oneline | grep -q init ) \
    || fail "git init+commit round-trip"
pass "git: init/commit round-trip (https transport compiled in)"

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
if $LOADER "$R/sqlite3" :memory: \
        'create virtual table t using fts5(x);' 2>/dev/null; then
    fail "fts5 still present after --opt fts5=0"
fi
pass "sqlite: --opt fts5=0 rebuilt without fts5"
ls -d "$ROOT"/usr/lib/pkg/.store/sqlite-*+p* >/dev/null 2>&1 \
    || fail "fts5=0 identity carries a +p tag"
pass "sqlite: fts5=0 earned a +p opt_tag identity"

"$PKG" db check >/dev/null 2>&1 || fail "final db check"
pass "final db check clean"

# ---------------------------------------------------------------
# 9. The chrootable set + chroot-equivalence: with the root bound at /
#    (bwrap stands in for chroot — same filesystem view, no root
#    needed), the installed shell and tools must run self-contained.
#    This is what makes the live-media flow work: PKG_ROOT-install,
#    then chroot in and continue testing.
#
#    Stage-1 bootstrap proof: NO /nix/store bind, no LD_LIBRARY_PATH.
#    Every binary carries the packaged interpreter
#    (/lib64/ld-linux -> /usr/lib/ld-linux, from the pkg-built glibc)
#    and the loader's compiled-in default search path is /usr/lib.
#    The root runs on its own glibc alone.
# ---------------------------------------------------------------
"$PKG" install -y bash coreutils grep sed gawk tar gzip xz \
    >"$E2E/chrootable.log" 2>&1 \
    || { tail -30 "$E2E/chrootable.log" >&2; fail "chrootable set install"; }
pass "installed the chrootable set (bash, coreutils, grep, sed, gawk, tar, gzip, xz)"

# Activation links must be RELATIVE (no $PKG_ROOT baked in) or every
# one of them dangles the moment the root becomes /.
if find "$ROOT/usr/bin" -type l -lname "$ROOT/*" | grep -q .; then
    fail "activation wrote absolute symlinks (chroot-unsafe)"
fi
pass "activation links are relative (chroot-safe)"

CHROOT_OUT=$("$REAL_BWRAP" --unshare-all --bind "$ROOT" / \
    --proc /proc --dev /dev \
    --setenv PATH /usr/bin:/bin --setenv HOME /root \
    -- /usr/bin/bash -ec '
        ls /usr/bin | wc -l
        echo "hello yakimo" | grep -c yakimo
        echo abc | sed s/b/B/
        echo "1 2" | awk "{print \$2}"
        echo data | gzip | gzip -d
        echo data | xz | xz -d
        echo data | bzip2 | bzip2 -d
        tar cf - -C /usr/bin lua | tar tf - | wc -l
        lua -e "print(2+2)"
        sqlite3 :memory: "select sqlite_version();"
        less --version | head -1
    ' 2>&1) || { echo "$CHROOT_OUT" >&2; fail "chroot-equivalence run"; }
pass "chroot-equivalence: $(echo "$CHROOT_OUT" | head -1) binaries in /usr/bin, all tools run"

[ "$(echo "$CHROOT_OUT" | sed -n 2p)" = "1" ] || fail "grep inside chroot"
[ "$(echo "$CHROOT_OUT" | sed -n 3p)" = "aBc" ] || fail "sed inside chroot"
[ "$(echo "$CHROOT_OUT" | sed -n 6p)" = "data" ] || fail "xz inside chroot"
[ "$(echo "$CHROOT_OUT" | sed -n 7p)" = "data" ] || fail "bzip2 inside chroot"
[ "$(echo "$CHROOT_OUT" | sed -n 8p)" = "1" ] || fail "tar round-trip inside chroot"
[ "$(echo "$CHROOT_OUT" | sed -n 9p)" = "4" ] || fail "lua inside chroot (rpath/relative-link chain)"
[ "$(echo "$CHROOT_OUT" | sed -n 10p)" = "3.45.3" ] || fail "sqlite inside chroot"
pass "chroot-equivalence: grep/sed/gzip/xz/bzip2/tar/lua/sqlite/less all work from inside"

"$PKG" db check >/dev/null 2>&1 || fail "db check after chrootable set"
pass "db check clean after chrootable set"

# ---------------------------------------------------------------
# 10. sysvinit as PID 1 — the strongest "the installed tree works"
#     proof available: boot a container whose init comes from the
#     tree, and have it run a userspace action from inittab.
# ---------------------------------------------------------------
"$PKG" install -y sysvinit >"$E2E/sysvinit.log" 2>&1 \
    || { tail -20 "$E2E/sysvinit.log" >&2; fail "sysvinit install"; }
[ -e "$ROOT/sbin/init" ] || fail "init activated"
pass "sysvinit: installed (/sbin/init in the live tree)"

# A one-action test inittab: at sysinit, prove userspace ran by writing
# a marker into the (RW-bound) root.
cat > "$ROOT/etc/inittab" <<'EOF'
id:3:initdefault:
si::sysinit:/usr/bin/bash -c "echo yakimo-booted > /boot-marker"
EOF
rm -f "$ROOT/boot-marker"

# PID 1 inside a pid namespace: --as-pid-1 makes the payload PID 1
# (bwrap otherwise installs its own reaper), --uid 0 because init
# refuses non-root. --dev gives the minimal device set; init writes to
# /dev/console when present and tolerates its absence. Same stage-1
# proof as the chroot test: no /nix/store, no LD_LIBRARY_PATH — init,
# the loader, and libc all come from the pkg-built packages.
timeout 60 "$REAL_BWRAP" --unshare-all --as-pid-1 --uid 0 --gid 0 \
    --bind "$ROOT" / \
    --proc /proc --dev /dev --tmpfs /run --tmpfs /tmp \
    --setenv PATH /usr/bin:/bin:/sbin:/usr/sbin \
    --setenv HOME /root \
    -- /sbin/init >"$E2E/boot.log" 2>&1 &
BOOT_PID=$!
for _ in $(seq 1 50); do
    [ -f "$ROOT/boot-marker" ] && break
    sleep 0.2
done
kill "$BOOT_PID" 2>/dev/null || true
wait "$BOOT_PID" 2>/dev/null || true
[ "$(cat "$ROOT/boot-marker" 2>/dev/null)" = "yakimo-booted" ] \
    || { cat "$E2E/boot.log" >&2; fail "init did not run the sysinit action"; }
pass "sysvinit as PID 1: booted and ran the inittab sysinit action"
rm -f "$ROOT/boot-marker" "$ROOT/etc/inittab"

"$PKG" db check >/dev/null 2>&1 || fail "db check after sysvinit"
pass "db check clean after sysvinit"

# ---------------------------------------------------------------
# 11. Packaging aid: a new recipe that underdeclares its deps must get a
#    gate failure that names the PROVIDING package, not just the soname.
#    badlink links the host's zlib with explicit -I/-L (the sandbox
#    binds the host toolchain tree RO — what a packager hits on a
#    normal FHS host where /usr/include/zlib.h is simply there) but
#    declares only glibc.
# ---------------------------------------------------------------
ZLIB_OUT=$(nix build --no-link --print-out-paths nixpkgs#zlib)
ZLIB_DEV=$(nix build --no-link --print-out-paths nixpkgs#zlib.dev)

mkdir -p "$E2E/overlay3/app-misc/badlink"
cat > "$E2E/overlay3/app-misc/badlink/pkg-meta.toml" <<'EOF'
name = "badlink"
version = "1.0"
category = "app-misc"

[dependencies]
runtime = ["glibc"]
EOF
cat > "$E2E/overlay3/app-misc/badlink/build" <<EOF
cat > badlink.c <<'C'
#include <stdio.h>
#include <zlib.h>
int main(void) {
    unsigned char dst[64]; unsigned long n = sizeof dst;
    const unsigned char src[] = "yakimo";
    if (compress2(dst, &n, src, sizeof src, 6) != Z_OK) return 1;
    printf("compressed %lu\n", n);
    return 0;
}
C
cc -O2 -I$ZLIB_DEV/include badlink.c -o badlink -L$ZLIB_OUT/lib -lz
EOF
cat > "$E2E/overlay3/app-misc/badlink/install" <<'EOF'
mkdir -p "$PKGDIR/usr/bin"
cp badlink "$PKGDIR/usr/bin/"
EOF
: > "$E2E/overlay3/app-misc/badlink/source.toml"

"$PORTS2REPO" "$REPO" "$BASE" --distfiles "$DIST" \
    "$WS/ports" "$E2E/overlay3" >/dev/null
sh ports/install.sh "$REPO" >/dev/null
"$PKG" sync --force >/dev/null 2>&1

if "$PKG" build badlink >"$E2E/badlink.log" 2>&1; then
    fail "badlink build should have been rejected by the gate"
fi
grep -q "undeclared dependencies: libz.so.1" "$E2E/badlink.log" \
    || { cat "$E2E/badlink.log"; fail "gate did not name the undeclared soname"; }
pass "undeclared-dep gate failure names the soname"
grep -q "libz.so.1 — provided by installed package zlib" "$E2E/badlink.log" \
    || { cat "$E2E/badlink.log"; fail "no provider hint in the failure report"; }
pass "failure report hints the providing package (add it to [dependencies])"

"$PKG" db check >/dev/null 2>&1 || fail "db check after rejected build"
pass "db check clean after the rejected build"
echo
echo "PORTS E2E PASSED"
