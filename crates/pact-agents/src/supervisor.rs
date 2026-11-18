use std::process::ExitStatus;
use std::sync::{Mutex, MutexGuard, OnceLock};

use command_group::GroupChild;

/// Result of a non-blocking peek at a registered child, used by
/// `run_and_stream` to race the direct agent process's own exit against a
/// stdout pipe that may never see EOF -- see docs/design/agents-adapters.md ("
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
/// Ctrl-C handler can kill all of them -- see docs/design/agents-adapters.md ("
/// Supervisor and group kill"). Every `Supervisor` in a process is a
/// handle on one registry (issue #366): `ctrlc` allows one handler per
/// process, so per-supervisor registries left every supervisor but the
/// first unreachable from Ctrl-C.
pub struct Supervisor {
    children: &'static Mutex<Vec<Option<GroupChild>>>,
}

struct Registry {
    children: Mutex<Vec<Option<GroupChild>>>,
    /// Processes pact started outside a `Supervisor` (the shared ACP
    /// agent processes), killed with their trees on Ctrl-C.
    pids: Mutex<Vec<u32>>,
}

static REGISTRY: OnceLock<Registry> = OnceLock::new();

fn registry() -> &'static Registry {
    REGISTRY.get_or_init(|| {
        if let Err(err) = ctrlc::set_handler(|| {
            kill_everything();
            std::process::exit(130);
        }) {
            tracing::warn!("could not install Ctrl-C handler: {err}");
        }
        Registry { children: Mutex::new(Vec::new()), pids: Mutex::new(Vec::new()) }
    })
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Registers a process pact started without a `Supervisor` so Ctrl-C
/// kills it and its children too; call [`forget_pid`] once it is gone.
pub fn register_pid(pid: u32) {
    lock(&registry().pids).push(pid);
}

pub fn forget_pid(pid: u32) {
    lock(&registry().pids).retain(|&p| p != pid);
}

/// What the Ctrl-C handler does before exiting: kill every registered
/// child group and every registered process tree. Public so it can be
/// tested without sending a real signal.
pub fn kill_everything() {
    let registry = registry();
    for child in lock(&registry.children).iter_mut().flatten() {
        tracing::info!("Ctrl-C received: killing agent process group {}", child.id());
        let _ = child.kill();
    }
    for pid in lock(&registry.pids).drain(..) {
        tracing::info!("Ctrl-C received: killing process tree {pid}");
        kill_tree(pid);
    }
}

#[cfg(windows)]
fn kill_tree(pid: u32) {
    let _ = std::process::Command::new("taskkill")
        .args(["/T", "/F", "/PID", &pid.to_string()])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

#[cfg(not(windows))]
fn kill_tree(pid: u32) {
    let _ = std::process::Command::new("pkill").args(["-KILL", "-P", &pid.to_string()]).status();
    let _ = std::process::Command::new("kill").args(["-KILL", &pid.to_string()]).status();
}

impl Supervisor {
    pub fn new() -> Self {
        Self { children: &registry().children }
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
