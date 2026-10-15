# SPEC.md — Yakimo (pkg) workspace interface contracts (v15 target)

Single source of truth for the current build-out. Source-builds only; binary repo machinery excluded. Specs: `/mnt/agents/upload/pkg-spec-v11.md`, deltas v12/v13, and `/mnt/agents/output/design-fixes-issues-1-5.md` (v14 content). All code lives in this repo; subagents implement to THESE contracts exactly.

## Crate layout & dependency pins

Workspace root `Cargo.toml` with members: pkg-core, pkg-resolver, pkg-db, pkg-sandbox, pkg-scanner, pkg-repo, pkg-build, pkg-audit, pkg-init, pkg-cli. Resolver: `edition = "2021"`, workspace lints default.

- pkg-core: `serde`/`serde_derive` 1, `toml` 0.8, `sha2` 0.10. Types only, no business logic.
- pkg-resolver: pkg-core, `pubgrub` 0.3, `serde`, `toml`.
- pkg-db: pkg-core, `rusqlite` 0.31 (feature `bundled`), `fs4` 0.8+, `sha2`, `tempfile` (dev).
- pkg-sandbox: pkg-core only (spawns `bwrap` via std::process; no bwrap crate).
- pkg-scanner: pkg-core, `goblin` 0.8, `sha2`, `tempfile`+`which` (dev).
- pkg-repo: pkg-core, `ed25519-dalek` 2, `ureq` 2 (rustls), `serde_json` 1, `toml`.
- pkg-build: pkg-core, pkg-db, pkg-sandbox, pkg-scanner, pkg-resolver, pkg-repo, `sha2`, `flate2` 1, `tar` 0.4, `zstd` 0.13, `tempfile`.
- pkg-audit: pkg-core, pkg-db, pkg-repo, `serde_json`.
- pkg-init: NO dependencies (std-only, musl-static). main.rs exists (glibc-apply).
- pkg-cli: all crates, `clap` 4 (derive). Release profile: musl-static per §17 (`panic="abort"`, `lto=true`, `codegen-units=1`).

Every crate's `lib.rs` re-exports its public modules flat (`pub use x::*`).

## pkg-core contracts

- `Version`, `AbiTag(pub u32)`, `Arch`, `Track`, string-based serde — already in package.rs (keep as-is).
- NEW in package.rs → **v3**: `OptTag(pub String)` (8-hex truncated SHA-256 of canonical sorted `name=value,` list; EMPTY string for all-default selection), `Identity { name, version, abi_tag: AbiTag, opt_tag: OptTag, arch: Arch }`, `Identity::store_dir_name() -> String` = `name-version+ab<abi>[+p<opt>]` (omit `+p` when opt empty), `Identity::parse_store_dir_name(&str)`. Derive serde, Clone, Eq, Ord, Hash, Display.
- virtual.rs stays v2 (`Dependency`, `VersionConstraint` incl. `Range`, `VirtualName`, serde).
- NEW manifest.rs → **v3** additions (keep all v2 behavior):
  - `DependencyKind { Link, Runtime, Build }` (default Link) — NOTE: pkg-db's schema.rs has its own Runtime/Build enum; manifest v3 MUST use `Link/Runtime/Build` and schema-v4 migrates its enum to three kinds `('link','runtime','build')`.
  - Manifest `[dependencies]` entries gain optional `kind = "runtime"|"build"` (default link).
  - `[options.<name>]` table: `{ description: String, default: bool, abi_affecting: bool = true, add_deps: Vec<String> = [], add_build_deps: Vec<String> = [] }`. Parser enforces: no option-to-option logic fields; additive only (struct shape enforces); unknown option keys rejected at user-config layer, not here.
  - `[build] compiler_cache: bool = true`, `critical: bool = false` (supply-chain critical set marker).
  - `Manifest::dep_set(&self, selection: &BTreeMap<String,bool>) -> Vec<Dependency>` — base deps + additive deps of enabled options; validates selection keys against declared options (unknown key → hard error `ManifestError::UnknownOption`).
  - `Manifest::opt_tag(selection) -> OptTag`, `Manifest::options_export_env(selection) -> Vec<(String,String)>` (`PKG_OPT_<NAME>=0|1`, name uppercased, `-`→`_`).
- error.rs stays v3 (`GlibcPending` exists). Add NOTHING.

## pkg-resolver contracts (files stay v2; small bumps allowed with version notes)

- Existing: `Catalog`, `resolve_with_constraints(catalog, root, version, pins, profile_constraints, installed)`, `ResolveError::NoSolution(DerivationTree<String, PkgVersionSet>)`, `explain_from_tree`, `PinSet`, `ProfileSet`.
- graph.rs → **v2**: fix to compile against pubgrub-v2 API (`Catalog` methods as defined in pubgrub.rs; `resolve_with_constraints` signature above). Virtuals materialize as synthetic packages; preferred-provider bias from `alternatives.toml` via a `BTreeMap<String,String>` param. No other behavior change.

## pkg-db contracts (schema.rs → v4, queries.rs → v4; integrity.rs, lock.rs unchanged v3/v2)

- SCHEMA_VERSION 2→3 (single additive migration carrying ALL new tables; `init_schema` migrates v1/v2 → v3).
- `packages` gains `opt_tag TEXT NOT NULL DEFAULT ''`; UNIQUE becomes `(name, version, abi_tag, opt_tag, arch)`. `dependencies.kind` CHECK becomes `('link','runtime','build')` (migration: 'binary' n/a; map old 'runtime'/'build' through, default 'link').
- New tables (exact names/cols per design doc): `package_options(package_id, name, value, abi_affecting)`; `generations(id, created_at, command, kind CHECK in ('install','remove','upgrade','undo'), undoes NULL, schema_ver, cascade_id TEXT NULL)`; `generation_packages(generation_id, name, version, abi_tag, opt_tag DEFAULT '', arch, installed_explicitly, PK(generation_id,name,arch))`; `provenance(package_id, recipe_commit, source_hashes, index_version, attestations, build_host, builder_version, started_at, finished_at, reproducibility NULL)`; `build_history(id, name, version, abi_tag, option_set_hash, duration_s, success, started_at, cc_hit_rate NULL, host_jobs)`; `link_interface(package_id, soname, symbol_versions TEXT, header_fingerprint TEXT, link_interface_hash TEXT)`.
- queries-v4 additions: CRUD for all new tables; `record_generation(conn, command, kind, cascade_id) -> Result<i64>` (snapshots current installed set, in-transaction); `generation_package_set(conn, gen_id)`; `restorable(conn, gen_id) -> Result<bool>` (checks store dirs — takes store_root param); `record_build(...)`; `build_eta(conn, name) -> Result<Option<Duration>>` (median of last 5 successes); `insert_link_interface`/`link_interface_hash(conn, name, version) -> Result<Option<String>>`; keep ALL v3 functions (glibc pending, gates) unchanged.
- GLIBC_PENDING_PROTOCOL stays 1.

## pkg-sandbox (NEW)

- `SandboxProfile` enum: `Build`, `Fetch`, `Hook`, plus data-driven from `/usr/lib/pkg/sandbox/*.profile` (TOML: bind_ro[], bind_rw[], net: bool, env_allow[]). Build profile: unshare-net, RW only scratch+`$PKGDIR`, RO toolchain+dep tree, no /etc write, no_new_privs, unprivileged UID.
- `run_sandboxed(profile, cmd, scratch_dir) -> Result<ExitStatus>` via `bwrap` argv construction (document seccomp/cgroup as out-of-band: cgroups applied by pkg-build's scheduler, seccomp noted as bwrap limitation TODO).
- `CgroupLimits { cpu_weight: u32=50, cpu_max_active_session: Option<u32>, io_weight: u32=50, memory_high: Option<u64> }`; `apply_cgroup(path, limits)` writes cgroup v2 files (best-effort, warn on failure).

## pkg-scanner (elf.rs → v2; NEW gate.rs, check.rs)

- elf-v2 additions: `LinkInterface { soname, symbol_versions: BTreeSet<String> }`, `extract_link_interface(path) -> Result<Vec<LinkInterface>>`, `link_interface_hash(&[LinkInterface], header_fp: &str) -> String` (SHA-256 of canonical sorted serialization, 8-hex), `detect_static_consumption(artifact, archive_paths) -> Vec<String>` (symbol-table comparison). Keep v1 floor/ceiling API.
- gate.rs: `gate_build(store_dir, host_glibc: &Version, required_runtime_deps) -> Result<(), PkgError::Gate>` — post-build verification per v12 §9 (framing: verification, not gate against host). `LinkagePolicy::{Strict, BinaryRedistributable}` via `verify_build_with_policy`: under the binary policy (manifest `[build] binary = true`) undeclared DT_NEEDED sonames are warnings, not hard failures — vendor blobs link system libs the distribution ships separately.
- check.rs: `check_system(conn, store_roots) -> Vec<CheckFinding>` — powers `pkg check`.
- heuristic.rs stays as-is (dlopen heuristics), ensure it compiles against elf-v2 (bump to v2 with import fix if needed).

## pkg-repo (NEW — source-only scope)

- `trust.rs`: ed25519 verify of signed index + recipe manifest (root→repository→timestamp roles, per §10.5; rollback protection via monotonic index_version). `TrustStore` loaded from `/var/lib/pkg/trust/`.
- `sync.rs`: `sync_index(mirrors, cache_dir, ttl) -> Result<Index>` with mirror fallback + staleness check (v11 §10.6).
- `recipe_manifest.rs`: types for the v12 §2 signed recipe manifest (package@version → recipe_commit + source hashes + attestations); `verify_attestations(entry, trust_store) -> Result<usize>`.
- `fetch.rs`: `fetch_sources(recipe, cache_dir) -> Vec<PathBuf>` — downloads declared `[source]`s, verifies SHA-256, refuses origins not in manifest (v14 T7).

## pkg-build (NEW)

- `cache.rs`: personal artifact cache (Issue 4.5): key `name/version-abitag-opttag-recipe_commit.pkg.tar.zst` under `/var/lib/pkg/cache/`; `lookup`, `store`, LRU evict at 30GiB cap, manifest-verify on reuse, miss = silent source build.
- `recipe.rs`: runs recipe phases (fetch→prepare→build→check→install into `$PKGDIR`) inside pkg-sandbox Build profile; exports PKG_OPT_* env; records build_history rows.
- `orchestrator.rs`: `execute_plan(plan) -> Result<Report>`: per-package transactions (cascade = N transactions, `cascade_id` shared); DAG-level parallel executor with token pool (`sched_tokens = ncpu`, inner_jobs 1..=4); failure marks subtree `blocked`, continues rest; cgroups via pkg-sandbox.
- `cascade.rs`: compute rebuild set from link_interface hashes (Issue 4.1.1): bump only cascades to dependents whose consumed `link_interface_hash` changed; glibc base case from release_core (v13 §8).
- `staging.rs`: stage→activate (rename-based, §7.5); used by normal transactions AND `pkg undo` (generation-table-driven reactivation, same code path).
- `glibc.rs`: category-1 inline patch apply + major-bump staging; integrates queries glibc_pending + integrity-v3 verify_staged_glibc.

## pkg-audit (NEW)

- Advisory matching against `security.json` (from pkg-repo sync cache); inode scan of `/proc/*/maps` for deleted-lib processes; `--glibc` (v13 §7 service restart list); `--supply-chain` (Issue 3.6: attestation counts, provenance completeness, reproducibility results; exit 0/1/2); enforcement-mode blocking surface.

## pkg-cli (NEW)

clap-derive. Subcommands: `install remove upgrade sync search info why explain pin unpin check autoremove clean history undo env{create,list,show,activate,update,export,delete} build-env audit{,--glibc,--supply-chain} isolate fetch build key db{check,regenerate} bootstrap apply-pending-restarts cascade`. Exit codes from PkgError::exit_code. `--opt name=val` on install/upgrade; `--dry-run` cascade preview (Issue 4.4.1 format); `--background`, `--skip-rebuilds` (+`--defer-security=I-understand`); `undo` sequential-only with loud `--package` refusal.

## Process rules

- Worktree per agent: `git worktree add $HOME/work-<branch> <branch>` from this repo; commit there; NEVER touch the shared repo working tree or prune.
- Modify a file ⇒ bump its version header (`## vN changes` block) and state the bump in the commit message.
- No rustc available: correctness by inspection + contract fidelity; write tests anyway (they'll run on the user's machine).
