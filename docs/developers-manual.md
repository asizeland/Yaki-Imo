# Yakimo developer manual

This guide is a map of the Yakimo package-manager source tree. It is for
contributors changing the Rust implementation or the recipes, not for a first
installation. Start with `../README.md` for project scope, `user-guide.md` for
command and recipe usage, and `technical-manual.md` for detailed runtime
semantics and known limitations.

`SPEC.md` in this directory is the workspace's interface contract and target design. The Rust
code is the authority for what currently works; the consolidated specification
and comments sometimes describe later milestones. When changing behavior,
check all three and make any intentional contract change explicit.

## 1. Build and explore

The workspace uses Rust 2021. `Cargo.toml` declares Rust 1.82, while
`rust-toolchain.toml` selects `stable`; use a current stable toolchain. In
particular, `pkg-db/src/lock.rs` documents its use of standard-library file
locking as requiring Rust 1.89 or newer, so the declared minimum and current
code should not be assumed to agree.

Common local commands are:

```sh
cargo build -p pkg-cli
cargo run -p pkg-cli -- --help
cargo test --workspace
cargo fmt --check
```

The Cargo binary is named `pkg-cli`, so the debug executable is
`target/debug/pkg-cli`. The installed/user-facing name is `pkg`. The release
command in `README.md` builds static musl binaries:

```sh
cargo build --release --target x86_64-unknown-linux-musl
```

This requires the musl Rust target and a suitable static build environment.
The Cargo outputs are named `pkg-cli` and `pkg-init`; deployment installs them
as `pkg` and `glibc-apply` respectively. The workspace release profile enables
LTO, one codegen unit, stripping, and abort-on-panic.

Useful first searches are the type or function names below, plus the `cmd_*`
dispatch functions in `pkg-cli/src/commands.rs`. Each library crate has a
`src/lib.rs` describing its modules; public modules are generally re-exported
at the crate root, although callers also use explicit module paths.

## 2. Workspace and dependency boundaries

The workspace members are listed in the root `Cargo.toml`. The internal
dependency shape is intentionally layered:

```text
pkg-resolver --> pkg-core
pkg-db       --> pkg-core
pkg-sandbox  --> pkg-core
pkg-scanner  --> pkg-core
pkg-repo     --> pkg-core

pkg-build --> core + resolver + db + sandbox + scanner + repo
pkg-audit --> core + db + repo
pkg-cli   --> every library crate above
pkg-init  --> std only; independent of pkg-cli and all library crates
```

In these arrows, `A --> B` means “A depends on B.” `pkg-cli` is the composition
layer: it loads configuration and recipes, constructs resolver and build
inputs, and calls the subsystem APIs. It deliberately does not depend on
`pkg-init`, which is a separate boot-time binary.

| Crate | Responsibility | Main modules |
|---|---|---|
| `pkg-core` | Shared identities, dependency/manifest formats, errors, and path rooting | `package.rs`, `manifest.rs`, `virtual.rs`, `error.rs`, `paths.rs` |
| `pkg-resolver` | PubGrub integration, virtual/OR materialization, pins, profiles, and explanations | `pubgrub.rs`, `graph.rs`, `pin.rs`, `profile.rs`, `explain.rs` |
| `pkg-db` | SQLite schema/migrations and queries, store reconciliation, glibc manifests, file locks | `schema.rs`, `queries.rs`, `integrity.rs`, `lock.rs` |
| `pkg-sandbox` | Bubblewrap policy and process execution; best-effort cgroup v2 controls | `profile.rs`, `run.rs`, `cgroups.rs` |
| `pkg-scanner` | ELF parsing, link interfaces, post-build verification, and diagnostic heuristics | `elf.rs`, `gate.rs`, `check.rs`, `heuristic.rs` |
| `pkg-repo` | Signed metadata, trust roles, mirror sync/cache, recipe manifests, and source downloads | `trust.rs`, `sync.rs`, `recipe_manifest.rs`, `fetch.rs` |
| `pkg-build` | Build plans and scheduling, managed and custom recipes, local artifact cache, cascades, activation, reproducibility, glibc, and DKMS | `orchestrator.rs`, `recipe.rs`, `build_system.rs`, `cache.rs`, `cascade.rs`, `staging.rs`, `repro.rs`, `glibc.rs`, `dkms.rs` |
| `pkg-audit` | Advisory matching, deleted-library process scans, and supply-chain findings | `advisories.rs`, `inode_scan.rs`, `supply_chain.rs` |
| `pkg-init` | Dependency-free, musl-static boot-time glibc exchange and rollback | `src/main.rs` |
| `pkg-cli` | Clap definitions and all command dispatch/orchestration | `cli.rs`, `main.rs`, `commands.rs`, `options.rs`, `preview.rs` |

Two boundary details are worth knowing:

- `pkg-resolver` and `pkg-scanner` do not depend on `pkg-db`. They accept plain
  data; the CLI or build layer performs database access. This keeps solving
  and ELF analysis usable in isolated unit tests.
- `pkg-db` owns most SQL, but it is not an absolute SQL firewall. The CLI
  accesses `alternative_prefs` directly in `commands.rs`, and `pkg-audit`
  queries provenance and acknowledgement tables directly. Follow those
  existing seams rather than assuming every query has a `pkg-db` helper.

`pkg-init` is exceptional by design: it cannot rely on SQLite, pkg-db, a libc
crate, or working userspace libraries during early boot.

## 3. End-to-end data flow

A normal install crosses the crates in this order:

```text
ports/<category>/<name>/
        |
        | pkg-repo/examples/ports2repo.rs + ports/install.sh
        v
signed local repository + /var/lib/pkg/recipes/<name>/
        |
        | pkg sync: pkg-repo verifies and caches repository documents
        v
pkg-cli::run_transaction
        |
        | manifests -> PackageGraph -> PubGrub solution
        | options -> effective selections and added dependencies
        | installed DB rows + link interfaces -> rebuild cascade
        v
BuildPlan / PlanNode / SchedConfig
        |
        | execute_plan: cache hit OR fetch + sandboxed recipe phases
        | scanner verification; optional critical double-build
        v
stage_into_store -> activate_identities -> SQLite generation commit
        |
        | measured interfaces + provenance; DKMS reconciliation
        v
live FHS symlinks, queryable DB state, audit/history/undo surfaces
```

The usual resolution universe is the **local recipe checkout**, not a direct
query against remote package metadata. `run_transaction` also accepts recipe
directories or manifest-file paths: `local_recipe_dir` normalizes the target,
`register_local_recipe` loads it, and `graph_from_recipes` overlays it on a
same-name checkout entry. `--recipe-tree` uses `discover_local_recipe_dirs` and
`load_local_recipe_tree` to add ports, flat, or generated-repository layouts to
that map. Explicit targets override tree entries; duplicate tree names fail.
The overlay flows through option expansion and `build_plan`, so every stage uses
the selected files.

For repository recipes, the signed recipe-manifest entry is consulted again when
sources are fetched. For local paths and tree entries, `Recipe::load_local`
carries an unattested entry on the `Recipe`; an all-local plan skips the
repository manifest entirely.
The signed index supplies package/version and recipe-commit metadata, while the
current sync implementation does not itself materialize the recipe checkout or
`profiles.toml`; full-tree development flows provision those separately,
normally through `ports/install.sh` and generated test data.

For a focused trace of `pkg install`, read in order:

1. `pkg-cli/src/commands.rs`: `run_transaction`, `graph_from_recipes`,
   `compute_abi_tag`, and `build_plan`.
2. `pkg-resolver/src/graph.rs` and `pubgrub.rs`: graph materialization and the
   actual solve.
3. `pkg-build/src/cascade.rs` and `orchestrator.rs`: rebuild-set and scheduler.
4. `pkg-build/src/recipe.rs`: one package's phases.
5. `pkg-build/src/staging.rs`: store and live-tree commit.
6. `pkg-db/src/queries.rs`: generation, provenance, history, and interface
   records.

## 4. Core vocabulary and persistent state

### 4.1 Types used across crate boundaries

`pkg-core/src/package.rs` defines:

- `Version`: dotted numeric releases with optional `-rN` patch revisions.
  Equality, ordering, hashing, and parsing share normalized version semantics.
- `AbiTag`, `Arch`, `Track`, and `OptTag`: the identity components. `Arch`
  currently models `amd64` and `i686`; `Track` retains `source`/`binary` in the
  data model even though there is no binary repository.
- `PackageId`: the older four-field `(name, version, abi_tag, arch)` form.
- `Identity`: the current five-field form adding `opt_tag`.
  `Identity::store_dir_name()` produces
  `name-version+abN[+p<opt-tag>]`; parsing also requires the caller to supply
  architecture because the directory name does not contain it.

`pkg-core/src/manifest.rs` turns `pkg-meta.toml` into `PackageManifest`.
Important methods are `dep_set`, `build_dep_set`, `opt_tag`, `abi_opt_tag`,
`options_export_env`, `filter_selection`, and `dependency_edges`.
`ManifestOption` makes options additive: enabled options can add dependencies,
but cannot remove base dependencies or express option-to-option rules.
`SourcePin` represents an embedded `[[source]]`; `PhaseScripts` holds optional
inline `[scripts]`; and `BuildSystem` distinguishes managed Autotools, Meson,
CMake, and Make builds from `none` and `custom`. The manifest also exposes
`configure_args` and `source_dir`, rejecting managed-only fields on custom or
binary builds and configure arguments on systems that do not consume them.

`pkg-core/src/virtual.rs` defines `Dependency`, `VersionConstraint`, and
`VirtualName`. Dependencies can be concrete packages, virtuals, or `AnyOf`
alternatives. `pkg-core/src/error.rs` supplies `PkgError`/`PkgResult`; its
normal exit mapping is 1 for resolution, 2 for build, 3 for gate/verification,
and 70 for other failures. Supply-chain audit has a separate 0/1/2 result
convention.

ABI identity is assembled in `pkg-cli/src/commands.rs::compute_abi_tag`, not in
pkg-core. It hashes the package name/release core, sorted link-interface hashes
of link dependencies, ABI-affecting option deviations, and optimization level.
glibc uses its release-core base case. The recipe commit is not in the store
identity, but it is in the artifact-cache key and provenance.

### 4.2 Where state lives

These are default paths; most package-state paths pass through
`pkg_core::paths::at` and therefore honor `PKG_ROOT`.

| Path | Contents / owning code |
|---|---|
| `/var/lib/pkg/recipes/<name>/` | Active local recipe, phase scripts, and `.recipe-commit`; loaded by `load_recipe` |
| `/var/lib/pkg/db/pkg.sqlite` | Installed packages and the tables below |
| `/var/lib/pkg/lock` | Shared/exclusive coordination from `pkg-db/src/lock.rs` |
| `/usr/lib/pkg/.store/` | amd64 package trees; each has `.pkg-store-manifest` and normally `.pkg-recipe-manifest.toml` |
| `/usr/lib32/pkg/.store/` | Parallel i686 store root |
| `/var/lib/pkg/build/` | Per-node scratch/`PKGDIR` trees and reproducibility quarantine |
| `/var/lib/pkg/cache/` | Personal tar+zstd artifact cache from `pkg-build/src/cache.rs` |
| `/var/cache/pkg/sources/` | CLI's verified source cache |
| `/var/cache/pkg/index/` | CLI's `index.json`, `recipe-manifest.toml`, `security.json`, and sync metadata |
| `/var/lib/pkg/trust/` | `root.json` and monotonic/revocation state in `state.json` |
| `/etc/pkg/config.toml` | Build/cache/path configuration |
| `/etc/pkg/options.toml` | Persistent global/per-package options and one-level includes |
| `/etc/pkg/mirrors.toml` | Ordered mirror base URLs |
| `/var/lib/pkg/config/` | `pins.toml` and `active-profile` |
| `/var/lib/pkg/pending-build.json` | Deferred/background package-name state, not a serialized executable plan |
| `/var/lib/pkg/envs/<name>/` | Named-environment manifest, lock, and symlink tree |

`pkg-db/src/schema.rs` is the schema authority. `SCHEMA_VERSION` is currently
3, and `init_schema` creates or migrates the database. The main tables are:

- `packages`, `dependencies`, `package_options`: installed identities,
  concrete resolved edges, and option assignments;
- `generations`, `generation_packages`: activation records and complete
  post-transaction package sets;
- `provenance`, `build_history`, `link_interface`: supply-chain inputs, ETA
  history, and measured library interfaces;
- `files`, `virtual_providers`, `alternative_prefs`, `conffile_state`,
  `glibc_pending`: additional installed-system and configuration state.

A table's existence does not prove that every activation path populates it.
For example, the current core activation path chiefly maintains package rows,
dependency edges, generations, build history, provenance, and link interfaces;
option rows have query support but are not populated by that path.

Versions are stored as SQLite text. Compare them in Rust with `Version`, never
with SQL lexical `ORDER BY`/`MAX`. Row structs such as `PackageRow`,
`GenerationRow`, `ProvenanceRow`, and `LinkInterfaceRow` live in `schema.rs`;
conversion and CRUD live mostly in `queries.rs`.

## 5. Resolution, pins, profiles, and options

### Plain PubGrub layer

`pkg-resolver/src/pubgrub.rs` stores raw candidates in `Catalog` and builds a
filtered `OfflineDependencyProvider` for each solve.
`resolve_with_constraints` applies:

1. `PinSet` filtering (`PinMode::Exact` or `PinMode::AbiTrack`);
2. effective repository-profile constraints; and
3. a preference for installed versions.

Installed preference is a two-pass mechanism: newer candidates are withheld on
the first pass, then allowed if the first pass has no solution. Thus it is not
an upgrade ban. `ResolveError::NoSolution` retains PubGrub's derivation tree;
`explain_from_tree` renders it without solving again.

### Virtuals and alternatives

`PackageGraph` in `graph.rs` first collects packages and providers, then
`finalize` materializes virtuals and `AnyOf` groups as synthetic `$or:N`
packages. Each synthetic version depends on one alternative, allowing ordinary
PubGrub backtracking to choose an alternative. Preferred providers are ordered
to bias that choice without making an unavailable preference a hard
requirement. `ResolvedGraph::resolve_with_constraints` removes synthetic names
from the returned package set.

`explain.rs` also implements `explain_why_installed`. It is generic over a
reverse-dependency map and explicitness map supplied by the CLI; the resolver
never opens the installed database itself.

### Option selection is CLI composition

`pkg-cli/src/options.rs` owns configuration precedence, strongest first:

1. `name[opt:key=value]` and transaction-wide `--opt key=value`;
2. `[package.<name>]` in `/etc/pkg/options.toml`;
3. `[defaults]` in that file.

Includes are limited to plain sibling file names, cannot nest, and are merged
before the main file. `effective_selections` validates names and reports
conflicting per-command requests. In `run_transaction`, enabled options'
`add_deps` are resolved in a widening loop until no new packages appear.
Options therefore affect resolution before the final `BuildPlan` exists; they
are not merely environment variables added after solving.

## 6. Repository, trust, and source fetching

### Trust and synchronization

`pkg-repo/src/trust.rs` implements a TUF-inspired Ed25519 role chain:

```text
root (offline threshold keys)
  |-- repository (index and advisory documents)
  |-- timestamp  (index hash/version binding and freshness)
  `-- reviewer   (recipe/source-review attestations)
```

`RoleDelegation` contains a key set and threshold. `TrustStore` loads persisted
state, verifies signed documents, enforces root/index version counters, tracks
local revocations, and applies or imports root manifests. `verify_index_at`
checks both signatures, the timestamp-to-index binding, rollback floor, and
freshness. Timestamps have a 24-hour expiry and 72-hour warning grace window in
the strict policy.

`sync.rs::sync_index` owns the 24-hour CLI cache flow, ordered mirror attempts,
atomic cache writes, and verified stale-cache fallback. It requires
`recipe-manifest.toml` for a usable mirror, verifies `security.json` before
caching it, and advances the persisted index floor only after the index cache
write succeeds. `sync_status` reads local metadata without networking.

`recipe_manifest.rs` maps name/version to `RecipeManifestEntry`: recipe commit,
source pins, attestations, and criticality. It owns the shared recipe-file list,
content-derived `recipe_commit`, legacy `source.toml` parser, and embedded-versus-
external source selection; duplicate source definitions are errors. `SourcePin`
lives in pkg-core so parsing and repository generation share one manifest model.
`local_manifest_entry` converts the selected pins into explicit-local source
authorization, resolving relative paths from the recipe directory. The repository
`Source` type preserves an optional staged basename.
`verify_attestations` checks the reviewer signatures and critical-set
independent-review count. Repository fixtures are executable examples:

- `pkg-repo/examples/mkrepo.rs` fabricates small signed repositories;
- `pkg-repo/examples/ports2repo.rs` converts one or more ports trees, verifies
  distfiles, derives content-based recipe commits, and emits a signed repo plus
  an installable `recipes/` tree.

Both use deterministic development keys. They are not production signing
tools.

### Fetch boundary

`fetch.rs::fetch_sources` accepts a repository `RecipeManifestEntry`, not
arbitrary recipe URLs. `fetch_url` refuses a URL absent from that entry.
`fetch_local_sources` is the separate explicit-local path: it additionally
accepts the canonical `file://` URLs produced by `local_manifest_entry` and
re-reads local files rather than letting an old cache entry hide an edit. Files
are cached as `<sha256>-<basename>`, transferred to `.part`, size capped, and
renamed only after their digest matches. Hash/origin refusals map to gate errors;
transport failures map to build errors.

Do not describe the current fetch path as network-sandboxed: although
`SandboxProfile::Fetch` exists, `pkg fetch` and `run_recipe_phases` call the
HTTP fetcher directly. The build phases themselves have no network. Also note
that the current sync path does not independently authenticate/bind the recipe
manifest to the verified index, and install/fetch does not call
`verify_attestations` for every selected entry. The CLI's embedded root is a
non-authorizing development placeholder. These are current security gaps, not
guarantees to rely on.

## 7. Build planning and orchestration

### From a solution to work

`pkg-cli::build_plan` creates `pkg_build::orchestrator::PlanNode` values. A node
contains its `Recipe`, `Version`, `AbiTag`, `OptTag`, architecture, effective
option selection, in-plan dependency edges, and whether the user requested it.
`BuildPlan` is the node map; `SchedConfig` carries cache, paths, resource and
reproducibility policy, store roots, command text, and a shared `cascade_id`.

`cascade.rs::compute_rebuild_set` walks reverse **link** edges from changed
packages. Runtime and build edges do not propagate cascades. Missing interface
measurements count as changed: uncertainty widens the set. Same-release-core
glibc patches do not cascade; release-core changes do. After each build,
freshly measured interfaces can prune pending non-requested dependents in
`orchestrator.rs`.

### Scheduler and workers

`orchestrator.rs::execute_plan` cycle-checks the plan, then starts a node once
all its in-plan dependencies are activated. `TokenPool` grants between one and
`max_pkg_jobs` compile tokens. `--jobs` overrides `[build] jobs`, which otherwise
defaults to host parallelism; `[build] max_pkg_jobs` defaults to four and becomes
`MAKEFLAGS=-jN`.

Worker threads perform cache lookup and recipe phases but no DB access. The
scheduler thread owns the `rusqlite::Connection`, performs activation, records
history/metadata, and turns structured `GateUndeclared` errors into provider
hints. Failures block only the failed node's transitive in-plan dependents;
independent work continues. OOM-like logs cause one retry at half the granted
job count. `Report`/`NodeOutcome` summarize activated, cached, failed, blocked,
and pruned nodes.

The local artifact cache is not a binary repository. `CacheKey` combines name,
version, ABI tag, option tag, and recipe commit. `ArtifactCache::fetch_into`
unpacks and verifies the embedded store manifest; invalid entries become source
build misses. Successful builds are packed with tar+zstd, and default LRU
eviction starts above 30 GiB. Cache hits skip recipe phases and the critical
double-build but still stage and activate normally.

`repro.rs::double_build` handles `[build] critical = true`: a second sandboxed
build with the same fixed `SOURCE_DATE_EPOCH`, deterministic tree comparison,
`self-match` recording on success, and quarantine plus `diff-report.txt` on a
mismatch. This is a local self-check, not independent multi-builder verification.

`setup_build_cgroup` and `pkg-sandbox/src/cgroups.rs` attempt to configure
`/sys/fs/cgroup/pkg-build`. Failure warns and continues. These resource weights
are not the security boundary, and current code does not explicitly move every
worker into that cgroup.

## 8. Recipe phases, sandbox, and scanner

### Recipe execution

`pkg-build/src/recipe.rs` defines `Recipe`, `BuildContext`, `Phase`, and
`BuildOutcome`. `Recipe::load` parses `pkg-meta.toml` and associates the recipe
directory and commit. `Recipe::load_local` canonicalizes an explicit path,
derives its content identity, and attaches its local source entry.
`run_recipe_phases` performs:

1. manifest-guarded source fetch;
2. fresh scratch and `$PKGDIR` creation, then source materialization;
3. `prepare`, `build`, `check`, and `install` through `/bin/sh -e`;
4. pkg-config prefix relocation and libtool `.la` removal;
5. post-build ELF verification.

`build_system.rs::generated_phase` supplies the conventional build/install
commands for `autotools`, `meson`, `cmake`, and `make`, including shell-safe
quoting of recipe-declared `configure_args`. Phase selection in `recipe.rs` is
external file, then inline `[scripts]`, then generated content. External and
inline definitions of the same phase are rejected by `Recipe::load`; either
custom form deliberately replaces the generated phase. Prepare and check are
optional. Every recipe needs a custom or generated install phase; a custom
non-binary recipe also needs build. Scratch trees are not incrementally reused;
artifact/compiler caches are the reuse mechanisms.

For a managed system, `prepare_managed_source` unpacks the first recognized tar
archive with the sandboxed `tar` tool. It selects the recipe's relative
`source_dir`, the single extracted directory, or scratch itself when there is no
recognized archive; ambiguous directory layouts and missing/escaping configured
paths are errors. `materialize_phase` prefixes every managed phase, including
custom overrides, with a quoted `cd` into that directory. `none` and `custom`
leave source materialization and working-directory selection to the recipe.

`build_env` is the environment contract: `PKGDIR`, `HOME`, `MAKEFLAGS`, fixed
`SOURCE_DATE_EPOCH`, `LC_ALL`, every declared `PKG_OPT_<NAME>`, baseline
`CFLAGS`/`CXXFLAGS`, dependency search paths, and optional `ccache`/`sccache`
wrappers. Dependencies appear read-only at their real store paths and at stable
`/pkg/deps/<name>` aliases. Build-only dependencies are visible but do not count
as providers for the runtime linkage gate.

### Sandbox

`pkg-sandbox/src/profile.rs` defines built-in `Build`, `Fetch`, `Hook`, and
`KernelModule` profiles plus `Custom(ProfileData)`. Built-ins are hardcoded
security floors; custom `*.profile` files use a deliberately small parser so
this crate needs only pkg-core.

`run.rs::run_sandboxed_with_binds` constructs and spawns `bwrap`.
`SandboxOutcome` contains exit status and a bounded stderr tail; a nonzero
phase status is not itself a spawn error. Build isolation includes no network,
private user/PID/IPC/UTS namespaces, minimal `/dev`, private `/tmp`, a cleared
environment, read-only toolchain/dependency binds, and writable scratch plus
`$PKGDIR`. No seccomp-BPF program or per-phase timeout is currently installed.
When invoked by root, the inner unprivileged UID still maps to the invoking
host UID without external subuid setup; read `run.rs` before strengthening or
making claims about this boundary.

### Scanner

`pkg-scanner/src/elf.rs` uses goblin to extract `ElfInfo` (`DT_NEEDED` and glibc
symbol floor), `LinkInterface` (soname and exported symbol versions), aggregate
interface hashes, and public-header fingerprints. Static-archive consumption
and the functions in `heuristic.rs` are hints, not complete dependency proofs.

`gate.rs::verify_build_with_policy` walks ELF by content. Strict source builds
require every needed soname to come from the package or declared link/runtime
dependencies and reject a floor above the newest libc in scope.
`BinaryRedistributable` downgrades undeclared sonames to warnings, but not the
foreign-glibc check. The gate returns data and errors; it does not access the
DB.

`check.rs::check_system` similarly accepts `InstalledPackage` data supplied by
the CLI. `pkg check` adds target selection, scan depths, lock-fallback store
enumeration, provider suggestions, and the narrow NSS file/module check in
`commands.rs`.

## 9. Activation, database transactions, generations, and cleanup

`pkg-build/src/staging.rs` is the single activation core:

- `stage_into_store` moves `$PKGDIR` through `<store>/.stage/`, writes and
  verifies the store manifest, and renames it to the identity-derived path;
- `plan_activation_ops`/`plan_deactivation_ops` derive filesystem changes from
  package trees rather than a separate symlink log;
- `apply_symlink_ops` creates relative links at temporary siblings and renames
  them individually into place;
- `activate_identities` computes an `ActivationSet` diff, verifies required
  store directories, applies links, then commits package changes and the
  generation record in one SQLite transaction;
- `activate_built_package` connects one orchestrator node to that path, keeps
  the recipe sidecar, records dependency edges, and handles same-identity
  resynchronization.

Relative activation links are essential: they must work from outside a
`PKG_ROOT`, inside a chroot, and after that root is booted as `/`. Store
renames, individual link renames, and the DB transaction are separate atomic
mechanisms; there is no single atomic operation covering an entire cascade or
all links plus SQLite. A cascade commits per package with a shared cascade ID.
Post-commit provenance/interface recording is best-effort.

`queries.rs::record_generation` must run after package changes and inside the
same transaction. It snapshots the complete resulting installed set into
`generation_packages`. `pkg history` reads those rows.

Undo starts in `commands.rs::cmd_undo` and enters `undo_to_generation`. It uses
`live_state_gen` and `pre_transaction_gen`, restores only the sequentially
previous package set, treats a cascade as one logical transaction, checks schema
version and all store directories before mutation, and then calls the same
`activate_identities` path. It appends an undo generation rather than deleting
history. It does not revert `/etc`, change pins, perform partial
`undo --package`, or roll glibc through ordinary symlink undo.
`--rebuild-missing` currently diagnoses missing directories but does not
actually restore/rebuild them.

Removal and autoremove construct a target `ActivationSet` and use the same
activation core. Link and runtime edges block removal; build edges do not.
`pkg clean` keeps installed trees and trees referenced by up to five recent
restorable generations. `--garbage` releases that hold; `--cache` clears the
personal artifact cache. Environment roots and deferred-name state are not
currently included in that keep set despite comments describing that goal.

`pkg db check` combines `integrity_check`, per-architecture `scan_store`, and
`cache::verify_manifest`. `pkg db regenerate` transactionally reinserts orphaned
store identities with explicit defaults; it cannot reconstruct generation
history, provenance, full options, or all dependency metadata from directory
names.

## 10. DKMS

DKMS metadata comes from `[dkms]` in `pkg-meta.toml`, parsed as
`PackageManifest::dkms: Option<DkmsInfo>`. Activation retains the manifest as
`.pkg-recipe-manifest.toml`, allowing later transactions to discover installed
DKMS packages without relying on the current recipe checkout.

`pkg-build/src/dkms.rs` is called by `execute_plan` before and after the plan:

- `installed_kernels` recognizes kernel releases with a usable
  `usr/lib/modules/<release>/build/Makefile`;
- `reconcile_after_plan` compares the before/after kernel sets and activated
  packages;
- an activated DKMS driver is built for every usable installed kernel;
- an unchanged driver is built only for newly appeared kernels;
- unrelated transactions do no DKMS work.

`run_target` invokes the installed `dkms` add/build/install flow and `depmod`
under `SandboxProfile::KernelModule`, using UID 0 inside the namespace and
`max_pkg_jobs`. Persistent logs go to
`/var/log/pkg/dkms-<package>-<kernel>.log`. A failed module build warns and
leaves the activated driver/kernel in place; it does not roll back the package
transaction. Module signing, removal/undo cleanup, initramfs integration, and
exact per-kernel resolver edges remain deferred.

For examples, see `ports/sys-devel/dkms/`,
`ports/x11-drivers/nvidia-drivers/pkg-meta.toml`, and the fabricated failure and
new-kernel cases in `e2e/dkms.sh`.

## 11. glibc and `pkg-init`

glibc deliberately has a second lifecycle beside normal activation. In
`pkg-cli/src/commands.rs`, `handle_glibc_update` distinguishes:

- **Same release core (`-rN` patch):** `pkg-build/src/glibc.rs` function
  `apply_category1_patch` atomically replaces changed existing regular files in
  `/usr/lib/pkg/glibc-anchor/`. New files or changed symlink structure reject
  the pure-patch assumption. Audit/restart tooling handles processes retaining
  old inodes.
- **Different release core:** `stage_major_bump` verifies a staged tree under
  `/usr/lib/pkg/.stage/glibc-active/`, writes `/var/lib/pkg/glibc-pending`
  first, then records `PendingGlibc` in SQLite. A newer staged major replaces
  the previous one rather than queueing.

`pkg-init/src/main.rs` builds the independent `glibc-apply` executable. With no
arguments, `run_apply` verifies the flag protocol and staged manifest, checks
that anchor and stage share a filesystem, and uses raw
`renameat2(RENAME_EXCHANGE)` to exchange them. The old tree remains staged for
`--rollback`. It then exits; the initramfs script, not this binary, continues
boot. Fail-safe validation errors boot the old tree, while a failed exchange or
layout check returns failure to the initramfs.

This binary does **not** honor `PKG_ROOT` and does not open SQLite. Its SHA-256
and manifest algorithm intentionally duplicate `pkg-db/src/integrity.rs`; the
pending protocol constant is also duplicated with `queries.rs`. Change those
only in lockstep. The raw syscall and bootstrap assumptions are Linux- and
architecture-specific; x86_64 is the implemented path.

## 12. CLI command-to-code map

`pkg-cli/src/main.rs` only parses `Cli`, calls `commands::dispatch`, prints
errors, and applies `PkgError::exit_code`. `cli.rs` is the complete Clap command
and flag surface. `commands.rs` contains the implementations:

| Command surface | Implementation and main collaborators |
|---|---|
| `install`, `upgrade` | `cmd_install`/`cmd_upgrade` -> `run_transaction`; resolver, options, cascade, orchestrator |
| `remove`, `autoremove` | `cmd_remove`, `cmd_autoremove`; DB reverse dependencies and `ActivationSet` |
| `sync`, `search`, `info`, `options` | `cmd_sync`, `cmd_search`, `cmd_info`, `cmd_options`; repo cache, local recipes, DB, option config |
| `why`, `explain` | `cmd_why`, `cmd_explain`; `pkg-resolver/src/explain.rs` |
| `pin`, `unpin` | `cmd_pin`, `cmd_unpin`; `PinSet` persistence |
| `alternatives`, `profile`, `rebuild` | `cmd_alternatives`, `cmd_profile`, `cmd_rebuild`; provider bias, synced profiles, re-resolution |
| `check` | `cmd_check` plus scanner helpers |
| `clean` | `cmd_clean`; store GC and `ArtifactCache` |
| `history`, `undo` | `cmd_history`, `cmd_undo`; generation queries and staging |
| `env ...`, `build-env` | `cmd_env`/`env_create` and `cmd_build_env`; derived environment links and `recipe::build_env` |
| `audit`, `apply-pending-restarts` | `cmd_audit`, `cmd_apply_pending_restarts`; all three pkg-audit modules |
| `isolate` | `cmd_isolate`; one command under `SandboxProfile::Hook` |
| `fetch`, `build` | `cmd_fetch`, `cmd_build`; recipe-manifest fetch or a one-node `BuildPlan` |
| `key ...` | `cmd_key`; `TrustStore` listing, chain display, out-of-band root import |
| `db check`, `db regenerate` | `cmd_db`; pkg-db integrity/reconciliation plus store manifests |
| `bootstrap` | `cmd_bootstrap`; currently layout verification or a no-op |
| `cascade` | `cmd_cascade`; pending-file display, schedule record, or a fresh upgrade pass |

`options.rs` owns option merging, and `preview.rs` formats an already computed
`DryRunPlan`; it does not resolve or estimate on its own. For a new command,
update the Clap enum, dispatch, and implementation together, then add focused
unit coverage near the pure logic and an e2e case if the installed behavior
changes.

Current CLI gaps worth recognizing before extending them: `--background` only
writes pending names and does not start a supervisor; `env create` currently
links already installed same-name packages rather than solving/building missing
ones; `bootstrap` is not a full bootstrap installer; and full multilib
co-selection is not implemented. `technical-manual.md` tracks these and other
behavioral limitations in more detail.

## 13. Ports tree and repository development

The source tree's recipes live at `ports/<category>/<package>/`. A managed
single-file recipe contains:

```text
pkg-meta.toml    identity, dependencies, [[source]], [build], optional [scripts]
watch.toml       optional upstream-update policy for ports/watch.sh
```

Custom and legacy recipes may additionally contain `source.toml` and external
`prepare`, `build`, `check`, or `install` files. Sources cannot be declared in
both manifest and legacy file, and a phase cannot be both inline and external.
Custom source builds need build; all recipes need generated or custom install.

Good small examples are:

- `ports/sys-apps/sed/pkg-meta.toml`: a single-file managed Autotools build;
- `ports/app-shells/bash/`: a conventional custom source build and install;
- `ports/dev-lang/lua/pkg-meta.toml`: option-driven `add_deps`;
- `ports/dev-db/sqlite/pkg-meta.toml`: `abi_affecting = false`;
- `ports/net-im/discord/`: vendor-blob repackaging;
- `ports/sys-kernel/linux/pkg-meta.toml`: `provides = ["virtual/kernel"]`;
- `ports/x11-drivers/nvidia-drivers/`: binary repackaging plus DKMS.

`ports/README.md` is the recipe-tree guide. The repository development flow is:

```sh
sh ports/fetch-distfiles.sh /tmp/yakimo-dist
cargo run --offline -p pkg-repo --example ports2repo -- \
  /tmp/yakimo-repo http://127.0.0.1:8778 \
  --distfiles /tmp/yakimo-dist ports
PKG_ROOT=/tmp/yakimo-root sh ports/install.sh /tmp/yakimo-repo
```

Serve `/tmp/yakimo-repo`, configure `$PKG_ROOT/etc/pkg/mirrors.toml`, import the
generated root with the `PKG_ROOT`-rooted CLI, then `sync` and install. The e2e
scripts show the exact working sequence and toolchain setup. Generated keys are
for localhost testing only.

`ports/fetch-distfiles.sh` is an idempotent, hash-verifying prefetcher.
`ports/install.sh` copies generated recipes and `.recipe-commit` files into the
selected root; it does not build packages. `ports/watch.sh` reports updates;
`--apply` edits version/source pins in place after downloading and hashing the
new distfile, so review the resulting diff like any other recipe change.

## 14. Testing layout and practical workflow

### Unit and package tests

Most Rust modules carry inline `#[cfg(test)]` modules, with many parsers and
policies split into pure functions specifically for direct testing. There are
no separate workspace-wide integration-test directories to learn first.
`pkg-repo/examples/` supplies executable repository fixtures rather than test
assertions.

For a narrow change:

```sh
cargo test -p pkg-core
cargo test -p pkg-resolver
cargo test -p pkg-db
cargo test -p pkg-build
cargo test -p pkg-cli
cargo test --workspace
cargo fmt --check
```

Choose the crate that owns the behavior; add neighbouring crates when changing
a public interface. Scanner tests may compile small ELF fixtures and therefore
need a host C compiler. Database tests commonly use in-memory SQLite or
temporary directories.

### End-to-end scripts

The shell harnesses build real debug binaries and exercise signed repos,
sandboxed builds, SQLite, and activation together:

| Script | Main coverage |
|---|---|
| `e2e/run.sh` | Fabricated signed repo, tamper rejection, source build/install, cache, cascade, provenance, reproducibility, supply-chain gate, advisories |
| `e2e/lifecycle.sh` | Removal/autoremove, store tamper detection, cascade undo, index/source rollback or tamper refusal, mirror failover |
| `e2e/options.sh` | Option identities and environment, config precedence/includes, GC/undo interaction, parallelism config |
| `e2e/dkms.sh` | Driver/new-kernel reconciliation, job propagation, nonfatal module-build failure, persistent logs |
| `e2e/real-package.sh` | Real pinned zlib and a linked consumer; needs network on a cache miss |
| `e2e/real-binary.sh` | Real Discord vendor-blob repackaging and relaxed linkage warnings; needs network on a cache miss |
| `e2e/ports.sh` | Long from-scratch ports build, real program/chroot/init/self-hosting and hardware-package checks; network and substantial time/disk required |
| `e2e/ports-inc.sh <pkg>...` | Faster iteration on the root left by `ports.sh`; `+name` forces a no-cache rebuild |

`e2e/run.sh` is the smoke suite, not an alias for every script. Run the scripts
relevant to the change, then the full from-scratch ports run for recipe or broad
build-system changes. `ports-inc.sh` requires a prior full run and skips its
clean-room and smoke guarantees.

The `live/` directory is a separate x86_64 NixOS/QEMU appliance for testing in a
VM, not a Yakimo distribution image. `live/build.sh`, `live/run.sh`, and
`live/verify.sh` provide build, interactive, and headless-smoke workflows; see
`live/README.md` before using its scratch-disk options.

### `PKG_ROOT` and host safety

`pkg-core/src/paths.rs` implements the test-root override. `at("/var/lib/pkg")`
becomes `$PKG_ROOT/var/lib/pkg`; an unset or empty variable leaves the path
unchanged. `[paths]` overrides are applied first and then prefixed by
`PKG_ROOT`. Both root and overrides are cached with `OnceLock` on first use, so
set them before starting each CLI process rather than trying to change them
mid-process.

**`PKG_ROOT` is path relocation, not a chroot, container, or complete safety
boundary.** Sandbox profiles bind host toolchain paths; fetches use the host
network; build cgroup setup targets `/sys/fs/cgroup/pkg-build`; audit/restart
commands read live `/proc` and may invoke real `rc-service`; several env,
cascade, bootstrap, and pkg-init paths are intentionally or currently absolute.
Do not point a test at `PKG_ROOT=/`, and do not assume every arbitrary command
is safe merely because the variable is set.

The e2e scripts also create local HTTP servers, require `nix`, Bubblewrap, a
compiler and Unix build tools, and need unprivileged user-namespace support.
They delete and recreate their documented fixed `/tmp/pkg-e2e*` work dirs and
use fixed local ports. Prefer a dedicated disposable development container or
VM. The Debian Docker flow in `user-guide.md` uses `--privileged` because nested
namespace/mount operations need it; grant that only to the disposable test
container. Alpine is used for musl/static builds, not the glibc-based e2e
suites.

Other platform constraints visible in the code are Linux `/proc` and cgroup v2,
glibc-specific ELF symbol checks, OpenRC `rc-service` integration, amd64/i686
identity modeling, and x86_64-focused release/live/initramfs flows. cgroup and
some service/host integrations degrade or require explicit platform work; they
are not portable abstractions.

### A sensible contribution loop

1. **Locate ownership.** Use the command map to find the CLI entry, then follow
   the called type/function into the subsystem. Avoid putting policy in
   pkg-core or DB/network access in resolver/scanner merely for convenience.
2. **Preserve cross-boundary invariants.** Identity spelling, dependency kinds,
   manifest hashing, SQL row shapes, store manifests, and the glibc pending
   protocol have consumers in multiple crates. Search every use before changing
   them.
3. **Test the smallest real layer.** Add unit coverage beside a parser or pure
   policy, run its crate tests, then run `cargo test --workspace` for shared
   changes.
4. **Exercise the actual path.** For build/activation/trust changes, use a
   scratch `PKG_ROOT` and the relevant e2e script; a compile-only check cannot
   prove sandbox, filesystem, SQLite, or transaction behavior.
5. **For recipe work, iterate then start clean.** Use `ports-inc.sh` only after
   a full ports run, force with `+name` when recipe/build-system edits need a
   rebuild, and finish with the appropriate real e2e test.
6. **Update docs with behavior.** Keep `SPEC.md`, this navigation guide,
   `docs/technical-manual.md`, `user-guide.md`, and `ports/README.md` aligned
   with the actual implementation and clearly label unfinished target features.
