//! `pkg why` / `pkg explain` machinery (§5.5).
//!
//! ## v2 changes
//!
//! - **MINOR** — no more double solve: `explain_resolution_failure` used
//!   to re-run `pubgrub::resolve` itself to recover the derivation tree
//!   that `resolve_dependencies` had just thrown away. The resolver's
//!   failure path (`pubgrub.rs`'s `ResolveError::NoSolution`) now
//!   carries that tree, and [`explain_from_tree`] renders it directly.
//!   A thin [`explain_resolution_failure`] convenience wrapper remains
//!   for callers that only have a catalog in hand.
//! - **MINOR** — the chain walk no longer clones the whole `Vec` of
//!   reverse dependents per step (`reverse_deps.get(current).cloned()`);
//!   it iterates by reference.
//! - **MINOR (docs)** — the module doc no longer claims this file is
//!   reused by `pkg remove`: `pkg remove` (§16.6) needs a package's
//!   *direct* dependents (one `reverse_dependents` lookup against
//!   `pkg-db`), not the full transitive chains walked here. Full chains
//!   answer "why is this installed at all", which is `pkg why`'s
//!   question, not `pkg remove`'s refusal check.
//! - **MINOR (v2.1)** — extern-crate imports are now absolute
//!   (`::pubgrub::...`): the crate root's `pub mod pubgrub;` shadows
//!   the extern crate in Rust 2018 uniform paths. No behavior change.
//!
//! Two genuinely different jobs live here, both about explaining a
//! resolver decision to a person rather than just reporting it:
//!
//! - **Why did resolution fail?** Renders the derivation tree carried by
//!   the resolver's `ResolveError::NoSolution` with `pubgrub`'s own
//!   `DefaultStringReporter` rather than reinventing tree-formatting.
//! - **Why is this package installed?** Walks the reverse-dependency
//!   graph from a target package up to whatever explicitly-installed
//!   package pulled it in.
//!
//! Note what this file does *not* do: reach into `pkg-db` for real
//! reverse-dependency data. `pkg-resolver` only depends on `pkg-core`
//! (see the workspace's own dependency graph) — it has no path to a
//! database at all. The chain-walking logic here is generic over
//! whatever reverse-dependency lookup the caller supplies, so it can be
//! tested with a plain in-memory map and later driven by `pkg-db`'s
//! `reverse_dependents` query without this file needing to change.

use std::collections::{HashMap, HashSet};

use pkg_core::{PkgError, Version};
// `::pubgrub`, not bare `pubgrub`: the crate root declares a module
// named `pubgrub`, which shadows the extern crate in Rust 2018 uniform
// paths (see the note in `pubgrub.rs`).
use ::pubgrub::{DefaultStringReporter, DerivationTree, Reporter};

use crate::{Catalog, PkgVersionSet, ResolveError};

// ---------------------------------------------------------------------
// Resolution failure explanation
// ---------------------------------------------------------------------

/// Renders a resolution failure's derivation tree as human-readable text
/// using `pubgrub`'s own reporter.
///
/// The tree comes from the resolver itself — `resolve_with_constraints`
/// returns it inside `ResolveError::NoSolution` — so explaining a
/// failure never requires solving a second time. The returned
/// `PkgError::Resolution` (§16.1: exit code 1) is the user-facing error
/// `pkg` prints.
pub fn explain_from_tree(tree: &DerivationTree<String, PkgVersionSet, String>) -> PkgError {
    PkgError::Resolution(DefaultStringReporter::report(tree))
}

/// Convenience wrapper: explains why resolving `root_name`/
/// `root_version` against `catalog` fails, or returns `None` if it
/// actually succeeds — since there's nothing to explain in that case.
///
/// This re-runs the solve (unconstrained — no pins/profiles/installed
/// bias, the same subset `resolve_dependencies` always used), so it's
/// only for callers that don't already have the failure in hand; a
/// caller that just ran `resolve_with_constraints` should use
/// [`explain_from_tree`] on the returned `ResolveError` instead.
pub fn explain_resolution_failure(
    catalog: &Catalog,
    root_name: &str,
    root_version: Version,
) -> Option<PkgError> {
    match crate::resolve_with_constraints(
        catalog,
        root_name,
        root_version,
        &crate::PinSet::new(),
        &crate::EffectiveConstraints::new(),
        &HashMap::new(),
    ) {
        Ok(_) => None,
        Err(ResolveError::NoSolution(tree)) => Some(explain_from_tree(&tree)),
        Err(ResolveError::Other(message)) => {
            // `OfflineDependencyProvider`'s `Err` associated type is
            // `Infallible` (confirmed directly against pubgrub 0.3.0's
            // source) — `ErrorRetrievingDependencies`,
            // `ErrorChoosingVersion`, and `ErrorInShouldCancel` all wrap
            // a `DP::Err`, so none of them can actually be constructed
            // for a `Catalog`, which is always backed by one. This arm
            // exists because the variant set has to be handled in full —
            // not because it's reachable here.
            Some(PkgError::Resolution(message))
        }
    }
}

// ---------------------------------------------------------------------
// Why-installed chain walking
// ---------------------------------------------------------------------

/// One complete path from a target package up to an explicitly-installed
/// package that (transitively) required it, e.g. `["openssl", "curl",
/// "wget"]` meaning openssl is pulled in by curl, which was itself
/// explicitly installed as wget's dependency... down to whichever
/// package in the chain is actually marked explicit.
///
/// `chain[0]` is always the original target; the last element is either
/// an explicitly-installed package or a dead end (nothing depends on it,
/// and it isn't explicit either — an orphan `pkg autoremove`, §16.6,
/// would be the one to actually deal with, not this function).
pub type DependencyChain = Vec<String>;

/// Walks every path from `target` up through its reverse dependents to
/// wherever each one terminates.
///
/// `reverse_deps` maps a package name to the names of packages that
/// directly depend on it — the same shape `pkg-db`'s `reverse_dependents`
/// query answers, just supplied here as plain data instead of a live
/// query. `is_explicit` marks which packages were direct install targets
/// (`installed_explicitly`, §9.2) rather than pulled in as dependencies.
///
/// Always returns at least one chain (of length 1, if `target` has no
/// reverse dependents at all) — never an empty result — so callers don't
/// need to special-case "nothing to explain" separately from "here's
/// what to show."
pub fn explain_why_installed(
    target: &str,
    reverse_deps: &HashMap<String, Vec<String>>,
    is_explicit: &HashMap<String, bool>,
) -> Vec<DependencyChain> {
    let mut chains = Vec::new();
    let mut path = vec![target.to_string()];
    let mut visited = HashSet::new();
    walk(target, reverse_deps, is_explicit, &mut path, &mut chains, &mut visited);
    chains
}

fn walk(
    current: &str,
    reverse_deps: &HashMap<String, Vec<String>>,
    is_explicit: &HashMap<String, bool>,
    path: &mut Vec<String>,
    chains: &mut Vec<DependencyChain>,
    visited: &mut HashSet<String>,
) {
    // Path-based cycle detection: inserted on entry, removed on exit
    // (below), so the same package can still appear in a *different*
    // chain later — this only rules out revisiting a package within the
    // current path. A well-formed resolved dependency graph shouldn't
    // have cycles at all, but a corrupted database could, and looping
    // forever on bad data would be a worse failure than an incomplete
    // explanation.
    if !visited.insert(current.to_string()) {
        chains.push(path.clone());
        return;
    }

    let explicit = is_explicit.get(current).copied().unwrap_or(false);
    // Iterate the dependents by reference — cloning the whole Vec at
    // every step of the walk was pure waste.
    let dependents = reverse_deps.get(current);

    if explicit || dependents.map_or(true, |deps| deps.is_empty()) {
        chains.push(path.clone());
    } else if let Some(dependents) = dependents {
        for dependent in dependents {
            path.push(dependent.clone());
            walk(dependent, reverse_deps, is_explicit, path, chains, visited);
            path.pop();
        }
    }

    visited.remove(current);
}

/// Renders `explain_why_installed`'s output as readable text, one line
/// per chain, e.g.:
///
/// ```text
/// openssl <- curl (explicitly installed)
/// openssl <- some-lib <- wget (explicitly installed)
/// ```
pub fn format_chains(chains: &[DependencyChain], is_explicit: &HashMap<String, bool>) -> String {
    chains
        .iter()
        .map(|chain| format_one_chain(chain, is_explicit))
        .collect::<Vec<_>>()
        .join("\n")
}

fn format_one_chain(chain: &DependencyChain, is_explicit: &HashMap<String, bool>) -> String {
    let root_is_explicit = chain
        .last()
        .map(|p| is_explicit.get(p).copied().unwrap_or(false))
        .unwrap_or(false);

    if chain.len() == 1 {
        return if root_is_explicit {
            format!("{} is explicitly installed.", chain[0])
        } else {
            format!(
                "{} is not required by any installed package and is not explicitly installed (orphaned).",
                chain[0]
            )
        };
    }

    let mut line = chain.join(" <- ");
    if root_is_explicit {
        line.push_str(" (explicitly installed)");
    }
    line
}

// ---------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Catalog;
    use pkg_core::VersionConstraint;

    fn v(s: &str) -> Version {
        s.parse().unwrap()
    }

    fn conflict_catalog() -> Catalog {
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
        catalog
    }

    // --- resolution failure explanation ---

    #[test]
    fn explains_a_genuine_conflict_from_the_returned_tree() {
        let catalog = conflict_catalog();

        // One solve; the explanation comes from the tree the failure
        // carried, with no second solve.
        let error = crate::resolve_with_constraints(
            &catalog,
            "root",
            v("1.0.0"),
            &crate::PinSet::new(),
            &crate::EffectiveConstraints::new(),
            &HashMap::new(),
        )
        .expect_err("this catalog is genuinely unsatisfiable");
        let tree = error.derivation_tree().expect("a NoSolution carries its tree");

        let PkgError::Resolution(explanation) = explain_from_tree(tree) else {
            panic!("explain_from_tree should produce PkgError::Resolution");
        };
        assert!(explanation.contains("root"));
        assert!(!explanation.is_empty());
    }

    #[test]
    fn wrapper_explains_a_genuine_conflict() {
        let catalog = conflict_catalog();
        let explanation = explain_resolution_failure(&catalog, "root", v("1.0.0"));
        let explanation = explanation.expect("this catalog is genuinely unsatisfiable");
        assert!(matches!(explanation, PkgError::Resolution(_)));
    }

    #[test]
    fn returns_none_when_resolution_succeeds() {
        let mut catalog = Catalog::new();
        catalog.add_package_version("root", v("1.0.0"), []);

        assert_eq!(explain_resolution_failure(&catalog, "root", v("1.0.0")).map(|e| e.to_string()), None);
    }

    // --- why-installed chain walking ---

    #[test]
    fn single_level_chain_to_explicit_root() {
        let mut reverse_deps = HashMap::new();
        reverse_deps.insert("openssl".to_string(), vec!["curl".to_string()]);
        let mut is_explicit = HashMap::new();
        is_explicit.insert("curl".to_string(), true);

        let chains = explain_why_installed("openssl", &reverse_deps, &is_explicit);
        assert_eq!(chains, vec![vec!["openssl".to_string(), "curl".to_string()]]);
    }

    #[test]
    fn multi_level_chain_through_transitively_installed_packages() {
        let mut reverse_deps = HashMap::new();
        reverse_deps.insert("openssl".to_string(), vec!["some-lib".to_string()]);
        reverse_deps.insert("some-lib".to_string(), vec!["wget".to_string()]);
        let mut is_explicit = HashMap::new();
        is_explicit.insert("wget".to_string(), true);

        let chains = explain_why_installed("openssl", &reverse_deps, &is_explicit);
        assert_eq!(
            chains,
            vec![vec![
                "openssl".to_string(),
                "some-lib".to_string(),
                "wget".to_string()
            ]]
        );
    }

    #[test]
    fn multiple_independent_chains_from_shared_dependency() {
        let mut reverse_deps = HashMap::new();
        reverse_deps.insert(
            "openssl".to_string(),
            vec!["curl".to_string(), "wget".to_string()],
        );
        let mut is_explicit = HashMap::new();
        is_explicit.insert("curl".to_string(), true);
        is_explicit.insert("wget".to_string(), true);

        let mut chains = explain_why_installed("openssl", &reverse_deps, &is_explicit);
        chains.sort();
        assert_eq!(
            chains,
            vec![
                vec!["openssl".to_string(), "curl".to_string()],
                vec!["openssl".to_string(), "wget".to_string()],
            ]
        );
    }

    #[test]
    fn orphaned_package_produces_single_element_chain() {
        let reverse_deps = HashMap::new();
        let is_explicit = HashMap::new();

        let chains = explain_why_installed("old-lib", &reverse_deps, &is_explicit);
        assert_eq!(chains, vec![vec!["old-lib".to_string()]]);
    }

    #[test]
    fn explicit_package_with_no_dependents_is_a_single_element_chain() {
        let reverse_deps = HashMap::new();
        let mut is_explicit = HashMap::new();
        is_explicit.insert("htop".to_string(), true);

        let chains = explain_why_installed("htop", &reverse_deps, &is_explicit);
        assert_eq!(chains, vec![vec!["htop".to_string()]]);
    }

    #[test]
    fn cycle_terminates_instead_of_recursing_forever() {
        // Not a well-formed graph — a depends on b's presence and vice
        // versa — but the walker should terminate rather than hang.
        let mut reverse_deps = HashMap::new();
        reverse_deps.insert("a".to_string(), vec!["b".to_string()]);
        reverse_deps.insert("b".to_string(), vec!["a".to_string()]);
        let is_explicit = HashMap::new();

        let chains = explain_why_installed("a", &reverse_deps, &is_explicit);
        assert!(!chains.is_empty(), "should still produce something, not hang");
    }

    #[test]
    fn format_chains_annotates_explicit_root() {
        let chains = vec![vec!["openssl".to_string(), "curl".to_string()]];
        let mut is_explicit = HashMap::new();
        is_explicit.insert("curl".to_string(), true);

        let text = format_chains(&chains, &is_explicit);
        assert_eq!(text, "openssl <- curl (explicitly installed)");
    }

    #[test]
    fn format_chains_describes_orphan_state() {
        let chains = vec![vec!["old-lib".to_string()]];
        let is_explicit = HashMap::new();

        let text = format_chains(&chains, &is_explicit);
        assert!(text.contains("orphaned"));
    }
}
