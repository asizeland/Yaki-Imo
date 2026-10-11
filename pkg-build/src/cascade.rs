//! pkg-build/src/cascade.rs — rebuild-set computation (SPEC "pkg-build",
//! design doc Issue 4.1, v13 §8).
//!
//! ## v1 changes
//!
//! - Initial implementation of link-interface-driven cascade
//!   computation: a bumped package cascades only to dependents whose
//!   *consumed* `link_interface_hash` changed (Issue 4.1.1); `runtime`-kind
//!   edges are excluded from cascades and `build`-kind edges never
//!   cascade (Issue 4.1.2); unclassifiable edges are stored as `link`
//!   in the DB by the scanner boundary, so treating every non-runtime
//!   edge as a link edge implements "uncertainty widens the cascade,
//!   never narrows it" (Issue 4.1.4). glibc is the base case
//!   (v13 §8): `abi_tag(glibc) = hash("glibc-" + release_core)`, so a
//!   category-1 patch (same release core) cascades to *nothing* and a
//!   major bump cascades unconditionally to every link dependent.
//!
//! The governing invariant (Issue 4): the computed set must remain a
//! superset of "actually broken" and approach "actually affected" from
//! above. Every `None` hash (unmeasured, never-built, DB-missing) is
//! treated as *changed* — safe widening, never pruning.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use pkg_core::{PkgError, PkgResult, Version};
use pkg_db::{queries, schema::DependencyKind};
use rusqlite::Connection;

/// Dependency-edge kinds that propagate a cascade. `Link` only:
/// `Runtime` edges are excluded by Issue 4.1.2 and `Build` edges never
/// cascade. Scanner-unclassifiable edges are recorded as `Link`
/// upstream (Issue 4.1.4), so they propagate here automatically.
pub const CASCADE_KINDS: &[DependencyKind] = &[DependencyKind::Link];

/// A package whose version bump (or rebuild) may force rebuilds of its
/// dependents. The interface hashes are *inputs* because the whole
/// point of Issue 4.1.1 is that the cascade decision is a comparison
/// between what dependents consumed and what the new build exports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BumpedPackage {
    pub name: String,
    /// The version being replaced (currently installed).
    pub old_version: Version,
    /// The version being installed.
    pub new_version: Version,
    /// `link_interface_hash` of the installed build. `None` (never
    /// measured) is treated as "changed" — uncertainty widens.
    pub old_link_hash: Option<String>,
    /// `link_interface_hash` of the new build, when already known
    /// (e.g. measured during a previous build of the same version, or
    /// post-build re-pruning mid-cascade). `None` is treated as
    /// "changed".
    pub new_link_hash: Option<String>,
}

impl BumpedPackage {
    /// Did this bump change the link interface its dependents consumed?
    /// Conservative: any missing measurement counts as changed.
    pub fn interface_changed(&self) -> bool {
        match (&self.old_link_hash, &self.new_link_hash) {
            (Some(old), Some(new)) => old != new,
            _ => true,
        }
    }

    /// The v13 §8 base case: glibc's tag derives from `release_core`
    /// alone, so a same-release-core bump (category-1 patch) perturbs
    /// the cascade *zero* times and a cross-release-core bump perturbs
    /// it exactly once (unconditionally).
    pub fn cascades(&self) -> bool {
        if self.name == "glibc" {
            return !self.old_version.same_release_core(&self.new_version);
        }
        self.interface_changed()
    }
}

/// Why a node sits in the rebuild set — surfaced by `--dry-run` and the
/// final report so the cascade is explainable, not just executed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RebuildReason {
    /// A direct update the user asked for (cascade root).
    Root,
    /// A consumed dependency's measured link interface changed.
    InterfaceChanged { dependency: String },
    /// glibc crossed a release-core boundary (v13 §8 base case).
    GlibcMajorBump,
}

/// One package in the rebuild DAG.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CascadeNode {
    pub name: String,
    pub version: Version,
    pub reason: RebuildReason,
    /// Names of other *in-cascade* packages this node depends on
    /// (in-cascade edges only — the orchestrator's activation-order
    /// constraints).
    pub deps_in_cascade: BTreeSet<String>,
}

/// A dependent pruned from the cascade because the interface it
/// consumed did not change — counted for the dry-run preview's
/// "link-interface pruning saved: N packages" line (Issue 4.4.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrunedNode {
    pub name: String,
    pub version: Version,
    /// The bumped dependency whose unchanged interface caused the prune.
    pub via: String,
}

/// The computed rebuild set: nodes plus the in-cascade dependency
/// edges between them.
#[derive(Debug, Clone, Default)]
pub struct CascadePlan {
    pub nodes: BTreeMap<String, CascadeNode>,
    pub pruned: Vec<PrunedNode>,
}

impl CascadePlan {
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    /// Names of the direct-update roots.
    pub fn roots(&self) -> Vec<&str> {
        self.nodes
            .values()
            .filter(|n| n.reason == RebuildReason::Root)
            .map(|n| n.name.as_str())
            .collect()
    }

    /// Names of the in-cascade dependents of `name` — the subtree a
    /// failure of `name` blocks (Issue 4.2.6).
    pub fn dependents_of(&self, name: &str) -> BTreeSet<String> {
        self.nodes
            .values()
            .filter(|n| n.deps_in_cascade.contains(name))
            .map(|n| n.name.clone())
            .collect()
    }

    /// Transitive in-cascade dependents of `name` (the blocked subtree).
    pub fn blocked_subtree(&self, name: &str) -> BTreeSet<String> {
        let mut out = BTreeSet::new();
        let mut queue: VecDeque<String> = self.dependents_of(name).into_iter().collect();
        while let Some(n) = queue.pop_front() {
            if out.insert(n.clone()) {
                queue.extend(self.dependents_of(&n));
            }
        }
        out
    }

    /// Topological levels of the rebuild DAG (Issue 4.2.1): level 0 is
    /// nodes with no in-cascade dependencies, level *k* is nodes whose
    /// in-cascade deps all sit below level *k*. Levels are a scheduling
    /// *correctness* device only — the executor starts a node the moment
    /// its deps are activated, not when its level completes.
    ///
    /// Returns `Err` naming a node if the in-cascade edges contain a
    /// cycle (which would indicate a resolver bug — package dependency
    /// graphs are acyclic).
    pub fn levels(&self) -> PkgResult<Vec<Vec<String>>> {
        let mut level_of: BTreeMap<&str, usize> = BTreeMap::new();
        // Kahn's algorithm by repeated relaxation; the DAG is small
        // (hundreds of nodes), so O(V·E) simplicity beats cleverness.
        loop {
            let mut progressed = false;
            for (name, node) in &self.nodes {
                if level_of.contains_key(name.as_str()) {
                    continue;
                }
                let mut level = 0usize;
                let mut ready = true;
                for dep in &node.deps_in_cascade {
                    match level_of.get(dep.as_str()) {
                        Some(dep_level) => level = level.max(dep_level + 1),
                        // A dep not in the plan at all is an
                        // out-of-cascade edge — already satisfied by the
                        // installed system, ignore it.
                        None if !self.nodes.contains_key(dep) => {}
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
            if level_of.len() == self.nodes.len() {
                break;
            }
            if !progressed {
                let stuck = self
                    .nodes
                    .keys()
                    .find(|n| !level_of.contains_key(n.as_str()))
                    .cloned()
                    .unwrap_or_default();
                return Err(PkgError::Build(format!(
                    "rebuild cascade contains a dependency cycle at {stuck}"
                )));
            }
        }
        let depth = level_of.values().copied().max().map_or(0, |m| m + 1);
        let mut levels: Vec<Vec<String>> = vec![Vec::new(); depth];
        for (name, level) in level_of {
            levels[level].push(name.to_string());
        }
        for level in &mut levels {
            level.sort();
        }
        Ok(levels)
    }
}

/// Computes the rebuild set for `bumped` packages (Issue 4.1.1).
///
/// Breadth-first over link-kind reverse dependencies out of the
/// installed database: a dependent enters the set iff the interface it
/// consumed changed; entering nodes propagate to their own link
/// dependents in turn (their interface change is *conservatively
/// assumed* at plan time — a dependent being rebuilt may itself rebuild
/// byte-identically, and post-build re-measurement then prunes its
/// subtree; see `prune_after_build`). glibc follows the v13 §8 base
/// case instead of hash comparison.
///
/// `bumped` entries whose `cascades()` is false contribute only their
/// own node (they are still rebuilt/updated; they just don't cascade).
pub fn compute_rebuild_set(
    conn: &Connection,
    bumped: &[BumpedPackage],
) -> PkgResult<CascadePlan> {
    let mut plan = CascadePlan::default();
    let installed = queries::list_installed_packages(conn)?;
    let id_of: BTreeMap<&str, i64> = installed
        .iter()
        .map(|p| (p.name.as_str(), p.id))
        .collect();

    let mut queue: VecDeque<(String, RebuildReason)> = VecDeque::new();
    for bump in bumped {
        let reason = if bump.name == "glibc" && !bump.old_version.same_release_core(&bump.new_version)
        {
            RebuildReason::GlibcMajorBump
        } else {
            RebuildReason::Root
        };
        plan.nodes.insert(
            bump.name.clone(),
            CascadeNode {
                name: bump.name.clone(),
                version: bump.new_version.clone(),
                reason: reason.clone(),
                deps_in_cascade: BTreeSet::new(),
            },
        );
        if bump.cascades() {
            queue.push_back((bump.name.clone(), reason));
        } else if let Some(&pkg_id) = id_of.get(bump.name.as_str()) {
            // The bump itself still happens, but the measured-unchanged
            // interface prunes every link dependent — counted for the
            // dry-run preview's "pruning saved" line (Issue 4.4.1).
            for dep in queries::reverse_dependents(conn, pkg_id, CASCADE_KINDS)? {
                plan.pruned.push(PrunedNode {
                    name: dep.name.clone(),
                    version: dep.version.clone(),
                    via: bump.name.clone(),
                });
            }
        }
    }

    while let Some((name, _reason)) = queue.pop_front() {
        let Some(&pkg_id) = id_of.get(name.as_str()) else {
            // Not installed (a genuinely new package) — nothing to cascade to.
            continue;
        };
        let dependents = queries::reverse_dependents(conn, pkg_id, CASCADE_KINDS)?;
        for dep in dependents {
            if plan.nodes.contains_key(&dep.name) {
                // Already in the set: record the edge so activation
                // ordering respects it, but don't re-propagate.
                if let Some(node) = plan.nodes.get_mut(&dep.name) {
                    node.deps_in_cascade.insert(name.clone());
                }
                continue;
            }
            // The queued node's interface is changed (measured or
            // conservatively assumed), so its dependents enter the set;
            // a dependent whose own rebuild turns out byte-identical
            // gets its subtree re-pruned post-build via
            // `prune_after_build` (Issue 4.1.1's measured narrowing).
            plan.nodes.insert(
                dep.name.clone(),
                CascadeNode {
                    name: dep.name.clone(),
                    version: dep.version.clone(),
                    reason: RebuildReason::InterfaceChanged {
                        dependency: name.clone(),
                    },
                    deps_in_cascade: BTreeSet::from([name.clone()]),
                },
            );
            queue.push_back((
                dep.name.clone(),
                RebuildReason::InterfaceChanged {
                    dependency: name.clone(),
                },
            ));
        }
    }
    Ok(plan)
}

/// Post-build re-pruning (Issue 4.1.1's "the cascade prunes itself at
/// that node"): after `name` is rebuilt, if its measured new
/// `link_interface_hash` equals the hash dependents consumed, its
/// in-cascade dependents (transitively, where each is itself
/// interface-unchanged — but at plan level we can't know, so the
/// orchestrator repeats this check per completed node) are removed
/// from the pending set. Returns the pruned names.
///
/// Pure over the plan plus the comparison result; the caller supplies
/// `interface_changed` from a fresh `link_interface_hash` measurement.
pub fn prune_after_build(plan: &mut CascadePlan, name: &str, interface_changed: bool) -> Vec<String> {
    if interface_changed || !plan.nodes.contains_key(name) {
        return Vec::new();
    }
    let subtree = plan.blocked_subtree(name);
    let mut pruned = Vec::new();
    for dep_name in subtree {
        if let Some(node) = plan.nodes.remove(&dep_name) {
            plan.pruned.push(PrunedNode {
                name: dep_name.clone(),
                version: node.version,
                via: name.to_string(),
            });
            pruned.push(dep_name);
        }
    }
    // Clean dangling in-cascade edges to pruned nodes.
    let remaining: std::collections::BTreeSet<String> = plan.nodes.keys().cloned().collect();
    for node in plan.nodes.values_mut() {
        node.deps_in_cascade.retain(|d| remaining.contains(d));
    }
    pruned
}

// ---------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> Version {
        s.parse().unwrap()
    }

    fn node(name: &str, deps: &[&str]) -> CascadeNode {
        CascadeNode {
            name: name.to_string(),
            version: v("1.0"),
            reason: RebuildReason::Root,
            deps_in_cascade: deps.iter().map(|d| d.to_string()).collect(),
        }
    }

    fn plan_of(nodes: Vec<CascadeNode>) -> CascadePlan {
        CascadePlan {
            nodes: nodes.into_iter().map(|n| (n.name.clone(), n)).collect(),
            pruned: Vec::new(),
        }
    }

    #[test]
    fn glibc_base_case_release_core() {
        // v13 §8: category-1 patch (same release core) never cascades…
        let patch = BumpedPackage {
            name: "glibc".into(),
            old_version: v("2.39-r1"),
            new_version: v("2.39-r2"),
            old_link_hash: None,
            new_link_hash: None,
        };
        assert!(!patch.cascades());
        // …a major bump cascades exactly once, unconditionally.
        let major = BumpedPackage {
            name: "glibc".into(),
            old_version: v("2.39"),
            new_version: v("2.40"),
            old_link_hash: Some("same".into()),
            new_link_hash: Some("same".into()),
        };
        assert!(major.cascades());
    }

    #[test]
    fn unchanged_interface_prunes() {
        let bump = BumpedPackage {
            name: "openssl".into(),
            old_version: v("3.0.13"),
            new_version: v("3.0.14"),
            old_link_hash: Some("abc123".into()),
            new_link_hash: Some("abc123".into()),
        };
        assert!(!bump.cascades());
    }

    #[test]
    fn unmeasured_interface_widens() {
        // None on either side = treated as changed (safe direction).
        let bump = BumpedPackage {
            name: "openssl".into(),
            old_version: v("3.0.13"),
            new_version: v("3.0.14"),
            old_link_hash: None,
            new_link_hash: Some("abc123".into()),
        };
        assert!(bump.cascades());
    }

    #[test]
    fn levels_of_diamond() {
        // openssl → {curl, git} → python: level 0, level 1, level 2.
        let plan = plan_of(vec![
            node("openssl", &[]),
            node("curl", &["openssl"]),
            node("git", &["openssl"]),
            node("python", &["curl", "git"]),
        ]);
        let levels = plan.levels().unwrap();
        assert_eq!(levels, vec![
            vec!["openssl".to_string()],
            vec!["curl".to_string(), "git".to_string()],
            vec!["python".to_string()],
        ]);
    }

    #[test]
    fn levels_ignore_out_of_cascade_edges() {
        // A dep not in the plan is already satisfied by the installed
        // system and must not affect leveling.
        let plan = plan_of(vec![node("app", &["not-in-plan"])]);
        assert_eq!(plan.levels().unwrap(), vec![vec!["app".to_string()]]);
    }

    #[test]
    fn levels_detect_cycle() {
        let plan = plan_of(vec![node("a", &["b"]), node("b", &["a"])]);
        assert!(plan.levels().is_err());
    }

    #[test]
    fn blocked_subtree_is_transitive() {
        let plan = plan_of(vec![
            node("openssl", &[]),
            node("curl", &["openssl"]),
            node("python", &["curl"]),
            node("zlib", &[]),
        ]);
        let blocked = plan.blocked_subtree("openssl");
        assert!(blocked.contains("curl") && blocked.contains("python"));
        assert!(!blocked.contains("zlib"));
    }

    #[test]
    fn prune_after_unchanged_build_removes_subtree() {
        let mut plan = plan_of(vec![
            node("openssl", &[]),
            node("curl", &["openssl"]),
            node("python", &["curl"]),
        ]);
        let pruned = prune_after_build(&mut plan, "openssl", false);
        assert_eq!(pruned.len(), 2);
        assert_eq!(plan.len(), 1);
        assert_eq!(plan.pruned.len(), 2);
        // A changed interface prunes nothing.
        let mut plan2 = plan_of(vec![node("a", &[]), node("b", &["a"])]);
        assert!(prune_after_build(&mut plan2, "a", true).is_empty());
        assert_eq!(plan2.len(), 2);
    }
}
