use std::path::{Path, PathBuf};

use anyhow::{bail, Result};
use clap::{Parser, Subcommand};

use pact_agents::{AgentEvent, AgentKind};
use pact_core::Orchestrator;

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

        /// Which agent CLI to launch (claude, copilot, codex). Codex's
        /// adapter is implemented from documentation only and has not
        /// been live-tested -- see the README.
        #[arg(long, default_value = "claude")]
        agent: String,

        /// Raw safety/approval override passed straight through to the
        /// chosen agent's own vocabulary: Claude Code's --permission-mode
        /// values (acceptEdits, bypassPermissions, ...), Codex's
        /// --ask-for-approval values (never, on-request, untrusted).
        /// Ignored by Copilot CLI, which has no gradient. Defaults to
        /// each adapter's own unattended-safety setting -- see the README
        /// for why headless mode requires *some* such setting regardless
        /// of adapter, not just for Claude Code.
        #[arg(long)]
        safety: Option<String>,
    },
    /// List active agent workspaces
    List,
    /// Tear down an agent workspace
    Teardown {
        /// Workspace id (as shown by `list`)
        id: String,

        /// Don't delete the pact/<id> branch -- keep it around to inspect
        /// or rebase the workspace's commits after tearing it down.
        #[arg(long)]
        keep_branch: bool,
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
                    "warning: running '{agent}' with its default unattended-safety setting \
                     ({}) -- it bypasses safety checks with no human in the loop. Pass --safety \
                     explicitly to use a different setting.",
                    adapter.default_safety_description()
                ),
            }

            let (workspace, outcome) =
                orchestrator.spawn(kind, &task, safety.as_deref(), |event| print_event(event))?;

            println!("workspace {} ({})", workspace.id, workspace.branch);
            println!("  path: {}", workspace.path.display());
            println!(
                "  {}: {}",
                if outcome.success { "done" } else { "failed" },
                outcome.summary
            );
        }
        Command::List => {
            let workspaces = orchestrator.list()?;
            if workspaces.is_empty() {
                println!("no active workspaces");
            }
            for workspace in workspaces {
                println!(
                    "{}  {}  {}",
                    workspace.id,
                    workspace.branch,
                    workspace.path.display()
                );
                println!("    task: {}", workspace.task);
            }
        }
        Command::Teardown { id, keep_branch } => {
            orchestrator.teardown(&id, keep_branch)?;
            println!("removed workspace {id}");
        }
        Command::McpServe { .. } => unreachable!("handled above, before the orchestrator opens"),
    }

    Ok(())
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
