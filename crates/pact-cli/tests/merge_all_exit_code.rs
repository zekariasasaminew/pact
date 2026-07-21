//! Integration coverage for `pact merge-all`'s process exit code (issue
//! #27): a partial success (one or more workspaces skipped, nothing
//! errored) must be distinguishable at the process level from a hard
//! failure, so a CI wrapper doesn't treat 50%-merged as a crash. Drives the
//! real `pact` binary end-to-end against a throwaway repo -- no agent CLI
//! involved, workspaces are created directly via `pact_vcs::WorkspaceManager`,
//! the same technique `pact-vcs`'s own `merge_all.rs` integration tests use.
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

fn init_repo() -> PathBuf {
    let root = std::env::temp_dir().join(format!("pact-cli-merge-all-exit-{}", Uuid::new_v4()));
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
}

#[test]
fn merge_all_exits_2_when_a_workspace_is_skipped_for_a_real_conflict() {
    let repo = init_repo();
    let manager = WorkspaceManager::open(&repo).unwrap();

    // Both edit the same line of the same single-line-context file --
    // reliably conflicts under git's plain 3-way merge (confirmed by hand,
    // same lesson as pact-vcs's own merge_all.rs tests).
    let a = manager.create_workspace("conflict a").unwrap();
    std::fs::write(a.path.join("a.txt"), "line1\nchange-a\n").unwrap();

    let b = manager.create_workspace("conflict b").unwrap();
    std::fs::write(b.path.join("a.txt"), "line1\nchange-b\n").unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_pact"))
        .args(["--repo", repo.to_str().unwrap(), "merge-all"])
        .output()
        .unwrap();

    assert_eq!(
        output.status.code(),
        Some(2),
        "expected exit code 2 (partial success, nothing errored) for a run that skips one \
         workspace for a real conflict -- got {:?}\nstdout: {}\nstderr: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    cleanup(&repo);
}

#[test]
fn merge_all_exits_0_when_everything_merges_cleanly() {
    let repo = init_repo();
    let manager = WorkspaceManager::open(&repo).unwrap();

    let a = manager.create_workspace("add b.txt").unwrap();
    std::fs::write(a.path.join("b.txt"), "new file\n").unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_pact"))
        .args(["--repo", repo.to_str().unwrap(), "merge-all"])
        .output()
        .unwrap();

    assert_eq!(
        output.status.code(),
        Some(0),
        "expected exit code 0 when every workspace merges cleanly -- got {:?}\nstdout: {}\nstderr: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    cleanup(&repo);
}
