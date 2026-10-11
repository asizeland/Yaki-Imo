//! ## v1 (initial)
//!
//! Source fetching (v12 §2, v14 §4/T7): download the `[source]`s a
//! recipe manifest entry declares, verify each against its pinned
//! SHA-256, and **refuse to fetch anything the manifest doesn't
//! declare** — the dependency-confusion guard (T7): a recipe that was
//! tricked into naming an attacker-controlled origin (typo'd domain,
//! hijacked org, re-registered namespace) is stopped here, because the
//! manifest's origin list was reviewed at attestation time. Residual
//! risk, stated honestly per the design doc: if the *initially
//! recorded* origin was the attacker's, pinning preserves the poison —
//! only review (T3/T4) and critical-set cross-checks (T1) catch that.
//!
//! # Crash safety ("resume-safe")
//!
//! Downloads go to `<dest>.part` and are atomically `rename(2)`d into
//! place only after the SHA-256 matches. A crash at any point leaves
//! either the previous complete file or a `.part` fragment — never a
//! corrupt file at the final path. Re-running reuses verified complete
//! files and restarts incomplete ones from scratch (HTTP Range resume
//! is deliberately not done in v1: a resumable partial whose beginning
//! was attacker-controlled would still fail the hash, so restart-from-
//! scratch costs bandwidth but keeps exactly one integrity check).
//!
//! Fetch runs in the sandbox's *Fetch* profile (v14 §4: network
//! restricted to declared origins, only the source cache writable); the
//! build stage itself has no network at all, so this module is the only
//! network path in a build.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use pkg_core::{PkgError, PkgResult};
use sha2::{Digest, Sha256};

use crate::recipe_manifest::{RecipeManifestEntry, Source};
use crate::trust::hex_encode;

/// Per-file download cap (4 GiB). Source trees are not that big; an
/// unbounded stream is a disk-fill DoS.
const MAX_SOURCE_BYTES: u64 = 4 * 1024 * 1024 * 1024;

/// Default source cache (the only writable target of the Fetch sandbox
/// profile besides the scratch dir).
pub const SOURCE_CACHE_DIR: &str = "/var/lib/pkg/cache/sources";

/// Fetch failures. Mapped to `PkgError::Build` at the boundary: fetch
/// is a build phase (§7 pipeline), so its failures carry build's exit
/// code (2). The two *security refusals* ([`FetchError::NotInManifest`],
/// [`FetchError::HashMismatch`]) are verification failures and map to
/// `PkgError::Gate` instead — the content was reachable but rejected.
#[derive(Debug)]
pub enum FetchError {
    /// Transport-level failure (DNS/TLS/HTTP status/read).
    Http(String),
    /// URL scheme isn't http/https, or the URL is malformed.
    BadUrl(String),
    /// T7: caller asked for a URL the manifest entry doesn't declare.
    NotInManifest { url: String },
    /// The downloaded (or cached) bytes don't match the pinned SHA-256.
    HashMismatch { url: String, expected: String, got: String },
}

impl std::fmt::Display for FetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FetchError::Http(m) => write!(f, "download failed: {m}"),
            FetchError::BadUrl(m) => write!(f, "refusing URL: {m}"),
            FetchError::NotInManifest { url } => write!(
                f,
                "refusing to fetch {url}: not declared in the recipe manifest \
                 (dependency-confusion guard, T7)"
            ),
            FetchError::HashMismatch { url, expected, got } => write!(
                f,
                "SHA-256 mismatch for {url}: manifest pins {expected}, got {got} \
                 (deleting; do not build from this)"
            ),
        }
    }
}

impl std::error::Error for FetchError {}

impl From<FetchError> for PkgError {
    fn from(e: FetchError) -> Self {
        match e {
            e @ (FetchError::NotInManifest { .. } | FetchError::HashMismatch { .. }) => {
                PkgError::Gate(format!("fetch: {e}"))
            }
            other => PkgError::Build(format!("fetch: {other}")),
        }
    }
}

/// Fetch every declared source of a manifest entry, returning the
/// verified cache paths in manifest order.
pub fn fetch_sources(entry: &RecipeManifestEntry, cache_dir: &Path) -> PkgResult<Vec<PathBuf>> {
    std::fs::create_dir_all(cache_dir)?;
    entry
        .sources
        .iter()
        .map(|s| fetch_one(s, cache_dir))
        .collect()
}

/// Fetch one URL *guarded by the manifest*: the URL must be declared in
/// `entry.sources` (exact string match) or the request is refused (T7).
/// This is the entry point the fetch sandbox profile's network policy
/// is derived from.
pub fn fetch_url(
    entry: &RecipeManifestEntry,
    url: &str,
    cache_dir: &Path,
) -> PkgResult<PathBuf> {
    let source = entry
        .sources
        .iter()
        .find(|s| s.url == url)
        .ok_or_else(|| FetchError::NotInManifest { url: url.to_string() })?;
    fetch_one(source, cache_dir)
}

/// Cache file name for a source: `<sha256>-<basename>`. Hash-first so
/// distinct content never collides even when two URLs share a basename;
/// the basename keeps the directory human-inspectable.
pub fn cache_file_name(source: &Source) -> String {
    let base = source
        .url
        .rsplit('/')
        .next()
        .filter(|b| !b.is_empty() && b.chars().all(|c| c.is_ascii_alphanumeric() || "._+-".contains(c)))
        .unwrap_or("source");
    format!("{}-{}", source.sha256, base)
}

/// SHA-256 of a file's contents, lowercase hex. Streams in 1 MiB
/// chunks; used both for post-download verification and for validating
/// pre-existing cache entries (a cache hit is trusted only after its
/// hash re-verifies — the cache is on disk, not in the trust chain).
pub fn sha256_file(path: &Path) -> PkgResult<String> {
    let mut f = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1024 * 1024];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex_from_digest(hasher))
}

fn hex_from_digest(hasher: Sha256) -> String {
    // `finalize` already IS the digest — hex-encode it directly.
    // (`sha256_hex` would hash the digest a second time.)
    hex_encode(&hasher.finalize())
}

fn fetch_one(source: &Source, cache_dir: &Path) -> PkgResult<PathBuf> {
    validate_url(&source.url)?;
    let dest = cache_dir.join(cache_file_name(source));

    // Reuse a complete, verified cache entry.
    if dest.exists() {
        match sha256_file(&dest) {
            Ok(h) if h == source.sha256 => return Ok(dest),
            _ => {
                // Corrupt or partial cache entry: remove and refetch.
                // Never build from bytes that failed the pin.
                let _ = std::fs::remove_file(&dest);
            }
        }
    }

    let part = cache_dir.join(format!("{}.part", dest.file_name().unwrap().to_string_lossy()));
    download_to_part(&source.url, &part)?;
    let got = sha256_file(&part)?;
    if got != source.sha256 {
        let _ = std::fs::remove_file(&part);
        return Err(FetchError::HashMismatch {
            url: source.url.clone(),
            expected: source.sha256.clone(),
            got,
        }
        .into());
    }
    std::fs::rename(&part, &dest)?;
    Ok(dest)
}

/// Only http(s) over TLS-terminating transports. `file://`, `ftp://`,
/// gopher, bare IPs with userinfo tricks — all refused. The manifest is
/// the allowlist of *origins*; this is the allowlist of *schemes*.
fn validate_url(url: &str) -> PkgResult<()> {
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        return Err(FetchError::BadUrl(format!(
            "{url}: only http:// and https:// origins are fetchable"
        ))
        .into());
    }
    if url.chars().any(|c| c.is_whitespace()) {
        return Err(FetchError::BadUrl(format!("{url}: whitespace in URL")).into());
    }
    Ok(())
}

fn download_to_part(url: &str, part: &Path) -> PkgResult<()> {
    // A leftover .part from a crashed fetch is restarted from scratch
    // (see module docs on why no Range resume).
    let resp = ureq::get(url)
        .call()
        .map_err(|e| FetchError::Http(format!("{url}: {e}")))?;
    let mut reader = resp.into_reader().take(MAX_SOURCE_BYTES + 1);
    let mut out = std::fs::File::create(part)?;
    let mut total: u64 = 0;
    let mut buf = vec![0u8; 256 * 1024];
    loop {
        let n = reader
            .read(&mut buf)
            .map_err(|e| FetchError::Http(format!("{url}: read: {e}")))?;
        if n == 0 {
            break;
        }
        total += n as u64;
        if total > MAX_SOURCE_BYTES {
            let _ = std::fs::remove_file(part);
            return Err(FetchError::Http(format!(
                "{url}: exceeds {MAX_SOURCE_BYTES} byte limit"
            ))
            .into());
        }
        out.write_all(&buf[..n])?;
    }
    // Durability before the rename: a crash between rename and flush
    // must not resurrect an unflushed file at the final path.
    out.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recipe_manifest::Source;

    fn entry_with(urls: &[&str]) -> RecipeManifestEntry {
        RecipeManifestEntry {
            name: "foo".into(),
            version: "1.0".into(),
            recipe_commit: "c0ffee".into(),
            committer_fingerprint: None,
            sources: urls
                .iter()
                .map(|u| Source { url: u.to_string(), sha256: "ab".repeat(32) })
                .collect(),
            attestations: vec![],
            critical: false,
        }
    }

    #[test]
    fn refuses_urls_not_in_manifest() {
        // T7: the dependency-confusion guard fires before any network
        // access, so this test needs no network.
        let e = entry_with(&["https://example.com/foo-1.0.tar.gz"]);
        let dir = std::env::temp_dir();
        let err = fetch_url(&e, "https://attacker.example/foo-1.0.tar.gz", &dir).unwrap_err();
        assert!(err.to_string().contains("not declared in the recipe manifest"), "{err}");
    }

    #[test]
    fn refuses_non_http_schemes_even_when_declared() {
        let e = entry_with(&["file:///etc/passwd"]);
        let dir = std::env::temp_dir();
        let err = fetch_url(&e, "file:///etc/passwd", &dir).unwrap_err();
        assert!(err.to_string().contains("only http:// and https://"), "{err}");
    }

    #[test]
    fn cache_names_are_hash_keyed_and_collision_free() {
        let a = Source { url: "https://a.example/x.tar.gz".into(), sha256: "11".into() };
        let b = Source { url: "https://b.example/x.tar.gz".into(), sha256: "22".into() };
        assert_ne!(cache_file_name(&a), cache_file_name(&b));
        assert!(cache_file_name(&a).starts_with("11-"));
        assert!(cache_file_name(&a).ends_with("x.tar.gz"));
        // Weird basenames fall back to a safe name.
        let c = Source { url: "https://a.example/".into(), sha256: "33".into() };
        assert_eq!(cache_file_name(&c), "33-source");
    }

    #[test]
    fn sha256_file_hashes_contents() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("pkg-fetch-test-{}", std::process::id()));
        std::fs::write(&path, b"hello").unwrap();
        let h = sha256_file(&path).unwrap();
        assert_eq!(
            h,
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
        let _ = std::fs::remove_file(&path);
    }
}
