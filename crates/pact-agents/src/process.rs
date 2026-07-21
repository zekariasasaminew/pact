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

    // Windows .cmd shim resolution -- see DESIGN.md ("pact-deps > Windows
    // .cmd shim resolution"), same rationale as cmdutil::run.
    let mut command = if cfg!(windows) {
        let mut c = Command::new("cmd");
        c.arg("/C").arg(program);
        c
    } else {
        Command::new(program)
    };
    let mut child = command
        .args(args)
        .current_dir(cwd)
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
