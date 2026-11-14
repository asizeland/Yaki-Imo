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
    "$ROOT/var/cache/fontconfig" "$ROOT/usr/share/mime" \
    "$ROOT/usr/share/applications" "$ROOT/usr/share/glib-2.0/schemas"

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
cat > "$E2E/update-desktop-database" <<'EOF'
#!/bin/busybox sh
[ "$#" -eq 1 ] && [ "$1" = /usr/share/applications ] || exit 64
echo update-desktop-database >> /usr/share/applications/invocations
EOF
cat > "$ROOT/usr/bin/glib-compile-schemas" <<'EOF'
#!/bin/busybox sh
[ "$#" -eq 1 ] && [ "$1" = /usr/share/glib-2.0/schemas ] || exit 64
echo glib-compile-schemas >> /usr/share/glib-2.0/schemas/invocations
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
    "$ROOT/usr/bin/glib-compile-schemas" "$ROOT/usr/bin/gtk-update-icon-cache" \
    "$ROOT/sbin/ldconfig" "$E2E/update-desktop-database"
printf '[Icon Theme]\nName=Test\nDirectories=.\n' > "$ROOT/usr/share/icons/hicolor/index.theme"

for package in a b; do
    cat > "$RECIPES/$package/pkg-meta.toml" <<EOF
name = "$package"
version = "1"
category = "app-misc"

[hooks]
operations = ["glib-schemas", "icon-cache", "desktop-database", "mime-cache", "font-cache", "ldconfig"]

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

# Failed operations must not prevent the others or undo activation.
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
[ ! -e "$ROOT/usr/share/applications/invocations" ] || fail "missing desktop tool ran"
[ "$(count "$ROOT/usr/share/glib-2.0/schemas/invocations")" = 1 ] || fail "schema hook did not continue"
"$PKG" hooks status >"$E2E/status.out"
grep -q 'font-cache' "$E2E/status.out" || fail "failed font hook missing from status"
grep -q 'desktop-database' "$E2E/status.out" || fail "missing desktop tool absent from status"
grep -q '/var/log/pkg/hook-font-cache.log' "$E2E/status.out" || fail "root-relative log path"
grep -q 'requested font failure' "$ROOT/var/log/pkg/hook-font-cache.log" || fail "stderr log"
grep -q 'stdout from fc-cache' "$ROOT/var/log/pkg/hook-font-cache.log" || fail "stdout log"
grep -q 'required executable /usr/bin/update-desktop-database is not installed' \
    "$ROOT/var/log/pkg/hook-desktop-database.log" || fail "missing desktop tool not logged"
"$PKG" db check >/dev/null || fail "database after hook failure"
pass "coalesced target-root hooks survive failures with logs and valid package state"

"$PKG" history > "$E2E/history.before"
if "$PKG" hooks retry >/dev/null 2>&1; then fail "failing retry returned success"; fi
"$PKG" hooks status > "$E2E/status.retry"
grep -q '2 attempt(s)' "$E2E/status.retry" || fail "retry did not increment attempts"
rm "$ROOT/etc/fail-font-cache"
cp "$E2E/update-desktop-database" "$ROOT/usr/bin/update-desktop-database"
"$PKG" hooks retry >"$E2E/retry.log" 2>&1 || { cat "$E2E/retry.log" >&2; fail "successful retry"; }
[ "$("$PKG" hooks status)" = "no failed hooks" ] || fail "successful retry did not clear status"
"$PKG" history > "$E2E/history.after"
diff -u "$E2E/history.before" "$E2E/history.after" >/dev/null || fail "retry created a generation"
[ "$(count "$ROOT/var/cache/fontconfig/invocations")" = 3 ] || fail "font failure and retry counts"
[ "$(count "$ROOT/usr/share/applications/invocations")" = 1 ] || fail "desktop retry did not recover"
[ "$(count "$ROOT/usr/share/glib-2.0/schemas/invocations")" = 1 ] || fail "retry reran successful schema hook"
pass "failed and successful retry persist correctly without a new generation"

"$PKG" remove -y a >/dev/null
[ "$(count "$ROOT/var/cache/fontconfig/invocations")" = 4 ] || fail "remove did not rerun hooks"
[ "$(count "$ROOT/usr/share/applications/invocations")" = 2 ] || fail "remove did not rerun desktop hook"
[ "$(count "$ROOT/usr/share/glib-2.0/schemas/invocations")" = 2 ] || fail "remove did not rerun schema hook"
sqlite3 "$ROOT/var/lib/pkg/db/pkg.sqlite" 'UPDATE generations SET schema_ver = 3;'
"$PKG" undo -y >/dev/null
[ "$(count "$ROOT/var/cache/fontconfig/invocations")" = 5 ] || fail "undo did not rerun hooks"
[ "$(count "$ROOT/usr/share/applications/invocations")" = 3 ] || fail "undo did not rerun desktop hook"
[ "$(count "$ROOT/usr/share/glib-2.0/schemas/invocations")" = 3 ] || fail "undo did not rerun schema hook"
pass "remove and schema-v3-compatible undo reconcile cache state"

"$PKG" build --no-cache "$RECIPES/a" >/dev/null
[ "$(count "$ROOT/var/cache/fontconfig/invocations")" = 6 ] || fail "same-identity rebuild did not rerun hooks"
[ "$(count "$ROOT/usr/share/applications/invocations")" = 4 ] || fail "rebuild did not rerun desktop hook"
[ "$(count "$ROOT/usr/share/glib-2.0/schemas/invocations")" = 4 ] || fail "rebuild did not rerun schema hook"
pass "same-identity rebuild reruns hooks"

mkdir -p "$RECIPES/c"
cat > "$RECIPES/c/pkg-meta.toml" <<'EOF'
name = "c"
version = "1"
category = "app-misc"

[hooks]
operations = ["icon-cache"]

[scripts]
build = ":"
install = ":"
EOF
rm "$ROOT/usr/share/icons/hicolor/index.theme" "$ROOT/usr/bin/gtk-update-icon-cache"
icon_cache_before=$(count "$ROOT/usr/share/icons/hicolor/icon.cache")
"$PKG" build --no-cache "$RECIPES/c" >"$E2E/noop.log" 2>&1 || {
    cat "$E2E/noop.log" >&2
    fail "icon hook without themes was not a no-op"
}
[ "$(count "$ROOT/usr/share/icons/hicolor/icon.cache")" = "$icon_cache_before" ] || fail "no-op icon hook ran"
grep -q 'no applicable input directories; nothing to do' \
    "$ROOT/var/log/pkg/hook-icon-cache.log" || fail "icon no-op not logged"
[ "$("$PKG" hooks status)" = "no failed hooks" ] || fail "icon no-op recorded a failure"
pass "icon hook without themes is a no-op even without its tool"

[ ! -e /etc/fail-font-cache ] || fail "test touched host /etc"
echo "HOOKS E2E PASSED"
