//! ## v1 (initial)
//!
//! Security-advisory matching for `pkg audit` (v11 §10.4, §16.1):
//! parse `security.json` from the pkg-repo sync cache and match it
//! against the installed package set (queried from pkg-db).
//!
//! # Trust boundary
//!
//! This module does **not** verify signatures. `security.json` arrives
//! signed by the repository role and is verified by
//! `pkg_repo::TrustStore::verify_signed` before `sync.rs` writes it to
//! the cache (v11 §10.6). Anything read from the cache here is
//! re-verifiable upstream; parsing is still written to reject malformed
//! input rather than assume it.
//!
//! # `security.json` schema (v1)
//!
//! ```json
//! {
//!   "format_version": 1,
//!   "advisories": [
//!     {
//!       "id": "PKG-2026-0001",
//!       "class": "cve" | "supply-chain" | "reproducibility",
//!       "severity": "low" | "medium" | "high" | "critical",
//!       "package": "openssl",
//!       "affected": ">=3.0,<3.0.8",
//!       "fixed": "3.0.8",
//!       "summary": "...",
//!       "references": ["https://..."]
//!     }
//!   ]
//! }
//! ```
//!
//! `affected` is a comma-separated comparator list (all must hold) over
//! the package's `Version`: `<`, `<=`, `>`, `>=`, `=` followed by a
//! version. An empty/`"*"` affected matches every installed version.
//!
//! The `supply-chain` class (v14 §4 emergency flow) is what enforcement
//! mode treats as blocking; `reproducibility` advisories ride the same
//! channel per v11 §10.8's "an unexplained build mismatch is its own
//! signal".

use std::path::Path;

use serde::Deserialize;

use pkg_core::{PkgError, PkgResult, Version};
use pkg_db::list_installed_packages;
use rusqlite::Connection;

/// Advisory classes. `SupplyChain` (v14 §4): revoked recipe commits /
/// tarball pins — under enforcement mode these block installs like a
/// stale index does. `Reproducibility` (v11 §10.8): Tier-2 mismatch
/// notices. Unknown future classes parse as `Other` so a newer
/// repository never breaks older clients' audits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdvisoryClass {
    Cve,
    SupplyChain,
    Reproducibility,
    Other(String),
}

impl<'de> Deserialize<'de> for AdvisoryClass {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        Ok(match s.as_str() {
            "cve" => AdvisoryClass::Cve,
            "supply-chain" => AdvisoryClass::SupplyChain,
            "reproducibility" => AdvisoryClass::Reproducibility,
            other => AdvisoryClass::Other(other.to_string()),
        })
    }
}

impl AdvisoryClass {
    pub fn as_str(&self) -> &str {
        match self {
            AdvisoryClass::Cve => "cve",
            AdvisoryClass::SupplyChain => "supply-chain",
            AdvisoryClass::Reproducibility => "reproducibility",
            AdvisoryClass::Other(s) => s.as_str(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Low,
    Medium,
    High,
    Critical,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Advisory {
    pub id: String,
    #[serde(default = "default_class")]
    pub class: AdvisoryClass,
    pub severity: Severity,
    pub package: String,
    /// Comparator list; empty string or `"*"` matches all versions.
    #[serde(default)]
    pub affected: String,
    #[serde(default)]
    pub fixed: Option<String>,
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub references: Vec<String>,
}

fn default_class() -> AdvisoryClass {
    AdvisoryClass::Cve
}

/// The parsed `security.json` document.
#[derive(Debug, Clone, Deserialize)]
pub struct AdvisoryIndex {
    #[serde(default)]
    pub format_version: u32,
    #[serde(default)]
    pub advisories: Vec<Advisory>,
}

/// Load and parse `security.json` from the sync cache. A missing file
/// is an error (not "no advisories") — silently reporting a clean audit
/// against an absent advisory index is exactly the failure v11 §10.6's
/// freshness gate exists to prevent; the caller decides whether to
/// block (enforcement mode) or warn.
pub fn load_advisories(cache_dir: &Path) -> PkgResult<AdvisoryIndex> {
    let path = cache_dir.join("security.json");
    let bytes = std::fs::read(&path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            PkgError::NotFound(format!(
                "{}: no advisory index cached — run 'pkg sync'",
                path.display()
            ))
        } else {
            PkgError::Io(e)
        }
    })?;
    // The cached file is the signed envelope; verification happened at
    // sync time. Here we peel the envelope (if still wrapped) and parse
    // the payload.
    parse_advisories(&bytes)
}

/// Parse advisory bytes, accepting either the bare payload or the full
/// signed envelope (`{ "signed": {...}, "signatures": [...] }`) so the
/// raw mirror document can be inspected too. Signature *verification*
/// is pkg-repo's job, not this module's.
pub fn parse_advisories(bytes: &[u8]) -> PkgResult<AdvisoryIndex> {
    let value: serde_json::Value = serde_json::from_slice(bytes)
        .map_err(|e| PkgError::Parse(format!("security.json: {e}")))?;
    let payload = if value.get("signed").is_some() && value.get("signatures").is_some() {
        value["signed"].clone()
    } else {
        value
    };
    serde_json::from_value(payload).map_err(|e| PkgError::Parse(format!("security.json payload: {e}")))
}

/// Version-constraint matching, pure. `affected` is a comma-separated
/// list of comparators; all must hold. Unknown operators are a parse
/// error, never a silent match/nomatch.
pub fn version_affected(constraint: &str, version: &Version) -> PkgResult<bool> {
    let c = constraint.trim();
    if c.is_empty() || c == "*" {
        return Ok(true);
    }
    for part in c.split(',') {
        let part = part.trim();
        let (op, vstr) = if let Some(rest) = part.strip_prefix("<=") {
            (Cmp::Le, rest)
        } else if let Some(rest) = part.strip_prefix(">=") {
            (Cmp::Ge, rest)
        } else if let Some(rest) = part.strip_prefix('<') {
            (Cmp::Lt, rest)
        } else if let Some(rest) = part.strip_prefix('>') {
            (Cmp::Gt, rest)
        } else if let Some(rest) = part.strip_prefix('=') {
            (Cmp::Eq, rest)
        } else {
            return Err(PkgError::Parse(format!(
                "advisory constraint '{part}': expected one of <, <=, >, >=, ="
            )));
        };
        let bound: Version = vstr.trim().parse().map_err(|e| {
            PkgError::Parse(format!("advisory constraint '{part}': bad version: {e}"))
        })?;
        let ord = version.cmp(&bound);
        let hit = match op {
            Cmp::Lt => ord.is_lt(),
            Cmp::Le => ord.is_le(),
            Cmp::Gt => ord.is_gt(),
            Cmp::Ge => ord.is_ge(),
            Cmp::Eq => ord.is_eq(),
        };
        if !hit {
            return Ok(false);
        }
    }
    Ok(true)
}

enum Cmp {
    Lt,
    Le,
    Gt,
    Ge,
    Eq,
}

/// One installed package matching one advisory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdvisoryMatch {
    pub advisory_id: String,
    pub class: String,
    pub severity: Severity,
    pub package: String,
    pub installed_version: String,
    pub fixed_version: Option<String>,
    pub summary: String,
}

/// Match the advisory index against the installed set (pkg-db's
/// `list_installed_packages`). Malformed `affected` constraints are
/// reported as errors, not skipped: a broken advisory that silently
/// matched nothing would hide a vulnerability.
pub fn match_advisories(
    conn: &Connection,
    index: &AdvisoryIndex,
) -> PkgResult<Vec<AdvisoryMatch>> {
    let installed = list_installed_packages(conn)?;
    let mut out = Vec::new();
    for pkg in &installed {
        for adv in &index.advisories {
            if adv.package != pkg.name {
                continue;
            }
            if version_affected(&adv.affected, &pkg.version)? {
                out.push(AdvisoryMatch {
                    advisory_id: adv.id.clone(),
                    class: adv.class.as_str().to_string(),
                    severity: adv.severity,
                    package: pkg.name.clone(),
                    installed_version: pkg.version.to_string(),
                    fixed_version: adv.fixed.clone(),
                    summary: adv.summary.clone(),
                });
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> Version {
        s.parse().unwrap()
    }

    #[test]
    fn constraint_matching() {
        assert!(version_affected("", &v("1.0")).unwrap());
        assert!(version_affected("*", &v("1.0")).unwrap());
        assert!(version_affected("<3.0.8", &v("3.0.7")).unwrap());
        assert!(!version_affected("<3.0.8", &v("3.0.8")).unwrap());
        assert!(version_affected(">=3.0,<3.0.8", &v("3.0.1")).unwrap());
        assert!(!version_affected(">=3.0,<3.0.8", &v("2.9")).unwrap());
        assert!(version_affected("=2.39", &v("2.39.0")).unwrap()); // zero-padded equality
        assert!(version_affected(">=2.39-r2", &v("2.39-r3")).unwrap());
        assert!(version_affected("< 6.1 , >= 6.0", &v("6.0.5")).unwrap());
        assert!(version_affected("~6.1", &v("6.1")).is_err());
        assert!(version_affected("<abc", &v("1.0")).is_err());
    }

    #[test]
    fn parses_envelope_and_bare_forms() {
        let bare = br#"{
            "format_version": 1,
            "advisories": [{
                "id": "PKG-2026-0001",
                "class": "supply-chain",
                "severity": "critical",
                "package": "xz",
                "affected": "=5.6.1",
                "fixed": "5.6.2",
                "summary": "malicious tarball pin revoked"
            }]
        }"#;
        let idx = parse_advisories(bare).unwrap();
        assert_eq!(idx.advisories.len(), 1);
        assert_eq!(idx.advisories[0].class, AdvisoryClass::SupplyChain);
        assert_eq!(idx.advisories[0].severity, Severity::Critical);

        let wrapped = br#"{
            "signed": { "format_version": 1, "advisories": [] },
            "signatures": [{ "key_fingerprint": "ab", "sig": "cd" }]
        }"#;
        let idx = parse_advisories(wrapped).unwrap();
        assert!(idx.advisories.is_empty());

        // Default class is cve; unknown classes round-trip as Other.
        let unknown = br#"{"advisories": [{
            "id": "X", "severity": "low", "package": "p", "class": "moon-phase"
        }]}"#;
        let idx = parse_advisories(unknown).unwrap();
        assert_eq!(idx.advisories[0].class.as_str(), "moon-phase");

        let nocls = br#"{"advisories": [{ "id": "X", "severity": "low", "package": "p" }]}"#;
        assert_eq!(parse_advisories(nocls).unwrap().advisories[0].class, AdvisoryClass::Cve);
    }
}
