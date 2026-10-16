# Yaki Imo live-test ISO

This is a disposable **NixOS host appliance for testing `pkg`**, not a
bootable Yaki Imo distribution. The distribution's boot integration is still
under development. The image carries a compiler, Bubblewrap, a representative
ports set, and a signed localhost repository so real source builds work
without external network access in the VM.

The repository uses the project's deterministic **test keys**. Do not publish
or reuse this image as a trusted distribution.

## Build and boot

An x86_64 NixOS/Nix host is required. The build downloads Nix dependencies and
the SHA-256-pinned upstream sources, but needs no root access:

```sh
sh live/build.sh
sh live/run.sh
```

The ISO is written under `live/result/iso/`; its filename includes the NixOS
version, and the build prints its full path and SHA-256. `run.sh` discovers it
automatically. Set `YAKIMO_ISO=/path/to/image.iso` to boot another build.

`run.sh` supplies QEMU through Nix and defaults to 4 CPUs and 4 GiB RAM. It
uses KVM when accessible and software emulation otherwise. Override resources
or pass extra QEMU arguments when needed:

```sh
YAKIMO_MEM=6G YAKIMO_SMP=6 sh live/run.sh
```

At the automatic login prompt:

```sh
yakimo-live smoke       # build Lua/less/SQLite and run verification
yakimo-live shell       # interactive shell with PKG_ROOT and sandbox setup
yakimo-live setup       # prepare the test root without entering the shell
```

Inside `yakimo-live shell`, ordinary commands such as `pkg search`,
`pkg install -y tty-solitaire`, `pkg history`, and `pkg undo` operate on the
isolated `$YAKIMO_WORK/root`, not the live host's `/`.

`run.sh` creates a sparse 20 GiB ext4 scratch image at
`live/.build/scratch.raw` and attaches it to the VM. The live command detects
its `YAKIMO_SCRATCH` filesystem label and mounts it automatically, so package
builds have enough space and persist across reboots. Set `YAKIMO_SCRATCH` to
use another image and `YAKIMO_DISK_SIZE` to change the size at creation.
Without that disk, the fallback `/tmp/yakimo` is RAM-backed and suitable only
for small tests.

When using the ISO in another VM manager, attach a new empty disk, identify it
carefully, and label its filesystem (replace `/dev/vda`; `mkfs` erases it):

```sh
sudo mkfs.ext4 -L YAKIMO_SCRATCH /dev/vda
yakimo-live shell       # discovers and mounts the labelled disk
```

To use an existing filesystem with another label, mount it yourself and set
`YAKIMO_WORK=/mount/point/work` when invoking `yakimo-live`.

## Automated verification

This boots the actual ISO, waits for the live shell, runs the full source-build
smoke test, and powers off:

```sh
sh live/verify.sh
```

The smoke test covers glibc and dependency resolution, linked ncurses
packages, Lua's optional Readline dependency, SQLite's non-ABI option flip,
executing installed binaries through the packaged loader, and `pkg db check`.
It can take a while on a software-emulated VM.

To rebuild from changed recipes, remove nothing manually: `build.sh` restages
the selected ports. Distfiles are reused from `/tmp/pkg-ports-dist` by default;
set `YAKIMO_DISTFILES` to choose another cache. The package list is in
`live/ports.txt`.
