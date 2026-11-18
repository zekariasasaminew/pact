use std::path::Path;
use std::process::Command;

use anyhow::{bail, Context, Result};
use pact_vcs::WorkspaceManager;

/// A canned, zero-cost walkthrough of what `pact run` does, for a
/// brand-new user (issue #417): a plan of units that own disjoint files,
/// lanes writing into one shared tree, one commit, and a check run before
/// and after so its verdict knows the baseline. Runs against a real,
/// disposable temp repo with pact's real `WorkspaceManager`; only the
/// planner and the agents are simulated, since those are the steps that
/// cost money and need an installed, signed-in agent. See DESIGN.md
/// ("pact-cli > `pact demo` (issue #119)") for why simulating just those
/// steps was the deliberate choice.
struct DemoTask {
    unit: &'static str,
    file_name: &'static str,
    file_contents: &'static str,
}

const DEMO_TASK: &str = "Add a greeting helper and a calculator helper";

const DEMO_TASKS: &[DemoTask] = &[
    DemoTask {
        unit: "greeting",
        file_name: "greeting.py",
        file_contents: "def greet(name):\n    return f\"Hello, {name}!\"\n",
    },
    DemoTask {
        unit: "calculator",
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

    println!("pact run \"{DEMO_TASK}\"");
    println!(
        "(The planner and the agents are simulated: they are the steps that cost money and \
         need a signed-in agent CLI. Everything else below is pact's real code on a real repo. \
         See the end of this output for how to run the real thing.)"
    );
    println!();

    println!("plan: {} units, each owning its own files", DEMO_TASKS.len());
    for demo_task in DEMO_TASKS {
        println!("  {}: {}", demo_task.unit, demo_task.file_name);
    }
    let both_exist = |dir: &Path| DEMO_TASKS.iter().all(|t| dir.join(t.file_name).exists());
    println!("verify: both files exist");
    println!();

    let workspaces = WorkspaceManager::open(repo_root)?;
    let batch = workspaces.create_shared_batch(DEMO_TASK, None)?;
    println!("shared tree {} ({})", batch.id, batch.branch);
    let baseline = both_exist(&batch.path);
    println!("  baseline on the untouched tree: {}", if baseline { "passes" } else { "fails" });
    for demo_task in DEMO_TASKS {
        let lane = workspaces.create_lane(&batch, demo_task.unit, Some(demo_task.unit))?;
        std::fs::write(lane.path.join(demo_task.file_name), demo_task.file_contents)
            .with_context(|| format!("failed to write {} for unit {}", demo_task.file_name, demo_task.unit))?;
        println!("  lane {}: simulated agent wrote {}", lane.id, demo_task.file_name);
    }
    let committed = workspaces.commit_all(&batch.id)?;
    println!("  {}", if committed { "committed the shared tree once" } else { "nothing to commit" });
    let after = both_exist(&batch.path);
    let verdict = match (baseline, after) {
        (false, true) => "passed (it failed on the base commit, so this run fixed it)",
        (true, true) => "passed",
        (true, false) => "FAILED (it passed on the base commit, so this run broke it)",
        (false, false) => "INCONCLUSIVE (it already fails on the base commit)",
    };
    println!("  verify: {verdict}");

    println!();
    println!(
        "That's pact run: units that own disjoint files run as parallel lanes in one tree, \
         with no merge step, and the result is checked against the base commit."
    );
    println!();
    println!("Try it for real, in your own repo:");
    println!("  pact doctor");
    println!("  pact run --agent copilot --dry-run \"<task>\"     (plans and writes the briefs, runs nothing)");
    println!("  pact run --agent copilot --verify \"npm test\" \"<task>\"");
    println!("Unrelated tasks, possibly in other repos: pact fleet tasks.toml, or pact ui for a browser page.");
    println!("Tasks that must edit the same files: pact spawn-many, then pact merge-all.");
    println!();
    println!("GETTING_STARTED.md has the full walkthrough.");

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
