//! Corruption detection and DB/filesystem reconciliation (§9.3).
//!
//! Two independent things can go wrong, checked separately: the SQLite
//! file itself can be corrupted (`integrity_check`), or the database can
//! simply disagree with what's actually sitting in `.store/` on disk —
//! a package recorded but missing, or a directory present that the
//! database has never heard of (`scan_store`). The second case is the
//! backstop §7.5's activation split relies on: if a crash lands between
//! the symlink swap and the DB commit, `scan_store` is what notices on
//! next boot.
//!
//! ## v2 changes
//!
//! - [MAJOR] `scan_store` now filters `installed` rows to `pkg.arch ==
//!   arch` before both scan directions, matching its own doc (one call
//!   per arch). `known_names` is built only from the filtered set, so
//!   directory-name matching is per-arch by construction. Added a
//!   multilib test (amd64 + i686 rows, one tree scanned) proving no
//!   false `MissingFromDisk`.
//! - [MAJOR] `regenerate_orphans` is now transactional: it takes
//!   `&mut Connection` and wraps all inserts in a single
//!   `conn.transaction()`, so a mid-failure rolls the whole batch back
//!   instead of leaving a partially regenerated DB.
//! - [MAJOR] Symlinked store directories are no longer silently skipped
//!   by the `is_dir()` check (`DirEntry::file_type` does not follow
//!   symlinks): they are reported as `UnrecognizedOnDisk`, since a
//!   store entry the DB doesn't own that isn't a real directory is
//!   exactly the "something unexpected wrote here" case.
//! - [MINOR] Documented the precondition that the caller holds the
//!   exclusive `/var/lib/pkg/lock` for the mutating/repair entry points
//!   (and shared-or-exclusive for the read-only scans), per the §9.4
//!   lock contract: the lock is held only for the short critical
//!   section, and regeneration is part of one.
//!
//! ## v4 changes
//!
//! - [FIX] `scan_store`/`regenerate_orphans` now reconcile via
//!   `pkg_core::Identity` (`store_dir_name`/`parse_store_dir_name`)
//!   instead of bare `PackageId`, whose string form omits the
//!   `+p<opt_tag>` suffix that store directories carry for non-default
//!   option selections. Option-tagged packages previously produced
//!   false `MissingFromDisk`/`UnrecognizedOnDisk` discrepancies, and
//!   regeneration reinserted them with a forced `opt_tag = ''`.
//!   `OrphanedOnDisk` now carries an `Identity` and
//!   `regenerate_orphans` takes `&[Identity]`, reinserting via
//!   `insert_package_with_opt_tag` with the recovered tag.
//!
//! ## v3 changes
//!
//! Spec v13 (atomic glibc upgrade) support:
//!
//! - [MAJOR] `StagedGlibcManifest` + `verify_staged_glibc`: hashes every
//!   regular file under the staged glibc tree, builds a sorted
//!   `(relative path, sha256)` manifest, hashes the manifest itself, and
//!   compares it to the expected digest recorded in the `glibc_pending`
//!   DB row / flag file. Called by `glibc-apply` before the
//!   `RENAME_EXCHANGE` swap (spec v13 §5) — unverified content is never
//!   swapped in — and usable by `pkg` pre-reboot as
//!   `pkg verify --staged-glibc`.
//! - [MAJOR] `verify_anchor_manifest`: post-boot verification helper
//!   (spec v13 §6). Compares the ACTIVE `/usr/lib/pkg/glibc-anchor/`
//!   tree against the DB-recorded file list for the installed glibc
//!   package. Returns an `AnchorManifestReport` listing every mismatch
//!   instead of failing on the first one, so the post-boot report is
//!   complete.
//! - [NOTE] The manifest-hash algorithm below is duplicated in the
//!   dependency-free `glibc-apply` static binary (pkg-init crate, which
//!   cannot link pkg-db). The algorithm is documented precisely in both
//!   places and the two implementations MUST stay in lockstep:
//!   `relpath<space>sha256hex\n` lines, sorted by relative path,
//!   SHA-256 over the concatenation.
//! - [XREF] The running-vs-recorded glibc version check (comparing the
//!   loaded glibc's reported version against the DB-recorded installed
//!   version) lives in `queries-v3.rs`, not here; see
//!   `GLIBC_PENDING_PROTOCOL` there, which is also duplicated in
//!   `glibc-apply`.
//! - [DEPS] The v3 additions require the `sha2` crate (already used by
//!   pkg-db for content hashing); add `sha2 = "0.10"` to Cargo.toml if
//!   it is not already a dependency.


use std::collections::HashSet;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use pkg_core::{Arch, Identity, OptTag, PackageId, PkgError, PkgResult, Track};
use rusqlite::Connection;
// v3: content hashing for the staged-glibc manifest (spec v13 §5).
// Dependency note: requires `sha2 = "0.10"` in this crate's Cargo.toml.
use sha2::{Digest, Sha256};

use crate::{
    current_schema_version, insert_package_with_opt_tag, list_installed_packages, PackageRow,
    SCHEMA_VERSION,
};

// ---------------------------------------------------------------------
// SQLite-level integrity
// ---------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IntegrityReport {
    pub sqlite_ok: bool,
    pub sqlite_errors: Vec<String>,
    pub schema_version: Option<i64>,
    pub schema_version_matches: bool,
}

/// Runs SQLite's own `PRAGMA integrity_check` and cross-checks the
/// schema version against what this build of `pkg-db` expects.
///
/// `integrity_check` returns a single row containing the literal text
/// `"ok"` when the database file is healthy, or one row per problem
/// found otherwise — never zero rows, so there's no empty-result case to
/// handle the way `current_schema_version` had to for `schema_meta`.
///
/// Lock precondition: read-only; a shared `/var/lib/pkg/lock` suffices.
pub fn integrity_check(conn: &Connection) -> PkgResult<IntegrityReport> {
    let mut stmt = conn
        .prepare("PRAGMA integrity_check")
        .map_err(|e| PkgError::Db(e.to_string()))?;
    let rows: Vec<String> = stmt
        .query_map([], |row| row.get(0))
        .map_err(|e| PkgError::Db(e.to_string()))?
        .collect::<Result<_, _>>()
        .map_err(|e| PkgError::Db(e.to_string()))?;

    let sqlite_ok = rows.len() == 1 && rows[0] == "ok";
    let sqlite_errors = if sqlite_ok { Vec::new() } else { rows };

    let schema_version = current_schema_version(conn)?;
    let schema_version_matches = schema_version == Some(SCHEMA_VERSION);

    Ok(IntegrityReport {
        sqlite_ok,
        sqlite_errors,
        schema_version,
        schema_version_matches,
    })
}

// ---------------------------------------------------------------------
// DB vs. filesystem reconciliation
// ---------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoreDiscrepancy {
    /// The database has a row for this package, but no matching
    /// directory exists under the store root.
    MissingFromDisk(PackageRow),
    /// A directory exists whose name doesn't parse as a valid store
    /// directory name at all (§7.7) — neither
    /// `Identity::parse_store_dir_name` nor the bare
    /// `PackageId::parse_canonical` fallback accepts it. Also used for
    /// symlinked directories, which the store must never contain (a
    /// symlink is not a package payload and would be silently skipped
    /// otherwise).
    UnrecognizedOnDisk(String),
    /// A directory exists, parses as a valid store directory name
    /// (`Identity::parse_store_dir_name`, `+p<opt_tag>` suffix
    /// included), but has no corresponding row in `packages`. The
    /// `Identity` carries the recovered `opt_tag` so
    /// `regenerate_orphans` can reinsert the row with it intact.
    OrphanedOnDisk(Identity),
}

/// Compares the database against what's actually present under
/// `store_root`, in both directions.
///
/// `arch` is required for the same reason `PackageId::parse_canonical`
/// needs it directly: a directory name alone doesn't encode which
/// architecture it belongs to (§6.1) — that's conveyed by which tree
/// you're scanning (`.store/` vs `lib32/pkg/`), not by the name itself.
/// Scanning both trees means calling this twice, once per `Arch`, and
/// each call compares that tree only against the installed packages
/// *of that arch* — DB rows for other arches are filtered out up front,
/// so a multilib install can't produce false `MissingFromDisk` reports
/// for packages living in the other tree.
///
/// Lock precondition: read-only; a shared `/var/lib/pkg/lock` suffices.
/// Callers that act on the results (e.g. `regenerate_orphans`) must
/// upgrade to the exclusive lock before mutating, per §9.4.
pub fn scan_store(
    conn: &Connection,
    store_root: &Path,
    arch: Arch,
) -> PkgResult<Vec<StoreDiscrepancy>> {
    let installed = list_installed_packages(conn)?;
    // One call per arch (see doc above): restrict the DB side of the
    // comparison to this arch's rows before doing anything else.
    let installed: Vec<PackageRow> = installed
        .into_iter()
        .filter(|pkg| pkg.arch == arch)
        .collect();
    let mut discrepancies = Vec::new();

    // Store dirs retained for restorability (any recorded generation's
    // package set, Issue 2 §3) are legitimate residents: after an
    // upgrade or undo, superseded dirs stay until `pkg clean` releases
    // them. Not orphans — seed the known set with them.
    let mut known_names: HashSet<String> =
        crate::queries::generation_pinned_dir_names(conn, arch)?;

    // DB -> disk: every installed package (of this arch) should have a
    // directory.
    for pkg in &installed {
        // Compare against `Identity::store_dir_name`, not
        // `PackageId::to_string`: store directories append `+p<opt_tag>`
        // for non-default option selections, and the bare `PackageId`
        // form omits it — which used to flag every option-tagged
        // package as MissingFromDisk. A corrupt `opt_tag` in the row
        // (shouldn't happen — it round-trips through `OptTag`) falls
        // back to the all-defaults tag.
        let identity = Identity {
            name: pkg.name.clone(),
            version: pkg.version.clone(),
            abi_tag: pkg.abi_tag,
            opt_tag: pkg.opt_tag.parse().unwrap_or_else(|_| OptTag::none()),
            arch: pkg.arch,
        };
        let canonical = identity.store_dir_name();
        if !store_root.join(&canonical).exists() {
            discrepancies.push(StoreDiscrepancy::MissingFromDisk(pkg.clone()));
        }
        known_names.insert(canonical);
    }
    // `known_names` is now per-arch by construction — every name in it
    // came from a row with `pkg.arch == arch` — so the disk->DB pass
    // below only ever excuses directories this tree legitimately owns.

    // disk -> DB: every directory should correspond to a known package.
    // `fs::read_dir` returns `io::Result`, and `?` converts its
    // `io::Error` straight into `PkgError` here via `pkg-core`'s
    // existing `impl From<io::Error> for PkgError` — usable from
    // `pkg-db` even though `pkg-db` didn't write it, since the orphan
    // rule only restricts *defining* a trait impl, not *using* one that
    // already exists. That's the opposite situation from `schema.rs`'s
    // `rusqlite::Error`: no equivalent `From` impl exists for it
    // anywhere in the workspace, so that file needed `.map_err(...)` at
    // every call instead of a bare `?`.
    for entry in fs::read_dir(store_root)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        // `file_type` does NOT follow symlinks: a symlinked directory
        // reports `is_dir() == false`. The store must only ever contain
        // real directories, so a symlinked dir isn't "not a package" —
        // it's unexpected content and gets reported rather than skipped.
        if file_type.is_symlink() {
            if entry.path().is_dir() {
                discrepancies.push(StoreDiscrepancy::UnrecognizedOnDisk(
                    entry.file_name().to_string_lossy().into_owned(),
                ));
            }
            continue;
        }
        if !file_type.is_dir() {
            continue;
        }
        let dir_name = entry.file_name().to_string_lossy().into_owned();
        // The staging area (pkg-build's `STAGE_DIR_NAME`) is a
        // legitimate non-package resident of the store root: activation
        // assembles trees there before the atomic rename.
        if dir_name == ".stage" {
            continue;
        }
        if known_names.contains(&dir_name) {
            continue;
        }
        match Identity::parse_store_dir_name(&dir_name, arch) {
            Ok(identity) => discrepancies.push(StoreDiscrepancy::OrphanedOnDisk(identity)),
            // Fallback: `parse_store_dir_name` also rejects an opt
            // suffix that isn't a valid tag (e.g. `+pZZZ`); try the
            // plain `PackageId` grammar before giving up on the name.
            Err(_) => match PackageId::parse_canonical(&dir_name, arch) {
                Ok(id) => discrepancies.push(StoreDiscrepancy::OrphanedOnDisk(Identity {
                    name: id.name,
                    version: id.version,
                    abi_tag: id.abi_tag,
                    opt_tag: OptTag::none(),
                    arch: id.arch,
                })),
                Err(_) => discrepancies.push(StoreDiscrepancy::UnrecognizedOnDisk(dir_name)),
            },
        }
    }

    Ok(discrepancies)
}

// ---------------------------------------------------------------------
// Regeneration
// ---------------------------------------------------------------------

/// Values needed to insert a `packages` row that aren't recoverable from
/// a `.store/` directory name.
///
/// `Identity::parse_store_dir_name` can recover identity — name,
/// version, ABI tag, and the `opt_tag` of a non-default option
/// selection — from a directory name, because that's what the name
/// actually encodes. `track`, `installed_at`, and `installed_explicitly`
/// aren't encoded anywhere in the current filesystem layout (§6.1) —
/// there's no sidecar metadata file recording them — so `regenerate_orphans`
/// takes them explicitly rather than guessing. A fabricated value
/// presented as recovered fact would be worse than an honest, visible
/// default the caller chose on purpose: "explicit over implicit" (§2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RegenerationDefaults {
    pub track: Track,
    pub installed_at: i64,
    pub installed_explicitly: bool,
}

/// Inserts `packages` rows for identities found on disk but missing from
/// the database — the repair half of `pkg db regenerate` (§9.3). Returns
/// the newly assigned row ids, in the same order as `orphans`.
///
/// All inserts run inside a single SQLite transaction: if any insert
/// fails (e.g. a constraint violation partway through `orphans`), the
/// whole batch is rolled back rather than leaving the database in a
/// half-regenerated state that a subsequent `scan_store` would no
/// longer flag as discrepant. That's why this takes `&mut Connection`
/// where the read-only functions take `&Connection`.
///
/// Lock precondition (§9.4): the caller must hold the exclusive
/// `/var/lib/pkg/lock`. This is a write operation against installed
/// state, and per the lock contract it must run inside the short
/// critical section alongside the scan that produced `orphans` — not
/// split across a lock release, where another writer could interleave.
pub fn regenerate_orphans(
    conn: &mut Connection,
    orphans: &[Identity],
    defaults: &RegenerationDefaults,
) -> PkgResult<Vec<i64>> {
    let tx = conn
        .transaction()
        .map_err(|e| PkgError::Db(e.to_string()))?;
    let mut ids = Vec::with_capacity(orphans.len());
    for identity in orphans {
        let id = PackageId {
            name: identity.name.clone(),
            version: identity.version.clone(),
            abi_tag: identity.abi_tag,
            arch: identity.arch,
        };
        // Reinsert with the `opt_tag` recovered from the directory
        // name — a forced `''` here would make the regenerated row
        // disagree with the very directory it was recovered from on the
        // next `scan_store` (UNIQUE includes opt_tag since schema v4).
        ids.push(insert_package_with_opt_tag(
            &tx,
            &id,
            identity.opt_tag.as_str(),
            defaults.track,
            defaults.installed_at,
            defaults.installed_explicitly,
        )?);
    }
    tx.commit().map_err(|e| PkgError::Db(e.to_string()))?;
    Ok(ids)
}

// ---------------------------------------------------------------------
// v3: staged glibc verification (spec v13 §5)
// ---------------------------------------------------------------------

/// Default staged glibc tree location (spec v13 §1). `verify_staged_glibc`
/// still takes the root as a parameter so tests and alternate roots work;
/// this constant is what production callers pass.
pub const STAGED_GLIBC_DIR: &str = "/usr/lib/pkg/.stage/glibc-active";

/// Default active glibc anchor tree location (spec v13 §1).
pub const GLIBC_ANCHOR_DIR: &str = "/usr/lib/pkg/glibc-anchor";

/// A single (relative path, sha256 hex) pair from a staged glibc tree.
pub type ManifestEntry = (String, String);

/// A sorted manifest of every regular file in a staged glibc tree.
///
/// MANIFEST-HASH ALGORITHM (frozen, spec v13 §5 — keep in lockstep with
/// the independent implementation in `glibc-apply.rs`):
///
/// 1. Walk the tree recursively, collecting every REGULAR file
///    (`DirEntry::file_type` — lstat semantics, so SYMLINKS ARE
///    SKIPPED, as are sockets/fifos; the staged tree must contain no
///    symlinks, and one appearing here is simply not hashed).
/// 2. For each file compute SHA-256 of its raw contents, hex-encoded
///    lowercase (64 chars).
/// 3. Form one line per file: `relpath<space>sha256hex\n`, where
///    `relpath` is the path relative to the staged root, using `/`
///    separators (Unix — paths are built with `join` on Linux so this
///    holds by construction).
/// 4. Sort the lines by `relpath` (byte-wise, ascending).
/// 5. The manifest hash is SHA-256 of the concatenation of those lines,
///    hex-encoded lowercase.
///
/// An empty tree therefore hashes to SHA-256 of the empty byte string
/// (`e3b0c442...`), which never matches a real staged digest — treating
/// an empty stage as "corrupt" is the desired fail-safe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StagedGlibcManifest {
    /// Sorted by relative path (byte-wise ascending).
    pub entries: Vec<ManifestEntry>,
}

impl StagedGlibcManifest {
    /// Builds the manifest for the tree rooted at `staged_root` by
    /// hashing every regular file found.
    ///
    /// Errors on: missing/non-directory root, unreadable directory
    /// entries, and any file that fails to open or read (a file that
    /// can't be hashed can't be verified, so it must not be skipped).
    pub fn build(staged_root: &Path) -> PkgResult<Self> {
        if !staged_root.is_dir() {
            // `From<io::Error> for PkgError` already exists in pkg-core
            // (see `scan_store` above), so a NotFound io::Error converts
            // cleanly without assuming extra PkgError variants.
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("staged glibc tree missing: {}", staged_root.display()),
            )
            .into());
        }
        let mut entries = Vec::new();
        collect_tree(staged_root, staged_root, &mut entries)?;
        entries.sort();
        Ok(Self { entries })
    }

    /// The frozen manifest-hash algorithm (see struct docs): SHA-256 of
    /// the concatenated sorted `relpath<space>sha256hex\n` lines,
    /// hex-encoded.
    pub fn manifest_sha256(&self) -> String {
        let mut hasher = Sha256::new();
        for (relpath, hex) in &self.entries {
            hasher.update(relpath.as_bytes());
            hasher.update(b" ");
            hasher.update(hex.as_bytes());
            hasher.update(b"\n");
        }
        hex_lower(&hasher.finalize())
    }
}

/// Streams one file through SHA-256 in 64 KiB chunks (same hashing style
/// as the rest of pkg-db's content hashing: chunked reads, hex-lowered
/// digest out). Errors propagate — an unreadable file is fatal to the
/// whole verification, never silently skipped.
fn sha256_file(path: &Path) -> PkgResult<String> {
    let mut file = fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex_lower(&hasher.finalize()))
}

fn hex_lower(digest: &[u8]) -> String {
    let mut out = String::with_capacity(digest.len() * 2);
    for b in digest {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// Recursive walker for `StagedGlibcManifest::build`. Only regular files
/// are recorded; directories are descended into; anything else (sockets,
/// fifos) is skipped. `strip_prefix` on a path produced by `join`ing
/// from `root` cannot fail, so the `expect` is unreachable in practice.
fn collect_tree(root: &Path, dir: &Path, out: &mut Vec<ManifestEntry>) -> PkgResult<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            collect_tree(root, &path, out)?;
        } else if file_type.is_file() {
            let rel = path
                .strip_prefix(root)
                .expect("walker path is always under root")
                .to_string_lossy()
                .into_owned();
            out.push((rel, sha256_file(&path)?));
        }
    }
    Ok(())
}

/// Verifies a staged glibc tree against the expected manifest digest
/// recorded in the `glibc_pending` DB row / flag file (spec v13 §5).
///
/// Called by `glibc-apply` immediately before the `RENAME_EXCHANGE`
/// swap: content that fails verification is NEVER swapped in. Also
/// usable by `pkg` pre-reboot as `pkg verify --staged-glibc`.
///
/// Fails (returns `Err`) on: missing staged root, unreadable file, or
/// manifest-hash mismatch. `expected_sha256` is compared case- and
/// whitespace-insensitively against the computed lowercase hex digest.
///
/// Lock precondition: read-only; a shared `/var/lib/pkg/lock` suffices
/// (§9.4).
pub fn verify_staged_glibc(staged_root: &Path, expected_sha256: &str) -> Result<(), PkgError> {
    let manifest = StagedGlibcManifest::build(staged_root)?;
    let actual = manifest.manifest_sha256();
    let expected = expected_sha256.trim().to_ascii_lowercase();
    if actual != expected {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "staged glibc manifest mismatch: expected {expected}, computed {actual} \
                 ({} files under {})",
                manifest.entries.len(),
                staged_root.display()
            ),
        )
        .into());
    }
    Ok(())
}

// ---------------------------------------------------------------------
// v3: post-boot anchor tree verification (spec v13 §6)
// ---------------------------------------------------------------------

/// One way the active anchor tree can disagree with the DB-recorded
/// file list for the installed glibc package.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AnchorMismatch {
    /// DB records this file but it is absent from the anchor tree.
    MissingOnDisk(String),
    /// File exists but its SHA-256 differs from the recorded digest.
    HashMismatch {
        path: String,
        expected: String,
        actual: String,
    },
    /// File present on disk that the DB file list doesn't record —
    /// unexpected content inside the anchor.
    UnexpectedOnDisk(String),
    /// A recorded file exists but could not be read/hashed.
    Unreadable { path: String, error: String },
}

/// Post-boot verification report (spec v13 §6). `is_clean()` is the
/// pass/fail summary; the individual mismatches are all collected so
/// the report is complete rather than failing on the first problem.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnchorManifestReport {
    pub anchor_root: PathBuf,
    pub files_checked: usize,
    pub mismatches: Vec<AnchorMismatch>,
}

impl AnchorManifestReport {
    pub fn is_clean(&self) -> bool {
        self.mismatches.is_empty()
    }
}

/// Verifies the ACTIVE `/usr/lib/pkg/glibc-anchor/` tree against the
/// DB-recorded file list for the installed glibc package (spec v13 §6).
///
/// `expected` is the `(relative path, sha256 hex)` list for the
/// installed glibc package, supplied by the caller (queried from the
/// DB by the caller — this function deliberately takes no `Connection`
/// so it stays usable from early-boot / rescue contexts without a DB
/// dependency). Paths are relative to `anchor_root`.
///
/// Unlike `verify_staged_glibc` this does NOT fail fast: every
/// discrepancy is collected into the returned report so the post-boot
/// check reports the full damage in one pass.
///
/// Lock precondition: read-only; a shared `/var/lib/pkg/lock` suffices.
pub fn verify_anchor_manifest(
    anchor_root: &Path,
    expected: &[(String, String)],
) -> AnchorManifestReport {
    let mut mismatches = Vec::new();
    let mut files_checked = 0usize;
    let mut expected_rels: HashSet<String> = HashSet::with_capacity(expected.len());

    for (rel, expected_hex) in expected {
        expected_rels.insert(rel.clone());
        let path = anchor_root.join(rel);
        if !path.is_file() {
            mismatches.push(AnchorMismatch::MissingOnDisk(rel.clone()));
            continue;
        }
        match sha256_file(&path) {
            Ok(actual) => {
                files_checked += 1;
                if !actual.eq_ignore_ascii_case(expected_hex.trim()) {
                    mismatches.push(AnchorMismatch::HashMismatch {
                        path: rel.clone(),
                        expected: expected_hex.trim().to_ascii_lowercase(),
                        actual,
                    });
                }
            }
            Err(e) => mismatches.push(AnchorMismatch::Unreadable {
                path: rel.clone(),
                error: e.to_string(),
            }),
        }
    }

    // disk -> DB: anything regular on disk that the file list doesn't
    // know about is unexpected content in the anchor.
    let mut on_disk = Vec::new();
    if collect_tree(anchor_root, anchor_root, &mut on_disk).is_ok() {
        for (rel, _) in on_disk {
            if !expected_rels.contains(&rel) {
                mismatches.push(AnchorMismatch::UnexpectedOnDisk(rel));
            }
        }
    } else {
        mismatches.push(AnchorMismatch::Unreadable {
            path: anchor_root.display().to_string(),
            error: "anchor tree walk failed".to_string(),
        });
    }

    AnchorManifestReport {
        anchor_root: anchor_root.to_path_buf(),
        files_checked,
        mismatches,
    }
}

// ---------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{init_schema, insert_package};
    use pkg_core::AbiTag;
    use tempfile::tempdir;

    fn fresh_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap();
        conn
    }

    fn test_id(name: &str) -> PackageId {
        PackageId {
            name: name.to_string(),
            version: "1.0.0".parse().unwrap(),
            abi_tag: AbiTag(1),
            arch: Arch::Amd64,
        }
    }

    fn test_id_arch(name: &str, arch: Arch) -> PackageId {
        PackageId {
            arch,
            ..test_id(name)
        }
    }

    /// The `Identity` form of `test_id` — what `scan_store` hands to
    /// `regenerate_orphans` for a default (no `+p` suffix) directory.
    fn test_identity(name: &str) -> Identity {
        let id = test_id(name);
        Identity {
            name: id.name,
            version: id.version,
            abi_tag: id.abi_tag,
            opt_tag: OptTag::none(),
            arch: id.arch,
        }
    }

    #[test]
    fn integrity_check_ok_and_version_matches_on_fresh_db() {
        let conn = fresh_db();
        let report = integrity_check(&conn).unwrap();
        assert!(report.sqlite_ok);
        assert!(report.sqlite_errors.is_empty());
        assert_eq!(report.schema_version, Some(SCHEMA_VERSION));
        assert!(report.schema_version_matches);
    }

    #[test]
    fn scan_store_reports_nothing_when_db_and_disk_agree() {
        let conn = fresh_db();
        let dir = tempdir().unwrap();
        let id = test_id("openssl");
        insert_package(&conn, &id, Track::Binary, 0, true).unwrap();
        fs::create_dir(dir.path().join(id.to_string())).unwrap();

        let discrepancies = scan_store(&conn, dir.path(), Arch::Amd64).unwrap();
        assert!(discrepancies.is_empty());
    }

    #[test]
    fn scan_store_finds_missing_from_disk() {
        let conn = fresh_db();
        let dir = tempdir().unwrap();
        insert_package(&conn, &test_id("openssl"), Track::Binary, 0, true).unwrap();
        // No directory created for it.

        let discrepancies = scan_store(&conn, dir.path(), Arch::Amd64).unwrap();
        assert_eq!(discrepancies.len(), 1);
        assert!(matches!(
            &discrepancies[0],
            StoreDiscrepancy::MissingFromDisk(row) if row.name == "openssl"
        ));
    }

    #[test]
    fn scan_store_multilib_ignores_other_archs_rows() {
        // Multilib: rows exist for both amd64 and i686, but only the
        // amd64 tree is scanned. The i686 row lives in lib32/pkg/ — a
        // different tree scanned by a different call — so scanning this
        // tree must not report it as MissingFromDisk.
        let conn = fresh_db();
        let dir = tempdir().unwrap();
        let amd64 = test_id_arch("openssl", Arch::Amd64);
        let i686 = test_id_arch("openssl", Arch::I686);
        insert_package(&conn, &amd64, Track::Binary, 0, true).unwrap();
        insert_package(&conn, &i686, Track::Binary, 0, true).unwrap();
        // Only the amd64 tree's directory exists here.
        fs::create_dir(dir.path().join(amd64.to_string())).unwrap();

        let discrepancies = scan_store(&conn, dir.path(), Arch::Amd64).unwrap();
        assert!(
            discrepancies.is_empty(),
            "i686 row must not be flagged while scanning the amd64 tree: {discrepancies:?}"
        );
    }

    #[test]
    fn scan_store_finds_orphaned_on_disk_with_correct_identity() {
        let conn = fresh_db();
        let dir = tempdir().unwrap();
        // Directory present, nothing in the DB — and using the
        // hyphenated-name hazard deliberately, to prove scan_store
        // inherits parse_canonical's correctness, not just the happy path.
        fs::create_dir(dir.path().join("ca-certificates-3.0.12+ab1")).unwrap();

        let discrepancies = scan_store(&conn, dir.path(), Arch::Amd64).unwrap();
        assert_eq!(discrepancies.len(), 1);
        match &discrepancies[0] {
            StoreDiscrepancy::OrphanedOnDisk(id) => {
                assert_eq!(id.name, "ca-certificates");
                assert_eq!(id.version.to_string(), "3.0.12");
            }
            other => panic!("expected OrphanedOnDisk, got {other:?}"),
        }
    }

    #[test]
    fn scan_store_finds_unrecognized_on_disk() {
        let conn = fresh_db();
        let dir = tempdir().unwrap();
        fs::create_dir(dir.path().join("this-is-not-a-valid-package-id")).unwrap();

        let discrepancies = scan_store(&conn, dir.path(), Arch::Amd64).unwrap();
        assert_eq!(discrepancies.len(), 1);
        assert!(matches!(
            &discrepancies[0],
            StoreDiscrepancy::UnrecognizedOnDisk(name) if name == "this-is-not-a-valid-package-id"
        ));
    }

    #[cfg(unix)]
    #[test]
    fn scan_store_reports_symlinked_dir_as_unrecognized() {
        let conn = fresh_db();
        let dir = tempdir().unwrap();
        let target = tempdir().unwrap();
        std::os::unix::fs::symlink(target.path(), dir.path().join("openssl-1.0.0+ab1")).unwrap();

        let discrepancies = scan_store(&conn, dir.path(), Arch::Amd64).unwrap();
        assert_eq!(discrepancies.len(), 1);
        assert!(matches!(
            &discrepancies[0],
            StoreDiscrepancy::UnrecognizedOnDisk(name) if name == "openssl-1.0.0+ab1"
        ));
    }

    #[test]
    fn scan_store_ignores_non_directory_entries() {
        let conn = fresh_db();
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("stray-file.txt"), b"not a package").unwrap();

        let discrepancies = scan_store(&conn, dir.path(), Arch::Amd64).unwrap();
        assert!(discrepancies.is_empty());
    }

    #[test]
    fn regenerate_orphans_inserts_with_supplied_defaults() {
        let mut conn = fresh_db();
        let orphan = Identity {
            name: "ca-certificates".to_string(),
            version: "3.0.12".parse().unwrap(),
            abi_tag: AbiTag(1),
            opt_tag: OptTag::none(),
            arch: Arch::Amd64,
        };
        let defaults = RegenerationDefaults {
            track: Track::Binary,
            installed_at: 1_700_000_000,
            installed_explicitly: false,
        };

        let ids = regenerate_orphans(&mut conn, &[orphan.clone()], &defaults).unwrap();
        assert_eq!(ids.len(), 1);

        let row = crate::get_package_by_id(&conn, ids[0]).unwrap().unwrap();
        assert_eq!(row.name, "ca-certificates");
        assert_eq!(row.track, Track::Binary);
        assert_eq!(row.installed_at, 1_700_000_000);
        assert!(!row.installed_explicitly);
    }

    #[test]
    fn regenerate_orphans_rolls_back_entirely_on_mid_failure() {
        let mut conn = fresh_db();
        // The second orphan has the same identity as the first: its
        // insert violates UNIQUE(name, version, abi_tag, opt_tag, arch).
        // The batch must fail AND the first insert must be rolled back too.
        let orphan = test_identity("openssl");
        let defaults = RegenerationDefaults {
            track: Track::Binary,
            installed_at: 0,
            installed_explicitly: false,
        };

        let result = regenerate_orphans(&mut conn, &[orphan.clone(), orphan.clone()], &defaults);
        assert!(result.is_err());
        assert!(
            crate::list_installed_packages(&conn).unwrap().is_empty(),
            "a failed batch must leave no partial inserts behind"
        );
    }

    #[test]
    fn regenerate_then_scan_reports_no_further_discrepancy() {
        let mut conn = fresh_db();
        let dir = tempdir().unwrap();
        let orphan = test_identity("openssl");
        fs::create_dir(dir.path().join(orphan.to_string())).unwrap();

        // Before regeneration: one OrphanedOnDisk.
        let before = scan_store(&conn, dir.path(), Arch::Amd64).unwrap();
        assert_eq!(before.len(), 1);

        let defaults = RegenerationDefaults {
            track: Track::Binary,
            installed_at: 0,
            installed_explicitly: false,
        };
        regenerate_orphans(&mut conn, &[orphan], &defaults).unwrap();

        // After: DB and disk agree.
        let after = scan_store(&conn, dir.path(), Arch::Amd64).unwrap();
        assert!(after.is_empty());
    }

    #[test]
    fn scan_store_and_regenerate_round_trip_option_tagged_packages() {
        // Regression test for the v4 fix: store directories for
        // non-default option selections carry a `+p<opt_tag>` suffix.
        // Reconciling via bare `PackageId` (no opt tag) produced false
        // MissingFromDisk/UnrecognizedOnDisk pairs, and regeneration
        // reinserted the row with a forced `opt_tag = ''`.
        let mut conn = fresh_db();
        let dir = tempdir().unwrap();
        let id = test_id("openssl");

        // DB -> disk: an installed option-tagged package whose
        // `+p`-suffixed directory exists must not be flagged. The
        // directory name is derived through the canonical
        // `store_dir_name` (normalized version rendering included),
        // the same way the real store creates it.
        insert_package_with_opt_tag(&conn, &id, "3f9a1b2c", Track::Binary, 0, true).unwrap();
        let tagged = Identity {
            name: id.name.clone(),
            version: id.version.clone(),
            abi_tag: id.abi_tag,
            opt_tag: "3f9a1b2c".parse().unwrap(),
            arch: id.arch,
        };
        fs::create_dir(dir.path().join(tagged.store_dir_name())).unwrap();
        let discrepancies = scan_store(&conn, dir.path(), Arch::Amd64).unwrap();
        assert!(
            discrepancies.is_empty(),
            "option-tagged package must reconcile both ways: {discrepancies:?}"
        );

        // disk -> DB: an unknown option-tagged directory is an orphan
        // whose recovered Identity carries the tag...
        fs::create_dir(dir.path().join("zlib-1.3+ab1+p00ff00ff")).unwrap();
        let discrepancies = scan_store(&conn, dir.path(), Arch::Amd64).unwrap();
        let orphan = match &discrepancies[..] {
            [StoreDiscrepancy::OrphanedOnDisk(identity)] => identity.clone(),
            other => panic!("expected one OrphanedOnDisk, got {other:?}"),
        };
        assert_eq!(orphan.name, "zlib");
        assert_eq!(orphan.opt_tag.as_str(), "00ff00ff");

        // ...and regeneration reinserts it WITH the tag, so a rescan
        // finds the DB and disk in agreement.
        let defaults = RegenerationDefaults {
            track: Track::Binary,
            installed_at: 0,
            installed_explicitly: false,
        };
        let ids = regenerate_orphans(&mut conn, &[orphan], &defaults).unwrap();
        let row = crate::get_package_by_id(&conn, ids[0]).unwrap().unwrap();
        assert_eq!(row.opt_tag, "00ff00ff");
        let after = scan_store(&conn, dir.path(), Arch::Amd64).unwrap();
        assert!(after.is_empty(), "rescan after regeneration: {after:?}");
    }

    // ---------------- v3: staged glibc / anchor manifest ----------------

    /// Builds a staged tree with known contents and returns the tempdir
    /// plus the expected manifest digest computed the same way
    /// `glibc-apply`'s flag file records it (via the public API, so the
    /// test exercises the same algorithm both sides rely on).
    fn make_staged_tree() -> (tempfile::TempDir, String) {
        let dir = tempdir().unwrap();
        fs::create_dir_all(dir.path().join("lib")).unwrap();
        fs::create_dir_all(dir.path().join("lib64")).unwrap();
        fs::write(dir.path().join("lib/libc.so.6"), b"fake glibc 2.40").unwrap();
        fs::write(dir.path().join("lib/ld-linux-x86-64.so.2"), b"fake loader").unwrap();
        fs::write(dir.path().join("lib64/ld.so.conf"), b"/lib\n").unwrap();
        let manifest = StagedGlibcManifest::build(dir.path()).unwrap();
        let digest = manifest.manifest_sha256();
        assert_eq!(manifest.entries.len(), 3);
        // Sorted by relative path, byte-wise.
        assert_eq!(manifest.entries[0].0, "lib/ld-linux-x86-64.so.2");
        assert_eq!(manifest.entries[1].0, "lib/libc.so.6");
        assert_eq!(manifest.entries[2].0, "lib64/ld.so.conf");
        (dir, digest)
    }

    #[test]
    fn verify_staged_glibc_accepts_matching_tree() {
        let (dir, digest) = make_staged_tree();
        verify_staged_glibc(dir.path(), &digest).unwrap();
        // Case/whitespace tolerance for flag-file sloppiness.
        verify_staged_glibc(dir.path(), &format!("  {} \n", digest.to_uppercase())).unwrap();
    }

    #[test]
    fn verify_staged_glibc_rejects_missing_dir() {
        let dir = tempdir().unwrap();
        let err = verify_staged_glibc(&dir.path().join("nope"), "00").unwrap_err();
        assert!(err.to_string().contains("missing"), "{err}");
    }

    #[test]
    fn verify_staged_glibc_rejects_hash_mismatch_and_content_change() {
        let (dir, digest) = make_staged_tree();
        // Wrong expected digest.
        assert!(verify_staged_glibc(dir.path(), &"0".repeat(64)).is_err());
        // Right digest, but content changed after staging.
        fs::write(dir.path().join("lib/libc.so.6"), b"tampered").unwrap();
        assert!(verify_staged_glibc(dir.path(), &digest).is_err());
    }

    #[test]
    fn verify_staged_glibc_rejects_added_file() {
        let (dir, digest) = make_staged_tree();
        fs::write(dir.path().join("lib/evil.so"), b"extra").unwrap();
        assert!(verify_staged_glibc(dir.path(), &digest).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn verify_staged_glibc_rejects_unreadable_file() {
        use std::os::unix::fs::PermissionsExt;
        let (dir, digest) = make_staged_tree();
        fs::set_permissions(
            dir.path().join("lib/libc.so.6"),
            fs::Permissions::from_mode(0o000),
        )
        .unwrap();
        // Root may still read mode-000 files; skip rather than lie.
        if fs::read(dir.path().join("lib/libc.so.6")).is_err() {
            assert!(verify_staged_glibc(dir.path(), &digest).is_err());
        }
    }

    fn expected_list(dir: &Path) -> Vec<(String, String)> {
        StagedGlibcManifest::build(dir).unwrap().entries
    }

    #[test]
    fn verify_anchor_manifest_clean_tree() {
        let (dir, _) = make_staged_tree();
        let report = verify_anchor_manifest(dir.path(), &expected_list(dir.path()));
        assert!(report.is_clean(), "{:?}", report.mismatches);
        assert_eq!(report.files_checked, 3);
    }

    #[test]
    fn verify_anchor_manifest_collects_all_mismatch_kinds() {
        let (dir, _) = make_staged_tree();
        let expected = expected_list(dir.path());
        // Mutate one file, delete another, add a third.
        fs::write(dir.path().join("lib/libc.so.6"), b"tampered").unwrap();
        fs::remove_file(dir.path().join("lib64/ld.so.conf")).unwrap();
        fs::write(dir.path().join("lib/evil.so"), b"extra").unwrap();

        let report = verify_anchor_manifest(dir.path(), &expected);
        assert!(!report.is_clean());
        assert_eq!(report.mismatches.len(), 3, "{:?}", report.mismatches);
        assert!(report.mismatches.iter().any(|m| matches!(
            m,
            AnchorMismatch::HashMismatch { path, .. } if path == "lib/libc.so.6"
        )));
        assert!(report
            .mismatches
            .iter()
            .any(|m| matches!(m, AnchorMismatch::MissingOnDisk(p) if p == "lib64/ld.so.conf")));
        assert!(report
            .mismatches
            .iter()
            .any(|m| matches!(m, AnchorMismatch::UnexpectedOnDisk(p) if p == "lib/evil.so")));
    }

    #[test]
    fn verify_anchor_manifest_missing_anchor_reports_everything_missing() {
        let (dir, _) = make_staged_tree();
        let expected = expected_list(dir.path());
        let empty = tempdir().unwrap();
        let report = verify_anchor_manifest(&empty.path().join("absent"), &expected);
        assert!(!report.is_clean());
        assert_eq!(
            report
                .mismatches
                .iter()
                .filter(|m| matches!(m, AnchorMismatch::MissingOnDisk(_)))
                .count(),
            3
        );
    }
}
