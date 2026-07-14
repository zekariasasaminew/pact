use std::sync::{Arc, Mutex};

use command_group::GroupChild;

/// Tracks every live child process group across however many concurrent
/// `run_and_stream` calls share this `Supervisor`, so one process-wide
/// Ctrl-C handler can kill all of them -- not the single-shot, one-child
/// assumption `run_and_stream`'s old self-installed handler made. Single-
/// `spawn` and `spawn-many` both go through a `Supervisor` now: `spawn`
/// just creates its own with exactly one registrant for the duration of
/// that one call, so its observable behavior (one handler, killing one
/// child, installed and torn down within a single `run_and_stream` call)
/// is unchanged; only the mechanism moved from a bare function into this
/// small object so `spawn-many` can share one across N threads.
///
/// Registering (and killing) the whole *group* -- a Job Object on Windows,
/// a real POSIX process group via `command_group`'s `process_group(0)` on
/// Unix -- rather than just the tracked child, also fixes a real gap the
/// single-agent Ctrl-C path had before this existed: a Bash tool call
/// spawns a child shell process, and the old plain `Child::kill()` only
/// killed the immediate agent process, leaving that shell (and anything
/// *it* started) still running. `teardown`'s Windows `taskkill /T` already
/// handled this; Ctrl-C during a still-running `spawn` did not, until now.
pub struct Supervisor {
    children: Arc<Mutex<Vec<Option<GroupChild>>>>,
}

impl Supervisor {
    pub fn new() -> Self {
        let children: Arc<Mutex<Vec<Option<GroupChild>>>> = Arc::new(Mutex::new(Vec::new()));
        let handler_children = Arc::clone(&children);
        let result = ctrlc::set_handler(move || {
            // A prior panic while holding this lock (e.g. inside another
            // thread's own cleanup) must not make every other live child
            // unkillable on Ctrl-C -- recover the poisoned guard rather
            // than bailing out of the handler.
            let mut guard = handler_children
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            for child in guard.iter_mut().flatten() {
                tracing::info!(
                    "Ctrl-C received: killing agent process group {}",
                    child.id()
                );
                let _ = child.kill();
            }
            std::process::exit(130);
        });
        if let Err(err) = result {
            // Not fatal -- e.g. a handler is already installed by an outer
            // caller. The agent process(es) just won't be killed on
            // Ctrl-C in that case.
            tracing::warn!("could not install Ctrl-C handler: {err}");
        }
        Self { children }
    }

    /// Registers a freshly spawned child group so Ctrl-C can reach it.
    /// Returns a slot index used to reclaim it once the process has
    /// actually exited normally (`take`), so a long spawn-many run doesn't
    /// keep dead entries around for the rest of the batch.
    pub fn register(&self, child: GroupChild) -> usize {
        let mut guard = self
            .children
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        guard.push(Some(child));
        guard.len() - 1
    }

    /// Takes ownership of the child back out of the registry (e.g. to call
    /// `.wait()` on it without the Ctrl-C handler also racing to kill it).
    /// Returns `None` if the Ctrl-C handler already reaped it first.
    pub fn take(&self, slot: usize) -> Option<GroupChild> {
        let mut guard = self
            .children
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        guard[slot].take()
    }
}

impl Default for Supervisor {
    fn default() -> Self {
        Self::new()
    }
}
