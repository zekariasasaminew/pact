use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use command_group::CommandGroup;

use crate::event::AgentEvent;
use crate::supervisor::Supervisor;

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
    let mut child = command
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .group_spawn()
        .with_context(|| format!("failed to spawn `{program}`"))?;

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

    let stderr_log = Arc::clone(&log);
    let stderr_thread = std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            if let Ok(mut f) = stderr_log.lock() {
                let _ = writeln!(f, "[stderr] {line}");
            }
        }
    });

    let mut saw_result: Option<RunOutcome> = None;
    for line in BufReader::new(stdout).lines().map_while(Result::ok) {
        if let Ok(mut f) = log.lock() {
            let _ = writeln!(f, "{line}");
        }
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

    let _ = stderr_thread.join();

    let status = match supervisor.take(slot) {
        Some(mut c) => Some(c.wait().context("waiting for agent process to exit")?),
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
