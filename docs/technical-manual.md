# Yakimo Technical Manual

This manual explains how Yakimo's package manager works internally. It is intended for readers who want to understand transactions, dependency resolution, builds, activation, recovery, and security machinery—not for a first install walkthrough or a recipe-writing tutorial.

The descriptions below follow the implementation in this tree. The consolidated v15 specification also contains intended behavior that the code does not yet implement; those differences are identified as **current limitations** rather than presented as working features. The specification remains useful for the rationale and the intended direction.

## 1. The operating model

Yakimo is source-first in a strict sense: an ordinary `pkg install` does not choose between a binary repository and a source build. There is no binary repository track in the current design. It either builds the package locally or reuses an artifact previously built on the same machine from the personal artifact cache.

These representations cooperate:

- **Recipes and signed repository metadata** describe available packages, their versions, sources, and dependencies.
- **The store** holds a completed package under a versioned identity. The main roots are `/usr/lib/pkg/.store/` for amd64 and `/usr/lib32/pkg/.store/` for i686.
- **The live filesystem** is an ordinary FHS tree. Its package-owned files are relative symlinks into the store, not copies and not a Nix-style store-path environment.
- **SQLite** at `/var/lib/pkg/db/pkg.sqlite` records the installed set, resolved dependency edges, transaction generations, build history, measured link interfaces, and provenance.

For example, a conceptual store entry and activation link look like this:

```text
/usr/lib/pkg/.store/openssl-3.0.14+ab198327144/
└── usr/lib/libssl.so.3

/usr/lib/libssl.so.3
  -> pkg/.store/openssl-3.0.14+ab198327144/usr/lib/libssl.so.3
```

The exact numeric tag depends on the build inputs. Relative links continue to work when a scratch root is entered with `chroot` or booted as `/`.

### 1.1 Identity: more than a name and version

A build identity has five fields:

```text
(name, version, abi_tag, opt_tag, architecture)
```

Its store-directory spelling is:

```text
name-version+ab<abi>[+p<option-tag>]
```

The `+p...` suffix is omitted for an all-default option selection.

The two hashes serve different purposes:

- **`opt_tag`** fingerprints option values that differ from their recipe defaults. It distinguishes, for example, the same package version built with and without a documentation option.
- **`abi_tag`** describes ABI-relevant build inputs: the package name and release core, the sorted set of measured link-interface hashes of its link dependencies, ABI-affecting option changes, and the configured optimization level. glibc is the special base case: only its name and release core feed the tag, so a `2.39-r1` to `2.39-r2` patch does not change the tag, while `2.40` does.

Identity is computed before a build from installed dependency measurements. It is not a hash of the finished package's complete contents, and the recipe commit is not part of the store directory name. The recipe commit is, however, part of the artifact-cache key and the provenance record.

### 1.2 Dependency kinds are operational instructions

Every dependency has one of three kinds:

- **Link**: the package consumes a library interface. These edges affect `abi_tag`, propagate rebuild cascades, and block removal.
- **Runtime**: the package needs data, a helper executable, a plugin, or another runtime facility without a link-interface relationship. These edges block removal but do not propagate cascades.
- **Build**: the dependency is needed while compiling. It is made visible in the build sandbox but neither blocks later removal nor propagates a cascade.

This split is why the scanner and the database are connected. A dependency declaration alone is not enough: post-build ELF inspection checks what the resulting binaries actually consumed, and measured interfaces later decide which dependents really need rebuilding.

## 2. `pkg install`: the complete transaction

`pkg install` and `pkg upgrade` share the same transaction engine. `install` requires at least one package specification. `upgrade` with no names targets all packages marked as explicitly installed; `--all` merely makes that intent explicit and suppresses the explanatory note.

Accepted package specifications are `name`, `name@version`, and `name=version`. The last form is useful for an explicit, resolver-checked downgrade. Option requests can be attached with `name[opt:key=value,...]` or supplied transaction-wide with repeated `--opt name=value` flags.

The high-level flow is:

```text
exclusive lock + supply-chain gate
              |
              v
recipe graph -> constrained resolution -> option/dependency fixpoint
              |
              v
 installed-state diff -> rebuild cascade -> build plan
              |
       +------+-------+--------------+
       |              |              |
   --dry-run    --skip-rebuilds   --background
    stop now     keep direct nodes   write pending state and stop
       |              |              |
       +------ execute plan ---------+
                      |
          cache or fetch -> sandboxed phases -> ELF verification
                      |
          optional critical double-build -> stage in store
                      |
       activate links + package row + generation in one DB commit
                      |
          record measurements/provenance; reconcile DKMS modules
```

The following sections explain each stage.

### 2.1 Inputs, locking, and preflight gates

The current transaction takes the exclusive `/var/lib/pkg/lock` before opening the database and holds it through resolution, builds, and activation. The lock uses file locking with a 60-second writer wait and records holder metadata for diagnostics. Read-only commands generally use a shared lock with a two-second wait.

This is deliberately described as current behavior because it differs from the v15 design: the specification calls for unlocked builds followed by a short locked revalidation/activation section. The current CLI has not implemented that split or the corresponding re-solve-on-drift rule. Consequently, one active install/build can block another writer for much longer than the final design intends.

Before resolution, install and upgrade run the supply-chain audit. Any unacknowledged hard finding blocks the transaction. An acknowledgement is scoped to package, version, and finding kind; it means “the operator has seen this,” not “the defect is fixed.”

Resolution also checks a package's declared minimum glibc against a staged-pending major glibc update. Building against a libc that is staged but not active would produce unusable binaries, so this fails before compilation.

### 2.2 Building the resolution graph

The resolver's package universe comes from the local checkout under `/var/lib/pkg/recipes/`. Each immediate child directory containing `pkg-meta.toml` contributes a package version. Base dependencies at their all-default selection, plus build dependencies, are included so the build's tools and headers are resolved as well as its runtime requirements.

Resolution uses PubGrub. For each requested root, Yakimo:

1. Selects the requested version, or the version in that package's current recipe.
2. Filters candidates through exact or ABI-track pins and the active repository profile.
3. Prefers installed versions where possible.
4. Runs PubGrub and merges the selected real packages into the transaction.

The installed-version preference is a two-pass mechanism, not a prohibition on upgrades. The first solve hides versions newer than the installed one. If that has no solution, a second, uncapped solve permits required upgrades.

PubGrub itself understands ordinary version constraints, not virtual packages or OR-groups. Yakimo encodes each virtual/OR choice as a synthetic package with one synthetic version per alternative. Selecting a synthetic version selects its alternative. Normal PubGrub backtracking therefore handles alternatives without a second solver. Synthetic entries are removed before the plan reaches the rest of the system.

An alternative preference is a **bias**, not a hard requirement. The preferred provider is tried first; if its dependencies cannot be satisfied, the solver may backtrack to another provider. A virtual with no registered provider is an explicit resolution error. `pkg alternatives list` shows known providers and preferences; `pkg alternatives set <virtual> <provider>` validates that the named recipe provides the virtual and stores the preference for future resolutions.

A failed solve carries PubGrub's derivation tree. `pkg install` renders that tree directly; `pkg explain <constraint>` performs a separate, unconstrained diagnostic solve to explain why that constraint does or does not resolve.

**Current limitations:** roots in one invocation are solved separately and their maps merged, rather than through one joint multi-root solve. The recipe graph initially uses default dependencies; option-driven additions are handled afterward. Explicit `name=version` does not currently bypass a conflicting pin, despite the specification's one-transaction override rule. Multilib co-selection is not implemented in the resolver, and ordinary CLI-created plan nodes are amd64 even though the store and schema have an i686 side.

### 2.3 Pins and repository profiles

Pins live in `/var/lib/pkg/config/pins.toml` and constrain future resolutions:

- `exact` accepts exactly the named version, including no later `-rN` patch.
- `abi-track` never permits a downgrade. For versions using the `-rN` convention, it accepts later patches within the same release core. For packages without that convention, the intended fallback compares ABI tags; because a candidate tag is not available to the pre-build resolver, a different release core is rejected at this stage.

`pkg pin package` displays a pin; `pkg pin package version [--mode ...]` writes it; `pkg unpin package` removes it. Creating a pin captures the installed build's ABI tag when available.

A profile is a repository-curated set of version constraints. `pkg profile list` reads the synced `profiles.toml`; `pkg profile apply name` stores the active profile name. On subsequent transactions, an explicit pin for a package removes that package's profile constraint rather than trying to intersect two different constraint languages. If a previously active profile disappears from the sync cache, the current code warns and proceeds without its constraints.

Profile-supplied **option** selections described in the specification are not implemented. The current option precedence is:

```text
strongest  per-command --opt and name[opt:...] requests
           /etc/pkg/options.toml [package.<name>]
           /etc/pkg/options.toml [defaults]
weakest    recipe defaults
```

The main options file may include plain-named sibling files, in listed order, with the main file strongest. Nested includes, path escapes, unknown keys on known packages, and contradictory per-command requests are errors. `pkg options` shows the standing configuration and its origins; `pkg options <name>` adds recipe defaults, effective values, descriptions, and any recorded installed values.

After initial resolution, enabled options can add dependencies. Yakimo repeatedly computes effective selections and resolves newly required packages until no new package appears. Unknown option names are rejected rather than silently ignored. The current fixpoint examines option-added link/runtime dependencies; expansion of `add_build_deps` during this fixpoint is incomplete.

### 2.4 From selected versions to a build plan

The selected set is compared with installed identities. An identical `(name, version, opt_tag)` is normally a no-op. A changed option tag at the same version is a real rebuild. A new dependency pulled into the plan is not marked as explicitly installed; only user-named roots acquire that status on first installation. An existing package's explicitness is preserved during activation.

Each new or changed package becomes a potential cascade root. Yakimo then walks **link-kind reverse dependencies** in the installed database:

- If old and new link-interface measurements are both available and equal, the dependent is pruned.
- If either measurement is missing, the interface is treated as changed. Uncertainty widens the rebuild set rather than risking a missed rebuild.
- Every dependent admitted to the cascade is conservatively assumed to propagate the change to its own link dependents.
- A same-release-core glibc patch does not cascade; a glibc release-core change cascades unconditionally.

The resulting directed acyclic graph contains build-order edges. Dependencies must be activated before their dependents start, so a dependent's sandbox can bind the exact store directories selected by the plan.

After each rebuilt node is activated, Yakimo measures its exported sonames, symbol versions, and public headers. If the interface matches what the old dependent consumed, pending non-requested dependents whose in-plan dependencies are all unchanged can be pruned. This second, measurement-driven narrowing reduces overly conservative planning without assuming stability in advance.

### 2.5 Preview, deferral, and background state

`--dry-run` performs resolution, identity diffing, and cascade computation but does not fetch or build. It reports direct updates, rebuilds, plan-time pruning, and estimates from local build history.

Per-package estimates use the median of the last five successful builds of that package name, scaled by the ratio of historical to current parallelism raised to the power 0.7. Cascade wall time is the DAG critical path; compile work is the sum of package estimates. Packages without history contribute zero to the current estimate—the specification's similar-source-size fallback is not implemented.

`--skip-rebuilds` reduces execution to directly requested packages and records the remaining names in `/var/lib/pkg/pending-build.json`. If any deferred name appears in the cached advisory document, deferral is refused unless the literal `--defer-security=I-understand` override is present. This check is deliberately conservative substring matching.

`pkg cascade` displays that pending file. `--at HH:MM` only writes `/var/lib/pkg/cascade-schedule`; an external timer is expected to act on it. `--execute-pending` checks the five-minute load average unless `--ignore-power` is given, removes the pending file, and starts a fresh upgrade of all explicit packages. It does not resume a serialized original plan. Battery and fullscreen-inhibit guards named in the specification are not implemented.

`--background` currently writes the effective plan's package names to the same pending file and exits before any build. It does **not** start `pkg-buildd`, detach a worker, or provide an attach/cancel mechanism. The printed reference to `pkg upgrade --attach` describes a planned interface, not an available command.

### 2.6 Scheduling and the personal artifact cache

Before execution, Yakimo cycle-checks the plan. A scheduler then starts each node as soon as its in-plan dependencies are activated, rather than waiting for an entire topological level.

A token pool bounds total compile parallelism:

- `--jobs N`, then `[build] jobs` in `/etc/pkg/config.toml`, then host core count determine pool size.
- `[build] max_pkg_jobs` caps one package's job count, defaulting to four.
- The build receives `MAKEFLAGS=-jN` from its token grant.

The scheduler also attempts to configure `/sys/fs/cgroup/pkg-build` with CPU/I/O weights, an active-session CPU ceiling, and a memory threshold. These operations are best-effort and warn on failure; they are not the sandbox security boundary. The current code configures the subtree but does not explicitly move each worker into it, and it does not periodically refresh session-dependent limits.

Before compiling a node, Yakimo probes `/var/lib/pkg/cache/`. The cache filename key contains package name, version, ABI tag, option tag, and recipe commit. A hit is unpacked as a tar+zstd tree and checked against its embedded file manifest. Corrupt or unsafe entries are discarded and become source-build misses. Successful source builds are added to the cache, which uses LRU eviction above a 30 GiB default cap.

This is a personal reuse mechanism, not a binary repository: there is no artifact index, mirror protocol, cross-host compatibility contract, or binary trust path. `pkg build --no-cache` bypasses both lookup and insertion for a forced source build. `pkg clean --cache` clears it.

A cache hit returns before recipe phases and before the critical-set double-build. The cached tree still goes through store staging, activation, and post-activation metadata measurement.

### 2.7 Fetching and verifying sources

A build first finds the exact package name/version in `/var/cache/pkg/index/recipe-manifest.toml`. Only sources declared in that entry can be fetched. Each source URL must use HTTP or HTTPS and has a pinned SHA-256 digest.

Sources live in `/var/cache/pkg/sources/` under `<sha256>-<basename>`. Existing files are re-hashed before reuse. Downloads go to a `.part` file, are size-limited, hashed, synced, and renamed into place only on a match. A mismatch deletes the partial file. Interrupted downloads restart rather than attempting HTTP range resume.

`pkg fetch <name>` performs this fetch/verify step without running a build.

The build then creates fresh scratch and `$PKGDIR` directories and copies the verified sources into scratch under the basename recipes expect. Scratch is never incrementally reused; sanctioned reuse happens at the artifact-cache or compiler-cache layer.

**Current limitation:** the source fetcher uses its HTTP client directly. Although a network-capable Fetch sandbox profile exists, neither `pkg fetch` nor the build's fetch stage invokes it. The mandatory controls currently in force are the manifest entry, URL scheme, digest, and download limits—not origin restriction enforced by a network sandbox.

### 2.8 Sandboxed recipe phases

`prepare`, `build`, `check`, and `install` run in that order. Prepare and check scripts are optional. Build is mandatory except for `[build] binary = true` repackaging; install is always mandatory. A script is copied into scratch and executed as `/bin/sh -e <script>`. A failed phase, including a failing check phase, fails the node.

The Build profile uses bubblewrap with:

- an empty network namespace, with no network access;
- private user, PID, IPC, and UTS namespaces;
- a fresh `/proc`, minimal `/dev`, and private `/tmp`;
- read-only toolchain paths and resolved dependency trees;
- read-write access only to scratch and `$PKGDIR`;
- a cleared environment followed by explicit or narrowly allowed variables;
- `no_new_privs` as supplied by bubblewrap, a new session, and parent-death cleanup.

Dependencies are read-only bound at both their real store paths and stable `/pkg/deps/<name>` aliases. Build tools' own link/runtime dependencies are included in the bind closure. This matters because a tool such as Python or Meson needs its own libraries even when the package being built never links them.

The generated environment supplies:

- `HOME`, `PKGDIR`, `MAKEFLAGS`, a fixed `SOURCE_DATE_EPOCH`, and `LC_ALL=C`;
- `PKG_OPT_<NAME>=0|1` for every declared option;
- baseline `CFLAGS`/`CXXFLAGS` from `[build] opt_level` (`o0`, `o1`, `o2`, `o3`, or `os`, default `o2`);
- dependency-provided executable directories on `PATH`;
- dependency library paths for running build tools and for linking;
- dependency include and pkg-config search paths;
- optional `ccache` or `sccache` wrappers from `[cache] compiler`, unless the recipe disables compiler caching.

`pkg build-env <name>` prints the environment for an installed recipe. `--resolve` is different: it prints a hypothetical version solution rather than a build environment.

The sandbox is not yet a complete syscall confinement system. No seccomp-BPF program or per-phase timeout is currently installed. Also, when `pkg` runs as root, the namespace's unprivileged inner UID maps to the invoking host UID unless external subuid machinery is used; the mount restrictions and `no_new_privs` are the compensating controls.

### 2.9 Tree normalization and post-build verification

After install, before staging, Yakimo normalizes the output tree:

- pkg-config files with a `/usr` prefix are rewritten relative to `${pcfiledir}`, so they work both in `/usr` and under `/pkg/deps/<name>`;
- libtool `.la` archives are removed because their absolute cross-package paths do not survive the per-build dependency layout.

The ELF verifier then walks files by content, not extension. It checks:

1. **Declared linkage.** Every `DT_NEEDED` soname must be provided by the package itself or a declared link/runtime dependency. Source-build violations fail the node and include hints naming installed providers of the missing soname. Vendor-blob packages using `[build] binary = true` receive warnings instead.
2. **glibc compatibility.** The binary's highest required `GLIBC_...` symbol version must not exceed the newest libc in scope. If a declared packaged glibc provides `libc.so.6`, its exported versions set the ceiling; otherwise the running host's glibc is used.
3. **Unknown or malformed inputs.** A binary linked to `libc.so.6` without versioned symbols produces an unknown-floor warning, not an assumed pass. ELF-magic files that cannot be parsed are also reported.

The verifier extracts exported sonames and symbol-version sets for later interface hashing. Public files under `usr/include` contribute a separate content fingerprint. Static-archive consumption detection exists as a best-effort symbol-table heuristic, but is not a hard gate and cannot see stripped artifacts reliably.

### 2.10 Critical-set double-build

A recipe marked `[build] critical = true` is built a second time by default. The second build is sequential, sandboxed, and uses the same fixed `SOURCE_DATE_EPOCH`. Yakimo compares deterministic hashes over file contents and symlink targets.

- A match discards the second tree and records `reproducibility = "self-match"` after activation.
- A mismatch prevents activation and moves both trees under `/var/lib/pkg/build/quarantine/` with a diff report.
- A second build that simply fails is reported as a build failure, not mislabeled as a mismatch.

`--no-repro-check` disables this for install/upgrade. This is a local self-consistency check, not independent reproduction by another builder. The specification's broader reproducibility tiers remain deferred.

### 2.11 Store staging, activation, and the database commit

A completed `$PKGDIR` is moved beneath the store's `.stage/` directory, with a copy fallback for cross-filesystem test/build layouts. Yakimo writes and verifies a manifest of regular-file hashes, then renames the tree to its identity-derived store name. The recipe manifest is retained as a hidden store sidecar for later inspection and DKMS reconciliation.

Activation computes the difference between the current and target package sets. For each incoming store tree it derives one symlink operation per file or symlink; directories become real directories in the live tree. New links are created at a temporary sibling and renamed over the destination, making each individual link replacement atomic. Removal is guarded so Yakimo does not unlink a path that has since been re-pointed elsewhere. If glibc supplies `ldconfig`, cache regeneration is attempted and failures are warnings.

Only after filesystem operations does Yakimo open the SQLite transaction. Package deletion/insertion, resolved dependency edges, and the generation snapshot commit together. If that DB transaction fails, it rolls back as a unit.

This is **honest atomicity, not a claim that the whole multi-package cascade is indivisible**. The store rename is atomic, each symlink replacement is atomic, and the DB package-set change is transactional, but there is no single atomic operation covering all links plus SQLite. An interruption can leave a stale or partially re-pointed link tree requiring reconciliation. A cascade intentionally commits one package at a time so a late failure does not discard earlier successful work.

After commit, recording of measured interfaces and provenance is best-effort; a metadata-write failure warns rather than pretending the already-committed activation did not happen. Provenance includes the recipe commit, source hashes, index version, attestation fingerprints, build host, builder/profile version, timestamps, and any self-match result.

### 2.12 Failures, retries, and DKMS follow-up

A failed node and its transitive in-plan dependents are marked failed/blocked; independent branches continue. The report includes the first part of the failing phase log. A build whose log resembles an out-of-memory kill gets one retry with half the original job grant.

The personal artifact cache makes a later invocation cheap for nodes that already succeeded. “Retry resumes at the failure” therefore means plan recomputation plus cache reuse, not a persistent scheduler checkpoint.

After the plan, Yakimo compares usable kernel trees before and after activation. It discovers DKMS packages from their installed recipe sidecars:

- an activated DKMS package is rebuilt for every usable installed kernel;
- an unchanged DKMS package is rebuilt only for newly appeared kernels;
- unrelated transactions trigger no DKMS work.

DKMS runs without networking in a dedicated KernelModule profile, with UID 0 inside the namespace for DKMS's checks. Writes are limited to scratch, `/var/lib/dkms`, and the target kernel's module directory. Logs persist under `/var/log/pkg/`. Failures warn without rolling back the package or kernel activation.

Exact per-kernel resolver edges, module signing, initramfs regeneration, and module cleanup on removal/undo remain deferred.

## 3. Commands that change the installed set

### 3.1 `pkg build` and forced rebuilds

`pkg build <name>` creates a one-node plan, computes current options and identity, and runs the same cache/build/verify/stage/activate path as install. It is not a dependency-solving substitute for install: its plan has no in-cascade dependency edges. It is useful when the package's dependencies are already present or when deliberately exercising one recipe.

`pkg rebuild --affected` handles provider switching. It finds installed packages whose recipes reference a virtual with a preferred provider that is not currently installed, re-resolves those packages at their installed versions, and builds the resulting plan. Because the DB does not record which provider satisfied each historical virtual edge, this is an approximation based on provider installedness, not proof of the old resolution.

### 3.2 Removal and autoremove

`pkg remove <name>` queries installed reverse dependents through link/runtime edges:

- the default refuses when blockers exist and names the direct blockers;
- `--recursive` computes the transitive blocker closure and asks before removing the whole set;
- `--force` removes only the named package and can leave dependents broken.

Removal constructs a target set equal to the current set minus the selected packages, then uses the same activation/generation machinery as installation.

`pkg autoremove` selects non-explicit packages with no current reverse dependency. The current CLI performs one query pass, not the full transitive orphan fixpoint described in comments/specification; another invocation may be needed after newly orphaned dependencies become visible.

### 3.3 Store cleanup

Ordinary `pkg clean` keeps installed store directories and directories referenced by up to five recent restorable generations. It removes other store directories. `--garbage` releases the generation hold and warns that history may cease to be restorable. `--cache` instead clears the personal artifact cache.

The five-generation hold is currently hardcoded; the specification's configurable hold count is not read. The current cleanup reference set also does not include environment roots or the pending-rebuild name file, despite those pins being part of the intended design.

A glibc safety guard refuses cleanup when the running glibc differs from the DB's active version or when no active glibc can be established. This protects the recovery copy after a rescue boot.

## 4. Inspection and explanation tools

These commands expose different layers of the system; they are not interchangeable aliases.

- **`pkg search <query>`** performs a case-insensitive substring search over directory names in the local recipe checkout and prints their manifest versions. It does not search descriptions or query a remote index.
- **`pkg info <name>`** reads installed rows and shows identity, architecture, source track, explicitness, option deltas, and provenance when present. Despite the broader CLI description, the current implementation does not print dependency or file lists.
- **`pkg why <name>`** walks the installed reverse-dependency graph to explicitly installed roots. It explains the recorded installed graph, not a hypothetical repository resolution.
- **`pkg explain <constraint>`** asks why a repository constraint fails and renders PubGrub's derivation tree. It exists because “no solution” hides which requirements conflicted.
- **`pkg options [name]`** explains option configuration and origins, preventing an option from becoming invisible recipe customization.
- **`pkg build-env <name>`** exposes the generated build environment for debugging; `--resolve` shows a hypothetical selected version set instead.

Commands use process status 0 for success, 1 for resolution failures, 2 for build failures, 3 for verification/gate failures, and 70 for other errors. Supply-chain audit deliberately has its own 0/1/2 result convention.

## 5. Repository synchronization and trust

### 5.1 What `pkg sync` does

`pkg sync --status` reads cache metadata without networking or locking and reports index version, age, and 24-hour TTL freshness.

A normal sync takes the exclusive lock and loads `/var/lib/pkg/trust/`. A fresh-enough cache is reused without network access but its index is still verified; `--force` sets the TTL to zero to force a fetch. Otherwise, mirrors from `/etc/pkg/mirrors.toml` are tried in order. For an accepted mirror, Yakimo caches:

- `index.json`, the repository-signed index and timestamp-signed pointer bundle;
- `recipe-manifest.toml`, the package-to-recipe/source/attestation mapping;
- `security.json`, after repository-role verification, when available;
- `meta.json`, the local fetch time and index version.

Writes use temporary files plus rename. The accepted index version becomes the rollback floor only after caching succeeds. If all mirrors fail, a still-verifiable cached index may be used with a stale-cache warning. There is no implicit sync during install or search.

### 5.2 The trust roles

The TUF-inspired chain has four roles:

```text
offline root, threshold-signed
  |-- repository: index and advisory documents
  |-- timestamp: index hash/version and freshness
  `-- reviewer: recipe/source-review attestations
```

Ed25519 signatures cover a deterministic, sorted-key JSON serialization. The timestamp must name both the accepted index version and its canonical-payload hash, preventing a mirror from combining a fresh timestamp with an old index. Index and root version counters provide rollback protection.

A timestamp has a 24-hour expiry followed by a 72-hour warning grace period. Beyond that, verification fails in the library's strict policy. An expired root manifest produces a warning.

`pkg key list` lists role keys, `pkg key show-chain` displays delegations and the last accepted index, and `pkg key trust-root <file>` is the deliberately unusual recovery path for a broken root chain. It displays fingerprints and requires interactive confirmation because it imports a new anchor out of band rather than proving it through the old chain.

**Important current trust limitations:**

- The CLI's compiled-in root is a development placeholder containing zero bytes and authorizes nothing. A production build must embed and provision real trust material.
- Sync does not currently fetch/rotate the root manifest as the bootstrap sequence in the specification describes.
- The recipe manifest is required and cached, but the current sync path does not independently verify its signature or bind its bytes to the verified index. Source hash verification therefore should not be mistaken for a complete implementation of the specification's end-to-end recipe-manifest trust chain.
- `profiles.toml` and the recipe checkout are not materialized by the current sync implementation; commands requiring them expect those files/checkouts to have been provisioned separately.
- The reviewer-attestation verifier exists, but install/fetch does not currently call it for every selected entry. Supply-chain audit primarily evaluates the fingerprints recorded in provenance.

These are implementation gaps, not intentional security guarantees.

## 6. Security and diagnostic commands

### 6.1 `pkg audit`

The default audit parses the cached `security.json` envelope and matches package names and affected-version expressions against installed versions. A missing advisory file is an error rather than a clean result.

`pkg audit --env <name>` applies the same advisory matching to identities in an environment's lock file and returns a gate error when matches exist.

`pkg audit --glibc` reads `/proc/<pid>/maps` for deleted mappings beneath `/usr/lib/pkg`. It maps processes to OpenRC services through cgroup v2 paths, with recognition of systemd-style service names for foreign layouts. Unmapped processes, user sessions, and PID 1 are listed but not restarted. Run with sufficient privilege for complete `/proc` visibility.

`pkg apply-pending-restarts` presents the same report and, after confirmation, invokes `rc-service <name> restart` for mapped services. The report is name-sorted; it does not independently calculate a service dependency topological order. Restart failures are warnings.

`pkg audit --supply-chain` examines provenance completeness, attestation counts, reproducibility state, origin/hash drift against the current recipe manifest, and retained quarantine trees. `--critical` limits the report to packages marked critical by the current manifest. Hard findings exit 2 and block install/upgrade until acknowledged; warnings alone exit 1. `pkg audit --supply-chain --acknowledge <package>` records acknowledgements for that package's current hard findings.

`--json` currently renders Rust debug representations for audit reports, not a stable JSON serialization. It should not yet be treated as a machine-stable interface. The specification's `enforce-restart` advisory-freshness gate is not wired into the current CLI; the supply-chain hard-finding gate is implemented independently of that setting.

### 6.2 `pkg check`

With a binary name or path, `pkg check` parses one ELF, lists `DT_NEEDED` entries and its glibc floor, and compares the floor with the running host. `--suggest` searches standard library directories and installed store trees for providers. `--verbose` adds raw parsed details.

Without a target, it scans the installed set:

- `minimal` examines executable `bin`/`sbin` trees;
- `standard`, the default, examines all ELF files;
- `deep` additionally prints soname-like strings that may indicate `dlopen` use.

The whole-root ceiling is the target root's installed glibc, falling back to the host when the root has none. Missing store directories, unparseable ELF files, unknown glibc floors, and floor-above-ceiling violations are reported separately. If the shared DB lock cannot be acquired, the command falls back to enumerating store directories.

`--nss` is a narrow configuration check: services named in `/etc/nsswitch.conf` must have corresponding `libnss_<service>` modules in standard library directories. The deep `dlopen` scan and static-consumption detector are heuristics, not proof of a complete runtime dependency set. `pkg check` is not a database dependency-consistency checker and does not prove that every declared package is installed.

### 6.3 `pkg isolate`

`pkg isolate <path> [args...]` runs a command once under the Hook sandbox profile with an empty `LD_LIBRARY_PATH` and a throwaway scratch directory. It is useful for exposing accidental library-path dependence.

It is not a general jail or container tool: it does not construct a full package-selected runtime root, and the Hook profile has broader `/etc` and `/var` write access than the Build profile.

## 7. Named environments

A named environment lives under `/var/lib/pkg/envs/<name>/` and reuses store content through another derived symlink tree. It is not a container. Activation is shell search-path precedence:

```sh
eval "$(pkg env activate media)"
```

For POSIX shells this prepends environment paths for executables, manuals, pkg-config, and XDG data. A fish-specific output form covers `PATH` and `MANPATH`. No `LD_LIBRARY_PATH` is set.

The command group provides `create`, `list`, `show`, `activate`, `update`, `export`, and `delete`. `show` reads the resolved identity lock; `export` prints the manifest plus that lock; `delete` removes the environment root.

The current implementation is narrower than the v15 design:

- creation parses a simple manifest and links packages that are already installed under the same name;
- it does not run PubGrub for environment constraints or build missing packages;
- update recreates from the stored manifest;
- the minimal manifest parser is not a general TOML environment format;
- environments do not currently participate in `pkg clean`'s keep set;
- operations do not consistently take the package-manager lock, and rootless environments are not implemented.

The intended endpoint remains a resolver-locked, source-built environment sharing the system store. That should not be confused with what `env create` currently proves or materializes.

## 8. History, undo, and database recovery

### 8.1 Generations

Every successful activation records a generation containing:

- timestamp, invoking command, operation kind, and schema version;
- an optional cascade identifier shared by per-package activations;
- the complete post-activation installed package set.

The package-set change and snapshot are written in the same SQLite transaction. Old store directories therefore remain meaningful recovery targets after the live links move on.

`pkg history` lists generations. `pkg history <id>` compares that package set with the preceding numeric generation and prints additions/removals. `--json` hand-builds JSON for this command.

### 8.2 What undo restores

`pkg undo` restores the state immediately before the live transaction. `pkg undo N` names the transaction whose pre-state is wanted, but only the current sequential step is legal. A cascade's per-package generations are treated as one logical transaction by using its first member.

Undo does not solve dependencies. It loads a historical package set, verifies all required store directories before changing anything, and feeds that set through ordinary activation. This is why it can be quick and why sequential restore is safer than inventing a state formed by deleting an arbitrary old transaction from history.

The restrictions are intentional:

- no out-of-order undo;
- no `undo --package`; use `pkg install name=<old-version>` followed by an appropriate pin for a resolver-checked downgrade;
- no undo across DB schema versions;
- no glibc identity change through symlink undo—glibc uses its anchor recovery path;
- `/etc` and migrated application data are not reverted;
- pins are not edited, though violations after restore produce warnings;
- running processes may still hold deleted library inodes and need audit/restart follow-up.

Undo appends a new undo generation rather than erasing history.

`--rebuild-missing` does not currently rebuild or restore from cache. It prints each missing directory and explains that the cache key cannot be recovered without the recipe commit; the subsequent existence check still fails. The promised source-rebuild fallback is therefore not implemented.

Undo is recovery from a successfully installed but unwanted package state. It is not bootloader rollback, filesystem snapshotting, disaster recovery, or protection against store corruption/cleanup.

### 8.3 Database checks and regeneration

`pkg db check` combines SQLite `PRAGMA integrity_check`, schema-version reporting, two-way DB/store identity reconciliation, and verification of installed store manifests. It can report missing directories, unknown/orphaned directories, and changed, missing, or extra regular files.

`pkg db regenerate` scans for store identities absent from the DB and inserts missing package rows transactionally. Recovered rows default to source track, the current timestamp, and non-explicit status. It is a reconciliation aid, not a complete restore of all package metadata: dependency edges, full option rows, provenance, and generation history are not reconstructed from directory names. History is explicitly not recoverable from the store.

The schema supports additional tables for files, virtual providers, alternative preferences, configuration state, and option assignments. Their presence does not imply that every current activation path populates them. In particular, the current core path chiefly maintains installed package rows, dependency edges, generations, build history, provenance, and link-interface measurements.

## 9. glibc's separate update machinery

glibc cannot safely be treated as just another symlinked library package. Running processes retain old mapped files, and replacing the C library tree requires a recovery path that does not depend on the new userspace already working.

For a same-release-core `-rN` update, Yakimo builds the new tree, compares it with `/usr/lib/pkg/glibc-anchor/`, and permits only a pure inline patch: changed existing regular files are allowed; new files or changed symlink structure are rejected. Each accepted file is copied to a sibling temporary name and atomically renamed over the active file. Running processes keep old inodes until restarted, hence the audit/restart commands.

For a release-core change, Yakimo copies the built tree into `/usr/lib/pkg/.stage/glibc-active/`, verifies its manifest, writes `/var/lib/pkg/glibc-pending` first, and records matching pending state in SQLite second. A second staged major replaces the first; there is no queue.

The musl-static `glibc-apply` runs from the initramfs before ordinary userspace is trusted. It verifies the protocol and staged manifest, checks the anchor/stage same-filesystem requirement, and uses one `renameat2(RENAME_EXCHANGE)` operation to exchange the two directories. The old glibc remains in the stage location for `glibc-apply --rollback`. The applier then exits; the initramfs script owns continuing boot.

`pkg bootstrap --verify-layout` checks that anchor and stage are on one filesystem. Plain `pkg bootstrap` currently performs no bootstrap installation. The reference source-build flow can borrow a host toolchain; the prebuilt-glibc bootstrap exception in the specification is not implemented by this CLI command.

The pending DB row also records the installed applier's hash for drift detection. The current staging flag omits that optional hash field, and the static applier cannot read SQLite, so the specified boot-time self-hash comparison is not fully connected end to end.

## 10. Deliberate non-features and roadmap boundaries

Some absences are design decisions, not unfinished accidents:

- no binary mirrors or binary-first installation;
- no distributed builds;
- no arbitrary `CFLAGS`/`RUSTFLAGS` passthrough or arbitrary configure-option channel;
- no third-party recipe trust roots or per-user/rootless install model;
- no Nix-style content-addressed store-path user model;
- no system snapshot/rollback facility inside the package manager;
- no custom replacement for DKMS;
- no claim that `isolate` or `env` provides container security.

The principal deferred or incomplete mechanisms called out in this manual are:

- binary distribution, retention policy, and independent multi-builder reproducibility;
- full multilib co-selection;
- seccomp and timeout hardening, sandboxed fetching, and complete unlocked-build locking/revalidation;
- a real background build supervisor and exact pending-plan resume;
- full environment resolution/building, environment GC pins, and rootless environments;
- production trust provisioning and complete recipe-manifest authentication;
- missing-store rebuild during undo;
- DKMS per-kernel dependency records, signing, initramfs integration, and lifecycle cleanup.

These boundaries matter when reading the consolidated specification: its pipeline describes the target system, while this manual distinguishes the code paths that currently perform the work from the mechanisms still awaiting implementation.
