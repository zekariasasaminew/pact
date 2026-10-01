//! Integration coverage for issue #325: `remove_workspace` refuses to
//! force-delete a branch whose commits no other branch reaches, unless
//! `keep_branch` or `force` is set. Real repo, real git. The check must
//! stay quiet in the normal flow (a branch still at the base tip, or one
//! whose commits `merge_all` already landed on a `pact/merged-*` branch)
//! and refuse only when deleting the branch would orphan committed work.
//! See DESIGN.md ("pact-vcs > Workspace teardown").
use std::path::{Path, PathBuf};
use std::process::Command;

use pact_vcs::{GateMode, WorkspaceManager};
use uuid::Uuid;

fn run_git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap_or_else(|err| panic!("failed to spawn `git {}`: {err}", args.join(" ")));
    assert!(output.status.success(), "`git {}` failed:\n{}", args.join(" "), String::from_utf8_lossy(&output.stderr));
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn branch_exists(repo: &Path, branch: &str) -> bool {
    Command::new("git")
        .args(["rev-parse", "--verify", "--quiet", &format!("refs/heads/{branch}")])
        .current_dir(repo)
        .output()
        .unwrap()
        .status
        .success()
}

fn init_repo() -> PathBuf {
    let root = std::env::temp_dir().join(format!("pact-vcs-teardown-orphan-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    run_git(&root, &["init", "-q"]);
    run_git(&root, &["config", "user.email", "test@test.com"]);
    run_git(&root, &["config", "user.name", "test"]);
    std::fs::write(root.join("a.txt"), "line1\n").unwrap();
    run_git(&root, &["add", "-A"]);
    run_git(&root, &["commit", "-q", "-m", "init"]);
    root
}

fn cleanup(root: &Path) {
    let _ = std::fs::remove_dir_all(root);
    if let Ok(state_dir) = WorkspaceManager::state_dir_for(root) {
        let _ = std::fs::remove_dir_all(state_dir);
    }
}

/// A workspace with one committed file on its branch and nothing else
/// referencing that commit.
fn workspace_with_a_commit(manager: &WorkspaceManager, file: &str) -> pact_vcs::Workspace {
    let workspace = manager.create_workspace(&format!("add {file}"), None).unwrap();
    std::fs::write(workspace.path.join(file), "work\n").unwrap();
    assert!(manager.commit_all(&workspace.id).unwrap(), "commit_all should have committed {file}");
    workspace
}

#[test]
fn refuses_to_delete_a_branch_whose_commit_no_other_branch_reaches() {
    let repo = init_repo();
    let manager = WorkspaceManager::open(&repo).unwrap();
    let workspace = workspace_with_a_commit(&manager, "b.txt");
    let tip = run_git(&repo, &["rev-parse", &workspace.branch]);

    let err = manager.remove_workspace(&workspace.id, false, false).unwrap_err().to_string();
    assert!(err.contains("1 commit(s) reachable from no other branch"), "got: {err}");
    assert!(err.contains("--keep-branch") && err.contains("--force") && err.contains("merge-all"), "the refusal must name every way out: {err}");
    assert!(workspace.path.exists(), "a refusal must leave the worktree untouched");
    assert_eq!(run_git(&repo, &["rev-parse", &workspace.branch]), tip, "a refusal must leave the branch untouched");
    assert!(manager.get_workspace(&workspace.id).is_ok(), "a refusal must keep the workspace registered");

    cleanup(&repo);
}

#[test]
fn keep_branch_removes_the_worktree_and_leaves_the_commit_on_its_branch() {
    let repo = init_repo();
    let manager = WorkspaceManager::open(&repo).unwrap();
    let workspace = workspace_with_a_commit(&manager, "b.txt");
    let tip = run_git(&repo, &["rev-parse", &workspace.branch]);

    manager.remove_workspace(&workspace.id, true, false).unwrap();
    assert!(!workspace.path.exists(), "the worktree goes");
    assert_eq!(run_git(&repo, &["rev-parse", &workspace.branch]), tip, "the branch keeps the commit");
    assert!(run_git(&repo, &["ls-tree", "--name-only", &workspace.branch]).contains("b.txt"));

    cleanup(&repo);
}

#[test]
fn force_discards_the_branch_and_its_commit() {
    let repo = init_repo();
    let manager = WorkspaceManager::open(&repo).unwrap();
    let workspace = workspace_with_a_commit(&manager, "b.txt");

    manager.remove_workspace(&workspace.id, false, true).unwrap();
    assert!(!workspace.path.exists());
    assert!(!branch_exists(&repo, &workspace.branch), "--force is the explicit way to throw the work away");

    cleanup(&repo);
}

#[test]
fn a_branch_still_at_the_base_tip_is_deleted_without_complaint() {
    let repo = init_repo();
    let manager = WorkspaceManager::open(&repo).unwrap();
    let workspace = manager.create_workspace("do nothing", None).unwrap();

    manager.remove_workspace(&workspace.id, false, false).unwrap();
    assert!(!workspace.path.exists());
    assert!(!branch_exists(&repo, &workspace.branch), "nothing to orphan, so the branch is cleaned up as before");

    cleanup(&repo);
}

#[test]
fn a_branch_merge_all_already_landed_is_deleted_without_complaint() {
    let repo = init_repo();
    let manager = WorkspaceManager::open(&repo).unwrap();
    let a = workspace_with_a_commit(&manager, "b.txt");
    let b = workspace_with_a_commit(&manager, "c.txt");

    let report = manager.merge_all(None, None, &[], None, None, None, GateMode::Each, false).unwrap();
    assert_eq!(report.merged.len(), 2, "both land: {:?}", report.skipped);

    // `merge_all` does a real merge into `pact/merged-*`, so the workspace
    // commits are reachable from there and the normal spawn -> commit-all
    // -> merge-all -> teardown flow needs no extra flags.
    manager.remove_workspace(&a.id, false, false).unwrap();
    manager.remove_workspace(&b.id, false, false).unwrap();
    assert!(!branch_exists(&repo, &a.branch) && !branch_exists(&repo, &b.branch), "merged branches are cleaned up");
    let files = run_git(&repo, &["ls-tree", "--name-only", &report.target_branch]);
    assert!(files.contains("b.txt") && files.contains("c.txt"), "the work survives on the merged branch: {files}");

    cleanup(&repo);
}
