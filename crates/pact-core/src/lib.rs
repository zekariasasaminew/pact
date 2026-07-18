use std::path::PathBuf;

use pact_agents::{AgentEvent, AgentKind, CoordConfig, RunOutcome, Supervisor};
use pact_vcs::{Workspace, WorkspaceDiff, WorkspaceManager};
use anyhow::{Context, Result};

/// Ties together workspace lifecycle (pact-vcs), dependency
/// materialization (pact-deps), and agent launch (pact-agents)
/// behind one stable interface.
pub struct Orchestrator {
    workspaces: WorkspaceManager,
    repo_root: PathBuf,
}

/// One (agent, task) pair to run as part of a `spawn_many` batch. A
/// separate, explicit `safety_override` per task (rather than one shared
/// across the whole batch) is deliberately not supported in this first cut
/// -- issue #3's acceptance criteria don't call for it, and `--safety`'s
/// existing single-spawn meaning (an adapter-vocabulary override) already
/// applies uniformly per invocation; extending it per-task is a plausible
/// follow-up, not something to speculatively build now.
pub struct SpawnManyTask {
    pub agent: AgentKind,
    pub task: String,
}

/// Points the generated MCP coordination config at an alternative command
/// instead of `pact mcp-serve` -- see issue #10. Pact does no protocol
/// translation: whatever this points at must speak the same tool contract
/// pact-coord defines (`claim_files`/`release_files`/`send_message`/
/// `check_messages`) on its own. Absent, every workspace gets today's
/// default (pact's own binary, unchanged).
pub struct CoordServerOverride {
    pub command: String,
    pub args: Vec<String>,
}

/// The outcome of one task within a `spawn_many` batch. `result` is `Err`
/// if workspace creation, dependency prep wiring, or the agent process
/// itself failed outright (including a panic inside that task's thread,
/// converted here rather than left to poison the whole batch) -- as
/// opposed to the agent *running* but reporting failure, which is a
/// successful `Ok` carrying `RunOutcome { success: false, .. }`.
pub struct SpawnManyOutcome {
    pub index: usize,
    pub agent: AgentKind,
    pub result: Result<(Workspace, RunOutcome)>,
}

/// One file touched by more than one active workspace sharing a common
/// merge-base -- see `Orchestrator::detect_conflicts` (issue #8).
pub struct FileConflict {
    pub file: String,
    /// At least 2 workspace ids -- every workspace (sharing the same
    /// merge-base as the others in this conflict) that touched `file`.
    pub workspace_ids: Vec<String>,
    /// `(pattern, holder)` pairs from the coordination DB whose glob
    /// matched `file` -- active or expired.
    pub related_leases: Vec<(String, String)>,
    /// Coarse pointer, not a full transcript: how many coordination
    /// messages exist from any of `workspace_ids`.
    pub related_message_count: usize,
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
    /// server for the agent CLI to launch. What each adapter *does* with
    /// this (a JSON file passed via a flag, or inline config overrides) is
    /// up to it -- see `AgentAdapter::build_command`. Defaults to `pact
    /// mcp-serve`; `coord_override`, if given, points at an alternative
    /// command/args instead (see `CoordServerOverride`, issue #10) --
    /// pact does no protocol translation, it just tells the agent CLI to
    /// launch something else instead of itself.
    fn coord_config(
        &self,
        workspace: &Workspace,
        server_name: &str,
        coord_override: Option<&CoordServerOverride>,
    ) -> Result<CoordConfig> {
        let config_path = self
            .workspaces
            .state_dir()
            .join("mcp")
            .join(format!("{}.json", workspace.id));

        if let Some(over) = coord_override {
            return Ok(CoordConfig {
                server_name: server_name.to_string(),
                command: over.command.clone(),
                args: over.args.clone(),
                config_path,
            });
        }

        let self_exe =
            std::env::current_exe().context("resolving pact's own executable path")?;
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
    ///
    /// Creates its own single-use `Supervisor` -- this call's Ctrl-C
    /// handling is exactly what it was before `spawn_many` existed, just
    /// routed through the same shared mechanism spawn_many uses for N
    /// concurrent children instead of a bare function.
    pub fn spawn(
        &self,
        agent: AgentKind,
        task: &str,
        safety_override: Option<&str>,
        coord_override: Option<&CoordServerOverride>,
        on_event: impl FnMut(&AgentEvent),
    ) -> Result<(Workspace, RunOutcome)> {
        let supervisor = Supervisor::new();
        self.spawn_with_supervisor(
            &supervisor,
            agent,
            task,
            safety_override,
            coord_override,
            on_event,
        )
    }

    /// Runs every `(agent, task)` pair in `tasks` concurrently, one
    /// `std::thread` each, sharing one `Supervisor` so a single Ctrl-C
    /// kills every still-running child at once. `on_event` receives each
    /// task's batch index alongside its event so the caller can attribute
    /// interleaved output back to its source; it's called from whichever
    /// task's thread produced the event, so it must be `Sync`.
    ///
    /// `workspaces: &WorkspaceManager` (via `self`) has no interior
    /// mutability beyond what `create_workspace` already serializes with
    /// `PidLock` -- the same concurrency Phase 0 verified against 6
    /// simultaneous `spawn` calls -- so sharing `&self` across scoped
    /// threads here doesn't need any new synchronization of its own.
    pub fn spawn_many(
        &self,
        tasks: Vec<SpawnManyTask>,
        safety_override: Option<&str>,
        coord_override: Option<&CoordServerOverride>,
        on_event: impl Fn(usize, &AgentKind, &AgentEvent) + Sync,
    ) -> Vec<SpawnManyOutcome> {
        let supervisor = Supervisor::new();
        std::thread::scope(|scope| {
            // Index and agent are captured here, outside the closure's
            // return value, specifically so a panic (which loses whatever
            // the closure would have returned) still leaves enough to
            // attribute the failure to the right task below.
            let handles: Vec<(usize, AgentKind, _)> = tasks
                .iter()
                .enumerate()
                .map(|(index, spec)| {
                    let supervisor = &supervisor;
                    let on_event = &on_event;
                    let handle = scope.spawn(move || {
                        self.spawn_with_supervisor(
                            supervisor,
                            spec.agent,
                            &spec.task,
                            safety_override,
                            coord_override,
                            |event| on_event(index, &spec.agent, event),
                        )
                    });
                    (index, spec.agent, handle)
                })
                .collect();

            handles
                .into_iter()
                .map(|(index, agent, handle)| {
                    let result = match handle.join() {
                        Ok(result) => result,
                        Err(panic) => {
                            // A panic in one task's thread must not lose
                            // the other tasks' results -- surface it as
                            // this task's own failure instead of
                            // propagating out of spawn_many entirely.
                            let message = panic
                                .downcast_ref::<&str>()
                                .map(|s| s.to_string())
                                .or_else(|| panic.downcast_ref::<String>().cloned())
                                .unwrap_or_else(|| "agent task thread panicked".to_string());
                            Err(anyhow::anyhow!("agent task thread panicked: {message}"))
                        }
                    };
                    SpawnManyOutcome {
                        index,
                        agent,
                        result,
                    }
                })
                .collect()
        })
    }

    fn spawn_with_supervisor(
        &self,
        supervisor: &Supervisor,
        agent: AgentKind,
        task: &str,
        safety_override: Option<&str>,
        coord_override: Option<&CoordServerOverride>,
        mut on_event: impl FnMut(&AgentEvent),
    ) -> Result<(Workspace, RunOutcome)> {
        let workspace = self.workspaces.create_workspace(task)?;
        let adapter = pact_agents::adapter(agent);

        if let Err(err) = pact_deps::prepare(&workspace.path) {
            // A dependency-prepare failure shouldn't destroy an otherwise
            // valid workspace -- the agent can still install for itself,
            // just without the head start.
            tracing::warn!(
                "dependency prepare failed for workspace {}: {err:#}",
                workspace.id
            );
        }

        let coord_name = adapter.coord_server_name();
        let coord = match self.coord_config(&workspace, coord_name, coord_override) {
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

        let (program, args) =
            adapter.build_command(task, safety_override, coord.as_ref(), &workspace.path);
        let log_path = self
            .workspaces
            .state_dir()
            .join("logs")
            .join(format!("{}.jsonl", workspace.id));

        let workspaces = &self.workspaces;
        let id = workspace.id.clone();
        let mut coord_seen = false;
        let outcome = pact_agents::run_and_stream(
            supervisor,
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

    /// Whether a workspace has uncommitted changes -- used by `list` to
    /// show a per-workspace dirty/clean indicator at a glance.
    pub fn is_dirty(&self, id: &str) -> Result<bool> {
        self.workspaces.is_dirty(id)
    }

    /// A workspace's committed (on-branch) and uncommitted (working-tree)
    /// changes -- see `pact_vcs::WorkspaceManager::workspace_diff`.
    pub fn diff(&self, id: &str) -> Result<WorkspaceDiff> {
        self.workspaces.workspace_diff(id)
    }

    /// Commits everything in a workspace's working tree -- see
    /// `pact_vcs::WorkspaceManager::commit_all`. Returns `false` if the
    /// workspace was already clean.
    pub fn commit_all(&self, id: &str) -> Result<bool> {
        self.workspaces.commit_all(id)
    }

    /// Reports files touched by more than one active workspace, among
    /// workspaces that share a common merge-base (i.e. forked from the
    /// same point in history) -- see issue #8. Informational only, same
    /// as MCP leases being advisory: nothing here blocks anything, it just
    /// surfaces overlap that would otherwise only become visible when a
    /// user tries to reconcile worktrees by hand. Each conflict is
    /// enriched with any coordination-DB lease that matched the file
    /// (active or expired -- lapsed-but-relevant context still counts) and
    /// a coarse related-message count, since a workspace's id is the same
    /// string as its MCP `agent_id`, making that join direct.
    pub fn detect_conflicts(&self) -> Result<Vec<FileConflict>> {
        let workspaces = self.workspaces.list_workspaces()?;

        let mut by_base: std::collections::HashMap<String, Vec<(String, Vec<String>)>> =
            std::collections::HashMap::new();
        for workspace in &workspaces {
            match self.workspaces.workspace_changes(&workspace.id) {
                Ok(changes) if !changes.merge_base.is_empty() => {
                    by_base
                        .entry(changes.merge_base)
                        .or_default()
                        .push((workspace.id.clone(), changes.files));
                }
                Ok(_) => {} // no merge-base found -- not comparable to anything
                Err(err) => tracing::warn!(
                    "could not compute changes for workspace {}: {err:#}",
                    workspace.id
                ),
            }
        }

        let mut conflicts = Vec::new();
        for group in by_base.into_values() {
            if group.len() < 2 {
                continue;
            }
            let mut file_to_workspaces: std::collections::HashMap<String, Vec<String>> =
                std::collections::HashMap::new();
            for (id, files) in &group {
                for file in files {
                    file_to_workspaces
                        .entry(file.clone())
                        .or_default()
                        .push(id.clone());
                }
            }
            for (file, workspace_ids) in file_to_workspaces {
                if workspace_ids.len() < 2 {
                    continue;
                }
                let related_leases =
                    pact_coord::leases_matching(&self.repo_root, &file).unwrap_or_default();
                let related_message_count =
                    pact_coord::message_count_involving(&self.repo_root, &workspace_ids)
                        .unwrap_or(0);
                conflicts.push(FileConflict {
                    file,
                    workspace_ids,
                    related_leases,
                    related_message_count,
                });
            }
        }

        conflicts.sort_by(|a, b| a.file.cmp(&b.file));
        Ok(conflicts)
    }

    pub fn teardown(&self, id: &str, keep_branch: bool, force: bool) -> Result<()> {
        // WorkspaceManager::remove_workspace already kills any live agent
        // process recorded against this workspace before removing it, and
        // refuses on uncommitted changes unless `force` is set.
        self.workspaces.remove_workspace(id, keep_branch, force)
    }
}
