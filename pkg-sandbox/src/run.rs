//! ## v1 changes
//!
//! - Initial implementation (SPEC: pkg-sandbox; design doc Issue 3.4,
//!   v14 §4).
//!
//! ## v2 changes
//!
//! - Output-aware builds stream complete stdout/stderr lines through callbacks
//!   while retaining bounded diagnostic tails.

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
use std::io::{BufRead as _, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};

use pkg_core::{PkgError, PkgResult};

use crate::profile::{ProfileData, SandboxProfile};

/// Bytes of trailing output retained per stream for error reports. Sized to
/// cover the last error block plus context without holding a whole build log in
/// memory. Output-aware callers receive lines as they arrive and still retain
/// these bounded tails when routine output is hidden.
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

/// Identifies which child output stream produced a callback line.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SandboxStream {
    Stdout,
    Stderr,
}

/// The result of one sandboxed invocation: the process exit status
/// plus bounded tails of its captured output.
#[derive(Debug)]
pub struct SandboxOutcome {
    /// Raw exit status of the bwrap child. Note bwrap itself exits 1
    /// for *sandbox setup* failures (unshare denied, bind source
    /// missing); callers distinguish "sandbox broke" from "build
    /// failed" via `stderr_tail` content, not the code.
    pub status: ExitStatus,
    /// Last up-to-[`STDERR_TAIL_CAP`] bytes of captured stdout, lossily
    /// decoded. Empty for runners that inherit stdout.
    pub stdout_tail: String,
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
    let binds: Vec<(PathBuf, PathBuf)> = extra_bind_ro
        .iter()
        .map(|p| (p.clone(), p.clone()))
        .collect();
    run_sandboxed_with_binds(profile, &binds, cmd, scratch_dir, env)
}

/// [`run_sandboxed_with_deps`] with an explicit in-sandbox destination
/// per bind: each pair is `(host source, in-sandbox path)`. This lets
/// the caller expose a dependency at a *stable* alias (pkg-build uses
/// `/pkg/deps/<name>`) in addition to its identity-hashed real store
/// path, so recipes reference `-I /pkg/deps/foo/usr/include` instead of
/// globbing `.store/<identity>` (which changes with every rebuild).
pub fn run_sandboxed_with_binds(
    profile: &SandboxProfile,
    extra_binds_ro: &[(PathBuf, PathBuf)],
    cmd: &[String],
    scratch_dir: &Path,
    env: &[(String, String)],
) -> PkgResult<SandboxOutcome> {
    if cmd.is_empty() {
        return Err(PkgError::Build("run_sandboxed: empty command".to_string()));
    }
    let pkgdir = env
        .iter()
        .find(|(k, _)| k == "PKGDIR")
        .map(|(_, v)| PathBuf::from(v));
    let mut data = profile.resolve(scratch_dir, pkgdir.as_deref());
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
    let binds: Vec<(PathBuf, PathBuf)> = extra_binds_ro
        .iter()
        .filter(|(src, _)| {
            let exists = src.exists();
            if !exists {
                eprintln!(
                    "pkg: sandbox: bind source {} does not exist; skipping",
                    src.display()
                );
            }
            exists
        })
        .cloned()
        .collect();
    let argv = build_argv_impl(&data, &binds, cmd, scratch_dir, scratch_dir, env);

    let mut child = Command::new(&argv[0])
        .args(&argv[1..])
        .stdin(Stdio::null())
        // Legacy callers inherit stdout; recipe builds use the output-aware
        // variant below when they need capture, prefixes, or quiet mode.
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
        stdout_tail: String::new(),
        stderr_tail: tail.tail_string(),
    })
}

/// [`run_sandboxed_with_binds`] with both output streams piped and delivered
/// to `output` one complete line at a time. Callbacks for the two streams may
/// run concurrently. Newline terminators are omitted, and a final unterminated
/// line is delivered at EOF.
pub fn run_sandboxed_with_binds_output(
    profile: &SandboxProfile,
    extra_binds_ro: &[(PathBuf, PathBuf)],
    cmd: &[String],
    scratch_dir: &Path,
    env: &[(String, String)],
    output: &(dyn Fn(SandboxStream, &str) + Sync),
) -> PkgResult<SandboxOutcome> {
    if cmd.is_empty() {
        return Err(PkgError::Build("run_sandboxed: empty command".to_string()));
    }
    let pkgdir = env
        .iter()
        .find(|(k, _)| k == "PKGDIR")
        .map(|(_, v)| PathBuf::from(v));
    let mut data = profile.resolve(scratch_dir, pkgdir.as_deref());
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
    let binds: Vec<(PathBuf, PathBuf)> = extra_binds_ro
        .iter()
        .filter(|(src, _)| {
            let exists = src.exists();
            if !exists {
                eprintln!(
                    "pkg: sandbox: bind source {} does not exist; skipping",
                    src.display()
                );
            }
            exists
        })
        .cloned()
        .collect();
    let argv = build_argv_impl(&data, &binds, cmd, scratch_dir, scratch_dir, env);

    let mut child = Command::new(&argv[0])
        .args(&argv[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| {
            PkgError::Build(format!(
                "failed to spawn bwrap (is bubblewrap installed?): {e}"
            ))
        })?;

    let stdout = child.stdout.take().expect("stdout was piped");
    let stderr = child.stderr.take().expect("stderr was piped");
    std::thread::scope(|scope| {
        let stdout_reader =
            scope.spawn(move || drain_output(stdout, SandboxStream::Stdout, output));
        let stderr_reader =
            scope.spawn(move || drain_output(stderr, SandboxStream::Stderr, output));

        let status = child.wait();
        let stdout_tail = stdout_reader.join().unwrap_or_else(|_| TailBuffer::new(0));
        let stderr_tail = stderr_reader.join().unwrap_or_else(|_| TailBuffer::new(0));

        status
            .map(|status| SandboxOutcome {
                status,
                stdout_tail: stdout_tail.tail_string(),
                stderr_tail: stderr_tail.tail_string(),
            })
            .map_err(PkgError::Io)
    })
}

fn drain_output<R: Read>(
    reader: R,
    stream: SandboxStream,
    output: &(dyn Fn(SandboxStream, &str) + Sync),
) -> TailBuffer {
    let mut reader = BufReader::new(reader);
    let mut tail = TailBuffer::new(STDERR_TAIL_CAP);
    let mut line = Vec::new();
    loop {
        line.clear();
        match reader.read_until(b'\n', &mut line) {
            Ok(0) => break,
            Ok(_) => {
                tail.push(&line);
                if line.last() == Some(&b'\n') {
                    line.pop();
                    if line.last() == Some(&b'\r') {
                        line.pop();
                    }
                }
                output(stream, String::from_utf8_lossy(&line).as_ref());
            }
            Err(_) => break,
        }
    }
    tail
}

/// Runs an interactive command with the ordinary Build sandbox contract.
/// Unlike a build phase, the shell inherits the caller's terminal and starts
/// in `work_dir`; scratch and `$PKGDIR` remain the only writable locations.
pub fn run_sandboxed_interactive_with_binds(
    profile: &SandboxProfile,
    extra_binds_ro: &[(PathBuf, PathBuf)],
    cmd: &[String],
    scratch_dir: &Path,
    work_dir: &Path,
    env: &[(String, String)],
) -> PkgResult<SandboxOutcome> {
    if cmd.is_empty() {
        return Err(PkgError::Build(
            "interactive sandbox: empty command".to_string(),
        ));
    }
    let pkgdir = env
        .iter()
        .find(|(k, _)| k == "PKGDIR")
        .map(|(_, v)| PathBuf::from(v));
    let mut data = profile.resolve(scratch_dir, pkgdir.as_deref());
    data.bind_ro.retain(|path| path.exists());
    let argv = build_argv_impl(&data, extra_binds_ro, cmd, scratch_dir, work_dir, env);
    let status = Command::new(&argv[0])
        .args(&argv[1..])
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .map_err(|e| PkgError::Build(format!("failed to spawn interactive bwrap: {e}")))?;
    Ok(SandboxOutcome {
        status,
        stdout_tail: String::new(),
        stderr_tail: String::new(),
    })
}

#[derive(Debug)]
pub struct TargetCommandOutcome {
    pub status: ExitStatus,
    pub stdout: String,
    pub stderr: String,
}

/// Runs a fixed maintenance command with the selected package root mounted
/// at `/`. Only listed root-relative paths and scratch are writable.
pub fn run_target_root_command(
    root: &Path,
    scratch_dir: &Path,
    writable: &[PathBuf],
    cmd: &[String],
) -> PkgResult<TargetCommandOutcome> {
    if cmd.is_empty() {
        return Err(PkgError::Build("target-root command is empty".into()));
    }
    let argv = target_root_argv(root, scratch_dir, writable, cmd)?;
    let output = Command::new(&argv[0])
        .args(&argv[1..])
        .stdin(Stdio::null())
        .output()
        .map_err(|e| PkgError::Build(format!("failed to spawn hook bwrap: {e}")))?;
    Ok(TargetCommandOutcome {
        status: output.status,
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

fn target_root_argv(
    root: &Path,
    scratch_dir: &Path,
    writable: &[PathBuf],
    cmd: &[String],
) -> PkgResult<Vec<String>> {
    let scratch_relative = scratch_dir.strip_prefix(root).map_err(|_| {
        PkgError::Build("hook scratch directory must be inside the target root".into())
    })?;
    let scratch_dest = Path::new("/").join(scratch_relative);
    let mut argv = vec![
        "bwrap".into(),
        "--die-with-parent".into(),
        "--unshare-user".into(),
        "--uid".into(),
        "0".into(),
        "--gid".into(),
        "0".into(),
        "--unshare-net".into(),
        "--unshare-pid".into(),
        "--unshare-ipc".into(),
        "--unshare-uts".into(),
        "--new-session".into(),
        "--ro-bind".into(),
        root.display().to_string(),
        "/".into(),
        "--proc".into(),
        "/proc".into(),
        "--dev".into(),
        "/dev".into(),
        "--tmpfs".into(),
        "/tmp".into(),
    ];
    for relative in writable {
        if relative.is_absolute()
            || relative
                .components()
                .any(|part| matches!(part, std::path::Component::ParentDir))
        {
            return Err(PkgError::Build(format!(
                "hook write path must be root-relative: {}",
                relative.display()
            )));
        }
        argv.push("--bind".into());
        argv.push(root.join(relative).display().to_string());
        argv.push(Path::new("/").join(relative).display().to_string());
    }
    argv.push("--bind".into());
    argv.push(scratch_dir.display().to_string());
    argv.push(scratch_dest.display().to_string());
    argv.extend([
        "--chdir".into(),
        scratch_dest.display().to_string(),
        "--clearenv".into(),
        "--setenv".into(),
        "HOME".into(),
        scratch_dest.display().to_string(),
        "--setenv".into(),
        "LC_ALL".into(),
        "C".into(),
        "--setenv".into(),
        "PATH".into(),
        "/usr/bin:/usr/sbin:/bin:/sbin".into(),
        "--".into(),
    ]);
    argv.extend(cmd.iter().cloned());
    Ok(argv)
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
/// - `--unshare-user --uid <profile> --gid <profile>`: private user
///   namespace; 65534 normally, 0 for DKMS's root check (see
///   [`SANDBOX_UID`] for the root-invocation caveat). bwrap sets
///   `no_new_privs` unconditionally in the child —
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
/// - `--ro-bind <src> <dst>` for each aliased bind passed via
///   [`run_sandboxed_with_binds`]: read-only at a caller-chosen
///   in-sandbox path (stable dep aliases like `/pkg/deps/<name>`).
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
    build_argv_impl(data, &[], cmd, scratch_dir, scratch_dir, env)
}

/// [`build_argv`] plus aliased read-only binds: `(host source,
/// in-sandbox destination)` pairs emitted as `--ro-bind src dst`.
/// See [`run_sandboxed_with_binds`].
fn build_argv_impl(
    data: &ProfileData,
    binds_ro: &[(PathBuf, PathBuf)],
    cmd: &[String],
    _scratch_dir: &Path,
    work_dir: &Path,
    env: &[(String, String)],
) -> Vec<String> {
    let mut argv: Vec<String> = vec!["bwrap".to_string()];

    argv.push("--die-with-parent".into());
    if !data.net {
        argv.push("--unshare-net".into());
    }
    argv.push("--unshare-user".into());
    argv.push("--uid".into());
    argv.push(data.uid.to_string());
    argv.push("--gid".into());
    argv.push(data.uid.to_string());
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
    for (src, dst) in binds_ro {
        argv.push("--ro-bind".into());
        argv.push(src.display().to_string());
        argv.push(dst.display().to_string());
    }
    for path in &data.bind_rw {
        argv.push("--bind".into());
        argv.push(path.display().to_string());
        argv.push(path.display().to_string());
    }

    argv.push("--chdir".into());
    argv.push(work_dir.display().to_string());

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
    if !argv
        .windows(3)
        .any(|w| w[0] == "--setenv" && w[1] == "PATH")
    {
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
            vec!["/var/lib/pkg/build/1", "/var/lib/pkg/build/1/pkgdir"]
        );
        let ro: Vec<&str> = argv
            .windows(3)
            .filter(|w| w[0] == "--ro-bind")
            .map(|w| w[1].as_str())
            .collect();
        assert!(ro.contains(&"/usr"));
        // The only permitted /etc bind is Debian's alternatives directory,
        // needed to resolve toolchain symlinks. No other host config enters
        // the build sandbox, and none of it is writable (Issue 3.4).
        assert!(!ro
            .iter()
            .any(|p| p.starts_with("/etc") && *p != "/etc/alternatives"));
        assert!(!rw.iter().any(|p| p.starts_with("/etc")));
    }

    #[test]
    fn aliased_binds_use_explicit_in_sandbox_destination() {
        let scratch = Path::new("/var/lib/pkg/build/1");
        let cmd = vec!["make".to_string()];
        let binds = vec![
            (
                PathBuf::from("/var/lib/pkg/store/libfoo-abc123"),
                PathBuf::from("/pkg/deps/libfoo"),
            ),
            (
                PathBuf::from("/var/lib/pkg/store/libfoo-abc123"),
                PathBuf::from("/var/lib/pkg/store/libfoo-abc123"),
            ),
        ];
        let argv = build_argv_impl(
            &build_data(scratch, None),
            &binds,
            &cmd,
            scratch,
            scratch,
            &[],
        );
        let pairs: Vec<(&str, &str)> = argv
            .windows(3)
            .filter(|w| w[0] == "--ro-bind")
            .map(|w| (w[1].as_str(), w[2].as_str()))
            .collect();
        assert!(pairs.contains(&("/var/lib/pkg/store/libfoo-abc123", "/pkg/deps/libfoo")));
        assert!(pairs.contains(&(
            "/var/lib/pkg/store/libfoo-abc123",
            "/var/lib/pkg/store/libfoo-abc123"
        )));
    }

    #[test]
    fn debug_argv_uses_source_workdir_without_widening_writes() {
        let scratch = Path::new("/var/lib/pkg/build/debug/foo/scratch");
        let work = scratch.join("foo-1");
        let argv = build_argv_impl(
            &build_data(scratch, None),
            &[],
            &["/bin/sh".into()],
            scratch,
            &work,
            &[],
        );
        let chdir = argv.windows(2).find(|pair| pair[0] == "--chdir").unwrap();
        assert_eq!(chdir[1], work.display().to_string());
        let rw: Vec<&str> = argv
            .windows(3)
            .filter(|args| args[0] == "--bind")
            .map(|args| args[1].as_str())
            .collect();
        assert_eq!(rw, [scratch.display().to_string()]);
    }

    #[test]
    fn target_root_argv_exposes_only_the_selected_root_and_write_paths() {
        let root = Path::new("/tmp/target");
        let scratch = root.join("var/lib/pkg/hooks/scratch");
        let argv = target_root_argv(
            root,
            &scratch,
            &[PathBuf::from("var/cache/fontconfig")],
            &["/usr/bin/fc-cache".into()],
        )
        .unwrap();
        assert!(argv
            .windows(3)
            .any(|args| args == ["--ro-bind", "/tmp/target", "/"]));
        assert!(argv.windows(3).any(|args| args
            == [
                "--bind",
                "/tmp/target/var/cache/fontconfig",
                "/var/cache/fontconfig"
            ]));
        assert!(argv.contains(&"--unshare-net".to_string()));
        assert!(argv
            .windows(2)
            .any(|args| args == ["--chdir", "/var/lib/pkg/hooks/scratch"]));
        assert!(target_root_argv(root, Path::new("/tmp/outside"), &[], &["true".into()]).is_err());
        assert!(
            target_root_argv(root, &scratch, &[PathBuf::from("/etc")], &["true".into()]).is_err()
        );
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
        let opt_pos = setenv
            .iter()
            .position(|(k, _)| *k == "PKG_OPT_SSL")
            .unwrap();
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
            uid: SANDBOX_UID,
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

    struct FragmentedReader {
        bytes: Vec<u8>,
        position: usize,
        fragment_size: usize,
    }

    impl FragmentedReader {
        fn new(bytes: Vec<u8>, fragment_size: usize) -> Self {
            Self {
                bytes,
                position: 0,
                fragment_size,
            }
        }
    }

    impl Read for FragmentedReader {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let remaining = self.bytes.len() - self.position;
            let count = remaining.min(self.fragment_size).min(buf.len());
            buf[..count].copy_from_slice(&self.bytes[self.position..self.position + count]);
            self.position += count;
            Ok(count)
        }
    }

    #[test]
    fn output_reader_reassembles_fragmented_lines_and_flushes_eof() {
        let input = b"caf\xC3\xA9\nsecond\r\nlast".to_vec();
        let lines = std::sync::Mutex::new(Vec::new());
        let tail = drain_output(
            FragmentedReader::new(input.clone(), 1),
            SandboxStream::Stdout,
            &|stream, line| {
                lines.lock().unwrap().push((stream, line.to_string()));
            },
        );

        assert_eq!(
            lines.into_inner().unwrap(),
            vec![
                (SandboxStream::Stdout, "caf\u{e9}".to_string()),
                (SandboxStream::Stdout, "second".to_string()),
                (SandboxStream::Stdout, "last".to_string()),
            ]
        );
        assert_eq!(tail.tail(), input);
    }

    #[test]
    fn output_reader_emits_empty_lines_without_an_extra_eof_line() {
        let lines = std::sync::Mutex::new(Vec::new());
        drain_output(
            FragmentedReader::new(b"\n\n".to_vec(), 1),
            SandboxStream::Stderr,
            &|stream, line| {
                lines.lock().unwrap().push((stream, line.to_string()));
            },
        );

        assert_eq!(
            lines.into_inner().unwrap(),
            vec![
                (SandboxStream::Stderr, String::new()),
                (SandboxStream::Stderr, String::new()),
            ]
        );
    }

    #[test]
    fn output_reader_retains_a_bounded_tail() {
        let mut input = b"discarded\n".to_vec();
        input.extend(vec![b'x'; STDERR_TAIL_CAP]);
        input.extend(b"tail");
        let tail = drain_output(
            FragmentedReader::new(input.clone(), 127),
            SandboxStream::Stderr,
            &|_, _| {},
        );

        assert_eq!(tail.tail(), input[input.len() - STDERR_TAIL_CAP..]);
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
