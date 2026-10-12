//! ## v1 changes
//!
//! - Initial implementation (SPEC: pkg-scanner gate.rs; v12 §9
//!   reframing; design doc Issue 4.1.1 for the link-interface handoff).

//! Post-build verification for one freshly built package's store dir.
//!
//! # Framing (v12 §9): verification, not an install gate
//!
//! In the source-only world, **floor ≤ ceiling holds by construction**:
//! a binary is built against the system's own glibc, so the binary's
//! GLIBC floor can never exceed the host ceiling — unless the recipe
//! escaped its sandbox and linked against a *foreign* libc. That
//! escape, not version skew, is the failure this gate detects. Per
//! v12 §9 the pipeline stage "reports and records rather than blocking
//! installs" — but a detected sandbox escape is a hard
//! `PkgError::Gate`, because installing an artifact linked against a
//! libc that isn't the system's is never the right move.
//!
//! The real checks (per SPEC):
//!
//! 1. **Undeclared DT_NEEDED linkage.** Every soname a built artifact
//!    needs must be provided by the package itself or by a *declared*
//!    dependency. Anything else means the build linked against
//!    something the resolver didn't account for — the ground truth for
//!    recorded runtime dependency edges (pkg-review: "DT_NEEDED
//!    extraction becomes more important, not less").
//! 2. **Foreign-libc detection** (the residual of the old floor/ceiling
//!    gate): floor > host ceiling ⇒ hard failure.
//! 3. **Link-interface extraction recorded.** Every artifact's
//!    [`crate::elf::LinkInterface`] is extracted and handed back in the
//!    [`GateReport`] for the caller (pkg-build) to record into
//!    pkg-db's `link_interface` table — the hash that cascade pruning
//!    (Issue 4.1.1) compares. gate.rs itself does no DB I/O:
//!    pkg-scanner's dependency pins (pkg-core, goblin, sha2) exclude
//!    pkg-db.
//!
//! A `None` GLIBC floor on a binary that *does* link `libc.so.6` is
//! **unknown, not "pass"** (the unversioned-libc-symbol review
//! finding): such binaries are surfaced as warnings in the report, not
//! silently accepted and not hard failures.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use pkg_core::{PkgError, PkgResult, Version};

use crate::elf::{extract_link_interface, parse_elf, ElfInfo, LinkInterface};

/// One dependency the recipe declared (link or runtime kind), with the
/// store dir its artifacts were activated from, so the gate can collect
/// the sonames that dependency *provides*.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeclaredDep {
    /// Package name, used in diagnostics.
    pub name: String,
    /// The dependency's store dir (`/var/lib/pkg/store/<identity>`),
    /// scanned for provided sonames.
    pub store_dir: PathBuf,
}

/// The full outcome of [`verify_build`]: hard failures become
/// `PkgError::Gate`; everything informational lands here.
#[derive(Debug, Clone, Default)]
pub struct GateReport {
    /// How many ELF artifacts were examined.
    pub binaries_checked: usize,
    /// Artifacts that link `libc.so.6` but carry no versioned symbol
    /// references — their floor is *unknown* (review finding), reported
    /// rather than passed silently.
    pub unknown_floor: Vec<PathBuf>,
    /// Every extracted link interface, paired with the artifact it came
    /// from. The caller (pkg-build) records these into pkg-db's
    /// `link_interface` table; see module docs for why gate.rs doesn't
    /// do that itself.
    pub link_interfaces: Vec<(PathBuf, LinkInterface)>,
    /// Non-fatal observations worth surfacing in build logs.
    pub warnings: Vec<String>,
}

/// How the undeclared-DT_NEEDED check treats sonames provided by
/// neither the package itself nor its declared dependencies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkagePolicy {
    /// Source builds: undeclared linkage is a hard `PkgError::Gate`
    /// failure — the build reached outside its declared dependency
    /// contract (a sandbox escape or an undeclared system lib).
    Strict,
    /// Vendor binary blobs (manifest `[build] binary = true`):
    /// undeclared linkage is recorded as a warning, not a failure. A
    /// redistributed blob legitimately links system libraries the
    /// distribution ships separately from the blob's declared deps;
    /// demanding the full closure as packages would make the binary
    /// pattern unrepresentable. The foreign-glibc floor check stays a
    /// hard failure under either policy.
    BinaryRedistributable,
}

/// SPEC entry point: post-build verification of the artifacts in
/// `store_dir` against `host_glibc` and the declared dependency set.
///
/// Returns `Ok(())` when verification passes (warnings — unversioned
/// floors, unparseable non-binary files — do not fail the gate; they
/// are available via [`verify_build`] for callers that want them), and
/// `Err(PkgError::Gate)` on the two hard failures: foreign-libc
/// linkage and undeclared DT_NEEDED linkage.
pub fn gate_build(
    store_dir: &Path,
    host_glibc: &Version,
    declared_deps: &[DeclaredDep],
) -> PkgResult<()> {
    verify_build(store_dir, host_glibc, declared_deps).map(|_| ())
}

/// The report-returning variant of [`gate_build`], for callers
/// (pkg-build) that also record link interfaces and surface warnings.
pub fn verify_build(
    store_dir: &Path,
    host_glibc: &Version,
    declared_deps: &[DeclaredDep],
) -> PkgResult<GateReport> {
    verify_build_with_policy(store_dir, host_glibc, declared_deps, LinkagePolicy::Strict)
}

/// [`verify_build`] with an explicit [`LinkagePolicy`] for the
/// undeclared-soname check. See [`LinkagePolicy::BinaryRedistributable`]
/// for when the relaxed policy is legitimate.
pub fn verify_build_with_policy(
    store_dir: &Path,
    host_glibc: &Version,
    declared_deps: &[DeclaredDep],
    policy: LinkagePolicy,
) -> PkgResult<GateReport> {
    // Sonames provided by the declared dependency set. Scanning each
    // dep's store dir is the ground truth — the resolver's *claim* is
    // checked against what the deps actually contain.
    let mut dep_provided: BTreeSet<String> = BTreeSet::new();
    for dep in declared_deps {
        for artifact in walk_elf_files(&dep.store_dir)? {
            for li in extract_link_interface(&artifact)? {
                dep_provided.insert(li.soname.clone());
            }
        }
    }

    let mut report = GateReport::default();
    let mut self_provided: BTreeSet<String> = BTreeSet::new();
    let mut artifacts: Vec<(PathBuf, ElfInfo)> = Vec::new();

    // First pass: parse everything, collect self-provided sonames, run
    // the floor checks.
    for artifact in walk_elf_files(store_dir)? {
        let bytes = std::fs::read(&artifact).map_err(PkgError::Io)?;
        let info = match parse_elf(&bytes) {
            Ok(info) => info,
            Err(e) => {
                // A file with ELF magic that doesn't parse is a broken
                // artifact — worth a warning, not a gate failure (it may
                // be data the package ships, not something it runs).
                report.warnings.push(format!(
                    "{}: ELF magic but unparseable ({e}); skipped",
                    artifact.display()
                ));
                continue;
            }
        };
        report.binaries_checked += 1;

        for li in extract_link_interface(&artifact)? {
            self_provided.insert(li.soname.clone());
            report.link_interfaces.push((artifact.clone(), li));
        }

        match &info.glibc_floor {
            Some(floor) if floor > host_glibc => {
                return Err(PkgError::Gate(format!(
                    "{}: requires GLIBC_{floor} but host glibc is {host_glibc} — \
                     the recipe linked against a foreign libc (sandbox escape?)",
                    artifact.display()
                )));
            }
            None if info.needed.iter().any(|n| n == "libc.so.6") => {
                // Links libc but references no versioned symbols:
                // unknown, not "pass" (review finding).
                report.unknown_floor.push(artifact.clone());
                report.warnings.push(format!(
                    "{}: links libc.so.6 with no versioned symbols — \
                     glibc floor unknown",
                    artifact.display()
                ));
            }
            _ => {}
        }

        artifacts.push((artifact, info));
    }

    // Second pass: undeclared-linkage check, now that self-provided
    // sonames are complete (a package's own libs may need each other).
    for (artifact, info) in &artifacts {
        let missing = undeclared_sonames(&info.needed, &self_provided, &dep_provided);
        if missing.is_empty() {
            continue;
        }
        match policy {
            LinkagePolicy::Strict => {
                return Err(PkgError::Gate(format!(
                    "{}: links against undeclared dependencies: {} \
                     (not provided by this package or any declared dep)",
                    artifact.display(),
                    missing.join(", ")
                )));
            }
            LinkagePolicy::BinaryRedistributable => {
                report.warnings.push(format!(
                    "{}: vendor blob links undeclared sonames: {} \
                     (allowed by [build] binary; expected to come from \
                     the host distribution at runtime)",
                    artifact.display(),
                    missing.join(", ")
                ));
            }
        }
    }

    Ok(report)
}

/// Which of `needed` sonames are provided by neither the package itself
/// nor its declared dependencies. Pure — the gate's core set logic,
/// unit-tested without touching the filesystem.
pub fn undeclared_sonames(
    needed: &[String],
    self_provided: &BTreeSet<String>,
    dep_provided: &BTreeSet<String>,
) -> Vec<String> {
    needed
        .iter()
        .filter(|n| !self_provided.contains(*n) && !dep_provided.contains(*n))
        .cloned()
        .collect()
}

/// Recursively collects every regular file under `root` that starts
/// with the ELF magic (`\x7fELF`). Identification by content, not name:
/// binaries live in `bin/`, `lib/`, `libexec/`, `usr/bin` (merged-usr
/// layouts), and unversioned `.so` files, none of which share a
/// reliable naming convention.
pub fn walk_elf_files(root: &Path) -> PkgResult<Vec<PathBuf>> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            // A dep store dir that doesn't exist is a caller bug, but
            // for *sub*directories (dangling symlinks etc.) skipping is
            // the graceful move. Distinguish: the root must exist.
            if dir.as_path() == root {
                return Err(PkgError::NotFound(format!(
                    "store dir {} does not exist",
                    root.display()
                )));
            }
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            if meta.is_dir() {
                stack.push(path);
            } else if meta.is_file() && has_elf_magic(&path) {
                out.push(path);
            }
        }
    }
    out.sort();
    Ok(out)
}

/// Cheap content sniff: first four bytes are `\x7fELF`.
fn has_elf_magic(path: &Path) -> bool {
    use std::io::Read as _;
    let Ok(mut f) = std::fs::File::open(path) else {
        return false;
    };
    let mut magic = [0u8; 4];
    matches!(f.read_exact(&mut magic), Ok(())) && &magic == b"\x7fELF"
}

// ---------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn set(items: &[&str]) -> BTreeSet<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn undeclared_sonames_flags_only_unprovided_needs() {
        let needed: Vec<String> = ["libc.so.6", "libssl.so.3", "libmystery.so.1"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let missing = undeclared_sonames(
            &needed,
            &set(&["libssl.so.3"]),   // self-provided
            &set(&["libc.so.6"]),     // dep-provided
        );
        assert_eq!(missing, vec!["libmystery.so.1".to_string()]);
    }

    #[test]
    fn undeclared_sonames_empty_when_everything_is_provided() {
        let needed: Vec<String> = vec!["libc.so.6".to_string()];
        assert!(undeclared_sonames(&needed, &set(&[]), &set(&["libc.so.6"])).is_empty());
        // Self-linkage: a package's libs needing each other is fine.
        assert!(undeclared_sonames(&needed, &set(&["libc.so.6"]), &set(&[])).is_empty());
    }

    #[test]
    fn undeclared_sonames_treats_no_needs_as_clean() {
        // Static binaries / no DT_NEEDED at all: nothing to check.
        assert!(undeclared_sonames(&[], &set(&[]), &set(&[])).is_empty());
    }

    #[test]
    fn walk_elf_files_errors_on_missing_root() {
        let missing = Path::new("/nonexistent/pkg-gate-test-root");
        assert!(matches!(
            walk_elf_files(missing),
            Err(PkgError::NotFound(_))
        ));
    }

    #[test]
    fn walk_elf_files_finds_elf_by_content_not_name() {
        let dir = std::env::temp_dir().join(format!(
            "pkg-gate-test-{}-walk",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("bin")).unwrap();
        // An ELF-magic file with a non-obvious name, and a non-ELF file
        // with a library-ish name.
        std::fs::write(dir.join("bin/tool"), b"\x7fELFrest").unwrap();
        std::fs::write(dir.join("libfake.so.1"), b"not elf").unwrap();

        let found = walk_elf_files(&dir).unwrap();
        assert_eq!(found, vec![dir.join("bin/tool")]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
