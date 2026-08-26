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
    if let Ok(state_dir) = WorkspaceManager::state_dir_for(root) {
        let _ = std::fs::remove_dir_all(state_dir);
    }
}

fn always_pass_cmd() -> &'static str {
    if cfg!(windows) { "exit 0" } else { "true" }
}

/// A gate that fails only once `b.txt` actually exists -- passes on the
/// unmodified base (nothing to fail on yet), so it can genuinely
/// distinguish "this workspace's change broke the gate" from "the
/// environment can't run the gate at all" (see the pre-flight tests
/// below). Deliberately not `always_fail_cmd()`/`always_pass_cmd()` --
/// issue #11's own critique: a fixture that fails unconditionally can't
/// tell a real content-caused failure apart from a broken environment,
/// which is exactly the distinction issue #232 is about.
fn fails_if_b_txt_exists() -> &'static str {
    if cfg!(windows) { "if exist b.txt (exit 1) else (exit 0)" } else { "! [ -f b.txt ]" }
}

#[test]
fn a_clean_merge_is_skipped_when_the_test_command_fails() {
    let repo = init_repo();
    let manager = WorkspaceManager::open(&repo).unwrap();

    let a = manager.create_workspace("add b.txt").unwrap();
    std::fs::write(a.path.join("b.txt"), "new file\n").unwrap();

    let report = manager.merge_all(None, None, &[], None, Some(fails_if_b_txt_exists()), false).unwrap();

    assert!(report.merged.is_empty(), "expected the workspace to be rejected by the failing test gate");
    assert_eq!(report.skipped.len(), 1);
    assert_eq!(report.skipped[0].id, a.id);
    assert!(
        report.skipped[0].reason.contains("failed the required test command"),
        "expected a test-gate-specific reason, got: {}",
        report.skipped[0].reason
    );
    assert!(
        report.skipped[0].reason.contains("exit code"),
        "expected the diagnosis (exit code, duration, output tail) in the skip reason, got: {}",
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

/// Regression test for issue #232's headline defect: a gate command that
/// can't even pass on the unmodified base commit (the "missing
/// dependencies" shape, simulated here with a marker file that's never
/// committed to any worktree) must abort the whole `merge_all` with an
/// environment diagnosis, not silently skip every workspace one by one
/// while blaming each one's "failed tests".
#[test]
fn a_gate_that_fails_on_the_unmodified_base_aborts_instead_of_skipping_every_workspace() {
    let repo = init_repo();
    let manager = WorkspaceManager::open(&repo).unwrap();

    let a = manager.create_workspace("add b.txt").unwrap();
    std::fs::write(a.path.join("b.txt"), "new file\n").unwrap();
    let b = manager.create_workspace("add c.txt").unwrap();
    std::fs::write(b.path.join("c.txt"), "new file\n").unwrap();

    // Never created/committed anywhere -- present in ZERO worktrees,
    // including the base commit, the same shape as `node_modules` being
    // absent from a fresh integration worktree in the real report.
    let requires_missing_marker =
        if cfg!(windows) { "if exist setup_marker.txt (exit 0) else (exit 1)" } else { "test -f setup_marker.txt" };

    let err = manager
        .merge_all(None, None, &[], None, Some(requires_missing_marker), false)
        .expect_err("a gate that fails on the unmodified base must abort merge_all, not return Ok with skips");

    let message = format!("{err:#}");
    assert!(message.contains("unmodified base commit"), "expected an environment diagnosis, got: {message}");
    assert!(message.contains("No workspaces were merged"), "got: {message}");
    assert!(message.contains("exit code"), "expected exit code/output diagnosis, got: {message}");

    // Neither workspace's branch was touched -- confirm this is a real
    // abort, not a report the caller could mistake for partial progress.
    let list = manager.list_workspaces().unwrap();
    assert_eq!(list.len(), 2, "both workspaces must still be present, untouched, for the caller to retry");

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

    // fails_if_b_txt_exists fails for a's merge (introduces b.txt), passes
    // for b's (introduces c.txt only).
    let report = manager.merge_all(None, None, &[], None, Some(fails_if_b_txt_exists()), false).unwrap();

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
