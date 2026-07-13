use std::path::PathBuf;

use agentyard_agents::{AgentEvent, RunOutcome};
use agentyard_vcs::{Workspace, WorkspaceManager};
use anyhow::Result;

/// Ties together workspace lifecycle (agentyard-vcs), dependency
/// materialization (agentyard-deps), and agent launch (agentyard-agents)
/// behind one stable interface.
pub struct Orchestrator {
    workspaces: WorkspaceManager,
}

impl Orchestrator {
    pub fn open(repo_root: impl Into<PathBuf>) -> Result<Self> {
        Ok(Self {
            workspaces: WorkspaceManager::open(repo_root)?,
        })
    }

    /// Creates a workspace, best-effort prepares its dependencies, then
    /// launches Claude Code headlessly in it and blocks until it finishes,
    /// forwarding each streamed event to `on_event` as it arrives.
    ///
    /// Only Claude Code is wired up so far (Phase 4 adds Codex/Copilot CLI
    /// as alternative adapters here); `permission_mode` is threaded through
    /// as a plain string rather than hardcoded so the CLI can surface and
    /// let the user override the safety tradeoff described in
    /// `agentyard_agents::claude_code::DEFAULT_PERMISSION_MODE`.
    pub fn spawn(
        &self,
        task: &str,
        permission_mode: &str,
        mut on_event: impl FnMut(&AgentEvent),
    ) -> Result<(Workspace, RunOutcome)> {
        let workspace = self.workspaces.create_workspace(task)?;

        if let Err(err) = agentyard_deps::prepare(&workspace.path) {
            // A dependency-prepare failure shouldn't destroy an otherwise
            // valid workspace -- the agent can still install for itself,
            // just without the head start.
            tracing::warn!(
                "dependency prepare failed for workspace {}: {err:#}",
                workspace.id
            );
        }

        let (program, args) = agentyard_agents::claude_code::build_command(task, permission_mode);
        let log_path = self
            .workspaces
            .state_dir()
            .join("logs")
            .join(format!("{}.jsonl", workspace.id));

        let workspaces = &self.workspaces;
        let id = workspace.id.clone();
        let outcome = agentyard_agents::run_and_stream(
            &program,
            &args,
            &workspace.path,
            &log_path,
            |event| on_event(event),
            |pid| {
                if let Err(err) = workspaces.set_agent_pid(&id, Some(pid)) {
                    tracing::warn!("failed to record agent pid for workspace {id}: {err:#}");
                }
            },
        )?;

        if let Err(err) = self.workspaces.set_agent_pid(&workspace.id, None) {
            tracing::warn!(
                "failed to clear agent pid for workspace {}: {err:#}",
                workspace.id
            );
        }

        Ok((workspace, outcome))
    }

    pub fn list(&self) -> Result<Vec<Workspace>> {
        self.workspaces.list_workspaces()
    }

    pub fn teardown(&self, id: &str) -> Result<()> {
        // WorkspaceManager::remove_workspace already kills any live agent
        // process recorded against this workspace before removing it.
        self.workspaces.remove_workspace(id)
    }
}
