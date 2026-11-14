# Future items and accepted developer tooling

This document records implemented developer tooling and ideas that should wait
until after the first successful live-system boot. The debug shell and typed
hooks below are implemented and covered by dedicated end-to-end tests.

## Implemented: interactive debug shell

Package authors need a way to inspect a real build environment, reproduce a
failure, and test a fix without rebuilding every source file after each edit.

```sh
pkg debug-shell foobar
pkg debug-shell --phase compile foobar
pkg debug-shell --resume foobar
pkg debug-shell --export ./fixes --resume foobar
pkg debug-shell --remove-tree foobar
```

A debug shell uses the same declared dependencies, environment variables,
paths, source pins, and sandbox restrictions as an ordinary build. It has no
network access and cannot modify the live installed root. Build logs and
generated configuration remain available in its workspace.

`--phase` runs successful prerequisite phases and enters the shell before the
named phase. `compile` is the user-facing name for the internal build phase.
A failed phase leaves the workspace available for inspection and resume.

Workspaces persist after the shell exits. `--resume` neither wipes nor
refetches them. Only `--remove-tree` deliberately removes the workspace.
Resuming after package identity, options, dependencies, or recipe inputs have
changed must produce an error rather than silently using stale state.

Incremental compilation is permitted only inside this development workspace.
Interactive output must never enter the artifact cache, become an installed
package, or be published as a verified build. After finding a fix, the author
must run an ordinary pristine build to validate and package it.

Export produces reviewable unified diffs against the original source and the
original recipe. Source patches belong in the recipe's `files/` directory and
must be covered by the normal pinned-source and clean-build path. Export must
not create a separate patch package.

## Implemented: typed transaction hooks

Packages need shared maintenance operations after their files change. Examples
include rebuilding font, MIME, and icon caches and refreshing the dynamic
loader cache. Users, groups, service registration, initramfs generation, and
arbitrary maintainer commands are not part of the initial implementation.

Recipes declare operations, not shell commands:

```toml
[hooks]
operations = ["font-cache", "mime-cache", "desktop-database", "glib-schemas"]
```

Yakimo owns and reviews each operation's implementation. The operation set is
`ldconfig`, `font-cache`, `mime-cache`, `icon-cache`, `desktop-database`, and
`glib-schemas`. Operations are
idempotent, have a centrally defined order, and run once per transaction or
build plan even when several packages request them. Activation, removal,
autoremove, undo, and same-identity rebuilds must all reconcile the relevant
state.

Hooks run after the package-state commit, without network access. They see the
selected target root rather than the host root and receive only the filesystem
write access required by the operation. Standard output, standard error, and
failures are logged in a readable, persistent location.

A hook failure does **not** roll back an already activated package set or make
the database pretend that activation did not happen. The transaction command
reports the failure, Yakimo records the outstanding operation, and later
operations still run. These commands inspect and retry outstanding work:

```sh
pkg hooks status
pkg hooks retry
```

A successful retry clears the recorded failure and does not create a package
transaction generation. Undo reruns applicable hooks; it does not attempt to
reverse arbitrary changes made by hook programs.

## Deferred until after the first live-system boot: ABI checking

A maintainer tool should compare old and new libraries for removed or added
symbols, changed symbol versions, function signatures where debug information
permits, public structure layout, sonames, and ELF dependencies:

```sh
pkg abi-diff libfoobar-2.0 libfoobar-2.1
```

The argument order is always old, then new. The report should distinguish
probably breaking, probably additive, and uncertain changes. Added symbols do
not by themselves require rebuilds, and unchanged symbols do not prove
compatibility. Tests and upstream knowledge remain necessary.

The existing scanner's soname, symbol-version, and header fingerprints remain
the lightweight build-time mechanism. For deep analysis, evaluate existing
tools such as libabigail before implementing DWARF and type-layout analysis in
Yakimo. The tool may inform maintainer review and rebuild planning, but must
not automatically suppress a conservative rebuild.

## Deferred until after the first live-system boot: kernel packages

Different users need different kernel policies, but every additional variant
adds patch, configuration, module, and hardware-testing work. Start with one
reliably booting kernel. Then consider distinct, parallel-installable packages
for LTS and current stable kernels. Release candidates belong in an explicitly
experimental channel. Hardened and CachyOS variants require their own patchset
maintenance and should follow only when they can be supported and tested.

GRUB and Limine can already present entries for several Linux kernels. Each
entry must select the matching kernel image, initramfs, microcode, modules,
and arguments. Keep a known-good entry, protect the running kernel, and never
remove the last bootable kernel. Test every supported variant with the
installed DKMS modules on real hardware.

## Rejected: rename recipe metadata to `.pkg` and `.watch`

Keep the established layout:

```text
foobar/
├── pkg-meta.toml
├── watch.toml
└── files/
    └── 0001-fix-something.patch
```

`pkg-meta.toml` and `watch.toml` describe their purpose and format. `.pkg`
would suggest a package artifact, while `.watch` hides the TOML format.
Renaming would also require changes to every recipe, parser, repository tool,
and documentation example without adding capability. Recipe directories and
`files/` already provide the desired organization for patches.
