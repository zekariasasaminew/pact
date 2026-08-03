//! Integration coverage for issue #121: when --agent is omitted and
//! pact.toml sets no default, spawn/spawn-many fall back to the
//! detected agent CLI if exactly one is installed. Overrides PATH for
//! the child `pact` process to a directory containing one fake
//! `claude` executable (a real cross-platform shim, not a mock of
//! pact's own detection logic), so `pact doctor`'s own detection
//! genuinely finds exactly one agent CLI, the same way it would on a
//! real machine that only has Claude Code installed.
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
    let root = std::env::temp_dir().join(format!("pact-cli-agent-auto-default-{name}-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    run_git(&root, &["init", "-q"]);
    run_git(&root, &["config", "user.email", "test@test.com"]);
    run_git(&root, &["config", "user.name", "test"]);
    std::fs::write(root.join("README.md"), "# demo\n").unwrap();
    run_git(&root, &["add", "-A"]);
    run_git(&root, &["commit", "-q", "-m", "init"]);
    root
}

fn cleanup(paths: &[&Path]) {
    for path in paths {
        let _ = std::fs::remove_dir_all(path);
        if let Ok(state_dir) = pact_vcs::WorkspaceManager::state_dir_for(path) {
            let _ = std::fs::remove_dir_all(state_dir);
        }
    }
}

/// Writes a fake `claude` executable into a fresh temp directory and
/// returns that directory -- setting the child process's PATH to
/// exactly this directory means only `claude` resolves, nothing else.
fn fake_claude_only_path() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("pact-fake-claude-bin-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();

    #[cfg(windows)]
    {
        std::fs::write(dir.join("claude.cmd"), "@echo claude version 1.2.3\r\n").unwrap();
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let script_path = dir.join("claude");
        std::fs::write(&script_path, "#!/bin/sh\necho \"claude version 1.2.3\"\n").unwrap();
        let mut perms = std::fs::metadata(&script_path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&script_path, perms).unwrap();
    }

    dir
}

#[test]
fn spawn_dry_run_auto_selects_the_sole_detected_agent() {
    let repo = init_repo("sole-agent");
    let fake_bin = fake_claude_only_path();

    let output = Command::new(env!("CARGO_BIN_EXE_pact"))
        .args(["--repo"])
        .arg(&repo)
        .args(["spawn", "--dry-run", "a test task"])
        .env("PATH", &fake_bin)
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "expected spawn --dry-run to succeed, stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stdout.contains("claude"), "expected the claude adapter's command to be previewed, got: {stdout}");
    assert!(
        stderr.contains("using detected agent CLI: claude"),
        "expected an explicit note that detection chose claude, got stderr: {stderr}"
    );

    cleanup(&[&repo, &fake_bin]);
}

#[test]
fn an_explicit_agent_flag_still_overrides_auto_detection() {
    let repo = init_repo("flag-overrides-detection");
    let fake_bin = fake_claude_only_path();

    // Only `claude` is on PATH, but --agent copilot is given explicitly --
    // detection must never override an explicit flag, even one naming an
    // agent that isn't (as far as this PATH can tell) actually installed.
    let output = Command::new(env!("CARGO_BIN_EXE_pact"))
        .args(["--repo"])
        .arg(&repo)
        .args(["spawn", "--dry-run", "--agent", "copilot", "a test task"])
        .env("PATH", &fake_bin)
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stdout.contains("copilot"), "expected the explicit --agent copilot to be honored, got: {stdout}");
    assert!(
        !stderr.contains("using detected agent CLI"),
        "an explicit --agent must never trigger the auto-detect message, got stderr: {stderr}"
    );

    cleanup(&[&repo, &fake_bin]);
}
