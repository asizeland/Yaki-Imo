# Yakimo Package Manager & Distribution Specification — v15 (Consolidated)

**Status:** Consolidated specification; all decisions settled
**Target:** Linux, amd64 (+ i686 multilib), glibc-based, init-system agnostic (sysvinit in the reference tree)
**Implementation Language:** Rust
**Character:** Gaming-capable generalist distribution
**Track:** Source-only (binary distribution deferred indefinitely — §10.2)

> **About this document.** This is v11 with the v12, v13, v14 and v15 deltas applied in place, so every
> section reads as it stands at v15. Where the merge required an editorial call (renumbering, fixing a
> cross-reference, placing a new table), it is listed in **Appendix A**. Section origin is marked in
> italics (*v12 Δ §n*, etc.) so any paragraph can be traced back to its source delta.
>
> v15 is the first revision written *against the running implementation*: its deltas record what the
> 66-recipe reference ports tree and its end-to-end harness (`e2e/ports.sh`) settled in practice.
>
> **Known gap.** v11 itself was a delta against v10, and v10 was not among the source files. Passages that v11
> marked "(Unchanged from v10)" are therefore kept as placeholders with that wording. They are listed in
> Appendix A.

---

## Revision History

### v11 changes (from v10) — and their status at v14

| Change | Section | Status at v14 |
|---|---|---|
| Trust model & key management | §10.5 | Retained; extended to four roles and recipe/source integrity (v12, v14) |
| Repository sync (`pkg sync`) | §10.6 | Retained; now a lock *writer* (v12) |
| Reverse-dependency protection | §9.2, §16.1, §16.6 | Retained; virtual-provider and migration rules added (v12) |
| Reproducible build verification (Tier 1/2) | §10.8 | **DEFERRED** (v12); critical set gets a local double-build instead (v14, §10.5.6) |
| DKMS / out-of-tree kernel modules | §4.2, §7.8 | **DEFERRED post-v1** (v12); design of record retained |
| Concurrency & locking | §9.4 | Retained; lock scope narrowed to critical section (v12) |
| Version retention policy | §10.7 | **DEFERRED** (v12) |
| Pin granularity | §5.6 | Retained; CLI syntax and semantics clarified (v12) |

### v12 changes (from v11)

Source-only track (binary repo, retention, reproducibility tiers deferred); recipe & source integrity via signed
recipe manifest; lock scope narrowed to the critical section; glibc copy-and-reboot fallback removed; DKMS
deferred with a canonical `[dkms]` table form; pin clarifications; autoremove under virtuals; enforce-restart gate
blocks installs *and* audits; ELF verification reframed as post-build verification.

### v13 changes (from v12) — glibc mechanism hardening

| # | Change | Lands in |
|---|--------|----------|
| 1 | glibc-apply interface freeze | §7.6.1 |
| 2 | Pending-flag protocol version | §7.6.2 |
| 3 | Initramfs drift detection | §7.6.3, §13.6 |
| 4 | Pending-window transaction rules | §7.6.4 |
| 5 | Stale staged-tree handling | §7.6.5 |
| 6 | Post-boot verification before recovery removal | §7.6.6 |
| 7 | Daemon restart integration | §3.7 |
| 8 | glibc abi_tag base case | §7.7 |
| 9 | Layout relocation procedure | §18.4 |

### v14 changes (from v13) — options, undo, environments, supply-chain trust, survivable cascades

| # | Change | Lands in |
|---|--------|----------|
| 1 | Declared, bounded build options (`[options]`, `opt_tag`) | §4.3, §10.3, §7.7, §20 |
| 2 | Transaction generations and undo | §14, §9.2, §12, §16.1 |
| 3 | Named environments (`pkg env`; old `pkg env` → `pkg build-env`) | §16.7 |
| 4 | Supply-chain trust model for source inputs | §10.5, §7.1, §16.4 |
| 5 | Survivable rebuild cascades | §7.7, §7.9, §9.2, §15, §16 |
| 6 | Schema v3 consolidation, CLI surface, terminology | §9.2, §16 |

### v15 changes (from v14) — the reference tree lands

Written against the running implementation: the seed ports tree (66 recipes, glibc through
NetworkManager plus the self-hosting sys-devel toolchain) and its end-to-end proof
(`e2e/ports.sh`) settled several points the earlier deltas left abstract.

| # | Change | Lands in |
|---|--------|----------|
| 1 | Build-sandbox environment contract settled: dep binds at `/pkg/deps/<name>`, full build-dep closure, dep-provided tools on PATH, compile-time search paths over bound deps | §7.1 |
| 2 | Post-build tree normalization: relocatable `.pc` rewrite, `.la` stripping, the Python-bytecode precompile convention | §7.10 (new) |
| 3 | Gate semantics clarified: undeclared linkage is a hard failure for source builds (exit 3); the report-and-record reframing applies to the glibc floor/ceiling check only | §8, §15, §16.1 |
| 4 | Parallelism configuration made concrete: `[build] jobs` / `max_pkg_jobs` | §7.9.1 |
| 5 | The reference ports tree and upstream version tracking (`watch.toml`) | §10.9 (new) |
| 6 | glibc is built from source in the reference bootstrap; the prebuilt-tarball exception is retained but no longer load-bearing | §18.2 |
| 7 | Reference-tree init is sysvinit (proven as PID 1); init-system agnosticism unchanged | header, §13 |
| 8 | Appendix-A rulings: A.4 (glibc `abi_tag` base case governs) and A.5 (exit 3 defined) | §7.7, §8, App. A |

---

## 1. Overview

(Unchanged from v10.)

---

## 2. Design Principles

(Unchanged from v10 — nine principles: explicit over implicit; sane defaults, explicit override; bounded
complexity; filesystem-agnostic core; static binary bootstrap; correctness over convenience; zero runtime
overhead; user autonomy, security accountability; honest atomicity.)

v12–v15 apply these repeatedly. The standing invariants every later mechanism must preserve: **honest
atomicity, statically enumerable dependency graphs, no daemons, source-only, fail-safe defaults.** *(v14 Δ preamble)*

---

## 3. Core Concepts

(§3.1–3.7 unchanged from v10 — package identity, runtime path model, tracks, system-anchor packages, glibc
stability policy, runtime overhead guarantee, glibc update categories.)

**Package identity (§3.1), as extended at v14:** a package's identity is the 5-tuple
`name-version+abitag+opttag` (§4.3). The all-default option selection has the empty `opt_tag`.

### 3.7 Glibc Update Categories — amendment: Daemon Restart Integration *(v13 Δ §7)*

§3.7 category-1's long-running-daemon caveat becomes a supported workflow:

- After a category-1 security patch, **`pkg audit --glibc`** lists the **specific PIDs and service names**
  holding deleted glibc inodes (existing inode-scan machinery — `/proc/*/maps` against unlinked `libc.so.6`
  inodes), and prints the **exact OpenRC restart commands** (`rc-service <name> restart`) for each affected
  service.
- New command: **`pkg apply-pending-restarts`** restarts the affected services **in dependency order**
  (OpenRC's own ordering), with PID→service mapping resolved via cgroups/`/proc`. Services without a mapping
  (user session processes, PID 1) are listed but not touched.

NSS version-mismatch hardening (glibc refusing to load NSS modules built against a mismatched internal ABI) is
recorded as an **upstream-watch item only** — tracked against upstream glibc development, **not built here**.

---

## 4. Package Metadata Format

### 4.1 Manifest Schema (`pkg-meta.toml`)

(Base schema unchanged from v10. Additions through v14:)

**`[options]` table (v14).** A recipe declares its entire option set here; see §4.3. Option fields include
`add_deps` and `add_build_deps` (additive-only dependency effects) and `abi_affecting` (default `true`).

**Dependency kinds (v14).** Manifest deps are `link` (default; feeds `abi_tag`), `runtime` (dlopen'd plugins,
exec'd helpers, data — excluded from the tag), or `build` (toolchain-only, never cascades). See §7.7.

**`critical = true` marker (v14).** Set on an explicit list of fewer than 50 packages (glibc, openssl,
bootstrap-path decompressors, openssh, the toolchain, `pkg` itself). See §10.5.6.

**`[build] compiler_cache = false` (v14).** Per-package opt-out of the compiler cache — a *recipe-author*
decision. See §7.9.6.

**Pinned `[source]` entries (v14).** cargo/npm/go inputs are declared as additional pinned `[source]`
entries; recipes that cannot express fetches as pinned sources are rejected at lint. See §7.1.

**`[dkms]` table — DEFERRED post-v1 (v12 Δ §5).** Retained as the design of record for when DKMS lands:

```toml
# Optional: marks this package as providing a DKMS-managed kernel module.
# The table's presence is the signal itself — no separate boolean flag
# alongside it, since a flag whose truth value has to stay consistent
# with fields next to it is its own hazard. This is also deliberately
# its own [dkms] table rather than flat top-level keys: TOML gives a
# table header its own scope, so [dkms] can appear before or after
# [dependencies] without either one silently absorbing the other's
# fields — a bare root-level key placed after a table header has no
# such protection, since TOML has no way to "return to root scope"
# once a header opens one.
[dkms]
name = "nvidia"       # name DKMS registers the module under
version = "550.78"
```

**Canonical manifest form, when implemented: presence of a `[dkms]` table — never flat keys, never a boolean.**

### 4.2 Field Semantics

(Existing entries for `category`, `multilib`, and OR-Groups unchanged from v10.)

**`[dkms]` table — DEFERRED post-v1.**
Its presence marks this package as providing a DKMS-managed out-of-tree kernel module. Both `name` and
`version` are required sub-fields when the table appears at all — TOML's own missing-field checking rejects a
`[dkms]` table lacking either, rather than the package manager validating that combination itself after
parsing. When implemented, the resolver adds an implicit dependency on `linux-headers` matched to *every
currently installed kernel* (not just the running one), and the package manager triggers DKMS rebuilds at
defined points rather than leaving this to the recipe. See §7.8.

**`[options]` table.** See §4.3.

### 4.3 Declared, Bounded Build Options *(v14 Δ §1)*

The §20 non-goal stands in spirit — the dependency graph never changes *shape*. What it never did was
eliminate customization demand; it pushed it into invisible recipe forks that collide with stock builds, miss
cascades, and get no CI coverage. Bounded options make variance declared, priced into identity, visible to
tooling.

**Manifest schema.** A recipe declares its entire option set in the `[options]` table; undeclared options do
not exist and cannot be invented at the command line. Parser-enforced rules, not lint:

1. **Additive-only dep effects.** An option may add deps (`add_deps`, `add_build_deps`) when enabled — never
   remove, downgrade, or re-constrain a base dep, never gate on another option's state. Every selection's dep
   set stays computable from the TOML alone.
2. **No option-logic matrices.** No `required`/`conflicts`/`implies`. Genuinely conflicting options mean the
   recipe is wrong; split it into two package names sharing a source.
3. **Base dep set is the floor.** `[dependencies]` is the all-options-off set; every selection is a superset,
   so disabling options never breaks a reverse dependent linked against the base surface.
4. **Observable in the recipe.** Options export as `PKG_OPT_<NAME>=0|1`; the recipe owns the configure
   plumbing. The §8 scanner is the honesty check: `add_deps` with no resulting soname linkage is flagged in CI.

**Identity.** A selection is canonicalized as sorted `name=value` pairs, hashed (truncated SHA-256, 8 hex
chars, `abi_tag` convention) into the **`opt_tag`**; store/DB identity becomes the 5-tuple
`name-version+abitag+opttag`. The all-default selection gets the **empty `opt_tag`** — default builds are
identity-identical to pre-v14 builds, so migration is a nil backfill.

**`abi_affecting` split.** Options default to `abi_affecting = true` and are hashed into the package's *own*
`abi_tag` before dependents derive from it: `x11=0` gtk is a different ABI from `x11=1` gtk and cascades
normally (§7.7). `abi_affecting = false` (docs, verbose logging) changes `opt_tag` only — the bounded-cost
escape valve.

**Resolver rule: widen, else conflict.** Options are system-global per package name; two variants cannot
coexist in one system set — both would own the same FHS symlinks, physically impossible, not merely
confusing. (1) If the union of requested effects is satisfiable as one assignment, the resolver **widens** —
enabling is always safe: base deps are the floor, the ABI surface only grows. (2) If assignments genuinely
conflict, it **fails at solve time** with a `pkg explain`-style report naming both requesters; option
requirements are PubGrub pseudo-dependencies `foo[opt:x11=1]`, so conflicts are ordinary unsatisfiable incompat
terms.

**Configuration precedence**, strongest first (pins' "explicit wins" philosophy): per-command `--opt
name=value` (transaction-scoped, not persisted) > per-package `/etc/pkg/options.toml` entries > profile
`[options]` sections (§10.3) > global `[defaults]`. Unknown option names, undeclared values, or options on
packages declaring none are **hard errors at parse time** — a silently ignored `x11 = false` is the
config-file twin of silent divergence. `pkg info` shows deltas from recipe default; `pkg options [name]`
shows the standing configuration or, for one package, every declared option's effective value, default,
description, and origin. **`options.toml` may split across files:** a top-level `include = ["sound.toml", …]`
pulls in plain-named sibling files (no absolute paths, no `..`, one level deep), merged in listed order with
the main file strongest; every value records its source file so origins stay auditable. **CI:** builds cover the
two extreme corners (all-default, all-on); middle points are uncovered, accepted, documented.

**Still forbidden (§20, amended wording):** arbitrary `CFLAGS`/`RUSTFLAGS` injection (toolchain flags are the
distribution's ABI contract; ISA variance has its sanctioned path in `glibc-hwcaps`) and arbitrary configure
args (anything reachable only by raw args is an undeclared option; declare it or fork visibly).

---

## 5. Dependency Resolution

(§5.1–5.5 unchanged from v10 — PubGrub algorithm, graph extensions, provider switching, resolution profiles,
`pkg why`/`pkg explain`. Extended at v14 by option pseudo-dependencies, §4.3.)

### 5.6 Pinning as a First-Class Feature

(Unchanged from v10 — pin precedence over auto-promotion and profile pins.)

#### 5.6.1 Pin Granularity *(v11, amended by v12 Δ §6)*

By default, `pkg pin <package> <version>` pins an **exact build** (`name-version+abi`). This is correct for
most packages — but for any package that distinguishes ABI-compatible patches from ABI-breaking bumps (§3.7
established this split for glibc specifically), an exact-build pin blocks security patches along with
everything else. That's the same problem §3.7 solved, reappearing one layer up, for every other package a user
might reasonably want to pin.

**Single CLI syntax:**
```bash
pkg pin <package> [<version>] [--mode=exact|abi-track]
pkg pin glibc 2.39                      # exact-build pin (default): blocks everything,
                                        # including security patches
pkg pin glibc 2.39 --mode=abi-track     # allows patch-level updates (2.39-r1, 2.39-r2, ...),
                                        # blocks major-version bumps (2.40)
pkg pin openssl 3.0 --mode=abi-track    # same pattern for any package using compatible versioning
```
The `2.39.*` glob-suffix form is dropped; mode belongs in an explicit flag, not a punctuation pun.

`pins.toml` carries a `mode` field and the pinned version's `abi_tag`:
```toml
[[pin]]
package = "glibc"
version = "2.39"
mode = "exact"       # "exact" | "abi-track"
# abi_tag is recorded here at pin time (v12 Δ §6)
```

**Semantics:**

- Pin entries **store the pinned version's `abi_tag` at pin time**, so the comparison key is recorded once
  rather than recomputed.
- `abi-track` mode instructs the resolver to accept any version that shares the pinned version's
  major/ABI-relevant prefix. For packages following the `<stable>-rN` patch-suffix convention (§3.7), this is
  the **primary mechanism** and is unambiguous. `abi_tag` comparison is a **fallback only** for versions not
  following the `-rN` convention: a new build sharing the pinned version's `abi_tag` is treated as within the
  track, since by construction an unchanged `abi_tag` means no reverse-dependent rebuild would be triggered
  either.
- `abi-track` **never permits downgrades**: candidate ≥ pinned, always.
- `abi_tag` is **verified post-build by the orchestrator**, since it isn't known at solve time — the resolver
  constrains by version set, the orchestrator confirms the tag after the build.
- Explicit `pkg install foo=x.y` **overrides a pin for that transaction only** (v8 behavior, restated — v11
  had silently lost it).
- `pkg undo` never modifies `pins.toml`, but re-checks pins after an undo and warns without blocking (§14.1).

`exact` remains the default. Defaulting to `abi-track` would be a silent behavior change with real security
implications — a user who pins expecting a hard freeze shouldn't discover updates were flowing through anyway.
`abi-track` is opt-in for users who specifically want "don't move me off this major version, but don't leave me
exposed to patched CVEs either."

Pins are stored in `/var/lib/pkg/config/pins.toml` and applied globally.

---

## 6. Filesystem Layout

(Base layout unchanged from v10. Paths added or fixed through v14:)

```
etc/pkg/
├── config.toml               # [cache], [defer], undo-hold-generations, enforce-restart, ...
├── mirrors.toml              # mirror URLs, transport-only (§10.6)
└── options.toml              # per-package option selections (§4.3)

usr/lib/pkg/
├── glibc-anchor/             # active glibc tree; swapped as a whole (§7.6)
├── .stage/
│   └── glibc-active/         # staged glibc (pending) / recovery copy (post-swap)
└── sandbox/
    └── build.profile         # explicit, versioned build sandbox profile (§7.1)

var/lib/pkg/
├── db/
├── cache/                    # personal artifact cache, §7.9.5
├── build/
│   └── quarantine/           # reproducibility-mismatch trees, §10.5.6
├── envs/<name>/              # named environments, §16.7
├── lock                      # flock-based coordination, see §9.4
│                             #   (+ sidecar holder-metadata file next to it)
├── glibc-pending             # pending-flag file (§7.6.2)
└── config/
    ├── pins.toml
    └── alternatives.toml
```

The three anchor symlinks (`/lib64`, `/lib`, `/usr/include`) are part of the glibc anchor layout and must be fixed up if `/usr/lib/pkg` is relocated (§18.4).
Recipe-repo provenance records live at `provenance/<name>/<version>.toml` (§10.5.6).

---

## 7. Build Orchestrator

(§7.1–7.7 base text unchanged from v10 — tracks, recipe format, hook sandboxing, network access, MESSAGE
support, `ldconfig` role, build environment, per-job scoped dependency tree, transaction staging and
activation, glibc special case, ABI tag derivation. Amendments through v14 follow, placed at the section they
amend.)

### 7.1 Build Sandbox — amendment: no network, mandatory vendoring *(v14 Δ §4)*

The bubblewrap build profile becomes explicit and versioned (`/usr/lib/pkg/sandbox/build.profile`):

- read-only dependency tree; read-write only the scratch dir and `$PKGDIR`;
- recipe materialized as a plain checkout (no `.git`);
- **empty network namespace for the build stage** — fetch is a separate, earlier sandbox restricted to the
  manifest's declared origins;
- cargo/npm/go inputs are declared as additional pinned `[source]` entries: vendoring is enforced at the
  manifest level; recipes that can't express fetches as pinned sources are rejected at lint;
- seccomp-BPF blocks (at minimum) `ptrace`, `mount`, `bpf`, `perf_event_open`, module loads,
  `io_uring_setup`, nested user namespaces;
- unprivileged UID, `no_new_privs`, cgroup v2 caps, per-phase timeouts.

**Environment contract *(v15 Δ §1)*** — what a phase script can rely on, settled by the reference tree:

- **Dependency binds.** Declared dependencies are RO-bound twice: at their real store paths (so baked-in
  RUNPATHs resolve) and at the stable alias `/pkg/deps/<name>` (so recipes reference
  `/pkg/deps/libfoo/usr/include` without globbing identity-hashed store paths). The bind set is the full
  *closure*: build-kind deps bring their own runtime dependencies along (a build tool's libraries must
  resolve), and link/runtime deps the same.
- **Tools from dependencies.** Every bound dep's `usr/bin` and `usr/sbin` is on `PATH`, *before* the seed
  toolchain — a dep-provided tool (pkgconf, python3, libtasn1's `asn1Parser`) is the one recipes are
  written against, and wins over the host's.
- **Runnable dep tools.** `LD_LIBRARY_PATH` spans every bound dep's `usr/lib`, because a dep-provided
  executable links its own libraries and the sandbox's `/usr/lib` is empty. This changes no link decisions
  — the sandbox has no competing libraries.
- **Compile-time discovery.** `C_INCLUDE_PATH`/`CPLUS_INCLUDE_PATH`/`LIBRARY_PATH`/`PKG_CONFIG_PATH` span
  the bound deps' include/lib/pkgconfig dirs, so a plain `#include` or `pkg-config` call finds declared
  deps without hand-assembled flags. The §8 gate verifies afterwards that whatever got linked was declared.
- **Deterministic anchors.** `HOME` is the scratch dir, `PKGDIR` the packaging destination, `MAKEFLAGS=-jN`
  from the token pool (§7.9.1), `SOURCE_DATE_EPOCH` pinned to a fixed constant, `LC_ALL=C`, and one
  `PKG_OPT_<NAME>=0|1` per declared option (§4.3).
- **Pristine scratch, always.** Every build starts from a fresh scratch dir — no incremental build-tree
  reuse (autotools caches bake in the previous build's answers). The sanctioned reuse mechanisms are the
  artifact cache (§7.9.5) and the compiler cache (§7.9.6), both content-keyed.

Two consequences recipe authors meet early, both deliberate: anything a build needs must be a *declared*
dep (undeclared linkage fails the §8 gate), and nothing may rely on `/usr/include` or `/usr/lib` existing in
the sandbox — they are empty by construction.

### 7.6 Glibc Special Case — amendments

*Base text (categories, anchor/staging model, `RENAME_EXCHANGE` swap) is unchanged from v10; the following
amendments, from v12 and v13, harden it end-to-end: the boot-time apply path, the pending window, drift
detection and post-boot verification.*

#### 7.6.0 No copy-and-reboot fallback *(v12 Δ §4)*

v11 contradicted itself: §21's "honest atomicity" row claimed no partial-swap state is reachable, while the
next row documented exactly such a fallback. **v8 semantics are restored: no copy-and-reboot fallback exists.**
Bootstrap and glibc major upgrades **refuse** if same-filesystem sibling staging is unavailable. The
`--allow-unsafe-copy` flag is removed. An opt-in flag whose failure mode is an unbootable system is not user
autonomy; it's a loaded foot-gun with a CLI spelling. The supported fix for a non-conforming layout is
documented in §18.4.

#### 7.6.1 glibc-apply interface freeze *(v13 Δ §1)*

The interface of the boot-time `glibc-apply` binary is **frozen**:

- Flag file path: `/var/lib/pkg/glibc-pending`
- Anchor path: `/usr/lib/pkg/glibc-anchor/`
- Staged path: `/usr/lib/pkg/.stage/glibc-active/`
- Exit codes: 0 = success (swap done or no flag present); non-zero = failure (drop to initramfs rescue shell)
- Options: exactly one — `--rollback`

`glibc-apply` takes **no config file** and implements **no features beyond**: check flag → verify staged tree
→ swap → clear flag → exec init → rollback.

**Rationale:** the copy of `glibc-apply` embedded in the initramfs ages independently of the package manager —
a user may go years without regenerating the initramfs. A minimal, frozen interface means that copy needs
updating rarely, and a version mismatch between the initramfs binary and the PM degrades into "refuses to act"
rather than "acts wrongly". Every feature added to `glibc-apply` is a feature that can drift; so there are
none.

#### 7.6.2 Pending-flag protocol version *(v13 Δ §2)*

`/var/lib/pkg/glibc-pending` carries a protocol version. Contents, one field per line:

```
protocol = 1
target = 2.40
manifest_sha256 = <hex digest of the staged tree's manifest>
created = <RFC 3339 timestamp>
```

Boot-time `glibc-apply` reads the protocol byte first. On an **unrecognized protocol version it refuses the
swap and boots the old glibc with a loud warning** — it never guesses at a format it doesn't know. Fail-safe on
drift: an initramfs older than the flag-file format yields a no-op boot, not a wrong swap.

#### 7.6.3 Initramfs drift detection *(v13 Δ §3)*

Two mechanisms, layered:

- **Self-check at boot.** The PM records the SHA-256 of the installed `glibc-apply` binary in the package DB.
  At boot, `glibc-apply` hashes itself and compares against the DB record. This is **best-effort**: if the DB is
  unreachable (filesystem not yet mounted, corruption), the check is skipped silently. On mismatch it warns
  loudly on the console and proceeds — the check is advisory, because blocking boot on a hashing disagreement is
  worse than the drift it detects.
- **Regeneration hook.** The `pkg` package's own recipe carries a post-install hook that regenerates the
  initramfs whenever `pkg` (and thus potentially `glibc-apply`) is updated. This uses the existing §7.2 hook
  machinery — **no new mechanism is introduced**. §13.6 gains this as a **second regeneration trigger**
  alongside kernel updates.

#### 7.6.4 Pending-window transaction rules *(v13 Δ §4)*

While `/var/lib/pkg/glibc-pending` exists:

a. **glibc category-1 security patches are allowed.** They apply inline to the active anchor per §7.6
   category 1 and **do not touch the staged tree**.
b. **Non-glibc transactions are allowed.** Builds and scans run against the **active (old) glibc**, exactly as
   before.
c. **A second glibc major bump REPLACES the staged tree.** The staged tree in `.stage/glibc-active/` is dormant
   until boot, so overwriting it is safe. **Latest-staged-wins; there is no queue.** The flag file is rewritten
   with the new target, manifest hash, and timestamp. The user is told the earlier staged version was
   discarded.
d. **Pending state is recorded in the DB** — target version, staged manifest hash, protocol, timestamp — not
   only in the flag file. This lets pre-build checks **fail fast**: a package version requiring glibc ≥ the
   pending target is rejected at **resolve/staging time** with a clear message, rather than after a multi-hour
   build.

   ```
   error: foo-3.2 requires glibc 2.40, but 2.40 is staged-pending
   (not yet active). Reboot to apply the pending update, or select
   a version of foo compatible with glibc 2.39.
   ```

e. **Undo does not cross the pending-stage boundary** (v14 Δ §2): while the flag exists, `pkg undo` across it
   is forbidden — two mechanisms, one source of truth each (§14.1).

#### 7.6.5 Stale staged-tree handling *(v13 Δ §5)*

At boot, **before swapping**, `glibc-apply` verifies the staged tree against the manifest hash recorded in the
flag file (§7.6.2). If the staged tree is **missing or corrupt**: clear the flag, log the reason, and **boot
the old glibc**. The user re-stages the update on next boot. `glibc-apply` **never swaps unverified content** —
a staged tree that doesn't match its manifest is indistinguishable from corruption or tampering, and the
recovery position (old glibc, intact) is strictly better than a partial boot.

#### 7.6.6 Post-boot verification before recovery removal *(v13 Δ §6)*

After a swap, the old glibc now sitting in `.stage/glibc-active/` is retained as the recovery copy until
**post-boot verification passes**. Verification runs on the first successful `pkg` invocation after the swap
(or via a small OpenRC service, so headless systems don't wait for a human to run `pkg`):

1. `ldd --version` output matches the DB-recorded active glibc version.
2. Smoke-exec of a known binary (e.g. `/bin/sh -c true`) succeeds.
3. Store-manifest verification of the new anchor tree passes.

Only after all three does `pkg clean` consider removing the recovery copy. Additionally, `pkg clean`
**refuses removal when the running glibc version ≠ the DB-recorded active version** — this is the signal that
the system was booted back into the old glibc via the rescue path, in which case the "recovery" copy is the
live system and removing it is fatal.

### 7.7 ABI Tag Derivation — amendments

*Base text unchanged from v10 (cascade math: a package's `abi_tag` derives from its dependencies' tags, and a
change cascades to reverse dependents).*

#### 7.7.1 Base case: glibc *(v13 Δ §8)*

glibc has no libc dependency to derive from. Fixed rule, documented as the cascade's **base case**:

```
abi_tag(glibc) = hash("glibc-" + release_core)
```

where `release_core` is the version **without the `-rN` patch suffix** (e.g. `2.39-r2` → core `2.39`).
Category-1 patches therefore never perturb the cascade; a major bump perturbs it exactly once, which is the
intended cascade semantics.

*(v15 ruling on Appendix A.4: this base case **is** the §7.7.2 general formula, not a competitor — glibc has
no link deps, so the interface-hash set is empty and the formula degenerates to hashing name, version core and
option set. The v13 wording stands because it states the operative constants.)*

#### 7.7.2 Link-interface hashing replaces per-dep-tag hashing *(v14 Δ §5.1)*

Each built package records a link interface — `(soname, sorted symbol-version-set, public-header-fingerprint)`
tuples — extracted by the §8 scanner, which gains `link_interface_hash` as an output:

```
abi_tag(pkg) = hash(name, version_core,
                    { link_interface_hash(dep) for dep in link_deps(pkg) },
                    abi-affecting option set)
```

A dependency rebuilt with a byte-identical link interface yields the same hash and the cascade **prunes itself
at that node** — stability is *measured* after every build, never assumed. A soname-preserving patch bump
collapses from 400 rebuilds to the 0–3 packages whose consumed interfaces genuinely changed. The
abi-affecting option set (§4.3) is hashed into the package's own tag before dependents derive from it.

**Governing principle.** The cascade set must remain a **superset of "actually broken"**, approaching
"actually affected" from above. Every mechanism in §7.7 and §7.9 shrinks the set *safely* or cuts per-package
cost; none trades correctness for speed.

#### 7.7.3 Dependency kinds *(v14 Δ §5.1)*

Manifest deps are `link` (default; feeds `abi_tag`), `runtime` (dlopen'd plugins, exec'd helpers, data —
excluded from the tag), or `build` (toolchain-only, never cascades). Demotion to `runtime` requires scanner
proof (no `DT_NEEDED`, no undefined symbols resolve to that package), and `pkg check` remains the safety net —
a missed rebuild is the bad direction. Unclassifiable edges are treated as `link`: **uncertainty always widens
the cascade**.

**Static linking:** consumed `.a` members contribute a content hash of their symbol tables, so a security fix
still cascades to static consumers while cosmetic rebuilds prune as above.

### 7.8 Out-of-Tree Kernel Modules (DKMS Integration) — DEFERRED post-v1 *(v11; deferred by v12 Δ §5)*

All DKMS integration (§4.2's `[dkms]` semantics, §7.8.1–7.8.4, §19 Phase 7) is **deferred to post-v1**. The v11
text below remains the design of record for when it lands, with the corrections noted.

§20 (Explicit Non-Goals) defers kernel module *building* to DKMS — that remains true. What v10 left
unspecified is everything around it: dependency tracking, sandboxing, and rebuild triggering. Nvidia's
proprietary driver is the concrete case (used as a live example in §5.4's `gaming-stable` profile), and it needs
a compiler and kernel headers at rebuild time, which fits neither existing sandbox:

- The category hook sandbox (§7.2.1) has no compiler present, is read-only on `/etc`, and blocks
  service-management binaries — too restrictive for a build.
- The build orchestrator's bubblewrap sandbox (§7.1) is scoped to building a *package* into `$PKGDIR` against
  dependencies resolved at solve time — not to rebuilding against an already-running kernel's headers outside
  the normal dependency graph.

#### 7.8.1 Dependency Tracking

**A package with a `[dkms]` table** (§4.1, §4.2) implicitly depends on `linux-headers` matched to **every
currently installed kernel**, not just the running one — a system may have multiple kernels installed for
rescue-boot purposes. The resolver tracks this as a per-kernel dependency edge: `nvidia-driver` →
`linux-headers(=<kernel-version>)` for each installed `virtual/kernel` provider.

*Known follow-ups, recorded so they are not rediscovered mid-implementation (v12 Δ §5):* depmod scope;
ownership of `/var/lib/dkms` state; module signing; and the **per-kernel edge schema**, which needs a "module
built for kernel" table plus kernel-transaction ordering rules (a `Dependency::Package` + `Exact` edge per
installed kernel, as above, does not by itself express "already built for this kernel").

#### 7.8.2 Trigger Points

The package manager — not the recipe — triggers DKMS rebuilds, at two points:

1. **Installing or upgrading a package with a `[dkms]` table.** Build against every currently installed kernel.
2. **Installing a new `virtual/kernel` provider** (§13.6). After initramfs regeneration for that kernel
   completes, the PM iterates every installed package with a `[dkms]` table and triggers a rebuild against the
   new kernel.

#### 7.8.3 Kernel-Module Sandbox Profile

A new profile is added to §7.2.1's sandbox table:

| Profile | Categories | Hook Permissions |
|---|---|---|
| **Kernel-Module** | packages with a `[dkms]` table | Read-write `/lib/modules/<kernel-version>/extra/`. Read-only access to the resolved `linux-headers` tree and standard build toolchain (`gcc`, `make`). No network. No `/etc` write. No service-management binaries (same restriction as all other hook profiles). |

#### 7.8.4 Failure Handling

A DKMS rebuild failure (e.g., a kernel ABI change the out-of-tree module hasn't caught up with) is reported the
same way a `post_install` hook failure is (§7.5, Phase B): a warning, not a transaction-aborting error. The
kernel install itself remains committed:

```
warning: DKMS rebuild failed for nvidia-driver against kernel 6.9.2.
         Kernel 6.9.2 is installed and initramfs is up to date, but the
         nvidia kernel module could not be built. GPU acceleration may be
         unavailable on this kernel. See /var/log/pkg/dkms-nvidia-6.9.2.log.
         Your previous kernel (6.8.9) retains a working module, if still installed.
```

This mirrors §3.7's security-patch pattern: the core system operation (kernel install) succeeds and is
honestly reported as committed, while a best-effort dependent operation (module rebuild) is allowed to fail
without dragging the whole transaction down — consistent with §2's "honest atomicity" principle.

### 7.9 Survivable Rebuild Cascades *(v14 Δ §5; the link-interface hashing half is §7.7.2–7.7.3)*

The §7.7 cascade is correct but silent on cost: a 400-package cascade is hours of compile time and the most-cited
reason users abandon source distributions. §7.7.2's governing principle applies throughout.

#### 7.9.1 DAG parallel scheduler, cgroup weights

A cascade runs as a topological sweep; a package starts the moment its in-cascade deps are *activated*, not at
level barriers. Parallelism is a **total-token pool**: each build holds `inner_jobs` tokens, throttled by
phase and load (configure is serial, holds 1 token). *(v15 Δ §4)* The two knobs are concrete user
configuration in `/etc/pkg/config.toml`, under `[build]`: **`jobs`** — the token pool's size, i.e. the total
compile jobs Yakimo may run at once (default `ncpu`); **`max_pkg_jobs`** — the per-package cap on
`inner_jobs` (default 4). `--jobs` on `pkg build`/`pkg install` overrides the pool for that invocation. Builds
run in a dedicated cgroup v2 subtree: `cpu.weight=50` (under contention the cascade gets ~1/3 of CPU; idle,
everything — **weights, not quotas**), `cpu.max=75%` only while a user session is active, `io.weight=50`,
`memory.high = RAM − 2 GiB`, `nice 10`, `ionice` best-effort 7.

#### 7.9.2 Per-package activation

A cascade is **N transactions, one per package**, activating in DAG order — not one giant transaction. The v12
lock amendment (§9.4) already builds unlocked and re-validates on activation; failure at package 387 of 400
must not discard 387 builds. The mixed-state window is already legal: a package enters the cascade only if its
consumed interface changed, and old-abi binaries resolve against retained old libraries until their turn
(store refcounting — disk cost, not correctness). The DB records a `cascade_id` so `pkg history` shows the
cascade as one logical operation and `pkg undo` reverses it as a unit. A build failure marks the failed package
and its in-cascade dependents `blocked` and continues; OOM gets one automatic retry at halved `inner_jobs`.

#### 7.9.3 ETAs from `build_history`

The `build_history` table (§9.2) records duration, success, timestamps, ccache hit rate, `host_jobs`, peak RSS.
Per-package ETA = median of the last 5 successful builds of that name (any version), scaled by
`(host_jobs_recorded / host_jobs_now)^0.7`; first builds fall back to similar-source-size medians. Cascade ETA
is the DAG critical path, displayed as "≈3.5 h wall (≈11 h compile across 8 cores)". History feeds display and
ordering only, never correctness — a poisoned row costs a bad ETA, nothing worse. No telemetry leaves the
machine.

#### 7.9.4 Deferral and preview

`--dry-run` prints the full plan before any fetch: direct updates, rebuild count with cascade root, pruning
savings, predicted wall/compile time, largest packages, and the deferral commands — it runs the full resolve
and interface comparison, so the preview is honest.

**`--skip-rebuilds` is safe, with proof:** old-abi dependents keep resolving against retained old-abi store
dirs (refcounted), and if the interface hadn't changed, pruning would have removed them anyway; the one
requirement is that a pending-rebuild package **pins its current dependency generation** against `pkg clean`.

**Security deferral guard:** `--skip-rebuilds` refuses any cascade whose root or members match an open
`security.json` advisory unless given `--defer-security=I-understand` — deliberately un-ergonomic, per the
standing foot-gun stance.

**Defer lists:** `[defer] never_rebuild_eagerly` in `/etc/pkg/config.toml` routes listed packages to the
deferred window; explicit `pkg upgrade <name>` overrides.

#### 7.9.5 Personal artifact cache

`/var/lib/pkg/cache/` holds locally built artifacts keyed by **full build identity**:
`<name>/<version>-<abi_tag>-<option_set_hash>-<recipe_commit>.pkg.tar.zst`. A hit is by construction what a
source build would produce — content-addressed reuse, **not** a binary repository: no index, no signatures
beyond store-manifest verification, no cross-host semantics. Invalidation is structural (any key component
differs) plus manifest verification at reuse; LRU above a 30 GiB cap; `pkg clean --cache`; `pkg build
--no-cache` for paranoia; a miss is silently a source build.

#### 7.9.6 ccache first-class

`[cache] compiler = "ccache"` (off | ccache | sccache) wraps CC/CXX; per-package opt-out is a *recipe-author*
decision (`[build] compiler_cache = false`). Content-keyed, so no invalidation logic; hit rates land in
`build_history`. Incremental build-dir reuse across abi_tag changes is **rejected** (autotools caches bake in
old-dependency answers); ccache captures ~90% of the speedup with none of the staleness risk.

#### 7.9.7 Recorded refusals — what NOT to do

- **distcc / distributed builds** — trust-surface multiplication for a niche speedup; neither shipped nor
  blessed.
- **Binary mirrors** — contradicts source-only, drags back the whole §10.2 trust stack; the personal cache
  covers the legitimate need.
- **One-giant-transaction cascades** — purity the abi model doesn't need, paid for with all-or-nothing failure.
- **Heuristic "probably fine" cascade skipping** — the link-interface hash is the *measured* version of every
  such heuristic; anything looser is a missed security rebuild.

### 7.10 Post-Build Tree Normalization *(v15 Δ §2)*

Between the install phase and the §8 gate, pkg-build normalizes the packaged tree. These passes exist because
packages are consumed at two locations — the live tree (`/usr/...`) and, during a dependent's build, the
sandbox alias (`/pkg/deps/<name>/...`) — and several upstream conventions silently bake in the first:

1. **Relocatable `.pc` rewrite.** A shipped `prefix=/usr` is rewritten to `${pcfiledir}`-relative form (the
   `..` count computed from the file's own depth below `usr/`), and — only when the prefix was relocatable —
   other hardcoded `name=/usr[...]` path variables become `${prefix}`-derived. `${pcfiledir}` expands at read
   time, so the file is correct in both worlds. Files with a non-`/usr` prefix are the recipe's business and
   are left alone.
2. **`.la` stripping.** Libtool archives are removed tree-wide. They record absolute paths of *other*
   packages' `.la` files, which do not exist inside a consumer's sandbox (deps live at `/pkg/deps/<name>`);
   without them, libtool falls back to plain `-l` linking, which the §7.1 search-path env already arranges.
   The removal is logged per package.

**Recipe convention: precompile Python bytecode.** Activated files are symlinks into the store, so a Python
payload that byte-compiles at first run writes `__pycache__` *through the symlinks into the store* — store
drift that `pkg db check` then reports as a content mismatch. Recipes shipping Python modules run
`compileall` in their install phase so the `.pyc` files are part of the manifest. (The reference tree does
this for meson and glib's codegen modules.)

---

## 8. Binary Scanner & Post-Build Verification *(v12 Δ §9; v14 Δ §5.1)*

(Base text unchanged from v10 — scope, static analysis, heuristic dlopen detection, NSS non-coverage,
`pkg check`. Reframed and extended as follows.)

**Post-build verification, split by failure class *(v15 Δ §3)*.** v12 reframed "the gate" as
report-and-record; the implementation sharpened that into two different answers for two different checks:

- **Foreign-glibc floor/ceiling and ISA notes: report and record.** Under source-only, floor ≤ ceiling holds
  by construction — a binary is built against the system's own glibc — so the interesting failure is a recipe
  escaping its sandbox to a foreign libc, not a version skew between artifact and host.
- **Undeclared `DT_NEEDED` linkage: a hard gate failure** (`PkgError::Gate`, CLI exit 3) for source builds.
  A binary linked against a library no declared dependency provides means the build saw something it
  shouldn't have — an undeclared system lib or a sandbox escape — and shipping it would break the dep graph
  the resolver, cascades, and `pkg remove` protection all rely on. The failure report names the missing
  soname *and the installed package that provides it*, so the fix is one line in `[dependencies]`.
  `[build] binary = true` repackagings run under a relaxed policy: vendor blobs routinely link system
  libraries the distribution packages separately from the blob, so undeclared sonames there are **warnings**,
  printed to the build log. The foreign-glibc floor check stays a hard failure regardless.

The §15 pipeline's "verify" stage reflects this split.

**`pkg check` scope.** `pkg check` **does not detect absent dependency packages** — it scans binaries for
missing *libraries*, not the DB for missing *packages*. The DB dependency check is the mechanism that flags
broken dependents (see §16.6).

**New scanner output (v14).** The scanner emits `link_interface_hash` per built package (§7.7.2) and checks
the honesty of declared options: `add_deps` with no resulting soname linkage is flagged in CI (§4.3).

---

## 9. Package Database

### 9.1 Storage

(Unchanged from v10.)

### 9.2 Schema [REVISED — schema v3]

```sql
CREATE TABLE packages (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL,
    version TEXT NOT NULL,
    abi_tag TEXT NOT NULL,
    opt_tag TEXT NOT NULL DEFAULT '',                  -- v14: see §4.3; '' = all-default selection
    arch TEXT NOT NULL DEFAULT 'amd64',
    track TEXT NOT NULL CHECK(track IN ('binary', 'source')),   -- 'binary' kept legal for forward compat
    installed_at INTEGER NOT NULL,
    installed_explicitly INTEGER NOT NULL DEFAULT 0,   -- see §16.6
    UNIQUE(name, version, abi_tag, opt_tag, arch)
);

-- dependencies, files, virtual_providers, alternative_prefs, conffile_state:
-- unchanged from v10.
```

**`track` enum.** The `'binary'` value stays legal even though the binary track is deferred (§10.2): removing it
now would force a schema migration later to add it back. *(v12 Δ §1)*

**`installed_explicitly`** is set to `1` when a package is the direct target of `pkg install <name>`, and left
`0` when it's pulled in solely to satisfy another package's dependency. Explicitly installing a package that
was previously present only as a transitive dependency flips it to `1`. This is the field `pkg autoremove`
(§16.6) uses to determine what's actually orphaned. **Migration note** *(v12 Δ §7)*: on any future DB
migration, packages lacking the flag are **seeded as explicit**. That is the safe direction — autoremove then
proposes *nothing* rather than *everything*. A false negative leaves disk usage; a false positive removes the
system.

**`opt_tag` backfill** *(v14 Δ §6)*: the `opt_tag = ''` backfill is the safe direction (same precedent as
above: a wrong empty tag causes a redundant rebuild, never a wrong link).

#### 9.2.1 Additional tables *(v14 Δ §1, §2, §4, §5, §6)*

**One schema bump, v3**, carries all v14 additions in a single migration: `packages.opt_tag` +
`package_options`; `generations` + `generation_packages` (derived approach — no `activation_log`);
`provenance`; `build_history`; `cascade_id`.

**`generations` and `generation_packages`** — committed in the same SQLite transaction as the installed-set
change (record and state atomic by construction). See §14.1 for the model.

```sql
CREATE TABLE generations (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    created_at INTEGER NOT NULL, command TEXT NOT NULL,
    kind TEXT NOT NULL CHECK(kind IN ('install','remove','upgrade','undo')),
    undoes INTEGER REFERENCES generations(id),
    schema_ver INTEGER NOT NULL
);
CREATE TABLE generation_packages (
    generation_id INTEGER NOT NULL REFERENCES generations(id),
    name TEXT NOT NULL, version TEXT NOT NULL, abi_tag TEXT NOT NULL,
    opt_tag TEXT NOT NULL DEFAULT '', arch TEXT NOT NULL,
    installed_explicitly INTEGER NOT NULL,
    PRIMARY KEY (generation_id, name, arch)
);
```

**`package_options`** — records the option selection behind each installed package's `opt_tag` (§4.3).

**`provenance`** — append-only, one row per installed artifact: `recipe_commit`, `source_hashes` (as verified
at fetch), `index_version`, attestation fingerprints, `build_host`, `builder_version` (pkg + sandbox profile),
timestamps, and `reproducibility` (`self-match` / `mismatch`). Purpose: answering, after the next xz-style
incident, which installed packages were built from a later-implicated recipe commit.

**`build_history`** — duration, success, timestamps, ccache hit rate, `host_jobs`, peak RSS (§7.9.3).

**`cascade_id`** — tags every per-package transaction belonging to one logical cascade, so `pkg history` and
`pkg undo` treat it as a unit (§7.9.2).

**Glibc pending state** *(v13 Δ §3, §4)*: the pending-window state (target version, staged manifest hash,
protocol, timestamp) and the SHA-256 of the installed `glibc-apply` binary are recorded in the DB (§7.6).

### 9.3 Database Corruption Handling

(Unchanged from v10 — startup integrity check, `pkg db check`/`pkg db regenerate`, reconciliation as rollback
backstop.)

**Generations and reconciliation** *(v14 Δ §2)*: reconciliation stays filesystem → DB: generations never
rebuild the installed set. A regenerated DB keeps generations if salvageable, else restarts at generation 1
with history marked truncated.

### 9.4 Concurrency & Locking *(v11, amended by v12 Δ §3)*

All package-manager state access is coordinated through a single `flock(2)`-based lock at
`/var/lib/pkg/lock` (§6).

**Lock scope (amended).** v11 had writers hold the exclusive flock for the whole transaction, including builds
— hours of contention for no correctness benefit. Now:

- The **exclusive lock covers only the critical section**: installed-state read for staging validation →
  staging → activation → DB commit. **Builds run unlocked.**
- On lock acquisition, the transaction **re-validates that resolution inputs haven't drifted** — i.e., the
  installed-set assumptions the solve was made against still hold. On drift: **re-solve once**, then fail with
  a clear error if the inputs still won't hold still. ("Re-solve once, then fail" bounds the retry loop; a
  system being concurrently mutated by other means is not ours to chase.)

**Who takes which lock:**

- **Writers** (`pkg install`, `pkg remove`, `pkg upgrade`, `pkg build`, `pkg clean`, `pkg db regenerate`,
  **`pkg sync`**) take an **exclusive** lock for the critical section above. `pkg sync` is a writer because it
  mutates local index cache state. Environment operations take the lock as well (§16.7).
- **Readers** (`pkg info`, `pkg why`, `pkg explain`, `pkg search`, `pkg check` when querying the DB) take a
  **shared** lock — multiple readers proceed concurrently, and each holds the lock only briefly (a single
  query), so they block a pending writer for a negligible window.
- A writer blocking behind active readers or another writer waits, rather than failing immediately, since most
  contention is transient:
  ```
  error: another pkg operation is in progress (pid 4821, started 12s ago).
         Waiting for lock (use --no-wait to fail immediately)...
  ```
  Default wait timeout: 60s, configurable via `--timeout`.

**Holder metadata.** The holder-pid message above requires a **sidecar metadata file** next to
`/var/lib/pkg/lock`, written on acquisition and **truncated on writer release** — flock itself carries no
holder information, and a stale message from a dead process is worse than none.

`pkg check`'s existing DB-unreachable fallback is reused for lock contention specifically: if a reader can't
acquire the shared lock within a short timeout (default 2s, distinct from the writer timeout above), it falls
back to filesystem scanning rather than blocking — consistent with `pkg check` already being designed to
degrade gracefully. *(v12 Δ §8 fixed this passage's stale "§8.6" reference to simply `pkg check`.)*

---

## 10. Repository Format

### 10.1 Source Repository

(Unchanged from v10.)

### 10.2 Binary Repository — DEFERRED *(v12 Δ §1)*

The binary distribution track is **deferred indefinitely**. `pkg install` is always a source build; there is no
binary-vs-source choice to make, and no "binary-first with source fallback" language survives anywhere in the
spec (§16.1's `pkg install` description is corrected accordingly).

*Retained for the record:* (v10/v11) flat index, `.pkg.tar.zst` archives, Ed25519 detached signatures.

**The one exception: the §18.2 bootstrap glibc tarball** is a **permanently imported external artifact**. No
toolchain exists on a bare system to compile glibc; this cannot be built from source by definition. §10.5
(trust chain) and §10.6 (sync) are **retained** in full precisely because of it: they authenticate this
artifact, plus the advisory index, plus the recipes themselves (§10.5.5).

### 10.3 Resolution Profiles

(Unchanged from v10, plus: profiles may carry `[options]` sections, which sit third in option-configuration
precedence — §4.3.)

### 10.4 Security Advisories

(Unchanged from v10 — `security.json` index. Freshness requirements for this index under security enforcement
mode are specified in §10.6.)

### 10.5 Trust Model & Key Management

v10 specified that packages and the index are signed, but not *what* verifies those signatures, or how trust
survives a key rotation or compromise. This section defines a trust chain, loosely inspired by The Update
Framework (TUF), simplified to what this project actually needs. v14 adds a fourth role (reviewer).

#### 10.5.1 Roles

1. **Root role.** The ultimate trust anchor: a small set of long-lived keys (e.g., five, held by distinct
   maintainers/entities), requiring a threshold (e.g., 3-of-5) to authorize any root-level change. Root keys
   are kept offline and are not used for routine operations — only for delegating and rotating the roles
   below.
2. **Repository (index-signing) role.** One or more online keys, delegated by the root role, used to sign
   `packages.json`, `security.json`, `profiles.toml` and the recipe manifest (§10.5.5) during normal CI
   operation. Rotatable by the root role without requiring a new `pkg` binary release.
3. **Timestamp role.** A short-lived key, re-signed frequently (default: every 24 hours), whose sole job is to
   sign a small pointer document containing the current index version/hash and its expiry. This prevents
   freeze/replay attacks — a party serving a stale-but-validly-signed old index indefinitely.
4. **Reviewer role** *(v14 Δ §4)*. A root-delegated role — maintainer keys allowlisted in the root-signed
   delegation document — whose attestations feed the critical-set rule (§10.5.6). Revocation rides §10.5.3's
   existing flow.

#### 10.5.2 Bootstrap Trust

The musl-static `pkg` binary embeds the root role's public keys (plural, threshold) at compile time — this is
the actual trust anchor; everything else is delegated from it. On first `pkg bootstrap` or `pkg sync`, the
client fetches, in order: the root manifest (signed by a threshold of root keys) → the repository key
delegation (signed by root) → the timestamp document (signed by the timestamp key) → the index (signed by the
repository key). Each layer's signer is authorized by the layer above it.

#### 10.5.3 Key Rotation

- **Root key rotation:** existing root-key holders, meeting the current threshold, sign a new root manifest
  listing the replacement key set. `pkg` verifies the new manifest against the *currently trusted* root keys
  before accepting it. Root rotation therefore never requires shipping a new `pkg` binary, provided the
  compromise doesn't take out enough keys to break the existing threshold.
- **Repository key rotation** (e.g., suspected CI compromise): the root role signs a revocation plus a new
  repository key delegation. Clients pick this up on their next `pkg sync` (§10.6).
- **Full root compromise** (a threshold of root keys lost or compromised simultaneously) is the one case this
  design cannot self-heal from — recovery requires a new `pkg` binary release with new embedded root keys,
  distributed through an out-of-band channel (project website, signed announcement). This is inherent to any
  root-of-trust design, not specific to this one, and is documented plainly rather than glossed over.

#### 10.5.4 `pkg key` CLI

```
pkg key list
    Show currently trusted root and repository keys, with expiry.

pkg key show-chain
    Print the full delegation chain backing the currently active index.

pkg key trust-root <file>
    Manually import a new root manifest (out-of-band recovery only).
    Displays the new key fingerprints and requires interactive confirmation,
    since this bypasses the normal delegation chain by design — it is the
    recovery path for when that chain is broken.
```

#### 10.5.5 Recipe & Source Integrity *(v12 Δ §2)*

Deferring binaries does not remove the trust requirement — it moves it. The attack surface under source-only
is the recipe repository and the fetched tarballs, and v11 left that path unstated. Decision:

The signed index (already authenticated via the root → repository → timestamp chain) gains a **recipe
manifest** mapping `package@version → { git commit hash of the recipe, SHA-256 of each fetched source tarball }`.
`pkg fetch` verifies both before any build begins: the recipe is checked out at exactly the recorded commit,
and every fetched tarball's digest must match the manifest. Git commit *signing* is optional and not required
— the signed index is the trust path; commits are pinned by hash, which the index's signature covers.

This is the same "mirrors are transport-only" argument applied one layer deeper: the git server and tarball
hosts are transport; the signed index is what's trusted.

#### 10.5.6 Supply-Chain Trust for Source Inputs *(v14 Δ §4)*

§10.5.5 closes the transport gap. But hash-pinning detects *substitution*, not *content* — xz-utils showed the
payload living in a release tarball's build machinery, absent from the VCS, committed by a trusted
maintainer's account.

**Threat taxonomy T1–T7**, each with mitigation and honest residual:

| # | Threat | Residual |
|---|---|---|
| T1 | Malicious upstream tarball (payload in tarball, absent from VCS — the xz pattern) | Payload in *both* survives |
| T2 | Compromised upstream VCS/release infra | Pin poisoned at first record |
| T3 | Malicious/compromised recipe maintainer | Collusion; one maintainer with two keys |
| T4 | Malicious or inattentive reviewer | Inattentive review; mitigated by small diff surfaces and lint |
| T5 | Build-host compromise | Compromised host root — out of scope by definition |
| T6 | Mirror/transport | Already fully addressed (§10.5.5, §10.6); no change |
| T7 | Dependency confusion on fetch | First-come poisoning; mitigated only by review |

**Critical-set two-attestation rule.** A `critical = true` marker on an explicit list of fewer than 50
packages (glibc, openssl, bootstrap-path decompressors, openssh, the toolchain, `pkg` itself). The repository
role **refuses to sign an index** containing a critical entry with fewer than two attestations from distinct
keys, distinct from the committer. Enforcement is at **index-signing time**, not client time; clients verify
the repository signature as today and spot-check critical packages they install (cheap, fail-loud).
Attestations claim only "I reviewed the recipe diff and recorded hashes against upstream at review time" —
never upstream benevolence.

**Cross-source corroboration in recipe CI** (gate on attestation eligibility, critical set):

1. **Tarball↔VCS differential** — fetch both independently, diff trees, normalizing generated files against a
   per-package allowlist (`configure`, `Makefile.in`); `.m4` macro diffs and test-suite binary blobs are *not*
   normalized — xz's payload lived exactly there; a diff outside the allowlist blocks attestation.
2. **External hash cross-check** — Software Heritage first-seen timestamps and other distributions' published
   hashes (Gentoo Manifests, Alpine APKBUILDs, Nixpkgs fixed-output); disagreement blocks and files an issue.
3. Results recorded as `provenance/<name>/<version>.toml` in the recipe repo. CI-only; clients never re-run
   them — the hardening lands where it matters: when a hash enters the index.

**Local double-build for the critical set.** With no binary repo, reproducibility verification degenerates to a
local self-check: critical-set builds **run twice by default** — sequential sandboxed builds,
`SOURCE_DATE_EPOCH` pinned, output tree hashes compared. Mismatch **aborts the install**, quarantines both
trees under `/var/lib/pkg/build/quarantine/` with a diff report, and records `reproducibility='mismatch'`
(§9.2.1). Opt-out (`--no-repro-check`), not opt-in — ~50 packages is a bounded cost; the default must be the
safe direction.

**`pkg audit --supply-chain`.** Per installed package: attestation count vs. requirement, reproducibility
result, provenance completeness, fetch-origin drift (T7), index-hash repinning events (T2). `--critical`
filters; exit 0 clean / 1 warnings / 2 hard findings. A hard finding **blocks** `pkg install`/`pkg upgrade`
until acknowledged via `--acknowledge <package>` — the same blocking surface as §16.4's stale-index gate.

**Explicit non-goals (stated, not attempted):** compromised host root/kernel; malicious upstreams whose payload
survives diff review (we raise attacker cost and retrospective detection, and the spec says so); trusting-trust
compiler backdoors; per-user fetch trust or third-party recipe repos — one trust root, one recipe repo.

### 10.6 Repository Sync (`pkg sync`)

```
pkg sync
    Fetch and verify the latest repository index (packages.json, security.json,
    profiles.toml, recipe manifest) per the trust chain in §10.5. Updates local cache.

pkg sync --force
    Bypass the local cache TTL, fetch immediately.

pkg sync --status
    Show cache age, last successful sync time, and timestamp-role expiry.
```

`pkg sync` is a **writer** for lock purposes (§9.4).

**Mirrors:** `/etc/pkg/mirrors.toml` lists mirror URLs. Mirrors are transport only — the signature chain in
§10.5 is what's actually trusted, so any mirror can serve the same signed artifacts without being individually
trusted itself. `pkg sync` tries mirrors in listed order, falling over to the next on failure.

**Cache TTL:** default 24 hours, matching the timestamp role's re-signing cadence. Ordinary commands (`pkg
search`, `pkg install`, etc.) use the cached index without triggering an implicit sync — operating on a
stale-but-not-expired cache is intentional, consistent with "explicit over implicit" (§2). A command warns if
the cache exceeds the TTL:
```
warning: repository index is 3 days old. Run 'pkg sync' to refresh.
```

**Interaction with security enforcement mode (§16.4):** `security.json` arrives as part of the same signed
index bundle. When `enforce-restart = true`, a stale index is treated as untrustworthy for its purpose rather
than silently reporting a clean result — and the gate blocks `pkg install`, `pkg upgrade` **and** `pkg audit`
(§16.4):
```
error: security advisory index is 9 days old (exceeds 24h freshness requirement).
       Run 'pkg sync' before proceeding, or operations under enforcement mode
       cannot be trusted to reflect current advisories.
```
This is deliberately stricter than the general staleness warning — enforcement mode's entire value depends on
the index being current, so a silent stale pass-through would be worse than blocking.

### 10.7 Version Retention Policy — DEFERRED *(v12 Δ §1)*

Retention governs published binaries; with the binary track deferred it has no purpose. The 30-day purge
policy and source-build fallback from v11 are not in force. **`--binary-only` is removed outright** — with
retention deferred it has no referent. `pkg install foo=<version>` is simply a source build of that version
(§16.1).

### 10.8 Reproducible Build Verification (Tiered) — DEFERRED *(v12 Δ §1)*

Tier 1 (CI self-reproducibility) and Tier 2 (independent third-party rebuild attestations) have no purpose
without published binaries. The reproducibility signal that survives under source-only is the **local
double-build of the critical set** (§10.5.6), with results stored in the `provenance` table and surfaced by
`pkg audit --supply-chain`. The recipe-CI Tier 1 corners (all-default, all-on options — §4.3) are unrelated to
this section.

### 10.9 The Reference Ports Tree & Upstream Version Tracking *(v15 Δ §5)*

The distribution's recipes live in a ports tree (`ports/`, Gentoo-style `<category>/<name>/`), and the tree —
not a hand-maintained index — is the repository's source of truth. A recipe directory is:

```
ports/<category>/<name>/
├── pkg-meta.toml    # identity, [dependencies] (link/runtime/build), [options], [build] markers
├── source.toml      # [[source]] url + sha256 (+ optional filename) — the pin
├── build            # /bin/sh build-phase script (configure && make, meson setup, ...)
├── install          # /bin/sh install-phase script (fills $PKGDIR)
└── watch.toml       # optional: how to poll upstream for newer versions
```

`ports2repo` turns the tree into a signed repository (index + recipe manifest, §10.5.5) in one pass;
`ports/fetch-distfiles.sh` prefetches every `source.toml`'s URL into a distfiles directory and verifies the
tree-pinned SHA-256. `pkg` then consumes that repository exactly like any other mirror — the tree is how the
distribution *authors* the repo, never a special case in the client.

**Version tracking.** Every recipe pins an exact version plus a distfile hash — that pin is what makes the
supply-chain rules in §10.5 enforceable, and it does not change on its own. What is automated is the *bump*: a
recipe may carry a `watch.toml` describing how to poll its upstream, and `ports/watch.sh` walks the tree
reporting (or with `--apply`, applying) available updates — the same role Debian's `uscan` plays for
`debian/watch`. `--apply` fetches the new distfile to compute its hash, so a bump is a complete, reviewable
tree change that flows through the normal attestation path (§10.5.6). Flat TOML:

```toml
type = "git-tags"                     # or "release-regex"
url = "https://github.com/madler/zlib.git"
tag_prefix = "v"                      # git-tags: stripped from tag names
regex = 'lua-([0-9.]+)\.tar\.gz'      # release-regex: group 1 = version
stable_regex = '^[0-9]+\.[0-9]+(\.[0-9]+)?$'   # optional filter — "latest stable", not "latest tag"
url_template = "https://zlib.net/fossils/zlib-{version}.tar.gz"
```

`stable_regex` is the honest answer to "just track the latest version": upstreams publish release candidates,
nightlies, and epoch-resetting forks from the same listings, so *which* versions count as stable is a
per-recipe declaration, not a heuristic. A recipe with no watch.toml (or no matching upstream listing) is
pinned by hand — the tree currently does this for sqlite (concatenated-version archive URLs) and discord
(vendor deletes old tarballs without notice).

**The reference set.** The tree carries 66 recipes in four deliberate spans: the base library/system set
(glibc, zlib, ncurses, sqlite, …), the git chain (openssl, curl, git), a minimal live system (util-linux,
iproute2, procps-ng, e2fsprogs, openssh, bash/coreutils/grep/sed/gawk/tar/gzip/xz, sysvinit), the meson
stack + NetworkManager and its full dependency closure (python3, glib, dbus, eudev, gnutls, wpa_supplicant,
…), and the **self-hosting toolchain** (make, diffutils, findutils, patch, m4, bison, flex, gperf, gettext,
mpfr, mpc, binutils, gcc). One repackaged binary blob (discord) exercises the `[build] binary` path. The
tree's own README tabulates every recipe with its build style and quirks.

**Deferred: recipe boilerplate reduction.** Most autotools recipes repeat the same three-line
build/install text (`./configure … && make`, `make DESTDIR=… install`), and a recipe directory currently
requires up to four files whose contents are largely mechanical. A future revision may add declared
build-system templates — e.g. `build_system = "autotools"` in pkg-meta.toml supplying default phase
scripts, with shipping the script files remaining the override — and may fold source.toml into
pkg-meta.toml. Deferred, not implemented: the explicit scripts are the audit surface while the tree is
young, and repetition is the price of every recipe being greppable and self-contained.

---

## 11. Configuration File Management

(Unchanged from v10. Note: `pkg undo` does not revert conffile state — §14.1.)

---

## 12. Garbage Collection

(Base text unchanged from v10 — manual GC, kernel reference counting, disk-pressure auto-GC. Amendments:)

**Generation-aware hold** *(v14 Δ §2)*. `pkg clean` refuses to prune store dirs referenced by the most recent
N restorable generations (default N=5, config `undo-hold-generations`); `--garbage` overrides but warns and
names the generations that become non-restorable. "Undo works until clean" is the explicit, surfaced contract;
restorability is computed on demand from store-dir existence.

**Reference set** *(v14 Δ §3)*. The GC/autoremove reference set is **installed set ∪ retained generations ∪
all environments**; `pkg env delete` releases an environment's pins (§16.7).

**Pending rebuilds** *(v14 Δ §5)*. A package with a deferred rebuild pins its current dependency generation
against `pkg clean` (§7.9.4).

**Glibc recovery copy** *(v13 Δ §6)*. `pkg clean` removes the glibc recovery copy only after post-boot
verification passes, and refuses when the running glibc version ≠ the DB-recorded active version (§7.6.6).

**Cache** *(v14 Δ §5)*. `pkg clean --cache` prunes the personal artifact cache (§7.9.5).

---

## 13. Init System Integration

(§13.1–13.5 unchanged from v10. The reference tree's init is **sysvinit** *(v15 Δ §7)* — `e2e/ports.sh` boots
it as PID 1 in a user namespace and verifies the inittab sysinit action runs. The rc-service command names in
§3.7 are examples for an OpenRC profile; the mechanism is init-agnostic either way.)

### 13.6 Initramfs Regeneration

(Unchanged in mechanism from v10; ordering relationship with §7.8: DKMS rebuilds — deferred post-v1 — are
triggered *after* initramfs regeneration completes for a newly installed kernel, not before.)

**Second regeneration trigger** *(v13 Δ §3)*: in addition to kernel updates, the initramfs is regenerated by a
post-install hook on the `pkg` package itself whenever `pkg` (and thus potentially `glibc-apply`) is updated
(§7.6.3).

---

## 14. Rollback and Undo

### 14.0 Rollback: Explicit Non-Feature

(Unchanged from v10 as to *system* rollback. Amended below by §14.1's adopted wording.)

**Adopted wording** *(v14 Δ §2)*:

> Rollback of the *system* — bootloader-level, catastrophe-grade — remains an explicit non-feature;
> filesystem snapshots are the correct Unix tool, unchanged. What this specification adds is
> *transaction-level undo*: restoring a prior package set from retained store dirs to recover from
> **semantic regressions** — successfully installed, successfully activated, worse software. The boundary
> is intent: snapshots recover from breakage; undo recovers from regret. Undo never substitutes for
> snapshots (it cannot survive `pkg clean`, store corruption, or glibc-anchor failure) and snapshots never
> substitute for undo (they are all-or-nothing, pre-arranged, and revert user data along with packages).

**Terminology, settled** *(v14 Δ §6)*: **"undo"** names transaction-level restore (`pkg undo`); **"rollback"**
is reserved for the glibc rescue path (`glibc-apply --rollback`). The two stay verbally distinct because they
are mechanically distinct: one re-points symlinks within the store; the other swaps the libc anchor at boot.

### 14.1 Transaction Generations and Undo *(v14 Δ §2)*

§14's original stance targeted *package-level* rollback and the PM pretending to be a snapshot tool; both
stand. The unaddressed case: a transaction **succeeds** and the resulting software is semantically worse. v14
adds transaction-level undo: restoring the pre-transaction package set and symlink tree, for non-glibc
packages, while the superseded store directories still exist.

**Model: generations, derived approach.** The symlink tree is fully derivable from `packages` + `files`, so
undo needs no snapshot machinery and **no `activation_log` table**: reconstructing from package sets (chosen
over "record every symlink op") reuses §7.5's stage→activate machinery wholesale, the generation table playing
the resolver's role — one activation code path, no divergent undo path. Tables: §9.2.1.

Generation *N* is the package set resulting from transaction *N*; undoing *N* restores generation *N−1*. Undo
never solves — it restores a historically occupied, internally consistent state, so cascades reduce to
re-pointing symlinks at retained old builds: data, not computation. Before staging, every reactivation's store
dir is existence-checked; if any was pruned, undo **fails atomically**, listing the missing dirs.

**Sequential undo only.** `pkg undo n` is legal only when *n* is the most recent restorable transaction;
further back means undoing repeatedly. Out-of-order undo asks for "the system as if *n* never happened but
*n+1..m* did" — a set that may never have existed in the store and needs a re-solve against constraints the
system no longer has. Sequential steps are states the system provably occupied.

**No partial undo.** `pkg undo --package firefox` is **rejected** — the flag is reserved and errors with an
explanation, so the refusal is discoverable. Single-package regression already has a resolver-checked
spelling: `pkg install firefox=<old-version>` plus `pkg pin`. Partial "undo" is an unchecked new resolution,
not undo. The split: undo for "the whole last transaction was a mistake"; downgrade+pin for "this one package
should be older."

**Exclusions, stated in docs rather than discovered:**

- **glibc.** Category-1 patches are deliberately one-way; major bumps have their own machinery (§7.6:
  recovery copy, `glibc-apply --rollback`). While `/var/lib/pkg/glibc-pending` exists, undo across the
  pending-stage boundary is forbidden — two mechanisms, one source of truth each.
- **Kernels** are undoable (store dirs + `/boot` entries), but undo never touches the running kernel's modules
  and never removes the last bootable kernel — §12's existing refcount guard.
- **`/etc` is out.** Conffile state is not reverted: restoring old configs on top of newer-migrated data is
  data corruption, not recovery. `pkg undo` output states this limitation every time.

**Pins, reconciliation, schema versions.** Undo never modifies `pins.toml` (it reverts state, not user
configuration) but re-checks pins post-undo and warns without blocking — pins constrain future resolutions;
undo is not a resolution. Reconciliation: §9.3. Undo across schema versions is **forbidden** (`schema_ver`
check). A cascade undoes as a unit (§7.9.2).

**GC interplay:** §12.

---

## 15. End-to-End Pipeline [REVISED]

```
sync      → fetch and verify signed index against trust chain (§10.5, §10.6)
  → resolve   → PubGrub graph, virtual/OR-group expansion, multilib co-selection,
                option pseudo-dependencies (§4.3); fail fast on staged-pending glibc (§7.6.4)
  → fetch     → recipe checked out at manifest commit; source tarballs verified against
                manifest SHA-256 before any build (§10.5.5); declared origins only (§7.1)
  → build     → source build (host glibc); bubblewrap sandbox, empty network namespace (§7.1);
                generated dep-env.sh; RUNPATH at link time; runs UNLOCKED (§9.4);
                critical set built twice, mismatch aborts (§10.5.6)
  → verify    → static: sonames + GLIBC floor/ceiling + ISA notes; link_interface_hash (§7.7.2);
                heuristic: dlopen string-grep + known plugin-config parsing.
                Undeclared linkage BLOCKS source builds (exit 3); floor/ceiling and
                binary-blob findings report and record (§8)
  → package   → tree normalization (relocatable .pc rewrite, .la stripping — §7.10);
                versioned directory in .store/; stable symlink
  → stage     → [exclusive lock] re-validate resolution inputs, re-solve once on drift (§9.4);
                full transaction set staged; /etc analysis recorded; glibc staged for atomic exchange
  → install   → atomic core: symlink swaps + /etc updates + DB commit (last; includes generation
                record, §14.1) [lock released]
                best-effort: post_install hooks, initramfs regeneration (§13.6) — reported
                separately. (DKMS rebuilds, §7.8, joins this stage when implemented.)
```

`sync` is shown as an explicit first stage, though as noted in §10.6, most commands operate against the cached
index rather than triggering it implicitly. A **cascade** (§7.9) runs this pipeline as N per-package
transactions in DAG order.

---

## 16. CLI Specification

### 16.1 Core Commands [REVISED]

```
pkg sync
    Fetch and verify the latest repository index. See §10.6.

pkg install <n>[@<version>][+<abi>] [--opt <name=value>]... [--no-repro-check]
    Install a package. Always a source build (§10.2); the personal artifact
    cache (§7.9.5) may satisfy it transparently.
    --opt           Transaction-scoped option override (§4.3); not persisted.
    --no-repro-check  Skip the critical-set double build (§10.5.6).
    Exit: 0 (success), 1 (resolution failure), 2 (build failure), 3 (gate failure —
    undeclared linkage on a source build, §8)

pkg install <n>=<version>
    Install a specific version. Recovery path, not rollback. Overrides a pin
    for this transaction only (§5.6.1). Always a source build.

pkg build <n>[@<version>] [--no-cache]
    Force source build.

pkg remove <n>
    Remove a package. Refuses by default if other installed packages depend
    on it. See §16.6.
    --recursive     Also remove reverse dependents.
    --force         Remove only the named package, ignoring dependents.

pkg autoremove
    Remove packages that were pulled in only as dependencies and are no
    longer required. See §16.6.

pkg upgrade [--all | <n>] [--opt <name=value>]...
    Upgrade all or named packages.
    --dry-run       Print the full plan before any fetch (§7.9.4).
    --skip-rebuilds Defer cascade rebuilds; guarded for security (§7.9.4).
    --defer-security=I-understand
    --background | --attach       See note below.

pkg cancel-build          See note below.
pkg cascade --at <time>   See note below.

pkg search <query>
    Search repository index.

pkg info <n>
    Show installed version, ABI tag, option deltas from recipe default,
    dependencies, files, and provenance/reproducibility status.

pkg why <n>
    Show dependency chain explaining why this package is installed.

pkg explain <constraint>
    Show PubGrub explanation for unsatisfiable constraint (including
    option conflicts, §4.3).

pkg alternatives list
    List virtual packages and current preferred providers.

pkg alternatives set <virtual> <provider>
    Set preferred provider for future resolutions.

pkg rebuild --affected
    Rebuild packages whose dependencies changed due to provider switching.

pkg check <name|path>
    Run scanner diagnostics on a binary. Searches $PATH if name given.
    Scans binaries for missing libraries; does NOT detect absent dependency
    packages (§8).
    --verbose       Show technical details
    --depth=minimal|standard|deep   Scan depth
    --nss           Validate NSS configuration
    --suggest       Suggest packages for missing libraries

pkg isolate <path> [args...]
    Run a binary in a temporary mount namespace with controlled LD_LIBRARY_PATH.

pkg build-env <n>
    Print generated build-time environment (dep-env.sh) for installed version.
    (Renamed from the v11 `pkg env`, §16.7.)

pkg build-env --resolve <n>
    Print generated build-time environment for hypothetical resolution.

pkg env create|list|show|activate|update|export|delete
    Named user-facing environments. See §16.7.

pkg audit
    Check installed packages against the security advisory index. Flags running
    processes using vulnerable library versions (inode-based, §16.5).
    --json              Machine-parseable output
    --glibc             List PIDs/services holding deleted glibc inodes and the
                        exact `rc-service <name> restart` commands (§3.7)
    --supply-chain      Attestations, reproducibility, provenance, fetch-origin
                        drift, repinning events (§10.5.6)
      --critical        Filter to the critical set
      --acknowledge <package>   Acknowledge a hard finding
    --env <name>        Environment staleness scope

pkg apply-pending-restarts
    Restart services affected by a glibc category-1 patch, in dependency order.
    Unmapped processes are listed, not touched (§3.7).

pkg clean [--garbage] [--cache]
    Prune unreferenced versioned directories. Generation-aware (§12).

pkg history [--json] [<gen>]
    Show transaction generations; cascades appear as one logical operation.

pkg undo [n] [-y]
    Undo the most recent restorable transaction (§14.1). Prints the computed
    undo set; confirms unless -y. `--package` is reserved and errors with an
    explanation. Output always states that /etc is not reverted.

pkg profile list
    List available resolution profiles.

pkg profile apply <n>
    Apply a resolution profile for next upgrade.

pkg pin <package> [<version>] [--mode=exact|abi-track]
    Pin a package to a specific version or ABI track. See §5.6.1.
    If no version given, show current pin.

pkg unpin <package>
    Remove pin.

pkg key list | show-chain | trust-root <file>
    Manage trust chain state. See §10.5.4.

pkg bootstrap --verify-layout
    Run the bootstrap same-filesystem check (§7.6, §18.4).

pkg db check
    Verify database integrity.

pkg db regenerate
    Rebuild database from installed packages.
```

**Note on `--background`, `--attach`, `pkg cancel-build`, `pkg cascade --at <time>`.** The v14 delta lists
these in its consolidated CLI surface (alongside `--dry-run` and `--skip-rebuilds`, which it specifies in
§7.9.4) but does not define their behavior beyond their names. They are carried here as named; their semantics
are an open item (Appendix A).

### 16.2 Global Options

(Unchanged from v10.)

### 16.3 Interactive Prompts for Major Actions

(Unchanged from v10.)

### 16.4 Security Enforcement Mode

(Base text unchanged from v10 — `enforce-restart` config, blocking behavior, remote-admin operational
warning. Amended as follows.)

**Index freshness gate** *(v11 §10.6; widened by v12 Δ §8)*. Under `enforce-restart = true`, the gate **blocks
installs AND audits** — the blocking surface is: `pkg install`, `pkg upgrade`, `pkg audit`, and any operation
consuming the advisory index. v11 implied install-blocking only, which would let `pkg audit` report a clean
bill of health against advisories it already knows are stale.

**Supply-chain hard findings** *(v14 Δ §4)*: a hard finding from `pkg audit --supply-chain` blocks `pkg
install`/`pkg upgrade` until acknowledged — the same blocking surface (§10.5.6).

### 16.5 `pkg audit` Process Flagging (Inode-Based)

(Unchanged from v10. Extended for glibc daemons by `pkg audit --glibc`, §3.7.)

### 16.6 Reverse-Dependency Protection *(v11, amended by v12 Δ §7, §8)*

`pkg remove <name>` refuses by default if any other installed package depends on it, reusing `pkg why`'s
existing explanation machinery to show the blocking chain rather than building a second explanation path:

```
$ pkg remove openssl

error: cannot remove openssl — required by 12 installed packages.
  Blocking dependents (showing direct, use 'pkg why openssl' for full chain):
    curl, git, python3, ...

Use 'pkg remove --recursive openssl' to also remove packages that depend
on it, or 'pkg remove --force openssl' to remove only this package and
leave dependents in a broken state (not recommended).
```

- `--recursive`: removes the named package and every reverse dependent, after displaying the full set and
  requiring confirmation (or `-y`).
- `--force`: removes only the named package regardless of dependents. The warning text references the **DB
  dependency check** as the mechanism that flags the resulting broken dependents — *not* `pkg check`, which
  scans binaries for missing libraries and does not detect absent dependency packages (§8).

`pkg autoremove` removes packages with `installed_explicitly = 0` (§9.2) that are no longer required by any
explicitly-installed package's dependency closure, showing the candidate list and requiring confirmation before
proceeding.

**Autoremove under virtuals.** When a dependency is satisfiable by **multiple installed providers** of a
virtual, the provider kept is the **currently preferred one per `alternatives.toml`**; the others are
autoremove candidates. Anything stricter would pin obsolete providers in place forever after an alternatives
switch; anything looser could remove the provider the user just asked for.

**Migration safety.** Packages lacking `installed_explicitly` are seeded as explicit on any future DB migration
(§9.2), so autoremove then proposes *nothing* rather than *everything*.

### 16.7 Named Environments *(v14 Δ §3)*

**Rename first:** the v11 `pkg env` (print the build-time dep-env.sh) becomes **`pkg build-env`** — a one-line
CLI change, pre-1.0 — freeing the name for the user-facing feature.

An environment is a named, exportable manifest materialized as a symlink tree under
`/var/lib/pkg/envs/<name>/` — the per-job scoped dependency tree the build orchestrator already constructs
(§7.4), persisted and user-facing. `pkg env create <name> [--from manifest.toml]` resolves through the normal
PubGrub machinery (pins, profiles, §4.3 option selections apply), builds missing store dirs as ordinary source
builds, records resolved 5-tuple identities in the manifest lock, and symlinks the selected FHS trees into the
env root — the same symlink-tree code path as system activation, rooted elsewhere.

**Store sharing.** Env packages share the system store: an env package at a system package's identity is a
second set of symlinks to one store dir, not a rebuild, and pins that dir against GC exactly like a generation
(§12). `pkg env delete` releases pins.

**Activation is PATH-priority — intended semantics, not an accident.** No container, no mount namespace:
`eval "$(pkg env activate media)"` prepends the env's `bin` to `PATH` (plus `MANPATH`, `PKG_CONFIG_PATH`,
`XDG_DATA_DIRS`). Because pkg-managed binaries carry RUNPATH into the store (§7.1), env binaries find their own
libraries without `LD_LIBRARY_PATH` games. An env is precisely "these tools win for this shell." What is
refused is *store conflict*: two identities of one package in one env is a create-time resolver error — §4.3's
single-assignment rule.

**CLI:** `pkg env create | list | show | activate | update | export | delete`. `export` emits manifest plus
resolved identity lock for sharing and CI.

**Deferred: rootless envs.** Every env writes under `/var/lib/pkg`, shares the root-owned store, and takes the
§9.4 lock. Rootless requires per-user stores (duplicate builds, no sharing — the main benefit gone) or
setuid/daemon mediation (violates §2). Deferred until someone makes that trade explicitly; the manifest format
is portable so the restriction can lift without a format break.

---

## 17. Implementation (Rust)

(Unchanged from v10 — language choice, build targets, crate architecture, musl vs. glibc clarification.)

---

## 18. Bootstrap & Self-Hosting

### 18.1 Static Binary Distribution

(Unchanged from v10.)

### 18.2 Bootstrap Glibc Acquisition [REVISED]

`pkg bootstrap` fetches a prebuilt, signed glibc binary tarball from the distribution repository. It is
verified against the trust chain specified in full in §10.5 (root → repository key → signed artifact). This
tarball is a **permanently imported external artifact — the one exception to source-only** (§10.2): no
toolchain exists on a bare system to compile glibc. Bootstrap also performs the same-filesystem verification
(§7.6) and **refuses** if the layout is non-conforming — there is no copy fallback (§7.6.0).

**Amendment *(v15 Δ §6)*: the reference path builds glibc from source.** The "no toolchain on a bare system"
premise holds only for a system with *no host at all*. The demonstrated bootstrap (live media → `PKG_ROOT`
install → chroot — exactly what `e2e/ports.sh` proves end-to-end) borrows the *host's* toolchain for the
first builds: a stage-1 "farm" of host tools serves the sandbox until the tree's own sys-devel packages
(binutils, gcc, make, …) exist, and glibc is recipe-built like anything else. The packaged gcc then compiles
C and C++ inside the packaged root — self-hosting is proven, not assumed. The prebuilt-tarball exception
above stays specified for hosts where borrowing a toolchain is impossible, but it is no longer load-bearing:
nothing in the reference flow requires a prebuilt glibc.

### 18.3 Cross-Installation

(Unchanged from v10.)

### 18.4 Layout Relocation Procedure *(v13 Δ §9)*

The same-filesystem requirement (§7.6) is enforced, not worked around — but "fix your layout" previously had
no recipe attached. Documented supported procedure for relocating `/usr/lib/pkg`:

1. Stop all `pkg` activity; verify no transaction lock is held.
2. `cp -a /usr/lib/pkg /mnt/target/usr/lib/pkg` onto the destination filesystem (preserving permissions,
   xattrs, and symlinks).
3. Fix up the three anchor symlinks (`/lib64`, `/lib`, `/usr/include`) and any `/usr/lib/pkg`-rooted symlinks
   elsewhere to the new location.
4. Rename the old tree aside (do not delete until step 5 passes).
5. Re-run the **bootstrap filesystem check** (`stat()` both parents of the swap pair, confirm matching
   `st_dev` — the §7.6/§18 check, now exposed as `pkg bootstrap --verify-layout`).
6. On success, remove the old tree; on failure, swap back.

The anchor/stage pair's same-filesystem invariant is re-verified at the end rather than assumed from the copy,
because a botched relocation that splits the pair across filesystems is exactly the failure §7.6 refuses to
degrade into.

---

## 19. Build Phases (Implementation Roadmap) [REVISED]

| Phase | Scope | Validation Milestone |
|---|---|---|
| **1. MVP** | Manifest format, PubGrub resolver, single-track source builds, generated `dep-env.sh`, basic SQLite db, atomic per-package install, `/var/lib/pkg/lock` (§9.4) | `pkg install hello` from source works end-to-end |
| **2. Correctness** | Static ELF scanner, GLIBC floor/ceiling post-build verification (§8), ABI tagging, link-time RUNPATH | Scanner catches undeclared deps |
| **3. Transactions, GC & Multilib** | Multi-package staging/single-activation, conffile tracking, reference-checked `pkg clean`, i686 multilib, resolver co-selection, `installed_explicitly` tracking + `pkg remove`/`autoremove` (§16.6) | `pkg install steam` pulls full i686 closure; Steam runs |
| **4. Trust & advisory infrastructure** | Bubblewrap sandboxing, root/repository/timestamp key roles (§10.5), recipe manifest verification (§10.5.5), `pkg sync` (§10.6) | Signatures verified against full trust chain |
| **5. Heuristic Scanning** | dlopen string-grep, plugin-config parsing, `pkg check` polish, `pkg isolate` | `pkg check /opt/steam/steam` produces useful hints |
| **6. ISA & Ecosystem** | `glibc-hwcaps`, multi-versioned packages, package creation tooling, repo server + curated profiles, recipe lint CI, `pkg audit` | Security advisories queryable; AVX2 packages run on v2 CPUs |
| **7. Hardware Enablement — DEFERRED post-v1** | DKMS integration (§7.8), Kernel-Module sandbox profile, per-kernel `linux-headers` tracking | `nvidia-driver` rebuilds automatically across a kernel upgrade |

*Removed from earlier phases (v12 Δ §1):* the binary repo, binary-repo format, Tier 1 reproducible-build gate
(Phase 4), and the version-retention policy and Tier 2 reporting (Phase 6). The trust chain and `pkg sync`
remain in Phase 4.

*Not yet phased:* the v14 features (build options, undo, environments, supply-chain trust, survivable
cascades) and the v13 glibc hardening were specified without roadmap placement. See Appendix A.

---

## 20. Explicit Non-Goals [REVISED]

- No systemd dependency anywhere in the toolchain
- No *conditional* dependency graphs; **options are additive-only and statically declared** (§4.3). No
  USE-flag-style option-logic matrices
- No arbitrary `CFLAGS`/`RUSTFLAGS` injection and no arbitrary configure args (§4.3)
- No content-addressed store / no Nix-style store paths
- No monolithic atomic base system
- No musl support as the distribution C library (the package manager binary uses musl for static linking; the
  distribution uses glibc)
- No *system-level* rollback and no package-level rollback (see §14.0); transaction-level **undo** is
  supported (§14.1), with no partial undo, no out-of-order undo, no `/etc` rollback
- No binary distribution track (deferred indefinitely, §10.2); no binary mirrors
- No restricted/sandboxed build language — bash recipes retained, RCE risk mitigated via bubblewrap sandboxing
  with no build-stage network (§7.1), the two-attestation critical-set rule and local double-build (§10.5.6)
- **No custom out-of-tree module build system.** DKMS remains the actual compiler/build orchestration tool for
  kernel modules; the package manager's role is triggering, sandboxing, and dependency tracking around DKMS
  invocations (§7.8, deferred post-v1), not reimplementing DKMS
- No runtime package manager dependency on the host's glibc
- No jail/container functionality in the package manager (see §16.1, `pkg isolate` is a library-path
  namespace, not a jail; `pkg env` is PATH-priority, not a container, §16.7)
- No per-user/rootless installs or environments; no distributed builds; no third-party recipe repos or
  per-user fetch trust (one trust root, one recipe repo)
- No `--allow-unsafe-copy` or any other opt-in non-atomic glibc fallback (§7.6.0)
- Out of scope for supply-chain defense: compromised host root/kernel, trusting-trust compiler backdoors,
  payloads that survive diff review (§10.5.6)

---

## 21. Documented Design Decisions [REVISED]

| Decision | Justification |
|---|---|
| Glibc updates require reboot only for ABI-breaking bumps | Security patches are ABI-compatible and can be applied inline; major bumps cross an ABI boundary that requires atomic replacement of the entire C library tree. |
| Single fixed-name glibc anchor, swapped as a whole | A multi-directory swap is not collectively atomic. One directory, one `renameat2` call, no partial-swap state is reachable. |
| Mandatory initramfs | PID 1 is dynamically linked against glibc and cannot perform or survive the swap that replaces it. |
| **Same-filesystem sibling staging, enforced; no copy-and-reboot fallback** *(v12)* | `RENAME_EXCHANGE` requires same-filesystem. Bootstrap and glibc major upgrades refuse otherwise. An opt-in flag whose failure mode is an unbootable system is a loaded foot-gun, not autonomy. §18.4 documents the supported fix. |
| Activation split into atomic core + best-effort hooks | SQLite `COMMIT` is irreversible. Hooks, initramfs regeneration (and DKMS rebuilds, once implemented) are inherently non-atomic and idempotent; separating them from the core keeps atomicity claims honest. |
| Transitive rebuild cascade | The price of Nix-grade correctness. The stable symlink eliminates rebuilds for patch updates; the cascade only triggers on ABI-breaking changes — and, since v14, only where the consumed link interface actually changed. |
| Stable glibc baseline + Gentoo-inspired major version upgrades | Default is stable and zero-config; opt-in path exists for newer features. |
| No *system-level* rollback | Bounded complexity. Filesystem snapshots are the correct Unix tool for system-level rollback. |
| Zero runtime overhead | No daemons, wrappers, or FUSE mounts. Installed binaries execute directly via the kernel's dynamic linker. |
| musl bootstrap binary | Pragmatic toolchain choice for a static, self-contained installer. The system libc remains glibc. |
| Bootstrap glibc via prebuilt tarball — **the one permanent external artifact** *(v12)* | No toolchain exists yet on a bare system to compile glibc. This is why the trust chain and sync are retained under source-only. |
| NSS module resolution out of scope for binary scanner | NSS modules are resolved by glibc at runtime, not `dlopen()`'d by the calling binary — no static signal exists to detect. |
| Category-based hook sandboxing | Hooks are the highest-risk surface for malicious or buggy packages; sandboxing by category constrains them without removing functionality. |
| Service management is user responsibility | Matches FreeBSD pkg behavior. The package manager installs files; the user decides what runs. (`pkg apply-pending-restarts` is a user-invoked command, not a daemon.) |
| Network source declarations in recipes → no build-stage network, mandatory vendoring *(v14)* | Fetch is a separate sandbox restricted to declared origins; the build runs in an empty network namespace. Recipes that can't express fetches as pinned sources are rejected at lint. |
| `pkg check` / `pkg isolate` naming | Reflects actual scope — checking any binary, and a temporary library namespace rather than a full jail. |
| Database corruption detection with regeneration | Offering regeneration from `.store/` manifests ensures recoverability without reinstalling. Doubles as a backstop for failed transaction commits. |
| Inode-based process auditing for `pkg audit` | The glibc anchor path is fixed; inode comparison is the only reliable signal distinguishing old from new glibc processes. |
| PM-native initramfs regeneration | A distro-breaking mistake if any individual kernel recipe author gets it slightly wrong. The PM owns the canonical static binaries. |
| `ldconfig` only for non-pkg binaries | pkg-managed binaries use explicit `RUNPATH`; `ldconfig` is a legacy compatibility measure, not a core mechanism. |
| Root/repository/timestamp (+ reviewer) key role split | A single unspecified signing key has no rotation story. Threshold-signed root keys, embedded at compile time, let repository keys rotate without a new `pkg` release except under full root compromise — an inherent limit of any root-of-trust design. |
| Mirrors are transport-only, never individually trusted | The signature chain is what's actually trusted; any mirror can serve the same signed artifacts. |
| **Source-only track; binaries deferred indefinitely** *(v12)* | Removes the binary-vs-source choice and the retention/reproducibility-tier machinery that had no purpose without published binaries. |
| **Signed recipe manifest** *(v12)* | The attack surface under source-only is the recipe repo and fetched tarballs. The same "transport-only" argument applied one layer deeper: the signed index is what's trusted. |
| **Lock covers only the critical section; re-solve once, then fail** *(v12)* | Hours-long builds under an exclusive lock buy no correctness. Re-validation on acquisition bounds the retry loop. |
| Security-enforcement mode gated on index freshness; blocks installs *and* audits *(v12)* | Enforcement mode's value depends entirely on `security.json` being current. A stale `pkg audit` reporting clean would be worse than blocking. |
| `pkg remove` refuses by default; `installed_explicitly` tracks true origin; migration seeds explicit *(v12)* | Matches established behavior in comparable tools. A false negative leaves disk usage; a false positive removes the system. |
| Autoremove keeps the `alternatives.toml`-preferred provider *(v12)* | Stricter pins obsolete providers forever; looser could remove what the user just asked for. |
| `abi-track` pin mode, opt-in; never downgrades; single `--mode` flag syntax *(v11, v12)* | Generalizes the glibc security-vs-major split to any package. Defaulting to it would silently change what a pin means. |
| DKMS deferred post-v1; canonical `[dkms]` table form *(v11, v12)* | Design of record retained; follow-ups (depmod scope, `/var/lib/dkms` ownership, module signing, per-kernel edge schema) recorded to avoid rediscovery. |
| ELF floor/ceiling check is post-build verification, not an install gate *(v12)* | Under source-only, floor ≤ ceiling holds by construction; the real failure is a recipe escaping its sandbox. |
| **Frozen `glibc-apply` interface; unknown protocol → no-op boot** *(v13)* | The initramfs copy ages independently of the PM. Minimal interface degrades to "refuses to act," never "acts wrongly." |
| **Drift check is advisory, not blocking; regeneration hook uses existing §7.2 machinery** *(v13)* | Blocking boot on a hashing disagreement is worse than the drift it detects; no new mechanism introduced. |
| **Latest-staged-wins during the pending window; pending state also in DB** *(v13)* | The staged tree is dormant, so overwriting is safe; DB state enables fail-fast rejection before multi-hour builds. |
| **`glibc-apply` never swaps unverified content** *(v13)* | A tree that doesn't match its manifest is indistinguishable from corruption or tampering; old glibc intact is strictly better than a partial boot. |
| **Recovery copy retained until post-boot verification passes; `pkg clean` refuses on version mismatch** *(v13)* | A booted-into-old-glibc system's "recovery" copy is the live system; removing it is fatal. |
| **glibc `abi_tag` base case from `release_core`** *(v13)* | Category-1 patches never perturb the cascade; a major bump perturbs it exactly once. |
| **Declared, bounded build options** *(v14)* | Customization demand pushed into invisible recipe forks is worse than declared, hashed, CI-visible variance. Additive-only, no logic matrices, widen-else-conflict. |
| **Transaction-level undo via derived generations; sequential only; no partial undo** *(v14)* | Reuses the stage→activate path; steps are states the system provably occupied. Partial undo is an unchecked new resolution. "Undo" and "rollback" stay verbally distinct. |
| **Named environments are PATH-priority over the shared store** *(v14)* | Reuses the per-job dependency-tree machinery; no container semantics; GC reference set extended to envs. |
| **Critical-set two-attestation at index-signing time; local double-build by default** *(v14)* | Hardening lands where a hash enters the index. ~50 packages is a bounded cost; the default must be the safe direction. |
| **Link-interface hashing; per-package cascade activation; uncertainty widens the cascade** *(v14)* | Stability is measured, not assumed. A failure at package 387 of 400 must not discard 387 builds. Cascade set stays a superset of "actually broken." |
| **Personal artifact cache, not a binary repository** *(v14)* | Content-addressed reuse of local builds only: no index, no cross-host semantics. Covers the legitimate need without contradicting source-only. |
| **Deps bound twice: real store paths + `/pkg/deps/<name>` aliases; closure-complete** *(v15)* | Baked-in RUNPATHs resolve through the real paths; recipes reference stable aliases without globbing identity-hashed dirs. A build tool's own runtime deps must resolve, so the bind set is the full closure, not the declared list. |
| **Dep-provided tools precede the seed toolchain on PATH; search-path env over bound deps** *(v15)* | Recipes are written and tested against the packaged tools. `C_INCLUDE_PATH`/`LIBRARY_PATH`/`PKG_CONFIG_PATH` let a plain `#include` or `pkg-config` find declared deps without hand-assembled flags; the §8 gate verifies the linkage was declared. |
| **`.la` stripping and relocatable-`.pc` rewrite are pipeline passes, not per-recipe chores** *(v15)* | Both fix cross-package path assumptions upstreams bake in; doing them centrally means recipes stay three-line `configure && make && make install` for the common case. |
| **Python payloads precompile bytecode at package time** *(v15)* | Activation symlinks point into the store; runtime `__pycache__` writes would pollute it and trip `db check`. |
| **Upstream tracking is per-recipe (`watch.toml`) with an explicit `stable_regex`** *(v15)* | "Latest" and "latest stable" differ per project; the filter is declared per recipe, and `--apply` produces a complete reviewable bump (version + hash) through the normal attestation path. |
| **glibc is recipe-built in the reference bootstrap; stage-1 borrows the host toolchain** *(v15)* | Live media always has a toolchain to borrow; the prebuilt-glibc exception survives for hosts where it truly doesn't, but nothing in the reference flow needs it. |
| **Undeclared linkage is a hard gate failure (exit 3); foreign-glibc findings report and record** *(v15)* | A binary linked against an undeclared library breaks the graph every other mechanism relies on; a floor/ceiling note under source-only is informational by construction. |

---

## Appendix A — Merge Notes (editorial decisions and open items)

This appendix is not part of the specification. It records where consolidating v11 + v12 + v13 + v14 required a
judgment call, so you can overrule any of them.

**A.1 Missing base text.** Everything marked "(Unchanged from v10)" is a placeholder, because v10 wasn't among
the files. This includes the full base text of §1–§3, §5.1–5.6, §7.1–7.7, §8, §9.1/9.3, §10.1/10.3/10.4, §11,
§12, §13, §14, §16.2–16.5, §17, §18.1/18.3. Notably **§7.6 (glibc special case) exists here only as amendments**
— the categories, anchor model and swap mechanics are in v10.

**A.2 Renumbering and placement.** Delta-internal references ("§2 below", "§1's single-assignment rule") were
rewritten to merged section numbers. Placement of delta content: v12 §2 → §10.5.5; v13 §1–6 → §7.6.1–7.6.6;
v13 §7 → §3.7; v13 §8 → §7.7.1; v13 §9 → §18.4; v14 §1 → §4.3; v14 §2 → §14.1; v14 §3 → §16.7; v14 §4 →
§10.5.6 + §7.1; v14 §5 → §7.7.2–7.7.3 + §7.9. v14's §5 ("Survivable Rebuild Cascades") sub-numbers 5.2–5.7 map
to §7.9.1–7.9.6 and the recorded refusals §7.9.7.

**A.3 Schema inferences.** (a) `UNIQUE` on `packages` now includes `opt_tag`, following v14's "identity
becomes the 5-tuple." (b) v14 says the DB "records a `cascade_id`" without naming the table; §9.2.1 describes
it as a tagging field rather than adding a column to a specific table. (c) v14 specifies column *contents* for
`provenance` and `build_history` but no DDL, and nothing for `package_options` beyond its name — these are
described, not defined.

**A.4 Possible tension: glibc `abi_tag`.** v13's base case is `hash("glibc-" + release_core)`; v14's general
formula is `hash(name, version_core, {link-interface hashes}, option set)`. Both are retained as written. They
aren't literally the same expression for glibc, and neither delta says which governs — worth a one-line ruling.
**Ruled at v15 (§7.7.1): the v13 base case is the v14 formula evaluated at glibc's empty link-dep set.**

**A.5 Exit code 3.** §16.1 still lists `3 (gate failure)` from v11, but v12 reframed the gate as
report-and-record rather than install-blocking. What now produces exit 3 is unspecified.
**Ruled at v15 (§8, §16.1): undeclared `DT_NEEDED` linkage on a source build.**

**A.6 Lock writers.** v12 names `pkg sync` as a writer; v14 says envs take the §9.4 lock. `pkg undo` mutates
the installed set and reuses the activation path, so it is presumably a writer too, but no delta says so.

**A.7 Undefined CLI.** `pkg upgrade --background`, `--attach`, `pkg cancel-build`, and `pkg cascade --at
<time>` appear in v14's consolidated CLI list with no behavioral spec.

**A.8 Phase 4 scope.** v12 says the binary-repo and reproducibility-gate items leave Phase 4. It doesn't
mention the "pinned baseline container" or bubblewrap sandboxing; I dropped the former (a binary-track concept)
and kept the latter (source builds need it, §7.1). Phase placement for v13/v14 features is not specified.

**A.9 Stale v11 text corrected, per the deltas.** `dkms-module = true` (→ `[dkms]` table); `--binary-only`
(removed); §8.6 reference (→ `pkg check`); "binary-first with source fallback" (removed); `--allow-unsafe-copy`
and the "dynamic sibling-staging" §21 row (replaced); §21 retention/Tier 1/Tier 2 rows (removed); the `2.39.*`
glob pin syntax (removed).

**A.10 v15 merge notes.** v15 was authored against the implementation, not a delta document; its placements:
sandbox environment contract → §7.1; normalization passes → §7.10 (new); gate split → §8, §15, §16.1;
parallelism keys → §7.9.1; ports tree + watch → §10.9 (new); bootstrap → §18.2; init wording → header + §13.
The v15 §8 wording narrows v12's "reports and records, does not block installs": that reframing survives only
for the foreign-glibc floor/ceiling check. Where this document and the code disagree beyond these rulings,
the code is younger than the text — file it.

---

End of Specification v15 (Consolidated)
