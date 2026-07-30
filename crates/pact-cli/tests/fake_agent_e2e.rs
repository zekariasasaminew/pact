//! End-to-end tests driving the real `pact` binary against a real git repo
//! with a real (fake) agent subprocess on `PATH` -- see DESIGN.md
//! ("pact-cli > fake-agent end-to-end harness", issue #157). Unlike this
//! project's existing unit/integration tests, which stub out agent
//! invocation entirely (e.g. `ArbiterResolver` closures) or never spawn a
//! process at all, these exercise the actual `spawn -> stream stdout ->
//! parse_line -> commit -> merge/conflict -> teardown` loop end to end, the
//! same way a real `claude` CLI install would, just without the cost or
//! flakiness of a real agent call.
//!
//! `fake_agent` (this package's second `[[bin]]`, auto-discovered from
//! `src/bin/fake_agent.rs`) is copied onto a scratch `PATH` entry under the
//! name `claude`/`claude.exe` for each test, so `pact --agent claude`
//! launches it exactly as it would the real CLI.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

use pact_core::agent_process_alive;
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
    let root = std::env::temp_dir().join(format!("pact-cli-fake-agent-{name}-{}", Uuid::new_v4()));
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

/// A scratch `PATH` entry containing a copy of `fake_agent`'s compiled
/// binary named `claude`/`claude.exe` -- what makes `pact --agent claude`
/// launch the fake agent instead of trying (and failing) to find a real
/// install.
fn shim_dir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("pact-cli-fake-agent-shim-{}", Uuid::new_v4()));
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

/// One fake-agent script: which file(s) to write (relative to the
/// workspace's worktree) and what result to report -- see
/// `src/bin/fake_agent.rs`'s `Script` for the exact shape this must match.
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

fn workspace_id_from_spawn_output(output: &Output) -> String {
    let text = stdout(output);
    let line = text
        .lines()
        .find(|l| l.starts_with("workspace "))
        .unwrap_or_else(|| panic!("no 'workspace <id>' line in output:\n{text}"));
    line.split_whitespace().nth(1).unwrap().to_string()
}

#[test]
fn spawn_runs_a_fake_agent_and_lands_its_scripted_edit() {
    let repo = init_repo("spawn-basic");
    let shim = shim_dir();

    let task = script(&[("hello.txt", "hello from a fake agent")], "created hello.txt");
    let output = pact(&repo, &shim, &["spawn", &task, "--agent", "claude"]);
    assert!(output.status.success(), "stdout: {}\nstderr: {}", stdout(&output), String::from_utf8_lossy(&output.stderr));

    let id = workspace_id_from_spawn_output(&output);
    let workspace_dir = {
        let list = pact(&repo, &shim, &["list"]);
        let text = stdout(&list);
        let path_line = text.lines().find(|l| l.starts_with(&id)).unwrap();
        PathBuf::from(path_line.split_whitespace().nth(2).unwrap())
    };
    assert_eq!(std::fs::read_to_string(workspace_dir.join("hello.txt")).unwrap(), "hello from a fake agent");

    cleanup(&repo);
    cleanup(&shim);
}

/// Regression test for issue #212, outside Windows Copilot report: a task
/// that reports success but touches zero files (a real shape -- an agent
/// that was told a target file must already exist, found it missing, and
/// silently reported success instead of failing loudly) must not look
/// identical to a normal clean run in `pact list`. Ground-truth
/// `files_touched` (via real `git status`), not the agent's own claimed
/// success, is what this distinguishes on.
#[test]
fn spawn_that_writes_nothing_is_flagged_as_no_files_touched_in_list() {
    let repo = init_repo("no-files-touched");
    let shim = shim_dir();

    let noop_task = script(&[], "reported success without doing anything");
    let spawn = pact(&repo, &shim, &["spawn", &noop_task, "--agent", "claude"]);
    assert!(spawn.status.success(), "stdout: {}\nstderr: {}", stdout(&spawn), String::from_utf8_lossy(&spawn.stderr));
    let id = workspace_id_from_spawn_output(&spawn);

    let list = pact(&repo, &shim, &["list"]);
    let list_text = stdout(&list);
    let workspace_line = list_text.lines().find(|l| l.starts_with(&id)).unwrap();
    assert!(
        workspace_line.contains("[clean, no files touched]"),
        "expected a distinct no-op signal, got: {workspace_line}"
    );

    cleanup(&repo);
    cleanup(&shim);
}

/// Contrast case for the test above: a spawn that genuinely writes a file
/// must show plain `[clean]`, not the no-op annotation -- confirms the new
/// signal doesn't fire on every successful run, only a real no-op one.
#[test]
fn spawn_that_writes_a_file_shows_plain_clean_in_list() {
    let repo = init_repo("files-touched");
    let shim = shim_dir();

    let real_task = script(&[("hello.txt", "hello")], "created hello.txt");
    let spawn = pact(&repo, &shim, &["spawn", &real_task, "--agent", "claude"]);
    assert!(spawn.status.success(), "stdout: {}\nstderr: {}", stdout(&spawn), String::from_utf8_lossy(&spawn.stderr));
    let id = workspace_id_from_spawn_output(&spawn);

    let list = pact(&repo, &shim, &["list"]);
    let list_text = stdout(&list);
    let workspace_line = list_text.lines().find(|l| l.starts_with(&id)).unwrap();
    assert!(workspace_line.contains("[dirty]"), "expected a real file write to show dirty, got: {workspace_line}");
    assert!(!workspace_line.contains("no files touched"), "got: {workspace_line}");

    cleanup(&repo);
    cleanup(&shim);
}

/// Regression test for issue #214, outside Windows Copilot report: `teardown`
/// had no bulk mode (unlike `commit-all`, which already supported "every
/// active workspace" when `--id` is omitted). Confirms `teardown` with no id
/// removes every active workspace, mirroring `commit-all`'s exact pattern.
#[test]
fn teardown_with_no_id_removes_every_active_workspace() {
    let repo = init_repo("teardown-bulk");
    let shim = shim_dir();

    let task_a = script(&[("alpha.txt", "ALPHA")], "created alpha.txt");
    let task_b = script(&[("beta.txt", "BETA")], "created beta.txt");
    let spawn = pact(
        &repo,
        &shim,
        &["spawn-many", "--agent", "claude", "--task", &task_a, "--task", &task_b],
    );
    assert!(spawn.status.success(), "spawn-many failed: {}", String::from_utf8_lossy(&spawn.stderr));

    let teardown = pact(&repo, &shim, &["teardown", "--force"]);
    assert!(
        teardown.status.success(),
        "stdout: {}\nstderr: {}",
        stdout(&teardown),
        String::from_utf8_lossy(&teardown.stderr)
    );
    let teardown_text = stdout(&teardown);
    assert_eq!(
        teardown_text.matches("removed workspace ").count(),
        2,
        "expected both workspaces torn down, got: {teardown_text}"
    );

    let list = pact(&repo, &shim, &["list"]);
    assert!(stdout(&list).contains("no active workspaces"), "got: {}", stdout(&list));

    cleanup(&repo);
    cleanup(&shim);
}

/// A dirty workspace without `--force` among a bulk teardown must be
/// reported and skipped, not abort the rest of the batch -- same
/// "report and continue" shape as `commit-all`.
#[test]
fn teardown_with_no_id_reports_a_dirty_workspace_but_continues_the_batch() {
    let repo = init_repo("teardown-bulk-partial-failure");
    let shim = shim_dir();

    let task_a = script(&[("alpha.txt", "ALPHA")], "created alpha.txt");
    let task_b = script(&[("beta.txt", "BETA")], "created beta.txt");
    let spawn = pact(
        &repo,
        &shim,
        &["spawn-many", "--agent", "claude", "--task", &task_a, "--task", &task_b],
    );
    assert!(spawn.status.success(), "spawn-many failed: {}", String::from_utf8_lossy(&spawn.stderr));

    // No --force: teardown refuses on a dirty workspace (both are dirty,
    // since a fake-agent write is never auto-committed), so this exercises
    // the "report and continue" path for every workspace in the batch.
    let teardown = pact(&repo, &shim, &["teardown"]);
    assert_eq!(teardown.status.code(), Some(1), "expected exit 1 since every workspace is dirty");
    let teardown_text = stdout(&teardown);
    assert_eq!(
        teardown_text.matches("failed to tear down").count(),
        2,
        "expected both dirty workspaces reported as failed, got: {teardown_text}"
    );

    let list = pact(&repo, &shim, &["list"]);
    assert!(!stdout(&list).contains("no active workspaces"), "expected both workspaces to still be active");

    cleanup(&repo);
    cleanup(&shim);
}

#[test]
fn spawn_many_runs_two_fake_agents_concurrently() {
    let repo = init_repo("spawn-many");
    let shim = shim_dir();

    let task_a = script(&[("alpha.txt", "ALPHA")], "created alpha.txt");
    let task_b = script(&[("beta.txt", "BETA")], "created beta.txt");
    let output = pact(
        &repo,
        &shim,
        &["spawn-many", "--agent", "claude", "--task", &task_a, "--task", &task_b],
    );
    assert!(output.status.success(), "stdout: {}\nstderr: {}", stdout(&output), String::from_utf8_lossy(&output.stderr));

    let text = stdout(&output);
    let workspace_lines: Vec<&str> = text.lines().filter(|l| l.starts_with("workspace ")).collect();
    assert_eq!(workspace_lines.len(), 2, "expected 2 workspaces, got:\n{text}");

    let list = pact(&repo, &shim, &["list"]);
    let list_text = stdout(&list);
    let mut found_alpha = false;
    let mut found_beta = false;
    for id in workspace_lines.iter().map(|l| l.split_whitespace().nth(1).unwrap()) {
        let path_line = list_text.lines().find(|l| l.starts_with(id)).unwrap();
        let path = PathBuf::from(path_line.split_whitespace().nth(2).unwrap());
        if path.join("alpha.txt").exists() {
            assert_eq!(std::fs::read_to_string(path.join("alpha.txt")).unwrap(), "ALPHA");
            found_alpha = true;
        }
        if path.join("beta.txt").exists() {
            assert_eq!(std::fs::read_to_string(path.join("beta.txt")).unwrap(), "BETA");
            found_beta = true;
        }
    }
    assert!(found_alpha && found_beta, "expected both scripted edits to land in their own workspace");

    cleanup(&repo);
    cleanup(&shim);
}

#[test]
fn merge_all_merges_two_non_conflicting_fake_agent_workspaces() {
    let repo = init_repo("merge-clean");
    let shim = shim_dir();

    let task_a = script(&[("alpha.txt", "ALPHA")], "created alpha.txt");
    let task_b = script(&[("beta.txt", "BETA")], "created beta.txt");
    let spawn = pact(
        &repo,
        &shim,
        &["spawn-many", "--agent", "claude", "--task", &task_a, "--task", &task_b],
    );
    assert!(spawn.status.success(), "spawn-many failed: {}", String::from_utf8_lossy(&spawn.stderr));

    let merge = pact(&repo, &shim, &["merge-all"]);
    assert!(merge.status.success(), "stdout: {}\nstderr: {}", stdout(&merge), String::from_utf8_lossy(&merge.stderr));

    let branches = Command::new("git").args(["branch", "--list", "pact/merged-*"]).current_dir(&repo).output().unwrap();
    let branch_list = String::from_utf8_lossy(&branches.stdout);
    let target = branch_list.trim().trim_start_matches('*').trim();
    assert!(!target.is_empty(), "expected a pact/merged-* branch, got: {branch_list}");

    for (file, expected) in [("alpha.txt", "ALPHA"), ("beta.txt", "BETA")] {
        let show = Command::new("git").args(["show", &format!("{target}:{file}")]).current_dir(&repo).output().unwrap();
        assert!(show.status.success(), "{file} missing from merged branch: {}", String::from_utf8_lossy(&show.stderr));
        assert_eq!(String::from_utf8_lossy(&show.stdout).trim(), expected);
    }

    cleanup(&repo);
    cleanup(&shim);
}

#[test]
fn merge_all_detects_and_persists_a_real_conflict_between_two_fake_agents() {
    let repo = init_repo("merge-conflict");
    run_git(&repo, &["checkout", "-b", "main-work"]);
    std::fs::write(repo.join("shared.txt"), "base\n").unwrap();
    run_git(&repo, &["add", "-A"]);
    run_git(&repo, &["commit", "-q", "-m", "add shared.txt"]);
    let shim = shim_dir();

    let task_a = script(&[("shared.txt", "agent A's version\n")], "edited shared.txt (A)");
    let task_b = script(&[("shared.txt", "agent B's version\n")], "edited shared.txt (B)");
    let spawn = pact(
        &repo,
        &shim,
        &["spawn-many", "--agent", "claude", "--task", &task_a, "--task", &task_b],
    );
    assert!(spawn.status.success(), "spawn-many failed: {}", String::from_utf8_lossy(&spawn.stderr));

    let merge = pact(&repo, &shim, &["merge-all"]);
    assert_eq!(merge.status.code(), Some(2), "expected exit 2 (skipped) for a real conflict, got: {:?}\nstdout: {}", merge.status.code(), stdout(&merge));
    let merge_text = stdout(&merge);
    assert!(merge_text.contains("skipped -- needs a human:"), "got: {merge_text}");
    assert!(merge_text.contains("shared.txt") || merge_text.contains("pact resolve"), "got: {merge_text}");

    let resolve = pact(&repo, &shim, &["resolve"]);
    let resolve_text = stdout(&resolve);
    assert!(resolve_text.contains("open conflicts:"), "got: {resolve_text}");
    assert!(resolve_text.contains("shared.txt"), "got: {resolve_text}");

    cleanup(&repo);
    cleanup(&shim);
}

fn state_dir_for(repo: &Path) -> PathBuf {
    repo.parent()
        .unwrap()
        .join(format!(".pact-{}", repo.file_name().unwrap().to_string_lossy()))
}

/// Regression test for issue #147's remaining Arbiter scope guard: a
/// lockfile needs the real package manager to regenerate it, not a
/// hand-written merge, so Arbiter must refuse one outright rather than
/// asking a real agent to resolve it. Confirms both the refusal (merge-all
/// still reports it skipped) and that the refusal happens *before* ever
/// spawning a real agent process -- no `arbiter-*.jsonl` log, which
/// `run_and_stream` only creates once a process actually starts.
#[test]
fn merge_all_refuses_to_let_arbiter_touch_a_conflicted_lockfile() {
    let repo = init_repo("arbiter-lockfile");
    run_git(&repo, &["checkout", "-b", "main-work"]);
    std::fs::write(repo.join("package-lock.json"), "{\n  \"lockfileVersion\": 1,\n  \"base\": true\n}\n").unwrap();
    run_git(&repo, &["add", "-A"]);
    run_git(&repo, &["commit", "-q", "-m", "add lockfile"]);
    let shim = shim_dir();

    let task_a = script(&[("package-lock.json", "{\n  \"lockfileVersion\": 1,\n  \"a\": true\n}\n")], "edited lockfile (A)");
    let task_b = script(&[("package-lock.json", "{\n  \"lockfileVersion\": 1,\n  \"b\": true\n}\n")], "edited lockfile (B)");
    let spawn = pact(&repo, &shim, &["spawn-many", "--agent", "claude", "--task", &task_a, "--task", &task_b]);
    assert!(spawn.status.success(), "spawn-many failed: {}", String::from_utf8_lossy(&spawn.stderr));

    let pass_cmd = if cfg!(windows) { "exit 0" } else { "true" };
    let merge = pact(&repo, &shim, &["merge-all", "--test-cmd", pass_cmd, "--arbiter-agent", "claude"]);
    assert_eq!(
        merge.status.code(),
        Some(2),
        "expected exit 2 (skipped) since arbiter must refuse the lockfile, got: {:?}\nstdout: {}",
        merge.status.code(),
        stdout(&merge)
    );
    assert!(stdout(&merge).contains("package-lock.json"), "got: {}", stdout(&merge));

    let logs_dir = state_dir_for(&repo).join("logs");
    let arbiter_log_exists = std::fs::read_dir(&logs_dir)
        .map(|entries| {
            entries
                .filter_map(|e| e.ok())
                .any(|e| e.file_name().to_string_lossy().starts_with("arbiter-") && e.file_name().to_string_lossy().ends_with(".jsonl"))
        })
        .unwrap_or(false);
    assert!(!arbiter_log_exists, "expected no arbiter-*.jsonl log -- a real agent process should never have been spawned for a lockfile");

    cleanup(&repo);
    cleanup(&shim);
}

/// Regression test for issue #178 (backfilled per #71, once this
/// harness existed to make it possible): `list_workspaces` used to crash
/// on the `-deps.json` sidecar file dependency prep writes alongside a
/// workspace's own `meta/<id>.json`, since nothing before this harness
/// ever drove a real `spawn -> (real dependency prep) -> list` round
/// trip -- every prior test either stubbed the agent out entirely or
/// never touched a real package manager. A zero-dependency `package.json`
/// with no lockfile takes the "plain-install-no-lockfile" prep strategy
/// (a real `npm install --no-package-lock`, no network access needed),
/// which is exactly what writes the sidecar file that broke `list`.
#[test]
fn spawn_through_real_dependency_prep_then_list_does_not_crash() {
    let repo = init_repo("dependency-prep-list");
    std::fs::write(repo.join("package.json"), "{\"name\":\"scratch\",\"version\":\"1.0.0\"}").unwrap();
    run_git(&repo, &["add", "-A"]);
    run_git(&repo, &["commit", "-q", "-m", "add package.json"]);
    let shim = shim_dir();

    let task = script(&[("hello.txt", "hello")], "created hello.txt");
    let spawn = pact(&repo, &shim, &["spawn", &task, "--agent", "claude"]);
    assert!(spawn.status.success(), "stdout: {}\nstderr: {}", stdout(&spawn), String::from_utf8_lossy(&spawn.stderr));

    let deps_dir = state_dir_for(&repo).join("meta");
    let has_deps_sidecar = std::fs::read_dir(&deps_dir)
        .map(|entries| entries.filter_map(|e| e.ok()).any(|e| e.file_name().to_string_lossy().ends_with("-deps.json")))
        .unwrap_or(false);
    assert!(has_deps_sidecar, "expected dependency prep to have written a -deps.json sidecar file");

    let list = pact(&repo, &shim, &["list"]);
    assert!(
        list.status.success(),
        "list must not crash on a workspace that went through real dependency prep -- stdout: {}\nstderr: {}",
        stdout(&list),
        String::from_utf8_lossy(&list.stderr)
    );
    let id = workspace_id_from_spawn_output(&spawn);
    assert!(stdout(&list).contains(&id), "expected the workspace to actually appear in `list`, got: {}", stdout(&list));

    cleanup(&repo);
    cleanup(&shim);
}

fn wait_until(mut condition: impl FnMut() -> bool, timeout: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if condition() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    condition()
}

#[test]
fn teardown_kills_a_still_running_fake_agent_process() {
    let repo = init_repo("teardown-kill");
    let shim = shim_dir();

    let long_sleep_task = serde_json::json!({
        "writes": {"slow.txt": "eventually"},
        "sleep_ms": 60_000u64,
        "summary": "done sleeping",
    })
    .to_string();

    let mut child = Command::new(env!("CARGO_BIN_EXE_pact"))
        .args(["--repo", repo.to_str().unwrap(), "spawn", &long_sleep_task, "--agent", "claude"])
        .env("PATH", path_with_shim_first(&shim))
        .spawn()
        .expect("failed to spawn `pact spawn` in the background");

    let mut recorded_pid: Option<u32> = None;
    let found_running = wait_until(
        || {
            let list = pact(&repo, &shim, &["list"]);
            let text = stdout(&list);
            let Some(pid_line) = text.lines().find(|l| l.trim_start().starts_with("agent pid:") && l.contains("(running)")) else {
                return false;
            };
            recorded_pid = pid_line.split_whitespace().nth(2).and_then(|s| s.parse().ok());
            recorded_pid.is_some()
        },
        Duration::from_secs(10),
    );
    assert!(found_running, "fake agent never reported as running before the sleep completed");
    let pid = recorded_pid.expect("recorded a running pid");
    assert!(agent_process_alive(pid), "expected pid {pid} to be alive right after `pact list` reported it running");

    let list = pact(&repo, &shim, &["list"]);
    let list_text = stdout(&list);
    let id = list_text.lines().next().unwrap().split_whitespace().next().unwrap().to_string();

    let teardown = pact(&repo, &shim, &["teardown", &id, "--force"]);
    assert!(teardown.status.success(), "stdout: {}\nstderr: {}", stdout(&teardown), String::from_utf8_lossy(&teardown.stderr));

    let killed = wait_until(|| !agent_process_alive(pid), Duration::from_secs(5));
    assert!(killed, "expected pid {pid} to no longer be alive after `pact teardown --force`");

    let _ = wait_until(
        || matches!(child.try_wait(), Ok(Some(_))),
        Duration::from_secs(5),
    );
    let _ = child.kill();
    let _ = child.wait();

    cleanup(&repo);
    cleanup(&shim);
}
