//! ## v5 changes
//!
//! - [FIX] `impl fmt::Display for ManifestError` gained the missing
//!   `UnknownOption` arm — the variant existed and was returned by
//!   `dep_set`/`opt_tag`/`options_export_env`, but its absence from the
//!   non-exhaustive `match` made the crate fail to compile (E0004).
//!
//! ## v4 changes
//!
//! - [FIX] `opt_tag` now delegates hashing to `OptTag::from_selection`
//!   (package.rs v3) over the deviation set — ABI-affecting options whose
//!   effective value differs from their declared default — instead of
//!   re-implementing the canonical serialization locally with `true`/
//!   `false` value spelling. The two implementations disagreed on value
//!   spelling (`0`/`1` vs `true`/`false`), which would have made store
//!   tags computed at resolve time diverge from tags computed at build
//!   time. The `0`/`1` spelling (shared with `PKG_OPT_<NAME>` exports)
//!   and the single hashing site in package.rs are now authoritative.
//!
//! ## v3 changes
//!
//! - [MAJOR] `DependencyKind { Link, Runtime, Build }` (default `Link`),
//!   added alongside the existing `runtime_deps`/`build_deps` model.
//!   `[dependencies]` gains a `link` array, and entries in *any* of the
//!   `link`/`runtime`/`build` arrays may now be either the v2 plain-string
//!   form or an inline table `{ dep = "zlib>=1.3", kind = "runtime" }`.
//!   A table entry's `kind` is optional: when omitted it defaults to the
//!   kind implied by the array it appears in (`link` array → `Link`,
//!   matching the type's `Default`), and when given it wins over the
//!   array's implied kind. v2 manifests parse to exactly the same
//!   `PackageManifest` as before — plain strings keep their array's kind.
//! - [MAJOR] `[options.<name>]` tables: `{ description, default: bool,
//!   abi_affecting = true, add_deps = [], add_build_deps = [] }`. The
//!   struct shape enforces additive-only options — there are no fields
//!   for option-to-option logic (`requires`/`conflicts`/`implies`), and
//!   `RawOption`'s `deny_unknown_fields` rejects any such keys as unknown
//!   at parse time. `add_deps`/`add_build_deps` entries use the same
//!   dependency-expression grammar as `[dependencies]`.
//! - [MAJOR] `[build]` table: `compiler_cache: bool = true`,
//!   `critical: bool = false` (supply-chain critical set marker).
//! - [MAJOR] Selection-driven accessors: `dep_set(selection)` (base
//!   link+runtime deps plus `add_deps` of enabled options),
//!   `build_dep_set(selection)` (base build deps plus `add_build_deps`),
//!   `opt_tag(selection)` (the `OptTag` contract from package.rs v3:
//!   8-hex truncated SHA-256 of the canonical sorted `name=value,` list
//!   over **ABI-affecting** options only — superseded in v4 to delegate
//!   to `OptTag::from_selection` over the deviation set; EMPTY string
//!   when every ABI-affecting option sits at its declared default —
//!   including the no-options case), and
//!   `options_export_env(selection)` (`PKG_OPT_<NAME>=0|1`, name
//!   uppercased, `-`→`_`, every declared option exported). All three
//!   validate selection keys against the declared options and fail with
//!   the new `ManifestError::UnknownOption` on any key that isn't, so a
//!   typo'd `--opt foo=bar` can never silently build the wrong thing.
//!
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

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};

// `Dependency`, `VersionConstraint`, and `VirtualName` are defined in
// pkg-core's virtual module (`virtual_pkg.rs`) and re-exported from the
// crate root — not redefined here. `VersionConstraint` includes the
// upper-bound-capable `Range` variant, and all three types are serde
// `Serialize`/`Deserialize`. `OptTag` comes from package.rs v3 (the
// SPEC-defined `OptTag(pub String)` newtype); this file computes the
// 8-hex truncated SHA-256 that fills it.
use crate::{
    AbiTag, Arch, Dependency, OptTag, PackageId, PkgError, PkgResult, VersionConstraint, VirtualName,
};

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
// DependencyKind (v3)
// ---------------------------------------------------------------------

/// How a declared dependency edge is consumed (SPEC "pkg-core contracts",
/// manifest v3).
///
/// - `Link` — the default. The package is linked against this dependency's
///   libraries at build time and needs them present at runtime; its
///   `link_interface_hash` (pkg-scanner elf-v2) is what a cascade bump
///   keys off.
/// - `Runtime` — needed to *use* the package, but nothing is linked
///   against it (data files, executables invoked at runtime, etc.).
/// - `Build` — needed only while building the package itself.
///
/// The serialized form is the lowercase variant name (`"link"`,
/// `"runtime"`, `"build"`), matching the `CHECK` constraint pkg-db's
/// schema-v4 applies to its own `dependencies.kind` column — this enum is
/// the single source of truth for that vocabulary, the same way `Track`
/// (package.rs) is for the `track` column.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DependencyKind {
    Link,
    Runtime,
    Build,
}

impl Default for DependencyKind {
    fn default() -> Self {
        DependencyKind::Link
    }
}

impl fmt::Display for DependencyKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            DependencyKind::Link => "link",
            DependencyKind::Runtime => "runtime",
            DependencyKind::Build => "build",
        };
        write!(f, "{s}")
    }
}

// ---------------------------------------------------------------------
// ManifestOption (v3)
// ---------------------------------------------------------------------

/// One declared package option — a `[options.<name>]` table (SPEC
/// "pkg-core contracts", manifest v3).
///
/// Options are **additive only**, and the struct shape is what enforces
/// it: an option can add dependencies (`add_deps`, `add_build_deps`) when
/// enabled, and nothing else. There is deliberately no `requires`,
/// `conflicts`, or `implies` field — option-to-option logic is rejected
/// as an unknown key at parse time (`RawOption` carries
/// `deny_unknown_fields`) rather than being representable here and
/// validated later.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestOption {
    /// Human-readable explanation of what the option toggles, shown by
    /// `pkg info`. Required in the TOML — an option nobody can describe
    /// is an option nobody should flip.
    pub description: String,
    /// The value used when the user's selection says nothing about this
    /// option. Required: an option whose default lived somewhere else
    /// would make "all-default selection" unanswerable, and `opt_tag`
    /// depends on that question having a local, unambiguous answer.
    pub default: bool,
    /// Whether toggling this option can change the built package's ABI
    /// (default `true`). Only ABI-affecting options feed into
    /// `opt_tag` — a non-ABI-affecting option (docs, locales, a pure
    /// runtime-data toggle) must not force dependents into a rebuild
    /// cascade by changing the tag that ends up in `Identity`'s store
    /// directory name.
    pub abi_affecting: bool,
    /// Dependencies (link-or-runtime sense) added to `dep_set` when the
    /// option is enabled.
    pub add_deps: Vec<Dependency>,
    /// Dependencies added to `build_dep_set` when the option is enabled.
    pub add_build_deps: Vec<Dependency>,
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
    /// `Link`-kind dependencies (v3): from `[dependencies] link = [...]`,
    /// plus any inline-table entry elsewhere whose explicit `kind` said so.
    pub link_deps: Vec<Dependency>,
    pub runtime_deps: Vec<Dependency>,
    pub build_deps: Vec<Dependency>,
    /// Declared options from `[options.<name>]` tables (v3), keyed by
    /// option name. A `BTreeMap` so every accessor that iterates it
    /// (`dep_set`, `opt_tag`, `options_export_env`) is deterministic.
    pub options: BTreeMap<String, ManifestOption>,
    /// `[build] compiler_cache` (v3, default `true`) — whether this
    /// package's builds may populate/consult the compiler cache.
    pub compiler_cache: bool,
    /// `[build] critical` (v3, default `false`) — marks membership in the
    /// supply-chain critical set, the set whose builds get stricter
    /// provenance/attestation treatment downstream.
    pub critical: bool,
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

        // v3: every entry in any of the three arrays carries a
        // `DependencyKind` — implied by the array for plain strings
        // (preserving v2 behavior exactly), overridable per-entry by an
        // inline table's explicit `kind`. The kind then decides which of
        // the three public lists the parsed dep lands in.
        let mut link_deps = Vec::new();
        let mut runtime_deps = Vec::new();
        let mut build_deps = Vec::new();
        for (entry, implied) in raw
            .dependencies
            .link
            .iter()
            .map(|e| (e, DependencyKind::Link))
            .chain(raw.dependencies.runtime.iter().map(|e| (e, DependencyKind::Runtime)))
            .chain(raw.dependencies.build.iter().map(|e| (e, DependencyKind::Build)))
        {
            let (dep, kind) = parse_dep_entry(entry, implied)?;
            match kind {
                DependencyKind::Link => link_deps.push(dep),
                DependencyKind::Runtime => runtime_deps.push(dep),
                DependencyKind::Build => build_deps.push(dep),
            }
        }

        let options = raw
            .options
            .into_iter()
            .map(|(name, opt)| {
                let add_deps = opt
                    .add_deps
                    .iter()
                    .map(|s| parse_dependency_expr(s))
                    .collect::<Result<Vec<_>, _>>()?;
                let add_build_deps = opt
                    .add_build_deps
                    .iter()
                    .map(|s| parse_dependency_expr(s))
                    .collect::<Result<Vec<_>, _>>()?;
                Ok((
                    name,
                    ManifestOption {
                        description: opt.description,
                        default: opt.default,
                        abi_affecting: opt.abi_affecting,
                        add_deps,
                        add_build_deps,
                    },
                ))
            })
            .collect::<Result<BTreeMap<_, _>, ManifestError>>()?;

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
            link_deps,
            runtime_deps,
            build_deps,
            options,
            compiler_cache: raw.build.compiler_cache,
            critical: raw.build.critical,
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

    // -----------------------------------------------------------------
    // v3: option selection
    // -----------------------------------------------------------------

    /// Hard-fails with `ManifestError::UnknownOption` if `selection`
    /// names an option the manifest never declared in any
    /// `[options.<name>]` table. Every selection-driven accessor below
    /// runs this first — a typo'd `--opt foo=bar` must surface as an
    /// error, never as a silently ignored flag building the wrong thing.
    fn validate_selection(&self, selection: &BTreeMap<String, bool>) -> Result<(), ManifestError> {
        for key in selection.keys() {
            if !self.options.contains_key(key) {
                return Err(ManifestError::UnknownOption(key.clone()));
            }
        }
        Ok(())
    }

    /// The effective value of a declared option under `selection`:
    /// the selection's explicit override when present, the declared
    /// `default` otherwise.
    fn option_enabled(&self, name: &str, selection: &BTreeMap<String, bool>) -> bool {
        match selection.get(name) {
            Some(&value) => value,
            None => self.options.get(name).map(|o| o.default).unwrap_or(false),
        }
    }

    /// The full dependency set for a build under `selection` (v3):
    /// the base `Link` and `Runtime` deps, plus the `add_deps` of every
    /// *enabled* option. Options are additive-only by construction (see
    /// `ManifestOption`), so this is the complete answer — nothing an
    /// option can declare ever *removes* a base dep.
    ///
    /// Build-kind deps are deliberately excluded: they aren't part of
    /// the set that has to be installed to *use* the package (see
    /// `build_dep_set`).
    pub fn dep_set(
        &self,
        selection: &BTreeMap<String, bool>,
    ) -> Result<Vec<Dependency>, ManifestError> {
        self.validate_selection(selection)?;
        let mut deps = self.link_deps.clone();
        deps.extend(self.runtime_deps.iter().cloned());
        for (name, option) in &self.options {
            if self.option_enabled(name, selection) {
                deps.extend(option.add_deps.iter().cloned());
            }
        }
        Ok(deps)
    }

    /// The build-time counterpart to `dep_set`: the base `Build` deps
    /// plus the `add_build_deps` of every enabled option. Not part of
    /// the SPEC's enumerated v3 API, but the only consumer-shaped answer
    /// for `add_build_deps` — without it that field would parse and then
    /// be readable by nothing.
    pub fn build_dep_set(
        &self,
        selection: &BTreeMap<String, bool>,
    ) -> Result<Vec<Dependency>, ManifestError> {
        self.validate_selection(selection)?;
        let mut deps = self.build_deps.clone();
        for (name, option) in &self.options {
            if self.option_enabled(name, selection) {
                deps.extend(option.add_build_deps.iter().cloned());
            }
        }
        Ok(deps)
    }

    /// The `OptTag` for a build under `selection` (v3), per the contract
    /// package.rs defines for the type: the 8-hex truncated SHA-256 of
    /// the canonical sorted `name=value,` list — and the **empty** tag
    /// when every option sits at its declared default, which is exactly
    /// the case `Identity::store_dir_name` renders without a `+p` suffix.
    ///
    /// Two decisions pinned here so every caller agrees on them:
    ///
    /// - Only `abi_affecting` options participate. A non-ABI-affecting
    ///   option can't change the built package's interface, so letting
    ///   it change the tag would cascade pointless rebuilds of every
    ///   dependent. Corollary: an all-default *ABI-affecting* selection
    ///   yields the empty tag even if a non-ABI-affecting option is
    ///   flipped away from its default.
    /// - The canonical list covers exactly the ABI-affecting options
    ///   whose effective value *deviates from their declared default*,
    ///   rendered `name=0,` / `name=1,` — the same spelling the
    ///   `PKG_OPT_<NAME>` exports use. An all-default selection is an
    ///   empty deviation set, which is precisely the empty-tag case.
    /// - The hashing itself is delegated to [`OptTag::from_selection`]
    ///   so the canonical serialization and digest exist in exactly one
    ///   place (package.rs v3); this function's job is only the
    ///   default-aware filtering that `OptTag` cannot do, since option
    ///   defaults live here.
    pub fn opt_tag(&self, selection: &BTreeMap<String, bool>) -> Result<OptTag, ManifestError> {
        self.validate_selection(selection)?;
        let mut deviations: BTreeMap<String, bool> = BTreeMap::new();
        for (name, option) in &self.options {
            if !option.abi_affecting {
                continue;
            }
            let value = self.option_enabled(name, selection);
            if value != option.default {
                deviations.insert(name.clone(), value);
            }
        }
        Ok(OptTag::from_selection(&deviations))
    }

    /// The environment a build runs with for this option selection (v3):
    /// one `PKG_OPT_<NAME>=0|1` pair per declared option — including
    /// options the selection didn't mention, exported at their default,
    /// so recipe scripts can test `$PKG_OPT_SSL` unconditionally rather
    /// than distinguishing "unset" from "off". `<NAME>` is the option
    /// name uppercased with `-` mapped to `_` (shell-unfriendly
    /// characters removed from a spot recipes would trip over).
    ///
    /// Iterating the `BTreeMap` keeps the output sorted by option name,
    /// so the result is deterministic for a given manifest+selection.
    pub fn options_export_env(
        &self,
        selection: &BTreeMap<String, bool>,
    ) -> Result<Vec<(String, String)>, ManifestError> {
        self.validate_selection(selection)?;
        Ok(self
            .options
            .keys()
            .map(|name| {
                let key = format!("PKG_OPT_{}", name.to_uppercase().replace('-', "_"));
                let value = if self.option_enabled(name, selection) {
                    "1"
                } else {
                    "0"
                }
                .to_string();
                (key, value)
            })
            .collect())
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
    /// v3: `[options.<name>]` tables. Keyed by option name; each value is
    /// validated for shape by `RawOption`'s `deny_unknown_fields` and its
    /// dependency expressions by `parse_dependency_expr` during the
    /// conversion in `from_toml_str`.
    #[serde(default)]
    options: BTreeMap<String, RawOption>,
    /// v3: `[build]` table. `RawBuild`'s `Default` carries the SPEC
    /// defaults (`compiler_cache = true`, `critical = false`), so a
    /// manifest without the table behaves identically to one spelling
    /// them out.
    #[serde(default)]
    build: RawBuild,
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
    /// v3: link-kind deps. Strings and inline tables both accepted.
    #[serde(default)]
    link: Vec<RawDepEntry>,
    #[serde(default)]
    runtime: Vec<RawDepEntry>,
    #[serde(default)]
    build: Vec<RawDepEntry>,
}

/// One entry in a `[dependencies]` array (v3). The v2 form is a bare
/// string (`"zlib>=1.3"`); the v3 form is an inline table
/// (`{ dep = "zlib>=1.3", kind = "runtime" }`) whose `kind` is optional.
/// `untagged` is what lets both live in the same array; the cost is that
/// `deny_unknown_fields` can't reach inside the table variant (serde
/// buffers untagged content before matching), so an unknown key in a
/// table entry is ignored rather than rejected — documented rather than
/// hidden, and the entry's *kind* is still validated strictly by the
/// `DependencyKind` deserializer.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum RawDepEntry {
    Simple(String),
    Detailed(RawDepEntryDetailed),
}

#[derive(Debug, Deserialize)]
struct RawDepEntryDetailed {
    dep: String,
    #[serde(default)]
    kind: Option<DependencyKind>,
}

/// v3: the literal shape of one `[options.<name>]` table.
///
/// `deny_unknown_fields` is load-bearing here: it is what rejects
/// option-to-option logic fields (`requires`, `conflicts`, `implies`, …)
/// as unknown keys at parse time, keeping options additive-only *by
/// structure* rather than by a hand-written validation pass.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawOption {
    description: String,
    default: bool,
    #[serde(default = "default_true")]
    abi_affecting: bool,
    #[serde(default)]
    add_deps: Vec<String>,
    #[serde(default)]
    add_build_deps: Vec<String>,
}

/// v3: the literal shape of the `[build]` table.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawBuild {
    #[serde(default = "default_true")]
    compiler_cache: bool,
    #[serde(default)]
    critical: bool,
}

/// `Default` is hand-written (not derived) because the SPEC default for
/// `compiler_cache` is `true` — a derived impl would give `false`.
impl Default for RawBuild {
    fn default() -> Self {
        RawBuild {
            compiler_cache: true,
            critical: false,
        }
    }
}

fn default_true() -> bool {
    true
}

/// Parses one `[dependencies]` array entry into a `(Dependency,
/// DependencyKind)` pair. A plain-string entry takes the kind implied by
/// the array it appears in; an inline-table entry's explicit `kind` wins
/// when present, and falls back to the implied kind when omitted. The
/// dependency expression itself goes through `parse_dependency_expr`
/// either way, so both forms share one grammar and one set of errors.
fn parse_dep_entry(
    entry: &RawDepEntry,
    implied: DependencyKind,
) -> Result<(Dependency, DependencyKind), ManifestError> {
    match entry {
        RawDepEntry::Simple(expr) => Ok((parse_dependency_expr(expr)?, implied)),
        RawDepEntry::Detailed(detailed) => Ok((
            parse_dependency_expr(&detailed.dep)?,
            detailed.kind.unwrap_or(implied),
        )),
    }
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
    /// A selection passed to `dep_set`/`opt_tag`/`options_export_env`
    /// named an option the manifest never declared in `[options.<name>]`
    /// (v3). A hard error rather than a silent no-op: an unrecognized
    /// option almost always means a typo, and silently ignoring it would
    /// build the wrong configuration while looking like success.
    UnknownOption(String),
}

impl fmt::Display for ManifestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ManifestError::Toml(msg) => write!(f, "TOML syntax error: {msg}"),
            ManifestError::EmptyDependency => write!(f, "empty dependency expression"),
            ManifestError::InvalidVirtual(msg) => write!(f, "invalid virtual dependency: {msg}"),
            ManifestError::InvalidVersion(msg) => write!(f, "invalid version in dependency: {msg}"),
            ManifestError::UnknownOption(name) => {
                write!(f, "unknown option in selection: '{name}'")
            }
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
        // FromStr normalizes trailing zero components: "1.0.0" stores as
        // [1] and displays canonically as "1".
        assert_eq!(m.version.to_string(), "1");
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

    // --- v3: dependency kinds ---

    #[test]
    fn parses_link_array_and_inline_table_entries() {
        let toml = r#"
            name = "example"
            version = "1.0.0"
            category = "app-misc"

            [dependencies]
            link = ["libssl>=3.0", { dep = "ca-certificates", kind = "runtime" }, { dep = "perl", kind = "build" }]
            runtime = ["wget"]
            build = ["make", { dep = "libz" }]
        "#;
        let m = PackageManifest::from_toml_str(toml).unwrap();
        assert_eq!(
            m.link_deps,
            vec![Dependency::at_least("libssl", "3.0".parse().unwrap())]
        );
        // The explicit `kind` on an inline-table entry wins over the
        // array it appears in; "wget" is a v2 string entry and keeps the
        // runtime array's implied kind.
        assert_eq!(m.runtime_deps, vec![
            Dependency::any_version("ca-certificates"),
            Dependency::any_version("wget"),
        ]);
        // { dep = "libz" } has no explicit kind: the build array's
        // implied kind applies.
        assert_eq!(m.build_deps, vec![
            Dependency::any_version("perl"),
            Dependency::any_version("make"),
            Dependency::any_version("libz"),
        ]);
    }

    #[test]
    fn kind_defaults_to_link_for_bare_link_array_strings() {
        let toml = r#"
            name = "example"
            version = "1.0.0"
            category = "app-misc"

            [dependencies]
            link = ["zlib"]
        "#;
        let m = PackageManifest::from_toml_str(toml).unwrap();
        assert_eq!(m.link_deps, vec![Dependency::any_version("zlib")]);
        assert!(m.runtime_deps.is_empty());
        assert!(m.build_deps.is_empty());
    }

    #[test]
    fn dependency_kind_serializes_lowercase_and_defaults_to_link() {
        #[derive(Deserialize)]
        struct KindWrap {
            kind: DependencyKind,
        }
        assert_eq!(DependencyKind::default(), DependencyKind::Link);
        assert_eq!(DependencyKind::Build.to_string(), "build");
        let w: KindWrap = toml::from_str("kind = \"runtime\"").unwrap();
        assert_eq!(w.kind, DependencyKind::Runtime);
        assert!(toml::from_str::<KindWrap>("kind = \"binary\"").is_err());
    }

    #[test]
    fn rejects_garbage_dependency_kind() {
        let toml = r#"
            name = "example"
            version = "1.0.0"
            category = "app-misc"

            [dependencies]
            link = [{ dep = "zlib", kind = "maybe" }]
        "#;
        assert!(PackageManifest::from_toml_str(toml).is_err());
    }

    // --- v3: options parsing ---

    const OPTIONS_TOML: &str = r#"
        name = "openssl"
        version = "3.0.13"
        category = "dev-libs"

        [dependencies]
        link = ["zlib>=1.3"]
        runtime = ["ca-certificates"]
        build = ["perl"]

        [options.ssl-weak]
        description = "Enable legacy TLS versions"
        default = false
        add_deps = ["openssl-legacy"]

        [options.docs]
        description = "Install documentation"
        default = true
        abi_affecting = false
        add_build_deps = ["doxygen"]
    "#;

    #[test]
    fn parses_options_with_spec_defaults() {
        let m = PackageManifest::from_toml_str(OPTIONS_TOML).unwrap();
        assert_eq!(m.options.len(), 2);

        let weak = &m.options["ssl-weak"];
        assert_eq!(weak.description, "Enable legacy TLS versions");
        assert!(!weak.default);
        assert!(weak.abi_affecting, "abi_affecting defaults to true");
        assert_eq!(weak.add_deps, vec![Dependency::any_version("openssl-legacy")]);
        assert!(weak.add_build_deps.is_empty());

        let docs = &m.options["docs"];
        assert!(docs.default);
        assert!(!docs.abi_affecting);
        assert_eq!(docs.add_build_deps, vec![Dependency::any_version("doxygen")]);
    }

    #[test]
    fn rejects_option_to_option_logic_fields_as_unknown_keys() {
        // `requires` is exactly the kind of option-to-option logic the
        // additive-only model refuses to represent; RawOption's
        // deny_unknown_fields turns it into a parse error.
        let toml = r#"
            name = "example"
            version = "1.0.0"
            category = "app-misc"

            [options.ssl]
            description = "TLS support"
            default = true
            requires = ["zlib"]
        "#;
        let err = PackageManifest::from_toml_str(toml).unwrap_err();
        assert!(matches!(err, PkgError::Parse(_)));
    }

    #[test]
    fn rejects_invalid_dependency_expr_inside_option_add_deps() {
        let toml = r#"
            name = "example"
            version = "1.0.0"
            category = "app-misc"

            [options.ssl]
            description = "TLS support"
            default = false
            add_deps = ["a||b"]
        "#;
        let err = PackageManifest::from_toml_str(toml).unwrap_err();
        assert!(err.to_string().contains("empty dependency expression"));
    }

    // --- v3: [build] table ---

    #[test]
    fn build_table_defaults_apply_when_absent() {
        let toml = r#"
            name = "hello"
            version = "1.0.0"
            category = "app-misc"
        "#;
        let m = PackageManifest::from_toml_str(toml).unwrap();
        assert!(m.compiler_cache, "compiler_cache defaults to true");
        assert!(!m.critical, "critical defaults to false");
    }

    #[test]
    fn build_table_values_parse_when_present() {
        let toml = r#"
            name = "hello"
            version = "1.0.0"
            category = "app-misc"

            [build]
            compiler_cache = false
            critical = true
        "#;
        let m = PackageManifest::from_toml_str(toml).unwrap();
        assert!(!m.compiler_cache);
        assert!(m.critical);
    }

    // --- v3: selection-driven accessors ---

    fn selection(pairs: &[(&str, bool)]) -> BTreeMap<String, bool> {
        pairs.iter().map(|(k, v)| (k.to_string(), *v)).collect()
    }

    #[test]
    fn dep_set_is_base_deps_when_options_sit_at_defaults() {
        let m = PackageManifest::from_toml_str(OPTIONS_TOML).unwrap();
        let deps = m.dep_set(&BTreeMap::new()).unwrap();
        assert_eq!(deps, vec![
            Dependency::at_least("zlib", "1.3".parse().unwrap()),
            Dependency::any_version("ca-certificates"),
        ]);
    }

    #[test]
    fn dep_set_adds_enabled_option_deps_additively() {
        let m = PackageManifest::from_toml_str(OPTIONS_TOML).unwrap();
        let deps = m.dep_set(&selection(&[("ssl-weak", true)])).unwrap();
        assert!(deps.contains(&Dependency::any_version("openssl-legacy")));
        assert!(deps.contains(&Dependency::at_least("zlib", "1.3".parse().unwrap())));
        // Disabling an option that defaults on removes its additions,
        // but never any base dep — options are additive-only.
        let deps_off = m.dep_set(&selection(&[("docs", false)])).unwrap();
        assert_eq!(deps_off.len(), 2);
    }

    #[test]
    fn build_dep_set_adds_enabled_option_build_deps() {
        let m = PackageManifest::from_toml_str(OPTIONS_TOML).unwrap();
        // docs defaults to true → doxygen present at default selection.
        let base = m.build_dep_set(&BTreeMap::new()).unwrap();
        assert_eq!(base, vec![
            Dependency::any_version("perl"),
            Dependency::any_version("doxygen"),
        ]);
        let without = m.build_dep_set(&selection(&[("docs", false)])).unwrap();
        assert_eq!(without, vec![Dependency::any_version("perl")]);
    }

    #[test]
    fn selection_with_unknown_option_is_a_hard_error() {
        let m = PackageManifest::from_toml_str(OPTIONS_TOML).unwrap();
        let sel = selection(&[("nls", true)]);
        assert_eq!(
            m.dep_set(&sel),
            Err(ManifestError::UnknownOption("nls".to_string()))
        );
        assert_eq!(
            m.opt_tag(&sel),
            Err(ManifestError::UnknownOption("nls".to_string()))
        );
        assert_eq!(
            m.options_export_env(&sel),
            Err(ManifestError::UnknownOption("nls".to_string()))
        );
        assert_eq!(
            m.build_dep_set(&sel),
            Err(ManifestError::UnknownOption("nls".to_string()))
        );
        assert!(ManifestError::UnknownOption("nls".to_string())
            .to_string()
            .contains("nls"));
    }

    #[test]
    fn opt_tag_is_empty_for_all_default_selection() {
        let m = PackageManifest::from_toml_str(OPTIONS_TOML).unwrap();
        assert_eq!(m.opt_tag(&BTreeMap::new()).unwrap(), OptTag(String::new()));
        // Explicitly restating the defaults is still all-default.
        assert_eq!(
            m.opt_tag(&selection(&[("ssl-weak", false), ("docs", true)])).unwrap(),
            OptTag(String::new())
        );
        // Flipping only a NON-abi-affecting option still yields the empty
        // tag — it can't change the built package's ABI.
        assert_eq!(
            m.opt_tag(&selection(&[("docs", false)])).unwrap(),
            OptTag(String::new())
        );
    }

    #[test]
    fn opt_tag_is_deterministic_and_changes_with_abi_affecting_selection() {
        let m = PackageManifest::from_toml_str(OPTIONS_TOML).unwrap();
        let on = m.opt_tag(&selection(&[("ssl-weak", true)])).unwrap();
        let on_again = m.opt_tag(&selection(&[("ssl-weak", true)])).unwrap();
        let off = m.opt_tag(&selection(&[("ssl-weak", false)])).unwrap();
        assert_eq!(on, on_again);
        assert_ne!(on, off);
        assert_eq!(on.0.len(), 8);
        assert!(on.0.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
    }

    #[test]
    fn opt_tag_of_optionless_manifest_is_always_empty() {
        let toml = r#"
            name = "hello"
            version = "1.0.0"
            category = "app-misc"
        "#;
        let m = PackageManifest::from_toml_str(toml).unwrap();
        assert_eq!(m.opt_tag(&BTreeMap::new()).unwrap(), OptTag(String::new()));
    }

    #[test]
    fn options_export_env_covers_every_declared_option() {
        let m = PackageManifest::from_toml_str(OPTIONS_TOML).unwrap();
        // Empty selection → defaults; name uppercased, '-' → '_'.
        let env = m.options_export_env(&BTreeMap::new()).unwrap();
        assert_eq!(env, vec![
            ("PKG_OPT_DOCS".to_string(), "1".to_string()),
            ("PKG_OPT_SSL_WEAK".to_string(), "0".to_string()),
        ]);
        // Overrides apply.
        let env = m.options_export_env(&selection(&[("ssl-weak", true), ("docs", false)])).unwrap();
        assert_eq!(env, vec![
            ("PKG_OPT_DOCS".to_string(), "0".to_string()),
            ("PKG_OPT_SSL_WEAK".to_string(), "1".to_string()),
        ]);
    }
}
