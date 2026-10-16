# Installed by live/default.nix as `yakimo-live` (a shell shebang is added).
set -eu

REPO=/etc/yakimo/repo
BASE=http://127.0.0.1:8784
WORK=${YAKIMO_WORK:-/tmp/yakimo}
ROOT=$WORK/root
TOOLBIN=$WORK/toolbin
SEED=$WORK/seed

usage() {
    cat <<EOF
usage: yakimo-live <setup|shell|smoke>

Environment:
  YAKIMO_WORK   writable work directory (default: labelled scratch disk,
                or /tmp/yakimo when no scratch disk is attached)

The package manager always operates on \$YAKIMO_WORK/root through PKG_ROOT.
EOF
}

select_scratch() {
    [ -z "${YAKIMO_WORK:-}" ] || return
    [ -b /dev/disk/by-label/YAKIMO_SCRATCH ] || return
    MNT=/mnt/yakimo-scratch
    if ! mountpoint -q "$MNT"; then
        sudo mkdir -p "$MNT"
        sudo mount /dev/disk/by-label/YAKIMO_SCRATCH "$MNT"
    fi
    sudo chown "$(id -u):$(id -g)" "$MNT"
    WORK=$MNT/work
    ROOT=$WORK/root
    TOOLBIN=$WORK/toolbin
    SEED=$WORK/seed
    echo "Using labelled scratch disk through $WORK"
}

activate() {
    export PKG_ROOT=$ROOT
    export PATH=$WORK/shim:$PATH
}

ensure_server() {
    if curl -fsS --max-time 2 "$BASE/index.json" >/dev/null 2>&1; then
        return
    fi
    mkdir -p "$WORK"
    python3 -m http.server 8784 --bind 127.0.0.1 --directory "$REPO" \
        >"$WORK/http.log" 2>&1 &
    echo $! >"$WORK/http.pid"
    sleep 1
    curl -fsS --max-time 5 "$BASE/index.json" >/dev/null
}

setup() {
    select_scratch
    mkdir -p "$ROOT/etc/pkg" "$ROOT/var/lib/pkg/recipes" \
        "$TOOLBIN" "$WORK/shim" "$SEED/lib64"

    if [ ! -f "$WORK/.recipes-installed" ]; then
        cp -R "$REPO"/recipes/* "$ROOT/var/lib/pkg/recipes/"
        : >"$WORK/.recipes-installed"
    fi
    printf 'mirrors = [\n  "%s",\n]\n' "$BASE" >"$ROOT/etc/pkg/mirrors.toml"
    jobs=$(nproc)
    [ "$jobs" -le 4 ] || jobs=4
    printf '[build]\njobs = %s\nmax_pkg_jobs = %s\n' "$jobs" "$jobs" \
        >"$ROOT/etc/pkg/config.toml"

    # NixOS has no /lib64 loader or FHS toolset. This is the same seed and
    # toolchain adaptation proven in e2e/ports.sh, with paths made local to
    # the disposable work directory.
    LD_PATH=$(ldd /bin/sh | awk '/ld-linux/{print $1; exit}')
    [ -n "$LD_PATH" ] || { echo "could not locate the host dynamic loader" >&2; exit 1; }
    cp "$(readlink -f "$LD_PATH")" "$SEED/lib64/ld-linux-x86-64.so.2"

    for tool in env sh bash ar ranlib make tar gzip xz sed grep \
                awk gawk cut diff cmp find xargs uname install cp mv mkdir ls pwd \
                ln rm cat chmod touch basename dirname sort uniq wc tr head \
                tail ld as nm readelf objcopy strip sleep date expr printf \
                test comm m4 od id bison python3 echo true false readlink \
                msgfmt objdump perl rmdir mktemp flex gperf bzip2; do
        p=$(command -v "$tool" 2>/dev/null || true)
        case "$p" in
            /*) ;;
            *) p=$(type -P "$tool" 2>/dev/null || true) ;;
        esac
        [ -n "$p" ] && ln -sf "$(readlink -f "$p")" "$TOOLBIN/$tool" || true
    done
    ln -sf "$(readlink -f "$(command -v dash)")" "$TOOLBIN/sh"

    for cc_tool in cc gcc g++ c++; do
        real=$(readlink -f "$(command -v "$cc_tool")")
        cat >"$TOOLBIN/$cc_tool" <<EOF
#!/bin/sh
case " \$* " in
    *" -c "*|*" -E "*|*" -S "*|*" -shared "*|*" -r "*|*" -print-"*) exec $real "\$@" ;;
esac
exec $real "\$@" -Wl,--dynamic-linker=/lib64/ld-linux-x86-64.so.2
EOF
        chmod +x "$TOOLBIN/$cc_tool"
    done

    REAL_BWRAP=$(command -v bwrap)
    cat >"$WORK/shim/bwrap" <<EOF
#!/bin/sh
filtered() {
    while [ "\$#" -gt 0 ]; do
        case "\$1" in
            --ro-bind|--bind)
                if [ -e "\$2" ]; then
                    printf '%s\\0%s\\0%s\\0' "\$1" "\$2" "\$3"
                else
                    echo "bwrap-shim: skipping missing bind source \$2" >&2
                fi
                shift 3 ;;
            --)
                printf '%s\\0' --ro-bind /nix/store /nix/store
                printf '%s\\0' --ro-bind "$TOOLBIN" /usr/bin
                printf '%s\\0' --ro-bind "$TOOLBIN" /bin
                printf '%s\\0%s\\0%s\\0' --ro-bind "$SEED/lib64" /lib64
                printf '%s\\0' "\$@"
                break ;;
            *) printf '%s\\0' "\$1"; shift ;;
        esac
    done
}
filtered "\$@" | xargs -0 sh -c 'exec "\$0" "\$@"' "$REAL_BWRAP"
EOF
    chmod +x "$WORK/shim/bwrap"

    activate
    ensure_server
    echo yes | pkg key trust-root "$REPO/root.json" >/dev/null
    pkg sync
    : >"$WORK/.ready"
    echo "Yaki Imo test root ready: $ROOT"
}

smoke() {
    setup
    echo "== building and installing Lua, less, and SQLite =="
    pkg install -y lua less sqlite

    R=$ROOT/usr/bin
    RLIBS=$ROOT/usr/lib
    LOADER="$ROOT/lib64/ld-linux-x86-64.so.2 --library-path $RLIBS"
    [ -x "$ROOT/lib64/ld-linux-x86-64.so.2" ]
    [ "$(echo "$($ROOT/lib64/ld-linux-x86-64.so.2 --version | head -1)" | awk '{print $NF}')" = "2.42." ]

    [ "$($LOADER "$R/lua" -e 'print(2+2)')" = "4" ]
    $LOADER "$R/less" --version >/dev/null 2>&1
    [ "$($LOADER "$R/sqlite3" :memory: 'select sqlite_version();')" = "3.45.3" ]
    $LOADER "$R/sqlite3" :memory: 'create virtual table t using fts5(x);'
    echo "PASS: packaged glibc, Lua, less, and SQLite run"

    echo "== checking option-driven Readline dependency =="
    pkg install -y 'lua[opt:readline=1]'
    [ -e "$RLIBS/libreadline.so.8" ]
    LUABIN=$(readlink -f "$R/lua")
    readelf -d "$LUABIN" | grep -q 'NEEDED.*libreadline'
    echo "PASS: lua[readline] rebuilt and linked against Readline"

    echo "== checking non-ABI SQLite option identity =="
    pkg install -y sqlite --opt fts5=0
    if $LOADER "$R/sqlite3" :memory: \
        'create virtual table t using fts5(x);' 2>/dev/null; then
        echo "SQLite FTS5 remained enabled after --opt fts5=0" >&2
        exit 1
    fi
    echo "PASS: sqlite --opt fts5=0 rebuilt without FTS5"

    pkg db check
    pkg history
    echo "YAKIMO_SMOKE_OK"
}

COMMAND=${1:-}
[ "$#" -eq 0 ] || shift
[ "$#" -eq 0 ] || { usage >&2; exit 2; }
case "$COMMAND" in
    setup)
        setup
        ;;
    shell)
        setup
        echo "PKG_ROOT=$PKG_ROOT; type 'exit' to return to the live host"
        exec bash
        ;;
    smoke)
        smoke
        ;;
    *)
        usage >&2
        exit 2
        ;;
esac
