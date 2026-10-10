# Design Fixes for Conceptual Gaps 1–5

**Status:** Design proposals (v14 candidates). Companion to pkg-spec-v11 + v12/v13 deltas and the review (`pkg-review.md`).
**Origin:** answers to "where would this package manager be conceptually lacking" — issues 1 (customization vacuum), 2 (no environments/generations), 3 (supply-chain trust for source inputs), 4 (rebuild cascades correct but not survivable), 5 (rollback assumes known failure modes). Issue numbering matches that analysis.
**Governing principle across all five:** every mechanism preserves the existing invariants — honest atomicity, statically enumerable dependency graphs, no daemons, no binary track, fail-safe defaults. Nothing below trades correctness for convenience.

---
---

## Issue 1: Declared, Bounded Build Options (Closing the USE-Flag Vacuum)

**Status:** Proposal, design-level. Amends the §20 non-goal "No USE-flag-style conditional dependency graphs" — the *conditional graph* stays forbidden; what is added is a statically declared, additive-only option layer.

### 1. Problem Restated

The system's consistency guarantee (§7.7 cascade, Tier-1 reproducibility) rests on a single silent premise: every consumer builds every package in the same configuration. The moment a user needs `ffmpeg` without X11, or `openldap` with a different SASL backend, the only escape hatch is forking the recipe. A forked recipe is invisible to everything: its `abi_tag` is computed identically to the stock recipe's, so it *collides* with the stock build in the store and DB rather than coexisting; it receives no cascade rebuilds because the resolver can't distinguish it; and Tier-1 CI never covers it. The non-goal didn't eliminate customization demand — it pushed it off the books. Bounded options bring the 90% case back under the consistency umbrella by making variance *declared, priced into identity, and visible to tooling*.

### 2. Manifest Schema (`pkg-meta.toml`)

A recipe declares its entire option set in a new `[options]` table. An option not declared here does not exist; there is no mechanism to invent options at the command line.

```toml
[options.x11]
description = "X11 client support (GTK/gdk backend)"
default = true
abi_affecting = true            # default: true; see §4 below
add_deps = ["gtk+3", "libX11"]  # ADDITIVE ONLY — see below
add_build_deps = ["wayland-protocols"]

[options.pulseaudio]
description = "PulseAudio output"
default = false
add_deps = ["libpulse"]
```

Hard rules, enforced by the manifest parser in `pkg-core/manifest.rs` (not by lint):

1. **Additive-only dependency effects.** An option may *add* dependencies when enabled. It may never remove, downgrade, or alter a version constraint of a base dependency, and it may never gate a dependency on another option's state. This is the load-bearing restriction: it is exactly Gentoo's `USE? ( dep )` conditional graph, minus the conditionals, which keeps the dependency graph statically enumerable from the TOML alone — the resolver can compute any option selection's full dep set without executing a line of bash. The §20 non-goal survives in spirit: the graph never changes *shape*, it only gains edges.
2. **No option-to-option logic.** No `required`, `conflicts`, `implies` matrices in v1. If two options genuinely conflict, the recipe is wrong and must be split into two recipes (e.g. `ffmpeg` vs `ffmpeg-headless` as separate package names sharing a source). Combinatorial constraint matrices are where USE-flag UX went to die; we refuse to import them.
3. **Base dependency set is the floor.** `[dependencies]` as today is what you get with *all options off*. Every option selection's dep set is a superset. This makes the "all-off" build the common denominator and guarantees disabling options never breaks a reverse dependent that linked against the base surface.
4. **Options must be observable in the recipe.** Each option name is exported to the recipe as `PKG_OPT_<NAME>=0|1`; what the recipe *does* with it (pass `--enable-x11`) is recipe code, as before. The PM owns the declaration and the dep effects; the recipe owns the configure plumbing. The scanner (§8) provides the honesty check: enabling an option that declares `add_deps` but produces no linkage to those deps' sonames is a Tier-1 CI warning.

### 3. Option Selection → Package Identity

An *option selection* is the full assignment of every declared option to a value. Identity is computed canonically: sort `(name, value)` pairs lexicographically, serialize as `x11=1,pulseaudio=0`, hash (truncated SHA-256, 8 hex chars, same convention as `abi_tag`). Call it the **`opt_tag`**.

Store and DB identity becomes a 5-tuple: `name-version+abitag+opttag`, with store dir `/usr/lib/pkg/.store/name-version+abitag+opttag/`. Two rules:

- **`abi_tag` composition.** `abi_affecting = true` options are hashed into the package's *own* `abi_tag` *before* dependents derive from it — i.e., `abi_tag(pkg) = hash(deps' abi_tags ++ canonical abi-affecting option set)`. Toggling an abi-affecting option therefore triggers the normal §7.7 cascade through reverse dependents, which is exactly correct: `x11=0` gtk is a different ABI than `x11=1` gtk. An option declared `abi_affecting = false` (e.g. "ship extra documentation", "enable verbose logging") changes `opt_tag` only, triggering no cascade — the bounded-cost escape valve.
- **The default selection gets the empty `opt_tag`.** A package built with all-default options has identity identical to today's pre-options identity (`name-version+abitag`). This is what makes migration a non-event (§8).

### 4. Resolver Semantics: Per-Consumer Variants — Refused

The decisive question: package A wants `foo[x11]`, package B wants `foo[-x11]`. The store layout technically supports coexistence (distinct dirs, and both could be activated into disjoint FHS paths — but they are *not* disjoint: both own `/usr/lib/libfoo.so`). Coexistence of two variants of the same package in one system set is therefore rejected: file collisions in the FHS symlink tree make it physically impossible, not merely confusing.

**Decision: conflict, with additive-widening resolution.** At solve time, the resolver tracks a single option assignment per package name across the whole system set (options are system-global per package, like alternatives preferences). When two consumers demand different assignments:

1. If the union of requested option *effects* is satisfiable as a single assignment — i.e., the conflict is only "A needs foo+x11, B doesn't care" — the resolver **widens** to the union (enabling an option is always safe per the additive rule: `foo[x11]` satisfies anything `foo[]` satisfies, since base deps are the floor and the ABI surface only grows). This is the common case and costs a few extra deps.
2. If assignments genuinely conflict (A pins `x11=1` via config, B pins `x11=0`), the resolver **fails at solve time** with a `pkg explain`-style report naming both requesters. The fix is a split recipe (§2.2), not a PM feature.

Justification over "last-wins with warning": a last-wins choice silently relinks half the system against a different ABI than it was built for — the exact invisible divergence this design exists to kill. PubGrub integration is clean: option requirements become pseudo-dependencies `foo[opt:x11=1]` versioned by the recipe's declaration; a conflict is just an unsatisfiable incompat term, so `pkg explain` works unchanged.

### 5. User-Facing Configuration

Precedence, strongest first (mirroring pins' "explicit wins" philosophy):

1. **Per-command:** `pkg install ffmpeg --opt x11=0 --opt pulseaudio=1`. Applies to that transaction; recorded in the generation log (Issue 2) but *not* persisted.
2. **Per-package persistent:** `/etc/pkg/options.toml`:
   ```toml
   [package.ffmpeg]
   x11 = false
   pulseaudio = true
   ```
3. **Global default policy:** `/etc/pkg/options.toml` `[defaults]` may only *narrow* to the recipe default (i.e., it exists so a profile can say "all packages: `nls=0`"); arbitrary global assignments are allowed but any per-package entry overrides.

Unknown option names, undeclared values, or options for packages that declare none are **hard errors at parse time**, not warnings — a silently ignored `x11 = false` is the config-file twin of the silent divergence problem.

`pkg info ffmpeg` shows the delta from default, not the full assignment:

```
ffmpeg 6.1+ab3+p9f2c1d
  Options (deltas from recipe default):
    x11        = false   (default: true)   [abi-affecting]
    pulseaudio = true    (default: false)
  Added deps from options: libpulse
```

### 6. Profiles, DB, CI

- **Profiles** (§10.3) may carry an `[options]` section equivalent to `options.toml`, lower precedence than the user's file. A `headless-server` profile is then just a profile with opinions, no new mechanism.
- **DB schema:** `packages` gains `opt_tag TEXT NOT NULL DEFAULT ''` and the UNIQUE constraint becomes `(name, version, abi_tag, opt_tag, arch)`. New table `package_options (package_id, name, value, abi_affecting)` records the full assignment (not just deltas) so `pkg db regenerate` can reconstruct it from the store manifest, which also records the assignment.
- **CI/reproducibility:** Tier 1 builds the **default selection** and the **all-on selection** (the two extreme corners), doubling gate cost only for packages that declare options. Tier-2 attestations must record the option assignment alongside the recipe commit (§v12-2 recipe manifest gains the selection). Combinatorial middle points are uncovered, accepted, and documented: the option count per recipe is expected ≤ ~6 by convention, enforced softly by recipe lint.

### 7. What Stays Forbidden, and Why

- **Arbitrary `CFLAGS` / `RUSTFLAGS` injection.** The toolchain flags are part of the distribution's ABI contract (ISA baseline, hardening flags); user CFLAGS reintroduce untracked variance with no declared dep surface to check against — the worst of both worlds. ISA-level variance already has its own sanctioned path (`glibc-hwcaps`, §19 Phase 6).
- **Arbitrary configure args.** Anything reachable only by raw args is, definitionally, an undeclared option. If a configure knob matters, the recipe declares it as an option with its dep effects spelled out; if the recipe author won't, forking the recipe remains the honest path — but now it's *visibly* off-umbrella (unknown `opt_tag`, no CI coverage), which is strictly better than today's indistinguishability.

### 8. Migration & Failure Modes

Migration is nil for existing installs: default-selection builds are identity-identical to pre-options builds, so an `opt_tag = ''` backfill on `packages` (safe direction, mirroring the `installed_explicitly` seeding argument — a wrong empty tag causes a redundant rebuild, never a wrong link). Recipes with no `[options]` parse unchanged. Failure modes: (a) a recipe declares `abi_affecting = false` on something that *is* ABI-affecting — caught by the soname scanner comparing variant outputs in CI; (b) option-space creep per recipe — mitigated by lint, not mechanism; (c) a user with local options loses Tier-2 attestation coverage for their selection — surfaced in `pkg info` ("this option selection has no reproducibility attestations"), honestly, rather than hidden.

---
---

## Issue 2: Named Environments and Transaction Generations

**Status:** Proposal, design-level. Amends §14 ("Rollback: Explicit Non-Feature") in a deliberately narrow way: §14's argument was against *package-level* rollback and against the PM pretending to be a snapshot tool. Generations here do not resurrect package-level rollback; they make the already-atomic transaction *inspectable and reversible as a whole*, using machinery the system already has.

### 1. Why Now, and Why Not Nix

Every transaction already passes through the atomic core: stage → symlink swap → DB commit (§7.5 Phase B, v12 §3). The only things missing for a generation are (a) a record of what the swap changed, and (b) a reason for the old store dirs to survive. Both are cheap because the store is already immutable and versioned. We explicitly do **not** copy Nix: no content addressing, no per-profile store paths, no purity. A generation is a *bookmark over an existing imperative system*, not a reified immutable system.

### 2. Generations

**Definition.** A generation is created by every successful activation (the §7.5 atomic core). It consists of:

- the **post-transaction package set** (the 5-tuple identities from Issue 1, or name-version+abitag pre-options),
- the **activation log**: the exact list of symlink operations performed (target → old link destination, new link destination), which the activation code already computes in order to perform the swaps,
- metadata: timestamp, invoking command line, user, option selections applied.

**DB schema** (SQLite, same DB, committed in the same transaction as the package-set change — this is critical: the generation record and the state it describes are atomically consistent by construction):

```sql
CREATE TABLE generations (
    id INTEGER PRIMARY KEY,           -- generation number, monotonic
    created_at INTEGER NOT NULL,
    command TEXT NOT NULL,            -- e.g. "pkg install ffmpeg --opt x11=0"
    active INTEGER NOT NULL DEFAULT 1 -- exactly one row (or the rolled-back-to one)
);
CREATE TABLE generation_packages (
    generation_id INTEGER NOT NULL REFERENCES generations(id),
    name TEXT NOT NULL, version TEXT NOT NULL,
    abi_tag TEXT NOT NULL, opt_tag TEXT NOT NULL DEFAULT ''
);
CREATE TABLE activation_log (
    generation_id INTEGER NOT NULL REFERENCES generations(id),
    path TEXT NOT NULL,               -- FHS symlink path
    old_target TEXT,                  -- NULL = link created
    new_target TEXT                   -- NULL = link removed
);
```

**`pkg rollback <gen>` scope.** Rollback re-points every symlink in the symmetric diff between the active generation and the target (both directions' logs are in `activation_log`), then flips the `active` marker in the same SQLite transaction. This is *the same* atomic-core discipline: renames only, DB commit last. Cost: rollback is O(diff size) symlink renames, no rebuilds — because generation membership **pins store dirs** (§3 below), the target generation's packages are guaranteed present on disk.

Hard limits, stated in the docs rather than discovered:

- **glibc is excluded.** Rolling back a symlink tree does not roll back the glibc anchor swap (a boot-time `renameat2` under initramfs, v13 §1). A generation that included a glibc major bump is recorded with `glibc_floor` in its metadata; `pkg rollback` **refuses** any target generation whose `glibc_floor` differs from the running glibc, with the v13 rescue path (`glibc-apply --rollback`) named as the correct tool. Mixing the two mechanisms would give two sources of truth for which libc is live.
- **Kernels** roll back as *files* (`/boot` entries) only; the bootloader's chosen default is out of scope (it isn't PM-managed state), and DKMS modules already-built for a kernel persist harmlessly.
- **`/etc` is out of scope, deliberately.** Config files carry `conffile_state` and user edits; rolling back package code while leaving or reverting configs are both wrong answers depending on the case. Generations restore the *symlink tree and package set*, nothing else. Users wanting /etc time travel want filesystem snapshots — §14's existing answer, unchanged.
- **No future rollback.** Rolling back to gen N while generations N+1..M exist does not delete them; the next transaction creates gen M+1 as a diff against the *currently active* state. Generations are a log, not a stack.

**CLI:**

```
pkg history [--json]          # gens: id, time, command, +k/-j package counts
pkg history <gen>             # full package-set diff vs. its parent
pkg rollback <gen>            # symlink-rename reversal; refuses on glibc_floor mismatch
```

### 3. GC Interplay

`pkg clean`'s rule changes from "unreferenced by installed set" to "unreferenced by installed set **or any retained generation**". Retention policy in `/etc/pkg/pkg.toml`:

```toml
[generations]
keep_last = 10          # count bound
keep_days = 30          # age bound; a gen must fail BOTH to be collectible
```

`pkg clean` reports per-generation reclaimable space (`pkg history` annotates generations with "pinning 1.2 GiB uniquely"). This mirrors the glibc recovery-copy logic (v13 §6): retention is conservative, cleanup is manual and explainable. The disk cost is bounded by `keep_last × per-transaction churn`, typically small because unchanged packages share store dirs across generations.

### 4. Environments (`pkg env`)

**Note on naming collision:** v11 §16.1 already uses `pkg env` for "print the build-time dep-env.sh". That command is renamed **`pkg build-env`** (one-line CLI change, pre-1.0), freeing `pkg env` for the user-facing feature.

**Definition.** An environment is a named, exportable manifest materialized as a symlink tree — a first-class version of the per-job scoped dependency tree the build orchestrator already constructs (§7.4), persisted and user-facing.

```toml
# /var/lib/pkg/envs/media.toml  (or exported/shared copy)
name = "media"
packages = [
  { name = "ffmpeg", version = ">=6.1", options = { x11 = false } },
  { name = "mpv" },
]
# Resolution recorded on materialize: exact 5-tuple identities, so the
# env is reproducible from the manifest + a recipe-repo commit pin.
```

**Materialization.** `pkg env create media` resolves the manifest through the normal PubGrub machinery (pins, profiles apply), builds any missing store dirs as ordinary source builds, then symlinks the selected store dirs' FHS trees into `/var/lib/pkg/envs/media/{bin,lib,share,...}` — the same symlink-tree code path as system activation, rooted elsewhere. **Store dirs are shared with the system set**: an env package at the same identity as a system package is a second set of symlinks to one store dir, not a rebuild, and the env pins that store dir against GC exactly like a generation does.

**Activation.** Not a container, no mount namespace: `eval "$(pkg env activate media)"` prepends `/var/lib/pkg/envs/media/bin` to `PATH` (plus `MANPATH`, `PKG_CONFIG_PATH`, `XDG_DATA_DIRS`). Because pkg-managed binaries carry RUNPATH into the store (§7.1), env binaries find *their own* libraries without `LD_LIBRARY_PATH` games — the RUNPATH design pays off here. **PATH-priority override is the intended semantics**, not an accident to refuse: an env is precisely "I want these tools to win for this shell." What we refuse is *store conflict*: two different identities of the same package inside one env is a resolver error at create time, same single-assignment rule as Issue 1 §4.

**Relationship to the system set.** Envs cannot *uninstall* anything system-level and never touch FHS paths outside their root. A package present only in envs is invisible to `pkg autoremove`'s system closure analysis only if env membership is consulted — so the GC/autoremove reference set becomes "installed set ∪ retained generations ∪ all envs", with `pkg env delete` releasing pins.

**CLI:**

```
pkg env create <name> [--from <manifest.toml>]
pkg env list | show <name>          # show = resolved identities + option deltas
pkg env activate <name>             # prints shell exports; fish/posix via --shell
pkg env update <name>               # re-resolve & rebuild within manifest constraints
pkg env export <name>               # manifest + resolved identity lock, for sharing/CI
pkg env delete <name>
```

**CI use:** `pkg env create ci --from build-env.toml` gives a hermetic-enough toolchain root; combined with `--from` manifests checked into a project repo, this is the "developer environment" story without Nix's daemon or purity costs.

### 5. Deliberately Out of Scope

- **Per-user (rootless) environments in v1.** Every env writes under `/var/lib/pkg`, shares the system store (owned root), and takes the §9.4 lock. Rootless envs require either per-user stores (duplicate builds, no sharing — the main benefit gone) or a setuid/daemon mediation layer (violates "zero runtime overhead, no daemons", §2). Deferred until someone makes that trade explicitly; the manifest format is designed to be portable so the restriction can lift without a format break.
- **Env-inherited-from-env layering, activation scripts, shell hooks.** A flat env with PATH precedence covers the stated cases; layering is the Nix profile complexity trap.

### 6. Risks

- **Generation→store-pin memory leak via forgotten generations:** mitigated by the dual count/age bound and `pkg clean`'s per-generation accounting — but the policy defaults (10 gens / 30 days) must be validated against real transaction churn before freezing.
- **`activation_log` fidelity:** rollback correctness is exactly as good as the log. Mitigation: the log is generated by the same code that performs the swaps (single source), and `pkg db regenerate` learns to rebuild generation records for the *active* generation only from the store — historical generations are unrecoverable after DB loss, documented as such.
- **Env staleness:** an env frozen at old identities accrues CVEs silently. `pkg audit` gains env-scope (`pkg audit --env media`) so staleness is visible, not forbidden — consistent with "user autonomy, security accountability" (§2).

---
---

## Issue 3: Supply-Chain Trust Model for Source Inputs

**Status:** Design proposal (v14 candidate) — amends v12 §2, §10.5, §7.1/§7.2.1, §16.1

The v12 recipe-manifest decision (v12 §2) closes the transport gap: the signed index pins `package@version → {recipe git commit, SHA-256 of each source tarball}`, so mirrors and git hosts are transport-only. But the trust model still ends at "the hashes match the index." Every build executes arbitrary upstream code — configure scripts, `build.rs`, Makefile generators, vendored blobs — and xz-utils (CVE-2024-3094) demonstrated that the payload can live in the *release tarball's build machinery* (a `build-to-host.m4` injection absent from the VCS), produced by a trusted maintainer's account over months. Hash-pinning detects *substitution*; it says nothing about *content*. This section states the threat model honestly, then layers mitigations by cost and blast-radius reduction.

### 3.1 Threat Model Taxonomy

| # | Threat | Current posture (v13) | Proposed mitigation | Residual risk (honest) |
|---|---|---|---|---|
| T1 | **Malicious upstream tarball** (payload in tarball, absent from VCS — the xz pattern) | Hash pinned in signed index; content unexamined | Cross-source verification (§3.3): tarball-vs-VCS diffing for flagged-critical packages; Software Heritage / foreign-distro hash cross-check | A payload present in *both* tarball and VCS, or obfuscated past diff review, survives. Reduces the attacker pool to "can compromise upstream durably," doesn't eliminate it. |
| T2 | **Compromised upstream VCS / release infra** (tag moved, release artifact replaced post-indexing) | Index pins commit hash + tarball hash; any post-hoc change fails verification | Retained as-is; add **pin-age reporting** (`pkg audit --supply-chain` flags packages whose upstream artifact hash changed in the index after initial publication — legitimate re-releases exist, silent ones are a signal) | An attacker who controls upstream *before* the recipe maintainer first records the hash poisons the pin at the source. Detection relies on cross-checks (T1) and reproducibility. |
| T3 | **Malicious or compromised recipe maintainer** (pins a malicious tarball hash into the index; edits recipe to exfiltrate or weaken sandbox) | Single-maintainer commit + repo-role signature covers the whole recipe repo equally | **Multi-attestation** (§3.2): security-critical set requires two distinct maintainer sign-offs encoded per-entry in the recipe manifest; reviewer keys distinct from committer keys | Colluding maintainers, or one maintainer with two keys, defeat it. This is a governance control raising the cost of a single-key compromise, not a proof of good faith. |
| T4 | **Malicious recipe reviewer** (rubber-stamps a bad recipe) | N/A (v12 has no review requirement) | Reviewer diversity rule: attestor must not share employer/key-custody domain with committer for critical packages; attestation log is append-only and published (§3.2) | Honest-but-inattentive review is the norm, not the exception. Cannot be fixed technically; mitigated by making the *diff surface small* (recipe lint CI rejects sandbox-escape patterns, network declarations outside allowlist). |
| T5 | **Build-host compromise** (attacker on the user's machine or CI worker injects into build outputs outside the sandbox) | Bubblewrap sandbox contains the build itself | Host-side integrity: store-manifest verification already exists (v13 §6.3); add build-output manifest hashing recorded in the provenance log (§3.5) at stage time | A compromised root on the build host owns everything, including `pkg`. Out of scope by definition (§3.7) — the threat model assumes the host OS below the sandbox is honest. |
| T6 | **Mirror / transport attacks** (tampering, downgrade, freeze) | Fully addressed: mirrors are transport-only (v11 §10.6); timestamp role prevents freeze (v11 §10.5.1) | No change | None beyond timestamp-key compromise, which root rotation covers (v11 §10.5.3). |
| T7 | **Dependency confusion on fetch** (recipe fetches from attacker-registered namesake — typo'd domain, hijacked GitHub org, re-registered crate/npm namespace) | Fetch allowlist is DNS-based, blast-radius reduction only (v11 §21) | Recipe manifest records the **canonical fetch origin** (exact URL template + TLS-spki pin optional) at recipe-review time; `pkg fetch` refuses origins not in the manifest; fetch of VCS sources pins the commit, never a floating ref | First-come poisoning: if the recipe *initially* records the attacker's origin, pinning preserves the poison. Mitigated only by review (T3/T4) and critical-package cross-checks (T1). |

### 3.2 Multi-Attestation for Recipe Review

The recipe manifest (v12 §2) gains an optional per-entry attestation set. The manifest entry becomes:

```toml
[[package]]
name = "xz"
version = "5.6.1"
recipe_commit = "a1b2c3..."
[[package.source]]
url = "https://github.com/tukaani-project/xz/releases/download/v5.6.1/xz-5.6.1.tar.xz"
sha256 = "..."
[[package.attestation]]          # required >= 2 for critical-set packages
key_fingerprint = " maintainer-1-ed25519-fp"
sig = "..."                      # Ed25519 over (name, version, recipe_commit, source.sha256 list)
[[package.attestation]]
key_fingerprint = "maintainer-2-ed25519-fp"
sig = "..."
```

- **Attestation semantics are narrow and stated:** an attestation means "I reviewed the recipe diff and the recorded hashes against the upstream sources reachable at review time." It is not a claim of upstream benevolence. The spec must say this, or attestations become security theater.
- **Critical set:** a new `critical = true` marker on a small explicit package list (glibc, openssl, xz/zstd/zlib and other decompressors in the bootstrap path, openssh, the toolchain, `pkg` itself — target: <50 packages). For critical packages, the repository role **refuses to sign an index** containing fewer than two attestations from distinct keys, distinct from the committer. Non-critical packages require one (the committer's). This keeps the review burden proportional: two-party review for ~50 packages, normal flow for the other thousands.
- **Attestation keys** live in the trust hierarchy as a **fourth role** delegated by root: the *reviewer role*, a set of maintainer keys with an allowlist in the root-signed delegation document. This ties revocation to v11 §10.5.3's existing flow — a compromised reviewer key is revoked by root re-delegation, picked up on next `pkg sync`.
- **Enforcement point is index-signing time**, not client time: clients verify the repository-role signature on the index as today and *spot-check* attestation count on critical packages they install (cheap, fail-loud). Centralizing enforcement at signing keeps the client trust logic unchanged in shape.

### 3.3 Independent Hash Verification Paths

A hash is only as trustworthy as the path by which it was first observed. For critical-set packages, recipe CI performs **cross-source corroboration** before a version bump is eligible for attestation:

1. **Tarball ↔ VCS differential.** Fetch the release tarball and the matching VCS tag independently; unpack both; diff the trees, normalizing generated files against an explicit per-package allowlist (`configure`, `Makefile.in`, `aclocal.m4` — autotools regen output is expected to differ; `.m4` *macro* diffs and test-suite binary blobs are not). xz's payload lived exactly in the un-normalized zone: build-system macros and "test files." The allowlist is per-package, reviewed like the recipe, and a diff outside it blocks attestation eligibility with a human-readable report.
2. **External-hash cross-check.** Where available, compare the tarball hash against an independent observer: Software Heritage's archive (query by content hash — presence of the identical artifact with a first-seen timestamp predating our pin is evidence the artifact is the same one the world saw) and/or hash comparison against another distribution's published source manifests (Gentoo's `Manifest` files, Alpine's `APKBUILD` sha512sums, Nixpkgs' `fixed-output` hashes). Agreement across two distributions that arrived at the hash independently is strong evidence against a targeted-to-us substitution. Disagreement blocks attestation and files an issue, not a silent pass.
3. Results are recorded in the recipe repo as `provenance/<name>/<version>.toml` (hash observations, sources, timestamps) so `pkg audit --supply-chain` (§3.6) can report them.

These checks run in CI and inform attestors; they are **not** re-run per client build — clients trust the index. This preserves "mirrors are transport-only" while hardening the one moment that matters: when a hash enters the index.

### 3.4 Sandbox Hardening Profile for Builds

The bubblewrap build sandbox (v11 §7.1) is hardened to an explicit, auditable profile:

- **Filesystem:** read-only bind of the resolved dependency tree and toolchain; read-write *only* the per-build scratch dir (`/var/lib/pkg/build/<pid>`) and `$PKGDIR`. No `/etc` write, no home directory, no `/var/lib/pkg` except the scratch dir, no access to the recipe repo's `.git` (recipe is materialized into the sandbox as a plain checkout at the pinned commit).
- **Network:** **none by default at build time.** Fetch happens in a *separate* earlier sandbox stage (`pkg fetch`) whose only write target is the source cache, with network restricted to the recipe manifest's declared origins (T7). The build stage runs with an empty network namespace (`--unshare-net`). Legitimate build-time fetchers (cargo, npm, go modules) are handled by declaring their inputs as additional pinned `[source]` entries in the manifest — i.e., vendoring is enforced at the manifest level, so the build itself never needs the network. This converts "DNS allowlist as blast-radius reduction" into "no runtime network, ever" for the build proper. Recipes that cannot express their fetches as pinned sources are rejected at lint time.
- **Syscall surface:** seccomp-BPF profile blocking (at minimum) `ptrace`, `mount`, `pivot_root`, `bpf`, `perf_event_open`, `kexec_load`, `init_module`/`finit_module`, `io_uring_setup` (high historical CVE density and unnecessary for builds), and user-namespace creation *inside* the sandbox (the sandbox itself uses a userns; nested ones are denied to remove a kernel attack-surface multiplier).
- **Identity:** build runs as a dedicated unprivileged UID inside a user namespace mapped to nobody-on-host; no setuid binaries in the sandbox; `no_new_privs` set.
- **Resource limits:** cgroup v2 CPU/memory/pids caps (fork-bomb and OOM containment), wall-clock timeout per phase.
- The full profile is a versioned data file (`/usr/lib/pkg/sandbox/build.profile`) shipped with `pkg`, so `pkg audit --supply-chain` can report which profile version built any given package.

### 3.5 Provenance Log

Every installed artifact gains an append-only provenance record in the DB:

```sql
CREATE TABLE provenance (
    package_id      INTEGER NOT NULL REFERENCES packages(id),
    recipe_commit   TEXT NOT NULL,
    source_hashes   TEXT NOT NULL,        -- JSON: [{url, sha256}], exactly as verified at fetch
    index_version   INTEGER NOT NULL,     -- which signed index authorized this build
    attestations    TEXT,                 -- JSON: fingerprints present in the manifest entry
    build_host      TEXT NOT NULL,        -- hostname + uname snapshot
    builder_version TEXT NOT NULL,        -- pkg version + sandbox profile version
    started_at      INTEGER NOT NULL,
    finished_at     INTEGER NOT NULL,
    reproducibility TEXT,                 -- NULL | 'self-match' | 'mismatch' (§3.5a)
    FOREIGN KEY (package_id) REFERENCES packages(id) ON DELETE CASCADE
);
```

This answers "who/what/when for every installed artifact" locally and gives `pkg audit` the raw material to answer "which installed packages were built from a recipe commit later implicated in an incident" — the retrospective question that matters after the next xz.

**§3.5a Reproducible-build self-check (local-only).** With no binary repo (v12 §1), reproducibility verification degenerates to a local double-build: for critical-set packages, `pkg build` (and `pkg install` of critical packages) **by default builds twice** — two sequential sandboxed builds with `SOURCE_DATE_EPOCH` pinned and the same normalization rules v11 §10.8 Tier 1 defined — and compares output tree hashes. On mismatch: the install **aborts** (critical set: hard fail), both trees are retained under `/var/lib/pkg/build/quarantine/` with a diff report, and the event is recorded in provenance (`reproducibility='mismatch'`). On match: record `'self-match'` and proceed. A mismatch means either non-determinism (bug, report upstream) or a build that responds to environment — which is how build-system payloads behave. Double-build doubles critical-package build time; it's opt-out (`--no-repro-check`) rather than opt-in, since ~50 packages is a bounded cost, but the default must be the safe direction per v12 §7's seeding precedent.

### 3.6 `pkg audit --supply-chain` Surface

New subcommand extending `pkg audit` (v11 §16.1):

```
pkg audit --supply-chain
    Per installed package, report: attestation count vs. requirement (critical
    set), reproducibility self-check result, provenance completeness, fetch-origin
    drift (T7), and index-hash repinning events affecting installed versions (T2).
    --json         Machine-parseable output.
    --critical     Restrict to the critical set.
    Exit: 0 clean, 1 warnings (e.g. unaudited provenance from pre-v14 installs),
    2 hard findings (mismatch quarantine present, critical package with <2 attestations).
```

Interaction with enforcement mode (v11 §16.4, v12 §8): a hard finding (exit 2) blocks subsequent `pkg install`/`pkg upgrade` until acknowledged via `pkg audit --supply-chain --acknowledge <package>` — same blocking surface as stale-index enforcement.

### 3.7 Governance Layer and Explicit Non-Goals

**Governance.** Recipe repo ACLs: commit rights restricted to keyholders in the root-delegated reviewer role; critical-set paths additionally restricted to a named subset; force-push disabled; the index-signing CI is the only writer of the signed recipe manifest, and it rebuilds the manifest *from the git repo at a tagged state*, never from working-tree input. **Emergency revocation flow:** on discovery of a malicious recipe/tarball pin, (1) root role revokes the implicated reviewer key(s) and any implicated repository key (v11 §10.5.3 mechanics), (2) a new index is signed removing/flagging the entries, (3) a security advisory is published to `security.json` with a new advisory class `supply-chain`, which enforcement mode treats as blocking, and (4) `pkg audit --supply-chain` reports any installed package whose `recipe_commit` or source hash matches the revoked set — this is exactly why the provenance log stores them.

**Explicitly out of scope, with reasons:**

- **Compromised host root / kernel** (T5's residual): nothing at the package-manager layer can defend against it. Stated, not attempted.
- **Malicious *upstream* whose payload is present in both VCS and tarball and survives diff review** (T1's residual): the project is not a code-auditing organization; the mitigations raise attacker cost and improve retrospective detection (provenance log), and the spec must say so rather than imply otherwise.
- **Binary-level compiler-backdoor detection** (trusting-trust): out of scope; the bootstrap toolchain chain is the same problem every source distro has, documented plainly.
- **Per-user fetch trust or third-party recipe repos with independent trust roots:** one trust root, one recipe repo. Multiple roots multiply the revocation problem without a governance story to match.

---
---

## Issue 4: Rebuild Cascades — Making Correct Rebuilds Survivable

**Status:** Design proposal; amends §7.7 (abi_tag derivation), §9.2 (schema), §15 (pipeline), §16 (CLI)
**Problem:** The §7.7 `abi_tag` cascade is *correct* — an openssl bump computes exactly the set of reverse dependencies that must be rebuilt — but correctness says nothing about cost. On a source-only system, a 400-package cascade is 3–6 hours of compile time, and it is the single most-cited reason users abandon source distributions. This section specifies the mitigation stack, in the order a cascade flows through it: (1) make the set smaller, (2) make the builds faster and kinder to a live system, (3) reuse past work, (4) let the user defer and preview, (5) guarantee retries are cheap, (6) measure everything so every layer above improves with use.

The governing principle: **the cascade set must remain a superset of "actually broken" and should approach "actually affected" from above.** Every mechanism below either shrinks the set *safely* (never below the true breakage set) or reduces the per-package and per-cascade cost without touching set membership. No mechanism trades correctness for speed.

---

### 4.1 Cascade Minimization: Hash Link-Facing Inputs, Not Dependency Tags

v11/§7.7 derives `abi_tag(pkg)` from the `abi_tag`s of all its dependencies. This over-triggers in three ways: (a) a dependency rebuilt for reasons invisible to the link interface (a patch that fixes a runtime bug without touching headers or symbols) still changes its tag and cascades; (b) runtime-only relationships (dlopen'd plugins, data consumers) cascade as if they were link edges; (c) static-only consumers don't care about a dependency's *soname* at all. The fix is to make `abi_tag` a hash of what a consumer *actually consumed at link time*, not of what its dependencies *are*.

**4.1.1 Link-interface tags.** Each built package records a **link interface** in the DB: the set of `(soname, sorted-symbol-version-set, header-fingerprint)` tuples it exports. Header fingerprinting is deliberately coarse: the recipe declares which of its installed header files are public (glob in the manifest, default `usr/include/**`), and the fingerprint is a hash of those files' normalized contents. Then:

```
abi_tag(pkg) = hash( name, version_core,
                     { link_interface_hash(dep) for dep in link_deps(pkg) },
                     build_option_set )
```

A dependency that rebuilds but whose link interface (exported sonames, symbol versions, public headers) is byte-identical produces the **same** `link_interface_hash` — and therefore its dependents' `abi_tag`s are unchanged, and the cascade **prunes itself at that node**. The post-build ELF scanner (§8) already extracts DT_NEEDED entries and symbol data from produced artifacts; it gains one more output: `link_interface_hash`, stored per-package-version in the DB. This is retroactive narrowing done right: we never *assume* an interface is stable, we *measure* it after every build, and the next cascade benefits. Soname-preserving patch bumps (openssl 3.0.13 → 3.0.14) collapse from a 400-package rebuild to the 1–3 packages whose consumed symbol sets or headers genuinely changed — typically zero.

**4.1.2 Dependency-kind refinement in the manifest.** Recipes distinguish three dependency kinds, and only the first feeds `abi_tag`:

- `link` — the default; provides headers/libraries consumed at build time. Feeds the tag.
- `runtime` — needed on the target system at execution time, but not at link time: dlopen'd plugin providers, helper binaries invoked via exec, data files. **Excluded from `abi_tag`.** A `runtime` dep bump never rebuilds the consumer; the scanner's existing dlopen heuristics (§8) plus the manifest declaration together justify the exclusion, and `pkg check` remains the safety net that flags a genuinely broken runtime pairing after the fact.
- `build` — toolchain-only (m4, bison, codegen). Already never cascades in v11; stated here for completeness.

Misclassification risk is asymmetric and we pick the safe side: a `link` dep wrongly demoted to `runtime` means a missed rebuild (bad — caught by `pkg check` post-install and by the gate's DT_NEEDED scan); a `runtime` dep promoted to `link` means an extra rebuild (harmless). Review rule: demotion requires the scanner to confirm the consumer's artifacts carry **no DT_NEEDED entry and no undefined symbol** resolvable to that package; otherwise the recipe lint rejects the `runtime` kind.

**4.1.3 Static linking.** A package that statically links a dependency has *no runtime coupling* to it, so soname changes are irrelevant — but the static archive's *content* is now inside the consumer, so a security fix in the archive still must cascade. Rule: static archives contribute to `abi_tag` via a **content hash of the consumed `.a` members' symbol tables**, not the soname machinery. This preserves the security-cascade guarantee (an openssl CVE fix *does* rebuild static consumers) while making cosmetic rebuilds prune exactly as in 4.1.1. The scanner identifies static consumption by comparing each artifact's defined symbols against installed `.a` member tables during the scan stage.

**4.1.4 What we explicitly do not narrow.** glibc (v13 §8 base case: tag from `release_core` only), and any dependency edge the scanner could not classify (unknown link mechanism → treated as `link`). Uncertainty always widens the cascade, never narrows it.

---

### 4.2 Build Scheduling: Fast, Parallel, and Rude-Free

**4.2.1 DAG-level parallel executor.** A cascade is scheduled as a topological-level sweep over the rebuild DAG. Within a level, packages are independent and run concurrently up to a scheduler budget. The unit of parallelism is the **package build**, each of which internally runs `make -j$(inner_jobs)`. Naively setting `inner_jobs = ncpu / concurrent_builds` thrashes; instead the scheduler uses a **total-token pool**: `sched_tokens = ncpu` (configurable), each running build holds `inner_jobs` tokens, `inner_jobs` is dynamically throttled between 1 and `max_pkg_jobs` (default 4) based on observed system load and build phase (configure phases are serial and I/O-bound — they hold 1 token; the compile phase requests more). Levels are barriers only where the DAG requires them; a package may start the moment all its in-cascade dependencies are *activated*, not when the level completes.

**4.2.2 Keeping a gaming desktop usable.** All build processes run in a dedicated cgroup v2 subtree (`/sys/fs/cgroup/pkg-build/`):

- `cpu.weight = 50` (vs. default 100) — under contention the cascade gets ≤ ~1/3 of CPU; with an idle desktop it still gets everything. This is the entire trick: weights, not quotas.
- `cpu.max = 75%` as a hard ceiling *only while user session activity is detected* (logind idle state); ceiling lifted when idle.
- `io.weight = 50`, and `memory.high` set to `total_ram - 2GiB` so the build set can never push the desktop into swap.
- All build processes `nice 10`, `ionice` best-effort class 7.

Defaults chosen for the single-user gaming-distro profile: a foreground game at 144 fps must not stutter because ffmpeg is recompiling.

**4.2.3 `--background` and the completion transaction.** `pkg upgrade --background` detaches the build phase: the resolver and cascade plan commit up front, builds run under a `pkg-buildd` service (tiny supervisor, not a daemon with new trust surface — it re-execs `pkg build` per node), and completion is signaled via a well-known state file + optional desktop notification. The user can `pkg upgrade --attach` to stream progress, or `pkg cancel-build` to stop scheduling new nodes (in-flight builds finish or are killed; already-activated packages stay activated — see 4.2.5).

**4.2.4 ETA from build history.** §9.2 gains:

```sql
CREATE TABLE build_history (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL, version TEXT NOT NULL,
    abi_tag TEXT NOT NULL, option_set_hash TEXT NOT NULL,
    duration_s INTEGER NOT NULL, success INTEGER NOT NULL,
    started_at INTEGER NOT NULL,          -- unix ts
    cc_hit_rate REAL,                     -- NULL if ccache disabled
    host_jobs INTEGER NOT NULL
);
```

ETA per package = median of the last 5 successful builds of that `name` (any version — version bumps change duration far less than packages differ from each other), scaled by `(host_jobs_recorded / host_jobs_now)^0.7` (sublinear speedup model; 0.7 is a sane default, refined by regression once enough rows exist). First-ever build of a package: ETA falls back to the median of packages with similar source-tree size (fetched-tarball bytes is recorded at fetch time and is a surprisingly good predictor). Cascade ETA = sum over the critical path of the DAG, not the total — displayed as "≈3.5 h wall (≈11 h compile work across 8 cores)". Historical rows never feed scheduling *correctness*, only display and ordering, so a poisoned row costs a bad ETA, nothing worse.

**4.2.5 Checkpointing: per-package activation, decided.** A cascade is **N transactions, one per package, activating as each build completes** — not one giant 400-package transaction. Rationale: (a) the v12 lock amendment already runs builds unlocked and re-validates on activation, so per-package activation costs nothing new mechanically; (b) a failure at package 387 of 400 must not discard 387 builds of progress, and with per-package activation + the §4.5 artifact cache, retry resumes at 387; (c) the mixed-state window this creates (old-abi and new-abi packages coexisting) is **already legal** in this system — §4.1.1's link-interface model means a package only enters the cascade if its consumed interface changed, and the scanner enforces floor≤ceiling at runtime (v12 §9), so a not-yet-rebuilt dependent linked against the *old* soname continues to resolve against the still-present old library until its turn comes (the old library remains installed until its last dependent activates away; the store is refcounted, so this costs disk, not correctness). The single-transaction alternative buys "all-or-nothing" purity that the abi model doesn't need and pays for it with exactly the all-or-nothing failure mode users hate. Within a cascade, packages activate **in DAG order, each in its own atomic stage→activate per §7.5**, and the DB records a `cascade_id` column so `pkg history` can show the whole cascade as one logical operation and `pkg undo <cascade_id>` can roll it back as a unit.

**4.2.6 Failure modes.** A build failure does not abort the cascade: the failed package and its in-cascade dependents are marked `blocked`, everything else continues, and the final report lists the blocked subtree with the first 40 lines of each failing log. On the *next* upgrade attempt the blocked subtree is retried first (artifact cache makes the rest free). Persistent failure leaves the last-good abi generation fully installed — see 4.4.2 for the coexistence argument. Sandboxed builds failing from OOM get one automatic retry with `inner_jobs` halved before being marked failed.

---

### 4.3 Amortization & Caching

**4.3.1 ccache/sccache, first-class.** Config:

```toml
# /etc/pkg/config.toml
[cache]
compiler = "ccache"          # off | ccache | sccache
dir = "/var/cache/pkg/ccache"
max_size = "20G"
```

The build environment wraps CC/CXX; per-package opt-out lives in the recipe manifest (`[build] compiler_cache = false`) for the rare packages ccache miscompiles (bootstrap-stage gcc, some -fplugin builds) — opt-out is a recipe author decision, not user-facing. Hit rates land in `build_history.cc_hit_rate`; a package whose hit rate collapses across rebuilds is exactly the anomaly §4.6 flags. The cache directory is keyed by content, not by package, so it needs no invalidation logic of its own — `ccache -c` style size eviction only.

**4.3.2 Incremental build dirs: unsafe, rejected (with one carve-out).** Reusing a build tree across *identical* versions is fine only if the tree is pristine, which the unpack step guarantees trivially — that's not incremental, that's just not deleting it. Reusing a tree across a *dependency* change (the cascade case) is unsafe in general: autotools caches, `pkg-config` results, and generated headers bake in the old dependency's answers. **Decision: no incremental rebuild across abi_tag changes.** The carve-out: *same-version, same-tag* rebuilds (a retry after an OOM or a transient fetch failure) may reuse the tree iff the failure occurred before the `install` phase, recorded as a phase marker in the build dir. Everything else: ccache is the sanctioned mechanism — it captures ~90% of the real speedup with none of the staleness risk, because it keys on preprocessed source, not timestamps.

**4.3.3 Idle scheduling.** Cascades above a size threshold (default: >50 packages or >1h predicted) are *offered* for deferred execution: a systemd timer unit (`pkg-cascade.timer`, nightly 03:00, with an OpenRC cron equivalent shipping the same command) runs `pkg upgrade --execute-pending` with guards: skip if on battery (via upower), skip if a fullscreen application holds the logind idle-inhibit lock (gaming!), skip if load5 > ncpu. All three guards are overridable (`--ignore-power`). This is scheduling policy, not mechanism — the build machinery is identical whether invoked by a human or the timer.

---

### 4.4 Deferral & Batching UX

**4.4.1 `--dry-run` cascade preview.** `pkg upgrade --dry-run` prints, before anything is fetched:

```
upgrade plan: 3 direct updates, 412 rebuilds (cascade root: openssl 3.0.14)
  link-interface pruning saved: 96 packages (interface unchanged)
  predicted: ≈3 h 25 m wall / ≈10.8 h compile (your history, 8 cores)
  largest: chromium (≈48 m), libreoffice (≈41 m), qtwebengine (≈36 m)
defer with: pkg upgrade --skip-rebuilds   schedule: pkg cascade --at 03:00
```

The preview must be honest: it runs the full resolve and interface-hash comparison, and it says "estimate" because first-build packages have wide error bars.

**4.4.2 `--skip-rebuilds`: safe, and here's the proof.** `pkg upgrade --skip-rebuilds` activates direct updates and *defers* the cascade. The coexistence question — old-abi dependents running against new-abi libraries — decomposes: (a) if the bump changed the consumed link interface, the old library's soname/symbol set still exists in the refcounted store (§4.2.5c) and old-abi binaries keep resolving against it — the scanner's floor/ceiling machinery permits precisely this, since ceiling is a property of what a binary *records*, not of what's newest; (b) if the interface did *not* change, 4.1.1 pruning would have removed those packages from the cascade anyway. So the deferred window is safe **provided the old abi generation is not garbage-collected while dependents exist** — `pkg clean` already respects store refcounts, and this section adds: a package with a *pending rebuild* pins its current dependency generation exactly as if it were freshly installed. Danger case: a deferred cascade containing a **security fix** — deferred rebuild means deferred fix. Mitigation: `--skip-rebuilds` refuses to defer any cascade whose root or whose member matches an open advisory in `security.json` unless given `--defer-security=I-understand` (deliberately un-ergonomic; cf. v12's foot-gun stance).

**4.4.3 Per-package defer lists.** `/etc/pkg/config.toml`:

```toml
[defer]
never_rebuild_eagerly = ["chromium", "libreoffice", "qtwebengine"]
```

Listed packages' cascades always route to the deferred window regardless of size; `pkg upgrade` reports them as "deferred: 3 (policy)". An explicit `pkg upgrade chromium` overrides the list for that invocation. Deferment is scheduling, not exclusion — the rebuild still happens, at the next window, and `pkg check` will nag about pending-rebuild packages in `--verbose`.

---

### 4.5 Escape Hatch: Personal Artifact Cache

`/var/lib/pkg/cache/` stores locally built, compressed artifacts keyed by the *full identity of the build*:

```
cache/<name>/<version>-<abi_tag>-<option_set_hash>-<recipe_commit>.pkg.tar.zst
```

Since the key includes `abi_tag` (which transitively encodes the consumed link interfaces), the option set, and the recipe commit (v12 §2's pinned recipe hash), a cache hit is *by construction* the artifact a source build would have produced — this is content-addressed reuse, not a binary repository, and it resurrects none of the deferred binary-track machinery (no index, no signatures beyond the local store's existing manifest verification, no cross-host semantics, no trust questions — you built it yourself). Consumption sites: (a) cascade retry after failure — already-built nodes hit cache, retry is seconds; (b) `pkg undo` followed by redo; (c) option-set flip-flops (`+wayland` ↔ `-wayland`). **Invalidation rules:** an entry is invalid iff any key component differs (structural, automatic) or the artifact fails store-manifest verification at reuse time (treated as a miss, logged, deleted). No TTL, no manual invalidation commands — the key *is* the invalidation. Eviction: LRU above a size cap (default 30 GiB), `pkg clean --cache` for manual reclaim. Cache is never authoritative: a miss is silently a source build, and `pkg build --no-cache` exists for paranoia.

---

### 4.6 Measurement

Per build, the orchestrator records into `build_history` (§4.2.4): duration, wall vs. CPU time, success/failure phase, option set, ccache hit rate, `host_jobs`, and peak RSS (from the cgroup's `memory.peak`). This powers, in ascending order of ambition:

1. **ETAs** (§4.2.4) — median-of-last-5 per package.
2. **Anomaly detection:** a build exceeding 5× its median duration, or a cascade exceeding 1.5× its prediction, triggers a notice in the final report ("ffmpeg took 5.2× its median — check thermals / ccache health") with the row ID for `pkg info --build <id>` inspection. Detection only; never blocks anything.
3. **Scheduler refinement:** the job-throttling exponent and per-phase token weights are re-fit from history monthly (a closed loop over our own telemetry; pure local computation).

No telemetry leaves the machine. There is no opt-out because there is nothing to opt out of.

---

### 4.7 What NOT to Do

- **distcc / distributed compilation as a default.** Multiplies the trust surface (code executing on peer machines), the configuration burden, and the failure modes (heterogeneous toolchains produce subtly wrong artifacts), for a speedup only users with idle LAN machines can realize. Recipes don't forbid a user configuring it via `CC` overrides — but the PM neither ships nor blesses it, and ccache+parallel-local covers the common case.
- **Binary mirrors.** Any "download a prebuilt artifact someone else made" path contradicts the source-only model and drags back the entire deferred trust/retention/reproducibility stack (v12 §1). The §4.5 personal cache captures the legitimate need (don't recompile identical work *you already did*) without crossing the trust boundary.
- **One-giant-transaction cascades.** Rejected in §4.2.5: purity the abi model doesn't need, paid for with all-or-nothing failure.
- **Heuristic "probably fine" cascade skipping** (mtime tricks, version-prefix guesses). The link-interface hash is the *measured* version of every such heuristic; anything looser is a missed security rebuild waiting to happen.

*Assumption flagged: v11 defers §7.7 details to v10, which was unavailable; §4.1's derivation rule is written to amend whatever v10 said — if v10 already hashes per-dep rather than per-interface, §4.1.1 is a refinement, not a rewrite.*

---
---

## Issue 5: Transaction-Level Undo (Bounded Rollback)

**Status:** Design proposal (v14 candidate) — amends §14, §9.2, §12, §16.1

### 5.1 Scope, Precisely

§14's non-feature stance targets *catastrophic* rollback (unbootable system, corrupted DB) — correctly delegated to filesystem snapshots. What remains unaddressed is the common case: a transaction **succeeds**, and the resulting software is semantically worse (a regression, an unwanted major version, a broken UI). Atomic stage→activate guarantees no *broken* state; it says nothing about *unwanted* states. This section adds **transaction-level undo**:

> Undo restores the package set and symlink tree recorded before a given transaction, for non-glibc packages, while the superseded store directories still exist (pre-`pkg clean`).

Boundaries:

- **In scope:** restoring the pre-transaction package set by re-pointing stable symlinks at retained store dirs and reverting the DB's installed-set rows. Ordinary packages, including libraries with abi_tag cascades.
- **glibc: excluded, with one existing exception.** Category-1 security patches are inline, ABI-compatible, and deliberately one-way (undoing a security patch is an anti-goal). Major bumps already have their recovery machinery: the post-swap recovery copy in `.stage/glibc-active/` and `glibc-apply --rollback` (v13 §1, §6). Undo must not duplicate or interfere with that path; while `/var/lib/pkg/glibc-pending` exists, undo of *any* transaction older than the pending stage is forbidden (v13 §4's pending-window rules are extended by one clause).
- **Kernels and out-of-tree modules:** kernel packages are undoable (they're just store dirs + `/boot` entries; undo re-points and re-runs the existing initramfs/bootloader integration), but undo never touches the *running* kernel's modules and never removes the last bootable kernel — the same reference-counting guard §12 already applies to `pkg clean`.
- **/etc config drift: explicitly out.** Conffile state (`conffile_state` table) is not reverted. Why: config files may have been edited by the user *or* migrated forward by the newer software (database schemas, state files); blindly restoring old configs on top of newer-migrated data is data corruption, not recovery. Undo restores packages; config reconciliation remains §11's interactive machinery, re-run on the reactivated old package's hooks. The spec must state this limitation in `pkg undo` output, every time.

### 5.2 Mechanism: Generations Table, Not Snapshots

The DB is already transactional and is the system of record for the installed set; symlink trees are **derivable** from it (`packages` + `files` tables fully determine every stable symlink the activation core manages). Therefore undo needs no filesystem snapshot machinery of its own — it needs the DB to remember prior installed sets. New tables:

```sql
CREATE TABLE generations (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    created_at  INTEGER NOT NULL,
    command     TEXT NOT NULL,             -- e.g. "pkg upgrade firefox"
    kind        TEXT NOT NULL CHECK(kind IN ('install','remove','upgrade','undo')),
    undoes      INTEGER REFERENCES generations(id),   -- set for kind='undo'
    schema_ver  INTEGER NOT NULL           -- DB schema version at creation (§5.7)
);

CREATE TABLE generation_packages (
    generation_id INTEGER NOT NULL REFERENCES generations(id),
    name TEXT NOT NULL, version TEXT NOT NULL, abi_tag TEXT NOT NULL,
    arch TEXT NOT NULL, installed_explicitly INTEGER NOT NULL,
    PRIMARY KEY (generation_id, name, arch)
);
```

On every committing transaction, after the DB commit succeeds (activation core, §15), the PM records the **pre-transaction** installed set as a new generation row — actually two logical states, but stored as one: generation *N* is the package set *resulting from* transaction *N*, so undoing transaction *N* means restoring generation *N−1*. `kind`/`command` make history legible; `undoes` links undo generations to their target so `pkg history` can show "transaction 42 was undone by transaction 57" rather than presenting a confusing linear list.

- **Where stored:** in the SQLite DB itself (same file, same backup story, same integrity checks per §9.3). No parallel state files that can diverge.
- **Symlink reconstruction:** undo does not record the symlink set separately. It computes the delta between the target generation's package set and the current set, resolves it into store-dir activations/deactivations, and re-points symlinks exactly as the normal activation core does — **reusing §7.5's stage→activate machinery wholesale**, with the generation table playing the role the resolver normally plays. One activation code path, no divergent "undo path."
- **Sizing/pruning:** `generation_packages` is ~1 row per installed package per transaction (~2–5 KB per transaction for typical sets). Retention is bounded by store reality, not by count: a generation whose required store dirs have been pruned by `pkg clean` becomes **non-restorable** but stays in history (marked `restorable = 0`, computed on demand by checking store-dir existence — cheaper and always accurate vs. maintaining a flag). `pkg clean` gains a **generation-aware hold**: by default it refuses to prune store dirs referenced by the most recent *N* restorable generations (default N=5, config `undo-hold-generations`), unless `--garbage` is given, in which case it warns and lists which generations become non-restorable. This makes "undo works until clean" the explicit, surfaced contract rather than a silent cliff.
- **WAL checkpoint vs. DB before-image: rejected.** Copying whole DB states per transaction is heavyweight, duplicates §9.3's integrity story, and would snapshot *unrelated* mutable state (pins, alternatives, advisory caches) that undo must not touch. The generations table records only the installed package set; undo reverts only that.

### 5.3 CLI

```
pkg history
    List transactions: id, time, command, packages added/removed/upgraded,
    restorable status, undo linkage.

pkg undo [n]
    Restore the pre-transaction state of transaction n (default: most recent
    restorable transaction). Prints the computed undo set (activations,
    deactivations, cascade re-activations) and requires confirmation, or -y.
    --package <name>   (see §5.4 — rejected; flag reserved and errors with
                       an explanation, so the refusal is discoverable)
```

**Sequentiality decision: undo is strictly sequential.** `pkg undo n` is only legal when *n* is the most recent restorable transaction; to go back further you undo repeatedly. Justification: undoing transaction *n* out of order requires computing "the system as if *n* never happened but *n+1..m* did" — that set may not exist in the store (transaction *n+1* may have built against *n*'s versions), may not be representable (a package installed in *n+1* depends on a version introduced in *n*), and in general requires a full re-solve against historical constraints the system no longer has. Sequential undo is always well-defined: each step is a state the system provably occupied. The error for non-sequential requests explains this and offers the sequential path (`pkg undo` × k).

### 5.4 Partial Undo: Not Allowed

Single-package undo (`pkg undo --package firefox`) is **rejected**, and the CLI refuses loudly rather than silently doing a downgrade-shaped thing. Reasons: (1) it *already has a supported spelling* — `pkg install firefox=<old-version>` followed by `pkg pin` is the explicit, resolver-checked path (v11 §16.1 "recovery path, not rollback"); giving it a second, weaker spelling invites confusion about which is checked. (2) Partial "undo" is not undo — it's a new resolution unconstrained by the other packages changed in the transaction, which is exactly what `pkg install foo=version` does properly through PubGrub with reverse-dependency checking. (3) Allowing it would break the sequentiality invariant (§5.3) since partial states were never recorded as generations. The two mechanisms are complementary: **undo** for "the whole last transaction was a mistake," **explicit downgrade + pin** for "this one package should be older."

### 5.5 Interaction with abi_tag Cascades

Undoing a library transaction is the hard case: the pre-transaction state had reverse-deps built against the *old* abi_tag. This is precisely why generation-based restore works where naive symlink-flipping would not: the generation table records the **whole package set**, including the old reverse-dep builds. Their store dirs still exist pre-clean, and their old stable symlinks are reconstructible from `files`. The undo set is computed as:

1. **Target set** = generation *n−1*'s full package set.
2. **Delta** = symmetric difference vs. current set: packages to reactivate (in target, not current — or present at different version/abi_tag) and packages to deactivate (in current, not target).
3. **Existence check:** every reactivation's store dir must exist; if any is missing (pruned), the generation is non-restorable and undo **fails atomically before staging**, listing the missing dirs and the `pkg clean` invocation that removed them. No partial undo states are reachable — the same honest-atomicity guarantee as normal transactions (§2).
4. **Cascade integrity:** because the target set is a *historically occupied, internally consistent* state (it passed the gate when it was live), no resolution is needed — undo never solves, it restores. The cascade problem ("rebuild reverse-deps against old lib") reduces to "re-point reverse-deps at their retained old builds," which is data, not computation.
5. **Store dirs missing but rebuildable:** if a needed store dir was pruned, undo offers `--rebuild-missing`: re-derive the build from the pinned recipe commit (v12 §2 means the recipe for that exact version is permanently addressable) — but this is a *fallback*, clearly labeled, since it converts instant undo into a source-build session and its output must pass the normal gate before activation.

### 5.6 Interaction with Pins, Reconciliation, and Running Processes

- **Pins:** undo does not modify `pins.toml` (undo reverts the installed set, not user configuration — same argument as /etc). But the *post-undo* system must be pin-consistent: after undo, the PM re-checks pins and warns if the restored set violates an existing pin (e.g., user pinned `foo` to the newer version, then undid the upgrade). The warning suggests `pkg unpin` or re-upgrade; it does not block, because pins constrain *future resolutions*, and undo is not a resolution.
- **DB reconciliation (§9.3):** generations integrate with the existing reconciliation machinery as follows — reconciliation's direction of truth is unchanged (**filesystem → DB**); generations are DB-only metadata and are *never* used to reconstruct the installed set during `pkg db regenerate`. A regenerated DB keeps its generations table if salvageable, else starts a fresh generation 1 and marks history as truncated. This avoids a circular dependency where the undo mechanism's state participates in repairing the DB that undo depends on.
- **Running processes holding deleted inodes:** after undo reactivates old library versions, processes started since the original transaction may hold deleted inodes of the *newer* (now deactivated) versions. Undo reuses the v13 §7 inode-scan machinery (`/proc/*/maps` against unlinked inodes — the same scan `pkg audit --glibc` runs) and prints the affected PIDs/services with OpenRC restart commands, and suggests `pkg apply-pending-restarts`. This is a warning, not a gate — matching the established category-1 treatment (v13 §7).

### 5.7 Why This Is Not Nix Generations, and the §14 Boundary

Explicitly not Nix: there are **no user profiles** (one system state, not per-user generation trees), **no concurrent generations** (exactly one active state; generations are history records, not bootable entries), **no content-addressed store** (§20 unchanged — store dirs are name-version-abi, not hash-addressed), and **no generation switcher at boot**. Nix generations are a *deployment model*; this is an *undo log*. The distinction matters for complexity budgeting: nothing here changes what the system is between transactions.

**§14 amendment (recommended wording):**

> Rollback of the *system* — bootloader-level, catastrophe-grade — remains an explicit non-feature; filesystem snapshots are the correct Unix tool, unchanged. What this specification adds (§5.x) is *transaction-level undo*: restoring a prior package set from retained store dirs to recover from **semantic regressions** — successfully installed, successfully activated, worse software. The boundary is intent: snapshots recover from breakage; undo recovers from regret. Undo never substitutes for snapshots (it cannot survive `pkg clean`, store corruption, or glibc-anchor failure) and snapshots never substitute for undo (they are all-or-nothing, pre-arranged, and revert user data along with packages).

### 5.8 Failure Modes

- **Interrupted undo:** undo is staged and activated through the *same* §7.5 two-phase machinery as any transaction, so interruption before the DB commit leaves the current (pre-undo) state fully intact; interruption after commit is indistinguishable from success, with post-commit hooks idempotent. The generation row for the undo is written *in the same DB commit* as the installed-set change — the history record and the state change are atomic together, so history can never claim an undo that didn't happen.
- **Undo after clean:** detected pre-staging by the existence check (§5.5 step 3); hard failure listing missing store dirs, no state change. The `undo-hold-generations` default (§5.2) makes this rare; the error message names the clean invocation responsible.
- **Undo across schema migrations: forbidden.** Each generation records `schema_ver` (§5.2); undo to a generation with `schema_ver ≠ current` refuses with an explanation. Rationale: the restored package set would be interpreted by code expecting different column semantics (v12 §7's `installed_explicitly` seeding rule is the precedent for how dangerous silent reinterpretation is). Migration prints the generations being invalidated. Old generations remain in `pkg history` as non-restorable.
- **Undo during glibc pending window:** undo of transactions newer than the pending stage is allowed (they're ordinary non-glibc states); undo *across* the pending stage boundary is forbidden (§5.1), and any undo while a glibc recovery copy is unverified (v13 §6) warns that glibc recovery and package undo are independent tracks.
- **Undo of an undo:** trivially supported — an undo is itself a transaction (`kind='undo'`), so its own pre-state is a generation; `pkg undo` again walks forward. The `undoes` linkage keeps `pkg history` honest about what happened.

---
---

## Cross-Issue Notes (integration guidance)

- **Schema convergence:** Issues 1–5 collectively add `opt_tag` + `package_options`, `generations`/`generation_packages`/`activation_log` (Issue 2) **or** `generations`/`generation_packages` (Issue 5 — note the two proposals differ: Issue 2 logs symlink operations, Issue 5 derives them from package sets; pick one — the recommendation is Issue 5's derived approach for simplicity, with Issue 2's `activation_log` kept only if debugging demands it), `provenance`, `build_history`, `cascade_id`. One schema bump (v3) should carry all of them together with a single migration.
- **Terminology to settle:** Issue 2 says "rollback," Issue 5 says "undo" — adopt **undo** (`pkg undo`) for transaction-level restore and reserve "rollback" for the glibc rescue path, to keep the two mechanisms verbally distinct.
- **CLI surface additions, consolidated:** `pkg history`, `pkg undo`, `pkg env {create,list,show,activate,update,export,delete}` (with existing `pkg env` renamed `pkg build-env`), `pkg audit --supply-chain`, `pkg upgrade --background/--skip-rebuilds/--dry-run`, `pkg cascade --at`, `pkg clean --cache`, `--opt` on install/upgrade.
- **What this set deliberately still does not add:** per-user/rootless installs, distributed builds, binary mirrors, option-conflict matrices, /etc rollback, content addressing. Each refusal is documented in its section.
