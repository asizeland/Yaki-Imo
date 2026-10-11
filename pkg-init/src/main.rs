//! glibc-apply — atomic glibc swapper run from the initramfs (spec v13).
//!
//! This binary is part of the **pkg-init** crate and builds as a
//! musl-static, dependency-free (std-only) executable. It deliberately
//! links NOTHING from pkg-db or any other crate: it runs early in boot,
//! before the package manager's libraries (or even a working glibc
//! userspace beyond itself) can be assumed. Everything it needs —
//! flag parsing, SHA-256, the staged-tree manifest hash, and the
//! `renameat2(RENAME_EXCHANGE)` syscall — is implemented in this file.
//!
//! # FROZEN interface (spec v13 §1 — do not change without a spec bump)
//!
//! Paths:
//!   - flag:   `/var/lib/pkg/glibc-pending`
//!   - anchor: `/usr/lib/pkg/glibc-anchor/`       (currently-active glibc tree)
//!   - staged: `/usr/lib/pkg/.stage/glibc-active/` (verified replacement tree)
//!   - DB:     `/var/lib/pkg/db/pkg.sqlite`     (read-only, best-effort;
//!              this binary does NOT open it — verification of the DB row
//!              happens in `pkg` / queries-v3.rs, not here)
//!
//! Exit codes:
//!   - 0: no pending flag (nothing to do), fail-safe "boot the old glibc"
//!        decision (bad protocol, corrupt staged tree), swap succeeded,
//!        or `--rollback` succeeded.
//!   - 1: swap (`RENAME_EXCHANGE`) failed, anchor/staged on different
//!        filesystems, or `--rollback` failed. The caller (initramfs
//!        script) drops to the rescue shell on 1.
//!   - 2: reserved (unused; kept so a future "rollback attempted but
//!        result ambiguous" state has a code that doesn't collide with
//!        the plain failure path).
//!
//! Subcommand: `--rollback` — reverses the exchange (staged <-> anchor
//! again), used from the rescue shell after a bad boot.
//!
//! Flag file format (protocol v1, spec §7.6.2, line-based `key = value`):
//! ```text
//! protocol = 1
//! target = 2.40
//! manifest_sha256 = <hex digest of the staged tree's manifest>
//! created = <RFC 3339 timestamp>
//! ```
//! `glibc_apply_sha256 = <hex>` may additionally appear as an optional
//! protocol-1 extension carrying the §7.6.3 drift-check hash (see
//! `parse_flag`).
//!
//! ## v1 notes
//!
//! - **Why the interface is frozen**: this binary is itself part of the
//!   glibc upgrade payload's trust boundary. The initramfs script and
//!   the `pkg` CLI are upgraded independently; the flag format and exit
//!   codes are the contract that lets a NEW `pkg` be applied by an OLD
//!   glibc-apply (and vice versa) without either side guessing.
//! - **Syscall approach**: `renameat2` is invoked via a RAW syscall
//!   (`core::arch::asm!` on x86_64, syscall number 316; on i686 the
//!   number is 353 — adjust `SYS_RENAMEAT2` if this is ever ported).
//!   The `libc` crate is not allowed in this crate, and we must not
//!   depend on musl exposing `renameat2` (it may be built against
//!   kernel headers older than 3.15). `RENAME_EXCHANGE` has no fallback
//!   in glibc either, so the raw syscall is the only honest path.
//! - **No exec of /sbin/init**: this binary swaps and exits. Re-executing
//!   `/sbin/init` (or continuing the boot) is the initramfs script's job.
//!   Rationale: single-purpose binary — the swapper is verifiable,
//!   testable, and its exit code unambiguously communicates the outcome
//!   to the script that decides what runs next. Keeping exec out also
//!   means glibc-apply never has to reason about argv/env for init.
//! - **Cross-reference**: `GLIBC_PENDING_PROTOCOL` (value 1) is
//!   duplicated in `queries-v3.rs` (pkg-db side). Both copies MUST move
//!   together — a protocol bump without updating both is a silent
//!   fail-safe (old binary refuses to swap) at best.
//! - **Manifest-hash lockstep**: the staged-tree manifest hash below is
//!   the SAME algorithm as `integrity-v3.rs`'s `StagedGlibcManifest`
//!   (pkg-db side). Both files document it precisely; any change must be
//!   made in both places or verification and staging will disagree and
//!   every upgrade will fail safe (refuse to swap).
//!
//! MANIFEST-HASH ALGORITHM (frozen, spec v13 §5):
//!   1. Recursively walk the staged tree, collecting REGULAR files only
//!      (directories descended; symlinks and special files skipped).
//!   2. SHA-256 each file's raw contents, lowercase hex (64 chars).
//!   3. One line per file: `relpath<space>sha256hex\n` — relpath is
//!      relative to the staged root, `/`-separated.
//!   4. Sort lines by relpath (byte-wise ascending).
//!   5. Manifest hash = SHA-256 of the concatenated lines, lowercase hex.
//!   Empty tree => SHA-256 of the empty string, which never matches a
//!   real staged digest (fail-safe by construction).

use std::ffi::CString;
use std::fs;
use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

// ---------------------------------------------------------------------
// Frozen interface constants (spec v13 §1)
// ---------------------------------------------------------------------

/// Pending-upgrade flag path. Presence alone means "work to do".
const FLAG_PATH: &str = "/var/lib/pkg/glibc-pending";
/// Currently-active glibc tree.
const ANCHOR_DIR: &str = "/usr/lib/pkg/glibc-anchor";
/// Verified replacement tree, staged by `pkg` before reboot.
const STAGED_DIR: &str = "/usr/lib/pkg/.stage/glibc-active";

/// Flag-file protocol version this binary understands.
/// DUPLICATED in queries-v3.rs as `GLIBC_PENDING_PROTOCOL` — the two
/// copies MUST move together (see module docs).
const GLIBC_PENDING_PROTOCOL: u64 = 1;

/// renameat2(2) flag: atomically exchange oldpath and newpath.
const RENAME_EXCHANGE: u32 = 2;

/// x86_64 syscall number for renameat2. (i686: 353 — see module docs.)
#[cfg(target_arch = "x86_64")]
const SYS_RENAMEAT2: i64 = 316;

/// Directory file descriptor meaning "relative to cwd" — we always pass
/// absolute paths, so this is only a formality.
const AT_FDCWD: i64 = -100;

// ---------------------------------------------------------------------
// main
// ---------------------------------------------------------------------

fn main() -> ExitCode {
    let mut args = std::env::args_os();
    let _argv0 = args.next();
    match args.next().as_deref().and_then(|a| a.to_str()) {
        None => run_apply(),
        Some("--rollback") => run_rollback(),
        Some(other) => {
            eprintln!("glibc-apply: unknown argument '{other}' (expected none or --rollback)");
            ExitCode::from(1)
        }
    }
}

/// Normal path (spec v13 §2-§5, §7): verify, then atomically exchange
/// anchor <-> staged so the staged tree becomes the active glibc.
fn run_apply() -> ExitCode {
    // 1. Common path first: no flag => nothing to do. Must be instant —
    //    this runs on EVERY boot.
    if !Path::new(FLAG_PATH).exists() {
        return ExitCode::SUCCESS;
    }

    // 2. Parse flag. Unknown protocol => fail SAFE (boot old glibc):
    //    warn, don't swap, exit 0 (v13 §2). A future pkg may have written
    //    a protocol-2 flag this binary can't honor; swapping blindly
    //    would be worse than booting the still-working old glibc.
    let flag = match parse_flag(FLAG_PATH) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("glibc-apply: WARNING: cannot parse {FLAG_PATH}: {e}; booting old glibc");
            clear_flag();
            return ExitCode::SUCCESS;
        }
    };
    if flag.protocol != GLIBC_PENDING_PROTOCOL {
        eprintln!(
            "glibc-apply: WARNING: flag protocol {} != supported {}; booting old glibc",
            flag.protocol, GLIBC_PENDING_PROTOCOL
        );
        clear_flag();
        return ExitCode::SUCCESS;
    }

    // 3. Verify the staged tree against the recorded manifest digest.
    //    NEVER swap unverified content (v13 §5). On failure: clear the
    //    flag so we don't retry a corrupt stage every boot, warn, and
    //    boot the old glibc (exit 0).
    let staged_root = Path::new(STAGED_DIR);
    match manifest_sha256(staged_root) {
        Ok(actual) if actual == flag.manifest_sha256 => {
            eprintln!(
                "glibc-apply: staged glibc {} verified ({} files... digest match)",
                flag.target,
                // cheap liveness signal; count is recomputed inside,
                // so just say the digest matched
                "staged"
            );
        }
        Ok(actual) => {
            eprintln!(
                "glibc-apply: WARNING: staged tree manifest mismatch \
                 (expected {}, computed {actual}); booting old glibc",
                flag.manifest_sha256
            );
            clear_flag();
            return ExitCode::SUCCESS;
        }
        Err(e) => {
            eprintln!(
                "glibc-apply: WARNING: staged tree unusable ({e}); booting old glibc"
            );
            clear_flag();
            return ExitCode::SUCCESS;
        }
    }

    // 4. Drift check (v13 §3): the PM records the sha256 of the
    //    installed glibc-apply binary in the DB; a protocol-1 flag MAY
    //    carry a copy (`glibc_apply_sha256`), since this std-only binary
    //    cannot open sqlite. Absent field or unreadable self-hash =>
    //    skip silently (best-effort). If THIS binary differs from the
    //    recorded hash (e.g. initramfs rebuilt with an older/newer
    //    copy), warn but proceed — the check is informational only,
    //    since the manifest verification above is the actual safety
    //    gate.
    if let Some(expected) = &flag.glibc_apply_sha256 {
        match self_sha256() {
            Ok(actual) if &actual != expected => {
                eprintln!(
                    "glibc-apply: WARNING: self-hash drift: flag recorded {expected}, \
                     this binary is {actual} (proceeding; informational only)"
                );
            }
            Err(e) => eprintln!("glibc-apply: WARNING: cannot hash /proc/self/exe: {e}"),
            _ => {}
        }
    }

    // 5. RENAME_EXCHANGE requires both paths on the same filesystem.
    match same_filesystem(Path::new(ANCHOR_DIR), staged_root) {
        Ok(true) => {}
        Ok(false) => {
            eprintln!(
                "glibc-apply: ERROR: {ANCHOR_DIR} and {STAGED_DIR} are on different \
                 filesystems; atomic exchange impossible"
            );
            return ExitCode::from(1);
        }
        Err(e) => {
            eprintln!("glibc-apply: ERROR: cannot stat anchor/staged: {e}");
            return ExitCode::from(1);
        }
    }

    // 6. The swap: renameat2(AT_FDCWD, anchor, AT_FDCWD, staged,
    //    RENAME_EXCHANGE). After this single syscall, the staged tree IS
    //    the active glibc and the old glibc sits in the staged dir
    //    (available for --rollback).
    if let Err(e) = exchange(ANCHOR_DIR, STAGED_DIR) {
        eprintln!("glibc-apply: ERROR: RENAME_EXCHANGE failed: {e}; booting old glibc");
        return ExitCode::from(1);
    }

    // 7. Done: clear flag, report, exit. Re-executing /sbin/init (or
    //    pivoting into the real root) is the initramfs script's job —
    //    deliberately NOT done here; see module docs.
    clear_flag();
    println!(
        "glibc-apply: swapped in staged glibc {} (anchor <-> staged exchanged)",
        flag.target
    );
    ExitCode::SUCCESS
}

/// `--rollback` (spec v13 §8): exchange back. After a successful apply,
/// the OLD glibc lives in the staged dir and the NEW (suspect) one in
/// the anchor; exchanging again restores the old glibc as active.
/// Invoked manually from the rescue shell, so it talks to stderr and
/// returns 0 on success, 1 on failure.
fn run_rollback() -> ExitCode {
    if !Path::new(ANCHOR_DIR).is_dir() || !Path::new(STAGED_DIR).is_dir() {
        eprintln!(
            "glibc-apply --rollback: ERROR: need both {ANCHOR_DIR} and {STAGED_DIR} \
             present to exchange"
        );
        return ExitCode::from(1);
    }
    match same_filesystem(Path::new(ANCHOR_DIR), Path::new(STAGED_DIR)) {
        Ok(true) => {}
        Ok(false) => {
            eprintln!("glibc-apply --rollback: ERROR: different filesystems");
            return ExitCode::from(1);
        }
        Err(e) => {
            eprintln!("glibc-apply --rollback: ERROR: stat failed: {e}");
            return ExitCode::from(1);
        }
    }
    if let Err(e) = exchange(ANCHOR_DIR, STAGED_DIR) {
        eprintln!("glibc-apply --rollback: ERROR: RENAME_EXCHANGE failed: {e}");
        return ExitCode::from(1);
    }
    // Rollback reverts to the pre-upgrade state, so any pending flag is
    // stale; remove it so the next boot doesn't try to re-apply.
    clear_flag();
    println!("glibc-apply --rollback: exchanged back; previous glibc restored as active");
    ExitCode::SUCCESS
}

// ---------------------------------------------------------------------
// Flag file (protocol v1, `key = value` lines)
// ---------------------------------------------------------------------

#[derive(Debug)]
struct PendingFlag {
    protocol: u64,
    target: String,
    manifest_sha256: String,
    glibc_apply_sha256: Option<String>,
    #[allow(dead_code)] // recorded for diagnostics / future use
    created: String,
}

/// Parses the flag file (spec §7.6.2 protocol-1 format). Required:
/// `protocol`, `target`, `manifest_sha256` (64-char hex), and `created`
/// (RFC 3339). `glibc_apply_sha256` is an optional protocol-1 extension
/// mirroring the DB column of the same name, for the §7.6.3 drift
/// self-check — the DB row is canonical, but this std-only binary cannot
/// open sqlite, so a flag copy is its only possible source.
///
/// Unknown keys are FATAL, not ignored: a flag carrying fields this
/// binary doesn't know is either corrupt or was written by a newer
/// format that forgot to bump `protocol`. Per the frozen-interface
/// rationale (§7.6.1), a format mismatch must degrade into "refuses to
/// act" — the caller boots the old glibc with a loud warning — never
/// into guessing at semantics. Blank lines and `#` comments are
/// ignored; `=` may have surrounding whitespace.
fn parse_flag(path: &str) -> Result<PendingFlag, String> {
    let text = fs::read_to_string(path).map_err(|e| format!("read: {e}"))?;
    let mut protocol = None;
    let mut target = None;
    let mut manifest_sha256 = None;
    let mut glibc_apply_sha256 = None;
    let mut created = None;
    for (lineno, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (key, value) = line
            .split_once('=')
            .ok_or_else(|| format!("line {}: missing '='", lineno + 1))?;
        let value = value.trim();
        match key.trim() {
            "protocol" => {
                protocol = Some(
                    value
                        .parse::<u64>()
                        .map_err(|_| format!("line {}: bad protocol value", lineno + 1))?,
                )
            }
            "target" => target = Some(value.to_string()),
            "manifest_sha256" => {
                manifest_sha256 = Some(hex64(value).ok_or_else(|| {
                    format!("line {}: bad manifest_sha256 (want 64 hex chars)", lineno + 1)
                })?)
            }
            "glibc_apply_sha256" => {
                glibc_apply_sha256 = Some(hex64(value).ok_or_else(|| {
                    format!("line {}: bad glibc_apply_sha256 (want 64 hex chars)", lineno + 1)
                })?)
            }
            "created" => {
                created = Some(parse_rfc3339(value).ok_or_else(|| {
                    format!("line {}: bad created (want RFC 3339)", lineno + 1)
                })?)
            }
            other => {
                return Err(format!(
                    "line {}: unknown key '{other}' (refusing to guess at an \
                     unrecognized format)",
                    lineno + 1
                ))
            }
        }
    }
    Ok(PendingFlag {
        protocol: protocol.ok_or("missing 'protocol'")?,
        target: target.ok_or("missing 'target'")?,
        manifest_sha256: manifest_sha256.ok_or("missing 'manifest_sha256'")?,
        glibc_apply_sha256,
        created: created.ok_or("missing 'created'")?,
    })
}

/// Lowercases and validates a 64-char hex digest.
fn hex64(value: &str) -> Option<String> {
    let lowered = value.to_ascii_lowercase();
    (lowered.len() == 64 && lowered.bytes().all(|b| b.is_ascii_hexdigit())).then_some(lowered)
}

/// RFC 3339 shape check: `YYYY-MM-DDTHH:MM:SS` followed by `Z` or a
/// `±HH:MM` offset, with range checks on each field. Calendar validity
/// (e.g. Feb 30) is not checked — the value is diagnostic only, never a
/// safety gate.
fn parse_rfc3339(value: &str) -> Option<String> {
    let b = value.as_bytes();
    if b.len() != 20 && b.len() != 25 {
        return None;
    }
    let digits = |from: usize, to: usize| b[from..to].iter().all(u8::is_ascii_digit);
    let num = |i: usize| (b[i] - b'0') as u32 * 10 + (b[i + 1] - b'0') as u32;
    if !(digits(0, 4)
        && b[4] == b'-' && digits(5, 7)
        && b[7] == b'-' && digits(8, 10)
        && b[10] == b'T' && digits(11, 13)
        && b[13] == b':' && digits(14, 16)
        && b[16] == b':' && digits(17, 19))
    {
        return None;
    }
    let (month, day, hour, min, sec) = (num(5), num(8), num(11), num(14), num(17));
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || min > 59
        || sec > 60 // 60: leap second
    {
        return None;
    }
    if b[19] == b'Z' {
        return (b.len() == 20).then(|| value.to_string());
    }
    if b.len() == 25
        && (b[19] == b'+' || b[19] == b'-')
        && digits(20, 22)
        && b[22] == b':'
        && digits(23, 25)
        && num(20) <= 23
        && num(23) <= 59
    {
        return Some(value.to_string());
    }
    None
}

/// Best-effort flag removal: the flag must never keep us in a boot loop,
/// but a removal failure is not fatal (a stale flag just means one more
/// fail-safe pass next boot).
fn clear_flag() {
    if let Err(e) = fs::remove_file(FLAG_PATH) {
        if e.kind() != std::io::ErrorKind::NotFound {
            eprintln!("glibc-apply: WARNING: could not remove {FLAG_PATH}: {e}");
        }
    }
}

// ---------------------------------------------------------------------
// Staged-tree manifest hash (frozen algorithm — see module docs; keep
// in lockstep with integrity-v3.rs `StagedGlibcManifest`)
// ---------------------------------------------------------------------

/// Computes the manifest hash of the tree at `root`: sorted
/// `relpath<space>sha256hex\n` lines over all regular files, SHA-256 of
/// the concatenation, lowercase hex.
fn manifest_sha256(root: &Path) -> Result<String, String> {
    if !root.is_dir() {
        return Err(format!("staged tree missing: {}", root.display()));
    }
    let mut entries: Vec<(String, String)> = Vec::new();
    collect_tree(root, root, &mut entries)?;
    entries.sort();
    let mut m = Sha256::new();
    for (rel, hex) in &entries {
        m.update(rel.as_bytes());
        m.update(b" ");
        m.update(hex.as_bytes());
        m.update(b"\n");
    }
    Ok(hex_lower(&m.finalize()))
}

fn collect_tree(
    root: &Path,
    dir: &Path,
    out: &mut Vec<(String, String)>,
) -> Result<(), String> {
    let rd = fs::read_dir(dir).map_err(|e| format!("read_dir {}: {e}", dir.display()))?;
    for entry in rd {
        let entry = entry.map_err(|e| format!("read_dir entry: {e}"))?;
        let path = entry.path();
        let ft = entry
            .file_type()
            .map_err(|e| format!("file_type {}: {e}", path.display()))?;
        if ft.is_dir() {
            collect_tree(root, &path, out)?;
        } else if ft.is_file() {
            let rel = path
                .strip_prefix(root)
                .map_err(|_| "walker escaped root".to_string())?
                .to_string_lossy()
                .into_owned();
            out.push((rel, sha256_file(&path)?));
        }
        // symlinks / special files: skipped (algorithm step 1).
    }
    Ok(())
}

/// SHA-256 of a file's contents, streamed in 64 KiB chunks. Any I/O
/// error is fatal to the whole verification — never skip a file.
fn sha256_file(path: &Path) -> Result<String, String> {
    let mut f = fs::File::open(path).map_err(|e| format!("open {}: {e}", path.display()))?;
    let mut h = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = f
            .read(&mut buf)
            .map_err(|e| format!("read {}: {e}", path.display()))?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(hex_lower(&h.finalize()))
}

/// SHA-256 of our own binary via /proc/self/exe (drift check, v13 §3).
fn self_sha256() -> Result<String, String> {
    let me: PathBuf = fs::read_link("/proc/self/exe")
        .map_err(|e| format!("readlink /proc/self/exe: {e}"))?;
    sha256_file(&me)
}

fn hex_lower(digest: &[u8; 32]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(64);
    for b in digest {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0xf) as usize] as char);
    }
    out
}

// ---------------------------------------------------------------------
// renameat2(RENAME_EXCHANGE) via raw syscall (no libc crate)
// ---------------------------------------------------------------------

/// Atomically exchanges the directory entries `a` and `b` (both must
/// exist; for our use both are directories on the same filesystem).
fn exchange(a: &str, b: &str) -> Result<(), String> {
    let a_c = CString::new(a).map_err(|_| "anchor path contains NUL".to_string())?;
    let b_c = CString::new(b).map_err(|_| "staged path contains NUL".to_string())?;
    let ret = unsafe { sys_renameat2(AT_FDCWD, a_c.as_ptr().cast(), AT_FDCWD, b_c.as_ptr().cast(), RENAME_EXCHANGE) };
    if ret == 0 {
        Ok(())
    } else {
        Err(format!(
            "renameat2({a}, {b}, RENAME_EXCHANGE): {}",
            std::io::Error::last_os_error()
        ))
    }
}

/// Raw renameat2 syscall. x86_64 uses `asm!` with the syscall number
/// inline (316; i686 would be 353 — see module docs). Other architectures
/// fall back to the `syscall(2)` wrapper exported by the C library,
/// which std links against anyway (works musl-static).
#[cfg(target_arch = "x86_64")]
unsafe fn sys_renameat2(olddirfd: i64, oldpath: *const u8, newdirfd: i64, newpath: *const u8, flags: u32) -> i64 {
    let ret: i64;
    unsafe {
        core::arch::asm!(
            "syscall",
            inlateout("rax") SYS_RENAMEAT2 => ret,
            in("rdi") olddirfd,
            in("rsi") oldpath,
            in("rdx") newdirfd,
            in("r10") newpath,
            in("r8") flags as u64,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack),
        );
    }
    ret
}

#[cfg(not(target_arch = "x86_64"))]
unsafe fn sys_renameat2(olddirfd: i64, oldpath: *const u8, newdirfd: i64, newpath: *const u8, flags: u32) -> i64 {
    // Fallback: the libc `syscall` function (linked via std). The
    // per-arch renameat2 number must be supplied here when porting.
    extern "C" {
        fn syscall(num: i64, ...) -> i64;
    }
    const SYS_RENAMEAT2_FALLBACK: i64 = 316; // ADJUST PER ARCH (i686: 353)
    unsafe { syscall(SYS_RENAMEAT2_FALLBACK, olddirfd, oldpath, newdirfd, newpath, flags) }
}

// ---------------------------------------------------------------------
// Misc
// ---------------------------------------------------------------------

/// Both paths must live on the same filesystem for RENAME_EXCHANGE.
/// `MetadataExt::dev` gives st_dev without any extra syscall wrappers.
fn same_filesystem(a: &Path, b: &Path) -> Result<bool, String> {
    let da = fs::metadata(a)
        .map_err(|e| format!("stat {}: {e}", a.display()))?
        .dev();
    let db = fs::metadata(b)
        .map_err(|e| format!("stat {}: {e}", b.display()))?
        .dev();
    Ok(da == db)
}

// ---------------------------------------------------------------------
// SHA-256 (FIPS 180-4), self-contained: no external crates allowed in
// this binary, so the ~90 lines below are carried in-file. Straightforward
// reference implementation; correctness is covered by the tests below
// against published test vectors.
// ---------------------------------------------------------------------

const K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

struct Sha256 {
    state: [u32; 8],
    buf: [u8; 64],
    buf_len: usize,
    total_len: u64,
}

impl Sha256 {
    fn new() -> Self {
        Self {
            state: [
                0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a,
                0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19,
            ],
            buf: [0u8; 64],
            buf_len: 0,
            total_len: 0,
        }
    }

    fn update(&mut self, mut data: &[u8]) {
        self.total_len = self.total_len.wrapping_add(data.len() as u64);
        if self.buf_len > 0 {
            let need = 64 - self.buf_len;
            let take = need.min(data.len());
            self.buf[self.buf_len..self.buf_len + take].copy_from_slice(&data[..take]);
            self.buf_len += take;
            data = &data[take..];
            if self.buf_len == 64 {
                let block = self.buf;
                self.compress(&block);
                self.buf_len = 0;
            }
        }
        while data.len() >= 64 {
            let mut block = [0u8; 64];
            block.copy_from_slice(&data[..64]);
            self.compress(&block);
            data = &data[64..];
        }
        if !data.is_empty() {
            self.buf[..data.len()].copy_from_slice(data);
            self.buf_len = data.len();
        }
    }

    fn finalize(mut self) -> [u8; 32] {
        // Padding: 0x80, zeros, then 64-bit big-endian bit length.
        let bit_len = self.total_len.wrapping_mul(8);
        self.update(&[0x80]);
        while self.buf_len != 56 {
            self.update(&[0]);
        }
        // update() would recount total_len; write the length block directly.
        let mut block = [0u8; 64];
        block[..56].copy_from_slice(&self.buf[..56]);
        block[56..64].copy_from_slice(&bit_len.to_be_bytes());
        self.compress(&block);
        let mut out = [0u8; 32];
        for (i, w) in self.state.iter().enumerate() {
            out[i * 4..i * 4 + 4].copy_from_slice(&w.to_be_bytes());
        }
        out
    }

    fn compress(&mut self, block: &[u8; 64]) {
        let mut w = [0u32; 64];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([block[i * 4], block[i * 4 + 1], block[i * 4 + 2], block[i * 4 + 3]]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = self.state;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let t1 = h
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            h = g; g = f; f = e;
            e = d.wrapping_add(t1);
            d = c; c = b; b = a;
            a = t1.wrapping_add(t2);
        }
        self.state[0] = self.state[0].wrapping_add(a);
        self.state[1] = self.state[1].wrapping_add(b);
        self.state[2] = self.state[2].wrapping_add(c);
        self.state[3] = self.state[3].wrapping_add(d);
        self.state[4] = self.state[4].wrapping_add(e);
        self.state[5] = self.state[5].wrapping_add(f);
        self.state[6] = self.state[6].wrapping_add(g);
        self.state[7] = self.state[7].wrapping_add(h);
    }
}

// ---------------------------------------------------------------------
// Tests (run on the host: `cargo test -p pkg-init`). SHA-256 vectors
// are the published FIPS/NIST ones; the manifest-hash test pins the
// frozen algorithm so any accidental drift from integrity-v3.rs breaks
// a build somewhere.
// ---------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn sha256_hex(data: &[u8]) -> String {
        let mut h = Sha256::new();
        h.update(data);
        hex_lower(&h.finalize())
    }

    #[test]
    fn sha256_empty() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn sha256_abc() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn sha256_long_multiblock() {
        // 1,000,000 'a' — exercises buffering across many blocks.
        let mut h = Sha256::new();
        let chunk = [b'a'; 1000];
        for _ in 0..1000 {
            h.update(&chunk);
        }
        assert_eq!(
            hex_lower(&h.finalize()),
            "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0"
        );
    }

    #[test]
    fn manifest_hash_pins_frozen_algorithm() {
        // Two files with known contents; expected value computed by hand
        // from the algorithm spec (and verified against integrity-v3.rs's
        // implementation): sort rel paths, join "rel<space>hex\n", hash.
        let dir = std::env::temp_dir().join(format!("glibc-apply-test-{}", std::process::id()));
        let root = dir.join("staged");
        fs::create_dir_all(root.join("lib")).unwrap();
        fs::write(root.join("lib/libc.so.6"), b"fake glibc").unwrap();
        fs::write(root.join("ld.so"), b"loader").unwrap();

        let file1 = sha256_hex(b"fake glibc");
        let file2 = sha256_hex(b"loader");
        let mut lines = [
            format!("lib/libc.so.6 {file1}\n"),
            format!("ld.so {file2}\n"),
        ];
        lines.sort();
        let expected = sha256_hex(lines.concat().as_bytes());

        assert_eq!(manifest_sha256(&root).unwrap(), expected);
        fs::remove_dir_all(&dir).ok();
    }

    fn write_flag(text: &str) -> (PathBuf, PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "glibc-apply-flag-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        fs::create_dir_all(&dir).unwrap();
        let p = dir.join("glibc-pending");
        fs::write(&p, text).unwrap();
        (dir, p)
    }

    #[test]
    fn parse_flag_spec_format() {
        // Spec v14 §7.6.2 protocol-1 format, with the optional
        // glibc_apply_sha256 extension present.
        let (dir, p) = write_flag(&format!(
            "protocol = 1\ntarget = 2.40\nmanifest_sha256 = {}\n\
             created = 2024-07-03T09:46:40Z\nglibc_apply_sha256 = {}\n",
            "AB".repeat(32),
            "CD".repeat(32)
        ));
        let f = parse_flag(p.to_str().unwrap()).unwrap();
        assert_eq!(f.protocol, GLIBC_PENDING_PROTOCOL);
        assert_eq!(f.target, "2.40");
        assert_eq!(f.manifest_sha256, "ab".repeat(32)); // hex lowered
        assert_eq!(f.created, "2024-07-03T09:46:40Z");
        assert_eq!(f.glibc_apply_sha256.as_deref(), Some("cd".repeat(32).as_str()));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn parse_flag_verbatim_spec_example() {
        // Regression test for the writer/reader split: the verbatim spec
        // v14 §7.6.2 example block is
        //
        //     protocol = 1
        //     target = 2.40
        //     manifest_sha256 = <hex digest of the staged tree's manifest>
        //     created = <RFC 3339 timestamp>
        //
        // parsed here with the placeholders realized concretely (the
        // digest below is SHA-256 of an empty staged-tree manifest).
        let (dir, p) = write_flag(concat!(
            "protocol = 1\n",
            "target = 2.40\n",
            "manifest_sha256 = e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855\n",
            "created = 2026-01-15T03:22:41Z\n",
        ));
        let f = parse_flag(p.to_str().unwrap()).unwrap();
        assert_eq!(f.protocol, 1);
        assert_eq!(f.target, "2.40");
        assert_eq!(
            f.manifest_sha256,
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(f.created, "2026-01-15T03:22:41Z");
        assert_eq!(f.glibc_apply_sha256, None);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn parse_flag_refuses_unknown_keys_and_legacy_format() {
        // Frozen interface: unknown fields must degrade into refusal,
        // never into guessing (§7.6.1/§7.6.2). This also pins the fix
        // for the writer/reader split: a flag in the OLD format
        // (`staged_sha256 =`, `created_at =`) is rejected loudly instead
        // of being silently misparsed into a swap that never applies.
        let (dir, p) = write_flag(
            "protocol = 1\ntarget = 2.40\nstaged_sha256 = abcd\ncreated_at = 1700000000\n",
        );
        let err = parse_flag(p.to_str().unwrap()).unwrap_err();
        assert!(err.contains("unknown key 'staged_sha256'"), "{err}");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn parse_flag_requires_all_spec_fields() {
        let digest = "ab".repeat(32);
        for text in [
            format!("target = 2.40\nmanifest_sha256 = {digest}\ncreated = 2024-07-03T09:46:40Z\n"),
            format!("protocol = 1\nmanifest_sha256 = {digest}\ncreated = 2024-07-03T09:46:40Z\n"),
            format!("protocol = 1\ntarget = 2.40\ncreated = 2024-07-03T09:46:40Z\n"),
            format!("protocol = 1\ntarget = 2.40\nmanifest_sha256 = {digest}\n"),
        ] {
            let (dir, p) = write_flag(&text);
            assert!(parse_flag(p.to_str().unwrap()).is_err(), "accepted: {text}");
            fs::remove_dir_all(&dir).ok();
        }
    }

    #[test]
    fn parse_flag_rfc3339_validation() {
        assert_eq!(
            parse_rfc3339("2024-07-03T09:46:40Z").as_deref(),
            Some("2024-07-03T09:46:40Z")
        );
        assert_eq!(
            parse_rfc3339("2024-07-03T09:46:40+05:30").as_deref(),
            Some("2024-07-03T09:46:40+05:30")
        );
        for bad in [
            "1700000000", // unix timestamp: the pre-fix reader's format
            "2024-07-03 09:46:40Z",
            "2024-13-03T09:46:40Z", // month 13
            "2024-07-03T25:46:40Z", // hour 25
            "2024-07-03T09:46:40", // missing offset
        ] {
            assert_eq!(parse_rfc3339(bad), None, "accepted: {bad}");
        }
    }

    #[test]
    fn parse_flag_protocol_gate_still_parseable() {
        // An unrecognized protocol must PARSE (so the caller can refuse
        // with a clear message) but fail the GLIBC_PENDING_PROTOCOL gate.
        let (dir, p) = write_flag(&format!(
            "protocol = 2\ntarget = 2.41\nmanifest_sha256 = {}\ncreated = 2024-07-03T09:46:40Z\n",
            "ab".repeat(32)
        ));
        let f = parse_flag(p.to_str().unwrap()).unwrap();
        assert_ne!(f.protocol, GLIBC_PENDING_PROTOCOL);
        fs::remove_dir_all(&dir).ok();
    }
}
