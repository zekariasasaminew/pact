//! Integration coverage for `WorkspaceManager::merge_all` (pact v0.2 P0
//! #1) against a real, throwaway git repo -- no agent CLI involved, since
//! `merge_all` operates purely on workspaces' committed git state. Each
//! test gets its own temp repo so they can run in parallel without
//! interfering with each other.
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

/// Creates a fresh temp git repo with two committed files: `src/index.ts`
/// (five lines, so an append at the end has real surrounding context and a
/// change to line 1 stays a well-separated edit) and `src/other.ts` (one
/// line, an entirely unrelated file). Returns the repo root.
fn init_repo() -> PathBuf {
    let root = std::env::temp_dir().join(format!("pact-vcs-merge-all-{}", Uuid::new_v4()));
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
    std::fs::write(root.join("src/other.ts"), "export const OTHER = 0;\n").unwrap();
    run_git(&root, &["add", "-A"]);
    run_git(&root, &["commit", "-q", "-m", "init"]);
    root
}

fn cleanup(root: &Path) {
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn merge_all_merges_compatible_changes_and_skips_real_conflict() {
    let repo = init_repo();
    let manager = WorkspaceManager::open(&repo).unwrap();

    // A appends a new line at the end of index.ts, well-separated (4 lines
    // of untouched context) from anything C/D touch. B edits a completely
    // different file. Both are genuinely compatible with everything else
    // and must always merge, regardless of order. C and D both rewrite
    // index.ts's *first* line differently -- a real, unavoidable conflict
    // between exactly those two, confirmed by hand against real git before
    // writing this (single-line-file appends turned out to conflict far
    // more readily than multi-line context does -- see the trial report
    // this whole feature is built against).
    let a = manager.create_workspace("append L6").unwrap();
    std::fs::write(
        a.path.join("src/index.ts"),
        "export const L1 = 1;\nexport const L2 = 2;\nexport const L3 = 3;\n\
         export const L4 = 4;\nexport const L5 = 5;\nexport const L6 = 6;\n",
    )
    .unwrap();

    let b = manager.create_workspace("bump OTHER").unwrap();
    std::fs::write(b.path.join("src/other.ts"), "export const OTHER = 99;\n").unwrap();

    let c = manager.create_workspace("bump L1 to 100").unwrap();
    std::fs::write(
        c.path.join("src/index.ts"),
        "export const L1 = 100;\nexport const L2 = 2;\nexport const L3 = 3;\n\
         export const L4 = 4;\nexport const L5 = 5;\n",
    )
    .unwrap();

    let d = manager.create_workspace("bump L1 to 200").unwrap();
    std::fs::write(
        d.path.join("src/index.ts"),
        "export const L1 = 200;\nexport const L2 = 2;\nexport const L3 = 3;\n\
         export const L4 = 4;\nexport const L5 = 5;\n",
    )
    .unwrap();

    let report = manager.merge_all(None, None, false).unwrap();

    let merged_ids: Vec<&str> = report.merged.iter().map(|w| w.id.as_str()).collect();
    assert!(merged_ids.contains(&a.id.as_str()), "expected {} (well-separated append) to always merge cleanly", a.id);
    assert!(merged_ids.contains(&b.id.as_str()), "expected {} (unrelated file) to always merge cleanly", b.id);

    // C and D touch the same single file so they tie on the
    // smallest-changeset-first heuristic -- which merges first (and
    // therefore which one the *other* conflicts against) isn't specified.
    let c_merged = merged_ids.contains(&c.id.as_str());
    let d_merged = merged_ids.contains(&d.id.as_str());
    assert!(
        c_merged ^ d_merged,
        "expected exactly one of C/D (they conflict with each other) to merge, got merged={merged_ids:?}"
    );
    assert_eq!(report.merged.len(), 3, "expected A, B, and exactly one of C/D to merge, got {merged_ids:?}");

    assert_eq!(report.skipped.len(), 1, "expected exactly one skipped workspace");
    let skipped = &report.skipped[0];
    assert!(
        skipped.id == c.id || skipped.id == d.id,
        "expected the skipped workspace to be C or D, got {}", skipped.id
    );
    assert!(
        skipped.reason.contains("merge conflict"),
        "expected a merge-conflict reason, got: {}",
        skipped.reason
    );

    // The report's branch must actually exist in the repo, containing A and
    // B's changes, and the repo's own checkout must be untouched (still on
    // whatever branch `git init` created, not the integration branch).
    let branches = String::from_utf8(
        Command::new("git")
            .args(["branch", "--list", &report.target_branch])
            .current_dir(&repo)
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap();
    assert!(
        branches.contains(&report.target_branch),
        "expected branch '{}' to exist after merge_all", report.target_branch
    );

    let current_branch = String::from_utf8(
        Command::new("git")
            .args(["branch", "--show-current"])
            .current_dir(&repo)
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap();
    assert!(
        !current_branch.trim().is_empty() && current_branch.trim() != report.target_branch,
        "merge_all must not check out the integration branch in the repo's own working tree"
    );

    cleanup(&repo);
}

#[test]
fn merge_all_dry_run_touches_no_git_state() {
    let repo = init_repo();
    let manager = WorkspaceManager::open(&repo).unwrap();

    let a = manager.create_workspace("add chunk export").unwrap();
    std::fs::write(
        a.path.join("src/index.ts"),
        "export {};\nexport * from './chunk';\n",
    )
    .unwrap();

    let branches_before = String::from_utf8(
        Command::new("git").args(["branch"]).current_dir(&repo).output().unwrap().stdout,
    )
    .unwrap();

    let report = manager.merge_all(None, None, true).unwrap();

    assert!(report.dry_run);
    assert!(report.merged.is_empty(), "dry run must not actually merge anything");
    assert_eq!(report.planned, vec![a.id.clone()]);

    let branches_after = String::from_utf8(
        Command::new("git").args(["branch"]).current_dir(&repo).output().unwrap().stdout,
    )
    .unwrap();
    assert_eq!(
        branches_before, branches_after,
        "dry run must not create the integration branch (or any other branch)"
    );

    cleanup(&repo);
}

#[test]
fn merge_all_skips_workspace_whose_base_is_no_longer_an_ancestor() {
    let repo = init_repo();
    let manager = WorkspaceManager::open(&repo).unwrap();

    let a = manager.create_workspace("some change").unwrap();
    std::fs::write(a.path.join("src/index.ts"), "export const X = 1;\n").unwrap();

    // Rewrite the repo's own init commit so the SHA `a.base_commit` recorded
    // no longer exists on this branch's history at all -- simulating "HEAD
    // was reset/rebased since this workspace was created" without needing a
    // second, unrelated repo.
    run_git(&repo, &["commit", "--amend", "-q", "--allow-empty", "-m", "init (amended)"]);

    let report = manager.merge_all(None, None, false).unwrap();

    assert!(report.merged.is_empty(), "workspace with a moved base must not be merged");
    assert_eq!(report.skipped.len(), 1);
    assert_eq!(report.skipped[0].id, a.id);
    assert!(
        report.skipped[0].reason.contains("no longer part of this branch's history"),
        "expected a moving-base reason, got: {}",
        report.skipped[0].reason
    );

    cleanup(&repo);
}
