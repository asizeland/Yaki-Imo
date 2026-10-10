//! ## v2 changes
//!
//! - [FIX] The test schema's `packages` DDL gained
//!   `opt_tag TEXT NOT NULL DEFAULT ''` (matching pkg-db's schema-v4
//!   `packages` table, positioned after `abi_tag`) —
//!   `pkg_db::list_installed_packages` SELECTs `opt_tag`, so every
//!   audit test failed at runtime against the old DDL.
//! - [FIX] The local joined-row struct `ProvenanceRow` was renamed to
//!   `ProvenanceRecord`: it shadowed `pkg_db::ProvenanceRow`, and both
//!   crates re-export their items with `pub use ...::*`, so any
//!   downstream `use pkg_audit::*; use pkg_db::*;` that named
//!   `ProvenanceRow` hit a glob-import ambiguity error.
//!
//! ## v1 (initial)
//!
//! `pkg audit --supply-chain` (v14 §4 / Issue 3.6). Per installed
//! package, report:
//!
//! - **attestation count vs. requirement** — critical-set packages need
//!   ≥2 distinct reviewer attestations distinct from the committer;
//!   the count comes from the provenance row's recorded fingerprints;
//! - **reproducibility self-check result** — `self-match` / `mismatch`
//!   / NULL (never double-built); a `mismatch` is a hard finding, and a
//!   non-empty quarantine dir is a hard finding on its own;
//! - **provenance completeness** — a package with no provenance row
//!   (pre-v14 install) is a *warning*, honestly reported rather than
//!   treated as clean;
//! - **fetch-origin drift (T7)** — URLs recorded at fetch time that are
//!   no longer (or never were) declared by the current recipe manifest.
//!
//! T2 (index-hash repinning events) is **not implementable client-side**
//! with the current data model: detecting a repin requires the history
//! of index entries, and the client retains only the latest index.
//! Recorded as a known gap; the provenance table's `index_version`
//! gives the raw material for a future index-history check.
//!
//! # Exit codes (v14 §4)
//!
//! 0 = clean, 1 = warnings only, 2 = hard findings. A hard finding
//! **blocks** subsequent `pkg install`/`pkg upgrade` until acknowledged
//! via `pkg audit --supply-chain --acknowledge <package>` — the same
//! blocking surface as stale-index enforcement (v11 §16.4). The block
//! check is [`unacknowledged_hard_findings`]; install/upgrade call it
//! and refuse while it returns non-empty.
//!
//! # Acknowledgements table (created on demand)
//!
//! ```sql
//! CREATE TABLE IF NOT EXISTS supply_chain_acknowledgements (
//!     id          INTEGER PRIMARY KEY AUTOINCREMENT,
//!     package     TEXT NOT NULL,          -- package name
//!     version     TEXT NOT NULL,          -- version string at ack time
//!     finding_kind TEXT NOT NULL,         -- FindingKind::as_str()
//!     acknowledged_at INTEGER NOT NULL,   -- unix seconds
//!     reason      TEXT                    -- operator-supplied, may be NULL
//! );
//! ```
//!
//! An ack is scoped to (package, version, finding_kind): a *new*
//! version or a *different* hard finding re-blocks. Acks are a deliberate
//! operator statement "I have seen this finding," not a fix — they
//! unblock installs, they don't make the finding disappear from reports.
//!
//! # Provenance access
//!
//! The `provenance` table (SPEC pkg-db schema v4) is queried directly
//! via rusqlite on the caller's connection — pkg-audit shares pkg-db's
//! connection rather than opening the DB twice. A DB that predates the
//! v4 migration (no `provenance` table) yields per-package
//! `MissingProvenance` warnings, never a hard failure: unaudited
//! provenance from pre-v14 installs is exactly the exit-1 case the spec
//! names.

use std::collections::BTreeMap;
use std::path::Path;

use rusqlite::{params, Connection};
use serde::Deserialize;

use pkg_core::{PkgError, PkgResult};
use pkg_db::list_installed_packages;
use pkg_repo::{unix_now, RecipeManifest};

/// The quarantine root for reproducibility mismatches (v14 §4:
/// double-build failures are retained here with a diff report).
pub const QUARANTINE_DIR: &str = "/var/lib/pkg/build/quarantine";

// ---------------------------------------------------------------------
// Findings
// ---------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum FindingSeverity {
    Warning,
    Hard,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum FindingKind {
    /// No provenance row (pre-v14 install) — warning.
    MissingProvenance,
    /// Attestation count below the requirement for the entry's
    /// criticality — hard for critical, warning otherwise.
    InsufficientAttestations,
    /// `reproducibility = 'mismatch'` recorded — hard.
    ReproMismatch,
    /// Critical package never double-built (`reproducibility` NULL) —
    /// warning (opt-outs exist; visibility, not punishment).
    ReproUnknown,
    /// A fetched origin isn't in the current manifest (T7) — hard for
    /// critical, warning otherwise.
    FetchOriginDrift,
    /// Quarantine dir has retained mismatch trees — hard.
    QuarantinePresent,
}

impl FindingKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            FindingKind::MissingProvenance => "missing-provenance",
            FindingKind::InsufficientAttestations => "insufficient-attestations",
            FindingKind::ReproMismatch => "repro-mismatch",
            FindingKind::ReproUnknown => "repro-unknown",
            FindingKind::FetchOriginDrift => "fetch-origin-drift",
            FindingKind::QuarantinePresent => "quarantine-present",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "missing-provenance" => FindingKind::MissingProvenance,
            "insufficient-attestations" => FindingKind::InsufficientAttestations,
            "repro-mismatch" => FindingKind::ReproMismatch,
            "repro-unknown" => FindingKind::ReproUnknown,
            "fetch-origin-drift" => FindingKind::FetchOriginDrift,
            "quarantine-present" => FindingKind::QuarantinePresent,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SupplyChainFinding {
    pub package: String,
    pub version: String,
    pub severity: FindingSeverity,
    pub kind: FindingKind,
    pub detail: String,
}

/// The audit result; `exit_code()` implements the 0/1/2 semantics.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SupplyChainReport {
    /// Packages examined (after `--critical` filtering).
    pub checked: usize,
    pub findings: Vec<SupplyChainFinding>,
}

impl SupplyChainReport {
    /// 0 clean, 1 warnings only, 2 any hard finding (v14 §4).
    pub fn exit_code(&self) -> i32 {
        let mut code = 0;
        for f in &self.findings {
            match f.severity {
                FindingSeverity::Hard => return 2,
                FindingSeverity::Warning => code = 1,
            }
        }
        code
    }
}

// ---------------------------------------------------------------------
// Provenance rows (queried directly; SPEC schema v4)
// ---------------------------------------------------------------------

/// One joined packages+provenance row. `Option` fields are the
/// provenance side (LEFT JOIN: absent rows are the MissingProvenance
/// warning case).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProvenanceRecord {
    pub name: String,
    pub version: String,
    pub recipe_commit: Option<String>,
    /// JSON `[{url, sha256}]` exactly as verified at fetch time.
    pub source_hashes: Option<String>,
    pub index_version: Option<i64>,
    /// JSON array of attestation fingerprints.
    pub attestations: Option<String>,
    pub reproducibility: Option<String>,
}

fn table_exists(conn: &Connection, name: &str) -> PkgResult<bool> {
    let n: i64 = conn
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE type='table' AND name=?1",
            params![name],
            |r| r.get(0),
        )
        .map_err(|e| PkgError::Db(e.to_string()))?;
    Ok(n > 0)
}

/// Read provenance for every installed package. When the table doesn't
/// exist (pre-v14 DB), returns one row per installed package with all
/// provenance fields `None` — the per-package warning path, not an
/// error.
pub fn provenance_rows(conn: &Connection) -> PkgResult<Vec<ProvenanceRecord>> {
    let installed = list_installed_packages(conn)?;
    if !table_exists(conn, "provenance")? {
        return Ok(installed
            .into_iter()
            .map(|p| ProvenanceRecord {
                name: p.name,
                version: p.version.to_string(),
                recipe_commit: None,
                source_hashes: None,
                index_version: None,
                attestations: None,
                reproducibility: None,
            })
            .collect());
    }
    // Keyed by package id so multiple installed identities of one name
    // (different abi_tag/opt_tag/arch) each get their own row.
    let mut stmt = conn
        .prepare(
            "SELECT package_id, recipe_commit, source_hashes, index_version,
                    attestations, reproducibility
             FROM provenance",
        )
        .map_err(|e| PkgError::Db(e.to_string()))?;
    let rows = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, i64>(3)?,
                r.get::<_, Option<String>>(4)?,
                r.get::<_, Option<String>>(5)?,
            ))
        })
        .map_err(|e| PkgError::Db(e.to_string()))?;
    let mut by_pkg: BTreeMap<i64, (String, String, i64, Option<String>, Option<String>)> =
        BTreeMap::new();
    for row in rows {
        let (pid, commit, hashes, idx, atts, repro) =
            row.map_err(|e| PkgError::Db(e.to_string()))?;
        by_pkg.insert(pid, (commit, hashes, idx, atts, repro));
    }
    Ok(installed
        .into_iter()
        .map(|p| {
            let prov = by_pkg.get(&p.id);
            ProvenanceRecord {
                name: p.name,
                version: p.version.to_string(),
                recipe_commit: prov.map(|r| r.0.clone()),
                source_hashes: prov.map(|r| r.1.clone()),
                index_version: prov.map(|r| r.2),
                attestations: prov.and_then(|r| r.3.clone()),
                reproducibility: prov.and_then(|r| r.4.clone()),
            }
        })
        .collect())
}

/// The `[{url, sha256}]` JSON recorded in `provenance.source_hashes`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct RecordedSource {
    pub url: String,
    pub sha256: String,
}

/// Distinct attestation fingerprints recorded for the package. A
/// malformed JSON blob is an empty set *plus* we return it so the
/// caller can flag it — but provenance is written by pkg-build from
/// verified data, so treat parse failure as "zero attestations" and let
/// the count rule flag it.
pub fn attestation_fingerprints(json: Option<&str>) -> Vec<String> {
    let Some(json) = json else { return Vec::new() };
    serde_json::from_str(json).unwrap_or_default()
}

// ---------------------------------------------------------------------
// The audit
// ---------------------------------------------------------------------

/// Run the supply-chain audit.
///
/// - `conn`: the pkg-db connection (caller holds the §9.4 read path).
/// - `manifest`: the current signed recipe manifest, if synced. Without
///   it the critical set and declared origins are unknown, so
///   manifest-dependent checks degrade to warnings (never hard) —
///   running the audit unsynced must not produce exit 2 it can't
///   justify, and `pkg audit`'s stale-index gate (v11 §10.6) is what
///   makes unsynced audits block under enforcement mode.
/// - `quarantine_dir`: checked for retained mismatch trees.
/// - `critical_only`: `--critical` — restrict reporting to the critical
///   set (packages marked critical in the manifest).
pub fn audit_supply_chain(
    conn: &Connection,
    manifest: Option<&RecipeManifest>,
    quarantine_dir: &Path,
    critical_only: bool,
) -> PkgResult<SupplyChainReport> {
    let rows = provenance_rows(conn)?;
    let mut report = SupplyChainReport::default();

    // Quarantine: any retained entry is a hard finding, reported once.
    match std::fs::read_dir(quarantine_dir) {
        Ok(mut rd) => {
            if rd.next().is_some() {
                report.findings.push(SupplyChainFinding {
                    package: "(system)".into(),
                    version: String::new(),
                    severity: FindingSeverity::Hard,
                    kind: FindingKind::QuarantinePresent,
                    detail: format!(
                        "reproducibility-mismatch trees retained under {}; inspect and \
                         clear with 'pkg clean --quarantine'",
                        quarantine_dir.display()
                    ),
                });
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(PkgError::Io(e)),
    }

    for row in &rows {
        let entry = manifest.and_then(|m| m.find(&row.name, &row.version));
        let critical = entry.map(|e| e.critical).unwrap_or(false);
        if critical_only && !critical {
            continue;
        }
        report.checked += 1;

        // --- provenance completeness -------------------------------
        let Some(recipe_commit) = &row.recipe_commit else {
            report.findings.push(SupplyChainFinding {
                package: row.name.clone(),
                version: row.version.clone(),
                severity: FindingSeverity::Warning,
                kind: FindingKind::MissingProvenance,
                detail: "no provenance row (pre-v14 install or unrecorded build); \
                         rebuilt packages gain full provenance"
                    .into(),
            });
            continue; // nothing else is checkable without a row
        };

        // --- attestations vs. requirement --------------------------
        let fps = attestation_fingerprints(row.attestations.as_deref());
        match entry {
            Some(e) if e.critical => {
                // The committer's fingerprint (when recorded) doesn't
                // count toward the required two (T3).
                let independent = fps
                    .iter()
                    .filter(|fp| Some(fp.as_str()) != e.committer_fingerprint.as_deref())
                    .count();
                if independent < 2 {
                    report.findings.push(SupplyChainFinding {
                        package: row.name.clone(),
                        version: row.version.clone(),
                        severity: FindingSeverity::Hard,
                        kind: FindingKind::InsufficientAttestations,
                        detail: format!(
                            "critical-set package built from recipe {recipe_commit} with \
                             {independent} independent reviewer attestation(s) (need 2; \
                             {} recorded total)",
                            fps.len()
                        ),
                    });
                }
            }
            _ => {
                if fps.is_empty() {
                    report.findings.push(SupplyChainFinding {
                        package: row.name.clone(),
                        version: row.version.clone(),
                        severity: FindingSeverity::Warning,
                        kind: FindingKind::InsufficientAttestations,
                        detail: "no attestation fingerprints recorded in provenance".into(),
                    });
                }
            }
        }

        // --- reproducibility ----------------------------------------
        match row.reproducibility.as_deref() {
            Some("mismatch") => report.findings.push(SupplyChainFinding {
                package: row.name.clone(),
                version: row.version.clone(),
                severity: FindingSeverity::Hard,
                kind: FindingKind::ReproMismatch,
                detail: "double-build outputs mismatched; trees retained in quarantine. \
                         Either non-determinism (bug — report upstream) or a build that \
                         responds to its environment."
                    .into(),
            }),
            None if critical => report.findings.push(SupplyChainFinding {
                package: row.name.clone(),
                version: row.version.clone(),
                severity: FindingSeverity::Warning,
                kind: FindingKind::ReproUnknown,
                detail: "critical-set package without a recorded double-build result \
                         (built with --no-repro-check or pre-v14)".into(),
            }),
            _ => {}
        }

        // --- fetch-origin drift (T7) ---------------------------------
        if let (Some(hashes_json), Some(e)) = (&row.source_hashes, entry) {
            let recorded: Vec<RecordedSource> =
                serde_json::from_str(hashes_json).unwrap_or_default();
            for rs in &recorded {
                match e.sources.iter().find(|s| s.url == rs.url) {
                    Some(decl) if decl.sha256 == rs.sha256 => {}
                    Some(_) => report.findings.push(SupplyChainFinding {
                        package: row.name.clone(),
                        version: row.version.clone(),
                        severity: FindingSeverity::Hard,
                        kind: FindingKind::FetchOriginDrift,
                        detail: format!(
                            "origin {} was fetched with hash {} but the manifest now pins \
                             a different hash — repinning event or tampering (T2/T7)",
                            rs.url, rs.sha256
                        ),
                    }),
                    None => report.findings.push(SupplyChainFinding {
                        package: row.name.clone(),
                        version: row.version.clone(),
                        severity: if critical {
                            FindingSeverity::Hard
                        } else {
                            FindingSeverity::Warning
                        },
                        kind: FindingKind::FetchOriginDrift,
                        detail: format!(
                            "fetched origin {} is not declared in the current recipe \
                             manifest (T7 drift)",
                            rs.url
                        ),
                    }),
                }
            }
        }
    }

    Ok(report)
}

// ---------------------------------------------------------------------
// Acknowledgements & the blocking surface
// ---------------------------------------------------------------------

const ACK_DDL: &str = "CREATE TABLE IF NOT EXISTS supply_chain_acknowledgements (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    package         TEXT NOT NULL,
    version         TEXT NOT NULL,
    finding_kind    TEXT NOT NULL,
    acknowledged_at INTEGER NOT NULL,
    reason          TEXT
)";

/// `pkg audit --supply-chain --acknowledge <package>`: acknowledge all
/// current hard findings for `package` (any installed version). Returns
/// the number of findings acknowledged. Refuses (NotFound) if the
/// package has no current hard findings — an ack that matches nothing
/// would be a silent no-op that *looks* like it unblocked something.
pub fn acknowledge(conn: &Connection, report: &SupplyChainReport, package: &str) -> PkgResult<usize> {
    conn.execute_batch(ACK_DDL)
        .map_err(|e| PkgError::Db(e.to_string()))?;
    let now = unix_now() as i64;
    let hard: Vec<&SupplyChainFinding> = report
        .findings
        .iter()
        .filter(|f| f.severity == FindingSeverity::Hard && f.package == package)
        .collect();
    if hard.is_empty() {
        return Err(PkgError::NotFound(format!(
            "no current hard supply-chain findings for '{package}'"
        )));
    }
    for f in &hard {
        conn.execute(
            "INSERT INTO supply_chain_acknowledgements
                 (package, version, finding_kind, acknowledged_at, reason)
             VALUES (?1, ?2, ?3, ?4, NULL)",
            params![f.package, f.version, f.kind.as_str(), now],
        )
        .map_err(|e| PkgError::Db(e.to_string()))?;
    }
    Ok(hard.len())
}

/// The §16.4-style blocking surface: hard findings with no matching
/// acknowledgement row. `pkg install` / `pkg upgrade` refuse while this
/// is non-empty.
pub fn unacknowledged_hard_findings(
    conn: &Connection,
    report: &SupplyChainReport,
) -> PkgResult<Vec<SupplyChainFinding>> {
    if !table_exists(conn, "supply_chain_acknowledgements")? {
        return Ok(report
            .findings
            .iter()
            .filter(|f| f.severity == FindingSeverity::Hard)
            .cloned()
            .collect());
    }
    let mut out = Vec::new();
    for f in report
        .findings
        .iter()
        .filter(|f| f.severity == FindingSeverity::Hard)
    {
        let n: i64 = conn
            .query_row(
                "SELECT count(*) FROM supply_chain_acknowledgements
                 WHERE package=?1 AND version=?2 AND finding_kind=?3",
                params![f.package, f.version, f.kind.as_str()],
                |r| r.get(0),
            )
            .map_err(|e| PkgError::Db(e.to_string()))?;
        if n == 0 {
            out.push(f.clone());
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pkg_repo::{RecipeManifestEntry, Source};

    fn setup_db(with_provenance: bool) -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE packages (
                id INTEGER PRIMARY KEY,
                name TEXT NOT NULL, version TEXT NOT NULL,
                abi_tag TEXT NOT NULL,
                opt_tag TEXT NOT NULL DEFAULT '',
                arch TEXT NOT NULL, track TEXT NOT NULL,
                installed_at INTEGER NOT NULL DEFAULT 0,
                installed_explicitly INTEGER NOT NULL DEFAULT 0
            );",
        )
        .unwrap();
        if with_provenance {
            conn.execute_batch(
                "CREATE TABLE provenance (
                    package_id INTEGER NOT NULL REFERENCES packages(id),
                    recipe_commit TEXT NOT NULL,
                    source_hashes TEXT NOT NULL,
                    index_version INTEGER NOT NULL,
                    attestations TEXT,
                    build_host TEXT NOT NULL,
                    builder_version TEXT NOT NULL,
                    started_at INTEGER NOT NULL,
                    finished_at INTEGER NOT NULL,
                    reproducibility TEXT
                );",
            )
            .unwrap();
        }
        conn
    }

    fn insert_pkg(conn: &Connection, id: i64, name: &str, version: &str) {
        conn.execute(
            "INSERT INTO packages (id, name, version, abi_tag, arch, track)
             VALUES (?1, ?2, ?3, 'ab1', 'amd64', 'source')",
            params![id, name, version],
        )
        .unwrap();
    }

    fn insert_prov(
        conn: &Connection,
        pkg_id: i64,
        attestations: Option<&str>,
        reproducibility: Option<&str>,
        source_hashes: &str,
    ) {
        conn.execute(
            "INSERT INTO provenance
                 (package_id, recipe_commit, source_hashes, index_version,
                  attestations, build_host, builder_version, started_at, finished_at,
                  reproducibility)
             VALUES (?1, 'c0ffee', ?2, 7, ?3, 'host', 'pkg 0.1', 1, 2, ?4)",
            params![pkg_id, source_hashes, attestations, reproducibility],
        )
        .unwrap();
    }

    fn manifest_with(critical: bool) -> RecipeManifest {
        RecipeManifest {
            packages: vec![RecipeManifestEntry {
                name: "xz".into(),
                version: "5.6.1".into(),
                recipe_commit: "c0ffee".into(),
                committer_fingerprint: Some("committer-fp".into()),
                sources: vec![Source {
                    url: "https://upstream.example/xz.tar.xz".into(),
                    sha256: "aa".into(),
                }],
                attestations: vec![],
                critical,
            }],
        }
    }

    fn no_quarantine() -> std::path::PathBuf {
        std::env::temp_dir().join("pkg-audit-test-no-such-quarantine")
    }

    #[test]
    fn exit_code_semantics() {
        let mut r = SupplyChainReport::default();
        assert_eq!(r.exit_code(), 0);
        r.findings.push(SupplyChainFinding {
            package: "a".into(), version: "1".into(),
            severity: FindingSeverity::Warning,
            kind: FindingKind::MissingProvenance, detail: String::new(),
        });
        assert_eq!(r.exit_code(), 1);
        r.findings.push(SupplyChainFinding {
            package: "b".into(), version: "1".into(),
            severity: FindingSeverity::Hard,
            kind: FindingKind::ReproMismatch, detail: String::new(),
        });
        assert_eq!(r.exit_code(), 2);
    }

    #[test]
    fn missing_provenance_table_warns_per_package() {
        let conn = setup_db(false);
        insert_pkg(&conn, 1, "xz", "5.6.1");
        let m = manifest_with(true);
        let report = audit_supply_chain(&conn, Some(&m), &no_quarantine(), false).unwrap();
        assert_eq!(report.exit_code(), 1); // warning, never hard, on pre-v14 DBs
        assert!(report
            .findings
            .iter()
            .all(|f| f.kind == FindingKind::MissingProvenance
                && f.severity == FindingSeverity::Warning));
    }

    #[test]
    fn critical_with_under_two_independent_attestations_is_hard() {
        let conn = setup_db(true);
        insert_pkg(&conn, 1, "xz", "5.6.1");
        insert_prov(
            &conn,
            1,
            Some(r#"["committer-fp","reviewer-1"]"#),
            Some("self-match"),
            r#"[{"url":"https://upstream.example/xz.tar.xz","sha256":"aa"}]"#,
        );
        let m = manifest_with(true);
        let report = audit_supply_chain(&conn, Some(&m), &no_quarantine(), false).unwrap();
        assert_eq!(report.exit_code(), 2);
        assert!(report
            .findings
            .iter()
            .any(|f| f.kind == FindingKind::InsufficientAttestations
                && f.severity == FindingSeverity::Hard));

        // Two independent reviewers: clean.
        conn.execute("DELETE FROM provenance", []).unwrap();
        insert_prov(
            &conn,
            1,
            Some(r#"["committer-fp","reviewer-1","reviewer-2"]"#),
            Some("self-match"),
            r#"[{"url":"https://upstream.example/xz.tar.xz","sha256":"aa"}]"#,
        );
        let report = audit_supply_chain(&conn, Some(&m), &no_quarantine(), false).unwrap();
        assert_eq!(report.exit_code(), 0, "{:?}", report.findings);
    }

    #[test]
    fn repro_mismatch_is_hard_regardless_of_criticality() {
        let conn = setup_db(true);
        insert_pkg(&conn, 1, "xz", "5.6.1");
        insert_prov(&conn, 1, Some(r#"["r1"]"#), Some("mismatch"), "[]");
        let m = manifest_with(false);
        let report = audit_supply_chain(&conn, Some(&m), &no_quarantine(), false).unwrap();
        assert_eq!(report.exit_code(), 2);
    }

    #[test]
    fn critical_without_repro_check_is_a_warning() {
        let conn = setup_db(true);
        insert_pkg(&conn, 1, "xz", "5.6.1");
        insert_prov(
            &conn,
            1,
            Some(r#"["r1","r2"]"#),
            None,
            r#"[{"url":"https://upstream.example/xz.tar.xz","sha256":"aa"}]"#,
        );
        let m = manifest_with(true);
        let report = audit_supply_chain(&conn, Some(&m), &no_quarantine(), false).unwrap();
        assert_eq!(report.exit_code(), 1);
        assert!(report.findings.iter().any(|f| f.kind == FindingKind::ReproUnknown));
    }

    #[test]
    fn fetch_origin_drift_detected() {
        let conn = setup_db(true);
        insert_pkg(&conn, 1, "xz", "5.6.1");
        // Built from an origin the manifest no longer declares.
        insert_prov(
            &conn,
            1,
            Some(r#"["r1","r2"]"#),
            Some("self-match"),
            r#"[{"url":"https://attacker.example/xz.tar.xz","sha256":"aa"}]"#,
        );
        let m = manifest_with(true);
        let report = audit_supply_chain(&conn, Some(&m), &no_quarantine(), false).unwrap();
        assert!(report.findings.iter().any(|f| f.kind == FindingKind::FetchOriginDrift));
        // Same URL but the manifest now pins a different hash: repin.
        conn.execute("DELETE FROM provenance", []).unwrap();
        insert_prov(
            &conn,
            1,
            Some(r#"["r1","r2"]"#),
            Some("self-match"),
            r#"[{"url":"https://upstream.example/xz.tar.xz","sha256":"bb"}]"#,
        );
        let report = audit_supply_chain(&conn, Some(&m), &no_quarantine(), false).unwrap();
        assert!(report
            .findings
            .iter()
            .any(|f| f.kind == FindingKind::FetchOriginDrift
                && f.severity == FindingSeverity::Hard));
    }

    #[test]
    fn acknowledge_unblocks_exactly_the_acked_finding() {
        let conn = setup_db(true);
        insert_pkg(&conn, 1, "xz", "5.6.1");
        insert_prov(&conn, 1, Some(r#"["r1"]"#), Some("mismatch"), "[]");
        let m = manifest_with(false);
        let report = audit_supply_chain(&conn, Some(&m), &no_quarantine(), false).unwrap();

        // Two hard findings on xz (insufficient attestations is only a
        // warning for non-critical; repro-mismatch is hard).
        let blocked = unacknowledged_hard_findings(&conn, &report).unwrap();
        assert_eq!(blocked.len(), 1);
        assert_eq!(blocked[0].kind, FindingKind::ReproMismatch);

        // Acking a package with no hard findings refuses.
        assert!(acknowledge(&conn, &report, "nonexistent").is_err());

        // Ack xz: blocked set empties.
        let n = acknowledge(&conn, &report, "xz").unwrap();
        assert_eq!(n, 1);
        assert!(unacknowledged_hard_findings(&conn, &report).unwrap().is_empty());

        // A NEW version's same finding is not covered by the old ack.
        conn.execute("DELETE FROM packages", []).unwrap();
        conn.execute("DELETE FROM provenance", []).unwrap();
        insert_pkg(&conn, 2, "xz", "5.6.2");
        insert_prov(&conn, 2, Some(r#"["r1"]"#), Some("mismatch"), "[]");
        let report2 = audit_supply_chain(&conn, Some(&m), &no_quarantine(), false).unwrap();
        // (manifest entry is for 5.6.1, so manifest-based checks don't
        // fire; repro-mismatch is manifest-independent and does.)
        let blocked = unacknowledged_hard_findings(&conn, &report2).unwrap();
        assert_eq!(blocked.len(), 1);
        assert_eq!(blocked[0].version, "5.6.2");
    }

    #[test]
    fn critical_only_filter() {
        let conn = setup_db(true);
        insert_pkg(&conn, 1, "xz", "5.6.1");
        insert_pkg(&conn, 2, "hello", "2.12");
        insert_prov(&conn, 1, Some(r#"["r1","r2"]"#), Some("self-match"), "[]");
        insert_prov(&conn, 2, Some(r#"[]"#), None, "[]");
        let m = manifest_with(true);
        let report = audit_supply_chain(&conn, Some(&m), &no_quarantine(), true).unwrap();
        assert_eq!(report.checked, 1);
        assert!(report.findings.iter().all(|f| f.package == "xz"));
    }
}
