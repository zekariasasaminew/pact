//! Integration coverage for issue #16: `pact inspect <id>` aggregates
//! everything pact knows about one workspace. Real repo, real workspace
//! creation via `WorkspaceManager` directly (no agent CLI involved,
//! same pattern as `list_agent_pid.rs`) -- the dependency-prep/run
//! metadata files are written directly to simulate what a real spawn
//! would have produced, since spawning a real agent CLI is out of
//! scope for this project's tests.
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
    let root = std::env::temp_dir().join(format!("pact-cli-inspect-{name}-{}", Uuid::new_v4()));
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

fn run_inspect(repo: &Path, id: &str) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_pact"))
        .args(["--repo", repo.to_str().unwrap(), "inspect", id])
        .output()
        .unwrap()
}

#[test]
fn inspect_shows_basic_metadata_for_a_freshly_created_workspace() {
    let repo = init_repo("basic");
    let manager = WorkspaceManager::open(&repo).unwrap();
    let ws = manager.create_workspace("add a feature").unwrap();

    let output = run_inspect(&repo, &ws.id);
    assert!(output.status.success(), "stderr: {}", String::from_utf8_lossy(&output.stderr));
    let stdout = String::from_utf8_lossy(&output.stdout);

    assert!(stdout.contains(&ws.id), "got: {stdout}");
    assert!(stdout.contains(&ws.branch), "got: {stdout}");
    assert!(stdout.contains("task: add a feature"), "got: {stdout}");
    assert!(stdout.contains("agent pid: none recorded"), "got: {stdout}");
    assert!(stdout.contains("no record (workspace wasn't spawned"), "expected 'no record' for dependency prep, got: {stdout}");
    assert!(stdout.contains("no active leases held by this workspace"), "got: {stdout}");
    assert!(stdout.contains("no open conflict"), "got: {stdout}");
    assert!(stdout.contains("none recorded"), "expected empty history, got: {stdout}");

    cleanup(&repo);
}

#[test]
fn inspect_shows_persisted_dependency_prep_and_run_metadata() {
    let repo = init_repo("with-records");
    let manager = WorkspaceManager::open(&repo).unwrap();
    let ws = manager.create_workspace("add a feature").unwrap();

    let meta_dir = manager.state_dir().join("meta");
    std::fs::write(
        meta_dir.join(format!("{}-deps.json", ws.id)),
        r#"[{"manager":"npm","strategy":"content-store","store_key":"key1","store_hit":true,"materialization":"reflink","success":true,"warnings":[]}]"#,
    )
    .unwrap();
    std::fs::write(
        meta_dir.join(format!("{}-run.json", ws.id)),
        format!(
            r#"{{"workspace_id":"{}","agent":"claude","program":"claude","args":["-p","do it"],"cwd":"/tmp","started_at":100,"ended_at":142,"exit_success":true,"summary":"Created foo.rs","coord_status":"connected","files_touched":true,"log_path":"/tmp/log.jsonl"}}"#,
            ws.id
        ),
    )
    .unwrap();

    let output = run_inspect(&repo, &ws.id);
    assert!(output.status.success(), "stderr: {}", String::from_utf8_lossy(&output.stderr));
    let stdout = String::from_utf8_lossy(&output.stdout);

    assert!(stdout.contains("npm via content-store [ok]"), "got: {stdout}");
    assert!(stdout.contains("store hit"), "got: {stdout}");
    assert!(stdout.contains("materialized via reflink"), "got: {stdout}");
    assert!(stdout.contains("agent: claude"), "got: {stdout}");
    assert!(stdout.contains("succeeded in 42s: Created foo.rs"), "got: {stdout}");
    assert!(stdout.contains("coordination: connected"), "got: {stdout}");

    cleanup(&repo);
}

#[test]
fn inspect_fails_cleanly_for_an_unknown_workspace_id() {
    let repo = init_repo("unknown");
    let output = run_inspect(&repo, "does-not-exist");
    assert!(!output.status.success());

    cleanup(&repo);
}
