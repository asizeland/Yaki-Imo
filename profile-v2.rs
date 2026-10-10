//! Resolution profiles (§5.4, §10.3) and profile-vs-pin precedence.
//!
//! ## v2 changes
//!
//! - **MINOR** — `ProfileSet::from_toml_str` now errors on duplicate
//!   `[[profile]] name` entries instead of last-write-wins silently
//!   discarding one of them.
//! - **MINOR (docs)** — the pin-vs-profile precedence rule is settled,
//!   and the module doc now says so: an explicit pin on a package drops
//!   that package's profile constraint entirely (explicit over implicit,
//!   spec §2). See below.
//!
//! A `Profile` is a named, repository-curated bundle of per-package
//! version constraints — `"gaming-stable"` pinning `kernel`, `mesa`, and
//! `nvidia-driver` together, for instance. Fetched read-only from the
//! repository index (`profiles.toml`, alongside `packages.json` and
//! `security.json`, §10.6) — unlike `pins.toml`, nothing here ever
//! writes a profile back out, which is why `ProfileSet` only parses and
//! `PinSet` (§5.6) both parses and serializes.
//!
//! Precedence — settled rule: **an explicit pin on a package drops that
//! package's profile constraint entirely (explicit over implicit, spec
//! §2).** A profile is fundamentally another *automatic*,
//! repository-curated constraint source, the same category
//! auto-promotion is in — and "sane defaults, explicit override" (§2)
//! already says which kind of source wins when they conflict with
//! something the person did on purpose. The constraint is dropped, not
//! merged or intersected with the pin — see `effective_constraints` for
//! why dropping, specifically, is the right operation rather than trying
//! to combine the two.

use std::collections::BTreeMap;

use pkg_core::{PkgError, PkgResult, Version, VersionConstraint};
use serde::Deserialize;

use crate::PinSet;

// ---------------------------------------------------------------------
// Profile
// ---------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Profile {
    pub name: String,
    pub constraints: BTreeMap<String, VersionConstraint>,
}

/// The profile's constraints that should actually apply, given the
/// active pin set.
///
/// Dropped, not merged: a profile constraint and a pin aren't
/// necessarily expressible as one combined constraint at all. `Pin`
/// (§5.6) can enforce `abi-track` mode — same release core, or a
/// matching ABI tag — which has no representation as a single
/// `VersionConstraint`; even with the new upper-bound-capable `Range`
/// variant, none of `Any`/`Exact`/`AtLeast`/`Range` can express "this
/// version or a later patch of it, but not the next major line." Trying
/// to intersect a profile's `VersionConstraint` with whatever the pin
/// implies would either lose the pin's real semantics or produce a
/// constraint neither the profile nor the pin actually asked for.
/// Dropping the profile's entry entirely and letting the pin's own
/// enforcement mechanism be the sole authority for that package
/// sidesteps needing to reconcile two different representations of
/// "acceptable version" into one.
///
/// How a pin's own enforcement actually reaches the solver is wired up
/// in `pubgrub.rs`: `resolve_with_constraints` filters which versions
/// of a pinned package get registered as candidates at all, via the
/// fixed `Pin::allows` (with a `None` candidate ABI tag — pre-build,
/// the build orchestrator re-verifies the tag post-build).
pub fn effective_constraints(
    profile: &Profile,
    pins: &PinSet,
) -> BTreeMap<String, VersionConstraint> {
    profile
        .constraints
        .iter()
        .filter(|(name, _)| pins.get(name).is_none())
        .map(|(name, constraint)| (name.clone(), constraint.clone()))
        .collect()
}

// ---------------------------------------------------------------------
// ProfileSet
// ---------------------------------------------------------------------

/// Every profile currently known (typically: everything `profiles.toml`
/// last supplied via `pkg sync`, §10.6), keyed by name.
#[derive(Debug, Clone, Default)]
pub struct ProfileSet {
    profiles: BTreeMap<String, Profile>,
}

impl ProfileSet {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get(&self, name: &str) -> Option<&Profile> {
        self.profiles.get(name)
    }

    pub fn insert(&mut self, profile: Profile) -> Option<Profile> {
        self.profiles.insert(profile.name.clone(), profile)
    }

    /// Profile names, for `pkg profile list` (§16.1).
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.profiles.keys().map(String::as_str)
    }

    pub fn from_toml_str(input: &str) -> PkgResult<Self> {
        let raw: RawProfilesFile =
            toml::from_str(input).map_err(|e| PkgError::Parse(e.to_string()))?;
        let mut set = Self::new();
        for raw_profile in raw.profile {
            // Two `[[profile]]` tables with the same name are a repo
            // authoring error, not an override — last-write-wins would
            // silently discard one of them and leave `pkg` applying
            // whichever constraint happened to come second ("explicit
            // over implicit", §2).
            if set.profiles.contains_key(&raw_profile.name) {
                return Err(PkgError::Parse(format!(
                    "duplicate profile entry named {:?}",
                    raw_profile.name
                )));
            }
            let mut constraints = BTreeMap::new();
            for (package, expr) in raw_profile.constraints {
                constraints.insert(package, parse_constraint_value(&expr)?);
            }
            set.insert(Profile {
                name: raw_profile.name,
                constraints,
            });
        }
        Ok(set)
    }
}

/// Parses one constraint value from `profiles.toml` — the right-hand
/// side of e.g. `mesa = ">=24.0"`.
///
/// Unlike `manifest.rs`'s dependency-expression grammar, a bare version
/// with no operator (`"24.0"`) is rejected rather than treated as
/// implicitly meaning `Any` or `Exact` — for a *dependency*, "no operator
/// means no constraint" is a reasonable default; for a curated profile
/// entry whose entire purpose is pinning something to a specific
/// generation, silently guessing which operator was meant would be the
/// wrong kind of implicit (§2).
///
/// The grammar only produces `AtLeast` and `Exact` today; pkg-core's
/// upper-bound-capable `Range` variant has no profile-file syntax yet.
/// That costs nothing here — `Range` constraints constructed elsewhere
/// flow through `effective_constraints` untouched and are mapped to
/// pubgrub `Ranges` by `pubgrub.rs`'s `constraint_to_range`.
fn parse_constraint_value(expr: &str) -> PkgResult<VersionConstraint> {
    let expr = expr.trim();
    if let Some(rest) = expr.strip_prefix(">=") {
        let version: Version = rest
            .trim()
            .parse()
            .map_err(|e: pkg_core::VersionParseError| PkgError::Parse(e.to_string()))?;
        return Ok(VersionConstraint::AtLeast(version));
    }
    if let Some(rest) = expr.strip_prefix('=') {
        let version: Version = rest
            .trim()
            .parse()
            .map_err(|e: pkg_core::VersionParseError| PkgError::Parse(e.to_string()))?;
        return Ok(VersionConstraint::Exact(version));
    }
    Err(PkgError::Parse(format!(
        "invalid profile constraint (expected \">=\" or \"=\" prefix): {expr}"
    )))
}

// ---------------------------------------------------------------------
// Raw TOML shape
// ---------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawProfilesFile {
    #[serde(default, rename = "profile")]
    profile: Vec<RawProfile>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawProfile {
    name: String,
    #[serde(default)]
    constraints: BTreeMap<String, String>,
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

    fn sample_toml() -> &'static str {
        r#"
            [[profile]]
            name = "gaming-stable"

            [profile.constraints]
            kernel = ">=6.1"
            mesa = ">=24.0"
            nvidia-driver = "=550.78"
        "#
    }

    fn pin(package: &str, version: &str, mode: PinMode) -> Pin {
        Pin {
            package: package.to_string(),
            version: v(version),
            mode,
            pinned_abi_tag: None,
        }
    }

    #[test]
    fn parses_a_profile_with_multiple_constraints() {
        let set = ProfileSet::from_toml_str(sample_toml()).unwrap();
        let profile = set.get("gaming-stable").unwrap();
        assert_eq!(
            profile.constraints.get("kernel"),
            Some(&VersionConstraint::AtLeast(v("6.1")))
        );
        assert_eq!(
            profile.constraints.get("nvidia-driver"),
            Some(&VersionConstraint::Exact(v("550.78")))
        );
    }

    #[test]
    fn duplicate_profile_name_is_an_error() {
        let toml = r#"
            [[profile]]
            name = "gaming-stable"

            [profile.constraints]
            mesa = ">=24.0"

            [[profile]]
            name = "gaming-stable"

            [profile.constraints]
            mesa = ">=23.0"
        "#;
        assert!(ProfileSet::from_toml_str(toml).is_err());
    }

    #[test]
    fn rejects_bare_version_without_operator() {
        let toml = r#"
            [[profile]]
            name = "bad"

            [profile.constraints]
            kernel = "6.1"
        "#;
        assert!(ProfileSet::from_toml_str(toml).is_err());
    }

    #[test]
    fn deny_unknown_fields_catches_a_typo() {
        let toml = r#"
            [[profile]]
            nmae = "gaming-stable"
        "#;
        assert!(ProfileSet::from_toml_str(toml).is_err());
    }

    #[test]
    fn profile_set_names_lists_everything_parsed() {
        let set = ProfileSet::from_toml_str(sample_toml()).unwrap();
        let names: Vec<&str> = set.names().collect();
        assert_eq!(names, vec!["gaming-stable"]);
    }

    #[test]
    fn effective_constraints_keeps_unpinned_entries() {
        let set = ProfileSet::from_toml_str(sample_toml()).unwrap();
        let profile = set.get("gaming-stable").unwrap();
        let pins = PinSet::new(); // nothing pinned

        let effective = effective_constraints(profile, &pins);
        assert_eq!(effective.len(), 3);
        assert!(effective.contains_key("kernel"));
        assert!(effective.contains_key("mesa"));
        assert!(effective.contains_key("nvidia-driver"));
    }

    #[test]
    fn effective_constraints_drops_a_pinned_package_entirely() {
        let set = ProfileSet::from_toml_str(sample_toml()).unwrap();
        let profile = set.get("gaming-stable").unwrap();

        let mut pins = PinSet::new();
        pins.insert(pin("nvidia-driver", "545.29", PinMode::AbiTrack));

        let effective = effective_constraints(profile, &pins);
        assert!(
            !effective.contains_key("nvidia-driver"),
            "pinned package's profile constraint should be dropped, not merged"
        );
        assert_eq!(effective.len(), 2);
        assert!(effective.contains_key("kernel"));
        assert!(effective.contains_key("mesa"));
    }

    #[test]
    fn effective_constraints_unaffected_by_unrelated_pins() {
        let set = ProfileSet::from_toml_str(sample_toml()).unwrap();
        let profile = set.get("gaming-stable").unwrap();

        let mut pins = PinSet::new();
        pins.insert(pin("openssl", "3.0.13", PinMode::Exact)); // not in the profile at all

        let effective = effective_constraints(profile, &pins);
        assert_eq!(effective.len(), 3, "an unrelated pin shouldn't remove anything");
    }

    #[test]
    fn effective_constraints_with_empty_profile_is_empty() {
        let profile = Profile {
            name: "empty".to_string(),
            constraints: BTreeMap::new(),
        };
        let pins = PinSet::new();
        assert!(effective_constraints(&profile, &pins).is_empty());
    }
}
