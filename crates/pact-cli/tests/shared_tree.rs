//! End-to-end coverage for issue #315: `spawn-many --shared-tree` runs every
//! lane in one worktree. Same fake-agent harness as `fake_agent_e2e.rs`
//! (real `pact` binary, real git repo, `fake_agent` on PATH as `claude`).
//! Measured motivation in issue #310: for file-disjoint batches, per-lane
//! worktrees plus `merge-all` were pure overhead (53.6 min vs 26.7 min).

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use uuid::Uuid;

fn run_git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap_or_else(|err| panic!("failed to spawn `git {}`: {err}", args.join(" ")));
    assert!(output.status.success(), "`git {}` failed: {}", args.join(" "), String::from_utf8_lossy(&output.stderr));
    String::from_utf8_lossy(&output.stdout).to_string()
}

fn init_repo(name: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("pact-cli-shared-tree-{name}-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    run_git(&root, &["init", "-q"]);
    run_git(&root, &["config", "user.email", "test@test.com"]);
    run_git(&root, &["config", "user.name", "test"]);
    std::fs::write(root.join("README.md"), "# demo\n").unwrap();
    run_git(&root, &["add", "-A"]);
    run_git(&root, &["commit", "-q", "-m", "init"]);
    root
}

fn cleanup(root: &Path) {
    let _ = std::fs::remove_dir_all(root);
    if let Ok(state_dir) = pact_vcs::WorkspaceManager::state_dir_for(root) {
        let _ = std::fs::remove_dir_all(state_dir);
    }
}

fn shim_dir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("pact-cli-shared-tree-shim-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let fake_agent = PathBuf::from(env!("CARGO_BIN_EXE_fake_agent"));
    let dest = if cfg!(windows) { dir.join("claude.exe") } else { dir.join("claude") };
    std::fs::copy(&fake_agent, &dest).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&dest).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&dest, perms).unwrap();
    }
    dir
}

fn path_with_shim_first(shim: &Path) -> String {
    let existing = std::env::var("PATH").unwrap_or_default();
    let sep = if cfg!(windows) { ";" } else { ":" };
    format!("{}{sep}{existing}", shim.display())
}

fn script(writes: &[(&str, &str)], summary: &str) -> String {
    serde_json::json!({
        "writes": writes.iter().cloned().collect::<std::collections::BTreeMap<&str, &str>>(),
        "summary": summary,
    })
    .to_string()
}

fn pact(repo: &Path, shim: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_pact"))
        .args(["--repo", repo.to_str().unwrap()])
        .args(args)
        .env("PATH", path_with_shim_first(shim))
        .output()
        .unwrap_or_else(|err| panic!("failed to spawn `pact {}`: {err}", args.join(" ")))
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).to_string()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).to_string()
}

#[test]
fn shared_tree_lanes_write_into_one_worktree_and_commit_all_lands_them_as_one_commit() {
    let repo = init_repo("basic");
    let shim = shim_dir();

    let task_a = script(&[("alpha.txt", "ALPHA")], "created alpha.txt");
    let task_b = script(&[("beta.txt", "BETA")], "created beta.txt");
    let task_c = script(&[("gamma.txt", "GAMMA")], "created gamma.txt");
    let spawn = pact(
        &repo,
        &shim,
        &[
            "spawn-many", "--agent", "claude", "--shared-tree",
            "--name", "lane-a", "--name", "lane-b", "--name", "lane-c",
            "--task", &task_a, "--task", &task_b, "--task", &task_c,
        ],
    );
    assert!(spawn.status.success(), "spawn-many failed:\nstdout: {}\nstderr: {}", stdout(&spawn), stderr(&spawn));

    let manager = pact_vcs::WorkspaceManager::open(&repo).unwrap();
    let all = manager.list_workspaces().unwrap();
    let batches: Vec<_> = all.iter().filter(|w| w.shared_batch.is_none()).collect();
    let lanes: Vec<_> = all.iter().filter(|w| w.shared_batch.is_some()).collect();
    assert_eq!(batches.len(), 1, "exactly one batch workspace, got: {:?}", all.iter().map(|w| &w.id).collect::<Vec<_>>());
    assert_eq!(lanes.len(), 3, "three lanes");
    let batch = batches[0];
    assert!(batch.id.starts_with("batch-"), "batch id: {}", batch.id);
    for lane in &lanes {
        assert_eq!(lane.path, batch.path, "lane {} must run in the batch worktree", lane.id);
        assert_eq!(lane.branch, batch.branch);
        assert_eq!(lane.shared_batch.as_deref(), Some(batch.id.as_str()));
    }
    for (file, expected) in [("alpha.txt", "ALPHA"), ("beta.txt", "BETA"), ("gamma.txt", "GAMMA")] {
        assert_eq!(std::fs::read_to_string(batch.path.join(file)).unwrap(), expected, "{file} in the shared tree");
    }

    // Only one real worktree exists for the whole batch.
    let worktrees = run_git(&repo, &["worktree", "list"]);
    let batch_worktrees = worktrees.lines().filter(|l| l.contains("batch-")).count();
    assert_eq!(batch_worktrees, 1, "worktree list:\n{worktrees}");

    // `list` says so.
    let list = stdout(&pact(&repo, &shim, &["list"]));
    assert!(list.contains("shared tree: lane of batch"), "list output:\n{list}");

    // One commit carries every lane's work.
    let commit = pact(&repo, &shim, &["commit-all"]);
    assert!(commit.status.success(), "commit-all failed: {}", stderr(&commit));
    let committed_lines = stdout(&commit).lines().filter(|l| l.ends_with(": committed")).count();
    assert_eq!(committed_lines, 1, "exactly one commit for the shared tree, got:\n{}", stdout(&commit));
    let log = run_git(&repo, &["log", "--oneline", &format!("HEAD..{}", batch.branch)]);
    assert_eq!(log.lines().count(), 1, "one commit on the batch branch, got:\n{log}");
    let message = run_git(&repo, &["log", "-1", "--format=%B", &batch.branch]);
    assert!(message.contains("3 lanes"), "commit message should name the lane count, got:\n{message}");
    for lane in &lanes {
        assert!(message.contains(&lane.id), "commit message should list lane {}, got:\n{message}", lane.id);
    }
    let files = run_git(&repo, &["ls-tree", "--name-only", &batch.branch]);
    for file in ["alpha.txt", "beta.txt", "gamma.txt"] {
        assert!(files.contains(file), "{file} must be on the batch branch: {files}");
    }

    cleanup(&repo);
    cleanup(&shim);
}

#[test]
fn shared_tree_refuses_overlapping_tasks_unless_allow_overlap() {
    let repo = init_repo("overlap");
    let shim = shim_dir();

    // Both task texts mention shared.txt; the heuristic flags that.
    let task_a = format!("edit shared.txt {}", script(&[("a.txt", "a")], "a"));
    let task_b = format!("also edit shared.txt {}", script(&[("b.txt", "b")], "b"));
    let refused = pact(
        &repo,
        &shim,
        &["spawn-many", "--agent", "claude", "--shared-tree", "--task", &task_a, "--task", &task_b, "--dry-run"],
    );
    assert!(!refused.status.success(), "expected --shared-tree to refuse an overlapping batch");
    assert!(stderr(&refused).contains("--shared-tree refused"), "stderr: {}", stderr(&refused));
    let manager = pact_vcs::WorkspaceManager::open(&repo).unwrap();
    assert!(manager.list_workspaces().unwrap().is_empty(), "a refusal must create nothing");

    let allowed = pact(
        &repo,
        &shim,
        &[
            "spawn-many", "--agent", "claude", "--shared-tree", "--allow-overlap",
            "--task", &task_a, "--task", &task_b, "--dry-run",
        ],
    );
    assert!(allowed.status.success(), "--allow-overlap should proceed: {}", stderr(&allowed));

    cleanup(&repo);
    cleanup(&shim);
}

#[test]
fn teardown_of_a_lane_keeps_the_worktree_and_the_batch_refuses_until_lanes_are_gone() {
    let repo = init_repo("teardown");
    let shim = shim_dir();

    let task_a = script(&[("alpha.txt", "ALPHA")], "a");
    let task_b = script(&[("beta.txt", "BETA")], "b");
    let spawn = pact(
        &repo,
        &shim,
        &["spawn-many", "--agent", "claude", "--shared-tree", "--name", "lane-a", "--name", "lane-b", "--task", &task_a, "--task", &task_b],
    );
    assert!(spawn.status.success(), "spawn-many failed: {}", stderr(&spawn));
    let manager = pact_vcs::WorkspaceManager::open(&repo).unwrap();
    let batch = manager.list_workspaces().unwrap().into_iter().find(|w| w.shared_batch.is_none()).unwrap();
    assert!(batch.path.join("alpha.txt").exists());

    // The batch refuses while lanes exist, even with --force.
    let refused = pact(&repo, &shim, &["teardown", &batch.id, "--force"]);
    assert!(!refused.status.success(), "batch teardown must refuse while lanes remain");
    assert!(stdout(&refused).contains("lane(s) still registered"), "stdout: {}", stdout(&refused));
    assert!(batch.path.exists(), "the worktree must survive a refused batch teardown");

    // Tearing down one lane drops only its metadata; the tree and the other lane stay.
    let one = pact(&repo, &shim, &["teardown", "lane-a", "--force"]);
    assert!(one.status.success(), "lane teardown failed: {}", stderr(&one));
    assert!(batch.path.exists() && batch.path.join("beta.txt").exists(), "the shared worktree must survive a lane teardown");
    let remaining: Vec<String> = manager.list_workspaces().unwrap().into_iter().map(|w| w.id).collect();
    assert!(remaining.contains(&"lane-b".to_string()) && !remaining.contains(&"lane-a".to_string()), "remaining: {remaining:?}");

    // The no-id sweep orders lanes before the batch and removes everything.
    let all = pact(&repo, &shim, &["teardown", "--force"]);
    assert!(all.status.success(), "sweep failed:\nstdout: {}\nstderr: {}", stdout(&all), stderr(&all));
    assert!(manager.list_workspaces().unwrap().is_empty(), "sweep must leave no workspaces");
    assert!(!batch.path.exists(), "sweep must remove the shared worktree last");

    cleanup(&repo);
    cleanup(&shim);
}

#[test]
fn a_bare_sweep_after_commit_all_keeps_the_batch_branch_instead_of_orphaning_its_commit() {
    // Issue #325: this is the exact sequence that orphaned the arm P
    // benchmark result. Every lane was clean after `commit-all`, so a
    // bare `teardown` swept the lanes, then the batch, then force-deleted
    // the only branch holding the batch's commit.
    let repo = init_repo("sweep-after-commit");
    let shim = shim_dir();

    let task_a = script(&[("alpha.txt", "ALPHA")], "a");
    let task_b = script(&[("beta.txt", "BETA")], "b");
    let spawn = pact(
        &repo,
        &shim,
        &["spawn-many", "--agent", "claude", "--shared-tree", "--name", "lane-a", "--name", "lane-b", "--task", &task_a, "--task", &task_b],
    );
    assert!(spawn.status.success(), "spawn-many failed: {}", stderr(&spawn));
    let manager = pact_vcs::WorkspaceManager::open(&repo).unwrap();
    let batch = manager.list_workspaces().unwrap().into_iter().find(|w| w.shared_batch.is_none()).unwrap();
    let commit = pact(&repo, &shim, &["commit-all"]);
    assert!(commit.status.success(), "commit-all failed: {}", stderr(&commit));
    let tip = run_git(&repo, &["rev-parse", &batch.branch]);

    let sweep = pact(&repo, &shim, &["teardown"]);
    assert_eq!(sweep.status.code(), Some(1), "the sweep must report the refused batch:\nstdout: {}", stdout(&sweep));
    let text = stdout(&sweep);
    assert!(text.contains("reachable from no other branch"), "the refusal must say why: {text}");
    assert!(text.contains("--keep-branch"), "the refusal must name the way out: {text}");
    assert!(batch.path.exists(), "a refused teardown must leave the worktree in place");
    assert_eq!(run_git(&repo, &["rev-parse", &batch.branch]), tip, "the batch branch must still point at the commit");
    let remaining: Vec<String> = manager.list_workspaces().unwrap().into_iter().map(|w| w.id).collect();
    assert_eq!(remaining, vec![batch.id.clone()], "lanes drop their metadata, the batch stays: {remaining:?}");

    let kept = pact(&repo, &shim, &["teardown", "--keep-branch"]);
    assert!(kept.status.success(), "--keep-branch should proceed:\nstdout: {}\nstderr: {}", stdout(&kept), stderr(&kept));
    assert!(!batch.path.exists(), "the worktree goes");
    assert_eq!(run_git(&repo, &["rev-parse", &batch.branch]), tip, "the branch and its commit survive");
    let files = run_git(&repo, &["ls-tree", "--name-only", &batch.branch]);
    assert!(files.contains("alpha.txt") && files.contains("beta.txt"), "the work is still on the branch: {files}");

    cleanup(&repo);
    cleanup(&shim);
}

#[test]
fn merge_all_treats_a_shared_tree_batch_as_one_workspace() {
    let repo = init_repo("merge");
    let shim = shim_dir();

    let task_a = script(&[("alpha.txt", "ALPHA")], "a");
    let task_b = script(&[("beta.txt", "BETA")], "b");
    let spawn = pact(
        &repo,
        &shim,
        &["spawn-many", "--agent", "claude", "--shared-tree", "--name", "lane-a", "--name", "lane-b", "--task", &task_a, "--task", &task_b],
    );
    assert!(spawn.status.success(), "spawn-many failed: {}", stderr(&spawn));

    let merge = pact(&repo, &shim, &["merge-all"]);
    assert!(merge.status.success(), "merge-all failed:\nstdout: {}\nstderr: {}", stdout(&merge), stderr(&merge));
    let merged_lines = stdout(&merge).lines().filter(|l| l.trim_start().starts_with("merged  ")).count();
    assert_eq!(merged_lines, 1, "the batch merges once, not once per lane, got:\n{}", stdout(&merge));
    assert!(stdout(&merge).contains("merged  batch-"), "the merged entry must be the batch, got:\n{}", stdout(&merge));

    let branches = run_git(&repo, &["branch", "--list", "pact/merged-*"]);
    let target = branches.trim().trim_start_matches('*').trim();
    let files = run_git(&repo, &["ls-tree", "--name-only", target]);
    assert!(files.contains("alpha.txt") && files.contains("beta.txt"), "merged branch files: {files}");

    cleanup(&repo);
    cleanup(&shim);
}

#[test]
fn shared_tree_dry_run_previews_one_worktree_for_many_lanes() {
    let repo = init_repo("dry-run");
    let shim = shim_dir();

    let task_a = script(&[("alpha.txt", "ALPHA")], "a");
    let task_b = script(&[("beta.txt", "BETA")], "b");
    let dry = pact(&repo, &shim, &["spawn-many", "--agent", "claude", "--shared-tree", "--task", &task_a, "--task", &task_b, "--dry-run"]);
    assert!(dry.status.success(), "dry-run failed: {}", stderr(&dry));
    let manager = pact_vcs::WorkspaceManager::open(&repo).unwrap();
    assert!(manager.list_workspaces().unwrap().is_empty(), "dry-run must create nothing");
    let worktrees = run_git(&repo, &["worktree", "list"]);
    assert_eq!(worktrees.lines().count(), 1, "dry-run must add no worktree: {worktrees}");

    cleanup(&repo);
    cleanup(&shim);
}
