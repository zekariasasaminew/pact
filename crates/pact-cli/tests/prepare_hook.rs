//! End-to-end coverage for repo-declared prepare commands (issue #301):
//! a fresh worktree has only what git tracks, so repos that depend on
//! generated, gitignored files declare the command that regenerates them
//! and pact runs it in every new workspace after dependency prep, and in
//! `merge-all`'s integration worktree before the gate. Same fake-agent
//! harness as `fake_agent_e2e.rs` (the JSONL fake as `claude`).

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use uuid::Uuid;

fn run_git(dir: &Path, args: &[&str]) {
    let output = Command::new("git").args(args).current_dir(dir).output().unwrap();
    assert!(output.status.success(), "`git {}` failed: {}", args.join(" "), String::from_utf8_lossy(&output.stderr));
}

fn init_repo(name: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("pact-cli-prepare-{name}-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    run_git(&root, &["init", "-q"]);
    run_git(&root, &["config", "user.email", "test@test.com"]);
    run_git(&root, &["config", "user.name", "test"]);
    std::fs::write(root.join("README.md"), "# demo\n").unwrap();
    // The generated file is ignored, as `next-env.d.ts` or a Prisma client would be.
    std::fs::write(root.join(".gitignore"), "generated.txt\n").unwrap();
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

fn shim_dir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("pact-cli-prepare-shim-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let fake_agent = PathBuf::from(env!("CARGO_BIN_EXE_fake_agent"));
    let dest = if cfg!(windows) { dir.join("claude.exe") } else { dir.join("claude") };
    std::fs::copy(&fake_agent, &dest).unwrap();
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

fn script(writes: &[(&str, &str)], summary: &str) -> String {
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
        .unwrap()
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).to_string()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).to_string()
}

/// Stands in for `npx next typegen`: produces the gitignored file.
fn generate_cmd() -> &'static str {
    if cfg!(windows) { "echo generated> generated.txt" } else { "echo generated > generated.txt" }
}

/// A gate that only passes where the generated file exists.
fn needs_generated_cmd() -> &'static str {
    if cfg!(windows) { "if exist generated.txt (exit 0) else (exit 1)" } else { "[ -f generated.txt ]" }
}

fn workspace_id(output: &Output) -> String {
    stdout(output).lines().find_map(|l| l.strip_prefix("workspace ")).map(|rest| rest.split(' ').next().unwrap().to_string()).expect("workspace line")
}

#[test]
fn prepare_runs_in_every_new_workspace_after_dependency_prep_and_is_recorded() {
    let repo = init_repo("spawn");
    let shim = shim_dir();
    let spawn = pact(&repo, &shim, &["spawn", "--agent", "claude", "--prepare", generate_cmd(), &script(&[("a.txt", "A")], "a")]);
    assert!(spawn.status.success(), "spawn failed:\nstdout: {}\nstderr: {}", stdout(&spawn), stderr(&spawn));
    let text = stdout(&spawn);
    assert!(text.contains(&format!("[phase] prepare: {}", generate_cmd())), "{text}");
    assert!(text.contains("[phase] prepare done in"), "{text}");
    let id = workspace_id(&spawn);
    let workspace = pact_vcs::WorkspaceManager::open(&repo).unwrap().get_workspace(&id).unwrap();
    assert_eq!(std::fs::read_to_string(workspace.path.join("generated.txt")).unwrap().trim(), "generated", "the prepare command ran in the worktree");
    assert!(!repo.join("generated.txt").exists(), "and not in the repo root");

    let inspect = pact(&repo, &shim, &["inspect", &id]);
    assert!(stdout(&inspect).contains("prepare commands (issue #301):") && stdout(&inspect).contains("[ok]"), "{}", stdout(&inspect));

    cleanup(&repo);
    cleanup(&shim);
}

#[test]
fn a_failing_prepare_command_warns_and_the_spawn_still_runs() {
    let repo = init_repo("failing");
    let shim = shim_dir();
    let spawn = pact(&repo, &shim, &["spawn", "--agent", "claude", "--prepare", "exit 7", "--prepare", generate_cmd(), &script(&[("a.txt", "A")], "a")]);
    assert!(spawn.status.success(), "a failed prepare must not fail the spawn:\nstdout: {}\nstderr: {}", stdout(&spawn), stderr(&spawn));
    let text = stdout(&spawn);
    assert!(text.contains("[phase] prepare FAILED (exit 7): exit 7"), "{text}");
    let id = workspace_id(&spawn);
    let workspace = pact_vcs::WorkspaceManager::open(&repo).unwrap().get_workspace(&id).unwrap();
    assert!(workspace.path.join("generated.txt").exists(), "the next command still ran");
    assert!(workspace.path.join("a.txt").exists(), "and so did the agent");
    let inspect = pact(&repo, &shim, &["inspect", &id]);
    assert!(stdout(&inspect).contains("`exit 7` [failed]"), "{}", stdout(&inspect));
    cleanup(&repo);
    cleanup(&shim);
}

#[test]
fn pact_toml_defaults_prepare_applies_and_the_flag_replaces_it() {
    let repo = init_repo("config");
    let shim = shim_dir();
    std::fs::write(repo.join("pact.toml"), format!("[defaults]\nprepare = [{:?}]\n", generate_cmd())).unwrap();
    run_git(&repo, &["add", "-A"]);
    run_git(&repo, &["commit", "-q", "-m", "config"]);

    let dry = pact(&repo, &shim, &["spawn", "--agent", "claude", "--dry-run", "x"]);
    assert!(stdout(&dry).contains(&format!("prepare: `{}`", generate_cmd())), "the config default shows in a dry run:\n{}", stdout(&dry));
    let spawn = pact(&repo, &shim, &["spawn", "--agent", "claude", &script(&[("a.txt", "A")], "a")]);
    assert!(spawn.status.success(), "{}", stderr(&spawn));
    let workspace = pact_vcs::WorkspaceManager::open(&repo).unwrap().get_workspace(&workspace_id(&spawn)).unwrap();
    assert!(workspace.path.join("generated.txt").exists(), "the config default ran");

    let replaced = pact(&repo, &shim, &["spawn", "--agent", "claude", "--prepare", "exit 0", "--dry-run", "x"]);
    assert!(stdout(&replaced).contains("prepare: `exit 0`") && !stdout(&replaced).contains("generated.txt"), "the flag replaces the config list:\n{}", stdout(&replaced));
    let none = pact(&repo, &shim, &["spawn-many", "--agent", "claude", "--task", "x", "--dry-run"]);
    assert!(stdout(&none).contains(&format!("prepare: `{}`", generate_cmd())), "spawn-many reads the same default:\n{}", stdout(&none));

    cleanup(&repo);
    cleanup(&shim);
}

#[test]
fn merge_all_prepares_the_integration_worktree_before_the_gate() {
    let repo = init_repo("merge");
    let shim = shim_dir();
    let spawn = pact(&repo, &shim, &["spawn-many", "--agent", "claude", "--task", &script(&[("a.txt", "A")], "a"), "--task", &script(&[("b.txt", "B")], "b")]);
    assert!(spawn.status.success(), "{}", stderr(&spawn));

    // Without the prepare command the gate cannot pass anywhere: the
    // #232 preflight on the base fails and merge-all aborts.
    let bare = pact(&repo, &shim, &["merge-all", "--require-passing-tests", needs_generated_cmd()]);
    assert!(!bare.status.success(), "the gate needs the generated file:\n{}", stdout(&bare));

    let prepared = pact(&repo, &shim, &["merge-all", "--require-passing-tests", needs_generated_cmd(), "--prepare", generate_cmd()]);
    assert!(prepared.status.success(), "with --prepare the integration worktree is a working project:\nstdout: {}\nstderr: {}", stdout(&prepared), stderr(&prepared));
    assert_eq!(stdout(&prepared).matches("merged  ").count(), 2, "{}", stdout(&prepared));

    cleanup(&repo);
    cleanup(&shim);
}
