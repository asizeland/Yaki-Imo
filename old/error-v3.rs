//! ## v3 changes
//!
//! - [SPEC v13 §4] New variant `GlibcPending { required, pending_target }`:
//!   a package requires the glibc major version currently staged and
//!   flagged pending (reboot-required). Raised by pkg-db's pre-build gate
//!   (`check_glibc_gate`) so the failure surfaces before a multi-hour
//!   source build. Exit code falls through to the generic 70 — the
//!   install didn't fail to *resolve*; it was refused by policy.
//!
//! ## v2 changes
//!
//! - [MINOR] Exit-code collision: the generic `_` fallback in
//!   `PkgError::exit_code` previously returned `1`, indistinguishable from
//!   a resolution failure (which the spec assigns exit code 1). The
//!   fallback is now `70` (`EX_SOFTWARE` from sysexits.h), so scripts and
//!   CI can tell "dependencies couldn't be resolved" apart from
//!   "something else went wrong" (I/O, lock contention, not-found, db).
//!   The spec-assigned mappings 1 (resolution), 2 (build), 3 (gate) are
//!   unchanged.

//! Shared error type used across the workspace.
//!
//! Subsystem-specific failures — a malformed manifest, an unsatisfiable
//! PubGrub resolution, a scanner gate rejection — all eventually funnel
//! into one of these variants by the time they reach `pkg-cli`. That's
//! what lets the CLI's dispatch loop turn *any* error, from *any*
//! subsystem, into the right process exit code (§16.1) without needing to
//! know the specifics of where the error originated.

use std::fmt;
use std::io;

/// The umbrella error type most workspace crates return from fallible
/// operations.
///
/// A few variants (`Parse`, `Build`, `Gate`, `NotFound`) carry a plain
/// `String` rather than a nested error type. That's a real trade-off, not
/// an oversight: a `String` is easy to construct from any subsystem
/// without that subsystem needing to depend on `pkg-core`'s error
/// internals, at the cost of losing structured access to *why* — callers
/// can display the message but can't match on specifics the way they can
/// with `Io`'s wrapped `io::Error`. If a later crate needs to branch on
/// the reason a build failed, that's the sign to give `Build` its own
/// nested error type instead of a string, the same way `Io` already has one.
#[derive(Debug)]
pub enum PkgError {
    /// Filesystem or other I/O failure. Wraps the original `io::Error`
    /// rather than stringifying it immediately, so callers further up the
    /// chain can still inspect the underlying `io::ErrorKind` if needed.
    Io(io::Error),

    /// A manifest, config file, or repository index failed to parse.
    /// Shared across formats (TOML manifests, `pins.toml`, the JSON
    /// repository index) rather than split per-format, since the caller
    /// usually just needs to report *that* parsing failed and where.
    Parse(String),

    /// Dependency resolution could not produce a satisfying graph (§5).
    /// Corresponds to exit code 1.
    Resolution(String),

    /// A source or binary build failed (§7). Corresponds to exit code 2.
    Build(String),

    /// The binary scanner's gate rejected a build (§8). Corresponds to
    /// exit code 3.
    Gate(String),

    /// Something was requested by name (a package, a pin, a profile) that
    /// doesn't exist.
    NotFound(String),

    /// `/var/lib/pkg/lock` (§9.4) could not be acquired within the
    /// configured timeout. Carries the holding process's PID and how long
    /// it's held the lock, matching the message shown in §9.4.
    Locked { holder_pid: u32, elapsed_secs: u64 },

    /// A database-layer failure (§9) — a query, migration, or constraint
    /// violation. Kept as a generic `String` rather than naming a
    /// specific database library here: `pkg-core` doesn't depend on
    /// whatever `pkg-db` uses internally (`rusqlite`, as it happens), and
    /// shouldn't need to just to describe "the database layer failed."
    /// `pkg-db` converts its own errors into this variant explicitly at
    /// its API boundary, rather than via a `From` impl — the orphan rule
    /// (§ see `pkg-db/src/schema.rs`) doesn't allow one crate to `impl
    /// From<ForeignError> for ForeignType` when *both* types belong to
    /// other crates, which is exactly the situation `rusqlite::Error` and
    /// `PkgError` are in from `pkg-db`'s point of view.
    Db(String),

    /// A package requires the glibc major version that is currently
    /// staged in `/usr/lib/pkg/.stage/glibc-active/` and flagged pending
    /// in `/var/lib/pkg/glibc-pending`, but not yet applied (§7.6, v13 §4).
    /// Raised by `pkg-db`'s pre-build gate so the failure happens before a
    /// multi-hour source build rather than at staging time. Carries the
    /// required floor and the pending target so the CLI can render the
    /// spec's message without re-querying the DB.
    ///
    /// Added in error-v3; closes the TODO left in queries-v3.rs, which
    /// used `PkgError::Resolution` as an interim carrier.
    GlibcPending { required: String, pending_target: String },
}

impl PkgError {
    /// Maps this error to the process exit code `pkg-cli` should return
    /// (§16.1: 1 = resolution failure, 2 = build failure, 3 = gate
    /// failure). Only those three have a code the spec assigns
    /// specifically; everything else — I/O problems, a bad pin, lock
    /// contention — falls back to `70` (`EX_SOFTWARE` from sysexits.h:
    /// "an internal software error has been detected"). The fallback used
    /// to be `1`, but that collided with resolution failure, making it
    /// impossible for callers to distinguish "the resolver found no
    /// solution" from "something unrelated broke" without parsing the
    /// error message. If a future revision of the spec assigns dedicated
    /// codes to any of those, this is the one place that needs to change.
    pub fn exit_code(&self) -> i32 {
        match self {
            PkgError::Resolution(_) => 1,
            PkgError::Build(_) => 2,
            PkgError::Gate(_) => 3,
            _ => 70,
        }
    }
}

impl fmt::Display for PkgError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PkgError::Io(e) => write!(f, "I/O error: {e}"),
            PkgError::Parse(msg) => write!(f, "parse error: {msg}"),
            PkgError::Resolution(msg) => write!(f, "dependency resolution failed: {msg}"),
            PkgError::Build(msg) => write!(f, "build failed: {msg}"),
            PkgError::Gate(msg) => write!(f, "scanner gate rejected build: {msg}"),
            PkgError::GlibcPending { required, pending_target } => write!(
                f,
                "package requires glibc {required}, but {pending_target} is staged \
                 pending reboot. Install an older version or reboot to apply."
            ),
            PkgError::NotFound(msg) => write!(f, "not found: {msg}"),
            PkgError::Locked {
                holder_pid,
                elapsed_secs,
            } => write!(
                f,
                "another pkg operation is in progress (pid {holder_pid}, started {elapsed_secs}s ago)"
            ),
            PkgError::Db(msg) => write!(f, "database error: {msg}"),
        }
    }
}

/// Implementing `std::error::Error` (rather than just `Display`) is what
/// makes `PkgError` a "real" error type as far as the rest of the
/// ecosystem is concerned — it's the trait `Box<dyn Error>`, `anyhow`, and
/// `?`-based error handling all expect. The one method worth overriding
/// is `source()`: it exposes the *original* `io::Error` underneath an
/// `Io` variant, so something printing an error chain (`error: I/O error:
/// ... caused by: permission denied`) has something to walk. The other
/// variants have no deeper cause to expose — they're already the root
/// cause — so `source()` returns `None` for them via the wildcard arm.
impl std::error::Error for PkgError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            PkgError::Io(e) => Some(e),
            _ => None,
        }
    }
}

/// `From<io::Error>` is what lets a plain `?` on any `io::Error`-returning
/// call inside a function that returns `PkgResult<T>` just work, without
/// an explicit `.map_err(...)` at every call site. This is the same
/// mechanism `Version`'s parser would use if it needed to read a file
/// rather than parse an in-memory string.
impl From<io::Error> for PkgError {
    fn from(e: io::Error) -> Self {
        PkgError::Io(e)
    }
}

/// Convenience alias, mirroring `std::io::Result`. Spelled out as
/// `PkgResult` rather than a bare `Result` so it can't silently shadow
/// `std::result::Result` for anything that does `use pkg_core::*;` — an
/// easy source of confusion if the two ever need to appear in the same
/// scope.
pub type PkgResult<T> = std::result::Result<T, PkgError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn io_errors_convert_via_question_mark() {
        fn read_missing_file() -> PkgResult<String> {
            let contents = std::fs::read_to_string("/nonexistent/path/pkg-test")?;
            Ok(contents)
        }
        let err = read_missing_file().unwrap_err();
        assert!(matches!(err, PkgError::Io(_)));
    }

    #[test]
    fn io_variant_exposes_source() {
        use std::error::Error;
        let io_err = io::Error::new(io::ErrorKind::NotFound, "missing");
        let err = PkgError::Io(io_err);
        assert!(err.source().is_some());
    }

    #[test]
    fn other_variants_have_no_source() {
        use std::error::Error;
        let err = PkgError::NotFound("openssl".to_string());
        assert!(err.source().is_none());
    }

    #[test]
    fn exit_codes_match_spec() {
        assert_eq!(PkgError::Resolution("x".into()).exit_code(), 1);
        assert_eq!(PkgError::Build("x".into()).exit_code(), 2);
        assert_eq!(PkgError::Gate("x".into()).exit_code(), 3);
    }

    #[test]
    fn generic_fallback_uses_ex_software_not_resolution_code() {
        // 70 = EX_SOFTWARE (sysexits.h). Anything not assigned a
        // spec-specific code must not collide with 1/2/3 — most
        // importantly not with 1 (resolution failure), which the old
        // `_ => 1` fallback collided with.
        for err in [
            PkgError::Io(io::Error::new(io::ErrorKind::Other, "boom")),
            PkgError::Parse("x".into()),
            PkgError::NotFound("x".into()),
            PkgError::Locked {
                holder_pid: 1,
                elapsed_secs: 0,
            },
            PkgError::Db("x".into()),
        ] {
            assert_eq!(err.exit_code(), 70);
        }
    }

    #[test]
    fn locked_display_matches_spec_wording() {
        let err = PkgError::Locked {
            holder_pid: 4821,
            elapsed_secs: 12,
        };
        assert_eq!(
            err.to_string(),
            "another pkg operation is in progress (pid 4821, started 12s ago)"
        );
    }

    #[test]
    fn db_errors_have_no_source_but_carry_a_message() {
        use std::error::Error;
        let err = PkgError::Db("UNIQUE constraint failed: packages.name".to_string());
        assert!(err.source().is_none());
        assert_eq!(
            err.to_string(),
            "database error: UNIQUE constraint failed: packages.name"
        );
        assert_eq!(err.exit_code(), 70);
    }
}
