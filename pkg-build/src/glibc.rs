//! pkg-build/src/glibc.rs — the glibc special case (SPEC "pkg-build";
//! v13 delta §1–§6; v11 §3.7 category-1 inline patches).
//!
//! ## v1 changes
//!
//! - Category-1 (security-patch) inline apply: per-file rename swap
//!   directly in the anchor (`<file>.pkg-new` → `rename(2)`), user
//!   prompt before touching the live libc, and a rate-limited restart
//!   nag (timestamp file) pointing at `pkg audit --glibc` /
//!   `pkg apply-pending-restarts` (v13 §7). Category-1 patches are
//!   allowed during a pending window and never touch the staged tree
//!   (v13 §4a).
//! - Major-bump staging: the new tree is assembled into
//!   `/usr/lib/pkg/.stage/glibc-active/`, manifest-verified, the
//!   `/var/lib/pkg/glibc-pending` flag file written FIRST (protocol-1
//!   format, v13 §2), and the DB row recorded second via
//!   `pkg_db::queries::set_pending_glibc` (v13 §4d). A second major
//!   bump while one is pending REPLACES the staged tree — latest
//!   staged wins, there is no queue (v13 §4c) — and the caller is told
//!   the earlier staged version was discarded.
//!
//! ## v2
//!
//! - [FIX] Dropped the unused `GLIBC_ANCHOR_DIR` import from
//!   `pkg_db::integrity` (callers pass the anchor path in).

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use pkg_core::{PkgError, PkgResult, Version};
use pkg_db::integrity::{StagedGlibcManifest, STAGED_GLIBC_DIR};
use pkg_db::queries::{self, PendingGlibc, GLIBC_PENDING_PROTOCOL};
use rusqlite::Connection;
use sha2::{Digest, Sha256};

/// Boot-time source of truth (v13 §1): its existence IS the pending
/// state. Written before the DB row so a crash between the two writes
/// leaves the flag side existing (pkg-db's `set_pending_glibc` doc).
pub const PENDING_FLAG: &str = "/var/lib/pkg/glibc-pending";

/// Rate limiter for the restart nag after a category-1 patch: one nag
/// per day, so every `pkg` invocation doesn't repeat it (v13 §7 makes
/// the *listing* machinery the durable surface; the nag is a pointer).
pub const NAG_STAMP: &str = "/var/lib/pkg/glibc-restart-nag.stamp";

const NAG_INTERVAL_SECS: u64 = 24 * 60 * 60;

// ---------------------------------------------------------------------
// Category-1 inline patches (v11 §3.7, v13 §4a)
// ---------------------------------------------------------------------

/// One file of a category-1 patch: new content plus its path relative
/// to the anchor root.
#[derive(Debug, Clone)]
pub struct InlinePatchFile {
    /// Path to the replacement file (from the just-built store dir).
    pub new_content: PathBuf,
    /// Path relative to the anchor, e.g. `lib/libc.so.6`.
    pub anchor_relative: PathBuf,
}

/// Applies a category-1 patch inline to the active anchor.
///
/// Per-file, the swap is: copy the new content to `<file>.pkg-new`
/// inside the anchor (same filesystem), then `rename(2)` over the live
/// file. Running processes keep the deleted inode (v11 §3.7's caveat,
/// which v13 §7 turns into the supported `pkg audit --glibc` /
/// `pkg apply-pending-restarts` workflow).
///
/// `confirm` is the CLI's prompt (v10 §16.3 interactive confirmation
/// for major actions); a non-interactive caller passes `-y` down as an
/// always-true closure. Category-1 patches are ABI-compatible and
/// deliberately one-way — `pkg undo` excludes glibc (Issue 5 §5.1).
///
/// Never touches the staged tree: during a pending window the staged
/// major bump keeps its own copy (v13 §4a).
pub fn apply_category1_patch(
    anchor: &Path,
    files: &[InlinePatchFile],
    confirm: &mut dyn FnMut(&str) -> bool,
) -> PkgResult<Vec<PathBuf>> {
    if files.is_empty() {
        return Ok(Vec::new());
    }
    let listing = files
        .iter()
        .map(|f| f.anchor_relative.display().to_string())
        .collect::<Vec<_>>()
        .join("\n  ");
    if !confirm(&format!(
        "Apply category-1 glibc security patch inline to the ACTIVE libc at {}?\n  \
         {listing}\n\
         Running processes keep the old libc until restarted.",
        anchor.display()
    )) {
        return Err(PkgError::Build(
            "category-1 glibc patch declined by user".into(),
        ));
    }

    let mut swapped = Vec::new();
    for file in files {
        let dest = anchor.join(&file.anchor_relative);
        if !dest.exists() {
            return Err(PkgError::NotFound(format!(
                "anchor file {} not present — refusing a partial patch",
                dest.display()
            )));
        }
        let tmp = dest.with_extension("pkg-new");
        fs::copy(&file.new_content, &tmp).map_err(PkgError::Io)?;
        fs::rename(&tmp, &dest).map_err(PkgError::Io)?;
        swapped.push(dest);
    }

    // Record the nag (rate-limited) pointing at the restart machinery.
    if let Some(nag) = restart_nag() {
        eprintln!("{nag}");
    }
    Ok(swapped)
}

/// The restart reminder, at most once per [`NAG_INTERVAL_SECS`]. Returns
/// `None` when the last nag is fresh. Stamp-update failures are ignored:
/// the nag is a courtesy, not a gate.
pub fn restart_nag() -> Option<String> {
    let stamp = Path::new(NAG_STAMP);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    if let Ok(meta) = fs::metadata(stamp) {
        if let Ok(modified) = meta.modified() {
            let age = SystemTime::now()
                .duration_since(modified)
                .map(|d| d.as_secs())
                .unwrap_or(u64::MAX);
            if age < NAG_INTERVAL_SECS {
                return None;
            }
        }
    }
    let _ = fs::write(stamp, format!("{now}\n"));
    Some(
        "glibc was patched in place; long-running processes still hold the old libc.\n\
         Run `pkg audit --glibc` to list affected PIDs/services, and\n\
         `pkg apply-pending-restarts` to restart them in dependency order."
            .to_string(),
    )
}

// ---------------------------------------------------------------------
// Major-bump staging (v13 §2, §4)
// ---------------------------------------------------------------------

/// The result of staging a glibc major bump.
#[derive(Debug)]
pub struct StageOutcome {
    /// The previously staged target, when this bump REPLACED a pending
    /// one (v13 §4c: latest-staged-wins, no queue). The caller tells
    /// the user the earlier staged version was discarded.
    pub replaced_previous: Option<Version>,
    /// The DB row / flag-file content now pending.
    pub pending: PendingGlibc,
}

/// Stages a glibc major bump for the boot-time `glibc-apply` swap.
///
/// Order (the crash-safety contract from pkg-db's `set_pending_glibc`
/// doc): assemble + verify the staged tree, write the flag file FIRST,
/// then the DB row. `confirm` prompts before anything is touched.
///
/// `new_tree` is the fully built glibc store content (the recipe's
/// `$PKGDIR`); `glibc_apply_bin` is hashed into the DB row for the
/// v13 §3 drift self-check.
pub fn stage_major_bump(
    conn: &Connection,
    new_tree: &Path,
    target: &Version,
    glibc_apply_bin: &Path,
    confirm: &mut dyn FnMut(&str) -> bool,
) -> PkgResult<StageOutcome> {
    let previous = queries::pending_glibc(conn)?;
    let replaced_previous = previous.as_ref().map(|p| p.target_version.clone());

    let prompt = match &replaced_previous {
        Some(old) => format!(
            "Stage glibc {target} for swap at next boot?\n\
             NOTE: a major bump to {old} is already staged — it will be \
             DISCARDED (latest-staged-wins; there is no queue, v13 §4c)."
        ),
        None => format!("Stage glibc {target} for atomic swap at next boot?"),
    };
    if !confirm(&prompt) {
        return Err(PkgError::Build("glibc major bump declined by user".into()));
    }

    // Assemble the staged tree (replacing any dormant one) and verify
    // it against its own manifest. `glibc-apply` re-verifies at boot
    // against the flag-file hash (v13 §5) and never swaps unverified
    // content — staging-time verification catches corruption early.
    let staged = Path::new(STAGED_GLIBC_DIR);
    if staged.exists() {
        fs::remove_dir_all(staged).map_err(PkgError::Io)?;
    }
    if let Some(parent) = staged.parent() {
        fs::create_dir_all(parent).map_err(PkgError::Io)?;
    }
    if fs::rename(new_tree, staged).is_err() {
        copy_tree(new_tree, staged)?;
    }
    let manifest = StagedGlibcManifest::build(staged)?;
    let staged_sha256 = manifest.manifest_sha256();
    pkg_db::integrity::verify_staged_glibc(staged, &staged_sha256)?;

    let pending = PendingGlibc {
        protocol: GLIBC_PENDING_PROTOCOL,
        target_version: target.clone(),
        staged_sha256: staged_sha256.clone(),
        glibc_apply_sha256: sha256_file(glibc_apply_bin)?,
        created_at: unix_now(),
    };

    // Flag file FIRST (v13 §2 format; the boot-time binary reads the
    // protocol field before anything else and refuses formats it
    // doesn't know).
    write_pending_flag(Path::new(PENDING_FLAG), &pending)?;
    // DB row second (v13 §4d) — lets pre-build checks fail fast from
    // the database alone (`queries::check_glibc_gate`).
    queries::set_pending_glibc(conn, &pending)?;

    Ok(StageOutcome {
        replaced_previous,
        pending,
    })
}

/// The protocol-1 pending-flag format (spec §7.6.2), one field per
/// line: exactly `protocol`, `target`, `manifest_sha256`, `created`
/// (RFC 3339). `glibc_apply_sha256` is deliberately NOT in the flag —
/// §7.6.3 records it in the DB pending row only (see
/// `queries::set_pending_glibc`); the boot-time reader treats it as an
/// optional extension. Kept as a pure function so the exact byte format
/// is unit-testable.
pub fn format_pending_flag(pending: &PendingGlibc) -> String {
    format!(
        "protocol = {}\ntarget = {}\nmanifest_sha256 = {}\ncreated = {}\n",
        pending.protocol,
        pending.target_version,
        pending.staged_sha256,
        rfc3339_utc(pending.created_at),
    )
}

fn write_pending_flag(path: &Path, pending: &PendingGlibc) -> PkgResult<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(PkgError::Io)?;
    }
    // Write-then-rename: a crash mid-write must never leave a torn
    // flag that glibc-apply would half-read.
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, format_pending_flag(pending)).map_err(PkgError::Io)?;
    fs::rename(&tmp, path).map_err(PkgError::Io)?;
    Ok(())
}

/// Reads a pending flag back (used by `pkg info glibc` / audit
/// surfaces). An unrecognized protocol is reported as such, never
/// guessed at — the same fail-safe glibc-apply applies at boot (v13 §2).
pub fn read_pending_flag(path: &Path) -> PkgResult<Option<(u8, String)>> {
    let Ok(text) = fs::read_to_string(path) else {
        return Ok(None);
    };
    let mut protocol = None;
    let mut target = None;
    for line in text.lines() {
        if let Some((k, v)) = line.split_once('=') {
            match (k.trim(), v.trim()) {
                ("protocol", p) => protocol = p.parse::<u8>().ok(),
                ("target", t) => target = Some(t.to_string()),
                _ => {}
            }
        }
    }
    match (protocol, target) {
        (Some(p), Some(t)) => Ok(Some((p, t))),
        _ => Err(PkgError::Parse(format!(
            "malformed glibc-pending flag {}",
            path.display()
        ))),
    }
}

// ---------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------

fn copy_tree(src: &Path, dst: &Path) -> PkgResult<()> {
    fs::create_dir_all(dst).map_err(PkgError::Io)?;
    for entry in fs::read_dir(src).map_err(PkgError::Io)? {
        let entry = entry.map_err(PkgError::Io)?;
        let path = entry.path();
        let target = dst.join(entry.file_name());
        let ftype = entry.file_type().map_err(PkgError::Io)?;
        if ftype.is_dir() {
            copy_tree(&path, &target)?;
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

fn sha256_file(path: &Path) -> PkgResult<String> {
    use std::io::Read;
    let mut file = fs::File::open(path).map_err(PkgError::Io)?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 8192];
    loop {
        let n = file.read(&mut buf).map_err(PkgError::Io)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// RFC 3339 (UTC) rendering of a unix timestamp, std-only (no chrono
/// in the dep pins). Civil-from-days algorithm (Howard Hinnant,
/// public domain).
pub fn rfc3339_utc(ts: i64) -> String {
    let days = ts.div_euclid(86_400);
    let secs = ts.rem_euclid(86_400);
    let (hour, min, sec) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    // days since 1970-01-01 → (year, month, day)
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if m <= 2 { y + 1 } else { y };
    format!("{year:04}-{m:02}-{d:02}T{hour:02}:{min:02}:{sec:02}Z")
}

// ---------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pending_flag_protocol1_format() {
        // Spec §7.6.2: exact one-field-per-line layout, protocol first,
        // exactly the four spec fields in spec order.
        let pending = PendingGlibc {
            protocol: 1,
            target_version: "2.40".parse().unwrap(),
            staged_sha256: "ab".repeat(32),
            glibc_apply_sha256: "cd".repeat(32),
            created_at: 1_720_000_000,
        };
        let text = format_pending_flag(&pending);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 4, "spec §7.6.2 flag has exactly four fields");
        assert_eq!(lines[0], "protocol = 1");
        assert_eq!(lines[1], "target = 2.40");
        assert_eq!(lines[2], format!("manifest_sha256 = {}", "ab".repeat(32)));
        assert_eq!(lines[3], "created = 2024-07-03T09:46:40Z");
        // The DB-only drift hash never leaks into the flag (§7.6.2/§7.6.3).
        assert!(!text.contains("glibc_apply_sha256"));
    }

    #[test]
    fn rfc3339_known_instant() {
        // 1970-01-01T00:00:00Z
        assert_eq!(rfc3339_utc(0), "1970-01-01T00:00:00Z");
        // 2024-07-03T09:46:40Z == 1720000000
        assert_eq!(rfc3339_utc(1_720_000_000), "2024-07-03T09:46:40Z");
    }

    #[test]
    fn flag_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let flag = tmp.path().join("glibc-pending");
        let pending = PendingGlibc {
            protocol: GLIBC_PENDING_PROTOCOL,
            target_version: "2.40".parse().unwrap(),
            staged_sha256: "ff".repeat(32),
            glibc_apply_sha256: "00".repeat(32),
            created_at: 1_720_000_000,
        };
        write_pending_flag(&flag, &pending).unwrap();
        // The on-disk bytes round-trip the spec §7.6.2 field names, so
        // the boot-time glibc-apply (pkg-init) parser accepts them.
        let text = fs::read_to_string(&flag).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(
            lines,
            [
                "protocol = 1".to_string(),
                "target = 2.40".to_string(),
                format!("manifest_sha256 = {}", "ff".repeat(32)),
                "created = 2024-07-03T09:46:40Z".to_string(),
            ]
        );
        let (protocol, target) = read_pending_flag(&flag).unwrap().unwrap();
        assert_eq!(protocol, 1);
        assert_eq!(target, "2.40");
        assert!(read_pending_flag(&tmp.path().join("nonexistent")).unwrap().is_none());
    }
}
