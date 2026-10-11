//! ## v2 changes
//!
//! - [MAJOR] Dependency parsing: an empty OR-alternative like the middle
//!   segment of `"cronie||fcron"` (or the edge segments of `"a|"`, `"|a"`)
//!   previously became a silent `Dependency::any_version("")`. Empty or
//!   whitespace-only alternatives are now rejected with the existing
//!   `ManifestError::EmptyDependency`, checked in `parse_single_dependency`
//!   so both plain expressions and individual `|`-alternatives are covered.
//! - [MAJOR] Doc comments: references to a flat `dkms-module = true` key
//!   were stale — the canonical manifest form is the `[dkms]` table, whose
//!   mere presence signals a DKMS package (see `RawManifest::dkms`). Docs
//!   now consistently describe the `[dkms]` table.

//! `pkg-meta.toml` parsing (§4.1–§4.2).
//!
//! This is the file that ties the rest of `pkg-core` together: a
//! `PackageManifest` is built from `Version` (`package.rs`), `Dependency`
//! and `VirtualName` (`virtual_pkg.rs`), and reports failures through
//! `PkgError` (`error.rs`). If those earlier files were the vocabulary,
//! this one is close to a full sentence.

use std::fmt;

use serde::Deserialize;

// `Dependency`, `VersionConstraint`, and `VirtualName` are defined in
// pkg-core's virtual module (`virtual_pkg.rs`) and re-exported from the
// crate root — not redefined here. `VersionConstraint` includes the
// upper-bound-capable `Range` variant, and all three types are serde
// `Serialize`/`Deserialize`.
use crate::{AbiTag, Arch, Dependency, PackageId, PkgError, PkgResult, VersionConstraint, VirtualName};

// ---------------------------------------------------------------------
// Category
// ---------------------------------------------------------------------

/// The package's category. Only a handful of categories carry special
/// meaning to the package manager itself — they select which hook sandbox
/// profile applies when a package declares `post_install` or similar
/// hooks (§7.2.1). Everything else exists purely for organization, the
/// same way Gentoo's `dev-libs`/`app-editors`/`games-arcade` categories do
/// without the package manager caring which is which.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Category {
    /// Hook sandbox: "None" profile — hooks rejected outright (§7.2.1).
    DevLibs,
    /// Hook sandbox: "None" profile.
    SysLibs,
    /// Hook sandbox: "None" profile.
    MediaLibs,
    /// Hook sandbox: "System" profile — write access to `/boot`,
    /// `/usr/share`, `/var/cache`; read-only `/etc` (§7.2.1).
    SysKernel,
    /// Any category the package manager doesn't branch on. `pkg search`
    /// and organizational tooling can still use these; the sandbox logic
    /// just doesn't look at them.
    Other(String),
}

impl Category {
    /// Converts a manifest's `category` string into a `Category`.
    ///
    /// Unlike `Version`'s `FromStr` impl (§7.7, `package.rs`), this can't
    /// fail — any string that isn't one of the four sandbox-relevant
    /// categories just becomes `Other`. That's why this is a plain
    /// associated function returning `Self`, not a `FromStr` impl
    /// returning `Result`: manufacturing a `Result` for something that
    /// can't actually fail would be ceremony, not correctness.
    pub fn from_name(name: &str) -> Self {
        match name {
            "dev-libs" => Category::DevLibs,
            "sys-libs" => Category::SysLibs,
            "media-libs" => Category::MediaLibs,
            "sys-kernel" => Category::SysKernel,
            other => Category::Other(other.to_string()),
        }
    }
}

impl fmt::Display for Category {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Category::DevLibs => "dev-libs",
            Category::SysLibs => "sys-libs",
            Category::MediaLibs => "media-libs",
            Category::SysKernel => "sys-kernel",
            Category::Other(s) => s,
        };
        write!(f, "{s}")
    }
}

// ---------------------------------------------------------------------
// DkmsInfo
// ---------------------------------------------------------------------

/// DKMS metadata for a package providing an out-of-tree kernel module
/// (§4.1, §7.8). Present only when the manifest contains a `[dkms]`
/// table — the table's presence is itself the "this is a DKMS package"
/// signal; there is no separate boolean flag.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DkmsInfo {
    /// Name DKMS registers the module under, e.g. `"nvidia"`.
    pub dkms_name: String,
    /// Module version passed to DKMS. Deliberately a plain `String`
    /// rather than this crate's `Version` type — DKMS module versions
    /// follow whatever scheme upstream uses (Nvidia's driver versions,
    /// for instance), with no guarantee of fitting `Version`'s
    /// dotted-numeric parsing.
    pub dkms_version: String,
}

// ---------------------------------------------------------------------
// PackageManifest
// ---------------------------------------------------------------------

/// A parsed `pkg-meta.toml` (§4.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageManifest {
    pub name: String,
    pub version: crate::Version,
    pub category: Category,
    /// Whether an i686 build should be produced alongside the amd64 one
    /// (§5.4 multilib co-selection).
    pub multilib: bool,
    /// Virtual packages this package declares itself a provider of — a
    /// kernel package declaring `provides = ["virtual/kernel"]`
    /// (§7.8, §13.6), for instance.
    pub provides: Vec<VirtualName>,
    pub runtime_deps: Vec<Dependency>,
    pub build_deps: Vec<Dependency>,
    /// `Some` only when the manifest has a `[dkms]` table (§7.8).
    pub dkms: Option<DkmsInfo>,
}

impl PackageManifest {
    /// Parses a `pkg-meta.toml` document.
    ///
    /// # Example
    /// ```toml
    /// name = "openssl"
    /// version = "3.0.13"
    /// category = "dev-libs"
    /// multilib = true
    /// provides = ["virtual/tls"]
    ///
    /// [dependencies]
    /// runtime = ["zlib>=1.3", "ca-certificates"]
    /// build = ["perl", "make"]
    /// ```
    pub fn from_toml_str(input: &str) -> PkgResult<Self> {
        let raw: RawManifest =
            toml::from_str(input).map_err(|e| ManifestError::Toml(e.to_string()))?;

        let version: crate::Version = raw
            .version
            .parse()
            .map_err(|e: crate::VersionParseError| ManifestError::InvalidVersion(e.to_string()))?;

        let category = Category::from_name(&raw.category);

        let provides = raw
            .provides
            .iter()
            .map(|s| {
                VirtualName::new(s.clone())
                    .map_err(|e| ManifestError::InvalidVirtual(e.to_string()))
            })
            .collect::<Result<Vec<_>, _>>()?;

        let runtime_deps = raw
            .dependencies
            .runtime
            .iter()
            .map(|s| parse_dependency_expr(s))
            .collect::<Result<Vec<_>, _>>()?;

        let build_deps = raw
            .dependencies
            .build
            .iter()
            .map(|s| parse_dependency_expr(s))
            .collect::<Result<Vec<_>, _>>()?;

        let dkms = raw.dkms.map(|d| DkmsInfo {
            dkms_name: d.name,
            dkms_version: d.version,
        });

        Ok(PackageManifest {
            name: raw.name,
            version,
            category,
            multilib: raw.multilib,
            provides,
            runtime_deps,
            build_deps,
            dkms,
        })
    }

    /// Builds the full `PackageId` (§7.7) for a build of this package,
    /// once its ABI tag and target architecture are known. The manifest
    /// can't produce a `PackageId` on its own — the ABI tag is only
    /// derived after a build actually happens (§7.7), never declared up
    /// front — so this takes both as arguments rather than trying to
    /// compute them.
    pub fn package_id(&self, abi_tag: AbiTag, arch: Arch) -> PackageId {
        PackageId {
            name: self.name.clone(),
            version: self.version.clone(),
            abi_tag,
            arch,
        }
    }
}

// ---------------------------------------------------------------------
// Raw TOML shape (private — see the doc comment on `RawManifest`)
// ---------------------------------------------------------------------

/// The literal shape of `pkg-meta.toml`, deserialized directly by serde.
///
/// This is deliberately a separate, private type from `PackageManifest`
/// rather than deriving `Deserialize` on the public type directly.
/// Dependency strings like `"zlib>=1.3"` or `"cronie|fcron|dcron"` need
/// real parsing logic to become `Dependency` values — more than serde's
/// derive macro can express on its own — so this struct exists purely to
/// receive the raw TOML shape, with `PackageManifest::from_toml_str`
/// doing the validating conversion into the domain type afterward. This
/// is a common split in Rust code that talks to external formats: a
/// "raw" wire-shape type, and a "real" type the rest of the program
/// actually works with.
///
/// `deny_unknown_fields` matters more than it might look here: without
/// it, a bare root-level key placed after a table header it doesn't
/// belong to silently becomes a field of that table instead — TOML has
/// no way to "return to root scope" once a `[header]` opens one, short of
/// giving the misplaced key its own header. `dkms` is exactly that fix,
/// applied structurally: it's its own `[dkms]` table (see `RawDkms`
/// below) rather than three bare top-level keys, so its scope is bounded
/// by its own header and can't be silently absorbed into `[dependencies]`
/// regardless of which comes first in the file. `deny_unknown_fields`
/// is the remaining backstop for anything that isn't — a typo'd or
/// genuinely misplaced key now fails loudly instead of vanishing.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawManifest {
    name: String,
    version: String,
    category: String,
    #[serde(default)]
    multilib: bool,
    #[serde(default)]
    provides: Vec<String>,
    #[serde(default)]
    dependencies: RawDependencies,
    /// The mere presence of this table *is* the "is this a DKMS package"
    /// signal — no separate boolean flag alongside it. A flag whose truth
    /// value has to stay consistent with whether the fields next to it
    /// are actually filled in is its own small hazard, same family as the
    /// scoping issue above: two things that are supposed to agree, with
    /// nothing structurally forcing them to. `Option` already encodes
    /// presence/absence; there's no need to also say so separately.
    #[serde(default)]
    dkms: Option<RawDkms>,
}

/// `name` and `version` are required (not `Option`) — if `[dkms]` appears
/// at all, TOML's own missing-field checking rejects it for lacking
/// either one, rather than the package manager hand-validating that
/// combination itself after the fact (compare the old `MissingDkmsFields`
/// check this replaced: the type system now enforces the invariant
/// serde's derive gives it for free, instead of code re-checking
/// something the shape of the data could already guarantee).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawDkms {
    name: String,
    version: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawDependencies {
    #[serde(default)]
    runtime: Vec<String>,
    #[serde(default)]
    build: Vec<String>,
}

// ---------------------------------------------------------------------
// Dependency expression parsing
// ---------------------------------------------------------------------

/// Parses one entry from a `runtime`/`build` dependency list.
///
/// Grammar, informally:
/// - `"name"` → any version
/// - `"name>=1.2.3"` → minimum version
/// - `"name=1.2.3"` → exact version
/// - `"virtual/name"` → a virtual dependency
/// - `"a|b|c"` → an OR-group (§5.2); each side of `|` is itself parsed by
///   this same grammar, so alternatives can mix plain names, version
///   constraints, and virtuals freely. The `|` separator mirrors Debian's
///   own `Depends:` field convention for alternative packages, chosen for
///   the same reason it works there: it's the one character unlikely to
///   collide with anything else a package name or version string needs.
///
/// Empty alternatives are an error, not a wildcard: `"a||b"`, `"a|"`, and
/// `"|a"` are rejected with `ManifestError::EmptyDependency` rather than
/// silently producing a dependency on the empty package name.
fn parse_dependency_expr(expr: &str) -> Result<Dependency, ManifestError> {
    let expr = expr.trim();
    if expr.is_empty() {
        return Err(ManifestError::EmptyDependency);
    }

    if expr.contains('|') {
        let alternatives = expr
            .split('|')
            .map(parse_single_dependency)
            .collect::<Result<Vec<_>, _>>()?;
        return Ok(Dependency::AnyOf(alternatives));
    }

    parse_single_dependency(expr)
}

fn parse_single_dependency(expr: &str) -> Result<Dependency, ManifestError> {
    let expr = expr.trim();
    // Reject empty alternatives here (not just in `parse_dependency_expr`)
    // so an OR-group like "a||b" — whose `split('|')` yields an empty
    // middle segment — fails loudly instead of becoming
    // `Dependency::any_version("")`, which would silently match nothing
    // (or worse, everything, depending on resolver handling of an empty
    // name).
    if expr.is_empty() {
        return Err(ManifestError::EmptyDependency);
    }

    if expr.starts_with("virtual/") {
        let vname =
            VirtualName::new(expr).map_err(|e| ManifestError::InvalidVirtual(e.to_string()))?;
        return Ok(Dependency::Virtual(vname));
    }

    // Checked before the bare "=" case below, since ">=" itself contains
    // "=" — splitting on "=" first would cut "openssl>=3.0" into
    // "openssl>" and "3.0", which is wrong.
    if let Some((name, version)) = expr.split_once(">=") {
        let version: crate::Version = version
            .trim()
            .parse()
            .map_err(|e: crate::VersionParseError| ManifestError::InvalidVersion(e.to_string()))?;
        return Ok(Dependency::at_least(name.trim(), version));
    }

    if let Some((name, version)) = expr.split_once('=') {
        let version: crate::Version = version
            .trim()
            .parse()
            .map_err(|e: crate::VersionParseError| ManifestError::InvalidVersion(e.to_string()))?;
        return Ok(Dependency::Package {
            name: name.trim().to_string(),
            constraint: VersionConstraint::Exact(version),
        });
    }

    Ok(Dependency::any_version(expr))
}

// ---------------------------------------------------------------------
// ManifestError
// ---------------------------------------------------------------------

/// Errors specific to parsing a manifest, before being folded into the
/// shared `PkgError::Parse` variant (see the `From` impl below). Kept
/// separate from `PkgError` itself so the `Display` formatting for each
/// failure mode lives in one place, rather than being built ad hoc at
/// every call site that can fail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManifestError {
    Toml(String),
    EmptyDependency,
    InvalidVirtual(String),
    InvalidVersion(String),
}

impl fmt::Display for ManifestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ManifestError::Toml(msg) => write!(f, "TOML syntax error: {msg}"),
            ManifestError::EmptyDependency => write!(f, "empty dependency expression"),
            ManifestError::InvalidVirtual(msg) => write!(f, "invalid virtual dependency: {msg}"),
            ManifestError::InvalidVersion(msg) => write!(f, "invalid version in dependency: {msg}"),
        }
    }
}

impl std::error::Error for ManifestError {}

/// This is what lets `?` convert a `ManifestError` into `PkgError`
/// automatically anywhere `from_toml_str` uses it — the same mechanism
/// `error.rs`'s `From<io::Error>` impl set up for I/O failures.
impl From<ManifestError> for PkgError {
    fn from(e: ManifestError) -> Self {
        PkgError::Parse(e.to_string())
    }
}

// ---------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_minimal_manifest() {
        let toml = r#"
            name = "hello"
            version = "1.0.0"
            category = "app-misc"
        "#;
        let m = PackageManifest::from_toml_str(toml).unwrap();
        assert_eq!(m.name, "hello");
        assert_eq!(m.version.to_string(), "1.0.0");
        assert_eq!(m.category, Category::Other("app-misc".to_string()));
        assert!(!m.multilib);
        assert!(m.provides.is_empty());
        assert!(m.runtime_deps.is_empty());
        assert!(m.dkms.is_none());
    }

    #[test]
    fn parses_full_manifest_with_deps_and_provides() {
        let toml = r#"
            name = "openssl"
            version = "3.0.13"
            category = "dev-libs"
            multilib = true
            provides = ["virtual/tls"]

            [dependencies]
            runtime = ["zlib>=1.3", "ca-certificates"]
            build = ["perl", "make"]
        "#;
        let m = PackageManifest::from_toml_str(toml).unwrap();
        assert_eq!(m.category, Category::DevLibs);
        assert!(m.multilib);
        assert_eq!(m.provides, vec![VirtualName::new("virtual/tls").unwrap()]);
        assert_eq!(
            m.runtime_deps,
            vec![
                Dependency::at_least("zlib", "1.3".parse().unwrap()),
                Dependency::any_version("ca-certificates"),
            ]
        );
        assert_eq!(m.build_deps, vec![
            Dependency::any_version("perl"),
            Dependency::any_version("make"),
        ]);
    }

    #[test]
    fn parses_or_group_dependency() {
        let toml = r#"
            name = "cron-meta"
            version = "1.0.0"
            category = "sys-process"

            [dependencies]
            runtime = ["cronie|fcron|dcron"]
        "#;
        let m = PackageManifest::from_toml_str(toml).unwrap();
        match &m.runtime_deps[0] {
            Dependency::AnyOf(alts) => assert_eq!(alts.len(), 3),
            other => panic!("expected AnyOf, got {other:?}"),
        }
    }

    #[test]
    fn rejects_empty_alternative_in_or_group() {
        // "a||b": the empty middle segment must not become an
        // any_version("") dependency.
        assert_eq!(
            parse_dependency_expr("a||b"),
            Err(ManifestError::EmptyDependency)
        );
    }

    #[test]
    fn rejects_trailing_separator_in_or_group() {
        assert_eq!(
            parse_dependency_expr("a|"),
            Err(ManifestError::EmptyDependency)
        );
    }

    #[test]
    fn rejects_leading_separator_in_or_group() {
        assert_eq!(
            parse_dependency_expr("|a"),
            Err(ManifestError::EmptyDependency)
        );
    }

    #[test]
    fn rejects_whitespace_only_dependency() {
        assert_eq!(
            parse_dependency_expr("   "),
            Err(ManifestError::EmptyDependency)
        );
        assert_eq!(
            parse_dependency_expr("a|  |b"),
            Err(ManifestError::EmptyDependency)
        );
    }

    #[test]
    fn empty_or_alternative_fails_whole_manifest() {
        let toml = r#"
            name = "cron-meta"
            version = "1.0.0"
            category = "sys-process"

            [dependencies]
            runtime = ["cronie||fcron"]
        "#;
        let err = PackageManifest::from_toml_str(toml).unwrap_err();
        assert!(matches!(err, PkgError::Parse(_)));
        assert!(err.to_string().contains("empty dependency expression"));
    }

    #[test]
    fn parses_virtual_dependency_shorthand() {
        let toml = r#"
            name = "nvidia-driver"
            version = "550.78"
            category = "sys-kernel"

            [dependencies]
            runtime = ["virtual/kernel"]
        "#;
        let m = PackageManifest::from_toml_str(toml).unwrap();
        assert!(matches!(m.runtime_deps[0], Dependency::Virtual(_)));
    }

    #[test]
    fn parses_exact_version_constraint() {
        let toml = r#"
            name = "example"
            version = "1.0.0"
            category = "app-misc"

            [dependencies]
            runtime = ["glibc=2.39"]
        "#;
        let m = PackageManifest::from_toml_str(toml).unwrap();
        assert_eq!(
            m.runtime_deps[0],
            Dependency::Package {
                name: "glibc".to_string(),
                constraint: VersionConstraint::Exact("2.39".parse().unwrap()),
            }
        );
    }

    #[test]
    fn dkms_fields_parsed_when_present() {
        let toml = r#"
            name = "nvidia-driver"
            version = "550.78"
            category = "sys-kernel"

            [dependencies]
            runtime = ["virtual/kernel"]
            build = ["gcc", "linux-headers"]

            [dkms]
            name = "nvidia"
            version = "550.78"
        "#;
        // [dkms] declared *after* [dependencies] — the exact ordering
        // that broke the old flat-key design, now safe because [dkms]
        // has its own header and TOML scopes it correctly regardless of
        // where it appears.
        let m = PackageManifest::from_toml_str(toml).unwrap();
        let dkms = m.dkms.expect("expected dkms info to be present");
        assert_eq!(dkms.dkms_name, "nvidia");
        assert_eq!(dkms.dkms_version, "550.78");
    }

    #[test]
    fn dkms_table_requires_both_fields() {
        let toml = r#"
            name = "nvidia-driver"
            version = "550.78"
            category = "sys-kernel"

            [dkms]
            name = "nvidia"
        "#;
        // [dkms] present, but its required "version" field is missing —
        // serde rejects this on its own; no hand-written validation
        // needed for the combination.
        let err = PackageManifest::from_toml_str(toml).unwrap_err();
        assert!(matches!(err, PkgError::Parse(_)));
    }

    #[test]
    fn manifest_without_dkms_table_has_no_dkms_info() {
        let toml = r#"
            name = "hello"
            version = "1.0.0"
            category = "app-misc"
        "#;
        let m = PackageManifest::from_toml_str(toml).unwrap();
        assert!(m.dkms.is_none());
    }

    #[test]
    fn rejects_invalid_toml_syntax() {
        let err = PackageManifest::from_toml_str("this is not valid toml {{{").unwrap_err();
        assert!(matches!(err, PkgError::Parse(_)));
    }

    #[test]
    fn rejects_invalid_version_in_dependency() {
        let toml = r#"
            name = "example"
            version = "1.0.0"
            category = "app-misc"

            [dependencies]
            runtime = ["zlib>=not-a-version"]
        "#;
        assert!(PackageManifest::from_toml_str(toml).is_err());
    }

    #[test]
    fn package_id_uses_manifest_name_and_version() {
        let toml = r#"
            name = "openssl"
            version = "3.0.13"
            category = "dev-libs"
        "#;
        let m = PackageManifest::from_toml_str(toml).unwrap();
        let id = m.package_id(AbiTag(1), Arch::Amd64);
        assert_eq!(id.to_string(), "openssl-3.0.13+ab1");
    }
}
