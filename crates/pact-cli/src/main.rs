use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};

use pact_agents::{AgentEvent, AgentKind};
use pact_core::{
    CoordServerOverride, FileConflict, MergeReport, Orchestrator, PredictedOverlap, SpawnManyReport, SpawnManyTask,
};

mod config;
mod demo;
use config::PactConfig;

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
    /// Write a pact.toml at the repo root with defaults for --agent/--safety,
    /// detected from what's actually installed (same detection `pact doctor`
    /// uses). Entirely optional -- pact works fully without one, this just
    /// removes the "which flags do I need" step for repeat use. A CLI flag
    /// always overrides whatever pact.toml sets.
    Init {
        /// Overwrite an existing pact.toml instead of refusing.
        #[arg(long)]
        force: bool,
    },
    /// A zero-decision, zero-cost walkthrough of the core loop: creates a
    /// disposable temp repo, runs two simulated agents on independent
    /// files, merges them, then deletes the temp repo. No real agent CLI
    /// or API call is made -- that's the one step this fakes, since it's
    /// the one that costs money and needs a real installed/authenticated
    /// agent. Works from anywhere, doesn't need to be run inside a git repo.
    Demo,
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
        /// Falls back to `pact.toml`'s `defaults.agent` if omitted, then to
        /// the sole detected installed agent CLI if exactly one is found,
        /// then to `claude` as a last resort.
        #[arg(long)]
        agent: Option<String>,

        /// Safety/approval override for the chosen agent. Accepts three
        /// portable profile names, resolved per-adapter (aliases over the
        /// raw vocabulary below, not a replacement for it): "strict" (each
        /// adapter's most restrictive real mode), "workspace-write"
        /// (pact's own existing default -- for Codex/Gemini this is
        /// already the only mode confirmed to complete real headless work,
        /// so there is no safer alias to offer instead), "unrestricted"
        /// (each adapter's full-bypass mode). Any other value is passed
        /// straight through raw, unchanged, to the chosen agent's own
        /// vocabulary: Claude Code's --permission-mode values (acceptEdits,
        /// bypassPermissions, plan, ...), Codex's --sandbox values
        /// (read-only, workspace-write, danger-full-access). Ignored by
        /// Copilot CLI, which has no gradient -- all three profile names
        /// and every raw value resolve to the same thing for it. Claude
        /// Code's "plan" mode (and "strict" on it) is read-only for the
        /// target repo, but its own plan-document feature may still write
        /// a real file to the host user's ~/.claude/plans/, outside this
        /// workspace's isolation and pact's own cleanup -- confirmed by
        /// hand, see DESIGN.md. Falls back to `pact.toml`'s
        /// `defaults.safety` if omitted.
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

        /// Skip dependency prep entirely for this task -- no package
        /// manager detection, no shared content-store lookup, no install.
        /// For tasks that don't need dependencies at all (e.g. editing a
        /// version string in a manifest file) -- issue #233: prep used to
        /// run unconditionally even when a task explicitly said not to
        /// touch dependencies, paying its full cost for nothing.
        #[arg(long)]
        no_deps: bool,

        /// Explicit workspace name -- drives the workspace id/branch
        /// directly (slugified, e.g. "Add Pagination" -> "add-pagination")
        /// instead of the default task-text-slug-plus-random-suffix
        /// scheme. Issue #234: without this, `--dry-run`'s previewed id
        /// and the real run's id can never agree (the random suffix is
        /// regenerated on every call), and a shared task preamble across
        /// a batch collapses every workspace to the same unreadable
        /// prefix. Must contain at least one ASCII alphanumeric character.
        #[arg(long)]
        name: Option<String>,
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
        /// also given. Falls back to `pact.toml`'s `defaults.agent`, then
        /// to the sole detected installed agent CLI if exactly one is
        /// found; at least one of --agent, `pact.toml`, detection, or a
        /// per-task prefix is required for every task.
        #[arg(long)]
        agent: Option<String>,

        /// Same raw safety/approval override as `spawn --safety`, applied
        /// to every task in this batch -- see `spawn`'s help for the
        /// per-adapter vocabulary. Per-task safety overrides aren't
        /// supported in this first cut (see `pact-core::SpawnManyTask`).
        /// Falls back to `pact.toml`'s `defaults.safety` if omitted.
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

        /// After the dry-run preview, print an estimated input-token count
        /// and a dollar range per adapter, using each task's raw text
        /// (before any spawn ever happens). Requires --dry-run. Output
        /// tokens, file-read tokens, and tool-call overhead aren't
        /// estimated -- see the printed notes for why. Issue #222.
        #[arg(long, requires = "dry_run")]
        estimate_cost: bool,

        /// Same as `spawn --no-deps`, applied to every task in this
        /// batch -- skips dependency prep entirely, for a batch whose
        /// tasks don't touch dependencies at all. Issue #233: a batch of
        /// pure manifest-text-edit tasks used to pay full dependency-prep
        /// cost (content-store contention included) for zero benefit.
        #[arg(long)]
        no_deps: bool,

        /// Explicit workspace name for the Nth --task, repeatable in the
        /// same order as --task -- same fix as `spawn --name` (issue
        /// #234), applied per task. Either give one --name per --task, or
        /// omit entirely for the default naming scheme on every task;
        /// partial naming (fewer --name than --task) is rejected rather
        /// than guessing which task each name belongs to.
        #[arg(long = "name")]
        names: Vec<String>,
    },
    /// List active agent workspaces
    List,
    /// One-screen repo-wide view: detected agent CLIs, an aggregate
    /// coordination snapshot (active leases/pending messages), open
    /// persisted conflicts, and every active workspace's dirty/agent-
    /// liveness/files-touched state -- what `list`/`coord-status`/
    /// `resolve --list`/`doctor` would otherwise take four separate
    /// commands to piece together. Read-only, computes nothing new (issue
    /// #221).
    Status {
        /// Print the full snapshot as JSON instead of a human-readable
        /// screen.
        #[arg(long)]
        json: bool,
    },
    /// Show what an agent has done in a workspace: committed changes on
    /// its branch (relative to where it forked from) and anything still
    /// only in its working tree.
    Diff {
        /// Workspace id (as shown by `list`)
        id: String,
    },
    /// Everything pact currently knows about one workspace, in one place:
    /// metadata, dirty status, agent pid liveness, committed/uncommitted
    /// diff, the dependency-prep report and run metadata recorded at
    /// spawn time (if this workspace went through a real spawn), this
    /// workspace's own active coordination leases and pending messages,
    /// its operation history, and any open persisted conflict. Read-only
    /// -- combines existing data sources (`list`/`diff`/`coord-status`/
    /// `history`/`resolve`'s own lookup), doesn't compute anything new.
    Inspect {
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
        /// set twice, non-JS/TS languages -- is checked. Use only for files
        /// where line order is not semantically important and combining
        /// unique lines is safe -- genuinely append-only, order-independent
        /// content: logs, CHANGELOG entries, ignore files. Do not use for
        /// source files unless you understand the limitations above.
        /// `--union` is accepted as an alias for backward compatibility,
        /// but `--append-only` is the more accurate name for what this
        /// actually does -- "union" implies something more general/safer
        /// than a naive line concat.
        #[arg(long = "append-only", visible_alias = "union")]
        append_only: Vec<String>,

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
        /// is set. Falls back to `pact.toml`'s `defaults.agent` if
        /// omitted, then to `claude` if neither is set.
        #[arg(long = "arbiter-agent")]
        arbiter_agent: Option<String>,

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

        /// Same as `merge-all --append-only` (`--union` also accepted),
        /// applied to this retry.
        #[arg(long = "append-only", visible_alias = "union")]
        append_only: Vec<String>,

        /// Same as `merge-all --test-cmd` -- enables the Arbiter fallback
        /// for this retry specifically.
        #[arg(long = "test-cmd")]
        test_cmd: Option<String>,

        /// Same as `merge-all --arbiter-agent`. Ignored unless --test-cmd
        /// is set.
        #[arg(long = "arbiter-agent")]
        arbiter_agent: Option<String>,

        /// Same as `merge-all --arbiter-safety`. Ignored unless
        /// --test-cmd is set.
        #[arg(long = "arbiter-safety")]
        arbiter_safety: Option<String>,
    },
    /// Tear down an agent workspace. Without an id, tears down every active
    /// workspace -- each one independently, so one that fails (e.g. dirty
    /// without --force) is reported and the rest still proceed, rather than
    /// aborting the whole batch (same "report and continue" shape as
    /// `commit-all`).
    Teardown {
        /// Only tear down this workspace (as shown by `list`), instead of
        /// every active workspace.
        id: Option<String>,

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
    Doctor {
        /// Print machine-readable JSON instead of the human-readable
        /// report -- for CI diagnostics and bug reports.
        #[arg(long)]
        json: bool,
    },
    /// Inspect and clean the shared npm dependency content store
    /// (`.pact-<repo>/store/npm`) -- see DESIGN.md ("pact-deps > npm
    /// store manifest, verification, cleanup", issue #160). Only npm has
    /// a shared content store today; other package managers use their
    /// own existing global caches.
    #[command(subcommand)]
    Store(StoreCommand),
}

#[derive(Subcommand)]
enum StoreCommand {
    /// List every store entry that has a manifest -- key, age, node/npm
    /// version, and size.
    List,
    /// Check that a store entry's file count and total byte size still
    /// match what its manifest recorded. Not a byte-for-byte content
    /// hash -- see the manifest's own doc comment for why a mismatch
    /// here is still a reliable corruption signal. Verifies every entry
    /// with a manifest if `key` is omitted.
    Verify {
        key: Option<String>,
    },
    /// Removes store entries -- never a workspace already materialized
    /// from one, only the shared entry itself, so nothing currently in
    /// use is affected. Requires either `--older-than` or `--all`.
    Clean {
        /// Remove entries whose `last_used_at` is older than this many
        /// days.
        #[arg(long)]
        older_than_days: Option<u64>,
        /// Remove every entry with a manifest, regardless of age.
        #[arg(long)]
        all: bool,
        /// List what would be removed without actually removing it.
        #[arg(long)]
        dry_run: bool,
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
    let verbose = cli.verbose;

    if let Command::Completions { shell } = cli.command {
        clap_complete::generate(shell, &mut <Cli as clap::CommandFactory>::command(), "pact", &mut std::io::stdout());
        return Ok(());
    }

    if let Command::Doctor { json } = cli.command {
        return run_doctor(json);
    }

    if let Command::Demo = cli.command {
        return demo::run();
    }

    let repo_root = match cli.repo {
        Some(p) => p,
        None => find_repo_root(&std::env::current_dir()?)?,
    };

    if let Command::Init { force } = cli.command {
        return run_init(&repo_root, force);
    }

    let config = PactConfig::load(&repo_root)?;

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
            no_deps,
            name,
        } => {
            if let Some(n) = &name {
                validate_workspace_name(n)?;
            }
            let agent = resolve_default_agent(agent, &config).unwrap_or_else(|| "claude".to_string());
            let safety = safety.or_else(|| config.default_safety().map(str::to_string));
            let kind = AgentKind::parse(&agent).ok_or_else(|| {
                anyhow::anyhow!(
                    "unknown --agent '{agent}' (expected claude, copilot, codex, or gemini) -- \
                     try: pact doctor"
                )
            })?;
            let adapter = pact_agents::adapter(kind);
            let coord_override = coord_command.map(|command| CoordServerOverride {
                command,
                args: coord_args,
            });

            if dry_run {
                let preview =
                    orchestrator.spawn_preview(kind, &task, name.as_deref(), safety.as_deref(), coord_override.as_ref())?;
                print_spawn_preview(&preview);
                return Ok(());
            }

            match &safety {
                Some(s) => eprintln!(
                    "warning: running '{agent}' with an explicit safety override ({}) -- \
                     verify this doesn't hang the session on a permission prompt in headless mode.",
                    describe_resolved_safety(kind, s)
                ),
                None => eprintln!(
                    "warning: running '{agent}' unattended with no human in the loop, using: {}. \
                     Pass --safety explicitly to use a different setting.",
                    adapter.default_safety_description()
                ),
            }

            let spawn_options = pact_core::SpawnOptions {
                safety_override: safety.as_deref(),
                coord_override: coord_override.as_ref(),
                no_deps,
            };
            let (workspace, outcome) = orchestrator.spawn(kind, &task, name.as_deref(), &spawn_options, |event| {
                print_event(event, verbose)
            })?;

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
            estimate_cost,
            no_deps,
            names,
        } => {
            if !names.is_empty() && names.len() != tasks.len() {
                bail!(
                    "--name given {} time(s) but --task given {} time(s) -- give exactly one \
                     --name per --task (in the same order), or omit --name entirely for the \
                     default naming scheme on every task",
                    names.len(),
                    tasks.len()
                );
            }
            for n in &names {
                validate_workspace_name(n)?;
            }
            if let Some(dup) = first_duplicate(&names) {
                bail!("--name '{dup}' was given more than once -- workspace names must be unique within one spawn-many batch");
            }
            let agent = resolve_default_agent(agent, &config);
            let safety = safety.or_else(|| config.default_safety().map(str::to_string));
            let default_agent = agent
                .as_deref()
                .map(|name| {
                    AgentKind::parse(name)
                        .map(|kind| (kind, name))
                        .ok_or_else(|| {
                            anyhow::anyhow!(
                                "unknown --agent '{name}' (expected claude, copilot, codex, or gemini) -- \
                                 try: pact doctor"
                            )
                        })
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
                            "warning: running '{agent_name}' with an explicit safety override ({}) -- \
                             verify this doesn't hang the session on a permission prompt in headless mode.",
                            describe_resolved_safety(kind, s)
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
                .enumerate()
                .map(|(index, (agent, task, _))| SpawnManyTask {
                    agent,
                    task,
                    name: names.get(index).cloned(),
                })
                .collect();

            let overlaps = pact_core::predict_task_overlap(&batch);
            if !overlaps.is_empty() {
                eprintln!(
                    "warning: {} of your tasks mention the same file(s) in their task text \
                     -- this is only a prompt-text heuristic, not real conflict detection (no \
                     files have been claimed yet, and it can both miss real overlaps and flag \
                     read-only mentions). Consider separating that work:",
                    distinct_overlapping_task_count(&overlaps)
                );
                for overlap in &overlaps {
                    let indices: Vec<String> = overlap.task_indices.iter().map(|i| i.to_string()).collect();
                    eprintln!("  possible overlap: '{}' mentioned by tasks #{}", overlap.token, indices.join(", #"));
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
                        task.name.as_deref(),
                        safety.as_deref(),
                        coord_override.as_ref(),
                    )?;
                    println!("task #{index} ({}):", agent_label(task.agent));
                    print_spawn_preview(&preview);
                }
                if estimate_cost {
                    print_cost_estimate(&batch);
                }
                return Ok(());
            }

            let requested = batch.len();
            let spawn_options = pact_core::SpawnOptions {
                safety_override: safety.as_deref(),
                coord_override: coord_override.as_ref(),
                no_deps,
            };
            let results = orchestrator.spawn_many(batch, &spawn_options, |index, agent, event| {
                print_event_labeled(&format!("{}:{index}", agent_label(*agent)), event, verbose);
            });

            let report = SpawnManyReport::new(requested, results);
            for outcome in &report.outcomes {
                match &outcome.result {
                    Ok((workspace, run)) => {
                        println!("workspace {} ({})", workspace.id, workspace.branch);
                        println!("  path: {}", workspace.path.display());
                        println!(
                            "  {}: {}",
                            if run.success { "done" } else { "failed" },
                            run.summary
                        );
                    }
                    Err(err) => {
                        println!(
                            "task #{} ({}): FAILED before/during launch -- {err:#}",
                            outcome.index,
                            agent_label(outcome.agent)
                        );
                    }
                }
            }
            // Printed unconditionally -- issue #231: "everything worked"
            // must be as visible and greppable as "something failed", and
            // exiting via this report's own exit_code() (not an ad hoc
            // bool tracked alongside the loop above) makes it structurally
            // impossible to report 0 while requested != created.
            println!("{}", report.summary_line());
            let exit_code = report.exit_code();
            if exit_code != 0 {
                std::process::exit(exit_code);
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
                // Distinguishes "clean because the run genuinely touched
                // nothing" from plain "clean" (which also covers the
                // normal post-commit/merge case) -- issue #212, outside
                // report: a task that should have failed loudly instead
                // reported success with zero files touched, and pact's
                // own [clean] display looked identical to a legitimate
                // read-only task. Informational only, not a fail signal --
                // a genuinely read-only task also touches zero files.
                let no_files_touched = dirty == "clean"
                    && orchestrator
                        .run_metadata(&workspace.id)
                        .is_some_and(|meta| meta.exit_success && !meta.files_touched);
                let suffix = if no_files_touched { ", no files touched" } else { "" };
                println!(
                    "{}  {}  {}  [{dirty}{suffix}]",
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
        Command::Status { json } => {
            run_status(&orchestrator, json)?;
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
        Command::Inspect { id } => {
            print_inspect(&orchestrator, &id)?;
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
            let active_workspace_ids: Vec<String> = orchestrator.list()?.into_iter().map(|w| w.id).collect();
            print_coord_status(&status, &active_workspace_ids);
        }
        Command::Store(store_command) => {
            let store = orchestrator.npm_store()?;
            run_store_command(&store, store_command)?;
        }
        Command::History { workspace, since, op_type, limit, json } => {
            let filter = pact_coord::HistoryFilter { workspace_id: workspace, since, op_type, limit };
            let operations = orchestrator.history(&filter)?;
            print_history(&operations, json);
        }
        Command::MergeAll { ids, into, dry_run, append_only, test_cmd, arbiter_agent, arbiter_safety, require_passing_tests } => {
            let ids = if ids.is_empty() { None } else { Some(ids) };
            let arbiter_agent = arbiter_agent
                .or_else(|| config.default_agent().map(str::to_string))
                .unwrap_or_else(|| "claude".to_string());
            let arbiter_safety = arbiter_safety.or_else(|| config.default_safety().map(str::to_string));
            let arbiter = build_arbiter_config(test_cmd, &arbiter_agent, arbiter_safety)?;
            let report = orchestrator.merge_all(
                ids.as_deref(),
                into.as_deref(),
                &append_only,
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
        Command::Resolve { workspace, abandon, append_only, test_cmd, arbiter_agent, arbiter_safety } => {
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

            let arbiter_agent = arbiter_agent
                .or_else(|| config.default_agent().map(str::to_string))
                .unwrap_or_else(|| "claude".to_string());
            let arbiter_safety = arbiter_safety.or_else(|| config.default_safety().map(str::to_string));
            let arbiter = build_arbiter_config(test_cmd, &arbiter_agent, arbiter_safety)?;
            let resolution = orchestrator.resolve_conflict(&workspace_id, &append_only, arbiter.as_ref())?;
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
            let ids: Vec<String> = match id {
                Some(id) => vec![id],
                None => orchestrator.list()?.into_iter().map(|w| w.id).collect(),
            };
            if ids.is_empty() {
                println!("no active workspaces");
                return Ok(());
            }

            let all_conflicts = orchestrator.detect_conflicts().ok();
            let mut any_failed = false;
            for id in ids {
                // Computed *before* removal -- workspace_changes needs the
                // branch, which teardown deletes. Informational only: this
                // never blocks the teardown itself, only warns.
                if let Some(all) = &all_conflicts {
                    let relevant: Vec<FileConflict> =
                        all.iter().filter(|c| c.workspace_ids.iter().any(|w| w == &id)).cloned().collect();
                    if !relevant.is_empty() {
                        eprintln!("warning: workspace {id} shares changes with another active workspace:");
                        print_conflicts(&relevant);
                    }
                }

                match orchestrator.teardown(&id, keep_branch, force) {
                    Ok(()) => println!("removed workspace {id}"),
                    Err(err) => {
                        println!("{id}: failed to tear down: {err:#}");
                        any_failed = true;
                    }
                }
            }
            if any_failed {
                std::process::exit(1);
            }
        }
        Command::McpServe { .. } => unreachable!("handled above, before the orchestrator opens"),
        Command::Completions { .. } => unreachable!("handled above, before the orchestrator opens"),
        Command::Doctor { .. } => unreachable!("handled above, before the orchestrator opens"),
        Command::Init { .. } => unreachable!("handled above, before the orchestrator opens"),
        Command::Demo => unreachable!("handled above, before the orchestrator opens"),
    }

    Ok(())
}

/// Prints a `merge-all` report -- the merge order/outcome for a real run,
/// or just the planned order for `--dry-run` (see `MergeReport::dry_run`).
fn print_merge_report(report: &MergeReport) {
    if report.dry_run {
        println!("dry run: would merge onto '{}' from {}", report.target_branch, short(&report.base_commit));
        println!("  planned order (risk score -- changed-file count plus a penalty for central files/lockfiles, see DESIGN.md):");
        for workspace in &report.planned {
            println!("    {} (risk: {})", workspace.id, workspace.risk_score);
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
        anyhow::anyhow!(
            "unknown --arbiter-agent '{arbiter_agent}' (expected claude, copilot, codex, or gemini) -- \
             try: pact doctor"
        )
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
/// Everything pact knows about one workspace, aggregated from existing
/// data sources -- see DESIGN.md ("pact-cli > `pact inspect`", issue
/// #16). Read-only, computes nothing new.
fn print_inspect(orchestrator: &Orchestrator, id: &str) -> Result<()> {
    let workspace = orchestrator.get_workspace(id)?;
    println!("workspace {} ({})", workspace.id, workspace.branch);
    println!("  path: {}", workspace.path.display());
    println!("  task: {}", workspace.task);
    println!("  base commit: {}", short(&workspace.base_commit));
    match orchestrator.is_dirty(id) {
        Ok(true) => println!("  status: dirty"),
        Ok(false) => println!("  status: clean"),
        Err(_) => println!("  status: unknown (workspace directory may be gone)"),
    }
    match workspace.agent_pid {
        Some(pid) => {
            let status = if pact_core::agent_process_alive(pid) { "running" } else { "not running" };
            println!("  agent pid: {pid} ({status})");
        }
        None => println!("  agent pid: none recorded"),
    }

    println!();
    println!("dependency prep:");
    match orchestrator.dependency_prep_report(id) {
        Some(reports) if reports.is_empty() => println!("  no package managers detected"),
        Some(reports) => {
            for report in reports {
                let outcome = if report.success { "ok" } else { "failed" };
                print!("  {} via {} [{outcome}]", report.manager, report.strategy);
                if let Some(hit) = report.store_hit {
                    print!(", store {}", if hit { "hit" } else { "populated" });
                }
                if let Some(mode) = &report.materialization {
                    print!(", materialized via {mode}");
                }
                println!();
                for warning in &report.warnings {
                    println!("    warning: {warning}");
                }
            }
        }
        None => println!("  no record (workspace wasn't spawned in this session, or the record is missing)"),
    }

    println!();
    println!("last run:");
    match orchestrator.run_metadata(id) {
        Some(run) => {
            let duration = run.ended_at.saturating_sub(run.started_at);
            println!("  agent: {}", run.agent);
            println!("  command: {} {}", run.program, run.args.join(" "));
            println!("  {} in {duration}s: {}", if run.exit_success { "succeeded" } else { "failed" }, run.summary);
            if run.exit_success && !run.files_touched {
                println!(
                    "  note: reported success but touched zero files -- could be a legitimate \
                     read-only task, or a task that silently didn't do what it was asked (issue #212)"
                );
            }
            println!("  coordination: {}", run.coord_status.as_deref().unwrap_or("no coordination config attached"));
            println!("  log: {}", run.log_path.display());
        }
        None => println!("  no record (workspace wasn't spawned in this session, or the record is missing)"),
    }

    println!();
    println!("coordination:");
    let coord = orchestrator.coord_status()?;
    let own_leases: Vec<_> = coord.active_leases.iter().filter(|l| l.holder == id).collect();
    if own_leases.is_empty() {
        println!("  no active leases held by this workspace");
    } else {
        for lease in own_leases {
            println!("  claims '{}' (expires {})", lease.pattern, lease.expires_at);
        }
    }
    let own_pending = coord.pending_messages.iter().find(|p| p.agent_id == id).map(|p| p.pending).unwrap_or(0);
    println!("  {own_pending} unread message(s)");

    println!();
    match orchestrator.open_conflict_for(id) {
        Ok(Some(conflict)) => {
            println!("open conflict: against '{}', files: {}", conflict.target_branch, conflict.files.join(", "));
        }
        Ok(None) => println!("no open conflict"),
        Err(err) => println!("conflict lookup failed: {err:#}"),
    }

    println!();
    let filter = pact_coord::HistoryFilter {
        workspace_id: Some(id.to_string()),
        since: None,
        op_type: None,
        limit: Some(20),
    };
    let operations = orchestrator.history(&filter)?;
    println!("recent history (last {}):", operations.len());
    if operations.is_empty() {
        println!("  none recorded");
    } else {
        for op in &operations {
            println!("  [{}] {}", op.op_type, op.detail);
        }
    }

    Ok(())
}

fn print_coord_status(status: &pact_coord::CoordStatus, active_workspace_ids: &[String]) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    // Issue #235: "no active leases" alone is indistinguishable from "no
    // agent's MCP client ever connected to the coord server at all" --
    // both look identical, and only one of them means the coordination
    // layer is actually working. never_connected is computed from real
    // active workspaces (pact list), not just the coord DB, so an
    // already-torn-down workspace's silence doesn't get flagged here.
    let never_connected: Vec<&String> =
        active_workspace_ids.iter().filter(|id| !status.connected_agent_ids.contains(*id)).collect();

    if status.active_leases.is_empty() {
        if never_connected.is_empty() {
            println!("no active leases");
        } else {
            println!("no active leases (and no leases are possible -- see below)");
        }
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

    if !never_connected.is_empty() {
        println!(
            "warning: {} active workspace(s) never connected to the coordination server -- \
             claim_files/release_files/send_message/check_messages were never reachable for \
             them, not just unused:",
            never_connected.len()
        );
        for id in &never_connected {
            println!("  {id}");
        }
    }
}

/// One workspace's row in `pact status` -- combines `Workspace`, dirty
/// status, agent liveness, the `files_touched` annotation (issue #212),
/// and this workspace's own coordination claims/pending messages. See
/// DESIGN.md ("pact-cli > `pact status` (issue #221)").
struct StatusWorkspaceRow {
    workspace: pact_vcs::Workspace,
    dirty: Option<bool>,
    agent_alive: Option<bool>,
    no_files_touched: bool,
    pending_messages: i64,
    held_claims: Vec<String>,
    /// Whether this workspace's coord server ever completed an MCP
    /// `initialize` handshake -- issue #235.
    coord_connected: bool,
}

fn build_status_rows(orchestrator: &Orchestrator, coord: &pact_coord::CoordStatus) -> Result<Vec<StatusWorkspaceRow>> {
    let workspaces = orchestrator.list()?;
    Ok(workspaces
        .into_iter()
        .map(|workspace| {
            let dirty = orchestrator.is_dirty(&workspace.id).ok();
            let agent_alive = workspace.agent_pid.map(pact_core::agent_process_alive);
            let no_files_touched = dirty == Some(false)
                && orchestrator
                    .run_metadata(&workspace.id)
                    .is_some_and(|meta| meta.exit_success && !meta.files_touched);
            let pending_messages = coord
                .pending_messages
                .iter()
                .find(|p| p.agent_id == workspace.id)
                .map(|p| p.pending)
                .unwrap_or(0);
            let held_claims =
                coord.active_leases.iter().filter(|l| l.holder == workspace.id).map(|l| l.pattern.clone()).collect();
            let coord_connected = coord.connected_agent_ids.contains(&workspace.id);
            StatusWorkspaceRow {
                workspace,
                dirty,
                agent_alive,
                no_files_touched,
                pending_messages,
                held_claims,
                coord_connected,
            }
        })
        .collect())
}

/// Pattern-matched "what next" hints over `pact status`'s own snapshot --
/// static rules, no fuzzy logic, same shape as the existing error-message
/// hint chain (issue #123).
fn status_hints(rows: &[StatusWorkspaceRow], open_conflicts: &[pact_coord::PersistedConflict]) -> Vec<String> {
    let mut hints = Vec::new();
    let running = rows.iter().filter(|r| r.agent_alive == Some(true)).count();
    let zero_files: Vec<&str> = rows.iter().filter(|r| r.no_files_touched).map(|r| r.workspace.id.as_str()).collect();
    // Only a *finished* agent that never connected is worth flagging --
    // a still-running one may just not have launched mcp-serve yet.
    let never_connected: Vec<&str> = rows
        .iter()
        .filter(|r| !r.coord_connected && r.agent_alive != Some(true))
        .map(|r| r.workspace.id.as_str())
        .collect();

    if running > 0 {
        hints.push(format!("{running} workspace(s) still running -- wait, or `pact list` for details"));
    }
    if let Some(first) = zero_files.first() {
        hints.push(format!("{} workspace(s) touched zero files -- inspect: pact inspect {first}", zero_files.len()));
    }
    if !never_connected.is_empty() {
        hints.push(format!(
            "{} workspace(s) finished without ever connecting to the coordination server -- \
             file leases/messages were never reachable for them: {}",
            never_connected.len(),
            never_connected.join(", ")
        ));
    }
    if !open_conflicts.is_empty() {
        hints.push(format!("{} open conflict(s) -- pact resolve --list", open_conflicts.len()));
    }
    if !rows.is_empty() && running == 0 && open_conflicts.is_empty() {
        hints.push("all workspaces idle, no open conflicts -- consider `pact merge-all` when ready".to_string());
    }
    hints
}

/// `pact status` (issue #221) -- one screen aggregating `list`/
/// `coord-status`/`resolve --list`/`doctor`'s data, computing nothing new.
fn run_status(orchestrator: &Orchestrator, json: bool) -> Result<()> {
    let coord = orchestrator.coord_status()?;
    let open_conflicts = orchestrator.open_conflicts()?;
    let rows = build_status_rows(orchestrator, &coord)?;
    let hints = status_hints(&rows, &open_conflicts);
    let agents: Vec<(&str, Option<String>)> =
        AGENT_CHECKS.iter().map(|check| (check.label, doctor_check_version(check))).collect();

    if json {
        let workspaces_json: Vec<serde_json::Value> = rows
            .iter()
            .map(|row| {
                serde_json::json!({
                    "id": row.workspace.id,
                    "branch": row.workspace.branch,
                    "path": row.workspace.path,
                    "task": row.workspace.task,
                    "dirty": row.dirty,
                    "agent_pid": row.workspace.agent_pid,
                    "agent_alive": row.agent_alive,
                    "no_files_touched": row.no_files_touched,
                    "pending_messages": row.pending_messages,
                    "held_claims": row.held_claims,
                    "coord_connected": row.coord_connected,
                    "run_metadata": orchestrator.run_metadata(&row.workspace.id),
                })
            })
            .collect();
        let report = serde_json::json!({
            "agents_detected": agents.iter().map(|(name, version)| serde_json::json!({
                "name": name, "found": version.is_some(), "version": version,
            })).collect::<Vec<_>>(),
            "active_leases": coord.active_leases,
            "pending_messages": coord.pending_messages,
            "open_conflicts": open_conflicts,
            "workspaces": workspaces_json,
            "hints": hints,
        });
        println!("{}", serde_json::to_string_pretty(&report).unwrap_or_else(|e| format!("error serializing result: {e}")));
        return Ok(());
    }

    println!("agents detected:");
    for (name, version) in &agents {
        match version {
            Some(v) => println!("  {name}: {v}"),
            None => println!("  {name}: not found"),
        }
    }

    println!();
    println!(
        "coordination: {} active lease(s), {} pending message(s) across {} agent(s)",
        coord.active_leases.len(),
        coord.pending_messages.iter().map(|p| p.pending).sum::<i64>(),
        coord.pending_messages.iter().filter(|p| p.pending > 0).count()
    );
    println!("open conflicts: {}", open_conflicts.len());

    println!();
    if rows.is_empty() {
        println!("no active workspaces");
    } else {
        println!("workspaces ({} active):", rows.len());
        for row in &rows {
            let dirty_label = match row.dirty {
                Some(true) => "dirty",
                Some(false) => "clean",
                None => "unknown",
            };
            let suffix = if row.no_files_touched { ", no files touched" } else { "" };
            let agent_label = match row.agent_alive {
                Some(true) => "running",
                Some(false) => "not running",
                None => "no agent recorded",
            };
            print!("  {}  [{dirty_label}{suffix}]  {agent_label}", row.workspace.id);
            if row.pending_messages > 0 {
                print!("  {} unread", row.pending_messages);
            }
            if !row.held_claims.is_empty() {
                print!("  claims: {}", row.held_claims.join(", "));
            }
            println!();
        }
    }

    if !hints.is_empty() {
        println!();
        println!("what next:");
        for hint in &hints {
            println!("  {hint}");
        }
    }

    Ok(())
}

/// Implements every `pact store` verb (issue #160) -- see DESIGN.md
/// ("pact-deps > npm store manifest, verification, cleanup").
fn run_store_command(store: &pact_deps::ContentStore, command: StoreCommand) -> Result<()> {
    match command {
        StoreCommand::List => {
            let mut entries = store.list_entries()?;
            if entries.is_empty() {
                println!("no store entries with a manifest");
                return Ok(());
            }
            entries.sort_by_key(|e| e.key.clone());
            let now = unix_now();
            for entry in &entries {
                println!(
                    "{}  node{} npm{}  {} file(s), {}  last used {}",
                    entry.key,
                    entry.node_major,
                    entry.npm_version,
                    entry.file_count,
                    format_bytes(entry.byte_size),
                    format_age(now.saturating_sub(entry.last_used_at))
                );
            }
        }
        StoreCommand::Verify { key } => {
            let keys = match key {
                Some(key) => vec![key],
                None => store.list_entries()?.into_iter().map(|e| e.key).collect(),
            };
            if keys.is_empty() {
                println!("no store entries with a manifest to verify");
                return Ok(());
            }
            let mut any_failed = false;
            for key in keys {
                match store.verify_entry(&key) {
                    Ok(true) => println!("ok: {key}"),
                    Ok(false) => {
                        println!("MISMATCH: {key} -- file count/byte size no longer match its manifest");
                        any_failed = true;
                    }
                    Err(err) => {
                        println!("error: {key}: {err:#}");
                        any_failed = true;
                    }
                }
            }
            if any_failed {
                std::process::exit(1);
            }
        }
        StoreCommand::Clean { older_than_days, all, dry_run } => {
            if !all && older_than_days.is_none() {
                bail!("pact store clean needs either --older-than-days <N> or --all");
            }
            let now = unix_now();
            let cutoff = older_than_days.map(|days| now.saturating_sub(days * 86_400));
            let entries = store.list_entries()?;
            let to_remove: Vec<_> = entries
                .into_iter()
                .filter(|e| all || cutoff.is_some_and(|cutoff| e.last_used_at < cutoff))
                .collect();

            if to_remove.is_empty() {
                println!("nothing to clean");
                return Ok(());
            }
            for entry in &to_remove {
                if dry_run {
                    println!(
                        "would remove: {} (last used {})",
                        entry.key,
                        format_age(now.saturating_sub(entry.last_used_at))
                    );
                } else {
                    match store.remove_entry(&entry.key) {
                        Ok(()) => println!("removed: {}", entry.key),
                        Err(err) => println!("failed to remove {}: {err:#}", entry.key),
                    }
                }
            }
        }
    }
    Ok(())
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn format_bytes(bytes: u64) -> String {
    const UNITS: &[&str] = &["B", "KB", "MB", "GB"];
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} {}", UNITS[0])
    } else {
        format!("{size:.1} {}", UNITS[unit])
    }
}

fn format_age(seconds: u64) -> String {
    if seconds < 60 {
        format!("{seconds}s ago")
    } else if seconds < 3600 {
        format!("{}m ago", seconds / 60)
    } else if seconds < 86_400 {
        format!("{}h ago", seconds / 3600)
    } else {
        format!("{}d ago", seconds / 86_400)
    }
}

/// Parses one `--task` value into `(agent, task text, agent display name)`
/// -- see DESIGN.md ("pact-cli") for the prefix-vs-`--agent`-default
/// precedence rules.
/// Issue #234: `--name` drives the workspace id directly (slugified, no
/// random suffix), so it needs at least one character that survives
/// slugifying -- otherwise the caller would get a confusing empty/opaque
/// id despite explicitly asking for a readable one.
fn validate_workspace_name(name: &str) -> Result<()> {
    if !name.chars().any(|c| c.is_ascii_alphanumeric()) {
        bail!("--name '{name}' must contain at least one ASCII alphanumeric character");
    }
    Ok(())
}

fn first_duplicate(items: &[String]) -> Option<&str> {
    let mut seen = std::collections::HashSet::new();
    items.iter().find(|item| !seen.insert(item.as_str())).map(String::as_str)
}

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
                "unknown agent '{agent_name}' in --task '{raw}' (expected claude, copilot, codex, or gemini) -- \
                 try: pact doctor"
            );
        }
    }

    let Some((kind, name)) = default else {
        bail!(
            "--task '{raw}' must be in the form <agent>:<task text>, e.g. claude:\"fix the bug\" \
             (or pass --agent to set a default agent for tasks without a prefix) -- \
             try: pact doctor to see what's installed, or pact init to set a default in pact.toml"
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

fn doctor_group_json(checks: &[DoctorCheck]) -> serde_json::Value {
    serde_json::Value::Array(
        checks
            .iter()
            .map(|check| {
                let version = doctor_check_version(check);
                serde_json::json!({
                    "name": check.label,
                    "found": version.is_some(),
                    "version": version,
                })
            })
            .collect(),
    )
}

fn run_doctor(json: bool) -> Result<()> {
    let git_check = DoctorCheck { label: "git", program: "git", args: &["--version"] };
    let git_version = doctor_check_version(&git_check);
    // An unparseable-but-present version string is treated as "can't
    // confirm, assume fine" (defaults to true), same as a missing version
    // is treated as "definitely not fine" (defaults to false) -- matches
    // the human-readable branch below exactly, just computed once up
    // front so both output formats agree.
    let worktree_ok = match &git_version {
        Some(version) => git_version_supports_worktree(version).unwrap_or(true),
        None => false,
    };
    let git_ok = git_version.is_some() && worktree_ok;

    if json {
        let report = serde_json::json!({
            "git": {
                "found": git_version.is_some(),
                "version": git_version,
                "worktree_supported": worktree_ok,
            },
            "os": std::env::consts::OS,
            "arch": std::env::consts::ARCH,
            "agent_clis": doctor_group_json(AGENT_CHECKS),
            "package_managers": doctor_group_json(PACKAGE_MANAGER_CHECKS),
        });
        println!("{}", serde_json::to_string_pretty(&report).unwrap_or_else(|e| format!("error serializing result: {e}")));
        if !git_ok {
            std::process::exit(1);
        }
        return Ok(());
    }

    match &git_version {
        Some(version) => {
            println!(
                "git: found, {version}{}",
                if worktree_ok { " (worktree supported)" } else { " (too old for `git worktree` -- need >= 2.5)" }
            );
        }
        None => {
            println!("git: not found -- required, pact can't create any workspace without it");
        }
    }

    println!();
    print_doctor_group("agent CLIs", AGENT_CHECKS);
    println!();
    print_doctor_group("package managers", PACKAGE_MANAGER_CHECKS);

    if !git_ok {
        std::process::exit(1);
    }
    Ok(())
}

/// Reuses `pact doctor`'s own agent-CLI detection (`AGENT_CHECKS` +
/// `doctor_check_version`) so `pact init`'s and `resolve_default_agent`'s
/// guesses are never out of sync with what `pact doctor` reports.
fn detect_installed_agents() -> Vec<&'static str> {
    AGENT_CHECKS
        .iter()
        .filter(|check| doctor_check_version(check).is_some())
        .map(|check| check.label)
        .collect()
}

/// Resolution order for `--agent` when it's omitted: `pact.toml`'s
/// `defaults.agent`, then auto-detection if exactly one supported agent
/// CLI is installed, printed so it's never a silent guess. Returns `None`
/// only when neither resolved anything -- callers decide their own final
/// fallback (`spawn` falls back to the pre-existing hardcoded `"claude"`;
/// `spawn-many` leaves it as a real per-task error, unchanged from before
/// this existed). Zero or multiple installed CLIs both fall through to
/// `None` rather than guessing between them -- see DESIGN.md ("pact-cli >
/// `--agent` auto-default", issue #121) for why guessing there would be
/// worse than asking.
/// For the "explicit safety override" warning: shows what a `--safety`
/// profile name (`strict`/`workspace-write`/`unrestricted`) actually
/// resolves to for this specific agent, since that's not obvious from the
/// profile name alone -- a raw, non-profile value (e.g. `acceptEdits`)
/// passes through unchanged, exactly as it always has.
fn describe_resolved_safety(kind: AgentKind, raw: &str) -> String {
    match pact_agents::resolve_safety_profile(kind, Some(raw)) {
        Some(resolved) if resolved != raw => format!("{raw} -> {resolved}"),
        Some(_) => raw.to_string(),
        None => format!("{raw} -> pact's own existing default for this adapter"),
    }
}

fn resolve_default_agent(explicit: Option<String>, config: &PactConfig) -> Option<String> {
    if explicit.is_some() {
        return explicit;
    }
    if let Some(agent) = config.default_agent() {
        return Some(agent.to_string());
    }
    match detect_installed_agents().as_slice() {
        [only] => {
            eprintln!(
                "no --agent given and no pact.toml default set -- using detected agent CLI: {only} \
                 (only one installed; pass --agent or set pact.toml's defaults.agent to change)"
            );
            Some(only.to_string())
        }
        _ => None,
    }
}

fn run_init(repo_root: &Path, force: bool) -> Result<()> {
    let config_path = repo_root.join(PactConfig::FILE_NAME);
    if config_path.exists() && !force {
        bail!(
            "{} already exists -- pass --force to overwrite it",
            config_path.display()
        );
    }

    let installed = detect_installed_agents();
    let agent_line = match installed.as_slice() {
        [only] => format!("agent = \"{only}\"       # detected: only agent CLI found installed"),
        [] => "# agent = \"claude\"    # no agent CLI detected installed -- run `pact doctor`".to_string(),
        many => format!(
            "# agent = \"claude\"    # multiple agent CLIs detected ({}) -- uncomment and pick one",
            many.join(", ")
        ),
    };

    let contents = format!(
        "# pact.toml -- generated by `pact init`.\n\
         #\n\
         # Sets defaults for --agent/--safety. A pact.toml value is only used\n\
         # when the equivalent CLI flag is omitted -- passing the flag always\n\
         # wins. Applies to `spawn`/`spawn-many`'s --agent/--safety and\n\
         # merge-all/resolve's --arbiter-agent/--arbiter-safety.\n\
         \n\
         [defaults]\n\
         {agent_line}\n\
         # safety = \"acceptEdits\"  # uncomment to stop the unattended-run warning on every spawn\n"
    );

    std::fs::write(&config_path, contents)
        .with_context(|| format!("failed to write {}", config_path.display()))?;

    println!("wrote {}", config_path.display());
    match installed.as_slice() {
        [only] => println!("detected agent CLI: {only} (set as default)"),
        [] => println!("no agent CLI detected installed -- run `pact doctor` for details"),
        many => println!("detected multiple agent CLIs ({}) -- edit pact.toml to pick a default", many.join(", ")),
    }
    println!("next: pact spawn \"<task>\"");
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

/// Static per-adapter rate card for `spawn-many --dry-run --estimate-cost`
/// (issue #222) -- (cheapest model, priciest model) input/output cost per
/// million tokens in USD, verified against each provider's own published
/// pricing page as of `RATES_LAST_UPDATED_LABEL`, not guessed. A range, not
/// one number: pact has no reliable way to know which specific model/tier a
/// user's account actually resolves to.
struct AdapterRate {
    input_per_mtok_usd: (f64, f64),
    output_per_mtok_usd: (f64, f64),
    /// Quota-bounded flat-rate plans (Copilot Pro/Business) aren't
    /// token-metered at all -- estimate quota impact instead of a dollar
    /// range.
    flat_rate: bool,
    note: &'static str,
}

const RATES_LAST_UPDATED_LABEL: &str = "2026-08-03";
const RATES_LAST_UPDATED_UNIX: u64 = 1_785_715_200;
const RATES_STALE_AFTER_SECS: u64 = 90 * 24 * 60 * 60;

fn adapter_rate(kind: AgentKind) -> AdapterRate {
    match kind {
        AgentKind::Claude => AdapterRate {
            input_per_mtok_usd: (1.0, 5.0),
            output_per_mtok_usd: (5.0, 25.0),
            flat_rate: false,
            note: "Haiku 4.5 (low) to Opus 5 (high), per Anthropic's published API pricing",
        },
        AgentKind::Codex => AdapterRate {
            input_per_mtok_usd: (0.05, 5.0),
            output_per_mtok_usd: (0.40, 30.0),
            flat_rate: false,
            note: "gpt-5-nano (low) to the flagship GPT-5-class model (high), per OpenAI's published API pricing",
        },
        AgentKind::Gemini => AdapterRate {
            input_per_mtok_usd: (0.30, 1.25),
            output_per_mtok_usd: (2.50, 10.0),
            flat_rate: false,
            note: "Flash-Lite (low) to Pro (high), per Google's published Gemini API pricing",
        },
        AgentKind::Copilot => AdapterRate {
            input_per_mtok_usd: (0.0, 0.0),
            output_per_mtok_usd: (0.0, 0.0),
            flat_rate: true,
            note: "Copilot Pro/Business is a flat monthly rate -- cost is bounded by request quota, not tokens",
        },
    }
}

/// `text.chars().count() / 4` -- within ~15% of cl100k for English prose
/// and code, verified against public tiktoken benchmarks. No tokenizer
/// dependency: the estimate is already presented as a wide range, so a
/// precise count wouldn't buy back any real confidence, and `tiktoken-rs`
/// would add ~200ms to every build for a number nobody can act on more
/// precisely than the range already allows.
fn estimate_input_tokens(text: &str) -> usize {
    text.chars().count() / 4
}

/// `spawn-many --dry-run --estimate-cost` (issue #222) -- an honest,
/// input-token-only estimate per adapter, grouped so a mixed-adapter batch
/// (`claude:`/`copilot:` task prefixes) gets one range per adapter instead
/// of one misleading combined number.
fn print_cost_estimate(batch: &[pact_core::SpawnManyTask]) {
    println!();
    println!("cost estimate");

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(RATES_LAST_UPDATED_UNIX);
    let age_secs = now.saturating_sub(RATES_LAST_UPDATED_UNIX);
    if age_secs > RATES_STALE_AFTER_SECS {
        println!("  WARN: rate card is {} day(s) old -- check each adapter's current pricing page", age_secs / 86_400);
    }

    let mut by_agent: std::collections::BTreeMap<&'static str, (usize, usize)> = std::collections::BTreeMap::new();
    for task in batch {
        let entry = by_agent.entry(agent_label(task.agent)).or_insert((0, 0));
        entry.0 += 1;
        entry.1 += estimate_input_tokens(&task.task);
    }

    for (label, (task_count, input_tokens)) in by_agent {
        let kind = AgentKind::parse(label).expect("agent_label output always round-trips through AgentKind::parse");
        let rate = adapter_rate(kind);
        println!();
        println!("  adapter:       {label}  ({})", rate.note);
        println!("  tasks:         {task_count}");
        println!("  input tokens:  ~{input_tokens} (task text only; excludes files the agent will read)");
        if rate.flat_rate {
            println!("  quota impact:  {task_count} quota-eligible request(s), no per-token dollar cost");
        } else {
            let mtok = input_tokens as f64 / 1_000_000.0;
            let low = mtok * rate.input_per_mtok_usd.0;
            let high = mtok * rate.input_per_mtok_usd.1;
            println!(
                "  input cost:    ${low:.4} - ${high:.4} (input tokens only, as of {RATES_LAST_UPDATED_LABEL})"
            );
            println!(
                "  output range:  10x-100x input tokens is typical, at ${:.2}-${:.2}/MTok -- highly task-dependent",
                rate.output_per_mtok_usd.0, rate.output_per_mtok_usd.1
            );
        }
    }

    println!();
    println!(
        "  notes: excludes file-read token usage (agent-dependent) and MCP tool-call overhead. \
         An honest floor, not a total -- rerun with a narrower/wider task scope to see the range move."
    );
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
/// for a known-noisy type (see `SUPPRESSED_OTHER_EVENT_TYPES`), a
/// non-`init` `system` event, or an `assistant` event (see below), and
/// `--verbose` wasn't passed. Anything else still prints unconditionally:
/// an unrecognized event is far more likely to be a real message an
/// adapter doesn't parse in detail yet than something safe to drop
/// silently, so only specifically-confirmed noise is ever suppressed.
fn should_print_other(value: &serde_json::Value, verbose: bool) -> bool {
    if verbose {
        return true;
    }
    match value.get("type").and_then(serde_json::Value::as_str) {
        // Claude Code's `system` events with `subtype: "init"` never reach
        // here at all -- `parse_line` converts those to `AgentEvent::Init`
        // before the generic `Other` fallback. Every `system` event that
        // *does* land here is therefore, by construction, some other
        // subtype -- `thinking_tokens` (issue #102), `task_started`/
        // `task_notification` (background bash tasks), and presumably
        // more not yet observed. None of the other three adapters use
        // `"type":"system"` for anything, so this is safe across all of
        // them, not just Claude Code.
        Some("system") => false,
        // Same reasoning for `assistant`: a real text or tool-call block
        // is already surfaced as `AssistantText`/`ToolUse` before ever
        // reaching `Other` (see `parse_assistant`). The only way an
        // `assistant`-typed value lands here is a thinking-only turn
        // (extended thinking, no text/tool_use block yet) -- confirmed
        // empty `thinking` content in every capture so far (redacted by
        // the API), just a large opaque `signature` blob (issue #102).
        // No other adapter uses a bare `"type":"assistant"` discriminator.
        Some("assistant") => false,
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
    let mut levels_up: u32 = 0;
    loop {
        if dir.join(".git").exists() {
            if let Some(warning) = unexpected_repo_root_warning(&dir, dirs::home_dir().as_deref(), levels_up) {
                eprintln!("warning: {warning}");
            }
            return Ok(dir);
        }
        if !dir.pop() {
            bail!(
                "no git repository found in '{}' or any parent directory; pass --repo explicitly \
                 -- or try: pact demo, which doesn't need a real repo",
                start.display()
            );
        }
        levels_up += 1;
    }
}

/// `find_repo_root` has no upper bound other than the filesystem root -- a
/// user running `pact` from a directory nested under an *unrelated* git
/// repo (most plausibly a dotfiles-manager home-directory repo, e.g.
/// chezmoi/yadm) silently gets that repo's `.pact` state instead of an
/// error (issue #136). Never blocks (an intentional home-directory-as-repo
/// setup is plausible, just unusual) -- only warns, with `--repo` as the
/// documented way to be explicit.
const REPO_ROOT_LEVELS_UP_WORTH_A_WARNING: u32 = 4;

fn unexpected_repo_root_warning(root: &Path, home: Option<&Path>, levels_up: u32) -> Option<String> {
    if home.is_some_and(|home| home == root) {
        return Some(format!(
            "pact found a git repository at your home directory ('{}') and is about to store \
             its state there -- if this isn't the repo you meant to run pact in, pass --repo \
             explicitly.",
            root.display()
        ));
    }
    if levels_up >= REPO_ROOT_LEVELS_UP_WORTH_A_WARNING {
        return Some(format!(
            "pact found the nearest git repository {levels_up} directories above the current \
             one, at '{}' -- if this isn't the repo you meant to run pact in, pass --repo \
             explicitly.",
            root.display()
        ));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_workspace_name_accepts_a_normal_name() {
        assert!(validate_workspace_name("add-pagination").is_ok());
    }

    #[test]
    fn validate_workspace_name_rejects_a_name_with_no_alphanumeric_characters() {
        let err = validate_workspace_name("---").unwrap_err();
        assert!(err.to_string().contains("alphanumeric"), "unexpected error: {err}");
    }

    #[test]
    fn first_duplicate_finds_a_repeated_name() {
        let names = vec!["a".to_string(), "b".to_string(), "a".to_string()];
        assert_eq!(first_duplicate(&names), Some("a"));
    }

    #[test]
    fn first_duplicate_returns_none_for_all_distinct_names() {
        let names = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        assert_eq!(first_duplicate(&names), None);
    }

    fn fake_status_row(id: &str, agent_alive: Option<bool>, coord_connected: bool) -> StatusWorkspaceRow {
        StatusWorkspaceRow {
            workspace: pact_vcs::Workspace {
                id: id.to_string(),
                path: std::path::PathBuf::from(id),
                branch: format!("pact/{id}"),
                task: "fake task".to_string(),
                created_at: 0,
                agent_pid: None,
                base_commit: "deadbeef".to_string(),
            },
            dirty: Some(false),
            agent_alive,
            no_files_touched: false,
            pending_messages: 0,
            held_claims: Vec::new(),
            coord_connected,
        }
    }

    #[test]
    fn status_hints_flags_a_finished_workspace_that_never_connected() {
        let rows = vec![fake_status_row("ws-a", Some(false), false)];
        let hints = status_hints(&rows, &[]);
        assert!(
            hints.iter().any(|h| h.contains("without ever connecting") && h.contains("ws-a")),
            "expected a never-connected hint naming ws-a, got: {hints:?}"
        );
    }

    #[test]
    fn status_hints_does_not_flag_a_still_running_workspace_that_has_not_connected_yet() {
        // A running agent may just not have launched mcp-serve yet -- not
        // worth flagging until it's actually finished.
        let rows = vec![fake_status_row("ws-a", Some(true), false)];
        let hints = status_hints(&rows, &[]);
        assert!(!hints.iter().any(|h| h.contains("without ever connecting")), "got: {hints:?}");
    }

    #[test]
    fn status_hints_does_not_flag_a_workspace_that_did_connect() {
        let rows = vec![fake_status_row("ws-a", Some(false), true)];
        let hints = status_hints(&rows, &[]);
        assert!(!hints.iter().any(|h| h.contains("without ever connecting")), "got: {hints:?}");
    }

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
    fn unexpected_repo_root_warning_fires_for_the_home_directory() {
        let home = Path::new("/home/zekar");
        assert!(unexpected_repo_root_warning(home, Some(home), 0).is_some());
    }

    #[test]
    fn unexpected_repo_root_warning_fires_when_walked_up_far_enough() {
        let root = Path::new("/some/very/distant/ancestor");
        assert!(unexpected_repo_root_warning(root, Some(Path::new("/home/zekar")), 4).is_some());
    }

    #[test]
    fn unexpected_repo_root_warning_is_silent_for_a_nearby_ordinary_repo() {
        let root = Path::new("/home/zekar/projects/pact");
        assert!(unexpected_repo_root_warning(root, Some(Path::new("/home/zekar")), 1).is_none());
    }

    #[test]
    fn unexpected_repo_root_warning_is_silent_with_no_home_dir_known() {
        let root = Path::new("/some/repo");
        assert!(unexpected_repo_root_warning(root, None, 0).is_none());
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

    /// Regression test for issue #102: `system` events with a subtype
    /// other than `init` (which never reaches `should_print_other` at
    /// all -- it's consumed into `AgentEvent::Init` first), and
    /// thinking-only `assistant` turns, both leak through as raw `[other]`
    /// noise unless suppressed here.
    #[test]
    fn should_print_other_suppresses_non_init_system_events_and_thinking_only_assistant_turns() {
        let value = serde_json::json!({"type": "system", "subtype": "thinking_tokens", "estimated_tokens": 50});
        assert!(!should_print_other(&value, false));

        let value = serde_json::json!({"type": "system", "subtype": "task_started"});
        assert!(!should_print_other(&value, false));

        let value = serde_json::json!({"type": "assistant", "message": {"content": [{"type": "thinking", "thinking": ""}]}});
        assert!(!should_print_other(&value, false));
    }

    #[test]
    fn should_print_other_shows_known_noisy_types_when_verbose() {
        let value = serde_json::json!({"type": "session.background_tasks_changed", "data": {}});
        assert!(should_print_other(&value, true));

        let value = serde_json::json!({"type": "system", "subtype": "thinking_tokens"});
        assert!(should_print_other(&value, true));

        let value = serde_json::json!({"type": "assistant", "message": {"content": [{"type": "thinking", "thinking": ""}]}});
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

    #[test]
    fn estimate_input_tokens_is_roughly_chars_over_four() {
        assert_eq!(estimate_input_tokens("abcd"), 1);
        assert_eq!(estimate_input_tokens(&"a".repeat(4000)), 1000);
        assert_eq!(estimate_input_tokens(""), 0);
    }

    #[test]
    fn adapter_rate_flags_copilot_as_flat_rate_and_the_others_as_metered() {
        assert!(adapter_rate(AgentKind::Copilot).flat_rate);
        assert!(!adapter_rate(AgentKind::Claude).flat_rate);
        assert!(!adapter_rate(AgentKind::Codex).flat_rate);
        assert!(!adapter_rate(AgentKind::Gemini).flat_rate);
    }

    #[test]
    fn adapter_rate_low_tier_is_never_pricier_than_high_tier() {
        for kind in [AgentKind::Claude, AgentKind::Codex, AgentKind::Gemini] {
            let rate = adapter_rate(kind);
            assert!(rate.input_per_mtok_usd.0 <= rate.input_per_mtok_usd.1);
            assert!(rate.output_per_mtok_usd.0 <= rate.output_per_mtok_usd.1);
        }
    }
}
