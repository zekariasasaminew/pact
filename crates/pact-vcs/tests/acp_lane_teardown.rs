//! Issue #331: a workspace whose agent is an ACP session inside a shared
//! agent process records that process's pid, and teardown must not kill
//! it (every other lane in the process would die too). It leaves a cancel
//! marker for the hosting process instead. The test uses its own pid as
//! the "shared process": if teardown killed it, the test would not get to
//! its assertions.
use std::path::{Path, PathBuf};
use std::process::Command;

use pact_vcs::WorkspaceManager;
use uuid::Uuid;

fn run_git(dir: &Path, args: &[&str]) {
    let output = Command::new("git").args(args).current_dir(dir).output().unwrap();
    assert!(output.status.success(), "`git {}` failed:\n{}", args.join(" "), String::from_utf8_lossy(&output.stderr));
}

fn init_repo() -> PathBuf {
    let root = std::env::temp_dir().join(format!("pact-vcs-acp-teardown-{}", Uuid::new_v4()));
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
fn tearing_down_an_acp_lane_leaves_a_cancel_marker_and_spares_the_shared_process() {
    let repo = init_repo();
    let manager = WorkspaceManager::open(&repo).unwrap();
    let workspace = manager.create_workspace("acp lane", Some("acp-lane")).unwrap();
    manager.set_acp_session(&workspace.id, Some("sess-42")).unwrap();
    manager.set_agent_pid(&workspace.id, Some(std::process::id())).unwrap();
    let marker = manager.cancel_marker_path(&workspace.id);
    assert!(!marker.exists());

    let stored = manager.get_workspace(&workspace.id).unwrap();
    assert_eq!(stored.acp_session.as_deref(), Some("sess-42"), "acp_session must round-trip through metadata");

    // Play the hosting process: wait for the marker, note what it names,
    // consume it (which is what lets teardown proceed without waiting
    // out its timeout).
    let host = {
        let marker = marker.clone();
        std::thread::spawn(move || {
            for _ in 0..100 {
                if let Ok(content) = std::fs::read_to_string(&marker) {
                    let _ = std::fs::remove_file(&marker);
                    return Some(content);
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            None
        })
    };
    let started = std::time::Instant::now();
    manager.remove_workspace(&workspace.id, false, true).unwrap();
    let elapsed = started.elapsed();

    // Still alive to say so: the shared process was not killed.
    assert_eq!(host.join().unwrap().as_deref(), Some("sess-42"), "the marker names the session to cancel");
    assert!(elapsed < std::time::Duration::from_secs(4), "teardown proceeds as soon as the marker is consumed, took {elapsed:?}");
    assert!(!marker.exists());
    assert!(!workspace.path.exists(), "the worktree itself is removed as usual");

    cleanup(&repo);
}

#[test]
fn an_unconsumed_cancel_marker_is_cleared_after_the_wait_and_teardown_still_proceeds() {
    // The hosting process is alive (this test) but nobody is watching the
    // marker: a hung host. Teardown waits it out, clears the marker and
    // proceeds rather than leaving stale state or blocking forever.
    let repo = init_repo();
    let manager = WorkspaceManager::open(&repo).unwrap();
    let workspace = manager.create_workspace("hung acp lane", Some("hung-lane")).unwrap();
    manager.set_acp_session(&workspace.id, Some("sess-7")).unwrap();
    manager.set_agent_pid(&workspace.id, Some(std::process::id())).unwrap();
    let marker = manager.cancel_marker_path(&workspace.id);

    let started = std::time::Instant::now();
    manager.remove_workspace(&workspace.id, false, true).unwrap();
    assert!(started.elapsed() >= std::time::Duration::from_secs(4), "with nobody to consume it, teardown waits the marker out");
    assert!(!marker.exists(), "a stale marker is not left behind");
    assert!(!workspace.path.exists());

    cleanup(&repo);
}

#[test]
fn a_finished_acp_lane_is_torn_down_without_any_marker() {
    // Once a lane's run ends its pid is cleared; `acp_session` stays as a
    // record. Nothing is running, so nothing is asked to cancel.
    let repo = init_repo();
    let manager = WorkspaceManager::open(&repo).unwrap();
    let workspace = manager.create_workspace("finished acp lane", Some("done-lane")).unwrap();
    manager.set_acp_session(&workspace.id, Some("sess-9")).unwrap();
    assert!(manager.get_workspace(&workspace.id).unwrap().agent_pid.is_none());

    let started = std::time::Instant::now();
    manager.remove_workspace(&workspace.id, false, true).unwrap();
    assert!(started.elapsed() < std::time::Duration::from_secs(3), "no wait for a lane that is not running");
    assert!(!manager.cancel_marker_path(&workspace.id).exists());
    assert!(!workspace.path.exists());

    cleanup(&repo);
}

#[test]
fn an_ordinary_workspace_has_no_acp_session_and_no_marker() {
    let repo = init_repo();
    let manager = WorkspaceManager::open(&repo).unwrap();
    let workspace = manager.create_workspace("plain", Some("plain")).unwrap();
    assert!(workspace.acp_session.is_none());
    manager.remove_workspace(&workspace.id, false, false).unwrap();
    assert!(!manager.cancel_marker_path(&workspace.id).exists(), "no marker for a process-runtime workspace");
    cleanup(&repo);
}
