use std::path::{Path, PathBuf};

use anyhow::{bail, Result};
use clap::{Parser, Subcommand};

use pact_agents::{AgentEvent, AgentKind};
use pact_core::{Orchestrator, SpawnManyTask};

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
    /// Create a new isolated agent workspace and run an agent CLI in it
    Spawn {
        /// Task/prompt to give the agent
        task: String,

        /// Which agent CLI to launch (claude, copilot, codex).
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
    },
    /// Create N isolated agent workspaces and run N agent CLIs in them
    /// concurrently, streaming their combined output live with each line
    /// attributed to its source. Existing single-`spawn` behavior and CLI
    /// surface are unchanged -- this is an entirely separate command, not
    /// an alternate mode of `spawn`.
    SpawnMany {
        /// One task per agent to run, repeatable: `--task <agent>:<text>`,
        /// e.g. `--task claude:"fix the bug" --task claude:"write tests"`
        /// (N instances of the same agent) or `--task copilot:"..."` mixed
        /// in (different agents), whichever the caller wants. Split on the
        /// first `:` only, so task text itself may contain colons.
        #[arg(long = "task", required = true)]
        tasks: Vec<String>,

        /// Same raw safety/approval override as `spawn --safety`, applied
        /// to every task in this batch -- see `spawn`'s help for the
        /// per-adapter vocabulary. Per-task safety overrides aren't
        /// supported in this first cut (see `pact-core::SpawnManyTask`).
        #[arg(long)]
        safety: Option<String>,
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
        } => {
            let kind = AgentKind::parse(&agent).ok_or_else(|| {
                anyhow::anyhow!("unknown --agent '{agent}' (expected claude, copilot, or codex)")
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

            let (workspace, outcome) = orchestrator.spawn(kind, &task, safety.as_deref(), print_event)?;

            println!("workspace {} ({})", workspace.id, workspace.branch);
            println!("  path: {}", workspace.path.display());
            println!(
                "  {}: {}",
                if outcome.success { "done" } else { "failed" },
                outcome.summary
            );
        }
        Command::SpawnMany { tasks, safety } => {
            let specs = tasks
                .iter()
                .map(|raw| parse_task_spec(raw))
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

            let results = orchestrator.spawn_many(batch, safety.as_deref(), |index, agent, event| {
                print_event_labeled(&format!("{}:{index}", agent_label(*agent)), event);
            });

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
        Command::Teardown {
            id,
            keep_branch,
            force,
        } => {
            orchestrator.teardown(&id, keep_branch, force)?;
            println!("removed workspace {id}");
        }
        Command::McpServe { .. } => unreachable!("handled above, before the orchestrator opens"),
    }

    Ok(())
}

/// Parses one `--task <agent>:<text>` argument, splitting on the *first*
/// `:` only so task text itself may freely contain colons (e.g.
/// `claude:implement X: handle the edge case`). Returns the parsed
/// `AgentKind`, the raw task text, and the original agent name (for
/// warning messages, which want the user's own spelling).
fn parse_task_spec(raw: &str) -> Result<(AgentKind, String, String)> {
    let (agent_name, task) = raw.split_once(':').ok_or_else(|| {
        anyhow::anyhow!("--task '{raw}' must be in the form <agent>:<task text>, e.g. claude:\"fix the bug\"")
    })?;
    if task.trim().is_empty() {
        bail!("--task '{raw}' has empty task text after the ':'");
    }
    let kind = AgentKind::parse(agent_name).ok_or_else(|| {
        anyhow::anyhow!(
            "unknown agent '{agent_name}' in --task '{raw}' (expected claude, copilot, or codex)"
        )
    })?;
    Ok((kind, task.to_string(), agent_name.to_string()))
}

fn agent_label(kind: AgentKind) -> &'static str {
    match kind {
        AgentKind::Claude => "claude",
        AgentKind::Copilot => "copilot",
        AgentKind::Codex => "codex",
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
