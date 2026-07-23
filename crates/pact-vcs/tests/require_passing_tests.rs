//! Integration coverage for issue #65: `merge_all`'s `require_passing_tests`
//! gate. Real repo, real shell commands -- see DESIGN.md ("pact-vcs >
//! Test-gated merge (issue #65)").
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
    let root = std::env::temp_dir().join(format!("pact-vcs-require-passing-tests-{}", Uuid::new_v4()));
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

fn always_fail_cmd() -> &'static str {
    if cfg!(windows) { "exit 1" } else { "false" }
}

fn always_pass_cmd() -> &'static str {
    if cfg!(windows) { "exit 0" } else { "true" }
}

#[test]
fn a_clean_merge_is_skipped_when_the_test_command_fails() {
    let repo = init_repo();
    let manager = WorkspaceManager::open(&repo).unwrap();

    let a = manager.create_workspace("add b.txt").unwrap();
    std::fs::write(a.path.join("b.txt"), "new file\n").unwrap();

    let report = manager.merge_all(None, None, &[], None, Some(always_fail_cmd()), false).unwrap();

    assert!(report.merged.is_empty(), "expected the workspace to be rejected by the failing test gate");
    assert_eq!(report.skipped.len(), 1);
    assert_eq!(report.skipped[0].id, a.id);
    assert!(
        report.skipped[0].reason.contains("failed the required test command"),
        "expected a test-gate-specific reason, got: {}",
        report.skipped[0].reason
    );

    let branch_files = Command::new("git")
        .args(["show", &format!("{}:", report.target_branch)])
        .current_dir(&repo)
        .output()
        .unwrap();
    let listing = String::from_utf8_lossy(&branch_files.stdout);
    assert!(!listing.contains("b.txt"), "the rejected workspace's file must not appear on the target branch, got: {listing}");

    cleanup(&repo);
}

#[test]
fn a_clean_merge_is_accepted_when_the_test_command_passes() {
    let repo = init_repo();
    let manager = WorkspaceManager::open(&repo).unwrap();

    let a = manager.create_workspace("add b.txt").unwrap();
    std::fs::write(a.path.join("b.txt"), "new file\n").unwrap();

    let report = manager.merge_all(None, None, &[], None, Some(always_pass_cmd()), false).unwrap();

    assert_eq!(report.merged.len(), 1);
    assert_eq!(report.merged[0].id, a.id);
    assert!(report.skipped.is_empty());

    cleanup(&repo);
}

#[test]
fn a_failed_gate_does_not_block_a_later_workspace_in_the_same_batch() {
    let repo = init_repo();
    let manager = WorkspaceManager::open(&repo).unwrap();

    let a = manager.create_workspace("add b.txt").unwrap();
    std::fs::write(a.path.join("b.txt"), "new file\n").unwrap();
    let b = manager.create_workspace("add c.txt").unwrap();
    std::fs::write(b.path.join("c.txt"), "new file\n").unwrap();

    // A cheap, real gate: only b.txt is allowed to exist -- fails for a's
    // merge (introduces b.txt), passes for b's (introduces c.txt).
    let gate = if cfg!(windows) { "if exist b.txt (exit 1) else (exit 0)" } else { "! [ -f b.txt ]" };
    let report = manager.merge_all(None, None, &[], None, Some(gate), false).unwrap();

    let merged_ids: Vec<&str> = report.merged.iter().map(|w| w.id.as_str()).collect();
    assert!(merged_ids.contains(&b.id.as_str()), "expected b's merge to pass the gate");
    assert!(!merged_ids.contains(&a.id.as_str()), "expected a's merge to fail the gate");
    assert_eq!(report.skipped.len(), 1);
    assert_eq!(report.skipped[0].id, a.id);

    cleanup(&repo);
}

#[test]
fn require_passing_tests_is_a_no_op_when_omitted() {
    let repo = init_repo();
    let manager = WorkspaceManager::open(&repo).unwrap();

    let a = manager.create_workspace("add b.txt").unwrap();
    std::fs::write(a.path.join("b.txt"), "new file\n").unwrap();

    let report = manager.merge_all(None, None, &[], None, None, false).unwrap();
    assert_eq!(report.merged.len(), 1, "expected unchanged behavior when the gate is omitted");

    cleanup(&repo);
}
