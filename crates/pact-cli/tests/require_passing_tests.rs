//! Integration coverage for issue #65: `pact merge-all --require-passing-tests`.
//! Drives the real built `pact` binary end-to-end, same technique
//! `merge_all_exit_code.rs` uses.
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
    assert!(output.status.success(), "`git {}` failed: {}", args.join(" "), String::from_utf8_lossy(&output.stderr));
}

fn init_repo(name: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("pact-cli-require-passing-tests-{name}-{}", Uuid::new_v4()));
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

fn run_pact(repo: &Path, args: &[&str]) -> std::process::Output {
    let mut full_args = vec!["--repo", repo.to_str().unwrap()];
    full_args.extend_from_slice(args);
    Command::new(env!("CARGO_BIN_EXE_pact")).args(&full_args).output().unwrap()
}

#[test]
fn merge_all_exits_2_when_require_passing_tests_rejects_a_clean_merge() {
    let repo = init_repo("fails");
    let manager = WorkspaceManager::open(&repo).unwrap();
    let a = manager.create_workspace("add b.txt", None).unwrap();
    std::fs::write(a.path.join("b.txt"), "new file\n").unwrap();

    // Passes on the unmodified base (b.txt doesn't exist yet), fails once
    // a's merge lands it -- a real content-caused gate failure, not an
    // environment problem (see the preflight test below for that case).
    let fail_cmd = if cfg!(windows) { "if exist b.txt (exit 1) else (exit 0)" } else { "! [ -f b.txt ]" };
    let output = run_pact(&repo, &["merge-all", "--require-passing-tests", fail_cmd]);

    assert_eq!(output.status.code(), Some(2), "expected exit 2, a rejected-but-clean merge is a skip, not a hard failure");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("failed the required test command"), "expected the test-gate reason in output, got: {stdout}");

    cleanup(&repo);
}

/// Regression test for issue #232: a gate command that can't even pass on
/// the unmodified base commit (simulating a missing-dependencies
/// environment) must abort with exit 1 and a clear diagnosis on stderr --
/// a fundamentally different, harder failure than "your code failed the
/// gate" (exit 2, tested above).
#[test]
fn merge_all_exits_1_and_diagnoses_the_environment_when_the_gate_fails_on_base() {
    let repo = init_repo("broken-environment");
    let manager = WorkspaceManager::open(&repo).unwrap();
    let a = manager.create_workspace("add b.txt", None).unwrap();
    std::fs::write(a.path.join("b.txt"), "new file\n").unwrap();

    // Never committed anywhere -- absent from every worktree, including
    // the base commit, the same shape as a real "node_modules missing"
    // environment problem.
    let requires_missing_marker =
        if cfg!(windows) { "if exist setup_marker.txt (exit 0) else (exit 1)" } else { "test -f setup_marker.txt" };
    let output = run_pact(&repo, &["merge-all", "--require-passing-tests", requires_missing_marker]);

    assert_eq!(
        output.status.code(),
        Some(1),
        "expected exit 1, a broken environment is a hard failure, not a per-workspace skip -- \
         stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("unmodified base commit"), "expected an environment diagnosis on stderr, got: {stderr}");
    assert!(stderr.contains("No workspaces were merged"), "got: {stderr}");

    cleanup(&repo);
}

#[test]
fn merge_all_exits_0_when_require_passing_tests_accepts_every_merge() {
    let repo = init_repo("passes");
    let manager = WorkspaceManager::open(&repo).unwrap();
    let a = manager.create_workspace("add b.txt", None).unwrap();
    std::fs::write(a.path.join("b.txt"), "new file\n").unwrap();

    let pass_cmd = if cfg!(windows) { "exit 0" } else { "true" };
    let output = run_pact(&repo, &["merge-all", "--require-passing-tests", pass_cmd]);

    assert!(
        output.status.success(),
        "expected exit 0, got {:?}\nstdout: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout)
    );

    cleanup(&repo);
}
