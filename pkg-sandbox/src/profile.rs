//! ## v1 changes
//!
//! - Initial implementation (SPEC: pkg-sandbox; design doc Issue 3.4,
//!   v14 §4 "Sandbox: no network, mandatory vendoring").

//! Sandbox profiles: the hardcoded `Build`/`Fetch`/`Hook` profiles plus
//! data-driven profiles loaded from `/usr/lib/pkg/sandbox/*.profile`.
//!
//! Issue 3.4 makes the build sandbox profile an *explicit, auditable,
//! versioned* artifact rather than a pile of flags buried in code. Two
//! mechanisms cooperate here:
//!
//! - The three built-in profiles ([`SandboxProfile::Build`],
//!   `Fetch`, `Hook`) are hardcoded — they are security policy, and the
//!   *absence* of a profile file must never silently weaken the build
//!   sandbox. `Build`'s defaults are the SPEC contract: no network, RW
//!   only scratch + `$PKGDIR`, RO toolchain/dep tree, no `/etc` write.
//! - `*.profile` files under [`PROFILE_DIR`] describe *additional*
//!   profiles (or deployment-specific variants) as TOML
//!   (`bind_ro[]`, `bind_rw[]`, `net`, `env_allow[]`). They become
//!   [`SandboxProfile::Custom`]. A malformed file is a hard
//!   `PkgError::Parse`, never a fallback to something more permissive.
//!
//! The profile is *data* — [`ProfileData`] — so `pkg audit
//! --supply-chain` can report exactly which profile (and version) a
//! package was built under (Issue 3.4: "the full profile is a versioned
//! data file … so `pkg audit --supply-chain` can report which profile
//! version built any given package"). [`ProfileData::canonical`]
//! produces the stable serialization that audit/provenance tooling
//! hashes.

use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use pkg_core::{PkgError, PkgResult};

/// Directory containing data-driven sandbox profiles.
pub const PROFILE_DIR: &str = "/usr/lib/pkg/sandbox";

/// The profile of record for source builds (Issue 3.4, v14 §4). Audit
/// and provenance records name this profile so the report ties a built
/// artifact to the exact sandbox policy it was built under.
pub const BUILD_PROFILE_NAME: &str = "build";

/// The declarative content of a sandbox profile: which host paths are
/// bound (read-only or read-write), whether the sandbox gets a network
/// namespace with real networking, and which host environment variables
/// may pass through.
///
/// This is deliberately a *dumb* data type: no policy logic, no
/// filesystem access. The built-in profiles produce `ProfileData`
/// values from hardcoded constructors; `*.profile` files deserialize
/// into it. `run.rs` consumes it to build a `bwrap` argv.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileData {
    /// Profile name — the file stem for loaded profiles (`build` for
    /// `/usr/lib/pkg/sandbox/build.profile`), or the built-in name.
    /// Recorded in provenance (`builder_version`) per Issue 3.4.
    pub name: String,
    /// Profile content version, if the profile file declares one.
    /// Built-ins carry [`BUILTIN_PROFILE_VERSION`].
    pub version: Option<String>,
    /// Host paths bound read-only at the same path inside the sandbox
    /// (toolchain, dependency store dirs, `/etc` for the fetch profile).
    pub bind_ro: Vec<PathBuf>,
    /// Host paths bound read-write at the same path inside the
    /// sandbox. For `Build` this is populated per-invocation with the
    /// scratch dir and `$PKGDIR` — see [`SandboxProfile::resolve`].
    pub bind_rw: Vec<PathBuf>,
    /// Whether the sandbox keeps network access. `false` ⇒
    /// `bwrap --unshare-net` (empty network namespace). Builds are
    /// *always* `false` (v14: "no runtime network, ever" for the build
    /// proper; fetching happens in the earlier Fetch stage).
    pub net: bool,
    /// Names of *host process* environment variables allowed to pass
    /// into the sandbox (each becomes `--setenv NAME <host value>`).
    /// Everything else is dropped via `--clearenv`. Environment the
    /// caller constructs explicitly (the `env` argument to
    /// `run_sandboxed` — `PKG_OPT_*`, `SOURCE_DATE_EPOCH`, …) is always
    /// set; this list only gates *inherited* host variables.
    pub env_allow: Vec<String>,
}

/// Version stamp carried by the hardcoded built-in profiles. Bumped
/// when the built-in policy changes, so provenance records can tell
/// which revision of the built-in policy built a package even before
/// `/usr/lib/pkg/sandbox/build.profile` ships as a data file.
pub const BUILTIN_PROFILE_VERSION: &str = "1";

/// A sandbox profile: one of the three hardcoded built-ins, or a
/// data-driven profile loaded from a `*.profile` file.
///
/// The built-ins are enum variants (not files that happen to be
/// pre-installed) because the build sandbox is part of the supply-chain
/// trust story: if the profile were only a data file, a missing or
/// corrupted `/usr/lib/pkg/sandbox/build.profile` could silently turn
/// into a weaker sandbox. Hardcoding the floor policy means "profile
/// file absent" is a non-event for the built-ins, and a *malformed*
/// custom profile is a loud parse error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SandboxProfile {
    /// Source-build profile (v14 §4, Issue 3.4): empty network
    /// namespace, RW only the per-build scratch dir and `$PKGDIR`, RO
    /// toolchain + resolved dependency tree, no `/etc` write. Identity
    /// and syscall restrictions beyond what bwrap can express are
    /// documented in `run.rs`.
    Build,
    /// Fetch profile: the separate, earlier stage that downloads pinned
    /// sources. Its only write target is the source cache; network is
    /// present here (unlike `Build`), restricted to the manifest's
    /// declared origins by pkg-repo's fetch verification (v14 T7) —
    /// bwrap itself cannot express a DNS/origin allowlist, and does not
    /// try to.
    Fetch,
    /// Hook profile for maintainer scripts (post-install hooks, §7.8):
    /// no network, no home, RW limited to system config/state the hook
    /// legitimately maintains.
    Hook,
    /// A data-driven profile loaded from
    /// `/usr/lib/pkg/sandbox/<name>.profile`.
    Custom(ProfileData),
}

impl SandboxProfile {
    /// Loads a data-driven profile by name from
    /// `/usr/lib/pkg/sandbox/<name>.profile`.
    ///
    /// The built-in names (`build`, `fetch`, `hook`) resolve to the
    /// hardcoded profiles *without* touching the filesystem — the
    /// built-ins are policy floors, not configuration. Any other name
    /// must exist as a profile file; a missing file is
    /// `PkgError::NotFound`, a malformed one `PkgError::Parse`. Neither
    /// falls back to a permissive default.
    pub fn load(name: &str) -> PkgResult<SandboxProfile> {
        match name {
            "build" => return Ok(SandboxProfile::Build),
            "fetch" => return Ok(SandboxProfile::Fetch),
            "hook" => return Ok(SandboxProfile::Hook),
            _ => {}
        }
        let path = Path::new(PROFILE_DIR).join(format!("{name}.profile"));
        let text = fs::read_to_string(&path).map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => {
                PkgError::NotFound(format!("sandbox profile '{}': no {}", name, path.display()))
            }
            _ => PkgError::Io(e),
        })?;
        parse_profile(name, &text).map(SandboxProfile::Custom)
    }

    /// Resolves this profile to its effective [`ProfileData`], given the
    /// per-invocation scratch dir and `$PKGDIR`.
    ///
    /// `scratch_dir` is the per-build scratch directory
    /// (`/var/lib/pkg/build/<pid>`); `pkgdir` is the packaging
    /// destination the recipe's install phase writes into. Only
    /// `Build` uses them — they are the *only* RW paths in the build
    /// sandbox, so they are parameters rather than profile data (a
    /// profile file must not be able to widen the build's RW surface).
    /// For `Fetch`, the RW source cache is static profile data. `Hook`
    /// ignores both.
    pub fn resolve(&self, scratch_dir: &Path, pkgdir: Option<&Path>) -> ProfileData {
        match self {
            SandboxProfile::Build => build_profile(scratch_dir, pkgdir),
            SandboxProfile::Fetch => fetch_profile(),
            SandboxProfile::Hook => hook_profile(),
            SandboxProfile::Custom(data) => {
                // Custom profiles declare their RW surface statically;
                // the scratch dir is still bound RW so the caller has a
                // guaranteed writable workspace regardless of profile.
                let mut data = data.clone();
                if !data.bind_rw.iter().any(|p| p == scratch_dir) {
                    data.bind_rw.push(scratch_dir.to_path_buf());
                }
                data
            }
        }
    }
}

impl fmt::Display for SandboxProfile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SandboxProfile::Build => write!(f, "build"),
            SandboxProfile::Fetch => write!(f, "fetch"),
            SandboxProfile::Hook => write!(f, "hook"),
            SandboxProfile::Custom(data) => write!(f, "{}", data.name),
        }
    }
}

/// The hardcoded Build profile (SPEC: pkg-sandbox; Issue 3.4).
///
/// - `net: false` — the build stage gets an empty network namespace,
///   always. Fetching is a separate earlier stage; build-time fetchers
///   (cargo, npm, go) are handled by manifest-level vendoring, so no
///   legitimate build needs the network.
/// - RO: the toolchain and system trees. Note `/etc` is *not* bound at
///   all for builds (rather than bound RO): "no /etc write" is the
///   floor, but nothing in a hermetic build should read host config
///   either — omitting it makes host-config leakage visible as an
///   immediate error instead of a silent impurity. (The toolchain's
///   compiled-in paths under `/usr` and `/lib` remain available; config
///   files the build genuinely needs are materialized into the scratch
///   dir by pkg-build's recipe runner.)
/// - RW: exactly `scratch_dir` and (if given) `pkgdir` — per Issue
///   3.4's "read-write *only* the per-build scratch dir and `$PKGDIR`".
/// - `env_allow`: the minimal inherited-environment surface. Notably
///   `PATH` (toolchain discovery), locale, and nothing credential-shaped
///   (no `HOME`, no `*_TOKEN`, no `SSH_*`).
fn build_profile(scratch_dir: &Path, pkgdir: Option<&Path>) -> ProfileData {
    let mut bind_rw = vec![scratch_dir.to_path_buf()];
    if let Some(pkgdir) = pkgdir {
        bind_rw.push(pkgdir.to_path_buf());
    }
    ProfileData {
        name: BUILD_PROFILE_NAME.to_string(),
        version: Some(BUILTIN_PROFILE_VERSION.to_string()),
        // Toolchain + runtime linker/config roots, read-only. The
        // dependency tree lives under the store (`/var/lib/pkg/store`),
        // which pkg-build binds RO per resolved dep at run time in
        // addition to these (see run.rs).
        bind_ro: ["/usr", "/bin", "/lib", "/lib64"]
            .iter()
            .map(PathBuf::from)
            .collect(),
        bind_rw,
        net: false,
        env_allow: ["PATH", "LANG", "LC_ALL", "TZ", "TERM"]
            .iter()
            .map(|s| s.to_string())
            .collect(),
    }
}

/// The hardcoded Fetch profile (Issue 3.4: fetch is a separate earlier
/// sandbox stage whose only write target is the source cache).
fn fetch_profile() -> ProfileData {
    ProfileData {
        name: "fetch".to_string(),
        version: Some(BUILTIN_PROFILE_VERSION.to_string()),
        // Fetch needs TLS roots and resolver config, hence RO /etc
        // (unlike Build, which omits it entirely).
        bind_ro: ["/usr", "/bin", "/lib", "/lib64", "/etc"]
            .iter()
            .map(PathBuf::from)
            .collect(),
        bind_rw: ["/var/lib/pkg/cache/sources"]
            .iter()
            .map(PathBuf::from)
            .collect(),
        net: true,
        env_allow: ["PATH", "LANG", "LC_ALL", "TZ"]
            .iter()
            .map(|s| s.to_string())
            .collect(),
    }
}

/// The hardcoded Hook profile: maintainer scripts (post-install hooks,
/// §7.8). No network, no home directory, RW limited to the system
/// config/state such hooks exist to maintain (`ldconfig`, sysusers,
/// icon caches, …).
fn hook_profile() -> ProfileData {
    ProfileData {
        name: "hook".to_string(),
        version: Some(BUILTIN_PROFILE_VERSION.to_string()),
        bind_ro: ["/usr", "/bin", "/lib", "/lib64"]
            .iter()
            .map(PathBuf::from)
            .collect(),
        bind_rw: ["/etc", "/var"].iter().map(PathBuf::from).collect(),
        net: false,
        env_allow: ["PATH", "LANG", "LC_ALL", "TZ"]
            .iter()
            .map(|s| s.to_string())
            .collect(),
    }
}

impl ProfileData {
    /// A stable, canonical serialization of the profile — sorted binds,
    /// one field per line. This is what provenance/audit tooling hashes
    /// to answer "was this package built under the same sandbox policy
    /// as that one?" without depending on TOML formatting details
    /// (Issue 3.4's "versioned data file … report which profile version
    /// built any given package").
    pub fn canonical(&self) -> String {
        let mut ro: Vec<&str> = self.bind_ro.iter().map(|p| p.to_str().unwrap_or("")).collect();
        ro.sort_unstable();
        let mut rw: Vec<&str> = self.bind_rw.iter().map(|p| p.to_str().unwrap_or("")).collect();
        rw.sort_unstable();
        let mut env: Vec<&str> = self.env_allow.iter().map(String::as_str).collect();
        env.sort_unstable();

        let mut out = String::new();
        out.push_str("name:");
        out.push_str(&self.name);
        out.push('\n');
        out.push_str("version:");
        out.push_str(self.version.as_deref().unwrap_or(""));
        out.push('\n');
        out.push_str(&format!("net:{}\n", self.net));
        for p in ro {
            out.push_str("bind_ro:");
            out.push_str(p);
            out.push('\n');
        }
        for p in rw {
            out.push_str("bind_rw:");
            out.push_str(p);
            out.push('\n');
        }
        for e in env {
            out.push_str("env_allow:");
            out.push_str(e);
            out.push('\n');
        }
        out
    }
}

/// Parses a `*.profile` TOML document into [`ProfileData`].
///
/// The accepted schema is exactly the SPEC contract:
///
/// ```toml
/// version = "3"                  # optional
/// bind_ro = ["/usr", "/lib"]
/// bind_rw = ["/var/cache/foo"]
/// net = false
/// env_allow = ["PATH", "LANG"]
/// ```
///
/// Parsing is hand-rolled rather than via serde/`toml` because
/// pkg-sandbox's dependency pin is **pkg-core only** (SPEC: "Crate
/// layout & dependency pins") — pulling in `toml` just for this file
/// would violate the pin. The format accepted is the intersection of
/// TOML and what a profile author actually writes: `key = value` lines
/// with string scalars, booleans, and string arrays; `#` comments;
/// blank lines. Anything else is a `PkgError::Parse` — profiles are
/// security policy, so "I didn't understand that line" must be loud.
pub fn parse_profile(name: &str, text: &str) -> PkgResult<ProfileData> {
    let mut data = ProfileData {
        name: name.to_string(),
        version: None,
        bind_ro: Vec::new(),
        bind_rw: Vec::new(),
        net: false,
        env_allow: Vec::new(),
    };

    for (lineno, raw_line) in text.lines().enumerate() {
        let line = strip_comment(raw_line).trim().to_string();
        if line.is_empty() {
            continue;
        }
        let err = |msg: &str| {
            PkgError::Parse(format!(
                "sandbox profile '{name}' line {}: {msg}",
                lineno + 1
            ))
        };
        let Some((key, value)) = line.split_once('=') else {
            return Err(err("expected 'key = value'"));
        };
        let key = key.trim();
        let value = value.trim();
        match key {
            "version" => data.version = Some(parse_string(value).map_err(|m| err(&m))?),
            "net" => {
                data.net = match value {
                    "true" => true,
                    "false" => false,
                    _ => return Err(err("net must be true or false")),
                }
            }
            "bind_ro" => {
                data.bind_ro = parse_string_array(value)
                    .map_err(|m| err(&m))?
                    .into_iter()
                    .map(PathBuf::from)
                    .collect()
            }
            "bind_rw" => {
                data.bind_rw = parse_string_array(value)
                    .map_err(|m| err(&m))?
                    .into_iter()
                    .map(PathBuf::from)
                    .collect()
            }
            "env_allow" => data.env_allow = parse_string_array(value).map_err(|m| err(&m))?,
            other => {
                return Err(err(&format!(
                    "unknown key '{other}' (allowed: version, bind_ro, bind_rw, net, env_allow)"
                )))
            }
        }
    }

    Ok(data)
}

/// Removes a trailing `#` comment, respecting double-quoted strings so
/// a `#` inside a quoted value isn't treated as a comment start.
fn strip_comment(line: &str) -> &str {
    let mut in_string = false;
    let mut prev = '\0';
    for (i, c) in line.char_indices() {
        match c {
            '"' if prev != '\\' => in_string = !in_string,
            '#' if !in_string => return &line[..i],
            _ => {}
        }
        prev = c;
    }
    line
}

/// Parses a `"quoted"` string scalar (the only string form profiles
/// use). Returns the unquoted content; backslash escapes other than
/// `\"` and `\\` are rejected rather than interpreted, keeping the
/// accepted grammar obvious.
fn parse_string(value: &str) -> Result<String, String> {
    let value = value.trim();
    let Some(inner) = value
        .strip_prefix('"')
        .and_then(|v| v.strip_suffix('"'))
    else {
        return Err(format!("expected a double-quoted string, got '{value}'"));
    };
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('"') => out.push('"'),
                Some('\\') => out.push('\\'),
                other => {
                    return Err(format!(
                        "unsupported escape '\\{}'",
                        other.unwrap_or('\0')
                    ))
                }
            }
        } else {
            out.push(c);
        }
    }
    Ok(out)
}

/// Parses `["a", "b"]` — a single-line array of double-quoted strings.
/// Multi-line arrays are rejected: profile files are meant to stay
/// short and auditable, and the restriction keeps the parser trivial.
fn parse_string_array(value: &str) -> Result<Vec<String>, String> {
    let value = value.trim();
    let Some(inner) = value
        .strip_prefix('[')
        .and_then(|v| v.strip_suffix(']'))
    else {
        return Err(format!("expected a string array like [\"a\", \"b\"], got '{value}'"));
    };
    let inner = inner.trim();
    if inner.is_empty() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for element in split_array_elements(inner)? {
        out.push(parse_string(element)?);
    }
    Ok(out)
}

/// Splits the inside of `[...]` on top-level commas (commas inside
/// quoted strings don't split). Each returned element is still quoted;
/// unquoting is `parse_string`'s job.
fn split_array_elements(inner: &str) -> Result<Vec<&str>, String> {
    let mut elements = Vec::new();
    let mut in_string = false;
    let mut start = 0;
    let mut prev = '\0';
    for (i, c) in inner.char_indices() {
        match c {
            '"' if prev != '\\' => in_string = !in_string,
            ',' if !in_string => {
                elements.push(inner[start..i].trim());
                start = i + 1;
            }
            _ => {}
        }
        prev = c;
    }
    if in_string {
        return Err("unterminated string in array".to_string());
    }
    elements.push(inner[start..].trim());
    if elements.iter().any(|e| e.is_empty()) {
        return Err("empty element in array (trailing or doubled comma)".to_string());
    }
    Ok(elements)
}

// ---------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_names_resolve_without_touching_the_filesystem() {
        // These must not require /usr/lib/pkg/sandbox to exist.
        assert_eq!(SandboxProfile::load("build").unwrap(), SandboxProfile::Build);
        assert_eq!(SandboxProfile::load("fetch").unwrap(), SandboxProfile::Fetch);
        assert_eq!(SandboxProfile::load("hook").unwrap(), SandboxProfile::Hook);
    }

    #[test]
    fn build_profile_defaults_match_spec() {
        let scratch = Path::new("/var/lib/pkg/build/1234");
        let pkgdir = Path::new("/var/lib/pkg/build/1234/pkgdir");
        let data = SandboxProfile::Build.resolve(scratch, Some(pkgdir));

        // No network, ever, for the build stage (v14 §4).
        assert!(!data.net);
        // RW surface is exactly scratch + $PKGDIR (Issue 3.4).
        assert_eq!(
            data.bind_rw,
            vec![scratch.to_path_buf(), pkgdir.to_path_buf()]
        );
        // Toolchain is RO…
        assert!(data.bind_ro.contains(&PathBuf::from("/usr")));
        // …and /etc is not bound at all for builds (no /etc write,
        // and no host-config leakage).
        assert!(!data.bind_ro.iter().any(|p| p.starts_with("/etc")));
        assert!(!data.bind_rw.iter().any(|p| p.starts_with("/etc")));
    }

    #[test]
    fn build_profile_without_pkgdir_binds_only_scratch_rw() {
        let scratch = Path::new("/var/lib/pkg/build/9");
        let data = SandboxProfile::Build.resolve(scratch, None);
        assert_eq!(data.bind_rw, vec![scratch.to_path_buf()]);
    }

    #[test]
    fn fetch_profile_has_network_but_build_does_not() {
        let scratch = Path::new("/var/lib/pkg/build/1");
        assert!(SandboxProfile::Fetch.resolve(scratch, None).net);
        assert!(!SandboxProfile::Build.resolve(scratch, None).net);
        assert!(!SandboxProfile::Hook.resolve(scratch, None).net);
    }

    #[test]
    fn parses_a_full_profile_file() {
        let text = r#"
            # example custom profile
            version = "3"
            net = true
            bind_ro = ["/usr", "/opt/toolchain"]  # toolchain
            bind_rw = ["/var/cache/ccache"]
            env_allow = ["PATH", "HOME"]
        "#;
        let data = parse_profile("ccache", text).unwrap();
        assert_eq!(data.name, "ccache");
        assert_eq!(data.version.as_deref(), Some("3"));
        assert!(data.net);
        assert_eq!(
            data.bind_ro,
            vec![PathBuf::from("/usr"), PathBuf::from("/opt/toolchain")]
        );
        assert_eq!(data.bind_rw, vec![PathBuf::from("/var/cache/ccache")]);
        assert_eq!(data.env_allow, vec!["PATH".to_string(), "HOME".to_string()]);
    }

    #[test]
    fn parse_rejects_unknown_keys_and_garbage() {
        assert!(parse_profile("x", "net = maybe").is_err());
        assert!(parse_profile("x", "seccomp = [\"ptrace\"]").is_err());
        assert!(parse_profile("x", "this is not toml").is_err());
        assert!(parse_profile("x", "bind_ro = /usr").is_err());
        // Multi-line arrays are intentionally out of grammar.
        assert!(parse_profile("x", "bind_ro = [\n\"/usr\"\n]").is_err());
    }

    #[test]
    fn hash_inside_quoted_string_is_not_a_comment() {
        let data = parse_profile("x", "env_allow = [\"A#B\"] # real comment").unwrap();
        assert_eq!(data.env_allow, vec!["A#B".to_string()]);
    }

    #[test]
    fn canonical_serialization_is_order_independent() {
        let a = ProfileData {
            name: "x".into(),
            version: Some("1".into()),
            bind_ro: vec![PathBuf::from("/usr"), PathBuf::from("/bin")],
            bind_rw: vec![PathBuf::from("/b"), PathBuf::from("/a")],
            net: false,
            env_allow: vec!["Z".into(), "A".into()],
        };
        let mut b = a.clone();
        b.bind_ro.reverse();
        b.bind_rw.reverse();
        b.env_allow.reverse();
        assert_eq!(a.canonical(), b.canonical());
        assert_eq!(
            a.canonical(),
            "name:x\nversion:1\nnet:false\nbind_ro:/bin\nbind_ro:/usr\n\
             bind_rw:/a\nbind_rw:/b\nenv_allow:A\nenv_allow:Z\n"
        );
    }

    #[test]
    fn custom_profile_still_gets_scratch_bound_rw() {
        let scratch = Path::new("/var/lib/pkg/build/7");
        let custom = SandboxProfile::Custom(ProfileData {
            name: "x".into(),
            version: None,
            bind_ro: vec![],
            bind_rw: vec![PathBuf::from("/var/cache/x")],
            net: false,
            env_allow: vec![],
        });
        let data = custom.resolve(scratch, None);
        assert!(data.bind_rw.contains(&scratch.to_path_buf()));
    }
}
