use std::path::{Path, PathBuf};

use anyhow::{bail, Result};
use clap::{Parser, Subcommand};

use pact_agents::{AgentEvent, AgentKind};
use pact_core::{CoordServerOverride, FileConflict, MergeReport, Orchestrator, PredictedOverlap, SpawnManyTask};

#[derive(Parser)]
#[command(name = "pact", version = env!("PACT_VERSION"), about = "Orchestrate parallel AI coding agent workspaces")]
struct Cli {
    /// Path to the git repository to operate on (defaults to the current directory's repo root)
    #[arg(long, global = true)]
    repo: Option<PathBuf>,

    /// Show every streamed agent event, including ones filtered out by
    /// default as noise (e.g. Copilot CLI's `session.background_tasks_changed`).
    /// Only affects what's printed live -- the full unfiltered stream is
    /// always written to the workspace's log file regardless of this flag.
    #[arg(short, long, global = true)]
    verbose: bool,

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

        /// Print what this spawn would do -- the workspace id/branch/path
        /// that would be created, detected package manager(s), and the
        /// exact program/args that would be launched -- then exit without
        /// creating a workspace, running dependency prep, or launching
        /// anything.
        #[arg(long)]
        dry_run: bool,
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

        /// Same as `spawn --dry-run`, applied to every task in this batch --
        /// prints each task's preview and exits without creating any
        /// workspace, running dependency prep, or launching anything.
        #[arg(long)]
        dry_run: bool,
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
    /// Show the coordination layer's current state: every active file
    /// lease and each known agent's pending (unread) message count.
    /// Read-only -- unlike an agent calling check_messages, looking here
    /// never marks anything as read.
    CoordStatus,
    /// Show the coordination layer's operation log: every claim, release,
    /// broadcast, direct message, merge-all invocation, arbiter decision,
    /// and teardown recorded this session (and any prior session against
    /// the same repo -- the log isn't cleared between runs). Read-only
    /// query only -- no undo, no replay-as-mutation.
    History {
        /// Only operations recorded against this workspace id.
        #[arg(long)]
        workspace: Option<String>,
        /// Only operations at or after this Unix timestamp (seconds).
        #[arg(long)]
        since: Option<i64>,
        /// Only operations of this type (claim, release, broadcast,
        /// message, merge_all, arbiter_decision, teardown).
        #[arg(long = "type")]
        op_type: Option<String>,
        /// Show at most this many operations (newest first).
        #[arg(long)]
        limit: Option<i64>,
        /// Print raw JSON rows instead of a human-readable summary line
        /// per operation.
        #[arg(long)]
        json: bool,
    },
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

        /// Gates each workspace's clean merge on this command passing in
        /// the integration worktree (e.g. "npm test", "cargo test")
        /// before it's accepted -- a failure undoes just that one merge
        /// and skips the workspace, same as a real conflict. Runs once
        /// per accepted workspace, not once at the end against the fully
        /// merged branch -- see the README for why. Distinct from
        /// --test-cmd: that verifies an Arbiter-proposed conflict
        /// resolution; this gates every clean merge, Arbiter-resolved or
        /// not. The two can be the same command or different ones.
        #[arg(long = "require-passing-tests")]
        require_passing_tests: Option<String>,
    },
    /// Without a workspace id, lists every open conflict `merge-all`
    /// skipped (which files, which target branch, when). With one,
    /// retries merging that workspace's branch into the target branch it
    /// conflicted against -- same auto-resolve/`--union`/Arbiter flags as
    /// `merge-all` itself, so a retry behaves identically to the original
    /// attempt. `--abandon` marks it abandoned instead of retrying.
    Resolve {
        /// Workspace id (as shown by an argument-less `pact resolve`).
        /// Omit to list every open conflict instead of acting on one.
        workspace: Option<String>,

        /// Mark the conflict abandoned instead of retrying it. Requires a
        /// workspace id.
        #[arg(long)]
        abandon: bool,

        /// Same as `merge-all --union`, applied to this retry.
        #[arg(long = "union")]
        union: Vec<String>,

        /// Same as `merge-all --test-cmd` -- enables the Arbiter fallback
        /// for this retry specifically.
        #[arg(long = "test-cmd")]
        test_cmd: Option<String>,

        /// Same as `merge-all --arbiter-agent`. Ignored unless --test-cmd
        /// is set.
        #[arg(long = "arbiter-agent", default_value = "claude")]
        arbiter_agent: String,

        /// Same as `merge-all --arbiter-safety`. Ignored unless
        /// --test-cmd is set.
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
    /// Print a shell completion script for `pact` to stdout -- e.g. `pact
    /// completions bash > /etc/bash_completion.d/pact` (or wherever your
    /// shell loads completions from) to install it.
    Completions {
        shell: clap_complete::Shell,
    },
    /// Check whether your environment is actually ready for `pact`: is
    /// `git` new enough for `worktree`, which agent CLIs (claude, copilot,
    /// codex, gemini) are installed, and which package-manager CLIs
    /// `pact-deps` already knows how to prep. Read-only -- doesn't install
    /// or fix anything. A missing agent CLI or package manager is
    /// informational, not a failure, since not everyone needs all of them;
    /// only a missing (or too-old) `git` makes this exit non-zero.
    Doctor,
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "pact=info".into()),
        )
        .init();

    let cli = Cli::parse();
    let verbose = cli.verbose;

    if let Command::Completions { shell } = cli.command {
        clap_complete::generate(shell, &mut <Cli as clap::CommandFactory>::command(), "pact", &mut std::io::stdout());
        return Ok(());
    }

    if let Command::Doctor = cli.command {
        return run_doctor();
    }

    let repo_root = match cli.repo {
        Some(p) => p,
        None => find_repo_root(&std::env::current_dir()?)?,
    };

    if let Command::McpServe { agent_id, workspace } = cli.command {
        // A current-thread runtime, not the default multi-threaded one --
        // see DESIGN.md ("pact-coord > mcp-serve startup latency", issue
        // #105) for why this matters specifically under concurrent
        // spawn-many: one stdio MCP server serving one client has no use
        // for a worker thread pool, and spinning one up anyway multiplies
        // real OS thread/CPU contention exactly when N of these subprocesses
        // start at once.
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
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
            dry_run,
        } => {
            let kind = AgentKind::parse(&agent).ok_or_else(|| {
                anyhow::anyhow!("unknown --agent '{agent}' (expected claude, copilot, codex, or gemini)")
            })?;
            let adapter = pact_agents::adapter(kind);
            let coord_override = coord_command.map(|command| CoordServerOverride {
                command,
                args: coord_args,
            });

            if dry_run {
                let preview = orchestrator.spawn_preview(kind, &task, safety.as_deref(), coord_override.as_ref())?;
                print_spawn_preview(&preview);
                return Ok(());
            }

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

            let (workspace, outcome) = orchestrator.spawn(
                kind,
                &task,
                safety.as_deref(),
                coord_override.as_ref(),
                |event| print_event(event, verbose),
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
            dry_run,
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

            if !dry_run {
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
                    distinct_overlapping_task_count(&overlaps)
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

            if dry_run {
                for (index, task) in batch.iter().enumerate() {
                    let preview = orchestrator.spawn_preview(
                        task.agent,
                        &task.task,
                        safety.as_deref(),
                        coord_override.as_ref(),
                    )?;
                    println!("task #{index} ({}):", agent_label(task.agent));
                    print_spawn_preview(&preview);
                }
                return Ok(());
            }

            let results = orchestrator.spawn_many(
                batch,
                safety.as_deref(),
                coord_override.as_ref(),
                |index, agent, event| {
                    print_event_labeled(&format!("{}:{index}", agent_label(*agent)), event, verbose);
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
                // A recorded agent_pid that's still alive is either a
                // normal in-progress spawn, or -- if the `pact` process
                // that launched it crashed -- an orphan `teardown --force`
                // hasn't cleaned up yet (issue #108). Can't distinguish
                // those two cases from here, so surface the raw fact
                // rather than guessing; a stale (dead) pid is worth
                // knowing about too, since it means metadata wasn't
                // cleared after the agent actually finished.
                if let Some(pid) = workspace.agent_pid {
                    let status = if pact_core::agent_process_alive(pid) { "running" } else { "not running" };
                    println!("    agent pid: {pid} ({status})");
                }
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
        Command::CoordStatus => {
            let status = orchestrator.coord_status()?;
            print_coord_status(&status);
        }
        Command::History { workspace, since, op_type, limit, json } => {
            let filter = pact_coord::HistoryFilter { workspace_id: workspace, since, op_type, limit };
            let operations = orchestrator.history(&filter)?;
            print_history(&operations, json);
        }
        Command::MergeAll { ids, into, dry_run, union, test_cmd, arbiter_agent, arbiter_safety, require_passing_tests } => {
            let ids = if ids.is_empty() { None } else { Some(ids) };
            let arbiter = build_arbiter_config(test_cmd, &arbiter_agent, arbiter_safety)?;
            let report = orchestrator.merge_all(
                ids.as_deref(),
                into.as_deref(),
                &union,
                arbiter.as_ref(),
                require_passing_tests.as_deref(),
                dry_run,
            )?;
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
        Command::Resolve { workspace, abandon, union, test_cmd, arbiter_agent, arbiter_safety } => {
            let Some(workspace_id) = workspace else {
                if abandon {
                    bail!("--abandon requires a workspace id");
                }
                let open = orchestrator.open_conflicts()?;
                print_open_conflicts(&open);
                return Ok(());
            };

            if abandon {
                if orchestrator.abandon_conflict(&workspace_id)? {
                    println!("abandoned the open conflict for workspace {workspace_id}");
                } else {
                    println!("no open conflict recorded for workspace {workspace_id}");
                }
                return Ok(());
            }

            let arbiter = build_arbiter_config(test_cmd, &arbiter_agent, arbiter_safety)?;
            let resolution = orchestrator.resolve_conflict(&workspace_id, &union, arbiter.as_ref())?;
            match resolution.outcome {
                pact_core::ResolveOutcome::Resolved { auto_resolved, arbiter_resolved } => {
                    println!("resolved: workspace {workspace_id} merged cleanly");
                    if !auto_resolved.is_empty() {
                        println!("  auto-resolved: {}", auto_resolved.join(", "));
                    }
                    if !arbiter_resolved.is_empty() {
                        println!("  arbiter-resolved: {}", arbiter_resolved.join(", "));
                    }
                }
                pact_core::ResolveOutcome::StillConflicted { files } => {
                    println!("still conflicted: {}", files.join(", "));
                    std::process::exit(2);
                }
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
        Command::Completions { .. } => unreachable!("handled above, before the orchestrator opens"),
        Command::Doctor => unreachable!("handled above, before the orchestrator opens"),
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
    if !report.conflicted.is_empty() {
        println!("(a real conflict, not just a moving base, is resumable later: `pact resolve <id>`)");
    }
}

fn print_open_conflicts(open: &[pact_coord::PersistedConflict]) {
    if open.is_empty() {
        println!("no open conflicts");
        return;
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    println!("open conflicts:");
    for conflict in open {
        let age = (now - conflict.created_at).max(0);
        println!(
            "  {} -> {} ({age}s ago): {}",
            conflict.workspace_id,
            conflict.target_branch,
            conflict.files.join(", ")
        );
    }
}

/// Builds an `ArbiterConfig` from `merge-all`/`resolve`'s shared
/// `--test-cmd`/`--arbiter-agent`/`--arbiter-safety` flags -- `None`
/// unless `--test-cmd` is given, since presence of that flag is what
/// turns Arbiter on at all for either command.
fn build_arbiter_config(
    test_cmd: Option<String>,
    arbiter_agent: &str,
    arbiter_safety: Option<String>,
) -> Result<Option<pact_core::ArbiterConfig>> {
    let Some(test_cmd) = test_cmd else { return Ok(None) };
    let agent = AgentKind::parse(arbiter_agent).ok_or_else(|| {
        anyhow::anyhow!("unknown --arbiter-agent '{arbiter_agent}' (expected claude, copilot, codex, or gemini)")
    })?;
    Ok(Some(pact_core::ArbiterConfig { agent, safety_override: arbiter_safety, test_cmd }))
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

/// Prints `pact history` results -- see DESIGN.md ("pact-coord >
/// Operation log / `pact history` (issue #84)"). `--json` prints the raw
/// rows; otherwise a compact, type-specific one-line summary derived from
/// each operation's `detail`, falling back to the raw JSON for any
/// `op_type` this doesn't have a specific summary for.
fn print_history(operations: &[pact_coord::Operation], json: bool) {
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(operations).unwrap_or_else(|e| format!("error serializing result: {e}"))
        );
        return;
    }

    if operations.is_empty() {
        println!("no operations recorded");
        return;
    }

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    for op in operations {
        let age = (now - op.created_at).max(0);
        let workspace = op.workspace_id.as_deref().unwrap_or("-");
        println!("{age}s ago  {:<17} {workspace:<10} {}", op.op_type, history_summary(op));
    }
}

fn history_summary(op: &pact_coord::Operation) -> String {
    let d = &op.detail;
    match op.op_type.as_str() {
        "claim" => format!(
            "{} (conflicts: {})",
            d.get("patterns").cloned().unwrap_or_default(),
            d.get("has_conflicts").and_then(|v| v.as_bool()).unwrap_or(false)
        ),
        "release" => format!(
            "{} ({} released)",
            d.get("patterns").cloned().unwrap_or_default(),
            d.get("released").and_then(|v| v.as_i64()).unwrap_or(0)
        ),
        "broadcast" => format!("\"{}\"", d.get("subject").and_then(|v| v.as_str()).unwrap_or("")),
        "message" => format!(
            "to {}: \"{}\"",
            d.get("to").and_then(|v| v.as_str()).unwrap_or("?"),
            d.get("subject").and_then(|v| v.as_str()).unwrap_or("")
        ),
        "merge_all" => format!(
            "-> {}: merged {}, skipped {}{}",
            d.get("target_branch").and_then(|v| v.as_str()).unwrap_or("?"),
            d.get("merged").and_then(|v| v.as_array()).map(Vec::len).unwrap_or(0),
            d.get("skipped").and_then(|v| v.as_array()).map(Vec::len).unwrap_or(0),
            if d.get("dry_run").and_then(|v| v.as_bool()).unwrap_or(false) { " (dry run)" } else { "" }
        ),
        "arbiter_decision" => format!(
            "{}: {}",
            d.get("files").cloned().unwrap_or_default(),
            if d.get("accepted").and_then(|v| v.as_bool()).unwrap_or(false) { "accepted" } else { "rejected" }
        ),
        "teardown" => format!(
            "force={} keep_branch={}",
            d.get("force").and_then(|v| v.as_bool()).unwrap_or(false),
            d.get("keep_branch").and_then(|v| v.as_bool()).unwrap_or(false)
        ),
        "conflict_resolve" => {
            let target = d.get("target_branch").and_then(|v| v.as_str()).unwrap_or("?");
            if d.get("abandoned").and_then(|v| v.as_bool()).unwrap_or(false) {
                format!("-> {target}: abandoned")
            } else {
                let resolved = d.get("resolved").and_then(|v| v.as_bool()).unwrap_or(false);
                format!("-> {target}: {}", if resolved { "resolved" } else { "still conflicted" })
            }
        }
        _ => d.to_string(),
    }
}

/// Prints a `pact coord-status` snapshot -- see DESIGN.md ("pact-coord >
/// Coord status") for why this exists (issue #64: the coordination layer
/// was otherwise a black box from the outside).
fn print_coord_status(status: &pact_coord::CoordStatus) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    if status.active_leases.is_empty() {
        println!("no active leases");
    } else {
        println!("active leases:");
        for lease in &status.active_leases {
            let remaining = (lease.expires_at - now).max(0);
            println!("  '{}' held by {} (expires in {}s)", lease.pattern, lease.holder, remaining);
        }
    }

    let with_pending: Vec<_> = status.pending_messages.iter().filter(|p| p.pending > 0).collect();
    if with_pending.is_empty() {
        println!("no pending messages");
    } else {
        println!("pending messages:");
        for agent in with_pending {
            println!("  {}: {} unread", agent.agent_id, agent.pending);
        }
    }
}

/// Parses one `--task` value into `(agent, task text, agent display name)`
/// -- see DESIGN.md ("pact-cli") for the prefix-vs-`--agent`-default
/// precedence rules.
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

/// Counts distinct tasks involved in *any* predicted overlap, not the sum
/// of each overlapping token's group size -- issue #59: two tasks sharing
/// 3 overlapping file mentions previously printed "6 of your tasks" (3
/// tokens x 2 tasks each), not the actual 2.
fn distinct_overlapping_task_count(overlaps: &[PredictedOverlap]) -> usize {
    overlaps
        .iter()
        .flat_map(|o| o.task_indices.iter().copied())
        .collect::<std::collections::HashSet<usize>>()
        .len()
}

struct DoctorCheck {
    label: &'static str,
    program: &'static str,
    args: &'static [&'static str],
}

const AGENT_CHECKS: &[DoctorCheck] = &[
    DoctorCheck { label: "claude", program: "claude", args: &["--version"] },
    DoctorCheck { label: "copilot", program: "copilot", args: &["--version"] },
    DoctorCheck { label: "codex", program: "codex", args: &["--version"] },
    DoctorCheck { label: "gemini", program: "gemini", args: &["--version"] },
];

// `go`'s version flag is a subcommand (`go version`), not `--version` --
// verified by hand, `go --version` fails with "flag provided but not
// defined: -version".
const PACKAGE_MANAGER_CHECKS: &[DoctorCheck] = &[
    DoctorCheck { label: "npm", program: "npm", args: &["--version"] },
    DoctorCheck { label: "pnpm", program: "pnpm", args: &["--version"] },
    DoctorCheck { label: "yarn", program: "yarn", args: &["--version"] },
    DoctorCheck { label: "bun", program: "bun", args: &["--version"] },
    DoctorCheck { label: "uv", program: "uv", args: &["--version"] },
    DoctorCheck { label: "poetry", program: "poetry", args: &["--version"] },
    DoctorCheck { label: "pipenv", program: "pipenv", args: &["--version"] },
    DoctorCheck { label: "pip", program: "pip", args: &["--version"] },
    DoctorCheck { label: "cargo", program: "cargo", args: &["--version"] },
    DoctorCheck { label: "go", program: "go", args: &["version"] },
    DoctorCheck { label: "mvn", program: "mvn", args: &["--version"] },
    DoctorCheck { label: "gradle", program: "gradle", args: &["--version"] },
];

/// Runs `check` and returns its first output line trimmed, or `None` if
/// the program isn't on `PATH`/failed to report a version -- see DESIGN.md
/// ("pact-cli > `pact doctor` (issue #18)").
fn doctor_check_version(check: &DoctorCheck) -> Option<String> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let output = pact_deps::run_shimmed(check.program, check.args, &cwd).ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let line = stdout.lines().next().unwrap_or("").trim();
    if !line.is_empty() {
        return Some(line.to_string());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let line = stderr.lines().next().unwrap_or("").trim();
    if line.is_empty() {
        None
    } else {
        Some(line.to_string())
    }
}

fn print_doctor_group(title: &str, checks: &[DoctorCheck]) {
    println!("{title}:");
    for check in checks {
        match doctor_check_version(check) {
            Some(version) => println!("  {}: found, {version}", check.label),
            None => println!("  {}: not found", check.label),
        }
    }
}

/// `git worktree` (which every workspace depends on) needs git >= 2.5 --
/// parses the leading `X.Y` out of `git version X.Y.Z...` and treats
/// anything unparseable as "can't confirm", not a hard failure, since a
/// git that responds to `--version` at all is almost certainly new enough
/// in practice.
fn git_version_supports_worktree(version_line: &str) -> Option<bool> {
    let version = version_line.strip_prefix("git version ")?;
    let mut parts = version.split('.');
    let major: u32 = parts.next()?.parse().ok()?;
    let minor: u32 = parts.next()?.parse().ok()?;
    Some((major, minor) >= (2, 5))
}

fn run_doctor() -> Result<()> {
    let git_check = DoctorCheck { label: "git", program: "git", args: &["--version"] };
    let git_version = doctor_check_version(&git_check);

    let git_ok = match &git_version {
        Some(version) => {
            let worktree_ok = git_version_supports_worktree(version).unwrap_or(true);
            println!(
                "git: found, {version}{}",
                if worktree_ok { " (worktree supported)" } else { " (too old for `git worktree` -- need >= 2.5)" }
            );
            worktree_ok
        }
        None => {
            println!("git: not found -- required, pact can't create any workspace without it");
            false
        }
    };

    println!();
    print_doctor_group("agent CLIs", AGENT_CHECKS);
    println!();
    print_doctor_group("package managers", PACKAGE_MANAGER_CHECKS);

    if !git_ok {
        std::process::exit(1);
    }
    Ok(())
}

fn print_spawn_preview(preview: &pact_core::SpawnPreview) {
    println!("would create workspace {} ({})", preview.workspace_id, preview.branch);
    println!("  path: {}", preview.path.display());
    if preview.package_managers.is_empty() {
        println!("  package managers: none detected");
    } else {
        let names: Vec<&str> = preview.package_managers.iter().map(|pm| package_manager_label(*pm)).collect();
        println!("  package managers: {}", names.join(", "));
    }
    println!("  command: {} {}", preview.program, preview.args.join(" "));
}

fn package_manager_label(pm: pact_deps::PackageManager) -> &'static str {
    match pm {
        pact_deps::PackageManager::Bun => "bun",
        pact_deps::PackageManager::Pnpm => "pnpm",
        pact_deps::PackageManager::Yarn => "yarn",
        pact_deps::PackageManager::Npm => "npm",
        pact_deps::PackageManager::Uv => "uv",
        pact_deps::PackageManager::Poetry => "poetry",
        pact_deps::PackageManager::Pipenv => "pipenv",
        pact_deps::PackageManager::PipPlain => "pip",
        pact_deps::PackageManager::Cargo => "cargo",
        pact_deps::PackageManager::GoModules => "go modules",
        pact_deps::PackageManager::Maven => "maven",
        pact_deps::PackageManager::Gradle => "gradle",
    }
}

fn agent_label(kind: AgentKind) -> &'static str {
    match kind {
        AgentKind::Claude => "claude",
        AgentKind::Copilot => "copilot",
        AgentKind::Codex => "codex",
        AgentKind::Gemini => "gemini",
    }
}

/// Raw `type` values that are real but uninteresting to a human watching
/// the stream live -- see DESIGN.md ("pact-cli > streamed event filtering")
/// for the measurements that justified suppressing these two specifically.
/// Suppressed only from the live terminal view by default; `--verbose`
/// restores them, and the full unfiltered stream is always in the
/// workspace's log file regardless (`run_and_stream` writes every raw line
/// there before any filtering).
const SUPPRESSED_OTHER_EVENT_TYPES: &[&str] = &[
    "session.background_tasks_changed",
    "tool.execution_partial_result",
    // Per-token deltas streamed while a tool call's arguments are being
    // generated -- confirmed the dominant source of remaining noise
    // (issue #58): one real spawn-many log was 75%+ this single type,
    // ~679 of 900 total lines. Same category as the other two: the
    // information is already fully present in the final tool-call event,
    // so the deltas are redundant for a human watching the stream live.
    "assistant.tool_call_delta",
    // Session/skills metadata, not agent output -- confirmed noise
    // (issue #80): reproduces on any spawn, even a trivial one-turn task,
    // as 500-1000 byte raw JSON blobs that say nothing about what the
    // agent is doing.
    "session.skills_loaded",
    // The first four entries above were all found via Copilot CLI
    // shakedowns -- these two are Claude Code's own, confirmed by hand
    // during a real spawn-many run (issue #100). Account rate-limit
    // metadata, not agent output.
    "rate_limit_event",
    // In headless mode there's no real interactive user turn, so every
    // `"type":"user"` event observed is the SDK echoing a tool result back
    // to itself -- duplicates what the `[tool]`/`[assistant]` events
    // already surfaced (issue #100).
    "user",
];

/// Whether an `AgentEvent::Other`'s raw JSON should be printed -- `false`
/// only when it's a known-noisy type (see `SUPPRESSED_OTHER_EVENT_TYPES`)
/// and `--verbose` wasn't passed. Anything not on that list still prints
/// unconditionally: an unrecognized event is far more likely to be a real
/// message an adapter doesn't parse in detail yet than something safe to
/// drop silently, so only specifically-confirmed noise is ever suppressed.
fn should_print_other(value: &serde_json::Value, verbose: bool) -> bool {
    if verbose {
        return true;
    }
    match value.get("type").and_then(serde_json::Value::as_str) {
        Some(t) => !SUPPRESSED_OTHER_EVENT_TYPES.contains(&t),
        None => true,
    }
}

/// Same event formatting as `print_event`, prefixed with `label` so N
/// interleaved concurrent agents' output stays attributable -- see
/// DESIGN.md ("pact-cli") for the line-granularity locking note.
fn print_event_labeled(label: &str, event: &AgentEvent, verbose: bool) {
    match event {
        AgentEvent::Init { session_id } => println!("[{label}] [init] session {session_id}"),
        AgentEvent::CoordStatus { name, status } => {
            println!("[{label}] [coord] {name}: {status}")
        }
        AgentEvent::AssistantText(text) => println!("[{label}] [assistant] {text}"),
        AgentEvent::ToolUse { name, input } => println!("[{label}] [tool] {name} {input}"),
        AgentEvent::Result { .. } => {}
        AgentEvent::Other(value) if should_print_other(value, verbose) => {
            println!("[{label}] [other] {value}")
        }
        AgentEvent::Other(_) => {}
    }
}

/// Prints one streamed agent event -- see DESIGN.md ("pact-cli") for why
/// `Other` is not skipped by default except for the short, confirmed list
/// of known-noisy raw types (`should_print_other`), suppressed unless
/// `--verbose` is passed.
fn print_event(event: &AgentEvent, verbose: bool) {
    match event {
        AgentEvent::Init { session_id } => println!("[init] session {session_id}"),
        AgentEvent::CoordStatus { name, status } => println!("[coord] {name}: {status}"),
        AgentEvent::AssistantText(text) => println!("[assistant] {text}"),
        AgentEvent::ToolUse { name, input } => println!("[tool] {name} {input}"),
        AgentEvent::Result { .. } => {} // surfaced by the caller as the final outcome instead
        AgentEvent::Other(value) if should_print_other(value, verbose) => println!("[other] {value}"),
        AgentEvent::Other(_) => {}
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
    fn git_version_supports_worktree_true_for_a_recent_version() {
        assert_eq!(git_version_supports_worktree("git version 2.46.0.windows.1"), Some(true));
    }

    #[test]
    fn git_version_supports_worktree_false_for_a_too_old_version() {
        assert_eq!(git_version_supports_worktree("git version 2.4.9"), Some(false));
    }

    #[test]
    fn git_version_supports_worktree_true_exactly_at_the_minimum() {
        assert_eq!(git_version_supports_worktree("git version 2.5.0"), Some(true));
    }

    #[test]
    fn git_version_supports_worktree_none_for_unparseable_input() {
        assert_eq!(git_version_supports_worktree("not a git version string"), None);
    }

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

    #[test]
    fn should_print_other_suppresses_known_noisy_types_by_default() {
        let value = serde_json::json!({"type": "session.background_tasks_changed", "data": {}});
        assert!(!should_print_other(&value, false));

        let value = serde_json::json!({"type": "tool.execution_partial_result", "data": {}});
        assert!(!should_print_other(&value, false));

        let value = serde_json::json!({"type": "assistant.tool_call_delta", "data": {}});
        assert!(!should_print_other(&value, false));

        let value = serde_json::json!({"type": "session.skills_loaded", "data": {}});
        assert!(!should_print_other(&value, false));

        let value = serde_json::json!({"type": "rate_limit_event", "data": {}});
        assert!(!should_print_other(&value, false));

        let value = serde_json::json!({"type": "user", "message": {}});
        assert!(!should_print_other(&value, false));
    }

    #[test]
    fn should_print_other_shows_known_noisy_types_when_verbose() {
        let value = serde_json::json!({"type": "session.background_tasks_changed", "data": {}});
        assert!(should_print_other(&value, true));
    }

    #[test]
    fn should_print_other_prints_unrecognized_types_by_default() {
        // Only specifically-confirmed noise is ever suppressed -- anything
        // else stays visible even without --verbose, since it's more
        // likely to be a real message this adapter doesn't parse yet.
        let value = serde_json::json!({"type": "assistant.some_new_event", "data": {}});
        assert!(should_print_other(&value, false));

        let value = serde_json::json!({"no_type_field": true});
        assert!(should_print_other(&value, false));
    }

    #[test]
    fn distinct_overlapping_task_count_matches_the_simple_two_task_case() {
        let overlaps = vec![PredictedOverlap { token: "src/index.js".to_string(), task_indices: vec![0, 1] }];
        assert_eq!(distinct_overlapping_task_count(&overlaps), 2);
    }

    #[test]
    fn distinct_overlapping_task_count_does_not_double_count_shared_tasks() {
        // Same 2 tasks (0 and 1) overlapping on 3 different tokens must
        // still report 2 distinct tasks, not 3 x 2 = 6.
        let overlaps = vec![
            PredictedOverlap { token: "src/index.js".to_string(), task_indices: vec![0, 1] },
            PredictedOverlap { token: "test/index.test.js".to_string(), task_indices: vec![0, 1] },
            PredictedOverlap { token: "package.json".to_string(), task_indices: vec![0, 1] },
        ];
        assert_eq!(distinct_overlapping_task_count(&overlaps), 2);
    }

    #[test]
    fn distinct_overlapping_task_count_unions_across_non_identical_task_sets() {
        // Tasks 0/1 overlap on one file, tasks 1/2 overlap on another --
        // task 1 must not be counted twice.
        let overlaps = vec![
            PredictedOverlap { token: "src/index.js".to_string(), task_indices: vec![0, 1] },
            PredictedOverlap { token: "src/other.js".to_string(), task_indices: vec![1, 2] },
        ];
        assert_eq!(distinct_overlapping_task_count(&overlaps), 3);
    }
}
