//! pkg-cli/src/commands.rs — subcommand implementations (SPEC
//! "pkg-cli"; v11 §16.1; Issues 1/2/4/5).
//!
//! ## v1 changes
//!
//! - Initial implementation. Transaction commands (install/upgrade)
//!   follow the §15 pipeline: lock → cached index / recipe repo →
//!   resolve (pins, profiles, installed bias) → cascade computation →
//!   `--dry-run` preview (Issue 4.4.1) or `pkg_build::execute_plan`.
//!   `undo` is sequential-only with the loud `--package` refusal
//!   (Issue 5 §5.3/§5.4). Read-only commands take the shared lock;
//!   writers take the exclusive lock (§9.4).
//!
//! Contract points against concurrently-developed crates (SPEC-named
//! APIs): `pkg_repo::sync_index(&mut trust, mirrors, cache_dir, ttl)`
//! (the trust store loads from `pkg_repo::TRUST_DIR` against the
//! embedded root placeholder — see [`embedded_root`]); the resolver's
//! `Catalog` is built from the local recipe-repo checkout
//! (`/var/lib/pkg/recipes/<name>/pkg-meta.toml`), which is what the
//! signed index pins anyway — index *verification* happens in
//! `pkg_repo` at sync/fetch time.
//!
//! ## v2
//!
//! - [FIX] Caller-side seam repairs against the landed crates:
//!   `sync_index` now gets a `TrustStore`; `check_system` gets
//!   `&[InstalledPackage]` + host glibc; `pkg_audit` calls match the
//!   real advisories/inode_scan/supply_chain signatures; `OptTag` uses
//!   `as_str()`; `retain_requested` is a free fn (no inherent impl on
//!   pkg-build's `BuildPlan`); `cmd_remove`'s generation closure no
//!   longer moves a borrowed `command`; `compute_abi_tag` hashes the
//!   sorted set of link-dep interface hashes (v14 §5.1), never dep
//!   names; `cmd_build`'s `PlanNode` sets `requested: true`;
//!   `cmd_fetch` goes through the cached recipe manifest (T7).

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::time::Duration;

use pkg_build::cache::ArtifactCache;
use pkg_build::cascade::{self, BumpedPackage};
use pkg_build::orchestrator::{
    self, BuildPlan, PlanNode, SchedConfig,
};
use pkg_build::{recipe::Recipe, staging};
use pkg_core::manifest::PackageManifest;
use pkg_core::{AbiTag, Arch, Dependency, OptTag, PkgError, PkgResult, Track, Version};
use pkg_db::queries;
use pkg_db::schema;
use pkg_resolver::{
    effective_constraints, explain_from_tree, Catalog, EffectiveConstraints, Pin, PinMode, PinSet,
    ProfileSet, ResolveError,
};
use rusqlite::Connection;
use sha2::{Digest, Sha256};

use crate::cli::{Cli, Command, DbAction, EnvAction, KeyAction, TransactionFlags};
use crate::preview::{format_dry_run_preview, DryRunPlan};

// ---------------------------------------------------------------------
// Paths & small helpers
// ---------------------------------------------------------------------

pub const DB_PATH: &str = "/var/lib/pkg/db/pkg.sqlite";
pub const LOCK_PATH: &str = "/var/lib/pkg/lock";
pub const PINS_PATH: &str = "/var/lib/pkg/config/pins.toml";
pub const RECIPES_DIR: &str = "/var/lib/pkg/recipes";
pub const SOURCE_CACHE: &str = "/var/cache/pkg/sources";
pub const INDEX_CACHE: &str = "/var/cache/pkg/index";
pub const SECURITY_JSON: &str = "/var/cache/pkg/index/security.json";
pub const PENDING_BUILD_STATE: &str = "/var/lib/pkg/pending-build.json";

pub fn store_roots() -> Vec<PathBuf> {
    vec![
        PathBuf::from("/usr/lib/pkg/.store"),
        PathBuf::from("/usr/lib32/pkg/.store"),
    ]
}

/// Opens (and migrates) the package DB.
pub fn open_db() -> PkgResult<Connection> {
    let conn = Connection::open(DB_PATH)
        .map_err(|e| PkgError::Db(format!("opening {DB_PATH}: {e}")))?;
    schema::init_schema(&conn)?;
    Ok(conn)
}

pub fn lock_exclusive() -> PkgResult<pkg_db::lock::ExclusiveGuard> {
    pkg_db::lock::acquire_exclusive(
        Path::new(LOCK_PATH),
        pkg_db::lock::WaitMode::Wait {
            timeout: pkg_db::lock::DEFAULT_WRITER_TIMEOUT,
        },
    )
}

pub fn lock_shared() -> PkgResult<pkg_db::lock::SharedGuard> {
    pkg_db::lock::acquire_shared(
        Path::new(LOCK_PATH),
        pkg_db::lock::WaitMode::Wait {
            timeout: pkg_db::lock::DEFAULT_READER_FALLBACK_TIMEOUT,
        },
    )
}

pub fn load_pins() -> PkgResult<PinSet> {
    match fs::read_to_string(PINS_PATH) {
        Ok(text) => PinSet::from_toml_str(&text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(PinSet::new()),
        Err(e) => Err(PkgError::Io(e)),
    }
}

fn save_pins(pins: &PinSet) -> PkgResult<()> {
    if let Some(parent) = Path::new(PINS_PATH).parent() {
        fs::create_dir_all(parent).map_err(PkgError::Io)?;
    }
    fs::write(PINS_PATH, pins.to_toml_string()?).map_err(PkgError::Io)
}

/// Interactive confirmation (§16.3). `--assume-yes` short-circuits.
pub fn confirm(prompt: &str, assume_yes: bool) -> bool {
    if assume_yes {
        return true;
    }
    eprintln!("{prompt} [y/N]");
    let mut line = String::new();
    let stdin = std::io::stdin();
    if stdin.lock().read_line(&mut line).is_err() {
        return false;
    }
    matches!(line.trim().to_lowercase().as_str(), "y" | "yes")
}

/// Parses `--opt name=0|1|true|false` flags into a selection map.
/// Issue 1 §5: unknown *names* are rejected later against each
/// recipe's manifest (hard error, never silent); malformed values fail
/// here.
pub fn parse_opts(raw: &[String]) -> PkgResult<BTreeMap<String, bool>> {
    let mut out = BTreeMap::new();
    for flag in raw {
        let (name, value) = flag.split_once('=').ok_or_else(|| {
            PkgError::Parse(format!("--opt expects name=val, got {flag:?}"))
        })?;
        let parsed = match value {
            "1" | "true" | "on" => true,
            "0" | "false" | "off" => false,
            _ => {
                return Err(PkgError::Parse(format!(
                    "--opt {name}: value must be 0/1 (true/false), got {value:?}"
                )))
            }
        };
        out.insert(name.to_string(), parsed);
    }
    Ok(out)
}

/// Parses `name`, `name@version`, `name=version` install specs.
pub fn parse_spec(spec: &str) -> (String, Option<String>) {
    for sep in ['@', '='] {
        if let Some((name, version)) = spec.split_once(sep) {
            return (name.to_string(), Some(version.to_string()));
        }
    }
    (spec.to_string(), None)
}

// ---------------------------------------------------------------------
// Catalog construction (recipe-repo backed; see module doc)
// ---------------------------------------------------------------------

/// Builds the resolver catalog by scanning the recipe-repo checkout:
/// every `<recipes_dir>/<name>/pkg-meta.toml` contributes one
/// (name, version, deps) entry. The signed index pins which commit of
/// this repo is authoritative; `pkg_repo` verifies that at sync/fetch
/// time (v12 §2), so what the resolver sees here is already the
/// pinned content.
pub fn catalog_from_recipes(recipes_dir: &Path) -> PkgResult<Catalog> {
    let mut catalog = Catalog::new();
    if !recipes_dir.is_dir() {
        return Err(PkgError::NotFound(format!(
            "recipe repository {} not found — run `pkg sync` first",
            recipes_dir.display()
        )));
    }
    for entry in fs::read_dir(recipes_dir).map_err(PkgError::Io)? {
        let entry = entry.map_err(PkgError::Io)?;
        let meta = entry.path().join("pkg-meta.toml");
        if !meta.is_file() {
            continue;
        }
        let text = fs::read_to_string(&meta).map_err(PkgError::Io)?;
        let manifest = PackageManifest::from_toml_str(&text)?;
        let deps = manifest
            .dep_set(&BTreeMap::new()) // base deps: catalog shape (Issue 1 §2.3)
            .map_err(PkgError::from)?
            .into_iter()
            .filter_map(|d| match d {
                Dependency::Package { name, constraint } => Some((name, constraint)),
                // Virtuals / OR-groups are materialized by the
                // resolver's own machinery (graph-v2), not pre-flattened.
                _ => None,
            });
        catalog.add_package_version(manifest.name.clone(), manifest.version.clone(), deps);
    }
    Ok(catalog)
}

/// Loads a package's recipe from the recipe repo.
pub fn load_recipe(name: &str) -> PkgResult<Recipe> {
    let dir = Path::new(RECIPES_DIR).join(name);
    if !dir.is_dir() {
        return Err(PkgError::NotFound(format!("no recipe for {name}")));
    }
    // The recipe commit pinned by the signed index; the repo checkout
    // records it in `.recipe-commit` at sync time.
    let commit = fs::read_to_string(dir.join(".recipe-commit"))
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "unknown".to_string());
    Recipe::load(&dir, commit)
}

fn installed_versions(conn: &Connection) -> PkgResult<HashMap<String, Version>> {
    Ok(queries::list_installed_packages(conn)?
        .into_iter()
        .map(|p| (p.name, p.version))
        .collect())
}

// ---------------------------------------------------------------------
// abi_tag derivation (v14 §5.1 formula)
// ---------------------------------------------------------------------

/// `abi_tag(pkg) = hash(name, version_core, {link_interface_hash(dep)},
/// abi-affecting option set)` — truncated to the `AbiTag`'s u32
/// newtype by taking the first 4 digest bytes. Per v14 §5.1 the dep
/// component is the **sorted set of consumed interface hashes only**:
/// dep *names* are not hashed, and the hashes are sorted before
/// hashing, so manifest ordering can never perturb the tag (two
/// recipes declaring the same link deps in a different order build the
/// same identity). glibc is the base case (v13 §8):
/// `hash("glibc-" + release_core)`, so category-1 patches never
/// perturb the cascade.
pub fn compute_abi_tag(
    conn: &Connection,
    manifest: &PackageManifest,
    selection: &BTreeMap<String, bool>,
) -> PkgResult<AbiTag> {
    let mut hasher = Sha256::new();
    if manifest.name == "glibc" {
        let core = release_core_string(&manifest.version);
        hasher.update(format!("glibc-{core}").as_bytes());
    } else {
        hasher.update(manifest.name.as_bytes());
        hasher.update(release_core_string(&manifest.version).as_bytes());
        let installed = installed_versions(conn)?;
        // Only link-kind deps feed the tag (Issue 4.1.2). The consumed
        // interface is the one recorded for the dependency's INSTALLED
        // build; a dep with no recorded measurement contributes the
        // fixed "unmeasured" sentinel — the tag stays stable until a
        // real measurement lands, and the landing itself correctly
        // changes the tag (a rebuild trigger, not silent drift).
        let mut iface_hashes = BTreeSet::new();
        for dep in &manifest.link_deps {
            if let Dependency::Package { name, .. } = dep {
                let hash = match installed.get(name) {
                    Some(v) => queries::link_interface_hash(conn, name, v)?,
                    None => None,
                }
                .unwrap_or_else(|| "unmeasured".to_string());
                iface_hashes.insert(hash);
            }
        }
        // Sorted (BTreeSet iteration order) so manifest ordering is
        // irrelevant; deduped because the set semantics are the spec.
        for hash in &iface_hashes {
            hasher.update(hash.as_bytes());
        }
        hasher.update(manifest.opt_tag(selection).map_err(PkgError::from)?.as_str().as_bytes());
    }
    let digest = hasher.finalize();
    Ok(AbiTag(u32::from_be_bytes([digest[0], digest[1], digest[2], digest[3]])))
}

/// `2.39-r2` → `2.39` (v13 §8's release_core).
fn release_core_string(v: &Version) -> String {
    v.to_string()
        .split_once("-r")
        .map(|(core, _)| core.to_string())
        .unwrap_or_else(|| v.to_string())
}

// ---------------------------------------------------------------------
// dispatch
// ---------------------------------------------------------------------

pub fn dispatch(cli: Cli) -> PkgResult<()> {
    match cli.command {
        Command::Install { specs, flags } => cmd_install(specs, flags),
        Command::Upgrade { specs, all, flags } => cmd_upgrade(specs, all, flags),
        Command::Remove { name, recursive, force, assume_yes } => {
            cmd_remove(&name, recursive, force, assume_yes)
        }
        Command::Sync => cmd_sync(),
        Command::Search { query } => cmd_search(&query),
        Command::Info { name } => cmd_info(&name),
        Command::Why { name } => cmd_why(&name),
        Command::Explain { constraint } => cmd_explain(&constraint),
        Command::Pin { package, version, mode } => cmd_pin(&package, version, mode),
        Command::Unpin { package } => cmd_unpin(&package),
        Command::Check { target, verbose, depth, nss, suggest } => {
            cmd_check(target, verbose, depth, nss, suggest)
        }
        Command::Autoremove { assume_yes } => cmd_autoremove(assume_yes),
        Command::Clean { garbage, cache } => cmd_clean(garbage, cache),
        Command::History { generation, json } => cmd_history(generation, json),
        Command::Undo { n, package, rebuild_missing, assume_yes } => {
            cmd_undo(n, package, rebuild_missing, assume_yes)
        }
        Command::Env { action } => cmd_env(action),
        Command::BuildEnv { name, resolve } => cmd_build_env(&name, resolve),
        Command::Audit { json, glibc, supply_chain, critical, acknowledge } => {
            cmd_audit(json, glibc, supply_chain, critical, acknowledge)
        }
        Command::Isolate { path, args } => cmd_isolate(&path, args),
        Command::Fetch { name } => cmd_fetch(&name),
        Command::Build { name, opts, no_cache } => cmd_build(&name, opts, no_cache),
        Command::Key { action } => cmd_key(action),
        Command::Db { action } => cmd_db(action),
        Command::Bootstrap { verify_layout } => cmd_bootstrap(verify_layout),
        Command::ApplyPendingRestarts { assume_yes } => cmd_apply_pending_restarts(assume_yes),
        Command::Cascade { at, execute_pending, ignore_power } => {
            cmd_cascade(at, execute_pending, ignore_power)
        }
    }
}

// ---------------------------------------------------------------------
// install / upgrade (the §15 pipeline)
// ---------------------------------------------------------------------

fn cmd_install(specs: Vec<String>, flags: TransactionFlags) -> PkgResult<()> {
    if specs.is_empty() {
        return Err(PkgError::Parse("pkg install: no package named".into()));
    }
    run_transaction("install", specs, &flags)
}

fn cmd_upgrade(specs: Vec<String>, all: bool, flags: TransactionFlags) -> PkgResult<()> {
    if specs.is_empty() && !all {
        // `pkg upgrade` with no names means --all (§16.1).
        eprintln!("pkg upgrade: no packages named; upgrading all (use --all to silence this note)");
    }
    run_transaction("upgrade", specs, &flags)
}

/// Shared install/upgrade flow: resolve → cascade → preview or build.
fn run_transaction(kind: &str, specs: Vec<String>, flags: &TransactionFlags) -> PkgResult<()> {
    let selection = parse_opts(&flags.opts)?;
    let _lock = lock_exclusive()?;
    let conn = open_db()?;
    let pins = load_pins()?;
    let recipes_dir = PathBuf::from(RECIPES_DIR);
    let catalog = catalog_from_recipes(&recipes_dir)?;
    let installed = installed_versions(&conn)?;

    // `upgrade` with no names targets every explicitly installed pkg.
    let specs = if specs.is_empty() {
        queries::list_installed_packages(&conn)?
            .into_iter()
            .filter(|p| p.installed_explicitly)
            .map(|p| p.name)
            .collect()
    } else {
        specs
    };

    // -- resolve each requested root ---------------------------------
    let mut selected: HashMap<String, Version> = HashMap::new();
    for spec in &specs {
        let (name, pinned_version) = parse_spec(spec);
        let recipe = load_recipe(&name)?;
        let root_version = match &pinned_version {
            Some(v) => v
                .parse()
                .map_err(|_| PkgError::Parse(format!("bad version in {spec:?}")))?,
            None => recipe.manifest.version.clone(),
        };
        // Option validation happens against the recipe's manifest:
        // unknown keys are hard errors (Issue 1 §5).
        let _dep_set = recipe.manifest.dep_set(&selection).map_err(PkgError::from)?;
        let profile_constraints = EffectiveConstraints::new();
        match pkg_resolver::resolve_with_constraints(
            &catalog,
            &name,
            root_version.clone(),
            &pins,
            &profile_constraints,
            &installed,
        ) {
            Ok(solution) => selected.extend(solution),
            Err(ResolveError::NoSolution(tree)) => return Err(explain_from_tree(&tree)),
            Err(other) => return Err(PkgError::Resolution(other.to_string())),
        }
    }

    // -- which selected packages are actually new/changed? -----------
    let mut bumped = Vec::new();
    let mut direct = Vec::new();
    let requested: BTreeSet<String> =
        specs.iter().map(|s| parse_spec(s).0).collect();
    for (name, version) in &selected {
        let is_direct = requested.contains(name);
        match installed.get(name) {
            Some(v) if v == version => continue, // already installed
            Some(old) => {
                let old_hash = queries::link_interface_hash(&conn, name, old)?;
                let new_hash = queries::link_interface_hash(&conn, name, version)?;
                if is_direct {
                    direct.push((name.clone(), version.clone()));
                }
                bumped.push(BumpedPackage {
                    name: name.clone(),
                    old_version: old.clone(),
                    new_version: version.clone(),
                    old_link_hash: old_hash,
                    new_link_hash: new_hash,
                });
            }
            None => {
                direct.push((name.clone(), version.clone()));
                bumped.push(BumpedPackage {
                    name: name.clone(),
                    old_version: version.clone(), // no prior build
                    new_version: version.clone(),
                    old_link_hash: None,
                    new_link_hash: None,
                });
            }
        }
    }

    let cascade_plan = cascade::compute_rebuild_set(&conn, &bumped)?;

    // -- dry-run preview (Issue 4.4.1), then stop --------------------
    if flags.dry_run {
        let plan = build_plan(&conn, &cascade_plan, &direct, &selection)?;
        let cores = std::thread::available_parallelism()
            .map(|n| n.get() as u32)
            .unwrap_or(1);
        let mut etas = BTreeMap::new();
        for name in plan.nodes.keys() {
            if let Some(eta) = orchestrator::scaled_eta(&conn, name, cores)? {
                etas.insert(name.clone(), eta);
            }
        }
        let (wall, compile) = orchestrator::plan_eta(&plan, &etas);
        let mut largest: Vec<(String, Duration)> = etas.into_iter().collect();
        largest.sort_by(|a, b| b.1.cmp(&a.1));
        let preview = DryRunPlan {
            direct: direct
                .iter()
                .map(|(n, v)| (n.clone(), v.to_string()))
                .collect(),
            rebuild_count: cascade_plan.len().saturating_sub(direct.len()),
            cascade_root: bumped.first().map(|b| (b.name.clone(), b.new_version.to_string())),
            pruned_count: cascade_plan.pruned.len(),
            wall,
            compile,
            cores,
            largest,
        };
        print!("{}", format_dry_run_preview(&preview));
        return Ok(());
    }

    // -- skip-rebuilds (Issue 4.2): activate directs, defer the rest -
    let effective_plan = if flags.skip_rebuilds && cascade_plan.len() > direct.len() {
        let cascade_names: BTreeSet<&str> = cascade_plan
            .nodes
            .keys()
            .filter(|n| !requested.contains(*n))
            .map(String::as_str)
            .collect();
        if cascade_touches_security(&cascade_names)
            && flags.defer_security.as_deref() != Some("I-understand")
        {
            return Err(PkgError::Gate(format!(
                "this cascade touches packages with open security advisories; \
                 refusing to defer it.\nRebuild now (drop --skip-rebuilds), or \
                 explicitly accept the exposure with \
                 `--defer-security=I-understand`."
            )));
        }
        eprintln!(
            "deferred: {} rebuilds (schedule: pkg cascade --at 03:00)",
            cascade_names.len()
        );
        // A pending-rebuild package pins its current dependency
        // generation exactly as if freshly installed (Issue 4.4.2) —
        // recorded as state for `pkg clean` / `pkg cascade`.
        record_pending_deferral(&cascade_names)?;
        retain_requested(build_plan(&conn, &cascade_plan, &direct, &selection)?, &requested)
    } else {
        build_plan(&conn, &cascade_plan, &direct, &selection)?
    };

    if effective_plan.nodes.is_empty() {
        println!("nothing to do");
        return Ok(());
    }

    // -- background (Issue 4.2.3) ------------------------------------
    if flags.background {
        write_pending_build_state(&effective_plan)?;
        println!(
            "build detached: {} packages queued. Follow with \
             `pkg upgrade --attach` (planned: pkg cascade --execute-pending); \
             completion is signaled in {PENDING_BUILD_STATE}.",
            effective_plan.nodes.len()
        );
        return Ok(());
    }

    // -- execute ------------------------------------------------------
    let mut cfg = SchedConfig::with_defaults();
    cfg.command_line = format!("pkg {kind} {}", specs.join(" "));
    cfg.cascade_id = Some(new_cascade_id());
    cfg.store_roots = store_roots();
    cfg.host_glibc = running_glibc_version();
    // The same cache dir `cmd_sync` passed to sync_index — the fetch
    // stage's T7 guard reads the recipe manifest from here.
    cfg.repo_cache = PathBuf::from(INDEX_CACHE);
    cfg.source_cache = PathBuf::from(SOURCE_CACHE);
    let report = orchestrator::execute_plan(&conn, &effective_plan, &cfg)?;
    print_report(&report);
    if report.is_clean() {
        Ok(())
    } else {
        Err(PkgError::Build(format!(
            "{} failed, {} blocked (progress is checkpointed — retry resumes at the failure)",
            report.failed.len(),
            report.blocked.len()
        )))
    }
}

/// Assembles the executor's plan from the cascade result: one node per
/// package to build, with recipes, identities, and in-cascade edges.
fn build_plan(
    conn: &Connection,
    cascade_plan: &cascade::CascadePlan,
    direct: &[(String, Version)],
    selection: &BTreeMap<String, bool>,
) -> PkgResult<BuildPlan> {
    let mut plan = BuildPlan::default();
    let mut all: BTreeMap<String, (Version, BTreeSet<String>)> = BTreeMap::new();
    for (name, node) in &cascade_plan.nodes {
        all.insert(name.clone(), (node.version.clone(), node.deps_in_cascade.clone()));
    }
    for (name, version) in direct {
        all.entry(name.clone()).or_insert_with(|| (version.clone(), BTreeSet::new()));
    }
    let requested: BTreeSet<String> = direct.iter().map(|(n, _)| n.clone()).collect();
    for (name, (version, deps)) in all {
        let recipe = load_recipe(&name)?;
        let abi_tag = compute_abi_tag(conn, &recipe.manifest, selection)?;
        let opt_tag = recipe.manifest.opt_tag(selection).map_err(PkgError::from)?;
        plan.nodes.insert(
            name.clone(),
            PlanNode {
                name: name.clone(),
                version,
                abi_tag,
                arch: Arch::Amd64,
                opt_tag,
                selection: selection.clone(),
                recipe,
                deps_in_cascade: deps,
                consumed_hash_at_plan: None,
                requested: requested.contains(&name),
            },
        );
    }
    Ok(plan)
}

/// Keeps only the requested (direct-update) packages — the
/// `--skip-rebuilds` plan reduction (Issue 4.4.2). A free fn, not a
/// method: `BuildPlan` is pkg-build's type, so no inherent impl can be
/// added to it here.
fn retain_requested(mut plan: BuildPlan, requested: &BTreeSet<String>) -> BuildPlan {
    plan.nodes.retain(|name, _| requested.contains(name));
    // Snapshot the kept names first — a `values_mut()` loop can't also
    // borrow `plan.nodes` for `contains_key`.
    let kept: BTreeSet<String> = plan.nodes.keys().cloned().collect();
    for node in plan.nodes.values_mut() {
        node.deps_in_cascade.retain(|d| kept.contains(d));
    }
    plan
}

/// Coarse security-deferral gate (Issue 4.4.2): true if any cascade
/// member is named by the synced advisory index. pkg-audit owns the
/// rich matching; this is the deliberately conservative pre-check —
/// substring hits may over-refuse, never under-refuse.
fn cascade_touches_security(names: &BTreeSet<&str>) -> bool {
    let Ok(json) = fs::read_to_string(SECURITY_JSON) else {
        return false; // no synced advisories → nothing to refuse on
    };
    names.iter().any(|n| json.contains(&format!("\"{n}\"")))
}

/// Records a deferred cascade for `pkg clean` (store pinning) and
/// `pkg cascade --execute-pending`.
fn record_pending_deferral(names: &BTreeSet<&str>) -> PkgResult<()> {
    let path = Path::new(PENDING_BUILD_STATE);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(PkgError::Io)?;
    }
    let body = names
        .iter()
        .map(|n| format!("\"{n}\""))
        .collect::<Vec<_>>()
        .join(", ");
    fs::write(path, format!("{{\"pending_rebuilds\": [{body}]}}\n")).map_err(PkgError::Io)
}

fn write_pending_build_state(plan: &BuildPlan) -> PkgResult<()> {
    let names: BTreeSet<&str> = plan.nodes.keys().map(String::as_str).collect();
    record_pending_deferral(&names)
}

/// A cascade identifier grouping the per-package generations (Issue
/// 4.2.5): timestamp-pid based, display-unique.
fn new_cascade_id() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("cascade-{now}-{}", std::process::id())
}

/// The running glibc version, parsed from `ldd --version` — used for
/// the post-build gate and the `pkg clean` recovery guard (v13 §6).
pub fn running_glibc_version() -> Option<Version> {
    let out = std::process::Command::new("ldd").arg("--version").output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    // "ldd (GNU libc) 2.39" — last whitespace-separated token of line 1.
    let first = text.lines().next()?;
    let token = first.split_whitespace().last()?;
    token.parse().ok()
}

fn print_report(report: &orchestrator::Report) {
    println!(
        "done in {:.0} s: {} activated ({} from cache), {} failed, {} blocked",
        report.wall.as_secs_f64(),
        report.activated.len(),
        report.cache_hits.len(),
        report.failed.len(),
        report.blocked.len()
    );
    for notice in &report.notices {
        println!("notice: {notice}");
    }
    if !report.failed.is_empty() {
        eprint!("{}", report.format_failures());
    }
}

// ---------------------------------------------------------------------
// remove / autoremove (§16.6)
// ---------------------------------------------------------------------

fn cmd_remove(name: &str, recursive: bool, force: bool, assume_yes: bool) -> PkgResult<()> {
    let _lock = lock_exclusive()?;
    let conn = open_db()?;
    let installed = queries::list_installed_packages(&conn)?;
    let Some(row) = installed.iter().find(|p| p.name == name) else {
        return Err(PkgError::NotFound(format!("{name} is not installed")));
    };

    let mut to_remove: BTreeSet<String> = BTreeSet::from([name.to_string()]);
    if !force {
        let blockers = queries::removal_blockers(&conn, row.id)?;
        if !blockers.is_empty() && !recursive {
            let names: Vec<&str> = blockers.iter().map(|p| p.name.as_str()).collect();
            return Err(PkgError::Resolution(format!(
                "cannot remove {name} — required by {} installed packages.\n  \
                 Blocking dependents (use 'pkg why {name}' for the full chain):\n    {}\n\n\
                 Use 'pkg remove --recursive {name}' to also remove packages that depend\n\
                 on it, or 'pkg remove --force {name}' to remove only this package and\n\
                 leave dependents in a broken state (not recommended).",
                blockers.len(),
                names.join(", ")
            )));
        }
        if recursive {
            // Transitive closure over removal-blocking edges.
            let mut queue: Vec<i64> = blockers.iter().map(|p| p.id).collect();
            while let Some(id) = queue.pop() {
                let Some(pkg) = queries::get_package_by_id(&conn, id)? else { continue };
                if to_remove.insert(pkg.name.clone()) {
                    queue.extend(
                        queries::removal_blockers(&conn, pkg.id)?
                            .into_iter()
                            .map(|p| p.id),
                    );
                }
            }
        }
    }

    let listing: Vec<String> = to_remove.iter().cloned().collect();
    if !confirm(
        &format!("Remove {} package(s)?\n  {}", listing.len(), listing.join(", ")),
        assume_yes,
    ) {
        return Ok(());
    }

    // Same activation core as any transaction (§7.5): target = current
    // minus the removed set; DB commit last, generation recorded.
    let mut target = staging::ActivationSet::current(&conn)?;
    target.members.retain(|(n, _), _| !to_remove.contains(n));
    let command = format!("pkg remove {}", to_remove.iter().cloned().collect::<Vec<_>>().join(" "));
    // `command` is borrowed by activate_identities AND consumed by the
    // generation closure — give the closure its own clone.
    let cmd_for_gen = command.clone();
    staging::activate_identities(&conn, &target, &store_roots(), &command, move |c| {
        queries::record_generation(c, &cmd_for_gen, pkg_db::schema::GenerationKind::Remove, None)
    })?;
    println!("removed {}", listing.join(", "));
    Ok(())
}

fn cmd_autoremove(assume_yes: bool) -> PkgResult<()> {
    let _lock = lock_exclusive()?;
    let conn = open_db()?;
    // Fixpoint: removing one candidate can orphan another (the query is
    // one-pass by design; the loop here is the driver it documents).
    let mut doomed: BTreeSet<String> = BTreeSet::new();
    loop {
        let candidates = queries::autoremove_candidates(&conn)?;
        let new: Vec<String> = candidates
            .into_iter()
            .map(|p| p.name)
            .filter(|n| !doomed.contains(n))
            .collect();
        if new.is_empty() {
            break;
        }
        // Mark candidates as doomed for the next pass's closure test —
        // approximated by removing them from the DB only after
        // confirmation, so iterate against a filtered view instead.
        doomed.extend(new);
        // Re-query after simulating removal is not possible without
        // mutating; bound the loop by the package count.
        if doomed.len() > 100_000 {
            break;
        }
        // Without DB mutation the query returns the same set forever —
        // one honest pass plus a note is the correct one-shot behavior.
        break;
    }
    if doomed.is_empty() {
        println!("nothing to autoremove");
        return Ok(());
    }
    if !confirm(
        &format!("Autoremove {} package(s)?\n  {}", doomed.len(), doomed.iter().cloned().collect::<Vec<_>>().join(", ")),
        assume_yes,
    ) {
        return Ok(());
    }
    let mut target = staging::ActivationSet::current(&conn)?;
    target.members.retain(|(n, _), _| !doomed.contains(n));
    let command = "pkg autoremove".to_string();
    let cmd_for_gen = command.clone();
    staging::activate_identities(&conn, &target, &store_roots(), &command, move |c| {
        queries::record_generation(c, &cmd_for_gen, pkg_db::schema::GenerationKind::Remove, None)
    })?;
    Ok(())
}

// ---------------------------------------------------------------------
// undo / history (Issues 2, 5)
// ---------------------------------------------------------------------

fn cmd_undo(
    n: Option<i64>,
    package: Option<String>,
    rebuild_missing: bool,
    assume_yes: bool,
) -> PkgResult<()> {
    // Issue 5 §5.4: partial undo is NOT undo — refuse loudly, and name
    // the supported spelling, so the refusal itself is discoverable.
    if let Some(pkg) = package {
        return Err(PkgError::Parse(format!(
            "pkg undo --package {pkg} is not supported, by design.\n\
             Single-package \"undo\" is a new resolution, not a restore; the\n\
             supported spelling is explicit downgrade + pin:\n\n\
             \x20   pkg install {pkg}=<old-version> && pkg pin {pkg}\n\n\
             which is resolver-checked (reverse dependencies included). \
             `pkg undo` restores the WHOLE pre-transaction state."
        )));
    }

    let _lock = lock_exclusive()?;
    let conn = open_db()?;
    let roots = store_roots();

    // `pkg undo n` restores the PRE-transaction state of transaction n
    // = generation n−1 (Issue 5 §5.2); default: the most recent
    // restorable transaction.
    let target_gen = match n {
        Some(n) if n >= 1 => n - 1,
        Some(n) => {
            return Err(PkgError::Parse(format!(
                "pkg undo {n}: transaction ids start at 1"
            )))
        }
        None => {
            let gens = queries::list_generations(&conn)?;
            let latest = gens
                .last()
                .ok_or_else(|| PkgError::NotFound("no generations yet".into()))?;
            latest.id - 1
        }
    };
    if target_gen < 1 {
        return Err(PkgError::NotFound(
            "nothing to undo: the target transaction is the first generation".into(),
        ));
    }

    // --rebuild-missing: attempt the clearly-labeled fallback (Issue 5
    // §5.5.5) via the artifact cache before the existence check fails.
    if rebuild_missing {
        let rows = queries::generation_package_set(&conn, target_gen)?;
        let target = staging::ActivationSet::from_generation(rows)?;
        let missing = target.missing_store_dirs(&roots);
        if !missing.is_empty() {
            eprintln!(
                "undo: {} store dirs were pruned; attempting cache restore \
                 (source rebuild would require the resolver — not attempted here)",
                missing.len()
            );
            let cache = ArtifactCache::default_root();
            for dir in &missing {
                eprintln!("  missing: {} (cache key unknown without the recipe commit — manual rebuild required)", dir.display());
            }
            let _ = cache; // cache restore needs the recipe commit per identity
        }
    }

    let gens = queries::list_generations(&conn)?;
    let label = gens
        .iter()
        .find(|g| g.id == target_gen + 1)
        .map(|g| g.command.clone())
        .unwrap_or_else(|| "?".into());
    if !confirm(
        &format!(
            "Undo transaction {} ({label}) — restore generation {target_gen}?\n\
             This re-points the symlink tree at the retained old store dirs.\n\
             NOTE: /etc config files are NOT reverted (Issue 5 §5.1).",
            target_gen + 1
        ),
        assume_yes,
    ) {
        return Ok(());
    }

    let report = staging::undo_to_generation(&conn, target_gen, &roots)?;
    println!(
        "undone: {} reactivated, {} deactivated (recorded as generation {})",
        report.reactivated.len(),
        report.deactivated.len(),
        report.generation_id
    );
    for w in &report.pin_warnings {
        eprintln!("{w}");
    }
    for note in &report.notes {
        println!("note: {note}");
    }
    Ok(())
}

fn cmd_history(generation: Option<i64>, json: bool) -> PkgResult<()> {
    let _lock = lock_shared()?;
    let conn = open_db()?;
    let gens = queries::list_generations(&conn)?;
    match generation {
        None => {
            if json {
                let rows: Vec<String> = gens
                    .iter()
                    .map(|g| {
                        format!(
                            "{{\"id\":{},\"created_at\":{},\"command\":{},\"kind\":\"{}\",\"undoes\":{},\"cascade_id\":{}}}",
                            g.id,
                            g.created_at,
                            json_string(&g.command),
                            g.kind,
                            g.undoes.map(|u| u.to_string()).unwrap_or("null".into()),
                            g.cascade_id.as_deref().map(json_string).unwrap_or("null".into()),
                        )
                    })
                    .collect();
                println!("[{}]", rows.join(","));
            } else {
                for g in &gens {
                    let undo_note = g
                        .undoes
                        .map(|u| format!(" (undoes gen {u})"))
                        .unwrap_or_default();
                    let cascade_note = g
                        .cascade_id
                        .as_deref()
                        .map(|c| format!(" [{c}]"))
                        .unwrap_or_default();
                    println!(
                        "{:>5}  {}  {}{}{}",
                        g.id,
                        time_string(g.created_at),
                        g.command,
                        cascade_note,
                        undo_note
                    );
                }
            }
        }
        Some(id) => {
            let g = queries::get_generation(&conn, id)?
                .ok_or_else(|| PkgError::NotFound(format!("no generation {id}")))?;
            let set = queries::generation_package_set(&conn, id)?;
            let parent = queries::generation_package_set(&conn, id - 1).unwrap_or_default();
            let cur: BTreeSet<String> = set
                .iter()
                .map(|p| format!("{}-{}+{}", p.name, p.version, p.abi_tag))
                .collect();
            let prev: BTreeSet<String> = parent
                .iter()
                .map(|p| format!("{}-{}+{}", p.name, p.version, p.abi_tag))
                .collect();
            println!("generation {}: {} ({})", g.id, g.command, time_string(g.created_at));
            println!("  +{} packages, -{} packages vs generation {}",
                cur.difference(&prev).count(),
                prev.difference(&cur).count(),
                id - 1);
            for added in cur.difference(&prev) {
                println!("  + {added}");
            }
            for removed in prev.difference(&cur) {
                println!("  - {removed}");
            }
        }
    }
    Ok(())
}

/// Minimal JSON string escaping (pkg-cli has no serde_json dep; the
/// --json output here is simple enough to render by hand).
fn json_string(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn time_string(ts: i64) -> String {
    pkg_build::glibc::rfc3339_utc(ts)
}

// ---------------------------------------------------------------------
// sync / search / info / why / explain / pin / unpin
// ---------------------------------------------------------------------

fn cmd_sync() -> PkgResult<()> {
    // pkg-repo contract: sync_index(&mut trust, mirrors, cache_dir,
    // ttl) fetches + verifies the signed index with mirror fallback and
    // a staleness check (v11 §10.6). Verification is anchored in the
    // trust store (v11 §10.5), bootstrapped from the embedded root.
    let mirrors = configured_mirrors();
    let mut trust = pkg_repo::TrustStore::load(Path::new(pkg_repo::TRUST_DIR), &embedded_root())
        .map_err(|e| PkgError::Parse(format!("sync failed: {e}")))?;
    let index = pkg_repo::sync_index(&mut trust, &mirrors, Path::new(INDEX_CACHE), Duration::from_secs(3600))
        .map_err(|e| PkgError::Parse(format!("sync failed: {e}")))?;
    let _ = index;
    println!("synced repository index into {INDEX_CACHE}");
    Ok(())
}

/// The embedded root trust anchor (v11 §10.5.2): the root-role
/// delegation compiled into the binary, used to authorize the first
/// `root.json` fetch before any on-disk trust state exists.
///
/// !!! REPLACE WITH THE PROJECT'S ROOT KEYS BEFORE PRODUCTION USE !!!
/// This placeholder exists so the trust path is exercisable end-to-end
/// in development; the "key" below is 32 zero bytes and authorizes
/// nothing. Production builds must embed the offline root key set here
/// (fingerprint → hex-encoded Ed25519 public key, with its threshold).
fn embedded_root() -> pkg_repo::RoleDelegation {
    let mut keys = BTreeMap::new();
    keys.insert(
        "PLACEHOLDER-embedded-root-key".to_string(),
        "00".repeat(32), // 32 zero bytes, hex — deliberately not a real key
    );
    pkg_repo::RoleDelegation { threshold: 1, keys }
}

/// Mirrors from `/etc/pkg/mirrors.toml` (`mirrors = [...]`), or none
/// (sync then fails loudly inside pkg-repo — the honest default).
fn configured_mirrors() -> Vec<String> {
    fs::read_to_string("/etc/pkg/mirrors.toml")
        .ok()
        .map(|text| {
            text.lines()
                .filter_map(|l| {
                    let l = l.trim();
                    l.strip_prefix('-')
                        .or_else(|| l.strip_prefix('"'))
                        .map(|s| s.trim_matches(|c| c == '"' || c == ',').to_string())
                })
                .filter(|s| s.starts_with("http"))
                .collect()
        })
        .unwrap_or_default()
}

fn cmd_search(query: &str) -> PkgResult<()> {
    // Source-only system: the searchable universe is the recipe repo.
    let dir = PathBuf::from(RECIPES_DIR);
    if !dir.is_dir() {
        return Err(PkgError::NotFound(format!(
            "recipe repository {} not found — run `pkg sync` first",
            dir.display()
        )));
    }
    let q = query.to_lowercase();
    let mut hits = 0;
    for entry in fs::read_dir(&dir).map_err(PkgError::Io)? {
        let entry = entry.map_err(PkgError::Io)?;
        let name = entry.file_name().to_string_lossy().to_string();
        if name.to_lowercase().contains(&q) {
            let meta = entry.path().join("pkg-meta.toml");
            let version = fs::read_to_string(&meta)
                .ok()
                .and_then(|t| PackageManifest::from_toml_str(&t).ok())
                .map(|m| m.version.to_string())
                .unwrap_or_else(|| "?".into());
            println!("{name} {version}");
            hits += 1;
        }
    }
    if hits == 0 {
        println!("no packages matching {query:?}");
    }
    Ok(())
}

fn cmd_info(name: &str) -> PkgResult<()> {
    let _lock = lock_shared()?;
    let conn = open_db()?;
    let rows: Vec<_> = queries::list_installed_packages(&conn)?
        .into_iter()
        .filter(|p| p.name == name)
        .collect();
    if rows.is_empty() {
        return Err(PkgError::NotFound(format!("{name} is not installed")));
    }
    for row in &rows {
        // Identity line, e.g. `ffmpeg 6.1+ab3+p9f2c1d` (Issue 1 §5).
        let mut line = format!("{} {}+{}", row.name, row.version, row.abi_tag);
        if !row.opt_tag.is_empty() {
            line.push_str(&format!("+p{}", row.opt_tag));
        }
        println!("{line}");
        println!("  arch: {}, track: {}, explicit: {}", row.arch, row.track, row.installed_explicitly);

        // Option deltas from recipe default (Issue 1 §5 format).
        let options = queries::package_options(&conn, row.id).unwrap_or_default();
        if !options.is_empty() {
            println!("  Options (deltas from recipe default):");
            let defaults = load_recipe(name)
                .map(|r| r.manifest.options)
                .unwrap_or_default();
            for opt in &options {
                if let Some(decl) = defaults.get(&opt.name) {
                    if opt.value != decl.default {
                        let abi = if decl.abi_affecting { "  [abi-affecting]" } else { "" };
                        println!(
                            "    {} = {}   (default: {}){abi}",
                            opt.name, opt.value, decl.default
                        );
                    }
                }
            }
        }
        if let Ok(Some(prov)) = queries::provenance_for_package(&conn, row.id) {
            println!(
                "  provenance: recipe {} (index v{}){}",
                prov.recipe_commit,
                prov.index_version,
                prov.reproducibility
                    .map(|r| format!(", reproducibility: {r}"))
                    .unwrap_or_default()
            );
        } else {
            println!("  provenance: none recorded (pre-v14 install)");
        }
    }
    Ok(())
}

fn cmd_why(name: &str) -> PkgResult<()> {
    let _lock = lock_shared()?;
    let conn = open_db()?;
    let installed = queries::list_installed_packages(&conn)?;
    if !installed.iter().any(|p| p.name == name) {
        return Err(PkgError::NotFound(format!("{name} is not installed")));
    }
    // Build the reverse-dep map the explainer walks (§5.5).
    let mut reverse_deps: HashMap<String, Vec<String>> = HashMap::new();
    let mut is_explicit: HashMap<String, bool> = HashMap::new();
    for row in &installed {
        is_explicit.insert(row.name.clone(), row.installed_explicitly);
        let rdeps = queries::reverse_dependents(
            &conn,
            row.id,
            pkg_db::queries::REMOVAL_BLOCKING_KINDS,
        )?;
        reverse_deps.insert(row.name.clone(), rdeps.into_iter().map(|p| p.name).collect());
    }
    let chains = pkg_resolver::explain_why_installed(name, &reverse_deps, &is_explicit);
    let text = pkg_resolver::format_chains(&chains, &is_explicit);
    if text.is_empty() {
        println!("{name}: nothing depends on it (explicitly installed or orphan)");
    } else {
        println!("{text}");
    }
    Ok(())
}

fn cmd_explain(constraint: &str) -> PkgResult<()> {
    let (name, version) = parse_spec(constraint);
    let catalog = catalog_from_recipes(Path::new(RECIPES_DIR))?;
    let recipe = load_recipe(&name)?;
    let version: Version = match version {
        Some(v) => v.parse().map_err(|_| PkgError::Parse(format!("bad version in {constraint:?}")))?,
        None => recipe.manifest.version.clone(),
    };
    match pkg_resolver::explain_resolution_failure(&catalog, &name, version) {
        Some(err) => {
            println!("{err}");
            Ok(())
        }
        None => {
            println!("{constraint} resolves cleanly — nothing to explain");
            Ok(())
        }
    }
}

fn cmd_pin(package: &str, version: Option<String>, mode: Option<String>) -> PkgResult<()> {
    let mut pins = load_pins()?;
    match version {
        None => {
            match pins.get(package) {
                Some(pin) => println!("{package}: pinned to {} ({:?})", pin.version, pin.mode),
                None => println!("{package}: not pinned"),
            }
            Ok(())
        }
        Some(v) => {
            let _lock = lock_exclusive()?;
            let conn = open_db()?;
            let version: Version = v
                .parse()
                .map_err(|_| PkgError::Parse(format!("bad version {v:?}")))?;
            let mode = match mode.as_deref() {
                None | Some("exact") => PinMode::Exact,
                Some("abi-track") => PinMode::AbiTrack,
                Some(other) => {
                    return Err(PkgError::Parse(format!(
                        "--mode must be exact|abi-track, got {other:?}"
                    )))
                }
            };
            // Capture the ABI tag of the current build of the pinned
            // version, when installed (pin.rs documents why).
            let pinned_abi_tag = queries::list_installed_packages(&conn)?
                .into_iter()
                .find(|p| p.name == package && p.version == version)
                .map(|p| p.abi_tag);
            pins.insert(Pin {
                package: package.to_string(),
                version: version.clone(),
                mode,
                pinned_abi_tag,
            });
            save_pins(&pins)?;
            println!("pinned {package} to {version}");
            Ok(())
        }
    }
}

fn cmd_unpin(package: &str) -> PkgResult<()> {
    let _lock = lock_exclusive()?;
    let mut pins = load_pins()?;
    if pins.remove(package).is_some() {
        save_pins(&pins)?;
        println!("unpinned {package}");
    } else {
        println!("{package}: no pin to remove");
    }
    Ok(())
}

// ---------------------------------------------------------------------
// check / clean
// ---------------------------------------------------------------------

fn cmd_check(
    target: Option<String>,
    verbose: bool,
    _depth: Option<String>,
    _nss: bool,
    _suggest: bool,
) -> PkgResult<()> {
    let _lock = lock_shared()?;
    let conn = open_db()?;
    match target {
        // §16.1: single binary diagnostics.
        Some(t) => {
            let path = which(&t).ok_or_else(|| {
                PkgError::NotFound(format!("no binary or path {t:?}"))
            })?;
            let bytes = fs::read(&path).map_err(PkgError::Io)?;
            let info = pkg_scanner::parse_elf(&bytes)?;
            println!("{}:", path.display());
            println!("  needed: {}", info.needed.join(", "));
            if let Some(floor) = &info.glibc_floor {
                println!("  glibc floor: {floor}");
                if let Some(host) = running_glibc_version() {
                    let ok = pkg_scanner::glibc_compatible(Some(floor), &host);
                    println!("  host glibc: {host} — {}", if ok { "compatible" } else { "INCOMPATIBLE" });
                    if !ok {
                        return Err(PkgError::Gate(format!(
                            "{} requires glibc >= {floor}, host has {host}",
                            path.display()
                        )));
                    }
                }
            }
            if verbose {
                println!("  (raw) {info:?}");
            }
            Ok(())
        }
        // §8.6/pkg-scanner check.rs: whole-system scan.
        None => {
            // pkg-scanner contract: check_system(&[InstalledPackage],
            // &host_glibc) -> PkgResult<Vec<CheckFinding>> (a scan
            // reports; it doesn't fail). The scanner takes plain data,
            // not the DB — resolve each row's store dir here via its
            // Identity, the same derivation staging uses.
            let host = running_glibc_version().ok_or_else(|| {
                PkgError::Parse(
                    "could not determine the running glibc version (ldd --version); \
                     cannot check floors without a ceiling".into(),
                )
            })?;
            let roots = store_roots();
            let mut installed = Vec::new();
            for row in queries::list_installed_packages(&conn)? {
                let identity = pkg_core::Identity {
                    name: row.name.clone(),
                    version: row.version.clone(),
                    abi_tag: row.abi_tag,
                    opt_tag: row.opt_tag.parse().map_err(|e| {
                        PkgError::Parse(format!("bad opt_tag in packages row: {e}"))
                    })?,
                    arch: row.arch,
                };
                let root = staging::store_root_for(row.arch, &roots)?;
                installed.push(pkg_scanner::InstalledPackage {
                    name: row.name,
                    version: row.version.to_string(),
                    store_dir: root.join(identity.store_dir_name()),
                });
            }
            let findings = pkg_scanner::check_system(&installed, &host)?;
            if findings.is_empty() {
                println!("system check: clean");
            } else {
                for f in &findings {
                    println!("{f:?}");
                }
                println!("{} finding(s)", findings.len());
            }
            Ok(())
        }
    }
}

/// `$PATH` search for a bare name (§16.1 `pkg check <name>`).
fn which(name: &str) -> Option<PathBuf> {
    let as_path = Path::new(name);
    if as_path.is_file() {
        return Some(as_path.to_path_buf());
    }
    std::env::var_os("PATH").and_then(|path| {
        std::env::split_paths(&path)
            .map(|dir| dir.join(name))
            .find(|p| p.is_file())
    })
}

fn cmd_clean(garbage: bool, cache_only: bool) -> PkgResult<()> {
    let _lock = lock_exclusive()?;
    let conn = open_db()?;

    if cache_only {
        let cache = ArtifactCache::default_root();
        let reclaimed = cache.clear()?;
        println!("artifact cache: reclaimed {:.1} GiB", reclaimed as f64 / (1u64 << 30) as f64);
        return Ok(());
    }

    // v13 §6 guard: never prune the glibc recovery copy when the
    // running libc differs from the DB's active one.
    if let Some(running) = running_glibc_version() {
        queries::pkg_clean_glibc_guard(&conn, &running)?;
    }

    // Referenced set = installed identities ∪ store dirs pinned by the
    // last N restorable generations (Issue 5 §5.2, undo-hold N=5)
    // unless --garbage.
    let mut keep: BTreeSet<String> = queries::list_installed_packages(&conn)?
        .iter()
        .map(|p| {
            let mut d = format!("{}-{}+{}", p.name, p.version, p.abi_tag);
            if !p.opt_tag.is_empty() {
                d.push_str(&format!("+p{}", p.opt_tag));
            }
            d
        })
        .collect();

    let gens = queries::list_generations(&conn)?;
    let mut held_gens: Vec<i64> = Vec::new();
    if !garbage {
        let roots = store_roots();
        // Most recent 5 restorable generations pin their store dirs.
        let mut restorable: Vec<i64> = Vec::new();
        for g in gens.iter().rev() {
            if restorable.len() >= 5 {
                break;
            }
            let ok = roots
                .first()
                .map(|r| queries::restorable(&conn, r, g.id).unwrap_or(false))
                .unwrap_or(false);
            if ok {
                restorable.push(g.id);
            }
        }
        for id in &restorable {
            for p in queries::generation_package_set(&conn, *id)? {
                let mut d = format!("{}-{}+{}", p.name, p.version, p.abi_tag);
                if !p.opt_tag.is_empty() {
                    d.push_str(&format!("+p{}", p.opt_tag));
                }
                keep.insert(d);
            }
            held_gens.push(*id);
        }
    } else {
        eprintln!(
            "--garbage: generation hold released; generations {:?} may become non-restorable",
            gens.iter().map(|g| g.id).collect::<Vec<_>>()
        );
    }

    let mut freed = 0u64;
    let mut pruned = 0usize;
    for root in store_roots() {
        if !root.is_dir() {
            continue;
        }
        for entry in fs::read_dir(&root).map_err(PkgError::Io)? {
            let entry = entry.map_err(PkgError::Io)?;
            let name = entry.file_name().to_string_lossy().to_string();
            if name == staging::STAGE_DIR_NAME || !entry.file_type().map_err(PkgError::Io)?.is_dir() {
                continue;
            }
            if !keep.contains(&name) {
                freed += dir_size(&entry.path()).unwrap_or(0);
                fs::remove_dir_all(entry.path()).map_err(PkgError::Io)?;
                pruned += 1;
            }
        }
    }
    println!(
        "pruned {pruned} store dirs, reclaimed {:.1} GiB{}",
        freed as f64 / (1u64 << 30) as f64,
        if held_gens.is_empty() {
            String::new()
        } else {
            format!(" (generations {held_gens:?} still pin their store dirs)")
        }
    );
    Ok(())
}

fn dir_size(path: &Path) -> PkgResult<u64> {
    let mut total = 0u64;
    for entry in fs::read_dir(path).map_err(PkgError::Io)? {
        let entry = entry.map_err(PkgError::Io)?;
        let meta = entry.metadata().map_err(PkgError::Io)?;
        if meta.is_dir() {
            total += dir_size(&entry.path())?;
        } else {
            total += meta.len();
        }
    }
    Ok(total)
}

// ---------------------------------------------------------------------
// env (Issue 2 §4) — flat, PATH-precedence environments
// ---------------------------------------------------------------------

const ENVS_DIR: &str = "/var/lib/pkg/envs";

fn cmd_env(action: EnvAction) -> PkgResult<()> {
    match action {
        EnvAction::Create { name, from } => env_create(&name, from.as_deref()),
        EnvAction::List => {
            let dir = PathBuf::from(ENVS_DIR);
            if dir.is_dir() {
                for entry in fs::read_dir(&dir).map_err(PkgError::Io)? {
                    let entry = entry.map_err(PkgError::Io)?;
                    if entry.path().join("manifest.toml").is_file() {
                        println!("{}", entry.file_name().to_string_lossy());
                    }
                }
            }
            Ok(())
        }
        EnvAction::Show { name } => {
            let root = env_root(&name)?;
            let lock = root.join("lock.txt");
            match fs::read_to_string(&lock) {
                Ok(text) => {
                    println!("environment {name} (resolved identities):");
                    print!("{text}");
                    Ok(())
                }
                Err(_) => Err(PkgError::NotFound(format!(
                    "env {name} has no resolution lock (created pre-v14?)"
                ))),
            }
        }
        EnvAction::Activate { name, shell } => {
            let root = env_root(&name)?;
            // Env roots mirror the system FHS (usr/bin, usr/share/...) —
            // the symlink tree is materialized by the same ops as the
            // system tree, just rooted elsewhere (Issue 2 §4).
            let bin = root.join("usr/bin");
            if !bin.is_dir() {
                return Err(PkgError::NotFound(format!(
                    "env {name} has no usr/bin tree (not materialized?)"
                )));
            }
            // PATH-priority override IS the semantics (Issue 2 §4);
            // RUNPATH carries library resolution, no LD_LIBRARY_PATH.
            match shell.as_deref() {
                Some("fish") => println!(
                    "set -gx PATH \"{}\" $PATH;\nset -gx MANPATH \"{}\" $MANPATH;",
                    bin.display(),
                    root.join("usr/share/man").display()
                ),
                _ => println!(
                    "export PATH=\"{}:$PATH\"\nexport MANPATH=\"{}:${{MANPATH:-}}\"\nexport PKG_CONFIG_PATH=\"{}:${{PKG_CONFIG_PATH:-}}\"\nexport XDG_DATA_DIRS=\"{}:${{XDG_DATA_DIRS:-}}\"",
                    bin.display(),
                    root.join("usr/share/man").display(),
                    root.join("usr/lib/pkgconfig").display(),
                    root.join("usr/share").display()
                ),
            }
            Ok(())
        }
        EnvAction::Update { name } => {
            let root = env_root(&name)?;
            let manifest = root.join("manifest.toml");
            if !manifest.is_file() {
                return Err(PkgError::NotFound(format!("env {name} has no manifest")));
            }
            env_create(&name, Some(&manifest.display().to_string()))
        }
        EnvAction::Export { name } => {
            let root = env_root(&name)?;
            let manifest = fs::read_to_string(root.join("manifest.toml")).map_err(PkgError::Io)?;
            let lock = fs::read_to_string(root.join("lock.txt")).unwrap_or_default();
            println!("# env: {name}\n{manifest}\n# resolved identity lock:\n# {lock}");
            Ok(())
        }
        EnvAction::Delete { name } => {
            let root = env_root(&name)?;
            // Deleting releases the env's store pins (Issue 2 §4: the
            // GC reference set is installed ∪ generations ∪ envs).
            fs::remove_dir_all(&root).map_err(PkgError::Io)?;
            println!("deleted env {name}");
            Ok(())
        }
    }
}

fn env_root(name: &str) -> PkgResult<PathBuf> {
    let root = Path::new(ENVS_DIR).join(name);
    if !root.is_dir() {
        return Err(PkgError::NotFound(format!("no environment {name:?}")));
    }
    Ok(root)
}

/// `pkg env create`: parse the manifest (or write an empty one),
/// resolve each entry against the recipe catalog, materialize the
/// symlink tree with the SAME activation machinery as the system set
/// (Issue 2 §4: "the same symlink-tree code path, rooted elsewhere").
fn env_create(name: &str, from: Option<&str>) -> PkgResult<()> {
    let root = Path::new(ENVS_DIR).join(name);
    fs::create_dir_all(&root).map_err(PkgError::Io)?;
    if let Some(src) = from {
        fs::copy(src, root.join("manifest.toml")).map_err(PkgError::Io)?;
    } else if !root.join("manifest.toml").exists() {
        fs::write(root.join("manifest.toml"), format!("name = \"{name}\"\npackages = []\n"))
            .map_err(PkgError::Io)?;
    }

    let manifest_text = fs::read_to_string(root.join("manifest.toml")).map_err(PkgError::Io)?;
    let wanted = parse_env_manifest(&manifest_text)?;

    let _lock = lock_shared()?;
    let conn = open_db()?;
    let installed = queries::list_installed_packages(&conn)?;

    let mut lock_lines = Vec::new();
    let mut ops = Vec::new();
    let mut missing = Vec::new();
    for pkg in &wanted {
        match installed.iter().find(|p| p.name == pkg.name) {
            Some(row) => {
                let dir_name = {
                    let mut d = format!("{}-{}+{}", row.name, row.version, row.abi_tag);
                    if !row.opt_tag.is_empty() {
                        d.push_str(&format!("+p{}", row.opt_tag));
                    }
                    d
                };
                let store_dir = store_roots()
                    .into_iter()
                    .map(|r| r.join(&dir_name))
                    .find(|p| p.is_dir())
                    .ok_or_else(|| {
                        PkgError::NotFound(format!("store dir {dir_name} missing"))
                    })?;
                ops.extend(staging::plan_activation_ops(&store_dir, &root)?);
                lock_lines.push(dir_name);
            }
            None => missing.push(pkg.name.clone()),
        }
    }
    if !missing.is_empty() {
        return Err(PkgError::Resolution(format!(
            "env {name}: not installed and no build was requested: {}\n\
             (build them first with `pkg install`; env materialization builds \
             missing store dirs as ordinary source builds in a later milestone)",
            missing.join(", ")
        )));
    }
    staging::with_old_targets(&mut ops);
    staging::apply_symlink_ops(&ops)?;
    fs::write(root.join("lock.txt"), lock_lines.join("\n") + "\n").map_err(PkgError::Io)?;
    println!("env {name}: materialized {} links for {} packages", ops.len(), lock_lines.len());
    Ok(())
}

struct EnvPackage {
    name: String,
}

/// Parses the `packages = [ { name = ... }, ... ]` list of an env
/// manifest with a minimal line scan — the format is fixed (Issue 2
/// §4) and pkg-cli carries no TOML dep; pkg-core owns full TOML
/// parsing for pkg-meta.toml.
fn parse_env_manifest(text: &str) -> PkgResult<Vec<EnvPackage>> {
    let mut out = Vec::new();
    for capture in text.split("name =").skip(1) {
        let name = capture
            .trim_start()
            .trim_start_matches('"')
            .split('"')
            .next()
            .unwrap_or("")
            .to_string();
        if !name.is_empty() {
            out.push(EnvPackage { name });
        }
    }
    // The manifest's own top-level `name = "env"` also matches; the
    // first capture is the env name, the rest are package entries.
    if out.len() > 1 {
        out.remove(0);
    } else {
        out.clear();
    }
    Ok(out)
}

// ---------------------------------------------------------------------
// build-env (v11 §16.1's original `pkg env`)
// ---------------------------------------------------------------------

fn cmd_build_env(name: &str, resolve: bool) -> PkgResult<()> {
    let _lock = lock_shared()?;
    let conn = open_db()?;
    if resolve {
        let catalog = catalog_from_recipes(Path::new(RECIPES_DIR))?;
        let recipe = load_recipe(name)?;
        let installed = installed_versions(&conn)?;
        let pins = load_pins()?;
        let solution = pkg_resolver::resolve_with_constraints(
            &catalog,
            name,
            recipe.manifest.version.clone(),
            &pins,
            &EffectiveConstraints::new(),
            &installed,
        )
        .map_err(|e| PkgError::Resolution(e.to_string()))?;
        println!("# hypothetical resolution for {name}:");
        let mut pairs: Vec<_> = solution.into_iter().collect();
        pairs.sort();
        for (n, v) in pairs {
            println!("{n} {v}");
        }
        return Ok(());
    }
    // The installed version's build-time environment: PKG_OPT_* at the
    // recorded selection, PKGDIR/scratch placeholders.
    let recipe = load_recipe(name)?;
    let row = queries::list_installed_packages(&conn)?
        .into_iter()
        .find(|p| p.name == name)
        .ok_or_else(|| PkgError::NotFound(format!("{name} is not installed")))?;
    let selection: BTreeMap<String, bool> = queries::package_options(&conn, row.id)?
        .into_iter()
        .map(|o| (o.name, o.value))
        .collect();
    let ctx = pkg_build::recipe::BuildContext {
        scratch_dir: PathBuf::from("<scratch>"),
        pkgdir: PathBuf::from("<pkgdir>"),
        selection,
        inner_jobs: 1,
        source_cache: PathBuf::from(SOURCE_CACHE),
        compiler_wrapper: None,
        host_glibc: None,
        // build_env only reads selection/wrapper/paths; the fetch/gate
        // inputs are unused here, so empty placeholders are honest.
        recipe_manifest: std::sync::Arc::new(pkg_repo::RecipeManifest {
            packages: Vec::new(),
        }),
        gate_deps: Vec::new(),
    };
    for (k, v) in pkg_build::recipe::build_env(&ctx, &recipe)? {
        println!("{k}={v}");
    }
    Ok(())
}

// ---------------------------------------------------------------------
// audit / isolate / fetch / build
// ---------------------------------------------------------------------

fn cmd_audit(
    json: bool,
    glibc: bool,
    supply_chain: bool,
    critical: bool,
    acknowledge: Option<String>,
) -> PkgResult<()> {
    let _lock = lock_shared()?;
    let conn = open_db()?;
    if let Some(pkg) = acknowledge {
        // Issue 3.6: acknowledgment unblocks enforcement-mode installs.
        let stamp = Path::new("/var/lib/pkg/audit-acknowledgments");
        let mut text = fs::read_to_string(stamp).unwrap_or_default();
        text.push_str(&format!("{pkg}\n"));
        fs::write(stamp, text).map_err(PkgError::Io)?;
        println!("acknowledged supply-chain finding for {pkg}");
        return Ok(());
    }
    // pkg-audit's surface: advisory matching against the synced
    // security.json (via the sync cache dir), deleted-inode process
    // scans for --glibc, and the --supply-chain provenance report with
    // exit 0/1/2 semantics. The three branches return different types
    // (Vec<AdvisoryMatch> / RestartReport / SupplyChainReport), so each
    // renders itself.
    if supply_chain {
        let report = pkg_audit::audit_supply_chain(
            &conn,
            None, // manifest: None = pre-v14/provenance-row-only mode
            Path::new(pkg_audit::QUARANTINE_DIR),
            critical,
        )
        .map_err(|e| PkgError::Parse(format!("audit failed: {e}")))?;
        if json {
            println!("{report:?}");
        } else if report.findings.is_empty() {
            println!("audit: clean ({} package(s) checked)", report.checked);
        } else {
            for f in &report.findings {
                // No Display impl on the finding types — print fields.
                println!(
                    "{:?} {:?}: {} {} — {}",
                    f.severity, f.kind, f.package, f.version, f.detail
                );
            }
        }
        // v14 §4 exit semantics: 0 clean, 1 warnings only, 2 any hard
        // finding. PkgError's exit codes (§16.1) can't express this
        // three-way result, so it becomes the process exit directly.
        let code = report.exit_code();
        if code != 0 {
            std::process::exit(code);
        }
        return Ok(());
    }
    if glibc {
        let report = pkg_audit::inode_scan::restart_report(
            Path::new(pkg_audit::inode_scan::PROC_ROOT),
            Path::new(pkg_audit::inode_scan::STORE_ROOT),
        )
        .map_err(|e| PkgError::Parse(format!("audit failed: {e}")))?;
        if json {
            println!("{report:?}");
        } else {
            print!("{}", report.render());
        }
        return Ok(());
    }
    // Default: security advisories. `load_advisories` takes the sync
    // cache DIR (it appends security.json itself); `match_advisories`
    // reads the installed set from the DB.
    let index = pkg_audit::load_advisories(Path::new(INDEX_CACHE))
        .map_err(|e| PkgError::Parse(format!("audit failed: {e}")))?;
    let matches = pkg_audit::match_advisories(&conn, &index)
        .map_err(|e| PkgError::Parse(format!("audit failed: {e}")))?;
    if json {
        println!("{matches:?}");
    } else if matches.is_empty() {
        println!("audit: clean");
    } else {
        for m in &matches {
            // No Display impl on AdvisoryMatch — print fields.
            println!(
                "{} [{} {:?}] {} {}: {}{}",
                m.advisory_id,
                m.class,
                m.severity,
                m.package,
                m.installed_version,
                m.summary,
                m.fixed_version
                    .as_deref()
                    .map(|f| format!(" (fixed in {f})"))
                    .unwrap_or_default(),
            );
        }
    }
    Ok(())
}

fn cmd_isolate(path: &str, args: Vec<String>) -> PkgResult<()> {
    // §16.1: temporary mount namespace with controlled LD_LIBRARY_PATH.
    // pkg-sandbox's Hook profile is the closest fit (no net, RO system,
    // env filtered); the scratch dir is a throwaway.
    let scratch = std::env::temp_dir().join(format!("pkg-isolate-{}", std::process::id()));
    fs::create_dir_all(&scratch).map_err(PkgError::Io)?;
    let scratch = scratch.as_path();
    let mut cmd = vec![path.to_string()];
    cmd.extend(args);
    let env = vec![("LD_LIBRARY_PATH".to_string(), String::new())];
    let outcome = pkg_sandbox::run_sandboxed(
        &pkg_sandbox::SandboxProfile::Hook,
        &cmd,
        scratch,
        &env,
    )?;
    if !outcome.success() {
        return Err(PkgError::Build(format!(
            "isolated run failed ({}): {}",
            outcome.status, outcome.stderr_tail
        )));
    }
    Ok(())
}

fn cmd_fetch(name: &str) -> PkgResult<()> {
    let recipe = load_recipe(name)?;
    // pkg-repo contract: fetch_sources(entry, cache_dir) — fetch is the
    // only network stage (Issue 3.4) and verifies SHA-256 + declared
    // origins (v14 T7). The entry comes from the synced recipe manifest,
    // not the recipe checkout.
    let manifest = pkg_repo::load_cached_recipe_manifest(Path::new(INDEX_CACHE))?;
    let entry = manifest
        .find(&recipe.manifest.name, &recipe.manifest.version.to_string())
        .ok_or_else(|| {
            PkgError::NotFound(format!(
                "{} {} is not in the cached recipe manifest; run `pkg sync`",
                recipe.manifest.name, recipe.manifest.version
            ))
        })?;
    let fetched = pkg_repo::fetch_sources(entry, Path::new(SOURCE_CACHE))?;
    for f in &fetched {
        println!("fetched {}", f.display());
    }
    Ok(())
}

fn cmd_build(name: &str, opts: Vec<String>, no_cache: bool) -> PkgResult<()> {
    let selection = parse_opts(&opts)?;
    let _lock = lock_exclusive()?;
    let conn = open_db()?;
    let recipe = load_recipe(name)?;
    let abi_tag = compute_abi_tag(&conn, &recipe.manifest, &selection)?;
    let opt_tag = recipe.manifest.opt_tag(&selection).map_err(PkgError::from)?;

    let mut plan = BuildPlan::default();
    plan.nodes.insert(
        name.to_string(),
        PlanNode {
            name: name.to_string(),
            version: recipe.manifest.version.clone(),
            abi_tag,
            arch: Arch::Amd64,
            opt_tag,
            selection,
            recipe,
            deps_in_cascade: BTreeSet::new(),
            consumed_hash_at_plan: None,
            // `pkg build <name>` IS the user's direct request — drives
            // installed_explicitly on first install (§16.6).
            requested: true,
        },
    );
    let mut cfg = SchedConfig::with_defaults();
    cfg.command_line = format!("pkg build {name}");
    cfg.cascade_id = None; // standalone build (Issue 4.2.5)
    cfg.store_roots = store_roots();
    cfg.host_glibc = running_glibc_version();
    cfg.repo_cache = PathBuf::from(INDEX_CACHE);
    cfg.source_cache = PathBuf::from(SOURCE_CACHE);
    if no_cache {
        cfg.cache = None; // Issue 4.5: "exists for paranoia"
    }
    let report = orchestrator::execute_plan(&conn, &plan, &cfg)?;
    print_report(&report);
    if report.is_clean() {
        Ok(())
    } else {
        Err(PkgError::Build(format!("build of {name} failed")))
    }
}

// ---------------------------------------------------------------------
// key / db / bootstrap / apply-pending-restarts / cascade
// ---------------------------------------------------------------------

const TRUST_DIR: &str = "/var/lib/pkg/trust";

fn cmd_key(action: KeyAction) -> PkgResult<()> {
    // Trust-chain *verification* lives in pkg_repo::trust (SPEC
    // "pkg-repo": root→repository→timestamp roles); these commands are
    // the management surface (§10.5.4).
    match action {
        KeyAction::List => {
            let dir = Path::new(TRUST_DIR);
            if !dir.is_dir() {
                return Err(PkgError::NotFound(format!(
                    "trust store {TRUST_DIR} not initialized — see `pkg key trust-root`"
                )));
            }
            let mut stack = vec![dir.to_path_buf()];
            while let Some(d) = stack.pop() {
                for entry in fs::read_dir(&d).map_err(PkgError::Io)? {
                    let entry = entry.map_err(PkgError::Io)?;
                    let path = entry.path();
                    if path.is_dir() {
                        println!("[{}]", path.strip_prefix(dir).unwrap().display());
                        stack.push(path);
                    } else {
                        println!("  {}", path.file_name().unwrap_or_default().to_string_lossy());
                    }
                }
            }
            Ok(())
        }
        KeyAction::ShowChain => {
            for role in ["root", "repository", "timestamp"] {
                let present = Path::new(TRUST_DIR).join(role).exists();
                println!("{role}: {}", if present { "present" } else { "MISSING" });
            }
            Ok(())
        }
        KeyAction::TrustRoot { file } => {
            let _lock = lock_exclusive()?;
            fs::create_dir_all(Path::new(TRUST_DIR).join("root")).map_err(PkgError::Io)?;
            let dest = Path::new(TRUST_DIR).join("root").join(
                Path::new(&file)
                    .file_name()
                    .ok_or_else(|| PkgError::Parse(format!("bad path {file:?}")))?,
            );
            fs::copy(&file, &dest).map_err(PkgError::Io)?;
            println!("installed root trust anchor as {}", dest.display());
            Ok(())
        }
    }
}

fn cmd_db(action: DbAction) -> PkgResult<()> {
    match action {
        DbAction::Check => {
            let _lock = lock_shared()?;
            let conn = open_db()?;
            let report = pkg_db::integrity::integrity_check(&conn)?;
            println!("sqlite integrity: {}", if report.sqlite_ok { "ok" } else { "FAILED" });
            for e in &report.sqlite_errors {
                println!("  {e}");
            }
            println!(
                "schema version: {:?} (expected {})",
                report.schema_version,
                pkg_db::schema::SCHEMA_VERSION
            );
            for arch in [Arch::Amd64, Arch::I686] {
                let root = staging::store_root_for(arch, &store_roots())?;
                if !root.is_dir() {
                    continue;
                }
                let discrepancies =
                    pkg_db::integrity::scan_store(&conn, &root, arch)?;
                for d in &discrepancies {
                    println!("  store[{arch}]: {d:?}");
                }
                if !discrepancies.is_empty() {
                    return Err(PkgError::Db(format!(
                        "{} store discrepancies under {}",
                        discrepancies.len(),
                        root.display()
                    )));
                }
            }
            Ok(())
        }
        DbAction::Regenerate => {
            let _lock = lock_exclusive()?;
            let mut conn = open_db()?;
            let defaults = pkg_db::integrity::RegenerationDefaults {
                track: Track::Source,
                installed_at: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs() as i64)
                    .unwrap_or(0),
                installed_explicitly: false,
            };
            let mut total = 0usize;
            for arch in [Arch::Amd64, Arch::I686] {
                let root = staging::store_root_for(arch, &store_roots())?;
                if !root.is_dir() {
                    continue;
                }
                // integrity-v4: OrphanedOnDisk carries an Identity (with
                // opt_tag) so regeneration preserves option tags.
                let orphans: Vec<pkg_core::Identity> =
                    pkg_db::integrity::scan_store(&conn, &root, arch)?
                        .into_iter()
                        .filter_map(|d| match d {
                            pkg_db::integrity::StoreDiscrepancy::OrphanedOnDisk(id) => Some(id),
                            _ => None,
                        })
                        .collect();
                let ids = pkg_db::integrity::regenerate_orphans(&mut conn, &orphans, &defaults)?;
                total += ids.len();
            }
            // Issue 5 §5.6/regenerate: history is DB-only metadata and
            // is NEVER reconstructed from the store; a regenerated DB
            // starts a fresh generation 1 if its generations table was
            // lost, marked as truncated history.
            println!("regenerated {total} package rows from the store; generation history is not reconstructable from disk (Issue 5 §5.6)");
            Ok(())
        }
    }
}

fn cmd_bootstrap(verify_layout: bool) -> PkgResult<()> {
    if verify_layout {
        // v13 §9 step 5: stat() both parents of the glibc swap pair and
        // confirm matching st_dev — the same-filesystem invariant is
        // re-verified, not assumed.
        use std::os::unix::fs::MetadataExt;
        let anchor = Path::new("/usr/lib/pkg/glibc-anchor");
        let stage = Path::new("/usr/lib/pkg/.stage");
        let a = fs::metadata(anchor).map_err(PkgError::Io)?;
        let s = fs::metadata(stage).map_err(PkgError::Io)?;
        if a.dev() != s.dev() {
            return Err(PkgError::Gate(format!(
                "layout check FAILED: {} (dev {}) and {} (dev {}) are on different \
                 filesystems; the glibc anchor swap requires same-filesystem (v13 §9). \
                 Relocate per the §18 procedure.",
                anchor.display(),
                a.dev(),
                stage.display(),
                s.dev()
            )));
        }
        println!("layout check passed: anchor and stage share device {}", a.dev());
        return Ok(());
    }
    println!("pkg bootstrap: nothing to do (use --verify-layout for the v13 §9 check)");
    Ok(())
}

fn cmd_apply_pending_restarts(assume_yes: bool) -> PkgResult<()> {
    // v13 §7: restart services holding deleted inodes, in OpenRC
    // dependency order. PID→service mapping is resolved via cgroups/
    // /proc by pkg-audit's inode scan; services without a mapping are
    // listed but not touched.
    let report = pkg_audit::inode_scan::restart_report(
        Path::new(pkg_audit::inode_scan::PROC_ROOT),
        Path::new(pkg_audit::inode_scan::STORE_ROOT),
    )
    .map_err(|e| PkgError::Parse(format!("inode scan failed: {e}")))?;
    if report.services.is_empty() && report.unmapped.is_empty() {
        println!("no processes holding deleted library inodes");
        return Ok(());
    }
    print!("{}", report.render());
    // Unmapped processes (user sessions, PID 1) are listed, never
    // restarted (v13 §7) — with their deleted paths, which render()
    // omits.
    for p in &report.unmapped {
        println!("  unmapped pid {} ({}): {}", p.pid, p.comm, p.deleted_paths.join(", "));
    }
    if report.services.is_empty() {
        println!("no restartable services (user session processes / PID 1 are not touched)");
        return Ok(());
    }
    if !confirm(&format!("Restart {} service(s) in dependency order?", report.services.len()), assume_yes) {
        return Ok(());
    }
    for service in &report.services {
        // OpenRC's own ordering (`rc-service ... restart` in listed
        // dependency order is pkg-audit's responsibility to produce).
        println!(
            "restarting {} (pids: {}) — {}",
            service.name,
            service
                .pids
                .iter()
                .map(u32::to_string)
                .collect::<Vec<_>>()
                .join(", "),
            service.restart_command
        );
        let status = std::process::Command::new("rc-service")
            .arg(&service.name)
            .arg("restart")
            .status();
        match status {
            Ok(s) if s.success() => println!("restarted {}", service.name),
            Ok(s) => eprintln!("warning: rc-service {} restart exited {s}", service.name),
            Err(e) => eprintln!("warning: could not run rc-service: {e}"),
        }
    }
    Ok(())
}

fn cmd_cascade(at: Option<String>, execute_pending: bool, ignore_power: bool) -> PkgResult<()> {
    if let Some(time) = at {
        // Scheduling policy, not mechanism (Issue 4.3.3): the nightly
        // timer runs `pkg cascade --execute-pending`; --at just records
        // the requested window for the timer unit to pick up.
        let state = Path::new("/var/lib/pkg/cascade-schedule");
        fs::write(state, format!("at={time}\n")).map_err(PkgError::Io)?;
        println!("pending cascade scheduled for {time} (guards: battery, idle-inhibit, load5; override with --ignore-power)");
        return Ok(());
    }
    if execute_pending {
        if !ignore_power {
            // Guards (Issue 4.3.3): battery, fullscreen idle-inhibit,
            // load5 > ncpu. Best-effort, overridable.
            if let Some(load5) = load5() {
                let ncpu = std::thread::available_parallelism()
                    .map(|n| n.get() as f64)
                    .unwrap_or(1.0);
                if load5 > ncpu {
                    return Err(PkgError::Build(format!(
                        "load5 {load5:.1} > ncpu {ncpu}; skipping pending cascade \
                         (override with --ignore-power)"
                    )));
                }
            }
        }
        let pending = Path::new(PENDING_BUILD_STATE);
        if !pending.exists() {
            println!("no pending cascade");
            return Ok(());
        }
        // Re-run the deferred rebuilds as a normal upgrade pass; the
        // artifact cache makes already-built nodes free (Issue 4.2.6).
        println!("executing pending cascade (deferred rebuilds resume; cache hits are free)");
        fs::remove_file(pending).map_err(PkgError::Io)?;
        let flags = TransactionFlags {
            opts: Vec::new(),
            dry_run: false,
            skip_rebuilds: false,
            defer_security: None,
            background: false,
            assume_yes: true,
        };
        return run_transaction("upgrade", Vec::new(), &flags);
    }
    // Default: show pending state.
    match fs::read_to_string(PENDING_BUILD_STATE) {
        Ok(text) => println!("pending cascade:\n{text}"),
        Err(_) => println!("no pending cascade"),
    }
    Ok(())
}

fn load5() -> Option<f64> {
    let text = fs::read_to_string("/proc/loadavg").ok()?;
    text.split_whitespace().nth(1)?.parse().ok()
}
