//! ## v1 changes
//!
//! - Initial implementation (SPEC: pkg-sandbox; design doc Issue 3.4,
//!   v14 §4).

//! Sandboxed execution: turn a [`SandboxProfile`] plus a command into a
//! `bwrap` argv, spawn it, and report the outcome.
//!
//! # Hardening scope — what this layer does and does not enforce
//!
//! **cgroups (CPU/IO/memory caps) are applied by the caller**, not
//! here. pkg-build's scheduler (Issue 4.2) places the whole build set
//! under `/sys/fs/cgroup/pkg-build/` via [`crate::cgroups::apply_cgroup`];
//! a per-invocation spawn function is the wrong place for it — cgroup
//! membership is a property of the process tree, not of one command.
//!
//! **seccomp-BPF is NOT expressible through the bwrap argv** in any way
//! this crate can produce safely. bwrap (as of the versions this crate
//! targets) accepts a caller-supplied seccomp-BPF *program* over a file
//! descriptor (`--seccomp FD`), but constructing a correct BPF program
//! is a code-generator problem, not an argv-construction problem, and
//! pkg-sandbox's dependency pin (pkg-core only, std::process spawning)
//! rules out linking a seccomp compiler.
//!
//! > **Hardening TODO (v14 §4, Issue 3.4 "Syscall surface"):**
//! > the build profile must additionally block, at minimum,
//! > `ptrace`, `mount`, `pivot_root`, `bpf`, `perf_event_open`,
//! > `kexec_load`, `init_module`/`finit_module`, `io_uring_setup`, and
//! > nested user-namespace creation (`clone`/`clone3`/`unshare` with
//! > `CLONE_NEWUSER`). Until a BPF program generator lands, the
//! > compensating controls are: empty network namespace (no network
//! > attack surface), `no_new_privs` (bwrap sets it unconditionally —
//! > setuid binaries inside the sandbox are inert), PID/IPC/UTS
//! > namespace isolation, and the cgroup pids cap applied by pkg-build
//! > (fork-bomb containment).
//!
//! # Environment contract
//!
//! The sandbox starts with a **cleared** environment (`--clearenv`).
//! Two sources then populate it:
//!
//! 1. `env` — pairs constructed by the caller (pkg-build): `PKGDIR`,
//!    `PKG_OPT_*` (manifest v3 `options_export_env`),
//!    `SOURCE_DATE_EPOCH`, `PATH`, … These are always set; the caller
//!    is trusted to construct them.
//! 2. Host-process variables named in the profile's `env_allow` list
//!    (e.g. `LANG`, `TZ`). Each is copied in only if present in the
//!    host environment. Nothing else leaks.

use std::collections::VecDeque;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};

use pkg_core::{PkgError, PkgResult};

use crate::profile::{ProfileData, SandboxProfile};

/// Bytes of trailing stderr retained in [`SandboxOutcome::stderr_tail`]
/// for error reports. Sized to cover the tail of a failing compile —
/// the last error block plus context — without holding a whole build
/// log in memory. (The *first* 40 lines of a failing log that Issue
/// 4.2.6's report wants are captured by pkg-build's log file; this
/// tail is the in-memory fallback for error messages.)
pub const STDERR_TAIL_CAP: usize = 8 * 1024;

/// The unprivileged uid/gid the sandboxed command runs as (nobody).
///
/// bwrap's `--uid`/`--gid` choose the *inner* uid of the user
/// namespace; the inner uid is mapped to the invoking process's real
/// uid on the host. Two consequences, both deliberate:
///
/// - When `pkg` runs unprivileged, inner 65534 ↔ the invoking user:
///   the build can't write anything the user couldn't, and no setuid
///   helper is needed.
/// - When `pkg` runs as root (the normal package-manager case), bwrap
///   maps the inner uid to the caller's — i.e. host root. True
///   "mapped to nobody-on-host" requires a subuid/newuidmap helper,
///   which the pins (std::process only) don't give us.
///   **TODO(identity):** plumb an optional subuid mapping (or a
///   setuid helper) so root-invoked builds get a genuinely
///   unprivileged host uid; until then the compensating controls are
///   the mount-namespace RW restriction (the host filesystem is
///   read-only or absent outside scratch/`$PKGDIR`) and
///   `no_new_privs`. This is a documented gap, not a silent one.
pub const SANDBOX_UID: u32 = 65534;
pub const SANDBOX_GID: u32 = 65534;

/// The result of one sandboxed invocation: the process exit status
/// plus the trailing stderr retained for error reporting.
#[derive(Debug)]
pub struct SandboxOutcome {
    /// Raw exit status of the bwrap child. Note bwrap itself exits 1
    /// for *sandbox setup* failures (unshare denied, bind source
    /// missing); callers distinguish "sandbox broke" from "build
    /// failed" via `stderr_tail` content, not the code.
    pub status: ExitStatus,
    /// Last up-to-[`STDERR_TAIL_CAP`] bytes of the child's stderr,
    /// lossily decoded. Intended for `PkgError::Build` messages and
    /// Issue 4.2.6's blocked-subtree report.
    pub stderr_tail: String,
}

impl SandboxOutcome {
    pub fn success(&self) -> bool {
        self.status.success()
    }
}

/// Runs `cmd` inside the sandbox described by `profile`.
///
/// `scratch_dir` is the per-build scratch dir (RW-bound; also the
/// working directory). `$PKGDIR` — the packaging destination — is taken
/// from the `PKGDIR` entry of `env` if present and RW-bound; the Build
/// profile's "RW only scratch + $PKGDIR" contract is enforced
/// structurally: those are the only RW binds the profile can produce.
///
/// The returned outcome never fails on a non-zero exit — a failing
/// build is a *successful* sandboxed run with `success() == false`;
/// `Err` is reserved for failing to spawn/communicate with bwrap.
///
/// `scratch_dir`, `$PKGDIR`, and every profile bind source must exist
/// before the call — bwrap refuses to bind a nonexistent path (and a
/// missing source fails the spawn with bwrap's own exit 1 plus an
/// explanatory [`SandboxOutcome::stderr_tail`]). Creating them is the
/// caller's job (pkg-build sets up the scratch/pkgdir pair per node).
pub fn run_sandboxed(
    profile: &SandboxProfile,
    cmd: &[String],
    scratch_dir: &Path,
    env: &[(String, String)],
) -> PkgResult<SandboxOutcome> {
    run_sandboxed_with_deps(profile, &[], cmd, scratch_dir, env)
}

/// [`run_sandboxed`] plus caller-supplied extra read-only binds — the
/// resolved dependency store dirs a build links against (v12 §9: the
/// dependency tree is visible to the build, read-only, at its real
/// store path). Dep binds are *added to* the profile's contract, never
/// a way around it: they're read-only like the toolchain binds.
pub fn run_sandboxed_with_deps(
    profile: &SandboxProfile,
    extra_bind_ro: &[PathBuf],
    cmd: &[String],
    scratch_dir: &Path,
    env: &[(String, String)],
) -> PkgResult<SandboxOutcome> {
    if cmd.is_empty() {
        return Err(PkgError::Build(
            "run_sandboxed: empty command".to_string(),
        ));
    }
    let pkgdir = env
        .iter()
        .find(|(k, _)| k == "PKGDIR")
        .map(|(_, v)| PathBuf::from(v));
    let mut data = profile.resolve(scratch_dir, pkgdir.as_deref());
    data.bind_ro.extend(extra_bind_ro.iter().cloned());
    // A bind source that doesn't exist on this host (e.g. `/lib` on a
    // NixOS-style layout) would make bwrap refuse the whole invocation;
    // there is nothing to protect at a path that isn't there, so drop it
    // — loudly, since on a conventional layout these binds are expected.
    data.bind_ro.retain(|p| {
        let exists = p.exists();
        if !exists {
            eprintln!(
                "pkg: sandbox: bind source {} does not exist; skipping",
                p.display()
            );
        }
        exists
    });
    let argv = build_argv(&data, cmd, scratch_dir, env);

    let mut child = Command::new(&argv[0])
        .args(&argv[1..])
        .stdin(Stdio::null())
        // stdout inherits: build logs stream to the caller's log
        // capture (pkg-build's per-node log file) unbuffered.
        .stdout(Stdio::inherit())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| {
            PkgError::Build(format!(
                "failed to spawn bwrap (is bubblewrap installed?): {e}"
            ))
        })?;

    // Drain stderr on a thread into the ring buffer. Without a reader
    // the child would deadlock once the pipe buffer fills; without a
    // cap the reader would hold an entire build log in memory.
    let mut stderr = child.stderr.take().expect("stderr was piped");
    let reader = std::thread::spawn(move || {
        let mut tail = TailBuffer::new(STDERR_TAIL_CAP);
        let mut chunk = [0u8; 4096];
        loop {
            match stderr.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => tail.push(&chunk[..n]),
                Err(_) => break,
            }
        }
        tail
    });

    let status = child.wait().map_err(PkgError::Io)?;
    let tail = reader.join().unwrap_or_else(|_| TailBuffer::new(0));

    Ok(SandboxOutcome {
        status,
        stderr_tail: tail.tail_string(),
    })
}

/// Constructs the full `bwrap` argv for a resolved profile. Pure —
/// this is the unit the tests exercise.
///
/// Flag-by-flag rationale (order matters to bwrap only in that binds
/// must precede the `--` command separator):
///
/// - `--die-with-parent`: kills the sandbox if pkg dies, so a crashed
///   scheduler never orphans a build holding RW scratch dirs.
/// - `--unshare-net` (iff `!net`): empty network namespace. The core of
///   the v14 build hardening — "no runtime network, ever" for builds.
/// - `--unshare-user --uid 65534 --gid 65534`: private user namespace,
///   unprivileged identity (see [`SANDBOX_UID`] for the root-invocation
///   caveat). bwrap sets `no_new_privs` unconditionally in the child —
///   there is no flag for it and no way to turn it off — so setuid
///   binaries inside the sandbox are inert.
/// - `--unshare-pid`: private PID namespace (build can't see/signal
///   host processes; fork bombs are additionally capped by the
///   caller's cgroup pids limit).
/// - `--unshare-ipc`, `--unshare-uts`: no shared memory/semaphore
///   leakage, private hostname.
/// - `--new-session`: `setsid(2)` — detaches from the controlling
///   terminal so the build can't inject input via `TIOCSTI`.
/// - `--proc /proc`: fresh procfs from the private PID namespace —
///   build systems probe `/proc` (cpu count, self-exe); hiding it
///   entirely breaks more builds than it protects.
/// - `--dev /dev`: *minimal* devtmpfs (`null`, `zero`, `full`,
///   `random`, `urandom`, `tty`) — not a host `/dev` bind.
/// - `--tmpfs /tmp`: private volatile /tmp, so builds can't snoop or
///   collide with host tempfiles.
/// - `--ro-bind <src> <src>` for each `bind_ro`: toolchain/dep tree
///   read-only at its real path (paths stay where the toolchain's
///   compiled-in prefixes expect them).
/// - `--bind <src> <src>` for each `bind_rw`: the only writable host
///   paths — for Build, exactly scratch + `$PKGDIR` (Issue 3.4).
/// - `--chdir <scratch>`: start in the writable workspace, not wherever
///   pkg happened to run from (which may not exist inside).
/// - `--clearenv`: drop the entire host environment…
/// - `--setenv K V`: …then re-add only the caller's explicit pairs and
///   profile-allowed host variables (see module docs).
/// - `-- <cmd…>`: separator + the command, argv passed through
///   untouched (no shell interpolation anywhere).
pub fn build_argv(
    data: &ProfileData,
    cmd: &[String],
    scratch_dir: &Path,
    env: &[(String, String)],
) -> Vec<String> {
    let mut argv: Vec<String> = vec!["bwrap".to_string()];

    argv.push("--die-with-parent".into());
    if !data.net {
        argv.push("--unshare-net".into());
    }
    argv.push("--unshare-user".into());
    argv.push("--uid".into());
    argv.push(SANDBOX_UID.to_string());
    argv.push("--gid".into());
    argv.push(SANDBOX_GID.to_string());
    argv.push("--unshare-pid".into());
    argv.push("--unshare-ipc".into());
    argv.push("--unshare-uts".into());
    argv.push("--new-session".into());

    argv.push("--proc".into());
    argv.push("/proc".into());
    argv.push("--dev".into());
    argv.push("/dev".into());
    argv.push("--tmpfs".into());
    argv.push("/tmp".into());

    for path in &data.bind_ro {
        argv.push("--ro-bind".into());
        argv.push(path.display().to_string());
        argv.push(path.display().to_string());
    }
    for path in &data.bind_rw {
        argv.push("--bind".into());
        argv.push(path.display().to_string());
        argv.push(path.display().to_string());
    }

    argv.push("--chdir".into());
    argv.push(scratch_dir.display().to_string());

    argv.push("--clearenv".into());
    // Deterministic ordering: caller pairs first (sorted by key for a
    // stable argv — makes logs/diffs reproducible), then the
    // profile-allowed host variables.
    let mut pairs: Vec<&(String, String)> = env.iter().collect();
    pairs.sort_by(|a, b| a.0.cmp(&b.0));
    for (k, v) in pairs {
        argv.push("--setenv".into());
        argv.push(k.clone());
        argv.push(v.clone());
    }
    for name in &data.env_allow {
        if env.iter().any(|(k, _)| k == name) {
            continue; // explicit caller pair wins
        }
        if let Ok(value) = std::env::var(name) {
            argv.push("--setenv".into());
            argv.push(name.clone());
            argv.push(value);
        }
    }
    // PATH is load-bearing for any build; if neither the caller nor the
    // host provides one, give the sandbox a sane minimal default.
    if !argv.windows(3).any(|w| w[0] == "--setenv" && w[1] == "PATH") {
        argv.push("--setenv".into());
        argv.push("PATH".into());
        argv.push("/usr/bin:/bin".into());
    }

    argv.push("--".into());
    argv.extend(cmd.iter().cloned());
    argv
}

/// A fixed-capacity ring buffer keeping the last `cap` bytes written.
/// Used to retain a bounded tail of a child's stderr for error reports
/// without buffering an entire build log.
///
/// Implementation: a double-ended queue would be O(1) per byte but
/// wasteful; instead we keep a `Vec` and, when appending would exceed
/// capacity, drain from the front in bulk. Appends come in 4KiB read
/// chunks, so the amortized cost is fine and the logic stays obvious.
pub struct TailBuffer {
    buf: VecDeque<u8>,
    cap: usize,
}

impl TailBuffer {
    pub fn new(cap: usize) -> Self {
        TailBuffer {
            buf: VecDeque::with_capacity(cap),
            cap,
        }
    }

    /// Appends `bytes`, retaining only the last `cap` bytes overall.
    pub fn push(&mut self, bytes: &[u8]) {
        if bytes.len() >= self.cap {
            // The incoming chunk alone fills/exceeds the buffer: keep
            // only its tail. (With cap == 0 this keeps nothing.)
            self.buf.clear();
            self.buf.extend(&bytes[bytes.len() - self.cap..]);
            return;
        }
        let total = self.buf.len() + bytes.len();
        if total > self.cap {
            self.buf.drain(..total - self.cap);
        }
        self.buf.extend(bytes);
    }

    /// The retained bytes, oldest first.
    pub fn tail(&self) -> Vec<u8> {
        self.buf.iter().copied().collect()
    }

    /// The retained bytes as a string (lossy — a UTF-8 boundary may sit
    /// mid-sequence at the cut point).
    pub fn tail_string(&self) -> String {
        String::from_utf8_lossy(&self.tail()).into_owned()
    }
}

// ---------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile::SandboxProfile;

    fn build_data(scratch: &Path, pkgdir: Option<&Path>) -> ProfileData {
        SandboxProfile::Build.resolve(scratch, pkgdir)
    }

    #[test]
    fn argv_unshares_net_for_build_but_not_fetch() {
        let scratch = Path::new("/var/lib/pkg/build/1");
        let cmd = vec!["make".to_string()];
        let build = build_argv(&build_data(scratch, None), &cmd, scratch, &[]);
        assert!(build.contains(&"--unshare-net".to_string()));

        let fetch = SandboxProfile::Fetch.resolve(scratch, None);
        let fetch_argv = build_argv(&fetch, &cmd, scratch, &[]);
        assert!(!fetch_argv.contains(&"--unshare-net".to_string()));
    }

    #[test]
    fn argv_binds_scratch_and_pkgdir_rw_and_toolchain_ro() {
        let scratch = Path::new("/var/lib/pkg/build/1");
        let pkgdir = Path::new("/var/lib/pkg/build/1/pkgdir");
        let cmd = vec!["make".to_string()];
        let argv = build_argv(&build_data(scratch, Some(pkgdir)), &cmd, scratch, &[]);

        let rw: Vec<&str> = argv
            .windows(3)
            .filter(|w| w[0] == "--bind")
            .map(|w| w[1].as_str())
            .collect();
        assert_eq!(
            rw,
            vec![
                "/var/lib/pkg/build/1",
                "/var/lib/pkg/build/1/pkgdir"
            ]
        );
        let ro: Vec<&str> = argv
            .windows(3)
            .filter(|w| w[0] == "--ro-bind")
            .map(|w| w[1].as_str())
            .collect();
        assert!(ro.contains(&"/usr"));
        // No /etc anywhere in a build sandbox (Issue 3.4: no /etc
        // write; we go further and don't bind it at all).
        assert!(!ro.iter().any(|p| p.starts_with("/etc")));
        assert!(!rw.iter().any(|p| p.starts_with("/etc")));
    }

    #[test]
    fn argv_drops_host_env_and_sets_sorted_explicit_pairs() {
        let scratch = Path::new("/var/lib/pkg/build/1");
        let cmd = vec!["make".to_string()];
        let env = vec![
            ("PKG_OPT_SSL".to_string(), "1".to_string()),
            ("PKGDIR".to_string(), "/pkg".to_string()),
        ];
        let argv = build_argv(&build_data(scratch, None), &cmd, scratch, &env);
        assert!(argv.contains(&"--clearenv".to_string()));

        let setenv: Vec<(&str, &str)> = argv
            .windows(3)
            .filter(|w| w[0] == "--setenv")
            .map(|w| (w[1].as_str(), w[2].as_str()))
            .collect();
        // Explicit pairs present, sorted by key.
        assert!(setenv.contains(&("PKGDIR", "/pkg")));
        assert!(setenv.contains(&("PKG_OPT_SSL", "1")));
        // Some PATH is always set — the Build profile allows the host's
        // PATH through, and the default only kicks in when neither
        // caller nor host provides one (tested separately below).
        assert!(setenv.iter().any(|(k, _)| *k == "PATH"));
        let pkgdir_pos = setenv.iter().position(|(k, _)| *k == "PKGDIR").unwrap();
        let opt_pos = setenv.iter().position(|(k, _)| *k == "PKG_OPT_SSL").unwrap();
        assert!(pkgdir_pos < opt_pos, "explicit pairs sorted by key");
    }

    #[test]
    fn argv_supplies_default_path_when_nobody_does() {
        // A custom profile with an empty env_allow: nothing is
        // inherited from the host, so the default PATH must appear.
        let data = ProfileData {
            name: "test".into(),
            version: None,
            bind_ro: vec![],
            bind_rw: vec![],
            net: false,
            env_allow: vec![],
        };
        let scratch = Path::new("/var/lib/pkg/build/1");
        let cmd = vec!["make".to_string()];
        let argv = build_argv(&data, &cmd, scratch, &[]);
        let setenv: Vec<(&str, &str)> = argv
            .windows(3)
            .filter(|w| w[0] == "--setenv")
            .map(|w| (w[1].as_str(), w[2].as_str()))
            .collect();
        assert_eq!(setenv, vec![("PATH", "/usr/bin:/bin")]);
    }

    #[test]
    fn explicit_caller_pair_wins_over_env_allow() {
        // Build's env_allow includes PATH; an explicit caller PATH must
        // not be duplicated/overridden by the host's.
        let scratch = Path::new("/var/lib/pkg/build/1");
        let cmd = vec!["make".to_string()];
        let env = vec![("PATH".to_string(), "/toolchain/bin".to_string())];
        let argv = build_argv(&build_data(scratch, None), &cmd, scratch, &env);
        let paths: Vec<&str> = argv
            .windows(3)
            .filter(|w| w[0] == "--setenv" && w[1] == "PATH")
            .map(|w| w[2].as_str())
            .collect();
        assert_eq!(paths, vec!["/toolchain/bin"]);
    }

    #[test]
    fn argv_never_interpolates_a_shell() {
        let scratch = Path::new("/var/lib/pkg/build/1");
        let cmd = vec![
            "sh".to_string(),
            "-c".to_string(),
            "echo $(evil)".to_string(),
        ];
        let argv = build_argv(&build_data(scratch, None), &cmd, scratch, &[]);
        let sep = argv.iter().position(|a| a == "--").unwrap();
        assert_eq!(&argv[sep + 1..], cmd.as_slice());
    }

    #[test]
    fn tail_buffer_keeps_exactly_the_last_cap_bytes() {
        let mut t = TailBuffer::new(8);
        t.push(b"hello ");
        t.push(b"world!");
        assert_eq!(t.tail(), b"o world!");
        t.push(b"!!");
        assert_eq!(t.tail(), b"world!!!");
    }

    #[test]
    fn tail_buffer_oversized_single_write_keeps_its_own_tail() {
        let mut t = TailBuffer::new(4);
        t.push(b"0123456789");
        assert_eq!(t.tail(), b"6789");
    }

    #[test]
    fn tail_buffer_exact_capacity_write_replaces_everything() {
        let mut t = TailBuffer::new(4);
        t.push(b"ab");
        t.push(b"cdef");
        assert_eq!(t.tail(), b"cdef");
    }

    #[test]
    fn tail_buffer_handles_invalid_utf8_boundaries_lossily() {
        let mut t = TailBuffer::new(2);
        t.push(&[0xC3, 0xA9, 0x21]); // 'é' (2 bytes) then '!'; the cap cut drops é's lead byte
        let s = t.tail_string();
        assert!(s.ends_with('!'));
        assert_eq!(s.chars().count(), 2); // replacement char + '!'
    }

    #[test]
    fn tail_buffer_zero_cap_keeps_nothing() {
        let mut t = TailBuffer::new(0);
        t.push(b"anything");
        assert!(t.tail().is_empty());
    }
}
