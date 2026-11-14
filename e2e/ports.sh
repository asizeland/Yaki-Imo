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
        nixpkgs#gcc nixpkgs#cargo nixpkgs#rustc nixpkgs#perl nixpkgs#bubblewrap nixpkgs#python3 \
        nixpkgs#coreutils nixpkgs#gnutar nixpkgs#gzip nixpkgs#gnumake \
        nixpkgs#sqlite nixpkgs#binutils nixpkgs#gnused nixpkgs#gnugrep \
        nixpkgs#gawk nixpkgs#diffutils nixpkgs#curl nixpkgs#xz \
        nixpkgs#findutils nixpkgs#util-linux nixpkgs#bison nixpkgs#m4 \
        nixpkgs#gettext nixpkgs#flex nixpkgs#gperf nixpkgs#bzip2 nixpkgs#dash \
        nixpkgs#kmod \
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

# Packaged-loader seed (foreign-glibc hosts only — on NixOS this never
# triggers meaningfully, see the shim): once the packaged glibc is
# installed, a build whose sandbox binds /pkg/deps/glibc gets the
# packaged libc.so.6 injected via LD_LIBRARY_PATH. The HOST loader
# (seeded above) paired with the packaged libc is a mismatched pair and
# segfaults even trivial tools (Debian trixie: 2.41 loader + 2.42 libc
# kills tar at startup — every post-glibc build died at `tar xf`). The
# consistent pair is packaged loader + packaged libc, so the shim below
# swaps the /lib64 bind to a copy of the activated packaged loader for
# exactly those sandboxes. Sandboxes without the packaged glibc bound
# (pre-glibc builds, dep-less packages, fetch/hook) keep the host seed,
# whose loader/libc pair is equally consistent.
SEEDPKG=$E2E/seed-pkg
mkdir -p "$SEEDPKG/lib64"

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
            tail cpp ld as nm readelf objcopy strip sleep date dd expr printf \
            test comm m4 od id bison python3 echo true false readlink \
            msgfmt objdump perl rmdir mktemp flex gperf bzip2 depmod; do
    # command -v reports shell builtins (echo, test, printf) as bare
    # names, which would produce dangling farm links — and make execs
    # simple commands DIRECTLY (no shell), so a builtin is not good
    # enough. Fall back to a PATH walk for the real binary. (type -P
    # would do this in one call, but it is a bashism and /bin/sh may
    # be dash — e.g. in the Debian test container.)
    p=$(command -v "$tool" 2>/dev/null || true)
    case "$p" in
        /*) ;;
        *)
            p=""
            old_ifs=$IFS; IFS=:
            for d in $PATH; do
                if [ -x "$d/$tool" ] && [ ! -d "$d/$tool" ]; then
                    p=$d/$tool
                    break
                fi
            done
            IFS=$old_ifs
            ;;
    esac
    [ -n "$p" ] && ln -sf "$(readlink -f "$p")" "$TOOLBIN/$tool" || true
done

# /bin/sh inside the build sandbox must link libc only. Dep lib dirs go
# on LD_LIBRARY_PATH, and a readline-linked host bash picks up the
# PACKAGED readline through it (undefined-symbol crash the moment a
# recipe's dep closure contains readline — e.g. networkmanager). dash
# has no such links, so it is immune.
ln -sf "$(readlink -f "$(command -v dash)")" "$TOOLBIN/sh"

# cc/gcc/g++ are wrapper scripts, not links: this host's gcc burns a
# /nix/store interpreter into every binary, which would make the
# installed root depend on /nix/store forever. Stamp the PACKAGED
# loader instead, on final links only (compile/assemble/shared-object
# invocations embed no interpreter). On a normal FHS host the system
# gcc already targets /lib64/ld-linux and none of this exists.
for cc_tool in cc gcc g++ c++; do
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
# Cache a copy of the ACTIVATED packaged loader for binding at /lib64
# (see the seed-pkg note in ports.sh). Resolved host-side through the
# root's relative activation link; re-taken whenever the target changes.
# Never fatal — callers fall back to the host seed.
pkg_loader_seed() {
    src=\$(readlink -f "$ROOT/lib64/ld-linux-x86-64.so.2" 2>/dev/null) || return 1
    [ -f "\$src" ] || return 1
    [ "\$(cat "$SEEDPKG/.src" 2>/dev/null)" = "\$src" ] && return 0
    cp "\$src" "$SEEDPKG/lib64/.tmp.\$\$" || return 1
    mv -f "$SEEDPKG/lib64/.tmp.\$\$" "$SEEDPKG/lib64/ld-linux-x86-64.so.2" || return 1
    printf '%s' "\$src" > "$SEEDPKG/.src.\$\$" &&
        mv -f "$SEEDPKG/.src.\$\$" "$SEEDPKG/.src"
}
filtered() {
    PKG_GLIBC_BOUND=0
    DKMS_ROOT_ACTIVE=0
    TARGET_ROOT_ACTIVE=0
    linux_headers_src=
    while [ "\$#" -gt 0 ]; do
        case "\$1" in
            --ro-bind|--bind)
                if [ -e "\$2" ]; then
                    [ "\$3" = /pkg/deps/linux-headers ] && linux_headers_src=\$2
                    [ "\$2" = "$ROOT" ] && [ "\$3" = / ] && TARGET_ROOT_ACTIVE=1
                    printf '%s\0%s\0%s\0' "\$1" "\$2" "\$3"
                else
                    echo "bwrap-shim: skipping missing bind source \$2" >&2
                fi
                shift 3 ;;
            --setenv)
                if [ "\$2" = LD_LIBRARY_PATH ]; then
                    # A build that binds the packaged glibc must pair it
                    # with the packaged loader — swap /lib64 below.
                    case "\$3" in
                        */pkg/deps/glibc/*) PKG_GLIBC_BOUND=1 ;;
                        "$ROOT/usr/lib") PKG_GLIBC_BOUND=1; DKMS_ROOT_ACTIVE=1 ;;
                    esac
                    # Foreign-host aux libs: the sandbox binds no /etc
                    # (no ld.so.cache), and an upstream-glibc loader's
                    # defaults exclude distro triarch dirs — host tools
                    # would die on libacl & co. Append AFTER the dep dirs
                    # so the packaged libc still wins the soname race.
                    v="\$3"
                    for d in /usr/lib/x86_64-linux-gnu /lib/x86_64-linux-gnu; do
                        [ -d "\$d" ] && v="\$v:\$d"
                    done
                    printf '%s\0%s\0%s\0' "\$1" "\$2" "\$v"
                else
                    printf '%s\0%s\0%s\0' "\$1" "\$2" "\$3"
                fi
                shift 3 ;;
            --)
                if [ "\$TARGET_ROOT_ACTIVE" = 0 ]; then
                # NixOS hosts only: the store and the toolchain farm
                # must resolve inside the sandbox. FHS hosts already
                # have /usr /bin /lib bound by the profile — overlaying
                # the farm there would shadow /usr/bin with symlinks
                # that point back into the farm itself (ELOOP).
                if [ -d /nix/store ]; then
                    printf '%s\0' --ro-bind /nix/store /nix/store
                    # A writable mountpoint parent is needed for the later
                    # /usr/lib tmpfs when packaged glibc is bound.
                    printf '%s\0%s\0' --tmpfs /usr
                    printf '%s\0' --ro-bind "$TOOLBIN" /usr/bin
                fi
                # /bin too: the sandbox RO-binds host /bin, and on this
                # host /bin/sh is the readline-linked nix bash — fatal
                # once LD_LIBRARY_PATH carries dep lib dirs (see the sh
                # override above). Later binds shadow earlier ones.
                printf '%s\0' --ro-bind "$TOOLBIN" /bin
                # /lib64: the host seed loader — unless the packaged glibc
                # is bound, in which case its libc comes from /pkg/deps/glibc
                # and the loader must be the packaged one. This also matters
                # on NixOS once a recipe uses the PACKAGED gcc: its linker
                # scripts and interpreter use /usr/lib and /lib64, while Nix
                # tools continue to use their absolute /nix/store interpreters.
                ld64="$SEED/lib64"
                if [ "\$PKG_GLIBC_BOUND" = 1 ] && pkg_loader_seed; then
                    ld64="$SEEDPKG/lib64"
                    # The packaged libc.so & co. are ld SCRIPTS with
                    # absolute /usr/lib references (glibc slibdir). A foreign
                    # host has different files there; NixOS has no /usr/lib.
                    # Bind the packaged files or every link dies with
                    # "cannot find /usr/lib/libc.so.6". Binding files into an
                    # RO /usr/lib is impossible, and a symlink-farm overlay
                    # breaks C++ headers through .. traversal after symlink
                    # resolution. Mount TMPFS and bind real entries instead:
                    # host entries where present, packaged entries otherwise.
                    # DKMS needs the target's full library set (objtool's
                    # libelf is the concrete case), not just glibc's files.
                    if [ "\$DKMS_ROOT_ACTIVE" = 1 ]; then
                        printf '%s\0%s\0' --tmpfs /usr/lib
                        printf '%s\0%s\0%s\0' --ro-bind "$ROOT/usr/lib" /usr/lib
                    else
                    printf '%s\0%s\0' --tmpfs /usr/lib
                    hostbases=" "
                    for f in /usr/lib/*; do
                        if [ -e "\$f" ]; then
                            base=\${f##*/}
                            hostbases="\$hostbases\$base "
                            printf '%s\0%s\0%s\0' --ro-bind "\$f" "/usr/lib/\$base"
                        fi
                    done
                    glib=\$(dirname "\$(dirname "\$src")")/usr/lib
                    for f in "\$glib"/*; do
                        if { [ -e "\$f" ] || [ -L "\$f" ]; } && [ ! -d "\$f" ]; then
                            base=\${f##*/}
                            case "\$hostbases" in
                                *" \$base "*) ;;
                                *) printf '%s\0%s\0%s\0' --ro-bind "\$f" "/usr/lib/\$base" ;;
                            esac
                        fi
                    done
                    fi
                    if [ -d /nix/store ]; then
                        # Packaged gcc's C++ include_next search also needs
                        # the packaged C/UAPI headers at /usr/include.
                        printf '%s\0%s\0' --tmpfs /usr/include
                        glib_inc=\$(dirname "\$(dirname "\$src")")/usr/include
                        for inc in "\$glib_inc" "\$linux_headers_src/usr/include"; do
                            [ -d "\$inc" ] || continue
                            for f in "\$inc"/*; do
                                if [ -e "\$f" ]; then
                                    base=\${f##*/}
                                    printf '%s\0%s\0%s\0' --ro-bind "\$f" "/usr/include/\$base"
                                fi
                            done
                        done
                    fi
                fi
                printf '%s\0%s\0%s\0' --ro-bind "\$ld64" /lib64
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
if ! "$PKG" install -y base-files zlib bzip2 ncurses sqlite lua less tty-solitaire \
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
[ ! -e "$ROOT/usr/bin/memusagestat" ] \
    || fail "glibc: optional host-libgd-dependent memusagestat was built"
pass "glibc: built from source, loader runs (2.42), no host libgd probe"

[ -d "$ROOT/proc" ] && [ -d "$ROOT/sys" ] && [ -d "$ROOT/dev" ] \
    || fail "base-files: kernel filesystem mount points"
[ "$(stat -c %a "$ROOT/root")" = "700" ] || fail "base-files: /root mode"
[ "$(stat -c %a "$ROOT/tmp")" = "1777" ] || fail "base-files: /tmp mode"
[ "$(stat -c %a "$ROOT/var/tmp")" = "1777" ] || fail "base-files: /var/tmp mode"
[ "$(cat "$ROOT/usr/lib/os-release")" = 'NAME=Yakimo
ID=yakimo
PRETTY_NAME="Yakimo Linux"' ] || fail "base-files: os-release identity"
BASE_STORE=$(find "$ROOT/usr/lib/pkg/.store" -maxdepth 1 -type d -name 'base-files-*')
[ "$(readlink "$BASE_STORE/etc/os-release")" = "/usr/lib/os-release" ] \
    || fail "base-files: /etc/os-release"
[ "$(readlink "$BASE_STORE/etc/mtab")" = "/proc/self/mounts" ] || fail "base-files: /etc/mtab"
[ "$(readlink "$BASE_STORE/var/run")" = "/run" ] || fail "base-files: /var/run"
[ -L "$ROOT/etc/os-release" ] && [ -L "$ROOT/etc/mtab" ] && [ -L "$ROOT/var/run" ] \
    || fail "base-files: compatibility links activated"
grep -q '^root:x:0:0:' "$ROOT/etc/passwd" || fail "base-files: root account"
grep -q '^messagebus:x:1:1:' "$ROOT/etc/passwd" || fail "base-files: messagebus account"
grep -q '^wheel:x:10:' "$ROOT/etc/group" || fail "base-files: standard groups"
grep -q '^hosts: files dns$' "$ROOT/etc/nsswitch.conf" || fail "base-files: NSS configuration"
[ "$(stat -Lc %a "$ROOT/etc/shadow")" = "600" ] || fail "base-files: shadow mode"
[ "$(stat -Lc %a "$ROOT/etc/gshadow")" = "600" ] || fail "base-files: gshadow mode"
pass "base-files: hierarchy, modes, links, and locked system accounts activated"

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

# --- batch 2: the minimal-system set --------------------------------
if ! "$PKG" install -y util-linux iproute2 procps-ng e2fsprogs openssh \
        >"$E2E/install2.log" 2>&1; then
    tail -40 "$E2E/install2.log" >&2
    fail "pkg install batch 2"
fi
pass "installed batch 2 (util-linux, iproute2, procps-ng, e2fsprogs, openssh)"

# util-linux keeps its tools in /bin and /sbin (split-/usr tree).
$LOADER "$ROOT/bin/mount" --version | grep -q "util-linux 2\.41\.1" \
    || fail "util-linux mount runs"
pass "util-linux: mount runs (2.41.1)"

$LOADER "$ROOT/usr/sbin/ip" -V | grep -q "iproute2-6\.15\.0" \
    || fail "iproute2 ip runs"
pass "iproute2: ip runs (6.15.0)"

[ "$($LOADER "$R/ps" --version)" = "ps from procps-ng 4.0.5" ] \
    || fail "procps-ng ps runs"
pass "procps-ng: ps runs (4.0.5, linked against pkg-built ncursesw)"

# A real filesystem round-trip: build an ext4 image and read it back.
dd if=/dev/zero of="$E2E/fs.img" bs=1M count=8 2>/dev/null
$LOADER "$ROOT/usr/sbin/mke2fs" -q -t ext4 -F "$E2E/fs.img" \
    || fail "mke2fs formats an image"
$LOADER "$ROOT/usr/sbin/dumpe2fs" -h "$E2E/fs.img" 2>/dev/null \
    | grep -q "has_journal.*extent" || fail "dumpe2fs reads the image back"
rm -f "$E2E/fs.img"
pass "e2fsprogs: mke2fs formats, dumpe2fs reads back (ext4)"

$LOADER "$R/ssh" -V 2>&1 | grep -q "^OpenSSH_10\.0" || fail "openssh ssh runs"
[ -d "$ROOT/var/empty" ] || fail "openssh privilege-separation directory activated"
KEY="$E2E/ssh-smoke-key"
rm -f "$KEY" "$KEY.pub"
HOME=$E2E/home $LOADER "$R/ssh-keygen" -q -t ed25519 -N '' -f "$KEY" \
    || fail "ssh-keygen generates a key"
[ "$(HOME=$E2E/home $LOADER "$R/ssh-keygen" -y -f "$KEY" | cut -d' ' -f1-2)" \
    = "$(cut -d' ' -f1-2 "$KEY.pub")" ] || fail "ssh-keygen -y matches .pub"
rm -f "$KEY" "$KEY.pub"
pass "openssh: ssh runs, ssh-keygen keygen/derive round-trip"

"$PKG" db check >/dev/null 2>&1 || fail "db check after batch 2"
pass "db check clean after batch 2"

# --- batch 3a: the meson stack + NM's system deps --------------------
if ! "$PKG" install -y python3 libffi meson ninja pcre2 glib dbus kmod eudev \
        >"$E2E/install3a.log" 2>&1; then
    tail -40 "$E2E/install3a.log" >&2
    fail "pkg install batch 3a"
fi
pass "installed batch 3a (python3, meson+ninja, libffi, pcre2, glib, dbus, kmod, eudev)"

[ "$($LOADER "$R/python3" --version)" = "Python 3.13.5" ] || fail "python3 runs"
# ssl/ctypes/compression modules prove the openssl/libffi/zlib links.
$LOADER "$R/python3" -c \
    'import ssl, ctypes, zlib, xml.dom.expatbuilder; print(ssl.OPENSSL_VERSION)' \
    | grep -q "^OpenSSL 3\.5\.2 " || fail "python3 stdlib native modules load"
pass "python3: runs (3.13.5); ssl/ctypes/zlib/expat modules load"

[ -e "$RLIBS/libffi.so.8" ] || fail "libffi activated"
pass "libffi: libffi.so.8 in live tree"

# The meson wrapper is a /bin/sh script — run its payload through the
# loader instead (the live tree's /usr/bin/python3 is what the wrapper
# execs once booted).
[ "$($LOADER "$R/python3" "$ROOT/usr/lib/meson/meson.py" --version)" = "1.8.2" ] \
    || fail "meson runs"
pass "meson: runs (1.8.2, python3 payload)"

[ "$($LOADER "$R/ninja" --version)" = "1.12.1" ] || fail "ninja runs"
pass "ninja: runs (1.12.1, statically linked C++ runtime)"

$LOADER "$R/pcre2grep" --version | grep -q "^pcre2grep version 10\.45 " \
    || fail "pcre2grep runs"
pass "pcre2: pcre2grep runs (10.45)"

PKG_CONFIG_PATH="$RLIBS/pkgconfig" $LOADER "$R/pkgconf" --modversion glib-2.0 \
    | grep -q "^2\.84\.3$" || fail "glib pkg-config metadata"
$LOADER "$R/glib-compile-schemas" --version >/dev/null 2>&1 \
    || fail "glib-compile-schemas runs"
pass "glib: pkg-config metadata + tooling run (2.84.3)"

$LOADER "$R/dbus-daemon" --version | grep -q "D-Bus Message Bus Daemon 1\.16\.2" \
    || fail "dbus-daemon runs"
pass "dbus: dbus-daemon runs (1.16.2)"

$LOADER "$R/kmod" --version | grep -q "^kmod version 34\.1$" || fail "kmod runs"
[ -e "$ROOT/usr/sbin/modprobe" ] || fail "modprobe symlink activated"
pass "kmod: runs (34.1), modprobe/insmod/depmod links in /usr/sbin"

[ "$($LOADER "$R/udevadm" --version)" = "251" ] || fail "udevadm runs"
[ -e "$RLIBS/libudev.so" ] || fail "libudev activated"
pass "eudev: udevadm runs (compat 251), libudev in live tree"

"$PKG" db check >/dev/null 2>&1 || fail "db check after batch 3a"
pass "db check clean after batch 3a"

# --- batch 3b: the gnutls chain + NetworkManager ----------------------
if ! "$PKG" install -y gmp nettle libtasn1 p11-kit libunistring gnutls \
        libnl jansson libndp wpa_supplicant networkmanager \
        >"$E2E/install3b.log" 2>&1; then
    tail -40 "$E2E/install3b.log" >&2
    fail "pkg install batch 3b"
fi
pass "installed batch 3b (gnutls chain, libnl, jansson, libndp, wpa_supplicant, networkmanager)"

for lib in libgmp.so.10 libnettle.so.8 libhogweed.so.6 libtasn1.so.6 \
           libp11-kit.so.0 libunistring.so.5 libnl-3.so.200 \
           libjansson.so.4 libndp.so.0 libgnutls.so.30; do
    [ -e "$RLIBS/$lib" ] || fail "$lib activated"
done
pass "gnutls chain + libnl/jansson/libndp: shared libs in live tree"

$LOADER "$R/gnutls-cli" --version | grep -q "^gnutls-cli 3\.8\.9$" \
    || fail "gnutls-cli runs"
# Real TLS end to end, second implementation: packaged gnutls +
# packaged CA bundle against a live host (the curl/openssl path was
# proven above).
echo | timeout 20 $LOADER "$R/gnutls-cli" \
    --x509cafile="$ROOT/etc/ssl/certs/ca-certificates.crt" \
    example.com -p 443 2>&1 | grep -q "subject .CN=example.com" \
    || fail "gnutls-cli TLS handshake through packaged CA bundle"
pass "gnutls: TLS handshake verified against the packaged CA bundle"

$LOADER "$ROOT/usr/sbin/wpa_supplicant" -v 2>&1 | grep -q "wpa_supplicant v2\.11" \
    || fail "wpa_supplicant runs"
pass "wpa_supplicant: runs (2.11)"

[ "$($LOADER "$ROOT/usr/sbin/NetworkManager" --version)" = "1.50.0" ] \
    || fail "NetworkManager runs"
$LOADER "$R/nmcli" --version | grep -q "version 1\.50\.0" || fail "nmcli runs"
pass "networkmanager: daemon + nmcli run (1.50.0)"

"$PKG" db check >/dev/null 2>&1 || fail "db check after batch 3b"
pass "db check clean after batch 3b"

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
# 10.1 OpenRC runlevels: SysVinit remains PID 1, OpenRC orders services,
#      and a service in the default runlevel performs a real action.
# ---------------------------------------------------------------
"$PKG" install -y openrc eudev dbus networkmanager >"$E2E/openrc.log" 2>&1 \
    || { tail -30 "$E2E/openrc.log" >&2; fail "OpenRC and service package install"; }
$LOADER "$ROOT/sbin/openrc" --version | grep -q "0\.64\.1" \
    || fail "OpenRC runs"
for service in udev dbus NetworkManager; do
    [ -x "$ROOT/etc/init.d/$service" ] || fail "OpenRC service $service installed"
done
pass "OpenRC 0.64.1 + eudev/dbus/NetworkManager service scripts installed"

cat > "$ROOT/etc/init.d/yakimo-test" <<'EOF'
#!/sbin/openrc-run
description="Yakimo end-to-end service marker"
start() {
    ebegin "Writing the Yakimo boot marker"
    echo openrc-booted > /boot-marker
    eend 0
}
EOF
chmod 0755 "$ROOT/etc/init.d/yakimo-test"
mkdir -p "$ROOT/etc/runlevels/default"
ln -sf ../../init.d/yakimo-test "$ROOT/etc/runlevels/default/yakimo-test"
cat > "$ROOT/etc/inittab" <<'EOF'
id:3:initdefault:
si::sysinit:/sbin/openrc sysinit
bo::boot:/sbin/openrc boot
rc:3:wait:/sbin/openrc default
EOF
rm -f "$ROOT/boot-marker"
"$REAL_BWRAP" --die-with-parent --unshare-all --as-pid-1 --uid 0 --gid 0 \
    --bind "$ROOT" / --proc /proc --dev /dev --tmpfs /run --tmpfs /tmp \
    --setenv PATH /usr/bin:/bin:/sbin:/usr/sbin --setenv HOME /root \
    -- /sbin/init >"$E2E/openrc-boot.log" 2>&1 &
BOOT_PID=$!
for _ in $(seq 1 100); do
    [ -f "$ROOT/boot-marker" ] && break
    sleep 0.2
done
kill "$BOOT_PID" 2>/dev/null || true
wait "$BOOT_PID" 2>/dev/null || true
[ "$(cat "$ROOT/boot-marker" 2>/dev/null)" = "openrc-booted" ] \
    || { cat "$E2E/openrc-boot.log" >&2; fail "OpenRC default-runlevel service did not run"; }
pass "SysVinit + OpenRC: default-runlevel service ran as part of init"
rm -f "$ROOT/boot-marker" "$ROOT/etc/inittab" \
      "$ROOT/etc/runlevels/default/yakimo-test" "$ROOT/etc/init.d/yakimo-test"

# ---------------------------------------------------------------
# 11. Batch 4: the self-hosting toolchain. A live system must be able
#     to build its own packages — that needs make, the patch/diff
#     tools, the autotools generators, and above all gcc + binutils.
# ---------------------------------------------------------------
"$PKG" install -y make diffutils findutils patch m4 bison flex gperf \
    gettext mpfr mpc binutils gcc >"$E2E/install4.log" 2>&1 \
    || { tail -40 "$E2E/install4.log" >&2; fail "pkg install batch 4"; }
pass "installed batch 4 (make, diffutils, findutils, patch, m4, bison, flex, gperf, gettext, mpfr, mpc, binutils, gcc)"

$LOADER "$R/make" --version | head -1 | grep -q "GNU Make 4\.4\.1" \
    || fail "make runs"
$LOADER "$R/diff" --version | head -1 | grep -q "diffutils) 3\.12" \
    || fail "diff runs"
$LOADER "$R/find" --version | head -1 | grep -q "findutils) 4\.10" \
    || fail "find runs"
$LOADER "$R/patch" --version | head -1 | grep -q "GNU patch 2\.8" \
    || fail "patch runs"
$LOADER "$R/m4" --version | head -1 | grep -q "GNU M4) 1\.4\.20" \
    || fail "m4 runs"
"$REAL_BWRAP" --unshare-all --bind "$ROOT" / --proc /proc --dev /dev \
    --setenv PATH /usr/bin:/bin -- /usr/bin/bison --version | head -1 | grep -q "GNU Bison) 3\.8\.2" \
    || fail "bison runs inside the packaged root"
$LOADER "$R/flex" --version | head -1 | grep -q "flex 2\.6\.4" \
    || fail "flex runs"
$LOADER "$R/gperf" --version | head -1 | grep -q "GNU gperf 3\.3" \
    || fail "gperf runs"
$LOADER "$R/msgfmt" --version | head -1 | grep -q "gettext-tools) 0\.26" \
    || fail "msgfmt runs"
$LOADER "$R/as" --version | head -1 | grep -q "Binutils) 2\.45" \
    || fail "as runs"
pass "batch 4 tools: make/diff/find/patch/m4/bison/flex/gperf/msgfmt/as all run"

# The headline proof of self-hosting: from inside the chroot-equivalent
# (packaged loader, packaged libc, no host anything), the PACKAGED gcc
# + binutils compile and run C and C++ programs. gcc finds as/ld in
# /usr/bin, cc1/cc1plus under its own libexec, and the glibc headers
# and crt files in /usr — all from packages. (A script file instead of
# an inline one-liner: `bash -e` aborts on the C program's deliberate
# exit-42, and deeply nested printf quoting mangles the C++ source.)
cat > "$E2E/gcc-smoke.sh" <<'EOF'
#!/usr/bin/bash
set -e
printf 'int main(void){return 42;}\n' > /tmp/t.c
gcc /tmp/t.c -o /tmp/t
set +e
/tmp/t
echo "C exit: $?"
set -e
cat > /tmp/t.cc <<'CPP'
#include <iostream>
#include <vector>
int main() {
    std::vector<int> v{1, 2, 3};
    int s = 0;
    for (int x : v) s += x;
    std::cout << "cxx-ok " << s << "\n";
}
CPP
g++ /tmp/t.cc -o /tmp/tcxx
/tmp/tcxx
gcc --version | head -1
EOF
GCC_OUT=$("$REAL_BWRAP" --unshare-all --bind "$ROOT" / \
    --proc /proc --dev /dev --tmpfs /tmp \
    --ro-bind "$E2E/gcc-smoke.sh" /gcc-smoke.sh \
    --setenv PATH /usr/bin:/bin --setenv HOME /tmp \
    -- /usr/bin/bash /gcc-smoke.sh 2>&1) || { echo "$GCC_OUT" >&2; fail "packaged gcc compile test"; }
echo "$GCC_OUT" | grep -q "C exit: 42" || fail "C program compiled+ran (exit 42)"
echo "$GCC_OUT" | grep -q "cxx-ok 6" || fail "C++ program compiled+ran"
pass "gcc: compiled and ran C and C++ programs inside the packaged root"

"$PKG" db check >/dev/null 2>&1 || fail "db check after batch 4"
pass "db check clean after batch 4"

# ---------------------------------------------------------------
# 12. Packaging aid: a new recipe that underdeclares its deps must get a
#    gate failure that names the PROVIDING package, not just the soname.
#    badlink links the host's zlib with explicit -I/-L (the sandbox
#    binds the host toolchain tree RO — what a packager hits on a
#    normal FHS host where /usr/include/zlib.h is simply there) but
#    declares only glibc.
# ---------------------------------------------------------------
if command -v nix >/dev/null 2>&1; then
    ZLIB_INC=$(nix build --no-link --print-out-paths nixpkgs#zlib.dev)/include
    ZLIB_LIBDIR=$(nix build --no-link --print-out-paths nixpkgs#zlib)/lib
else
    # No nix on this host (e.g. the Debian test container): the system
    # zlib plays the same role — present in the sandbox through the RO
    # host binds, undeclared in the recipe.
    for d in /usr/include /usr/local/include; do
        [ -f "$d/zlib.h" ] && { ZLIB_INC=$d; break; }
    done
    for d in /usr/lib/x86_64-linux-gnu /usr/lib /usr/local/lib \
             /lib/x86_64-linux-gnu; do
        [ -e "$d/libz.so" ] && { ZLIB_LIBDIR=$d; break; }
    done
fi
: "${ZLIB_INC:?no host zlib headers for the badlink test}"
: "${ZLIB_LIBDIR:?no host libz for the badlink test}"

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
cc -O2 -I$ZLIB_INC badlink.c -o badlink -L$ZLIB_LIBDIR -lz
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

# ---------------------------------------------------------------
# 13. Hardware foundation: a configured kernel, a usable external-module
#     tree, Intel microcode, and NVIDIA modules/userspace. These tests do
#     not claim hardware was detected; that requires the physical PC.
# ---------------------------------------------------------------
"$PKG" install -y bc elfutils zstd linux intel-microcode nvidia-drivers \
    >"$E2E/hardware.log" 2>&1 \
    || { tail -50 "$E2E/hardware.log" >&2; fail "hardware package install"; }
KREL=6.19.14-yakimo
[ -s "$ROOT/boot/vmlinuz-$KREL" ] || fail "kernel image installed"
[ -s "$ROOT/boot/config-$KREL" ] || fail "kernel config installed"
[ -s "$ROOT/usr/lib/modules/$KREL/build/Module.symvers" ] \
    || fail "kernel symbol versions installed"
[ -s "$ROOT/usr/lib/modules/$KREL/build/Makefile" ] \
    || fail "external-module build tree installed"
case $(readlink "$ROOT/usr/lib/modules/$KREL/build") in
    /*) fail "kernel build link is absolute" ;;
esac
find -L "$ROOT/usr/lib/firmware/intel-ucode" -type f | grep -q . \
    || fail "Intel microcode files installed"
pass "kernel + Intel microcode: image, config, module tree, and firmware installed"

$LOADER "$ROOT/usr/sbin/modinfo" -b "$ROOT/usr" -k "$KREL" nvidia \
    | grep -q "vermagic:.*$KREL" || fail "NVIDIA module matches packaged kernel"
for module in nvidia nvidia-modeset nvidia-drm nvidia-uvm; do
    [ -s "$ROOT/usr/lib/modules/$KREL/extra/$module.ko" ] \
        || fail "$module installed"
done
[ -L "$ROOT/var/lib/dkms/nvidia/595.112.07/source" ] \
    || fail "NVIDIA source registered with DKMS"
[ -s "$ROOT/var/log/pkg/dkms-nvidia-drivers-$KREL.log" ] \
    || fail "DKMS transaction log installed"
[ ! -e "$ROOT/.pkg-recipe-manifest.toml" ] \
    || fail "store recipe sidecar leaked into the FHS root"
[ -s "$ROOT/usr/src/nvidia-595.112.07/dkms.conf" ] \
    || fail "NVIDIA DKMS source configuration installed"
[ -s "$ROOT/usr/lib/firmware/nvidia/595.112.07/gsp_ga10x.bin" ] \
    || fail "NVIDIA Ada GSP firmware installed"
[ -s "$ROOT/usr/share/vulkan/icd.d/nvidia_icd.json" ] \
    || fail "NVIDIA Vulkan ICD manifest installed"
[ -s "$ROOT/usr/share/egl/egl_vendor.d/10_nvidia.json" ] \
    || fail "NVIDIA EGL vendor manifest installed"
$LOADER "$R/nvidia-smi" --help | grep -q "v595\.112\.07" \
    || fail "nvidia-smi runs without querying a GPU"
[ -s "$ROOT/usr/lib/xorg/modules/drivers/nvidia_drv.so" ] \
    || fail "NVIDIA Xorg driver installed"
[ -L "$ROOT/usr/lib/xorg/modules/extensions/libglxserver_nvidia.so" ] \
    || fail "NVIDIA GLX server extension installed"
pass "NVIDIA: four kernel modules, GSP firmware, Vulkan/EGL/Xorg, and nvidia-smi"

"$PKG" db check >/dev/null 2>&1 || fail "db check after hardware packages"
pass "db check clean after hardware packages"

# ---------------------------------------------------------------
# 14. Full X11 desktop: Xorg, GTK3, the complete XFCE session and its
#     authorization/storage services. Real Xvfb sessions exercise the
#     clients; physical GPU, power policy, and mounting remain live tests.
# ---------------------------------------------------------------
"$PKG" install -y xinit xf86-input-libinput xterm gtk3 xfce4-meta \
    >"$E2E/desktop.log" 2>&1 \
    || { tail -80 "$E2E/desktop.log" >&2; fail "desktop package install"; }
[ -x "$ROOT/usr/bin/Xvfb" ] || fail "Xvfb installed"
[ -s "$ROOT/usr/lib/xorg/modules/input/libinput_drv.so" ] \
    || fail "Xorg libinput driver installed"
[ -s "$ROOT/usr/include/xorg/libinput-properties.h" ] \
    || fail "libinput SDK header installed in /usr, not /pkg/deps"
[ -s "$ROOT/usr/lib/dri/swrast_dri.so" ] || fail "Mesa software DRI driver"
[ -e "$RLIBS/libgtk-3.so.0" ] || fail "GTK3 library activated"
[ -s "$ROOT/usr/share/applications/mimeinfo.cache" ] \
    || fail "desktop database generated by the transaction hook"
[ -s "$ROOT/usr/share/glib-2.0/schemas/gschemas.compiled" ] \
    || fail "GLib schema cache generated by the transaction hook"
for tool in startxfce4 xfwm4 xfce4-panel xfdesktop xfsettingsd \
            xfce4-appfinder thunar xfconf-query iceauth xrdb notify-send \
            xfce4-terminal xfce4-taskmanager xfce4-screensaver \
            xfce4-power-manager udisksctl upower pkaction gio; do
    [ -x "$R/$tool" ] || fail "desktop tool missing: $tool"
done
[ -x "$ROOT/usr/lib/xfce4/notifyd/xfce4-notifyd" ] \
    || fail "XFCE notification service binary missing"
for lib in libxfconf-0.so libxfce4ui-2.so libxfce4windowing-0.so \
           libexo-2.so libgarcon-1.so libwnck-3.so libdisplay-info.so \
           libvte-2.91.so; do
    [ -e "$RLIBS/$lib" ] || fail "desktop library missing: $lib"
done
pass "full XFCE desktop, terminal, notification, power, and storage packages installed"

DESKTOP_LOG=$E2E/desktop.log
if ! BWRAP="$REAL_BWRAP" sh e2e/desktop.sh "$ROOT" >"$DESKTOP_LOG" 2>&1; then
    cat "$DESKTOP_LOG" >&2
    fail "X11/XFCE headless session"
fi
grep -q "xterm-ok" "$DESKTOP_LOG" || fail "xterm under startx/Xvfb"
grep -q "GTK window created and event loop completed" "$DESKTOP_LOG" \
    || fail "GTK window under startx/Xvfb"
grep -q "XFCE session works" "$DESKTOP_LOG" || fail "XFCE session processes"
grep -q "xfce-terminal-ok" "$DESKTOP_LOG" || fail "XFCE terminal command"
grep -q "xfce-notification-ok" "$DESKTOP_LOG" || fail "XFCE notification service"
grep -q "xfce-thumbnail-ok" "$DESKTOP_LOG" || fail "Tumbler thumbnail generation"
grep -q "desktop-services-ok" "$DESKTOP_LOG" || fail "desktop service clients"
if grep -q "bad display name" "$DESKTOP_LOG"; then
    cat "$DESKTOP_LOG" >&2
    fail "xauth display setup"
fi
pass "startx + Xvfb: GTK clients and the full XFCE session run"

"$PKG" db check >/dev/null 2>&1 || fail "db check after desktop packages"
pass "db check clean after desktop packages"
echo
echo "PORTS E2E PASSED"
