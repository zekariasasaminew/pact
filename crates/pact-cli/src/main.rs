use std::path::{Path, PathBuf};

use anyhow::{bail, Result};
use clap::{Parser, Subcommand};

use pact_agents::{AgentEvent, AgentKind};
use pact_core::{CoordServerOverride, FileConflict, MergeReport, Orchestrator, SpawnManyTask};

#[derive(Parser)]
#[command(name = "pact", about = "Orchestrate parallel AI coding agent workspaces")]
struct Cli {
    /// Path to the git repository to operate on (defaults to the current directory's repo root)
    #[arg(long, global = true)]
    repo: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Create a new isolated agent workspace and run an agent CLI in it.
    /// The agent's changes land in the workspace's working tree, not a
    /// commit -- `list` will show it as `[dirty]` when the agent is done,
    /// not because something needs your attention, but because `commit-all`
    /// or `merge-all` is what actually commits it (`merge-all` does so
    /// automatically before merging).
    Spawn {
        /// Task/prompt to give the agent
        task: String,

        /// Which agent CLI to launch (claude, copilot, codex, gemini).
        #[arg(long, default_value = "claude")]
        agent: String,

        /// Raw safety/approval override passed straight through to the
        /// chosen agent's own vocabulary: Claude Code's --permission-mode
        /// values (acceptEdits, bypassPermissions, ...), Codex's
        /// --sandbox values (read-only, workspace-write, danger-full-access).
        /// Ignored by Copilot CLI, which has no gradient. Defaults to each
        /// adapter's own unattended-safety setting -- see the README for
        /// why that default differs by adapter (Claude Code has a real
        /// safer default; Copilot CLI and Codex don't yet).
        #[arg(long)]
        safety: Option<String>,

        /// Point the generated MCP coordination config at this command
        /// instead of launching `pact mcp-serve` -- see issue #10. Pact
        /// does no protocol translation: whatever this points at must
        /// speak pact-coord's own tool contract (claim_files/
        /// release_files/send_message/check_messages) on its own.
        #[arg(long)]
        coord_command: Option<String>,

        /// Argument for --coord-command, repeatable. Ignored if
        /// --coord-command isn't given.
        #[arg(long = "coord-arg")]
        coord_args: Vec<String>,
    },
    /// Create N isolated agent workspaces and run N agent CLIs in them
    /// concurrently, streaming their combined output live with each line
    /// attributed to its source. Existing single-`spawn` behavior is
    /// unchanged -- this is an entirely separate command, not an alternate
    /// mode of `spawn`. Same as `spawn`, each workspace stays `[dirty]` in
    /// `list` until `commit-all` or `merge-all` commits it.
    SpawnMany {
        /// One task per agent to run, repeatable. A task without an
        /// `<agent>:` prefix uses --agent (error if --agent wasn't given);
        /// a task with a prefix always uses that agent regardless of
        /// --agent, for mixing agents in one batch, e.g. `--agent claude
        /// --task "fix the bug" --task copilot:"write tests"` runs the
        /// first on claude and the second on copilot. Prefix is split on
        /// the first `:` only, so task text itself may contain colons.
        #[arg(long = "task", required = true)]
        tasks: Vec<String>,

        /// Default agent CLI for any --task without an explicit
        /// `<agent>:` prefix (claude, copilot, codex, gemini). A task with
        /// a prefix always uses that agent instead, even when --agent is
        /// also given. At least one of --agent or a per-task prefix is
        /// required for every task.
        #[arg(long)]
        agent: Option<String>,

        /// Same raw safety/approval override as `spawn --safety`, applied
        /// to every task in this batch -- see `spawn`'s help for the
        /// per-adapter vocabulary. Per-task safety overrides aren't
        /// supported in this first cut (see `pact-core::SpawnManyTask`).
        #[arg(long)]
        safety: Option<String>,

        /// Same coordination-server override as `spawn --coord-command`,
        /// applied to every task in this batch.
        #[arg(long)]
        coord_command: Option<String>,

        /// Argument for --coord-command, repeatable.
        #[arg(long = "coord-arg")]
        coord_args: Vec<String>,
    },
    /// List active agent workspaces
    List,
    /// Show what an agent has done in a workspace: committed changes on
    /// its branch (relative to where it forked from) and anything still
    /// only in its working tree.
    Diff {
        /// Workspace id (as shown by `list`)
        id: String,
    },
    /// Commit everything in a workspace's working tree with a message
    /// derived from its task ("agent <id>: <task>"). Without --id, commits
    /// every active workspace that's dirty; a clean workspace is a no-op,
    /// not an error. This is the same step `merge-all` runs on your behalf
    /// before merging -- run it standalone if you just want workspaces'
    /// work captured in a real commit without merging yet.
    CommitAll {
        /// Only commit this workspace (as shown by `list`), instead of every
        /// dirty active workspace.
        #[arg(long)]
        id: Option<String>,
    },
    /// Report files touched by more than one active workspace that forked
    /// from the same point in history. Informational only -- nothing here
    /// blocks anything, same as MCP leases being advisory.
    Conflicts,
    /// Merge every (or a chosen set of) active workspace onto a fresh
    /// integration branch. Auto-commits each workspace first, refuses any
    /// whose base commit is no longer part of this branch's history, then
    /// merges smallest-changeset-first, skipping (not aborting on) real
    /// conflicts. Never touches the repo's own checkout -- the result is a
    /// new local branch (default `pact/merged-<id>`); pushing it or opening
    /// a PR is a separate, deliberate step you take yourself.
    ///
    /// Exit code: 0 if every workspace merged, 2 if one or more were
    /// skipped (a real conflict, or a moving-base refusal) but nothing
    /// errored outright, 1 only for a hard/unexpected failure. A CI wrapper
    /// that wants "fail unless everything merged cleanly" should treat any
    /// non-zero exit as failure; one that's fine with partial merges landing
    /// (and a human resolving the rest) can treat exit 2 as a soft signal.
    MergeAll {
        /// Restrict the merge to these workspace ids (as shown by `list`),
        /// repeatable. Defaults to every active workspace.
        #[arg(long = "id")]
        ids: Vec<String>,

        /// Name for the resulting branch. Defaults to `pact/merged-<id>`.
        #[arg(long)]
        into: Option<String>,

        /// Show the planned merge order (after sequencing and the
        /// moving-base check) without touching any git state.
        #[arg(long)]
        dry_run: bool,

        /// Glob (repeatable) for files safe to resolve with a plain
        /// line-union merge on conflict (ours' lines, then any of theirs'
        /// not already present) -- e.g. a barrel export file. Only files
        /// you name here are ever touched this way; package.json's
        /// dependency blocks get their own JSON-aware merge automatically,
        /// no flag needed, and lockfiles are never auto-resolved. This is a
        /// naive line-level concat, not a code merge: for JS/TS files it
        /// refuses (falls back to a real conflict) if the result would
        /// contain two `module.exports =`/`export default` statements or a
        /// redeclared binding, but nothing else -- CSS cascade, config keys
        /// set twice, non-JS/TS languages -- is checked. Best suited to
        /// genuinely append-only, order-independent content: logs,
        /// CHANGELOG entries, ignore files.
        #[arg(long = "union")]
        union: Vec<String>,

        /// Enables the Arbiter fallback: for any file mechanical/semantic
        /// resolution still can't handle, a one-shot agent proposes a fix,
        /// accepted only if this command then exits successfully in the
        /// same worktree (e.g. "npm test", "cargo test"). Presence of this
        /// flag is what turns Arbiter on at all -- omit it and merge-all
        /// behaves exactly as before, no extra agent ever spawned or
        /// billed.
        #[arg(long = "test-cmd")]
        test_cmd: Option<String>,

        /// Which agent CLI Arbiter should use. Ignored unless --test-cmd
        /// is set.
        #[arg(long = "arbiter-agent", default_value = "claude")]
        arbiter_agent: String,

        /// Same raw safety/approval override as `spawn --safety`, applied
        /// to the Arbiter agent specifically. Ignored unless --test-cmd is
        /// set.
        #[arg(long = "arbiter-safety")]
        arbiter_safety: Option<String>,
    },
    /// Tear down an agent workspace
    Teardown {
        /// Workspace id (as shown by `list`)
        id: String,

        /// Don't delete the pact/<id> branch -- keep it around to inspect
        /// or rebase the workspace's commits after tearing it down.
        #[arg(long)]
        keep_branch: bool,

        /// Tear down even if the workspace has uncommitted changes,
        /// discarding them. Without this, `teardown` refuses on a dirty
        /// workspace -- see `pact diff <id>` to inspect what would be
        /// lost first.
        #[arg(long)]
        force: bool,
    },
    /// Run the coordination MCP server over stdio. Not meant to be invoked
    /// directly -- `spawn` launches this itself (as the agent CLI's own
    /// child process, per the generated --mcp-config) with these arguments
    /// already filled in.
    #[command(hide = true)]
    McpServe {
        #[arg(long)]
        agent_id: String,
        #[arg(long)]
        workspace: PathBuf,
    },
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "pact=info".into()),
        )
        .init();

    let cli = Cli::parse();
    let repo_root = match cli.repo {
        Some(p) => p,
        None => find_repo_root(&std::env::current_dir()?)?,
    };

    // mcp-serve gets its own, self-contained tokio runtime rather than
    // making the whole CLI async -- it's the only command that needs one
    // (rmcp requires async), and every other command stays exactly as
    // synchronous as it already is. See the README for why that tradeoff
    // was made deliberately, not by default.
    if let Command::McpServe { agent_id, workspace } = cli.command {
        let runtime = tokio::runtime::Runtime::new()?;
        return runtime.block_on(pact_coord::serve(&repo_root, agent_id, workspace));
    }

    let orchestrator = Orchestrator::open(repo_root)?;

    match cli.command {
        Command::Spawn {
            task,
            agent,
            safety,
            coord_command,
            coord_args,
        } => {
            let kind = AgentKind::parse(&agent).ok_or_else(|| {
                anyhow::anyhow!("unknown --agent '{agent}' (expected claude, copilot, codex, or gemini)")
            })?;
            let adapter = pact_agents::adapter(kind);
            match &safety {
                Some(s) => eprintln!(
                    "warning: running '{agent}' with an explicit safety override ({s}) -- \
                     verify this doesn't hang the session on a permission prompt in headless mode."
                ),
                None => eprintln!(
                    "warning: running '{agent}' unattended with no human in the loop, using: {}. \
                     Pass --safety explicitly to use a different setting.",
                    adapter.default_safety_description()
                ),
            }
            let coord_override = coord_command.map(|command| CoordServerOverride {
                command,
                args: coord_args,
            });

            let (workspace, outcome) = orchestrator.spawn(
                kind,
                &task,
                safety.as_deref(),
                coord_override.as_ref(),
                print_event,
            )?;

            println!("workspace {} ({})", workspace.id, workspace.branch);
            println!("  path: {}", workspace.path.display());
            println!(
                "  {}: {}",
                if outcome.success { "done" } else { "failed" },
                outcome.summary
            );
        }
        Command::SpawnMany {
            tasks,
            agent,
            safety,
            coord_command,
            coord_args,
        } => {
            let default_agent = agent
                .as_deref()
                .map(|name| {
                    AgentKind::parse(name)
                        .map(|kind| (kind, name))
                        .ok_or_else(|| anyhow::anyhow!("unknown --agent '{name}' (expected claude, copilot, codex, or gemini)"))
                })
                .transpose()?;
            let specs = tasks
                .iter()
                .map(|raw| parse_task_spec(raw, default_agent))
                .collect::<Result<Vec<_>>>()?;

            let mut warned_agents = std::collections::HashSet::new();
            for (kind, agent_name) in specs.iter().map(|(k, _, name)| (*k, name.clone())) {
                if !warned_agents.insert(kind) {
                    continue;
                }
                let adapter = pact_agents::adapter(kind);
                match &safety {
                    Some(s) => eprintln!(
                        "warning: running '{agent_name}' with an explicit safety override ({s}) -- \
                         verify this doesn't hang the session on a permission prompt in headless mode."
                    ),
                    None => eprintln!(
                        "warning: running '{agent_name}' unattended with no human in the loop, using: {}. \
                         Pass --safety explicitly to use a different setting.",
                        adapter.default_safety_description()
                    ),
                }
            }

            let batch: Vec<SpawnManyTask> = specs
                .into_iter()
                .map(|(agent, task, _)| SpawnManyTask { agent, task })
                .collect();

            let overlaps = pact_core::predict_task_overlap(&batch);
            if !overlaps.is_empty() {
                eprintln!(
                    "warning: {} of your tasks look like they'll touch the same file(s) -- \
                     expect a merge conflict there unless you separate that work:",
                    overlaps.iter().map(|o| o.task_indices.len()).sum::<usize>()
                );
                for overlap in &overlaps {
                    let indices: Vec<String> = overlap.task_indices.iter().map(|i| i.to_string()).collect();
                    eprintln!("  '{}' -- mentioned by tasks #{}", overlap.token, indices.join(", #"));
                }
            }

            let coord_override = coord_command.map(|command| CoordServerOverride {
                command,
                args: coord_args,
            });

            let results = orchestrator.spawn_many(
                batch,
                safety.as_deref(),
                coord_override.as_ref(),
                |index, agent, event| {
                    print_event_labeled(&format!("{}:{index}", agent_label(*agent)), event);
                },
            );

            let mut any_failed = false;
            for outcome in &results {
                match &outcome.result {
                    Ok((workspace, run)) => {
                        println!("workspace {} ({})", workspace.id, workspace.branch);
                        println!("  path: {}", workspace.path.display());
                        println!(
                            "  {}: {}",
                            if run.success { "done" } else { "failed" },
                            run.summary
                        );
                        any_failed |= !run.success;
                    }
                    Err(err) => {
                        println!("task #{}: failed before/during launch: {err:#}", outcome.index);
                        any_failed = true;
                    }
                }
            }
            if any_failed {
                std::process::exit(1);
            }
        }
        Command::List => {
            let workspaces = orchestrator.list()?;
            if workspaces.is_empty() {
                println!("no active workspaces");
            }
            for workspace in workspaces {
                let dirty = match orchestrator.is_dirty(&workspace.id) {
                    Ok(true) => "dirty",
                    Ok(false) => "clean",
                    Err(_) => "unknown", // e.g. workspace directory itself is gone
                };
                println!(
                    "{}  {}  {}  [{dirty}]",
                    workspace.id,
                    workspace.branch,
                    workspace.path.display()
                );
                println!("    task: {}", workspace.task);
            }
        }
        Command::Diff { id } => {
            let diff = orchestrator.diff(&id)?;
            println!("workspace {id}: committed on branch (vs. merge-base)");
            if diff.commit_log.is_empty() {
                println!("  (no commits on this branch yet)");
            } else {
                for line in diff.commit_log.lines() {
                    println!("  {line}");
                }
                for line in diff.committed_summary.lines() {
                    println!("  {line}");
                }
            }
            println!("workspace {id}: uncommitted (working tree)");
            if diff.uncommitted_status.is_empty() {
                println!("  (clean)");
            } else {
                for line in diff.uncommitted_status.lines() {
                    println!("  {line}");
                }
                for line in diff.uncommitted_summary.lines() {
                    println!("  {line}");
                }
            }
        }
        Command::CommitAll { id } => {
            let ids: Vec<String> = match id {
                Some(id) => vec![id],
                None => orchestrator.list()?.into_iter().map(|w| w.id).collect(),
            };
            if ids.is_empty() {
                println!("no active workspaces");
                return Ok(());
            }

            let mut any_failed = false;
            for id in ids {
                match orchestrator.commit_all(&id) {
                    Ok(true) => println!("{id}: committed"),
                    Ok(false) => println!("{id}: clean, nothing to commit"),
                    Err(err) => {
                        println!("{id}: failed to commit: {err:#}");
                        any_failed = true;
                    }
                }
            }
            if any_failed {
                std::process::exit(1);
            }
        }
        Command::Conflicts => {
            let conflicts = orchestrator.detect_conflicts()?;
            print_conflicts(&conflicts);
        }
        Command::MergeAll { ids, into, dry_run, union, test_cmd, arbiter_agent, arbiter_safety } => {
            let ids = if ids.is_empty() { None } else { Some(ids) };
            let arbiter = match test_cmd {
                Some(test_cmd) => {
                    let agent = AgentKind::parse(&arbiter_agent).ok_or_else(|| {
                        anyhow::anyhow!(
                            "unknown --arbiter-agent '{arbiter_agent}' (expected claude, copilot, codex, or gemini)"
                        )
                    })?;
                    Some(pact_core::ArbiterConfig {
                        agent,
                        safety_override: arbiter_safety,
                        test_cmd,
                    })
                }
                None => None,
            };
            let report = orchestrator.merge_all(ids.as_deref(), into.as_deref(), &union, arbiter.as_ref(), dry_run)?;
            print_merge_report(&report);
            // Exit 1 is reserved for a hard/unexpected failure (the `?`
            // above already exits 1 on one, via anyhow::Result's
            // Termination impl). One or more workspaces skipped -- a real
            // conflict, or the moving-base check -- is a distinct, softer
            // outcome: some work landed, it's just not everything. A CI
            // wrapper around `pact merge-all` shouldn't have to treat a
            // 50%-successful run identically to a crash.
            if !report.skipped.is_empty() {
                std::process::exit(2);
            }
        }
        Command::Teardown {
            id,
            keep_branch,
            force,
        } => {
            // Computed *before* removal -- workspace_changes needs the
            // branch, which teardown deletes. Informational only: this
            // never blocks the teardown itself, only warns.
            match orchestrator.detect_conflicts() {
                Ok(all) => {
                    let relevant: Vec<_> = all
                        .into_iter()
                        .filter(|c| c.workspace_ids.iter().any(|w| w == &id))
                        .collect();
                    if !relevant.is_empty() {
                        eprintln!(
                            "warning: workspace {id} shares changes with another active workspace:"
                        );
                        print_conflicts(&relevant);
                    }
                }
                Err(err) => tracing::warn!("could not check for cross-workspace conflicts: {err:#}"),
            }

            orchestrator.teardown(&id, keep_branch, force)?;
            println!("removed workspace {id}");
        }
        Command::McpServe { .. } => unreachable!("handled above, before the orchestrator opens"),
    }

    Ok(())
}

/// Prints a `merge-all` report -- the merge order/outcome for a real run,
/// or just the planned order for `--dry-run` (see `MergeReport::dry_run`).
fn print_merge_report(report: &MergeReport) {
    if report.dry_run {
        println!("dry run: would merge onto '{}' from {}", report.target_branch, short(&report.base_commit));
        println!("  planned order:");
        for id in &report.planned {
            println!("    {id}");
        }
    } else {
        println!("merged onto '{}' from {}", report.target_branch, short(&report.base_commit));
        if report.merged.is_empty() {
            println!("  (nothing merged cleanly)");
        }
        for workspace in &report.merged {
            println!("  merged  {} ({})", workspace.id, workspace.branch);
            if !workspace.auto_resolved.is_empty() {
                println!("          auto-resolved: {}", workspace.auto_resolved.join(", "));
            }
            if !workspace.arbiter_resolved.is_empty() {
                println!("          arbiter-resolved (agent + tests verified): {}", workspace.arbiter_resolved.join(", "));
            }
        }
    }

    if !report.skipped.is_empty() {
        println!("skipped -- needs a human:");
        for workspace in &report.skipped {
            println!("  {} ({}): {}", workspace.id, workspace.branch, workspace.reason);
        }
    }
}

fn short(sha: &str) -> &str {
    &sha[..sha.len().min(12)]
}

/// Prints a cross-workspace conflict report -- shared by the standalone
/// `conflicts` command and the informational warning `teardown` prints
/// before removing a workspace that shares changes with another.
fn print_conflicts(conflicts: &[FileConflict]) {
    if conflicts.is_empty() {
        println!("no cross-workspace conflicts found");
        return;
    }
    for conflict in conflicts {
        println!(
            "  {} -- touched by workspaces: {}",
            conflict.file,
            conflict.workspace_ids.join(", ")
        );
        for (pattern, holder) in &conflict.related_leases {
            println!("    lease: '{pattern}' held by {holder}");
        }
        if conflict.related_message_count > 0 {
            println!(
                "    {} related coordination message(s) -- see the message log for context",
                conflict.related_message_count
            );
        }
    }
}

/// Parses one `--task <agent>:<text>` argument, splitting on the *first*
/// `:` only so task text itself may freely contain colons (e.g.
/// `claude:implement X: handle the edge case`). Returns the parsed
/// `AgentKind`, the raw task text, and the original agent name (for
/// warning messages, which want the user's own spelling).
/// Parses one `--task` value into `(agent, task text, agent display name)`.
/// A `<agent>:` prefix, when present and recognized, always wins -- that's
/// what makes mixing agents in one `spawn-many` batch possible even when
/// `--agent` also sets a default. `default` (from `--agent`) is what a task
/// without a recognized prefix falls back to; without one, a prefix is
/// mandatory, same as before `--agent` existed on this command.
fn parse_task_spec(raw: &str, default: Option<(AgentKind, &str)>) -> Result<(AgentKind, String, String)> {
    if let Some((agent_name, task)) = raw.split_once(':') {
        if let Some(kind) = AgentKind::parse(agent_name) {
            if task.trim().is_empty() {
                bail!("--task '{raw}' has empty task text after the ':'");
            }
            return Ok((kind, task.to_string(), agent_name.to_string()));
        }
        if default.is_none() {
            bail!(
                "unknown agent '{agent_name}' in --task '{raw}' (expected claude, copilot, codex, or gemini)"
            );
        }
    }

    let Some((kind, name)) = default else {
        bail!(
            "--task '{raw}' must be in the form <agent>:<task text>, e.g. claude:\"fix the bug\" \
             (or pass --agent to set a default agent for tasks without a prefix)"
        );
    };
    if raw.trim().is_empty() {
        bail!("--task '{raw}' is empty");
    }
    Ok((kind, raw.to_string(), name.to_string()))
}

fn agent_label(kind: AgentKind) -> &'static str {
    match kind {
        AgentKind::Claude => "claude",
        AgentKind::Copilot => "copilot",
        AgentKind::Codex => "codex",
        AgentKind::Gemini => "gemini",
    }
}

/// Same event formatting as `print_event`, prefixed with `label` so N
/// interleaved concurrent agents' output stays attributable. No extra
/// locking beyond what `println!`'s own internal `Stdout` lock already
/// gives per call -- each event here becomes one complete line written in
/// one call, so concurrent threads' lines interleave at line granularity,
/// never mid-line.
fn print_event_labeled(label: &str, event: &AgentEvent) {
    match event {
        AgentEvent::Init { session_id } => println!("[{label}] [init] session {session_id}"),
        AgentEvent::CoordStatus { name, status } => {
            println!("[{label}] [coord] {name}: {status}")
        }
        AgentEvent::AssistantText(text) => println!("[{label}] [assistant] {text}"),
        AgentEvent::ToolUse { name, input } => println!("[{label}] [tool] {name} {input}"),
        AgentEvent::Result { .. } => {}
        AgentEvent::Other(value) => println!("[{label}] [other] {value}"),
    }
}

/// Prints one streamed agent event. `Other` is deliberately not skipped --
/// an unrecognized event is far more likely to be a real message this
/// adapter doesn't parse in detail yet (a tool-result echo, for instance)
/// than something safe to drop silently.
fn print_event(event: &AgentEvent) {
    match event {
        AgentEvent::Init { session_id } => println!("[init] session {session_id}"),
        AgentEvent::CoordStatus { name, status } => println!("[coord] {name}: {status}"),
        AgentEvent::AssistantText(text) => println!("[assistant] {text}"),
        AgentEvent::ToolUse { name, input } => println!("[tool] {name} {input}"),
        AgentEvent::Result { .. } => {} // surfaced by the caller as the final outcome instead
        AgentEvent::Other(value) => println!("[other] {value}"),
    }
}

/// Walks up from `start` looking for a directory containing `.git`.
fn find_repo_root(start: &Path) -> Result<PathBuf> {
    let mut dir = start.to_path_buf();
    loop {
        if dir.join(".git").exists() {
            return Ok(dir);
        }
        if !dir.pop() {
            bail!(
                "no git repository found in '{}' or any parent directory; pass --repo explicitly",
                start.display()
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_task_spec_uses_explicit_prefix_with_no_default() {
        let (kind, task, name) = parse_task_spec("claude:fix the bug", None).unwrap();
        assert_eq!(kind, AgentKind::Claude);
        assert_eq!(task, "fix the bug");
        assert_eq!(name, "claude");
    }

    #[test]
    fn parse_task_spec_requires_a_prefix_when_no_default_is_set() {
        let err = parse_task_spec("fix the bug", None).unwrap_err();
        assert!(err.to_string().contains("must be in the form"), "unexpected error: {err}");
    }

    #[test]
    fn parse_task_spec_falls_back_to_default_when_no_prefix_present() {
        let (kind, task, name) =
            parse_task_spec("fix the bug", Some((AgentKind::Copilot, "copilot"))).unwrap();
        assert_eq!(kind, AgentKind::Copilot);
        assert_eq!(task, "fix the bug");
        assert_eq!(name, "copilot");
    }

    #[test]
    fn parse_task_spec_prefix_wins_over_default_for_mixed_batches() {
        let (kind, task, name) =
            parse_task_spec("claude:fix the bug", Some((AgentKind::Copilot, "copilot"))).unwrap();
        assert_eq!(kind, AgentKind::Claude);
        assert_eq!(task, "fix the bug");
        assert_eq!(name, "claude");
    }

    #[test]
    fn parse_task_spec_falls_back_to_default_when_colon_prefix_is_not_a_known_agent() {
        // A colon in the task text itself, not a real agent prefix -- with
        // a default set, the whole string is the task text.
        let (kind, task, name) =
            parse_task_spec("fix the bug: handle empty array", Some((AgentKind::Copilot, "copilot"))).unwrap();
        assert_eq!(kind, AgentKind::Copilot);
        assert_eq!(task, "fix the bug: handle empty array");
        assert_eq!(name, "copilot");
    }

    #[test]
    fn parse_task_spec_reports_unknown_agent_prefix_when_no_default_is_set() {
        let err = parse_task_spec("bogus:fix the bug", None).unwrap_err();
        assert!(err.to_string().contains("unknown agent 'bogus'"), "unexpected error: {err}");
    }

    #[test]
    fn parse_task_spec_rejects_empty_task_text_after_prefix() {
        let err = parse_task_spec("claude:", None).unwrap_err();
        assert!(err.to_string().contains("empty task text"), "unexpected error: {err}");
    }
}
