//! pkg-build/src/repro.rs — critical-set local double-build (§10.5.6).
//!
//! ## v1 changes
//!
//! - Initial implementation: packages whose recipe manifest carries
//!   `[build] critical = true` are built **twice** by default — a second
//!   sequential sandboxed build under the same pinned
//!   `SOURCE_DATE_EPOCH` (recipe.rs pins a fixed constant, which is what
//!   makes the comparison meaningful) — followed by a deterministic
//!   tree-hash comparison of the two `$PKGDIR` output trees.
//! - Match: the second tree is discarded and the orchestrator records
//!   `provenance.reproducibility = 'self-match'`. Mismatch: the install
//!   aborts, both trees are moved under
//!   `<build_root>/quarantine/<name>-<version>-<unix-ts>/{first,second}/`
//!   and a `diff-report.txt` listing the differing paths is written
//!   alongside them.
//!
//! ## Why a mismatch is not a provenance row
//!
//! §10.5.6 says a mismatch "records `reproducibility='mismatch'`" — but
//! `provenance.package_id` references `packages(id)`, and a mismatching
//! package is never installed, so there is no row to reference. The
//! durable mismatch record is the quarantine tree itself:
//! `pkg audit --supply-chain` treats a non-empty quarantine dir as a
//! hard finding (`quarantine-present`) independently of the provenance
//! table, so the signal survives exactly where the auditor looks.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use pkg_core::{PkgError, PkgResult};
use sha2::{Digest, Sha256};

use crate::cache::MANIFEST_NAME;
use crate::recipe::{run_recipe_phases, BuildContext, Recipe};

/// Directory name under the build root holding quarantined mismatch
/// trees (§10.5.6: `/var/lib/pkg/build/quarantine/`).
pub const QUARANTINE_DIR_NAME: &str = "quarantine";

/// The diff report written next to the two quarantined trees.
pub const DIFF_REPORT_NAME: &str = "diff-report.txt";

// ---------------------------------------------------------------------
// Deterministic tree hashing
// ---------------------------------------------------------------------

/// Every entry under `root` as `relative path -> identity`, where the
/// identity is `sha256:<hex>` for regular files and `symlink:<target>`
/// for symlinks. Same discipline as the store manifest
/// (`cache::write_manifest`): sorted relative paths, per-file SHA-256 —
/// extended to symlinks, whose targets *are* content for a
/// reproducibility comparison (the store manifest can ignore them
/// because tar preserves them; a hash comparison cannot). The root-level
/// store-manifest file is bookkeeping, not content, and is skipped.
///
/// A `BTreeMap` iterates in sorted-key order, so downstream hashing and
/// diffing are deterministic for free.
fn tree_entries(root: &Path) -> PkgResult<BTreeMap<String, String>> {
    let mut entries = BTreeMap::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir).map_err(PkgError::Io)? {
            let entry = entry.map_err(PkgError::Io)?;
            let path = entry.path();
            let rel = path.strip_prefix(root).map_err(|_| {
                PkgError::Build(format!(
                    "tree hash: {} escapes {}",
                    path.display(),
                    root.display()
                ))
            })?;
            if rel.as_os_str() == MANIFEST_NAME {
                continue;
            }
            let rel = rel.display().to_string();
            let ftype = entry.file_type().map_err(PkgError::Io)?;
            if ftype.is_dir() {
                stack.push(path);
            } else if ftype.is_symlink() {
                let target = fs::read_link(&path).map_err(PkgError::Io)?;
                entries.insert(rel, format!("symlink:{}", target.display()));
            } else {
                entries.insert(rel, format!("sha256:{}", sha256_file(&path)?));
            }
        }
    }
    Ok(entries)
}

/// The deterministic hash of an output tree: SHA-256 over the sorted
/// `path NUL identity NL` stream of [`tree_entries`]. Same inputs always
/// hash identically regardless of the order the filesystem handed
/// directory entries back in.
pub fn tree_hash(root: &Path) -> PkgResult<String> {
    let entries = tree_entries(root)?;
    let mut hasher = Sha256::new();
    for (rel, identity) in &entries {
        hasher.update(rel.as_bytes());
        hasher.update(b"\0");
        hasher.update(identity.as_bytes());
        hasher.update(b"\n");
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

/// The differing paths between two entry maps, one line per difference.
/// Human-facing (it lands in the quarantine diff report), so the lines
/// say *how* each path differs.
pub fn diff_trees(
    first: &BTreeMap<String, String>,
    second: &BTreeMap<String, String>,
) -> Vec<String> {
    let mut diff = Vec::new();
    for (path, a) in first {
        match second.get(path) {
            Some(b) if a == b => {}
            Some(b) => diff.push(format!("content differs: {path} ({a} vs {b})")),
            None => diff.push(format!("only in first build: {path}")),
        }
    }
    for path in second.keys() {
        if !first.contains_key(path) {
            diff.push(format!("only in second build: {path}"));
        }
    }
    diff
}

fn sha256_file(path: &Path) -> PkgResult<String> {
    let bytes = fs::read(path).map_err(PkgError::Io)?;
    Ok(Sha256::digest(&bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

// ---------------------------------------------------------------------
// Quarantine
// ---------------------------------------------------------------------

/// Moves both mismatching output trees under
/// `<quarantine_root>/<name>-<version>-<unix-ts>/{first,second}/` and
/// writes the diff report next to them. Returns the quarantine dir.
///
/// Layout (§10.5.6):
///
/// ```text
/// <quarantine_root>/<name>-<version>-<unix-ts>/
/// ├── first/            the build that would have been installed
/// ├── second/           the reproducibility re-build
/// └── diff-report.txt   one line per differing path
/// ```
pub fn quarantine_mismatch(
    quarantine_root: &Path,
    name: &str,
    version: &str,
    first_tree: &Path,
    second_tree: &Path,
    diff: &[String],
) -> PkgResult<PathBuf> {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let dir = quarantine_root.join(format!("{name}-{version}-{ts}"));
    if dir.exists() {
        // The same package re-mismatching within the same second: the
        // newer evidence supersedes the older.
        fs::remove_dir_all(&dir).map_err(PkgError::Io)?;
    }
    fs::create_dir_all(&dir).map_err(PkgError::Io)?;
    move_tree(first_tree, &dir.join("first"))?;
    move_tree(second_tree, &dir.join("second"))?;
    let report = format!(
        "reproducibility mismatch: {name} {version}\n\
         recorded: {ts} (unix)\n\
         {} differing path(s):\n\
         {}\n",
        diff.len(),
        diff.join("\n")
    );
    fs::write(dir.join(DIFF_REPORT_NAME), report).map_err(PkgError::Io)?;
    Ok(dir)
}

/// Same-filesystem rename when possible; copy+remove as the cross-fs
/// fallback (mirrors `staging::stage_into_store`'s discipline for test
/// environments with a tmpfs build root).
fn move_tree(src: &Path, dst: &Path) -> PkgResult<()> {
    if fs::rename(src, dst).is_ok() {
        return Ok(());
    }
    copy_dir_recursive(src, dst)?;
    fs::remove_dir_all(src).map_err(PkgError::Io)
}

fn copy_dir_recursive(src: &Path, dst: &Path) -> PkgResult<()> {
    fs::create_dir_all(dst).map_err(PkgError::Io)?;
    for entry in fs::read_dir(src).map_err(PkgError::Io)? {
        let entry = entry.map_err(PkgError::Io)?;
        let path = entry.path();
        let target = dst.join(entry.file_name());
        let ftype = entry.file_type().map_err(PkgError::Io)?;
        if ftype.is_dir() {
            copy_dir_recursive(&path, &target)?;
        } else if ftype.is_symlink() {
            let link = fs::read_link(&path).map_err(PkgError::Io)?;
            let _ = fs::remove_file(&target);
            std::os::unix::fs::symlink(link, &target).map_err(PkgError::Io)?;
        } else {
            fs::copy(&path, &target).map_err(PkgError::Io)?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------
// The double build
// ---------------------------------------------------------------------

/// Runs the §10.5.6 reproducibility re-build: a second full sandboxed
/// recipe run into fresh sibling scratch/`$PKGDIR` dirs, then a
/// tree-hash comparison against the first build's `$PKGDIR`.
///
/// - Match: the second tree is removed and `Ok(())` returned; the caller
///   records `reproducibility='self-match'` at activation.
/// - Mismatch: both trees are quarantined (see [`quarantine_mismatch`]
///   for the layout) and an `Err` aborting the install is returned.
/// - A second build that *fails* (rather than differing) is a build
///   failure, not a mismatch: the first tree is left in place for the
///   orchestrator's normal failure path and no quarantine happens.
///
/// Sequential and sandboxed per the spec: `run_recipe_phases` re-enters
/// the `Build` profile, and `SOURCE_DATE_EPOCH` is the same pinned
/// constant both runs see.
pub fn double_build(
    recipe: &Recipe,
    first_ctx: &BuildContext,
    quarantine_root: &Path,
) -> PkgResult<()> {
    let name = &recipe.manifest.name;
    let version = recipe.manifest.version.clone();
    let second_ctx = BuildContext {
        scratch_dir: sibling(&first_ctx.scratch_dir, "repro2"),
        pkgdir: sibling(&first_ctx.pkgdir, "repro2"),
        ..first_ctx.clone()
    };
    let outcome = run_recipe_phases(recipe, &second_ctx);
    outcome.result.map_err(|e| {
        let _ = fs::remove_dir_all(&second_ctx.scratch_dir);
        let _ = fs::remove_dir_all(&second_ctx.pkgdir);
        PkgError::Build(format!("{name} {version}: reproducibility re-build failed: {e}"))
    })?;

    let first_entries = tree_entries(&first_ctx.pkgdir)?;
    let second_entries = tree_entries(&second_ctx.pkgdir)?;
    if first_entries == second_entries {
        let _ = fs::remove_dir_all(&second_ctx.scratch_dir);
        let _ = fs::remove_dir_all(&second_ctx.pkgdir);
        return Ok(());
    }

    let diff = diff_trees(&first_entries, &second_entries);
    let _ = fs::remove_dir_all(&second_ctx.scratch_dir);
    match quarantine_mismatch(
        quarantine_root,
        name,
        &version.to_string(),
        &first_ctx.pkgdir,
        &second_ctx.pkgdir,
        &diff,
    ) {
        Ok(dir) => Err(PkgError::Build(format!(
            "{name} {version}: reproducibility check failed — two sandboxed builds \
             produced different trees ({} differing path(s)); both quarantined under {} \
             (see {DIFF_REPORT_NAME} there)",
            diff.len(),
            dir.display()
        ))),
        Err(e) => Err(PkgError::Build(format!(
            "{name} {version}: reproducibility check failed ({} differing path(s)) AND \
             quarantining the trees failed: {e}; first tree remains at {}",
            diff.len(),
            first_ctx.pkgdir.display()
        ))),
    }
}

/// `<dir>` renamed to `<dir>.<tag>` next to itself.
fn sibling(path: &Path, tag: &str) -> PathBuf {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    path.with_file_name(format!("{name}.{tag}"))
}

// ---------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn write_tree(root: &Path, files: &[(&str, &[u8])]) {
        for (rel, contents) in files {
            let path = root.join(rel);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, contents).unwrap();
        }
    }

    #[test]
    fn tree_hash_is_deterministic_and_order_independent() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        // Same logical tree, created in a different order: readdir order
        // must not leak into the hash.
        write_tree(a.path(), &[("usr/lib/libfoo.so", b"elf"), ("usr/bin/foo", b"bin")]);
        write_tree(b.path(), &[("usr/bin/foo", b"bin"), ("usr/lib/libfoo.so", b"elf")]);
        std::os::unix::fs::symlink("libfoo.so", a.path().join("usr/lib/libfoo.so.1")).unwrap();
        std::os::unix::fs::symlink("libfoo.so", b.path().join("usr/lib/libfoo.so.1")).unwrap();

        let ha = tree_hash(a.path()).unwrap();
        assert_eq!(ha, tree_hash(a.path()).unwrap(), "same tree hashes twice identically");
        assert_eq!(ha, tree_hash(b.path()).unwrap(), "creation order must not matter");
    }

    #[test]
    fn tree_hash_detects_content_and_symlink_changes() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        write_tree(a.path(), &[("f", b"one")]);
        write_tree(b.path(), &[("f", b"two")]);
        assert_ne!(tree_hash(a.path()).unwrap(), tree_hash(b.path()).unwrap());

        // Symlink targets are content for this comparison.
        let c = tempfile::tempdir().unwrap();
        let d = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink("x", c.path().join("l")).unwrap();
        std::os::unix::fs::symlink("y", d.path().join("l")).unwrap();
        assert_ne!(tree_hash(c.path()).unwrap(), tree_hash(d.path()).unwrap());

        // An extra file is a difference too.
        write_tree(b.path(), &[("extra", b"!")]);
        assert_ne!(tree_hash(a.path()).unwrap(), tree_hash(b.path()).unwrap());
    }

    #[test]
    fn tree_hash_ignores_store_manifest_bookkeeping() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        write_tree(a.path(), &[("f", b"data")]);
        write_tree(b.path(), &[("f", b"data")]);
        fs::write(b.path().join(MANIFEST_NAME), b"hash  f\n").unwrap();
        assert_eq!(tree_hash(a.path()).unwrap(), tree_hash(b.path()).unwrap());
    }

    #[test]
    fn diff_trees_reports_each_direction() {
        let first = BTreeMap::from([
            ("same".to_string(), "sha256:aa".to_string()),
            ("changed".to_string(), "sha256:bb".to_string()),
            ("gone".to_string(), "sha256:cc".to_string()),
        ]);
        let second = BTreeMap::from([
            ("same".to_string(), "sha256:aa".to_string()),
            ("changed".to_string(), "sha256:dd".to_string()),
            ("new".to_string(), "sha256:ee".to_string()),
        ]);
        let diff = diff_trees(&first, &second);
        assert_eq!(diff.len(), 3);
        assert!(diff.iter().any(|l| l.starts_with("content differs: changed")));
        assert!(diff.iter().any(|l| l == "only in first build: gone"));
        assert!(diff.iter().any(|l| l == "only in second build: new"));
    }

    #[test]
    fn quarantine_layout_moves_both_trees_and_writes_report() {
        let tmp = tempfile::tempdir().unwrap();
        let first = tmp.path().join("first-pkgdir");
        let second = tmp.path().join("second-pkgdir");
        write_tree(&first, &[("usr/bin/x", b"one")]);
        write_tree(&second, &[("usr/bin/x", b"two")]);
        let quarantine = tmp.path().join("quarantine");
        let diff = vec!["content differs: usr/bin/x".to_string()];

        let dir = quarantine_mismatch(&quarantine, "openssl", "3.0.14", &first, &second, &diff)
            .unwrap();

        // <quarantine>/<name>-<version>-<ts>/{first,second}/ + report.
        let dir_name = dir.file_name().unwrap().to_string_lossy().into_owned();
        assert!(dir_name.starts_with("openssl-3.0.14-"), "got {dir_name}");
        assert_eq!(fs::read(dir.join("first/usr/bin/x")).unwrap(), b"one");
        assert_eq!(fs::read(dir.join("second/usr/bin/x")).unwrap(), b"two");
        let report = fs::read_to_string(dir.join(DIFF_REPORT_NAME)).unwrap();
        assert!(report.contains("openssl 3.0.14"));
        assert!(report.contains("content differs: usr/bin/x"));

        // The trees were MOVED — the build dirs no longer exist.
        assert!(!first.exists());
        assert!(!second.exists());
    }
}
