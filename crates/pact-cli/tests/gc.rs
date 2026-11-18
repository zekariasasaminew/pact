//! `pact gc` (issue #351) through the real binary: a live workspace's
//! state stays, a gone workspace's state and old run state go, plans stay,
//! and `--dry-run` removes nothing.
use std::path::{Path, PathBuf};
use std::process::Command;

use pact_vcs::WorkspaceManager;
use uuid::Uuid;

fn run_git(dir: &Path, args: &[&str]) {
    let output = Command::new("git").args(args).current_dir(dir).output().unwrap();
    assert!(output.status.success(), "`git {}` failed: {}", args.join(" "), String::from_utf8_lossy(&output.stderr));
}

fn init_repo() -> PathBuf {
    let root = std::env::temp_dir().join(format!("pact-cli-gc-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    run_git(&root, &["init", "-q"]);
    run_git(&root, &["config", "user.email", "test@test.com"]);
    run_git(&root, &["config", "user.name", "test"]);
    std::fs::write(root.join("README.md"), "# demo\n").unwrap();
    run_git(&root, &["add", "-A"]);
    run_git(&root, &["commit", "-q", "-m", "init"]);
    root
}

fn gc(repo: &Path, args: &[&str]) -> String {
    let coord = repo.with_extension("coord");
    let out = Command::new(env!("CARGO_BIN_EXE_pact"))
        .args(["--repo", repo.to_str().unwrap(), "gc"])
        .args(args)
        .env("PACT_COORD_DIR", &coord)
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).to_string()
}

#[test]
fn gc_removes_what_no_workspace_uses_and_keeps_the_rest() {
    let repo = init_repo();
    let manager = WorkspaceManager::open(&repo).unwrap();
    let live = manager.create_workspace("live task", Some("live")).unwrap();
    let state = manager.state_dir().to_path_buf();
    for file in ["logs/live.jsonl", "logs/gone.jsonl", "logs/planner-1.jsonl", "meta/runs/gone.json", "meta/plans/1-task.json", "briefs/1/alpha.md"] {
        let path = state.join(file);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "0123456789").unwrap();
    }

    let recent = gc(&repo, &[]);
    assert!(recent.contains("removed 0 item(s)"), "everything is younger than the default 7d:\n{recent}");

    let preview = gc(&repo, &["--older-than", "0", "--dry-run"]);
    assert!(preview.contains("would remove 4 item(s)"), "{preview}");
    assert!(state.join("logs/gone.jsonl").exists(), "a dry run removes nothing");

    let done = gc(&repo, &["--older-than", "0"]);
    assert!(done.contains("removed 4 item(s), 40 B"), "{done}");
    for gone in ["logs/gone.jsonl", "logs/planner-1.jsonl", "meta/runs/gone.json", "briefs/1"] {
        assert!(!state.join(gone).exists(), "{gone} should be gone");
    }
    for kept in ["logs/live.jsonl", "meta/plans/1-task.json"] {
        assert!(state.join(kept).exists(), "{kept} should stay");
    }
    assert!(live.path.exists(), "the live worktree is untouched");

    // Issue #416: a coordination directory whose repository is gone goes too.
    let coord = repo.with_extension("coord");
    let orphan = coord.join("0123456789abcdef");
    let owned = coord.join("fedcba9876543210");
    for (dir, owner) in [(&orphan, repo.with_extension("deleted")), (&owned, repo.clone())] {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join(pact_coord::OWNER_FILE), owner.to_string_lossy().as_bytes()).unwrap();
    }
    let pruned = gc(&repo, &["--older-than", "0"]);
    assert!(pruned.contains("which no longer exists") && !orphan.exists(), "{pruned}");
    assert!(owned.exists(), "a coordination directory whose repository exists stays");

    let _ = std::fs::remove_dir_all(&repo);
    let _ = std::fs::remove_dir_all(&state);
    let _ = std::fs::remove_dir_all(&coord);
}
