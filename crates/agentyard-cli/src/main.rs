use std::path::{Path, PathBuf};

use anyhow::{bail, Result};
use clap::{Parser, Subcommand};

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
    /// Create a new isolated agent workspace
    Spawn {
        /// Short description of the task this workspace is for
        task: String,
    },
    /// List active agent workspaces
    List,
    /// Tear down an agent workspace
    Teardown {
        /// Workspace id (as shown by `list`)
        id: String,
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
    let orchestrator = Orchestrator::open(repo_root)?;

    match cli.command {
        Command::Spawn { task } => {
            let workspace = orchestrator.spawn(&task)?;
            println!("created workspace {}", workspace.id);
            println!("  path:   {}", workspace.path.display());
            println!("  branch: {}", workspace.branch);
            println!("  task:   {}", workspace.task);
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
    }

    Ok(())
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
