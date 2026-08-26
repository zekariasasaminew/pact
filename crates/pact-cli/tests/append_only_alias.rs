//! Integration coverage for issue #152: `--append-only` is the new
//! preferred name for what was `--union`, with `--union` kept working
//! as a backward-compatible alias. Drives the real `pact` binary
//! end-to-end against a real conflict shape that only resolves through
//! this flag (same shape pact-vcs's own `merge_all.rs` tests use), for
//! both flag names, confirming they're truly functionally identical --
//! not just that both parse without a clap error.
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

fn init_repo_with_barrel(name: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("pact-cli-append-only-{name}-{}", Uuid::new_v4()));
    std::fs::create_dir_all(root.join("src")).unwrap();
    run_git(&root, &["init", "-q"]);
    run_git(&root, &["config", "user.email", "test@test.com"]);
    run_git(&root, &["config", "user.name", "test"]);
    std::fs::write(root.join("src").join("barrel.ts"), "export {};\n").unwrap();
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

fn run_merge_all(repo: &Path, flag: &str) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_pact"))
        .args(["--repo", repo.to_str().unwrap(), "merge-all", flag, "src/barrel.ts"])
        .output()
        .unwrap()
}

#[test]
fn append_only_flag_resolves_a_real_barrel_export_conflict() {
    let repo = init_repo_with_barrel("primary-flag");
    let manager = WorkspaceManager::open(&repo).unwrap();

    let a = manager.create_workspace("export chunk", None).unwrap();
    std::fs::write(a.path.join("src/barrel.ts"), "export {};\nexport * from './chunk';\n").unwrap();
    let b = manager.create_workspace("export omit", None).unwrap();
    std::fs::write(b.path.join("src/barrel.ts"), "export {};\nexport * from './omit';\n").unwrap();

    let output = run_merge_all(&repo, "--append-only");
    assert!(output.status.success(), "stdout: {}\nstderr: {}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));

    // Target branch name includes a generated id, so find it by pattern
    // instead of guessing the exact name.
    let branches = Command::new("git").args(["branch", "--list", "pact/merged-*"]).current_dir(&repo).output().unwrap();
    let branch_list = String::from_utf8_lossy(&branches.stdout);
    let target = branch_list.trim().trim_start_matches('*').trim();
    assert!(!target.is_empty(), "expected a pact/merged-* branch to exist, got: {branch_list}");

    let content_output = Command::new("git").args(["show", &format!("{target}:src/barrel.ts")]).current_dir(&repo).output().unwrap();
    let content = String::from_utf8_lossy(&content_output.stdout);
    assert!(content.contains("export * from './chunk';"), "got: {content}");
    assert!(content.contains("export * from './omit';"), "got: {content}");

    cleanup(&repo);
}

#[test]
fn union_alias_resolves_the_same_real_conflict_identically() {
    let repo = init_repo_with_barrel("alias-flag");
    let manager = WorkspaceManager::open(&repo).unwrap();

    let a = manager.create_workspace("export chunk", None).unwrap();
    std::fs::write(a.path.join("src/barrel.ts"), "export {};\nexport * from './chunk';\n").unwrap();
    let b = manager.create_workspace("export omit", None).unwrap();
    std::fs::write(b.path.join("src/barrel.ts"), "export {};\nexport * from './omit';\n").unwrap();

    let output = run_merge_all(&repo, "--union");
    assert!(output.status.success(), "stdout: {}\nstderr: {}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));

    let branches = Command::new("git").args(["branch", "--list", "pact/merged-*"]).current_dir(&repo).output().unwrap();
    let branch_list = String::from_utf8_lossy(&branches.stdout);
    let target = branch_list.trim().trim_start_matches('*').trim();
    assert!(!target.is_empty(), "expected a pact/merged-* branch to exist, got: {branch_list}");

    let content_output = Command::new("git").args(["show", &format!("{target}:src/barrel.ts")]).current_dir(&repo).output().unwrap();
    let content = String::from_utf8_lossy(&content_output.stdout);
    assert!(content.contains("export * from './chunk';"), "got: {content}");
    assert!(content.contains("export * from './omit';"), "got: {content}");

    cleanup(&repo);
}
