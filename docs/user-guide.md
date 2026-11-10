# Yakimo User Guide

Yakimo builds software on your machine from small build instructions called
**recipes**. It stores each build separately and can undo package changes.
At present, it builds from source; it does not yet download prebuilt packages.

This guide explains how to use Yakimo and how to make packages. It starts
with the basic ideas, then gives commands and examples. Technical terms are
explained where they are first used.

The documentation has three parts:

- **This user guide:** installation, daily use, configuration, and packaging.
- **[Technical manual](technical-manual.md):** what happens inside each
  feature and why the less obvious tools exist.
- **[Developer's manual](developers-manual.md):** how the source code is
  organized and how to contribute to Yakimo itself.

Eventually user guide will be expanded into a larger document:

- [NTBD Handbook](NTBD_Handbook.odt) which will contain the information
  relevant to managing the base system, and most relevant day to day activities:
  working on different kinds of documents, development, virtualization, printing
  playing games...

## Contents

1. The five basic ideas
2. Setting up mirrors, trust, and synchronization
3. Everyday commands
4. Build options (similar to Gentoo's USE flags)
5. Configuration files
6. Controlling build speed and processor use
7. Undo, generations, and disk cleanup
8. Security and verification
9. Named environments
10. Testing safely with `PKG_ROOT`
11. Making and publishing your own packages
12. Exit codes and troubleshooting
13. Testing in containers (Docker)

---

## 1. The five basic ideas

**The store keeps the real files.** A package does not copy its files
straight into `/usr`. Each build gets its own directory under
`/usr/lib/pkg/.store/`. The usual locations, such as `/usr/bin/lua`,
contain **symlinks**: small pointers to those stored files. For example:

```
/usr/bin/lua -> ../lib/pkg/.store/lua-5.4.7+ab1463413912/usr/bin/lua
```

The pointers use relative paths. This lets you move the whole system tree,
enter it with `chroot`, or boot it as `/` without breaking the links.

**Identity identifies an exact build.** A package is more than a name and
version. Its identity also records its architecture, the libraries it was
built against (its **ABI tag**), and any non-default build options (its
**option tag**). The tags are short hashes: fixed-length fingerprints of
those inputs. Two builds of version 2.1 with different options can therefore
stay in the store at the same time. This makes option changes and undo safe.

**Recipes describe how to build software.** A recipe is a small directory
containing package details, an exact source download and checksum, and shell
scripts for the build steps. Chapter 11 explains the format.

**Generations make undo possible.** Each install, upgrade, or removal is a
transaction. Yakimo records the package set before and after it as a
**generation**. Undo restores an earlier recorded package set; it does not
try to reverse individual file edits.

**The sandbox and scanner check builds.** A build runs in an isolated
bubblewrap sandbox. It has no network access and can write only to its work
and output directories. Afterward, the scanner checks the new programs and
libraries. They may use only libraries declared by the recipe, and they must
not require a newer C library than the system provides. Yakimo rejects a
build that breaks these rules instead of installing it.

---

## 2. Setup

### 2.1 Mirrors

`/etc/pkg/mirrors.toml` lists the repositories Yakimo syncs from:

```toml
mirrors = [
  "https://repo.example.org/pkg",
  "https://mirror2.example.org/pkg",   # fallback, tried in order
]
```

The first entry is the main mirror. Yakimo goes down the list item by item. 
Yakimo remembers the newest repository index it accepted and rejects an older one.

### 2.2 Choosing which repository keys to trust

Every repository index has a digital signature. Before the first sync, tell
Yakimo which root keys may authorize that repository:

```
# pkg key trust-root /path/to/root.json
```

Obtain `root.json` separately from the repository or mirror, using sources you trust.
The command shows the key fingerprints and asks for confirmation. This step
matters because a file downloaded from an untrusted server cannot, by itself,
prove that the server should be trusted.

`pkg key list` shows the current trust state. `pkg key show-chain` shows how
the root keys authorize the repository and timestamp keys.

### 2.3 Synchronizing package information

```
# pkg sync            # fetch and verify the package index
# pkg sync --status   # show cache age and freshness; no network
# pkg sync --force    # fetch now even if the cache is still fresh
```

Sync downloads the signed package index and recipe manifest. The manifest
lists the exact sources and checksums for each recipe. Repository builds use
this synced manifest. 

Direct local recipe paths are the explicit exception for
development; chapter 11 explains their checksum and trust rules.

---

## 3. Everyday commands

In these examples, `#` means to run the command as root and `$` means an
ordinary user. Do not type the prompt character itself.

```
# pkg search hello            # search the package index
# pkg info hello              # show the installed build and its options
# pkg install hello           # build and install; ask before changing anything
# pkg install -y hello        # install without asking for confirmation
# pkg install hello@2.1       # request an exact version
# pkg upgrade                 # upgrade packages you installed explicitly
# pkg upgrade --dry-run       # show the plan without changing the system
# pkg remove hello            # refuse if an installed package still needs it
# pkg remove --recursive foo  # also remove packages that depend on foo
# pkg autoremove              # remove dependencies that are no longer needed
# pkg why hello               # show which required package led to this install
# pkg explain 'foo>=2'        # explain why this version request cannot be met
```

`pkg info` also shows where the installed build came from: its recipe commit,
repository index version, and any options that differ from the defaults.

If one package fails to build, packages that do not depend on it can still
finish. Completed builds are saved. Fix the problem and run the same command
again; Yakimo does not rebuild those completed packages.

### Pins: preventing unwanted upgrades

A pin keeps a package on a chosen version or upgrade track:

```
# pkg pin ffmpeg 6.1                  # keep this exact version
# pkg pin ffmpeg 6.1 --mode abi-track # allow updates that keep the same ABI
# pkg pin ffmpeg                      # show the current pin
# pkg unpin ffmpeg                    # allow normal upgrades again
```

Use a pin when an upgrade would break something you need. ABI means the
interface that lets already-compiled programs use a library. `abi-track`
allows compatible fixes but avoids an interface change that would require
other packages to be rebuilt.

### Alternatives and profiles

```
# pkg alternatives list
# pkg alternatives set virtual/awk gawk
# pkg rebuild --affected  # rebuild packages affected by a provider change

# pkg profile list
# pkg profile apply <name>
```

A **virtual** is a requirement that several packages can satisfy. For example,
both gawk and mawk may provide `virtual/awk`. `pkg alternatives` chooses which
one Yakimo prefers. A **profile** is a named set of package rules supplied by
the distribution, such as rules for a hardened system. 

### Rebuild cascades

When a library's interface changes, programs built against the old interface
may need to be rebuilt. Yakimo calls this group of dependent rebuilds a
**cascade**. It includes the cascade in the installation plan. You can postpone
it and run it later:

```
# pkg cascade --at 03:00        # schedule the pending rebuilds
# pkg cascade --execute-pending # run the pending rebuilds now
```

---

## 4. Build options (similar to Gentoo's USE flags)

Build options control optional package features. Each recipe declares its
available options and their default values. Options are **additive**: enabling
one can add a dependency or a configure argument, but it cannot secretly
remove or replace another part of the package. This makes the result easier
to predict.

### 4.1 Viewing available options

```
# pkg options sqlite
sqlite 3.45.3
  fts5 = true   (default: true)
      Full-text search (FTS4/FTS5) support

# pkg options               # show your saved choices and their source files
```

`pkg info <name>` shows the options used by the build that is actually
installed.

### 4.2 Changing options for one command

Either form works:

```
# pkg install ffmpeg --opt x11=0 --opt pulseaudio=1
# pkg install 'ffmpeg[opt:x11=0]'
```

These choices apply only to this command. Yakimo does not save them.

### 4.3 Saving your choices

Put permanent choices in `/etc/pkg/options.toml`:

```toml
# Applies to every package that declares this option.
[defaults]
nls = false

# Applies only to ffmpeg.
[package.ffmpeg]
x11 = true
pulseaudio = false
```

If more than one setting applies, the strongest wins, in this order:
command-line option → package setting → default setting → recipe default.

### 4.4 Organizing options in separate files

You can keep large option sets in separate files next to `options.toml`:

```toml
# /etc/pkg/options.toml
include = ["sound.toml", "desktop.toml"]
```

Included files use the same `[defaults]` and `[package.<name>]` tables.
Only plain file names in the same directory are allowed: no absolute paths,
`..`, or subdirectories. An included file cannot include another file.
Yakimo reads includes in the listed order; settings in the main file win.
`pkg options` shows which file supplied each value.

Unknown options, values other than `true`/`false`, misspelled package names,
and missing include files are errors. Yakimo reports the file instead of
silently ignoring a possible mistake.

### 4.5 What changes when an option changes

Changing an option creates a new package identity, so Yakimo rebuilds the
package and keeps the old build for undo. A recipe may mark an option as
`abi_affecting = false` when it changes only documentation or runtime data.
That option still creates a new identity but does not force dependent
packages to rebuild.

---

## 5. Configuration files

All configuration lives under `/etc/pkg/`:

| File | Purpose |
|---|---|
| `mirrors.toml` | repository URLs (§2.1) |
| `options.toml` | standing build-option selections (§4.3, §4.4) |
| `config.toml` | machine-level settings (below) |

`config.toml`:

```toml
# Compiler cache: wrap the compiler with ccache or sccache so rebuilds
# of unchanged source are near-instant. "off" (or absent) disables it.
# A recipe can opt out with [build] compiler_cache = false.
[cache]
compiler = "ccache"

# Build parallelism.
#   jobs         — total compile jobs across the machine (the
#                  scheduler's token pool). Default: your core count.
#   max_pkg_jobs — per-package `make -j` cap. Default: 4.
#   opt_level    — the optimization level every package is built with:
#                  "o0" | "o1" | "o2" (default) | "o3" | "os".
[build]
jobs = 12
max_pkg_jobs = 6
opt_level = "o2"

# Relocate state paths from their FHS defaults. Keys are the default
# absolute paths; values must be absolute. Rarely needed.
[paths]
# "/var/cache/pkg" = "/fast-disk/pkg-cache"
```

The `[paths]` overrides exist because some machines keep state on
unusual mounts. The defaults follow the filesystem hierarchy standard;
override only if you need to.

Unknown keys and wrong value types are hard errors everywhere. Yakimo
never guesses what a config line meant.

---

## 6. Controlling parallelism

Three knobs, from widest to narrowest:

- `[build] jobs` in `config.toml`: the total job budget for the whole
  machine. Default: the number of CPU cores.
- `[build] max_pkg_jobs`: the largest `make -j` a single package may
  get. Default: 4 — one package should not starve the others when
  several build at once.
- `pkg install --jobs N`: override the total budget for one command.

The scheduler hands out tokens: a configure phase holds one token, a
compile phase up to `max_pkg_jobs`. A build can always make progress
on one token, so `jobs` builds never deadlock; they just queue.

## 6.1 Optimization level

`[build] opt_level` sets the optimization flag every package is
compiled with: `o2` (the default), `o3` for more aggressive
optimization, `os` for smaller binaries, `o0`/`o1` for debugging.

This is deliberately the *only* compiler-flag knob. Yakimo does not
accept arbitrary `CFLAGS`: the flags a distribution is compiled with
are part of its ABI contract, so the level is declared, bounded, and
hashed into every package's identity. Two consequences follow:

- **Flipping the level rebuilds the world.** An `o2` artifact and an
  `o3` artifact are different packages to Yakimo, so the artifact
  cache can never hand you one when you asked for the other.
- **A recipe can still pin its own flags.** If a package is known to
  miscompile under aggressive optimization, its build script exports
  its own `CFLAGS` and wins over the profile. The reference tree does
  this for glibc, which stays at `-O2` whatever you choose.

`-march=native`-style ISA flags are not offered at all: they bake your
CPU into the binaries, which breaks cache correctness and any future
binary redistribution. The planned `glibc-hwcaps` support is the safe
path to ISA-specific builds.

---

## 7. Undo, generations, and disk cleanup

```
# pkg history            # list transactions

# pkg history 7          # show what generation 7 changed

# pkg undo               # undo the most recent transaction

# pkg undo 4             # undo transaction 4, if it is the next sequential undo

```

Undo is strictly sequential: undo the latest transaction, then the one before
it. Yakimo restores the recorded package set rather than trying to reverse a
list of individual commands.

Undo restores packages and their symlinks. It does not change `/etc`, user
data, pins, or a staged glibc swap. Running programs may still use old
libraries until they are restarted.

```
# pkg clean              # remove store builds not needed by the system or recent undo

# pkg clean --garbage    # also release old builds kept for undo

# pkg clean --cache      # clear the reusable build-artifact cache

```

Ordinary `pkg clean` keeps installed builds and builds referenced by up to
five recent restorable generations. `pkg clean --garbage` releases those undo
holds and warns which generations will stop being restorable.

**Current limitation:** after those stored builds are removed, undo cannot yet
rebuild them. The `--rebuild-missing` option explains the missing directories
but does not complete the promised rebuild. Do not use `--garbage` if you still
need the affected undo history.

Generations make old builds easy to select: returning to an earlier package
set is a lookup and activation operation, not a manual reinstall.

---

## 8. Security and verification

```
# pkg audit                            # show security notices for installed packages

# pkg audit --supply-chain             # check signatures, source records, and reproducibility

# pkg audit --supply-chain --critical  # check only the critical package set

# pkg audit --glibc                    # find running processes using a removed C library

# pkg audit --acknowledge <pkg-name>   # accept a finding after investigating it

# pkg check                            # check library links across the system

# pkg check firefox --depth deep       # perform a deeper scan of one package

# pkg check ./some-binary --suggest    # suggest providers for missing libraries

# pkg db check                         # compare the package database with the store

# pkg db regenerate                    # rebuild the database from the store
```

Two similar checks have different jobs:

- **The supply-chain check runs before a transaction.** A serious,
  unacknowledged finding blocks installs and upgrades so that you cannot
  overlook it. Read `pkg audit --supply-chain` first. Use
  `pkg audit --acknowledge <pkg>` only after investigating the finding.
- **`pkg check` examines files that are already installed.** The scanner
  checks new files at build time; this command is useful after manual changes,
  restoring a backup, or suspected corruption.

Other useful inspection and maintenance commands are:

```
# pkg fetch hello                 # download and verify hello's declared sources

# pkg build hello --no-cache      # force a build without using the artifact cache

# pkg build-env hello             # show the environment a build would receive

# pkg isolate /usr/bin/hello      # run a program in a temporary mount namespace

# pkg bootstrap --verify-layout   # check that a staged glibc swap is safe

# pkg apply-pending-restarts      # restart OpenRC services using deleted files
```

`pkg isolate` is for trying a program with extra isolation; it is not a
security boundary for an untrusted program. `pkg apply-pending-restarts`
exists because replacing a library on disk does not make an already running
service use the new copy. Restarting the affected services completes that
kind of upgrade.

---

## 9. Named environments

A named environment adds a selected package set to your shell's search paths
without changing the system-wide package set. It can provide a project
toolchain or a separate set of media tools:

```
# pkg env create media
# pkg env create ci --from manifest.toml
# eval "$(pkg env activate media)"  # prepend PATH, MANPATH, and PKG_CONFIG_PATH
# pkg env show media                # show the exact builds in the environment
# pkg env update media              # recreate it from its saved manifest
# pkg env export media              # print its manifest and exact identity lock
# pkg env delete media
```

An exported environment records exact identities, so another setup can use the
same set rather than whichever versions are newest that day.

**Current limitation:** environment creation links packages that are already
installed; it does not yet resolve and build missing packages from the
manifest. An environment changes shell search paths. It is not a container or
a security sandbox.

---

## 10. Testing safely with `PKG_ROOT`

`PKG_ROOT` places Yakimo's main package state beneath a test directory. Use it
to try repository, build, install, and undo operations without installing into
your real `/usr` tree:

```
$ export PKG_ROOT=/tmp/yakimo-test
$ mkdir -p "$PKG_ROOT/etc/pkg"
$ $EDITOR "$PKG_ROOT/etc/pkg/mirrors.toml"  # point at a repository
$ pkg key trust-root root.json             # import the test root's trust keys
$ pkg sync
$ pkg install hello
```

`PKG_ROOT` changes paths; it is not a container. Host tools and network access
are still used for builds, and some host-inspection, service-restart, cgroup,
and early-boot operations are not redirected. Use the disposable container
below when a test needs stronger separation.

### 10.1 The seed ports tree

The seed recipe tree is a small ebuild/ports-style collection meant for live
testing. It is maintained in its own Git repository, separately from the
Yakimo source. Clone it anywhere and remember its path:

```sh
git clone <ports-git-url> "$HOME/src/yakimo-ports"
export PORTS=$HOME/src/yakimo-ports
```

The tree contains the C library and its prerequisites (linux-headers, glibc,
libxcrypt — all built from source), libraries (zlib, bzip2, ncurses, readline),
tools (sqlite, lua, less, tar, gzip, xz, grep, sed, gawk), documentation
(man-pages), a game (tty-solitaire), a vendor binary (discord), the chrootable
core (bash, coreutils), an init system (sysvinit), the git chain (pkgconf,
openssl, expat, ca-certificates, curl, git — proven with a real HTTPS fetch),
a minimal-system set (util-linux, iproute2, procps-ng, e2fsprogs, openssh),
and the NetworkManager stack (python3, meson, ninja, libffi, pcre2, glib, dbus,
kmod, eudev, the gnutls chain, libnl, wpa_supplicant, networkmanager).
`$PORTS/README.md` has the details.

The package-manager repository's `e2e/ports.sh` automates the full signed-repo
flow. That harness expects the recipe checkout at `ports/` in the workspace;
use a checkout or symlink there when running it. Direct `--recipe-tree`
commands accept any location and do not need that layout.

Activation links are relative, so a `PKG_ROOT`-installed tree is
directly chrootable. On live installation media the flow is:

```sh
# Mount the target partition at /mnt/target first.
export PKG_ROOT=/mnt/target
pkg install --recipe-tree "$PORTS" \
    bash coreutils grep sed gawk tar gzip xz less lua sysvinit
# Mount /proc, /sys, and /dev (arch-chroot does this for you), then:
chroot /mnt/target /usr/bin/bash
# Inside the chroot, do not retain the host-side PKG_ROOT setting:
unset PKG_ROOT
```

Testing — more installs, option flips, service experiments — continues inside
the chroot. To run package operations there, provide a musl-static `pkg` binary
and make the recipe tree visible with a bind mount; a host dynamic binary is
not guaranteed to run against the target libc. The e2e suite proves the base
works by booting the installed sysvinit as PID 1 in a container and running an
inittab action through the tree's own `/bin/sh`.

### 10.2 Bootable test appliance

`live/` builds an x86_64 NixOS live ISO for testing `pkg` in a VM. It is
a disposable host with a local signed test repository and build toolchain,
not a bootable Yakimo distribution. Build and boot it with:

```
$ sh live/build.sh
$ sh live/run.sh
```

At the live prompt, `yakimo-live smoke` performs real source builds and
checks them; `yakimo-live shell` opens an isolated `PKG_ROOT` shell for
interactive testing. See `live/README.md` for resource settings, persistent
VM disks, test-key warnings, and headless verification.

### 10.3 Keeping versions current

Recipes always pin an exact version and a source hash — that pin is
what makes builds reproducible and signable. So there is no "install
the latest" pointer a client follows. Instead, the *bump* is
automated on the repository side:

```sh
"$PORTS/watch.sh"           # which recipes are behind upstream?
"$PORTS/watch.sh" --apply   # bump version + hash in place
```

A recipe carries a `watch.toml` describing how to poll its upstream (Git tags
or a release-page pattern, with a stable-only filter). `--apply` downloads the
new distfile, recomputes the hash, and rewrites the recipe. This is a maintainer
tool, not an automatic client-side upgrade. Section 11.6 explains the complete
tree-maintenance workflow.

### 10.4 OpenRC and Intel/NVIDIA hardware recipes

The tree includes OpenRC alongside SysVinit, a configured Linux kernel,
Intel CPU microcode, and NVIDIA's proprietary driver. Install them with:

```sh
pkg install sysvinit openrc linux intel-microcode nvidia-drivers
```

OpenRC installs service definitions but does not enable services for you.
On the live system, use `rc-update` to choose runlevels. A basic service
setup looks like this:

```sh
rc-update add devfs sysinit
rc-update add sysfs sysinit
rc-update add procfs sysinit
rc-update add udev sysinit
rc-update add localmount boot
rc-update add dbus default
rc-update add NetworkManager default
rc-status                 # show the current runlevel and service state
```

The kernel package installs `/boot/vmlinuz-6.19.14-yakimo`, its modules,
configuration, and a matching external-module build tree under
`/usr/src/`. It does not yet configure a bootloader or generate an
initramfs. Early Intel microcode loading requires that boot configuration;
installing the files alone does not prove the CPU received an update.

The NVIDIA recipe installs the source for `nvidia`, `nvidia-modeset`,
`nvidia-drm`, and `nvidia-uvm`. At the end of the transaction, Yakimo asks
DKMS to build and install those modules for each installed kernel. It also
installs NVML and `nvidia-smi`, CUDA/OpenCL userspace, the Ada GSP firmware,
and Vulkan/EGL vendor manifests. Vulkan/OpenGL loader and X11 libraries are
separate packages needed by the future desktop stack; installing the driver
alone is not a complete graphical desktop.

Installing a new kernel automatically rebuilds the installed NVIDIA modules
for that kernel. Reinstalling or upgrading the driver builds it for every
installed kernel. Use `dkms status` to inspect DKMS's view of the modules.
A failed module build produces a warning but does not remove the new kernel.
Read `/var/log/pkg/dkms-nvidia-drivers-<kernel>.log`, fix the cause, and run
`pkg build nvidia-drivers` to try again before booting that kernel.

DKMS modules are generated system files, not immutable store payloads.
Module signing and automatic cleanup during package removal or undo remain
follow-up work.

On the Intel + RTX 4070 test machine, check the actual results:

```sh
grep -m1 microcode /proc/cpuinfo   # inspect the applied microcode revision
modprobe nvidia
modprobe nvidia-modeset
modprobe nvidia-drm
nvidia-smi                         # must report the RTX 4070 and driver 595.112.07
```

Container tests verify recipes, linkage, files, and module metadata. They
cannot verify that your CPU accepted microcode or that the GPU initialized.

---

## 11. Making your own packages

A recipe is a directory containing `pkg-meta.toml`. For common build systems,
that one file is enough. Here is a complete source recipe:

```toml
name = "hello"
version = "2.12"
category = "app-misc"

[dependencies]
runtime = ["glibc"]

[[source]]
url = "https://ftp.gnu.org/gnu/hello/hello-2.12.tar.gz"
sha256 = "31e066137a962676e89f69d1b65382de95a7ef7d914b8cb956f41ea72e0f516b"

[build]
system = "autotools"
configure_args = ["--disable-nls"]
```

This tells Yakimo what to download, how to verify it, and how to build it. You
do not need to write shell commands for this package. Start by copying a similar
recipe from your tree. `$PORTS/sys-apps/sed/` is a single-file Autotools
example.

Older recipes may still use `source.toml` and external `prepare`, `build`,
`check`, and `install` files. Do not define the same source list or phase in
both forms: Yakimo reports the conflict instead of choosing one silently.
`watch.toml` remains a separate optional file for upstream version tracking.

### 11.1 Package details and dependencies

The beginning of `pkg-meta.toml` gives the package name, exact version, and
category. Dependencies are divided by when they are needed:

```toml
[dependencies]
link = ["ncurses"]        # libraries this package links against
runtime = ["glibc"]       # needed to run, but not linked
build = ["perl"]          # needed only while building
```

Entries accept version constraints (`zlib>=1.3`, `foo=2.0`), alternatives
(`cronie|fcron`), and virtuals (`virtual/awk`). If your program links a library
that you did not declare, the post-build check fails and can name the package
that provides it.

Options are declared booleans. They can add dependencies but cannot remove
base dependencies:

```toml
[options.readline]
default = false
description = "Interactive line editing via readline"
abi_affecting = true            # default; false is suitable for docs/data
add_deps = ["readline"]         # added when the option is enabled
add_build_deps = ["help2man"]   # extra build-time dependency
```

For an out-of-tree kernel module, add a `[dkms]` table:

```toml
[dependencies]
runtime = ["dkms", "kmod", "gcc", "make"]

[dkms]
name = "example"
version = "1.0"
```

The install phase must place the module source and its upstream-style
`dkms.conf` in `$PKGDIR/usr/src/example-1.0/`. Do not compile the module in the
normal build phase. After activation, Yakimo runs DKMS without network access.
It builds a new or upgraded module for every installed kernel and builds an
existing module when a new kernel appears. Kernel packages declare
`provides = ["virtual/kernel"]` and install a usable
`/usr/lib/modules/<release>/build` tree. DKMS failures are warnings; the package
remains installed and the log names the cause.

### 11.2 Sources and checksums

Add one `[[source]]` table for each required file. SHA-256 is mandatory:

```toml
[[source]]
url = "https://ftp.gnu.org/gnu/hello/hello-2.12.tar.gz"
sha256 = "31e066137a962676e89f69d1b65382de95a7ef7d914b8cb956f41ea72e0f516b"
filename = "hello.tar.gz"  # optional name seen by the build
```

Compute the checksum with `sha256sum <file>`. `filename` is useful when an
upstream download has an ambiguous name.

For a recipe supplied by directory path, `url` may be HTTP(S), a path relative
to the recipe directory, an absolute path, or a `file://` URL:

```toml
[[source]]
url = "./hello-2.12.tar.gz"
sha256 = "31e066137a962676e89f69d1b65382de95a7ef7d914b8cb956f41ea72e0f516b"
```

Local files are re-read and checked on every build, so changing a file without
updating its hash is an error. HTTP(S) downloads use the same checksum rule.
The build sandbox itself has no network access.

Repository-installed packages accept only HTTP(S) sources from the signed
recipe manifest. `file://` remains disabled on that path.

### 11.3 Build systems and custom steps

Set `[build] system` to one of these values:

| Value | What Yakimo does |
|---|---|
| `autotools` | Runs `./configure --prefix=/usr`, `make`, and `make install`. |
| `meson` | Creates `.pkg-build` with Meson, compiles it, and runs Meson install. |
| `cmake` | Configures `.pkg-build` with CMake, compiles it, and runs CMake install. |
| `make` | Runs `make` and `make install`. |
| `none` | Does not generate a build or install phase; supply custom install steps. |
| `custom` | Uses your phase scripts. This is the default for older recipes. |

Managed systems unpack the first recognized tar archive automatically. Yakimo
uses the single extracted directory as the working directory. If that is not
unambiguous, set it explicitly:

```toml
[build]
system = "meson"
source_dir = "program-1.0"
configure_args = ["-Ddocumentation=false"]
```

`configure_args` is available for Autotools, Meson, and CMake. Every generated
install phase places files beneath `$PKGDIR`; Yakimo then checks and installs
that output.

When a stock build system does not fit, put custom steps in `[scripts]`:

```toml
[build]
system = "custom"

[scripts]
prepare = '''
# Apply patches or make other source changes here.
'''

build = '''
make linux-readline
'''

check = '''
make test
'''

install = '''
make DESTDIR="$PKGDIR" install
'''
```

All four keys are optional individually and override the corresponding generated
phase. `custom` requires `build` unless this is a binary repackage, and always
requires `install`. A failing `check` stops the build; omit it if you do not
intend to run tests. Literal triple-quoted TOML strings preserve shell text.

Scripts run with `/bin/sh -e`, in order, inside the sandbox. They receive:

| Variable / path | Meaning |
|---|---|
| `$PKGDIR` | Output tree; everything installed here becomes the package. |
| `PKG_OPT_<NAME>` | `0` or `1` for each declared option. |
| `MAKEFLAGS` | The granted parallelism, such as `-j4`. |
| `/pkg/deps/<name>` | A dependency's headers, libraries, and tools. |
| `SOURCE_DATE_EPOCH` | Fixed timestamp for reproducible builds. |
| `CC`, `CXX` | Compiler commands, including a cache wrapper when enabled. |

The only writable locations are the build scratch and `$PKGDIR`. Sources are
fetched and hash-checked before these scripts run.

For a repackaged binary, set `[build] binary = true`. The build phase becomes
optional; supply an install phase that unpacks and places the vendor files.
The linkage check reports undeclared vendor-library references as warnings
instead of errors.

### 11.4 Test and improve a recipe

Pass the recipe directory or its `pkg-meta.toml` file directly. You do not need
to generate a repository, install a recipe checkout, configure trust, or run
`pkg sync`:

```sh
export PKG_ROOT=/tmp/hello-test

# Fetch and check the sources without compiling.
pkg fetch ./myrepo/app-misc/hello/pkg-meta.toml

# Build the current files, bypassing previously cached package artifacts.
pkg build --no-cache ./myrepo/app-misc/hello

# Resolve dependencies from the local tree and install the result.
pkg install --recipe-tree ./myrepo ./myrepo/app-misc/hello/pkg-meta.toml
```

The path may be relative or absolute. Yakimo reads the package name and version
from `pkg-meta.toml`, so the recipe directory itself does not need a particular
name. `--recipe-tree` accepts a ports tree (`category/package/`), a flat tree
(`package/`), or a generated repository containing `recipes/package/`. You can
also install a package by name with `--recipe-tree` instead of giving its path.

Dependencies otherwise come from the installed recipe checkout. You can still
pass several local recipe paths together when you do not want to load a tree:

```sh
pkg install ./my-lib ./my-app
```

An explicit recipe path overrides a same-name recipe in the tree; the tree
overrides the installed checkout. Duplicate package names inside one tree are
an error.

`pkg build <path>` builds one recipe without resolving missing dependencies.
Use `pkg install` when the dependency chain also needs building. `--background`
cannot be used with a local path or tree because the current background state
cannot preserve those paths.

After an edit, run the same command again. No repository regeneration or sync
is needed. Use `pkg build --no-cache <path>` when you want to force a rebuild of
an already installed version. The recipe's content hash prevents stale package
artifacts from being reused after a recipe or source-pin change.

Local recipes and trees produce a warning because they have not been reviewed
or attested by a repository. Their source hashes, sandbox, and linkage checks
still apply. The explicit paths are your authorization to build those local
instructions.

If the build uses a library that you did not declare, the error names the
missing dependency and may name its provider:

```
FAILED: hello-1: scanner gate rejected build: .../hello: links against
undeclared dependencies: libz.so.1 (not provided by this package or
any declared dep)
  hint: libz.so.1 — provided by installed package zlib (add it to [dependencies])
```

Add the provider to the correct dependency list and build again. Also try the
installed program: successful compilation alone does not prove that it works.

### 11.5 Put a new package or update into a repository

There are two different things called the repository here:

1. The **recipe source tree** is a Git repository containing directories such
   as `app-misc/hello/`. People write and review recipes here. It can be used
   directly with `--recipe-tree`; it does not have to live inside Yakimo's
   source checkout.
2. The **generated package repository** contains signed indexes, recipe
   manifests, and source files. This is what `pkg sync` downloads.

Editing `/var/lib/pkg/recipes/` changes only your machine. It does not update
either repository for other people.

To contribute a package, work in the recipe tree's own repository:

```sh
git clone <ports-git-url> yakimo-ports
cd yakimo-ports
git switch -c add-hello
export PORTS=$PWD

# Add and test app-misc/hello/ as described in section 11.4.
git add app-misc/hello
git diff --cached
git commit -m "Add hello 2.12"
git push -u origin add-hello
```

If you do not have permission to push to the main tree, push to your fork and
open a pull request. Include `watch.toml` where possible. Do not commit
downloaded tarballs, test roots, build output, or generated repository files.
Explain how you tested the package and mention any runtime steps or
limitations.

A repository maintainer then reviews and merges the change, generates a new
package repository with an increased `index_version`, signs it with the
deployment's real keys, and publishes the generated files. The test keys
produced by `ports2repo` must not become a public trust root. After
publication, users receive the package with:

```sh
pkg sync
pkg install hello
```

If you operate your own test repository, you can regenerate it directly. When
deleting or renaming a package, use a new output directory or remove its stale
generated recipe so the old copy is not published again.

### 11.6 Maintain a recipe tree

A recipe tree is the source of truth for packagers. Keep it in Git, make one
focused change per branch, and test the actual package before proposing the
change. A typical tree uses `<category>/<package>/pkg-meta.toml`, with an
optional `watch.toml` beside it. Package names must be unique within a tree:
Yakimo rejects duplicate names instead of choosing one silently.

Start from a current, clean checkout:

```sh
cd "$PORTS"
git status --short
git switch main
git pull --ff-only
git switch -c update-hello
```

For a new package, choose the closest existing recipe, declare every link,
runtime, and build dependency, and pin every source with SHA-256. Add a watch
rule when upstream exposes usable tags or release listings. For an existing
package, first see which watched recipes have updates:

```sh
"$PORTS/watch.sh"
```

This command only reports. It does not edit recipes. A recipe without
`watch.toml` is skipped, so a clean report does not prove that every package in
the tree is current.

#### Describe where updates come from

`watch.toml` is a flat TOML file. This example discovers stable Git tags and
turns a version into the archive URL used by the recipe:

```toml
type = "git-tags"
url = "https://github.com/example/hello.git"
tag_prefix = "v"
stable_regex = '^[0-9]+\.[0-9]+(\.[0-9]+)?$'
url_template = "https://github.com/example/hello/archive/refs/tags/v{version}.tar.gz"
filename_template = "hello-{version}.tar.gz"
```

The Git URL is used only to list tags. The archive URL in `url_template`
produces the downloadable source. For a project that publishes a release index
instead, use `type = "release-regex"`, set `url` to that page, and set `regex`
to a pattern whose first capture group is the version:

```toml
type = "release-regex"
url = "https://ftp.gnu.org/gnu/hello/"
regex = 'hello-([0-9.]+)\.tar\.gz'
stable_regex = '^[0-9]+\.[0-9]+(\.[0-9]+)?$'
url_template = "https://ftp.gnu.org/gnu/hello/hello-{version}.tar.gz"
```

`stable_regex` excludes versions whose names do not fit your stability policy.
It is a filter, not a security assessment. Check upstream's release notes and
do not automatically exclude security releases merely because their version
format is unusual.

#### Apply and inspect an update

```sh
"$PORTS/watch.sh" --apply
git status --short
git diff
```

For each outdated watched recipe, `--apply` downloads the archive named by
`url_template`, calculates its SHA-256, and updates the exact version and
source pin in `pkg-meta.toml` or legacy `source.toml`. The script checks the
whole tree and may change several recipes. There is currently no per-package
filter, so stage only the changes you intend to keep.

A newly calculated hash proves that future downloads must match the archive
you just fetched. It does not prove that the archive is authentic. Check the
release source, signatures where available, and upstream changes before
accepting the bump. Do not replace a hash merely to make an unexpected
checksum failure disappear.

Some updates need manual work: changed archive layouts may require a new
`source_dir`; new libraries need dependency declarations; and changed build
options may need recipe edits. Recipes with no suitable watch rule must be
updated by hand using the same version-and-hash process.

#### Test, review, and publish

Test each changed recipe in a disposable root, including its dependency
closure:

```sh
export PKG_ROOT=/tmp/yakimo-tree-test
pkg fetch "$PORTS/app-misc/hello/pkg-meta.toml"
pkg install --recipe-tree "$PORTS" "$PORTS/app-misc/hello/pkg-meta.toml"
hello --version
```

Use the program's real test suite in the recipe's `check` phase when one is
available. Test important options and packages that depend on the changed
library, not just whether compilation finishes. `pkg install --dry-run` shows
the resulting rebuild cascade. After a failure, fix the recipe and repeat the
test; do not treat a cached old artifact as proof that the new recipe works.

Review the final diff and commit only recipe sources:

```sh
git diff --check
git diff
git add app-misc/hello
git commit -m "Update hello to 2.12.1"
```

Send the branch through the tree's normal review process. Direct tree users
choose when to pull and can then preview updates with:

```sh
pkg upgrade --all --recipe-tree "$PORTS" --dry-run
```

No `pkg sync` is involved in that local-tree workflow. If the tree also feeds
a generated package repository, regenerate, sign, and publish it only after
the recipe changes have passed review. Never publish deterministic test keys
as a production trust root.

---

## 12. Exit codes and troubleshooting

| Code | Meaning |
|---|---|
| 0 | success |
| 1 | resolution failure (`pkg explain <constraint>` can show the conflict) |
| 2 | build failure (completed build artifacts can be reused on retry) |
| 3 | a security or verification check refused the operation |
| 70 | another kind of error |

Common messages:

- **`unknown option in selection: 'x11'`** — no package in this
  transaction declares that option. Check spelling with
  `pkg options <name>`.
- **`option conflict on ffmpeg.x11: ...`** — two command-line requests
  disagree; choose one value.
- **`options.toml: [package.foo] names undeclared option 'bar'`** —
  the configuration no longer matches the recipe. Fix or remove the entry.
  The message names the file that contains it.
- **`<pkg> requires glibc X, but Y is staged-pending`** — reboot to
  apply the pending glibc, or install an older package version.
- **`no longer restorable`** on undo — `pkg clean --garbage` removed stored
  builds needed by that generation. Automatic rebuilding during undo is not
  implemented yet.
- **supply-chain gate** — read `pkg audit --supply-chain`; acknowledge
  with `pkg audit --acknowledge <pkg>` only after investigating it.

---

## 13. Testing in containers (Docker)

Two Dockerfiles live in the project root. They exist so you can build
and exercise Yakimo without touching your host system — no packages to
install, no state to clean up.

| File | Base | What it is for |
|---|---|---|
| `dockerfile` | Debian (glibc) | The main test environment. Runs the **full end-to-end suite** and lets you poke at `pkg` interactively. |
| `dockerfile.alpine` | Alpine (musl) | Verifies the workspace compiles on **musl** and builds the **musl-static release binaries** (the distribution-independent deliverable). The e2e suites cannot run here: they wrap the host's glibc as a package, and Alpine has no glibc. |

Build and enter (from the project root):

```sh
docker build -f dockerfile -t yakimo-test .
docker run --rm -it --privileged yakimo-test

docker build -f dockerfile.alpine -t yakimo-test-alpine .
docker run --rm -it yakimo-test-alpine
```

**Building the image is itself a test:** the Dockerfile runs the whole
`cargo test` suite (and, on Alpine, a static-link check) as a build
step, so an image that builds successfully already proves the unit
suite passed.

**Why `--privileged` for the Debian container:** Yakimo builds run in
`bwrap` sandboxes, and `bwrap` inside a container must create nested
user/mount namespaces and mount `proc` — Docker's default security
profile forbids that (and `seccomp=unconfined` plus `SYS_ADMIN` alone
are not enough). Grant it only to this disposable test container; the
Alpine container needs no such flag for its unit/static-build role.

Inside the Debian container, from `/src`, the end-to-end scripts:

```sh
sh e2e/run.sh           # smoke: signed repo, install, build gate
sh e2e/lifecycle.sh     # remove/autoremove, upgrade, undo, mirrors
sh e2e/options.sh       # build options, store GC, undo interplay
sh e2e/dkms.sh          # kernel-triggered modules and nonfatal failure
sh e2e/real-package.sh  # a real autotools package from the internet
sh e2e/real-binary.sh   # a repackaged binary blob (needs network)
sh e2e/ports.sh         # the whole ports tree from scratch — hours
```

### Installing from the ports tree by hand

The e2e scripts automate everything below, but doing it by hand is the best way
to learn how the pieces fit. This is the four-stage **signed-repository** flow.
The simpler unsigned local-tree flow is in section 11.4.

Set `PORTS` to the separate recipe checkout. In the container, you can clone it
or bind it at `/ports`:

```sh
export PORTS=/ports
```

**1. Fetch the sources.** In this flow, `pkg` fetches from a repository you
control, against hashes the recipes pin. First download every tarball the tree
pins and verify each SHA-256:

```sh
sh "$PORTS/fetch-distfiles.sh" /tmp/dist
ls /tmp/dist | wc -l        # count the currently pinned source files
```

The script is idempotent (verified files are kept) and retries transient
network errors, so re-running it is always safe.

**2. Build a signed repo and install the recipes.** `ports2repo` turns the tree
into a repo: every recipe gets a content-derived commit id, the distfiles are
copied in, and the index/manifest/root documents are signed. `install.sh` then
copies the recipes into the root:

```sh
/src/target/debug/examples/ports2repo /tmp/repo http://127.0.0.1:8784 \
    --distfiles /tmp/dist "$PORTS"
sh "$PORTS/install.sh" /tmp/repo
```

Two things to note here:

- The second argument is the **URL the repo will be served at**, not a
  directory — it is baked into the signed manifest as the download
  prefix for every source.
- `ports2repo` signs with deterministic **test keys**. That is fine for
  a localhost repo, but never use it for a repo you distribute.

**3. Serve, trust, sync.** Yakimo fetches only over http(s) —
`file://` mirrors are refused by design — so serve the repo locally,
point the configuration at it, trust its root key, and sync:

```sh
python3 -m http.server 8784 --directory /tmp/repo >/tmp/pkg-http.log 2>&1 &
sleep 1
curl -fsS http://127.0.0.1:8784/index.json >/dev/null
mkdir -p /etc/pkg
printf 'mirrors = [\n  "http://127.0.0.1:8784",\n]\n' > /etc/pkg/mirrors.toml
echo yes | pkg key trust-root /tmp/repo/root.json
pkg sync
```

Do not continue if the `curl` check fails: inspect `/tmp/pkg-http.log`
and confirm that `root.json` and `index.json` exist. This catches a server
startup race or a failed repository-generation step before `pkg sync`.

Why sync matters: the resolver reads the local recipe checkout, but
*building* fetches sources against the signed, cached recipe manifest —
and that cache is exactly what `pkg sync` creates.

**4. Install.** Package names on the command line are **bare names** —
`git`, not `dev-vcs/git`. The category directories exist only as
organization in the source tree; the installed recipe layout is flat.

```sh
# optional but recommended: let builds use all cores
printf '[build]\njobs = %s\nmax_pkg_jobs = %s\n' "$(nproc)" "$(nproc)" \
    > /etc/pkg/config.toml

pkg install --dry-run git   # preview the plan first
pkg install git             # builds the whole chain from source
```

The first install compiles the entire dependency chain (glibc, openssl,
curl, …), so expect it to take a long time.

If something goes wrong, the three most common errors map to the stages
above:

| Error | Cause |
|---|---|
| `there is no version of <category>/<name> in <version>` | You used a category-qualified name, or copied the `ports/` tree into `/var/lib/pkg/recipes/` with its category directories. Use bare names, and install recipes via `install.sh` (stage 2). |
| `not found: no recipe for <name>` | `/var/lib/pkg/recipes/` has no recipe for that name — stage 2 was not run (note the directory is `recipes`, plural). |
| `no cached recipe manifest … run pkg sync first` | Stage 3 is incomplete: the repo is not served/trusted/synced. |

Two things to know about how the image is put together:

- **`/src` is a snapshot.** It contains the sources as they were when
  you built the image, plus the compiled workspace and the cargo
  registry cache (so builds inside work offline). After changing code
  on the host, rebuild the image to test the change. To iterate on a
  live checkout instead, bind-mount it at a *different* path and use a
  fresh target dir, e.g. `-v "$PWD":/work` then
  `CARGO_TARGET_DIR=/tmp/target cargo build` inside `/work`.
- **Git is included for recipe work.** Both images have `git` and an
  SSH client, with a placeholder identity ("Yakimo Tester") so
  `git commit` works immediately — set your own name/email before
  authoring for real. To *push*, mount your credentials at run time:

  ```sh
  docker run --rm -it \
      -v "$HOME/.gitconfig:/root/.gitconfig:ro" \
      -v "$HOME/.ssh:/root/.ssh:ro" \
      yakimo-test
  ```

  (Or forward your SSH agent instead of mounting keys:
  `-v "$SSH_AUTH_SOCK:/ssh-agent" -e SSH_AUTH_SOCK=/ssh-agent`.) The
  mounted-repo case is covered too: `safe.directory=*` is preset so
  root in the container can work in a checkout owned by your host user.
