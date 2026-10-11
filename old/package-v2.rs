//! ## v2 changes
//!
//! - [CRITICAL] `Version`: fixed Eq/Ord/Hash contract violation. Previously
//!   derived `PartialEq`/`Eq`/`Hash` compared raw fields while the
//!   hand-written `Ord` zero-padded trailing release components and treated
//!   `patch: None` as `-r0` — so `2.39` vs `2.39.0` were `Ord::Equal` but
//!   `!=`, breaking `BTreeSet`/`HashMap` invariants. Now `PartialEq`, `Eq`,
//!   and `Hash` are hand-implemented to match `Ord` semantics exactly
//!   (`a == b` iff `a.cmp(b) == Equal`; hash on the zero-padded comparison
//!   tuple), and `FromStr` normalizes trailing zero components out of
//!   `release` (keeping at least one) so `2.39.0` stores as `[2, 39]`.
//!   `same_release_core` now uses the same padded comparison logic.
//!   Public field layout and API are unchanged.
//!
//! Core identity types: `Version`, `AbiTag`, `Arch`, `Track`, and `PackageId`.
//!
//! Everything else in the workspace builds on these — the resolver compares
//! `Version`s, the scanner reads `AbiTag`s off installed packages, the
//! database's `packages` table mirrors this module's fields almost exactly
//! (§9.2). Keeping them in one place, with no dependencies of their own,
//! is what lets `pkg-core` sit at the bottom of the dependency graph.

use std::cmp::Ordering;
use std::fmt;
use std::hash::{Hash, Hasher};
use std::str::FromStr;

// ---------------------------------------------------------------------
// Version
// ---------------------------------------------------------------------

/// A package's release version, independent of ABI tag or patch revision.
///
/// Two version shapes appear across the distribution: plain dotted-numeric
/// releases (`3.0.12`, `550.78`) and the Gentoo-inspired split between
/// major bumps and inline security patches (§3.7), expressed as a `-rN`
/// suffix on the same release (`2.39-r1`, `2.39-r2`). Both are handled by
/// this one type, rather than two separate ones, since `abi-track` pins
/// (§5.6.1) need to compare across that suffix for any package that
/// adopts the convention — not just glibc.
///
/// # Ordering/equality invariant
///
/// Equality, ordering, and hashing are all defined by the same comparison:
/// `a == b` **iff** `a.cmp(b) == Ordering::Equal`, and equal versions
/// always hash identically. The comparison treats a shorter `release` as
/// padded with trailing zeros and a missing `patch` as `-r0`, so `2.39`,
/// `2.39.0`, and `2.39-r0` are all equal. To support that invariant,
/// `FromStr` normalizes trailing zero components out of `release` (always
/// keeping at least one component), so the stored form of `"2.39.0"` is
/// `[2, 39]`. `patch` is *not* normalized away: a bare `None` still
/// displays as `"2.39"` rather than `"2.39-r0"` (see the field doc below),
/// and the hand-written `PartialEq`/`Hash` impls account for that.
#[derive(Debug, Clone)]
pub struct Version {
    /// The dotted-numeric core, e.g. `[3, 0, 12]` for "3.0.12". Never
    /// ends in a zero component (normalized by `FromStr`), so `[2, 39]`
    /// is the canonical stored form of both "2.39" and "2.39.0".
    release: Vec<u64>,
    /// The `-rN` patch revision, if present. `None` means this version has
    /// never been patched — equivalent to `-r0` for ordering, equality, and
    /// hashing purposes, but kept as `Option` rather than defaulted to zero
    /// so `Display` can round-trip the original string exactly (a bare
    /// "2.39" should print back as "2.39", not "2.39-r0").
    patch: Option<u32>,
}

impl Version {
    /// True if `self` and `other` share the same release core and differ
    /// only in patch revision — e.g. `2.39` and `2.39-r2`. This is the
    /// check `abi-track` pins (§5.6.1) use for any package following the
    /// `-rN` convention: it tells the resolver "same major/ABI line,
    /// different patch level" without needing to know anything else about
    /// the package.
    ///
    /// Uses the same zero-padded comparison as `Ord`, so a short release
    /// and its padded form count as the same core (`2.39` vs `2.39.0`),
    /// consistent with the equality invariant on the struct.
    pub fn same_release_core(&self, other: &Version) -> bool {
        let len = self.release.len().max(other.release.len());
        (0..len).all(|i| {
            self.release.get(i).copied().unwrap_or(0)
                == other.release.get(i).copied().unwrap_or(0)
        })
    }

    fn patch_or_zero(&self) -> u32 {
        self.patch.unwrap_or(0)
    }

    /// The value this version contributes to comparisons: its release
    /// components zero-padded out to `len`, plus the patch revision with
    /// `None` treated as `-r0`. Shared by `Ord`, `PartialEq`, and `Hash`
    /// so the three can never drift apart — the invariant on the struct
    /// doc holds by construction, not by three impls happening to agree.
    fn cmp_tuple(&self, len: usize) -> (Vec<u64>, u32) {
        let mut padded = Vec::with_capacity(len);
        padded.extend_from_slice(&self.release);
        padded.resize(len, 0);
        (padded, self.patch_or_zero())
    }
}

/// `PartialEq` is hand-written (not derived) so equality means exactly
/// what `Ord` says: equal under zero-padded comparison. A derived impl
/// would compare the raw `patch: Option<u32>` and declare `2.39 != 2.39-r0`
/// even though `cmp` says `Equal` — a contract violation that corrupts
/// sorted containers and hash maps.
impl PartialEq for Version {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for Version {}

/// `Hash` must agree with `PartialEq`: equal versions hash identically.
/// It hashes the same padded tuple the comparison uses, at the pair's
/// combined length — but since `FromStr` strips trailing zeros, `self`'s
/// own length is always the canonical one, so hashing at `self.release
/// .len()` is already correct for any two values that can exist.
impl Hash for Version {
    fn hash<H: Hasher>(&self, state: &mut H) {
        let (padded, patch) = self.cmp_tuple(self.release.len());
        padded.hash(state);
        patch.hash(state);
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let release = self
            .release
            .iter()
            .map(u64::to_string)
            .collect::<Vec<_>>()
            .join(".");
        write!(f, "{release}")?;
        if let Some(rev) = self.patch {
            write!(f, "-r{rev}")?;
        }
        Ok(())
    }
}

/// `FromStr` is what lets `"2.39-r1".parse::<Version>()` work — the
/// standard library's generic parsing hook, rather than a one-off
/// `Version::parse` method. Anything that implements it gets `.parse()`
/// for free, which is why string-heavy formats (recipe manifests, CLI
/// arguments) tend to reach for this trait rather than inherent methods.
impl FromStr for Version {
    type Err = VersionParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        // Split off an optional "-rN" patch suffix before touching the
        // dotted-numeric part.
        let (release_part, patch) = match s.split_once("-r") {
            Some((release, rev)) => {
                let rev: u32 = rev
                    .parse()
                    .map_err(|_| VersionParseError(s.to_string()))?;
                (release, Some(rev))
            }
            None => (s, None),
        };

        let mut release: Vec<u64> = release_part
            .split('.')
            .map(str::parse::<u64>)
            .collect::<Result<_, _>>()
            .map_err(|_| VersionParseError(s.to_string()))?;

        if release.is_empty() {
            return Err(VersionParseError(s.to_string()));
        }

        // Normalize: strip trailing zero components (keeping at least one)
        // so "2.39.0" stores identically to "2.39". Ordering already treated
        // missing trailing components as zeros; normalizing here makes the
        // stored form canonical, which keeps `Display` round-trips, hashing,
        // and equality all consistent with that comparison.
        while release.len() > 1 && release.last() == Some(&0) {
            release.pop();
        }

        Ok(Version { release, patch })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionParseError(String);

impl fmt::Display for VersionParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid version string: {}", self.0)
    }
}

impl std::error::Error for VersionParseError {}

/// `Ord` and `PartialOrd` are two separate traits in Rust because not every
/// partially-ordered type has a *total* order (floats are the classic
/// example: `NaN` compares to nothing). Versions always compare cleanly, so
/// `partial_cmp` just delegates to `cmp` — but we still have to write both
/// impls, since `Ord` doesn't imply `PartialOrd` is derived from it.
impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        // Compare release components pairwise. A shorter list is treated
        // as though padded with trailing zeros, so "2.39" and "2.39.0"
        // compare equal — mirrors how most dotted-version schemes behave.
        let len = self.release.len().max(other.release.len());
        for i in 0..len {
            let a = self.release.get(i).copied().unwrap_or(0);
            let b = other.release.get(i).copied().unwrap_or(0);
            match a.cmp(&b) {
                Ordering::Equal => continue,
                ord => return ord,
            }
        }
        // Release cores are equal; patch revision breaks the tie.
        self.patch_or_zero().cmp(&other.patch_or_zero())
    }
}

// ---------------------------------------------------------------------
// AbiTag
// ---------------------------------------------------------------------

/// A package's ABI-compatibility generation, e.g. `ab1`, `ab2`.
///
/// Incremented only when a package's actual ABI changes (§7.7) — not on
/// every version bump. Two builds sharing an `AbiTag` are guaranteed
/// binary-compatible with each other's reverse dependents, which is what
/// lets the stable `.store/` symlink (§6.1) skip a rebuild cascade for
/// ordinary patch releases, and only trigger one when this value changes.
///
/// This is a "newtype" — a single-field tuple struct wrapping a `u32`.
/// The point isn't the extra field, it's that `AbiTag` and a bare `u32`
/// are different types as far as the compiler is concerned, so a function
/// expecting an `AbiTag` can't accidentally be handed a raw revision
/// count, a PID, or any other unrelated integer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AbiTag(pub u32);

impl fmt::Display for AbiTag {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ab{}", self.0)
    }
}

/// The counterpart to `Display` — needed anywhere an `AbiTag` has to be
/// read back from stored text, e.g. a `TEXT` column in `pkg-db`.
impl FromStr for AbiTag {
    type Err = AbiTagParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        s.strip_prefix("ab")
            .and_then(|rest| rest.parse::<u32>().ok())
            .map(AbiTag)
            .ok_or_else(|| AbiTagParseError(s.to_string()))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AbiTagParseError(String);

impl fmt::Display for AbiTagParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid ABI tag (expected \"abN\"): {}", self.0)
    }
}

impl std::error::Error for AbiTagParseError {}

// ---------------------------------------------------------------------
// Arch
// ---------------------------------------------------------------------

/// Target architecture. Multilib support (§5.4) means both variants can be
/// installed side by side; `PackageId` (below) plus this field is what the
/// database's uniqueness constraint on `packages` is built from (§9.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Arch {
    Amd64,
    I686,
}

impl fmt::Display for Arch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Arch::Amd64 => "amd64",
            Arch::I686 => "i686",
        };
        write!(f, "{s}")
    }
}

impl FromStr for Arch {
    type Err = ArchParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "amd64" => Ok(Arch::Amd64),
            "i686" => Ok(Arch::I686),
            other => Err(ArchParseError(other.to_string())),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchParseError(String);

impl fmt::Display for ArchParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid architecture (expected \"amd64\" or \"i686\"): {}", self.0)
    }
}

impl std::error::Error for ArchParseError {}

// ---------------------------------------------------------------------
// Track
// ---------------------------------------------------------------------

/// Which pipeline produced this package: built from source locally, or
/// fetched as a prebuilt binary from the repository (§10.2).
///
/// Mirrors the `track` column in the package database — the
/// `CHECK(track IN ('binary', 'source'))` constraint there (§9.2) and this
/// enum should never drift apart. That's exactly why `pkg-db` doesn't just
/// store a raw string: this enum is the single source of truth, and
/// conversion to/from the database's text representation happens at one
/// boundary in `pkg-db`, not scattered across the workspace.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Track {
    Binary,
    Source,
}

impl fmt::Display for Track {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Track::Binary => "binary",
            Track::Source => "source",
        };
        write!(f, "{s}")
    }
}

impl FromStr for Track {
    type Err = TrackParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "binary" => Ok(Track::Binary),
            "source" => Ok(Track::Source),
            other => Err(TrackParseError(other.to_string())),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackParseError(String);

impl fmt::Display for TrackParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid track (expected \"binary\" or \"source\"): {}", self.0)
    }
}

impl std::error::Error for TrackParseError {}

// ---------------------------------------------------------------------
// PackageId
// ---------------------------------------------------------------------

/// The full identity of one installed or installable build: name, version,
/// ABI tag, and architecture together — matching the `UNIQUE(name, version,
/// abi_tag, arch)` constraint on the `packages` table (§9.2).
///
/// `Display` produces the canonical on-disk form used under `.store/`
/// (§6.1), e.g. `openssl-3.0.12+ab1`. Architecture isn't part of that
/// string — multilib packages live under a parallel `lib32/pkg/` tree
/// (§6.1) rather than being disambiguated by filename.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PackageId {
    pub name: String,
    pub version: Version,
    pub abi_tag: AbiTag,
    pub arch: Arch,
}

impl fmt::Display for PackageId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}-{}+{}", self.name, self.version, self.abi_tag)
    }
}

impl PackageId {
    /// Parses a canonical `.store/` directory name (`Display`'s output,
    /// e.g. `"openssl-3.0.12+ab1"`) back into a `PackageId`, given the
    /// architecture separately.
    ///
    /// Deliberately *not* a `FromStr` impl. `Display` above doesn't
    /// encode `arch` at all — architecture is conveyed by which tree a
    /// directory lives under (`.store/` vs `lib32/pkg/`, §6.1), not by
    /// the directory name itself. A `FromStr` impl would have to either
    /// silently guess `arch` (wrong whenever it guessed wrong) or reject
    /// a perfectly valid string, and neither reflects what's actually
    /// true here: the string alone is genuinely insufficient, so the
    /// caller supplies the missing piece explicitly instead of this type
    /// pretending it can recover something it structurally can't.
    ///
    /// The harder part is splitting `name` from `version` at all: both
    /// can legally contain hyphens (`ca-certificates` as a name; `2.39-r1`
    /// as a version's patch suffix, §3.7), so a naive first-`-` or
    /// last-`-` split is wrong for one case or the other. The fix relies
    /// on a fact specific to `Version`'s own grammar: its only possible
    /// internal hyphen is `-r` (a patch suffix), always followed by the
    /// letter `r` — never a digit. So scanning right-to-left for the
    /// last `-` immediately followed by a digit always lands on the true
    /// name/version boundary, regardless of what hyphens `name` itself
    /// contains: nothing inside `version` can produce a false match to
    /// the right of that boundary, so the true boundary is always the
    /// first match found scanning from the right.
    pub fn parse_canonical(s: &str, arch: Arch) -> Result<Self, PackageIdParseError> {
        let (name_and_version, abi_str) = s
            .rsplit_once('+')
            .ok_or_else(|| PackageIdParseError(s.to_string()))?;
        let abi_tag: AbiTag = abi_str
            .parse()
            .map_err(|_| PackageIdParseError(s.to_string()))?;

        let boundary = name_and_version
            .char_indices()
            .rev()
            .find(|&(i, c)| {
                c == '-'
                    && name_and_version[i + 1..]
                        .chars()
                        .next()
                        .map(|next| next.is_ascii_digit())
                        .unwrap_or(false)
            })
            .map(|(i, _)| i)
            .ok_or_else(|| PackageIdParseError(s.to_string()))?;

        let name = &name_and_version[..boundary];
        let version_str = &name_and_version[boundary + 1..];

        if name.is_empty() {
            return Err(PackageIdParseError(s.to_string()));
        }

        let version: Version = version_str
            .parse()
            .map_err(|_| PackageIdParseError(s.to_string()))?;

        Ok(PackageId {
            name: name.to_string(),
            version,
            abi_tag,
            arch,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageIdParseError(String);

impl fmt::Display for PackageIdParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid canonical package id: {}", self.0)
    }
}

impl std::error::Error for PackageIdParseError {}

// ---------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_plain_release() {
        let v: Version = "3.0.12".parse().unwrap();
        assert_eq!(v.to_string(), "3.0.12");
    }

    #[test]
    fn parses_patch_suffix() {
        let v: Version = "2.39-r1".parse().unwrap();
        assert_eq!(v.to_string(), "2.39-r1");
    }

    #[test]
    fn rejects_garbage() {
        assert!("not-a-version".parse::<Version>().is_err());
        assert!("".parse::<Version>().is_err());
    }

    #[test]
    fn orders_release_cores() {
        let a: Version = "2.39".parse().unwrap();
        let b: Version = "2.40".parse().unwrap();
        assert!(a < b);
    }

    #[test]
    fn orders_patch_revisions_within_same_release() {
        let a: Version = "2.39-r1".parse().unwrap();
        let b: Version = "2.39-r2".parse().unwrap();
        assert!(a < b);
    }

    #[test]
    fn bare_release_orders_before_its_own_patches() {
        let base: Version = "2.39".parse().unwrap();
        let patched: Version = "2.39-r1".parse().unwrap();
        assert!(base < patched);
    }

    #[test]
    fn short_and_padded_releases_compare_equal() {
        let a: Version = "2.39".parse().unwrap();
        let b: Version = "2.39.0".parse().unwrap();
        assert_eq!(a.cmp(&b), Ordering::Equal);
    }

    #[test]
    fn equality_follows_ordering_for_padded_releases() {
        // The v1 bug: derived PartialEq said these differ while Ord said
        // Equal. Now equality is defined by cmp.
        let a: Version = "2.39".parse().unwrap();
        let b: Version = "2.39.0".parse().unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn equality_treats_missing_patch_as_r0() {
        let bare: Version = "2.39".parse().unwrap();
        let r0: Version = "2.39-r0".parse().unwrap();
        assert_eq!(bare.cmp(&r0), Ordering::Equal);
        assert_eq!(bare, r0);
    }

    #[test]
    fn equal_versions_hash_identically() {
        use std::collections::hash_map::DefaultHasher;
        fn hash_of(v: &Version) -> u64 {
            let mut h = DefaultHasher::new();
            v.hash(&mut h);
            h.finish()
        }
        let a: Version = "2.39".parse().unwrap();
        let b: Version = "2.39.0".parse().unwrap();
        let c: Version = "2.39-r0".parse().unwrap();
        assert_eq!(hash_of(&a), hash_of(&b));
        assert_eq!(hash_of(&a), hash_of(&c));

        let d: Version = "2.39-r1".parse().unwrap();
        assert_ne!(hash_of(&a), hash_of(&d));
    }

    #[test]
    fn btreeset_deduplicates_equal_versions() {
        use std::collections::BTreeSet;
        let mut set = BTreeSet::new();
        set.insert("2.39".parse::<Version>().unwrap());
        set.insert("2.39.0".parse::<Version>().unwrap());
        set.insert("2.39-r0".parse::<Version>().unwrap());
        set.insert("2.39-r1".parse::<Version>().unwrap());
        set.insert("2.40".parse::<Version>().unwrap());
        assert_eq!(set.len(), 3);
    }

    #[test]
    fn parsing_normalizes_trailing_zero_components() {
        let v: Version = "2.39.0".parse().unwrap();
        assert_eq!(v.to_string(), "2.39");
        // A trailing zero that is the only component is kept.
        let zero: Version = "0.0".parse().unwrap();
        assert_eq!(zero.to_string(), "0");
        // Interior zeros are significant and survive.
        let v: Version = "3.0.12".parse().unwrap();
        assert_eq!(v.to_string(), "3.0.12");
    }

    #[test]
    fn same_release_core_ignores_patch() {
        let a: Version = "2.39".parse().unwrap();
        let b: Version = "2.39-r2".parse().unwrap();
        assert!(a.same_release_core(&b));

        let c: Version = "2.40".parse().unwrap();
        assert!(!a.same_release_core(&c));
    }

    #[test]
    fn same_release_core_uses_padded_comparison() {
        let short: Version = "2.39".parse().unwrap();
        let padded: Version = "2.39.0".parse().unwrap();
        assert!(short.same_release_core(&padded));
        // A real component difference is still a different core.
        let other: Version = "2.39.1".parse().unwrap();
        assert!(!short.same_release_core(&other));
    }

    #[test]
    fn abi_tag_displays_with_prefix() {
        assert_eq!(AbiTag(1).to_string(), "ab1");
        assert_eq!(AbiTag(12).to_string(), "ab12");
    }

    #[test]
    fn abi_tag_round_trips_through_string() {
        let tag = AbiTag(12);
        let parsed: AbiTag = tag.to_string().parse().unwrap();
        assert_eq!(tag, parsed);
        assert!("not-a-tag".parse::<AbiTag>().is_err());
    }

    #[test]
    fn arch_round_trips_through_string() {
        for arch in [Arch::Amd64, Arch::I686] {
            let parsed: Arch = arch.to_string().parse().unwrap();
            assert_eq!(arch, parsed);
        }
        assert!("armhf".parse::<Arch>().is_err());
    }

    #[test]
    fn track_round_trips_through_string() {
        for track in [Track::Binary, Track::Source] {
            let parsed: Track = track.to_string().parse().unwrap();
            assert_eq!(track, parsed);
        }
        assert!("neither".parse::<Track>().is_err());
    }

    #[test]
    fn package_id_matches_store_naming_convention() {
        let id = PackageId {
            name: "openssl".to_string(),
            version: "3.0.12".parse().unwrap(),
            abi_tag: AbiTag(1),
            arch: Arch::Amd64,
        };
        assert_eq!(id.to_string(), "openssl-3.0.12+ab1");
    }

    #[test]
    fn parse_canonical_round_trips_simple_case() {
        let id = PackageId {
            name: "openssl".to_string(),
            version: "3.0.12".parse().unwrap(),
            abi_tag: AbiTag(1),
            arch: Arch::Amd64,
        };
        let parsed = PackageId::parse_canonical(&id.to_string(), Arch::Amd64).unwrap();
        assert_eq!(parsed, id);
    }

    #[test]
    fn parse_canonical_handles_hyphenated_name() {
        let id = PackageId {
            name: "ca-certificates".to_string(),
            version: "3.0.12".parse().unwrap(),
            abi_tag: AbiTag(1),
            arch: Arch::Amd64,
        };
        let parsed = PackageId::parse_canonical(&id.to_string(), Arch::Amd64).unwrap();
        assert_eq!(parsed.name, "ca-certificates");
        assert_eq!(parsed.version.to_string(), "3.0.12");
    }

    #[test]
    fn parse_canonical_handles_hyphenated_patch_version() {
        let id = PackageId {
            name: "glibc".to_string(),
            version: "2.39-r1".parse().unwrap(),
            abi_tag: AbiTag(1),
            arch: Arch::Amd64,
        };
        let parsed = PackageId::parse_canonical(&id.to_string(), Arch::Amd64).unwrap();
        assert_eq!(parsed.name, "glibc");
        assert_eq!(parsed.version.to_string(), "2.39-r1");
    }

    #[test]
    fn parse_canonical_handles_both_hazards_at_once() {
        let id = PackageId {
            name: "ca-certificates".to_string(),
            version: "2.39-r1".parse().unwrap(),
            abi_tag: AbiTag(2),
            arch: Arch::Amd64,
        };
        let parsed = PackageId::parse_canonical(&id.to_string(), Arch::Amd64).unwrap();
        assert_eq!(parsed, id);
    }

    #[test]
    fn parse_canonical_uses_the_supplied_arch_not_a_guess() {
        // Same string, different supplied arch — proving arch genuinely
        // comes from the caller, not from anything recoverable in `s`.
        let s = "openssl-3.0.12+ab1";
        let amd64 = PackageId::parse_canonical(s, Arch::Amd64).unwrap();
        let i686 = PackageId::parse_canonical(s, Arch::I686).unwrap();
        assert_eq!(amd64.arch, Arch::Amd64);
        assert_eq!(i686.arch, Arch::I686);
        assert_eq!(amd64.name, i686.name);
        assert_eq!(amd64.version, i686.version);
    }

    #[test]
    fn parse_canonical_rejects_malformed_strings() {
        assert!(PackageId::parse_canonical("no-abi-tag-here", Arch::Amd64).is_err());
        assert!(PackageId::parse_canonical("+ab1", Arch::Amd64).is_err()); // empty name
        assert!(PackageId::parse_canonical("openssl+ab1", Arch::Amd64).is_err()); // no version
    }
}


// ---------------------------------------------------------------------
// Serde (string-based) — added in v2.1
// ---------------------------------------------------------------------
//
// `VersionConstraint`/`Dependency` derive serde in the virtual module,
// which requires serde impls on the value types they contain. All four
// serialize through their canonical string form (Display/FromStr), so
// TOML pins/profiles/manifests stay human-readable and round-trip
// through the same parser the CLI uses.

mod serde_impls {
    use super::{AbiTag, AbiTagParseError, Arch, ArchParseError, Track, TrackParseError, Version, VersionParseError};
    use serde::{de, Deserialize, Deserializer, Serialize, Serializer};
    use std::fmt;
    use std::marker::PhantomData;
    use std::str::FromStr;

    fn serialize_string<T: fmt::Display, S: Serializer>(value: &T, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&value.to_string())
    }

    fn deserialize_string<'de, T, E, D>(d: D) -> Result<T, D::Error>
    where
        T: FromStr<Err = E>,
        E: fmt::Display,
        D: Deserializer<'de>,
    {
        struct Visitor<T, E>(PhantomData<(T, E)>);
        impl<'de, T, E> de::Visitor<'de> for Visitor<T, E>
        where
            T: FromStr<Err = E>,
            E: fmt::Display,
        {
            type Value = T;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a string in canonical form")
            }
            fn visit_str<Er: de::Error>(self, v: &str) -> Result<T, Er> {
                v.parse::<T>().map_err(de::Error::custom)
            }
        }
        d.deserialize_str(Visitor::<T, E>(PhantomData))
    }

    impl Serialize for Version {
        fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> { serialize_string(self, s) }
    }
    impl<'de> Deserialize<'de> for Version {
        fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
            deserialize_string::<Version, VersionParseError, D>(d)
        }
    }

    impl Serialize for AbiTag {
        fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> { serialize_string(self, s) }
    }
    impl<'de> Deserialize<'de> for AbiTag {
        fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
            deserialize_string::<AbiTag, AbiTagParseError, D>(d)
        }
    }

    impl Serialize for Arch {
        fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> { serialize_string(self, s) }
    }
    impl<'de> Deserialize<'de> for Arch {
        fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
            deserialize_string::<Arch, ArchParseError, D>(d)
        }
    }

    impl Serialize for Track {
        fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> { serialize_string(self, s) }
    }
    impl<'de> Deserialize<'de> for Track {
        fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
            deserialize_string::<Track, TrackParseError, D>(d)
        }
    }
}
