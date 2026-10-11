//! Pinning logic — exact vs. `abi-track` modes (§5.6, §5.6.1).
//!
//! ## v2 changes
//!
//! - **CRITICAL** — `Pin::allows` redefined (it previously leaked major
//!   bumps whenever the ABI-tag check succeeded and never rejected
//!   downgrades, and it demanded a `candidate_abi_tag` that does not
//!   exist pre-build):
//!   - Signature is now `allows(&self, candidate: &Version,
//!     candidate_abi_tag: Option<&AbiTag>)` — the candidate's ABI tag is
//!     unknown until its build finishes, so it is optional.
//!   - `AbiTrack` rules, in order:
//!     1. `candidate >= self.version` is always required (no downgrades).
//!     2. If the pinned version follows the `-rN` convention (has a
//!        patch component) OR the candidate shares its release core,
//!        `same_release_core` is authoritative — accept only the same
//!        release core.
//!     3. ABI-tag equality is a FALLBACK, used only when the release-core
//!        convention is inapplicable (pinned version has no `-rN` patch)
//!        AND both ABI tags are known and equal.
//!   - When `candidate_abi_tag` is `None` (pre-build), the decision is
//!     made on release core alone; the build orchestrator re-verifies
//!     the ABI tag post-build (§7.7).
//! - **CRITICAL** — `from_toml_str` now rejects duplicate `package`
//!   entries with an error instead of silently overwriting.
//! - **Spec gap** — a pin entry should store the pinned version's ABI
//!   tag, captured at `pkg pin` time (the spec never recorded it, which
//!   is what forced the old caller-lookup design). `Pin` now carries
//!   `pinned_abi_tag: Option<AbiTag>`, populated by `pkg pin` at
//!   creation, with full TOML round-trip.
//!
//! A `Pin` on its own is `{package, version, mode, pinned_abi_tag?}` —
//! matching `pins.toml`'s schema exactly, no more.

use std::collections::BTreeMap;

use pkg_core::{AbiTag, PkgError, PkgResult, Version};
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------
// PinMode
// ---------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PinMode {
    /// Only the exact pinned version satisfies the pin — including
    /// security patches (§3.7's `-rN` suffix). Blocks everything, which
    /// is the whole point of an exact pin, but it's worth being clear
    /// that "everything" really does include patches.
    Exact,
    /// Any version sharing the pinned version's release core (§7.7's
    /// `Version::same_release_core`) satisfies the pin, provided it is
    /// not a downgrade — or, as a fallback for packages that don't use
    /// the `-rN` convention at all (where release-core equality would
    /// otherwise collapse to meaning the same thing as `Exact`), any
    /// non-downgrade version whose build shares the pinned build's
    /// `AbiTag`. See `Pin::allows` for the exact rule ordering.
    AbiTrack,
}

// ---------------------------------------------------------------------
// Pin
// ---------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pin {
    pub package: String,
    pub version: Version,
    pub mode: PinMode,
    /// The ABI tag of the build that was current at `self.version` when
    /// the pin was created — captured by `pkg pin` at pin time (the spec
    /// never said to record this; that was a spec gap this field closes).
    /// `None` for pins written before this field existed, or when the
    /// pinned version was not installed and no tag could be captured.
    pub pinned_abi_tag: Option<AbiTag>,
}

/// Whether a version follows the `-rN` patch-suffix convention (§3.7).
///
/// `Version`'s public API doesn't expose its components individually, so
/// this checks the canonical string form for the suffix marker. That is
/// stable: `Version`'s `Display` is what gets written into `pins.toml`,
/// manifests, and the database.
fn uses_patch_suffix(version: &Version) -> bool {
    version.to_string().contains("-r")
}

impl Pin {
    /// Whether `candidate` is permitted under this pin.
    ///
    /// `candidate_abi_tag` is the ABI tag the candidate build would have,
    /// if known. It is `None` pre-build — a source-build package has no
    /// ABI tag until the build finishes — in which case the decision is
    /// made on release core alone and the build orchestrator re-verifies
    /// the ABI tag post-build before committing the result.
    ///
    /// `AbiTrack` rules, in order:
    ///
    /// 1. **No downgrades.** `candidate >= self.version` is required no
    ///    matter what the other checks say.
    /// 2. **Release core is authoritative when the `-rN` convention
    ///    applies.** If the pinned version has a patch component, or the
    ///    candidate shares the pinned version's release core, accept
    ///    exactly when `same_release_core` holds. The ABI-tag fallback
    ///    must NOT override this — otherwise a major bump that happens to
    ///    keep the ABI tag (a soname-only-stable library jumping e.g.
    ///    2.39-r2 -> 2.40) would slip through the pin.
    /// 3. **ABI-tag equality is a fallback only.** It applies when the
    ///    release-core convention is inapplicable — the pinned version
    ///    has no `-rN` patch (a package that doesn't use the convention,
    ///    where `same_release_core` degenerates to plain equality) — and
    ///    both tags are known and equal.
    pub fn allows(&self, candidate: &Version, candidate_abi_tag: Option<&AbiTag>) -> bool {
        match self.mode {
            PinMode::Exact => candidate == &self.version,
            PinMode::AbiTrack => {
                // Rule 1: never allow a downgrade under abi-track.
                if candidate < &self.version {
                    return false;
                }
                let same_core = candidate.same_release_core(&self.version);
                // Rule 2: when the -rN convention is in play (either the
                // pin itself uses it, or the candidate already shares the
                // release core), release-core equality decides, full stop.
                if uses_patch_suffix(&self.version) || same_core {
                    return same_core;
                }
                // Rule 3: fallback for non -rN packages — ABI-tag equality,
                // only when both sides are known. Pre-build (candidate tag
                // unknown) or a pre-v2 pin (pinned tag never captured) is
                // a rejection here; the release-core check above is the
                // pre-build decision path.
                matches!(
                    (self.pinned_abi_tag.as_ref(), candidate_abi_tag),
                    (Some(pinned), Some(candidate)) if pinned == candidate
                )
            }
        }
    }
}

// ---------------------------------------------------------------------
// PinSet
// ---------------------------------------------------------------------

/// All active pins, keyed by package name — matching `pkg pin`'s own
/// model of at most one active pin per package at a time (pinning again
/// replaces, the same contract `Catalog::add_package_version` documents
/// for its own re-registration case).
///
/// A `BTreeMap`, not a `HashMap`: `to_toml_string` needs a deterministic
/// iteration order, since `pins.toml` is a file a person might diff or
/// keep under version control, and a `HashMap`'s iteration order varying
/// from run to run would mean rewriting it in a different order every
/// time for no reason.
#[derive(Debug, Clone, Default)]
pub struct PinSet {
    pins: BTreeMap<String, Pin>,
}

impl PinSet {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get(&self, package: &str) -> Option<&Pin> {
        self.pins.get(package)
    }

    /// Inserts or replaces the pin for `pin.package`, returning whatever
    /// was previously pinned for that package, if anything.
    pub fn insert(&mut self, pin: Pin) -> Option<Pin> {
        self.pins.insert(pin.package.clone(), pin)
    }

    pub fn remove(&mut self, package: &str) -> Option<Pin> {
        self.pins.remove(package)
    }

    pub fn is_empty(&self) -> bool {
        self.pins.is_empty()
    }

    pub fn len(&self) -> usize {
        self.pins.len()
    }

    pub fn from_toml_str(input: &str) -> PkgResult<Self> {
        let raw: RawPinsFile = toml::from_str(input).map_err(|e| PkgError::Parse(e.to_string()))?;
        let mut set = Self::new();
        for raw_pin in raw.pin {
            // Duplicate `[[pin]]` entries for the same package are a
            // config error, not a replace — silently keeping the last one
            // would hide a genuine authoring mistake in a human-edited
            // file ("explicit over implicit", §2).
            if set.pins.contains_key(&raw_pin.package) {
                return Err(PkgError::Parse(format!(
                    "duplicate pin entry for package {:?}",
                    raw_pin.package
                )));
            }
            let version: Version = raw_pin
                .version
                .parse()
                .map_err(|e: pkg_core::VersionParseError| PkgError::Parse(e.to_string()))?;
            set.insert(Pin {
                package: raw_pin.package,
                version,
                mode: raw_pin.mode.into(),
                pinned_abi_tag: raw_pin.abi_tag.map(AbiTag),
            });
        }
        Ok(set)
    }

    pub fn to_toml_string(&self) -> PkgResult<String> {
        let raw = RawPinsFile {
            pin: self
                .pins
                .values()
                .map(|p| RawPin {
                    package: p.package.clone(),
                    version: p.version.to_string(),
                    mode: p.mode.into(),
                    abi_tag: p.pinned_abi_tag.as_ref().map(|t| t.0),
                })
                .collect(),
        };
        toml::to_string(&raw).map_err(|e| PkgError::Parse(e.to_string()))
    }
}

// ---------------------------------------------------------------------
// Raw TOML shape
// ---------------------------------------------------------------------

/// Mirrors `manifest.rs`'s split between the literal wire shape and the
/// domain type: `Pin.version` is a real `Version`, not a `String`, and
/// TOML's `[[pin]]` array-of-tables needs somewhere to land as plain
/// strings before that parsing happens.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPinsFile {
    #[serde(default, rename = "pin")]
    pin: Vec<RawPin>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPin {
    package: String,
    version: String,
    #[serde(default)]
    mode: RawPinMode,
    /// Serialized as the raw tag value (`AbiTag` itself has no serde
    /// impl in pkg-core); absent for pre-v2 pin files.
    #[serde(default, rename = "abi-tag", skip_serializing_if = "Option::is_none")]
    abi_tag: Option<u32>,
}

#[derive(Debug, Default, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum RawPinMode {
    #[default]
    Exact,
    AbiTrack,
}

impl From<PinMode> for RawPinMode {
    fn from(mode: PinMode) -> Self {
        match mode {
            PinMode::Exact => RawPinMode::Exact,
            PinMode::AbiTrack => RawPinMode::AbiTrack,
        }
    }
}

impl From<RawPinMode> for PinMode {
    fn from(raw: RawPinMode) -> Self {
        match raw {
            RawPinMode::Exact => PinMode::Exact,
            RawPinMode::AbiTrack => PinMode::AbiTrack,
        }
    }
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

    fn pin(package: &str, version: &str, mode: PinMode, tag: Option<u32>) -> Pin {
        Pin {
            package: package.to_string(),
            version: v(version),
            mode,
            pinned_abi_tag: tag.map(AbiTag),
        }
    }

    #[test]
    fn mode_defaults_to_exact_when_omitted() {
        let toml = r#"
            [[pin]]
            package = "glibc"
            version = "2.39"
        "#;
        let set = PinSet::from_toml_str(toml).unwrap();
        assert_eq!(set.get("glibc").unwrap().mode, PinMode::Exact);
        assert_eq!(set.get("glibc").unwrap().pinned_abi_tag, None);
    }

    #[test]
    fn parses_explicit_abi_track_mode() {
        let toml = r#"
            [[pin]]
            package = "glibc"
            version = "2.39"
            mode = "abi-track"
        "#;
        let set = PinSet::from_toml_str(toml).unwrap();
        assert_eq!(set.get("glibc").unwrap().mode, PinMode::AbiTrack);
    }

    #[test]
    fn round_trips_through_toml() {
        let mut set = PinSet::new();
        set.insert(pin("glibc", "2.39", PinMode::AbiTrack, Some(7)));
        set.insert(pin("openssl", "3.0.13", PinMode::Exact, None));

        let text = set.to_toml_string().unwrap();
        let round_tripped = PinSet::from_toml_str(&text).unwrap();

        assert_eq!(round_tripped.get("glibc"), set.get("glibc"));
        assert_eq!(round_tripped.get("openssl"), set.get("openssl"));
        assert_eq!(round_tripped.len(), 2);
    }

    #[test]
    fn abi_tag_round_trips_through_toml() {
        let toml = r#"
            [[pin]]
            package = "glibc"
            version = "2.39"
            mode = "abi-track"
            abi-tag = 42
        "#;
        let set = PinSet::from_toml_str(toml).unwrap();
        assert_eq!(set.get("glibc").unwrap().pinned_abi_tag, Some(AbiTag(42)));

        let text = set.to_toml_string().unwrap();
        assert!(text.contains("abi-tag = 42"));
        let round_tripped = PinSet::from_toml_str(&text).unwrap();
        assert_eq!(round_tripped.get("glibc"), set.get("glibc"));
    }

    #[test]
    fn duplicate_pin_entry_is_an_error() {
        let toml = r#"
            [[pin]]
            package = "glibc"
            version = "2.39"

            [[pin]]
            package = "glibc"
            version = "2.40"
        "#;
        assert!(PinSet::from_toml_str(toml).is_err());
    }

    #[test]
    fn deny_unknown_fields_catches_a_typo() {
        let toml = r#"
            [[pin]]
            package = "glibc"
            versoin = "2.39"
        "#;
        assert!(PinSet::from_toml_str(toml).is_err());
    }

    #[test]
    fn exact_mode_rejects_a_patch_even_with_same_release_core() {
        // The whole reason abi-track mode exists: an exact pin blocks
        // security patches too, not just major bumps.
        let p = pin("glibc", "2.39", PinMode::Exact, None);
        assert!(p.allows(&v("2.39"), None));
        assert!(!p.allows(&v("2.39-r1"), None));
    }

    #[test]
    fn abi_track_mode_allows_same_release_core_regardless_of_abi_tag() {
        let p = pin("glibc", "2.39", PinMode::AbiTrack, Some(1));
        // Different candidate ABI tag on purpose — proving the
        // release-core check alone is sufficient here.
        assert!(p.allows(&v("2.39-r1"), Some(&AbiTag(2))));
    }

    #[test]
    fn abi_track_mode_rejects_major_bump_even_with_equal_abi_tag() {
        // Pinned on a -rN version: release core is authoritative, so a
        // 2.40 bump is rejected even though the ABI tag matches — the
        // old inclusive-OR logic would have leaked this through.
        let p = pin("glibc", "2.39-r1", PinMode::AbiTrack, Some(1));
        assert!(!p.allows(&v("2.40"), Some(&AbiTag(1))));
    }

    #[test]
    fn abi_track_mode_rejects_downgrades() {
        // Even within the same release core, going backwards is not
        // permitted under abi-track.
        let p = pin("glibc", "2.39-r2", PinMode::AbiTrack, Some(1));
        assert!(!p.allows(&v("2.39-r1"), Some(&AbiTag(1))));
        assert!(!p.allows(&v("2.39"), Some(&AbiTag(1))));
        assert!(p.allows(&v("2.39-r3"), Some(&AbiTag(1))));
    }

    #[test]
    fn abi_track_mode_falls_back_to_abi_tag_for_non_rn_packages() {
        // openssl doesn't use the -rN convention. 3.0.12 -> 3.0.13 is a
        // different release core, so same_release_core is false — but
        // with no -rN patch on the pinned version and matching ABI tags
        // (§7.7: no ABI-breaking change between them), the fallback
        // applies and abi-track allows it.
        let p = pin("openssl", "3.0.12", PinMode::AbiTrack, Some(4));
        assert!(p.allows(&v("3.0.13"), Some(&AbiTag(4))));
    }

    #[test]
    fn abi_track_mode_blocks_when_neither_condition_holds() {
        let p = pin("openssl", "3.0.12", PinMode::AbiTrack, Some(4));
        assert!(!p.allows(&v("3.0.13"), Some(&AbiTag(5))));
    }

    #[test]
    fn abi_track_mode_without_candidate_abi_tag_decides_on_release_core_alone() {
        // Pre-build: no candidate tag exists yet. Same release core
        // passes; anything else must wait for the orchestrator's
        // post-build ABI re-verification and is rejected here.
        let p = pin("glibc", "2.39-r1", PinMode::AbiTrack, Some(1));
        assert!(p.allows(&v("2.39-r2"), None));
        assert!(!p.allows(&v("2.40"), None));

        let p = pin("openssl", "3.0.12", PinMode::AbiTrack, Some(4));
        assert!(!p.allows(&v("3.0.13"), None));
    }

    #[test]
    fn pin_set_insert_replaces_and_returns_previous() {
        let mut set = PinSet::new();
        let first = pin("glibc", "2.39", PinMode::Exact, None);
        let second = pin("glibc", "2.40", PinMode::AbiTrack, Some(3));
        assert_eq!(set.insert(first.clone()), None);
        assert_eq!(set.insert(second.clone()), Some(first));
        assert_eq!(set.get("glibc"), Some(&second));
        assert_eq!(set.len(), 1);
    }

    #[test]
    fn pin_set_remove() {
        let mut set = PinSet::new();
        set.insert(pin("glibc", "2.39", PinMode::Exact, None));
        assert!(set.remove("glibc").is_some());
        assert!(set.is_empty());
        assert!(set.remove("glibc").is_none());
    }
}
