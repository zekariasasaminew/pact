//! Regression coverage for issue #283's safety finding: `git worktree
//! remove` follows a reparse point (an NTFS junction or a directory
//! symlink) inside the worktree and deletes the *target's* contents. For a
//! `node_modules` linked to the repo's own install, that would wipe the
//! user's real dependencies on every teardown. Real repo, real `git
//! worktree remove`, real junction -- see DESIGN.md ("pact-vcs > Reparse
//! points and worktree removal").
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
    let root = std::env::temp_dir().join(format!("pact-vcs-reparse-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    run_git(&root, &["init", "-q"]);
    run_git(&root, &["config", "user.email", "test@test.com"]);
    run_git(&root, &["config", "user.name", "test"]);
    std::fs::write(root.join("README.md"), "# reparse\n").unwrap();
    // Slash-free on purpose: this file tests teardown safety, not ignore
    // semantics. A trailing-slash `node_modules/` pattern matches
    // directories only, so a raw Unix symlink created below (bypassing
    // pact-deps, which handles that case via `ensure_git_ignores`) would
    // read as untracked and trip the dirty check before teardown ran.
    std::fs::write(root.join(".gitignore"), "node_modules\n").unwrap();
    run_git(&root, &["add", "-A"]);
    run_git(&root, &["commit", "-q", "-m", "init"]);
    root
}

fn cleanup(root: &Path, extra: &[&Path]) {
    for path in extra {
        let _ = std::fs::remove_dir_all(path);
    }
    let _ = std::fs::remove_dir_all(root);
    if let Ok(state_dir) = WorkspaceManager::state_dir_for(root) {
        let _ = std::fs::remove_dir_all(state_dir);
    }
}

/// Creates a directory link at `link` pointing at `target` the way pact's
/// link-mode dependency prep does: a junction on Windows (no privilege
/// needed, unlike a directory symlink), a plain symlink elsewhere.
fn link_dir(target: &Path, link: &Path) {
    #[cfg(windows)]
    {
        let output = Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(link)
            .arg(target)
            .output()
            .expect("failed to spawn cmd /C mklink /J");
        assert!(
            output.status.success(),
            "mklink /J failed:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    #[cfg(not(windows))]
    {
        std::os::unix::fs::symlink(target, link).expect("symlink failed");
    }
}

#[test]
fn remove_workspace_does_not_follow_a_node_modules_link_into_its_target() {
    let repo = init_repo();
    let manager = WorkspaceManager::open(&repo).unwrap();
    let workspace = manager.create_workspace("link teardown safety", None).unwrap();

    // The target stands in for the repo root's real node_modules: anything
    // pact links a workspace at, and must never delete through the link.
    let target = std::env::temp_dir().join(format!("pact-vcs-reparse-target-{}", Uuid::new_v4()));
    std::fs::create_dir_all(target.join("some-package")).unwrap();
    let marker = target.join("some-package").join("marker.txt");
    std::fs::write(&marker, "still here\n").unwrap();

    let link = workspace.path.join("node_modules");
    link_dir(&target, &link);
    assert!(link.join("some-package").join("marker.txt").exists(), "link must resolve to the target before teardown");

    manager.remove_workspace(&workspace.id, false, false).unwrap();

    assert!(!workspace.path.exists(), "the worktree itself must be gone");
    assert!(
        marker.exists(),
        "teardown followed the node_modules link and deleted the target's contents -- \
         issue #283's exact data-loss hazard"
    );
    assert!(manager.list_workspaces().unwrap().is_empty());

    cleanup(&repo, &[&target]);
}

#[test]
fn set_linked_paths_round_trips_through_workspace_metadata() {
    let repo = init_repo();
    let manager = WorkspaceManager::open(&repo).unwrap();
    let workspace = manager.create_workspace("linked paths metadata", None).unwrap();
    assert!(workspace.linked_paths.is_empty());

    manager.set_linked_paths(&workspace.id, vec!["node_modules".to_string()]).unwrap();

    let reloaded = manager.get_workspace(&workspace.id).unwrap();
    assert_eq!(reloaded.linked_paths, vec!["node_modules".to_string()]);
    assert_eq!(reloaded.base_commit, workspace.base_commit, "other fields must survive the rewrite");

    manager.remove_workspace(&workspace.id, false, true).unwrap();
    cleanup(&repo, &[]);
}

#[test]
fn unlink_top_level_reparse_points_removes_only_the_link() {
    let base = std::env::temp_dir().join(format!("pact-vcs-unlink-{}", Uuid::new_v4()));
    let target = base.join("target");
    let tree = base.join("tree");
    std::fs::create_dir_all(&target).unwrap();
    std::fs::create_dir_all(tree.join("real-dir")).unwrap();
    std::fs::write(target.join("marker.txt"), "x").unwrap();
    std::fs::write(tree.join("real-file.txt"), "y").unwrap();
    link_dir(&target, &tree.join("linked"));

    let unlinked = pact_vcs::unlink_top_level_reparse_points(&tree);

    assert_eq!(unlinked, vec![tree.join("linked")]);
    assert!(!tree.join("linked").exists(), "the link itself must be removed");
    assert!(target.join("marker.txt").exists(), "the target must be untouched");
    assert!(tree.join("real-dir").exists() && tree.join("real-file.txt").exists(), "real entries must be untouched");

    let _ = std::fs::remove_dir_all(&base);
}
