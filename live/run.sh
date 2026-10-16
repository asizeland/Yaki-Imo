#!/bin/sh
# Boot the test ISO in the terminal. Extra arguments are passed to QEMU.
set -eu

if [ -z "${YAKIMO_QEMU_INNER:-}" ]; then
    exec nix shell nixpkgs#qemu nixpkgs#e2fsprogs \
        -c env YAKIMO_QEMU_INNER=1 sh "$0" "$@"
fi

LIVE=$(cd "$(dirname "$0")" && pwd)
if [ -n "${YAKIMO_ISO:-}" ]; then
    ISO=$YAKIMO_ISO
else
    ISO=$(find "$LIVE/result/iso" -maxdepth 1 -type f -name '*.iso' -print -quit 2>/dev/null || true)
fi
[ -n "$ISO" ] && [ -f "$ISO" ] || {
    echo "ISO not found under $LIVE/result/iso (run 'sh live/build.sh' first)" >&2
    exit 1
}

ACCEL=tcg
if [ -c /dev/kvm ] && [ -r /dev/kvm ] && [ -w /dev/kvm ]; then
    ACCEL=kvm
else
    echo "KVM unavailable; using slower software emulation" >&2
fi

SCRATCH=${YAKIMO_SCRATCH:-$LIVE/.build/scratch.raw}
if [ ! -f "$SCRATCH" ]; then
    mkdir -p "$(dirname "$SCRATCH")"
    tmp=$SCRATCH.tmp
    rm -f "$tmp"
    qemu-img create -f raw "$tmp" "${YAKIMO_DISK_SIZE:-20G}" >/dev/null
    mkfs.ext4 -q -F -L YAKIMO_SCRATCH "$tmp"
    mv "$tmp" "$SCRATCH"
fi

MEM=${YAKIMO_MEM:-4G}
SMP=${YAKIMO_SMP:-4}
echo "Booting $ISO ($MEM RAM, $SMP CPUs, $ACCEL)"
echo "Persistent package-build scratch disk: $SCRATCH"
exec qemu-system-x86_64 \
    -machine "q35,accel=$ACCEL" \
    -m "$MEM" -smp "$SMP" \
    -cdrom "$ISO" -boot d \
    -display none -serial stdio -monitor none \
    -nic user,model=e1000 \
    -drive "file=$SCRATCH,if=virtio,format=raw" \
    "$@"
