//! ## v1 (initial)
//!
//! ## v2
//!
//! - [FIX] Close the "recipe manifest acquisition" gap: a mirror that
//!   serves a verifying `index.json` but no `recipe-manifest.toml` is
//!   now a *failed* mirror (fall through to the next one), because
//!   builds are impossible without the manifest (v12 §2). The manifest
//!   is cached atomically next to `index.json` via the same
//!   tmp+rename pattern; `pkg-build` reads it back through
//!   `recipe_manifest::load_cached_recipe_manifest`.
//!
//! Repository sync (v11 §10.6): fetch the signed index bundle from the
//! configured mirrors with fallback, verify it through `trust.rs`
//! **before** accepting anything (mirrors are transport-only — the
//! signature chain is the trust, so any mirror can serve the artifacts
//! and no mirror is individually trusted), maintain a TTL cache, and
//! report staleness honestly.
//!
//! Cache layout under `cache_dir` (conventionally `/var/lib/pkg/cache/repo/`):
//!
//! - `index.json` — the raw, verified index bundle bytes (kept raw so
//!   every read re-verifies the signatures; a tampered cache file fails
//!   verification rather than being trusted because it's on disk).
//! - `recipe-manifest.toml` — the recipe manifest (v12 §2), fetched
//!   from the same mirror that served the accepted index and cached
//!   atomically; mandatory — a mirror without it is a failed mirror.
//! - `security.json` — the repository-signed advisory document, cached
//!   raw; `pkg-audit` parses it from here.
//! - `meta.json` — `{ fetched_at, index_version }` for TTL decisions.
//!
//! Failure policy (v11 §10.6): ordinary commands operate on a
//! stale-but-verified cache *intentionally* — "explicit over implicit".
//! So when every mirror fails but a cache exists, `sync_index` returns
//! the verified cached index with `Index::stale_cache = true` and a
//! warning attached, rather than erroring. Only a total miss (no cache,
//! no reachable mirror) is an error.
//!
//! `pkg sync` is a **writer** for lock purposes (v12 §3) — the CLI takes
//! the §9.4 lock around `sync_index`.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use pkg_core::{PkgError, PkgResult};

use crate::trust::{unix_now, IndexPackage, TrustStore, TrustWarning, VerifiedIndex};

/// Upper bound on an accepted index bundle (16 MiB). The index is
/// metadata; a multi-hundred-MiB "index" is a DoS, not a repository.
const MAX_INDEX_BYTES: u64 = 16 * 1024 * 1024;

/// Same bound for `security.json`.
const MAX_SECURITY_BYTES: u64 = 16 * 1024 * 1024;

/// Same bound for `recipe-manifest.toml` — metadata, like the index.
const MAX_MANIFEST_BYTES: u64 = 16 * 1024 * 1024;

/// The verified index plus the staleness picture the CLI needs for
/// `pkg sync --status` and the "repository index is N days old" warning
/// (v11 §10.6).
#[derive(Debug, Clone)]
pub struct Index {
    pub index_version: u64,
    pub generated_at: u64,
    pub packages: Vec<IndexPackage>,
    /// Trust warnings from verification (timestamp grace, root expiry).
    pub trust_warnings: Vec<TrustWarning>,
    /// True when this index came from the on-disk cache past its TTL
    /// (either deliberately — fresh-enough policy — or because every
    /// mirror failed).
    pub stale_cache: bool,
    /// Age of the cache entry this index was loaded from, in seconds.
    /// Zero when freshly fetched.
    pub cache_age_secs: u64,
    /// Non-fatal notes (e.g. mirrors that failed before one succeeded).
    pub notes: Vec<String>,
}

/// `pkg sync --status` output: cache age, last sync, freshness.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncStatus {
    /// When the cache was last written (unix seconds); `None` = never synced.
    pub fetched_at: Option<u64>,
    /// Age of the cache in seconds; `None` = never synced.
    pub age_secs: Option<u64>,
    /// `age < ttl`.
    pub fresh: bool,
    /// The cached index's version counter, if a cache exists.
    pub index_version: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CacheMeta {
    fetched_at: u64,
    index_version: u64,
}

/// Pure TTL decision, split out for testing: a cache entry written at
/// `fetched_at` is fresh at `now` iff `now - fetched_at < ttl`
/// (saturating; a clock that moved backwards counts as fresh, which is
/// the safe direction for staleness *avoidance* — the timestamp role,
/// not the local clock, is the freeze-attack defense).
pub fn cache_is_fresh(fetched_at: u64, now: u64, ttl: Duration) -> bool {
    now.saturating_sub(fetched_at) < ttl.as_secs()
}

/// Join a mirror base URL and a document name. Mirrors are configured
/// as base URLs in `/etc/pkg/mirrors.toml`.
fn mirror_url(mirror: &str, doc: &str) -> String {
    format!("{}/{}", mirror.trim_end_matches('/'), doc)
}

fn read_cache(cache_dir: &Path, name: &str) -> PkgResult<Option<Vec<u8>>> {
    match std::fs::read(cache_dir.join(name)) {
        Ok(b) => Ok(Some(b)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(PkgError::Io(e)),
    }
}

/// Atomic cache write: tmp file + rename, so a crashed sync never
/// leaves a truncated `index.json` that the next run would try to
/// verify. (Verification would catch truncation anyway; the rename
/// keeps the *last good* cache intact instead.)
fn write_cache(cache_dir: &Path, name: &str, bytes: &[u8]) -> PkgResult<()> {
    std::fs::create_dir_all(cache_dir)?;
    let tmp = cache_dir.join(format!("{name}.tmp"));
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, cache_dir.join(name))?;
    Ok(())
}

fn read_meta(cache_dir: &Path) -> PkgResult<Option<CacheMeta>> {
    match read_cache(cache_dir, "meta.json")? {
        Some(bytes) => {
            let meta = serde_json::from_slice(&bytes)
                .map_err(|e| PkgError::Parse(format!("sync cache meta.json: {e}")))?;
            Ok(Some(meta))
        }
        None => Ok(None),
    }
}

/// The index version of the currently cached (verified) index, for
/// provenance recording (§9.2.1: `index_version` is the signed index
/// the recipe pin came from). `None` when never synced.
pub fn cached_index_version(cache_dir: &Path) -> PkgResult<Option<u64>> {
    Ok(read_meta(cache_dir)?.map(|m| m.index_version))
}

/// HTTP GET with a size cap, returning the body. Any failure (DNS,
/// TLS, HTTP status, read) is an error string so mirror fallback can
/// record it and move on — no failure of one mirror is fatal.
fn http_get(url: &str, cap: u64) -> Result<Vec<u8>, String> {
    let resp = ureq::get(url).call().map_err(|e| e.to_string())?;
    let mut reader = resp.into_reader().take(cap + 1);
    let mut buf = Vec::new();
    use std::io::Read;
    reader
        .read_to_end(&mut buf)
        .map_err(|e| format!("read error: {e}"))?;
    if buf.len() as u64 > cap {
        return Err(format!("response exceeds {cap} byte limit"));
    }
    Ok(buf)
}

/// Fetch, verify, and cache the repository index (v11 §10.6).
///
/// - If the cache is fresh (`age < ttl`), return it without any network
///   I/O — but still *verified*; the cache being local doesn't make it
///   trusted.
/// - Otherwise try mirrors in listed order; the first mirror whose
///   bundle verifies through `trust` wins. The accepted bundle replaces
///   the cache atomically and its `index_version` becomes the new
///   rollback floor ([`TrustStore::record_index_version`], called only
///   after the cache write succeeds).
/// - If every mirror fails, fall back to the verified cache (stale
///   flag set) if one exists; error only on total miss.
///
/// `trust` is `&mut` because accepting a new index advances the
/// persisted rollback counter.
pub fn sync_index(
    trust: &mut TrustStore,
    mirrors: &[String],
    cache_dir: &Path,
    ttl: Duration,
) -> PkgResult<Index> {
    let now = unix_now();

    // Fresh cache: no network at all (v11 §10.6 — ordinary operation
    // never triggers an implicit sync).
    if let Some(meta) = read_meta(cache_dir)? {
        if cache_is_fresh(meta.fetched_at, now, ttl) {
            if let Some(bytes) = read_cache(cache_dir, "index.json")? {
                // A fresh index WITHOUT a cached recipe manifest (e.g.
                // the cache predates manifest acquisition) is not a
                // usable cache — refetch rather than letting every
                // build fail on the missing manifest.
                if read_cache(cache_dir, "recipe-manifest.toml")?.is_some() {
                    let vi = trust.verify_index(&bytes)?;
                    return Ok(index_from_verified(vi, false, now - meta.fetched_at, Vec::new()));
                }
            }
        }
    }

    // Mirror fallback.
    let mut notes = Vec::new();
    for mirror in mirrors {
        let url = mirror_url(mirror, "index.json");
        match http_get(&url, MAX_INDEX_BYTES) {
            Ok(bytes) => {
                // Verify BEFORE writing anything to the cache: a bad
                // bundle leaves the last good cache untouched.
                let vi = trust.verify_index(&bytes)?;
                // recipe-manifest.toml (v12 §2) is NOT optional: no
                // source may be fetched except through it (T7), so a
                // mirror that doesn't serve it cannot support builds at
                // all. Treat its absence as a hard failure FOR THIS
                // MIRROR and fall through to the next one.
                let manifest = match http_get(
                    &mirror_url(mirror, "recipe-manifest.toml"),
                    MAX_MANIFEST_BYTES,
                ) {
                    Ok(m) => m,
                    Err(e) => {
                        notes.push(format!(
                            "mirror {mirror}: recipe-manifest.toml unavailable: {e}"
                        ));
                        continue;
                    }
                };
                write_cache(cache_dir, "index.json", &bytes)?;
                write_cache(cache_dir, "recipe-manifest.toml", &manifest)?;
                // security.json is part of the same signed bundle
                // conceptually (v11 §10.6) but a separate document;
                // fetch best-effort and verify before caching.
                match http_get(&mirror_url(mirror, "security.json"), MAX_SECURITY_BYTES) {
                    Ok(sec) => {
                        if let Err(e) = trust
                            .verify_signed::<serde_json::Value>(&sec, crate::trust::Role::Repository)
                            .and_then(|_| write_cache(cache_dir, "security.json", &sec))
                        {
                            notes.push(format!("security.json from {mirror} rejected: {e}"));
                        }
                    }
                    Err(e) => notes.push(format!("security.json unavailable from {mirror}: {e}")),
                }
                write_cache(
                    cache_dir,
                    "meta.json",
                    &serde_json::to_vec(&CacheMeta {
                        fetched_at: now,
                        index_version: vi.index_version,
                    })
                    .map_err(|e| PkgError::Parse(format!("meta serialize: {e}")))?,
                )?;
                // Advance the rollback floor only now that the index is
                // durably cached.
                trust.record_index_version(vi.index_version)?;
                return Ok(index_from_verified(vi, false, 0, notes));
            }
            Err(e) => notes.push(format!("mirror {mirror}: {e}")),
        }
    }

    // All mirrors failed: stale-cache fallback (v11 §10.6's intentional
    // stale-cache operation). Still verified through the trust chain.
    if let Some(bytes) = read_cache(cache_dir, "index.json")? {
        let vi = trust.verify_index(&bytes)?;
        let age = read_meta(cache_dir)?.map(|m| now.saturating_sub(m.fetched_at)).unwrap_or(0);
        notes.push("all mirrors failed; using stale cache".to_string());
        return Ok(index_from_verified(vi, true, age, notes));
    }

    Err(PkgError::Io(std::io::Error::new(
        std::io::ErrorKind::NotFound,
        format!("no reachable mirror and no cached index: {}", notes.join("; ")),
    )))
}

/// `pkg sync --status` (v11 §10.6): cache age, last sync, freshness —
/// without touching the network.
pub fn sync_status(cache_dir: &Path, ttl: Duration) -> PkgResult<SyncStatus> {
    let now = unix_now();
    match read_meta(cache_dir)? {
        Some(meta) => Ok(SyncStatus {
            fetched_at: Some(meta.fetched_at),
            age_secs: Some(now.saturating_sub(meta.fetched_at)),
            fresh: cache_is_fresh(meta.fetched_at, now, ttl),
            index_version: Some(meta.index_version),
        }),
        None => Ok(SyncStatus {
            fetched_at: None,
            age_secs: None,
            fresh: false,
            index_version: None,
        }),
    }
}

/// The default cache location for the synced repository documents.
pub fn default_cache_dir() -> PathBuf {
    pkg_core::paths::at("/var/lib/pkg/cache/repo")
}

fn index_from_verified(
    vi: VerifiedIndex,
    stale_cache: bool,
    cache_age_secs: u64,
    notes: Vec<String>,
) -> Index {
    Index {
        index_version: vi.index_version,
        generated_at: vi.generated_at,
        packages: vi.packages,
        trust_warnings: vi.warnings,
        stale_cache,
        cache_age_secs,
        notes,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ttl_decision() {
        let ttl = Duration::from_secs(24 * 3600);
        assert!(cache_is_fresh(1000, 2000, ttl));
        assert!(!cache_is_fresh(1000, 1000 + 24 * 3600, ttl)); // age == ttl is stale
        assert!(cache_is_fresh(2000, 1000, ttl)); // clock skew backwards: treat as fresh
    }

    #[test]
    fn mirror_url_joining() {
        assert_eq!(mirror_url("https://m1.example/repo", "index.json"),
                   "https://m1.example/repo/index.json");
        assert_eq!(mirror_url("https://m1.example/repo/", "index.json"),
                   "https://m1.example/repo/index.json");
    }

    #[test]
    fn status_with_no_cache_is_unfresh_not_error() {
        let dir = std::env::temp_dir().join("pkg-repo-sync-test-nonexistent-dir");
        let _ = std::fs::remove_dir_all(&dir);
        let st = sync_status(&dir, Duration::from_secs(3600)).unwrap();
        assert!(!st.fresh);
        assert_eq!(st.fetched_at, None);
    }

    #[test]
    fn cache_roundtrip_and_meta() {
        let dir = std::env::temp_dir().join(format!(
            "pkg-repo-sync-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        write_cache(&dir, "index.json", b"{}").unwrap();
        assert_eq!(read_cache(&dir, "index.json").unwrap().as_deref(), Some(&b"{}"[..]));
        // meta roundtrip
        let meta = CacheMeta { fetched_at: 42, index_version: 7 };
        write_cache(&dir, "meta.json", &serde_json::to_vec(&meta).unwrap()).unwrap();
        let back = read_meta(&dir).unwrap().unwrap();
        assert_eq!(back.fetched_at, 42);
        assert_eq!(back.index_version, 7);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
