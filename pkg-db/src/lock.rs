//! `/var/lib/pkg/lock` — shared/exclusive coordination for the
//! package-manager state *critical section* (§9.4).
//!
//! ## v2 changes
//!
//! - [MAJOR, design change — decided by project owner] Builds now run
//!   OUTSIDE the exclusive lock. The lock contract is: the exclusive
//!   lock is held only for the short critical section — the DB reads of
//!   installed state used for staging validation, staging, activation,
//!   and the DB commit. Long-running builds happen unlocked; at lock
//!   acquisition the caller re-validates that the resolution inputs (the
//!   installed set relevant to the plan) haven't drifted since solve
//!   time. The module doc, guard names (`SharedGuard`/`ExclusiveGuard`
//!   instead of a single `LockGuard` whose lifetime implied "the whole
//!   operation"), and `acquire_exclusive`'s documentation have been
//!   rewritten to state this contract. flock mechanics, timeouts, and
//!   holder metadata are unchanged.
//! - [MINOR] Locking now uses std's stabilized file-lock API
//!   (`File::try_lock` / `try_lock_shared`, Rust 1.89+, flock on Linux)
//!   instead of the fs4 crate — std's inherent methods shadow the fs4
//!   trait methods, so the fs4 import went from load-bearing to unused.
//! - [MINOR] Stale holder metadata: `ExclusiveGuard` now implements
//!   `Drop`, truncating the holder-info sidecar when the exclusive lock
//!   is released, so a subsequent `NoWait` failure no longer reports a
//!   dead writer's PID. See `ExclusiveGuard`'s doc for the pid-reuse
//!   caveat.
//! - [MINOR] Documented that readers use shared locks with a 2s NoWait
//!   fallback timeout and writers 60s, per §9.4.
//!
//! ## Lock contract
//!
//! Readers (`pkg info`, `pkg why`, `pkg explain`, `pkg search`,
//! `pkg check` querying the DB) take [`acquire_shared`]; per §9.4 they
//! use [`WaitMode::NoWait`] semantics with the 2s
//! [`DEFAULT_READER_FALLBACK_TIMEOUT`], falling back to filesystem
//! scanning when contended.
//!
//! Writers (`pkg install`, `pkg remove`, `pkg upgrade`, `pkg clean`,
//! `pkg db regenerate`) take [`acquire_exclusive`] — but **only for the
//! critical section**, not for the whole operation:
//!
//! 1. **Solve and build unlocked.** Resolution runs against a DB read
//!    taken under no lock (or a briefly held shared lock), and the —
//!    potentially hours-long — source build proceeds with no lock held,
//!    so concurrent readers and other builds are never blocked by it.
//! 2. **Acquire exclusive; re-validate.** Before touching installed
//!    state, the writer takes the exclusive lock (§9.4 default: 60s
//!    [`DEFAULT_WRITER_TIMEOUT`]) and immediately re-reads the installed
//!    set the plan depends on. If it has drifted since solve time
//!    (packages installed/removed by another writer's critical section),
//!    the plan is stale: abort or re-solve — do NOT proceed on solve-time
//!    assumptions.
//! 3. **Stage, activate, commit; release.** Staging validation, staging,
//!    activation (the §7.5 symlink swap), and the DB commit all happen
//!    while the exclusive guard is alive. Dropping the guard ends the
//!    critical section.
//!
//! This file is mechanism only, not policy: it takes a [`WaitMode`]
//! explicitly rather than hardcoding what a writer or reader "should" do
//! when contended, and it cannot enforce the re-validation step in item
//! 2 — that discipline lives in the caller. [`DEFAULT_WRITER_TIMEOUT`]
//! and [`DEFAULT_READER_FALLBACK_TIMEOUT`] exist so `pkg-cli` has a
//! single place to get the spec's stated defaults from, without
//! hardcoding the same numbers again — but choosing *when* to use them,
//! and honoring `--no-wait`/`--timeout`, is `pkg-cli`'s job, not this
//! file's.

use std::fs::{File, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::path::Path;
use std::thread::sleep;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

// Locking uses std's stabilized file-lock API (`File::try_lock` /
// `try_lock_shared`, flock on Linux); an earlier revision used the fs4
// crate for the same syscalls.
use pkg_core::{PkgError, PkgResult};

/// Default wait timeout for writers (§9.4): 60 seconds, applied when
/// acquiring the exclusive lock for the critical section (staging
/// validation, staging, activation, DB commit — NOT builds; see the
/// module doc). `pkg-cli`'s `--timeout` flag overrides this; this is
/// just what it defaults to.
pub const DEFAULT_WRITER_TIMEOUT: Duration = Duration::from_secs(60);

/// Default timeout before a reader gives up waiting and falls back to
/// filesystem scanning instead (§9.4, reusing `pkg check`'s existing
/// DB-unreachable fallback, §8.6): readers use shared locks with a 2s
/// NoWait-style fallback.
pub const DEFAULT_READER_FALLBACK_TIMEOUT: Duration = Duration::from_secs(2);

const POLL_INTERVAL: Duration = Duration::from_millis(200);

/// Whether an acquisition attempt should retry until `timeout` elapses,
/// or fail immediately if the lock is currently held. `pkg`'s
/// `--no-wait` flag (§9.4) maps directly to `NoWait`.
#[derive(Debug, Clone, Copy)]
pub enum WaitMode {
    Wait { timeout: Duration },
    NoWait,
}

/// Which kind of lock a guard holds. Kept as a queryable fact (tests,
/// diagnostics) even though the guard types now encode it statically.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockMode {
    Shared,
    Exclusive,
}

/// Holds an acquired shared (reader) lock for as long as this value is
/// alive — typically the duration of a single DB read, not a whole
/// command.
///
/// Deliberately has no `Drop` impl. The underlying `flock` releases
/// automatically when the file handle closes — which happens on its own
/// the moment this struct's `file` field is dropped, since the OS ties
/// the lock to the *open file description*, not to any explicit unlock
/// call. Ordinary Rust field-dropping already does the right thing here;
/// a `Drop` impl calling `.unlock()` would only be redundant with what
/// closing the fd already guarantees — verified directly: two
/// independently opened handles to the same path really do contend with
/// each other via `flock`, and dropping the first (with no explicit
/// unlock) releases it for the second, exactly as advertised.
#[derive(Debug)]
pub struct SharedGuard {
    _file: File,
}

impl SharedGuard {
    pub fn mode(&self) -> LockMode {
        LockMode::Shared
    }
}

/// Holds an acquired exclusive (writer) lock for the critical section
/// only: re-validation of the plan's installed-state inputs, staging
/// validation, staging, activation, and the DB commit (see the module
/// doc). Builds must already be done — or not yet started — when this
/// guard is alive; holding it across a build would reintroduce the
/// writer-blocks-the-world behavior the contract removed.
///
/// Unlike [`SharedGuard`], this guard DOES have a `Drop` impl — not to
/// release the lock (closing the fd still does that on its own, as with
/// the shared guard), but to truncate the holder-info sidecar written by
/// [`acquire_exclusive`] first. Without that, a later `NoWait` failure
/// would read back a *stale* PID: the writer that already finished,
/// rather than the one currently holding the lock.
///
/// Caveat: the metadata is advisory. PIDs are recycled by the OS, so a
/// reported holder PID can name an unrelated process that reused the
/// number after the real holder exited; and truncation on drop means a
/// PID is only present while that writer is actually inside its critical
/// section. Nothing may consume the sidecar for correctness — it exists
/// for the "another pkg operation is in progress (pid N)" message in
/// §9.4 and nothing else.
#[derive(Debug)]
pub struct ExclusiveGuard {
    file: File,
}

impl ExclusiveGuard {
    pub fn mode(&self) -> LockMode {
        LockMode::Exclusive
    }
}

impl Drop for ExclusiveGuard {
    fn drop(&mut self) {
        // Best-effort: clear the holder metadata so post-release
        // contention doesn't report this (possibly now-dead) writer's
        // PID. A failure here — e.g. the fd is already in a bad state —
        // is not worth propagating from Drop; the stale-metadata risk
        // that remains is exactly the pid-reuse caveat documented above.
        let _ = self.file.set_len(0);
        let _ = self.file.seek(SeekFrom::Start(0));
        let _ = self.file.flush();
        // `self.file` closes after this, releasing the flock itself.
    }
}

fn open_lock_file(path: &Path) -> PkgResult<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .open(path)
        .map_err(PkgError::from)
}

/// Overwrites the lock file's contents with the current process's PID
/// and acquisition time, so a blocked acquirer elsewhere can report a
/// useful message. Only called by exclusive acquisition (see
/// `acquire_exclusive`) — multiple readers can hold a shared lock at
/// once, so there's no single "the" holder to record for that case, and
/// the spec's own example message (§9.4) is specifically about one
/// writer blocking behind another. Cleared again when the exclusive
/// guard drops (see `ExclusiveGuard::drop`).
fn write_holder_metadata(file: &mut File) -> PkgResult<()> {
    let pid = std::process::id();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_secs();

    file.set_len(0).map_err(PkgError::from)?;
    file.seek(SeekFrom::Start(0)).map_err(PkgError::from)?;
    writeln!(file, "{pid}\n{now}").map_err(PkgError::from)?;
    file.flush().map_err(PkgError::from)?;
    Ok(())
}

fn read_holder_metadata(path: &Path) -> Option<(u32, u64)> {
    let contents = std::fs::read_to_string(path).ok()?;
    let mut lines = contents.lines();
    let pid: u32 = lines.next()?.parse().ok()?;
    let acquired_at: u64 = lines.next()?.parse().ok()?;
    Some((pid, acquired_at))
}

/// Builds the `PkgError::Locked` reported when an acquisition fails.
///
/// Best-effort: this reads the lock file's content without holding any
/// lock itself — by definition, we just failed to acquire one — so
/// there's a narrow window where this races the holder's own metadata
/// write and reads partial or stale content. That's an acceptable
/// trade-off for a diagnostic message: nothing elsewhere relies on this
/// for correctness, only for telling the person something useful. If the
/// metadata can't be read (races the write, was already truncated by a
/// released writer's `Drop`, or a shared lock never wrote any), this
/// falls back to reporting `holder_pid: 0` rather than failing the whole
/// operation over a diagnostic detail.
fn locked_error(path: &Path) -> PkgError {
    match read_holder_metadata(path) {
        Some((holder_pid, acquired_at)) => {
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or(Duration::ZERO)
                .as_secs();
            PkgError::Locked {
                holder_pid,
                elapsed_secs: now.saturating_sub(acquired_at),
            }
        }
        None => PkgError::Locked {
            holder_pid: 0,
            elapsed_secs: 0,
        },
    }
}

fn wait_for(
    file: &File,
    timeout: Duration,
    path: &Path,
    try_fn: impl Fn(&File) -> Result<(), std::fs::TryLockError>,
) -> PkgResult<()> {
    let start = Instant::now();
    loop {
        match try_fn(file) {
            Ok(()) => return Ok(()),
            Err(_) if start.elapsed() < timeout => sleep(POLL_INTERVAL),
            Err(_) => return Err(locked_error(path)),
        }
    }
}

/// Acquires a shared lock — multiple readers may hold this
/// simultaneously; it only excludes a concurrent exclusive holder.
/// Readers hold this for a single DB read, and per §9.4 use `NoWait`
/// with the 2s [`DEFAULT_READER_FALLBACK_TIMEOUT`] before falling back
/// to filesystem scanning.
pub fn acquire_shared(path: &Path, wait: WaitMode) -> PkgResult<SharedGuard> {
    let file = open_lock_file(path)?;
    match wait {
        WaitMode::NoWait => {
            file.try_lock_shared().map_err(|_| locked_error(path))?;
        }
        WaitMode::Wait { timeout } => {
            wait_for(&file, timeout, path, |f| f.try_lock_shared())?;
        }
    }
    Ok(SharedGuard { _file: file })
}

/// Acquires an exclusive lock — excludes every other shared or exclusive
/// holder for as long as the returned guard is alive.
///
/// CRITICAL SECTION ONLY (see the module doc for the full contract):
/// acquire this after builds have completed, immediately re-validate
/// that the installed set the resolution plan was computed against
/// hasn't drifted since solve time, then do staging validation,
/// staging, activation, and the DB commit — and drop the guard. Writers
/// default to a 60s timeout ([`DEFAULT_WRITER_TIMEOUT`], §9.4); the
/// short hold time is what makes that timeout reasonable.
pub fn acquire_exclusive(path: &Path, wait: WaitMode) -> PkgResult<ExclusiveGuard> {
    let mut file = open_lock_file(path)?;
    match wait {
        WaitMode::NoWait => {
            file.try_lock().map_err(|_| locked_error(path))?;
        }
        WaitMode::Wait { timeout } => {
            wait_for(&file, timeout, path, |f| f.try_lock())?;
        }
    }
    write_holder_metadata(&mut file)?;
    Ok(ExclusiveGuard { file })
}

// ---------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::thread;
    use tempfile::tempdir;

    fn lock_path() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempdir().unwrap();
        let path = dir.path().join("lock");
        (dir, path)
    }

    #[test]
    fn exclusive_excludes_second_exclusive_attempt() {
        let (_dir, path) = lock_path();
        let _guard1 = acquire_exclusive(&path, WaitMode::NoWait).unwrap();
        let result = acquire_exclusive(&path, WaitMode::NoWait);
        assert!(result.is_err());
    }

    #[test]
    fn exclusive_lock_released_on_drop_with_no_explicit_unlock() {
        let (_dir, path) = lock_path();
        let guard1 = acquire_exclusive(&path, WaitMode::NoWait).unwrap();
        assert!(acquire_exclusive(&path, WaitMode::NoWait).is_err());

        drop(guard1); // no .unlock() call anywhere — see SharedGuard's doc comment

        let guard2 = acquire_exclusive(&path, WaitMode::NoWait);
        assert!(guard2.is_ok(), "lock should be free once guard1 is dropped");
    }

    #[test]
    fn shared_locks_can_coexist() {
        let (_dir, path) = lock_path();
        let guard1 = acquire_shared(&path, WaitMode::NoWait).unwrap();
        let guard2 = acquire_shared(&path, WaitMode::NoWait);
        assert!(guard2.is_ok(), "two shared holders should coexist");
        drop(guard1);
    }

    #[test]
    fn shared_lock_blocks_exclusive_attempt() {
        let (_dir, path) = lock_path();
        let _reader = acquire_shared(&path, WaitMode::NoWait).unwrap();
        let writer = acquire_exclusive(&path, WaitMode::NoWait);
        assert!(writer.is_err(), "a pending reader should block a writer");
    }

    #[test]
    fn exclusive_lock_blocks_shared_attempt() {
        let (_dir, path) = lock_path();
        let _writer = acquire_exclusive(&path, WaitMode::NoWait).unwrap();
        let reader = acquire_shared(&path, WaitMode::NoWait);
        assert!(reader.is_err(), "an active writer should block a reader");
    }

    #[test]
    fn wait_mode_retries_and_succeeds_once_released() {
        let (_dir, path) = lock_path();
        let guard1 = acquire_exclusive(&path, WaitMode::NoWait).unwrap();

        let (tx, rx) = mpsc::channel();
        let path_clone = path.clone();
        let handle = thread::spawn(move || {
            // Hold the lock briefly, then release it.
            rx.recv().unwrap(); // wait for the signal to start releasing
            drop(guard1);
            let _ = path_clone; // path unused after drop, keeps clone simple
        });

        thread::spawn(move || {
            thread::sleep(Duration::from_millis(150));
            tx.send(()).unwrap();
        });

        let start = Instant::now();
        let result = acquire_exclusive(
            &path,
            WaitMode::Wait {
                timeout: Duration::from_secs(3),
            },
        );
        let elapsed = start.elapsed();

        assert!(result.is_ok(), "should eventually acquire once released");
        assert!(
            elapsed < Duration::from_secs(2),
            "should succeed well before the 3s timeout via polling, took {elapsed:?}"
        );
        handle.join().unwrap();
    }

    #[test]
    fn wait_mode_gives_up_after_timeout_if_never_released() {
        let (_dir, path) = lock_path();
        let _guard1 = acquire_exclusive(&path, WaitMode::NoWait).unwrap();

        let start = Instant::now();
        let result = acquire_exclusive(
            &path,
            WaitMode::Wait {
                timeout: Duration::from_millis(400),
            },
        );
        let elapsed = start.elapsed();

        assert!(result.is_err());
        assert!(elapsed >= Duration::from_millis(400));
        assert!(elapsed < Duration::from_secs(2), "shouldn't overshoot the timeout by much");
    }

    #[test]
    fn no_wait_fails_immediately_without_polling() {
        let (_dir, path) = lock_path();
        let _guard1 = acquire_exclusive(&path, WaitMode::NoWait).unwrap();

        let start = Instant::now();
        let result = acquire_exclusive(&path, WaitMode::NoWait);
        let elapsed = start.elapsed();

        assert!(result.is_err());
        assert!(elapsed < POLL_INTERVAL, "NoWait shouldn't poll at all, took {elapsed:?}");
    }

    #[test]
    fn locked_error_reports_this_processs_pid_and_small_elapsed() {
        let (_dir, path) = lock_path();
        let _guard1 = acquire_exclusive(&path, WaitMode::NoWait).unwrap();

        let err = acquire_exclusive(&path, WaitMode::NoWait).unwrap_err();
        match err {
            PkgError::Locked {
                holder_pid,
                elapsed_secs,
            } => {
                // Both handles are this same test process, so we know
                // exactly what PID to expect.
                assert_eq!(holder_pid, std::process::id());
                assert!(elapsed_secs < 5);
            }
            other => panic!("expected PkgError::Locked, got {other:?}"),
        }
    }

    #[test]
    fn exclusive_guard_truncates_holder_metadata_on_drop() {
        let (_dir, path) = lock_path();
        let guard1 = acquire_exclusive(&path, WaitMode::NoWait).unwrap();
        assert!(
            read_holder_metadata(&path).is_some(),
            "holder metadata should exist while the writer holds the lock"
        );

        drop(guard1);
        assert_eq!(
            read_holder_metadata(&path),
            None,
            "released writer must not leave a stale PID behind"
        );

        // And a subsequent holder sees only its own metadata.
        let guard2 = acquire_exclusive(&path, WaitMode::NoWait).unwrap();
        assert_eq!(
            read_holder_metadata(&path).map(|(pid, _)| pid),
            Some(std::process::id())
        );
        drop(guard2);
    }

    #[test]
    fn guard_reports_its_own_mode() {
        let (_dir, path) = lock_path();
        let shared = acquire_shared(&path, WaitMode::NoWait).unwrap();
        assert_eq!(shared.mode(), LockMode::Shared);
        drop(shared);

        let exclusive = acquire_exclusive(&path, WaitMode::NoWait).unwrap();
        assert_eq!(exclusive.mode(), LockMode::Exclusive);
    }
}
