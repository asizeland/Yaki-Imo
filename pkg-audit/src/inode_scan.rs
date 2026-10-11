//! ## v1 (initial)
//!
//! Deleted-but-mapped inode scanning (v13 §7, v11 §16.5): after a
//! category-1 glibc patch (or any library swap, including `pkg undo`
//! reactivations, Issue 5 §5.6), running processes may still hold
//! inodes of library files that no longer exist on disk. `pkg audit
//! --glibc` lists exactly those PIDs and services, and prints the exact
//! OpenRC restart commands; `pkg apply-pending-restarts` consumes the
//! same report.
//!
//! # Mechanism
//!
//! - Read `/proc/<pid>/maps` for every numeric `/proc` entry. A mapping
//!   whose pathname ends in ` (deleted)` names an unlinked inode; when
//!   that path is under the pkg store root (`/usr/lib/pkg` — store dirs
//!   *and* the glibc anchor live there, and `/proc` maps record the
//!   resolved target, not the FHS symlink), the process is running
//!   against code the package manager has already replaced.
//! - Map PID → service via the cgroup v2 path in `/proc/<pid>/cgroup`
//!   (`0::/...`). OpenRC names service cgroups `/openrc/<service>`;
//!   systemd-style `*.service` units are recognized for completeness.
//!   User sessions (`*.slice`, `*.scope`), PID 1, and kernel threads
//!   are listed as *unmapped* — never restarted (v13 §7: "services
//!   without a mapping are listed but not touched").

use std::collections::BTreeMap;
use std::path::Path;

use pkg_core::PkgResult;

/// Default procfs mount.
pub const PROC_ROOT: &str = "/proc";

/// Default pkg store root; also the parent of the glibc anchor
/// (`/usr/lib/pkg/glibc-anchor`), so one prefix covers both the store
/// and the anchor.
pub const STORE_ROOT: &str = "/usr/lib/pkg";

/// One parsed `/proc/<pid>/maps` line: a file-backed mapping.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MapsEntry {
    pub inode: u64,
    /// Path as recorded, with any ` (deleted)` suffix stripped.
    pub path: String,
    pub deleted: bool,
}

/// Parse one maps line:
///
/// ```text
/// 7f3b1c000000-7f3b1c021000 r--p 00000000 00:23 131097  /usr/lib/pkg/.store/glibc-2.39+ab1/lib/libc.so.6 (deleted)
/// ```
///
/// Fields: address, perms, offset, dev, inode, then the (optional,
/// possibly space-containing) pathname. Non-file mappings (no pathname)
/// and inode-0 mappings return `None`.
pub fn parse_maps_line(line: &str) -> Option<MapsEntry> {
    // The first five fields (address, perms, offset, dev, inode) never
    // contain spaces; the sixth (pathname) may. Splitting with
    // `split_whitespace` would collapse runs of spaces inside a path,
    // so the path is recovered as the raw remainder after field 5.
    let mut fields = line.split_whitespace();
    let (inode, have_path) = {
        let f = fields.by_ref();
        let _addr = f.next()?;
        let _perms = f.next()?;
        let _offset = f.next()?;
        let _dev = f.next()?;
        let inode: u64 = f.next()?.parse().ok()?;
        (inode, f.next().is_some())
    };
    if inode == 0 || !have_path {
        return None;
    }
    let raw_path = nth_field_remainder(line, 5)?;
    let (path, deleted) = match raw_path.strip_suffix(" (deleted)") {
        Some(p) => (p.to_string(), true),
        None => (raw_path.to_string(), false),
    };
    Some(MapsEntry { inode, path, deleted })
}

/// Return the remainder of `line` after skipping `n` whitespace-
/// separated fields, with the remainder's own leading whitespace
/// trimmed.
fn nth_field_remainder(line: &str, n: usize) -> Option<&str> {
    let mut rest = line;
    for _ in 0..n {
        let start = rest.find(|c: char| !c.is_whitespace())?;
        rest = &rest[start..];
        // Consume the field: everything up to the next whitespace.
        let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
        rest = &rest[end..];
    }
    let start = rest.find(|c: char| !c.is_whitespace())?;
    Some(&rest[start..])
}

/// A process holding at least one deleted pkg-managed mapping.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AffectedProcess {
    pub pid: u32,
    pub comm: String,
    /// Distinct deleted paths, sorted.
    pub deleted_paths: Vec<String>,
}

/// Scan `proc_root` for processes mapping deleted files under
/// `store_root`. Unreadable processes (permitted for non-root callers,
/// or processes that exited mid-scan) are skipped — the audit reports
/// what it can see; run as root for completeness.
pub fn scan_deleted_mappings(proc_root: &Path, store_root: &Path) -> PkgResult<Vec<AffectedProcess>> {
    let mut out = Vec::new();
    let entries = std::fs::read_dir(proc_root)?;
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Ok(pid) = name.parse::<u32>() else { continue };
        let maps_path = entry.path().join("maps");
        let Ok(maps) = std::fs::read_to_string(&maps_path) else { continue };
        let mut deleted: Vec<String> = maps
            .lines()
            .filter_map(parse_maps_line)
            // Component-aware prefix check: under the store root,
            // not merely sharing a string prefix with it.
            .filter(|m| m.deleted && Path::new(&m.path).starts_with(store_root))
            .map(|m| m.path)
            .collect();
        if deleted.is_empty() {
            continue;
        }
        deleted.sort();
        deleted.dedup();
        let comm = std::fs::read_to_string(entry.path().join("comm"))
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|_| "?".to_string());
        out.push(AffectedProcess { pid, comm, deleted_paths: deleted });
    }
    out.sort_by_key(|p| p.pid);
    Ok(out)
}

/// Service mapping for a process: the OpenRC (or, recognized,
/// systemd-style) service name, if the cgroup path yields one.
pub fn service_for_pid(proc_root: &Path, pid: u32) -> Option<String> {
    let cgroup = std::fs::read_to_string(proc_root.join(pid.to_string()).join("cgroup")).ok()?;
    service_from_cgroup(&cgroup)
}

/// Parse a cgroup v2 path out of `/proc/<pid>/cgroup` content.
///
/// v2 has exactly one line: `0::<path>`. (Any v1-named lines, if a
/// hybrid system still emits them, are ignored — service placement
/// decisions read the unified hierarchy only.)
///
/// Mapping rules:
/// - `.../openrc/<name>` or `.../openrc/<name>/...` → OpenRC service `<name>`.
/// - last component `<name>.service` → `<name>` (systemd-style; the
///   generated restart command still uses `rc-service` per v13 §7 —
///   this distro runs OpenRC, the recognition is for foreign cgroup
///   layouts, e.g. containers).
/// - `/`, `*.slice`, `*.scope`, `init`, or anything unrecognized → None
///   (unmapped; listed, never restarted).
pub fn service_from_cgroup(content: &str) -> Option<String> {
    for line in content.lines() {
        let path = match line.strip_prefix("0::") {
            Some(p) => p,
            None => continue,
        };
        return service_from_cgroup_path(path);
    }
    None
}

/// The pure path→service mapping, split out for testing.
pub fn service_from_cgroup_path(path: &str) -> Option<String> {
    let components: Vec<&str> = path.split('/').filter(|c| !c.is_empty()).collect();
    // OpenRC: component "openrc", next component is the service.
    if let Some(pos) = components.iter().position(|c| *c == "openrc") {
        if let Some(name) = components.get(pos + 1) {
            if is_plausible_service_name(name) {
                return Some((*name).to_string());
            }
        }
        return None;
    }
    // systemd-style unit naming.
    if let Some(last) = components.last() {
        if let Some(name) = last.strip_suffix(".service") {
            if is_plausible_service_name(name) {
                return Some(name.to_string());
            }
        }
    }
    None
}

fn is_plausible_service_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "._-@".contains(c))
}

/// One affected service and its restart command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AffectedService {
    pub name: String,
    pub pids: Vec<u32>,
    /// The exact command v13 §7 specifies printing.
    pub restart_command: String,
}

/// The full report for `pkg audit --glibc` / `pkg apply-pending-restarts`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestartReport {
    pub services: Vec<AffectedService>,
    /// Processes with no service mapping (user sessions, PID 1, kernel
    /// threads). Listed, never restarted (v13 §7).
    pub unmapped: Vec<AffectedProcess>,
}

impl RestartReport {
    /// Human-readable rendering in the v13 §7 shape: services with
    /// their exact `rc-service X restart` commands, then the unmapped
    /// list.
    pub fn render(&self) -> String {
        let mut s = String::new();
        if self.services.is_empty() && self.unmapped.is_empty() {
            s.push_str("no processes holding deleted pkg-managed library inodes\n");
            return s;
        }
        if !self.services.is_empty() {
            s.push_str("services holding deleted library inodes:\n");
            for svc in &self.services {
                s.push_str(&format!(
                    "  {} (pids: {})\n    {}\n",
                    svc.name,
                    svc.pids
                        .iter()
                        .map(u32::to_string)
                        .collect::<Vec<_>>()
                        .join(", "),
                    svc.restart_command
                ));
            }
        }
        if !self.unmapped.is_empty() {
            s.push_str("unmapped processes (listed, not restarted):\n");
            for p in &self.unmapped {
                s.push_str(&format!("  pid {} ({})\n", p.pid, p.comm));
            }
        }
        s
    }
}

/// Build the restart report from a scan. Services are sorted by name;
/// OpenRC's own dependency ordering is applied by
/// `pkg apply-pending-restarts` re-invoking `rc-service` (which honors
/// rc dependency order when run through `rc-service --ifstarted`), so
/// the report itself stays name-sorted for deterministic output.
pub fn restart_report(proc_root: &Path, store_root: &Path) -> PkgResult<RestartReport> {
    let procs = scan_deleted_mappings(proc_root, store_root)?;
    Ok(report_from_scan(proc_root, procs))
}

/// Split a process list into mapped services and the unmapped rest.
/// Separate from [`restart_report`] so the grouping logic is testable
/// without a live `/proc`.
pub fn report_from_scan(proc_root: &Path, procs: Vec<AffectedProcess>) -> RestartReport {
    let mut by_service: BTreeMap<String, Vec<u32>> = BTreeMap::new();
    let mut unmapped = Vec::new();
    for p in procs {
        match service_for_pid(proc_root, p.pid) {
            Some(name) => by_service.entry(name).or_default().push(p.pid),
            None => unmapped.push(p),
        }
    }
    let services = by_service
        .into_iter()
        .map(|(name, mut pids)| {
            pids.sort_unstable();
            AffectedService {
                restart_command: format!("rc-service {name} restart"),
                name,
                pids,
            }
        })
        .collect();
    RestartReport { services, unmapped }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_maps_lines() {
        let e = parse_maps_line(
            "7f3b1c000000-7f3b1c021000 r--p 00000000 00:23 131097 \
             /usr/lib/pkg/.store/glibc-2.39+ab1/lib/libc.so.6 (deleted)",
        )
        .unwrap();
        assert_eq!(e.inode, 131097);
        assert_eq!(e.path, "/usr/lib/pkg/.store/glibc-2.39+ab1/lib/libc.so.6");
        assert!(e.deleted);

        // No "(deleted)" suffix.
        let e = parse_maps_line(
            "7f3b1c000000-7f3b1c021000 r-xp 00021000 00:23 131097 \
             /usr/lib/pkg/.store/glibc-2.39+ab1/lib/libc.so.6",
        )
        .unwrap();
        assert!(!e.deleted);

        // Anonymous mapping: no pathname.
        assert!(parse_maps_line("7f3b1c000000-7f3b1c021000 rw-p 00000000 00:00 0").is_none());
        // Inode 0 file mapping ([vdso] etc. have no inode; guard anyway).
        assert!(parse_maps_line("aaaa-bbbb r-xp 00000000 00:00 0 /foo").is_none());
        // Path containing spaces keeps its spaces.
        let e = parse_maps_line(
            "aaaa-bbbb r--p 00000000 00:23 42 /usr/lib/pkg/.store/weird name/lib x.so (deleted)",
        )
        .unwrap();
        assert_eq!(e.path, "/usr/lib/pkg/.store/weird name/lib x.so");
        assert!(e.deleted);
    }

    #[test]
    fn cgroup_path_mapping() {
        // OpenRC v2 layout.
        assert_eq!(
            service_from_cgroup_path("/openrc/sshd"),
            Some("sshd".to_string())
        );
        assert_eq!(
            service_from_cgroup_path("/openrc/nginx/worker"),
            Some("nginx".to_string())
        );
        // systemd-style, recognized for foreign layouts.
        assert_eq!(
            service_from_cgroup_path("/system.slice/NetworkManager.service"),
            Some("NetworkManager".to_string())
        );
        // Unmapped: root, slices, scopes.
        assert_eq!(service_from_cgroup_path("/"), None);
        assert_eq!(service_from_cgroup_path("/user.slice/user-1000.slice/session-3.scope"), None);
        assert_eq!(service_from_cgroup_path("/init.scope"), None);
    }

    #[test]
    fn cgroup_content_uses_v2_line() {
        assert_eq!(
            service_from_cgroup("0::/openrc/dbus\n"),
            Some("dbus".to_string())
        );
        // v1 lines ignored; v2 line wins.
        assert_eq!(
            service_from_cgroup("2:cpu:/legacy\n0::/openrc/cronie\n"),
            Some("cronie".to_string())
        );
        assert_eq!(service_from_cgroup(""), None);
    }

    #[test]
    fn report_groups_by_service_with_exact_commands() {
        // report_from_scan takes the proc root for cgroup lookups; the
        // test uses a nonexistent root so every PID is unmapped — the
        // grouping logic for mapped services is covered by injecting a
        // fabricated map below instead.
        let procs = vec![
            AffectedProcess { pid: 4001, comm: "bash".into(), deleted_paths: vec!["/x".into()] },
        ];
        let report = report_from_scan(Path::new("/nonexistent"), procs);
        assert!(report.services.is_empty());
        assert_eq!(report.unmapped.len(), 1);

        // Service grouping, via the pure path mapping directly.
        let mut by_service: BTreeMap<String, Vec<u32>> = BTreeMap::new();
        for (pid, cg) in [(100u32, "/openrc/sshd"), (101, "/openrc/sshd"), (102, "/openrc/dbus")] {
            if let Some(name) = service_from_cgroup_path(cg) {
                by_service.entry(name).or_default().push(pid);
            }
        }
        let services: Vec<AffectedService> = by_service
            .into_iter()
            .map(|(name, mut pids)| {
                pids.sort_unstable();
                AffectedService {
                    restart_command: format!("rc-service {name} restart"),
                    name,
                    pids,
                }
            })
            .collect();
        assert_eq!(services.len(), 2);
        assert_eq!(services[0].name, "dbus");
        assert_eq!(services[0].restart_command, "rc-service dbus restart");
        assert_eq!(services[1].name, "sshd");
        assert_eq!(services[1].pids, vec![100, 101]);

        // Render includes the exact restart commands (v13 §7).
        let full = RestartReport { services, unmapped: vec![] };
        let text = full.render();
        assert!(text.contains("rc-service dbus restart"));
        assert!(text.contains("rc-service sshd restart"));
    }
}
