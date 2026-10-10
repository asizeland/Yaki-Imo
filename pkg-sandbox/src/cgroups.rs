//! ## v1 changes
//!
//! - Initial implementation (SPEC: pkg-sandbox; design doc Issue 4.2.2).

//! cgroup v2 resource limits for the build set, plus the logind-based
//! "is a user session active?" signal that gates the CPU ceiling.
//!
//! Issue 4.2.2's model: builds run in a dedicated cgroup v2 subtree
//! (`/sys/fs/cgroup/pkg-build/`), throttled by **weights, not quotas** —
//! under contention the cascade gets ≤ ~1/3 of CPU, on an idle machine
//! it still gets everything. The one hard quota (`cpu.max = 75%`)
//! applies *only while a user session is active*; it is lifted when the
//! desktop goes idle, so overnight builds run at full tilt.
//!
//! Everything here is **best-effort**: a system without cgroup v2
//! (containers, some VMs), a read-only cgroupfs, or no logind (OpenRC
//! without elogind) must degrade to "no limits" with warnings, never to
//! a failed build. Resource limits protect the *desktop experience*;
//! they are not part of the sandbox's security boundary.

use std::fs;
use std::path::{Path, PathBuf};

use pkg_core::PkgResult;

/// The cgroup v2 subtree all pkg build processes run under
/// (Issue 4.2.2).
pub const BUILD_CGROUP_ROOT: &str = "/sys/fs/cgroup/pkg-build";

/// The cgroup v2 `cpu.max` period (microseconds) — the denominator of
/// the bandwidth quota. 100 ms is the kernel's default period and what
/// the "75% of CPU" phrasing in Issue 4.2.2 is naturally expressed
/// against.
pub const CPU_MAX_PERIOD_US: u32 = 100_000;

/// Resource limits applied to the build cgroup subtree (SPEC:
/// pkg-sandbox; Issue 4.2.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CgroupLimits {
    /// `cpu.weight` (1..=10000, default 100). 50 ⇒ under contention the
    /// build set gets at most ~1/3 of CPU vs. default-weight work, but
    /// *all* of an idle machine. This weight — not a quota — is the
    /// entire trick per Issue 4.2.2.
    pub cpu_weight: u32,
    /// Hard CPU ceiling (as a percentage 1..=100) applied via `cpu.max`
    /// *only while a user session is active* ([`session_active`]); the
    /// ceiling is lifted (`cpu.max = "max"`) when the desktop is idle.
    /// `None` disables the ceiling entirely.
    pub cpu_max_active_session: Option<u32>,
    /// `io.weight` (1..=10000, default 100) — same weights-not-quotas
    /// model for block I/O.
    pub io_weight: u32,
    /// `memory.high` in bytes: the throttle (not kill) threshold. The
    /// SPEC default is `total_ram - 2GiB`, so the build set can never
    /// push the desktop into swap; `None` leaves it untouched.
    pub memory_high: Option<u64>,
}

impl Default for CgroupLimits {
    /// The Issue 4.2.2 desktop defaults: weights 50/50, a 75% CPU
    /// ceiling active only during session activity, and `memory.high`
    /// at `total_ram - 2GiB` (computed best-effort from
    /// `/proc/meminfo`; `None` where that isn't readable).
    fn default() -> Self {
        CgroupLimits {
            cpu_weight: 50,
            cpu_max_active_session: Some(75),
            io_weight: 50,
            memory_high: default_memory_high(),
        }
    }
}

/// `total_ram - 2GiB` read from `/proc/meminfo`'s `MemTotal`, or `None`
/// where that's unavailable (non-Linux, containers without meminfo).
/// Saturating: a machine with ≤ 2GiB RAM gets `None` (no memory.high
/// written) rather than a nonsense zero limit.
fn default_memory_high() -> Option<u64> {
    let total = meminfo_mem_total_bytes().ok()?;
    const TWO_GIB: u64 = 2 * 1024 * 1024 * 1024;
    total.checked_sub(TWO_GIB)
}

/// Parses `MemTotal` (reported in KiB) out of `/proc/meminfo`.
fn meminfo_mem_total_bytes() -> PkgResult<u64> {
    let text = fs::read_to_string("/proc/meminfo")?;
    parse_mem_total(&text).ok_or_else(|| {
        pkg_core::PkgError::Parse("/proc/meminfo: no MemTotal line".to_string())
    })
}

/// Pure parser for the `MemTotal:   32768 kB` line — the unit the tests
/// exercise.
fn parse_mem_total(meminfo: &str) -> Option<u64> {
    for line in meminfo.lines() {
        let Some(rest) = line.strip_prefix("MemTotal:") else {
            continue;
        };
        let mut fields = rest.split_whitespace();
        let kib: u64 = fields.next()?.parse().ok()?;
        return Some(kib * 1024);
    }
    None
}

/// One non-fatal failure from [`apply_cgroup`] — which control file
/// couldn't be written and why. Returned in a list rather than raised
/// because cgroup application is best-effort by design (module docs).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CgroupWarning {
    /// The control file that couldn't be written (e.g.
    /// `/sys/fs/cgroup/pkg-build/cpu.max`).
    pub file: PathBuf,
    /// What went wrong (I/O error text, or a policy note like "cgroup
    /// v2 not mounted").
    pub reason: String,
}

impl std::fmt::Display for CgroupWarning {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "cgroup: {}: {}", self.file.display(), self.reason)
    }
}

/// Applies `limits` to the cgroup v2 subtree at `path`, creating it if
/// needed. Best-effort per the module contract: every write failure
/// becomes a [`CgroupWarning`] in the returned list (empty = fully
/// applied), never an `Err`.
///
/// `path` is a parameter (rather than hardcoded to
/// [`BUILD_CGROUP_ROOT`]) so tests and non-default layouts can point it
/// at a tmpdir; production callers pass `BUILD_CGROUP_ROOT`.
///
/// The `cpu.max` ceiling depends on live session state:
/// [`session_active`] decides between `"<pct>% of period"` and lifting
/// the ceiling to `"max"`. Because session activity changes while a
/// cascade runs, the caller (pkg-build's scheduler) is expected to
/// re-invoke this periodically — application is idempotent.
pub fn apply_cgroup(path: &Path, limits: &CgroupLimits) -> Vec<CgroupWarning> {
    let mut warnings = Vec::new();

    // Create the subtree (and its parents) if needed. Failing here is
    // not terminal: the writes below will each produce their own
    // warnings, which are more actionable than one mkdir error.
    if let Err(e) = fs::create_dir_all(path) {
        warnings.push(CgroupWarning {
            file: path.to_path_buf(),
            reason: format!("cannot create cgroup subtree: {e}"),
        });
    }

    write_control(path, "cpu.weight", &limits.cpu_weight.to_string(), &mut warnings);
    write_control(path, "io.weight", &limits.io_weight.to_string(), &mut warnings);

    match limits.cpu_max_active_session {
        Some(pct) if session_active() => {
            let pct = pct.min(100);
            let quota = CPU_MAX_PERIOD_US as u64 * pct as u64 / 100;
            write_control(
                path,
                "cpu.max",
                &format!("{quota} {CPU_MAX_PERIOD_US}"),
                &mut warnings,
            );
        }
        Some(_) => {
            // Session idle: lift the ceiling so the build set gets the
            // whole machine (Issue 4.2.2).
            write_control(path, "cpu.max", "max", &mut warnings);
        }
        None => {}
    }

    if let Some(memory_high) = limits.memory_high {
        write_control(path, "memory.high", &memory_high.to_string(), &mut warnings);
    }

    warnings
}

/// Writes one cgroup control file, recording a warning on failure.
fn write_control(
    dir: &Path,
    name: &str,
    value: &str,
    warnings: &mut Vec<CgroupWarning>,
) {
    let file = dir.join(name);
    if let Err(e) = fs::write(&file, value) {
        warnings.push(CgroupWarning {
            file,
            reason: e.to_string(),
        });
    }
}

/// Whether a user session is currently *active* — the signal Issue
/// 4.2.2's conditional `cpu.max` ceiling keys on.
///
/// Best-effort, std-only (no D-Bus dependency — the crate pins forbid
/// one). The true logind `IdleHint` is only exposed over D-Bus; what we
/// can read from the filesystem is logind's session state under
/// `/run/systemd/sessions/`, where each session file carries an
/// `ACTIVE=0|1` field. We report "active" iff at least one session is
/// marked active.
///
/// Fallback chain (documented contract):
///
/// 1. `/run/systemd/sessions/` readable and non-empty → parse
///    `ACTIVE=` fields (this also covers elogind, which writes the same
///    layout).
/// 2. `/run/systemd/` absent entirely (OpenRC without elogind, or any
///    non-logind init) **or** `/run/dbus/system_bus_socket` missing
///    (no system bus ⇒ no logind even if the layout exists) → return
///    `false`: **assume idle**. The failure direction is deliberate —
///    "assume idle" lifts the CPU ceiling (builds get full speed), and
///    the weight-based limits still protect responsiveness. The
///    alternative (assume active) would permanently cap builds at 75%
///    on every non-logind system, punishing exactly the users who chose
///    a minimal init.
/// 3. Sessions directory exists but is empty → no sessions at all →
///    idle (`false`).
pub fn session_active() -> bool {
    let sessions_dir = Path::new("/run/systemd/sessions");
    if !sessions_dir.is_dir() {
        return false;
    }
    if !Path::new("/run/dbus/system_bus_socket").exists() {
        // No system bus: whatever left /run/systemd/sessions behind is
        // stale; logind isn't there to keep it current.
        return false;
    }
    let Ok(entries) = fs::read_dir(sessions_dir) else {
        return false;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_some_and(|e| e == "ref") {
            continue; // "<id>.ref" companion files carry no state
        }
        let Ok(text) = fs::read_to_string(&path) else {
            continue;
        };
        if session_file_active(&text) {
            return true;
        }
    }
    false
}

/// Parses one `/run/systemd/sessions/<id>` file: active iff it carries
/// an `ACTIVE=1` line. Pure — the unit the tests exercise.
fn session_file_active(text: &str) -> bool {
    text.lines().any(|line| line.trim() == "ACTIVE=1")
}

// ---------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_issue_4_2_2() {
        let limits = CgroupLimits::default();
        assert_eq!(limits.cpu_weight, 50);
        assert_eq!(limits.cpu_max_active_session, Some(75));
        assert_eq!(limits.io_weight, 50);
        // memory.high is total_ram - 2GiB where meminfo is readable;
        // on this host that's Some — assert only that it's sane
        // (strictly positive) rather than pinning the machine's RAM.
        if let Some(mh) = limits.memory_high {
            assert!(mh > 0);
        }
    }

    #[test]
    fn parse_mem_total_reads_the_kib_line() {
        let meminfo = "MemTotal:       32768000 kB\nMemFree:         1000000 kB\n";
        assert_eq!(parse_mem_total(meminfo), Some(32768000 * 1024));
        assert_eq!(parse_mem_total("MemFree: 1 kB\n"), None);
    }

    #[test]
    fn memory_high_saturates_below_2gib_ram() {
        // The total_ram - 2GiB default is a checked subtraction: a
        // machine with less than 2GiB gets None (no memory.high
        // written), never an underflowed nonsense value.
        let one_gib: u64 = 1024 * 1024 * 1024;
        assert_eq!(one_gib.checked_sub(2 * 1024 * 1024 * 1024), None);
        let four_gib: u64 = 4 * 1024 * 1024 * 1024;
        assert_eq!(
            four_gib.checked_sub(2 * 1024 * 1024 * 1024),
            Some(2 * 1024 * 1024 * 1024)
        );
    }

    /// A unique scratch directory under the system temp dir. Used
    /// instead of the `tempfile` crate: pkg-sandbox's dependency pin is
    /// "pkg-core only" (SPEC), with no dev-dependencies listed, so
    /// tests make do with std.
    fn scratch_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "pkg-sandbox-test-{}-{}",
            std::process::id(),
            tag
        ));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn apply_cgroup_writes_control_files_best_effort() {
        // Pointed at a scratch dir, apply_cgroup must write the control
        // files it can and report none it couldn't.
        let dir = scratch_dir("apply");
        let limits = CgroupLimits {
            cpu_weight: 50,
            cpu_max_active_session: None, // None: no session probing in tests
            io_weight: 50,
            memory_high: Some(12345),
        };
        let warnings = apply_cgroup(&dir, &limits);
        assert!(warnings.is_empty(), "unexpected warnings: {warnings:?}");
        assert_eq!(fs::read_to_string(dir.join("cpu.weight")).unwrap(), "50");
        assert_eq!(fs::read_to_string(dir.join("io.weight")).unwrap(), "50");
        assert_eq!(fs::read_to_string(dir.join("memory.high")).unwrap(), "12345");
        // cpu.max untouched when the ceiling is disabled.
        assert!(!dir.join("cpu.max").exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn apply_cgroup_reports_unwritable_path_as_warning_not_error() {
        // A path that cannot be created (parent is a *file*) must
        // yield warnings, not a panic or an Err.
        let anchor = scratch_dir("anchor");
        fs::write(&anchor, b"not a directory").unwrap();
        let impossible = anchor.join("pkg-build");
        let limits = CgroupLimits {
            cpu_weight: 50,
            cpu_max_active_session: None,
            io_weight: 50,
            memory_high: Some(1),
        };
        let warnings = apply_cgroup(&impossible, &limits);
        assert!(!warnings.is_empty());
        assert!(warnings
            .iter()
            .all(|w| w.file.starts_with(&impossible) || w.file == impossible));
        let _ = fs::remove_file(&anchor);
    }

    #[test]
    fn cpu_max_quota_math() {
        // 75% of the 100ms period, written when a session is active.
        // Pure arithmetic checked directly rather than through
        // apply_cgroup (which depends on host session state).
        let quota = CPU_MAX_PERIOD_US as u64 * 75u64 / 100;
        assert_eq!(format!("{quota} {CPU_MAX_PERIOD_US}"), "75000 100000");
    }

    #[test]
    fn session_file_parser() {
        assert!(session_file_active("UID=1000\nACTIVE=1\nSTATE=active\n"));
        assert!(!session_file_active("UID=1000\nACTIVE=0\nSTATE=online\n"));
        assert!(!session_file_active("garbage"));
    }

    #[test]
    fn session_active_is_false_without_logind() {
        // Whatever the host, session_active must not panic; on a
        // logind-less host it must be false (assume idle). Where logind
        // *is* present the result is environment-dependent and not
        // asserted.
        if !Path::new("/run/systemd/sessions").is_dir() {
            assert!(!session_active());
        }
    }
}
