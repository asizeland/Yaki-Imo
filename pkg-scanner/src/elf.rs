//! DT_NEEDED/soname extraction and GLIBC floor/ceiling checking (§8.1–§8.4).
//!
//! A dynamically-linked ELF binary references specific *versions* of the
//! symbols it needs from libc — `memcpy@GLIBC_2.14`, say, if it needs
//! behavior only guaranteed from that version onward. These references
//! live in the `.gnu.version_r` ("version needs") section, one entry per
//! needed library, each carrying the set of version strings that library
//! must satisfy. glibc's own versioning is cumulative — anything running
//! GLIBC_2.34 also satisfies GLIBC_2.14 — so the *highest* GLIBC_X.Y
//! string referenced against `libc.so.6` specifically is the actual
//! minimum glibc version the binary needs to run at all. This file calls
//! that the binary's **floor**; the host's installed glibc version is
//! its **ceiling**; a binary runs only if floor ≤ ceiling.
//!
//! "ISA notes" — the other item §15's pipeline lists alongside this
//! (`glibc-hwcaps` / x86 microarchitecture-level variants, §6.1) — isn't
//! covered in this pass. That's a different ELF mechanism entirely
//! (`.note.gnu.property`, not symbol versioning) and arguably belongs to
//! how `ld.so` selects among already-installed ISA variants rather than
//! something this scanner needs to read out of binary content itself.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use goblin::elf::Elf;
use pkg_core::{PkgError, PkgResult, Version};
use sha2::Digest as _;

/// What this file extracts from one ELF binary's static structure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ElfInfo {
    /// DT_NEEDED sonames, e.g. `["libc.so.6", "libssl.so.3"]`.
    pub needed: Vec<String>,
    /// The highest `GLIBC_X.Y[.Z]` symbol version this binary references
    /// against `libc.so.6` specifically. `None` if the binary has no
    /// `libc.so.6` version-need entry at all — statically linked, or
    /// dynamically linked but happening to reference only unversioned
    /// symbols (rare in practice, but not impossible).
    pub glibc_floor: Option<Version>,
}

/// Parses raw ELF bytes into an `ElfInfo`.
pub fn parse_elf(bytes: &[u8]) -> PkgResult<ElfInfo> {
    let elf = Elf::parse(bytes).map_err(|e| PkgError::Parse(e.to_string()))?;

    let needed = elf.libraries.iter().map(|s| s.to_string()).collect();
    let glibc_floor = extract_glibc_floor(&elf);

    Ok(ElfInfo { needed, glibc_floor })
}

fn extract_glibc_floor(elf: &Elf) -> Option<Version> {
    let verneed = elf.verneed.as_ref()?;
    let mut floor: Option<Version> = None;

    for need in verneed.iter() {
        if elf.dynstrtab.get_at(need.vn_file) != Some("libc.so.6") {
            continue;
        }
        // A plain loop rather than an iterator chain here deliberately:
        // `need.iter()` borrows from `need` itself (not from the ELF
        // bytes), and `need` is a value local to this loop body — a
        // `flat_map`/`filter_map` chain trying to return that iterator
        // from a closure doesn't live long enough to compile. Collecting
        // into a running `floor` as we go sidesteps the borrow issue
        // entirely rather than fighting it.
        for aux in need.iter() {
            let Some(name) = elf.dynstrtab.get_at(aux.vna_name) else {
                continue;
            };
            let Some(version_str) = name.strip_prefix("GLIBC_") else {
                continue;
            };
            let Ok(version) = version_str.parse::<Version>() else {
                continue;
            };
            floor = Some(match floor {
                Some(current) if current >= version => current,
                _ => version,
            });
        }
    }

    floor
}

/// Whether a binary whose GLIBC floor is `floor` can run on a host whose
/// installed glibc version is `host_ceiling`. A `None` floor always
/// passes — there's nothing to check it against.
pub fn glibc_compatible(floor: Option<&Version>, host_ceiling: &Version) -> bool {
    match floor {
        None => true,
        Some(floor) => floor <= host_ceiling,
    }
}

// ---------------------------------------------------------------------
// v2: link interfaces
// ---------------------------------------------------------------------

/// `VER_FLG_BASE` — the flag marking a `.gnu.version_d` entry as the
/// *base* version definition (whose aux name is the library's own
/// soname, not a version the library exports). goblin 0.8 doesn't name
/// this constant, so it's spelled out here; the value is fixed by the
/// ELF spec, not by goblin.
const VER_FLG_BASE: u16 = 0x1;

/// The linkable interface one shared library presents to its dependents
/// (v2): its soname plus the set of symbol-version names it *defines*
/// (`GLIBC_2.2.5`, `OPENSSL_3.0.0`, …). Two builds of the same library
/// version can differ here — a dropped symbol version is an ABI break —
/// which is exactly what pkg-build's cascade logic hashes to decide
/// whether dependents need rebuilding (Issue 4.1.1).
///
/// `symbol_versions` is a `BTreeSet` so canonicalization for hashing is
/// free: iteration order is already sorted, and duplicates can't exist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkInterface {
    pub soname: String,
    pub symbol_versions: BTreeSet<String>,
}

/// Extracts the link interface(s) a built artifact provides (v2).
///
/// An artifact only *has* a link interface if it declares a soname
/// (DT_SONAME) — an executable, or a shared object nobody is meant to
/// link against, yields an empty `Vec` rather than an error. In
/// practice the result is at most one entry per ELF (an ELF carries one
/// soname); the `Vec` keeps the signature aligned with how pkg-db's
/// `link_interface` table stores rows (one per soname, § pkg-db v4) and
/// leaves room for multi-artifact aggregation at the call site.
///
/// Symbol versions come from `.gnu.version_d` (goblin's `verdef`),
/// excluding the base version definition (`VER_FLG_BASE`), whose aux
/// name is the soname itself rather than an exported version.
pub fn extract_link_interface(path: &Path) -> PkgResult<Vec<LinkInterface>> {
    let bytes = std::fs::read(path).map_err(PkgError::Io)?;
    let elf = Elf::parse(&bytes).map_err(|e| PkgError::Parse(e.to_string()))?;

    let Some(soname) = elf.soname else {
        return Ok(Vec::new());
    };

    let mut symbol_versions = BTreeSet::new();
    if let Some(verdef) = &elf.verdef {
        for def in verdef.iter() {
            if def.vd_flags & VER_FLG_BASE != 0 {
                continue;
            }
            // Plain loops rather than iterator chains, same reason as
            // `extract_glibc_floor`: `def.iter()` borrows the loop-local
            // `def`, so a chain returning that iterator can't compile.
            for aux in def.iter() {
                if let Some(name) = elf.dynstrtab.get_at(aux.vda_name) {
                    symbol_versions.insert(name.to_string());
                }
            }
        }
    }

    Ok(vec![LinkInterface {
        soname: soname.to_string(),
        symbol_versions,
    }])
}

/// A stable fingerprint for a set of link interfaces plus the header
/// fingerprint of the build that produced them (v2): SHA-256 over the
/// canonical serialization, truncated to eight lowercase hex chars —
/// the same truncation convention pkg-core's `AbiTag`/`OptTag`
/// vocabulary uses, so a hash from either family is the same shape in
/// the database and in logs.
///
/// Canonical form: interfaces sorted by soname; each rendered
/// `soname:VER1,VER2;` (versions already sorted by the `BTreeSet`);
/// the header fingerprint appended as `header:<fp>`. Everything the
/// hash covers is sorted or caller-fixed, so the same logical interface
/// always hashes identically regardless of the order `interfaces` was
/// gathered in — the determinism cascade.rs relies on when comparing a
/// stored hash against a fresh build's.
pub fn link_interface_hash(interfaces: &[LinkInterface], header_fp: &str) -> String {
    let mut sorted: Vec<&LinkInterface> = interfaces.iter().collect();
    sorted.sort_by(|a, b| a.soname.cmp(&b.soname));

    let mut canonical = String::new();
    for li in sorted {
        canonical.push_str(&li.soname);
        canonical.push(':');
        for (i, v) in li.symbol_versions.iter().enumerate() {
            if i > 0 {
                canonical.push(',');
            }
            canonical.push_str(v);
        }
        canonical.push(';');
    }
    canonical.push_str("header:");
    canonical.push_str(header_fp);

    let digest = sha2::Sha256::digest(canonical.as_bytes());
    digest[..4].iter().map(|b| format!("{b:02x}")).collect()
}

/// Which of `archive_paths` the artifact at `artifact` statically
/// consumed, detected by symbol-table comparison (v2).
///
/// Static linking copies the archive member's object code into the
/// artifact, so — as long as the artifact wasn't subsequently stripped
/// — the member's defined symbols show up in the artifact's own
/// `.symtab`. An archive counts as consumed when at least one symbol it
/// defines appears among the artifact's defined symbols. Returns the
/// paths as given, in input order.
///
/// Deliberately best-effort, not a `PkgResult`: a stripped artifact
/// yields an empty list (there is no `.symtab` to compare — the absence
/// of evidence is not evidence of absence, documented rather than
/// hidden), and unreadable/unparseable candidates are skipped. This is
/// a heuristic scan in the same family as `heuristic.rs`'s dlopen
/// candidates; the caller (audit/cascade tooling) treats a positive as
/// a hint, never as a gate.
pub fn detect_static_consumption(artifact: &Path, archive_paths: &[PathBuf]) -> Vec<String> {
    let Some(artifact_syms) = elf_defined_symbols(artifact) else {
        return Vec::new();
    };

    let mut consumed = Vec::new();
    for path in archive_paths {
        let Some(archive_syms) = archive_defined_symbols(path) else {
            continue;
        };
        if archive_syms.iter().any(|s| artifact_syms.contains(s)) {
            consumed.push(path.to_string_lossy().into_owned());
        }
    }
    consumed
}

/// The set of *defined* symbol names in one ELF file's `.symtab` —
/// `None` when the file can't be read or isn't ELF. Undefined symbols
/// (imports) are excluded: for `detect_static_consumption` the question
/// is "did this code get copied in", and only defined symbols answer it.
fn elf_defined_symbols(path: &Path) -> Option<BTreeSet<String>> {
    let bytes = std::fs::read(path).ok()?;
    let elf = Elf::parse(&bytes).ok()?;
    Some(defined_symbols_of(&elf))
}

fn defined_symbols_of(elf: &Elf) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for sym in elf.syms.iter() {
        if sym.st_shndx == goblin::elf::section_header::SHN_UNDEF as usize || sym.st_name == 0 {
            continue;
        }
        if let Some(name) = elf.strtab.get_at(sym.st_name) {
            if !name.is_empty() {
                out.insert(name.to_string());
            }
        }
    }
    out
}

/// The union of defined symbol names across all ELF members of a static
/// archive. Non-ELF members (the archive's own index, thin-archive
/// placeholders) are skipped by the `Elf::parse` failure arm.
fn archive_defined_symbols(path: &Path) -> Option<BTreeSet<String>> {
    let bytes = std::fs::read(path).ok()?;
    let archive = goblin::archive::Archive::parse(&bytes).ok()?;
    let mut out = BTreeSet::new();
    for member in archive.members() {
        let Ok(member_bytes) = archive.extract(member, &bytes) else {
            continue;
        };
        let Ok(elf) = Elf::parse(member_bytes) else {
            continue;
        };
        out.extend(defined_symbols_of(&elf));
    }
    Some(out)
}

// ---------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use tempfile::tempdir;

    /// Compiles `source` with the system `cc` into `output_path`.
    /// Panics (rather than silently skipping) if that fails — this
    /// project already needs a working C toolchain for its own
    /// bootstrap, so an environment without one is a genuinely broken
    /// state for it to run tests in, not a case worth quietly working
    /// around.
    fn compile_c(source: &str, output_path: &Path) {
        let src_path = output_path.with_extension("c");
        std::fs::write(&src_path, source).unwrap();
        let status = Command::new("cc")
            .arg(&src_path)
            .arg("-o")
            .arg(output_path)
            .status()
            .expect("failed to invoke cc — is a C compiler installed?");
        assert!(status.success(), "cc failed to compile the test fixture");
    }

    #[test]
    fn extracts_dt_needed_from_a_real_binary() {
        let dir = tempdir().unwrap();
        let binary_path = dir.path().join("hello");
        compile_c(
            "#include <stdio.h>\nint main(){printf(\"hi\\n\");return 0;}",
            &binary_path,
        );

        let bytes = std::fs::read(&binary_path).unwrap();
        let info = parse_elf(&bytes).unwrap();

        assert!(
            info.needed.contains(&"libc.so.6".to_string()),
            "expected libc.so.6 in DT_NEEDED, got {:?}",
            info.needed
        );
    }

    #[test]
    fn glibc_floor_is_present_and_at_least_the_oldest_baseline_version() {
        // GLIBC_2.2.5 is the oldest versioned symbol baseline that
        // exists at all — true on every glibc release, not something
        // tied to this sandbox's specific glibc version. Asserting
        // against that baseline, rather than a specific pinned version
        // number, keeps this test correct in any environment with a
        // reasonably current glibc rather than only this one.
        let dir = tempdir().unwrap();
        let binary_path = dir.path().join("hello");
        compile_c(
            "#include <stdio.h>\nint main(){printf(\"hi\\n\");return 0;}",
            &binary_path,
        );

        let bytes = std::fs::read(&binary_path).unwrap();
        let info = parse_elf(&bytes).unwrap();

        let floor = info.glibc_floor.expect("a dynamically-linked glibc binary should have a floor");
        let baseline: Version = "2.2.5".parse().unwrap();
        assert!(floor >= baseline, "floor {floor} should be at least {baseline}");
    }

    #[test]
    fn statically_linked_binary_has_no_glibc_floor() {
        let dir = tempdir().unwrap();
        let binary_path = dir.path().join("static_hello");
        let src_path = binary_path.with_extension("c");
        std::fs::write(&src_path, "int main(){return 0;}").unwrap();
        let status = Command::new("cc")
            .arg(&src_path)
            .arg("-static")
            .arg("-o")
            .arg(&binary_path)
            .status();

        // Not every environment ships static libc (Ubuntu doesn't by
        // default) — treat a failed static build as "can't test this
        // case here" rather than a real failure, since it says nothing
        // about parse_elf's correctness.
        let Ok(status) = status else { return };
        if !status.success() {
            return;
        }

        let bytes = std::fs::read(&binary_path).unwrap();
        let info = parse_elf(&bytes).unwrap();
        assert_eq!(info.glibc_floor, None);
        assert!(info.needed.is_empty(), "a static binary has no DT_NEEDED entries");
    }

    #[test]
    fn rejects_non_elf_input() {
        let garbage = b"this is not an ELF file at all, just text";
        assert!(parse_elf(garbage).is_err());
    }

    #[test]
    fn glibc_compatible_passes_when_floor_at_or_below_ceiling() {
        let floor: Version = "2.34".parse().unwrap();
        let ceiling: Version = "2.39".parse().unwrap();
        assert!(glibc_compatible(Some(&floor), &ceiling));

        let exact: Version = "2.39".parse().unwrap();
        assert!(glibc_compatible(Some(&exact), &ceiling));
    }

    #[test]
    fn glibc_compatible_fails_when_floor_exceeds_ceiling() {
        let floor: Version = "2.40".parse().unwrap();
        let ceiling: Version = "2.39".parse().unwrap();
        assert!(!glibc_compatible(Some(&floor), &ceiling));
    }

    #[test]
    fn glibc_compatible_passes_when_there_is_no_floor() {
        let ceiling: Version = "2.39".parse().unwrap();
        assert!(glibc_compatible(None, &ceiling));
    }

    // --- v2: link interfaces ---

    fn li(soname: &str, versions: &[&str]) -> LinkInterface {
        LinkInterface {
            soname: soname.to_string(),
            symbol_versions: versions.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn link_interface_hash_is_order_independent_and_deterministic() {
        let a = li("liba.so.1", &["A_1", "A_2"]);
        let b = li("libb.so.0", &[]);
        let h1 = link_interface_hash(&[a.clone(), b.clone()], "deadbeef");
        let h2 = link_interface_hash(&[b, a], "deadbeef");
        assert_eq!(h1, h2, "input order must not affect the hash");
        // Same truncation convention as AbiTag/OptTag: 8 lowercase hex.
        assert_eq!(h1.len(), 8);
        assert!(h1.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
    }

    #[test]
    fn link_interface_hash_changes_with_header_fp_or_version_set() {
        let a = li("liba.so.1", &["A_1"]);
        assert_ne!(
            link_interface_hash(&[a.clone()], "fp1"),
            link_interface_hash(&[a.clone()], "fp2"),
            "the header fingerprint is part of the hashed content"
        );
        let a_extended = li("liba.so.1", &["A_1", "A_2"]);
        assert_ne!(
            link_interface_hash(&[a], "fp1"),
            link_interface_hash(&[a_extended], "fp1"),
            "an added symbol version is an interface change and must change the hash"
        );
    }

    #[test]
    fn extract_link_interface_reads_soname_and_defined_symbol_versions() {
        let dir = tempdir().unwrap();
        let lib_path = dir.path().join("libpkgtest.so.1");
        let src_path = dir.path().join("pkgtest.c");
        let map_path = dir.path().join("pkgtest.map");
        std::fs::write(&src_path, "int pkgtest_symbol(void){return 42;}\n").unwrap();
        std::fs::write(&map_path, "PKGTEST_1.0 { global: pkgtest_symbol; };\n").unwrap();
        let status = Command::new("cc")
            .arg("-shared")
            .arg("-fPIC")
            .arg("-Wl,-soname,libpkgtest.so.1")
            .arg(format!("-Wl,--version-script={}", map_path.display()))
            .arg(&src_path)
            .arg("-o")
            .arg(&lib_path)
            .status()
            .expect("failed to invoke cc — is a C compiler installed?");
        assert!(status.success(), "cc failed to compile the test fixture");

        let interfaces = extract_link_interface(&lib_path).unwrap();
        assert_eq!(interfaces.len(), 1);
        let interface = &interfaces[0];
        assert_eq!(interface.soname, "libpkgtest.so.1");
        assert!(
            interface.symbol_versions.contains("PKGTEST_1.0"),
            "expected PKGTEST_1.0 in defined symbol versions, got {:?}",
            interface.symbol_versions
        );
        // The base version definition (whose name is the soname itself)
        // must be filtered out — it isn't an exported version.
        assert!(!interface.symbol_versions.contains("libpkgtest.so.1"));
    }

    #[test]
    fn executable_without_soname_has_no_link_interface() {
        let dir = tempdir().unwrap();
        let binary_path = dir.path().join("hello");
        compile_c("int main(){return 0;}", &binary_path);
        let interfaces = extract_link_interface(&binary_path).unwrap();
        assert!(interfaces.is_empty(), "an executable provides no linkable interface");
    }

    // --- v2: static consumption ---

    /// Compiles and archives the test fixtures; returns `None` when the
    /// environment can't produce them (no cc/ar — the same
    /// "can't test this case here" precedent the static-link test above
    /// established), `Some((binary_path, archive_path, other_archive))`
    /// otherwise. `dir` must outlive the returned paths.
    fn build_static_consumption_fixture(dir: &Path) -> Option<(PathBuf, PathBuf, PathBuf)> {
        let run = |cmd: &mut Command| -> bool {
            cmd.status().map(|s| s.success()).unwrap_or(false)
        };

        let lib_src = dir.join("pkglib.c");
        std::fs::write(&lib_src, "int pkgtest_archive_only_symbol(void){return 42;}\n").unwrap();
        let lib_obj = dir.join("pkglib.o");
        if !run(Command::new("cc").arg("-c").arg(&lib_src).arg("-o").arg(&lib_obj)) {
            return None;
        }
        let archive = dir.join("libpkgconsumed.a");
        if !run(Command::new("ar").arg("rcs").arg(&archive).arg(&lib_obj)) {
            return None;
        }

        // An unrelated archive the binary never links against.
        let other_src = dir.join("other.c");
        std::fs::write(&other_src, "int pkgtest_other_symbol(void){return 7;}\n").unwrap();
        let other_obj = dir.join("other.o");
        if !run(Command::new("cc").arg("-c").arg(&other_src).arg("-o").arg(&other_obj)) {
            return None;
        }
        let other_archive = dir.join("libpkgother.a");
        if !run(Command::new("ar").arg("rcs").arg(&other_archive).arg(&other_obj)) {
            return None;
        }

        let main_src = dir.join("main.c");
        std::fs::write(
            &main_src,
            "extern int pkgtest_archive_only_symbol(void);\n\
             int main(){return pkgtest_archive_only_symbol();}\n",
        )
        .unwrap();
        let binary = dir.join("main");
        // Default cc output is unstripped, so the copied-in symbols land
        // in .symtab where the comparison can see them.
        if !run(Command::new("cc").arg(&main_src).arg(&archive).arg("-o").arg(&binary)) {
            return None;
        }

        Some((binary, archive, other_archive))
    }

    #[test]
    fn detect_static_consumption_flags_the_linked_archive_only() {
        let dir = tempdir().unwrap();
        let Some((binary, archive, other_archive)) = build_static_consumption_fixture(dir.path())
        else {
            return;
        };

        let consumed = detect_static_consumption(&binary, &[archive.clone(), other_archive.clone()]);
        assert!(
            consumed.contains(&archive.to_string_lossy().into_owned()),
            "expected the linked archive to be flagged, got {consumed:?}"
        );
        assert!(
            !consumed.contains(&other_archive.to_string_lossy().into_owned()),
            "an archive the binary never linked must not be flagged: {consumed:?}"
        );
    }

    #[test]
    fn detect_static_consumption_tolerates_missing_and_garbage_inputs() {
        let dir = tempdir().unwrap();
        let missing_artifact = dir.path().join("does-not-exist");
        let garbage = dir.path().join("garbage.a");
        std::fs::write(&garbage, b"not an archive at all").unwrap();

        // A missing artifact yields nothing rather than erroring.
        assert!(detect_static_consumption(&missing_artifact, &[garbage.clone()]).is_empty());

        let binary_path = dir.path().join("hello");
        compile_c("int main(){return 0;}", &binary_path);
        // A garbage archive candidate is skipped, not an error.
        assert!(detect_static_consumption(&binary_path, &[garbage]).is_empty());
    }
}
