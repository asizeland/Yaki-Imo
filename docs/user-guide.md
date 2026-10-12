# Yakimo User Guide (draft)

Yakimo is a source-first Linux package manager. Its command-line tool is
called `pkg` (the `pkg-*` crate and command names are unchanged; "Yakimo"
is the project's public name).

This guide is written for people testing Yakimo on a real machine. It
covers day-to-day use, configuration, and the safety machinery you will
want to understand before trusting it with a system you care about.

> **Status: pre-bootstrap draft.** Yakimo has an extensive unit and
> end-to-end test suite (see `e2e/`), but it has not yet bootstrapped a
> live distribution. Test on a machine — or a `PKG_ROOT` sandbox — whose
> loss you can afford. See *Testing safely* below.

---

## 1. Core concepts in five minutes

**Source builds, always.** Installing a package means fetching its
declared sources, verifying them against the signed repository index, and
building them in a sandbox. There is no binary package channel; there is
a local *artifact cache* so identical rebuilds are free.

**Identity is content-addressed.** A build is identified by
`name-version+abitag[+popttag]`. The ABI tag changes when a package's
linked dependencies' interfaces change; the option tag appears when you
build with non-default options (§6). Every distinct identity gets its own
directory under `/usr/lib/pkg/.store/`; the live filesystem is symlinks
into those directories.

**Generations and undo.** Every transaction records a *generation* — the
full package set before and after. `pkg undo` restores the previous
generation, strictly sequentially. Store directories belonging to recent
generations are pinned, so undo is instant; `pkg clean --garbage` is how
you deliberately release that safety net (§7).

**Signed repositories.** The package index is TUF-style signed
(root → repository → timestamp). You bootstrap trust once with
`pkg key trust-root`; tampering with the index or sources is rejected,
not warned about.

**glibc is special.** A glibc security patch is applied inline; a major
glibc bump is staged and applied at boot. Packages requiring a newer
glibc than the running one are rejected *before* a multi-hour build,
not after.

---

## 2. Setup

### 2.1 Mirrors

`/etc/pkg/mirrors.toml` lists repository mirrors, tried in order:

```toml
mirrors = [
  "https://repo.example.com/pkg",
  "https://mirror2.example.com/pkg",
]
```

### 2.2 Trust bootstrap

Install the repository's root trust anchor once (interactive
confirmation required — this is the root of all package authenticity):

```
# pkg key trust-root /path/to/root.json
```

Inspect the chain later with `pkg key list` and `pkg key show-chain`.

### 2.3 Sync

```
# pkg sync            # fetch + verify the index
# pkg sync --status   # cache age and freshness, no network
# pkg sync --force    # bypass the cache TTL
```

---

## 3. Everyday use

```
# pkg search hello            # search the index
# pkg info hello              # version, ABI/option tags, deps, files,
                              # option deltas from recipe defaults
# pkg install hello           # build + install (asks for confirmation)
# pkg install -y hello        # skip confirmations
# pkg install hello@2.1       # a specific version
# pkg upgrade                 # upgrade every explicitly installed package
# pkg upgrade --dry-run       # full resolve + cascade preview, then stop
# pkg remove hello            # refuses if other packages need it
# pkg remove --recursive foo  # also remove its reverse dependents
# pkg autoremove              # drop dependency-only packages nothing needs
# pkg why hello               # dependency chain explaining its presence
```

Useful transaction flags (`install`/`upgrade`):

| Flag | Effect |
|---|---|
| `--dry-run` | Resolve and preview the whole cascade — counts, ETA from *your* build history, largest packages — then stop. |
| `--jobs N` | Cap total compile parallelism for this run (§5). |
| `--opt name=0\|1` | Override a declared build option (§6). |
| `--skip-rebuilds` | Activate direct updates, defer the rebuild cascade (refused for security-relevant cascades unless you also pass `--defer-security=I-understand`). |
| `--background` | Detach the build phase; resume later with `pkg cascade --execute-pending`. |
| `--no-repro-check` | Skip the double-build reproducibility check for critical packages (on by default because safe is the default). |
| `-y` / `--assume-yes` | Non-interactive. |

Large cascades can be scheduled for the night instead of run under your
fingers:

```
# pkg cascade --at 03:00        # schedule the pending cascade
# pkg cascade --execute-pending # run it now (what the timer calls)
```

### Pins

```
# pkg pin ffmpeg 6.1                  # exact build
# pkg pin ffmpeg 6.1 --mode abi-track # track ABI-compatible updates only
# pkg pin ffmpeg                      # show the current pin
# pkg unpin ffmpeg
```

### Alternatives and profiles

Packages can depend on *virtual* names (`virtual/kernel`, `cron`); you
choose the preferred provider:

```
# pkg alternatives list
# pkg alternatives set virtual/awk gawk
# pkg rebuild --affected              # rebuild what a provider switch invalidated
```

Repositories can ship *resolution profiles* (curated constraint sets):

```
# pkg profile list
# pkg profile apply <name>
```

---

## 4. Configuration reference

All system configuration lives under `/etc/pkg/`. Every file is TOML.
Yakimo's rule for configuration is **loud, never silent**: an unknown
key, a wrong type, or an option a package doesn't declare is a hard
error — you will never have a setting quietly ignored.

### 4.1 `config.toml`

```toml
# Compiler cache: wrap CC/CXX with ccache or sccache ("off" or absent
# disables). Rebuilds of unchanged source become near-instant.
[cache]
compiler = "ccache"

# Build parallelism.
#   jobs         — total compile jobs across the whole machine
#                  (the scheduler token pool). Default: your core count.
#   max_pkg_jobs — per-package `make -j` cap. Default: 4.
# `pkg install --jobs N` overrides `jobs` for one invocation.
[build]
jobs = 8
max_pkg_jobs = 4

# Relocate state paths from their FHS defaults. Keys are the default
# absolute paths; values must be absolute.
[paths]
"/var/lib/pkg/cache" = "/bigdisk/pkg-cache"
```

### 4.2 `options.toml` — standing build-option selections

Where `--opt` applies to one transaction, `options.toml` is the
system's standing selection (§6 explains what options are):

```toml
# Applies to every package that declares the key.
[defaults]
docs = false

# Applies to one package.
[package.ffmpeg]
x11 = false
pulseaudio = true
```

Precedence, strongest first:

1. `--opt` / `name[opt:key=val]` on the command line (never persisted)
2. `[package.<name>]` entries
3. `[defaults]`

An entry naming an option the recipe doesn't declare is a hard error on
your next transaction — by design, so a typo can't silently change what
gets built.

---

## 5. Controlling parallelism

Yakimo schedules builds as a DAG with a *token pool*: the pool size is
how many compile jobs run at once machine-wide, and each package's
internal `make -j` is throttled between 1 and `max_pkg_jobs` depending
on phase (configure is serial) and load.

- **Persistently:** `[build] jobs` and `[build] max_pkg_jobs` in
  `config.toml` (above).
- **Per command:** `pkg install --jobs 4 firefox` — also respected by
  `--dry-run` ETAs.

Turning `jobs` down trades wall time for a responsive machine; on a
gaming box, `jobs = <cores - 2>` keeps a compile in the background from
stuttering your game.

---

## 6. Build options (declared, bounded customization)

Recipes declare their options in `[options]` tables — there are no
arbitrary `CFLAGS` or configure-flag escapes, by design. Each option is
a named boolean with a default, may *add* dependencies when enabled
(never remove), and is exported to the build as `PKG_OPT_<NAME>=0|1`.

Selecting non-default options changes the package's identity: the store
directory gains a `+p<hash>` suffix, so `ffmpeg` built with `x11=0` is a
different build from the default and shows as such in `pkg info`:

```
ffmpeg 6.1+ab3+p9f2c1d
  Options (deltas from recipe default):
    x11        = false   (default: true)   [abi-affecting]
    pulseaudio = true    (default: false)
```

Ways to select:

```
# pkg install ffmpeg --opt x11=0 --opt pulseaudio=1   # per command
# pkg install 'ffmpeg[opt:x11=0]'                     # same, spec syntax
# /etc/pkg/options.toml                               # standing (§4.2)
```

Semantics worth knowing:

- **One variant per package system-wide.** Two builds of the same
  package can't coexist in the live tree (they'd own the same files),
  so a selection change *rebuilds and replaces*.
- **Widen, else conflict.** Requests that can merge, merge (enabling is
  always safe). Two per-command requests assigning the same option
  differently fail and name both requesters — fix the conflict, don't
  let the PM guess.
- **`abi_affecting`.** Options default to ABI-affecting: toggling them
  cascades rebuilds of dependents, exactly like a version bump. Options
  declared `abi_affecting = false` (docs, extra logging) still get a
  distinct identity but cascade nothing.
- **Reinstalling the same version with different options is a real
  rebuild**, not a no-op — identity, not version, decides.

---

## 7. Undo, generations, and garbage collection

```
# pkg history            # list transactions
# pkg history 7          # what generation 7 changed
# pkg undo               # restore the pre-transaction state of the most
                         # recent restorable transaction
# pkg undo 4             # undo transaction 4 — strictly sequential:
                         # you can only undo the latest, then the one
                         # before, and so on
# pkg undo --rebuild-missing   # rebuild pruned store dirs instead of
                               # failing (turns instant undo into a
                               # source-build session)
```

`pkg undo --package foo` is deliberately **not supported** (it errors
loudly). Single-package downgrades are `pkg install foo=<old-version>`
plus `pkg pin`.

Store cleanup:

```
# pkg clean              # prune store dirs nothing references —
                         # generations keep theirs pinned, so undo
                         # survives a plain clean
# pkg clean --garbage    # ALSO release the generation pins; warns which
                         # generations become non-restorable
# pkg clean --cache      # reclaim the artifact cache
```

After `clean --garbage`, undoing into a pruned generation fails honestly
("no longer restorable") and leaves the live system untouched.

---

## 8. Security and verification

```
# pkg audit                       # advisories affecting installed packages
# pkg audit --supply-chain        # attestations, provenance, reproducibility
# pkg audit --supply-chain --critical
# pkg audit --glibc               # processes holding deleted glibc inodes
# pkg audit --acknowledge <pkg>   # accept a supply-chain finding
# pkg check                       # scanner diagnostics (link health)
# pkg check firefox --depth deep    # deeper scan of one package
# pkg db check                    # database ↔ store integrity
# pkg db regenerate               # rebuild the db from the store
```

Enforcement highlights:

- Unacknowledged supply-chain hard findings **block** install/upgrade.
- Critical packages are built twice and compared (reproducibility);
  divergence fails the build.
- Sources are hash-checked at fetch; index rollbacks are rejected.
- After a glibc patch, `pkg apply-pending-restarts` restarts services
  still holding the old library's deleted inodes.

---

## 9. Named environments

Project-style environments that don't touch the system set:

```
# pkg env create media
# pkg env create ci --from manifest.toml
# eval "$(pkg env activate media)"    # PATH/MANPATH/PKG_CONFIG_PATH prepended
# pkg env show media                  # resolved identities + option deltas
# pkg env update media
# pkg env export media                # shareable manifest + identity lock
# pkg env delete media
```

---

## 10. Testing safely with `PKG_ROOT`

Every state path Yakimo uses is prefixed by `PKG_ROOT` when set, so you
can exercise the full toolchain against a throwaway root without
touching `/`:

```
$ export PKG_ROOT=/tmp/yakimo-test
$ mkdir -p $PKG_ROOT/etc/pkg
$ $EDITOR $PKG_ROOT/etc/pkg/mirrors.toml     # point at a repo
$ pkg key trust-root root.json               # trust bootstrap (in-root)
$ pkg sync && pkg install hello
```

This is exactly how the project's end-to-end suite runs (`e2e/run.sh`
and siblings fabricate a signed toy repo, build real packages in a
bubblewrap sandbox, and exercise install → upgrade → undo → GC). To see
a real system bootstrap, the missing piece is installation media and a
seed recipe set — that's the next milestone, and this guide is its
dress rehearsal.

---

## 11. Exit codes and troubleshooting

| Code | Meaning |
|---|---|
| 0 | success |
| 1 | resolution failure (`pkg explain <constraint>` for detail) |
| 2 | build failure (progress is checkpointed — re-running resumes) |
| 3 | gate refusal (security, supply-chain, glibc staging) |
| 70 | anything else |

Common messages:

- **`unknown option in selection: 'x11'`** — no package in this
  transaction declares that option. Check spelling with `pkg info`.
- **`option conflict on ffmpeg.x11: --opt requests x11=0, but
  ffmpeg[opt:x11=1] requests x11=1`** — two per-command requests
  disagree; pick one.
- **`[package.foo] names undeclared option 'bar'`** — `options.toml`
  drifted from the recipe; fix or remove the entry.
- **`<pkg> requires glibc X, but Y is staged-pending`** — reboot to
  apply the pending glibc, or choose an older package version.
- **`no longer restorable`** on undo — the generation's store dirs were
  released by `clean --garbage`. Use `pkg undo --rebuild-missing` to
  rebuild them from source instead.
- **supply-chain gate** — read `pkg audit --supply-chain`; acknowledge
  with `pkg audit --acknowledge <pkg>` only after you've actually looked.
