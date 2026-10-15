//! pkg-build/src/recipe.rs — recipe phase runner (SPEC "pkg-build";
//! v11 §7, §15 pipeline; Issue 1 §2.4 for option env; Issue 3.4 for
//! sandbox discipline; Issue 4.2.4 for build-history recording).
//!
//! ## v1 changes
//!
//! - Initial implementation: fetch → prepare → build → check → install
//!   into `$PKGDIR`, with prepare/build/check/install executed inside
//!   pkg-sandbox's `Build` profile and sources fetched via
//!   `pkg_repo::fetch_sources`. Exports `PKG_OPT_<NAME>=0|1` for every
//!   declared option (pkg-core manifest v3 `options_export_env`), pins
//!   `SOURCE_DATE_EPOCH`, wraps the compiler when the recipe permits a
//!   compiler cache (`[build] compiler_cache`, default true), and
//!   records one `build_history` row per run via
//!   `pkg_db::queries::record_build` — successes and failures alike
//!   (§4.2.4: history feeds ETAs and anomaly notices).
//!
//! ## v2
//!
//! - [FIX] `BuildContext` now carries the cached recipe manifest
//!   (`Arc<pkg_repo::RecipeManifest>`, shared by the orchestrator
//!   across workers/attempts) and the resolved gate deps
//!   (`Vec<pkg_scanner::DeclaredDep>`). Fetch looks the package up in
//!   the manifest and calls `pkg_repo::fetch_sources(entry, ..)` — the
//!   T7 guard needs the manifest entry, not the recipe dir; a missing
//!   entry points at `pkg sync`. The gate call passes `&[DeclaredDep]`
//!   (store dirs to scan for provided sonames), not bare names.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use pkg_core::manifest::PackageManifest;
use pkg_core::{AbiTag, PkgError, PkgResult, Version};
use pkg_db::queries::{self, BuildRecord};
use pkg_sandbox::{run_sandboxed_with_binds, SandboxProfile};
use rusqlite::Connection;

/// Recipe phase names, in execution order after `Fetch`.
pub const PHASES: [Phase; 4] = [Phase::Prepare, Phase::Build, Phase::Check, Phase::Install];

/// One recipe phase. `Fetch` is listed for completeness but runs
/// through `pkg_repo::fetch_sources` (against the package's
/// recipe-manifest entry), not a phase script (Issue 3.4: fetch is a
/// separate earlier stage whose only write target is the source
/// cache).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Phase {
    Fetch,
    Prepare,
    Build,
    Check,
    Install,
}

impl Phase {
    /// The conventional script name inside the recipe directory.
    pub fn script_name(&self) -> &'static str {
        match self {
            Phase::Fetch => "fetch",
            Phase::Prepare => "prepare",
            Phase::Build => "build",
            Phase::Check => "check",
            Phase::Install => "install",
        }
    }
}

impl std::fmt::Display for Phase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.script_name())
    }
}

/// A recipe: the parsed `pkg-meta.toml`, the recipe directory holding
/// the phase scripts, and the pinned recipe-repo commit (v12 §2) that
/// feeds provenance and the artifact-cache key.
#[derive(Debug, Clone)]
pub struct Recipe {
    pub manifest: PackageManifest,
    pub dir: PathBuf,
    pub recipe_commit: String,
}

impl Recipe {
    pub fn load(dir: &Path, recipe_commit: impl Into<String>) -> PkgResult<Self> {
        let meta = dir.join("pkg-meta.toml");
        let text = fs::read_to_string(&meta).map_err(|e| {
            PkgError::Parse(format!("reading {}: {e}", meta.display()))
        })?;
        let manifest = PackageManifest::from_toml_str(&text)?;
        Ok(Recipe {
            manifest,
            dir: dir.to_path_buf(),
            recipe_commit: recipe_commit.into(),
        })
    }

    /// The script for `phase`, if the recipe provides one. A missing
    /// `prepare`/`check` script is a no-op phase; `build` and `install`
    /// are mandatory.
    pub fn phase_script(&self, phase: Phase) -> Option<PathBuf> {
        let path = self.dir.join(phase.script_name());
        path.is_file().then_some(path)
    }
}

/// Everything one build needs that the recipe doesn't carry itself.
#[derive(Debug, Clone)]
pub struct BuildContext {
    /// Per-build scratch dir (RW inside the sandbox; the working dir).
    pub scratch_dir: PathBuf,
    /// `$PKGDIR` — the packaging destination the install phase fills.
    /// RW inside the sandbox; becomes the store-dir contents.
    pub pkgdir: PathBuf,
    /// The full option selection (Issue 1 §3): exported as `PKG_OPT_*`
    /// and hashed into the package's `opt_tag`.
    pub selection: BTreeMap<String, bool>,
    /// `make -j` budget granted by the scheduler's token pool.
    pub inner_jobs: u32,
    /// Source cache directory the fetch stage populates.
    pub source_cache: PathBuf,
    /// Compiler-cache wrapper (`ccache`/`sccache`) when enabled by
    /// config AND permitted by the recipe's `[build] compiler_cache`.
    pub compiler_wrapper: Option<String>,
    /// Running glibc version, when known — enables the post-build
    /// scanner gate (v12 §9 verification framing).
    pub host_glibc: Option<Version>,
    /// The recipe manifest cached by `pkg sync` — the fetch stage's T7
    /// guard (`fetch_sources` takes the manifest *entry*). `Arc`
    /// because the orchestrator shares one loaded manifest across every
    /// worker thread and build attempt.
    pub recipe_manifest: std::sync::Arc<pkg_repo::RecipeManifest>,
    /// The node's runtime deps resolved to their installed store dirs
    /// (populated by the orchestrator, which owns the DB connection).
    /// The gate scans these for the sonames each dep provides. May be
    /// a partial view — deps built in the same cascade aren't
    /// installed yet; the gate's undeclared-soname check just can't
    /// attribute those.
    pub gate_deps: Vec<pkg_scanner::DeclaredDep>,
    /// The node's *build-kind* deps, resolved the same way. These are
    /// bound at `/pkg/deps/<name>` like the runtime deps — a build-only
    /// header or tool package (linux-headers for glibc) is useless if
    /// the build cannot see it — but they are NOT gate providers: a
    /// build-only dep may be absent at runtime, so linking against one
    /// without a link/runtime declaration must still fail the gate.
    pub build_deps: Vec<pkg_scanner::DeclaredDep>,
}

/// The result of one recipe run.
#[derive(Debug)]
pub struct BuildOutcome {
    /// The filled `$PKGDIR`, ready to become a store dir.
    pub pkgdir: PathBuf,
    pub abi_tag: AbiTag,
    pub opt_tag: pkg_core::OptTag,
    pub duration: Duration,
    /// Per-phase stderr tails, for the orchestrator's failure reports.
    pub phase_logs: Vec<(Phase, String)>,
}

/// Assembles the environment a sandboxed phase runs with. Pure (given
/// the inputs) so the exact `PKG_OPT_*`/flag contract is unit-testable.
pub fn build_env(ctx: &BuildContext, recipe: &Recipe) -> PkgResult<Vec<(String, String)>> {
    let mut env: Vec<(String, String)> = vec![
        ("HOME".into(), ctx.scratch_dir.display().to_string()),
        ("PKGDIR".into(), ctx.pkgdir.display().to_string()),
        ("MAKEFLAGS".into(), format!("-j{}", ctx.inner_jobs.max(1))),
        ("SOURCE_DATE_EPOCH".into(), pinned_source_date_epoch()),
        ("LC_ALL".into(), "C".into()),
    ];
    // PATH: toolchain wrappers first, then every bound dep's bin dirs
    // (a build tool shipped BY a dependency package — libtasn1's
    // asn1Parser, glib's gdbus-codegen — must be findable by
    // configure/meson's find_program), then the farm. Dep dirs precede
    // /usr/bin so a dep-provided tool wins over the farm's host build
    // of the same name: the packaged one is the one the recipes were
    // written and tested against.
    let mut path = vec!["/usr/lib/pkg/toolchain/bin".to_string()];
    path.extend(
        ctx.gate_deps
            .iter()
            .chain(ctx.build_deps.iter())
            .flat_map(|d| {
                [
                    format!("/pkg/deps/{}/usr/bin", d.name),
                    format!("/pkg/deps/{}/usr/sbin", d.name),
                ]
            }),
    );
    path.extend(["/usr/bin".to_string(), "/bin".to_string()]);
    env.push(("PATH".into(), path.join(":")));
    // Build tools consumed from dependency packages (pkgconf, python3,
    // meson's python process, ...) link against their own libraries, and
    // the sandbox's /usr/lib is empty — the packaged loader's compiled-in
    // default path finds nothing. Put every bound dep's usr/lib on
    // LD_LIBRARY_PATH so /pkg/deps/<name>/usr/bin/* actually runs. This
    // changes no link decisions (the sandbox has no competing libs); it
    // only makes dep-provided executables runnable.
    let dep_lib_path: Vec<String> = ctx
        .gate_deps
        .iter()
        .chain(ctx.build_deps.iter())
        .map(|d| format!("/pkg/deps/{}/usr/lib", d.name))
        .collect();
    if !dep_lib_path.is_empty() {
        env.push(("LD_LIBRARY_PATH".into(), dep_lib_path.join(":")));
    }
    // The compile-time halves of the same idea: with deps mounted at
    // /pkg/deps/<name> rather than merged into /usr, a compiler looking
    // for a declared dep's headers/libraries finds nothing on its own.
    // GCC honors C_INCLUDE_PATH / CPLUS_INCLUDE_PATH / LIBRARY_PATH as
    // extra search roots, and pkg-config honors PKG_CONFIG_PATH (the
    // shipped .pc files are ${pcfiledir}-relocatable, see
    // fixup_pkgconfig_prefixes). Exporting these for the bound deps is
    // what lets a plain `cc.find_library` or `#include` work without
    // every recipe hand-assembling -I/-L flags. Declared deps only, and
    // the post-build gate still verifies the linkage was declared.
    let dep_inc_path: Vec<String> = ctx
        .gate_deps
        .iter()
        .chain(ctx.build_deps.iter())
        .map(|d| format!("/pkg/deps/{}/usr/include", d.name))
        .collect();
    if !dep_inc_path.is_empty() {
        let joined = dep_inc_path.join(":");
        env.push(("C_INCLUDE_PATH".into(), joined.clone()));
        env.push(("CPLUS_INCLUDE_PATH".into(), joined));
        env.push(("LIBRARY_PATH".into(), dep_lib_path.join(":")));
    }
    let dep_pc_path: Vec<String> = ctx
        .gate_deps
        .iter()
        .chain(ctx.build_deps.iter())
        .flat_map(|d| {
            [
                format!("/pkg/deps/{}/usr/lib/pkgconfig", d.name),
                format!("/pkg/deps/{}/usr/share/pkgconfig", d.name),
            ]
        })
        .collect();
    if !dep_pc_path.is_empty() {
        env.push(("PKG_CONFIG_PATH".into(), dep_pc_path.join(":")));
    }
    // Issue 1 §2.4: one PKG_OPT_<NAME>=0|1 per declared option,
    // including unmentioned options at their defaults. The selection is
    // transaction-wide, so filter it down to the options this package
    // actually declares (typo protection happened at plan time).
    env.extend(
        recipe
            .manifest
            .options_export_env(&recipe.manifest.filter_selection(&ctx.selection))?,
    );
    match (&ctx.compiler_wrapper, recipe.manifest.compiler_cache) {
        (Some(wrapper), true) => {
            env.push(("CC".into(), format!("{wrapper} cc")));
            env.push(("CXX".into(), format!("{wrapper} c++")));
        }
        // compiler_cache = false is a recipe-author opt-out (Issue
        // 4.3.1), never user-facing: no wrapper is exported.
        _ => {}
    }
    Ok(env)
}

/// Deterministic-build anchor (Issue 3.5a): the epoch recipes see. The
/// recipe commit pins the sources; this pins time. A fixed constant —
/// not "now" — is what makes a double-build self-check meaningful.
fn pinned_source_date_epoch() -> String {
    // 2024-01-01T00:00:00Z; the exact value matters less than that
    // every build of a recipe sees the same one.
    "1704067200".to_string()
}

/// Runs the full recipe pipeline for one package:
///
/// 1. **fetch** — `pkg_repo::fetch_sources` populates the source cache
///    (SHA-256-verified, origins restricted to the package's
///    recipe-manifest entry, v14 T7). Outside the Build sandbox per
///    Issue 3.4: the network belongs to the fetch stage only.
/// 2. **prepare → build → check → install** — each phase script runs
///    inside pkg-sandbox's `Build` profile (unshared net, RW scratch +
///    `$PKGDIR` only). `check` failure fails the build.
/// 3. **record** — one `build_history` row regardless of outcome.
///
/// The option selection is validated through the manifest (unknown
/// option key → hard error, never silently ignored — Issue 1 §5).
pub fn run_recipe(
    conn: &Connection,
    recipe: &Recipe,
    ctx: &BuildContext,
    abi_tag: AbiTag,
) -> PkgResult<BuildOutcome> {
    let started = unix_now();
    let clock = Instant::now();
    let opt_tag = recipe
        .manifest
        .opt_tag(&recipe.manifest.filter_selection(&ctx.selection))?;

    let outcome = run_recipe_phases(recipe, ctx);
    let duration = clock.elapsed();
    let peak_rss = children_peak_rss_kib();
    let phase_logs = outcome.logs;
    let result = outcome.result;

    // §4.2.4: record successes AND failures; history feeds only
    // display/ordering, never scheduling correctness.
    let record = BuildRecord {
        name: recipe.manifest.name.clone(),
        version: recipe.manifest.version.clone(),
        abi_tag,
        option_set_hash: opt_tag.to_string(),
        duration,
        success: result.is_ok(),
        started_at: started,
        // ccache hit-rate extraction needs the wrapper's stats output;
        // not plumbed through the sandbox in v1 — recorded as NULL
        // rather than fabricated (§4.2.4: NULL if unknown).
        cc_hit_rate: None,
        peak_rss,
        host_jobs: ctx.inner_jobs,
    };
    // A history-write failure is reported but must not mask the build
    // result itself.
    if let Err(e) = queries::record_build(conn, &record) {
        eprintln!("pkg: warning: failed to record build history: {e}");
    }

    result.map(|()| BuildOutcome {
        pkgdir: ctx.pkgdir.clone(),
        abi_tag,
        opt_tag,
        duration,
        phase_logs,
    })
}

/// The phases-only result: the overall result plus every executed
/// phase's stderr tail (kept even on failure — the orchestrator's
/// Issue 4.2.6 failure report needs the failing log).
#[derive(Debug)]
pub struct PhaseOutcome {
    pub result: PkgResult<()>,
    pub logs: Vec<(Phase, String)>,
}

/// Runs fetch → prepare → build → check → install without touching the
/// DB. Split from [`run_recipe`] so the orchestrator can run phases on
/// worker threads (no `&Connection` — `rusqlite::Connection` is not
/// `Sync`) and record `build_history` on the scheduler thread.
pub fn run_recipe_phases(recipe: &Recipe, ctx: &BuildContext) -> PhaseOutcome {
    let mut logs = Vec::new();
    let result = run_recipe_phases_impl(recipe, ctx, &mut logs);
    PhaseOutcome { result, logs }
}

fn run_recipe_phases_impl(
    recipe: &Recipe,
    ctx: &BuildContext,
    phase_logs: &mut Vec<(Phase, String)>,
) -> PkgResult<()> {
    // -- fetch --------------------------------------------------------
    // pkg-repo contract: fetch_sources(entry, cache_dir) downloads the
    // [source]s declared in the package's *recipe manifest entry*,
    // verifies SHA-256, and refuses origins the manifest doesn't
    // declare (T7). The manifest comes from the `pkg sync` cache, not
    // from the recipe checkout — the checkout is transport (v12 §2).
    let manifest_entry = ctx
        .recipe_manifest
        .find(&recipe.manifest.name, &recipe.manifest.version.to_string())
        .ok_or_else(|| {
            PkgError::NotFound(format!(
                "{} {} is not in the cached recipe manifest; run `pkg sync`",
                recipe.manifest.name, recipe.manifest.version
            ))
        })?;
    let sources: Vec<PathBuf> =
        pkg_repo::fetch_sources(manifest_entry, &ctx.source_cache)?;

    // A build always starts from a pristine tree (design 4.3.2: no
    // incremental rebuilds — autotools caches and generated headers
    // bake in the previous build's answers; an option flip would link
    // stale objects). The artifact cache, not the scratch tree, is the
    // sanctioned reuse mechanism. (4.3.2's same-tag retry carve-out
    // would need the phase-marker machinery, which doesn't exist yet.)
    for dir in [&ctx.scratch_dir, &ctx.pkgdir] {
        if dir.exists() {
            fs::remove_dir_all(dir).map_err(PkgError::Io)?;
        }
        fs::create_dir_all(dir).map_err(PkgError::Io)?;
    }

    // Materialize the fetched sources into the scratch dir: the Build
    // profile's only RW bind is scratch, so a tarball left in the
    // source cache is unreachable from inside the sandbox. Cache file
    // names carry a `<sha256>-` prefix (pkg-repo's `cache_file_name`);
    // recipe scripts reference their sources by the URL's bare file
    // name, so the prefix is stripped here.
    for src in &sources {
        let name = src
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| {
                PkgError::Build(format!("source path {} has no file name", src.display()))
            })?;
        let bare = match name.split_once('-') {
            Some((hash, rest))
                if hash.len() == 64 && hash.chars().all(|c| c.is_ascii_hexdigit()) =>
            {
                rest
            }
            _ => name,
        };
        fs::copy(src, ctx.scratch_dir.join(bare)).map_err(PkgError::Io)?;
    }

    let env = build_env(ctx, recipe)?;
    let profile = SandboxProfile::Build;
    // The resolved dependency tree, RO-bound into the build sandbox
    // twice (v12 §9): at its real store paths (the gate scans those,
    // and any baked-in rpaths resolve) and at a stable per-name alias
    // `/pkg/deps/<name>`, so recipes reference e.g.
    // `-I /pkg/deps/libfoo/usr/include` without globbing the
    // identity-hashed store path (which changes on every rebuild).
    // Build-kind deps join the binds (visibility) but not the gate
    // provider set — see `BuildContext::build_deps`.
    let dep_binds: Vec<(PathBuf, PathBuf)> = ctx
        .gate_deps
        .iter()
        .chain(ctx.build_deps.iter())
        .flat_map(|d| {
            [
                (d.store_dir.clone(), d.store_dir.clone()),
                (
                    d.store_dir.clone(),
                    PathBuf::from(format!("/pkg/deps/{}", d.name)),
                ),
            ]
        })
        .collect();

    // -- prepare / build / check / install ----------------------------
    for phase in PHASES {
        let Some(script) = recipe.phase_script(phase) else {
            // A binary-blob repackaging ([build] binary = true) needs
            // no compile step; extraction/placement happens in install.
            let required = matches!(phase, Phase::Install)
                || (matches!(phase, Phase::Build) && !recipe.manifest.binary);
            if required {
                return Err(PkgError::Build(format!(
                    "recipe {} has no `{}` script",
                    recipe.dir.display(),
                    phase.script_name()
                )));
            }
            continue;
        };
        // The recipe dir is NOT bound into the sandbox — copy the phase
        // script into the scratch dir (the build's only always-RW bind)
        // and run it from there.
        let local_script = ctx
            .scratch_dir
            .join(format!(".pkg-phase-{}", phase.script_name()));
        fs::copy(script, &local_script).map_err(PkgError::Io)?;
        let cmd = vec![
            "/bin/sh".to_string(),
            "-e".to_string(),
            local_script.display().to_string(),
        ];
        let outcome =
            run_sandboxed_with_binds(&profile, &dep_binds, &cmd, &ctx.scratch_dir, &env)?;
        phase_logs.push((phase, outcome.stderr_tail.clone()));
        if !outcome.success() {
            return Err(PkgError::Build(format!(
                "{} {}: `{}` phase failed ({})",
                recipe.manifest.name,
                recipe.manifest.version,
                phase,
                outcome.status
            )));
        }
    }

    // -- pkg-config prefix fixup ----------------------------------------
    // Shipped .pc files commonly hardcode `prefix=/usr`, which is wrong
    // for a consumer building against this package inside the sandbox,
    // where the package is mounted at /pkg/deps/<name> — pkg-config
    // would emit -I/usr/include pointing at the sandbox's empty /usr.
    // Rewriting to ${pcfiledir}-relative form keeps the file correct in
    // BOTH locations (see fixup_pkgconfig_prefixes).
    fixup_pkgconfig_prefixes(&ctx.pkgdir)?;

    // -- libtool archive removal ----------------------------------------
    // Shipped .la files record absolute paths of OTHER packages' .la
    // files (mpc's build failed: libmpfr.la pointed at a /usr/lib/
    // libgmp.la that doesn't exist in a consumer's sandbox, where deps
    // live at /pkg/deps/<name>). Without .la files libtool falls back
    // to plain -l linking, which is what the dep search-path env already
    // arranges. Distros strip them tree-wide for the same reason.
    drop_libtool_archives(&ctx.pkgdir)?;

    // -- gate (v12 §9 verification framing) ---------------------------
    // Post-build verification of the produced tree against the running
    // glibc, when the caller supplied it. The declared-dep set (runtime
    // deps resolved to their installed store dirs by the orchestrator)
    // is what the gate scans for provided sonames; it may be a partial
    // view when deps were built in the same cascade — the undeclared-
    // soname check then just can't attribute those (see BuildContext).
    // Vendor binary blobs run under the relaxed linkage policy (see
    // the manifest's `[build] binary`); their undeclared sonames come
    // back as warnings, which the build log must surface.
    if let Some(host) = &ctx.host_glibc {
        let policy = if recipe.manifest.binary {
            pkg_scanner::LinkagePolicy::BinaryRedistributable
        } else {
            pkg_scanner::LinkagePolicy::Strict
        };
        let report = pkg_scanner::verify_build_with_policy(
            &ctx.pkgdir,
            host,
            &ctx.gate_deps,
            policy,
        )?;
        for warning in &report.warnings {
            eprintln!("pkg: gate warning: {warning}");
        }
    }
    Ok(())
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Makes shipped pkg-config files relocatable by rewriting a hardcoded
/// `prefix=/usr` line to `${pcfiledir}`-relative form.
///
/// pkg-config expands `${pcfiledir}` to the directory containing the
/// .pc file at *read* time, so `prefix=${pcfiledir}/../..` resolves to
/// the right place in both worlds: in the live tree
/// (/usr/lib/pkgconfig → /usr) and in the build sandbox
/// (/pkg/deps/<name>/usr/lib/pkgconfig → /pkg/deps/<name>/usr). The
/// number of `..` segments is computed from the .pc file's own depth
/// below `usr/`, so deeper layouts (usr/lib/<triplet>/pkgconfig) stay
/// correct.
///
/// Many upstream .pc files (xz, util-linux, libffi) additionally
/// hardcode path variables instead of deriving them from ${prefix}:
/// `libdir=/usr/lib`, `includedir=/usr/include`, ... — the rewritten
/// prefix alone doesn't fix those. So when the prefix was relocatable,
/// any other `name=/usr[...]` assignment gets its leading /usr
/// replaced by ${prefix}. Non-path lines are unaffected (values like
/// URLs don't start with /usr; `sysconfdir=/etc` doesn't either).
/// Files with an unusual (non-/usr) prefix are left entirely alone —
/// that is the recipe's business.
fn fixup_pkgconfig_prefixes(pkgdir: &Path) -> PkgResult<()> {
    let usr = pkgdir.join("usr");
    if !usr.is_dir() {
        return Ok(());
    }
    let mut pc_files = Vec::new();
    collect_pc_files(&usr, &mut pc_files)?;
    for pc in pc_files {
        // Depth below usr/: usr/lib/pkgconfig/x.pc → ["lib",
        // "pkgconfig"] → prefix is two levels up from ${pcfiledir}.
        let Ok(rel) = pc.strip_prefix(&usr) else {
            continue; // walked under usr — unreachable
        };
        let depth = rel.components().count().saturating_sub(1);
        if depth == 0 {
            continue; // directly under usr/ — not a pkgconfig layout
        }
        let ups = vec![".."; depth].join("/");
        let content = fs::read_to_string(&pc).map_err(PkgError::Io)?;
        let relocatable = content
            .lines()
            .any(|l| l == "prefix=/usr" || l.starts_with("prefix=${pcfiledir}/"));
        if !relocatable {
            continue;
        }
        let mut out = String::with_capacity(content.len());
        let mut changed = false;
        for line in content.split_inclusive('\n') {
            let body = line.strip_suffix('\n').unwrap_or(line);
            let nl = if line.ends_with('\n') { "\n" } else { "" };
            if body == "prefix=/usr" {
                out.push_str(&format!("prefix=${{pcfiledir}}/{ups}{nl}"));
                changed = true;
            } else if let Some((var, value)) = body.split_once('=') {
                let is_var = !var.is_empty()
                    && var
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.')
                    && var.chars().next().is_some_and(|c| c.is_ascii_alphabetic());
                if is_var
                    && var != "prefix"
                    && (value == "/usr" || value.starts_with("/usr/"))
                {
                    out.push_str(&format!("{var}=${{prefix}}{}{nl}", &value[4..]));
                    changed = true;
                } else {
                    out.push_str(line);
                }
            } else {
                out.push_str(line);
            }
        }
        if changed {
            fs::write(&pc, out).map_err(PkgError::Io)?;
        }
    }
    Ok(())
}

/// Removes every libtool archive (`*.la`) under the pkgdir. See the
/// call site for why they must not ship.
fn drop_libtool_archives(pkgdir: &Path) -> PkgResult<()> {
    let mut la_files = Vec::new();
    collect_la_files(pkgdir, &mut la_files)?;
    for la in &la_files {
        fs::remove_file(la).map_err(PkgError::Io)?;
    }
    if !la_files.is_empty() {
        eprintln!("pkg: dropped {} libtool archive(s) (.la)", la_files.len());
    }
    Ok(())
}

fn collect_la_files(dir: &Path, out: &mut Vec<PathBuf>) -> PkgResult<()> {
    for entry in fs::read_dir(dir).map_err(PkgError::Io)? {
        let entry = entry.map_err(PkgError::Io)?;
        let path = entry.path();
        if entry.file_type().map_err(PkgError::Io)?.is_dir() {
            collect_la_files(&path, out)?;
        } else if path.extension().and_then(|e| e.to_str()) == Some("la") {
            out.push(path);
        }
    }
    Ok(())
}

fn collect_pc_files(dir: &Path, out: &mut Vec<PathBuf>) -> PkgResult<()> {
    for entry in fs::read_dir(dir).map_err(PkgError::Io)? {
        let entry = entry.map_err(PkgError::Io)?;
        let path = entry.path();
        if entry.file_type().map_err(PkgError::Io)?.is_dir() {
            collect_pc_files(&path, out)?;
        } else if path.extension().and_then(|e| e.to_str()) == Some("pc")
            && path
                .parent()
                .and_then(|p| p.file_name())
                .and_then(|n| n.to_str())
                == Some("pkgconfig")
        {
            out.push(path);
        }
    }
    Ok(())
}

/// Peak RSS of all child processes reaped so far, in KiB
/// (`getrusage(RUSAGE_CHILDREN)`'s `ru_maxrss`, which is in KiB on
/// Linux). `None` when the syscall fails.
///
/// This is a process-wide *running maximum*, not a per-build
/// measurement: under the orchestrator's parallel workers the value read
/// after one build's child exits is the max over every build child
/// reaped by the process up to that point. The recorded `peak_rss` is
/// therefore an upper bound that converges to a package's true peak over
/// repeat builds — the right shape for §7.9.3's display/ordering input,
/// which never feeds correctness.
pub fn children_peak_rss_kib() -> Option<i64> {
    // SAFETY: `usage` is fully initialized by a successful getrusage
    // call; the result is only read when the return code is 0.
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::getrusage(libc::RUSAGE_CHILDREN, &mut usage) };
    if rc == 0 {
        Some(usage.ru_maxrss as i64)
    } else {
        None
    }
}

// ---------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn libtool_archives_are_dropped() {
        let tmp = tempfile::tempdir().unwrap();
        let lib = tmp.path().join("usr/lib");
        fs::create_dir_all(&lib).unwrap();
        fs::write(lib.join("libfoo.la"), "dlname='libfoo.so'\n").unwrap();
        fs::write(lib.join("libfoo.so"), b"").unwrap();

        drop_libtool_archives(tmp.path()).unwrap();

        assert!(!lib.join("libfoo.la").exists());
        assert!(lib.join("libfoo.so").exists());
    }

    #[test]
    fn pkgconfig_fixup_rewrites_usr_prefix_pcfiledir_relative() {
        let tmp = tempfile::tempdir().unwrap();
        let pcdir = tmp.path().join("usr/lib/pkgconfig");
        fs::create_dir_all(&pcdir).unwrap();
        let pc = pcdir.join("libfoo.pc");
        fs::write(
            &pc,
            "prefix=/usr\nexec_prefix=${prefix}\nlibdir=${exec_prefix}/lib\nName: foo\n",
        )
        .unwrap();
        // A .pc outside a pkgconfig dir, and one with a non-/usr
        // prefix, must be left alone.
        let stray = tmp.path().join("usr/share/foo.pc");
        fs::create_dir_all(stray.parent().unwrap()).unwrap();
        fs::write(&stray, "prefix=/usr\n").unwrap();
        let custom = pcdir.join("opt.pc");
        fs::write(&custom, "prefix=/opt/vendor\n").unwrap();

        fixup_pkgconfig_prefixes(tmp.path()).unwrap();

        let fixed = fs::read_to_string(&pc).unwrap();
        assert!(
            fixed.starts_with("prefix=${pcfiledir}/../..\n"),
            "got: {fixed}"
        );
        assert!(fixed.contains("exec_prefix=${prefix}"));
        assert_eq!(fs::read_to_string(&stray).unwrap(), "prefix=/usr\n");
        assert_eq!(fs::read_to_string(&custom).unwrap(), "prefix=/opt/vendor\n");
    }

    #[test]
    fn pkgconfig_fixup_rewrites_hardcoded_usr_variables() {
        let tmp = tempfile::tempdir().unwrap();
        let pcdir = tmp.path().join("usr/lib/pkgconfig");
        fs::create_dir_all(&pcdir).unwrap();
        // xz's liblzma.pc shape: hardcoded exec_prefix/libdir/includedir.
        let pc = pcdir.join("liblzma.pc");
        fs::write(
            &pc,
            "prefix=/usr\nexec_prefix=/usr\nlibdir=/usr/lib\nincludedir=/usr/include\nURL=https://tukaani.org/xz/\n",
        )
        .unwrap();

        fixup_pkgconfig_prefixes(tmp.path()).unwrap();

        let fixed = fs::read_to_string(&pc).unwrap();
        assert!(fixed.contains("prefix=${pcfiledir}/../..\n"), "got: {fixed}");
        assert!(fixed.contains("exec_prefix=${prefix}\n"), "got: {fixed}");
        assert!(fixed.contains("libdir=${prefix}/lib\n"), "got: {fixed}");
        assert!(fixed.contains("includedir=${prefix}/include\n"), "got: {fixed}");
        assert!(
            fixed.contains("URL=https://tukaani.org/xz/\n"),
            "non-path values untouched: {fixed}"
        );
    }

    #[test]
    fn pkgconfig_fixup_handles_deeper_triplet_layout() {
        let tmp = tempfile::tempdir().unwrap();
        let pcdir = tmp.path().join("usr/lib/x86_64-pc-linux-gnu/pkgconfig");
        fs::create_dir_all(&pcdir).unwrap();
        let pc = pcdir.join("libbar.pc");
        fs::write(&pc, "prefix=/usr\n").unwrap();

        fixup_pkgconfig_prefixes(tmp.path()).unwrap();

        assert_eq!(
            fs::read_to_string(&pc).unwrap(),
            "prefix=${pcfiledir}/../../..\n"
        );
    }

    fn manifest_with_options() -> PackageManifest {
        PackageManifest::from_toml_str(
            r##"
name = "ffmpeg"
version = "6.1"
category = "media"

[options.x11]
description = "X11 client support"
default = true
abi_affecting = true

[options.pulseaudio]
description = "PulseAudio output"
default = false
"##,
        )
        .unwrap()
    }

    fn ctx(selection: BTreeMap<String, bool>) -> BuildContext {
        BuildContext {
            scratch_dir: PathBuf::from("/tmp/scratch"),
            pkgdir: PathBuf::from("/tmpdir"),
            selection,
            inner_jobs: 3,
            source_cache: PathBuf::from("/var/cache/pkg/sources"),
            compiler_wrapper: Some("ccache".into()),
            host_glibc: None,
            recipe_manifest: std::sync::Arc::new(pkg_repo::RecipeManifest {
                packages: Vec::new(),
            }),
            gate_deps: Vec::new(),
            build_deps: Vec::new(),
        }
    }

    #[test]
    fn children_peak_rss_is_available_after_reaping_a_child() {
        // The syscall itself must succeed on Linux even with no
        // children (0), and after reaping a real child the running max
        // is populated.
        std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg("true")
            .status()
            .unwrap();
        let peak = children_peak_rss_kib();
        assert!(peak.is_some(), "getrusage(RUSAGE_CHILDREN) must succeed");
    }

    #[test]
    fn env_exports_all_options_at_effective_values() {
        let manifest = manifest_with_options();
        let recipe = Recipe {
            manifest,
            dir: PathBuf::from("/recipe"),
            recipe_commit: "abc".into(),
        };
        // Empty selection → defaults; still exported (recipe scripts
        // test $PKG_OPT_* unconditionally).
        let env = build_env(&ctx(BTreeMap::new()), &recipe).unwrap();
        let get = |k: &str| env.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
        assert_eq!(get("PKG_OPT_X11").as_deref(), Some("1"));
        assert_eq!(get("PKG_OPT_PULSEAUDIO").as_deref(), Some("0"));
        assert_eq!(get("MAKEFLAGS").as_deref(), Some("-j3"));
        assert_eq!(get("CC").as_deref(), Some("ccache cc"));
        assert!(get("SOURCE_DATE_EPOCH").is_some());
    }

    #[test]
    fn env_selection_overrides_default() {
        let manifest = manifest_with_options();
        let recipe = Recipe {
            manifest,
            dir: PathBuf::from("/recipe"),
            recipe_commit: "abc".into(),
        };
        let sel = BTreeMap::from([("pulseaudio".to_string(), true)]);
        let env = build_env(&ctx(sel), &recipe).unwrap();
        let pa = env.iter().find(|(k, _)| k == "PKG_OPT_PULSEAUDIO").unwrap();
        assert_eq!(pa.1, "1");
    }

    #[test]
    fn undeclared_options_are_filtered_out_of_the_env() {
        // The selection is transaction-wide: keys a package does not
        // declare belong to other packages in the plan and must not
        // fail (or leak into) this package's build env. Typo protection
        // lives at the transaction level (pkg-cli run_transaction
        // rejects keys NO selected package declares).
        let manifest = manifest_with_options();
        let recipe = Recipe {
            manifest,
            dir: PathBuf::from("/recipe"),
            recipe_commit: "abc".into(),
        };
        let sel = BTreeMap::from([("wayland".to_string(), true)]);
        let env = build_env(&ctx(sel), &recipe).unwrap();
        assert!(!env.iter().any(|(k, _)| k == "PKG_OPT_WAYLAND"));
        // Declared options still export at their (default) values.
        let get = |k: &str| env.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
        assert_eq!(get("PKG_OPT_X11").as_deref(), Some("1"));
        assert_eq!(get("PKG_OPT_PULSEAUDIO").as_deref(), Some("0"));
    }

    #[test]
    fn recipe_opt_out_disables_compiler_wrapper() {
        let manifest = PackageManifest::from_toml_str(
            r##"
name = "gcc"
version = "13.2"
category = "sys-devel"

[build]
compiler_cache = false
"##,
        )
        .unwrap();
        let recipe = Recipe {
            manifest,
            dir: PathBuf::from("/recipe"),
            recipe_commit: "abc".into(),
        };
        let env = build_env(&ctx(BTreeMap::new()), &recipe).unwrap();
        assert!(env.iter().all(|(k, _)| k != "CC" && k != "CXX"));
    }
}
