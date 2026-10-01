//! Integration coverage for issue #343: the dependency-prep and run
//! sidecars live in `meta/deps/` and `meta/runs/`, so the top of `meta/`
//! holds workspace records only and a workspace *named* `smoke-run` or
//! `db-deps` is listed like any other. State directories written before
//! the move still hold `<id>-deps.json` / `<id>-run.json` beside the
//! records; `WorkspaceManager::open` relocates those once and leaves
//! real workspace records with such names alone. Real repo, real git.
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
    assert!(output.status.success(), "`git {}` failed:\n{}", args.join(" "), String::from_utf8_lossy(&output.stderr));
}

fn init_repo() -> PathBuf {
    let root = std::env::temp_dir().join(format!("pact-vcs-sidecar-names-{}", Uuid::new_v4()));
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

#[test]
fn workspaces_named_like_sidecars_are_listed_and_torn_down() {
    let root = init_repo();
    let manager = WorkspaceManager::open(&root).unwrap();

    let run = manager.create_workspace("smoke the run path", Some("smoke-run")).unwrap();
    let deps = manager.create_workspace("database dependencies", Some("db-deps")).unwrap();
    assert_eq!(run.id, "smoke-run");
    assert_eq!(deps.id, "db-deps");

    let mut listed: Vec<String> = manager.list_workspaces().unwrap().into_iter().map(|w| w.id).collect();
    listed.sort();
    assert_eq!(listed, vec!["db-deps", "smoke-run"], "both records sit at the top of meta/ and both must be listed");

    manager.remove_workspace("smoke-run", false, false).unwrap();
    manager.remove_workspace("db-deps", false, false).unwrap();
    assert!(manager.list_workspaces().unwrap().is_empty());
    cleanup(&root);
}

#[test]
fn legacy_sidecars_beside_the_records_move_into_their_directories_on_open() {
    let root = init_repo();
    let manager = WorkspaceManager::open(&root).unwrap();
    let real = manager.create_workspace("a real lane", Some("lane")).unwrap();
    let named_like_a_sidecar = manager.create_workspace("named like a sidecar", Some("smoke-run")).unwrap();
    let meta = manager.state_dir().join("meta");

    // What a pre-#343 pact left behind: the lane's two sidecars beside the
    // records, plus a sidecar for a workspace that was torn down long ago.
    std::fs::write(meta.join("lane-deps.json"), "[{\"manager\":\"npm\",\"success\":true}]").unwrap();
    std::fs::write(meta.join("lane-run.json"), "{\"workspace_id\":\"lane\",\"agent\":\"copilot\"}").unwrap();
    std::fs::write(meta.join("gone-run.json"), "{\"workspace_id\":\"gone\"}").unwrap();

    // Before the move, list_workspaces would either skip `smoke-run` or
    // choke on the sidecars' shape. Opening again performs the migration.
    let reopened = WorkspaceManager::open(&root).unwrap();
    let mut listed: Vec<String> = reopened.list_workspaces().unwrap().into_iter().map(|w| w.id).collect();
    listed.sort();
    assert_eq!(listed, vec![real.id.clone(), named_like_a_sidecar.id.clone()]);

    assert!(!meta.join("lane-deps.json").exists());
    assert!(!meta.join("lane-run.json").exists());
    assert!(!meta.join("gone-run.json").exists());
    assert_eq!(
        std::fs::read_to_string(reopened.deps_report_path("lane")).unwrap(),
        "[{\"manager\":\"npm\",\"success\":true}]"
    );
    assert_eq!(
        std::fs::read_to_string(reopened.run_report_path("lane")).unwrap(),
        "{\"workspace_id\":\"lane\",\"agent\":\"copilot\"}"
    );
    assert!(reopened.run_report_path("gone").exists(), "a sidecar for a torn-down workspace is kept, just relocated");
    assert!(meta.join("smoke-run.json").exists(), "a workspace record named like a sidecar must not be moved");

    // A leftover whose new home is already occupied is stale and is dropped
    // rather than overwriting the newer record.
    std::fs::write(meta.join("lane-run.json"), "{\"stale\":true}").unwrap();
    WorkspaceManager::open(&root).unwrap();
    assert!(!meta.join("lane-run.json").exists());
    assert_eq!(
        std::fs::read_to_string(reopened.run_report_path("lane")).unwrap(),
        "{\"workspace_id\":\"lane\",\"agent\":\"copilot\"}"
    );
    cleanup(&root);
}
