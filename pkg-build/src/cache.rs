//! pkg-build/src/cache.rs — personal artifact cache (SPEC "pkg-build",
//! design doc Issue 4.5).
//!
//! ## v1 changes
//!
//! - Initial implementation: content-keyed cache of locally built
//!   artifacts under `/var/lib/pkg/cache/`, keyed by the *full identity
//!   of the build* — `name/version-abitag-opttag-recipe_commit.pkg.tar.zst`.
//!   `lookup` / `fetch_into` (unpack + manifest-verify on reuse),
//!   `store` (tar+zstd pack of a store dir), LRU eviction above a 30
//!   GiB default cap. A miss is silently a source build: `lookup`
//!   returns `Ok(None)`, never an error, and a failed
//!   manifest-verification at reuse time is treated as a miss (logged,
//!   entry deleted) per Issue 4.5's invalidation rules.
//!
//! The key *is* the invalidation: because it includes `abi_tag` (which
//! transitively encodes the consumed link interfaces), the option set,
//! and the pinned recipe commit (v12 §2), a cache hit is by construction
//! the artifact a source build would have produced. No TTL, no manual
//! invalidation commands, no index, no signatures beyond the local
//! store's own manifest verification — this is content-addressed reuse
//! of your *own* past work, not a binary repository (Issue 4.7).

use std::fs::{self, File};
use std::io::{self, BufReader, BufWriter};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use pkg_core::{AbiTag, OptTag, PkgError, PkgResult, Version};
use sha2::{Digest, Sha256};

/// Default cache root (§6 layout: `/var/lib/pkg/cache/`).
pub const CACHE_ROOT: &str = "/var/lib/pkg/cache";

/// Default size cap: 30 GiB (Issue 4.5). Eviction is LRU above this.
pub const DEFAULT_CAP_BYTES: u64 = 30 * 1024 * 1024 * 1024;

/// Name of the per-artifact manifest embedded at the root of every
/// cached tarball (and of every store dir `store` packs): one
/// `<sha256>  <relative-path>` line per regular file, sorted by path.
/// Manifest verification on reuse is what makes a tampered or
/// corrupted entry degrade into a miss instead of a bad install.
pub const MANIFEST_NAME: &str = ".pkg-store-manifest";

// ---------------------------------------------------------------------
// CacheKey
// ---------------------------------------------------------------------

/// The full identity of one cached build. The on-disk form is
/// `<root>/<name>/<file_name>` where `file_name` is
/// `{version}-{abi_tag}-{opt}-{recipe_commit}.pkg.tar.zst`, with `-`
/// standing in for the empty (all-default) option tag so the five
/// dash-separated fields always parse unambiguously from the right:
/// the recipe commit is dash-free (a git object name / hex digest) and
/// the abi tag always matches `ab<N>`, so a `-rN` patch suffix inside
/// the version cannot shift the field boundaries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheKey {
    pub name: String,
    pub version: Version,
    pub abi_tag: AbiTag,
    pub opt_tag: OptTag,
    pub recipe_commit: String,
}

/// Placeholder field for the empty (all-default) option tag in a cache
/// file name. `-` can never collide with a real tag: real tags are
/// exactly 8 lowercase hex digits (`OptTag`'s `FromStr` shape).
const EMPTY_OPT_FIELD: &str = "-";

impl CacheKey {
    pub fn new(
        name: impl Into<String>,
        version: Version,
        abi_tag: AbiTag,
        opt_tag: OptTag,
        recipe_commit: impl Into<String>,
    ) -> Self {
        CacheKey {
            name: name.into(),
            version,
            abi_tag,
            opt_tag,
            recipe_commit: recipe_commit.into(),
        }
    }

    /// `<version>-<abi_tag>-<opt>-<recipe_commit>.pkg.tar.zst`.
    pub fn file_name(&self) -> String {
        let opt = if self.opt_tag.is_default() {
            EMPTY_OPT_FIELD.to_string()
        } else {
            self.opt_tag.to_string()
        };
        format!(
            "{}-{}-{}-{}.pkg.tar.zst",
            self.version, self.abi_tag, opt, self.recipe_commit
        )
    }

    /// Parses a cache file name produced by [`CacheKey::file_name`].
    /// Fields are peeled off the right (commit, opt, abi) so a version
    /// containing dashes (e.g. `2.39-r2`) round-trips.
    pub fn parse_file_name(name: &str, file_name: &str) -> Option<Self> {
        let stem = file_name.strip_suffix(".pkg.tar.zst")?;
        let (rest, commit) = stem.rsplit_once('-')?;
        if commit.is_empty() || commit.contains('.') {
            return None;
        }
        // The empty-opt placeholder "-" means the opt+commit separators
        // collapse into a trailing "--" on `rest` (`<v>-abN--` after the
        // commit is split off). A real tag is always 8 hex digits.
        let (rest, opt_tag) = if let Some(r) = rest.strip_suffix("--") {
            (r, OptTag::none())
        } else {
            let (r, opt) = rest.rsplit_once('-')?;
            if opt == EMPTY_OPT_FIELD {
                (r, OptTag::none())
            } else {
                (r, opt.parse().ok()?)
            }
        };
        let (version_str, abi_str) = rest.rsplit_once('-')?;
        // The abi field must be `ab` followed by digits — this is what
        // keeps a `-rN` version suffix from being mistaken for it.
        let abi_num = abi_str.strip_prefix("ab")?;
        if abi_num.is_empty() || !abi_num.chars().all(|c| c.is_ascii_digit()) {
            return None;
        }
        let version: Version = version_str.parse().ok()?;
        let abi_tag = AbiTag(abi_num.parse().ok()?);
        Some(CacheKey::new(name, version, abi_tag, opt_tag, commit))
    }
}

// ---------------------------------------------------------------------
// ArtifactCache
// ---------------------------------------------------------------------

/// Usage metadata for one cached artifact, fed to [`plan_eviction`].
/// Kept separate from [`ArtifactCache`] so the eviction policy is a
/// pure function over this data — directly unit-testable without a
/// filesystem.
#[derive(Debug, Clone)]
pub struct CacheEntryMeta {
    pub path: PathBuf,
    pub size: u64,
    /// Last-use timestamp (file atime, falling back to mtime): the
    /// "R" in LRU.
    pub last_used: SystemTime,
}

/// The personal artifact cache. Never authoritative: every failure to
/// find or validate an entry is reported as a miss, and the caller
/// falls back to a source build.
#[derive(Debug, Clone)]
pub struct ArtifactCache {
    root: PathBuf,
    cap_bytes: u64,
}

impl ArtifactCache {
    pub fn new(root: PathBuf, cap_bytes: u64) -> Self {
        ArtifactCache { root, cap_bytes }
    }

    /// The cache at the spec location with the spec cap.
    pub fn default_root() -> Self {
        Self::new(PathBuf::from(CACHE_ROOT), DEFAULT_CAP_BYTES)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn cap_bytes(&self) -> u64 {
        self.cap_bytes
    }

    /// Where `key`'s artifact lives (whether or not it exists).
    pub fn path_for(&self, key: &CacheKey) -> PathBuf {
        self.root.join(&key.name).join(key.file_name())
    }

    /// Looks up `key`, returning the artifact path on a hit.
    ///
    /// A miss — including any I/O error looking — is `Ok(None)`:
    /// the cache is never authoritative and a lookup failure must
    /// silently degrade into a source build (Issue 4.5), not fail the
    /// transaction.
    pub fn lookup(&self, key: &CacheKey) -> PkgResult<Option<PathBuf>> {
        let path = self.path_for(key);
        match fs::metadata(&path) {
            Ok(meta) if meta.is_file() => {
                // Touch atime-equivalent bookkeeping: mtime is what
                // survives on noatime mounts, and it doubles as the
                // LRU clock for eviction.
                let _ = filetime_touch(&path);
                Ok(Some(path))
            }
            _ => Ok(None),
        }
    }

    /// Packs `store_dir` (a completed `.store/` directory, contents
    /// rooted at `/`) into the cache under `key`, evicting LRU entries
    /// first if the cap would be exceeded. Returns the artifact path.
    ///
    /// The store-manifest is (re)generated at pack time so a reuse-time
    /// verification always has something to check against, even for
    /// store dirs produced before the manifest convention existed.
    pub fn store(&self, key: &CacheKey, store_dir: &Path) -> PkgResult<PathBuf> {
        if !store_dir.is_dir() {
            return Err(PkgError::Build(format!(
                "artifact cache: store dir {} does not exist",
                store_dir.display()
            )));
        }
        write_manifest(store_dir)?;
        let dest = self.path_for(key);
        let parent = dest.parent().expect("path_for always has a parent");
        fs::create_dir_all(parent).map_err(|e| {
            PkgError::Io(io::Error::new(
                e.kind(),
                format!("creating cache dir {}: {e}", parent.display()),
            ))
        })?;

        // Write to a temp file in the same directory, then rename:
        // a crashed or interrupted `store` must never leave a
        // half-written tarball that a later `lookup` would treat as a
        // hit.
        let tmp = parent.join(format!(".{}.tmp", key.file_name()));
        let pack_result = pack_tar_zst(store_dir, &tmp);
        match pack_result {
            Ok(()) => {}
            Err(e) => {
                let _ = fs::remove_file(&tmp);
                return Err(e);
            }
        }
        fs::rename(&tmp, &dest).map_err(PkgError::Io)?;

        self.evict_to_cap()?;
        Ok(dest)
    }

    /// Reuse path: unpack `key`'s artifact into `dest_dir` and verify
    /// every file against the embedded store manifest.
    ///
    /// Returns `Ok(true)` on a verified hit. A miss — absent entry OR a
    /// failed manifest verification — is `Ok(false)`; a failed
    /// verification additionally deletes the offending entry (Issue
    /// 4.5: "treated as a miss, logged, deleted"). The caller builds
    /// from source on `false`.
    pub fn fetch_into(&self, key: &CacheKey, dest_dir: &Path) -> PkgResult<bool> {
        let Some(path) = self.lookup(key)? else {
            return Ok(false);
        };
        fs::create_dir_all(dest_dir).map_err(PkgError::Io)?;
        if let Err(e) = unpack_tar_zst(&path, dest_dir) {
            // A corrupt tarball is a miss, not an error.
            eprintln!(
                "pkg: cache entry {} failed to unpack ({e}); \
                 discarding and building from source",
                path.display()
            );
            let _ = fs::remove_file(&path);
            let _ = fs::remove_dir_all(dest_dir);
            return Ok(false);
        }
        match verify_manifest(dest_dir) {
            Ok(()) => Ok(true),
            Err(e) => {
                eprintln!(
                    "pkg: cache entry {} failed manifest verification ({e}); \
                     discarding and building from source",
                    path.display()
                );
                let _ = fs::remove_file(&path);
                let _ = fs::remove_dir_all(dest_dir);
                Ok(false)
            }
        }
    }

    /// Total size of every cached artifact, in bytes.
    pub fn total_size(&self) -> PkgResult<u64> {
        Ok(self.collect_entries()?.iter().map(|e| e.size).sum())
    }

    /// LRU-evicts entries until the total size is at or under the cap.
    /// Returns the paths that were removed (for `pkg clean --cache`
    /// reporting).
    pub fn evict_to_cap(&self) -> PkgResult<Vec<PathBuf>> {
        let entries = self.collect_entries()?;
        let doomed = plan_eviction(&entries, self.cap_bytes);
        for path in &doomed {
            fs::remove_file(path).map_err(PkgError::Io)?;
        }
        Ok(doomed)
    }

    /// Removes every cached artifact (`pkg clean --cache`). Returns the
    /// number of bytes reclaimed.
    pub fn clear(&self) -> PkgResult<u64> {
        let entries = self.collect_entries()?;
        let mut reclaimed = 0u64;
        for entry in entries {
            fs::remove_file(&entry.path).map_err(PkgError::Io)?;
            reclaimed += entry.size;
        }
        Ok(reclaimed)
    }

    /// Walks the cache collecting every `*.pkg.tar.zst` entry with its
    /// size and last-use time. Entries that can't be stat'ed are
    /// skipped — a transient read failure must not break eviction for
    /// the rest of the cache.
    fn collect_entries(&self) -> PkgResult<Vec<CacheEntryMeta>> {
        let mut entries = Vec::new();
        if !self.root.is_dir() {
            return Ok(entries);
        }
        let mut stack = vec![self.root.clone()];
        while let Some(dir) = stack.pop() {
            for entry in fs::read_dir(&dir).map_err(PkgError::Io)? {
                let entry = entry.map_err(PkgError::Io)?;
                let path = entry.path();
                let meta = match entry.metadata() {
                    Ok(m) => m,
                    Err(_) => continue,
                };
                if meta.is_dir() {
                    stack.push(path);
                } else if meta.is_file()
                    && path
                        .file_name()
                        .and_then(|n| n.to_str())
                        .is_some_and(|n| n.ends_with(".pkg.tar.zst"))
                {
                    let last_used = meta
                        .accessed()
                        .or_else(|_| meta.modified())
                        .unwrap_or(SystemTime::UNIX_EPOCH);
                    entries.push(CacheEntryMeta {
                        path,
                        size: meta.len(),
                        last_used,
                    });
                }
            }
        }
        Ok(entries)
    }
}

/// Pure eviction policy: given every entry and the cap, returns which
/// paths to delete — least-recently-used first — so the survivors fit
/// under `cap_bytes`. Split out of `evict_to_cap` so the policy is
/// unit-testable without a filesystem.
///
/// The single most-recently-used entry is never evicted: it is usually
/// the artifact a build just produced, and deleting it to satisfy a cap
/// smaller than one artifact would empty the cache on every build.
pub fn plan_eviction(entries: &[CacheEntryMeta], cap_bytes: u64) -> Vec<PathBuf> {
    let total: u64 = entries.iter().map(|e| e.size).sum();
    if total <= cap_bytes {
        return Vec::new();
    }
    let mut by_age: Vec<&CacheEntryMeta> = entries.iter().collect();
    // Oldest last-used first; ties broken by path for determinism.
    by_age.sort_by(|a, b| {
        a.last_used
            .cmp(&b.last_used)
            .then_with(|| a.path.cmp(&b.path))
    });
    let mut remaining = total;
    let mut doomed = Vec::new();
    let last_idx = by_age.len().saturating_sub(1);
    for (idx, entry) in by_age.into_iter().enumerate() {
        if remaining <= cap_bytes || idx == last_idx {
            break;
        }
        remaining -= entry.size;
        doomed.push(entry.path.clone());
    }
    doomed
}

// ---------------------------------------------------------------------
// Store manifest (write at pack time, verify at reuse time)
// ---------------------------------------------------------------------

/// Writes `<dir>/.pkg-store-manifest`: one `<sha256>  <relpath>` line
/// per regular file under `dir`, sorted by relative path. The manifest
/// never lists itself.
pub fn write_manifest(dir: &Path) -> PkgResult<()> {
    let mut lines = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for entry in fs::read_dir(&d).map_err(PkgError::Io)? {
            let entry = entry.map_err(PkgError::Io)?;
            let path = entry.path();
            let meta = entry.metadata().map_err(PkgError::Io)?;
            if meta.is_dir() {
                stack.push(path);
            } else if meta.is_file() {
                if path.file_name().and_then(|n| n.to_str()) == Some(MANIFEST_NAME)
                    && path.parent() == Some(dir)
                {
                    continue;
                }
                let rel = path.strip_prefix(dir).map_err(|_| {
                    PkgError::Build(format!(
                        "cache manifest: {} escapes {}",
                        path.display(),
                        dir.display()
                    ))
                })?;
                let digest = sha256_file(&path)?;
                lines.push(format!("{}  {}", digest, rel.display()));
            }
            // Symlinks are preserved as-is by tar and verified
            // structurally by unpack; only regular-file *contents* need
            // hashes.
        }
    }
    lines.sort();
    fs::write(dir.join(MANIFEST_NAME), lines.join("\n") + "\n").map_err(PkgError::Io)
}

/// Verifies every regular file under `dir` against its embedded
/// manifest. Missing files, extra regular files, and hash mismatches
/// are all failures — the entry is indistinguishable from corruption
/// or tampering (v13 §5's staged-tree reasoning applied to the cache).
pub fn verify_manifest(dir: &Path) -> PkgResult<()> {
    let manifest_path = dir.join(MANIFEST_NAME);
    let text = fs::read_to_string(&manifest_path).map_err(|e| {
        PkgError::Parse(format!(
            "store manifest {} unreadable: {e}",
            manifest_path.display()
        ))
    })?;
    let mut listed = 0usize;
    for (lineno, line) in text.lines().enumerate() {
        if line.is_empty() {
            continue;
        }
        let (hash, rel) = line.split_once("  ").ok_or_else(|| {
            PkgError::Parse(format!(
                "store manifest line {} malformed: {line:?}",
                lineno + 1
            ))
        })?;
        let path = dir.join(rel);
        let actual = sha256_file(&path).map_err(|e| {
            PkgError::Build(format!("manifest-listed file {rel} unreadable: {e}"))
        })?;
        if actual != hash {
            return Err(PkgError::Build(format!(
                "manifest mismatch on {rel}: expected {hash}, got {actual}"
            )));
        }
        listed += 1;
    }
    // Count regular files actually present (manifest excluded) — an
    // *extra* file is as much a mismatch as a changed one.
    let mut present = 0usize;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for entry in fs::read_dir(&d).map_err(PkgError::Io)? {
            let entry = entry.map_err(PkgError::Io)?;
            let path = entry.path();
            let meta = entry.metadata().map_err(PkgError::Io)?;
            if meta.is_dir() {
                stack.push(path);
            } else if meta.is_file() {
                if path == manifest_path {
                    continue;
                }
                present += 1;
            }
        }
    }
    if present != listed {
        return Err(PkgError::Build(format!(
            "manifest lists {listed} files but {present} are present"
        )));
    }
    Ok(())
}

fn sha256_file(path: &Path) -> PkgResult<String> {
    let mut file = BufReader::new(File::open(path).map_err(PkgError::Io)?);
    let mut hasher = Sha256::new();
    io::copy(&mut file, &mut hasher).map_err(PkgError::Io)?;
    Ok(hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

// ---------------------------------------------------------------------
// tar + zstd pack / unpack
// ---------------------------------------------------------------------

/// Packs the *contents* of `src` (paths relative to `src`, so the
/// tarball unpacks as a store-dir root) into `dest` as tar+zstd.
fn pack_tar_zst(src: &Path, dest: &Path) -> PkgResult<()> {
    let file = BufWriter::new(File::create(dest).map_err(PkgError::Io)?);
    let encoder = zstd::Encoder::new(file, 3).map_err(PkgError::Io)?;
    let mut builder = tar::Builder::new(encoder);
    builder.follow_symlinks(false);
    for entry in fs::read_dir(src).map_err(PkgError::Io)? {
        let entry = entry.map_err(PkgError::Io)?;
        let path = entry.path();
        let name = entry.file_name();
        let file_type = entry.file_type().map_err(PkgError::Io)?;
        if file_type.is_dir() {
            // append_path_with_name archives only the directory entry
            // itself; recursion is append_dir_all's job (and it honors
            // follow_symlinks(false) for everything below).
            builder
                .append_dir_all(Path::new(&name), &path)
                .map_err(PkgError::Io)?;
        } else {
            builder
                .append_path_with_name(&path, Path::new(&name))
                .map_err(PkgError::Io)?;
        }
    }
    let encoder = builder.into_inner().map_err(PkgError::Io)?;
    encoder.finish().map_err(PkgError::Io)?;
    Ok(())
}

/// Unpacks a tar+zstd artifact into `dest_dir`, which must exist.
/// Refuses entries that would escape `dest_dir` (`..` components or
/// absolute paths) — a cache entry is locally produced, but unpacking
/// is exactly the moment a tampered entry would try to write outside
/// the store.
fn unpack_tar_zst(artifact: &Path, dest_dir: &Path) -> PkgResult<()> {
    let file = BufReader::new(File::open(artifact).map_err(PkgError::Io)?);
    let decoder = zstd::Decoder::new(file).map_err(PkgError::Io)?;
    let mut archive = tar::Archive::new(decoder);
    for entry in archive.entries().map_err(PkgError::Io)? {
        let mut entry = entry.map_err(PkgError::Io)?;
        let rel = entry.path().map_err(PkgError::Io)?.into_owned();
        let mut clean = PathBuf::new();
        for component in rel.components() {
            match component {
                std::path::Component::Normal(part) => clean.push(part),
                std::path::Component::CurDir => {}
                _ => {
                    return Err(PkgError::Build(format!(
                        "cache artifact {} contains unsafe path {}",
                        artifact.display(),
                        rel.display()
                    )))
                }
            }
        }
        let target = dest_dir.join(&clean);
        if entry.header().entry_type().is_dir() {
            fs::create_dir_all(&target).map_err(PkgError::Io)?;
        } else {
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent).map_err(PkgError::Io)?;
            }
            entry.unpack(&target).map_err(PkgError::Io)?;
        }
    }
    Ok(())
}

/// Best-effort atime/mtime refresh for LRU bookkeeping. Errors are
/// ignored: LRU precision is a cache-quality detail, never correctness.
fn filetime_touch(path: &Path) -> io::Result<()> {
    // Opening O_RDWR and writing nothing updates neither time
    // portably; the cheap portable touch is metadata write via
    // `OpenOptions` + `set_modified`. atime itself updates on the
    // `metadata()` read paths on relatime mounts, which is good
    // enough — eviction only needs an ordering, not exact times.
    let now = filetime_now();
    let file = fs::OpenOptions::new().write(true).open(path)?;
    file.set_modified(now)?;
    Ok(())
}

fn filetime_now() -> SystemTime {
    SystemTime::now()
}

// ---------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn key(opt: &str) -> CacheKey {
        CacheKey::new(
            "openssl",
            "3.0.14".parse().unwrap(),
            AbiTag(1),
            opt.parse().unwrap(),
            "a1b2c3d4e5f6",
        )
    }

    #[test]
    fn cache_key_file_name_format() {
        // Spec: cache/<name>/<version>-<abi_tag>-<opt>-<recipe_commit>.pkg.tar.zst
        assert_eq!(
            key("").file_name(),
            "3.0.14-ab1---a1b2c3d4e5f6.pkg.tar.zst"
        );
        assert_eq!(
            key("9f2c1d00").file_name(),
            "3.0.14-ab1-9f2c1d00-a1b2c3d4e5f6.pkg.tar.zst"
        );
    }

    #[test]
    fn cache_key_roundtrip_with_patched_version() {
        // A `-rN` patch suffix in the version must not shift fields.
        let k = CacheKey::new(
            "glibc",
            "2.39-r2".parse().unwrap(),
            AbiTag(7),
            OptTag::none(),
            "deadbeef",
        );
        let parsed = CacheKey::parse_file_name("glibc", &k.file_name()).unwrap();
        assert_eq!(parsed, k);
    }

    #[test]
    fn cache_key_rejects_garbage() {
        assert!(CacheKey::parse_file_name("x", "not-a-cache-file.tar.zst").is_none());
        assert!(CacheKey::parse_file_name("x", "1.0-ab1--.pkg.tar.zst").is_none());
        assert!(CacheKey::parse_file_name("x", "1.0-r1-9f2c1d00-c0ffee.pkg.tar.zst").is_none());
    }

    #[test]
    fn eviction_is_lru_and_stops_at_cap() {
        let t0 = SystemTime::UNIX_EPOCH;
        let mk = |n: &str, size: u64, secs: u64| CacheEntryMeta {
            path: PathBuf::from(n),
            size,
            last_used: t0 + std::time::Duration::from_secs(secs),
        };
        // total 30, cap 20 → must evict 10, oldest first.
        let entries = vec![
            mk("a", 10, 100), // oldest
            mk("b", 10, 200),
            mk("c", 10, 300), // newest
        ];
        let doomed = plan_eviction(&entries, 20);
        assert_eq!(doomed, vec![PathBuf::from("a")]);

        // cap 5 → evict a and b.
        let doomed = plan_eviction(&entries, 5);
        assert_eq!(doomed, vec![PathBuf::from("a"), PathBuf::from("b")]);
    }

    #[test]
    fn eviction_noop_under_cap() {
        let t0 = SystemTime::UNIX_EPOCH;
        let entries = vec![CacheEntryMeta {
            path: PathBuf::from("a"),
            size: 10,
            last_used: t0,
        }];
        assert!(plan_eviction(&entries, 10).is_empty());
        assert!(plan_eviction(&entries, 100).is_empty());
    }

    #[test]
    fn manifest_roundtrip_verifies() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("usr/lib")).unwrap();
        fs::write(dir.path().join("usr/lib/libfoo.so"), b"elf-bytes").unwrap();
        fs::write(dir.path().join("usr/bin-tool"), b"tool").unwrap();
        write_manifest(dir.path()).unwrap();
        verify_manifest(dir.path()).unwrap();
    }

    #[test]
    fn manifest_catches_tampering() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("f"), b"original").unwrap();
        write_manifest(dir.path()).unwrap();
        fs::write(dir.path().join("f"), b"tampered!").unwrap();
        assert!(verify_manifest(dir.path()).is_err());
    }

    #[test]
    fn pack_unpack_roundtrip() {
        let src = tempfile::tempdir().unwrap();
        fs::create_dir_all(src.path().join("usr/lib")).unwrap();
        fs::write(src.path().join("usr/lib/libfoo.so"), b"elf").unwrap();
        std::os::unix::fs::symlink("libfoo.so", src.path().join("usr/lib/libfoo.so.1"))
            .unwrap();
        write_manifest(src.path()).unwrap();

        // The artifact must live OUTSIDE the dir being packed —
        // otherwise the tarball would try to contain itself.
        let artifact_dir = tempfile::tempdir().unwrap();
        let artifact = artifact_dir.path().join("out.pkg.tar.zst");
        pack_tar_zst(src.path(), &artifact).unwrap();

        let dest = tempfile::tempdir().unwrap();
        unpack_tar_zst(&artifact, dest.path()).unwrap();
        assert_eq!(
            fs::read(dest.path().join("usr/lib/libfoo.so")).unwrap(),
            b"elf"
        );
        assert!(dest.path().join("usr/lib/libfoo.so.1").is_symlink());
        verify_manifest(dest.path()).unwrap();
    }
}
