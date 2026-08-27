//! The "slow integration" tier issue #240 asked for: tests that are
//! deliberately expensive (a real N=5 concurrent spawn, a real npm
//! install, real dependency-prep contention) rather than shaped to finish
//! fast. Marked `#[ignore]` -- run explicitly with `cargo test --ignored`
//! or `cargo test -- --include-ignored`, not part of the default `cargo
//! test --workspace` most contributors and CI run on every push. See
//! DESIGN.md ("pact-cli > Slow integration tier (issue #240)") for why
//! this tier exists and what it's for.
//!
//! Issue #11 (renumbered #240)'s own critique, verified against this
//! project's prior test suite before this file existed: `require_passing_
//! tests` was covered only by `true`/`false` fixtures that never needed
//! any dependency, nothing exercised real concurrent `npm ci` contention,
//! and nothing asserted N spawn-many tasks produce N workspaces under real
//! (not simulated) contention. This file's single test exercises the
//! real, reported failure shape end to end: N=5 concurrent tasks, a real
//! (tiny, no-network) `npm ci` in each workspace against npm's own shared
//! global cache (issue #233 removed pact's own custom content store in
//! favor of it), and a dependency-requiring `--require-passing-tests`
//! gate -- the exact combination the original production report hit.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

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
    let root = std::env::temp_dir().join(format!("pact-cli-slow-integration-{name}-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    run_git(&root, &["init", "-q"]);
    run_git(&root, &["config", "user.email", "test@test.com"]);
    run_git(&root, &["config", "user.name", "test"]);
    // Zero-dependency package.json/package-lock.json -- a real `npm ci`
    // against this needs no network access and stays fast, while still
    // exercising the real `npm ci` path, unlike every prior
    // `require_passing_tests` fixture (issue #11/#240's own complaint: a
    // fixture that needs nothing can't catch a "needs dependencies" bug).
    std::fs::write(root.join("package.json"), "{\"name\":\"scratch\",\"version\":\"1.0.0\"}").unwrap();
    std::fs::write(
        root.join("package-lock.json"),
        "{\"name\":\"scratch\",\"version\":\"1.0.0\",\"lockfileVersion\":3,\"packages\":{\"\":{\"name\":\"scratch\",\"version\":\"1.0.0\"}}}",
    )
    .unwrap();
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

fn shim_dir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("pact-cli-slow-integration-shim-{}", Uuid::new_v4()));
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
        .unwrap_or_else(|err| panic!("failed to spawn `pact {}`: {err}", args.join(" ")))
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).to_string()
}

fn state_dir_for(repo: &Path) -> PathBuf {
    repo.parent().unwrap().join(format!(".pact-{}", repo.file_name().unwrap().to_string_lossy()))
}

/// The real, reported failure shape end to end: 5 concurrent tasks
/// against a repo with real (if trivial) npm dependencies. Exercises
/// issue #230 (count fidelity under real concurrency, real `git worktree
/// add` contention, not simulated), #233 (each of the 5 concurrent tasks
/// runs its own real `npm ci` against npm's own shared global cache --
/// proving that's safe under real concurrent load, with no pact-side
/// locking, the same thing verified by hand before removing the old
/// custom content store), and #232 (a merge gate that genuinely needs
/// dependencies correctly gets an environment diagnosis, not a false
/// per-workspace "your code failed" -- since `merge_all`'s integration
/// worktree still has no dependency prep performed on it, a still-open,
/// deliberately deferred gap documented in DESIGN.md) -- all in one real
/// run.
#[test]
#[ignore = "slow: 5 concurrent workspaces + a real npm ci -- run explicitly with `cargo test --ignored`"]
fn five_concurrent_tasks_each_run_a_real_npm_ci_under_real_contention() {
    let repo = init_repo("five-way");
    let shim = shim_dir();

    const N: usize = 5;
    let tasks: Vec<String> = (0..N).map(|i| script(&[(&format!("file{i}.txt"), "content")], "wrote a file")).collect();
    let mut args = vec!["spawn-many".to_string(), "--agent".to_string(), "claude".to_string()];
    for task in &tasks {
        args.push("--task".to_string());
        args.push(task.clone());
    }
    let args_ref: Vec<&str> = args.iter().map(String::as_str).collect();

    let spawn = pact(&repo, &shim, &args_ref);
    assert!(spawn.status.success(), "stdout: {}\nstderr: {}", stdout(&spawn), String::from_utf8_lossy(&spawn.stderr));
    let spawn_text = stdout(&spawn);
    let workspace_lines: Vec<&str> = spawn_text.lines().filter(|l| l.starts_with("workspace ")).collect();
    assert_eq!(workspace_lines.len(), N, "expected {N} workspaces from {N} tasks, got:\n{spawn_text}");
    assert!(
        spawn_text.contains(&format!("spawn-many: {N} tasks requested, {N} workspaces created, 0 failed")),
        "expected the issue #231 reconciliation line to confirm full count fidelity, got:\n{spawn_text}"
    );

    // issue #233: every one of the N concurrent tasks runs its own real
    // `npm ci` (no shared staging/materialization step anymore) against
    // npm's own global cache -- all N must succeed under real concurrent
    // contention, with npm's own cache locking, not pact's.
    let deps_dir = state_dir_for(&repo).join("meta");
    let mut npm_ci_success_count = 0;
    for entry in std::fs::read_dir(&deps_dir).unwrap().filter_map(|e| e.ok()) {
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.ends_with("-deps.json") {
            continue;
        }
        let reports: Vec<serde_json::Value> = serde_json::from_str(&std::fs::read_to_string(entry.path()).unwrap()).unwrap();
        for report in reports {
            if report["manager"] == "npm" {
                assert_eq!(report["strategy"], "npm-ci", "expected the lockfile-present path, got: {report}");
                assert_eq!(report["success"], true, "expected npm ci to succeed under real concurrency, got: {report}");
                npm_ci_success_count += 1;
            }
        }
    }
    assert_eq!(npm_ci_success_count, N, "expected all {N} concurrent tasks to run and report a successful npm ci");

    // issue #232, honest end-to-end check: `merge_all`'s integration
    // worktree never gets dependency prep performed on it (a known,
    // deliberately deferred gap -- see DESIGN.md), so a gate that
    // genuinely needs node_modules can't pass there regardless of
    // which workspace merges. The correct, fixed behavior is a *single*
    // preflight abort with an environment diagnosis before any workspace
    // is even considered -- not #232's pre-fix bug (every workspace
    // individually reported as "failed the required test command").
    let gate = if cfg!(windows) {
        "if exist node_modules (exit 0) else (exit 1)"
    } else {
        "test -d node_modules"
    };
    let merge = pact(&repo, &shim, &["merge-all", "--require-passing-tests", gate]);
    assert_eq!(merge.status.code(), Some(1), "expected the preflight to abort with an environment diagnosis, not per-workspace skips");
    let merge_stderr = String::from_utf8_lossy(&merge.stderr);
    assert!(merge_stderr.contains("unmodified base commit"), "expected issue #232's environment diagnosis, got: {merge_stderr}");

    cleanup(&repo);
    cleanup(&shim);
}
