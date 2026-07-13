use std::path::PathBuf;

use agentyard_vcs::{Workspace, WorkspaceManager};
use anyhow::Result;

/// Ties together workspace lifecycle (agentyard-vcs), dependency
/// materialization (agentyard-deps, Phase 1), and agent launch
/// (agentyard-agents, Phase 2+) behind one stable interface. Phase 0 only
/// wires up workspace lifecycle -- `spawn` is written so later phases can
/// insert a "prepare dependencies" and "launch agent" step in between
/// creating the workspace and returning it, without changing this
/// function's signature or the CLI that calls it.
pub struct Orchestrator {
    workspaces: WorkspaceManager,
}

impl Orchestrator {
    pub fn open(repo_root: impl Into<PathBuf>) -> Result<Self> {
        Ok(Self {
            workspaces: WorkspaceManager::open(repo_root)?,
        })
    }

    pub fn spawn(&self, task: &str) -> Result<Workspace> {
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

        // Phase 2 will insert: agentyard_agents::launch(&workspace)?;

        Ok(workspace)
    }

    pub fn list(&self) -> Result<Vec<Workspace>> {
        self.workspaces.list_workspaces()
    }

    pub fn teardown(&self, id: &str) -> Result<()> {
        // Phase 2+ will insert: kill the agent process and release leases
        // for this workspace before removing it.
        self.workspaces.remove_workspace(id)
    }
}
