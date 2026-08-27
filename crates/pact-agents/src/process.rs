use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use command_group::CommandGroup;

use crate::event::AgentEvent;
use crate::supervisor::{ChildPoll, Supervisor};

/// How often the main loop re-checks the direct child's own exit status
/// while waiting for the next stdout line -- see the loop in
/// `run_and_stream` for why this polling exists at all.
const CHILD_EXIT_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// How long to keep waiting for more stdout lines after the direct child
/// is first observed to have exited, before giving up on the reader
/// thread entirely -- found necessary by a real, intermittent failure
/// under genuine parallel test contention (not a hypothetical): `try_wait`
/// reporting the child gone and the reader thread delivering that child's
/// already-written final line (its `Result` event, in practice) are two
/// independent signals, racing each other, so a `child_exited` check that
/// broke immediately could observe the exit before the last line arrived
/// and silently drop it. A short grace period, not an immediate break,
/// closes that window while staying bounded (unlike the pipe itself, a
/// grandchild can hold open indefinitely).
const CHILD_EXIT_GRACE_PERIOD: Duration = Duration::from_millis(500);

/// How an agent process run ended.
#[derive(Debug, Clone)]
pub struct RunOutcome {
    pub success: bool,
    pub summary: String,
}

/// Builds the `Command` used to launch an agent CLI. On Windows, resolving
/// straight to a real, directly-executable target (a native `.exe`, or the
/// interpreter+script a `.cmd` shim ultimately execs -- see
/// `windows_shim::resolve`) avoids `cmd.exe` reparsing the command line at
/// all, since that reparsing silently truncates a multi-line `-p` prompt
/// (and every flag after it) at the first embedded newline -- confirmed by
/// hand, see DESIGN.md ("pact-agents > Windows multi-line prompt
/// truncation", issue #210). Only falls back to the old `cmd /C <program>`
/// wrapper when `program` doesn't resolve either way (an unrecognized
/// shim shape), preserving prior behavior for that case exactly.
#[cfg(windows)]
fn build_agent_command(program: &str) -> Command {
    match crate::windows_shim::resolve(program) {
        Some(resolved) => {
            let mut c = Command::new(resolved.program);
            c.args(&resolved.leading_args);
            c
        }
        None => {
            let mut c = Command::new("cmd");
            c.arg("/C").arg(program);
            c
        }
    }
}

#[cfg(not(windows))]
fn build_agent_command(program: &str) -> Command {
    Command::new(program)
}

/// Spawns `program args` in `cwd`, streaming its stdout as NDJSON to
/// `on_event` and appending every raw line to `log_path` -- see DESIGN.md
/// ("pact-agents > run_and_stream") for `on_pid`'s timing, the stderr
/// draining approach, and why `parse_line` returns zero-or-more events per
/// line rather than exactly one.
///
/// Deliberately does not wait for stdout EOF to decide the run is over
/// (issue #253): on Windows, `std::process::Command`'s `bInheritHandles`
/// duplicates every inheritable handle in this process into the spawned
/// agent, including the stdout pipe's write end -- if that agent spawns its
/// own subprocess (an MCP sidecar) without explicitly excluding it, the
/// grandchild holds the write end open long after the agent itself exits,
/// and a naive "read until EOF" blocks for the grandchild's entire
/// lifetime. Instead, stdout is read on a background thread that feeds a
/// channel, and the main loop races each line against a poll of the
/// *direct* child's own exit status, returning as soon as that child is
/// gone -- see DESIGN.md for the full writeup and why this is preferred
/// over OS-level handle-inheritance controls (which can restrict what the
/// direct child receives, but can't stop that child from re-exposing an
/// inherited, still-inheritable handle to its own children).
#[allow(clippy::too_many_arguments)]
pub fn run_and_stream(
    supervisor: &Supervisor,
    program: &str,
    args: &[String],
    cwd: &Path,
    log_path: &Path,
    parse_line: impl Fn(&str) -> Vec<AgentEvent>,
    mut on_event: impl FnMut(&AgentEvent),
    on_pid: impl FnOnce(u32),
) -> Result<RunOutcome> {
    if let Some(parent) = log_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let log = Arc::new(Mutex::new(
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(log_path)
            .with_context(|| format!("opening log file {}", log_path.display()))?,
    ));

    let mut command = build_agent_command(program);
    // pact only ever feeds the agent via the task/`-p` argument -- it never
    // writes to the child's stdin. Explicitly closed rather than left
    // inherited (Rust's default): an inherited, ambiguous stdin handle
    // (a terminal, a pipe, whatever pact's own parent process happened to
    // have) is exactly the kind of thing that can make a CLI behave
    // differently than a genuinely headless invocation would -- confirmed
    // by hand, a real Claude Code invocation once printed "no stdin data
    // received in 3s, proceeding without it" and appeared to have received
    // an empty task despite a real, non-empty `-p` value (issue #184).
    command
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut group_builder = command.group();
    // Windows-only OS-level hardening (issue #237): closing the job
    // handle -- which happens automatically when pact's own process
    // exits, cleanly or not -- kills every process still in the job.
    // `command_group`'s std-`Command` integration only exposes this on
    // Windows without the crate's `with-tokio` feature (which pact
    // doesn't enable); the explicit post-wait kill below is the portable
    // half of the fix and covers Unix too.
    #[cfg(windows)]
    group_builder.kill_on_drop(true);
    let mut child = group_builder.spawn().with_context(|| format!("failed to spawn `{program}`"))?;

    let pid = child.id();
    on_pid(pid);

    let stdout = child
        .inner()
        .stdout
        .take()
        .context("child had no stdout pipe")?;
    let stderr = child
        .inner()
        .stderr
        .take()
        .context("child had no stderr pipe")?;

    let slot = supervisor.register(child);

    // Both reader threads are deliberately detached, not joined: on
    // Windows, either pipe can be kept open indefinitely by a grandchild
    // neither thread controls (issue #253), so joining would just move the
    // hang from the main loop into the join call. Whatever they've already
    // written to the log by the time this function returns is what a real
    // agent CLI would actually have produced by then anyway -- nothing
    // meaningful is lost by not waiting for their natural end.
    let stderr_log = Arc::clone(&log);
    std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            if let Ok(mut f) = stderr_log.lock() {
                let _ = writeln!(f, "[stderr] {line}");
            }
        }
    });

    let (stdout_tx, stdout_rx) = mpsc::channel::<String>();
    let stdout_log = Arc::clone(&log);
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if let Ok(mut f) = stdout_log.lock() {
                let _ = writeln!(f, "{line}");
            }
            if stdout_tx.send(line).is_err() {
                break;
            }
        }
    });

    let mut saw_result: Option<RunOutcome> = None;
    // Set once the direct child is observed to have exited -- from that
    // point on, a still-open pipe is a leaked grandchild's problem, not
    // this loop's. Not an immediate break, though: the exit is observed
    // via `try_wait`, a separate signal from the reader thread's own
    // channel, so a line the child already wrote (its final `Result`
    // event, say) can still be in flight, not yet delivered, at the exact
    // instant `try_wait` reports it gone. `child_exited` switches the
    // poll to a short, bounded grace period instead of stopping cold, so
    // that race can't silently drop the run's last few lines.
    let mut child_exited = false;
    loop {
        let poll_interval = if child_exited { CHILD_EXIT_GRACE_PERIOD } else { CHILD_EXIT_POLL_INTERVAL };
        match stdout_rx.recv_timeout(poll_interval) {
            Ok(line) => {
                for parsed in parse_line(&line) {
                    if let AgentEvent::Result { success, summary } = &parsed {
                        saw_result = Some(RunOutcome {
                            success: *success,
                            summary: summary.clone(),
                        });
                    }
                    on_event(&parsed);
                }
            }
            // Real EOF (the direct child, or nothing else, held the write
            // end -- the common, unaffected case): no more lines are
            // coming, so there's nothing left to race.
            Err(RecvTimeoutError::Disconnected) => break,
            Err(RecvTimeoutError::Timeout) => {
                if child_exited {
                    // Already gave it a full grace period since the exit
                    // was observed, with nothing arriving -- a lingering
                    // grandchild's copy of the handle is not this
                    // process's problem to wait out any further.
                    break;
                }
                if !matches!(supervisor.try_wait(slot), ChildPoll::Running) {
                    child_exited = true;
                }
            }
        }
    }

    let status = match supervisor.take(slot) {
        Some(mut c) => {
            let status = c.wait().context("waiting for agent process to exit")?;
            // Issue #237: waiting only for the direct agent process to
            // exit is not the same as the whole process *group* being
            // gone -- a real production run found agent-CLI grandchildren
            // (the agent's own MCP-server sidecar, spawned by the agent
            // CLI itself, not by pact) still alive after `spawn-many`
            // finished and pact's own process had already exited, holding
            // the terminal's stdout pipe open. Sweeping the group here,
            // right after the direct child exits, is a no-op if nothing
            // is left (the common case) and catches exactly this
            // survivor case when something is. Best-effort: a failure to
            // kill an already-gone group is expected, not worth failing
            // this run over.
            if let Err(err) = c.kill() {
                tracing::debug!("post-exit group sweep for pid {pid} found nothing to kill (expected in the common case): {err}");
            }
            Some(status)
        }
        None => None, // already reaped by the ctrlc handler
    };

    Ok(saw_result.unwrap_or_else(|| {
        // No adapter-level Result event -- see DESIGN.md ("pact-agents >
        // run_and_stream") for why the exit code is the fallback signal.
        match status {
            Some(status) => RunOutcome {
                success: status.success(),
                summary: format!("process exited ({status}) without emitting a result event"),
            },
            None => RunOutcome {
                success: false,
                summary: "process was interrupted before emitting a result event".to_string(),
            },
        }
    }))
}
