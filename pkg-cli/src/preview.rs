//! pkg-cli/src/preview.rs — the `--dry-run` cascade preview (design doc
//! Issue 4.4.1).
//!
//! ## v1 changes
//!
//! - Initial implementation. The preview is *honest*: the caller has
//!   already run the full resolve and the interface-hash comparison
//!   before calling [`format_dry_run_preview`]; this module only owns
//!   the exact output format, kept as a pure function so the format is
//!   unit-testable byte-for-byte.

use std::fmt::Write as _;
use std::time::Duration;

/// Everything the preview needs, pre-computed by the resolve+cascade
/// pipeline.
#[derive(Debug, Clone, Default)]
pub struct DryRunPlan {
    /// Direct updates the user asked for: (name, new version).
    pub direct: Vec<(String, String)>,
    /// Packages rebuilt because a consumed link interface changed.
    pub rebuild_count: usize,
    /// The cascade root, when rebuilds are non-empty: (name, version).
    pub cascade_root: Option<(String, String)>,
    /// Packages the link-interface comparison pruned (Issue 4.1.1).
    pub pruned_count: usize,
    /// Critical-path wall-clock estimate (DAG-aware, Issue 4.2.4).
    pub wall: Duration,
    /// Sum of all per-package estimates (compile work).
    pub compile: Duration,
    /// Scheduler tokens (= cores) the estimate assumes.
    pub cores: u32,
    /// The slowest estimated packages, largest first: (name, eta).
    pub largest: Vec<(String, Duration)>,
}

/// `≈3 h 25 m`, `≈48 m`, `≈40 s` — human-scale durations.
pub fn format_eta(d: Duration) -> String {
    let secs = d.as_secs();
    if secs >= 3600 {
        let h = secs / 3600;
        let m = (secs % 3600) / 60;
        format!("≈{h} h {m:02} m")
    } else if secs >= 60 {
        format!("≈{} m", secs / 60)
    } else {
        format!("≈{secs} s")
    }
}

/// Compile-work totals render in hours with one decimal (`≈10.8 h`),
/// minutes below one hour.
fn format_work(d: Duration) -> String {
    let secs = d.as_secs();
    if secs >= 3600 {
        format!("≈{:.1} h", secs as f64 / 3600.0)
    } else {
        format!("≈{} m", secs / 60)
    }
}

/// The Issue 4.4.1 preview, verbatim structure:
///
/// ```text
/// upgrade plan: 3 direct updates, 412 rebuilds (cascade root: openssl 3.0.14)
///   link-interface pruning saved: 96 packages (interface unchanged)
///   predicted: ≈3 h 25 m wall / ≈10.8 h compile (your history, 8 cores)
///   largest: chromium (≈48 m), libreoffice (≈41 m), qtwebengine (≈36 m)
/// defer with: pkg upgrade --skip-rebuilds   schedule: pkg cascade --at 03:00
/// ```
pub fn format_dry_run_preview(plan: &DryRunPlan) -> String {
    let mut out = String::new();

    let mut header = format!(
        "upgrade plan: {} direct updates, {} rebuilds",
        plan.direct.len(),
        plan.rebuild_count
    );
    if let Some((name, version)) = &plan.cascade_root {
        let _ = write!(header, " (cascade root: {name} {version})");
    }
    out.push_str(&header);
    out.push('\n');

    if plan.pruned_count > 0 {
        let _ = writeln!(
            out,
            "  link-interface pruning saved: {} packages (interface unchanged)",
            plan.pruned_count
        );
    }

    let _ = writeln!(
        out,
        "  predicted: {} wall / {} compile (your history, {} cores)",
        format_eta(plan.wall),
        format_work(plan.compile),
        plan.cores
    );

    if !plan.largest.is_empty() {
        let largest = plan
            .largest
            .iter()
            .take(3)
            .map(|(name, eta)| format!("{name} ({})", format_eta(*eta)))
            .collect::<Vec<_>>()
            .join(", ");
        let _ = writeln!(out, "  largest: {largest}");
    }

    out.push_str(
        "defer with: pkg upgrade --skip-rebuilds   schedule: pkg cascade --at 03:00\n",
    );
    out
}

// ---------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dry_run_format_matches_issue_441() {
        let plan = DryRunPlan {
            direct: vec![
                ("openssl".into(), "3.0.14".into()),
                ("curl".into(), "8.6".into()),
                ("git".into(), "2.44".into()),
            ],
            rebuild_count: 412,
            cascade_root: Some(("openssl".into(), "3.0.14".into())),
            pruned_count: 96,
            wall: Duration::from_secs(3 * 3600 + 25 * 60),
            compile: Duration::from_secs(38880), // 10.8 h
            cores: 8,
            largest: vec![
                ("chromium".into(), Duration::from_secs(48 * 60)),
                ("libreoffice".into(), Duration::from_secs(41 * 60)),
                ("qtwebengine".into(), Duration::from_secs(36 * 60)),
            ],
        };
        let text = format_dry_run_preview(&plan);
        assert_eq!(
            text,
            "upgrade plan: 3 direct updates, 412 rebuilds (cascade root: openssl 3.0.14)\n\
             \x20 link-interface pruning saved: 96 packages (interface unchanged)\n\
             \x20 predicted: ≈3 h 25 m wall / ≈10.8 h compile (your history, 8 cores)\n\
             \x20 largest: chromium (≈48 m), libreoffice (≈41 m), qtwebengine (≈36 m)\n\
             defer with: pkg upgrade --skip-rebuilds   schedule: pkg cascade --at 03:00\n"
        );
    }

    #[test]
    fn dry_run_omits_pruning_line_when_zero() {
        let plan = DryRunPlan {
            direct: vec![("zlib".into(), "1.3.1".into())],
            rebuild_count: 0,
            pruned_count: 0,
            wall: Duration::from_secs(90),
            compile: Duration::from_secs(90),
            cores: 4,
            largest: vec![("zlib".into(), Duration::from_secs(90))],
            ..Default::default()
        };
        let text = format_dry_run_preview(&plan);
        assert!(!text.contains("pruning"));
        assert!(text.contains("≈1 m wall"));
        assert!(text.contains("4 cores"));
    }

    #[test]
    fn eta_scaling() {
        assert_eq!(format_eta(Duration::from_secs(30)), "≈30 s");
        assert_eq!(format_eta(Duration::from_secs(48 * 60)), "≈48 m");
        assert_eq!(format_eta(Duration::from_secs(3 * 3600 + 25 * 60)), "≈3 h 25 m");
        assert_eq!(format_work(Duration::from_secs(38880)), "≈10.8 h");
    }
}
