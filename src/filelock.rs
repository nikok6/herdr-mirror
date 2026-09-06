// OS-backed advisory locking (`flock(2)`), shared by every serialization this
// crate needs across processes: the ssh master-socket lock (remote.rs) and
// the per-host state-map lock (state.rs). One implementation so both get the
// identical acquire/release/timeout semantics rather than two hand-rolled
// variants drifting apart.
//
// Lock order (must be respected by every caller that could hold more than
// one at once, to rule out deadlock rather than merely avoid it in today's
// call graph): `daemon.lock` (whole-daemon singleton, cmd_start) outermost,
// then a per-host state lock (state.rs), then a per-master-socket lock
// (remote.rs) innermost. In practice only converge ever holds two at once —
// the state lock while it acquires a stream-pool slot's master lock — and it
// does so in exactly this order.

use std::fs;
use std::os::fd::AsRawFd;
use std::path::Path;
use std::time::Duration;

use crate::util::{err, Result};

/// One process's hold on a path's flock. Dropping it closes the file, which
/// releases the flock immediately — including on a crash, since the kernel
/// releases every flock an exiting process still holds. There is
/// deliberately no "is this lock stale" check anywhere in this module:
/// flock's release-on-close is exactly what makes one unnecessary, and a
/// hand-rolled staleness heuristic (a PID file, an mtime cutoff, ...) would
/// itself be a second race to get wrong.
pub struct FileLock {
    _file: fs::File,
}

fn open_lock_file(path: &Path) -> Result<fs::File> {
    use std::os::unix::fs::OpenOptionsExt;

    // Mode is set on create only (existing files keep whatever mode they
    // already have); umask can only narrow 0600 further, never widen it.
    fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .open(path)
        .map_err(|e| err(format!("cannot open lock file {}: {e}", path.display())))
}

/// One nonblocking `flock` attempt.
pub fn try_lock_nb(path: &Path) -> Result<Option<FileLock>> {
    let file = open_lock_file(path)?;
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
        return Ok(Some(FileLock { _file: file }));
    }
    let e = std::io::Error::last_os_error();
    if e.kind() == std::io::ErrorKind::WouldBlock {
        return Ok(None);
    }
    Err(err(format!("cannot lock {}: {e}", path.display())))
}

/// Acquire `path`'s lock, blocking the calling thread until held or `budget`
/// elapses. For synchronous call sites only (e.g. `cmd_restore`) that have no
/// tokio runtime entered to use `acquire_async` — anything running on a
/// tokio worker thread must use `acquire_async` instead, or this would
/// freeze the whole runtime. Polls `try_lock_nb` on the same interval as
/// `acquire_async` rather than a true blocking `flock`, so a stuck holder
/// fails this loudly on a bound rather than hanging the caller forever —
/// unlike `cmd_start`'s separate, deliberately unbounded `daemon.lock` (a
/// one-time startup singleton, not a per-operation lock).
pub fn acquire_blocking(path: &Path, budget: Duration) -> Result<FileLock> {
    let deadline = std::time::Instant::now() + budget;
    loop {
        if let Some(lock) = try_lock_nb(path)? {
            return Ok(lock);
        }
        if std::time::Instant::now() >= deadline {
            return Err(err(format!("timed out waiting for the lock on {}", path.display())));
        }
        std::thread::sleep(RETRY_INTERVAL);
    }
}

const RETRY_INTERVAL: Duration = Duration::from_millis(50);

/// Acquire `path`'s lock asynchronously, retrying a nonblocking `flock` on a
/// bounded interval rather than parking a tokio worker thread on a real
/// blocking wait. Each attempt runs on tokio's blocking-task pool via
/// `spawn_blocking` (a single syscall, back almost immediately either way);
/// the `sleep` between attempts is an ordinary async wait, so this never
/// blocks the async runtime. Bounded by `budget`: a peer that never releases
/// (crashed processes release automatically; a genuinely wedged one does
/// not) can't stall this caller past it.
pub async fn acquire_async(path: &Path, budget: Duration) -> Result<FileLock> {
    let deadline = tokio::time::Instant::now() + budget;
    loop {
        let attempt_path = path.to_path_buf();
        let acquired = tokio::task::spawn_blocking(move || try_lock_nb(&attempt_path))
            .await
            .map_err(|e| err(format!("lock task did not complete: {e}")))??;
        if let Some(lock) = acquired {
            return Ok(lock);
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(err(format!("timed out waiting for the lock on {}", path.display())));
        }
        tokio::time::sleep(RETRY_INTERVAL).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// pid + nanos alone can repeat across a genuinely parallel full-suite
    /// run (an isolated sandbox reusing low pids in its own pid namespace);
    /// the atomic counter rules out same-process reuse deterministically.
    fn test_path(name: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);

        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
        let salt = &counter as *const u64 as usize;
        let unique =
            crate::util::short_hash(&format!("{}-{nanos}-{counter}-{salt:x}", std::process::id()));
        std::env::temp_dir().join(format!("herdr-mirror-filelock-{name}-{unique}.lock"))
    }

    #[test]
    fn try_lock_nb_excludes_a_second_open_file_description_on_the_same_path() {
        let path = test_path("excl");
        let first = try_lock_nb(&path).unwrap().expect("first attempt must acquire");
        assert!(try_lock_nb(&path).unwrap().is_none(), "a second open must not also acquire");
        drop(first);
        // No caller in this crate assumes a release is visible to the very
        // next attempt — `acquire_async`/`acquire_blocking` always retry on a
        // bounded interval — so the test shouldn't assert a same-instant
        // guarantee either; under heavy concurrent test-suite scheduling
        // (only there — never serial, never in an isolated stress loop of
        // thousands of iterations) an immediate recheck occasionally saw a
        // just-released lock reported as still held for a few milliseconds.
        let mut relockable = false;
        for _ in 0..20 {
            if try_lock_nb(&path).unwrap().is_some() {
                relockable = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(relockable, "the path must be lockable again once released");
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn a_freshly_created_lock_file_is_private() {
        use std::os::unix::fs::PermissionsExt;

        let path = test_path("mode");
        let _lock = try_lock_nb(&path).unwrap().expect("must acquire");
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "lock files must not be group/world readable regardless of umask");
        let _ = fs::remove_file(&path);
    }

    /// The property every caller of this module relies on: two concurrent
    /// holders of the same path must never be inside the locked section at
    /// the same time. Real OS threads with independent `File::open`s, since
    /// `flock` exclusion is keyed on the open file description, not the
    /// owning process — the same kernel mechanism a second process gets.
    #[test]
    fn locks_actually_exclude_concurrent_holders() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        let path = test_path("serialize");
        let overlap = Arc::new(AtomicUsize::new(0));
        let max_overlap = Arc::new(AtomicUsize::new(0));

        let handles: Vec<_> = (0..8)
            .map(|_| {
                let path = path.clone();
                let overlap = overlap.clone();
                let max_overlap = max_overlap.clone();
                std::thread::spawn(move || loop {
                    let Some(_guard) = try_lock_nb(&path).unwrap() else {
                        std::thread::sleep(Duration::from_millis(1));
                        continue;
                    };
                    let now = overlap.fetch_add(1, Ordering::SeqCst) + 1;
                    max_overlap.fetch_max(now, Ordering::SeqCst);
                    std::thread::sleep(Duration::from_millis(5));
                    overlap.fetch_sub(1, Ordering::SeqCst);
                    break;
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(max_overlap.load(Ordering::SeqCst), 1, "holders of the same path must never overlap");
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn acquire_blocking_waits_for_release_within_its_budget() {
        let path = test_path("blocking");
        let held = try_lock_nb(&path).unwrap().expect("test setup must acquire first");
        let release_at = Duration::from_millis(30);
        let path2 = path.clone();
        let releaser = std::thread::spawn(move || {
            std::thread::sleep(release_at);
            drop(held);
        });
        let started = std::time::Instant::now();
        let _second = acquire_blocking(&path2, Duration::from_secs(2)).expect("must eventually acquire");
        assert!(started.elapsed() >= release_at, "must have actually waited for the release");
        releaser.join().unwrap();
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn acquire_blocking_times_out_on_a_held_lock() {
        let path = test_path("blocking-timeout");
        let _held = try_lock_nb(&path).unwrap().expect("test setup must acquire first");
        let budget = Duration::from_millis(150);
        let started = std::time::Instant::now();
        let result = acquire_blocking(&path, budget);
        assert!(result.is_err(), "a lock nobody ever releases must time out, not hang forever");
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "must fail within its budget rather than blocking indefinitely"
        );
        let _ = fs::remove_file(&path);
    }

    #[tokio::test]
    async fn acquire_async_succeeds_once_the_holder_releases() {
        let path = test_path("async-acquire");
        let held = try_lock_nb(&path).unwrap().expect("test setup must acquire first");
        let path2 = path.clone();
        let released = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            drop(held);
            path2
        });
        acquire_async(&path, Duration::from_secs(2)).await.expect("must acquire once the holder releases");
        released.await.unwrap();
        let _ = fs::remove_file(&path);
    }

    /// A budget that expires before the holder releases must fail loudly,
    /// not hang — every caller's non-fatal fallback depends on this
    /// returning in bounded time.
    #[tokio::test]
    async fn acquire_async_times_out_on_a_held_lock() {
        let path = test_path("async-timeout");
        let _held = try_lock_nb(&path).unwrap().expect("test setup must acquire first");
        let result = acquire_async(&path, Duration::from_millis(150)).await;
        assert!(result.is_err(), "must not hang past its budget");
        let _ = fs::remove_file(&path);
    }
}
