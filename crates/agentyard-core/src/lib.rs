use std::path::PathBuf;

use agentyard_agents::{AgentEvent, AgentKind, CoordConfig, RunOutcome};
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

    /// Builds the (adapter-agnostic) description of the coordination
    /// server for `agentyard mcp-serve` to be launched with. What each
    /// adapter *does* with this (a JSON file passed via a flag, or inline
    /// config overrides) is up to it -- see `AgentAdapter::build_command`.
    fn coord_config(&self, workspace: &Workspace, server_name: &str) -> Result<CoordConfig> {
        let self_exe =
            std::env::current_exe().context("resolving agentyard's own executable path")?;
        let config_path = self
            .workspaces
            .state_dir()
            .join("mcp")
            .join(format!("{}.json", workspace.id));
        Ok(CoordConfig {
            server_name: server_name.to_string(),
            command: self_exe.to_string_lossy().to_string(),
            args: vec![
                "--repo".to_string(),
                self.repo_root.to_string_lossy().to_string(),
                "mcp-serve".to_string(),
                "--agent-id".to_string(),
                workspace.id.clone(),
                "--workspace".to_string(),
                workspace.path.to_string_lossy().to_string(),
            ],
            config_path,
        })
    }

    /// Creates a workspace, best-effort prepares its dependencies, then
    /// launches the chosen agent CLI headlessly in it and blocks until it
    /// finishes, forwarding each streamed event to `on_event` as it
    /// arrives. `safety_override`, if given, is passed through raw to
    /// that adapter's own safety/approval vocabulary (see
    /// `AgentAdapter::build_command`); if `None`, the adapter's own
    /// unattended-safety default is used and should be warned about by the
    /// caller (see `AgentAdapter::default_safety_description`).
    pub fn spawn(
        &self,
        agent: AgentKind,
        task: &str,
        safety_override: Option<&str>,
        mut on_event: impl FnMut(&AgentEvent),
    ) -> Result<(Workspace, RunOutcome)> {
        let workspace = self.workspaces.create_workspace(task)?;
        let adapter = agentyard_agents::adapter(agent);

        if let Err(err) = agentyard_deps::prepare(&workspace.path) {
            // A dependency-prepare failure shouldn't destroy an otherwise
            // valid workspace -- the agent can still install for itself,
            // just without the head start.
            tracing::warn!(
                "dependency prepare failed for workspace {}: {err:#}",
                workspace.id
            );
        }

        let coord_name = adapter.coord_server_name();
        let coord = match self.coord_config(&workspace, coord_name) {
            Ok(c) => Some(c),
            Err(err) => {
                tracing::warn!(
                    "failed to prepare coordination config for workspace {}: {err:#} \
                     (agent will run without file-lease/messaging coordination)",
                    workspace.id
                );
                None
            }
        };

        let (program, args) = adapter.build_command(task, safety_override, coord.as_ref());
        let log_path = self
            .workspaces
            .state_dir()
            .join("logs")
            .join(format!("{}.jsonl", workspace.id));

        let workspaces = &self.workspaces;
        let id = workspace.id.clone();
        let mut coord_seen = false;
        let outcome = agentyard_agents::run_and_stream(
            &program,
            &args,
            &workspace.path,
            &log_path,
            |line| adapter.parse_line(line),
            |event| {
                if let AgentEvent::CoordStatus { name, status } = event {
                    if name == coord_name {
                        coord_seen = true;
                        if status != "connected" {
                            tracing::warn!(
                                "workspace {id}: coordination server '{coord_name}' reported \
                                 status '{status}', not 'connected' -- file leases and \
                                 messaging will not work for this session"
                            );
                        }
                    }
                }
                on_event(event);
            },
            |pid| {
                if let Err(err) = workspaces.set_agent_pid(&id, Some(pid)) {
                    tracing::warn!("failed to record agent pid for workspace {id}: {err:#}");
                }
            },
        )?;

        if coord.is_some() && !coord_seen {
            tracing::warn!(
                "workspace {}: coordination server '{coord_name}' never reported a status at \
                 all -- file leases and messaging will not work for this session (this is \
                 expected for adapters without a confirmed event schema, e.g. Codex; see README)",
                workspace.id
            );
        }

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
