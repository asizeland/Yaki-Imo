#!/bin/sh
# Extract the real stage 3, install a kernel with its own pkg, and boot it.
set -eu

WS=$(cd "$(dirname "$0")/.." && pwd)
OUT=$(readlink -f "${1:-$WS/image/out}")
CACHE_ROOT=$(readlink -f "${2:-/tmp/pkg-e2e-ports/root}")
ARCHIVE=yakimo-stage3-0.1.0-x86_64.tar.xz
[ -f "$OUT/$ARCHIVE" ] || { echo "missing stage 3 archive: $OUT/$ARCHIVE" >&2; exit 1; }
(
    cd "$OUT"
    sha256sum -c "$ARCHIVE.sha256"
)

docker run --rm -v "$OUT:/out" yakimo-test \
    sh -c 'rm -rf /out/stage3-extracted && mkdir -p /out/stage3-extracted && tar -xJf /out/yakimo-stage3-0.1.0-x86_64.tar.xz -C /out/stage3-extracted'

for path in var/lib/pkg/build var/lib/pkg/cache var/cache/pkg/sources; do
    if find "$OUT/stage3-extracted/$path" -mindepth 1 -print -quit | grep -q .; then
        echo "stage 3 contains excluded state under $path" >&2
        exit 1
    fi
done
if find "$OUT/stage3-extracted" -type l -lname '/cache/*' -print -quit | grep -q .; then
    echo "stage 3 contains an external cache link" >&2
    exit 1
fi

docker run --rm --privileged \
    -e "YAKIMO_JOBS=${YAKIMO_JOBS:-$(nproc)}" \
    -v "$WS:/src:ro" -v "$OUT:/out" \
    -v "$CACHE_ROOT/var/lib/pkg/cache:/cache/artifacts" \
    -v "$CACHE_ROOT/var/cache/pkg/sources:/cache/sources" \
    yakimo-test /src/image/stage3-test-inner.sh

docker run --rm --privileged -v "$WS:/src:ro" -v "$OUT:/out" \
    yakimo-test /src/image/build-dev.sh /out/stage3-extracted /out
sh "$WS/e2e/qemu.sh" "$OUT"

LOG=$OUT/qemu-smoke.log
for warning in \
    "owner 'root:uucp' not found" \
    "rc.stopping" \
    "regulatory.db failed" \
    "Model not found in latest microcode" \
    "specified group" \
    "hostname: command not found" \
    "Failed to set the hostname"
do
    if grep -F "$warning" "$LOG" >/dev/null; then
        echo "warning remains in QEMU log: $warning" >&2
        exit 1
    fi
done
grep -q 'Linux yakimo-dev' "$LOG" || { echo "hostname service did not run" >&2; exit 1; }
printf 'PASS: stage 3 extraction, chroot, kernel installation, and QEMU boot\n'
