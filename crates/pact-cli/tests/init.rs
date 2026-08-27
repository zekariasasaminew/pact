//! Integration coverage for issue #118: `pact init` writes a pact.toml at
//! the repo root, refuses to overwrite one without --force, and its
//! `defaults.agent`/`defaults.safety` values are picked up as fallbacks by
//! `spawn`/`spawn-many`/`merge-all`/`resolve` when the equivalent flag is
//! omitted. Drives the real built `pact` binary against a real throwaway
//! repo, same pattern as `list_agent_pid.rs`.
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
    let root = std::env::temp_dir().join(format!("pact-cli-init-{name}-{}", Uuid::new_v4()));
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
fn init_writes_a_pact_toml_at_the_repo_root() {
    let repo = init_repo("writes-file");

    let output = run_pact(&repo, &["init"]);
    assert!(
        output.status.success(),
        "expected `pact init` to succeed, stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let config_path = repo.join("pact.toml");
    assert!(config_path.exists(), "expected pact.toml to be written at the repo root");
    let contents = std::fs::read_to_string(&config_path).unwrap();
    assert!(contents.contains("[defaults]"), "expected a [defaults] section, got: {contents}");

    cleanup(&repo);
}

#[test]
fn init_refuses_to_overwrite_an_existing_pact_toml_without_force() {
    let repo = init_repo("refuses-overwrite");

    let first = run_pact(&repo, &["init"]);
    assert!(first.status.success());

    let second = run_pact(&repo, &["init"]);
    assert!(!second.status.success(), "expected a re-run without --force to fail");
    let stderr = String::from_utf8_lossy(&second.stderr);
    assert!(stderr.contains("already exists"), "expected an 'already exists' error, got: {stderr}");
    assert!(stderr.contains("--force"), "expected the error to mention --force, got: {stderr}");

    cleanup(&repo);
}

#[test]
fn init_force_overwrites_an_existing_pact_toml() {
    let repo = init_repo("force-overwrites");

    assert!(run_pact(&repo, &["init"]).status.success());
    let output = run_pact(&repo, &["init", "--force"]);
    assert!(
        output.status.success(),
        "expected `pact init --force` to succeed, stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    cleanup(&repo);
}

/// Issue #219: `--register-skill` must never fail `pact init` itself,
/// regardless of whether any agent CLI with a confirmed registration
/// mechanism (today: only Copilot, via `copilot skill add`) happens to be
/// installed on the machine running this test -- `detect_installed_agents`
/// already gates whether that shell-out is even attempted, so this is a
/// no-op on a machine without `copilot` on PATH and a real (fast, free,
/// no-LLM-call) registration on one with it. Either way, `pact init`
/// itself must still succeed and still write pact.toml.
#[test]
fn init_register_skill_never_fails_the_command_regardless_of_detected_agents() {
    let repo = init_repo("register-skill");

    let output = run_pact(&repo, &["init", "--register-skill"]);
    assert!(
        output.status.success(),
        "expected `pact init --register-skill` to succeed either way, stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(repo.join("pact.toml").exists(), "expected pact.toml to still be written");

    cleanup(&repo);
}

#[test]
fn spawn_dry_run_picks_up_pact_toml_defaults_agent_when_flag_omitted() {
    let repo = init_repo("spawn-picks-up-config");
    std::fs::write(repo.join("pact.toml"), "[defaults]\nagent = \"copilot\"\n").unwrap();

    let output = run_pact(&repo, &["spawn", "--dry-run", "a test task"]);
    assert!(
        output.status.success(),
        "expected spawn --dry-run to succeed, stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("copilot"),
        "expected the copilot adapter's command to be previewed since pact.toml sets defaults.agent, got: {stdout}"
    );

    cleanup(&repo);
}

#[test]
fn spawn_dry_run_still_defaults_to_claude_with_no_pact_toml_and_no_flag() {
    let repo = init_repo("spawn-default-claude");

    let output = run_pact(&repo, &["spawn", "--dry-run", "a test task"]);
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("claude"),
        "expected the pre-existing hardcoded claude default to still apply, got: {stdout}"
    );

    cleanup(&repo);
}

#[test]
fn an_explicit_agent_flag_overrides_pact_toml() {
    let repo = init_repo("flag-overrides-config");
    std::fs::write(repo.join("pact.toml"), "[defaults]\nagent = \"copilot\"\n").unwrap();

    let output = run_pact(&repo, &["spawn", "--dry-run", "--agent", "claude", "a test task"]);
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("claude"),
        "expected --agent claude to override pact.toml's copilot default, got: {stdout}"
    );

    cleanup(&repo);
}

#[test]
fn a_malformed_pact_toml_is_a_hard_error_not_a_silent_fallback() {
    let repo = init_repo("malformed-config");
    std::fs::write(repo.join("pact.toml"), "this is not [ valid toml").unwrap();

    let output = run_pact(&repo, &["spawn", "--dry-run", "a test task"]);
    assert!(!output.status.success(), "expected a malformed pact.toml to fail loudly, not fall back silently");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("pact.toml"), "expected the error to mention pact.toml, got: {stderr}");

    cleanup(&repo);
}
