//! Core PubGrub solver integration (§5.1).
//!
//! ## v2 changes
//!
//! - **CRITICAL (pubgrub 0.3 API)** — `resolve` now returns
//!   `SelectedDependencies<P, V>` and `IntoIterator` on it is not
//!   confirmed in 0.3, so the success path collects via
//!   `selected.iter().map(|(p, v)| (p.clone(), v.clone())).collect()`.
//! - **CRITICAL** — pins, profile constraints, and installed-version
//!   bias are now actually wired into resolution via
//!   [`resolve_with_constraints`]; [`resolve_dependencies`] remains as a
//!   thin wrapper. Candidate filtering happens at registration time:
//!   `Catalog` now retains the raw `(name, version, dependencies)` data
//!   it was fed so a filtered `OfflineDependencyProvider` can be rebuilt
//!   per solve (pins via the fixed `Pin::allows` with `None` abi_tag —
//!   pre-build, release core alone; profile constraints via
//!   `Ranges::contains`; installed bias by capping candidates at the
//!   installed version on the first pass and retrying uncapped on
//!   failure — `OfflineDependencyProvider` otherwise always picks the
//!   newest matching version).
//! - **CRITICAL** — the failure path now returns the pubgrub derivation
//!   tree (`ResolveError::NoSolution`) so `explain.rs` can render it
//!   without re-solving; see `explain_resolution_failure`'s replacement
//!   `explain_from_tree`.
//! - **MAJOR** — `constraint_to_range` handles pkg-core's new
//!   upper-bound-capable `VersionConstraint::Range { min, max }`
//!   variant (imported from `pkg_core`, not defined here).
//!
//! This file only handles plain `(name, VersionConstraint)` dependencies
//! — the subset that maps directly onto what the `pubgrub` crate already
//! understands. OR-groups and virtuals (§5.2) need to be translated into
//! that plain form before they can reach the solver at all — PubGrub has
//! no native concept of "any one of several alternatives satisfies this
//! dependency," so that translation is `graph.rs`'s job, layered on top
//! of what's built here, not this file's.

use std::collections::HashMap;
use std::fmt;

use pkg_core::{PkgError, PkgResult, Version, VersionConstraint};
use pubgrub::{DefaultStringReporter, OfflineDependencyProvider, Ranges, Reporter, VersionSet};
use pubgrub::report::DerivationTree;

use crate::PinSet;

/// The version-set representation the solver reasons about internally.
///
/// No adapter type needed here: `pubgrub`'s `Ranges<T>` works for any `T:
/// Debug + Display + Clone + Eq + Ord`, and `pkg_core::Version` already
/// satisfies all of that — `Ord` in particular was built for entirely
/// different reasons (comparing pins, ordering `pkg list` output) back
/// in `package.rs`, and it turns out to be exactly what an unrelated
/// third-party crate's generic bound needed too.
pub type PkgVersionSet = Ranges<Version>;

/// Effective per-package constraints applied on top of what the
/// dependency graph itself asks for — the output of
/// `profile.rs`'s `effective_constraints` (profile constraints with
/// pinned packages already dropped, "explicit over implicit", §2).
pub type EffectiveConstraints = HashMap<String, VersionConstraint>;

/// In-memory catalog of known (name, version) pairs and their plain
/// package dependencies, backing `pubgrub`'s `OfflineDependencyProvider`.
#[derive(Debug, Default)]
pub struct Catalog {
    /// Everything registered so far, in raw form. `add_package_version`
    /// used to push data straight into the provider and forget it, which
    /// made filtered re-registration (pins, profiles, installed bias)
    /// impossible — `OfflineDependencyProvider` exposes no way to list
    /// what it holds. Now the raw data is kept and a provider is built
    /// (or rebuilt, filtered) from it at solve time.
    packages: HashMap<String, Vec<(Version, Vec<(String, VersionConstraint)>)>>,
}

impl Catalog {
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers one available version of a package, along with its
    /// plain dependencies (name + version constraint pairs — no
    /// OR-groups or virtuals; see the module doc).
    ///
    /// Mirrors `OfflineDependencyProvider::add_dependencies`'s own
    /// contract: calling this again for the same `(name, version)` pair
    /// replaces its dependencies rather than adding to them.
    pub fn add_package_version(
        &mut self,
        name: impl Into<String>,
        version: Version,
        dependencies: impl IntoIterator<Item = (String, VersionConstraint)>,
    ) {
        let entry = self.packages.entry(name.into()).or_default();
        let deps: Vec<(String, VersionConstraint)> = dependencies.into_iter().collect();
        match entry.iter_mut().find(|(v, _)| *v == version) {
            Some((_, existing)) => *existing = deps,
            None => entry.push((version, deps)),
        }
    }

    /// Builds an `OfflineDependencyProvider` over this catalog, applying
    /// the active pins, the effective (profile) constraints, and —
    /// optionally — an installed-version cap per package.
    ///
    /// - **Pins** filter via the fixed `Pin::allows` with a `None`
    ///   candidate ABI tag: pre-build there is no candidate tag, so the
    ///   decision is made on release core alone and the build
    ///   orchestrator re-verifies the ABI tag post-build (§5.6.1).
    /// - **Profile constraints** filter by `Ranges::contains` — a
    ///   candidate version is registered only if it satisfies the
    ///   package's effective constraint.
    /// - **Installed bias** (`cap_at_installed`): versions strictly newer
    ///   than the currently installed one are withheld, so the
    ///   provider's "newest matching version" pick lands on the
    ///   installed version whenever that version is otherwise allowed.
    ///   `resolve_with_constraints` runs this as a first pass and retries
    ///   uncapped if it fails, so an upgrade that is *required* (by a
    ///   dependency constraint or an explicit pin) still goes through —
    ///   the cap is a preference, not a prohibition.
    fn filtered_provider(
        &self,
        pins: &PinSet,
        profile_constraints: &EffectiveConstraints,
        installed: &HashMap<String, Version>,
        cap_at_installed: bool,
    ) -> OfflineDependencyProvider<String, PkgVersionSet> {
        let mut provider = OfflineDependencyProvider::new();
        for (name, versions) in &self.packages {
            let pin = pins.get(name);
            let constraint = profile_constraints.get(name);
            let installed_version = installed.get(name);
            for (version, deps) in versions {
                if let Some(pin) = pin {
                    if !pin.allows(version, None) {
                        continue;
                    }
                }
                if let Some(constraint) = constraint {
                    if !constraint_to_range(constraint).contains(version) {
                        continue;
                    }
                }
                if cap_at_installed {
                    if let Some(installed_version) = installed_version {
                        if version > installed_version {
                            continue;
                        }
                    }
                }
                provider.add_dependencies(
                    name.clone(),
                    version.clone(),
                    deps.iter()
                        .map(|(dep_name, c)| (dep_name.clone(), constraint_to_range(c))),
                );
            }
        }
        provider
    }
}

/// Converts a manifest-level `VersionConstraint` (§4.2) into the version
/// set `pubgrub` actually reasons about.
///
/// `Range { min, max }`'s `max` is EXCLUSIVE, matching pkg-core's
/// `VersionConstraint::matches` contract (`<max`, e.g. "mesa >= 24.0,
/// < 25"). pubgrub's `higher_than(v)` is inclusive (>= v) and
/// `strictly_higher_than(v)` is > v, so "< max" is the complement of
/// `higher_than(max)`.
pub fn constraint_to_range(constraint: &VersionConstraint) -> PkgVersionSet {
    match constraint {
        VersionConstraint::Any => Ranges::full(),
        VersionConstraint::Exact(v) => Ranges::singleton(v.clone()),
        VersionConstraint::AtLeast(v) => Ranges::higher_than(v.clone()),
        VersionConstraint::Range { min, max } => {
            let lower = Ranges::higher_than(min.clone());
            match max {
                // Inclusive upper bound = complement of "strictly above max".
                Some(max) => lower.intersection(&Ranges::strictly_higher_than(max.clone()).complement()),
                None => lower,
            }
        }
    }
}

// ---------------------------------------------------------------------
// Resolution
// ---------------------------------------------------------------------

/// A failed resolution. The `NoSolution` variant carries pubgrub's full
/// derivation tree — the structured explanation of *why* no solution
/// exists — so `explain.rs` can render it with `DefaultStringReporter`
/// without solving a second time.
#[derive(Debug)]
pub enum ResolveError {
    NoSolution(DerivationTree<String, PkgVersionSet>),
    /// Any other `PubGrubError`. Unreachable in practice for a
    /// `Catalog`-backed provider (`OfflineDependencyProvider`'s `Err`
    /// associated type is `Infallible`), kept because the match has to
    /// be exhaustive over `PubGrubError`'s full variant set.
    Other(String),
}

impl ResolveError {
    /// The derivation tree, when this failure is a genuine conflict.
    pub fn derivation_tree(&self) -> Option<&DerivationTree<String, PkgVersionSet>> {
        match self {
            ResolveError::NoSolution(tree) => Some(tree),
            ResolveError::Other(_) => None,
        }
    }
}

impl fmt::Display for ResolveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ResolveError::NoSolution(tree) => {
                write!(f, "{}", DefaultStringReporter::report(tree))
            }
            ResolveError::Other(message) => write!(f, "{message}"),
        }
    }
}

impl std::error::Error for ResolveError {}

/// Resolves `root_name` at `root_version` against `catalog`, applying
/// the active `pins`, the effective profile constraints, and a bias
/// toward already-installed versions.
///
/// Candidate filtering happens at provider registration time (see
/// [`Catalog::filtered_provider`]): pinned packages only register
/// versions `Pin::allows` accepts (with a `None` candidate ABI tag —
/// pre-build, so release core alone decides and the orchestrator
/// re-verifies the tag post-build), and profile-constrained packages
/// only register versions the constraint contains.
///
/// Installed-version bias is a two-pass preference, not a hard rule:
/// the first pass withholds versions newer than the installed one so the
/// solver's "newest wins" pick (`OfflineDependencyProvider` has no
/// preference hook) stays on the installed version when it is otherwise
/// allowed; if that pass has no solution, a second uncapped pass runs,
/// so required upgrades still resolve.
///
/// On failure the returned [`ResolveError`] carries the derivation tree
/// from whichever pass actually failed (the uncapped pass, when both
/// fail — it explains the real conflict, not the self-imposed cap).
pub fn resolve_with_constraints(
    catalog: &Catalog,
    root_name: &str,
    root_version: Version,
    pins: &PinSet,
    profile_constraints: &EffectiveConstraints,
    installed: &HashMap<String, Version>,
) -> Result<HashMap<String, Version>, ResolveError> {
    let solve = |cap_at_installed: bool| {
        let provider =
            catalog.filtered_provider(pins, profile_constraints, installed, cap_at_installed);
        pubgrub::resolve(&provider, root_name.to_string(), root_version.clone()).map_err(|e| {
            match e {
                pubgrub::PubGrubError::NoSolution(tree) => ResolveError::NoSolution(tree),
                other => ResolveError::Other(other.to_string()),
            }
        })
    };

    let selected = match solve(true) {
        Ok(selected) => selected,
        Err(first_error) => {
            if installed.is_empty() {
                // No bias was applied; the failure is already final.
                return Err(first_error);
            }
            match solve(false) {
                Ok(selected) => selected,
                // The uncapped failure is the truthful explanation.
                Err(uncapped_error) => return Err(uncapped_error),
            }
        }
    };

    // pubgrub 0.3: `resolve` returns `SelectedDependencies<P, V>`;
    // `IntoIterator` on it is not confirmed, so collect by reference.
    Ok(selected
        .iter()
        .map(|(p, v)| (p.clone(), v.clone()))
        .collect())
}

/// Resolves `root_name` at `root_version` against `catalog`, returning
/// the chosen version of every package pulled in — including
/// `root_name` itself.
///
/// Thin wrapper over [`resolve_with_constraints`] with no pins, no
/// profile constraints, and no installed-version bias, flattening the
/// error to `PkgError::Resolution` (§16.1: exit code 1) as before. Use
/// the full entry point when the derivation tree is needed — see
/// `explain.rs`'s `explain_from_tree`.
pub fn resolve_dependencies(
    catalog: &Catalog,
    root_name: &str,
    root_version: Version,
) -> PkgResult<HashMap<String, Version>> {
    resolve_with_constraints(
        catalog,
        root_name,
        root_version,
        &PinSet::new(),
        &EffectiveConstraints::new(),
        &HashMap::new(),
    )
    .map_err(|e| PkgError::Resolution(e.to_string()))
}

// ---------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Pin, PinMode};
    use pkg_core::AbiTag;

    fn v(s: &str) -> Version {
        s.parse().unwrap()
    }

    fn no_constraints() -> EffectiveConstraints {
        EffectiveConstraints::new()
    }

    fn no_installed() -> HashMap<String, Version> {
        HashMap::new()
    }

    #[test]
    fn resolves_a_linear_chain() {
        // root -> menu -> dropdown -> icons, each with no alternative
        // versions — the simplest possible case, mirroring pubgrub's own
        // canonical example but through pkg-core's types instead of
        // plain &str/u32.
        let mut catalog = Catalog::new();
        catalog.add_package_version(
            "root",
            v("1.0.0"),
            [
                ("menu".to_string(), VersionConstraint::Any),
            ],
        );
        catalog.add_package_version(
            "menu",
            v("1.0.0"),
            [("dropdown".to_string(), VersionConstraint::Any)],
        );
        catalog.add_package_version(
            "dropdown",
            v("1.0.0"),
            [("icons".to_string(), VersionConstraint::Any)],
        );
        catalog.add_package_version("icons", v("1.0.0"), []);

        let solution = resolve_dependencies(&catalog, "root", v("1.0.0")).unwrap();
        assert_eq!(solution.get("menu"), Some(&v("1.0.0")));
        assert_eq!(solution.get("dropdown"), Some(&v("1.0.0")));
        assert_eq!(solution.get("icons"), Some(&v("1.0.0")));
    }

    #[test]
    fn diamond_dependency_picks_a_version_satisfying_both_constraints() {
        // root depends on both a and b; a requires c >= 2.0, b accepts
        // any c. Two versions of c are available (1.0 and 2.0) — only
        // 2.0 satisfies both branches of the diamond simultaneously.
        let mut catalog = Catalog::new();
        catalog.add_package_version(
            "root",
            v("1.0.0"),
            [
                ("a".to_string(), VersionConstraint::Any),
                ("b".to_string(), VersionConstraint::Any),
            ],
        );
        catalog.add_package_version(
            "a",
            v("1.0.0"),
            [("c".to_string(), VersionConstraint::AtLeast(v("2.0.0")))],
        );
        catalog.add_package_version(
            "b",
            v("1.0.0"),
            [("c".to_string(), VersionConstraint::Any)],
        );
        catalog.add_package_version("c", v("1.0.0"), []);
        catalog.add_package_version("c", v("2.0.0"), []);

        let solution = resolve_dependencies(&catalog, "root", v("1.0.0")).unwrap();
        assert_eq!(solution.get("c"), Some(&v("2.0.0")));
    }

    #[test]
    fn at_least_constraint_excludes_older_versions() {
        let mut catalog = Catalog::new();
        catalog.add_package_version(
            "root",
            v("1.0.0"),
            [("lib".to_string(), VersionConstraint::AtLeast(v("2.0.0")))],
        );
        catalog.add_package_version("lib", v("1.0.0"), []);
        catalog.add_package_version("lib", v("2.5.0"), []);

        let solution = resolve_dependencies(&catalog, "root", v("1.0.0")).unwrap();
        assert_eq!(solution.get("lib"), Some(&v("2.5.0")));
    }

    #[test]
    fn exact_constraint_pins_precisely() {
        let mut catalog = Catalog::new();
        catalog.add_package_version(
            "root",
            v("1.0.0"),
            [("lib".to_string(), VersionConstraint::Exact(v("1.5.0")))],
        );
        catalog.add_package_version("lib", v("1.5.0"), []);
        catalog.add_package_version("lib", v("2.0.0"), []); // newer, but not what was asked for

        let solution = resolve_dependencies(&catalog, "root", v("1.0.0")).unwrap();
        assert_eq!(solution.get("lib"), Some(&v("1.5.0")));
    }

    #[test]
    fn range_constraint_respects_its_upper_bound() {
        let mut catalog = Catalog::new();
        catalog.add_package_version(
            "root",
            v("1.0.0"),
            [("lib".to_string(), VersionConstraint::Range {
                min: v("2.0.0"),
                max: Some(v("2.5.0")),
            })],
        );
        catalog.add_package_version("lib", v("1.9.0"), []); // below the floor
        catalog.add_package_version("lib", v("2.4.0"), []);
        catalog.add_package_version("lib", v("2.5.0"), []); // == max: excluded (max is exclusive)
        catalog.add_package_version("lib", v("3.0.0"), []); // above the cap

        let solution = resolve_dependencies(&catalog, "root", v("1.0.0")).unwrap();
        assert_eq!(solution.get("lib"), Some(&v("2.4.0")));
    }

    #[test]
    fn conflicting_exact_constraints_are_unsatisfiable() {
        // a requires lib==1.0.0, b requires lib==2.0.0 — no single
        // version of lib can satisfy both.
        let mut catalog = Catalog::new();
        catalog.add_package_version(
            "root",
            v("1.0.0"),
            [
                ("a".to_string(), VersionConstraint::Any),
                ("b".to_string(), VersionConstraint::Any),
            ],
        );
        catalog.add_package_version(
            "a",
            v("1.0.0"),
            [("lib".to_string(), VersionConstraint::Exact(v("1.0.0")))],
        );
        catalog.add_package_version(
            "b",
            v("1.0.0"),
            [("lib".to_string(), VersionConstraint::Exact(v("2.0.0")))],
        );
        catalog.add_package_version("lib", v("1.0.0"), []);
        catalog.add_package_version("lib", v("2.0.0"), []);

        let result = resolve_dependencies(&catalog, "root", v("1.0.0"));
        assert!(matches!(result, Err(PkgError::Resolution(_))));
    }

    #[test]
    fn unsatisfiable_resolution_returns_the_derivation_tree() {
        let mut catalog = Catalog::new();
        catalog.add_package_version(
            "root",
            v("1.0.0"),
            [("lib".to_string(), VersionConstraint::Exact(v("9.9.9")))],
        );
        catalog.add_package_version("lib", v("1.0.0"), []);

        let result = resolve_with_constraints(
            &catalog,
            "root",
            v("1.0.0"),
            &PinSet::new(),
            &no_constraints(),
            &no_installed(),
        );
        match result {
            Err(ResolveError::NoSolution(_)) => {}
            other => panic!("expected NoSolution with a derivation tree, got {other:?}"),
        }
    }

    #[test]
    fn patch_suffixed_versions_participate_correctly() {
        // Exercises Version's Ord impl (package.rs) through the solver:
        // 2.39 < 2.39-r1 < 2.40, and an AtLeast(2.39-r1) constraint
        // should exclude the bare 2.39 build.
        let mut catalog = Catalog::new();
        catalog.add_package_version(
            "root",
            v("1.0.0"),
            [("glibc".to_string(), VersionConstraint::AtLeast(v("2.39-r1")))],
        );
        catalog.add_package_version("glibc", v("2.39"), []);
        catalog.add_package_version("glibc", v("2.39-r1"), []);
        catalog.add_package_version("glibc", v("2.39-r2"), []);

        let solution = resolve_dependencies(&catalog, "root", v("1.0.0")).unwrap();
        // Newest available version satisfying the constraint.
        assert_eq!(solution.get("glibc"), Some(&v("2.39-r2")));
    }

    #[test]
    fn pins_filter_candidate_versions() {
        // glibc pinned at 2.39-r1 abi-track: 2.40 must never be chosen
        // even though it's the newest and unconstrained by the graph.
        let mut catalog = Catalog::new();
        catalog.add_package_version(
            "root",
            v("1.0.0"),
            [("glibc".to_string(), VersionConstraint::Any)],
        );
        catalog.add_package_version("glibc", v("2.39-r1"), []);
        catalog.add_package_version("glibc", v("2.39-r2"), []);
        catalog.add_package_version("glibc", v("2.40"), []);

        let mut pins = PinSet::new();
        pins.insert(Pin {
            package: "glibc".to_string(),
            version: v("2.39-r1"),
            mode: PinMode::AbiTrack,
            pinned_abi_tag: Some(AbiTag(1)),
        });

        let solution = resolve_with_constraints(
            &catalog,
            "root",
            v("1.0.0"),
            &pins,
            &no_constraints(),
            &no_installed(),
        )
        .unwrap();
        assert_eq!(solution.get("glibc"), Some(&v("2.39-r2")));
    }

    #[test]
    fn profile_constraints_filter_candidate_versions() {
        let mut catalog = Catalog::new();
        catalog.add_package_version(
            "root",
            v("1.0.0"),
            [("mesa".to_string(), VersionConstraint::Any)],
        );
        catalog.add_package_version("mesa", v("23.3"), []);
        catalog.add_package_version("mesa", v("24.0"), []);
        catalog.add_package_version("mesa", v("24.1"), []);

        let mut constraints = no_constraints();
        constraints.insert("mesa".to_string(), VersionConstraint::Exact(v("24.0")));

        let solution = resolve_with_constraints(
            &catalog,
            "root",
            v("1.0.0"),
            &PinSet::new(),
            &constraints,
            &no_installed(),
        )
        .unwrap();
        assert_eq!(solution.get("mesa"), Some(&v("24.0")));
    }

    #[test]
    fn installed_bias_prefers_the_installed_version() {
        // Without bias the solver picks the newest (2.0.0); with lib
        // installed at 1.0.0 and no requirement forcing an upgrade, the
        // installed version should be kept.
        let mut catalog = Catalog::new();
        catalog.add_package_version(
            "root",
            v("1.0.0"),
            [("lib".to_string(), VersionConstraint::Any)],
        );
        catalog.add_package_version("lib", v("1.0.0"), []);
        catalog.add_package_version("lib", v("2.0.0"), []);

        let mut installed = no_installed();
        installed.insert("lib".to_string(), v("1.0.0"));

        let solution = resolve_with_constraints(
            &catalog,
            "root",
            v("1.0.0"),
            &PinSet::new(),
            &no_constraints(),
            &installed,
        )
        .unwrap();
        assert_eq!(solution.get("lib"), Some(&v("1.0.0")));
    }

    #[test]
    fn installed_bias_yields_to_a_required_upgrade() {
        // root requires lib >= 2.0.0, which the installed 1.0.0 can't
        // satisfy — the capped pass fails and the retry must upgrade.
        let mut catalog = Catalog::new();
        catalog.add_package_version(
            "root",
            v("1.0.0"),
            [("lib".to_string(), VersionConstraint::AtLeast(v("2.0.0")))],
        );
        catalog.add_package_version("lib", v("1.0.0"), []);
        catalog.add_package_version("lib", v("2.0.0"), []);

        let mut installed = no_installed();
        installed.insert("lib".to_string(), v("1.0.0"));

        let solution = resolve_with_constraints(
            &catalog,
            "root",
            v("1.0.0"),
            &PinSet::new(),
            &no_constraints(),
            &installed,
        )
        .unwrap();
        assert_eq!(solution.get("lib"), Some(&v("2.0.0")));
    }
}
