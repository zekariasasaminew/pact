//! Integration coverage for issue #309: `merge_all`'s `GateMode::Final`,
//! which runs `require_passing_tests` once against the fully merged branch
//! instead of after every clean merge. Real repo, real shell commands,
//! content-based fixtures (a gate whose verdict depends on what was
//! merged, never a bare `true`/`false`) -- see DESIGN.md ("pact-vcs >
//! Gate timing (issue #309)").
use std::path::{Path, PathBuf};
use std::process::Command;

use pact_vcs::{GateMode, WorkspaceManager};
use uuid::Uuid;

fn run_git(dir: &Path, args: &[&str]) {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap_or_else(|err| panic!("failed to spawn `git {}`: {err}", args.join(" ")));
    assert!(output.status.success(), "`git {}` failed:\n{}", args.join(" "), String::from_utf8_lossy(&output.stderr));
}

fn init_repo() -> PathBuf {
    let root = std::env::temp_dir().join(format!("pact-vcs-gate-mode-{}", Uuid::new_v4()));
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

fn branch_listing(repo: &Path, branch: &str) -> String {
    let output = Command::new("git").args(["ls-tree", "--name-only", branch]).current_dir(repo).output().unwrap();
    String::from_utf8_lossy(&output.stdout).to_string()
}

/// Fails only once *both* b.txt and c.txt exist -- a combination failure
/// that no single workspace triggers on its own. This is the exact case
/// `Each` cannot see (each merge passes alone) and `Final` exists to
/// catch, and it is also what makes `Final`'s "whole batch rejected"
/// semantics observable rather than assumed.
fn fails_if_both_b_and_c_exist() -> &'static str {
    if cfg!(windows) {
        "if exist b.txt (if exist c.txt (exit 1) else (exit 0)) else (exit 0)"
    } else {
        "! { [ -f b.txt ] && [ -f c.txt ]; }"
    }
}

/// Passes on the unmodified base (via the `base.ok` sentinel, so the #232
/// preflight is satisfied) and otherwise only when *both* b.txt and
/// c.txt exist. Each workspace removes the sentinel and adds one file, so
/// under `Each` every merge is gated alone and fails, while under `Final`
/// the single gate sees both files and passes. A gate whose verdict flips
/// between the modes proves the per-merge gate is really skipped in
/// `Final`, without counting invocations through a shell-quoting-
/// sensitive log fixture.
fn passes_if_sentinel_or_both_b_and_c() -> &'static str {
    if cfg!(windows) {
        "if exist base.ok (exit 0) else (if exist b.txt (if exist c.txt (exit 0) else (exit 1)) else (exit 1))"
    } else {
        "[ -f base.ok ] || { [ -f b.txt ] && [ -f c.txt ]; }"
    }
}

#[test]
fn final_mode_skips_the_per_merge_gate_so_a_batch_each_mode_would_reject_lands() {
    let repo = init_repo();
    let manager = WorkspaceManager::open(&repo).unwrap();

    // Base carries base.ok so the preflight passes; each workspace removes
    // it (so the post-merge gate has to rely on b.txt+c.txt) and adds its
    // own file. Both workspaces removing the same file merges cleanly.
    std::fs::write(repo.join("base.ok"), "ok\n").unwrap();
    run_git(&repo, &["add", "-A"]);
    run_git(&repo, &["commit", "-q", "-m", "add base sentinel"]);
    let a = manager.create_workspace("add b.txt", None).unwrap();
    std::fs::remove_file(a.path.join("base.ok")).unwrap();
    std::fs::write(a.path.join("b.txt"), "b\n").unwrap();
    let b = manager.create_workspace("add c.txt", None).unwrap();
    std::fs::remove_file(b.path.join("base.ok")).unwrap();
    std::fs::write(b.path.join("c.txt"), "c\n").unwrap();

    let report = manager
        .merge_all(None, None, &[], None, None, Some(passes_if_sentinel_or_both_b_and_c()), GateMode::Final, false)
        .unwrap();

    assert_eq!(report.merged.len(), 2, "Final runs the gate once with both files present, so both land: {:?}", report.skipped);
    assert!(report.skipped.is_empty());
    let listing = branch_listing(&repo, &report.target_branch);
    assert!(listing.contains("b.txt") && listing.contains("c.txt"), "got: {listing}");
    assert!(!listing.contains("base.ok"), "the sentinel removal must have merged too: {listing}");

    cleanup(&repo);
}

#[test]
fn each_mode_rejects_the_same_batch_because_each_merge_is_gated_alone() {
    let repo = init_repo();
    let manager = WorkspaceManager::open(&repo).unwrap();

    std::fs::write(repo.join("base.ok"), "ok\n").unwrap();
    run_git(&repo, &["add", "-A"]);
    run_git(&repo, &["commit", "-q", "-m", "add base sentinel"]);
    let a = manager.create_workspace("add b.txt", None).unwrap();
    std::fs::remove_file(a.path.join("base.ok")).unwrap();
    std::fs::write(a.path.join("b.txt"), "b\n").unwrap();
    let b = manager.create_workspace("add c.txt", None).unwrap();
    std::fs::remove_file(b.path.join("base.ok")).unwrap();
    std::fs::write(b.path.join("c.txt"), "c\n").unwrap();

    let report = manager
        .merge_all(None, None, &[], None, None, Some(passes_if_sentinel_or_both_b_and_c()), GateMode::Each, false)
        .unwrap();

    // First merge: base.ok gone, only b.txt -> gate fails -> undone. Second
    // merge: base.ok gone, only c.txt -> gate fails -> undone. Nothing lands.
    assert!(report.merged.is_empty(), "Each gates every merge alone, so neither passes: {:?}", report.merged);
    assert_eq!(report.skipped.len(), 2);
    assert!(report.skipped.iter().all(|s| s.reason.contains("failed the required test command")));

    cleanup(&repo);
}

#[test]
fn final_mode_rejects_the_whole_batch_when_the_combined_suite_fails_and_leaves_the_branch_at_base() {
    let repo = init_repo();
    let manager = WorkspaceManager::open(&repo).unwrap();
    let base = {
        let out = Command::new("git").args(["rev-parse", "HEAD"]).current_dir(&repo).output().unwrap();
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    };

    let a = manager.create_workspace("add b.txt", None).unwrap();
    std::fs::write(a.path.join("b.txt"), "b\n").unwrap();
    let b = manager.create_workspace("add c.txt", None).unwrap();
    std::fs::write(b.path.join("c.txt"), "c\n").unwrap();

    let report = manager
        .merge_all(None, None, &[], None, None, Some(fails_if_both_b_and_c_exist()), GateMode::Final, false)
        .unwrap();

    assert!(report.merged.is_empty(), "a combined-suite failure must reject the whole batch, got merged: {:?}", report.merged);
    assert_eq!(report.skipped.len(), 2);
    for skipped in &report.skipped {
        assert!(
            skipped.reason.contains("--gate final") && skipped.reason.contains("--gate each"),
            "the skip reason must explain the mode and how to localize, got: {}",
            skipped.reason
        );
    }
    assert!(report.conflicted.is_empty(), "a gate rejection is not a merge conflict");

    let tip = Command::new("git").args(["rev-parse", &report.target_branch]).current_dir(&repo).output().unwrap();
    assert_eq!(
        String::from_utf8_lossy(&tip.stdout).trim(),
        base,
        "after a final-gate rejection the integration branch must point back at the base commit"
    );
    let listing = branch_listing(&repo, &report.target_branch);
    assert!(!listing.contains("b.txt") && !listing.contains("c.txt"), "rejected files must not be on the branch: {listing}");

    cleanup(&repo);
}

#[test]
fn each_mode_cannot_see_a_combination_failure_that_final_mode_catches() {
    // The same fixture as above under `Each`: b.txt merges and passes
    // (c.txt absent), then c.txt merges and the gate fails, so only the
    // second workspace is skipped and the first stays merged. Documents
    // the localization/coverage tradeoff between the two modes rather
    // than leaving it as prose in DESIGN.md.
    let repo = init_repo();
    let manager = WorkspaceManager::open(&repo).unwrap();

    let a = manager.create_workspace("add b.txt", None).unwrap();
    std::fs::write(a.path.join("b.txt"), "b\n").unwrap();
    let b = manager.create_workspace("add c.txt", None).unwrap();
    std::fs::write(b.path.join("c.txt"), "c\n").unwrap();

    let report = manager
        .merge_all(None, None, &[], None, None, Some(fails_if_both_b_and_c_exist()), GateMode::Each, false)
        .unwrap();

    assert_eq!(report.merged.len(), 1, "one of the two merges passes alone under Each");
    assert_eq!(report.skipped.len(), 1, "the second merge trips the combination and is skipped");

    cleanup(&repo);
}

#[test]
fn final_mode_still_skips_a_real_conflict_independently_of_the_gate() {
    let repo = init_repo();
    let manager = WorkspaceManager::open(&repo).unwrap();

    // Two workspaces editing the same line of a.txt: a real conflict.
    let a = manager.create_workspace("edit a one way", None).unwrap();
    std::fs::write(a.path.join("a.txt"), "from a\n").unwrap();
    let b = manager.create_workspace("edit a another way", None).unwrap();
    std::fs::write(b.path.join("a.txt"), "from b\n").unwrap();
    // And one clean, independent workspace.
    let c = manager.create_workspace("add d.txt", None).unwrap();
    std::fs::write(c.path.join("d.txt"), "d\n").unwrap();

    let gate = if cfg!(windows) { "exit 0" } else { "true" };
    let report = manager.merge_all(None, None, &[], None, None, Some(gate), GateMode::Final, false).unwrap();

    assert_eq!(report.conflicted.len(), 1, "exactly one of the two a.txt editors conflicts with whichever merged first");
    assert_eq!(report.merged.len(), 2, "the first a.txt editor and the independent d.txt workspace both land");
    assert!(report.merged.iter().any(|w| w.id == c.id));
    assert!(report.skipped.iter().all(|s| s.reason.contains("merge conflict")), "the only skip must be the conflict, got: {:?}", report.skipped);

    cleanup(&repo);
}

#[test]
fn final_mode_is_a_no_op_without_a_gate_command() {
    let repo = init_repo();
    let manager = WorkspaceManager::open(&repo).unwrap();

    let a = manager.create_workspace("add b.txt", None).unwrap();
    std::fs::write(a.path.join("b.txt"), "b\n").unwrap();

    let report = manager.merge_all(None, None, &[], None, None, None, GateMode::Final, false).unwrap();
    assert_eq!(report.merged.len(), 1);
    assert!(report.skipped.is_empty());

    cleanup(&repo);
}
