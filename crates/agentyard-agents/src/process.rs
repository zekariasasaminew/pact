use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};

use crate::event::{self, AgentEvent};

/// How an agent process run ended.
#[derive(Debug, Clone)]
pub struct RunOutcome {
    pub success: bool,
    pub summary: String,
}

/// Spawns `program args` in `cwd`, streaming its stdout as NDJSON.
///
/// Every raw stdout line is appended to `log_path` as-is (not the
/// re-serialized `AgentEvent`) so schema drift or fields this parser
/// doesn't know about yet aren't lost -- then parsed and handed to
/// `on_event`. `on_pid` is called once, immediately after spawning, so the
/// caller can persist the PID before this function blocks -- that's what
/// lets a `teardown` invoked from a different process find and kill a
/// still-running agent.
///
/// Installs a process-wide Ctrl-C handler that kills the child before
/// letting the interrupt terminate `agentyard` itself. This is a
/// single-shot design: it assumes at most one call to `run_and_stream` is
/// active per process, matching today's blocking one-agent-per-`spawn`
/// architecture. A future phase that supervises several agents
/// concurrently *in one process* will need a different signal-handling
/// strategy (e.g. a shared registry of live children), not another call
/// to this function.
///
/// stderr is drained on its own thread into the same log file (prefixed
/// `[stderr] `) rather than left inherited or piped-but-undrained --
/// either of those risks interleaved garbage in the terminal or a
/// full-pipe deadlock if the child writes enough of it.
pub fn run_and_stream(
    program: &str,
    args: &[String],
    cwd: &Path,
    log_path: &Path,
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

    let mut child = Command::new(program)
        .args(args)
        .current_dir(cwd)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("failed to spawn `{program}`"))?;

    let pid = child.id();
    on_pid(pid);

    let stdout = child.stdout.take().context("child had no stdout pipe")?;
    let stderr = child.stderr.take().context("child had no stderr pipe")?;

    let child = Arc::new(Mutex::new(Some(child)));
    install_ctrlc_handler(Arc::clone(&child));

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
        let parsed = event::parse_line(&line);
        if let AgentEvent::Result { success, summary } = &parsed {
            saw_result = Some(RunOutcome {
                success: *success,
                summary: summary.clone(),
            });
        }
        on_event(&parsed);
    }

    let _ = stderr_thread.join();

    let status = {
        let mut guard = child.lock().expect("child mutex poisoned");
        match guard.take() {
            Some(mut c) => Some(c.wait().context("waiting for agent process to exit")?),
            None => None, // already reaped by the ctrlc handler
        }
    };

    Ok(saw_result.unwrap_or_else(|| RunOutcome {
        success: false,
        summary: match status {
            Some(status) => {
                format!("process exited ({status}) without emitting a result event")
            }
            None => "process was interrupted before emitting a result event".to_string(),
        },
    }))
}

fn install_ctrlc_handler(child: Arc<Mutex<Option<Child>>>) {
    let result = ctrlc::set_handler(move || {
        if let Ok(mut guard) = child.lock() {
            if let Some(mut c) = guard.take() {
                tracing::info!("Ctrl-C received: killing agent process");
                let _ = c.kill();
            }
        }
        std::process::exit(130);
    });
    if let Err(err) = result {
        // Not fatal -- e.g. a handler is already installed by an outer
        // caller. The agent process just won't be killed on Ctrl-C in
        // that case.
        tracing::warn!("could not install Ctrl-C handler: {err}");
    }
}
