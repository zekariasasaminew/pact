use std::path::PathBuf;

use agentyard_agents::{AgentEvent, RunOutcome};
use agentyard_vcs::{Workspace, WorkspaceManager};
use anyhow::{Context, Result};

/// Ties together workspace lifecycle (agentyard-vcs), dependency
/// materialization (agentyard-deps), and agent launch (agentyard-agents)
/// behind one stable interface.
pub struct Orchestrator {
    workspaces: WorkspaceManager,
    repo_root: PathBuf,
}

impl Orchestrator {
    pub fn open(repo_root: impl Into<PathBuf>) -> Result<Self> {
        let repo_root = repo_root.into();
        Ok(Self {
            workspaces: WorkspaceManager::open(&repo_root)?,
            repo_root,
        })
    }

    /// Writes the MCP config file that points Claude Code at
    /// `agentyard mcp-serve` for this workspace, launched as the agent
    /// CLI's own child process over stdio (not run in-process here).
    /// Confirmed against the real CLI: `{"mcpServers": {...}}` is the
    /// correct shape for a `--mcp-config` *file* -- an unwrapped file is
    /// rejected with a loud error before the session starts, so getting
    /// this wrong is never a silent no-op.
    fn write_mcp_config(&self, workspace: &Workspace) -> Result<PathBuf> {
        let self_exe = std::env::current_exe().context("resolving agentyard's own executable path")?;
        let config = serde_json::json!({
            "mcpServers": {
                agentyard_agents::claude_code::COORD_SERVER_NAME: {
                    "command": self_exe.to_string_lossy(),
                    "args": [
                        "--repo", self.repo_root.to_string_lossy(),
                        "mcp-serve",
                        "--agent-id", workspace.id,
                        "--workspace", workspace.path.to_string_lossy(),
                    ],
                }
            }
        });

        let dir = self.workspaces.state_dir().join("mcp");
        std::fs::create_dir_all(&dir)?;
        let path = dir.join(format!("{}.json", workspace.id));
        std::fs::write(&path, serde_json::to_vec_pretty(&config)?)
            .context("writing MCP config file")?;
        Ok(path)
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

        let mcp_config = match self.write_mcp_config(&workspace) {
            Ok(path) => Some(path),
            Err(err) => {
                tracing::warn!(
                    "failed to write MCP config for workspace {}: {err:#} \
                     (agent will run without file-lease/messaging coordination)",
                    workspace.id
                );
                None
            }
        };
        let (program, args) = agentyard_agents::claude_code::build_command(
            task,
            permission_mode,
            mcp_config.as_deref(),
        );
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
            |event| {
                if let AgentEvent::Init { mcp_servers, .. } = event {
                    check_coord_connected(mcp_servers, &id);
                }
                on_event(event);
            },
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

/// Warns loudly if the coordination server isn't reported as connected in
/// the session's init event -- without this check, a coordination server
/// that fails to start (bad path, panic, whatever) would be a completely
/// silent no-op: the session runs normally, file leases and messages just
/// quietly never work, with nothing anywhere saying so.
fn check_coord_connected(mcp_servers: &[(String, String)], workspace_id: &str) {
    let name = agentyard_agents::claude_code::COORD_SERVER_NAME;
    match mcp_servers.iter().find(|(server_name, _)| server_name == name) {
        Some((_, status)) if status == "connected" => {}
        Some((_, status)) => tracing::warn!(
            "workspace {workspace_id}: coordination server '{name}' reported status \
             '{status}', not 'connected' -- file leases and messaging will not work \
             for this session"
        ),
        None => tracing::warn!(
            "workspace {workspace_id}: coordination server '{name}' did not appear in \
             the session's MCP server list at all -- file leases and messaging will not \
             work for this session"
        ),
    }
}
