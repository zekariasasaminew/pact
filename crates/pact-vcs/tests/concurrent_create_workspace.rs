//! Regression coverage for issue #230: `spawn-many` silently produced fewer
//! workspaces than tasks requested once (N-1) waiters queued behind the
//! `locks/git.lock` guard for longer than the (then 30s) timeout allowed.
//! Real repo, real concurrent `git worktree add` calls -- see DESIGN.md
//! ("pact-vcs > PidLock origin").
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
    let root = std::env::temp_dir().join(format!("pact-vcs-concurrent-create-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    run_git(&root, &["init", "-q"]);
    run_git(&root, &["config", "user.email", "test@test.com"]);
    run_git(&root, &["config", "user.name", "test"]);
    // Enough tracked files that `git worktree add`'s checkout takes long
    // enough for real contention to actually happen between the N threads
    // below, rather than each one finishing before the next even asks.
    for i in 0..300 {
        std::fs::write(root.join(format!("file{i}.txt")), format!("content {i}\n")).unwrap();
    }
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

#[test]
fn n_concurrent_create_workspace_calls_produce_n_workspaces() {
    let repo = init_repo();
    let manager = WorkspaceManager::open(&repo).unwrap();

    const N: usize = 8;
    let tasks: Vec<String> = (0..N).map(|i| format!("task number {i} for issue 230")).collect();

    let results: Vec<_> = std::thread::scope(|scope| {
        let handles: Vec<_> = tasks
            .iter()
            .map(|task| {
                let manager = &manager;
                scope.spawn(move || manager.create_workspace(task, None))
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });

    let failures: Vec<String> = results
        .iter()
        .enumerate()
        .filter_map(|(i, r)| r.as_ref().err().map(|e| format!("task {i}: {e:#}")))
        .collect();
    assert!(
        failures.is_empty(),
        "all {N} concurrent creations must succeed, got failures:\n{}",
        failures.join("\n")
    );

    let workspaces = manager.list_workspaces().unwrap();
    assert_eq!(
        workspaces.len(),
        N,
        "expected exactly {N} workspaces on disk after {N} concurrent create_workspace calls, found {}",
        workspaces.len()
    );

    cleanup(&repo);
}
