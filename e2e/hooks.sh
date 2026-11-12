#!/bin/sh
# Typed hooks through real local builds and a target-root bwrap namespace.
set -eu

if [ -z "${E2E_INNER:-}" ]; then
    exec nix shell \
        nixpkgs#gcc nixpkgs#cargo nixpkgs#rustc nixpkgs#bubblewrap \
        nixpkgs#coreutils nixpkgs#gnugrep nixpkgs#diffutils nixpkgs#sqlite nixpkgs#pkgsStatic.busybox \
        -c env E2E_INNER=1 sh "$0" "$@"
fi

WS=$(cd "$(dirname "$0")/.." && pwd)
cd "$WS"
E2E=/tmp/pkg-e2e-hooks
ROOT=$E2E/root
RECIPES=$E2E/recipes
export PKG_ROOT=$ROOT
fail() { echo "FAIL: $1" >&2; exit 1; }
pass() { echo "PASS: $1"; }
count() { wc -l < "$1" | tr -d ' '; }

cargo build --offline -q -p pkg-cli
PKG=$WS/target/debug/pkg-cli
rm -rf "$E2E"
mkdir -p "$RECIPES/a" "$RECIPES/b" "$ROOT/etc" \
    "$ROOT/usr/bin" "$ROOT/sbin" "$ROOT/usr/share/icons/hicolor" \
    "$ROOT/var/cache/fontconfig" "$ROOT/usr/share/mime"

mkdir -p "$ROOT/bin"
cp "$(command -v busybox)" "$ROOT/bin/busybox"
cat > "$ROOT/usr/bin/fc-cache" <<'EOF'
#!/bin/busybox sh
echo fc-cache >> /var/cache/fontconfig/invocations
echo 'stdout from fc-cache'
echo 'stderr from fc-cache' >&2
if [ -e /etc/fail-font-cache ]; then
    echo 'requested font failure' >&2
    exit 2
fi
EOF
cat > "$ROOT/usr/bin/update-mime-database" <<'EOF'
#!/bin/busybox sh
echo update-mime-database >> /usr/share/mime/invocations
echo 'stdout from update-mime-database'
echo 'stderr from update-mime-database' >&2
EOF
cat > "$ROOT/usr/bin/gtk-update-icon-cache" <<'EOF'
#!/bin/busybox sh
for last do :; done
echo updated >> "$last/icon.cache"
echo 'icon cache updated'
EOF
cat > "$ROOT/sbin/ldconfig" <<'EOF'
#!/bin/busybox sh
echo ldconfig >> /etc/invocations
echo 'stdout from ldconfig'
echo 'stderr from ldconfig' >&2
EOF
chmod +x "$ROOT/usr/bin/fc-cache" "$ROOT/usr/bin/update-mime-database" \
    "$ROOT/usr/bin/gtk-update-icon-cache" "$ROOT/sbin/ldconfig"
printf '[Icon Theme]\nName=Test\nDirectories=.\n' > "$ROOT/usr/share/icons/hicolor/index.theme"

for package in a b; do
    cat > "$RECIPES/$package/pkg-meta.toml" <<EOF
name = "$package"
version = "1"
category = "app-misc"

[hooks]
operations = ["icon-cache", "mime-cache", "font-cache", "ldconfig"]

[scripts]
build = ":"
install = '''
mkdir -p "\$PKGDIR/usr/share/$package"
printf "$package\\n" > "\$PKGDIR/usr/share/$package/payload"
'''
EOF
done

TOOLBIN=$E2E/toolbin
mkdir -p "$TOOLBIN" "$E2E/shim"
REAL_BWRAP=$(command -v bwrap)
for tool in sh mkdir printf; do
    p=$(command -v "$tool") && ln -sf "$(readlink -f "$p")" "$TOOLBIN/$tool"
done
cat > "$E2E/shim/bwrap" <<EOF
#!/bin/sh
target_root=0
filtered() {
    while [ "\$#" -gt 0 ]; do
        case "\$1" in
            --ro-bind|--bind)
                [ "\$2" = "$ROOT" ] && [ "\$3" = / ] && target_root=1
                if [ -e "\$2" ]; then
                    printf '%s\0%s\0%s\0' "\$1" "\$2" "\$3"
                fi
                shift 3 ;;
            --)
                if [ "\$target_root" = 0 ] && [ -d /nix/store ]; then
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

# One failed operation must not prevent the others or undo activation.
: > "$ROOT/etc/fail-font-cache"
if "$PKG" install -y "$RECIPES/a" "$RECIPES/b" >"$E2E/install.log" 2>&1; then
    fail "install with failed hook returned success"
fi
"$PKG" info a >/dev/null || fail "package a not committed"
"$PKG" info b >/dev/null || fail "package b not committed"
[ "$(count "$ROOT/var/cache/fontconfig/invocations")" = 1 ] || fail "font hook not coalesced"
[ "$(count "$ROOT/usr/share/mime/invocations")" = 1 ] || fail "mime hook did not continue"
[ "$(count "$ROOT/etc/invocations")" = 1 ] || fail "ldconfig did not run first"
[ "$(count "$ROOT/usr/share/icons/hicolor/icon.cache")" = 1 ] || fail "icon hook did not run"
"$PKG" hooks status >"$E2E/status.out"
grep -q 'font-cache' "$E2E/status.out" || fail "failed font hook missing from status"
grep -q '/var/log/pkg/hook-font-cache.log' "$E2E/status.out" || fail "root-relative log path"
grep -q 'requested font failure' "$ROOT/var/log/pkg/hook-font-cache.log" || fail "stderr log"
grep -q 'stdout from fc-cache' "$ROOT/var/log/pkg/hook-font-cache.log" || fail "stdout log"
"$PKG" db check >/dev/null || fail "database after hook failure"
pass "coalesced target-root hooks survive failure with logs and valid package state"

"$PKG" history > "$E2E/history.before"
if "$PKG" hooks retry >/dev/null 2>&1; then fail "failing retry returned success"; fi
"$PKG" hooks status > "$E2E/status.retry"
grep -q '2 attempt(s)' "$E2E/status.retry" || fail "retry did not increment attempts"
rm "$ROOT/etc/fail-font-cache"
"$PKG" hooks retry >"$E2E/retry.log" 2>&1 || { cat "$E2E/retry.log" >&2; fail "successful retry"; }
[ "$("$PKG" hooks status)" = "no failed hooks" ] || fail "successful retry did not clear status"
"$PKG" history > "$E2E/history.after"
diff -u "$E2E/history.before" "$E2E/history.after" >/dev/null || fail "retry created a generation"
[ "$(count "$ROOT/var/cache/fontconfig/invocations")" = 3 ] || fail "font failure and retry counts"
pass "failed and successful retry persist correctly without a new generation"

"$PKG" remove -y a >/dev/null
[ "$(count "$ROOT/var/cache/fontconfig/invocations")" = 4 ] || fail "remove did not rerun hooks"
sqlite3 "$ROOT/var/lib/pkg/db/pkg.sqlite" 'UPDATE generations SET schema_ver = 3;'
"$PKG" undo -y >/dev/null
[ "$(count "$ROOT/var/cache/fontconfig/invocations")" = 5 ] || fail "undo did not rerun hooks"
pass "remove and schema-v3-compatible undo reconcile cache state"

"$PKG" build --no-cache "$RECIPES/a" >/dev/null
[ "$(count "$ROOT/var/cache/fontconfig/invocations")" = 6 ] || fail "same-identity rebuild did not rerun hooks"
pass "same-identity rebuild reruns hooks"

[ ! -e /etc/fail-font-cache ] || fail "test touched host /etc"
echo "HOOKS E2E PASSED"
