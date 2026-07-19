use std::sync::{Arc, Mutex};

use command_group::GroupChild;

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
}

impl Default for Supervisor {
    fn default() -> Self {
        Self::new()
    }
}
