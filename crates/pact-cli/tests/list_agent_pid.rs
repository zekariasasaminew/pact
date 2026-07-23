//! Integration coverage for issue #108: `pact list` surfaces a workspace's
//! recorded `agent_pid` liveness, since an abrupt crash can leave the
//! actual agent process running as an orphan with nothing else to notice
//! it automatically. Real repo, real process liveness check -- no agent
//! CLI involved, `set_agent_pid` is called directly the same way
//! `spawn_with_supervisor` would.
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
    let root = std::env::temp_dir().join(format!("pact-cli-list-agent-pid-{name}-{}", Uuid::new_v4()));
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
}

#[test]
fn list_reports_a_live_agent_pid_as_running() {
    let repo = init_repo("live");
    let manager = WorkspaceManager::open(&repo).unwrap();
    let ws = manager.create_workspace("some task").unwrap();
    // This test process's own pid is guaranteed to be alive for the
    // duration of the test -- a real, if borrowed, live pid.
    manager.set_agent_pid(&ws.id, Some(std::process::id())).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_pact"))
        .args(["--repo", repo.to_str().unwrap(), "list"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains(&format!("agent pid: {} (running)", std::process::id())),
        "expected a live agent pid to be reported as running, got: {stdout}"
    );

    cleanup(&repo);
}

#[test]
fn list_reports_a_dead_agent_pid_as_not_running() {
    let repo = init_repo("dead");
    let manager = WorkspaceManager::open(&repo).unwrap();
    let ws = manager.create_workspace("some task").unwrap();
    manager.set_agent_pid(&ws.id, Some(u32::MAX)).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_pact"))
        .args(["--repo", repo.to_str().unwrap(), "list"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains(&format!("agent pid: {} (not running)", u32::MAX)),
        "expected a dead agent pid to be reported as not running, got: {stdout}"
    );

    cleanup(&repo);
}

#[test]
fn list_omits_the_agent_pid_line_when_none_is_recorded() {
    let repo = init_repo("none");
    let manager = WorkspaceManager::open(&repo).unwrap();
    manager.create_workspace("some task").unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_pact"))
        .args(["--repo", repo.to_str().unwrap(), "list"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(!stdout.contains("agent pid:"), "expected no agent pid line when none was recorded, got: {stdout}");

    cleanup(&repo);
}
