use std::path::{Path, PathBuf};

use anyhow::{bail, Result};
use clap::{Parser, Subcommand};

use agentyard_agents::AgentEvent;
use agentyard_core::Orchestrator;

#[derive(Parser)]
#[command(name = "agentyard", about = "Orchestrate parallel AI coding agent workspaces")]
struct Cli {
    /// Path to the git repository to operate on (defaults to the current directory's repo root)
    #[arg(long, global = true)]
    repo: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Create a new isolated agent workspace and run Claude Code in it
    Spawn {
        /// Task/prompt to give the agent
        task: String,

        /// Permission mode for the headless session (acceptEdits, auto,
        /// bypassPermissions, manual, dontAsk, plan). Defaults to
        /// bypassPermissions, the only mode guaranteed not to hang with no
        /// TTY to answer a permission prompt -- see the README.
        #[arg(long)]
        permission_mode: Option<String>,
    },
    /// List active agent workspaces
    List,
    /// Tear down an agent workspace
    Teardown {
        /// Workspace id (as shown by `list`)
        id: String,
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
                .unwrap_or_else(|_| "agentyard=info".into()),
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
        return runtime.block_on(agentyard_coord::serve(&repo_root, agent_id, workspace));
    }

    let orchestrator = Orchestrator::open(repo_root)?;

    match cli.command {
        Command::Spawn {
            task,
            permission_mode,
        } => {
            let permission_mode = permission_mode.unwrap_or_else(|| {
                agentyard_agents::claude_code::DEFAULT_PERMISSION_MODE.to_string()
            });
            if permission_mode == agentyard_agents::claude_code::DEFAULT_PERMISSION_MODE {
                eprintln!(
                    "warning: running with --permission-mode {permission_mode} -- the agent \
                     bypasses every permission check with no human in the loop. Pass \
                     --permission-mode explicitly to use a different mode."
                );
            }

            let (workspace, outcome) =
                orchestrator.spawn(&task, &permission_mode, |event| print_event(event))?;

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
        Command::Teardown { id } => {
            orchestrator.teardown(&id)?;
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
        AgentEvent::Init { session_id, .. } => println!("[init] session {session_id}"),
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
