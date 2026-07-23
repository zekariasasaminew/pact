//! Integration coverage for issue #85: `pact resolve` lists and retries
//! persisted conflicts. Drives the real built `pact` binary end-to-end
//! against a throwaway repo, same technique `history.rs` and
//! `merge_all_exit_code.rs` use -- no agent CLI involved.
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

fn init_repo(name: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("pact-cli-resolve-{name}-{}", Uuid::new_v4()));
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

fn run_pact(repo: &Path, args: &[&str]) -> std::process::Output {
    let mut full_args = vec!["--repo", repo.to_str().unwrap()];
    full_args.extend_from_slice(args);
    Command::new(env!("CARGO_BIN_EXE_pact")).args(&full_args).output().unwrap()
}

fn show_file(repo: &Path, rev: &str, path: &str) -> String {
    let output = Command::new("git").args(["show", &format!("{rev}:{path}")]).current_dir(repo).output().unwrap();
    assert!(output.status.success());
    String::from_utf8(output.stdout).unwrap()
}

fn create_conflicting_pair(repo: &Path) -> (pact_vcs::Workspace, pact_vcs::Workspace) {
    let manager = WorkspaceManager::open(repo).unwrap();

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

    (a, b)
}

#[test]
fn resolve_with_no_workspace_lists_the_conflict_merge_all_recorded() {
    let repo = init_repo("list");
    create_conflicting_pair(&repo);

    let merge_output = run_pact(&repo, &["merge-all"]);
    assert_eq!(
        merge_output.status.code(),
        Some(2),
        "expected merge-all to exit 2 (one workspace conflicted): {}",
        String::from_utf8_lossy(&merge_output.stderr)
    );

    let output = run_pact(&repo, &["resolve"]);
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("open conflicts:"), "expected an open conflict listed, got: {stdout}");
    assert!(stdout.contains("src/index.ts"), "expected the conflicted file named, got: {stdout}");

    cleanup(&repo);
}

#[test]
fn resolve_succeeds_once_the_workspace_branch_no_longer_conflicts_and_updates_history() {
    let repo = init_repo("succeed");
    let manager = WorkspaceManager::open(&repo).unwrap();
    let (_a, _b) = create_conflicting_pair(&repo);

    let merge_output = run_pact(&repo, &["merge-all"]);
    assert_eq!(merge_output.status.code(), Some(2), "expected merge-all to exit 2 (one workspace conflicted)");

    let list_output = run_pact(&repo, &["resolve"]);
    let list_stdout = String::from_utf8_lossy(&list_output.stdout);
    let loser_id = list_stdout
        .lines()
        .find(|line| line.contains(" -> "))
        .map(|line| line.trim().split(" -> ").next().unwrap())
        .expect("expected exactly one open conflict line")
        .to_string();

    let conflict = manager.get_workspace(&loser_id).unwrap();
    let target_line = list_stdout.lines().find(|l| l.contains(&loser_id)).unwrap();
    let target_branch = target_line.split(" -> ").nth(1).unwrap().split(" (").next().unwrap().to_string();
    let winning_content = show_file(&repo, &target_branch, "src/index.ts");
    std::fs::write(conflict.path.join("src/index.ts"), &winning_content).unwrap();
    run_git(&conflict.path, &["add", "-A"]);
    run_git(&conflict.path, &["commit", "-q", "-m", "fix by hand"]);

    let resolve_output = run_pact(&repo, &["resolve", &loser_id]);
    assert!(
        resolve_output.status.success(),
        "expected resolve to succeed, got: {}\nstdout: {}",
        String::from_utf8_lossy(&resolve_output.stderr),
        String::from_utf8_lossy(&resolve_output.stdout)
    );
    assert!(String::from_utf8_lossy(&resolve_output.stdout).contains("resolved:"));

    let list_after = run_pact(&repo, &["resolve"]);
    assert!(
        String::from_utf8_lossy(&list_after.stdout).contains("no open conflicts"),
        "expected the conflict to no longer be open after a successful resolve"
    );

    let history_output = run_pact(&repo, &["history", "--type", "conflict_resolve"]);
    let history_stdout = String::from_utf8_lossy(&history_output.stdout);
    assert!(history_stdout.contains("resolved"), "expected the resolve attempt in history, got: {history_stdout}");

    cleanup(&repo);
}

#[test]
fn resolve_reports_exit_code_2_when_still_conflicted() {
    let repo = init_repo("still-conflicted");
    create_conflicting_pair(&repo);

    run_pact(&repo, &["merge-all"]);
    let list_output = run_pact(&repo, &["resolve"]);
    let loser_id = String::from_utf8_lossy(&list_output.stdout)
        .lines()
        .find(|line| line.contains(" -> "))
        .map(|line| line.trim().split(" -> ").next().unwrap())
        .unwrap()
        .to_string();

    let output = run_pact(&repo, &["resolve", &loser_id]);
    assert_eq!(output.status.code(), Some(2), "expected exit 2 when the retry conflicts again");
    assert!(String::from_utf8_lossy(&output.stdout).contains("still conflicted"));

    cleanup(&repo);
}

#[test]
fn resolve_abandon_marks_the_conflict_no_longer_open() {
    let repo = init_repo("abandon");
    create_conflicting_pair(&repo);

    run_pact(&repo, &["merge-all"]);
    let list_output = run_pact(&repo, &["resolve"]);
    let loser_id = String::from_utf8_lossy(&list_output.stdout)
        .lines()
        .find(|line| line.contains(" -> "))
        .map(|line| line.trim().split(" -> ").next().unwrap())
        .unwrap()
        .to_string();

    let output = run_pact(&repo, &["resolve", &loser_id, "--abandon"]);
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("abandoned"));

    let list_after = run_pact(&repo, &["resolve"]);
    assert!(String::from_utf8_lossy(&list_after.stdout).contains("no open conflicts"));

    cleanup(&repo);
}
