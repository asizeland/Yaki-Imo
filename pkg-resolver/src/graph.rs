//! Virtuals and OR-groups (§5.2) as an extension over `pubgrub.rs`'s
//! plain solver integration. Multilib co-selection (§5.4) is not yet
//! covered in this pass.
//!
//! ## v2 changes
//!
//! - **[MAJOR] Aligned with pubgrub.rs's v2 API.** `finalize` now builds
//!   on the raw-data-retaining [`Catalog`] (`add_package_version`), and
//!   resolution goes through `pubgrub.rs`'s `resolve_with_constraints`
//!   — pins, effective profile constraints, and installed-version bias
//!   are honored for graph resolution exactly as they are for plain
//!   resolution. The full entry point is
//!   [`ResolvedGraph::resolve_with_constraints`], returning
//!   [`ResolveError`] (derivation tree intact) so `explain.rs` can
//!   render failures; the free [`resolve`] function remains as a thin
//!   unconstrained wrapper flattening to `PkgError::Resolution`.
//! - **[MAJOR] Preferred-provider bias.** `finalize` takes the user's
//!   `alternatives.toml` preferences as a `&BTreeMap<String, String>`
//!   (virtual name → preferred provider, keyed by the full
//!   `virtual/<name>` string with a short-name fallback). When the
//!   preferred provider is among the registered providers, its
//!   synthetic alternative is ordered LAST — `OfflineDependencyProvider`
//!   tries the newest synthetic version first, so the preferred
//!   provider is the pick whenever it is satisfiable. A preference
//!   naming a provider that isn't registered is ignored: this is a
//!   bias, not a requirement, so `pkg alternatives set` can never make
//!   a graph unresolvable.
//! - No other behavior change: synthetic-package encoding, the
//!   "$or:" naming convention, provider dedup-by-name, and the
//!   no-provider early error are all as before.
//!
//! PubGrub has no native concept of "any one of several alternatives
//! satisfies this dependency" — `get_dependencies` returns a fixed AND
//! of constraints for a given (package, version). The standard technique
//! for encoding an OR anyway: introduce a synthetic package, and give it
//! one synthetic *version* per alternative, where synthetic version N's
//! only dependency is alternative N. Since PubGrub always picks exactly
//! one version of any package it selects, picking the synthetic
//! package's version N *is* choosing alternative N — the solver's
//! ordinary backtracking search does the actual OR-selection for free,
//! using the same machinery it already uses for everything else.
//!
//! A virtual (§5.2) is a special case of this: "depends on
//! `virtual/kernel`" reduces to "depends on any one of: every package
//! currently registered as a provider of `virtual/kernel`" — the same
//! synthetic-package trick, just with the alternative list computed from
//! a provider registry instead of given directly.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use pkg_core::{Dependency, PkgError, PkgResult, Version, VersionConstraint, VirtualName};

use crate::{Catalog, EffectiveConstraints, PinSet, ResolveError};

// ---------------------------------------------------------------------
// PackageGraph
// ---------------------------------------------------------------------

struct PackageEntry {
    name: String,
    version: Version,
    dependencies: Vec<Dependency>,
}

/// Accumulates package registrations — identity, `provides`, and
/// dependencies — before they're translated into a `pubgrub`-ready
/// `Catalog`.
///
/// Registration and translation are deliberately separate steps
/// (`add_package` vs. `finalize`), not combined into one. A package
/// might declare `Dependency::Virtual("virtual/kernel")` before any
/// provider of that virtual has been registered yet — packages can be
/// added in any order — so there's no way to correctly resolve a virtual
/// reference at `add_package` time; the provider registry has to be
/// complete first, which is only guaranteed once every package has been
/// added.
#[derive(Default)]
pub struct PackageGraph {
    packages: Vec<PackageEntry>,
    providers: HashMap<VirtualName, BTreeSet<String>>,
}

impl PackageGraph {
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers one package version: its `provides` list (§5.2) and
    /// its dependencies (§4.1) — runtime or build, whichever the caller
    /// is building a graph for.
    pub fn add_package(
        &mut self,
        name: impl Into<String>,
        version: Version,
        provides: &[VirtualName],
        dependencies: Vec<Dependency>,
    ) {
        let name = name.into();
        for virt in provides {
            self.providers.entry(virt.clone()).or_default().insert(name.clone());
        }
        self.packages.push(PackageEntry {
            name,
            version,
            dependencies,
        });
    }

    /// Translates every registered package into a `pubgrub`-ready
    /// `Catalog`, materializing a synthetic package for each OR-group
    /// and virtual reference encountered along the way.
    ///
    /// `preferred_providers` is the user's `alternatives.toml` content
    /// (§16.1): a map from virtual name (the full `"virtual/<name>"`
    /// string; the short name is accepted as a fallback) to the
    /// preferred provider package. It is a *bias* only — see
    /// `Materializer::materialize_virtual` for how it orders synthetic
    /// alternatives, and why an unsatisfiable preference is ignored
    /// rather than treated as an error.
    ///
    /// Errors if a `Dependency::Virtual` references a virtual with zero
    /// registered providers — an unsatisfiable reference the solver
    /// would otherwise only discover indirectly, as an opaque "no
    /// solution" deep inside a derivation tree, when the graph itself
    /// already has enough information to name the real problem directly.
    pub fn finalize(self, preferred_providers: &BTreeMap<String, String>) -> PkgResult<ResolvedGraph> {
        let PackageGraph { packages, providers } = self;
        let mut catalog = Catalog::new();
        let mut materializer = Materializer {
            providers: &providers,
            preferred_providers,
            next_synthetic_id: 0,
            synthetic_registrations: Vec::new(),
        };

        for entry in &packages {
            let mut plain_deps = Vec::with_capacity(entry.dependencies.len());
            for dep in &entry.dependencies {
                plain_deps.push(materializer.resolve_dependency(dep)?);
            }
            catalog.add_package_version(entry.name.clone(), entry.version.clone(), plain_deps);
        }

        let mut synthetic_names = HashSet::new();
        for (name, version, deps) in materializer.synthetic_registrations {
            synthetic_names.insert(name.clone());
            catalog.add_package_version(name, version, deps);
        }

        Ok(ResolvedGraph {
            catalog,
            synthetic_names,
        })
    }
}

// ---------------------------------------------------------------------
// Materializer — the translation itself, scoped to one `finalize` call
// ---------------------------------------------------------------------

/// Does the actual OR-group/virtual-to-synthetic-package translation.
/// Kept as its own short-lived struct rather than methods directly on
/// `PackageGraph`, since none of this needs to outlive one `finalize`
/// call, and `next_synthetic_id` in particular would be misleading as
/// persistent `PackageGraph` state — it's a counter scoped to a single
/// translation pass, not something meaningful to inspect afterward.
struct Materializer<'a> {
    providers: &'a HashMap<VirtualName, BTreeSet<String>>,
    preferred_providers: &'a BTreeMap<String, String>,
    next_synthetic_id: u64,
    synthetic_registrations: Vec<(String, Version, Vec<(String, VersionConstraint)>)>,
}

impl<'a> Materializer<'a> {
    /// Resolves one `Dependency` into a single plain (name, constraint)
    /// pair — the shape `pubgrub.rs`'s `Catalog` actually understands —
    /// materializing a synthetic package first if `dep` is a virtual or
    /// an OR-group.
    fn resolve_dependency(&mut self, dep: &Dependency) -> PkgResult<(String, VersionConstraint)> {
        match dep {
            Dependency::Package { name, constraint } => Ok((name.clone(), constraint.clone())),
            Dependency::Virtual(v) => {
                let synthetic = self.materialize_virtual(v)?;
                Ok((synthetic, VersionConstraint::Any))
            }
            Dependency::AnyOf(alternatives) => {
                let synthetic = self.materialize_or_group(alternatives)?;
                Ok((synthetic, VersionConstraint::Any))
            }
        }
    }

    fn materialize_virtual(&mut self, virt: &VirtualName) -> PkgResult<String> {
        let provider_names = self.providers.get(virt).cloned().unwrap_or_default();
        if provider_names.is_empty() {
            return Err(PkgError::Resolution(format!(
                "no provider registered for {virt}"
            )));
        }
        // Deduplicated by name (a `BTreeSet`, both for the dedup and for
        // deterministic ordering): "any version of this provider" per
        // distinct provider package, not one alternative per registered
        // version. A provider registered at several versions shouldn't
        // multiply into that many near-duplicate OR-branches — the
        // provider's own version gets decided by the solver like any
        // other package, unconstrained by the virtual specifically.
        let mut provider_names: Vec<String> = provider_names.into_iter().collect();

        // Preferred-provider bias (§16.1, `alternatives.toml`). The
        // synthetic encoding gives alternative N synthetic version N,
        // and `OfflineDependencyProvider` always tries the NEWEST
        // version first — so the alternative placed last is the one the
        // solver picks whenever it's satisfiable. Moving the preferred
        // provider to the end is therefore exactly "prefer this
        // provider, but backtrack past it if it can't work" — a bias,
        // expressed through the solver's ordinary search rather than a
        // special case.
        //
        // A preference naming a package that isn't a registered provider
        // of this virtual is ignored: the bias must never turn a
        // resolvable graph into an unresolvable one.
        let preferred = self
            .preferred_providers
            .get(&virt.to_string())
            .or_else(|| self.preferred_providers.get(virt.short_name()));
        if let Some(preferred) = preferred {
            if let Some(position) = provider_names.iter().position(|name| name == preferred) {
                let name = provider_names.remove(position);
                provider_names.push(name);
            }
        }

        let alternatives = provider_names
            .into_iter()
            .map(|name| (name, VersionConstraint::Any))
            .collect();
        Ok(self.materialize_synthetic(alternatives))
    }

    fn materialize_or_group(&mut self, alternatives: &[Dependency]) -> PkgResult<String> {
        // Recurses through `resolve_dependency` for each alternative
        // rather than duplicating its match arms here — an alternative
        // can itself be a virtual or a nested `AnyOf` (§5.2's `Dependency`
        // is recursive on exactly this point, `virtual_pkg.rs`), and
        // this is the same dispatch either way.
        let mut resolved = Vec::with_capacity(alternatives.len());
        for alt in alternatives {
            resolved.push(self.resolve_dependency(alt)?);
        }
        Ok(self.materialize_synthetic(resolved))
    }

    /// Registers a fresh synthetic package with one version per
    /// alternative, and returns its name for the caller to depend on
    /// with `VersionConstraint::Any`.
    fn materialize_synthetic(&mut self, alternatives: Vec<(String, VersionConstraint)>) -> String {
        // The "$or:" prefix has no correctness role — filtering synthetic
        // packages out of a solution (see `resolve`, below) is done via
        // the tracked `synthetic_names` set, not by pattern-matching this
        // name. It exists purely so a human staring at a raw dependency
        // dump mid-debugging can tell at a glance that a given package
        // name is solver bookkeeping, not something a manifest declared.
        let synthetic_name = format!("$or:{}", self.next_synthetic_id);
        self.next_synthetic_id += 1;

        for (i, alternative) in alternatives.into_iter().enumerate() {
            // A meaningless ordinal, never surfaced to a user — its only
            // job is to be a distinct `Version` per alternative so
            // PubGrub can pick exactly one.
            let synthetic_version: Version = format!("0.0.{i}")
                .parse()
                .expect("synthetic version string is always valid");
            self.synthetic_registrations
                .push((synthetic_name.clone(), synthetic_version, vec![alternative]));
        }

        synthetic_name
    }
}

// ---------------------------------------------------------------------
// ResolvedGraph
// ---------------------------------------------------------------------

/// The output of `PackageGraph::finalize`: a `pubgrub`-ready `Catalog`,
/// paired with the set of synthetic package names introduced while
/// building it.
///
/// Tracked explicitly rather than recognized later by the "$or:" naming
/// convention, so that filtering them back out of a solution (`resolve`,
/// below) is based on what was actually generated during this specific
/// translation — not a pattern that merely happens not to collide with
/// any real package name today.
#[derive(Debug)]
pub struct ResolvedGraph {
    catalog: Catalog,
    synthetic_names: HashSet<String>,
}

impl ResolvedGraph {
    /// The materialized catalog, for callers that need direct access to
    /// the `pubgrub`-ready view (e.g. handing it to `explain.rs`).
    pub fn catalog(&self) -> &Catalog {
        &self.catalog
    }

    /// Whether `name` is one of the synthetic OR-group/virtual
    /// bookkeeping packages introduced by `finalize` — i.e. something a
    /// solution can contain that no manifest ever declared.
    pub fn is_synthetic(&self, name: &str) -> bool {
        self.synthetic_names.contains(name)
    }

    /// The full resolution entry point: resolves `root_name`/
    /// `root_version` against the materialized catalog with the active
    /// `pins`, the effective profile constraints, and installed-version
    /// bias (see `pubgrub.rs`'s `resolve_with_constraints` for the
    /// semantics of all three), then strips the synthetic bookkeeping
    /// packages out of the result — callers only ever see real
    /// packages, never `finalize`'s internal machinery.
    ///
    /// On failure the returned [`ResolveError`] carries pubgrub's
    /// derivation tree, so `explain.rs`'s `explain_from_tree` can
    /// render it without re-solving.
    ///
    /// Pins and profile constraints apply to *real* packages only, in
    /// practice: synthetic names are generated inside `finalize` and
    /// can't be named in a pin file or profile — which is exactly what
    /// we want, since constraining a synthetic version would be
    /// constraining solver bookkeeping, not a package.
    pub fn resolve_with_constraints(
        &self,
        root_name: &str,
        root_version: Version,
        pins: &PinSet,
        profile_constraints: &EffectiveConstraints,
        installed: &HashMap<String, Version>,
    ) -> Result<HashMap<String, Version>, ResolveError> {
        let raw = crate::resolve_with_constraints(
            &self.catalog,
            root_name,
            root_version,
            pins,
            profile_constraints,
            installed,
        )?;
        Ok(raw
            .into_iter()
            .filter(|(name, _)| !self.synthetic_names.contains(name))
            .collect())
    }
}

/// Resolves `root_name`/`root_version` against a finalized graph with no
/// pins, no profile constraints, and no installed-version bias, then
/// strips the synthetic OR-group/virtual bookkeeping packages out of the
/// result — callers only ever see real packages, never `finalize`'s
/// internal machinery.
///
/// Thin wrapper over [`ResolvedGraph::resolve_with_constraints`],
/// flattening the error to `PkgError::Resolution` (§16.1: exit code 1).
/// Use the method when the derivation tree is needed — see
/// `explain.rs`'s `explain_from_tree`.
pub fn resolve(
    graph: &ResolvedGraph,
    root_name: &str,
    root_version: Version,
) -> PkgResult<HashMap<String, Version>> {
    graph
        .resolve_with_constraints(
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

    fn v(s: &str) -> Version {
        s.parse().unwrap()
    }

    fn kernel_virtual() -> VirtualName {
        VirtualName::new("virtual/kernel").unwrap()
    }

    fn no_preferences() -> BTreeMap<String, String> {
        BTreeMap::new()
    }

    #[test]
    fn plain_dependency_is_unaffected_by_graph_machinery() {
        let mut graph = PackageGraph::new();
        graph.add_package("root", v("1.0.0"), &[], vec![Dependency::any_version("openssl")]);
        graph.add_package("openssl", v("3.0.13"), &[], vec![]);

        let resolved = graph.finalize(&BTreeMap::new()).unwrap();
        let solution = resolve(&resolved, "root", v("1.0.0")).unwrap();

        assert_eq!(solution.get("openssl"), Some(&v("3.0.13")));
        assert_eq!(solution.get("root"), Some(&v("1.0.0")));
        assert!(
            solution.keys().all(|k| !k.starts_with('$')),
            "no synthetic packages should leak into the result: {solution:?}"
        );
    }

    #[test]
    fn virtual_resolves_to_one_registered_provider() {
        let mut graph = PackageGraph::new();
        graph.add_package("linux-lts", v("6.6.0"), &[kernel_virtual()], vec![]);
        graph.add_package("linux-hardened", v("6.6.0"), &[kernel_virtual()], vec![]);
        graph.add_package(
            "root",
            v("1.0.0"),
            &[],
            vec![Dependency::Virtual(kernel_virtual())],
        );

        let resolved = graph.finalize(&BTreeMap::new()).unwrap();
        let solution = resolve(&resolved, "root", v("1.0.0")).unwrap();

        let has_lts = solution.contains_key("linux-lts");
        let has_hardened = solution.contains_key("linux-hardened");
        assert!(
            has_lts ^ has_hardened,
            "exactly one provider should be chosen, got: {solution:?}"
        );
        assert!(
            solution.keys().all(|k| !k.starts_with('$')),
            "no synthetic package should be visible in the result: {solution:?}"
        );
    }

    #[test]
    fn virtual_with_no_provider_is_a_named_error() {
        let mut graph = PackageGraph::new();
        graph.add_package(
            "root",
            v("1.0.0"),
            &[],
            vec![Dependency::Virtual(kernel_virtual())],
        );

        let err = graph.finalize(&BTreeMap::new()).unwrap_err();
        match err {
            PkgError::Resolution(msg) => assert!(msg.contains("virtual/kernel")),
            other => panic!("expected PkgError::Resolution, got {other:?}"),
        }
    }

    #[test]
    fn or_group_resolves_to_one_alternative() {
        let mut graph = PackageGraph::new();
        graph.add_package("cronie", v("1.0.0"), &[], vec![]);
        graph.add_package("fcron", v("1.0.0"), &[], vec![]);
        graph.add_package(
            "root",
            v("1.0.0"),
            &[],
            vec![Dependency::AnyOf(vec![
                Dependency::any_version("cronie"),
                Dependency::any_version("fcron"),
            ])],
        );

        let resolved = graph.finalize(&BTreeMap::new()).unwrap();
        let solution = resolve(&resolved, "root", v("1.0.0")).unwrap();

        let has_cronie = solution.contains_key("cronie");
        let has_fcron = solution.contains_key("fcron");
        assert!(has_cronie ^ has_fcron, "exactly one alternative should be chosen");
    }

    #[test]
    fn or_group_skips_an_alternative_no_version_can_satisfy() {
        // cronie is only registered at 1.0.0, but the OR-group's cronie
        // branch requires >=2.0.0 — unsatisfiable. A correct resolution
        // has to pick fcron instead, not choose blindly.
        let mut graph = PackageGraph::new();
        graph.add_package("cronie", v("1.0.0"), &[], vec![]);
        graph.add_package("fcron", v("1.0.0"), &[], vec![]);
        graph.add_package(
            "root",
            v("1.0.0"),
            &[],
            vec![Dependency::AnyOf(vec![
                Dependency::at_least("cronie", v("2.0.0")),
                Dependency::any_version("fcron"),
            ])],
        );

        let resolved = graph.finalize(&BTreeMap::new()).unwrap();
        let solution = resolve(&resolved, "root", v("1.0.0")).unwrap();

        assert!(!solution.contains_key("cronie"));
        assert_eq!(solution.get("fcron"), Some(&v("1.0.0")));
    }

    #[test]
    fn nested_or_group_resolves_to_one_leaf() {
        let mut graph = PackageGraph::new();
        graph.add_package("a", v("1.0.0"), &[], vec![]);
        graph.add_package("b", v("1.0.0"), &[], vec![]);
        graph.add_package("c", v("1.0.0"), &[], vec![]);
        graph.add_package(
            "root",
            v("1.0.0"),
            &[],
            vec![Dependency::AnyOf(vec![
                Dependency::any_version("a"),
                Dependency::AnyOf(vec![
                    Dependency::any_version("b"),
                    Dependency::any_version("c"),
                ]),
            ])],
        );

        let resolved = graph.finalize(&BTreeMap::new()).unwrap();
        let solution = resolve(&resolved, "root", v("1.0.0")).unwrap();

        let chosen: Vec<&str> = ["a", "b", "c"]
            .into_iter()
            .filter(|name| solution.contains_key(*name))
            .collect();
        assert_eq!(chosen.len(), 1, "exactly one leaf should be chosen, got {chosen:?}");
        assert!(
            solution.keys().all(|k| !k.starts_with('$')),
            "no synthetic package should be visible: {solution:?}"
        );
    }

    // -- v2: preferred-provider bias --------------------------------------

    #[test]
    fn without_a_preference_the_lexically_last_provider_wins() {
        // Baseline for the bias tests: alternatives are sorted by name
        // and the solver's newest-synthetic-version-first pick lands on
        // the last one. Deterministic, so the preference tests below
        // can prove the bias actually changes the outcome.
        let mut graph = PackageGraph::new();
        graph.add_package("linux-hardened", v("6.6.0"), &[kernel_virtual()], vec![]);
        graph.add_package("linux-lts", v("6.6.0"), &[kernel_virtual()], vec![]);
        graph.add_package(
            "root",
            v("1.0.0"),
            &[],
            vec![Dependency::Virtual(kernel_virtual())],
        );

        let resolved = graph.finalize(&no_preferences()).unwrap();
        let solution = resolve(&resolved, "root", v("1.0.0")).unwrap();

        assert_eq!(solution.get("linux-lts"), Some(&v("6.6.0")));
        assert!(!solution.contains_key("linux-hardened"));
    }

    #[test]
    fn preferred_provider_is_chosen_when_it_is_a_registered_provider() {
        let mut graph = PackageGraph::new();
        graph.add_package("linux-hardened", v("6.6.0"), &[kernel_virtual()], vec![]);
        graph.add_package("linux-lts", v("6.6.0"), &[kernel_virtual()], vec![]);
        graph.add_package(
            "root",
            v("1.0.0"),
            &[],
            vec![Dependency::Virtual(kernel_virtual())],
        );

        let mut preferred = BTreeMap::new();
        preferred.insert(
            "virtual/kernel".to_string(),
            "linux-hardened".to_string(),
        );

        let resolved = graph.finalize(&preferred).unwrap();
        let solution = resolve(&resolved, "root", v("1.0.0")).unwrap();

        assert_eq!(solution.get("linux-hardened"), Some(&v("6.6.0")));
        assert!(!solution.contains_key("linux-lts"));
    }

    #[test]
    fn preferred_provider_lookup_accepts_the_short_name_as_fallback() {
        let mut graph = PackageGraph::new();
        graph.add_package("linux-hardened", v("6.6.0"), &[kernel_virtual()], vec![]);
        graph.add_package("linux-lts", v("6.6.0"), &[kernel_virtual()], vec![]);
        graph.add_package(
            "root",
            v("1.0.0"),
            &[],
            vec![Dependency::Virtual(kernel_virtual())],
        );

        let mut preferred = BTreeMap::new();
        preferred.insert("kernel".to_string(), "linux-hardened".to_string());

        let resolved = graph.finalize(&preferred).unwrap();
        let solution = resolve(&resolved, "root", v("1.0.0")).unwrap();

        assert_eq!(solution.get("linux-hardened"), Some(&v("6.6.0")));
    }

    #[test]
    fn preference_for_an_unregistered_provider_is_ignored_not_an_error() {
        // The bias must never make a resolvable graph unresolvable.
        let mut graph = PackageGraph::new();
        graph.add_package("linux-lts", v("6.6.0"), &[kernel_virtual()], vec![]);
        graph.add_package(
            "root",
            v("1.0.0"),
            &[],
            vec![Dependency::Virtual(kernel_virtual())],
        );

        let mut preferred = BTreeMap::new();
        preferred.insert(
            "virtual/kernel".to_string(),
            "linux-zen".to_string(), // not registered as a provider
        );

        let resolved = graph.finalize(&preferred).unwrap();
        let solution = resolve(&resolved, "root", v("1.0.0")).unwrap();

        assert_eq!(solution.get("linux-lts"), Some(&v("6.6.0")));
    }

    #[test]
    fn preference_is_a_bias_the_solver_can_backtrack_past() {
        // The preferred provider is unsatisfiable on its own terms (it
        // depends on a package that isn't in the catalog), so picking
        // its synthetic branch dead-ends — the solver must backtrack
        // and choose the other provider rather than fail.
        let mut graph = PackageGraph::new();
        graph.add_package(
            "linux-hardened",
            v("6.6.0"),
            &[kernel_virtual()],
            vec![Dependency::any_version("missing-lib")],
        );
        graph.add_package("linux-lts", v("6.6.0"), &[kernel_virtual()], vec![]);
        graph.add_package(
            "root",
            v("1.0.0"),
            &[],
            vec![Dependency::Virtual(kernel_virtual())],
        );

        let mut preferred = BTreeMap::new();
        preferred.insert(
            "virtual/kernel".to_string(),
            "linux-hardened".to_string(),
        );

        let resolved = graph.finalize(&preferred).unwrap();
        let solution = resolve(&resolved, "root", v("1.0.0")).unwrap();

        assert_eq!(solution.get("linux-lts"), Some(&v("6.6.0")));
        assert!(!solution.contains_key("linux-hardened"));
    }

    // -- v2: constrained resolution through the graph ---------------------

    #[test]
    fn resolve_with_constraints_applies_pins_across_virtual_materialization() {
        // Two versions of the same provider; an exact pin must hold the
        // solver to the older one even though the virtual itself is
        // materialized as a synthetic package.
        let mut graph = PackageGraph::new();
        graph.add_package("linux-lts", v("6.6.0"), &[kernel_virtual()], vec![]);
        graph.add_package("linux-lts", v("6.7.0"), &[kernel_virtual()], vec![]);
        graph.add_package(
            "root",
            v("1.0.0"),
            &[],
            vec![Dependency::Virtual(kernel_virtual())],
        );

        let resolved = graph.finalize(&no_preferences()).unwrap();

        let mut pins = PinSet::new();
        pins.insert(Pin {
            package: "linux-lts".to_string(),
            version: v("6.6.0"),
            mode: PinMode::Exact,
            pinned_abi_tag: None,
        });

        let solution = resolved
            .resolve_with_constraints(
                "root",
                v("1.0.0"),
                &pins,
                &EffectiveConstraints::new(),
                &HashMap::new(),
            )
            .unwrap();
        assert_eq!(solution.get("linux-lts"), Some(&v("6.6.0")));
        assert!(
            solution.keys().all(|k| !k.starts_with('$')),
            "no synthetic package should be visible: {solution:?}"
        );
    }

    #[test]
    fn resolve_with_constraints_applies_profile_constraints() {
        let mut graph = PackageGraph::new();
        graph.add_package("mesa", v("23.3"), &[], vec![]);
        graph.add_package("mesa", v("24.0"), &[], vec![]);
        graph.add_package(
            "root",
            v("1.0.0"),
            &[],
            vec![Dependency::any_version("mesa")],
        );

        let resolved = graph.finalize(&no_preferences()).unwrap();

        let mut constraints = EffectiveConstraints::new();
        constraints.insert("mesa".to_string(), VersionConstraint::Exact(v("24.0")));

        let solution = resolved
            .resolve_with_constraints("root", v("1.0.0"), &PinSet::new(), &constraints, &HashMap::new())
            .unwrap();
        assert_eq!(solution.get("mesa"), Some(&v("24.0")));
    }

    #[test]
    fn resolve_with_constraints_returns_the_derivation_tree_on_failure() {
        // The virtual resolves fine, but the root's exact constraint on
        // the provider conflicts — the failure must surface as a
        // ResolveError::NoSolution carrying the tree, not a flattened
        // PkgError, so explain.rs can render it.
        let mut graph = PackageGraph::new();
        graph.add_package("linux-lts", v("6.6.0"), &[kernel_virtual()], vec![]);
        graph.add_package(
            "root",
            v("1.0.0"),
            &[],
            vec![
                Dependency::Virtual(kernel_virtual()),
                Dependency::Package {
                    name: "linux-lts".to_string(),
                    constraint: VersionConstraint::Exact(v("9.9.9")),
                },
            ],
        );

        let resolved = graph.finalize(&no_preferences()).unwrap();
        let result = resolved.resolve_with_constraints(
            "root",
            v("1.0.0"),
            &PinSet::new(),
            &EffectiveConstraints::new(),
            &HashMap::new(),
        );
        assert!(
            matches!(result, Err(ResolveError::NoSolution(_))),
            "expected NoSolution with a derivation tree, got {result:?}"
        );
    }

    #[test]
    fn is_synthetic_marks_only_finalize_generated_names() {
        let mut graph = PackageGraph::new();
        graph.add_package("linux-lts", v("6.6.0"), &[kernel_virtual()], vec![]);
        graph.add_package(
            "root",
            v("1.0.0"),
            &[],
            vec![Dependency::Virtual(kernel_virtual())],
        );

        let resolved = graph.finalize(&no_preferences()).unwrap();
        assert!(resolved.is_synthetic("$or:0"));
        assert!(!resolved.is_synthetic("linux-lts"));
        assert!(!resolved.is_synthetic("root"));
    }
}
