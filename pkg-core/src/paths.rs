//! Test-root override for every hardcoded absolute state path
//! (SPEC: "pkg-core contracts").
//!
//! When the environment variable `PKG_ROOT` is set, state paths that
//! would normally be absolute under `/` (the DB, the lock, the caches,
//! the store roots) are resolved UNDER that root instead:
//! `at("/var/lib/pkg/lock")` yields `$PKG_ROOT/var/lib/pkg/lock`. When
//! `PKG_ROOT` is unset (or empty), `at` returns its argument unchanged,
//! byte-identical to the historical behavior.
//!
//! This exists so the whole toolchain can run end-to-end against a
//! throwaway root directory in tests. It is NOT a general chroot:
//! paths that must stay on the real system by design (sandbox bind
//! mounts, the boot-time pkg-init tool, audit's live-system scans)
//! deliberately do not go through here.
//!
//! # `[paths]` config
//!
//! On top of the root prefix, individual paths can be relocated via a
//! `[paths]` table in `<root>/etc/pkg/config.toml`, keyed by the
//! default (SPEC) absolute path:
//!
//! ```toml
//! [paths]
//! "/var/lib/pkg/cache" = "/bigdisk/pkg-cache"
//! ```
//!
//! Overrides are applied before the root prefix, so `PKG_ROOT` testing
//! keeps working: the override above resolves to
//! `$PKG_ROOT/bigdisk/pkg-cache` when `PKG_ROOT` is set. Override
//! values must be absolute paths. A missing config file means no
//! overrides; an unparseable one is a loud warning with defaults kept
//! (this helper sits too deep to propagate errors — every state path
//! in the workspace flows through it).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// The environment variable naming the test root.
pub const ROOT_ENV_VAR: &str = "PKG_ROOT";

/// The configured root: the value of `PKG_ROOT` (cached on first use),
/// or `/` when the variable is unset or empty.
pub fn root() -> &'static Path {
    static ROOT: OnceLock<PathBuf> = OnceLock::new();
    ROOT.get_or_init(|| match std::env::var_os(ROOT_ENV_VAR) {
        Some(v) if !v.is_empty() => PathBuf::from(v),
        _ => PathBuf::from("/"),
    })
}

/// Resolves an absolute-path `suffix` (e.g. `/var/lib/pkg/lock`) to its
/// effective location: first any `[paths]` override for that exact
/// default path, then the [`root()`] prefix.
pub fn at(suffix: &str) -> PathBuf {
    resolve(root(), overrides(), suffix)
}

/// The `[paths]` override table from `<root>/etc/pkg/config.toml`,
/// loaded once. Invalid entries (non-absolute values, non-string
/// values) are dropped with a warning; an unparseable file warns and
/// yields an empty table.
fn overrides() -> &'static BTreeMap<String, String> {
    static OVERRIDES: OnceLock<BTreeMap<String, String>> = OnceLock::new();
    OVERRIDES.get_or_init(|| {
        let config = root().join("etc/pkg/config.toml");
        let Ok(text) = std::fs::read_to_string(&config) else {
            return BTreeMap::new();
        };
        let value: toml::Value = match toml::from_str(&text) {
            Ok(v) => v,
            Err(e) => {
                eprintln!(
                    "pkg: warning: ignoring {}: {e}",
                    config.display()
                );
                return BTreeMap::new();
            }
        };
        let mut map = BTreeMap::new();
        if let Some(table) = value.get("paths").and_then(|p| p.as_table()) {
            for (default, v) in table {
                match v.as_str() {
                    Some(s) if s.starts_with('/') => {
                        map.insert(default.clone(), s.to_string());
                    }
                    _ => eprintln!(
                        "pkg: warning: [paths] override for {default:?} must be an \
                         absolute-path string; ignoring"
                    ),
                }
            }
        }
        map
    })
}

/// The pure resolution [`at`] delegates to (unit-testable without
/// touching process-global env or the filesystem).
fn resolve(root: &Path, overrides: &BTreeMap<String, String>, suffix: &str) -> PathBuf {
    let effective = overrides.get(suffix).map(String::as_str).unwrap_or(suffix);
    join_root(root, effective)
}

/// The pure join [`at`] delegates to: `root` + `suffix` with the
/// suffix's leading `/` stripped when `root` is not `/` itself (a
/// plain `Path::join` would treat the absolute suffix as a replacement,
/// not a descent).
fn join_root(root: &Path, suffix: &str) -> PathBuf {
    if root == Path::new("/") {
        PathBuf::from(suffix)
    } else {
        root.join(suffix.strip_prefix('/').unwrap_or(suffix))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_root_passes_suffixes_through_unchanged() {
        // PKG_ROOT unset semantics: byte-identical to the old hardcoded
        // path.
        assert_eq!(
            join_root(Path::new("/"), "/var/lib/pkg/lock"),
            PathBuf::from("/var/lib/pkg/lock")
        );
    }

    #[test]
    fn override_root_prefixes_suffixes() {
        assert_eq!(
            join_root(Path::new("/tmp/pkg-e2e"), "/var/lib/pkg/lock"),
            PathBuf::from("/tmp/pkg-e2e/var/lib/pkg/lock")
        );
        // A suffix without a leading slash joins the same way.
        assert_eq!(
            join_root(Path::new("/tmp/pkg-e2e"), "var/lib/pkg/lock"),
            PathBuf::from("/tmp/pkg-e2e/var/lib/pkg/lock")
        );
    }

    #[test]
    fn paths_config_overrides_apply_before_the_root_prefix() {
        let overrides = BTreeMap::from([(
            "/var/lib/pkg/cache".to_string(),
            "/bigdisk/pkg-cache".to_string(),
        )]);
        // No root: the override is the path.
        assert_eq!(
            resolve(Path::new("/"), &overrides, "/var/lib/pkg/cache"),
            PathBuf::from("/bigdisk/pkg-cache")
        );
        // With PKG_ROOT: override first, prefix second.
        assert_eq!(
            resolve(Path::new("/tmp/pkg-e2e"), &overrides, "/var/lib/pkg/cache"),
            PathBuf::from("/tmp/pkg-e2e/bigdisk/pkg-cache")
        );
        // Unlisted paths pass through unchanged.
        assert_eq!(
            resolve(Path::new("/"), &overrides, "/var/lib/pkg/lock"),
            PathBuf::from("/var/lib/pkg/lock")
        );
    }
}
