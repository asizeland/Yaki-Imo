//! mkrepo — fabricate a complete, validly-signed test repository.
//!
//! Dev/e2e tooling only (an `examples/` binary, never installed). It
//! generates a deterministic Ed25519 key set (fixed seeds — NOT for
//! production), then writes into the output directory:
//!
//! - `root.json` — a signed root manifest (import with
//!   `pkg key trust-root`).
//! - `index.json` — the signed index bundle (`{index, timestamp}`)
//!   that `pkg sync` verifies.
//! - `recipe-manifest.toml` — source declarations with real SHA-256s
//!   and reviewer attestations.
//! - `security.json` — a signed advisory index (empty unless
//!   `--advisory` is given).
//!
//! Usage:
//!
//! ```text
//! mkrepo <repo-dir> <base-url> <pkg>... [--advisory <json-file>]
//! pkg := name:version:commit:critical:src1,src2,...[:noattest]
//! ```
//!
//! Source files are copied into `<repo-dir>/sources/` and their URLs
//! recorded as `<base-url>/sources/<file>`. A package spec ending in
//! `:noattest` gets NO reviewer attestations — for exercising the
//! supply-chain gate (an under-attested critical package must block
//! the next transaction until acknowledged).

use std::collections::BTreeMap;
use std::path::PathBuf;

use ed25519_dalek::{Signer, SigningKey};
use pkg_repo::recipe_manifest::{
    attestation_message, Attestation, RecipeManifest, RecipeManifestEntry, Source,
};
use pkg_repo::trust::{
    canonical_bytes, hex_encode, key_fingerprint, sha256_hex, RoleDelegation, RootManifest,
    SignatureEntry, SignedDocument,
};

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

struct PkgSpec {
    name: String,
    version: String,
    commit: String,
    critical: bool,
    sources: Vec<PathBuf>,
    attest: bool,
}

fn parse_pkg(arg: &str) -> PkgSpec {
    let parts: Vec<&str> = arg.split(':').collect();
    let (fields, attest) = match parts.as_slice() {
        [rest @ .., "noattest"] => (rest, false),
        _ => (parts.as_slice(), true),
    };
    if fields.len() != 5 {
        eprintln!(
            "bad package spec {arg:?} — want name:version:commit:critical:src1,src2[:noattest]"
        );
        std::process::exit(2);
    }
    PkgSpec {
        name: fields[0].to_string(),
        version: fields[1].to_string(),
        commit: fields[2].to_string(),
        critical: fields[3] == "true",
        sources: fields[4]
            .split(',')
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .collect(),
        attest,
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() < 3 {
        eprintln!(
            "usage: mkrepo <repo-dir> <base-url> <pkg>... [--advisory <json-file>]\n\
             pkg := name:version:commit:critical:src1,src2,..."
        );
        std::process::exit(2);
    }
    let repo_dir = PathBuf::from(&args[0]);
    let base_url = args[1].trim_end_matches('/').to_string();
    let mut pkgs = Vec::new();
    let mut advisory_file: Option<PathBuf> = None;
    let mut it = args[2..].iter();
    while let Some(a) = it.next() {
        if a == "--advisory" {
            advisory_file = it.next().map(PathBuf::from);
        } else {
            pkgs.push(parse_pkg(a));
        }
    }

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();

    let root_keys = [test_key(1), test_key(2), test_key(3)];
    let repo_key = test_key(10);
    let ts_key = test_key(11);
    // Reviewer pool: 12, 13, 14. Entries are "committed" by 13 and
    // attested by 12 and 14, so a properly attested critical package
    // has exactly the required 2 INDEPENDENT reviewer sign-offs (the
    // committer's own attestation never counts, v14 §4/T3).
    let reviewer_keys = [test_key(12), test_key(13), test_key(14)];
    let committer = test_key(13);
    let attesters = [test_key(12), test_key(14)];

    std::fs::create_dir_all(repo_dir.join("sources")).unwrap();

    // recipe-manifest.toml entries (attestations signed by both
    // reviewer keys; committer is reviewer_keys[1]).
    let mut manifest_entries: Vec<RecipeManifestEntry> = Vec::new();
    for p in &pkgs {
        let mut sources = Vec::new();
        for src in &p.sources {
            let bytes = std::fs::read(src).unwrap();
            let file_name = src.file_name().unwrap().to_str().unwrap().to_string();
            std::fs::write(repo_dir.join("sources").join(&file_name), &bytes).unwrap();
            sources.push(Source {
                url: format!("{base_url}/sources/{file_name}"),
                sha256: sha256_hex(&bytes),
            });
        }
        let mut entry = RecipeManifestEntry {
            name: p.name.clone(),
            version: p.version.clone(),
            recipe_commit: p.commit.clone(),
            committer_fingerprint: Some(key_fingerprint(&committer.verifying_key())),
            sources,
            attestations: Vec::new(),
            critical: p.critical,
        };
        if p.attest {
            let msg = attestation_message(&entry);
            entry.attestations = attesters
                .iter()
                .map(|k| Attestation {
                    key_fingerprint: key_fingerprint(&k.verifying_key()),
                    sig: hex_encode(&k.sign(&msg).to_bytes()),
                })
                .collect();
        }
        manifest_entries.push(entry);
    }
    let manifest_toml = toml::to_string(&RecipeManifest {
        packages: manifest_entries,
    })
    .unwrap();
    std::fs::write(repo_dir.join("recipe-manifest.toml"), manifest_toml).unwrap();

    // index.json bundle: repository-signed index + timestamp.
    let packages: Vec<serde_json::Value> = pkgs
        .iter()
        .map(|p| {
            serde_json::json!({
                "name": p.name,
                "version": p.version,
                "recipe_commit": p.commit,
                "critical": p.critical,
            })
        })
        .collect();
    let index_payload = serde_json::json!({
        "format_version": 1,
        "index_version": 1,
        "generated_at": now,
        "packages": packages,
    });
    let index_env: SignedDocument<serde_json::Value> =
        serde_json::from_slice(&sign_doc(index_payload, &[&repo_key])).unwrap();
    let ts_payload = serde_json::json!({
        "index_version": 1,
        "index_sha256": sha256_hex(&canonical_bytes(&index_env.signed).unwrap()),
        "issued_at": now,
        "expires_at": now + 24 * 3600,
    });
    let ts_env: SignedDocument<serde_json::Value> =
        serde_json::from_slice(&sign_doc(ts_payload, &[&ts_key])).unwrap();
    let bundle = serde_json::to_vec(&serde_json::json!({
        "index": index_env,
        "timestamp": ts_env,
    }))
    .unwrap();
    std::fs::write(repo_dir.join("index.json"), &bundle).unwrap();

    // security.json: signed advisory index (from file, or empty).
    let advisory_payload: serde_json::Value = match &advisory_file {
        Some(f) => serde_json::from_slice(&std::fs::read(f).unwrap()).unwrap(),
        None => serde_json::json!({"format_version": 1, "advisories": []}),
    };
    std::fs::write(
        repo_dir.join("security.json"),
        sign_doc(advisory_payload, &[&repo_key]),
    )
    .unwrap();

    // root.json: signed by 2 of 3 root keys (threshold 2).
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
    let root_doc = sign_doc(serde_json::to_value(&root).unwrap(), &[&root_keys[0], &root_keys[1]]);
    std::fs::write(repo_dir.join("root.json"), &root_doc).unwrap();

    println!("repo written to {}", repo_dir.display());
    println!("root fingerprint: {}", key_fingerprint(&root_keys[0].verifying_key()));
}
