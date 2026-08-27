//! Integration coverage for issue #209: `pact clear-leases` -- an explicit,
//! unconditional escape hatch for stale cross-run lease persistence,
//! deliberately not a "stale" heuristic. The deeper SQL-level behavior
//! (actually removing both active and expired rows) is covered directly
//! against a real coordination database in
//! `crates/pact-coord/src/leases.rs`'s own tests; this only confirms the
//! CLI command is wired up and never fails.
use std::path::{Path, PathBuf};
use std::process::Command;

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
    let root = std::env::temp_dir().join(format!("pact-cli-clear-leases-{name}-{}", Uuid::new_v4()));
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

fn run_pact(repo: &Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_pact"))
        .args(["--repo"])
        .arg(repo)
        .args(args)
        .output()
        .unwrap()
}

#[test]
fn clear_leases_succeeds_and_reports_zero_on_a_fresh_repo() {
    let repo = init_repo("fresh");

    let output = run_pact(&repo, &["clear-leases"]);
    assert!(
        output.status.success(),
        "expected `pact clear-leases` to succeed, stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("cleared 0 lease(s)"), "expected a zero-lease report on a fresh repo, got: {stdout}");

    cleanup(&repo);
}

#[test]
fn clear_leases_is_idempotent() {
    let repo = init_repo("idempotent");

    assert!(run_pact(&repo, &["clear-leases"]).status.success());
    let second = run_pact(&repo, &["clear-leases"]);
    assert!(second.status.success(), "a second call must also succeed, not error on an already-empty table");

    cleanup(&repo);
}
