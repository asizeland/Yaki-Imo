#!/bin/sh
# Real local-recipe debug session: sandbox, phases, resume, export, removal.
set -eu

if [ -z "${E2E_INNER:-}" ]; then
    exec nix shell \
        nixpkgs#gcc nixpkgs#cargo nixpkgs#rustc nixpkgs#bubblewrap \
        nixpkgs#coreutils nixpkgs#gnused nixpkgs#diffutils \
        -c env E2E_INNER=1 sh "$0" "$@"
fi

WS=$(cd "$(dirname "$0")/.." && pwd)
cd "$WS"
E2E=/tmp/pkg-e2e-debug
ROOT=$E2E/root
RECIPE=$E2E/recipe
SRC=$E2E/src
TOOLBIN=$E2E/toolbin
export PKG_ROOT=$ROOT

fail() { echo "FAIL: $1" >&2; exit 1; }
pass() { echo "PASS: $1"; }

cargo build --offline -q -p pkg-cli
PKG=$WS/target/debug/pkg-cli
REAL_BWRAP=$(command -v bwrap)
rm -rf "$E2E"
mkdir -p "$ROOT" "$RECIPE" "$SRC" "$TOOLBIN" "$E2E/shim"
printf 'original source\n' > "$SRC/hello.txt"
SHA=$(sha256sum "$SRC/hello.txt" | cut -d' ' -f1)
cat > "$RECIPE/pkg-meta.toml" <<EOF
name = "debug-me"
version = "1"
category = "app-misc"

[[source]]
url = "file://$SRC/hello.txt"
sha256 = "$SHA"

[scripts]
build = '''
cp hello.txt built.txt
'''
install = '''
mkdir -p "\$PKGDIR/usr/share/debug-me"
cp built.txt "\$PKGDIR/usr/share/debug-me/result.txt"
'''
EOF

for tool in sh cp mkdir cat sed grep diff test false printf pwd basename dirname; do
    p=$(command -v "$tool" 2>/dev/null) && ln -sf "$(readlink -f "$p")" "$TOOLBIN/$tool"
done
cat > "$E2E/shim/bwrap" <<EOF
#!/bin/sh
exec 3<&0
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
filtered "\$@" | xargs -0 sh -c 'exec "\$0" "\$@" 0<&3' "$REAL_BWRAP"
EOF
chmod +x "$E2E/shim/bwrap"
export PATH="$E2E/shim:$PATH"

printf 'test ! -e built.txt\nprintf "changed source\\n" > hello.txt\nexit\n' | \
    "$PKG" debug-shell "$RECIPE" >"$E2E/first.log" 2>&1 || {
        cat "$E2E/first.log" >&2; fail "initial debug shell"; }
WORK=$ROOT/var/lib/pkg/build/debug/debug-me
[ "$(cat "$WORK/scratch/hello.txt")" = "changed source" ] || fail "source edit persisted"
[ -d "$WORK/baseline/source" ] || fail "pristine source baseline"
[ ! -e "$WORK/scratch/built.txt" ] || fail "default session ran build phase"
pass "real sandbox enters before prepare and retains edits"

printf 'exit\n' | "$PKG" debug-shell --resume --phase compile "$RECIPE" \
    >"$E2E/compile.log" 2>&1 || { cat "$E2E/compile.log" >&2; fail "resume before compile"; }
[ ! -e "$WORK/scratch/built.txt" ] || fail "--phase compile ran the build phase"
printf 'exit\n' | "$PKG" debug-shell --resume --phase check "$RECIPE" \
    >"$E2E/check.log" 2>&1 || { cat "$E2E/check.log" >&2; fail "resume before check"; }
grep -q 'changed source' "$WORK/scratch/built.txt" || fail "build prerequisite did not run"
[ -f "$WORK/logs/build.log" ] || fail "per-phase debug log"
[ -f "$WORK/logs/prepare.log" ] || fail "no-op phase debug log"
pass "phase entry, retained edits, and per-phase logs"

printf 'check = """\ntest -f built.txt\n"""\n' >> "$WORK/scratch/.pkg-debug-recipe/pkg-meta.toml"
sed -i 's/test -f built.txt/false/' "$WORK/scratch/.pkg-debug-recipe/pkg-meta.toml"
if printf 'exit\n' | "$PKG" debug-shell --resume --phase install "$RECIPE" \
    >"$E2E/failure.log" 2>&1; then
    fail "failing check phase returned success"
fi
grep -q 'check.*phase failed' "$WORK/logs/check.log" || fail "failed phase log"
[ -f "$WORK/scratch/built.txt" ] || fail "failed phase destroyed retained tree"
sed -i 's/^false$/test -f built.txt/' "$WORK/scratch/.pkg-debug-recipe/pkg-meta.toml"
printf 'exit\n' | "$PKG" debug-shell --resume --phase install "$RECIPE" \
    >"$E2E/install-phase.log" 2>&1 || { cat "$E2E/install-phase.log" >&2; fail "resume after phase failure"; }
pass "failed phase remains resumable after an editable-recipe fix"

printf 'exit\n' | "$PKG" debug-shell --resume --export "$E2E/export" "$RECIPE" \
    >"$E2E/export.log" 2>&1 || { cat "$E2E/export.log" >&2; fail "patch export"; }
grep -q '^--- a/hello.txt' "$E2E/export/debug-me-source.patch" || fail "source patch old path"
grep -q '^+++ b/hello.txt' "$E2E/export/debug-me-source.patch" || fail "source patch new path"
grep -q '^+changed source' "$E2E/export/debug-me-source.patch" || fail "source patch content"
grep -q '^+check =' "$E2E/export/debug-me-recipe.patch" || fail "recipe patch content"
pass "source and recipe unified diffs exported"

"$PKG" history >"$E2E/history.out"
[ ! -s "$E2E/history.out" ] || fail "debug shell created a generation"
[ ! -d "$ROOT/usr/lib/pkg/.store" ] || fail "debug shell staged a package"
pass "debug session creates no package, generation, or store artifact"

printf '# changed original input\n' >> "$RECIPE/pkg-meta.toml"
if printf 'exit\n' | "$PKG" debug-shell --resume "$RECIPE" >/dev/null 2>&1; then
    fail "resume accepted changed original recipe inputs"
fi
pass "resume rejects changed original recipe inputs"

"$PKG" debug-shell --remove-tree debug-me >/dev/null
[ ! -e "$WORK" ] || fail "explicit workspace removal"
if "$PKG" debug-shell --resume "$RECIPE" >/dev/null 2>&1; then
    fail "resume after removal unexpectedly succeeded"
fi
pass "explicit removal and missing-resume error"

echo "DEBUG SHELL E2E PASSED"
