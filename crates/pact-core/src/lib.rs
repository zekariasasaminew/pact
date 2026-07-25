use std::path::{Path, PathBuf};
use std::process::Command;

use pact_agents::{AgentEvent, AgentKind, CoordConfig, RunOutcome, Supervisor};
use pact_vcs::{Workspace, WorkspaceDiff, WorkspaceManager};
use anyhow::{bail, Context, Result};

pub use pact_vcs::{
    agent_process_alive, ArbiterResolver, ConflictedWorkspace, MergedWorkspace, MergeReport, ResolveOutcome,
    SkippedWorkspace,
};

/// A `resolve_conflict` attempt's result -- see DESIGN.md ("pact-coord >
/// Persisted conflicts / `pact resolve` (issue #85)").
pub struct ConflictResolution {
    pub conflict_id: i64,
    pub outcome: ResolveOutcome,
}

/// Configuration for the Arbiter fallback resolver -- entirely opt-in,
/// see DESIGN.md ("pact-core > Arbiter -- agent invocation").
pub struct ArbiterConfig {
    pub agent: AgentKind,
    pub safety_override: Option<String>,
    pub test_cmd: String,
}

/// A durable, structured record of one real agent run -- see DESIGN.md
/// ("pact-core > structured run metadata", issue #15). Persisted to
/// `state_dir/meta/<id>-run.json`, sibling to the workspace's own
/// `meta/<id>.json` and the dependency-prep report (issue #12). Before
/// this, none of these fields survived past the terminal output and the
/// raw JSONL agent log -- there was no queryable "what actually
/// happened" record for a real spawn.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RunMetadata {
    pub workspace_id: String,
    pub agent: String,
    pub program: String,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    pub started_at: u64,
    pub ended_at: u64,
    pub exit_success: bool,
    pub summary: String,
    /// The coordination server's last reported status before the process
    /// exited (e.g. "connected", "pending", "failed"), or `None` if no
    /// coordination config was attached to this run at all.
    pub coord_status: Option<String>,
    pub log_path: PathBuf,
}

/// Ties together workspace lifecycle (pact-vcs), dependency
/// materialization (pact-deps), and agent launch (pact-agents)
/// behind one stable interface.
pub struct Orchestrator {
    workspaces: WorkspaceManager,
    repo_root: PathBuf,
}

/// One (agent, task) pair to run as part of a `spawn_many` batch -- see
/// DESIGN.md ("pact-core > spawn / spawn_many concurrency") for why a
/// per-task safety override isn't supported yet.
pub struct SpawnManyTask {
    pub agent: AgentKind,
    pub task: String,
}

/// What `spawn`/`spawn-many` would do for one task, without doing any of
/// it -- see `Orchestrator::spawn_preview` (issue #16's `--dry-run`).
pub struct SpawnPreview {
    pub workspace_id: String,
    pub branch: String,
    pub path: PathBuf,
    pub package_managers: Vec<pact_deps::PackageManager>,
    pub program: String,
    pub args: Vec<String>,
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

/// One file-like token mentioned in more than one task's text within the
/// same `spawn_many` batch -- "Weaver", the prevention half of the
/// conflict-avoidance story. See DESIGN.md ("pact-core > Weaver -- task
/// overlap prediction").
pub struct PredictedOverlap {
    pub token: String,
    /// Indices into the `spawn_many` task list (0-based) whose text
    /// mentioned `token`. Always at least 2 entries.
    pub task_indices: Vec<usize>,
}

/// Scans every task's text for file-path-like tokens and reports any token
/// mentioned by two or more tasks -- see DESIGN.md ("pact-core > Weaver --
/// task overlap prediction").
pub fn predict_task_overlap(tasks: &[SpawnManyTask]) -> Vec<PredictedOverlap> {
    let mut token_to_tasks: std::collections::HashMap<String, Vec<usize>> =
        std::collections::HashMap::new();
    for (index, task) in tasks.iter().enumerate() {
        for token in extract_file_tokens(&task.task) {
            token_to_tasks.entry(token).or_default().push(index);
        }
    }

    let mut overlaps: Vec<PredictedOverlap> = token_to_tasks
        .into_iter()
        .filter(|(_, indices)| indices.len() >= 2)
        .map(|(token, task_indices)| PredictedOverlap { token, task_indices })
        .collect();
    overlaps.sort_by(|a, b| a.token.cmp(&b.token));
    overlaps
}

/// Splits `task` on whitespace and common surrounding punctuation, keeping
/// whichever chunks look like a file path (see `looks_like_file_path`).
fn extract_file_tokens(task: &str) -> std::collections::HashSet<String> {
    let mut tokens = std::collections::HashSet::new();
    for word in task.split(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | '(' | ')' | ',' | ';' | ':' | '`')) {
        let trimmed = word.trim_matches(|c: char| matches!(c, '.' | '!' | '?'));
        if looks_like_file_path(trimmed) {
            tokens.insert(trimmed.to_string());
        }
    }
    tokens
}

/// A conservative, regex-free "does this look like a file path" check --
/// see DESIGN.md ("pact-core > Weaver -- task overlap prediction").
fn looks_like_file_path(s: &str) -> bool {
    let Some(dot) = s.rfind('.') else { return false };
    let ext = &s[dot + 1..];
    if ext.is_empty() || ext.len() > 5 || !ext.chars().all(|c| c.is_ascii_alphanumeric()) {
        return false;
    }
    let stem = &s[..dot];
    !stem.is_empty() && stem.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '_' | '-' | '.'))
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
    /// server for the agent CLI to launch. Defaults to `pact mcp-serve`;
    /// `coord_override`, if given, points at an alternative command/args
    /// instead -- see `CoordServerOverride`.
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
    /// finishes, forwarding each streamed event to `on_event`.
    /// `safety_override`, if given, is passed through raw to that
    /// adapter's own safety/approval vocabulary; if `None`, the adapter's
    /// own unattended-safety default is used and should be warned about by
    /// the caller.
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
    /// task's thread produced the event, so it must be `Sync`. See
    /// DESIGN.md ("pact-core > spawn / spawn_many concurrency") for the
    /// synchronization argument.
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

    /// Previews what `spawn`/`spawn-many` would do for one task -- the
    /// workspace id/branch/path it would create, the package manager(s)
    /// detected for the repo, and the exact `program args...` that would be
    /// launched (matching `AgentAdapter::build_command`'s real output) --
    /// without creating a workspace, running dependency prep, or launching
    /// anything. See DESIGN.md ("pact-core > --dry-run preview") for why
    /// `build_command`'s MCP-config-file side effect has to be cleaned up
    /// here rather than left as a stray file.
    pub fn spawn_preview(
        &self,
        agent: AgentKind,
        task: &str,
        safety_override: Option<&str>,
        coord_override: Option<&CoordServerOverride>,
    ) -> Result<SpawnPreview> {
        let (workspace_id, branch, path) = self.workspaces.preview_workspace_location(task);
        let package_managers = pact_deps::detect(&self.repo_root);
        let adapter = pact_agents::adapter(agent);

        let preview_workspace = Workspace {
            id: workspace_id.clone(),
            path: path.clone(),
            branch: branch.clone(),
            task: task.to_string(),
            created_at: 0,
            agent_pid: None,
            base_commit: String::new(),
        };
        let coord_name = adapter.coord_server_name();
        let coord = self
            .coord_config(&preview_workspace, coord_name, coord_override)
            .ok();

        let (program, args) = adapter.build_command(task, safety_override, coord.as_ref(), &path);
        if let Some(coord) = &coord {
            let _ = std::fs::remove_file(&coord.config_path);
        }

        Ok(SpawnPreview {
            workspace_id,
            branch,
            path,
            package_managers,
            program,
            args,
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

        // A dependency-prepare failure shouldn't destroy an otherwise
        // valid workspace -- the agent can still install for itself, just
        // without the head start. Persisted alongside the workspace's own
        // metadata (issue #12) so "what actually happened during prep" is
        // queryable later, not just a log line at spawn time.
        let dep_reports = pact_deps::prepare(&workspace.path);
        for report in &dep_reports {
            if !report.success {
                tracing::warn!(
                    "dependency prepare step for {} failed in workspace {}: {:?}",
                    report.manager,
                    workspace.id,
                    report.warnings
                );
            }
        }
        let deps_path = self.workspaces.state_dir().join("meta").join(format!("{}-deps.json", workspace.id));
        if let Err(err) = std::fs::write(&deps_path, serde_json::to_vec_pretty(&dep_reports).unwrap_or_default()) {
            tracing::warn!("failed to persist dependency prep report to {}: {err:#}", deps_path.display());
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
        // Tracks the *last* status reported for this coord server, not the
        // first -- a real coord connection reliably goes through a
        // transient 'pending' status before 'connected' within a fraction
        // of a second (confirmed: every single spawn in manual testing hit
        // this), and warning on that transient value trained users to
        // ignore pact WARNs in general, which made the genuinely bad case
        // (coord stuck on 'pending', or 'failed', for the whole run) read
        // almost identically to normal. Only what the server had settled
        // on by the time the process actually exited matters here.
        let mut coord_last_status: Option<String> = None;
        let started_at = unix_now();
        let run_result = pact_agents::run_and_stream(
            supervisor,
            &program,
            &args,
            &workspace.path,
            &log_path,
            |line| adapter.parse_line(line),
            |event| {
                if let AgentEvent::CoordStatus { name, status } = event {
                    if name == coord_name {
                        coord_last_status = Some(status.clone());
                    }
                }
                on_event(event);
            },
            |pid| {
                if let Err(err) = workspaces.set_agent_pid(&id, Some(pid)) {
                    tracing::warn!("failed to record agent pid for workspace {id}: {err:#}");
                }
            },
        );
        let ended_at = unix_now();

        if let Some(message) = coord_warning(coord.is_some(), coord_last_status.as_deref(), coord_name) {
            tracing::warn!("workspace {}: {message}", workspace.id);
        }

        if let Err(err) = self.workspaces.set_agent_pid(&workspace.id, None) {
            tracing::warn!(
                "failed to clear agent pid for workspace {}: {err:#}",
                workspace.id
            );
        }

        // Recorded regardless of success/failure -- a run that failed to
        // even start is exactly the kind of thing worth a durable record,
        // not just an error propagated up and otherwise lost.
        let run_metadata = RunMetadata {
            workspace_id: workspace.id.clone(),
            agent: agent_kind_name(agent).to_string(),
            program: program.clone(),
            args: args.clone(),
            cwd: workspace.path.clone(),
            started_at,
            ended_at,
            exit_success: run_result.as_ref().map(|r| r.success).unwrap_or(false),
            summary: match &run_result {
                Ok(run) => run.summary.clone(),
                Err(err) => format!("failed to run: {err:#}"),
            },
            coord_status: coord_last_status,
            log_path: log_path.clone(),
        };
        let run_meta_path = self.workspaces.state_dir().join("meta").join(format!("{}-run.json", workspace.id));
        if let Err(err) = std::fs::write(&run_meta_path, serde_json::to_vec_pretty(&run_metadata).unwrap_or_default()) {
            tracing::warn!("failed to persist run metadata to {}: {err:#}", run_meta_path.display());
        }

        let outcome = run_result?;
        Ok((workspace, outcome))
    }

    pub fn list(&self) -> Result<Vec<Workspace>> {
        self.workspaces.list_workspaces()
    }

    /// A single workspace by id -- see `pact_vcs::WorkspaceManager::get_workspace`.
    pub fn get_workspace(&self, id: &str) -> Result<Workspace> {
        self.workspaces.get_workspace(id)
    }

    /// Whether a workspace has uncommitted changes -- used by `list` to
    /// show a per-workspace dirty/clean indicator at a glance.
    pub fn is_dirty(&self, id: &str) -> Result<bool> {
        self.workspaces.is_dirty(id)
    }

    /// The dependency-prep report recorded for this workspace at spawn
    /// time (issue #12), if any survives -- `None` if the workspace never
    /// went through a real spawn (e.g. this environment's tests) or the
    /// file is missing/unreadable, not an error either way; this is
    /// purely informational, feeding `pact inspect` (issue #16).
    pub fn dependency_prep_report(&self, id: &str) -> Option<Vec<pact_deps::ManagerPrepReport>> {
        let path = self.workspaces.state_dir().join("meta").join(format!("{id}-deps.json"));
        let contents = std::fs::read_to_string(path).ok()?;
        serde_json::from_str(&contents).ok()
    }

    /// The structured run-metadata record for this workspace's spawn
    /// (issue #15), if any survives -- same "informational, not an
    /// error" contract as `dependency_prep_report`.
    pub fn run_metadata(&self, id: &str) -> Option<RunMetadata> {
        let path = self.workspaces.state_dir().join("meta").join(format!("{id}-run.json"));
        let contents = std::fs::read_to_string(path).ok()?;
        serde_json::from_str(&contents).ok()
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

    /// Closes the loop from "N dirty workspaces" to "one clean integration
    /// branch" -- see `pact_vcs::WorkspaceManager::merge_all`. `arbiter`,
    /// if given, is wired in as pact-vcs's `ArbiterResolver` hook -- see
    /// DESIGN.md ("pact-core > Arbiter -- agent invocation").
    pub fn merge_all(
        &self,
        ids: Option<&[String]>,
        target_branch: Option<&str>,
        union_globs: &[String],
        arbiter: Option<&ArbiterConfig>,
        require_passing_tests: Option<&str>,
        dry_run: bool,
    ) -> Result<MergeReport> {
        let resolver = |worktree_path: &Path, task_text: &str, files: &[String]| -> Vec<String> {
            self.run_arbiter(arbiter.expect("resolver only invoked when arbiter is Some"), worktree_path, task_text, files)
        };
        let resolver_ref: Option<&ArbiterResolver<'_>> = if arbiter.is_some() { Some(&resolver) } else { None };
        let report =
            self.workspaces.merge_all(ids, target_branch, union_globs, resolver_ref, require_passing_tests, dry_run)?;
        self.log_operation(
            "merge_all",
            None,
            serde_json::json!({
                "target_branch": report.target_branch,
                "dry_run": report.dry_run,
                "merged": report.merged.iter().map(|w| &w.id).collect::<Vec<_>>(),
                "skipped": report.skipped.iter().map(|w| serde_json::json!({"id": w.id, "reason": w.reason})).collect::<Vec<_>>(),
                "planned": report.planned,
            }),
        );
        for conflict in &report.conflicted {
            if let Err(err) =
                pact_coord::record_conflict(&self.repo_root, &conflict.id, &conflict.target_branch, &conflict.files)
            {
                tracing::warn!("failed to persist conflict for workspace {}: {err:#}", conflict.id);
            }
        }
        Ok(report)
    }

    /// Every currently-open persisted conflict -- what `pact resolve` (no
    /// workspace id) lists. See DESIGN.md ("pact-coord > Persisted
    /// conflicts / `pact resolve` (issue #85)").
    pub fn open_conflicts(&self) -> Result<Vec<pact_coord::PersistedConflict>> {
        pact_coord::open_conflicts(&self.repo_root)
    }

    /// The most recent open conflict for one workspace, if any -- same
    /// data `pact resolve <id>` itself would act on, surfaced read-only
    /// for `pact inspect` (issue #16).
    pub fn open_conflict_for(&self, workspace_id: &str) -> Result<Option<pact_coord::PersistedConflict>> {
        pact_coord::open_conflict_for_workspace(&self.repo_root, workspace_id)
    }

    /// Retries the most recent open conflict recorded against
    /// `workspace_id`. On success, marks the persisted conflict resolved;
    /// on a repeat conflict, leaves it open. Either way, records a
    /// `conflict_resolve` operation so the attempt itself shows up in
    /// `pact history` even when it didn't resolve anything.
    pub fn resolve_conflict(
        &self,
        workspace_id: &str,
        union_globs: &[String],
        arbiter: Option<&ArbiterConfig>,
    ) -> Result<ConflictResolution> {
        let Some(conflict) = pact_coord::open_conflict_for_workspace(&self.repo_root, workspace_id)? else {
            bail!("no open conflict recorded for workspace {workspace_id}");
        };

        let resolver = |worktree_path: &Path, task_text: &str, files: &[String]| -> Vec<String> {
            self.run_arbiter(arbiter.expect("resolver only invoked when arbiter is Some"), worktree_path, task_text, files)
        };
        let resolver_ref: Option<&ArbiterResolver<'_>> = if arbiter.is_some() { Some(&resolver) } else { None };
        let outcome = self.workspaces.resolve_conflict(&conflict.target_branch, workspace_id, union_globs, resolver_ref)?;

        let resolved = matches!(outcome, ResolveOutcome::Resolved { .. });
        if resolved {
            if let Err(err) = pact_coord::mark_conflict_resolved(&self.repo_root, conflict.id) {
                tracing::warn!("failed to mark conflict {} resolved: {err:#}", conflict.id);
            }
        }
        self.log_operation(
            "conflict_resolve",
            Some(workspace_id),
            serde_json::json!({ "target_branch": conflict.target_branch, "resolved": resolved }),
        );

        Ok(ConflictResolution { conflict_id: conflict.id, outcome })
    }

    /// Marks the most recent open conflict for `workspace_id` abandoned
    /// without retrying it -- the manual escape hatch for a conflict
    /// that's not worth resolving. Returns `false` if there was no open
    /// conflict to abandon.
    pub fn abandon_conflict(&self, workspace_id: &str) -> Result<bool> {
        let Some(conflict) = pact_coord::open_conflict_for_workspace(&self.repo_root, workspace_id)? else {
            return Ok(false);
        };
        pact_coord::mark_conflict_abandoned(&self.repo_root, conflict.id)?;
        self.log_operation(
            "conflict_resolve",
            Some(workspace_id),
            serde_json::json!({ "target_branch": conflict.target_branch, "abandoned": true }),
        );
        Ok(true)
    }

    /// Best-effort operation-log write -- see DESIGN.md ("pact-coord >
    /// Operation log / `pact history` (issue #84)"). Never fails the
    /// caller: recording history must not become a new way for
    /// `merge_all`/`teardown`/Arbiter to fail.
    fn log_operation(&self, op_type: &str, workspace_id: Option<&str>, detail: serde_json::Value) {
        if let Err(err) = pact_coord::log_operation(&self.repo_root, op_type, workspace_id, &detail) {
            tracing::warn!("failed to record {op_type} operation: {err:#}");
        }
    }

    /// Invokes the Arbiter fallback for one workspace's still-unresolved
    /// conflicted files, then records an `arbiter_decision` operation --
    /// see DESIGN.md ("pact-coord > Operation log / `pact history` (issue
    /// #84)"). `workspace_id` is derived from `worktree_path`'s final path
    /// component, which is always the workspace id (see
    /// `WorkspaceManager::preview_workspace_location`), rather than
    /// widening `ArbiterResolver`'s signature just to pass it through.
    fn run_arbiter(&self, config: &ArbiterConfig, worktree_path: &Path, task_text: &str, files: &[String]) -> Vec<String> {
        let identifier = worktree_path.file_name().and_then(|n| n.to_str()).unwrap_or("unknown");
        let resolved = self.run_arbiter_inner(config, worktree_path, identifier, task_text, files);
        self.log_operation(
            "arbiter_decision",
            Some(identifier),
            serde_json::json!({
                "files": files,
                "accepted": !resolved.is_empty(),
                "resolved_files": resolved,
            }),
        );
        resolved
    }

    fn run_arbiter_inner(
        &self,
        config: &ArbiterConfig,
        worktree_path: &Path,
        identifier: &str,
        task_text: &str,
        files: &[String],
    ) -> Vec<String> {
        // Both written to the same stable `state_dir/logs/` location a
        // normal workspace's own log uses -- deliberately NOT inside
        // `worktree_path` (the throwaway integration/resolve worktree),
        // which gets torn down unconditionally once merge_all/resolve_conflict
        // finishes. See DESIGN.md ("pact-core > Arbiter diagnosability",
        // issue #106) for why the raw log survives every rejection path;
        // `decision.json` extends that same convention (issue #148)
        // rather than introducing a second, competing directory
        // structure, per the outside-review triage this came from.
        let log_path = self.workspaces.state_dir().join("logs").join(format!("arbiter-{identifier}.jsonl"));
        let decision_path = self.workspaces.state_dir().join("logs").join(format!("arbiter-{identifier}.decision.json"));

        let started_at = unix_now();
        let outcome = attempt_arbiter_resolution(config, worktree_path, task_text, files, &log_path);
        let ended_at = unix_now();

        // Written on every attempt, accepted or rejected -- a passing
        // test command doesn't prove semantic correctness, so successful
        // attempts need the same durable record as failed ones (issue
        // #148). Best-effort: a write failure here must never mask the
        // actual accept/reject decision it would have recorded.
        let decision = build_arbiter_decision(identifier, config.agent, files, &config.test_cmd, &outcome, started_at, ended_at);
        if let Err(err) = std::fs::write(&decision_path, serde_json::to_vec_pretty(&decision).unwrap_or_default()) {
            tracing::warn!("arbiter: failed to write decision record to {}: {err:#}", decision_path.display());
        }

        match outcome {
            ArbiterOutcome::Accepted { resolved_files } => {
                let _ = std::fs::remove_file(&log_path);
                resolved_files
            }
            ArbiterOutcome::Rejected { reason, .. } => {
                tracing::warn!(
                    "arbiter: {reason} (log kept at {}, decision kept at {})",
                    log_path.display(),
                    decision_path.display()
                );
                Vec::new()
            }
        }
    }

    /// Reports files touched by more than one active workspace, among
    /// workspaces that share a common merge-base -- see issue #8.
    /// Informational only, same as MCP leases being advisory: nothing here
    /// blocks anything. Each conflict is enriched with any coordination-DB
    /// lease that matched the file (active or expired) and a coarse
    /// related-message count, since a workspace's id is the same string as
    /// its MCP `agent_id`, making that join direct.
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

    /// A full snapshot of the coordination layer's current state (issue
    /// #64) -- active leases and every known agent's pending message
    /// count, for `pact coord-status`.
    pub fn coord_status(&self) -> Result<pact_coord::CoordStatus> {
        pact_coord::status(&self.repo_root)
    }

    pub fn teardown(&self, id: &str, keep_branch: bool, force: bool) -> Result<()> {
        // WorkspaceManager::remove_workspace already kills any live agent
        // process recorded against this workspace before removing it, and
        // refuses on uncommitted changes unless `force` is set.
        self.workspaces.remove_workspace(id, keep_branch, force)?;
        self.log_operation("teardown", Some(id), serde_json::json!({ "keep_branch": keep_branch, "force": force }));
        Ok(())
    }

    /// The operation log for `pact history` -- see DESIGN.md ("pact-coord
    /// > Operation log / `pact history` (issue #84)").
    pub fn history(&self, filter: &pact_coord::HistoryFilter) -> Result<Vec<pact_coord::Operation>> {
        pact_coord::history(&self.repo_root, filter)
    }
}

/// Checks an Arbiter agent's resolution against the conflicted-file
/// scope it was given -- see DESIGN.md ("pact-core > Arbiter scope
/// enforcement", issue #146/#147). Returns `Err(reason)` for the first
/// violation found, `Ok(())` if the resolution passes every check.
///
/// Deliberately doesn't try to catch a merely *suspiciously large*
/// shrink in a conflicted file beyond "went to nothing": removing
/// marker lines and one side's content is an expected, normal part of
/// every correct resolution, so a size-based heuristic risks rejecting
/// good resolutions, not just bad ones. A missing file (the read fails)
/// is treated as a violation too, covering "arbiter deleted a
/// conflicted file" without a separate check for it.
///
/// Extracted from `run_arbiter_inner` specifically so it's testable
/// against a real git repo without spawning a real agent -- the prompt
/// instruction telling the agent not to touch anything outside `files`
/// is just that, a prompt instruction, not enforcement, so this is what
/// actually verifies it didn't.
/// One Arbiter attempt's result, detailed enough to build a full
/// `decision.json` record from -- `test_passed` is `None` whenever the
/// attempt was rejected before ever reaching the test-command step
/// (agent failure, leftover markers, out-of-scope changes, staging
/// failure), distinct from `Some(false)` (the test command itself ran
/// and failed).
enum ArbiterOutcome {
    Accepted { resolved_files: Vec<String> },
    Rejected { reason: String, test_passed: Option<bool> },
}

fn agent_kind_name(kind: AgentKind) -> &'static str {
    match kind {
        AgentKind::Claude => "claude",
        AgentKind::Copilot => "copilot",
        AgentKind::Codex => "codex",
        AgentKind::Gemini => "gemini",
    }
}

fn unix_now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Builds one Arbiter attempt's `decision.json` value (issue #148) --
/// pure and separate from `run_arbiter_inner` specifically so the field
/// mapping is testable without spawning a real agent.
fn build_arbiter_decision(
    identifier: &str,
    agent: AgentKind,
    files: &[String],
    test_cmd: &str,
    outcome: &ArbiterOutcome,
    started_at: u64,
    ended_at: u64,
) -> serde_json::Value {
    let (accepted, rejection_reason, test_passed) = match outcome {
        ArbiterOutcome::Accepted { .. } => (true, None, Some(true)),
        ArbiterOutcome::Rejected { reason, test_passed } => (false, Some(reason.as_str()), *test_passed),
    };
    serde_json::json!({
        "workspace_id": identifier,
        "agent": agent_kind_name(agent),
        "accepted": accepted,
        "rejection_reason": rejection_reason,
        "conflicted_files": files,
        "test_command": test_cmd,
        "test_passed": test_passed,
        "started_at": started_at,
        "ended_at": ended_at,
    })
}

/// The actual Arbiter attempt -- spawns the agent, then validates its
/// result -- split from `run_arbiter_inner` so every exit path funnels
/// through one `ArbiterOutcome` instead of scattering `tracing::warn!` +
/// early-return pairs, which is what made building a complete
/// `decision.json` on every path (issue #148) straightforward instead
/// of repetitive.
fn attempt_arbiter_resolution(
    config: &ArbiterConfig,
    worktree_path: &Path,
    task_text: &str,
    files: &[String],
    log_path: &Path,
) -> ArbiterOutcome {
    // Captured before the agent runs so the post-run "did it wipe a
    // conflicted file" check (in `validate_arbiter_scope`) has something
    // to compare against -- every listed file has conflict markers in it
    // at this point, so a non-empty pre-run length is a given; it's
    // recorded anyway rather than assumed, in case that ever stops being
    // true.
    let pre_run_lengths: std::collections::HashMap<&String, usize> = files
        .iter()
        .map(|file| {
            let len = std::fs::read_to_string(worktree_path.join(file)).map(|c| c.trim().len()).unwrap_or(0);
            (file, len)
        })
        .collect();

    let prompt = build_arbiter_prompt(task_text, files);
    let adapter = pact_agents::adapter(config.agent);
    let (program, args) = adapter.build_command(&prompt, config.safety_override.as_deref(), None, worktree_path);

    let supervisor = Supervisor::new();
    let outcome = pact_agents::run_and_stream(
        &supervisor,
        &program,
        &args,
        worktree_path,
        log_path,
        |line| adapter.parse_line(line),
        |_event| {},
        |_pid| {},
    );

    match outcome {
        Ok(run) if run.success => {}
        Ok(run) => {
            return ArbiterOutcome::Rejected {
                reason: format!("arbiter agent reported failure resolving {files:?}: {}", run.summary),
                test_passed: None,
            }
        }
        Err(err) => {
            return ArbiterOutcome::Rejected {
                reason: format!("arbiter agent failed to run for {files:?}: {err:#}"),
                test_passed: None,
            }
        }
    }

    // The agent's own reported success isn't trusted on its own -- see
    // `validate_arbiter_scope` for what's actually checked and why
    // (conflict markers, an emptied file, and anything changed outside
    // `files`).
    if let Err(reason) = validate_arbiter_scope(worktree_path, files, &pre_run_lengths) {
        return ArbiterOutcome::Rejected { reason, test_passed: None };
    }

    for file in files {
        let add = Command::new("git").args(["add", "--", file]).current_dir(worktree_path).output();
        if !matches!(add, Ok(ref o) if o.status.success()) {
            return ArbiterOutcome::Rejected {
                reason: format!("failed to stage {file} after resolution"),
                test_passed: None,
            };
        }
    }

    match run_shell(worktree_path, &config.test_cmd) {
        Ok(true) => ArbiterOutcome::Accepted { resolved_files: files.to_vec() },
        Ok(false) => ArbiterOutcome::Rejected {
            reason: format!(
                "arbiter's resolution for {files:?} failed the test command ('{}') -- not accepting it",
                config.test_cmd
            ),
            test_passed: Some(false),
        },
        Err(err) => ArbiterOutcome::Rejected {
            reason: format!("failed to run the arbiter test command '{}': {err:#}", config.test_cmd),
            test_passed: None,
        },
    }
}

fn validate_arbiter_scope(
    worktree_path: &Path,
    files: &[String],
    pre_run_lengths: &std::collections::HashMap<&String, usize>,
) -> Result<(), String> {
    for file in files {
        let Ok(content) = std::fs::read_to_string(worktree_path.join(file)) else {
            return Err(format!("could not re-read {file} after the agent ran"));
        };
        if content.contains("<<<<<<<") || content.contains("=======") || content.contains(">>>>>>>") {
            return Err(format!("left conflict markers in {file}"));
        }
        if pre_run_lengths.get(file).copied().unwrap_or(0) > 0 && content.trim().is_empty() {
            return Err(format!("emptied {file} entirely"));
        }
    }

    match pact_vcs::changed_paths(worktree_path) {
        Ok(changed) => {
            let out_of_scope: Vec<&String> = changed.iter().filter(|path| !files.contains(path)).collect();
            if !out_of_scope.is_empty() {
                return Err(format!("changed files outside the conflicted-file list {out_of_scope:?}"));
            }
        }
        Err(err) => return Err(format!("could not verify change scope via git status: {err:#}")),
    }
    Ok(())
}

fn build_arbiter_prompt(task_text: &str, files: &[String]) -> String {
    format!(
        "You are resolving a real git merge conflict left behind by pact's `merge-all`. \
         The change being merged in came from this task:\n\n{task_text}\n\n\
         It conflicts with work already merged from other agents. Git has left standard \
         conflict markers (<<<<<<<, =======, >>>>>>>) in the following file(s), which is the \
         directory you are working in right now: {}. \
         Resolve every conflict marker in these files so the result reflects the intent of BOTH \
         sides -- do not just pick one side and discard the other unless they are truly \
         incompatible. Do not edit, create, or delete any file outside this list. Do not run any \
         `git` command yourself -- pact stages and verifies your result afterward.",
        files.join(", ")
    )
}

/// Runs `cmd` as a shell command in `dir` (`cmd /C` on Windows, `sh -c`
/// elsewhere), returning whether it exited successfully.
fn run_shell(dir: &Path, cmd: &str) -> Result<bool> {
    let mut command = if cfg!(windows) {
        let mut c = Command::new("cmd");
        c.args(["/C", cmd]);
        c
    } else {
        let mut c = Command::new("sh");
        c.args(["-c", cmd]);
        c
    };
    let output = command
        .current_dir(dir)
        .output()
        .with_context(|| format!("failed to spawn arbiter test command '{cmd}'"))?;
    Ok(output.status.success())
}

/// Decides what (if anything) to warn about a spawned agent's coordination
/// connection, given the *last* status reported for `coord_name` over the
/// whole run -- not the first. A real connection reliably goes through a
/// transient `pending` status before `connected` within a fraction of a
/// second, so warning on that transient value (as opposed to whatever it
/// finally settled on) is a false positive that trains users to ignore
/// pact WARNs, making the genuinely bad case -- stuck on `pending`, or
/// `failed`, for the whole run -- read almost identically to normal.
/// Returns `None` when there's nothing worth warning about: coord wasn't
/// configured for this spawn at all, or it reached `connected`.
fn coord_warning(coord_configured: bool, last_status: Option<&str>, coord_name: &str) -> Option<String> {
    if !coord_configured {
        return None;
    }
    match last_status {
        None => Some(format!(
            "coordination server '{coord_name}' never reported a status at all -- file leases \
             and messaging will not work for this session (this is expected for adapters \
             without a confirmed event schema, e.g. Codex; see README)"
        )),
        Some("connected") => None,
        Some(status) => Some(format!(
            "coordination server '{coord_name}' never reached 'connected' (last reported \
             status: '{status}') -- file leases and messaging will not work for this session"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(agent: AgentKind, text: &str) -> SpawnManyTask {
        SpawnManyTask { agent, task: text.to_string() }
    }

    #[test]
    fn predict_task_overlap_finds_shared_barrel_file() {
        let tasks = vec![
            task(AgentKind::Claude, "add chunk.ts and export it from src/index.ts"),
            task(AgentKind::Claude, "add omit.ts and export it from src/index.ts"),
            task(AgentKind::Claude, "add pick.ts, no barrel export needed"),
        ];
        let overlaps = predict_task_overlap(&tasks);
        assert_eq!(overlaps.len(), 1);
        assert_eq!(overlaps[0].token, "src/index.ts");
        assert_eq!(overlaps[0].task_indices, vec![0, 1]);
    }

    #[test]
    fn predict_task_overlap_empty_when_nothing_shared() {
        let tasks = vec![
            task(AgentKind::Claude, "add chunk.ts"),
            task(AgentKind::Claude, "add omit.ts"),
        ];
        assert!(predict_task_overlap(&tasks).is_empty());
    }

    #[test]
    fn predict_task_overlap_ignores_a_file_mentioned_only_once() {
        let tasks = vec![
            task(AgentKind::Claude, "refactor src/index.ts entirely"),
            task(AgentKind::Claude, "add omit.ts, unrelated"),
        ];
        assert!(predict_task_overlap(&tasks).is_empty());
    }

    #[test]
    fn looks_like_file_path_accepts_plausible_paths() {
        assert!(looks_like_file_path("chunk.ts"));
        assert!(looks_like_file_path("src/index.ts"));
        assert!(looks_like_file_path("package.json"));
    }

    #[test]
    fn looks_like_file_path_rejects_plain_words_and_sentence_punctuation() {
        assert!(!looks_like_file_path("docs"));
        assert!(!looks_like_file_path(""));
        assert!(!looks_like_file_path("index"));
    }

    #[test]
    fn extract_file_tokens_trims_trailing_sentence_punctuation() {
        let tokens = extract_file_tokens("please update src/index.ts.");
        assert!(tokens.contains("src/index.ts"));
        assert!(!tokens.contains("src/index.ts."));
    }

    #[test]
    fn build_arbiter_prompt_includes_task_and_files_and_forbids_git() {
        let prompt = build_arbiter_prompt(
            "add chunk.ts export",
            &["src/index.ts".to_string(), "package.json".to_string()],
        );
        assert!(prompt.contains("add chunk.ts export"));
        assert!(prompt.contains("src/index.ts"));
        assert!(prompt.contains("package.json"));
        assert!(prompt.contains("Do not run any `git` command"));
    }

    #[test]
    fn run_shell_reports_success_and_failure() {
        let dir = std::env::temp_dir();
        assert!(run_shell(&dir, if cfg!(windows) { "exit 0" } else { "true" }).unwrap());
        assert!(!run_shell(&dir, if cfg!(windows) { "exit 1" } else { "false" }).unwrap());
    }

    #[test]
    fn coord_warning_is_none_when_coord_not_configured() {
        assert_eq!(coord_warning(false, None, "pact-coord"), None);
        assert_eq!(coord_warning(false, Some("pending"), "pact-coord"), None);
    }

    #[test]
    fn coord_warning_is_none_when_last_status_is_connected() {
        // The false-positive case this fixes: a normal spawn transitions
        // pending -> connected within the run, so only the last status
        // (connected) should be considered.
        assert_eq!(coord_warning(true, Some("connected"), "pact-coord"), None);
    }

    #[test]
    fn coord_warning_fires_when_status_never_settled_on_connected() {
        let warning = coord_warning(true, Some("pending"), "pact-coord").unwrap();
        assert!(warning.contains("never reached 'connected'"));
        assert!(warning.contains("last reported status: 'pending'"));
    }

    #[test]
    fn coord_warning_fires_on_explicit_failed_status() {
        let warning = coord_warning(true, Some("failed"), "pact-coord").unwrap();
        assert!(warning.contains("last reported status: 'failed'"));
    }

    #[test]
    fn coord_warning_fires_when_no_status_ever_reported() {
        let warning = coord_warning(true, None, "pact-coord").unwrap();
        assert!(warning.contains("never reported a status at all"));
    }

    fn arbiter_test_repo(name: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let root = std::env::temp_dir().join(format!("pact-core-arbiter-scope-{name}-{nanos}"));
        std::fs::create_dir_all(&root).unwrap();
        let run = |args: &[&str]| {
            let output = Command::new("git").args(args).current_dir(&root).output().unwrap();
            assert!(output.status.success(), "git {args:?} failed: {}", String::from_utf8_lossy(&output.stderr));
        };
        run(&["init", "-q"]);
        run(&["config", "user.email", "test@test.com"]);
        run(&["config", "user.name", "test"]);
        // conflicted.txt starts with real conflict-marker content, the
        // same shape the file would actually be in when Arbiter is
        // invoked -- pre_run_lengths reads this real state, not a stub.
        std::fs::write(root.join("conflicted.txt"), "<<<<<<< ours\nours\n=======\ntheirs\n>>>>>>> theirs\n").unwrap();
        std::fs::write(root.join("untouched.txt"), "unrelated content\n").unwrap();
        run(&["add", "-A"]);
        run(&["commit", "-q", "-m", "init"]);
        root
    }

    fn pre_run_lengths_for<'a>(root: &std::path::Path, files: &'a [String]) -> std::collections::HashMap<&'a String, usize> {
        files
            .iter()
            .map(|f| (f, std::fs::read_to_string(root.join(f)).map(|c| c.trim().len()).unwrap_or(0)))
            .collect()
    }

    #[test]
    fn validate_arbiter_scope_accepts_a_clean_in_scope_resolution() {
        let root = arbiter_test_repo("accepts-clean");
        let files = vec!["conflicted.txt".to_string()];
        let pre = pre_run_lengths_for(&root, &files);

        std::fs::write(root.join("conflicted.txt"), "resolved content\n").unwrap();

        assert!(validate_arbiter_scope(&root, &files, &pre).is_ok());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn validate_arbiter_scope_rejects_leftover_conflict_markers() {
        let root = arbiter_test_repo("rejects-markers");
        let files = vec!["conflicted.txt".to_string()];
        let pre = pre_run_lengths_for(&root, &files);
        // conflicted.txt is untouched -- markers still present.

        let err = validate_arbiter_scope(&root, &files, &pre).unwrap_err();
        assert!(err.contains("conflict markers"), "got: {err}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn validate_arbiter_scope_rejects_an_emptied_file() {
        let root = arbiter_test_repo("rejects-emptied");
        let files = vec!["conflicted.txt".to_string()];
        let pre = pre_run_lengths_for(&root, &files);

        std::fs::write(root.join("conflicted.txt"), "   \n\n").unwrap();

        let err = validate_arbiter_scope(&root, &files, &pre).unwrap_err();
        assert!(err.contains("emptied"), "got: {err}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn validate_arbiter_scope_rejects_a_deleted_conflicted_file() {
        let root = arbiter_test_repo("rejects-deleted");
        let files = vec!["conflicted.txt".to_string()];
        let pre = pre_run_lengths_for(&root, &files);

        std::fs::remove_file(root.join("conflicted.txt")).unwrap();

        let err = validate_arbiter_scope(&root, &files, &pre).unwrap_err();
        assert!(err.contains("could not re-read"), "got: {err}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn validate_arbiter_scope_rejects_changes_outside_the_conflicted_file_list() {
        let root = arbiter_test_repo("rejects-out-of-scope");
        let files = vec!["conflicted.txt".to_string()];
        let pre = pre_run_lengths_for(&root, &files);

        std::fs::write(root.join("conflicted.txt"), "resolved content\n").unwrap();
        // Arbiter was only told about conflicted.txt -- touching this
        // unrelated file is exactly what the prompt says not to do.
        std::fs::write(root.join("untouched.txt"), "surprise edit\n").unwrap();

        let err = validate_arbiter_scope(&root, &files, &pre).unwrap_err();
        assert!(err.contains("outside the conflicted-file list"), "got: {err}");
        assert!(err.contains("untouched.txt"), "got: {err}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn validate_arbiter_scope_accepts_a_new_file_created_within_scope() {
        // files can legitimately include a path that didn't exist pre-run
        // (e.g. a rename-vs-modify conflict resolved by keeping a new
        // path) -- pre_run_lengths defaults such a file to 0, which must
        // not itself trigger the "emptied" check once real content exists.
        let root = arbiter_test_repo("accepts-new-file");
        let files = vec!["brand_new.txt".to_string()];
        let pre = pre_run_lengths_for(&root, &files);
        assert_eq!(pre.get(&files[0]).copied(), Some(0));

        std::fs::write(root.join("brand_new.txt"), "new content\n").unwrap();

        assert!(validate_arbiter_scope(&root, &files, &pre).is_ok());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn run_metadata_round_trips_through_json_with_a_coord_status() {
        let metadata = RunMetadata {
            workspace_id: "ws-1".to_string(),
            agent: "claude".to_string(),
            program: "claude".to_string(),
            args: vec!["-p".to_string(), "do the thing".to_string()],
            cwd: PathBuf::from("/tmp/ws-1"),
            started_at: 100,
            ended_at: 142,
            exit_success: true,
            summary: "Created foo.rs".to_string(),
            coord_status: Some("connected".to_string()),
            log_path: PathBuf::from("/tmp/state/logs/ws-1.jsonl"),
        };

        let json = serde_json::to_string(&metadata).unwrap();
        let round_tripped: RunMetadata = serde_json::from_str(&json).unwrap();

        assert_eq!(round_tripped.workspace_id, "ws-1");
        assert_eq!(round_tripped.agent, "claude");
        assert_eq!(round_tripped.args, vec!["-p", "do the thing"]);
        assert_eq!(round_tripped.started_at, 100);
        assert_eq!(round_tripped.ended_at, 142);
        assert!(round_tripped.exit_success);
        assert_eq!(round_tripped.coord_status.as_deref(), Some("connected"));
    }

    #[test]
    fn run_metadata_round_trips_with_no_coord_status() {
        // A run with no coordination config attached at all -- distinct
        // from a coord status that was reported but never settled.
        let metadata = RunMetadata {
            workspace_id: "ws-2".to_string(),
            agent: "copilot".to_string(),
            program: "copilot".to_string(),
            args: vec![],
            cwd: PathBuf::from("/tmp/ws-2"),
            started_at: 0,
            ended_at: 5,
            exit_success: false,
            summary: "failed to run: spawn error".to_string(),
            coord_status: None,
            log_path: PathBuf::from("/tmp/state/logs/ws-2.jsonl"),
        };

        let json = serde_json::to_string(&metadata).unwrap();
        let round_tripped: RunMetadata = serde_json::from_str(&json).unwrap();
        assert!(round_tripped.coord_status.is_none());
        assert!(!round_tripped.exit_success);
    }

    #[test]
    fn build_arbiter_decision_records_an_accepted_attempt() {
        let files = vec!["a.rs".to_string(), "b.rs".to_string()];
        let outcome = ArbiterOutcome::Accepted { resolved_files: files.clone() };
        let decision = build_arbiter_decision("ws-1", AgentKind::Claude, &files, "cargo test", &outcome, 100, 105);

        assert_eq!(decision["workspace_id"], "ws-1");
        assert_eq!(decision["agent"], "claude");
        assert_eq!(decision["accepted"], true);
        assert!(decision["rejection_reason"].is_null());
        assert_eq!(decision["conflicted_files"], serde_json::json!(["a.rs", "b.rs"]));
        assert_eq!(decision["test_command"], "cargo test");
        assert_eq!(decision["test_passed"], true);
        assert_eq!(decision["started_at"], 100);
        assert_eq!(decision["ended_at"], 105);
    }

    #[test]
    fn build_arbiter_decision_records_a_rejected_attempt_with_its_reason() {
        let files = vec!["a.rs".to_string()];
        let outcome = ArbiterOutcome::Rejected {
            reason: "left conflict markers in a.rs".to_string(),
            test_passed: None,
        };
        let decision = build_arbiter_decision("ws-2", AgentKind::Copilot, &files, "npm test", &outcome, 200, 201);

        assert_eq!(decision["accepted"], false);
        assert_eq!(decision["rejection_reason"], "left conflict markers in a.rs");
        assert!(decision["test_passed"].is_null());
    }

    #[test]
    fn build_arbiter_decision_distinguishes_test_failure_from_never_reaching_the_test_step() {
        let files = vec!["a.rs".to_string()];
        let reached_test = ArbiterOutcome::Rejected {
            reason: "failed the test command".to_string(),
            test_passed: Some(false),
        };
        let decision = build_arbiter_decision("ws-3", AgentKind::Codex, &files, "go test", &reached_test, 0, 0);
        assert_eq!(decision["test_passed"], false);

        let never_reached = ArbiterOutcome::Rejected {
            reason: "agent failed to run".to_string(),
            test_passed: None,
        };
        let decision = build_arbiter_decision("ws-3", AgentKind::Codex, &files, "go test", &never_reached, 0, 0);
        assert!(decision["test_passed"].is_null());
    }
}
