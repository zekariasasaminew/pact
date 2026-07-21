use std::path::{Path, PathBuf};
use std::process::Command;

use pact_agents::{AgentEvent, AgentKind, CoordConfig, RunOutcome, Supervisor};
use pact_vcs::{Workspace, WorkspaceDiff, WorkspaceManager};
use anyhow::{Context, Result};

pub use pact_vcs::{ArbiterResolver, MergedWorkspace, MergeReport, SkippedWorkspace};

/// Configuration for the Arbiter fallback resolver -- the "verified" half
/// of pact's conflict story (see the merge-all design notes): a one-shot
/// headless agent proposes a resolution for a file the mechanical/semantic
/// auto-resolution in `merge_all` couldn't handle, but that resolution is
/// only ever accepted if `test_cmd` then passes in the same worktree.
/// Entirely opt-in -- `Orchestrator::merge_all` with `arbiter: None` never
/// spawns an extra agent or spends anything beyond what `spawn_many`
/// already would.
pub struct ArbiterConfig {
    pub agent: AgentKind,
    pub safety_override: Option<String>,
    /// Shell command run (via `cmd /C` on Windows, `sh -c` elsewhere) in the
    /// worktree after the agent finishes -- a non-zero exit means the
    /// resolution is rejected and the merge falls back to a reported
    /// conflict exactly as if Arbiter hadn't run. There is deliberately no
    /// "skip verification if no test command is configured" path: a
    /// resolution nothing verified isn't something `merge_all` will accept.
    pub test_cmd: String,
}

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

/// One file-like token mentioned in more than one task's text within the
/// same `spawn_many` batch -- "Weaver": the prevention half of the
/// conflict-avoidance story (see the merge-all design notes). Pure text
/// analysis, no agent spawned, run *before* anything is spawned at all, on
/// the theory that decomposition-time prevention is cheaper and more
/// reliable than any amount of post-hoc merge cleverness -- this is a
/// heuristic prediction, not a guarantee: it never blocks `spawn_many`, it
/// only gives the caller something to warn about (same "informational,
/// nothing here blocks anything" posture `Orchestrator::detect_conflicts`
/// already established for git-level overlap).
pub struct PredictedOverlap {
    pub token: String,
    /// Indices into the `spawn_many` task list (0-based) whose text
    /// mentioned `token`. Always at least 2 entries.
    pub task_indices: Vec<usize>,
}

/// Scans every task's text for file-path-like tokens and reports any token
/// mentioned by two or more tasks -- e.g. 5 of 10 tasks each saying "export
/// it from `src/index.ts`" predicts exactly the conflict the pact v0.2
/// trial report hit. Deliberately conservative about false negatives, not
/// false positives: missing a real overlap just means this specific
/// prediction isn't caught (no worse than not running this at all), while
/// an occasional false-positive token (e.g. "next.js" read as a file) costs
/// nothing worse than one harmless extra line in a warning.
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

/// A conservative, regex-free "does this look like a file path" check: ends
/// in a short alphanumeric extension after the last `.`, with a non-empty
/// stem made of path-ish characters. Not a real path grammar -- see
/// `predict_task_overlap`'s doc comment for why that's an acceptable
/// tradeoff here.
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
        )?;

        if let Some(message) = coord_warning(coord.is_some(), coord_last_status.as_deref(), coord_name) {
            tracing::warn!("workspace {}: {message}", workspace.id);
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

    /// Closes the loop from "N dirty workspaces" to "one clean integration
    /// branch" -- see `pact_vcs::WorkspaceManager::merge_all`. `arbiter`, if
    /// given, is wired in as pact-vcs's `ArbiterResolver` hook -- pact-vcs
    /// itself has no dependency on `pact-agents`, so this is the one place
    /// that bridges "a file mechanical/semantic resolution couldn't handle"
    /// to "actually spawn an agent to look at it."
    pub fn merge_all(
        &self,
        ids: Option<&[String]>,
        target_branch: Option<&str>,
        union_globs: &[String],
        arbiter: Option<&ArbiterConfig>,
        dry_run: bool,
    ) -> Result<MergeReport> {
        let resolver = |worktree_path: &Path, task_text: &str, files: &[String]| -> Vec<String> {
            self.run_arbiter(arbiter.expect("resolver only invoked when arbiter is Some"), worktree_path, task_text, files)
        };
        let resolver_ref: Option<&ArbiterResolver<'_>> = if arbiter.is_some() { Some(&resolver) } else { None };
        self.workspaces.merge_all(ids, target_branch, union_globs, resolver_ref, dry_run)
    }

    /// Invokes the Arbiter fallback for one workspace's still-unresolved
    /// conflicted files: a one-shot headless agent is given the conflicting
    /// file(s) (git's own `<<<<<<<`/`=======`/`>>>>>>>` markers still in
    /// place) and the conflicting workspace's task text, asked to resolve
    /// them in place. The result is accepted only if (a) no conflict
    /// markers remain, (b) the files stage cleanly, and (c)
    /// `config.test_cmd` then exits successfully in the same worktree --
    /// any failure at any step returns an empty list, and the caller
    /// (pact-vcs) aborts the whole merge attempt exactly as if this were
    /// never called. Never partially accepted.
    fn run_arbiter(&self, config: &ArbiterConfig, worktree_path: &Path, task_text: &str, files: &[String]) -> Vec<String> {
        let prompt = build_arbiter_prompt(task_text, files);
        let adapter = pact_agents::adapter(config.agent);
        let (program, args) = adapter.build_command(&prompt, config.safety_override.as_deref(), None, worktree_path);

        let log_path = worktree_path.join(".pact-arbiter.jsonl");
        let supervisor = Supervisor::new();
        let outcome = pact_agents::run_and_stream(
            &supervisor,
            &program,
            &args,
            worktree_path,
            &log_path,
            |line| adapter.parse_line(line),
            |_event| {},
            |_pid| {},
        );
        let _ = std::fs::remove_file(&log_path);

        match outcome {
            Ok(run) if run.success => {}
            Ok(run) => {
                tracing::warn!("arbiter agent reported failure resolving {files:?}: {}", run.summary);
                return Vec::new();
            }
            Err(err) => {
                tracing::warn!("arbiter agent failed to run for {files:?}: {err:#}");
                return Vec::new();
            }
        }

        // The agent's own reported success isn't trusted on its own --
        // conflict markers left behind mean it didn't actually finish, no
        // matter what it said.
        for file in files {
            let Ok(content) = std::fs::read_to_string(worktree_path.join(file)) else {
                tracing::warn!("arbiter: could not re-read {file} after the agent ran");
                return Vec::new();
            };
            if content.contains("<<<<<<<") || content.contains("=======") || content.contains(">>>>>>>") {
                tracing::warn!("arbiter left conflict markers in {file}, not accepting its resolution");
                return Vec::new();
            }
        }

        for file in files {
            let add = Command::new("git").args(["add", "--", file]).current_dir(worktree_path).output();
            if !matches!(add, Ok(ref o) if o.status.success()) {
                tracing::warn!("arbiter: failed to stage {file} after resolution");
                return Vec::new();
            }
        }

        match run_shell(worktree_path, &config.test_cmd) {
            Ok(true) => files.to_vec(),
            Ok(false) => {
                tracing::warn!(
                    "arbiter's resolution for {files:?} failed the test command ('{}') -- not \
                     accepting it",
                    config.test_cmd
                );
                Vec::new()
            }
            Err(err) => {
                tracing::warn!("failed to run the arbiter test command '{}': {err:#}", config.test_cmd);
                Vec::new()
            }
        }
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

/// Builds the Arbiter agent's task text: the conflicting workspace's own
/// task, the exact files it's being asked to edit (and nothing else), and
/// an explicit instruction not to run `git` itself -- pact stages and
/// verifies the result afterward, not the agent.
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
}
