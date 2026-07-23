//! Integration coverage for issue #84: `pact history` surfaces the
//! coordination layer's operation log. Drives the real built `pact`
//! binary end-to-end against a throwaway repo, same technique
//! `merge_all_exit_code.rs` uses -- workspaces created directly via
//! `pact_vcs::WorkspaceManager`, no agent CLI involved, so `merge-all` and
//! `teardown` (both of which log an operation) can be exercised for real.
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
    let root = std::env::temp_dir().join(format!("pact-cli-history-{name}-{}", Uuid::new_v4()));
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

fn run_pact(repo: &Path, args: &[&str]) -> std::process::Output {
    let mut full_args = vec!["--repo", repo.to_str().unwrap()];
    full_args.extend_from_slice(args);
    Command::new(env!("CARGO_BIN_EXE_pact")).args(&full_args).output().unwrap()
}

#[test]
fn history_records_a_merge_all_invocation_and_a_teardown() {
    let repo = init_repo("merge-and-teardown");
    let manager = WorkspaceManager::open(&repo).unwrap();

    let a = manager.create_workspace("add b.txt").unwrap();
    std::fs::write(a.path.join("b.txt"), "new file\n").unwrap();

    let merge_output = run_pact(&repo, &["merge-all"]);
    assert!(
        merge_output.status.success(),
        "merge-all failed: {}",
        String::from_utf8_lossy(&merge_output.stderr)
    );

    let history_output = run_pact(&repo, &["history"]);
    assert!(history_output.status.success());
    let stdout = String::from_utf8_lossy(&history_output.stdout);
    assert!(stdout.contains("merge_all"), "expected a merge_all operation, got: {stdout}");
    assert!(stdout.contains("merged 1, skipped 0"), "expected the merge summary to reflect 1 merged workspace, got: {stdout}");

    let teardown_output = run_pact(&repo, &["teardown", &a.id, "--force"]);
    assert!(
        teardown_output.status.success(),
        "teardown failed: {}",
        String::from_utf8_lossy(&teardown_output.stderr)
    );

    let history_after_teardown = run_pact(&repo, &["history", "--type", "teardown"]);
    let stdout = String::from_utf8_lossy(&history_after_teardown.stdout);
    assert!(stdout.contains("teardown"), "expected a teardown operation, got: {stdout}");
    assert!(stdout.contains(&a.id), "expected the teardown operation to reference the torn-down workspace id, got: {stdout}");

    cleanup(&repo);
}

#[test]
fn history_json_output_is_valid_json() {
    let repo = init_repo("json-output");
    let manager = WorkspaceManager::open(&repo).unwrap();
    manager.create_workspace("noop").unwrap();

    run_pact(&repo, &["merge-all", "--dry-run"]);

    let output = run_pact(&repo, &["history", "--json"]);
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: serde_json::Value = serde_json::from_str(&stdout).unwrap_or_else(|e| panic!("expected valid JSON, got error {e}: {stdout}"));
    assert!(parsed.is_array(), "expected a JSON array of operations, got: {stdout}");
    assert!(!parsed.as_array().unwrap().is_empty(), "expected at least the dry-run merge_all operation, got: {stdout}");

    cleanup(&repo);
}

#[test]
fn history_with_no_operations_reports_that_clearly() {
    let repo = init_repo("empty");

    let output = run_pact(&repo, &["history"]);
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("no operations recorded"), "expected an empty-state message, got: {stdout}");

    cleanup(&repo);
}
