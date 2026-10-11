//! ## v1 (initial)
//!
//! TUF-inspired verification of the signed repository index
//! (v11 §10.5, v12 §2, v14 §4). Three-role chain from v11 plus the
//! v14 fourth role:
//!
//! ```text
//! root (offline, threshold, embedded in the pkg binary)
//!   └─ delegates ─► repository role (signs packages.json / security.json)
//!   └─ delegates ─► timestamp role (signs the freshness pointer doc)
//!   └─ delegates ─► reviewer role (v14 §4: recipe-review attestations)
//! ```
//!
//! # Review findings mitigated here (pkg-review.md Part 2 #12; reviewer
//! # annexes M4/M5)
//!
//! - **M4 — trust-chain bootstrap origin unspecified.** Resolved: the
//!   initial trust anchor is the *embedded root key set* compiled into
//!   the (musl-static) `pkg` binary, passed to [`TrustStore::load`] as
//!   `embedded_root`. With no on-disk state the embedded set is trusted
//!   directly (first `pkg bootstrap` / `pkg sync`, v11 §10.5.2); with
//!   on-disk state, the persisted root manifest must verify against the
//!   persisted *or* embedded root signers and carry a `version` no lower
//!   than the persisted counter. Out-of-band recovery (`pkg key
//!   trust-root <file>`) goes through [`TrustStore::apply_root_manifest`]
//!   like any other rotation — the CLI layer adds the interactive
//!   confirmation (v11 §10.5.4).
//! - **M5a — no rollback protection (version counters).** Both the root
//!   manifest and the index carry monotonic version counters.
//!   `apply_root_manifest` requires a *strictly greater* root `version`;
//!   `verify_index_at` rejects any `index_version` below the last
//!   accepted one (persisted in `state.json` by [`TrustStore::record_index_version`],
//!   which sync calls only after the new index is safely cached). A
//!   mirror serving an old-but-validly-signed index is rejected.
//! - **M5b — the online timestamp role is a single point of failure with
//!   no grace period.** A timestamp signer outage must not brick
//!   installs, but a freeze attack must not pass silently either.
//!   Resolved with a two-tier expiry: the timestamp document expires
//!   after [`TIMESTAMP_EXPIRY_SECS`] (24 h, the re-signing cadence);
//!   between expiry and expiry + [`TIMESTAMP_GRACE_SECS`] (72 h) the
//!   index is accepted with a loud [`TrustWarning::TimestampStaleGrace`]
//!   returned to the caller (never swallowed — `VerifiedIndex.warnings`);
//!   beyond the grace window verification **fails**. An operator who has
//!   independently confirmed a timestamp-infrastructure outage can pass
//!   [`VerifyPolicy::ignore_stale`] (the `--ignore-stale` CLI flag),
//!   which downgrades even the post-grace failure to a recorded warning.
//!   This is deliberately worse-UX-than-silent-accept: a stale timestamp
//!   is exactly the signal of a freeze attack (T6), so the safe default
//!   is noisy degradation, and the override is explicit.
//!
//! # Local revocations vs. signed-state mutation
//!
//! The trusted root manifest is a *signed document*; editing it locally
//! would invalidate its signatures. Local administrative revocation
//! (`pkg key revoke`, [`TrustStore::revoke_key`]) therefore records
//! fingerprints in `state.json`'s `local_revocations` list, and every
//! verification path filters them out of the effective delegation.
//! Authoritative revocation still arrives via root re-delegation on the
//! next `pkg sync` (v11 §10.5.3); local revocation is defense in depth
//! for "I don't trust this key *right now*".
//!
//! # Canonical signing bytes
//!
//! Signatures cover the canonical JSON serialization of the `signed`
//! payload: the payload is parsed to `serde_json::Value` and
//! re-serialized with `serde_json::to_vec`. serde_json's default `Map`
//! is `BTreeMap`-backed (the `preserve_order` feature is *not*
//! enabled), so map keys serialize in sorted order and the encoding is
//! deterministic — signers and verifiers agree on the byte string
//! without a canonicalization spec of our own. Anything verifying these
//! documents outside this crate must reproduce exactly this: sorted
//! keys, no insignificant whitespace, UTF-8.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use ed25519_dalek::{Signature, VerifyingKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use pkg_core::{PkgError, PkgResult};

/// Default on-disk trust store location (v11 §10.5).
pub const TRUST_DIR: &str = "/var/lib/pkg/trust";

/// Timestamp-document lifetime: the timestamp role re-signs every 24 h
/// (v11 §10.5.1), so a document older than this is stale.
pub const TIMESTAMP_EXPIRY_SECS: u64 = 24 * 60 * 60;

/// Grace window after timestamp expiry (M5b mitigation): stale-but-
/// validly-signed timestamps are accepted with a warning for 72 h, then
/// rejected. Chosen to cover a long-weekend infrastructure outage
/// without opening an indefinite freeze-attack window.
pub const TIMESTAMP_GRACE_SECS: u64 = 72 * 60 * 60;

// ---------------------------------------------------------------------
// Small utilities
// ---------------------------------------------------------------------

/// Lowercase hex encoding. Fingerprints, signatures, and hashes are all
/// hex in the JSON documents; a shared helper keeps the encoding
/// consistent (and avoids a `hex` crate the SPEC does not pin).
pub fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

/// Decode lowercase/uppercase hex; `Err` on odd length or non-hex digit.
pub fn hex_decode(s: &str) -> Result<Vec<u8>, String> {
    let s = s.trim();
    if s.len() % 2 != 0 {
        return Err(format!("odd-length hex string ({} chars)", s.len()));
    }
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len() / 2);
    for pair in bytes.chunks_exact(2) {
        let hi = hex_nibble(pair[0]).ok_or_else(|| format!("invalid hex byte '{}'", s))?;
        let lo = hex_nibble(pair[1]).ok_or_else(|| format!("invalid hex byte '{}'", s))?;
        out.push((hi << 4) | lo);
    }
    Ok(out)
}

fn hex_nibble(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

/// SHA-256 of `bytes` as lowercase hex.
pub fn sha256_hex(bytes: &[u8]) -> String {
    hex_encode(&Sha256::digest(bytes))
}

/// Key fingerprints are the first 32 hex chars (16 bytes) of the
/// SHA-256 of the raw Ed25519 public key. 16 bytes keeps fingerprints
/// short enough to display while making second-preimage collisions
/// across a handful of maintainer keys a non-concern.
pub fn key_fingerprint(key: &VerifyingKey) -> String {
    let full = sha256_hex(key.as_bytes());
    full[..32].to_string()
}

/// Current time as seconds since the Unix epoch.
pub fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// ---------------------------------------------------------------------
// Roles and delegations
// ---------------------------------------------------------------------

/// The four trust roles. Root delegates to the other three; the
/// reviewer role (v14 §4) signs recipe-review attestations and never
/// signs indices.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Role {
    Root,
    Repository,
    Timestamp,
    Reviewer,
}

impl Role {
    pub fn as_str(&self) -> &'static str {
        match self {
            Role::Root => "root",
            Role::Repository => "repository",
            Role::Timestamp => "timestamp",
            Role::Reviewer => "reviewer",
        }
    }
}

impl fmt::Display for Role {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A role's key set plus the threshold of distinct keys required to
/// authorize a document for that role. `keys` maps
/// fingerprint → hex-encoded Ed25519 public key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoleDelegation {
    pub threshold: u32,
    #[serde(default)]
    pub keys: BTreeMap<String, String>,
}

/// Default for the `#[serde(default)]` reviewer field on pre-v14 root
/// manifests: an empty key set with threshold 1 — a delegation that
/// exists but authorizes nothing. (A derived `Default` would give
/// threshold 0, which under "count >= threshold" verification would let
/// *zero* signatures satisfy the role.)
impl Default for RoleDelegation {
    fn default() -> Self {
        RoleDelegation { threshold: 1, keys: BTreeMap::new() }
    }
}

impl RoleDelegation {
    pub fn single(key: &VerifyingKey) -> Self {
        let mut keys = BTreeMap::new();
        keys.insert(key_fingerprint(key), hex_encode(key.as_bytes()));
        RoleDelegation { threshold: 1, keys }
    }

    /// Decode one member key. Unknown fingerprints yield `None`.
    pub fn verifying_key(&self, fingerprint: &str) -> Option<VerifyingKey> {
        let hex = self.keys.get(fingerprint)?;
        let bytes = hex_decode(hex).ok()?;
        let arr: [u8; 32] = bytes.try_into().ok()?;
        VerifyingKey::from_bytes(&arr).ok()
    }

    /// Verify `signatures` over `msg` against this delegation. Returns
    /// the number of *distinct member keys* with valid signatures.
    ///
    /// A signature entry that names a member key but fails verification
    /// is a hard error ([`TrustError::BadSignature`]) — never silently
    /// skipped, because a bad signature over the right bytes is an
    /// attack signal, not a partial-credit situation. Entries naming
    /// non-member keys are ignored for counting (they may belong to a
    /// newer delegation the client doesn't know yet).
    pub fn verify_threshold(
        &self,
        msg: &[u8],
        signatures: &[SignatureEntry],
    ) -> Result<usize, TrustError> {
        let mut valid: BTreeSet<&str> = BTreeSet::new();
        for entry in signatures {
            let Some(key) = self.verifying_key(&entry.key_fingerprint) else {
                continue; // not a member of this delegation
            };
            let sig_bytes = hex_decode(&entry.sig)
                .map_err(|e| TrustError::Malformed(format!("bad signature hex: {e}")))?;
            let arr: [u8; 64] = sig_bytes
                .try_into()
                .map_err(|_| TrustError::Malformed("signature is not 64 bytes".into()))?;
            let sig = Signature::from_bytes(&arr);
            // verify_strict (not plain verify): rejects non-canonical /
            // small-order encodings, closing the malleability class.
            key.verify_strict(msg, &sig)
                .map_err(|_| TrustError::BadSignature {
                    fingerprint: entry.key_fingerprint.clone(),
                })?;
            valid.insert(entry.key_fingerprint.as_str());
        }
        let have = valid.len();
        if (have as u32) < self.threshold {
            return Err(TrustError::InsufficientSignatures {
                have,
                need: self.threshold,
            });
        }
        Ok(have)
    }
}

// ---------------------------------------------------------------------
// Signed-document envelope
// ---------------------------------------------------------------------

/// One signature on a document: who claims to have signed, and the
/// hex-encoded Ed25519 signature.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignatureEntry {
    pub key_fingerprint: String,
    pub sig: String,
}

/// TUF-style envelope: the payload plus the signatures over its
/// canonical serialization (see module docs).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignedDocument<P> {
    pub signed: P,
    pub signatures: Vec<SignatureEntry>,
}

/// The canonical byte string that signatures cover for a raw JSON
/// `signed` value: sorted-key, whitespace-free re-serialization.
/// Public so repository tooling (index-signing CI, test fixtures) can
/// reproduce the exact signed bytes.
pub fn canonical_bytes(value: &serde_json::Value) -> Result<Vec<u8>, TrustError> {
    serde_json::to_vec(value)
        .map_err(|e| TrustError::Malformed(format!("cannot canonicalize payload: {e}")))
}

// ---------------------------------------------------------------------
// Documents
// ---------------------------------------------------------------------

/// The root manifest: the root key set itself plus the delegations to
/// the repository, timestamp, and reviewer roles. Signed by a threshold
/// of the *currently trusted* root keys (v11 §10.5.2/§10.5.3).
///
/// `version` is the monotonic counter closing review finding M5a:
/// a replacement root manifest must have a strictly greater version
/// than the one it replaces, so an attacker with an old (leaked but
/// once-valid) root signature set cannot roll the trust store back to
/// a delegation they control.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RootManifest {
    pub version: u64,
    pub created_at: u64,
    pub expires_at: u64,
    pub root: RoleDelegation,
    pub repository: RoleDelegation,
    pub timestamp: RoleDelegation,
    /// v14 §4's fourth role: maintainer keys allowed to attest recipe
    /// reviews. Consumed by `recipe_manifest.rs::verify_attestations`.
    #[serde(default)]
    pub reviewer: RoleDelegation,
}

/// The timestamp role's pointer document (v11 §10.5.1): binds the
/// current index version to the SHA-256 of the index payload's
/// canonical bytes, with an explicit expiry. This is what makes a
/// freeze attack (serving an old, validly-signed index forever)
/// detectable.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TimestampDoc {
    pub index_version: u64,
    pub index_sha256: String,
    pub issued_at: u64,
    pub expires_at: u64,
}

/// One package entry in the signed index (v12 §2): the recipe for
/// `name@version` lives at exactly `recipe_commit` in the recipe repo.
/// Source hashes and attestations for the entry live in the recipe
/// manifest (`recipe_manifest.rs`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexPackage {
    pub name: String,
    pub version: String,
    pub recipe_commit: String,
    /// v14 §4 critical-set marker (bootstrap path, toolchain, `pkg`
    /// itself — target <50 packages). Critical entries require ≥2
    /// distinct reviewer attestations.
    #[serde(default)]
    pub critical: bool,
}

/// The signed index payload (`packages.json`'s `signed` object).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexPayload {
    pub format_version: u32,
    pub index_version: u64,
    pub generated_at: u64,
    #[serde(default)]
    pub packages: Vec<IndexPackage>,
}

/// What a mirror serves as `index.json`: the repository-signed index
/// envelope and the timestamp-signed pointer envelope, bundled so the
/// client always verifies them together (a mirror that could serve the
/// index without the current timestamp would re-open the freeze hole).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexBundle {
    pub index: SignedDocument<serde_json::Value>,
    pub timestamp: SignedDocument<serde_json::Value>,
}

// ---------------------------------------------------------------------
// Errors, warnings, policy
// ---------------------------------------------------------------------

/// Trust-chain verification failures. Richer than `PkgError`'s variants
/// need to be; converts into `PkgError::Parse` at the workspace
/// boundary with a `trust:` prefix so messages stay greppable. (A
/// dedicated `PkgError::Trust` variant would be the cleaner end-state,
/// but SPEC freezes error.rs at v3 — "Add NOTHING".)
#[derive(Debug)]
pub enum TrustError {
    /// A document didn't parse or a key/signature wasn't valid hex.
    Malformed(String),
    /// Verification failed for a member key — always fail-loud.
    BadSignature { fingerprint: String },
    /// Fewer distinct valid member-key signatures than the threshold.
    InsufficientSignatures { have: usize, need: u32 },
    /// A version counter moved backwards (M5a).
    Rollback {
        what: &'static str,
        found: u64,
        expected_at_least: u64,
    },
    /// Timestamp expired beyond the 72 h grace window (M5b).
    TimestampExpired { expired_secs_ago: u64 },
    /// Timestamp doc doesn't point at the index it accompanied.
    TimestampBindingMismatch(String),
    /// `pkg key revoke` would break a role's threshold.
    ThresholdViolation(String),
    /// Key or fingerprint not found where one was required.
    UnknownKey(String),
}

impl fmt::Display for TrustError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TrustError::Malformed(m) => write!(f, "malformed trust document: {m}"),
            TrustError::BadSignature { fingerprint } => {
                write!(f, "invalid signature from key {fingerprint}")
            }
            TrustError::InsufficientSignatures { have, need } => {
                write!(f, "insufficient valid signatures ({have} of {need} required)")
            }
            TrustError::Rollback {
                what,
                found,
                expected_at_least,
            } => write!(
                f,
                "rollback refused: {what} version {found} < last accepted {expected_at_least}"
            ),
            TrustError::TimestampExpired { expired_secs_ago } => write!(
                f,
                "timestamp expired {}s ago (beyond the {}s grace window)",
                expired_secs_ago, TIMESTAMP_GRACE_SECS
            ),
            TrustError::TimestampBindingMismatch(m) => {
                write!(f, "timestamp document does not match the index: {m}")
            }
            TrustError::ThresholdViolation(m) => write!(f, "refused: {m}"),
            TrustError::UnknownKey(k) => write!(f, "unknown key: {k}"),
        }
    }
}

impl std::error::Error for TrustError {}

impl From<TrustError> for PkgError {
    fn from(e: TrustError) -> Self {
        PkgError::Parse(format!("trust: {e}"))
    }
}

/// Non-fatal trust conditions, returned (never printed-and-forgotten)
/// on the `VerifiedIndex` so the CLI can render them prominently.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrustWarning {
    /// Timestamp expired but inside the 72 h grace window (M5b).
    TimestampStaleGrace { expired_secs_ago: u64 },
    /// Timestamp expired beyond grace; accepted only because the
    /// operator passed `--ignore-stale` / `VerifyPolicy::ignore_stale`.
    TimestampStaleIgnored { expired_secs_ago: u64 },
    /// The trusted root manifest is past its own expiry — rotation is
    /// overdue; signatures still verify, so this is advisory.
    RootManifestExpired { expired_secs_ago: u64 },
}

impl fmt::Display for TrustWarning {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TrustWarning::TimestampStaleGrace { expired_secs_ago } => write!(
                f,
                "warning: timestamp is {expired_secs_ago}s past expiry (within 72h grace); \
                 run 'pkg sync' — a persistent outage may indicate a freeze attack"
            ),
            TrustWarning::TimestampStaleIgnored { expired_secs_ago } => write!(
                f,
                "warning: timestamp is {expired_secs_ago}s past expiry (BEYOND 72h grace); \
                 accepted only due to --ignore-stale"
            ),
            TrustWarning::RootManifestExpired { expired_secs_ago } => write!(
                f,
                "warning: root manifest expired {expired_secs_ago}s ago; key rotation overdue"
            ),
        }
    }
}

/// Operator-controlled verification relaxations. Default: everything
/// strict. `ignore_stale` backs the `--ignore-stale` CLI flag (M5b).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct VerifyPolicy {
    pub ignore_stale: bool,
}

/// The result of a successful `verify_index`: the authenticated index
/// contents plus any warnings raised along the way.
#[derive(Debug, Clone)]
pub struct VerifiedIndex {
    pub index_version: u64,
    pub generated_at: u64,
    pub packages: Vec<IndexPackage>,
    pub timestamp_expires_at: u64,
    pub warnings: Vec<TrustWarning>,
}

/// Summary of an accepted root re-delegation (`pkg key` rotation).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RootRotationReport {
    pub old_version: u64,
    pub new_version: u64,
    pub added_keys: Vec<String>,
    pub removed_keys: Vec<String>,
}

/// One row of `pkg key list` output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyInfo {
    pub role: Role,
    pub fingerprint: String,
    /// Locally revoked (state.json `local_revocations`); still present
    /// in the signed manifest but filtered out of verification.
    pub revoked: bool,
}

// ---------------------------------------------------------------------
// TrustStore
// ---------------------------------------------------------------------

/// The on-disk state (`state.json`). Integrity of this file relies on
/// the host OS — per the v14 threat model (T5), a compromised host root
/// is out of scope. What we *do* protect here is rollback by a
/// transport attacker: `last_index_version` and `root_version` only
/// ever move forward.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct PersistedState {
    #[serde(default)]
    last_index_version: u64,
    #[serde(default)]
    root_version: u64,
    /// The root delegation trusted to authorize the *next* rotation.
    /// Persisted because rotations chain (v1 signs v2 signs v3…): the
    /// on-disk root manifest is verified against THIS, not against the
    /// embedded keys — after two rotations the embedded keys may be
    /// entirely retired. `None` until the first accepted rotation, in
    /// which case the embedded keys are the signers.
    #[serde(default)]
    root_signers: Option<RoleDelegation>,
    /// Fingerprints removed by local `pkg key revoke`.
    #[serde(default)]
    local_revocations: Vec<String>,
}

/// The verified trust hierarchy, loaded from `/var/lib/pkg/trust/`.
pub struct TrustStore {
    /// Store directory; `None` for in-memory stores (tests, bootstrap).
    dir: Option<PathBuf>,
    /// The currently trusted root manifest (its delegations authorize
    /// the repository/timestamp/reviewer roles).
    root: RootManifest,
    /// The root keys trusted to authorize the *next* root manifest.
    /// Initially the embedded set; after each accepted rotation, the
    /// new manifest's `root` delegation.
    root_signers: RoleDelegation,
    state: PersistedState,
}

impl TrustStore {
    /// Load the trust store from `dir` (usually [`TRUST_DIR`]).
    ///
    /// Bootstrap resolution of review finding M4: with no on-disk
    /// `root.json`, trust the `embedded_root` delegation directly — it
    /// was compiled into the binary and is the actual anchor
    /// (v11 §10.5.2). With on-disk state, verify the persisted
    /// `root.json` against the persisted signer set (rotations chain:
    /// v1 signs v2 signs v3…) or, before the first accepted rotation,
    /// the embedded keys — and require its version to be at least the
    /// persisted counter (an older on-disk manifest than what we've
    /// already accepted is a rollback attempt).
    pub fn load(dir: &Path, embedded_root: &RoleDelegation) -> PkgResult<Self> {
        let root_path = dir.join("root.json");
        let state_path = dir.join("state.json");
        let state: PersistedState = match std::fs::read(&state_path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map_err(|e| PkgError::Parse(format!("trust: state.json: {e}")))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => PersistedState::default(),
            Err(e) => return Err(PkgError::Io(e)),
        };
        if !root_path.exists() {
            // First boot: no fetched root manifest yet. The embedded
            // keys authorize the first fetch (v11 §10.5.2). Nothing is
            // written here — the first successful `apply_root_manifest`
            // persists state.
            let root = RootManifest {
                version: 0,
                created_at: 0,
                expires_at: u64::MAX, // embedded anchor never expires
                root: embedded_root.clone(),
                repository: RoleDelegation { threshold: 1, keys: BTreeMap::new() },
                timestamp: RoleDelegation { threshold: 1, keys: BTreeMap::new() },
                reviewer: RoleDelegation { threshold: 1, keys: BTreeMap::new() },
            };
            return Ok(TrustStore {
                dir: Some(dir.to_path_buf()),
                root,
                root_signers: embedded_root.clone(),
                state,
            });
        }
        let bytes = std::fs::read(&root_path)?;
        let env: SignedDocument<serde_json::Value> = serde_json::from_slice(&bytes)
            .map_err(|e| TrustError::Malformed(format!("root.json: {e}")))?;
        let payload_bytes = canonical_bytes(&env.signed)?;
        // Verify against the persisted signer set if one exists
        // (rotations chain); otherwise against the embedded anchor.
        let signers = state.root_signers.clone().unwrap_or_else(|| embedded_root.clone());
        signers
            .verify_threshold(&payload_bytes, &env.signatures)
            .map_err(|e| TrustError::Malformed(format!("root.json does not verify against the trusted root keys: {e}")))?;
        let manifest: RootManifest = serde_json::from_value(env.signed.clone())
            .map_err(|e| TrustError::Malformed(format!("root.json payload: {e}")))?;
        if manifest.version < state.root_version {
            return Err(TrustError::Rollback {
                what: "root manifest",
                found: manifest.version,
                expected_at_least: state.root_version,
            }
            .into());
        }
        Ok(TrustStore {
            dir: Some(dir.to_path_buf()),
            root_signers: manifest.root.clone(),
            root: manifest,
            state,
        })
    }

    /// Construct a store purely in memory (no persistence) — used by
    /// tests and by `recipe_manifest.rs`'s attestation verification in
    /// contexts where only the delegations matter.
    pub fn in_memory(root: RootManifest, root_signers: RoleDelegation) -> Self {
        TrustStore {
            dir: None,
            root,
            root_signers,
            state: PersistedState::default(),
        }
    }

    /// The effective (revocation-filtered) delegation for a role.
    fn effective_delegation(&self, role: Role) -> RoleDelegation {
        let src = match role {
            Role::Root => &self.root.root,
            Role::Repository => &self.root.repository,
            Role::Timestamp => &self.root.timestamp,
            Role::Reviewer => &self.root.reviewer,
        };
        let mut out = src.clone();
        for fp in &self.state.local_revocations {
            out.keys.remove(fp);
        }
        out
    }

    /// Look up a reviewer-role key by fingerprint (v14 §4), honoring
    /// local revocations. Used by attestation verification.
    pub fn reviewer_key(&self, fingerprint: &str) -> Option<VerifyingKey> {
        if self.state.local_revocations.iter().any(|r| r == fingerprint) {
            return None;
        }
        self.root.reviewer.verifying_key(fingerprint)
    }

    /// The fingerprint recorded as the recipe committer is *excluded*
    /// from reviewer counts by the caller; this accessor just exposes
    /// the raw delegation for `pkg key` display.
    pub fn root_manifest(&self) -> &RootManifest {
        &self.root
    }

    /// Verify a signed document against a role's delegation, returning
    /// the payload and the number of distinct valid member signatures.
    /// Used for non-index documents (`security.json`, `profiles.toml`)
    /// that ride the same chain (v11 §10.5.1).
    pub fn verify_signed<T: serde::de::DeserializeOwned>(
        &self,
        bytes: &[u8],
        role: Role,
    ) -> PkgResult<(T, usize)> {
        let env: SignedDocument<serde_json::Value> = serde_json::from_slice(bytes)
            .map_err(|e| TrustError::Malformed(format!("{} document: {e}", role.as_str())))?;
        let payload_bytes = canonical_bytes(&env.signed)?;
        let delegation = self.effective_delegation(role);
        let n = delegation.verify_threshold(&payload_bytes, &env.signatures)?;
        let payload: T = serde_json::from_value(env.signed.clone())
            .map_err(|e| TrustError::Malformed(format!("{} payload: {e}", role.as_str())))?;
        Ok((payload, n))
    }

    /// Verify a fetched (or cached) index bundle: repository signature,
    /// timestamp signature and binding, rollback protection, and the
    /// 24 h expiry / 72 h grace rules. Uses the system clock and the
    /// strict default policy.
    pub fn verify_index(&self, bytes: &[u8]) -> PkgResult<VerifiedIndex> {
        self.verify_index_at(bytes, unix_now(), VerifyPolicy::default())
    }

    /// `verify_index` with an explicit clock and policy, so tests and
    /// `--ignore-stale` don't need to fake time or fork the logic.
    pub fn verify_index_at(
        &self,
        bytes: &[u8],
        now: u64,
        policy: VerifyPolicy,
    ) -> PkgResult<VerifiedIndex> {
        let bundle: IndexBundle = serde_json::from_slice(bytes)
            .map_err(|e| TrustError::Malformed(format!("index bundle: {e}")))?;

        // 1. Index envelope, repository role threshold.
        let index_bytes = canonical_bytes(&bundle.index.signed)?;
        let repo = self.effective_delegation(Role::Repository);
        repo.verify_threshold(&index_bytes, &bundle.index.signatures)?;
        let payload: IndexPayload = serde_json::from_value(bundle.index.signed.clone())
            .map_err(|e| TrustError::Malformed(format!("index payload: {e}")))?;

        // 2. Timestamp envelope, timestamp role threshold.
        let ts_bytes = canonical_bytes(&bundle.timestamp.signed)?;
        let ts_role = self.effective_delegation(Role::Timestamp);
        ts_role.verify_threshold(&ts_bytes, &bundle.timestamp.signatures)?;
        let tdoc: TimestampDoc = serde_json::from_value(bundle.timestamp.signed.clone())
            .map_err(|e| TrustError::Malformed(format!("timestamp payload: {e}")))?;

        // 3. Binding: the timestamp must point at *this* index, or a
        //    mirror could mix a fresh timestamp with an old index.
        let want_hash = sha256_hex(&index_bytes);
        if tdoc.index_sha256 != want_hash {
            return Err(TrustError::TimestampBindingMismatch(format!(
                "hash {} != expected {}",
                tdoc.index_sha256, want_hash
            ))
            .into());
        }
        if tdoc.index_version != payload.index_version {
            return Err(TrustError::TimestampBindingMismatch(format!(
                "version {} != index's {}",
                tdoc.index_version, payload.index_version
            ))
            .into());
        }

        // 4. Rollback protection (M5a): never accept an index older
        //    than the last one we accepted and recorded.
        if payload.index_version < self.state.last_index_version {
            return Err(TrustError::Rollback {
                what: "index",
                found: payload.index_version,
                expected_at_least: self.state.last_index_version,
            }
            .into());
        }

        // 5. Freshness (M5b): 24 h expiry, 72 h warn-window, then hard
        //    failure unless the operator explicitly overrode.
        let mut warnings = Vec::new();
        if now > tdoc.expires_at {
            let stale_for = now - tdoc.expires_at;
            if stale_for <= TIMESTAMP_GRACE_SECS {
                warnings.push(TrustWarning::TimestampStaleGrace {
                    expired_secs_ago: stale_for,
                });
            } else if policy.ignore_stale {
                warnings.push(TrustWarning::TimestampStaleIgnored {
                    expired_secs_ago: stale_for,
                });
            } else {
                return Err(TrustError::TimestampExpired {
                    expired_secs_ago: stale_for,
                }
                .into());
            }
        }
        if now > self.root.expires_at {
            warnings.push(TrustWarning::RootManifestExpired {
                expired_secs_ago: now - self.root.expires_at,
            });
        }

        Ok(VerifiedIndex {
            index_version: payload.index_version,
            generated_at: payload.generated_at,
            packages: payload.packages,
            timestamp_expires_at: tdoc.expires_at,
            warnings,
        })
    }

    /// Persist acceptance of an index version (monotonic). Called by
    /// `sync.rs` only *after* the new index is safely written to the
    /// cache — recording first would let a crashed sync permanently
    /// reject the very index it failed to cache.
    pub fn record_index_version(&mut self, version: u64) -> PkgResult<()> {
        if version < self.state.last_index_version {
            return Err(TrustError::Rollback {
                what: "index",
                found: version,
                expected_at_least: self.state.last_index_version,
            }
            .into());
        }
        self.state.last_index_version = version;
        self.persist_state()
    }

    /// Accept a root re-delegation document (key rotation, v11 §10.5.3;
    /// also the `pkg key trust-root <file>` recovery path — the CLI
    /// owns the interactive confirmation, v11 §10.5.4).
    ///
    /// The new manifest must (a) verify against the *currently trusted*
    /// root signers at their threshold, and (b) carry a strictly
    /// greater `version` (M5a). On success it becomes the trusted
    /// manifest and its `root` delegation becomes the signer set for
    /// the next rotation.
    pub fn apply_root_manifest(&mut self, bytes: &[u8]) -> PkgResult<RootRotationReport> {
        let env: SignedDocument<serde_json::Value> = serde_json::from_slice(bytes)
            .map_err(|e| TrustError::Malformed(format!("root manifest: {e}")))?;
        let payload_bytes = canonical_bytes(&env.signed)?;
        self.root_signers
            .verify_threshold(&payload_bytes, &env.signatures)?;
        let manifest: RootManifest = serde_json::from_value(env.signed.clone())
            .map_err(|e| TrustError::Malformed(format!("root manifest payload: {e}")))?;
        if manifest.version <= self.root.version {
            return Err(TrustError::Rollback {
                what: "root manifest",
                found: manifest.version,
                expected_at_least: self.root.version.saturating_add(1),
            }
            .into());
        }
        if manifest.root.keys.is_empty() || manifest.root.threshold == 0 {
            return Err(TrustError::ThresholdViolation(
                "new root manifest has an empty root key set or zero threshold".into(),
            )
            .into());
        }
        if (manifest.root.keys.len() as u32) < manifest.root.threshold {
            return Err(TrustError::ThresholdViolation(format!(
                "new root threshold {} exceeds its key count {}",
                manifest.root.threshold,
                manifest.root.keys.len()
            ))
            .into());
        }

        let old_fps: BTreeSet<&String> = self
            .root
            .repository
            .keys
            .keys()
            .chain(self.root.timestamp.keys.keys())
            .chain(self.root.reviewer.keys.keys())
            .collect();
        let new_fps: BTreeSet<&String> = manifest
            .repository
            .keys
            .keys()
            .chain(manifest.timestamp.keys.keys())
            .chain(manifest.reviewer.keys.keys())
            .collect();
        let report = RootRotationReport {
            old_version: self.root.version,
            new_version: manifest.version,
            added_keys: new_fps.difference(&old_fps).map(|s| s.to_string()).collect(),
            removed_keys: old_fps.difference(&new_fps).map(|s| s.to_string()).collect(),
        };

        self.root_signers = manifest.root.clone();
        self.state.root_version = manifest.version;
        self.state.root_signers = Some(manifest.root.clone());
        self.root = manifest;
        // Persist the exact bytes we verified — never a reserialization.
        if let Some(dir) = &self.dir {
            std::fs::create_dir_all(dir)?;
            let tmp = dir.join("root.json.tmp");
            std::fs::write(&tmp, bytes)?;
            std::fs::rename(&tmp, dir.join("root.json"))?;
        }
        self.persist_state()?;
        Ok(report)
    }

    // -----------------------------------------------------------------
    // `pkg key` support (v11 §10.5.4)
    // -----------------------------------------------------------------

    /// `pkg key list`: every key in every role, with local-revocation
    /// status.
    pub fn list_keys(&self) -> Vec<KeyInfo> {
        let mut out = Vec::new();
        for role in [Role::Root, Role::Repository, Role::Timestamp, Role::Reviewer] {
            let src = match role {
                Role::Root => &self.root.root,
                Role::Repository => &self.root.repository,
                Role::Timestamp => &self.root.timestamp,
                Role::Reviewer => &self.root.reviewer,
            };
            for fp in src.keys.keys() {
                out.push(KeyInfo {
                    role,
                    fingerprint: fp.clone(),
                    revoked: self.state.local_revocations.iter().any(|r| r == fp),
                });
            }
        }
        out
    }

    /// `pkg key show-chain`: a human-readable rendering of the
    /// delegation chain backing the currently trusted index.
    pub fn show_chain(&self) -> String {
        let mut s = String::new();
        s.push_str(&format!(
            "root manifest v{} (expires {}) — threshold {}/{} keys\n",
            self.root.version,
            self.root.expires_at,
            self.root.root.threshold,
            self.root.root.keys.len()
        ));
        for info in self.list_keys() {
            let mark = if info.revoked { " [locally revoked]" } else { "" };
            s.push_str(&format!("  {:10} {}{}\n", info.role.as_str(), info.fingerprint, mark));
        }
        s.push_str(&format!(
            "last accepted index_version: {}\n",
            self.state.last_index_version
        ));
        s
    }

    /// `pkg key revoke <fingerprint>`: local administrative revocation.
    /// Records the fingerprint in `local_revocations` rather than
    /// editing the signed root manifest (which would invalidate its
    /// signatures — see module docs). Refused for root keys (root
    /// changes require a signed re-delegation; there is no legitimate
    /// local-only root-key edit) and whenever the removal would drop
    /// the repository role below its signing threshold, since that
    /// would brick index verification until the next sync.
    pub fn revoke_key(&mut self, fingerprint: &str) -> PkgResult<()> {
        if self.root.root.keys.contains_key(fingerprint) {
            return Err(TrustError::ThresholdViolation(
                "root keys cannot be revoked locally; use 'pkg key trust-root' with a \
                 signed re-delegation document (v11 §10.5.3)"
                    .into(),
            )
            .into());
        }
        let known = [Role::Repository, Role::Timestamp, Role::Reviewer]
            .iter()
            .any(|r| self.effective_delegation(*r).keys.contains_key(fingerprint));
        if !known {
            return Err(TrustError::UnknownKey(fingerprint.to_string()).into());
        }
        // Would repository fall below threshold?
        let repo = self.effective_delegation(Role::Repository);
        if repo.keys.contains_key(fingerprint)
            && (repo.keys.len() as u32) <= repo.threshold
        {
            return Err(TrustError::ThresholdViolation(format!(
                "revoking {fingerprint} would drop the repository role below its \
                 threshold of {}",
                repo.threshold
            ))
            .into());
        }
        // Timestamp must keep at least one key or every sync fails.
        let ts = self.effective_delegation(Role::Timestamp);
        if ts.keys.contains_key(fingerprint) && ts.keys.len() <= 1 {
            return Err(TrustError::ThresholdViolation(
                "refusing to revoke the last timestamp key".into(),
            )
            .into());
        }
        if !self.state.local_revocations.iter().any(|r| r == fingerprint) {
            self.state.local_revocations.push(fingerprint.to_string());
        }
        self.persist_state()
    }

    /// The last accepted index version (rollback counter), for
    /// `pkg sync --status`.
    pub fn last_index_version(&self) -> u64 {
        self.state.last_index_version
    }

    fn persist_state(&self) -> PkgResult<()> {
        let Some(dir) = &self.dir else { return Ok(()) };
        std::fs::create_dir_all(dir)?;
        let bytes = serde_json::to_vec_pretty(&self.state)
            .map_err(|e| PkgError::Parse(format!("trust: state serialize: {e}")))?;
        let tmp = dir.join("state.json.tmp");
        std::fs::write(&tmp, bytes)?;
        std::fs::rename(&tmp, dir.join("state.json"))?;
        Ok(())
    }

    /// Parses a root-manifest document (the `pkg key trust-root <file>`
    /// input, §10.5.4) WITHOUT verifying or persisting it — this is the
    /// out-of-band recovery path for a broken delegation chain, so by
    /// design there is no trusted chain left to verify against. The
    /// caller displays the returned manifest's fingerprints and obtains
    /// interactive confirmation before calling [`TrustStore::import_root_manifest`].
    pub fn parse_root_manifest_doc(bytes: &[u8]) -> PkgResult<RootManifest> {
        let env: SignedDocument<serde_json::Value> = serde_json::from_slice(bytes)
            .map_err(|e| PkgError::Parse(format!("root manifest document: {e}")))?;
        serde_json::from_value(env.signed.clone())
            .map_err(|e| PkgError::Parse(format!("root manifest payload: {e}")))
    }

    /// Imports a root anchor out of band (§10.5.4): writes the document
    /// verbatim as `root.json` and resets the persisted state so the new
    /// manifest's own root delegation authorizes the *next* rotation
    /// (the old chain is definitionally broken — that's why this path
    /// exists). Returns the imported manifest.
    pub fn import_root_manifest(dir: &Path, bytes: &[u8]) -> PkgResult<RootManifest> {
        let manifest = Self::parse_root_manifest_doc(bytes)?;
        std::fs::create_dir_all(dir)?;
        let tmp = dir.join("root.json.tmp");
        std::fs::write(&tmp, bytes)?;
        std::fs::rename(&tmp, dir.join("root.json"))?;
        let state = PersistedState {
            root_version: manifest.version,
            root_signers: Some(manifest.root.clone()),
            ..PersistedState::default()
        };
        let state_bytes = serde_json::to_vec_pretty(&state)
            .map_err(|e| PkgError::Parse(format!("trust: state serialize: {e}")))?;
        let tmp = dir.join("state.json.tmp");
        std::fs::write(&tmp, state_bytes)?;
        std::fs::rename(&tmp, dir.join("state.json"))?;
        Ok(manifest)
    }
}

// ---------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};

    /// Deterministic test keys from fixed seeds — no RNG needed, so the
    /// tests are reproducible.
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

    /// Sign a payload value with the given keys, producing the JSON
    /// envelope bytes. This mirrors exactly what the repository CI does:
    /// sign the canonical serialization.
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

    #[allow(dead_code)] // committer_key is part of the fixture's reviewer set; some tests don't read it
    struct Fixture {
        root_keys: [SigningKey; 3],
        repo_key: SigningKey,
        ts_key: SigningKey,
        reviewer_key: SigningKey,
        committer_key: SigningKey,
        store: TrustStore,
    }

    fn fixture(now: u64) -> Fixture {
        let root_keys = [test_key(1), test_key(2), test_key(3)];
        let repo_key = test_key(10);
        let ts_key = test_key(11);
        let reviewer_key = test_key(12);
        let committer_key = test_key(13);
        let root = RootManifest {
            version: 1,
            created_at: now,
            expires_at: now + 365 * 24 * 3600,
            root: delegation_of(&[&root_keys[0], &root_keys[1], &root_keys[2]], 2),
            repository: delegation_of(&[&repo_key], 1),
            timestamp: delegation_of(&[&ts_key], 1),
            reviewer: delegation_of(&[&reviewer_key, &committer_key], 1),
        };
        let signers = delegation_of(&[&root_keys[0], &root_keys[1], &root_keys[2]], 2);
        let store = TrustStore::in_memory(root, signers);
        Fixture { root_keys, repo_key, ts_key, reviewer_key, committer_key, store }
    }

    fn index_bundle(
        f: &Fixture,
        index_version: u64,
        ts_expires_at: u64,
        ts_signers: &[&SigningKey],
    ) -> Vec<u8> {
        let index_payload = serde_json::json!({
            "format_version": 1,
            "index_version": index_version,
            "generated_at": 1_700_000_000u64,
            "packages": [{
                "name": "xz",
                "version": "5.6.1",
                "recipe_commit": "a1b2c3",
                "critical": true
            }]
        });
        let index_env: SignedDocument<serde_json::Value> =
            serde_json::from_slice(&sign_doc(index_payload, &[&f.repo_key])).unwrap();
        let ts_payload = serde_json::json!({
            "index_version": index_version,
            "index_sha256": sha256_hex(&canonical_bytes(&index_env.signed).unwrap()),
            "issued_at": ts_expires_at.saturating_sub(TIMESTAMP_EXPIRY_SECS),
            "expires_at": ts_expires_at,
        });
        let ts_env: SignedDocument<serde_json::Value> =
            serde_json::from_slice(&sign_doc(ts_payload, ts_signers)).unwrap();
        serde_json::to_vec(&serde_json::json!({
            "index": index_env,
            "timestamp": ts_env,
        }))
        .unwrap()
    }

    const NOW: u64 = 1_700_100_000;

    #[test]
    fn happy_path_verifies() {
        let f = fixture(NOW);
        let bundle = index_bundle(&f, 7, NOW + 3600, &[&f.ts_key]);
        let vi = f.store.verify_index_at(&bundle, NOW, VerifyPolicy::default()).unwrap();
        assert_eq!(vi.index_version, 7);
        assert_eq!(vi.packages.len(), 1);
        assert!(vi.packages[0].critical);
        assert!(vi.warnings.is_empty());
    }

    #[test]
    fn wrong_index_signer_rejected() {
        let f = fixture(NOW);
        let index_payload = serde_json::json!({
            "format_version": 1, "index_version": 7, "generated_at": NOW, "packages": []
        });
        // Signed by the *timestamp* key, not the repository key.
        let index_env: SignedDocument<serde_json::Value> =
            serde_json::from_slice(&sign_doc(index_payload, &[&f.ts_key])).unwrap();
        let ts_payload = serde_json::json!({
            "index_version": 7,
            "index_sha256": sha256_hex(&canonical_bytes(&index_env.signed).unwrap()),
            "issued_at": NOW, "expires_at": NOW + 3600,
        });
        let ts_env: SignedDocument<serde_json::Value> =
            serde_json::from_slice(&sign_doc(ts_payload, &[&f.ts_key])).unwrap();
        let bundle = serde_json::to_vec(&serde_json::json!({
            "index": index_env, "timestamp": ts_env
        }))
        .unwrap();
        let err = f.store.verify_index_at(&bundle, NOW, VerifyPolicy::default()).unwrap_err();
        assert!(err.to_string().contains("insufficient valid signatures"), "{err}");
    }

    /// Cloneable snapshot of the fixture's current root manifest as a
    /// JSON value, with `version` overridden — avoids partial moves out
    /// of the fixture when building rotation documents.
    fn root_payload(f: &Fixture, version: u64) -> serde_json::Value {
        let mut v = serde_json::to_value(f.store.root_manifest().clone()).unwrap();
        v["version"] = serde_json::json!(version);
        v
    }

    #[test]
    fn below_root_threshold_rejected_on_rotation() {
        let f = fixture(NOW);
        let new_root = root_payload(&f, 2);
        // Only ONE root signature where threshold is 2.
        let doc = sign_doc(new_root, &[&f.root_keys[0]]);
        let mut store = f.store;
        let err = store.apply_root_manifest(&doc).unwrap_err();
        assert!(err.to_string().contains("insufficient valid signatures"), "{err}");
    }

    #[test]
    fn index_rollback_rejected() {
        let mut f_store = fixture(NOW).store;
        let f = fixture(NOW);
        let bundle = index_bundle(&f, 7, NOW + 3600, &[&f.ts_key]);
        f_store.verify_index_at(&bundle, NOW, VerifyPolicy::default()).unwrap();
        f_store.record_index_version(7).unwrap();
        // Same version again is fine (idempotent re-sync).
        f_store.verify_index_at(&bundle, NOW, VerifyPolicy::default()).unwrap();
        // Older is a rollback.
        let old = index_bundle(&f, 6, NOW + 3600, &[&f.ts_key]);
        let err = f_store.verify_index_at(&old, NOW, VerifyPolicy::default()).unwrap_err();
        assert!(err.to_string().contains("rollback refused"), "{err}");
    }

    #[test]
    fn timestamp_grace_window_warns_then_fails() {
        let f = fixture(NOW);
        let expiry = NOW; // expired exactly at NOW
        let bundle = index_bundle(&f, 7, expiry, &[&f.ts_key]);

        // Within grace: accepted with a warning.
        let vi = f
            .store
            .verify_index_at(&bundle, NOW + 3600, VerifyPolicy::default())
            .unwrap();
        assert!(matches!(
            vi.warnings.as_slice(),
            [TrustWarning::TimestampStaleGrace { expired_secs_ago: 3600 }]
        ));

        // Past grace: hard failure.
        let past = NOW + TIMESTAMP_GRACE_SECS + 1;
        let err = f.store.verify_index_at(&bundle, past, VerifyPolicy::default()).unwrap_err();
        assert!(err.to_string().contains("timestamp expired"), "{err}");

        // --ignore-stale overrides, with a louder warning recorded.
        let vi = f
            .store
            .verify_index_at(&bundle, past, VerifyPolicy { ignore_stale: true })
            .unwrap();
        assert!(matches!(
            vi.warnings.as_slice(),
            [TrustWarning::TimestampStaleIgnored { .. }]
        ));
    }

    #[test]
    fn timestamp_binding_mismatch_rejected() {
        let f = fixture(NOW);
        // A fresh timestamp pointing at a DIFFERENT index version.
        let index_payload = serde_json::json!({
            "format_version": 1, "index_version": 8, "generated_at": NOW, "packages": []
        });
        let index_env: SignedDocument<serde_json::Value> =
            serde_json::from_slice(&sign_doc(index_payload, &[&f.repo_key])).unwrap();
        let ts_payload = serde_json::json!({
            "index_version": 8,
            "index_sha256": sha256_hex(b"some other index"),
            "issued_at": NOW, "expires_at": NOW + 3600,
        });
        let ts_env: SignedDocument<serde_json::Value> =
            serde_json::from_slice(&sign_doc(ts_payload, &[&f.ts_key])).unwrap();
        let bundle = serde_json::to_vec(&serde_json::json!({
            "index": index_env, "timestamp": ts_env
        }))
        .unwrap();
        let err = f.store.verify_index_at(&bundle, NOW, VerifyPolicy::default()).unwrap_err();
        assert!(err.to_string().contains("does not match"), "{err}");
    }

    #[test]
    fn root_rotation_requires_strictly_greater_version() {
        let f = fixture(NOW);
        // Snapshot the payload template and clone the signing keys BEFORE
        // moving `store` out of the fixture — a closure borrowing `f`
        // would make the move illegal.
        let template = root_payload(&f, 0);
        let k0 = f.root_keys[0].clone();
        let k1 = f.root_keys[1].clone();
        let mk = |version: u64| {
            let mut v = template.clone();
            v["version"] = serde_json::json!(version);
            sign_doc(v, &[&k0, &k1])
        };
        let mut store = f.store;
        // v2 accepted.
        let report = store.apply_root_manifest(&mk(2)).unwrap();
        assert_eq!(report.old_version, 1);
        assert_eq!(report.new_version, 2);
        // Replaying v2 (or v1) is a rollback, refused.
        assert!(store.apply_root_manifest(&mk(2)).is_err());
    }

    #[test]
    fn rotation_replaces_signer_set() {
        let f = fixture(NOW);
        let new_repo = test_key(20);
        let mut new_root_payload = root_payload(&f, 2);
        new_root_payload["repository"] =
            serde_json::to_value(delegation_of(&[&new_repo], 1)).unwrap();
        // Build everything that borrows `f` before moving `store` out.
        let rotation_doc = sign_doc(new_root_payload, &[&f.root_keys[0], &f.root_keys[1]]);
        let bundle = index_bundle(&f, 9, NOW + 3600, &[&f.ts_key]);
        let mut store = f.store;
        let report = store.apply_root_manifest(&rotation_doc).unwrap();
        assert_eq!(report.added_keys.len(), 1);
        assert_eq!(report.removed_keys.len(), 1); // old repo key gone
        // Old repo key no longer verifies an index.
        assert!(store.verify_index_at(&bundle, NOW, VerifyPolicy::default()).is_err());
    }

    #[test]
    fn revoke_guards_thresholds_and_root_keys() {
        let f = fixture(NOW);
        let mut store = f.store;
        let root_fp = key_fingerprint(&f.root_keys[0].verifying_key());
        let repo_fp = key_fingerprint(&f.repo_key.verifying_key());
        let ts_fp = key_fingerprint(&f.ts_key.verifying_key());
        // Root keys: refused.
        assert!(store.revoke_key(&root_fp).is_err());
        // Sole repository key (threshold 1): refused.
        assert!(store.revoke_key(&repo_fp).is_err());
        // Sole timestamp key: refused.
        assert!(store.revoke_key(&ts_fp).is_err());
        // Unknown fingerprint: refused.
        assert!(store.revoke_key("deadbeef").is_err());
        // Reviewer key (threshold 1, 2 members): allowed, and the key
        // disappears from the effective reviewer set.
        let reviewer_fp = key_fingerprint(&f.reviewer_key.verifying_key());
        store.revoke_key(&reviewer_fp).unwrap();
        assert!(store.reviewer_key(&reviewer_fp).is_none());
        assert!(store
            .list_keys()
            .iter()
            .any(|k| k.fingerprint == reviewer_fp && k.revoked));
    }

    #[test]
    fn bad_signature_from_member_key_is_fail_loud() {
        let f = fixture(NOW);
        let mut bundle = index_bundle(&f, 7, NOW + 3600, &[&f.ts_key]);
        // Corrupt the index signature: right key id, wrong bytes.
        let mut parsed: serde_json::Value = serde_json::from_slice(&bundle).unwrap();
        parsed["index"]["signatures"][0]["sig"] =
            serde_json::Value::String(hex_encode(&[0xAB; 64]));
        bundle = serde_json::to_vec(&parsed).unwrap();
        let err = f.store.verify_index_at(&bundle, NOW, VerifyPolicy::default()).unwrap_err();
        assert!(err.to_string().contains("invalid signature"), "{err}");
    }

    #[test]
    fn hex_roundtrip_and_rejects_garbage() {
        assert_eq!(hex_encode(&[0x00, 0xff, 0x5a]), "00ff5a");
        assert_eq!(hex_decode("00ff5a").unwrap(), vec![0x00, 0xff, 0x5a]);
        assert_eq!(hex_decode("00FF").unwrap(), vec![0x00, 0xff]);
        assert!(hex_decode("0").is_err());
        assert!(hex_decode("zz").is_err());
    }

    #[test]
    fn fingerprint_is_sha256_prefix() {
        let k = test_key(42).verifying_key();
        let fp = key_fingerprint(&k);
        let full = sha256_hex(k.as_bytes());
        assert_eq!(fp.len(), 32);
        assert!(full.starts_with(&fp));
    }
}
