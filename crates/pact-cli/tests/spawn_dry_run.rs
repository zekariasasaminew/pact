//! Integration coverage for issue #16: `spawn`/`spawn-many --dry-run` must
//! preview a workspace id/branch/path, detected package manager(s), and the
//! exact command that would be launched, without creating a workspace,
//! running dependency prep, or launching an agent. Drives the real `pact`
//! binary end-to-end against a throwaway repo, same technique
//! `merge_all_exit_code.rs` uses -- no agent CLI involved, since `--agent
//! claude`'s `build_command` never actually spawns `claude` itself.
use std::path::{Path, PathBuf};
use std::process::Command;

use uuid::Uuid;

fn run_git(dir: &Path, args: &[&str]) {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap_or_else(|err| panic!("failed to spawn `git {}`: {err}", args.join(" ")));
    assert!(
        output.status.success(),
        "`git {}` failed:\n{}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn init_repo() -> PathBuf {
    let root = std::env::temp_dir().join(format!("pact-cli-spawn-dry-run-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    run_git(&root, &["init", "-q"]);
    run_git(&root, &["config", "user.email", "test@test.com"]);
    run_git(&root, &["config", "user.name", "test"]);
    std::fs::write(root.join("package.json"), "{}\n").unwrap();
    run_git(&root, &["add", "-A"]);
    run_git(&root, &["commit", "-q", "-m", "init"]);
    root
}

fn state_dir(repo: &Path) -> PathBuf {
    let repo_name = repo.file_name().unwrap().to_str().unwrap();
    repo.parent().unwrap().join(format!(".pact-{repo_name}"))
}

fn cleanup(root: &Path) {
    let _ = std::fs::remove_dir_all(root);
    let _ = std::fs::remove_dir_all(state_dir(root));
}

#[test]
fn spawn_dry_run_prints_a_preview_and_creates_no_workspace() {
    let repo = init_repo();

    let output = Command::new(env!("CARGO_BIN_EXE_pact"))
        .args(["--repo", repo.to_str().unwrap(), "spawn", "do the thing", "--dry-run"])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "expected exit 0, got {:?}\nstdout: {}\nstderr: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("would create workspace"), "expected a workspace preview line, got: {stdout}");
    assert!(stdout.contains("npm"), "expected the detected npm package manager, got: {stdout}");
    assert!(stdout.contains("claude"), "expected the command preview to name the agent binary, got: {stdout}");
    assert!(stdout.contains("do the thing"), "expected the command preview to include the task text, got: {stdout}");

    let workspaces_dir = state_dir(&repo).join("workspaces");
    let has_workspace = std::fs::read_dir(&workspaces_dir).map(|mut d| d.next().is_some()).unwrap_or(false);
    assert!(!has_workspace, "--dry-run must not create a real workspace, found one under {}", workspaces_dir.display());

    let mcp_dir = state_dir(&repo).join("mcp");
    let has_mcp_config = std::fs::read_dir(&mcp_dir).map(|mut d| d.next().is_some()).unwrap_or(false);
    assert!(!has_mcp_config, "--dry-run must not leave a stray MCP config file under {}", mcp_dir.display());

    let worktrees = Command::new("git").args(["worktree", "list"]).current_dir(&repo).output().unwrap();
    let worktree_count = String::from_utf8_lossy(&worktrees.stdout).lines().count();
    assert_eq!(worktree_count, 1, "--dry-run must not add a git worktree, only the repo's own checkout should be listed");

    cleanup(&repo);
}

#[test]
fn spawn_many_dry_run_previews_every_task_and_creates_no_workspace() {
    let repo = init_repo();

    let output = Command::new(env!("CARGO_BIN_EXE_pact"))
        .args([
            "--repo",
            repo.to_str().unwrap(),
            "spawn-many",
            "--agent",
            "claude",
            "--task",
            "task one",
            "--task",
            "copilot:task two",
            "--dry-run",
        ])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "expected exit 0, got {:?}\nstdout: {}\nstderr: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("task #0 (claude)"), "expected task #0's preview labeled claude, got: {stdout}");
    assert!(stdout.contains("task #1 (copilot)"), "expected task #1's preview labeled copilot, got: {stdout}");
    assert_eq!(
        stdout.matches("would create workspace").count(),
        2,
        "expected one preview per task, got: {stdout}"
    );

    let workspaces_dir = state_dir(&repo).join("workspaces");
    let has_workspace = std::fs::read_dir(&workspaces_dir).map(|mut d| d.next().is_some()).unwrap_or(false);
    assert!(!has_workspace, "--dry-run must not create a real workspace, found one under {}", workspaces_dir.display());

    cleanup(&repo);
}
