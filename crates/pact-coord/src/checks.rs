//! `run_check` (issue #461): run one check command in a lane's tree, but
//! only while one of N machine-wide check slots is free. A slot is an OS
//! file lock on `<coord root>/check-slots/slot-<i>.lock`, so every pact
//! process on the machine shares the same N and a crashed holder frees
//! its slot. See docs/design/coord.md ("run_check and check slots").

use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde::Serialize;
use tokio::io::{AsyncRead, AsyncReadExt};

pub const DEFAULT_CHECK_SLOTS: usize = 2;
pub const DEFAULT_TIMEOUT_SECONDS: u64 = 600;
pub const MAX_TIMEOUT_SECONDS: u64 = 1800;
pub(crate) const CHECK_SLOTS_DIR: &str = "check-slots";

const SLOT_POLL: Duration = Duration::from_millis(500);
const DRAIN_GRACE: Duration = Duration::from_secs(2);
const TAIL_LINES: usize = 200;
const TAIL_CHARS: usize = 20_000;
const CAPTURE_LIMIT_BYTES: usize = 1024 * 1024;

/// The directory every pact process on this machine takes slots from.
pub fn slot_dir() -> Result<PathBuf> {
    Ok(crate::db::coord_root()?.join(CHECK_SLOTS_DIR))
}

#[derive(Debug, Clone, Serialize)]
pub struct CheckReport {
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub waited_ms: u64,
    pub ran_ms: u64,
    pub slots: usize,
    pub output: String,
}

/// Waits for a free slot, runs `command` in `cwd`, and returns its report.
/// The slot is held until the command and everything it started has ended
/// or been killed.
pub(crate) async fn run_check(slot_dir: &Path, slots: usize, command: &str, cwd: &Path, timeout: Duration) -> Result<CheckReport> {
    let slots = slots.max(1);
    let queued = Instant::now();
    let _slot = acquire_slot(slot_dir, slots).await?;
    let waited_ms = queued.elapsed().as_millis() as u64;
    let started = Instant::now();
    let (status, timed_out, output) = run_command(command, cwd, timeout).await?;
    Ok(CheckReport {
        exit_code: status.and_then(|s| s.code()),
        timed_out,
        waited_ms,
        ran_ms: started.elapsed().as_millis() as u64,
        slots,
        output,
    })
}

async fn acquire_slot(dir: &Path, slots: usize) -> Result<std::fs::File> {
    std::fs::create_dir_all(dir).with_context(|| format!("creating the check slot directory {}", dir.display()))?;
    loop {
        for index in 0..slots {
            let path = dir.join(format!("slot-{index}.lock"));
            let file = std::fs::OpenOptions::new()
                .create(true)
                .truncate(false)
                .write(true)
                .open(&path)
                .with_context(|| format!("opening check slot {}", path.display()))?;
            match file.try_lock() {
                Ok(()) => return Ok(file),
                Err(std::fs::TryLockError::WouldBlock) => {}
                Err(std::fs::TryLockError::Error(err)) => {
                    return Err(err).with_context(|| format!("locking check slot {}", path.display()));
                }
            }
        }
        tokio::time::sleep(SLOT_POLL).await;
    }
}

fn shell(command: &str) -> tokio::process::Command {
    #[cfg(windows)]
    {
        let mut c = tokio::process::Command::new("cmd");
        // `/S /C "<command>"` hands cmd the text exactly as the caller
        // wrote it; Rust's own argument quoting would escape its quotes.
        c.raw_arg(format!("/S /C \"{command}\""));
        c
    }
    #[cfg(not(windows))]
    {
        let mut c = tokio::process::Command::new("sh");
        c.args(["-c", command]);
        c.process_group(0);
        c
    }
}

async fn run_command(command: &str, cwd: &Path, timeout: Duration) -> Result<(Option<ExitStatus>, bool, String)> {
    let mut child = shell(command)
        .current_dir(cwd)
        .env("CI", "true")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("failed to start `{command}` in {}", cwd.display()))?;
    let stdout = child.stdout.take().context("child stdout")?;
    let stderr = child.stderr.take().context("child stderr")?;
    let captured = Mutex::new(Captured::default());
    let mut pumps = std::pin::pin!(async { tokio::join!(pump(stdout, &captured), pump(stderr, &captured)) });
    let mut pumps_done = false;
    let deadline = tokio::time::Instant::now() + timeout;
    let ended = loop {
        tokio::select! {
            status = child.wait() => break Some(status),
            _ = &mut pumps, if !pumps_done => pumps_done = true,
            _ = tokio::time::sleep_until(deadline) => break None,
        }
    };
    let status = match ended {
        Some(status) => Some(status.with_context(|| format!("waiting for `{command}`"))?),
        None => {
            kill_tree(&mut child).await;
            None
        }
    };
    if !pumps_done {
        // Something the command started may still hold its output open;
        // the check itself is over, so stop reading after a short grace.
        let _ = tokio::time::timeout(DRAIN_GRACE, &mut pumps).await;
    }
    let output = captured.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).tail();
    Ok((status, status.is_none(), output))
}

async fn kill_tree(child: &mut tokio::process::Child) {
    if let Some(pid) = child.id() {
        #[cfg(windows)]
        let mut killer = {
            let mut c = tokio::process::Command::new("taskkill");
            c.args(["/T", "/F", "/PID", &pid.to_string()]);
            c
        };
        #[cfg(not(windows))]
        let mut killer = {
            let mut c = tokio::process::Command::new("kill");
            c.args(["-s", "KILL", "--", &format!("-{pid}")]);
            c
        };
        let _ = killer.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).status().await;
    }
    let _ = child.kill().await;
}

async fn pump(mut reader: impl AsyncRead + Unpin, sink: &Mutex<Captured>) {
    let mut buffer = [0u8; 8192];
    while let Ok(n) = reader.read(&mut buffer).await {
        if n == 0 {
            break;
        }
        sink.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).push(&buffer[..n]);
    }
}

/// Combined stdout and stderr in arrival order. Only the tail is ever
/// returned, so the front is dropped past `CAPTURE_LIMIT_BYTES`, counting
/// the lines it held.
#[derive(Default)]
struct Captured {
    bytes: Vec<u8>,
    dropped_lines: usize,
}

impl Captured {
    fn push(&mut self, chunk: &[u8]) {
        self.bytes.extend_from_slice(chunk);
        if self.bytes.len() > CAPTURE_LIMIT_BYTES {
            let cut = self.bytes.len() - CAPTURE_LIMIT_BYTES / 2;
            self.dropped_lines += self.bytes[..cut].iter().filter(|&&b| b == b'\n').count();
            self.bytes.drain(..cut);
        }
    }

    fn tail(&self) -> String {
        tail(&String::from_utf8_lossy(&self.bytes), self.dropped_lines)
    }
}

/// The last `TAIL_LINES` lines of `text`, at most `TAIL_CHARS` characters,
/// after a line saying how many earlier lines were cut when any were.
fn tail(text: &str, already_cut: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let mut kept: Vec<String> = Vec::new();
    let mut chars = 0;
    for line in lines.iter().rev() {
        if kept.len() == TAIL_LINES {
            break;
        }
        let len = line.chars().count() + 1;
        if chars + len > TAIL_CHARS {
            if kept.is_empty() {
                let skip = line.chars().count().saturating_sub(TAIL_CHARS);
                kept.push(line.chars().skip(skip).collect());
            }
            break;
        }
        chars += len;
        kept.push((*line).to_string());
    }
    kept.reverse();
    let cut = already_cut + lines.len() - kept.len();
    let body = kept.join("\n");
    if cut > 0 {
        format!("[{cut} earlier lines cut]\n{body}")
    } else {
        body
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tail_stops_at_the_character_cap_and_counts_lines_dropped_before_it() {
        let wide: String = (1..=10).map(|i| format!("{i}{}\n", "x".repeat(4_999))).collect();
        let out = tail(&wide, 3);
        assert!(out.starts_with("[10 earlier lines cut]\n8x"), "7 cut by the cap plus 3 dropped earlier: {}", &out[..40]);
        assert!(out.chars().count() <= TAIL_CHARS + 40);

        let one_huge_line = "y".repeat(TAIL_CHARS * 2);
        assert_eq!(tail(&one_huge_line, 0).chars().count(), TAIL_CHARS);
        assert_eq!(tail("ok\n", 0), "ok");
    }
}
