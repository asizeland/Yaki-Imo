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

/// Resolves an absolute-path `suffix` (e.g. `/var/lib/pkg/lock`)
/// against [`root()`]: prefixed when `PKG_ROOT` is set, unchanged when
/// it is not.
pub fn at(suffix: &str) -> PathBuf {
    join_root(root(), suffix)
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
}
