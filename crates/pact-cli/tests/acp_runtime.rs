//! End-to-end coverage for issue #331: `spawn-many --runtime acp` runs
//! every lane as an Agent Client Protocol session inside ONE shared agent
//! process. Same harness shape as `shared_tree.rs`, with pact-acp's fake agent
//! on PATH as `copilot`: the fake speaks ACP, writes each task's files into
//! the session's cwd, and numbers its sessions from one counter, so three
//! lanes seeing `sess-1`, `sess-2`, `sess-3` proves one process hosted them
//! all (three processes would each have said `sess-1`).

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use uuid::Uuid;

fn run_git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap_or_else(|err| panic!("failed to spawn `git {}`: {err}", args.join(" ")));
    assert!(output.status.success(), "`git {}` failed: {}", args.join(" "), String::from_utf8_lossy(&output.stderr));
    String::from_utf8_lossy(&output.stdout).to_string()
}

fn init_repo(name: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("pact-cli-acp-{name}-{}", Uuid::new_v4()));
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

/// `fake_acp_copilot` (pact-acp's fake agent, built here under a name that
/// does not collide with pact-acp's own test binary) installed as `copilot`.
fn shim_dir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("pact-cli-acp-shim-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let fake = PathBuf::from(env!("CARGO_BIN_EXE_fake_acp_copilot"));
    let dest = if cfg!(windows) { dir.join("copilot.exe") } else { dir.join("copilot") };
    std::fs::copy(&fake, &dest).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&dest).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&dest, perms).unwrap();
    }
    dir
}

fn path_with_shim_first(shim: &Path) -> String {
    let existing = std::env::var("PATH").unwrap_or_default();
    let sep = if cfg!(windows) { ";" } else { ":" };
    format!("{}{sep}{existing}", shim.display())
}

fn task(writes: &[(&str, &str)], summary: &str) -> String {
    serde_json::json!({
        "writes": writes.iter().cloned().collect::<std::collections::BTreeMap<&str, &str>>(),
        "summary": summary,
    })
    .to_string()
}

fn pact(repo: &Path, shim: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_pact"))
        .args(["--repo", repo.to_str().unwrap()])
        .args(args)
        .env("PATH", path_with_shim_first(shim))
        .output()
        .unwrap_or_else(|err| panic!("failed to spawn `pact {}`: {err}", args.join(" ")))
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).to_string()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).to_string()
}

fn state_dir(repo: &Path) -> PathBuf {
    pact_vcs::WorkspaceManager::state_dir_for(repo).unwrap()
}

fn run_record(repo: &Path, id: &str) -> serde_json::Value {
    let path = state_dir(repo).join("meta").join(format!("{id}-run.json"));
    serde_json::from_str(&std::fs::read_to_string(&path).unwrap_or_else(|err| panic!("{}: {err}", path.display()))).unwrap()
}

#[test]
fn runtime_acp_runs_every_lane_as_a_session_in_one_shared_process() {
    let repo = init_repo("three-lanes");
    let shim = shim_dir();

    let spawn = pact(
        &repo,
        &shim,
        &[
            "spawn-many", "--agent", "copilot", "--runtime", "acp",
            "--name", "lane-a", "--name", "lane-b", "--name", "lane-c",
            "--task", &task(&[("alpha.txt", "ALPHA")], "wrote alpha"),
            "--task", &task(&[("beta.txt", "BETA")], "wrote beta"),
            "--task", &task(&[("gamma.txt", "GAMMA")], "wrote gamma"),
        ],
    );
    assert!(spawn.status.success(), "spawn-many failed:\nstdout: {}\nstderr: {}", stdout(&spawn), stderr(&spawn));
    let text = stdout(&spawn);
    assert_eq!(text.matches("done: stop reason end_turn").count(), 3, "every lane ends its turn:\n{text}");

    let manager = pact_vcs::WorkspaceManager::open(&repo).unwrap();
    let mut workspaces = manager.list_workspaces().unwrap();
    workspaces.sort_by(|a, b| a.id.cmp(&b.id));
    assert_eq!(workspaces.iter().map(|w| w.id.as_str()).collect::<Vec<_>>(), vec!["lane-a", "lane-b", "lane-c"]);
    for (workspace, file, content) in [(&workspaces[0], "alpha.txt", "ALPHA"), (&workspaces[1], "beta.txt", "BETA"), (&workspaces[2], "gamma.txt", "GAMMA")] {
        assert_eq!(std::fs::read_to_string(workspace.path.join(file)).unwrap(), content, "{file} lands in its own worktree");
        assert!(workspace.agent_pid.is_none(), "the shared pid is cleared once the lane is done");
        let session = workspace.acp_session.as_deref().expect("acp_session recorded");
        assert!(session.starts_with("sess-"), "session id from the agent: {session}");
        assert_eq!(workspace.session_id.as_deref(), Some(session), "session_id is the ACP session id");
    }
    let mut sessions: Vec<&str> = workspaces.iter().filter_map(|w| w.acp_session.as_deref()).collect();
    sessions.sort();
    assert_eq!(sessions, vec!["sess-1", "sess-2", "sess-3"], "one counter, one process, three sessions");

    for workspace in &workspaces {
        let record = run_record(&repo, &workspace.id);
        assert_eq!(record["runtime"], "acp");
        assert_eq!(record["exit_success"], true);
        assert_eq!(record["summary"], "stop reason end_turn");
        assert_eq!(record["session_id"], workspace.acp_session.as_deref().unwrap());
        assert!(record["args"].as_array().unwrap().iter().any(|a| a == "--acp"), "the record names the shared process launch: {}", record["args"]);
        let log = std::fs::read_to_string(record["log_path"].as_str().unwrap()).unwrap();
        assert!(log.contains("\"sessionUpdate\":\"tool_call\"") || log.contains("\"sessionUpdate\": \"tool_call\""), "the lane log holds the raw session updates:\n{log}");
    }

    let list = stdout(&pact(&repo, &shim, &["list"]));
    assert_eq!(list.matches("runtime: acp session sess-").count(), 3, "list output:\n{list}");

    cleanup(&repo);
    cleanup(&shim);
}

#[test]
fn runtime_acp_combines_with_shared_tree() {
    let repo = init_repo("shared");
    let shim = shim_dir();

    let spawn = pact(
        &repo,
        &shim,
        &[
            "spawn-many", "--agent", "copilot", "--runtime", "acp", "--shared-tree",
            "--name", "lane-a", "--name", "lane-b",
            "--task", &task(&[("alpha.txt", "ALPHA")], "a"),
            "--task", &task(&[("beta.txt", "BETA")], "b"),
        ],
    );
    assert!(spawn.status.success(), "spawn-many failed:\nstdout: {}\nstderr: {}", stdout(&spawn), stderr(&spawn));
    let manager = pact_vcs::WorkspaceManager::open(&repo).unwrap();
    let batch = manager.list_workspaces().unwrap().into_iter().find(|w| w.shared_batch.is_none()).unwrap();
    assert_eq!(std::fs::read_to_string(batch.path.join("alpha.txt")).unwrap(), "ALPHA");
    assert_eq!(std::fs::read_to_string(batch.path.join("beta.txt")).unwrap(), "BETA");

    let commit = pact(&repo, &shim, &["commit-all"]);
    assert!(commit.status.success(), "commit-all failed: {}", stderr(&commit));
    let files = run_git(&repo, &["ls-tree", "--name-only", &batch.branch]);
    assert!(files.contains("alpha.txt") && files.contains("beta.txt"), "both lanes' work on the batch branch: {files}");

    cleanup(&repo);
    cleanup(&shim);
}

#[test]
fn a_lane_whose_shared_process_dies_mid_turn_is_reported_as_failed() {
    let repo = init_repo("dies");
    let shim = shim_dir();

    let dying = serde_json::json!({ "writes": { "never.txt": "x" }, "summary": "dies", "exit_mid_turn": true }).to_string();
    let spawn = pact(
        &repo,
        &shim,
        &["spawn-many", "--agent", "copilot", "--runtime", "acp", "--name", "doomed", "--task", &dying],
    );
    assert!(!spawn.status.success(), "a lane whose agent process died must fail the batch:\nstdout: {}", stdout(&spawn));
    let text = format!("{}{}", stdout(&spawn), stderr(&spawn));
    assert!(text.contains("exited"), "the failure names the process exit:\n{text}");

    let record = run_record(&repo, "doomed");
    assert_eq!(record["runtime"], "acp");
    assert_eq!(record["exit_success"], false);
    assert!(record["summary"].as_str().unwrap().contains("exited"), "summary: {}", record["summary"]);
    let workspace = pact_vcs::WorkspaceManager::open(&repo).unwrap().get_workspace("doomed").unwrap();
    assert!(!workspace.path.join("never.txt").exists());
    assert!(workspace.agent_pid.is_none(), "no dangling pid after the process died");

    cleanup(&repo);
    cleanup(&shim);
}

#[test]
fn runtime_acp_dry_run_names_the_shared_process_and_refuses_agents_without_acp() {
    let repo = init_repo("dry-run");
    let shim = shim_dir();

    let dry = pact(
        &repo,
        &shim,
        &["spawn-many", "--agent", "copilot", "--runtime", "acp", "--task", &task(&[("a.txt", "a")], "a"), "--task", &task(&[("b.txt", "b")], "b"), "--dry-run"],
    );
    assert!(dry.status.success(), "dry-run failed: {}", stderr(&dry));
    assert!(
        stdout(&dry).contains("runtime: acp (1 shared agent process for 2 lanes: copilot"),
        "dry run names the process grouping:\n{}",
        stdout(&dry)
    );
    assert!(
        stdout(&dry).contains("each running agent reserves 400 MB against that, the default for the acp runtime"),
        "the memory reserve default follows the runtime (issue #332):\n{}",
        stdout(&dry)
    );
    assert!(pact_vcs::WorkspaceManager::open(&repo).unwrap().list_workspaces().unwrap().is_empty(), "dry-run creates nothing");

    // The default is `auto` (issue #337): a Copilot-only batch resolves to
    // acp and takes acp's reserve default; a claude batch resolves to
    // process; a mixed batch cannot share a process, so process.
    let plain = pact(&repo, &shim, &["spawn-many", "--agent", "copilot", "--task", &task(&[("a.txt", "a")], "a"), "--dry-run"]);
    assert!(stdout(&plain).contains("runtime: auto -> acp (1 shared agent process for 1 lane: copilot"), "auto picks acp for Copilot:\n{}", stdout(&plain));
    assert!(
        stdout(&plain).contains("each running agent reserves 400 MB against that, the default for the acp runtime"),
        "the reserve default follows the resolved runtime:\n{}",
        stdout(&plain)
    );
    let claude = pact(&repo, &shim, &["spawn-many", "--agent", "claude", "--task", "do a", "--dry-run"]);
    assert!(stdout(&claude).contains("runtime: auto -> process (one agent CLI process per lane)"), "auto falls back for claude:\n{}", stdout(&claude));
    assert!(
        stdout(&claude).contains("each running agent reserves 1200 MB against that, the default for the process runtime"),
        "process lanes keep the 1200 MB default:\n{}",
        stdout(&claude)
    );
    let mixed = pact(&repo, &shim, &["spawn-many", "--task", "claude:do a", "--task", "copilot:do b", "--dry-run"]);
    assert!(mixed.status.success(), "mixed dry run failed: {}", stderr(&mixed));
    assert!(stdout(&mixed).contains("runtime: auto -> process"), "a mixed batch cannot share one process:\n{}", stdout(&mixed));
    let forced = pact(&repo, &shim, &["spawn-many", "--agent", "copilot", "--runtime", "process", "--task", "do a", "--dry-run"]);
    assert!(stdout(&forced).contains("runtime: process (one agent CLI process per lane)"), "an explicit runtime prints without an arrow:\n{}", stdout(&forced));

    let explicit = pact(&repo, &shim, &["spawn-many", "--agent", "copilot", "--runtime", "acp", "--per-lane-reserve-mb", "900", "--task", &task(&[("a.txt", "a")], "a"), "--dry-run"]);
    assert!(
        stdout(&explicit).contains("each running agent reserves 900 MB against that), "),
        "an explicit reserve is reported without the default note:\n{}",
        stdout(&explicit)
    );

    let refused = pact(&repo, &shim, &["spawn-many", "--agent", "claude", "--runtime", "acp", "--task", &task(&[("a.txt", "a")], "a")]);
    assert!(!refused.status.success(), "claude has no ACP mode yet");
    let text = format!("{}{}", stdout(&refused), stderr(&refused));
    assert!(text.contains("has no Agent Client Protocol mode"), "got:\n{text}");

    let bad = pact(&repo, &shim, &["spawn-many", "--agent", "copilot", "--runtime", "threads", "--task", "x", "--dry-run"]);
    assert!(!bad.status.success());
    assert!(stderr(&bad).contains("--runtime: unknown value 'threads'"), "stderr: {}", stderr(&bad));

    cleanup(&repo);
    cleanup(&shim);
}

/// Issue #337: with no `--runtime` at all, a Copilot batch runs as ACP
/// sessions. Same proof as the explicit test: session ids from one counter.
#[test]
fn a_copilot_batch_runs_as_acp_sessions_by_default() {
    let repo = init_repo("auto-default");
    let shim = shim_dir();
    let spawn = pact(
        &repo,
        &shim,
        &["spawn-many", "--agent", "copilot", "--name", "lane-a", "--name", "lane-b", "--task", &task(&[("a.txt", "A")], "a"), "--task", &task(&[("b.txt", "B")], "b")],
    );
    assert!(spawn.status.success(), "spawn-many failed:\nstdout: {}\nstderr: {}", stdout(&spawn), stderr(&spawn));
    let manager = pact_vcs::WorkspaceManager::open(&repo).unwrap();
    let mut sessions: Vec<String> = manager.list_workspaces().unwrap().into_iter().filter_map(|w| w.acp_session).collect();
    sessions.sort();
    assert_eq!(sessions, vec!["sess-1", "sess-2"], "auto chose the ACP runtime without being asked");
    assert_eq!(run_record(&repo, "lane-a")["runtime"], "acp");
    cleanup(&repo);
    cleanup(&shim);
}

#[test]
fn single_spawn_supports_runtime_acp_too() {
    let repo = init_repo("single");
    let shim = shim_dir();

    let spawn = pact(
        &repo,
        &shim,
        &["spawn", "--agent", "copilot", "--runtime", "acp", "--name", "solo", &task(&[("solo.txt", "SOLO")], "solo")],
    );
    assert!(spawn.status.success(), "spawn failed:\nstdout: {}\nstderr: {}", stdout(&spawn), stderr(&spawn));
    let workspace = pact_vcs::WorkspaceManager::open(&repo).unwrap().get_workspace("solo").unwrap();
    assert_eq!(std::fs::read_to_string(workspace.path.join("solo.txt")).unwrap(), "SOLO");
    assert_eq!(workspace.acp_session.as_deref(), Some("sess-1"));
    assert_eq!(run_record(&repo, "solo")["runtime"], "acp");

    cleanup(&repo);
    cleanup(&shim);
}
