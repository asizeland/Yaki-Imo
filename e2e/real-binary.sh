#!/bin/sh
# e2e/real-binary.sh — end-to-end run repackaging a REAL vendor binary
# blob: Discord's official Linux tarball. This exercises the binary-
# redistributable packaging pattern (v14 [build] binary = true): no
# compilation, vendor ELF + bundled libs placed under /usr/lib/discord,
# a launcher symlink in /usr/bin, desktop file + icon — and the gate's
# relaxed linkage policy for blob-linked system libraries.
#
# Usage:  sh e2e/real-binary.sh        (from the workspace root)
#
# Requires network for the one-time tarball fetch (cached under
# /tmp/pkg-e2e-bin-cache afterwards). The Discord permalink floats to
# the latest release, so the version is resolved from the redirect and
# the hash is recorded by mkrepo rather than pinned here — bump-free,
# at the cost of trusting whatever upstream serves at fetch time.

set -eu

if [ -z "${E2E_INNER:-}" ]; then
    exec nix shell \
        nixpkgs#gcc nixpkgs#perl nixpkgs#bubblewrap nixpkgs#python3 \
        nixpkgs#coreutils nixpkgs#gnutar nixpkgs#gzip nixpkgs#gnumake \
        nixpkgs#sqlite nixpkgs#binutils nixpkgs#gnused nixpkgs#gnugrep \
        nixpkgs#gawk nixpkgs#diffutils nixpkgs#curl \
        -c env E2E_INNER=1 sh "$0" "$@"
fi

WS=$(cd "$(dirname "$0")/.." && pwd)
cd "$WS"

E2E=/tmp/pkg-e2e-bin
CACHE=/tmp/pkg-e2e-bin-cache
ROOT=$E2E/root
REPO=$E2E/repo
TOOLBIN=$E2E/toolbin
PORT=8779
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
mkdir -p "$ROOT" "$REPO" "$TOOLBIN" "$E2E/shim" "$CACHE"

# ---------------------------------------------------------------
# 1. Resolve + fetch the vendor tarball (cached between runs).
# ---------------------------------------------------------------
URL=$(curl -sIL --max-time 60 -o /dev/null -w '%{url_effective}' \
    "https://discord.com/api/download?platform=linux&format=tar.gz")
case "$URL" in
    */discord-*.tar.gz) ;;
    *) fail "unexpected Discord redirect: $URL" ;;
esac
FILE=${URL##*/}                       # discord-X.Y.Z.tar.gz
VER=${FILE#discord-}; VER=${VER%.tar.gz}
TARBALL=$CACHE/$FILE
[ -f "$TARBALL" ] || {
    echo "fetching $FILE from discordapp.net"
    curl -sL --max-time 600 -o "$TARBALL" "$URL"
}
[ -s "$TARBALL" ] || fail "empty Discord tarball"
pass "Discord $VER vendor tarball ready ($(du -h "$TARBALL" | cut -f1))"

# Host glibc, as seen through ldd (the gate requires libc.so.6 from a
# declared dependency — see e2e/run.sh).
LIBC_PATH=$(ldd /bin/sh | awk '/libc\.so/{print $3; exit}')
LD_PATH=$(ldd /bin/sh | awk '/ld-linux/{print $1; exit}')
GLIBC_VER=$(ldd --version | head -1 | awk '{print $NF}')
echo "host glibc: $GLIBC_VER ($LIBC_PATH)"

# ---------------------------------------------------------------
# 2. Signed test repo carrying the vendor tarball.
# ---------------------------------------------------------------
"$MKREPO" "$REPO" "$BASE" \
    "glibc:$GLIBC_VER:e2e0001:false:" \
    "discord:$VER:e2e0001:false:$TARBALL" >/dev/null
pass "mkrepo fabricated signed repo with the vendor tarball"

# ---------------------------------------------------------------
# 3. Recipes. discord has NO build script: [build] binary = true
#    makes the compile phase optional; placement happens in install.
# ---------------------------------------------------------------
for p in glibc discord; do
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

cat > "$ROOT/var/lib/pkg/recipes/discord/pkg-meta.toml" <<EOF
name = "discord"
version = "$VER"
category = "net"

[dependencies]
runtime = ["glibc"]

[build]
binary = true
EOF
cat > "$ROOT/var/lib/pkg/recipes/discord/install" <<EOF
tar xzf $FILE
mkdir -p "\$PKGDIR/usr/lib/discord" "\$PKGDIR/usr/bin" \
         "\$PKGDIR/usr/share/applications" "\$PKGDIR/usr/share/pixmaps"
cp -r Discord/. "\$PKGDIR/usr/lib/discord/"
# The vendor launcher is a shell script that bootstraps the real
# client into the user's config dir on first run; install it as the
# command (its desktop file already points at /usr/bin/discord).
cp Discord/discord "\$PKGDIR/usr/bin/discord"
chmod 755 "\$PKGDIR/usr/bin/discord"
cp Discord/discord.desktop "\$PKGDIR/usr/share/applications/"
cp Discord/discord.png "\$PKGDIR/usr/share/pixmaps/discord.png"
EOF

# ---------------------------------------------------------------
# 4. Config + toolchain farm + bwrap shim (see e2e/run.sh).
# ---------------------------------------------------------------
mkdir -p "$ROOT/etc/pkg"
printf 'mirrors = [\n  "%s",\n]\n' "$BASE" > "$ROOT/etc/pkg/mirrors.toml"

for tool in env sh bash tar gzip sed grep awk cut find xargs uname \
            install cp mv mkdir ln rm cat chmod touch basename dirname \
            sort uniq wc tr head tail readelf sleep date expr printf \
            test du; do
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
# 6. Repackage + install the blob.
# ---------------------------------------------------------------
if ! "$PKG" install -y discord >"$E2E/install.log" 2>&1; then
    echo "---- discord install output ----" >&2
    cat "$E2E/install.log" >&2
    fail "pkg install discord (vendor blob)"
fi
pass "pkg install discord repackaged the vendor blob"

# The blob links system libraries (libX11, libnss3, …) that no package
# in this minimal repo provides: under [build] binary the gate must
# have WARNED, not failed — prove the relaxation actually engaged.
grep -q "vendor blob links undeclared sonames" "$E2E/install.log" \
    && pass "gate warned (not failed) on blob-linked system libraries" \
    || fail "expected a relaxed linkage warning in the install log"

[ -f "$ROOT/usr/bin/discord" ] || fail "discord launcher"
head -1 "$ROOT/usr/bin/discord" | grep -q "^#!/bin/sh" \
    && pass "vendor launcher installed at /usr/bin/discord" \
    || fail "discord launcher is the vendor script"
[ -f "$ROOT/usr/lib/discord/updater_bootstrap" ] || fail "bootstrap ELF in live tree"
readelf -h "$ROOT/usr/lib/discord/updater_bootstrap" | grep -q "Class:.*ELF64" \
    && pass "installed updater_bootstrap is a real x86-64 ELF" \
    || fail "updater_bootstrap ELF"
grep -q "^Exec=/usr/bin/discord" "$ROOT/usr/share/applications/discord.desktop" \
    && pass "desktop file installed (Exec=/usr/bin/discord)" \
    || fail "desktop file"
[ -f "$ROOT/usr/share/pixmaps/discord.png" ] \
    && pass "icon installed" || fail "icon"

"$PKG" db check >/dev/null && pass "pkg db check" || fail "pkg db check"

echo
echo "REAL-BINARY E2E PASSED (Discord $VER)"
