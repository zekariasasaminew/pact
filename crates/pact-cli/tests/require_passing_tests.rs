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
    let a = manager.create_workspace("add b.txt").unwrap();
    std::fs::write(a.path.join("b.txt"), "new file\n").unwrap();

    let fail_cmd = if cfg!(windows) { "exit 1" } else { "false" };
    let output = run_pact(&repo, &["merge-all", "--require-passing-tests", fail_cmd]);

    assert_eq!(output.status.code(), Some(2), "expected exit 2, a rejected-but-clean merge is a skip, not a hard failure");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("failed the required test command"), "expected the test-gate reason in output, got: {stdout}");

    cleanup(&repo);
}

#[test]
fn merge_all_exits_0_when_require_passing_tests_accepts_every_merge() {
    let repo = init_repo("passes");
    let manager = WorkspaceManager::open(&repo).unwrap();
    let a = manager.create_workspace("add b.txt").unwrap();
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
