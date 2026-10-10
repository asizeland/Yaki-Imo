//! Virtual packages, OR-groups, and dependency declarations (§5.2).
//!
//! **Crate placement:** this is `pkg-core/src/virtual.rs` — the canonical
//! home of [`Dependency`], [`VersionConstraint`], and [`VirtualName`].
//! Every other crate in the workspace (`pkg-resolver`, `pkg-db`, the CLI
//! front-end, the orchestrator) imports these types `from pkg_core`; none
//! of them redefine or shadow them. The file is named `virtual.rs`
//! because `virtual` is a reserved keyword in Rust — reserved for possible
//! future language use, the same way C++ reserves words it doesn't
//! currently use either. Trying `mod virtual;` won't compile.
//!
//! This module is pure data representation: what a manifest can *express*
//! about its dependencies. Actually solving a dependency graph — deciding
//! which alternative in an `AnyOf` gets picked, unioning version
//! constraints during search — is `pkg-resolver`'s job, not this crate's.
//! Keeping that boundary is why `pkg-core` can sit at the bottom of the
//! workspace's dependency graph with nothing of its own to depend on.
//!
//! ## v2 changes
//!
//! - **[MAJOR] Crate placement made explicit.** The module doc now states
//!   this is `pkg-core/src/virtual.rs` and the canonical home of
//!   `Dependency` / `VersionConstraint` / `VirtualName`; all other crates
//!   import these `from pkg_core`.
//! - **[MAJOR] serde.** `Dependency`, `VersionConstraint`, `VirtualName`,
//!   and `AlternativePreference` now implement serde
//!   `Serialize`/`Deserialize` — §4.1 manifests and the profile/pin files
//!   (§10.3, §5.6.1) are TOML, so these types cross the serialization
//!   boundary constantly. See the note on [`VersionConstraint`] about the
//!   corresponding requirement on `Version` itself.
//! - **[MAJOR] `VersionConstraint::Range`.** New upper-bound-capable
//!   variant (`min` inclusive, `max` exclusive when `Some`) so profiles
//!   can express "mesa >= 24.0, < 25" and abi-track pins (§5.6.1) can be
//!   represented as version sets. `Display` and `matches` updated.
//! - **[MINOR] Versioned edges to virtuals documented.** See the note on
//!   [`Dependency::Virtual`]: per spec §7.8.1, edges like DKMS's
//!   `linux-headers(=<kver>)` are `Dependency::Package` with an `Exact`
//!   constraint, constructed programmatically — never `Dependency::Virtual`.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::Version;

// ---------------------------------------------------------------------
// VirtualName
// ---------------------------------------------------------------------

/// A virtual package name, e.g. `virtual/kernel`.
///
/// Virtuals let a dependency be satisfied by *any* package that declares
/// itself as a provider, rather than one specific package — the mechanism
/// behind `virtual/kernel` (§7.8, §13.6) and `pkg alternatives` (§16.1).
/// The `virtual/` namespace convention mirrors Gentoo's own virtual
/// category, for the same reason it works there: it keeps virtuals
/// visually and structurally distinct from concrete package names at a
/// glance, rather than relying on context to tell them apart.
///
/// Serialization is the plain string form (`"virtual/kernel"`).
/// Deserialization goes through the same validation as [`VirtualName::new`]
/// rather than the derived field-by-field path — a TOML file must not be
/// able to construct a `VirtualName` that `new` would have rejected.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
pub struct VirtualName(String);

impl VirtualName {
    /// Constructs a `VirtualName`, rejecting anything that isn't exactly
    /// `virtual/<non-empty-name>` — no missing namespace, no empty name
    /// after the slash, no extra path segments.
    pub fn new(name: impl Into<String>) -> Result<Self, VirtualNameError> {
        let name = name.into();
        match name.strip_prefix("virtual/") {
            Some(rest) if !rest.is_empty() && !rest.contains('/') => Ok(VirtualName(name)),
            _ => Err(VirtualNameError(name)),
        }
    }

    /// The provider-facing part after `virtual/` — `"kernel"` from
    /// `virtual/kernel`. Useful for display contexts that already make
    /// clear a virtual is being discussed and don't need the prefix
    /// repeated.
    pub fn short_name(&self) -> &str {
        &self.0["virtual/".len()..]
    }
}

impl fmt::Display for VirtualName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl FromStr for VirtualName {
    type Err = VirtualNameError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        VirtualName::new(s)
    }
}

impl<'de> Deserialize<'de> for VirtualName {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        VirtualName::new(s).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VirtualNameError(String);

impl fmt::Display for VirtualNameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "invalid virtual package name (expected \"virtual/<name>\"): {}",
            self.0
        )
    }
}

impl std::error::Error for VirtualNameError {}

// ---------------------------------------------------------------------
// VersionConstraint
// ---------------------------------------------------------------------

/// What a dependency requires of a candidate version.
///
/// Deliberately minimal — this only needs to represent what a recipe
/// author actually wrote in a manifest. PubGrub's real range algebra
/// (unioning and intersecting constraints while searching for a solution)
/// belongs to `pkg-resolver`, which can build whatever internal
/// representation it needs on top of this one.
///
/// **serde note:** deriving `Serialize`/`Deserialize` here requires
/// `pkg-core`'s `Version` (see `version.rs`) to implement both traits via
/// its canonical string form (e.g. `"24.0.1-r2"`). If `version.rs` does
/// not already do so, that impl must be added there — constraints are
/// embedded in TOML manifests (§4.1) and profile/pin files (§5.6.1,
/// §10.3), where a version is always written as a string, never as a
/// structured object.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum VersionConstraint {
    /// No constraint — any version satisfies this dependency.
    Any,
    /// Exactly this version. Used by `pkg install foo=<version>` pins and
    /// glibc's `exact` pin mode (§5.6.1).
    Exact(Version),
    /// This version or newer, per `Version`'s ordering (§7.7).
    AtLeast(Version),
    /// A bounded range: `min` inclusive, `max` exclusive when `Some`
    /// (unbounded above when `None`).
    ///
    /// Rationale: resolution profiles need to write things like
    /// "mesa >= 24.0, < 25", and abi-track pins (§5.6.1) are most
    /// naturally expressed as a version set — `Range { min: pinned,
    /// max: Some(next_abi_break) }` — rather than a bare floor.
    Range { min: Version, max: Option<Version> },
}

impl VersionConstraint {
    /// Does `version` satisfy this constraint?
    ///
    /// This is the single-candidate check used by callers that already
    /// have a concrete version in hand (pin enforcement, display
    /// filtering, tests). It is *not* the resolver's search logic —
    /// `pkg-resolver` performs its own algebra over these constraints.
    pub fn matches(&self, version: &Version) -> bool {
        match self {
            VersionConstraint::Any => true,
            VersionConstraint::Exact(v) => version == v,
            VersionConstraint::AtLeast(v) => version >= v,
            VersionConstraint::Range { min, max } => {
                version >= min && max.as_ref().is_none_or(|max| version < max)
            }
        }
    }
}

impl fmt::Display for VersionConstraint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            VersionConstraint::Any => write!(f, "*"),
            VersionConstraint::Exact(v) => write!(f, "={v}"),
            VersionConstraint::AtLeast(v) => write!(f, ">={v}"),
            VersionConstraint::Range { min, max } => match max {
                Some(max) => write!(f, ">={min}, <{max}"),
                None => write!(f, ">={min}"),
            },
        }
    }
}

// ---------------------------------------------------------------------
// Dependency
// ---------------------------------------------------------------------

/// One dependency requirement declared in a manifest (§4.1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Dependency {
    /// A concrete package by name, with a version constraint.
    Package {
        name: String,
        constraint: VersionConstraint,
    },
    /// A virtual package — satisfied by whichever provider is installed,
    /// or the user's preferred one if set (§16.1, `pkg alternatives`).
    ///
    /// **Carries no version constraint, deliberately.** A versioned edge
    /// *to a virtual's subject* — e.g. DKMS's per-kernel
    /// `linux-headers(=<kver>)` edges (§7.8.1) — must instead be expressed
    /// as `Dependency::Package { name: "linux-headers", constraint:
    /// VersionConstraint::Exact(...) }`, constructed programmatically by
    /// the orchestrator when it enumerates installed `virtual/kernel`
    /// providers. The version requirement belongs to the concrete
    /// provider package, not to the virtual itself.
    Virtual(VirtualName),
    /// Any one of several alternatives satisfies this dependency — the
    /// "OR-group" mechanism referenced throughout §5.
    ///
    /// This is `Vec<Dependency>`, not `Box<Dependency>`. A `Vec` already
    /// stores its elements on the heap, so nesting `Dependency` inside
    /// itself this way sidesteps the "type would have infinite size"
    /// problem a bare recursive field (`AnyOf(Dependency)`) would hit —
    /// no explicit `Box` needed, and it naturally holds more than two
    /// alternatives without a nested pairwise structure.
    AnyOf(Vec<Dependency>),
}

impl Dependency {
    /// Convenience constructor for the common case: a named package, no
    /// version constraint.
    pub fn any_version(name: impl Into<String>) -> Self {
        Dependency::Package {
            name: name.into(),
            constraint: VersionConstraint::Any,
        }
    }

    /// Convenience constructor for a minimum-version requirement.
    pub fn at_least(name: impl Into<String>, version: Version) -> Self {
        Dependency::Package {
            name: name.into(),
            constraint: VersionConstraint::AtLeast(version),
        }
    }

    /// Convenience constructor for a bounded-range requirement
    /// (`min` inclusive, `max` exclusive; pass `None` for no upper bound).
    pub fn range(name: impl Into<String>, min: Version, max: Option<Version>) -> Self {
        Dependency::Package {
            name: name.into(),
            constraint: VersionConstraint::Range { min, max },
        }
    }
}

// ---------------------------------------------------------------------
// AlternativePreference
// ---------------------------------------------------------------------

/// A user's preferred provider for a virtual package
/// (`pkg alternatives set <virtual> <provider>`, §16.1).
///
/// Just the data shape. Persisting it to
/// `/var/lib/pkg/config/alternatives.toml` is `pkg-db`'s job; applying it
/// during resolution is `pkg-resolver`'s. This type is what those two
/// crates pass between each other — and since the persistence format is
/// TOML, it derives serde traits alongside the rest of this module.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AlternativePreference {
    pub virtual_name: VirtualName,
    pub provider: String,
}

// ---------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_valid_virtual_name() {
        let v: VirtualName = "virtual/kernel".parse().unwrap();
        assert_eq!(v.to_string(), "virtual/kernel");
        assert_eq!(v.short_name(), "kernel");
    }

    #[test]
    fn rejects_missing_namespace() {
        assert!("kernel".parse::<VirtualName>().is_err());
    }

    #[test]
    fn rejects_empty_short_name() {
        assert!("virtual/".parse::<VirtualName>().is_err());
    }

    #[test]
    fn rejects_extra_path_segments() {
        assert!("virtual/kernel/extra".parse::<VirtualName>().is_err());
    }

    #[test]
    fn builds_simple_package_dependency() {
        let dep = Dependency::any_version("openssl");
        assert_eq!(
            dep,
            Dependency::Package {
                name: "openssl".to_string(),
                constraint: VersionConstraint::Any,
            }
        );
    }

    #[test]
    fn builds_virtual_dependency() {
        let dep = Dependency::Virtual("virtual/kernel".parse().unwrap());
        match dep {
            Dependency::Virtual(v) => assert_eq!(v.short_name(), "kernel"),
            _ => panic!("expected Virtual variant"),
        }
    }

    #[test]
    fn or_group_holds_multiple_alternatives() {
        // Models something like "needs any of: cronie, fcron, dcron" —
        // an OR-group of concrete packages with no virtual involved.
        let dep = Dependency::AnyOf(vec![
            Dependency::any_version("cronie"),
            Dependency::any_version("fcron"),
            Dependency::any_version("dcron"),
        ]);
        match dep {
            Dependency::AnyOf(alts) => assert_eq!(alts.len(), 3),
            _ => panic!("expected AnyOf variant"),
        }
    }

    #[test]
    fn or_group_can_mix_virtual_and_concrete() {
        let dep = Dependency::AnyOf(vec![
            Dependency::Virtual("virtual/kernel".parse().unwrap()),
            Dependency::any_version("linux-lts"),
        ]);
        assert!(matches!(dep, Dependency::AnyOf(_)));
    }

    #[test]
    fn or_groups_can_nest() {
        // Compiling at all is most of the proof here — this would fail to
        // build if `AnyOf` needed an explicit `Box` and didn't have one.
        let dep = Dependency::AnyOf(vec![
            Dependency::any_version("a"),
            Dependency::AnyOf(vec![
                Dependency::any_version("b"),
                Dependency::any_version("c"),
            ]),
        ]);
        assert!(matches!(dep, Dependency::AnyOf(_)));
    }

    #[test]
    fn at_least_constraint_uses_version_ordering() {
        let dep = Dependency::at_least("glibc", "2.39".parse().unwrap());
        match dep {
            Dependency::Package { constraint, .. } => {
                assert_eq!(constraint, VersionConstraint::AtLeast("2.39".parse().unwrap()));
            }
            _ => panic!("expected Package variant"),
        }
    }

    // -- v2: Range variant -------------------------------------------------

    #[test]
    fn range_matches_within_bounds() {
        // "mesa >= 24.0, < 25" — the profile use case.
        let c = VersionConstraint::Range {
            min: "24.0".parse().unwrap(),
            max: Some("25".parse().unwrap()),
        };
        assert!(c.matches(&"24.0".parse().unwrap())); // min is inclusive
        assert!(c.matches(&"24.2.3".parse().unwrap()));
        assert!(!c.matches(&"25.0".parse().unwrap())); // max is exclusive
        assert!(!c.matches(&"23.9.9".parse().unwrap()));
    }

    #[test]
    fn range_with_no_upper_bound_matches_like_at_least() {
        let c = VersionConstraint::Range {
            min: "24.0".parse().unwrap(),
            max: None,
        };
        assert!(c.matches(&"99.0".parse().unwrap()));
        assert!(!c.matches(&"23.0".parse().unwrap()));
    }

    #[test]
    fn constraint_display_rounds_out_each_variant() {
        assert_eq!(VersionConstraint::Any.to_string(), "*");
        assert_eq!(
            VersionConstraint::Exact("2.39".parse().unwrap()).to_string(),
            "=2.39"
        );
        assert_eq!(
            VersionConstraint::AtLeast("2.39".parse().unwrap()).to_string(),
            ">=2.39"
        );
        assert_eq!(
            VersionConstraint::Range {
                min: "24.0".parse().unwrap(),
                max: Some("25".parse().unwrap()),
            }
            .to_string(),
            ">=24.0, <25"
        );
        assert_eq!(
            VersionConstraint::Range {
                min: "24.0".parse().unwrap(),
                max: None,
            }
            .to_string(),
            ">=24.0"
        );
    }

    #[test]
    fn versioned_edge_to_virtual_subject_is_a_package_dependency() {
        // §7.8.1's `linux-headers(=<kver>)` per-kernel DKMS edge: a
        // concrete Package dep with an Exact constraint, built
        // programmatically — Dependency::Virtual carries no constraint.
        let kver: Version = "6.9.2".parse().unwrap();
        let dep = Dependency::Package {
            name: "linux-headers".to_string(),
            constraint: VersionConstraint::Exact(kver.clone()),
        };
        match dep {
            Dependency::Package { constraint, .. } => {
                assert!(constraint.matches(&kver));
                assert!(!constraint.matches(&"6.9.3".parse().unwrap()));
            }
            _ => panic!("expected Package variant"),
        }
    }

    #[test]
    fn virtual_name_deserialization_still_validates() {
        // The serde path must not bypass `VirtualName::new`'s checks.
        let good: VirtualName = serde_json::from_str("\"virtual/kernel\"").unwrap();
        assert_eq!(good.short_name(), "kernel");
        assert!(serde_json::from_str::<VirtualName>("\"kernel\"").is_err());
        assert!(serde_json::from_str::<VirtualName>("\"virtual/\"").is_err());
    }
}
