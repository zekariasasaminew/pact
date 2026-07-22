use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};

/// A serialized, PID-aware file lock: if the previous holder's process has
/// died without cleaning up, the lock is stolen (checked via PID liveness)
/// rather than left stale forever. See DESIGN.md ("pact-vcs > PidLock
/// origin") for why this exists and where else it's reused.
pub struct PidLock {
    path: PathBuf,
}

impl PidLock {
    pub fn acquire(lock_path: &Path, timeout: Duration) -> Result<Self> {
        let start = Instant::now();
        loop {
            match OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(lock_path)
            {
                Ok(mut f) => {
                    writeln!(f, "{}", holder_token())?;
                    return Ok(Self {
                        path: lock_path.to_path_buf(),
                    });
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    if Self::steal_if_stale(lock_path)? {
                        continue;
                    }
                    if start.elapsed() > timeout {
                        bail!(
                            "timed out after {:?} waiting for lock at {}",
                            timeout,
                            lock_path.display()
                        );
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(e) => {
                    return Err(e).with_context(|| {
                        format!("failed to create lock file at {}", lock_path.display())
                    })
                }
            }
        }
    }

    /// Returns Ok(true) if a stale lock was found and removed (caller should retry).
    ///
    /// A recorded PID that still shows up as running isn't, by itself,
    /// enough to conclude the lock is genuinely held -- if the original
    /// holder died and the OS later recycled that PID for an unrelated
    /// process, a PID-only liveness check would treat the lock as live
    /// forever. `holder_token`/`parse_holder_token` also record the
    /// holder's process start time (`sysinfo::Process::start_time`,
    /// available cross-platform); a live process whose start time doesn't
    /// match the recorded one is a different process that happens to share
    /// the PID, not the original holder, so the lock is stolen. A lock file
    /// written before this field existed (bare PID, no `:start_time`) falls
    /// back to the old PID-only check rather than erroring.
    fn steal_if_stale(lock_path: &Path) -> Result<bool> {
        let contents = match fs::read_to_string(lock_path) {
            Ok(c) => c,
            Err(_) => return Ok(false),
        };
        let Some((pid, recorded_start_time)) = parse_holder_token(contents.trim()) else {
            return Ok(false);
        };

        let mut sys = sysinfo::System::new();
        sys.refresh_processes(sysinfo::ProcessesToUpdate::All, true);
        let is_stale = match sys.process(sysinfo::Pid::from_u32(pid)) {
            None => true,
            Some(process) => match recorded_start_time {
                Some(recorded) => process.start_time() != recorded,
                None => false,
            },
        };

        if is_stale {
            let _ = fs::remove_file(lock_path);
            return Ok(true);
        }
        Ok(false)
    }
}

impl Drop for PidLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// Builds this process' own lock-file contents: `<pid>:<start_time>`. Falls
/// back to a bare PID (the pre-fix format) if this process' own start time
/// can't be looked up (e.g. a `sysinfo` snapshot that doesn't include it,
/// never observed but harmless to guard) -- still correct, just loses the
/// PID-reuse protection for that one lock, same as it worked before this
/// existed.
fn holder_token() -> String {
    let pid = std::process::id();
    let mut sys = sysinfo::System::new();
    sys.refresh_processes(sysinfo::ProcessesToUpdate::All, true);
    let start_time = sysinfo::get_current_pid()
        .ok()
        .and_then(|p| sys.process(p))
        .map(|p| p.start_time());
    match start_time {
        Some(t) => format!("{pid}:{t}"),
        None => pid.to_string(),
    }
}

/// Parses a lock file's contents into `(pid, start_time)` -- `start_time`
/// is `None` for a bare-PID lock file predating this field. Returns `None`
/// if the contents don't parse as either format at all (a corrupt/foreign
/// file), same as the pre-fix behavior of treating an unparseable lock file
/// as "not ours to steal, leave it alone."
fn parse_holder_token(token: &str) -> Option<(u32, Option<u64>)> {
    if let Some((pid_str, start_str)) = token.split_once(':') {
        let pid = pid_str.parse().ok()?;
        let start_time = start_str.parse().ok()?;
        return Some((pid, Some(start_time)));
    }
    token.parse().ok().map(|pid| (pid, None))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_holder_token_reads_new_format_with_start_time() {
        assert_eq!(parse_holder_token("1234:5678"), Some((1234, Some(5678))));
    }

    #[test]
    fn parse_holder_token_reads_old_bare_pid_format() {
        assert_eq!(parse_holder_token("1234"), Some((1234, None)));
    }

    #[test]
    fn parse_holder_token_rejects_garbage() {
        assert_eq!(parse_holder_token("not-a-pid"), None);
        assert_eq!(parse_holder_token("1234:not-a-time"), None);
        assert_eq!(parse_holder_token(""), None);
    }

    #[test]
    fn acquire_writes_a_token_steal_if_stale_can_parse_back() {
        let dir = std::env::temp_dir().join(format!("pact-pidlock-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let lock_path = dir.join("test.lock");

        let lock = PidLock::acquire(&lock_path, Duration::from_secs(1)).unwrap();
        let contents = std::fs::read_to_string(&lock_path).unwrap();
        let (pid, start_time) = parse_holder_token(contents.trim()).expect("lock file must parse");
        assert_eq!(pid, std::process::id());
        assert!(start_time.is_some(), "expected this process' own start time to be recorded");

        drop(lock);
        assert!(!lock_path.exists(), "Drop must remove the lock file");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn steal_if_stale_steals_a_lock_whose_pid_no_longer_exists() {
        let dir = std::env::temp_dir().join(format!("pact-pidlock-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let lock_path = dir.join("test.lock");
        // PID 0 is never a real user process on any platform pact targets.
        std::fs::write(&lock_path, "0:1").unwrap();

        assert!(PidLock::steal_if_stale(&lock_path).unwrap());
        assert!(!lock_path.exists());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn steal_if_stale_steals_a_lock_whose_pid_was_reused() {
        let dir = std::env::temp_dir().join(format!("pact-pidlock-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let lock_path = dir.join("test.lock");

        // This process' own PID is genuinely live, but recorded with a
        // start time that doesn't match -- simulates the PID having been
        // recycled since the lock was written.
        let pid = std::process::id();
        std::fs::write(&lock_path, format!("{pid}:1")).unwrap();

        assert!(
            PidLock::steal_if_stale(&lock_path).unwrap(),
            "a live PID with a mismatched start time must be treated as stale, not as still held"
        );
        assert!(!lock_path.exists());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn steal_if_stale_does_not_steal_a_genuinely_live_lock() {
        let dir = std::env::temp_dir().join(format!("pact-pidlock-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let lock_path = dir.join("test.lock");
        std::fs::write(&lock_path, holder_token()).unwrap();

        assert!(!PidLock::steal_if_stale(&lock_path).unwrap());
        assert!(lock_path.exists());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn steal_if_stale_still_honors_the_old_bare_pid_format() {
        let dir = std::env::temp_dir().join(format!("pact-pidlock-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let lock_path = dir.join("test.lock");
        // Bare PID, no start time -- a lock file from before this fix. This
        // process' own PID is live, so a bare-format entry for it must not
        // be stolen (can't detect PID reuse without a recorded start time,
        // same limitation the pre-fix code had for every lock).
        std::fs::write(&lock_path, std::process::id().to_string()).unwrap();

        assert!(!PidLock::steal_if_stale(&lock_path).unwrap());
        assert!(lock_path.exists());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
