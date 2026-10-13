//! ports2repo — turn a ports tree into a complete, signed repository.
//!
//! Dev/pre-bootstrap tooling (an `examples/` binary, never installed).
//! Walks one or more ports trees (`<tree>/<category>/<pkg>/` holding
//! `pkg-meta.toml`, phase scripts, and `source.toml`), verifies the
//! pinned distfiles, and writes into the output directory:
//!
//! - `root.json`, `index.json`, `security.json`, `recipe-manifest.toml`
//!   — the signed repo documents (same format as `mkrepo`),
//! - `sources/<file>` — the distfiles the repo serves,
//! - `recipes/<name>/` — the recipe tree ready to copy into
//!   `$PKG_ROOT/var/lib/pkg/recipes/` (with `.recipe-commit` filled in;
//!   see `ports/install.sh`).
//!
//! The recipe commit is derived from the recipe's own contents
//! (truncated SHA-256 over the sorted file set), so editing a recipe
//! changes its commit and the provenance chain notices.
//!
//! WARNING: keys are the same deterministic test seeds `mkrepo` uses —
//! fine for a localhost pre-bootstrap repo, NOT for distribution.
//!
//! Usage:
//!
//! ```text
//! ports2repo <repo-dir> <base-url> --distfiles <dir> <tree>... [--index-version <n>]
//! ```

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use ed25519_dalek::{Signer, SigningKey};
use pkg_core::manifest::PackageManifest;
use pkg_repo::recipe_manifest::{
    attestation_message, Attestation, RecipeManifest, RecipeManifestEntry, Source,
};
use pkg_repo::trust::{
    canonical_bytes, hex_encode, key_fingerprint, sha256_hex, RoleDelegation, RootManifest,
    SignatureEntry, SignedDocument,
};

#[derive(serde::Deserialize, Default)]
struct SourceToml {
    #[serde(default)]
    source: Vec<SourcePin>,
}

#[derive(serde::Deserialize)]
struct SourcePin {
    url: String,
    sha256: String,
    /// Staged filename (default: URL basename). Lets a recipe rename
    /// ambiguous upstream names like GitHub's `v1.2.3.tar.gz`.
    filename: Option<String>,
}

fn test_key(seed: u8) -> SigningKey {
    SigningKey::from_bytes(&[seed; 32])
}

fn delegation_of(keys: &[&SigningKey], threshold: u32) -> RoleDelegation {
    let mut map = BTreeMap::new();
    for k in keys {
        let vk = k.verifying_key();
        map.insert(key_fingerprint(&vk), hex_encode(vk.as_bytes()));
    }
    RoleDelegation { threshold, keys: map }
}

fn sign_doc(payload: serde_json::Value, signers: &[&SigningKey]) -> Vec<u8> {
    let msg = canonical_bytes(&payload).unwrap();
    let signatures = signers
        .iter()
        .map(|k| {
            let vk = k.verifying_key();
            SignatureEntry {
                key_fingerprint: key_fingerprint(&vk),
                sig: hex_encode(&k.sign(&msg).to_bytes()),
            }
        })
        .collect();
    serde_json::to_vec(&SignedDocument { signed: payload, signatures }).unwrap()
}

/// The recipe files that make up a package's identity, in fixed order.
const RECIPE_FILES: [&str; 6] =
    ["pkg-meta.toml", "source.toml", "prepare", "build", "check", "install"];

/// Content-derived recipe commit: first 12 hex of the SHA-256 over the
/// sorted (name, bytes) pairs of the recipe files present.
fn recipe_commit(dir: &Path) -> String {
    let mut hasher = sha2::Sha256::default();
    use sha2::Digest;
    for f in RECIPE_FILES {
        let path = dir.join(f);
        if let Ok(bytes) = std::fs::read(&path) {
            hasher.update(f.as_bytes());
            hasher.update(&bytes);
        }
    }
    hex_encode(&hasher.finalize())[..12].to_string()
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() < 4 {
        eprintln!(
            "usage: ports2repo <repo-dir> <base-url> --distfiles <dir> <tree>... [--index-version <n>]"
        );
        std::process::exit(2);
    }
    let repo_dir = PathBuf::from(&args[0]);
    let base_url = args[1].trim_end_matches('/').to_string();
    let mut distfiles: Option<PathBuf> = None;
    let mut trees: Vec<PathBuf> = Vec::new();
    let mut index_version: u64 = 1;
    let mut it = args[2..].iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--distfiles" => distfiles = it.next().map(PathBuf::from),
            "--index-version" => {
                index_version = it
                    .next()
                    .and_then(|v| v.parse().ok())
                    .expect("--index-version wants a number")
            }
            _ => trees.push(PathBuf::from(a)),
        }
    }
    let distfiles = distfiles.expect("--distfiles <dir> is required");

    // Collect <tree>/<category>/<pkg>/ directories.
    let mut pkg_dirs: Vec<PathBuf> = Vec::new();
    for tree in &trees {
        for cat in std::fs::read_dir(tree).expect("read tree") {
            let cat = cat.unwrap().path();
            if !cat.is_dir() {
                continue;
            }
            for pkg in std::fs::read_dir(&cat).expect("read category") {
                let pkg = pkg.unwrap().path();
                if pkg.join("pkg-meta.toml").is_file() {
                    pkg_dirs.push(pkg);
                }
            }
        }
    }
    pkg_dirs.sort();
    assert!(!pkg_dirs.is_empty(), "no packages found in the given trees");

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();

    let root_keys = [test_key(1), test_key(2), test_key(3)];
    let repo_key = test_key(10);
    let ts_key = test_key(11);
    let reviewer_keys = [test_key(12), test_key(13), test_key(14)];
    let committer = test_key(13);
    let attesters = [test_key(12), test_key(14)];

    std::fs::create_dir_all(repo_dir.join("sources")).unwrap();
    std::fs::create_dir_all(repo_dir.join("recipes")).unwrap();

    let mut manifest_entries: Vec<RecipeManifestEntry> = Vec::new();
    let mut index_pkgs: Vec<serde_json::Value> = Vec::new();

    for dir in &pkg_dirs {
        let meta_text = std::fs::read_to_string(dir.join("pkg-meta.toml")).unwrap();
        let meta = PackageManifest::from_toml_str(&meta_text)
            .unwrap_or_else(|e| panic!("{}: {e}", dir.display()));
        let srcs: SourceToml = match std::fs::read_to_string(dir.join("source.toml")) {
            Ok(text) => toml::from_str(&text)
                .unwrap_or_else(|e| panic!("{}: source.toml: {e}", dir.display())),
            Err(_) => SourceToml::default(),
        };
        let commit = recipe_commit(dir);

        // Verify + stage the pinned distfiles.
        let mut sources = Vec::new();
        for pin in &srcs.source {
            let file_name = pin
                .filename
                .clone()
                .unwrap_or_else(|| pin.url.rsplit('/').next().unwrap().to_string());
            let dist = distfiles.join(&file_name);
            let bytes = std::fs::read(&dist).unwrap_or_else(|e| {
                panic!("{}: distfile {file_name}: {e} (run ports/fetch-distfiles.sh)", dir.display())
            });
            let got = sha256_hex(&bytes);
            if got != pin.sha256 {
                panic!(
                    "{}: distfile {file_name} sha256 mismatch: tree pins {}, file has {got}",
                    dir.display(),
                    pin.sha256
                );
            }
            std::fs::write(repo_dir.join("sources").join(&file_name), &bytes).unwrap();
            sources.push(Source {
                url: format!("{base_url}/sources/{file_name}"),
                sha256: pin.sha256.clone(),
            });
        }

        let mut entry = RecipeManifestEntry {
            name: meta.name.clone(),
            version: meta.version.to_string(),
            recipe_commit: commit.clone(),
            committer_fingerprint: Some(key_fingerprint(&committer.verifying_key())),
            sources,
            attestations: Vec::new(),
            critical: meta.critical,
        };
        let msg = attestation_message(&entry);
        entry.attestations = attesters
            .iter()
            .map(|k| Attestation {
                key_fingerprint: key_fingerprint(&k.verifying_key()),
                sig: hex_encode(&k.sign(&msg).to_bytes()),
            })
            .collect();
        manifest_entries.push(entry);
        index_pkgs.push(serde_json::json!({
            "name": meta.name,
            "version": meta.version.to_string(),
            "recipe_commit": commit,
            "critical": meta.critical,
        }));

        // Ready-to-install recipe tree (recipes/<name>/ with the commit
        // filled in) — ports/install.sh copies this into a root.
        let out = repo_dir.join("recipes").join(&meta.name);
        std::fs::create_dir_all(&out).unwrap();
        for f in RECIPE_FILES {
            let src = dir.join(f);
            if src.is_file() {
                std::fs::copy(&src, out.join(f)).unwrap();
            }
        }
        std::fs::write(out.join(".recipe-commit"), &commit).unwrap();
    }

    std::fs::write(
        repo_dir.join("recipe-manifest.toml"),
        toml::to_string(&RecipeManifest { packages: manifest_entries }).unwrap(),
    )
    .unwrap();

    let index_payload = serde_json::json!({
        "format_version": 1,
        "index_version": index_version,
        "generated_at": now,
        "packages": index_pkgs,
    });
    let index_env: SignedDocument<serde_json::Value> =
        serde_json::from_slice(&sign_doc(index_payload, &[&repo_key])).unwrap();
    let ts_payload = serde_json::json!({
        "index_version": index_version,
        "index_sha256": sha256_hex(&canonical_bytes(&index_env.signed).unwrap()),
        "issued_at": now,
        "expires_at": now + 24 * 3600,
    });
    let ts_env: SignedDocument<serde_json::Value> =
        serde_json::from_slice(&sign_doc(ts_payload, &[&ts_key])).unwrap();
    std::fs::write(
        repo_dir.join("index.json"),
        serde_json::to_vec(&serde_json::json!({"index": index_env, "timestamp": ts_env})).unwrap(),
    )
    .unwrap();

    std::fs::write(
        repo_dir.join("security.json"),
        sign_doc(serde_json::json!({"format_version": 1, "advisories": []}), &[&repo_key]),
    )
    .unwrap();

    let root = RootManifest {
        version: 1,
        created_at: now,
        expires_at: now + 365 * 24 * 3600,
        root: delegation_of(&[&root_keys[0], &root_keys[1], &root_keys[2]], 2),
        repository: delegation_of(&[&repo_key], 1),
        timestamp: delegation_of(&[&ts_key], 1),
        reviewer: delegation_of(
            &[&reviewer_keys[0], &reviewer_keys[1], &reviewer_keys[2]],
            1,
        ),
    };
    std::fs::write(
        repo_dir.join("root.json"),
        sign_doc(serde_json::to_value(&root).unwrap(), &[&root_keys[0], &root_keys[1]]),
    )
    .unwrap();

    println!(
        "repo written to {} ({} packages; recipes under {}/recipes)",
        repo_dir.display(),
        pkg_dirs.len(),
        repo_dir.display()
    );
    println!("WARNING: deterministic test keys — localhost/pre-bootstrap use only");
}
