# Yakimo User Guide

Yakimo is a source-based package manager. It builds every package from
a recipe on your machine, keeps every build in a content-addressed
store, and can undo any transaction.

This guide uses short sentences and plain words on purpose. Each
section says what a mechanism does, how to use it, and — when the
mechanism is not obvious — why it exists.

## Contents

1. The five ideas you need
2. Setup: mirrors, trust, sync
3. Everyday commands
4. Build options (the "USE flags")
5. Configuration files
6. Controlling parallelism
7. Undo, generations, garbage collection
8. Security and verification
9. Named environments
10. Testing safely with `PKG_ROOT`
11. Making your own packages
12. Exit codes and troubleshooting

---

## 1. The five ideas you need

**The store.** A package never installs files directly into `/usr`.
A build produces a tree under `/usr/lib/pkg/.store/`, one directory
per *identity*. The visible system is a tree of relative symlinks that
point into the store. Example:

```
/usr/bin/lua -> ../lib/pkg/.store/lua-5.4.7+ab1463413912/usr/bin/lua
```

The links are relative. You can copy the whole tree, chroot into it,
or boot it as `/`. Absolute links would break in all three cases.

**Identity.** A package is not just a name and a version. Its identity
also carries an ABI tag (a hash of what it links against) and an
option tag (a hash of the build options you chose, when they differ
from the defaults). Two builds of the same version with different
options are different identities. They can both exist in the store at
the same time. This is why undo and option flips are safe.

**Recipes.** Yakimo builds from recipes, not binary packages. A recipe
is a small directory: a manifest (`pkg-meta.toml`), a pinned source
list (`source.toml`), and shell scripts for the build phases. Chapter
11 covers the format.

**Generations and undo.** Every transaction records a *generation*:
the exact set of identities that were live before and after. Undo
re-applies an older set. There is no separate undo log to go stale —
the generation table fully determines the symlink tree.

**The sandbox and the gate.** Builds run in a bubblewrap sandbox: no
network, the only writable places are the build scratch and the output
directory. After the build, the scanner checks every produced binary:
it must link only libraries that the recipe declared as dependencies,
and it must not link a libc newer than the one running. A build that
breaks these rules is rejected, not installed. This is what keeps
"it worked on my machine" out of the package set.

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

The first mirror is canonical. Later mirrors are fallbacks for
transient failures only. Rollback protection (an older index is
rejected) is pinned to the first mirror.

### 2.2 Trust bootstrap

Every repository index is signed. Before the first sync you must tell
Yakimo which root key set to trust:

```
# pkg key trust-root /path/to/root.json
```

This is the one step that can never be fully automatic: trust has to
enter the system from outside it. Get `root.json` from the project out
of band. The command shows you the fingerprints and asks before it
writes anything.

`pkg key list` shows the current trust state. `pkg key show-chain`
shows the full delegation chain (root → repository → timestamp roles).

### 2.3 Sync

```
# pkg sync            # fetch and verify the index
# pkg sync --status   # cache age and freshness, no network
# pkg sync --force    # ignore the cache TTL
```

Sync downloads the signed index and the recipe manifest. Builds only
use the synced manifest — not the raw recipe files — so what you build
is always what the repository signed.

---

## 3. Everyday commands

```
# pkg search hello            # search the index
# pkg info hello              # identity, options in use, provenance
# pkg install hello           # build + install (asks first)
# pkg install -y hello        # no questions
# pkg install hello@2.1       # an exact version
# pkg upgrade                 # upgrade everything installed explicitly
# pkg upgrade --dry-run       # resolve and preview, then stop
# pkg remove hello            # refuses if something still needs it
# pkg remove --recursive foo  # also remove the reverse dependents
# pkg autoremove              # drop dependency-only packages nobody needs
# pkg why hello               # the dependency chain that pulled it in
# pkg explain 'foo>=2'        # show the resolver's work for a constraint
```

`pkg info` shows the installed identity including the option tag, the
option values that differ from the recipe defaults, and the provenance
record (which recipe commit and index version the build came from).

If a package fails to build, the failed subtree is marked and the rest
of the transaction continues. The build is checkpointed: run the same
command again and already-built packages are not rebuilt.

### Pins

A pin freezes a package:

```
# pkg pin ffmpeg 6.1                  # stay on exactly this build
# pkg pin ffmpeg 6.1 --mode abi-track # follow ABI-compatible updates only
# pkg pin ffmpeg                      # show the current pin
# pkg unpin ffmpeg
```

Use a pin when an upgrade would break something you care about.
`abi-track` is the middle ground: you get fixes, but not a soname bump
that would cascade rebuilds through your system.

### Alternatives and profiles

```
# pkg alternatives list
# pkg alternatives set virtual/awk gawk
# pkg rebuild --affected              # rebuild what a provider switch invalidated

# pkg profile list
# pkg profile apply <name>            # apply a named system profile
```

A *virtual* is a name several packages can provide (`virtual/awk` →
gawk, mawk, …). `pkg alternatives` picks the provider. A *profile* is
a named set of constraints the distribution ships (e.g. a hardened
profile); applying one constrains what the resolver may pick.

### Cascades

When a library's link interface changes, every package built against
it must be rebuilt. That set is a *cascade*. Cascades are planned at
resolve time and can be deferred to a quiet hour:

```
# pkg cascade --at 03:00        # schedule the pending cascade
# pkg cascade --execute-pending # run it now (what the timer calls)
```

---

## 4. Build options (the "USE flags")

Options are Yakimo's answer to Gentoo's USE flags. The design rule is
that options are **declared and additive**: a recipe declares exactly
which knobs exist, and an option can only *add* things (a dependency,
a configure flag). An option can never silently remove or rewrite
another part of the package. This keeps "what will I get?" answerable.

### 4.1 Seeing them

Options are meant to be visible, not buried in recipe files:

```
# pkg options sqlite
sqlite 3.45.3
  fts5 = true   (default: true)
      Full-text search (FTS4/FTS5) support

# pkg options               # your whole standing configuration,
                            # including the file each value came from
```

`pkg info <name>` complements this for installed packages: it shows
which option values the installed build actually used.

### 4.2 Setting them once

Per command, either form works:

```
# pkg install ffmpeg --opt x11=0 --opt pulseaudio=1
# pkg install 'ffmpeg[opt:x11=0]'
```

A per-command choice is never saved. It applies to this transaction
only.

### 4.3 Setting them standing

`/etc/pkg/options.toml`:

```toml
# Applies to every package that declares the key.
[defaults]
nls = false

# Applies to one package.
[package.ffmpeg]
x11 = true
pulseaudio = false
```

Precedence, strongest first: per-command flags → `[package.<name>]` →
`[defaults]` → the recipe's declared default.

### 4.4 Splitting the config into several files

If your option set grows large, you do not have to keep it in one
file. The main file can pull in siblings:

```toml
# /etc/pkg/options.toml
include = ["sound.toml", "desktop.toml"]
```

The names are plain file names in the same directory — no absolute
paths, no `..`, no subdirectories. Included files use the same
`[defaults]` / `[package.<name>]` tables. An included file cannot
include further files: one level, so the merge order stays obvious.
Includes apply in the order listed; the main file is strongest.
`pkg options` shows which file each value came from.

Why the strictness? A silently ignored option is the config-file
version of a silent misconfiguration. Unknown keys, non-boolean
values, misspelled package names, and missing include files are all
hard errors that name the file.

### 4.5 What an option flip does

Flipping an option produces a *new identity* (the option tag changes),
so the package is rebuilt and the old build stays in the store. Undo
can return to it. Options marked `abi_affecting = false` by the recipe
(documentation, pure runtime data) still get a new identity but do not
force dependents to rebuild.

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
[build]
jobs = 12
max_pkg_jobs = 6

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

---

## 7. Undo, generations, garbage collection

```
# pkg history            # list transactions
# pkg history 7          # what generation 7 changed
# pkg undo               # undo the most recent transaction
# pkg undo 4             # undo transaction 4
# pkg undo --rebuild-missing   # rebuild pruned store dirs instead of failing
```

Undo is strictly sequential. You can only undo the latest transaction,
then the one before it. The package set — not a diff — is restored, so
undo cannot half-apply.

Undo restores the package set and the symlink tree. It does not touch
`/etc`, and it does not roll back a staged glibc swap (that has its
own rollback path; undo says so when it applies).

```
# pkg clean              # prune store dirs nothing references
# pkg clean --garbage    # ALSO release the generation holds (undo data)
# pkg clean --cache      # reclaim the artifact cache
```

`pkg clean` is always safe for undo: store dirs pinned by a restorable
generation are kept. `pkg clean --garbage` releases those holds — it
prints exactly which generations stop being restorable before it does
anything. After that, undoing them needs `--rebuild-missing`, which
rebuilds the missing store dirs from source.

Why generations exist at all: the store makes old builds cheap to
keep, and generations make them *addressable*. "Go back to how the
system was on Tuesday" is a data lookup, not a reinstall.

---

## 8. Security and verification

```
# pkg audit                       # advisories affecting installed packages
# pkg audit --supply-chain        # attestations, provenance, reproducibility
# pkg audit --supply-chain --critical
# pkg audit --glibc               # processes still running a deleted glibc
# pkg audit --acknowledge <pkg>   # accept a finding after you looked at it
# pkg check                       # link health across the system
# pkg check firefox --depth deep  # deeper scan of one package
# pkg check ./some-binary --suggest   # which installed package provides
                                     # each unresolved library
# pkg db check                    # database ↔ store integrity
# pkg db regenerate               # rebuild the db from the store
```

Two notes on why these exist:

- **The supply-chain gate.** An unacknowledged hard supply-chain
  finding blocks new transactions. This is deliberate friction: the
  point is that you cannot *not notice*. `pkg audit --acknowledge`
  unblocks once you have actually read the finding.
- **`pkg check` vs the build gate.** The gate runs at build time (new
  artifacts). `pkg check` runs against what is installed now — useful
  after manual changes, restores from backup, or suspicion of
  corruption.

---

## 9. Named environments

Environments are package sets you can activate into your shell without
touching the system — a project toolchain, a media box, a CI
sandbox:

```
# pkg env create media
# pkg env create ci --from manifest.toml
# eval "$(pkg env activate media)"    # PATH/MANPATH/PKG_CONFIG_PATH prepended
# pkg env show media                  # resolved identities + option deltas
# pkg env update media
# pkg env export media                # a shareable manifest + identity lock
# pkg env delete media
```

An exported environment pins exact identities, so a teammate (or next
month's you) gets the same set, not "whatever the index has today".

---

## 10. Testing safely with `PKG_ROOT`

Every state path Yakimo uses is prefixed by `PKG_ROOT` when set. You
can exercise the full toolchain against a throwaway root without
touching `/`:

```
$ export PKG_ROOT=/tmp/yakimo-test
$ mkdir -p $PKG_ROOT/etc/pkg
$ $EDITOR $PKG_ROOT/etc/pkg/mirrors.toml     # point at a repo
$ pkg key trust-root root.json               # trust bootstrap (in-root)
$ pkg sync && pkg install hello
```

### 10.1 The seed ports tree

The `ports/` directory in the Yakimo source tree is a small
Gentoo-ports-style recipe collection meant for live testing: the C
library and its prerequisites (linux-headers, glibc, libxcrypt — all
built from source), libraries
(zlib, bzip2, ncurses, readline), tools (sqlite, lua, less, tar, gzip,
xz, grep, sed, gawk), documentation (man-pages), a game
(tty-solitaire), a vendor binary (discord), the chrootable core
(bash, coreutils), and an init system (sysvinit). `ports/README.md`
has the details; `e2e/ports.sh` automates the whole flow.

Activation links are relative, so a `PKG_ROOT`-installed tree is
directly chrootable. On live installation media the flow is:

```
# mount the target partition at /mnt/target
$ export PKG_ROOT=/mnt/target
$ sh ports/install.sh /path/to/repo && pkg sync
$ pkg install bash coreutils grep sed gawk tar gzip xz less lua sysvinit
# mount /proc /sys /dev (arch-chroot does this for you), then:
$ chroot /mnt/target /bin/sh
```

Testing — more installs, option flips, service experiments —
continues inside the chroot. The e2e suite proves this works by
booting the installed sysvinit as PID 1 in a container and running an
inittab action through the tree's own `/bin/sh`.

### 10.2 Keeping versions current

Recipes always pin an exact version and a source hash — that pin is
what makes builds reproducible and signable. So there is no "install
the latest" pointer a client follows. Instead, the *bump* is
automated on the repository side:

```
$ ports/watch.sh           # which recipes are behind upstream?
$ ports/watch.sh --apply   # bump version + hash in place
```

A recipe carries a `watch.toml` describing how to poll its upstream
(git tags or a release-page pattern, with a stable-only filter).
`--apply` downloads the new distfile, recomputes the hash, and rewrites
the recipe. The bump then goes through review and signing like any
other tree change. (This is the same role Debian's `uscan` plays.)

---

## 11. Making your own packages

A recipe is a directory with up to six files. Two are required
(`pkg-meta.toml`, `build`, `install` — three, strictly):

```
myrepo/app-misc/hello/
  pkg-meta.toml    # what it is, what it needs   (required)
  source.toml      # where the source comes from  (required)
  build            # how to compile it            (required)
  install          # how to fill $PKGDIR          (required)
  prepare          # patches, pre-configure fixups (optional)
  check            # run the test suite           (optional)
  watch.toml       # how to poll upstream         (optional)
```

### 11.1 pkg-meta.toml

```toml
name = "hello"
version = "2.12"
category = "app-misc"

[dependencies]
link = ["ncurses"]        # libraries this package links against
runtime = ["glibc"]       # needed to run, but not linked
build = ["perl"]          # needed only while building

# Options are declared, boolean, and additive-only.
[options.readline]
default = false
description = "Interactive line editing via readline"
abi_affecting = true            # default; set false for docs/data knobs
add_deps = ["readline"]         # extra deps when the option is on
add_build_deps = ["help2man"]   # extra build-time deps when on
```

Dependency entries accept version constraints (`zlib>=1.3`, `foo=2.0`),
OR-groups (`cronie|fcron`), and virtuals (`virtual/awk`). An entry in
`link` or `runtime` is what the post-build gate checks against: if
your binary links a library you did not declare, the build fails and
the error tells you which installed package provides that library.

### 11.2 source.toml

```toml
[[source]]
url = "https://ftp.gnu.org/gnu/hello/hello-2.12.tar.gz"
sha256 = "31e066137a962676e89f69d1b65382de95a7ef7d914b8cb956f41ea72e0f516b"
# filename = "..."   # optional: stage the distfile under a different
                     # name (useful for ambiguous GitHub archive names)
```

The hash is required and always verified. Sources are fetched only
through the signed recipe manifest — a recipe cannot pull from a URL
the repository did not pin.

### 11.3 The phase scripts

`prepare`, `build`, `check`, and `install` are plain `sh` scripts,
run with `-e` (any failing command fails the phase), in order, inside
the sandbox. A failed `check` fails the build — do not declare one you
intend to ignore.

What a script sees:

| Variable / path | Meaning |
|---|---|
| `$PKGDIR` | the output tree. Everything `install` puts here becomes the package. |
| `PKG_OPT_<NAME>` | `0` or `1` for every declared option (e.g. `$PKG_OPT_READLINE`). |
| `MAKEFLAGS` | already set to the granted `-jN`; just call `make`. |
| `/pkg/deps/<name>` | each resolved dependency, at a stable path: headers in `/pkg/deps/foo/usr/include`, libraries in `/pkg/deps/foo/usr/lib`. |
| `SOURCE_DATE_EPOCH` | pinned, for reproducible builds. |
| `CC`, `CXX` | point at the compiler cache wrapper when enabled. |

The sandbox has **no network**. Fetching happens earlier, from
`source.toml`, verified against the hash. The only writable
directories are the build scratch (your working directory) and
`$PKGDIR`.

A typical pair:

```sh
# build
tar xzf hello-2.12.tar.gz
cd hello-2.12
./configure --prefix=/usr
make

# install
make -C hello-2.12 DESTDIR="$PKGDIR" install
```

A real option-aware example (lua in `ports/`):

```sh
if [ "$PKG_OPT_READLINE" = "1" ]; then
    make linux-readline \
        MYCFLAGS="-I/pkg/deps/readline/usr/include" \
        MYLIBS="-L/pkg/deps/readline/usr/lib -lreadline ..."
else
    make linux
fi
```

Binary redistributables (Discord, Steam) declare `[build] binary =
true` in the manifest: the `build` script becomes optional, the
`install` script unpacks the vendor tarball into `$PKGDIR`, and the
gate relaxes its undeclared-library check to a warning (the blob
legitimately links host libraries it does not ship).

### 11.4 The packager's loop

```
$ ports2repo /tmp/repo http://127.0.0.1:8765 --distfiles /tmp/dist myrepo/
$ python3 -m http.server 8765 --directory /tmp/repo &
$ export PKG_ROOT=/tmp/test-root
$ pkg sync && pkg build hello
```

Then iterate. If the build links something undeclared, the failure
says exactly what to add:

```
FAILED: hello-1: scanner gate rejected build: .../hello: links against
undeclared dependencies: libz.so.1 (not provided by this package or
any declared dep)
  hint: libz.so.1 — provided by installed package zlib (add it to [dependencies])
```

Add the dependency, run `pkg build hello` again. `pkg build` forces a
rebuild even when the version did not change — it is the recipe-author
command. (Note: a rebuilt recipe replaces its store tree and re-syncs
the installed symlinks immediately, so added or removed files show up
in the live tree at once.)

When the recipe is ready, `ports/watch.sh` keeps its version current;
`ports/README.md` covers turning a whole tree into a signed repo.

---

## 12. Exit codes and troubleshooting

| Code | Meaning |
|---|---|
| 0 | success |
| 1 | resolution failure (`pkg explain <constraint>` for detail) |
| 2 | build failure (progress is checkpointed — re-running resumes) |
| 3 | gate refusal (security, supply-chain, glibc staging) |
| 70 | anything else |

Common messages:

- **`unknown option in selection: 'x11'`** — no package in this
  transaction declares that option. Check spelling with
  `pkg options <name>`.
- **`option conflict on ffmpeg.x11: ...`** — two per-command requests
  disagree; pick one.
- **`options.toml: [package.foo] names undeclared option 'bar'`** —
  the config drifted from the recipe. Fix or remove the entry. The
  message names the file (including included files) it came from.
- **`<pkg> requires glibc X, but Y is staged-pending`** — reboot to
  apply the pending glibc, or install an older package version.
- **`no longer restorable`** on undo — the generation's store dirs were
  released by `clean --garbage`. Use `pkg undo --rebuild-missing` to
  rebuild them from source.
- **supply-chain gate** — read `pkg audit --supply-chain`; acknowledge
  with `pkg audit --acknowledge <pkg>` only after you have looked.
