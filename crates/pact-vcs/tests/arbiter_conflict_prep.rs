//! Integration coverage for `WorkspaceManager::neutralize_conflict` /
//! `restore_conflict` against a real, throwaway git repo -- see DESIGN.md
//! ("pact-core > Arbiter Write-fresh redesign", issue #106). These exist
//! specifically because a real agent (confirmed by hand) refuses to
//! operate on a file git still reports as unmerged ("UU"), so Arbiter
//! needs to be able to temporarily clear that status and put it back
//! exactly if the resulting resolution isn't accepted.

use std::path::{Path, PathBuf};
use std::process::Command;

use pact_vcs::WorkspaceManager;
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

fn git_output(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap_or_else(|err| panic!("failed to spawn `git {}`: {err}", args.join(" ")));
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn cleanup(root: &Path) {
    let _ = std::fs::remove_dir_all(root);
}

/// Sets up a real conflict directly in the repo's own working tree --
/// acceptable here (unlike in pact's own runtime code) since this repo is
/// a single-test-owned throwaway fixture, never touched by anything else.
fn init_conflicted_repo() -> PathBuf {
    let root = std::env::temp_dir().join(format!("pact-vcs-arbiter-conflict-prep-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    run_git(&root, &["init", "-q"]);
    run_git(&root, &["config", "user.email", "test@test.com"]);
    run_git(&root, &["config", "user.name", "test"]);
    std::fs::write(root.join("file.txt"), "line1\nline2\nline3\n").unwrap();
    run_git(&root, &["add", "-A"]);
    run_git(&root, &["commit", "-q", "-m", "init"]);
    let base_branch = git_output(&root, &["rev-parse", "--abbrev-ref", "HEAD"]);

    run_git(&root, &["checkout", "-q", "-b", "a"]);
    std::fs::write(root.join("file.txt"), "line1\nAAA\nline3\n").unwrap();
    run_git(&root, &["commit", "-q", "-am", "a's change"]);

    run_git(&root, &["checkout", "-q", "-B", "b", &base_branch]);
    std::fs::write(root.join("file.txt"), "line1\nBBB\nline3\n").unwrap();
    run_git(&root, &["commit", "-q", "-am", "b's change"]);

    let merge = Command::new("git").args(["merge", "--no-edit", "a"]).current_dir(&root).output().unwrap();
    assert!(!merge.status.success(), "expected a real conflict between branches a and b");

    root
}

fn is_unmerged(repo: &Path, file: &str) -> bool {
    git_output(repo, &["status", "--porcelain", "--", file]).starts_with("UU")
}

#[test]
fn neutralize_conflict_clears_the_unmerged_status() {
    let repo = init_conflicted_repo();
    let manager = WorkspaceManager::open(&repo).unwrap();
    assert!(is_unmerged(&repo, "file.txt"));

    manager.neutralize_conflict(&repo, "file.txt").unwrap();
    assert!(!is_unmerged(&repo, "file.txt"), "expected neutralize_conflict to clear the UU status");
    assert_eq!(
        std::fs::read_to_string(repo.join("file.txt")).unwrap(),
        "line1\nBBB\nline3\n",
        "expected the placeholder content to be 'ours' (stage 2), not the raw conflict-marker text"
    );

    cleanup(&repo);
}

#[test]
fn restore_conflict_puts_the_original_marker_content_and_unmerged_status_back() {
    let repo = init_conflicted_repo();
    let manager = WorkspaceManager::open(&repo).unwrap();
    let original_content = std::fs::read_to_string(repo.join("file.txt")).unwrap();
    assert!(original_content.contains("<<<<<<<"));

    let snapshot = manager.neutralize_conflict(&repo, "file.txt").unwrap();
    assert!(!is_unmerged(&repo, "file.txt"));

    manager.restore_conflict(&repo, &snapshot).unwrap();
    assert!(is_unmerged(&repo, "file.txt"), "expected the file to be unmerged again after restoring");
    assert_eq!(
        std::fs::read_to_string(repo.join("file.txt")).unwrap(),
        original_content,
        "expected the exact original conflict-marker content back"
    );

    cleanup(&repo);
}

#[test]
fn conflict_stages_survive_a_neutralize_restore_round_trip() {
    let repo = init_conflicted_repo();
    let manager = WorkspaceManager::open(&repo).unwrap();

    let before = manager.conflict_stages(&repo, "file.txt").unwrap().expect("file should be conflicted");
    let snapshot = manager.neutralize_conflict(&repo, "file.txt").unwrap();
    manager.restore_conflict(&repo, &snapshot).unwrap();
    let after = manager.conflict_stages(&repo, "file.txt").unwrap().expect("file should be conflicted again");

    assert_eq!(before.ours.0, after.ours.0);
    assert_eq!(before.theirs.0, after.theirs.0);
    assert_eq!(before.base.map(|b| b.0), after.base.map(|b| b.0));

    cleanup(&repo);
}
