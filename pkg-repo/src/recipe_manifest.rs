//! ## v1 (initial)
//!
//! ## v2
//!
//! - [FIX] Added [`load_cached_recipe_manifest`]: reads the
//!   `recipe-manifest.toml` that `sync.rs` caches next to the index,
//!   with a `PkgError::NotFound` pointing at `pkg sync` when the cache
//!   is absent. This is the acquisition path `pkg-build`'s fetch stage
//!   uses — previously nothing ever wrote *or* read a cached manifest.
//!
//! The signed recipe manifest (v12 §2) plus v14 §4 attestation types.
//! The manifest maps `package@version → { recipe_commit, source hashes,
//! attestations }`; it is rebuilt by the index-signing CI *from the
//! recipe git repo at a tagged state*, never from working-tree input
//! (v14 §4 governance), and the client trusts it because the index
//! signature covers it — the git host and tarball hosts are transport.
//!
//! # Attestation semantics (v14 §4, deliberately narrow)
//!
//! An attestation means exactly: *"I reviewed the recipe diff and the
//! recorded hashes against the upstream sources reachable at review
//! time."* It is not a claim of upstream benevolence. The critical-set
//! rule: entries marked `critical = true` require **≥2 attestations
//! from distinct reviewer-role keys, distinct from the committer**
//! (T3/T4 mitigation — a governance control raising the cost of a
//! single-key compromise, not a proof of good faith). Non-critical
//! entries require ≥1 valid reviewer attestation (the committer's own
//! counts, per "non-critical packages require one (the committer's)").
//!
//! # Signed message format
//!
//! Each attestation's Ed25519 signature covers
//! [`attestation_message`]: a deterministic, unambiguous textual
//! serialization of (name, version, recipe_commit, source list sorted
//! by URL). Anything attesting outside this crate must reproduce that
//! byte string exactly.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use pkg_core::manifest::{PackageManifest, SourcePin};
use pkg_core::{PkgError, PkgResult};

use crate::trust::{hex_decode, TrustStore};
use ed25519_dalek::Signature;

/// The whole manifest file (`recipe-manifest.toml` in the index
/// bundle's universe). TOML shape follows the v14 design doc:
/// top-level `[[package]]` entries.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecipeManifest {
    #[serde(rename = "package", default)]
    pub packages: Vec<RecipeManifestEntry>,
}

impl RecipeManifest {
    pub fn from_toml_str(input: &str) -> PkgResult<Self> {
        toml::from_str(input).map_err(|e| PkgError::Parse(format!("recipe manifest: {e}")))
    }

    pub fn find(&self, name: &str, version: &str) -> Option<&RecipeManifestEntry> {
        self.packages
            .iter()
            .find(|e| e.name == name && e.version == version)
    }
}

/// The file name under the sync cache dir where [`crate::sync`]
/// caches the manifest alongside `index.json`.
pub const RECIPE_MANIFEST_NAME: &str = "recipe-manifest.toml";

/// Loads the recipe manifest `pkg sync` cached at
/// `cache_dir/recipe-manifest.toml`. A missing cache file is a
/// `PkgError::NotFound` that names the remedy — nothing can be fetched
/// (T7) or built until a sync has populated the cache.
pub fn load_cached_recipe_manifest(cache_dir: &Path) -> PkgResult<RecipeManifest> {
    let path = cache_dir.join(RECIPE_MANIFEST_NAME);
    let text = std::fs::read_to_string(&path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            PkgError::NotFound(format!(
                "no cached recipe manifest at {} — run `pkg sync` first",
                path.display()
            ))
        } else {
            PkgError::Io(e)
        }
    })?;
    RecipeManifest::from_toml_str(&text)
}

/// One package's pinned recipe state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecipeManifestEntry {
    pub name: String,
    pub version: String,
    /// Git commit hash of the recipe; the recipe is checked out at
    /// exactly this commit (v12 §2 — commits pinned by hash, covered by
    /// the index signature; commit *signing* not required).
    pub recipe_commit: String,
    /// Fingerprint of the maintainer key that committed this entry,
    /// recorded by the index-signing CI. Used by the critical-set rule:
    /// the committer's own attestation never counts toward the required
    /// two *reviewer* sign-offs (v14 §4). `Option` because pre-v14
    /// manifests don't carry it; the rule then degrades to "≥2 distinct
    /// reviewer keys" (documented, fail-open in exactly the legacy
    /// direction).
    #[serde(default)]
    pub committer_fingerprint: Option<String>,
    /// TOML: `[[package.source]]`.
    #[serde(rename = "source", default)]
    pub sources: Vec<Source>,
    /// TOML: `[[package.attestation]]`.
    #[serde(rename = "attestation", default)]
    pub attestations: Vec<Attestation>,
    /// Critical-set marker (v14 §4): bootstrap path, toolchain, `pkg`
    /// itself — target <50 packages.
    #[serde(default)]
    pub critical: bool,
}

/// A pinned source tarball. `pkg fetch` refuses to fetch anything not
/// declared here (T7 dependency-confusion guard).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Source {
    pub url: String,
    pub sha256: String,
    /// Optional basename used in the source cache and build scratch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filename: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
pub struct SourceToml {
    #[serde(default)]
    pub source: Vec<SourcePin>,
}

/// Recipe files covered by the content-derived recipe identity.
pub const RECIPE_FILES: [&str; 6] = [
    "pkg-meta.toml",
    "source.toml",
    "prepare",
    "build",
    "check",
    "install",
];

/// First 12 hexadecimal digits of SHA-256 over the sorted recipe files.
pub fn recipe_commit(dir: &Path) -> String {
    let mut hasher = Sha256::new();
    for name in RECIPE_FILES {
        if let Ok(bytes) = std::fs::read(dir.join(name)) {
            hasher.update(name.as_bytes());
            hasher.update(&bytes);
        }
    }
    crate::trust::hex_encode(&hasher.finalize())[..12].to_string()
}

pub fn read_source_pins(dir: &Path) -> PkgResult<Vec<SourcePin>> {
    let meta_path = dir.join("pkg-meta.toml");
    let meta_text = std::fs::read_to_string(&meta_path).map_err(PkgError::Io)?;
    let manifest = PackageManifest::from_toml_str(&meta_text)?;
    select_source_pins(&manifest, dir)
}

fn select_source_pins(manifest: &PackageManifest, dir: &Path) -> PkgResult<Vec<SourcePin>> {
    let path = dir.join("source.toml");
    let external = match std::fs::read_to_string(&path) {
        Ok(text) => toml::from_str::<SourceToml>(&text)
            .map(|parsed| parsed.source)
            .map_err(|e| PkgError::Parse(format!("{}: {e}", path.display()))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(PkgError::Io(e)),
    }?;
    if !manifest.sources.is_empty() && !external.is_empty() {
        return Err(PkgError::Parse(format!(
            "{} declares sources in both pkg-meta.toml and source.toml; use only one",
            dir.display()
        )));
    }
    Ok(if manifest.sources.is_empty() {
        external
    } else {
        manifest.sources.clone()
    })
}

/// Builds the source authorization for an explicitly named local recipe.
/// Relative paths are resolved from the recipe directory. No repository
/// attestations are invented for this user-supplied entry.
pub fn local_manifest_entry(
    manifest: &PackageManifest,
    dir: &Path,
) -> PkgResult<RecipeManifestEntry> {
    let mut sources = Vec::new();
    for pin in select_source_pins(manifest, dir)? {
        let url = if pin.url.starts_with("https://") || pin.url.starts_with("http://") {
            pin.url.clone()
        } else {
            if pin.url.contains("://") && !pin.url.starts_with("file://") {
                return Err(PkgError::Parse(format!(
                    "{}: unsupported local source scheme in {}",
                    dir.join("source.toml").display(),
                    pin.url
                )));
            }
            let raw = pin.url.strip_prefix("file://").unwrap_or(&pin.url);
            let path = PathBuf::from(raw);
            let path = if path.is_absolute() {
                path
            } else {
                dir.join(path)
            };
            let path = std::fs::canonicalize(&path).map_err(|e| {
                PkgError::Parse(format!(
                    "local source {} for {}: {e}",
                    path.display(),
                    manifest.name
                ))
            })?;
            if !path.is_file() {
                return Err(PkgError::Parse(format!(
                    "local source {} for {} is not a regular file",
                    path.display(),
                    manifest.name
                )));
            }
            let text = path.to_str().ok_or_else(|| {
                PkgError::Parse(format!(
                    "local source path for {} is not valid UTF-8",
                    manifest.name
                ))
            })?;
            format!("file://{text}")
        };
        sources.push(Source {
            url,
            sha256: pin.sha256,
            filename: pin.filename,
        });
    }
    Ok(RecipeManifestEntry {
        name: manifest.name.clone(),
        version: manifest.version.to_string(),
        recipe_commit: recipe_commit(dir),
        committer_fingerprint: None,
        sources,
        attestations: Vec::new(),
        critical: manifest.critical,
    })
}

/// One reviewer's sign-off: an Ed25519 signature by a reviewer-role key
/// over [`attestation_message`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attestation {
    pub key_fingerprint: String,
    /// Hex-encoded Ed25519 signature.
    pub sig: String,
}

/// The canonical byte string an attestation signs:
///
/// ```text
/// pkg-recipe-attestation-v1\n
/// <name>\n<version>\n<recipe_commit>\n
/// <sha256> <url>\n      (one line per source, sorted by URL)
/// ```
///
/// Length-prefix-free but newline-delimited with fields that cannot
/// contain newlines by construction (validated below), so the encoding
/// is unambiguous.
pub fn attestation_message(entry: &RecipeManifestEntry) -> Vec<u8> {
    let mut s = String::new();
    s.push_str("pkg-recipe-attestation-v1\n");
    s.push_str(&entry.name);
    s.push('\n');
    s.push_str(&entry.version);
    s.push('\n');
    s.push_str(&entry.recipe_commit);
    s.push('\n');
    let mut sources: Vec<&Source> = entry.sources.iter().collect();
    sources.sort_by(|a, b| a.url.cmp(&b.url));
    for src in sources {
        s.push_str(&src.sha256);
        s.push(' ');
        s.push_str(&src.url);
        s.push('\n');
    }
    s.into_bytes()
}

/// Errors from attestation verification. Converted to `PkgError::Gate`
/// at the boundary: a failed attestation check is a *verification
/// refusal* of content that must not be built, which is the gate's
/// semantic (verification, per v12 §9 framing).
#[derive(Debug)]
pub enum AttestationError {
    /// The entry's fields can't form an unambiguous message.
    Malformed(String),
    /// A signature was invalid hex or failed verification (fail-loud —
    /// never silently skipped).
    BadSignature { fingerprint: String },
    /// Not enough valid attestations for the entry's requirement.
    Insufficient {
        critical: bool,
        have: usize,
        need: usize,
        detail: String,
    },
}

impl std::fmt::Display for AttestationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AttestationError::Malformed(m) => write!(f, "malformed manifest entry: {m}"),
            AttestationError::BadSignature { fingerprint } => {
                write!(f, "invalid attestation signature from {fingerprint}")
            }
            AttestationError::Insufficient {
                critical,
                have,
                need,
                detail,
            } => write!(
                f,
                "insufficient attestations ({have} of {need} required{}): {detail}",
                if *critical {
                    " for critical-set package"
                } else {
                    ""
                }
            ),
        }
    }
}

impl std::error::Error for AttestationError {}

impl From<AttestationError> for PkgError {
    fn from(e: AttestationError) -> Self {
        PkgError::Gate(format!("attestation: {e}"))
    }
}

/// Verify an entry's attestation set against the trust store's
/// reviewer role (v14 §4). Returns the number of **distinct, valid
/// reviewer-role keys** that attested (including the committer's, if
/// present and a reviewer).
///
/// Rules:
///
/// - Every attestation must validate or the whole check fails —
///   partial credit would let an attacker pad a good entry with a
///   forgery to test verification paths.
/// - Attestations from keys not in the reviewer role (or locally
///   revoked — `TrustStore::reviewer_key` filters those) do not count.
/// - **Critical set:** ≥2 valid attestations from distinct reviewer
///   keys *distinct from `committer_fingerprint`* when recorded.
/// - **Non-critical:** ≥1 valid reviewer attestation (the committer's
///   counts).
///
/// Client-side this is a spot-check (cheap, fail-loud); the *enforcement
/// point* is index-signing time, where the repository role refuses to
/// sign an index containing an under-attested critical entry (v14 §4).
pub fn verify_attestations(entry: &RecipeManifestEntry, trust: &TrustStore) -> PkgResult<usize> {
    for field in [&entry.name, &entry.version, &entry.recipe_commit] {
        if field.contains('\n') {
            return Err(AttestationError::Malformed(
                "name/version/recipe_commit must not contain newlines".into(),
            )
            .into());
        }
    }
    let msg = attestation_message(entry);

    let mut valid_reviewers: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for att in &entry.attestations {
        let Some(key) = trust.reviewer_key(&att.key_fingerprint) else {
            // Unknown or locally-revoked key: doesn't count, not an
            // error by itself (the entry may have been attested by a
            // since-rotated key; the *count* rules below decide).
            continue;
        };
        let sig_bytes = hex_decode(&att.sig)
            .map_err(|e| AttestationError::Malformed(format!("attestation sig hex: {e}")))?;
        let arr: [u8; 64] = sig_bytes
            .try_into()
            .map_err(|_| AttestationError::Malformed("attestation sig not 64 bytes".into()))?;
        let sig = Signature::from_bytes(&arr);
        key.verify_strict(&msg, &sig)
            .map_err(|_| AttestationError::BadSignature {
                fingerprint: att.key_fingerprint.clone(),
            })?;
        valid_reviewers.insert(att.key_fingerprint.clone());
    }

    let total_valid = valid_reviewers.len();

    if entry.critical {
        // Reviewer sign-offs excluding the committer (T3: two-party
        // review; the committer approving their own recipe is not
        // review).
        let committer = entry.committer_fingerprint.as_deref();
        let independent = valid_reviewers
            .iter()
            .filter(|fp| Some(fp.as_str()) != committer)
            .count();
        if independent < 2 {
            return Err(AttestationError::Insufficient {
                critical: true,
                have: independent,
                need: 2,
                detail: format!(
                    "critical-set package {} {} needs 2 attestations from distinct \
                     reviewer keys other than the committer (found {} valid reviewer \
                     attestation(s) total{})",
                    entry.name,
                    entry.version,
                    total_valid,
                    if committer.is_none() {
                        "; no committer fingerprint recorded, so committer exclusion \
                         could not be applied"
                    } else {
                        ""
                    }
                ),
            }
            .into());
        }
    } else if total_valid < 1 {
        return Err(AttestationError::Insufficient {
            critical: false,
            have: 0,
            need: 1,
            detail: format!(
                "package {} {} has no valid attestation from any reviewer-role key",
                entry.name, entry.version
            ),
        }
        .into());
    }

    Ok(total_valid)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trust::{hex_encode, key_fingerprint, RoleDelegation, RootManifest};
    use ed25519_dalek::{Signer, SigningKey};
    use std::collections::BTreeMap;

    fn test_key(seed: u8) -> SigningKey {
        SigningKey::from_bytes(&[seed; 32])
    }

    /// A trust store whose reviewer role contains exactly `keys`.
    fn store_with_reviewers(keys: &[&SigningKey]) -> TrustStore {
        let mut reviewers = BTreeMap::new();
        for k in keys {
            let vk = k.verifying_key();
            reviewers.insert(key_fingerprint(&vk), hex_encode(vk.as_bytes()));
        }
        let root_key = test_key(99);
        let root = RootManifest {
            version: 1,
            created_at: 0,
            expires_at: u64::MAX,
            root: RoleDelegation::single(&root_key.verifying_key()),
            repository: RoleDelegation {
                threshold: 1,
                keys: BTreeMap::new(),
            },
            timestamp: RoleDelegation {
                threshold: 1,
                keys: BTreeMap::new(),
            },
            reviewer: RoleDelegation {
                threshold: 1,
                keys: reviewers,
            },
        };
        TrustStore::in_memory(root, RoleDelegation::single(&test_key(99).verifying_key()))
    }

    fn attest(entry: &RecipeManifestEntry, key: &SigningKey) -> Attestation {
        let msg = attestation_message(entry);
        Attestation {
            key_fingerprint: key_fingerprint(&key.verifying_key()),
            sig: hex_encode(&key.sign(&msg).to_bytes()),
        }
    }

    fn xz_entry(critical: bool, committer: Option<String>) -> RecipeManifestEntry {
        RecipeManifestEntry {
            name: "xz".into(),
            version: "5.6.1".into(),
            recipe_commit: "a1b2c3".into(),
            committer_fingerprint: committer,
            sources: vec![Source {
                url:
                    "https://github.com/tukaani-project/xz/releases/download/v5.6.1/xz-5.6.1.tar.xz"
                        .into(),
                sha256: "deadbeef".into(),
                filename: None,
            }],
            attestations: vec![],
            critical,
        }
    }

    #[test]
    fn parses_design_doc_toml_shape() {
        let toml_src = r#"
[[package]]
name = "xz"
version = "5.6.1"
recipe_commit = "a1b2c3"
critical = true

[[package.source]]
url = "https://github.com/tukaani-project/xz/releases/download/v5.6.1/xz-5.6.1.tar.xz"
sha256 = "abc"

[[package.attestation]]
key_fingerprint = "maintainer-1"
sig = "00"

[[package.attestation]]
key_fingerprint = "maintainer-2"
sig = "11"
"#;
        let m = RecipeManifest::from_toml_str(toml_src).unwrap();
        let e = &m.packages[0];
        assert!(e.critical);
        assert_eq!(e.sources.len(), 1);
        assert_eq!(e.attestations.len(), 2);
        assert_eq!(e.attestations[0].key_fingerprint, "maintainer-1");
        assert_eq!(m.find("xz", "5.6.1").unwrap().recipe_commit, "a1b2c3");
        assert!(m.find("xz", "9.9").is_none());
    }

    #[test]
    fn critical_requires_two_distinct_reviewers_excluding_committer() {
        let committer = test_key(1);
        let rev1 = test_key(2);
        let rev2 = test_key(3);
        let store = store_with_reviewers(&[&committer, &rev1, &rev2]);
        let committer_fp = key_fingerprint(&committer.verifying_key());

        // committer + one reviewer: NOT enough for critical.
        let mut e = xz_entry(true, Some(committer_fp.clone()));
        e.attestations = vec![attest(&e, &committer), attest(&e, &rev1)];
        let err = verify_attestations(&e, &store).unwrap_err();
        assert!(
            err.to_string().contains("insufficient attestations"),
            "{err}"
        );

        // committer + two reviewers: accepted; counts all three.
        e.attestations = vec![attest(&e, &committer), attest(&e, &rev1), attest(&e, &rev2)];
        assert_eq!(verify_attestations(&e, &store).unwrap(), 3);

        // Duplicate attestation from the same key counts once.
        e.attestations = vec![
            attest(&e, &rev1),
            attest(&e, &rev1),
            attest(&e, &rev2),
            attest(&e, &committer),
        ];
        assert_eq!(verify_attestations(&e, &store).unwrap(), 3);
    }

    #[test]
    fn critical_without_committer_record_needs_two_distinct() {
        let rev1 = test_key(2);
        let rev2 = test_key(3);
        let store = store_with_reviewers(&[&rev1, &rev2]);
        let mut e = xz_entry(true, None);
        e.attestations = vec![attest(&e, &rev1)];
        assert!(verify_attestations(&e, &store).is_err());
        e.attestations.push(attest(&e, &rev2));
        assert_eq!(verify_attestations(&e, &store).unwrap(), 2);
    }

    #[test]
    fn non_critical_needs_one_reviewer() {
        let rev1 = test_key(2);
        let store = store_with_reviewers(&[&rev1]);
        let mut e = xz_entry(false, None);
        assert!(verify_attestations(&e, &store).is_err());
        e.attestations = vec![attest(&e, &rev1)];
        assert_eq!(verify_attestations(&e, &store).unwrap(), 1);
    }

    #[test]
    fn forged_signature_is_fail_loud_not_skipped() {
        let rev1 = test_key(2);
        let rev2 = test_key(3);
        let store = store_with_reviewers(&[&rev1, &rev2]);
        let mut e = xz_entry(true, None);
        let mut bad = attest(&e, &rev1);
        bad.sig = hex_encode(&[0xEE; 64]); // right key id, forged bytes
        e.attestations = vec![bad, attest(&e, &rev2)];
        let err = verify_attestations(&e, &store).unwrap_err();
        assert!(
            err.to_string().contains("invalid attestation signature"),
            "{err}"
        );
    }

    #[test]
    fn non_reviewer_keys_do_not_count() {
        let outsider = test_key(50);
        let rev1 = test_key(2);
        let store = store_with_reviewers(&[&rev1]);
        let mut e = xz_entry(true, None);
        e.attestations = vec![attest(&e, &outsider), attest(&e, &rev1)];
        // outsider ignored, only 1 distinct reviewer → insufficient.
        assert!(verify_attestations(&e, &store).is_err());
    }

    #[test]
    fn message_is_source_order_independent() {
        let mut a = xz_entry(false, None);
        a.sources.push(Source {
            url: "https://b.example/b.tar".into(),
            sha256: "22".into(),
            filename: None,
        });
        let mut b = a.clone();
        b.sources.reverse();
        assert_eq!(attestation_message(&a), attestation_message(&b));
    }

    #[test]
    fn local_entry_resolves_paths_and_keeps_remote_sources() {
        let dir = std::env::temp_dir().join(format!("pkg-local-entry-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("payload.txt"), b"hello").unwrap();
        std::fs::write(
            dir.join("source.toml"),
            r#"[[source]]
url = "./payload.txt"
sha256 = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
filename = "renamed.txt"

[[source]]
url = "https://example.com/remote.tar.gz"
sha256 = "aa"
"#,
        )
        .unwrap();
        let manifest = PackageManifest::from_toml_str(
            "name = \"local-app\"\nversion = \"1.0\"\ncategory = \"app-misc\"\n",
        )
        .unwrap();
        let entry = local_manifest_entry(&manifest, &dir).unwrap();
        assert_eq!(entry.name, "local-app");
        assert_eq!(entry.recipe_commit.len(), 12);
        assert!(entry.attestations.is_empty());
        assert_eq!(entry.sources[0].filename.as_deref(), Some("renamed.txt"));
        assert_eq!(
            entry.sources[0].url,
            format!("file://{}", dir.join("payload.txt").display())
        );
        assert_eq!(entry.sources[1].url, "https://example.com/remote.tar.gz");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn embedded_sources_work_and_conflict_with_legacy_source_file() {
        let dir = std::env::temp_dir().join(format!("pkg-embedded-source-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("pkg-meta.toml"),
            r#"name = "local-app"
version = "1"
category = "app-misc"
[[source]]
url = "https://example.com/app.tar.gz"
sha256 = "abc"
"#,
        )
        .unwrap();
        assert_eq!(read_source_pins(&dir).unwrap().len(), 1);
        std::fs::write(
            dir.join("source.toml"),
            "[[source]]\nurl = \"https://example.com/other.tar.gz\"\nsha256 = \"def\"\n",
        )
        .unwrap();
        let err = read_source_pins(&dir).unwrap_err();
        assert!(err
            .to_string()
            .contains("both pkg-meta.toml and source.toml"));
        std::fs::remove_dir_all(dir).unwrap();
    }
}
