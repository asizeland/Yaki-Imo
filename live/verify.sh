#!/bin/sh
# Headless end-to-end verification: boot the ISO and run the in-VM smoke test.
set -eu

if [ -z "${YAKIMO_VERIFY_INNER:-}" ]; then
    exec nix shell nixpkgs#qemu nixpkgs#expect nixpkgs#e2fsprogs \
        -c env YAKIMO_VERIFY_INNER=1 sh "$0" "$@"
fi

LIVE=$(cd "$(dirname "$0")" && pwd)
if [ -n "${YAKIMO_ISO:-}" ]; then
    ISO=$YAKIMO_ISO
else
    ISO=$(find "$LIVE/result/iso" -maxdepth 1 -type f -name '*.iso' -print -quit 2>/dev/null || true)
fi
[ -n "$ISO" ] && [ -f "$ISO" ] || { echo "ISO not found under $LIVE/result/iso" >&2; exit 1; }
ACCEL=tcg
if [ -c /dev/kvm ] && [ -r /dev/kvm ] && [ -w /dev/kvm ]; then
    ACCEL=kvm
fi
SCRATCH=${YAKIMO_SCRATCH:-$LIVE/.build/verify-scratch.raw}
if [ ! -f "$SCRATCH" ]; then
    mkdir -p "$(dirname "$SCRATCH")"
    tmp=$SCRATCH.tmp
    rm -f "$tmp"
    qemu-img create -f raw "$tmp" "${YAKIMO_DISK_SIZE:-20G}" >/dev/null
    mkfs.ext4 -q -F -L YAKIMO_SCRATCH "$tmp"
    mv "$tmp" "$SCRATCH"
fi
export ISO ACCEL SCRATCH
export YAKIMO_MEM=${YAKIMO_MEM:-4G}
export YAKIMO_SMP=${YAKIMO_SMP:-4}

expect <<'EXPECT'
set timeout 300
proc abort {message} {
    puts stderr $message
    catch { exec kill [exp_pid] }
    exit 1
}

spawn qemu-system-x86_64 \
    -machine "q35,accel=$env(ACCEL)" \
    -m $env(YAKIMO_MEM) -smp $env(YAKIMO_SMP) \
    -cdrom $env(ISO) -boot d \
    -display none -serial stdio -monitor none \
    -nic user,model=e1000 \
    -drive "file=$env(SCRATCH),if=virtio,format=raw"

expect {
    -re {YAKIMO# $} {}
    timeout { abort "Timed out waiting for the live shell" }
    eof { abort "QEMU exited before the live shell appeared" }
}

send -- "sudo mkdir -p /mnt/yakimo-scratch && sudo mount /dev/disk/by-label/YAKIMO_SCRATCH /mnt/yakimo-scratch && sudo chown nixos /mnt/yakimo-scratch && echo YAKIMO_DISK_\"\"READY\r"
set timeout 60
expect {
    "YAKIMO_DISK_READY" {}
    -re {YAKIMO# $} { abort "Could not mount the scratch disk" }
    timeout { abort "Timed out mounting the scratch disk" }
    eof { abort "QEMU exited while mounting the scratch disk" }
}

send -- "YAKIMO_WORK=/mnt/yakimo-scratch/work yakimo-live smoke; echo YAKIMO_SMOKE_EXIT=\$?\r"
set timeout 3600
expect {
    "YAKIMO_SMOKE_EXIT=0" {}
    -re {YAKIMO_SMOKE_EXIT=[1-9][0-9]*} { abort "The in-VM smoke test failed" }
    timeout { abort "Timed out during the in-VM smoke test" }
    eof { abort "QEMU exited during the in-VM smoke test" }
}

send -- "sudo poweroff\r"
set timeout 120
expect {
    eof {}
    timeout { abort "VM did not power off cleanly" }
}
puts "YAKIMO_ISO_VERIFIED"
EXPECT
