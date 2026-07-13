use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};

/// A serialized, PID-aware lock around `git worktree` operations.
///
/// Git itself races on `.git/config.lock` when `git worktree add`/`remove`
/// run concurrently (see anthropics/claude-code#34645) -- this lock makes
/// the orchestrator serialize those calls instead of relying on git's own
/// (currently unsafe-under-concurrency) locking. If the previous holder's
/// process has died without cleaning up, the lock is stolen rather than
/// left stale forever.
pub struct GitLock {
    path: PathBuf,
}

impl GitLock {
    pub fn acquire(lock_path: &Path, timeout: Duration) -> Result<Self> {
        let start = Instant::now();
        loop {
            match OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(lock_path)
            {
                Ok(mut f) => {
                    writeln!(f, "{}", std::process::id())?;
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
                            "timed out after {:?} waiting for git lock at {}",
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
    fn steal_if_stale(lock_path: &Path) -> Result<bool> {
        let contents = match fs::read_to_string(lock_path) {
            Ok(c) => c,
            Err(_) => return Ok(false),
        };
        let pid: u32 = match contents.trim().parse() {
            Ok(p) => p,
            Err(_) => return Ok(false),
        };

        let mut sys = sysinfo::System::new();
        sys.refresh_processes(sysinfo::ProcessesToUpdate::All, true);
        if sys.process(sysinfo::Pid::from_u32(pid)).is_none() {
            let _ = fs::remove_file(lock_path);
            return Ok(true);
        }
        Ok(false)
    }
}

impl Drop for GitLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}
