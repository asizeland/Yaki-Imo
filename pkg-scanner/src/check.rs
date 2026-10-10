//! ## v1 changes
//!
//! - Initial implementation (SPEC: pkg-scanner check.rs; v12 §9; the
//!   unversioned-libc review finding).

//! System-wide scanner diagnostics — the engine behind `pkg check`.
//!
//! Scans every installed package's binaries for:
//!
//! - **Floor/ceiling violations** (`floor > host glibc`): a binary that
//!   cannot run on this system. In the source-only world this "can't
//!   happen" for anything built locally (v12 §9) — so when it *does*
//!   appear, it's a meaningful signal: a binary installed out-of-band,
//!   a `--force`-broken dependency edge (v11 §16.5), or a foreign-libc
//!   artifact that slipped past the build gate.
//! - **Unversioned-libc unknowns**: binaries that link `libc.so.6` but
//!   reference no versioned symbols. Per the review finding, a `None`
//!   floor from `elf.rs` means **unknown, not "pass"** — these are
//!   reported as their own finding kind so `pkg check` output can say
//!   "cannot determine" instead of implying a clean bill of health.
//!
//! # SPEC deviation (documented)
//!
//! SPEC's signature is `check_system(conn, store_roots) ->
//! Vec<CheckFinding>`, but pkg-scanner's dependency pins (pkg-core,
//! goblin, sha2) exclude pkg-db — this crate cannot name
//! `rusqlite::Connection`. The caller (pkg-cli) therefore runs pkg-db's
//! `list_installed_packages` itself and passes the result in as
//! [`InstalledPackage`]s; the store root is implicit in each package's
//! `store_dir`. This keeps the DB query and the scanning on opposite
//! sides of the crate boundary, and it makes the DB-unreachable
//! fallback (v11 §9.4/v12: `pkg check` degrades to filesystem scanning
//! under lock contention) trivial — the fallback just enumerates store
//! dirs instead of querying.

use std::fmt;
use std::path::PathBuf;

use pkg_core::{PkgResult, Version};

use crate::elf::ElfInfo;
use crate::gate::walk_elf_files;

/// One installed package, as much of its identity as the check needs.
/// Produced by the caller from pkg-db rows (or from store-dir names in
/// the DB-unreachable fallback).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstalledPackage {
    pub name: String,
    pub version: String,
    /// This package's store dir (e.g.
    /// `/var/lib/pkg/store/<name-version+abN>`).
    pub store_dir: PathBuf,
}

impl fmt::Display for InstalledPackage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}-{}", self.name, self.version)
    }
}

/// What `pkg check` found wrong (or undeterminable) about one binary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckFinding {
    /// The binary's GLIBC floor exceeds the host's glibc — it cannot
    /// run on this system.
    FloorAboveCeiling {
        package: String,
        path: PathBuf,
        floor: Version,
        ceiling: Version,
    },
    /// The binary links `libc.so.6` but references no versioned
    /// symbols, so its floor is *unknown* — not "pass" (review
    /// finding).
    UnknownGlibcFloor { package: String, path: PathBuf },
    /// A file with ELF magic that failed to parse — a corrupted
    /// artifact in the store.
    UnparseableElf {
        package: String,
        path: PathBuf,
        reason: String,
    },
    /// The package is recorded installed but its store dir is missing
    /// or unreadable — a DB/store inconsistency worth surfacing on its
    /// own, not lumped in with per-file problems.
    MissingStoreDir { package: String, path: PathBuf },
}

impl CheckFinding {
    /// The package this finding belongs to.
    pub fn package(&self) -> &str {
        match self {
            CheckFinding::FloorAboveCeiling { package, .. }
            | CheckFinding::UnknownGlibcFloor { package, .. }
            | CheckFinding::UnparseableElf { package, .. }
            | CheckFinding::MissingStoreDir { package, .. } => package,
        }
    }

    /// The binary this finding is about.
    pub fn path(&self) -> &PathBuf {
        match self {
            CheckFinding::FloorAboveCeiling { path, .. }
            | CheckFinding::UnknownGlibcFloor { path, .. }
            | CheckFinding::UnparseableElf { path, .. }
            | CheckFinding::MissingStoreDir { path, .. } => path,
        }
    }

    /// Hard violations are findings that mean "this binary cannot run
    /// here". Unknowns and unparseable files are diagnostics, not
    /// violations — `pkg check`'s exit code keys off this distinction.
    pub fn is_violation(&self) -> bool {
        matches!(self, CheckFinding::FloorAboveCeiling { .. })
    }
}

impl fmt::Display for CheckFinding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CheckFinding::FloorAboveCeiling {
                package,
                path,
                floor,
                ceiling,
            } => write!(
                f,
                "{package}: {}: requires GLIBC_{floor}, host glibc is {ceiling} — \
                 this binary cannot run on this system",
                path.display()
            ),
            CheckFinding::UnknownGlibcFloor { package, path } => write!(
                f,
                "{package}: {}: links libc.so.6 with no versioned symbols — \
                 glibc floor unknown (not verified)",
                path.display()
            ),
            CheckFinding::UnparseableElf {
                package,
                path,
                reason,
            } => write!(
                f,
                "{package}: {}: ELF magic but unparseable: {reason}",
                path.display()
            ),
            CheckFinding::MissingStoreDir { package, path } => write!(
                f,
                "{package}: store dir {} is missing or unreadable",
                path.display()
            ),
        }
    }
}

/// The classification of one parsed binary — the pure decision logic,
/// separated from I/O so it's unit-testable without fixtures.
///
/// `None` means "clean": floor ≤ ceiling, or no libc linkage at all
/// (static binaries have nothing to be incompatible *with*). A `None`
/// floor *with* libc linkage is `UnknownGlibcFloor`, never clean.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FindingKind {
    FloorAboveCeiling { floor: Version },
    UnknownGlibcFloor,
}

/// Classifies one binary's [`ElfInfo`] against the host glibc ceiling.
pub fn classify(info: &ElfInfo, host_glibc: &Version) -> Option<FindingKind> {
    match &info.glibc_floor {
        Some(floor) if floor > host_glibc => Some(FindingKind::FloorAboveCeiling {
            floor: floor.clone(),
        }),
        Some(_) => None,
        None => {
            if info.needed.iter().any(|n| n == "libc.so.6") {
                Some(FindingKind::UnknownGlibcFloor)
            } else {
                // No libc linkage (static binary, or non-glibc libc):
                // nothing to check it against.
                None
            }
        }
    }
}

/// Scans every installed package's binaries and returns all findings
/// (violations and diagnostics), grouped in package order.
///
/// Powers `pkg check` (SPEC: pkg-scanner). See the module docs for why
/// the installed set arrives as data rather than a database handle.
/// A package whose store dir is missing is *skipped with a finding*
/// (an absent store dir for an installed package is itself a system
/// inconsistency worth reporting), not an error that aborts the scan.
pub fn check_system(
    installed: &[InstalledPackage],
    host_glibc: &Version,
) -> PkgResult<Vec<CheckFinding>> {
    let mut findings = Vec::new();

    for pkg in installed {
        let artifacts = match walk_elf_files(&pkg.store_dir) {
            Ok(files) => files,
            Err(_) => {
                findings.push(CheckFinding::MissingStoreDir {
                    package: pkg.to_string(),
                    path: pkg.store_dir.clone(),
                });
                continue;
            }
        };

        for artifact in artifacts {
            let bytes = std::fs::read(&artifact).map_err(pkg_core::PkgError::Io)?;
            let info = match crate::elf::parse_elf(&bytes) {
                Ok(info) => info,
                Err(e) => {
                    findings.push(CheckFinding::UnparseableElf {
                        package: pkg.to_string(),
                        path: artifact.clone(),
                        reason: e.to_string(),
                    });
                    continue;
                }
            };
            match classify(&info, host_glibc) {
                Some(FindingKind::FloorAboveCeiling { floor }) => {
                    findings.push(CheckFinding::FloorAboveCeiling {
                        package: pkg.to_string(),
                        path: artifact,
                        floor,
                        ceiling: host_glibc.clone(),
                    });
                }
                Some(FindingKind::UnknownGlibcFloor) => {
                    findings.push(CheckFinding::UnknownGlibcFloor {
                        package: pkg.to_string(),
                        path: artifact,
                    });
                }
                None => {}
            }
        }
    }

    Ok(findings)
}

// ---------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn info(needed: &[&str], floor: Option<&str>) -> ElfInfo {
        ElfInfo {
            needed: needed.iter().map(|s| s.to_string()).collect(),
            glibc_floor: floor.map(|f| f.parse().unwrap()),
        }
    }

    #[test]
    fn floor_above_ceiling_is_a_violation() {
        let host: Version = "2.39".parse().unwrap();
        let kind = classify(&info(&["libc.so.6"], Some("2.40")), &host);
        assert_eq!(
            kind,
            Some(FindingKind::FloorAboveCeiling {
                floor: "2.40".parse().unwrap()
            })
        );
    }

    #[test]
    fn floor_at_or_below_ceiling_is_clean() {
        let host: Version = "2.39".parse().unwrap();
        assert_eq!(classify(&info(&["libc.so.6"], Some("2.39")), &host), None);
        assert_eq!(classify(&info(&["libc.so.6"], Some("2.17")), &host), None);
    }

    #[test]
    fn none_floor_with_libc_linkage_is_unknown_not_pass() {
        // The review finding: elf.rs's None floor must not read as
        // "pass" when the binary does link libc.
        let host: Version = "2.39".parse().unwrap();
        assert_eq!(
            classify(&info(&["libc.so.6"], None), &host),
            Some(FindingKind::UnknownGlibcFloor)
        );
    }

    #[test]
    fn none_floor_without_libc_linkage_is_clean() {
        // Static binaries: no libc.so.6 need, no floor — nothing to
        // check against, genuinely clean.
        let host: Version = "2.39".parse().unwrap();
        assert_eq!(classify(&info(&[], None), &host), None);
        // A binary linked against a non-glibc libc is likewise outside
        // this check's scope (NSS-style non-coverage, v11 §8).
        assert_eq!(classify(&info(&["libc.musl-x86_64.so.1"], None), &host), None);
    }

    #[test]
    fn only_floor_violations_are_violations() {
        let host: Version = "2.39".parse().unwrap();
        let floor: Version = "2.40".parse().unwrap();
        let violation = CheckFinding::FloorAboveCeiling {
            package: "foo-1.0".into(),
            path: PathBuf::from("/x"),
            floor,
            ceiling: host,
        };
        let unknown = CheckFinding::UnknownGlibcFloor {
            package: "foo-1.0".into(),
            path: PathBuf::from("/x"),
        };
        assert!(violation.is_violation());
        assert!(!unknown.is_violation());
    }

    #[test]
    fn missing_store_dir_becomes_a_finding_not_an_error() {
        let pkgs = vec![InstalledPackage {
            name: "ghost".into(),
            version: "1.0".into(),
            store_dir: PathBuf::from("/nonexistent/pkg-check-test-store"),
        }];
        let host: Version = "2.39".parse().unwrap();
        let findings = check_system(&pkgs, &host).unwrap();
        assert_eq!(findings.len(), 1);
        assert!(matches!(
            findings[0],
            CheckFinding::MissingStoreDir { .. }
        ));
        assert_eq!(findings[0].package(), "ghost-1.0");
    }
}
