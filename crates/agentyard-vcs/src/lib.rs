mod lock;

pub use lock::PidLock;

use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

const LOCK_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Workspace {
    pub id: String,
    pub path: PathBuf,
    pub branch: String,
    pub task: String,
    pub created_at: u64,
    /// PID of the agent process currently running in this workspace, if
    /// any. Set right after the process is spawned (before blocking on its
    /// output) so a `teardown` invoked from a *different* CLI call -- while
    /// the agent is still running in another terminal -- can find and kill
    /// it before removing the worktree out from under it.
    #[serde(default)]
    pub agent_pid: Option<u32>,
}

/// Owns the lifecycle of git-worktree-backed agent workspaces for one repo.
/// State (locks, worktree metadata, and the worktrees themselves) lives as a
/// sibling of the repo, not inside its working tree, so it never shows up in
/// `git status` for the main repo.
pub struct WorkspaceManager {
    repo_root: PathBuf,
    state_dir: PathBuf,
}

impl WorkspaceManager {
    pub fn open(repo_root: impl Into<PathBuf>) -> Result<Self> {
        let repo_root = repo_root.into();
        if !repo_root.join(".git").exists() {
            bail!(
                "{} does not look like a git repository root (no .git found)",
                repo_root.display()
            );
        }

        let repo_name = repo_root
            .file_name()
            .context("repo root has no directory name")?;
        let state_dir = repo_root
            .parent()
            .context("repo root has no parent directory")?
            .join(format!(".agentyard-{}", repo_name.to_string_lossy()));

        std::fs::create_dir_all(state_dir.join("locks"))?;
        std::fs::create_dir_all(state_dir.join("meta"))?;
        std::fs::create_dir_all(state_dir.join("workspaces"))?;

        Ok(Self {
            repo_root,
            state_dir,
        })
    }

    pub fn state_dir(&self) -> &PathBuf {
        &self.state_dir
    }

    fn lock_path(&self) -> PathBuf {
        self.state_dir.join("locks").join("git.lock")
    }

    fn meta_path(&self, id: &str) -> PathBuf {
        self.state_dir.join("meta").join(format!("{id}.json"))
    }

    pub fn create_workspace(&self, task: &str) -> Result<Workspace> {
        let id = short_id();
        let branch = format!("agentyard/{id}");
        let path = self.state_dir.join("workspaces").join(&id);

        {
            let _lock = PidLock::acquire(&self.lock_path(), LOCK_TIMEOUT)
                .context("acquiring git worktree lock")?;

            let output = Command::new("git")
                .args(["worktree", "add"])
                .arg(&path)
                .args(["-b", &branch])
                .current_dir(&self.repo_root)
                .output()
                .context("failed to spawn `git worktree add`")?;

            if !output.status.success() {
                bail!(
                    "git worktree add failed:\n{}",
                    String::from_utf8_lossy(&output.stderr)
                );
            }
        } // lock released here

        let workspace = Workspace {
            id: id.clone(),
            path,
            branch,
            task: task.to_string(),
            created_at: now_unix(),
            agent_pid: None,
        };

        std::fs::write(self.meta_path(&id), serde_json::to_vec_pretty(&workspace)?)
            .context("writing workspace metadata")?;

        Ok(workspace)
    }

    /// Records (or clears, with `None`) the PID of the agent process running
    /// in workspace `id`. Best-effort: a failure to persist this shouldn't
    /// abort whatever launched the agent, so callers typically log rather
    /// than propagate an error from this.
    pub fn set_agent_pid(&self, id: &str, pid: Option<u32>) -> Result<()> {
        let mut workspace = self.get_workspace(id)?;
        workspace.agent_pid = pid;
        std::fs::write(self.meta_path(id), serde_json::to_vec_pretty(&workspace)?)
            .context("writing workspace metadata")?;
        Ok(())
    }

    pub fn remove_workspace(&self, id: &str) -> Result<()> {
        let workspace = self.get_workspace(id)?;
        kill_if_alive(&workspace);

        {
            let _lock = PidLock::acquire(&self.lock_path(), LOCK_TIMEOUT)
                .context("acquiring git worktree lock")?;
            self.remove_worktree_retrying(&workspace.path)?;
        }

        let _ = std::fs::remove_file(self.meta_path(id));
        Ok(())
    }

    /// Removes a worktree directory, tolerating the two Windows failure
    /// modes confirmed against a real killed agent process (not
    /// theoretical): (1) killing a process doesn't mean its handles on its
    /// own `current_dir` are released the instant `kill()` returns, so an
    /// immediate `git worktree remove` can fail with "Permission denied"
    /// even though the process is already gone -- retrying briefly usually
    /// clears this; (2) git unregisters a worktree from its own metadata
    /// *before* attempting to delete the directory, so if that deletion
    /// fails, a later `git worktree remove` on the same path fails with
    /// "is not a working tree" even though the directory (and whatever's
    /// in it) is still sitting there orphaned. In that case this falls
    /// back to removing the directory directly, also with retries, since
    /// it's the same underlying handle-release race, just past the point
    /// where git itself can help.
    fn remove_worktree_retrying(&self, path: &std::path::Path) -> Result<()> {
        let mut last_err = String::new();
        for attempt in 0..10 {
            if attempt > 0 {
                std::thread::sleep(Duration::from_millis(300));
            }
            let output = Command::new("git")
                .args(["worktree", "remove"])
                .arg(path)
                .arg("--force")
                .current_dir(&self.repo_root)
                .output()
                .context("failed to spawn `git worktree remove`")?;
            if output.status.success() {
                return Ok(());
            }
            last_err = String::from_utf8_lossy(&output.stderr).to_string();
            if last_err.contains("is not a working tree") {
                break; // git already unregistered it; fall through to a plain directory removal
            }
        }

        if !path.exists() {
            return Ok(());
        }

        for attempt in 0..10 {
            if attempt > 0 {
                std::thread::sleep(Duration::from_millis(300));
            }
            match std::fs::remove_dir_all(path) {
                Ok(()) => return Ok(()),
                Err(err) => last_err = err.to_string(),
            }
        }

        bail!("failed to remove worktree directory {}: {last_err}", path.display());
    }

    pub fn list_workspaces(&self) -> Result<Vec<Workspace>> {
        let mut out = Vec::new();
        let meta_dir = self.state_dir.join("meta");
        for entry in std::fs::read_dir(&meta_dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "json") {
                let contents = std::fs::read_to_string(&path)
                    .with_context(|| format!("reading {}", path.display()))?;
                out.push(serde_json::from_str(&contents)?);
            }
        }
        out.sort_by_key(|w: &Workspace| w.created_at);
        Ok(out)
    }

    pub fn get_workspace(&self, id: &str) -> Result<Workspace> {
        let contents = std::fs::read_to_string(self.meta_path(id))
            .with_context(|| format!("no workspace found with id '{id}'"))?;
        Ok(serde_json::from_str(&contents)?)
    }
}

/// If `workspace` has a live agent process recorded, kills its whole
/// process tree before the worktree it's running in gets removed out from
/// under it, and waits (briefly) for it to actually be gone. Best-effort: a
/// dead/already-exited PID is silently ignored, not an error.
///
/// Killing only the tracked PID is not enough: confirmed directly, a
/// `claude` session running a Bash tool call spawns a child shell process,
/// and killing just the parent left that child alive, still holding a
/// handle into the workspace directory (as its own current_dir) for the
/// rest of its life -- which made every subsequent `git worktree remove`
/// and even a plain `remove_dir_all` fail with "used by another process."
/// On Windows, `taskkill /T` terminates the full descendant tree in one
/// call, which is what actually fixed it. The Unix equivalent (a process
/// group killed via `setsid` at spawn time) isn't implemented yet since
/// this hasn't been exercised outside Windows -- tracked as a known gap,
/// not silently assumed to be fine.
fn kill_if_alive(workspace: &Workspace) {
    let Some(pid) = workspace.agent_pid else {
        return;
    };
    let sys_pid = sysinfo::Pid::from_u32(pid);
    let mut sys = sysinfo::System::new();
    sys.refresh_processes(sysinfo::ProcessesToUpdate::All, true);
    if sys.process(sys_pid).is_none() {
        return;
    }

    tracing::info!("killing agent process {pid} (and its children) before removing its workspace");
    if cfg!(windows) {
        let _ = Command::new("taskkill")
            .args(["/F", "/T", "/PID", &pid.to_string()])
            .output();
    } else if let Some(process) = sys.process(sys_pid) {
        process.kill();
    }

    // Wait for the OS to actually reap it -- killing a process doesn't
    // mean its file handles (e.g. on its own current_dir) are released the
    // instant the kill call returns.
    for _ in 0..20 {
        sys.refresh_processes(sysinfo::ProcessesToUpdate::All, true);
        if sys.process(sys_pid).is_none() {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn short_id() -> String {
    Uuid::new_v4().simple().to_string()[..8].to_string()
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
