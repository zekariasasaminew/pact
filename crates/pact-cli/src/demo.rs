use std::path::Path;
use std::process::Command;

use anyhow::{bail, Context, Result};
use pact_vcs::WorkspaceManager;

/// A canned, zero-cost walkthrough of the core loop for a brand-new user:
/// isolated worktrees, independent work landing in parallel, one clean
/// merged branch at the end. Runs against a real, disposable temp repo
/// using pact's real `WorkspaceManager`/`merge_all` -- everything except
/// the agent CLI call itself is real, since that's the one step that
/// costs money and needs a real installed/authenticated agent, neither of
/// which a fresh install can assume. See DESIGN.md ("pact-cli > `pact demo`
/// (issue #119)") for why simulating just that one step was the deliberate
/// choice, not an oversight.
struct DemoTask {
    task: &'static str,
    file_name: &'static str,
    file_contents: &'static str,
}

const DEMO_TASKS: &[DemoTask] = &[
    DemoTask {
        task: "Add a friendly greeting helper",
        file_name: "greeting.py",
        file_contents: "def greet(name):\n    return f\"Hello, {name}!\"\n",
    },
    DemoTask {
        task: "Add a simple calculator helper",
        file_name: "calculator.py",
        file_contents: "def add(a, b):\n    return a + b\n",
    },
];

pub fn run() -> Result<()> {
    let repo_root = std::env::temp_dir().join(format!("pact-demo-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&repo_root).context("failed to create a temp directory for the demo repo")?;

    println!("pact demo -- a temporary, disposable repo, deleted automatically when this finishes.");
    println!("  {}", repo_root.display());
    println!();

    let result = run_inner(&repo_root);

    println!();
    println!("cleaning up the demo repo...");
    let _ = std::fs::remove_dir_all(&repo_root);
    // `run_inner` opens a real WorkspaceManager, which creates its state
    // directory as a *sibling* of repo_root (`.pact-<repo-name>`), not
    // inside it -- removing repo_root alone left this orphaned every time
    // (found in an outside R4 regression report, 2026-07-29, issue #195).
    if let Ok(state_dir) = WorkspaceManager::state_dir_for(&repo_root) {
        let _ = std::fs::remove_dir_all(&state_dir);
    }

    result
}

fn run_inner(repo_root: &Path) -> Result<()> {
    init_git_repo(repo_root)?;

    println!(
        "Simulating {} agents working in parallel, each on its own file.",
        DEMO_TASKS.len()
    );
    println!(
        "(No real agent CLI or API call happens here -- that's the one step \
         this demo fakes, since it's the one that costs money and needs a real \
         installed/authenticated agent. See the end of this output for how to \
         run the real thing.)"
    );
    println!();

    let workspaces = WorkspaceManager::open(repo_root)?;
    let mut created = Vec::new();
    for demo_task in DEMO_TASKS {
        let workspace = workspaces.create_workspace(demo_task.task, None)?;
        std::fs::write(workspace.path.join(demo_task.file_name), demo_task.file_contents).with_context(|| {
            format!("failed to write {} into workspace {}", demo_task.file_name, workspace.id)
        })?;
        println!("workspace {} ({})", workspace.id, workspace.branch);
        println!("  task: {}", demo_task.task);
        println!("  simulated agent output: wrote {}", demo_task.file_name);
        created.push(workspace);
    }

    println!();
    println!("pact list:");
    for workspace in &created {
        println!("  {}  {}  [dirty]", workspace.id, workspace.branch);
    }

    println!();
    println!("running merge-all...");
    let report = workspaces.merge_all(None, None, &[], None, None, None, false)?;
    crate::print_merge_report(&report);

    println!();
    println!(
        "That's the core loop: isolated worktrees so parallel work never \
         collides, merged back automatically onto one clean branch."
    );
    println!();
    println!("Try it for real, in your own repo:");
    println!("  pact init");
    println!("  pact spawn --agent claude \"<task>\"");
    println!("  pact spawn-many --task claude:\"<task 1>\" --task claude:\"<task 2>\"");
    println!("  pact merge-all");
    println!();
    println!("See examples/tasks/ for task-text patterns, GETTING_STARTED.md for the full walkthrough.");

    Ok(())
}

fn init_git_repo(repo_root: &Path) -> Result<()> {
    run_git(repo_root, &["init", "-q"])?;
    run_git(repo_root, &["config", "user.email", "demo@pact.local"])?;
    run_git(repo_root, &["config", "user.name", "pact demo"])?;
    std::fs::write(repo_root.join("README.md"), "# pact demo\n\nA disposable repo created by `pact demo`.\n")
        .context("failed to write the demo repo's README.md")?;
    run_git(repo_root, &["add", "-A"])?;
    run_git(repo_root, &["commit", "-q", "-m", "init"])?;
    Ok(())
}

fn run_git(dir: &Path, args: &[&str]) -> Result<()> {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .with_context(|| format!("failed to spawn `git {}`", args.join(" ")))?;
    if !output.status.success() {
        bail!("`git {}` failed: {}", args.join(" "), String::from_utf8_lossy(&output.stderr));
    }
    Ok(())
}
