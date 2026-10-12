//! pkg-build/src/orchestrator.rs — DAG-level parallel executor
//! (SPEC "pkg-build"; design doc Issue 4.2, 4.5, 4.6).
//!
//! ## v1 changes
//!
//! - Initial implementation of `execute_plan`: topological-level sweep
//!   over the rebuild DAG with a **token pool** (`sched_tokens = ncpu`,
//!   each build holds `inner_jobs` tokens throttled 1..=`max_pkg_jobs`
//!   (default 4); a configure phase holds 1 token — see
//!   [`TokenPool`]). All build processes live in the
//!   `/sys/fs/cgroup/pkg-build/` cgroup v2 subtree (Issue 4.2.2)
//!   applied via pkg-sandbox, best-effort. Per-package activation
//!   transactions share a `cascade_id` (Issue 4.2.5); a failure marks
//!   the failed node's subtree `blocked` and the rest of the cascade
//!   continues (Issue 4.2.6); the final report lists the blocked
//!   subtree with the first 40 lines of each failing log. Sandboxed
//!   builds failing from OOM get one automatic retry with `inner_jobs`
//!   halved before being marked failed. ETAs come from
//!   `pkg_db::queries::build_eta`, scaled by the §4.2.4
//!   `(host_jobs_recorded / host_jobs_now)^0.7` model, and the
//!   cascade-level figure is the DAG's critical path, not the sum.
//!
//! Threading model: worker threads only *build* (cache probe or
//! sandboxed recipe phases — no DB access, since `rusqlite::Connection`
//! is not `Sync`); the scheduler thread owns the connection and performs
//! every activation transaction, history record, and generation record.
//!
//! ## v2
//!
//! - [FIX] `GenerationKind` is defined in `pkg_db::schema`, not
//!   re-exported by `pkg_db::queries` — dropped from the import (it was
//!   unused here).
//! - [FIX] `execute_plan` now loads the cached recipe manifest once
//!   (`pkg_repo::load_cached_recipe_manifest`, from the new
//!   `SchedConfig::repo_cache`) and shares it with every worker's
//!   `BuildContext` via `Arc` — the fetch stage needs the *manifest
//!   entry*, not the recipe dir.
//! - [FIX] `spawn_worker` (scheduler thread, DB access OK) resolves
//!   each node's runtime deps to `pkg_scanner::DeclaredDep`s for the
//!   post-build gate; deps being built in the same cascade are skipped
//!   (partial view — see `gate_deps_for`).
//!
//! ## v3
//!
//! - [MAJOR] `SchedConfig.compiler_wrapper` now comes from `[cache]
//!   compiler` in `/etc/pkg/config.toml` (§7.9.6) instead of a hardcoded
//!   `ccache` — see [`parse_compiler_wrapper`]. The recipe's `[build]
//!   compiler_cache = false` opt-out still wins (recipe.rs).
//! - [MAJOR] Critical-set double-build (§10.5.6): a node whose recipe
//!   manifest marks `[build] critical = true` is built a second time
//!   (sequential, sandboxed, same pinned `SOURCE_DATE_EPOCH`) and the
//!   two output trees are hash-compared — `repro::double_build`, gated
//!   by the new `SchedConfig::repro_check` (default true;
//!   `--no-repro-check` clears it).
//! - [MAJOR] Post-activation recording (§9.2.1, §7.7.2): after the
//!   activation transaction commits, the scheduler thread records the
//!   measured link interface (`insert_link_interface`) and the
//!   provenance row (`insert_provenance`). Best-effort: the DB commit
//!   already happened, so failures warn rather than abort.
//! - [MAJOR] Post-build cascade pruning (Issue 4.1.1): when a rebuilt
//!   node's freshly measured `link_interface_hash` equals
//!   `PlanNode::consumed_hash_at_plan`, pending cascade-driven
//!   dependents are pruned (`NodeState::Pruned`) — see
//!   `prune_unchanged_dependents`.
//! - [MINOR] `build_history.peak_rss` is recorded from
//!   `getrusage(RUSAGE_CHILDREN)` (§7.9.3).

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::mpsc::channel;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use pkg_core::{AbiTag, Identity, OptTag, PkgError, PkgResult, Version};
use pkg_db::queries::{self, BuildRecord};
use pkg_db::{LinkInterfaceRow, ProvenanceRow};
use pkg_sandbox::{apply_cgroup, CgroupLimits};
use pkg_scanner::DeclaredDep;
use rusqlite::Connection;

use crate::cache::ArtifactCache;
use crate::recipe::{children_peak_rss_kib, run_recipe_phases, BuildContext, Phase, Recipe};
use crate::staging;

/// Where the build cgroup subtree lives (Issue 4.2.2). Matches
/// pkg-sandbox's `BUILD_CGROUP_ROOT`.
pub const BUILD_CGROUP_ROOT: &str = "/sys/fs/cgroup/pkg-build";

/// How many lines of a failing log the final report shows (Issue 4.2.6).
pub const FAILURE_LOG_LINES: usize = 40;

// ---------------------------------------------------------------------
// Plan / config / report types
// ---------------------------------------------------------------------

/// One package to (re)build and activate.
#[derive(Debug, Clone)]
pub struct PlanNode {
    pub name: String,
    pub version: Version,
    pub abi_tag: AbiTag,
    /// Which tree's store root this node activates into (§6.1 multilib:
    /// i686 packages live under the parallel `lib32/pkg/` tree).
    pub arch: pkg_core::Arch,
    pub opt_tag: OptTag,
    /// The full option selection for `PKG_OPT_*` export and cache key.
    pub selection: BTreeMap<String, bool>,
    pub recipe: Recipe,
    /// In-cascade dependencies: this node may start the moment every
    /// one of them is *activated* (Issue 4.2.1) — not when its level
    /// completes.
    pub deps_in_cascade: BTreeSet<String>,
    /// `link_interface_hash` the installed build of this package
    /// consumed (Issue 4.1.1). v1 pruning happens at plan time in
    /// `cascade::compute_rebuild_set`; this field is the hook for the
    /// post-build re-measurement narrowing (`cascade::prune_after_build`)
    /// once the scanner v2 measurement is wired into activation.
    pub consumed_hash_at_plan: Option<String>,
    /// Directly requested by the user (drives `installed_explicitly` on
    /// first install; an existing row's flag is always preserved).
    pub requested: bool,
}

/// A unit of build work: a set of nodes with in-cascade edges.
#[derive(Debug, Default)]
pub struct BuildPlan {
    pub nodes: BTreeMap<String, PlanNode>,
}

impl BuildPlan {
    /// Topological levels (delegates to the same relaxation logic the
    /// cascade plan uses, duplicated here over `PlanNode` so the
    /// orchestrator doesn't depend on cascade's plan type). Pure and
    /// unit-tested below.
    pub fn levels(&self) -> PkgResult<Vec<Vec<String>>> {
        level_dag(
            self.nodes
                .iter()
                .map(|(n, node)| (n.as_str(), &node.deps_in_cascade))
                .collect(),
        )
    }
}

/// Kahn-by-relaxation leveling over `name -> in-cascade deps` maps.
/// Shared by [`BuildPlan::levels`] and kept free-standing so it is
/// directly unit-testable.
pub fn level_dag(
    nodes: BTreeMap<&str, &BTreeSet<String>>,
) -> PkgResult<Vec<Vec<String>>> {
    let mut level_of: BTreeMap<&str, usize> = BTreeMap::new();
    loop {
        let mut progressed = false;
        for (name, deps) in &nodes {
            if level_of.contains_key(name) {
                continue;
            }
            let mut level = 0usize;
            let mut ready = true;
            for dep in *deps {
                if !nodes.contains_key(dep.as_str()) {
                    continue; // out-of-plan edge: already installed
                }
                match level_of.get(dep.as_str()) {
                    Some(l) => level = level.max(l + 1),
                    None => {
                        ready = false;
                        break;
                    }
                }
            }
            if ready {
                level_of.insert(name, level);
                progressed = true;
            }
        }
        if level_of.len() == nodes.len() {
            break;
        }
        if !progressed {
            return Err(PkgError::Build(format!(
                "build plan contains a dependency cycle at {}",
                nodes
                    .keys()
                    .find(|n| !level_of.contains_key(**n))
                    .copied()
                    .unwrap_or("<unknown>")
            )));
        }
    }
    let depth = level_of.values().copied().max().map_or(0, |m| m + 1);
    let mut levels = vec![Vec::new(); depth];
    for (name, level) in level_of {
        levels[level].push(name.to_string());
    }
    for level in &mut levels {
        level.sort();
    }
    Ok(levels)
}

/// Scheduler configuration (Issue 4.2.1/4.2.2).
#[derive(Debug, Clone)]
pub struct SchedConfig {
    /// Total scheduler tokens; defaults to `ncpu` at construction.
    pub sched_tokens: u32,
    /// Cap on one package's `make -j` (default 4).
    pub max_pkg_jobs: u32,
    /// Cgroup v2 subtree for all build processes.
    pub cgroup_root: PathBuf,
    /// The personal artifact cache; `None` = `--no-cache` (Issue 4.5:
    /// "pkg build --no-cache exists for paranoia").
    pub cache: Option<ArtifactCache>,
    /// Where `$PKGDIR` staging dirs are created per node.
    pub build_root: PathBuf,
    /// Source cache passed to the fetch stage.
    pub source_cache: PathBuf,
    /// Repository sync cache (the dir `pkg sync` wrote `index.json` and
    /// `recipe-manifest.toml` into). The fetch stage's T7 guard reads
    /// the recipe manifest from here via
    /// `pkg_repo::load_cached_recipe_manifest`.
    pub repo_cache: PathBuf,
    /// Running glibc for the post-build gate, when known.
    pub host_glibc: Option<Version>,
    /// Compiler-cache wrapper from config (Issue 4.3.1): `[cache]
    /// compiler` in `/etc/pkg/config.toml` (§7.9.6) — `off`/absent →
    /// `None`. See [`compiler_wrapper_from_config`].
    pub compiler_wrapper: Option<String>,
    /// Run the §10.5.6 double-build for critical-set packages (default
    /// true; `--no-repro-check` clears it). Opt-out, not opt-in: the
    /// default must be the safe direction.
    pub repro_check: bool,
    /// The invoking command line, recorded on each generation row.
    pub command_line: String,
    /// Shared by every per-package generation of this cascade (Issue
    /// 4.2.5); `None` for a standalone single-package build.
    pub cascade_id: Option<String>,
    /// The store roots activation symlinks point into, per arch tree.
    pub store_roots: Vec<PathBuf>,
}

impl SchedConfig {
    /// Spec defaults: `sched_tokens = ncpu`, `max_pkg_jobs = 4`.
    pub fn with_defaults() -> Self {
        SchedConfig {
            sched_tokens: available_parallelism(),
            max_pkg_jobs: 4,
            cgroup_root: PathBuf::from(BUILD_CGROUP_ROOT),
            cache: Some(ArtifactCache::default_root()),
            build_root: pkg_core::paths::at("/var/lib/pkg/build"),
            source_cache: pkg_core::paths::at("/var/cache/pkg/sources"),
            repo_cache: pkg_repo::sync::default_cache_dir(),
            host_glibc: None,
            // with_defaults cannot fail (its callers build one
            // unconditionally), so an unreadable/invalid config degrades
            // to no wrapper with a loud warning — the safe direction for
            // a cache. Callers that can propagate errors should prefer
            // `compiler_wrapper_from_config` directly (hard error per
            // §4.3's unknown-values philosophy).
            compiler_wrapper: match compiler_wrapper_from_config(&pkg_core::paths::at(CONFIG_PATH)) {
                Ok(wrapper) => wrapper,
                Err(e) => {
                    eprintln!("pkg: warning: ignoring {CONFIG_PATH}: {e}");
                    None
                }
            },
            repro_check: true,
            command_line: String::new(),
            cascade_id: None,
            store_roots: vec![pkg_core::paths::at("/usr/lib/pkg/.store")],
        }
    }
}

fn available_parallelism() -> u32 {
    thread::available_parallelism()
        .map(|n| n.get() as u32)
        .unwrap_or(1)
}

// ---------------------------------------------------------------------
// [cache] compiler config (§7.9.6)
// ---------------------------------------------------------------------

/// The system config file the scheduler reads for the compiler-cache
/// wrapper (§7.9.6). Only `[cache] compiler` is consumed here; other
/// sections belong to other subsystems.
pub const CONFIG_PATH: &str = "/etc/pkg/config.toml";

/// Parses `[cache] compiler` from config TOML text: absent or `"off"` →
/// `None` (no wrapper); `"ccache"`/`"sccache"` → that wrapper. Unknown
/// values — including a non-string value — are hard errors, per §4.3's
/// "unknown config is loud, never silently ignored" philosophy.
pub fn parse_compiler_wrapper(config_text: &str) -> PkgResult<Option<String>> {
    let value: toml::Value =
        toml::from_str(config_text).map_err(|e| PkgError::Parse(format!("config TOML: {e}")))?;
    let Some(compiler) = value.get("cache").and_then(|c| c.get("compiler")) else {
        return Ok(None);
    };
    let Some(s) = compiler.as_str() else {
        return Err(PkgError::Parse(
            "[cache] compiler must be a string (off | ccache | sccache)".into(),
        ));
    };
    match s {
        "off" => Ok(None),
        "ccache" | "sccache" => Ok(Some(s.to_string())),
        other => Err(PkgError::Parse(format!(
            "unknown [cache] compiler value `{other}` (expected off, ccache or sccache)"
        ))),
    }
}

/// Reads `[cache] compiler` from the config file at `path`. A missing
/// file means no setting (no wrapper); an unreadable or invalid file is
/// a hard error.
pub fn compiler_wrapper_from_config(path: &std::path::Path) -> PkgResult<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(text) => parse_compiler_wrapper(&text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(PkgError::Io(e)),
    }
}

/// What happened to one node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NodeOutcome {
    Built,
    /// Issue 4.5: identical work already done on this machine.
    CacheHit,
    Failed,
    /// Transitive dependent of a failed node (Issue 4.2.6).
    Blocked,
    /// Issue 4.1.1: rebuilt byte-identical interface, subtree pruned.
    Pruned,
}

/// A failed build's report entry.
#[derive(Debug, Clone)]
pub struct FailedBuild {
    pub name: String,
    pub version: Version,
    pub error: String,
    /// First [`FAILURE_LOG_LINES`] lines of the failing phase's log.
    pub log_head: Vec<String>,
}

/// The final cascade report (Issue 4.2.6 + §4.6 anomaly notices).
#[derive(Debug, Default)]
pub struct Report {
    pub activated: Vec<String>,
    pub cache_hits: Vec<String>,
    pub failed: Vec<FailedBuild>,
    pub blocked: Vec<String>,
    pub pruned: Vec<String>,
    /// Wall time of the whole plan.
    pub wall: Duration,
    /// §4.6(2) notices ("took 5.2× its median"), display-only.
    pub notices: Vec<String>,
}

impl Report {
    /// Exit-relevant summary: true when nothing failed and nothing was
    /// blocked.
    pub fn is_clean(&self) -> bool {
        self.failed.is_empty() && self.blocked.is_empty()
    }

    /// The blocked subtree with log heads, formatted per Issue 4.2.6.
    pub fn format_failures(&self) -> String {
        let mut out = String::new();
        for f in &self.failed {
            out.push_str(&format!(
                "\nFAILED: {}-{}: {}\n",
                f.name, f.version, f.error
            ));
            out.push_str("  --- first log lines ---\n");
            for line in &f.log_head {
                out.push_str(&format!("  | {line}\n"));
            }
            let blocked: Vec<&str> = self
                .blocked
                .iter()
                .map(String::as_str)
                .collect();
            if !blocked.is_empty() {
                out.push_str(&format!("  blocked subtree: {}\n", blocked.join(", ")));
            }
        }
        out
    }
}

// ---------------------------------------------------------------------
// Token pool (Issue 4.2.1)
// ---------------------------------------------------------------------

/// The scheduler's total-token pool. `sched_tokens = ncpu` by default;
/// a running build holds `inner_jobs` tokens, throttled dynamically
/// between 1 and `max_pkg_jobs`: configure phases are serial and
/// I/O-bound so they hold 1 token; the compile phase requests more.
///
/// Granting `min..=max` instead of all-or-nothing is what keeps `ncpu`
/// builds from deadlocking: a build can always make progress on 1 token
/// and scales up only when the pool has slack.
#[derive(Debug, Clone)]
pub struct TokenPool {
    total: u32,
    free: u32,
}

impl TokenPool {
    pub fn new(total: u32) -> Self {
        TokenPool {
            total: total.max(1),
            free: total.max(1),
        }
    }

    pub fn total(&self) -> u32 {
        self.total
    }

    pub fn free(&self) -> u32 {
        self.free
    }

    /// Grants between `min` and `max` tokens, or nothing when even
    /// `min` aren't free. The grant is as large as the pool allows
    /// (up to `max`) so a compile phase starting on an idle machine
    /// immediately gets its full `inner_jobs`.
    pub fn acquire(&mut self, min: u32, max: u32) -> Option<u32> {
        let min = min.max(1);
        let max = max.max(min);
        if self.free < min {
            return None;
        }
        let granted = self.free.min(max);
        self.free -= granted;
        Some(granted)
    }

    /// Returns tokens to the pool. Over-release is clamped (and a bug,
    /// but must never corrupt the accounting into `free > total`,
    /// which would let the pool grant tokens that don't exist).
    pub fn release(&mut self, n: u32) {
        self.free = (self.free + n).min(self.total);
    }
}

// ---------------------------------------------------------------------
// ETA estimation (Issue 4.2.4) — display-only, never correctness
// ---------------------------------------------------------------------

/// Per-package ETA: median of the last 5 successful builds (from
/// `queries::build_eta`), scaled by `(host_jobs_recorded /
/// host_jobs_now)^0.7`. A first-ever build returns `None` — the
/// caller's fallback is the similar-source-size median (§4.2.4).
pub fn scaled_eta(conn: &Connection, name: &str, host_jobs_now: u32) -> PkgResult<Option<Duration>> {
    let Some(raw) = queries::build_eta(conn, name)? else {
        return Ok(None);
    };
    // Median recorded host_jobs over the same recent rows; scaling
    // against *recorded* parallelism keeps ETAs comparable across
    // machines with different core counts.
    let history = queries::build_history_for(conn, name)?;
    let mut recorded_jobs: Vec<u32> = history
        .iter()
        .filter(|r| r.success)
        .take(5)
        .map(|r| r.host_jobs.max(1))
        .collect();
    let recorded_median = if recorded_jobs.is_empty() {
        host_jobs_now.max(1) as f64
    } else {
        recorded_jobs.sort_unstable();
        recorded_jobs[recorded_jobs.len() / 2] as f64
    };
    let scale = (recorded_median / host_jobs_now.max(1) as f64).powf(0.7);
    Ok(Some(Duration::from_secs_f64(raw.as_secs_f64() * scale)))
}

/// Cascade ETA = the DAG's critical path under per-package ETAs, not
/// the total work (§4.2.4: "≈3.5 h wall (≈11 h compile work across 8
/// cores)"). Returns `(critical_path, total_work)`. Pure: takes the
/// ETA map as input so it's unit-testable without a DB.
pub fn plan_eta(
    plan: &BuildPlan,
    etas: &BTreeMap<String, Duration>,
) -> (Duration, Duration) {
    // Longest-path DP over the DAG in leveling order.
    let mut longest: BTreeMap<&str, Duration> = BTreeMap::new();
    // Iterate to a fixpoint; DAG, so it converges in depth passes.
    for _ in 0..plan.nodes.len().max(1) {
        let mut progressed = false;
        for (name, node) in &plan.nodes {
            if longest.contains_key(name.as_str()) {
                continue;
            }
            let eta = etas.get(name).copied().unwrap_or(Duration::ZERO);
            let mut base = Duration::ZERO;
            let mut ready = true;
            for dep in &node.deps_in_cascade {
                if !plan.nodes.contains_key(dep) {
                    continue;
                }
                match longest.get(dep.as_str()) {
                    Some(d) => base = base.max(*d),
                    None => {
                        ready = false;
                        break;
                    }
                }
            }
            if ready {
                longest.insert(name, base + eta);
                progressed = true;
            }
        }
        if !progressed {
            break;
        }
    }
    let critical = longest.values().copied().max().unwrap_or(Duration::ZERO);
    let total = plan
        .nodes
        .keys()
        .map(|n| etas.get(n).copied().unwrap_or(Duration::ZERO))
        .fold(Duration::ZERO, |a, b| a + b);
    (critical, total)
}

// ---------------------------------------------------------------------
// OOM detection (Issue 4.2.6)
// ---------------------------------------------------------------------

/// Heuristic: did this build die of OOM? Signal-9 kills with no
/// compiler diagnostic, exit 137 (128+9), and the kernel's OOM-kill
/// wording in the log tail are all treated as OOM — the retry with
/// halved `inner_jobs` is cheap and the false-positive cost is one
/// slower rebuild.
pub fn looks_like_oom(status_code: Option<i32>, log_tail: &str) -> bool {
    if status_code == Some(137) {
        return true;
    }
    let lower = log_tail.to_lowercase();
    lower.contains("out of memory")
        || lower.contains("oom-kill")
        || lower.contains("oom_kill")
        || lower.contains("cannot allocate memory")
        || lower.contains("killed process")
}

// ---------------------------------------------------------------------
// Executor
// ---------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NodeState {
    Pending,
    Running,
    Activated,
    Failed,
    Blocked,
    CacheHitActivated,
    /// Rebuilt byte-identical link interface: node itself is done and
    /// its dependents may proceed against the *old* build (their own
    /// re-measurement decides whether they still need to build). Set by
    /// post-build pruning; treated as satisfied for readiness.
    Pruned,
}

/// Message a worker sends back when its build attempt finishes.
struct WorkerDone {
    name: String,
    tokens_held: u32,
    result: Result<WorkerOk, String>,
    /// stderr tail of the (last) failing phase, or of the build phase.
    log_tail: String,
    duration: Duration,
    cache_hit: bool,
    started_at: i64,
}

struct WorkerOk {
    pkgdir: PathBuf,
    /// The §10.5.6 double-build ran and both trees matched — only ever
    /// true for critical-set packages built (not cache-fetched) with
    /// `SchedConfig::repro_check` on. Recorded as
    /// `reproducibility='self-match'` in the provenance row.
    repro_match: bool,
}

/// Executes a build plan (Issue 4.2.1): DAG-level parallel builds under
/// the token pool, per-package activation transactions as each build
/// completes (Issue 4.2.5), blocked-subtree propagation on failure
/// (Issue 4.2.6), one OOM retry with halved jobs.
pub fn execute_plan(
    conn: &Connection,
    plan: &BuildPlan,
    cfg: &SchedConfig,
) -> PkgResult<Report> {
    let wall_clock = Instant::now();
    let mut report = Report::default();
    if plan.nodes.is_empty() {
        return Ok(report);
    }
    // Cycle-check up front; leveling order itself isn't used for
    // scheduling (nodes start the moment their deps are activated).
    plan.levels()?;

    // The recipe manifest is the fetch stage's T7 guard: no source may
    // be fetched except through a manifest entry, so without the cached
    // manifest EVERY node's fetch would fail. Fail the whole plan up
    // front instead (the error names `pkg sync` as the remedy).
    let recipe_manifest = Arc::new(pkg_repo::load_cached_recipe_manifest(&cfg.repo_cache)?);

    let mut pool = TokenPool::new(cfg.sched_tokens);
    let mut states: BTreeMap<String, NodeState> = plan
        .nodes
        .keys()
        .map(|n| (n.clone(), NodeState::Pending))
        .collect();

    // Best-effort cgroup setup for the whole build subtree (Issue
    // 4.2.2). Failures warn and continue: weights keep a desktop
    // usable, but a system without cgroup v2 must still build.
    for warning in setup_build_cgroup(cfg) {
        eprintln!(
            "pkg: cgroup warning: {}: {}",
            warning.file.display(),
            warning.reason
        );
    }

    let (tx, rx) = channel::<WorkerDone>();
    let mut running: BTreeMap<String, u32> = BTreeMap::new(); // name → tokens held
    // Nodes whose post-build re-measurement proved the link interface
    // unchanged (§7.7.2) — the pruning step below consults this.
    let mut iface_unchanged: BTreeSet<String> = BTreeSet::new();

    loop {
        // -- collect finished workers --------------------------------
        while let Ok(done) = rx.try_recv() {
            pool.release(done.tokens_held);
            running.remove(&done.name);
            handle_worker_done(conn, plan, cfg, &mut states, &mut report, done, &mut iface_unchanged)?;
        }

        // -- prune unchanged subtrees (§7.7.2) ------------------------
        // A pending node whose in-cascade deps ALL completed with an
        // unchanged measured interface no longer needs its rebuild:
        // the interface it consumes is byte-identical. Roots (no
        // in-cascade deps) and explicitly requested nodes are never
        // pruned. Unmeasured/failed deps never satisfy this — widening
        // is the safe direction.
        let prunable: Vec<String> = plan
            .nodes
            .iter()
            .filter(|(name, node)| {
                states.get(*name) == Some(&NodeState::Pending)
                    && !node.requested
                    && !node.deps_in_cascade.is_empty()
                    && node.deps_in_cascade.iter().all(|d| {
                        match states.get(d) {
                            // Deps outside the plan weren't rebuilt —
                            // unchanged by definition.
                            None => true,
                            Some(NodeState::Activated)
                            | Some(NodeState::CacheHitActivated)
                            | Some(NodeState::Pruned) => iface_unchanged.contains(d),
                            _ => false,
                        }
                    })
            })
            .map(|(n, _)| n.clone())
            .collect();
        for name in prunable {
            states.insert(name.clone(), NodeState::Pruned);
            report.pruned.push(name);
        }

        // -- launch ready nodes --------------------------------------
        let ready: Vec<String> = plan
            .nodes
            .iter()
            .filter(|(name, node)| {
                states.get(*name) == Some(&NodeState::Pending)
                    && node.deps_in_cascade.iter().all(|d| {
                        !plan.nodes.contains_key(d)
                            || matches!(
                                states.get(d),
                                Some(NodeState::Activated)
                                    | Some(NodeState::CacheHitActivated)
                                    | Some(NodeState::Pruned)
                            )
                    })
            })
            .map(|(n, _)| n.clone())
            .collect();

        for name in ready {
            // Reserve 1 token minimum; grant up to max_pkg_jobs.
            let Some(tokens) = pool.acquire(1, cfg.max_pkg_jobs) else {
                break; // pool exhausted; wait for a worker to finish
            };
            states.insert(name.clone(), NodeState::Running);
            running.insert(name.clone(), tokens);
            spawn_worker(conn, &tx, plan, cfg, name, tokens, &recipe_manifest);
        }

        if running.is_empty() {
            let pending = states
                .values()
                .any(|s| *s == NodeState::Pending);
            if !pending {
                break; // everything terminal
            }
            // Pending nodes but none running and none ready: can only
            // happen if pool.acquire refused despite an idle machine —
            // never, since min=1 and free>=1 when nothing runs — or if
            // all remaining pendings are blocked, which
            // handle_worker_done already converts. Guard anyway.
            if ready_is_impossible(plan, &states) {
                for (name, state) in states.iter_mut() {
                    if *state == NodeState::Pending {
                        *state = NodeState::Blocked;
                        report.blocked.push(name.clone());
                    }
                }
                break;
            }
        } else {
            // Block until at least one worker reports.
            match rx.recv() {
                Ok(done) => {
                    pool.release(done.tokens_held);
                    running.remove(&done.name);
                    handle_worker_done(conn, plan, cfg, &mut states, &mut report, done, &mut iface_unchanged)?;
                }
                Err(_) => break, // all senders dropped; nothing more can finish
            }
        }
    }

    report.wall = wall_clock.elapsed();
    Ok(report)
}

fn ready_is_impossible(plan: &BuildPlan, states: &BTreeMap<String, NodeState>) -> bool {
    !plan.nodes.iter().any(|(name, node)| {
        states.get(name.as_str()) == Some(&NodeState::Pending)
            && node
                .deps_in_cascade
                .iter()
                .all(|d| !plan.nodes.contains_key(d))
    })
}

/// Spawns a build worker. The worker never touches the DB — this runs
/// on the scheduler thread, so the gate's dependency resolution (a DB
/// read) happens here, before the spawn.
fn spawn_worker(
    conn: &Connection,
    tx: &std::sync::mpsc::Sender<WorkerDone>,
    plan: &BuildPlan,
    cfg: &SchedConfig,
    name: String,
    tokens: u32,
    recipe_manifest: &Arc<pkg_repo::RecipeManifest>,
) {
    let node = plan.nodes.get(&name).expect("node exists").clone();
    let gate_deps = gate_deps_for(conn, &node, cfg);
    let cfg = cfg.clone();
    let tx = tx.clone();
    let recipe_manifest = recipe_manifest.clone();
    thread::spawn(move || {
        let done = build_node(&node, &cfg, tokens, recipe_manifest, gate_deps);
        let _ = tx.send(done);
    });
}

/// Resolves a node's declared *runtime* deps to the installed store
/// dirs the post-build gate scans for provided sonames
/// (`pkg_scanner::DeclaredDep`). A dep with no installed row — one
/// being built earlier in this same cascade — is skipped: `gate_build`
/// tolerates a partial view (its undeclared-soname check simply can't
/// attribute sonames provided by not-yet-installed deps). Runs on the
/// scheduler thread (DB access); failures degrade to a partial/empty
/// view with a warning rather than failing the build.
fn gate_deps_for(conn: &Connection, node: &PlanNode, cfg: &SchedConfig) -> Vec<DeclaredDep> {
    let installed = match queries::list_installed_packages(conn) {
        Ok(rows) => rows,
        Err(e) => {
            eprintln!("pkg: warning: cannot list installed packages for the build gate: {e}");
            return Vec::new();
        }
    };
    let mut deps = Vec::new();
    for dep in &node.recipe.manifest.runtime_deps {
        let pkg_core::Dependency::Package { name, .. } = dep else {
            continue; // virtuals/OR-groups have no single concrete store dir
        };
        let Some(row) = installed
            .iter()
            .find(|r| &r.name == name && r.arch == node.arch)
        else {
            continue; // being built in this cascade — see fn docs
        };
        let Ok(opt_tag) = row.opt_tag.parse::<OptTag>() else {
            eprintln!(
                "pkg: warning: bad opt_tag on installed row for {}; skipping it for the gate",
                row.name
            );
            continue;
        };
        let identity = pkg_core::Identity {
            name: row.name.clone(),
            version: row.version.clone(),
            abi_tag: row.abi_tag,
            opt_tag,
            arch: row.arch,
        };
        let Ok(root) = staging::store_root_for(node.arch, &cfg.store_roots) else {
            continue;
        };
        deps.push(DeclaredDep {
            name: name.clone(),
            store_dir: root.join(identity.store_dir_name()),
        });
    }
    deps
}

/// One node's full build attempt, incl. the cache probe and the single
/// OOM retry. Runs on a worker thread.
fn build_node(
    node: &PlanNode,
    cfg: &SchedConfig,
    tokens: u32,
    recipe_manifest: Arc<pkg_repo::RecipeManifest>,
    gate_deps: Vec<DeclaredDep>,
) -> WorkerDone {
    let started_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let clock = Instant::now();

    let scratch = cfg.build_root.join(format!("{}-scratch", node.name));
    let pkgdir = cfg.build_root.join(format!("{}-pkgdir", node.name));

    // -- cache probe (Issue 4.5) --------------------------------------
    if let Some(cache) = &cfg.cache {
        let key = crate::cache::CacheKey::new(
            node.name.clone(),
            node.version.clone(),
            node.abi_tag,
            node.opt_tag.clone(),
            node.recipe.recipe_commit.clone(),
        );
        match cache.fetch_into(&key, &pkgdir) {
            Ok(true) => {
                return WorkerDone {
                    name: node.name.clone(),
                    tokens_held: tokens,
                    // A cache hit is byte-identical to what a source
                    // build would produce (content-addressed, §7.9.5);
                    // the double-build did not run, so no self-match.
                    result: Ok(WorkerOk { pkgdir, repro_match: false }),
                    log_tail: String::new(),
                    duration: clock.elapsed(),
                    cache_hit: true,
                    started_at,
                };
            }
            Ok(false) => {} // miss → silent source build
            Err(e) => {
                eprintln!("pkg: cache lookup failed for {} ({e}); building", node.name);
            }
        }
    }

    let attempt = |jobs: u32| -> (PkgResult<Vec<(Phase, String)>>, String) {
        let ctx = BuildContext {
            scratch_dir: scratch.clone(),
            pkgdir: pkgdir.clone(),
            selection: node.selection.clone(),
            inner_jobs: jobs,
            source_cache: cfg.source_cache.clone(),
            compiler_wrapper: cfg.compiler_wrapper.clone(),
            host_glibc: cfg.host_glibc.clone(),
            recipe_manifest: recipe_manifest.clone(),
            gate_deps: gate_deps.clone(),
        };
        let outcome = run_recipe_phases(&node.recipe, &ctx);
        let tail = outcome
            .logs
            .last()
            .map(|(_, t)| t.clone())
            .unwrap_or_default();
        (outcome.result.map(|_| outcome.logs), tail)
    };

    // First attempt at the granted job count.
    let (mut result, mut tail) = attempt(tokens);
    // One automatic OOM retry with halved jobs (Issue 4.2.6).
    if result.is_err() && tokens > 1 && looks_like_oom(None, &tail) {
        let halved = (tokens / 2).max(1);
        eprintln!(
            "pkg: {}: build appears OOM-killed; retrying once with -j{halved}",
            node.name
        );
        let retry = attempt(halved);
        result = retry.0;
        tail = retry.1;
    }

    // -- critical-set double-build (§10.5.6) --------------------------
    // A successful critical build is built a second time (sequential,
    // sandboxed, same pinned SOURCE_DATE_EPOCH) and the trees compared;
    // a mismatch aborts here, quarantining both trees. Opt-out via
    // `SchedConfig::repro_check` (`--no-repro-check`), never the default.
    let mut repro_match = false;
    if result.is_ok() && cfg.repro_check && node.recipe.manifest.critical {
        let first_ctx = BuildContext {
            scratch_dir: scratch.clone(),
            pkgdir: pkgdir.clone(),
            selection: node.selection.clone(),
            inner_jobs: tokens,
            source_cache: cfg.source_cache.clone(),
            compiler_wrapper: cfg.compiler_wrapper.clone(),
            host_glibc: cfg.host_glibc.clone(),
            recipe_manifest: recipe_manifest.clone(),
            gate_deps: gate_deps.clone(),
        };
        match crate::repro::double_build(
            &node.recipe,
            &first_ctx,
            &cfg.build_root.join(crate::repro::QUARANTINE_DIR_NAME),
        ) {
            Ok(()) => repro_match = true,
            Err(e) => {
                result = Err(e);
                tail = String::new();
            }
        }
    }

    let duration = clock.elapsed();

    // -- populate the cache on success (Issue 4.5) --------------------
    if result.is_ok() {
        if let Some(cache) = &cfg.cache {
            let key = crate::cache::CacheKey::new(
                node.name.clone(),
                node.version.clone(),
                node.abi_tag,
                node.opt_tag.clone(),
                node.recipe.recipe_commit.clone(),
            );
            if let Err(e) = cache.store(&key, &pkgdir) {
                eprintln!("pkg: warning: failed to cache {}: {e}", node.name);
            }
        }
    }

    WorkerDone {
        name: node.name.clone(),
        tokens_held: tokens,
        result: result.map(|_| WorkerOk { pkgdir, repro_match }).map_err(|e| e.to_string()),
        log_tail: tail,
        duration,
        cache_hit: false,
        started_at,
    }
}

/// Scheduler-thread completion handling: history row, per-package
/// activation transaction, failure → blocked-subtree marking.
fn handle_worker_done(
    conn: &Connection,
    plan: &BuildPlan,
    cfg: &SchedConfig,
    states: &mut BTreeMap<String, NodeState>,
    report: &mut Report,
    done: WorkerDone,
    iface_unchanged: &mut BTreeSet<String>,
) -> PkgResult<()> {
    let node = plan.nodes.get(&done.name).expect("node exists");

    // §4.2.4: history for every completed build (cache hits record
    // nothing — no build happened).
    if !done.cache_hit {
        let record = BuildRecord {
            name: node.name.clone(),
            version: node.version.clone(),
            abi_tag: node.abi_tag,
            option_set_hash: node.opt_tag.to_string(),
            duration: done.duration,
            success: done.result.is_ok(),
            started_at: done.started_at,
            cc_hit_rate: None,
            // Process-wide running maximum of reaped build children
            // (see recipe::children_peak_rss_kib) — an upper bound, never
            // a correctness input.
            peak_rss: children_peak_rss_kib(),
            host_jobs: done.tokens_held,
        };
        if let Err(e) = queries::record_build(conn, &record) {
            eprintln!("pkg: warning: failed to record build history: {e}");
        }
        // §4.6(2): 5×-median anomaly notice, detection only.
        if let Ok(Some(eta)) = queries::build_eta(conn, &node.name) {
            if done.duration > eta.mul_f64(5.0) && done.result.is_ok() {
                report.notices.push(format!(
                    "{} took {:.1}× its median — check thermals / ccache health",
                    node.name,
                    done.duration.as_secs_f64() / eta.as_secs_f64().max(1.0)
                ));
            }
        }
    }

    match done.result {
        Ok(ok) => {
            // Per-package activation transaction (Issue 4.2.5): stage →
            // activate → DB commit LAST, generation row in the same
            // commit, cascade_id shared.
            let outcome = staging::activate_built_package(
                conn,
                &ok.pkgdir,
                node,
                &cfg.store_roots,
                &cfg.command_line,
                cfg.cascade_id.as_deref(),
            );
            match outcome {
                Ok(()) => {
                    // Post-activation recording (§9.2.1, §7.7.2) and
                    // cascade narrowing both read the ACTIVATED tree:
                    // staging moved the pkgdir into the store.
                    let store_dir = staging::store_root_for(node.arch, &cfg.store_roots).map(|r| {
                        r.join(
                            Identity {
                                name: node.name.clone(),
                                version: node.version.clone(),
                                abi_tag: node.abi_tag,
                                opt_tag: node.opt_tag.clone(),
                                arch: node.arch,
                            }
                            .store_dir_name(),
                        )
                    });
                    // Best-effort: the activation commit already
                    // happened, so failures warn rather than abort.
                    if let Ok(dir) = &store_dir {
                        record_build_metadata(conn, node, dir, done.started_at, ok.repro_match, cfg);
                    }
                    // Post-build cascade narrowing (§7.7.2): compare the
                    // freshly measured interface hash against what the
                    // plan assumed; a match means dependents don't need
                    // their cascade rebuilds.
                    let measured = store_dir
                        .as_ref()
                        .ok()
                        .and_then(|d| measure_package_interface(d));
                    match measured {
                        Some(new_hash) => {
                            if node.consumed_hash_at_plan.as_deref() == Some(new_hash.as_str()) {
                                iface_unchanged.insert(done.name.clone());
                            }
                        }
                        None => {
                            // No measurable interface (or scan failure):
                            // treat as changed — uncertainty always
                            // widens the cascade (§7.7.3).
                        }
                    }
                    states.insert(
                        done.name.clone(),
                        if done.cache_hit {
                            NodeState::CacheHitActivated
                        } else {
                            NodeState::Activated
                        },
                    );
                    report.activated.push(done.name.clone());
                    if done.cache_hit {
                        report.cache_hits.push(done.name.clone());
                    }
                }
                Err(e) => {
                    mark_failed_and_block_subtree(
                        plan,
                        states,
                        report,
                        &done.name,
                        format!("activation failed: {e}"),
                        first_lines(&done.log_tail, FAILURE_LOG_LINES),
                    );
                }
            }
        }
        Err(err) => {
            mark_failed_and_block_subtree(
                plan,
                states,
                report,
                &done.name,
                err,
                first_lines(&done.log_tail, FAILURE_LOG_LINES),
            );
        }
    }
    Ok(())
}

/// Walks an activated store tree and returns the package's aggregate
/// `link_interface_hash` (§7.7.2; empty header fingerprint until header
/// fingerprinting lands). `None` when the tree exports no sonames at
/// all — the hash of an empty set would be a constant shared by every
/// non-library package, useless as a pruning comparison.
fn measure_package_interface(tree: &Path) -> Option<String> {
    let interfaces = collect_link_interfaces(tree)?;
    if interfaces.is_empty() {
        return None;
    }
    Some(pkg_scanner::link_interface_hash(&interfaces, ""))
}

/// Every link interface exported by ELF members of `tree`, recursively.
/// Unparseable files are skipped (a data file is not a scan failure).
fn collect_link_interfaces(tree: &Path) -> Option<Vec<pkg_scanner::LinkInterface>> {
    let mut interfaces = Vec::new();
    let mut stack = vec![tree.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).ok()?.flatten() {
            let path = entry.path();
            let Ok(ft) = entry.file_type() else {
                continue;
            };
            if ft.is_dir() {
                stack.push(path);
            } else if ft.is_file() {
                if let Ok(mut found) = pkg_scanner::extract_link_interface(&path) {
                    interfaces.append(&mut found);
                }
            }
        }
    }
    Some(interfaces)
}

/// Post-activation recording (§9.2.1, §7.7.2): the measured link
/// interface rows and the provenance row for a just-activated build.
/// Best-effort — the activation commit already happened, so failures
/// warn rather than abort.
fn record_build_metadata(
    conn: &Connection,
    node: &PlanNode,
    store_dir: &Path,
    started_at: i64,
    repro_match: bool,
    cfg: &SchedConfig,
) {
    if let Err(e) =
        record_build_metadata_inner(conn, node, store_dir, started_at, repro_match, cfg)
    {
        eprintln!(
            "pkg: warning: failed to record build metadata for {}: {e}",
            node.name
        );
    }
}

fn record_build_metadata_inner(
    conn: &Connection,
    node: &PlanNode,
    store_dir: &Path,
    started_at: i64,
    repro_match: bool,
    cfg: &SchedConfig,
) -> PkgResult<()> {
    let package_id = pkg_core::PackageId {
        name: node.name.clone(),
        version: node.version.clone(),
        abi_tag: node.abi_tag,
        arch: node.arch,
    };
    let Some(row) = queries::find_package_by_identity_opt(conn, &package_id, node.opt_tag.as_str())?
    else {
        return Err(PkgError::NotFound(format!(
            "activated package {} not found in DB",
            node.name
        )));
    };

    // Every row of a package carries the same aggregate hash — the
    // cascade compares interfaces per package, not per soname (see
    // queries::link_interface_hash). The header fingerprint (§7.7.2's
    // public-header-fingerprint) is measured from the shipped tree and
    // feeds both the per-row column and the aggregate.
    let interfaces = collect_link_interfaces(store_dir).unwrap_or_default();
    let header_fp = pkg_scanner::public_header_fingerprint(store_dir).unwrap_or_default();
    let aggregate = if interfaces.is_empty() {
        String::new()
    } else {
        pkg_scanner::link_interface_hash(&interfaces, &header_fp)
    };
    for li in &interfaces {
        queries::insert_link_interface(
            conn,
            &LinkInterfaceRow {
                package_id: row.id,
                soname: li.soname.clone(),
                symbol_versions: li.symbol_versions.iter().cloned().collect::<Vec<_>>().join("\n"),
                header_fingerprint: header_fp.clone(),
                link_interface_hash: aggregate.clone(),
            },
        )?;
    }

    // Provenance (§9.2.1): from the signed recipe manifest where
    // available; unknown fields are recorded as empty/0, never
    // fabricated.
    let entry = pkg_repo::load_cached_recipe_manifest(&cfg.repo_cache)
        .ok()
        .and_then(|m| m.find(&node.name, &node.version.to_string()).cloned());
    let source_hashes = entry
        .as_ref()
        .map(|e| serde_json::to_string(&e.sources).unwrap_or_else(|_| "[]".to_string()))
        .unwrap_or_else(|| "[]".to_string());
    let attestations = entry.as_ref().map(|e| {
        let fps: Vec<&str> = e.attestations.iter().map(|a| a.key_fingerprint.as_str()).collect();
        serde_json::to_string(&fps).unwrap_or_else(|_| "[]".to_string())
    });
    let hostname = std::fs::read_to_string("/proc/sys/kernel/hostname")
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "unknown".to_string());
    let osrelease = std::fs::read_to_string("/proc/sys/kernel/osrelease")
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "unknown".to_string());
    queries::insert_provenance(
        conn,
        &ProvenanceRow {
            package_id: row.id,
            recipe_commit: node.recipe.recipe_commit.clone(),
            source_hashes,
            index_version: pkg_repo::sync::cached_index_version(&cfg.repo_cache)
                .ok()
                .flatten()
                .map(|v| v as i64)
                .unwrap_or(0),
            attestations,
            build_host: format!("{hostname} (linux {osrelease})"),
            builder_version: format!(
                "pkg {} / build.profile v{}",
                env!("CARGO_PKG_VERSION"),
                pkg_sandbox::BUILTIN_PROFILE_VERSION
            ),
            started_at,
            finished_at: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0),
            reproducibility: if repro_match {
                Some("self-match".to_string())
            } else {
                None
            },
        },
    )?;
    Ok(())
}

fn mark_failed_and_block_subtree(
    plan: &BuildPlan,
    states: &mut BTreeMap<String, NodeState>,
    report: &mut Report,
    name: &str,
    error: String,
    log_head: Vec<String>,
) {
    states.insert(name.to_string(), NodeState::Failed);
    let node = &plan.nodes[name];
    report.failed.push(FailedBuild {
        name: name.to_string(),
        version: node.version.clone(),
        error,
        log_head,
    });
    // Everything depending on the failed node, transitively, is
    // blocked — the rest of the cascade continues (Issue 4.2.6).
    let subtree: Vec<String> = plan
        .nodes
        .keys()
        .filter(|n| {
            n.as_str() != name
                && states.get(n.as_str()) == Some(&NodeState::Pending)
                && depends_on_transitively(plan, n, name)
        })
        .cloned()
        .collect();
    for dep in subtree {
        states.insert(dep.clone(), NodeState::Blocked);
        report.blocked.push(dep);
    }
}

/// Is `name` a transitive in-plan dependent of `root`?
fn depends_on_transitively(plan: &BuildPlan, name: &str, root: &str) -> bool {
    let mut seen = BTreeSet::new();
    let mut queue: VecDeque<&str> = VecDeque::from([name]);
    while let Some(n) = queue.pop_front() {
        if !seen.insert(n.to_string()) {
            continue;
        }
        let Some(node) = plan.nodes.get(n) else { continue };
        for dep in &node.deps_in_cascade {
            if dep == root {
                return true;
            }
            queue.push_back(dep);
        }
    }
    false
}

/// First `n` lines of a log tail (Issue 4.2.6 report).
pub fn first_lines(text: &str, n: usize) -> Vec<String> {
    text.lines().take(n).map(|l| l.to_string()).collect()
}

/// Issue 4.2.2 cgroup setup, best-effort: weights, not quotas, so an
/// idle machine gives the cascade everything while a foreground game
/// never stutters. Returns the warnings for the report log.
pub fn setup_build_cgroup(cfg: &SchedConfig) -> Vec<pkg_sandbox::CgroupWarning> {
    let root = &cfg.cgroup_root;
    if let Err(e) = std::fs::create_dir_all(root) {
        eprintln!(
            "pkg: warning: cannot create build cgroup {} ({e}); \
             continuing without resource weights",
            root.display()
        );
        return Vec::new();
    }
    let limits = CgroupLimits {
        cpu_weight: 50,
        cpu_max_active_session: Some(75),
        io_weight: 50,
        // memory.high = total_ram - 2GiB; unmeasurable RAM → no cap
        // rather than a guessed one.
        memory_high: total_ram_bytes().map(|ram| ram.saturating_sub(2 * 1024 * 1024 * 1024)),
    };
    apply_cgroup(root, &limits)
}

fn total_ram_bytes() -> Option<u64> {
    let text = std::fs::read_to_string("/proc/meminfo").ok()?;
    let kb: u64 = text
        .lines()
        .find(|l| l.starts_with("MemTotal:"))?
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()?;
    Some(kb * 1024)
}

// ---------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn deps(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn token_pool_grants_up_to_max_and_refuses_when_empty() {
        let mut pool = TokenPool::new(8);
        // First build on an idle pool gets its full request.
        assert_eq!(pool.acquire(1, 4), Some(4));
        assert_eq!(pool.free(), 4);
        assert_eq!(pool.acquire(1, 4), Some(4));
        assert_eq!(pool.free(), 0);
        // Pool exhausted: even a 1-token configure must wait.
        assert_eq!(pool.acquire(1, 1), None);
        pool.release(2);
        assert_eq!(pool.acquire(1, 4), Some(2)); // gets what exists
    }

    #[test]
    fn token_pool_never_over_frees() {
        let mut pool = TokenPool::new(4);
        pool.release(100); // bug-tolerant clamp
        assert_eq!(pool.free(), 4);
    }

    #[test]
    fn token_pool_configure_holds_one() {
        let mut pool = TokenPool::new(2);
        assert_eq!(pool.acquire(1, 1), Some(1)); // configure phase
        assert_eq!(pool.acquire(1, 1), Some(1));
        assert_eq!(pool.acquire(1, 1), None);
    }

    #[test]
    fn leveling_orders_dag() {
        let openssl = deps(&[]);
        let curl = deps(&["openssl"]);
        let python = deps(&["curl"]);
        let zlib = deps(&[]);
        let levels = level_dag(BTreeMap::from([
            ("openssl", &openssl),
            ("curl", &curl),
            ("python", &python),
            ("zlib", &zlib),
        ]))
        .unwrap();
        assert_eq!(levels[0], vec!["openssl".to_string(), "zlib".to_string()]);
        assert_eq!(levels[1], vec!["curl".to_string()]);
        assert_eq!(levels[2], vec!["python".to_string()]);
    }

    #[test]
    fn oom_heuristic() {
        assert!(looks_like_oom(Some(137), ""));
        assert!(looks_like_oom(None, "cc1plus: fatal error: Killed signal terminated program cc1plus\nout of memory"));
        assert!(looks_like_oom(None, "kernel: oom-kill: constraint=CONSTRAINT_NONE"));
        assert!(!looks_like_oom(Some(1), "error: expected ';'"));
    }

    #[test]
    fn critical_path_not_total() {
        // Two parallel branches: a(10m)→b(20m) and c(25m). Critical
        // path = 30m; total work = 55m.
        let mut plan = BuildPlan::default();
        let recipe = |n: &str| Recipe {
            manifest: pkg_core::manifest::PackageManifest::from_toml_str(&format!(
                "name = \"{n}\"\nversion = \"1.0\"\ncategory = \"app\"\n"
            ))
            .unwrap(),
            dir: PathBuf::from("/r"),
            recipe_commit: "c".into(),
        };
        for (n, ds) in [("a", vec![]), ("b", vec!["a"]), ("c", vec![])] {
            plan.nodes.insert(
                n.to_string(),
                PlanNode {
                    name: n.to_string(),
                    version: "1.0".parse().unwrap(),
                    abi_tag: AbiTag(1),
                    arch: pkg_core::Arch::Amd64,
                    opt_tag: OptTag::none(),
                    selection: BTreeMap::new(),
                    recipe: recipe(n),
                    deps_in_cascade: ds.into_iter().map(String::from).collect(),
                    consumed_hash_at_plan: None,
                    requested: false,
                },
            );
        }
        let etas = BTreeMap::from([
            ("a".to_string(), Duration::from_secs(600)),
            ("b".to_string(), Duration::from_secs(1200)),
            ("c".to_string(), Duration::from_secs(1500)),
        ]);
        let (critical, total) = plan_eta(&plan, &etas);
        assert_eq!(critical, Duration::from_secs(1800));
        assert_eq!(total, Duration::from_secs(3300));
    }

    #[test]
    fn first_lines_caps() {
        let text = "1\n2\n3\n4\n5";
        assert_eq!(first_lines(text, 3), vec!["1", "2", "3"]);
        assert_eq!(first_lines("only", 40), vec!["only"]);
    }
}
