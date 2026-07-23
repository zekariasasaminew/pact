//! Integration coverage for `WorkspaceManager::resolve_conflict` (issue
//! #85) against a real, throwaway git repo -- see DESIGN.md ("pact-vcs >
//! Persisted conflicts (issue #85)"). Same fixture shape as
//! `merge_all.rs`'s own real-conflict test: two workspaces edit the same
//! line of a multi-line file so git's plain 3-way merge always conflicts,
//! confirmed by hand against real git first (same lesson this codebase's
//! CLAUDE.md already calls out).
use std::path::{Path, PathBuf};
use std::process::Command;

use pact_vcs::{ResolveOutcome, WorkspaceManager};
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
    let root = std::env::temp_dir().join(format!("pact-vcs-resolve-conflict-{}", Uuid::new_v4()));
    std::fs::create_dir_all(root.join("src")).unwrap();
    run_git(&root, &["init", "-q"]);
    run_git(&root, &["config", "user.email", "test@test.com"]);
    run_git(&root, &["config", "user.name", "test"]);
    std::fs::write(
        root.join("src/index.ts"),
        "export const L1 = 1;\nexport const L2 = 2;\nexport const L3 = 3;\n\
         export const L4 = 4;\nexport const L5 = 5;\n",
    )
    .unwrap();
    run_git(&root, &["add", "-A"]);
    run_git(&root, &["commit", "-q", "-m", "init"]);
    root
}

fn cleanup(root: &Path) {
    let _ = std::fs::remove_dir_all(root);
}

fn show_file(repo: &Path, rev: &str, path: &str) -> String {
    let output = Command::new("git").args(["show", &format!("{rev}:{path}")]).current_dir(repo).output().unwrap();
    assert!(output.status.success(), "git show {rev}:{path} failed: {}", String::from_utf8_lossy(&output.stderr));
    String::from_utf8(output.stdout).unwrap()
}

#[test]
fn resolve_conflict_succeeds_once_the_workspace_branch_no_longer_conflicts() {
    let repo = init_repo();
    let manager = WorkspaceManager::open(&repo).unwrap();

    let a = manager.create_workspace("bump L1 to 100").unwrap();
    std::fs::write(
        a.path.join("src/index.ts"),
        "export const L1 = 100;\nexport const L2 = 2;\nexport const L3 = 3;\n\
         export const L4 = 4;\nexport const L5 = 5;\n",
    )
    .unwrap();

    let b = manager.create_workspace("bump L1 to 200").unwrap();
    std::fs::write(
        b.path.join("src/index.ts"),
        "export const L1 = 200;\nexport const L2 = 2;\nexport const L3 = 3;\n\
         export const L4 = 4;\nexport const L5 = 5;\n",
    )
    .unwrap();

    let report = manager.merge_all(None, None, &[], None, None, false).unwrap();
    assert_eq!(report.merged.len(), 1, "expected exactly one of A/B to merge cleanly");
    assert_eq!(report.conflicted.len(), 1, "expected exactly one real conflict");

    let conflict = &report.conflicted[0];
    assert_eq!(conflict.target_branch, report.target_branch);
    assert_eq!(conflict.files, vec!["src/index.ts".to_string()]);

    let loser = manager.get_workspace(&conflict.id).unwrap();
    let winning_content = show_file(&repo, &report.target_branch, "src/index.ts");
    std::fs::write(loser.path.join("src/index.ts"), &winning_content).unwrap();
    run_git(&loser.path, &["add", "-A"]);
    run_git(&loser.path, &["commit", "-q", "-m", "resolve conflict by hand"]);

    let outcome = manager.resolve_conflict(&conflict.target_branch, &conflict.id, &[], None).unwrap();
    assert!(
        matches!(outcome, ResolveOutcome::Resolved { .. }),
        "expected the retried merge to succeed once the workspace branch no longer conflicts, got {outcome:?}"
    );

    let final_content = show_file(&repo, &report.target_branch, "src/index.ts");
    assert_eq!(final_content, winning_content, "target_branch's tip must reflect the successful resolve");

    let current_branch =
        String::from_utf8(Command::new("git").args(["branch", "--show-current"]).current_dir(&repo).output().unwrap().stdout).unwrap();
    assert!(
        !current_branch.trim().is_empty() && current_branch.trim() != report.target_branch,
        "resolve_conflict must not check out the target branch in the repo's own working tree"
    );

    cleanup(&repo);
}

#[test]
fn resolve_conflict_reports_still_conflicted_when_nothing_changed() {
    let repo = init_repo();
    let manager = WorkspaceManager::open(&repo).unwrap();

    let a = manager.create_workspace("bump L1 to 100").unwrap();
    std::fs::write(
        a.path.join("src/index.ts"),
        "export const L1 = 100;\nexport const L2 = 2;\nexport const L3 = 3;\n\
         export const L4 = 4;\nexport const L5 = 5;\n",
    )
    .unwrap();

    let b = manager.create_workspace("bump L1 to 200").unwrap();
    std::fs::write(
        b.path.join("src/index.ts"),
        "export const L1 = 200;\nexport const L2 = 2;\nexport const L3 = 3;\n\
         export const L4 = 4;\nexport const L5 = 5;\n",
    )
    .unwrap();

    let report = manager.merge_all(None, None, &[], None, None, false).unwrap();
    let conflict = &report.conflicted[0];

    let outcome = manager.resolve_conflict(&conflict.target_branch, &conflict.id, &[], None).unwrap();
    assert!(
        matches!(outcome, ResolveOutcome::StillConflicted { .. }),
        "expected the retry to conflict again since nothing changed, got {outcome:?}"
    );

    cleanup(&repo);
}
