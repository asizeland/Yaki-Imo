#!/bin/sh
# Boot the development disk and run package-integrity checks over serial.
set -eu

if [ -z "${E2E_INNER:-}" ]; then
    exec nix develop -c env E2E_INNER=1 sh "$0" "$@"
fi

WS=$(cd "$(dirname "$0")/.." && pwd)
OUT=$(readlink -f "${1:-$WS/image/out}")
export QEMU_IMAGE_DIR=$OUT
export QEMU_RUNNER=$WS/image/run-qemu.sh
python3 - <<'PY'
import os
import pathlib
import select
import subprocess
import sys
import time

out = pathlib.Path(os.environ["QEMU_IMAGE_DIR"])
log_path = out / "qemu-smoke.log"
runner = os.environ["QEMU_RUNNER"]
process = subprocess.Popen(
    [runner, "--snapshot", str(out)],
    stdin=subprocess.PIPE,
    stdout=subprocess.PIPE,
    stderr=subprocess.STDOUT,
)
deadline = time.monotonic() + 240
buffer = b""
log = bytearray()
ready = False
success = False
shutdown_sent = False
try:
    while time.monotonic() < deadline:
        if process.poll() is not None:
            break
        readable, _, _ = select.select([process.stdout], [], [], 1.0)
        if not readable:
            continue
        chunk = os.read(process.stdout.fileno(), 65536)
        if not chunk:
            break
        sys.stdout.buffer.write(chunk)
        sys.stdout.buffer.flush()
        log.extend(chunk)
        buffer = (buffer + chunk)[-262144:]
        if not ready and b"YAKIMO_QEMU_READY" in buffer:
            ready = True
            time.sleep(1)
            process.stdin.write(
                b"echo QEMU_COMMANDS_BEGIN\n"
                b"uname -a\n"
                b"cat /usr/lib/os-release\n"
                b"/usr/bin/pkg info yakimo\n"
                b"/usr/bin/pkg db check && printf 'QEMU_%s\\n' DB_OK\n"
                b"printf 'QEMU_%s\\n' SMOKE_OK\n"
            )
            process.stdin.flush()
        if not shutdown_sent and b"QEMU_SMOKE_OK" in buffer and b"QEMU_DB_OK" in buffer:
            success = True
            shutdown_sent = True
            process.stdin.write(b"/sbin/poweroff -f\n")
            process.stdin.flush()
            deadline = time.monotonic() + 30
    if process.poll() is None:
        try:
            process.wait(timeout=10)
        except subprocess.TimeoutExpired:
            process.terminate()
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait()
finally:
    log_path.write_bytes(log)
    if process.poll() is None:
        process.kill()
        process.wait()

if not ready:
    raise SystemExit(f"QEMU did not reach the Yakimo console; see {log_path}")
if not success:
    raise SystemExit(f"QEMU boot reached the console but checks failed; see {log_path}")
print(f"PASS: QEMU booted SysVinit/OpenRC and package checks passed; log: {log_path}")
PY
