use std::process::ExitStatus;
use std::sync::{Arc, Mutex};

use command_group::GroupChild;

/// Result of a non-blocking peek at a registered child, used by
/// `run_and_stream` to race the direct agent process's own exit against a
/// stdout pipe that may never see EOF -- see DESIGN.md ("pact-agents >
/// Windows pipe-inheritance root cause", issue #253).
pub enum ChildPoll {
    /// Still running as far as this check can tell.
    Running,
    /// Exited; here's its status.
    Exited(ExitStatus),
    /// Already removed from the registry by someone else (the Ctrl-C
    /// handler, or a prior `take`) -- nothing left to poll.
    TakenElsewhere,
}

/// Tracks every live child process group across however many concurrent
/// `run_and_stream` calls share this `Supervisor`, so one process-wide
/// Ctrl-C handler can kill all of them -- see DESIGN.md ("pact-agents >
/// Supervisor and group kill").
pub struct Supervisor {
    children: Arc<Mutex<Vec<Option<GroupChild>>>>,
}

impl Supervisor {
    pub fn new() -> Self {
        let children: Arc<Mutex<Vec<Option<GroupChild>>>> = Arc::new(Mutex::new(Vec::new()));
        let handler_children = Arc::clone(&children);
        let result = ctrlc::set_handler(move || {
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

    /// Non-blocking peek at whether the registered child has exited yet,
    /// without taking ownership of it. A read-only `try_wait` best-effort
    /// error is reported as `Running` -- the eventual real `wait()` after
    /// `take` will surface anything that actually matters.
    pub fn try_wait(&self, slot: usize) -> ChildPoll {
        let mut guard = self
            .children
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match guard[slot].as_mut() {
            None => ChildPoll::TakenElsewhere,
            Some(child) => match child.try_wait() {
                Ok(Some(status)) => ChildPoll::Exited(status),
                Ok(None) | Err(_) => ChildPoll::Running,
            },
        }
    }
}

impl Default for Supervisor {
    fn default() -> Self {
        Self::new()
    }
}
