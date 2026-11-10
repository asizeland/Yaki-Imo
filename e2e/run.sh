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
        nixpkgs#gcc nixpkgs#cargo nixpkgs#rustc nixpkgs#perl nixpkgs#bubblewrap nixpkgs#python3 \
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
cat > "$SRC/libfoo-1.0/foo.h" <<'EOF'
#ifndef FOO_H
#define FOO_H
int foo_answer(void);
#endif
EOF
cat > "$SRC/hello-1.0/hello.c" <<'EOF'
#include <stdio.h>
#include <foo.h>
int main(void) { printf("hello from pkg e2e: %d\n", foo_answer()); return 0; }
EOF
tar -czf "$SRC/libfoo-1.0.tar.gz" -C "$SRC" libfoo-1.0
tar -czf "$SRC/hello-1.0.tar.gz" -C "$SRC" hello-1.0

# Two critical-set packages: crtpkg is properly attested (exercises the
# §10.5.6 double-build), badcrt carries no attestations (exercises the
# §10.5.6/§16.4 supply-chain gate).
for p in crtpkg badcrt; do
    mkdir -p "$SRC/$p-1"
    printf '#include <stdio.h>\nint main(void){puts("%s");return 0;}\n' "$p" > "$SRC/$p-1/main.c"
    tar -czf "$SRC/$p-1.tar.gz" -C "$SRC" "$p-1"
done

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
    "hello:1:e2e0001:false:$SRC/hello-1.0.tar.gz" \
    "crtpkg:1:e2e0001:true:$SRC/crtpkg-1.tar.gz" \
    "badcrt:1:e2e0001:true:$SRC/badcrt-1.tar.gz:noattest" >/dev/null
pass "mkrepo fabricated signed repo"

# ---------------------------------------------------------------
# 3. Recipes checked out under the test root.
# ---------------------------------------------------------------
for p in glibc libfoo hello crtpkg badcrt; do
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
mkdir -p "$PKGDIR/usr/lib" "$PKGDIR/usr/include"
cp libfoo-1.0/libfoo.so.1.0 "$PKGDIR/usr/lib/"
cp libfoo-1.0/foo.h "$PKGDIR/usr/include/"
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
# Deps are RO-bound into the sandbox at the stable alias
# /pkg/deps/<name> (no .store/<identity> globbing needed).
cc -O2 -o hello hello.c -I /pkg/deps/libfoo/usr/include \
   -L /pkg/deps/libfoo/usr/lib -lfoo -Wl,-rpath,/usr/lib
EOF
cat > "$ROOT/var/lib/pkg/recipes/hello/install" <<'EOF'
mkdir -p "$PKGDIR/usr/bin"
cp hello-1.0/hello "$PKGDIR/usr/bin/"
EOF

# Critical-set recipes: [build] critical triggers the double-build; the
# index/manifest critical flag (mkrepo) drives the supply-chain audit.
for p in crtpkg badcrt; do
    cat > "$ROOT/var/lib/pkg/recipes/$p/pkg-meta.toml" <<EOF
name = "$p"
version = "1"
category = "base"

[dependencies]
runtime = ["glibc"]

[build]
critical = true
EOF
    cat > "$ROOT/var/lib/pkg/recipes/$p/build" <<EOF
tar xzf $p-1.tar.gz
cd $p-1
cc -O2 -o $p main.c
EOF
    cat > "$ROOT/var/lib/pkg/recipes/$p/install" <<EOF
mkdir -p "\$PKGDIR/usr/bin"
cp $p-1/$p "\$PKGDIR/usr/bin/"
EOF
done

# ---------------------------------------------------------------
# 4. Config: mirrors pointing at the local server.
# ---------------------------------------------------------------
mkdir -p "$ROOT/etc/pkg"
printf 'mirrors = [\n  "%s",\n]\n' "$BASE" > "$ROOT/etc/pkg/mirrors.toml"
# Relocate the artifact cache via [paths] — exercises the config-file
# override for every build below.
printf '[paths]\n"/var/lib/pkg/cache" = "/e2e-cache"\n' > "$ROOT/etc/pkg/config.toml"

# ---------------------------------------------------------------
# 5. Toolchain symlink farm + bwrap shim (host-quirk adaptation).
# ---------------------------------------------------------------
for tool in env sh bash cc gcc make tar gzip sed grep find xargs uname \
            install cp mv mkdir ln rm cat chmod touch basename dirname \
            sort uniq wc tr head tail ld as nm readelf objcopy strip \
            sleep date expr printf test; do
    p=$(command -v "$tool" 2>/dev/null) && ln -sf "$(readlink -f "$p")" "$TOOLBIN/$tool"
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
                if [ -d /nix/store ]; then
                    # NixOS: no /usr toolset — the store and the
                    # toolchain farm must resolve inside the sandbox.
                    printf '%s\0' --ro-bind /nix/store /nix/store
                    printf '%s\0' --ro-bind "$TOOLBIN" /usr/bin
                else
                    # FHS hosts: /usr /bin /lib are already bound by
                    # the profile. No farm — overlaying /usr/bin would
                    # shadow it with symlinks pointing back into the
                    # farm (ELOOP). Only /etc/alternatives is needed,
                    # so cc/awk-style alternative symlinks resolve.
                    [ -d /etc/alternatives ] && printf '%s\0' --ro-bind /etc/alternatives /etc/alternatives
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
ls "$ROOT"/e2e-cache/libfoo/*.pkg.tar.zst >/dev/null 2>&1 || fail "[paths] cache override"
pass "[paths] override relocated the artifact cache"
[ -e "$ROOT/usr/lib/libfoo.so.1.0" ] || fail "libfoo activated symlink"
pass "libfoo activated into live tree"

# Provenance honesty: libfoo ships a public header, so its link-interface
# row must carry a real header fingerprint, and provenance must record
# the repo index version the build resolved against.
hfp=$(sqlite3 "$ROOT/var/lib/pkg/db/pkg.sqlite" \
    "SELECT li.header_fingerprint FROM link_interface li JOIN packages p ON p.id=li.package_id WHERE p.name='libfoo'")
[ -n "$hfp" ] || fail "libfoo header_fingerprint should be recorded"
pass "libfoo header_fingerprint recorded ($hfp)"
idxv=$(sqlite3 "$ROOT/var/lib/pkg/db/pkg.sqlite" \
    "SELECT pv.index_version FROM provenance pv JOIN packages p ON p.id=pv.package_id WHERE p.name='libfoo'")
[ "$idxv" = "1" ] || { echo "index_version: $idxv" >&2; fail "provenance index_version"; }
pass "provenance records index_version = 1"

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
cat > "$SRC/libfoo-1.1/foo.h" <<'EOF'
#ifndef FOO_H
#define FOO_H
int foo_answer(void);
int foo_extra(void);
#endif
EOF
tar -czf "$SRC/libfoo-1.1.tar.gz" -C "$SRC" libfoo-1.1

sed -i 's/^version = "1"$/version = "1.1"/' "$ROOT/var/lib/pkg/recipes/libfoo/pkg-meta.toml"
sed -i 's/libfoo-1\.0/libfoo-1.1/g' "$ROOT/var/lib/pkg/recipes/libfoo/build" \
    "$ROOT/var/lib/pkg/recipes/libfoo/install"
echo "e2e0002" > "$ROOT/var/lib/pkg/recipes/libfoo/.recipe-commit"

"$MKREPO" "$REPO" "$BASE" \
    "glibc:$GLIBC_VER:e2e0001:false:" \
    "libfoo:1.1:e2e0002:false:$SRC/libfoo-1.1.tar.gz" \
    "hello:1:e2e0001:false:$SRC/hello-1.0.tar.gz" \
    "crtpkg:1:e2e0001:true:$SRC/crtpkg-1.tar.gz" \
    "badcrt:1:e2e0001:true:$SRC/badcrt-1.tar.gz:noattest" >/dev/null
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
# 12. Critical-set double build (§10.5.6): a properly attested
#     critical package builds twice, trees compared, self-match
#     recorded in provenance, quarantine stays empty.
# ---------------------------------------------------------------
"$PKG" install -y crtpkg >/dev/null 2>&1 || fail "pkg install crtpkg"
repro=$(sqlite3 "$ROOT/var/lib/pkg/db/pkg.sqlite" \
    "SELECT p2.reproducibility FROM provenance p2 JOIN packages p ON p.id=p2.package_id WHERE p.name='crtpkg'")
[ "$repro" = "self-match" ] || { echo "reproducibility: $repro" >&2; fail "crtpkg double-build self-match recorded"; }
pass "critical double-build recorded self-match"
[ -z "$(find "$ROOT/var/lib/pkg/build/quarantine" -mindepth 1 2>/dev/null)" ] \
    && pass "quarantine empty after matching double-build" || fail "quarantine should be empty"

# ---------------------------------------------------------------
# 13. Supply-chain gate (§10.5.6/§16.4): an under-attested critical
#     package installs (nothing to block it yet), but the NEXT
#     transaction is blocked until the finding is acknowledged.
# ---------------------------------------------------------------
"$PKG" install -y badcrt >/dev/null 2>&1 || fail "pkg install badcrt"
"$PKG" audit --supply-chain 2>&1 | grep -q "badcrt" \
    && pass "supply-chain audit flags badcrt" || fail "audit should flag badcrt"
if "$PKG" install -y hello >/dev/null 2>&1; then
    fail "gate allowed a transaction with an unacknowledged hard finding"
else
    "$PKG" install -y hello 2>&1 | grep -q "unacknowledged supply-chain hard findings" \
        && pass "gate blocks the next transaction" || fail "gate block message"
fi
"$PKG" audit --supply-chain --acknowledge badcrt >/dev/null
"$PKG" install -y hello >/dev/null 2>&1 \
    && pass "acknowledgement unblocks transactions" || fail "acknowledge badcrt"

# ---------------------------------------------------------------
# 14. Security: plant an advisory for hello, re-sync, audit.
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
    "crtpkg:1:e2e0001:true:$SRC/crtpkg-1.tar.gz" \
    "badcrt:1:e2e0001:true:$SRC/badcrt-1.tar.gz:noattest" \
    --advisory "$E2E/advisory.json" >/dev/null
"$PKG" sync --force >/dev/null
if "$PKG" audit 2>&1 | grep -q "E2E-2026-0001"; then
    pass "pkg audit finds planted advisory for installed hello"
else
    fail "pkg audit"
fi

# ---------------------------------------------------------------
# 15. Direct local recipes: no recipe checkout, signed repo, trust
#     store, HTTP server, or sync cache in this fresh root.
# ---------------------------------------------------------------
LOCAL=$E2E/local-recipes
LROOT=$E2E/local-root
mkdir -p "$LOCAL/dep" "$LOCAL/app" "$LROOT"
cat > "$LOCAL/dep/pkg-meta.toml" <<'EOF'
name = "local-dep"
version = "1"
category = "app-misc"
EOF
printf ':\n' > "$LOCAL/dep/build"
cat > "$LOCAL/dep/install" <<'EOF'
mkdir -p "$PKGDIR/usr/share/local-dep"
printf 'dependency\n' > "$PKGDIR/usr/share/local-dep/installed"
EOF
cat > "$LOCAL/app/pkg-meta.toml" <<'EOF'
name = "local-app"
version = "1"
category = "app-misc"
[dependencies]
runtime = ["local-dep"]
EOF
printf ':\n' > "$LOCAL/app/build"
printf 'first local payload\n' > "$LOCAL/app/payload.txt"
sum=$(sha256sum "$LOCAL/app/payload.txt" | awk '{print $1}')
cat > "$LOCAL/app/source.toml" <<EOF
[[source]]
url = "./payload.txt"
sha256 = "$sum"
filename = "renamed.txt"
EOF
cat > "$LOCAL/app/install" <<'EOF'
mkdir -p "$PKGDIR/usr/share/local-app"
cp renamed.txt "$PKGDIR/usr/share/local-app/payload"
EOF
export PKG_ROOT=$LROOT
"$PKG" install -y "$LOCAL/dep" "$LOCAL/app" >"$E2E/local-install.log" 2>&1 \
    || { cat "$E2E/local-install.log" >&2; fail "direct local install"; }
[ "$(cat "$LROOT/usr/share/local-app/payload")" = "first local payload" ] \
    || fail "relative local source with staged filename"
[ -f "$LROOT/usr/share/local-dep/installed" ] || fail "local dependency installed"
[ ! -e "$LROOT/var/cache/pkg/index/recipe-manifest.toml" ] \
    || fail "local install unexpectedly required a synced manifest"
"$PKG" db check >/dev/null || fail "local install database consistency"
pass "pkg install <recipe-path> resolves local dependencies without a repository"

printf 'second local payload\n' > "$LOCAL/app/payload.txt"
sum=$(sha256sum "$LOCAL/app/payload.txt" | awk '{print $1}')
sed -i "s/^sha256 = .*/sha256 = \"$sum\"/" "$LOCAL/app/source.toml"
"$PKG" build --no-cache "$LOCAL/app" >"$E2E/local-rebuild.log" 2>&1 \
    || { cat "$E2E/local-rebuild.log" >&2; fail "direct local rebuild"; }
[ "$(cat "$LROOT/usr/share/local-app/payload")" = "second local payload" ] \
    || fail "edited local recipe rebuilt payload"
pass "pkg build --no-cache <recipe-path> rebuilds an edited local source pin"

printf 'unverified payload\n' > "$LOCAL/app/payload.txt"
if "$PKG" build --no-cache "$LOCAL/app" >"$E2E/local-mismatch.log" 2>&1; then
    fail "local source hash mismatch built successfully"
fi
grep -q 'SHA-256 mismatch' "$E2E/local-mismatch.log" \
    || fail "local source mismatch diagnostic"
[ "$(cat "$LROOT/usr/share/local-app/payload")" = "second local payload" ] \
    || fail "failed local build changed the installed package"
pass "local source hash mismatch is rejected without changing the installed package"

# A single pkg-meta.toml can provide the source and select a managed Make
# build. No source.toml, build, or install files exist for this recipe.
MANAGED=$LOCAL/managed
mkdir -p "$MANAGED" "$E2E/managed-src/managed-1"
printf 'managed payload\n' > "$E2E/managed-src/managed-1/payload.txt"
cat > "$E2E/managed-src/managed-1/Makefile" <<'EOF'
all:
	cp payload.txt managed.txt
install:
	mkdir -p "$(DESTDIR)/usr/share/managed"
	cp managed.txt "$(DESTDIR)/usr/share/managed/payload"
EOF
tar -C "$E2E/managed-src" -czf "$MANAGED/managed-1.tar.gz" managed-1
sum=$(sha256sum "$MANAGED/managed-1.tar.gz" | awk '{print $1}')
cat > "$MANAGED/pkg-meta.toml" <<EOF
name = "managed-local"
version = "1"
category = "app-misc"

[[source]]
url = "./managed-1.tar.gz"
sha256 = "$sum"

[build]
system = "make"
EOF
"$PKG" install -y "$MANAGED" >"$E2E/managed-local.log" 2>&1 \
    || { cat "$E2E/managed-local.log" >&2; fail "single-file managed recipe"; }
[ "$(cat "$LROOT/usr/share/managed/payload")" = "managed payload" ] \
    || fail "managed Make recipe installed payload"
[ ! -e "$MANAGED/source.toml" ] && [ ! -e "$MANAGED/build" ] && [ ! -e "$MANAGED/install" ] \
    || fail "managed recipe unexpectedly used external recipe files"
pass "single-file recipe builds from a managed Make declaration"

# Custom install logic can be inline when no stock build system applies.
INLINE=$LOCAL/inline
mkdir -p "$INLINE"
cat > "$INLINE/pkg-meta.toml" <<'EOF'
name = "inline-local"
version = "1"
category = "app-misc"

[build]
system = "none"

[scripts]
install = '''
mkdir -p "$PKGDIR/usr/share/inline-local"
printf 'inline payload\n' > "$PKGDIR/usr/share/inline-local/payload"
'''
EOF
"$PKG" install -y "$INLINE" >/dev/null 2>&1 || fail "inline script recipe"
[ "$(cat "$LROOT/usr/share/inline-local/payload")" = "inline payload" ] \
    || fail "inline install script payload"
pass "single-file recipe supports an inline custom phase"

# A local tree supplies dependencies for an external manifest-file target.
# No repository, trust store, sync, or installed recipe checkout is involved.
TREE=$LOCAL/tree
TREE_APP=$LOCAL/tree-app
mkdir -p "$TREE/sys-libs/tree-dep" "$TREE_APP"
cat > "$TREE/sys-libs/tree-dep/pkg-meta.toml" <<'EOF'
name = "tree-dep"
version = "1"
category = "sys-libs"
[build]
system = "none"
[scripts]
install = '''
mkdir -p "$PKGDIR/usr/share/tree-dep"
printf 'tree dependency\n' > "$PKGDIR/usr/share/tree-dep/installed"
'''
EOF
cat > "$TREE_APP/pkg-meta.toml" <<'EOF'
name = "tree-app"
version = "1"
category = "app-misc"
[dependencies]
runtime = ["tree-dep"]
[build]
system = "none"
[scripts]
install = '''
mkdir -p "$PKGDIR/usr/share/tree-app"
printf 'tree application\n' > "$PKGDIR/usr/share/tree-app/installed"
'''
EOF
"$PKG" install -y --recipe-tree "$TREE" "$TREE_APP/pkg-meta.toml" \
    >"$E2E/local-tree.log" 2>&1 \
    || { cat "$E2E/local-tree.log" >&2; fail "manifest target with local recipe tree"; }
[ -f "$LROOT/usr/share/tree-dep/installed" ] && [ -f "$LROOT/usr/share/tree-app/installed" ] \
    || fail "local tree dependency payloads"
[ ! -e "$LROOT/var/lib/pkg/recipes" ] && [ ! -e "$LROOT/var/cache/pkg/index/recipe-manifest.toml" ] \
    || fail "local tree unexpectedly required repository state"
"$PKG" db check >/dev/null || fail "local tree database consistency"
pass "pkg install --recipe-tree resolves dependencies for a pkg-meta.toml target"

# A flat local checkout also permits installing a package by manifest name.
FLAT=$LOCAL/flat-tree
mkdir -p "$FLAT/flat-local"
cat > "$FLAT/flat-local/pkg-meta.toml" <<'EOF'
name = "flat-local"
version = "1"
category = "app-misc"
[build]
system = "none"
[scripts]
install = '''
mkdir -p "$PKGDIR/usr/share/flat-local"
printf 'flat tree\n' > "$PKGDIR/usr/share/flat-local/installed"
'''
EOF
"$PKG" install -y --recipe-tree "$FLAT" flat-local >"$E2E/flat-tree.log" 2>&1 \
    || { cat "$E2E/flat-tree.log" >&2; fail "named package from flat local tree"; }
[ -f "$LROOT/usr/share/flat-local/installed" ] || fail "flat local tree payload"
pass "pkg install --recipe-tree accepts a flat tree and a package name"

echo
echo "ALL E2E CHECKS PASSED"
