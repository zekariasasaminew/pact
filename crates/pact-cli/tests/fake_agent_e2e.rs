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
    if let Ok(state_dir) = pact_vcs::WorkspaceManager::state_dir_for(root) {
        let _ = std::fs::remove_dir_all(state_dir);
    }
}

/// A scratch `PATH` entry containing a copy of `fake_agent`'s compiled
/// binary named `claude`/`claude.exe` -- what makes `pact --agent claude`
/// launch the fake agent instead of trying (and failing) to find a real
/// install.
fn shim_dir() -> PathBuf {
    shim_dir_for("claude")
}

/// Same as `shim_dir`, impersonating a different CLI: `fake_agent` picks
/// its output schema from its own executable name (issue #284 needed a
/// `copilot` impersonation to cover that adapter's lean launch).
fn shim_dir_for(cli: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("pact-cli-fake-agent-shim-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let fake_agent = PathBuf::from(env!("CARGO_BIN_EXE_fake_agent"));
    let dest = if cfg!(windows) { dir.join(format!("{cli}.exe")) } else { dir.join(cli) };
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
    pact_with_env(repo, shim, args, &[])
}

fn pact_with_env(repo: &Path, shim: &Path, args: &[&str], env: &[(&str, &str)]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_pact"))
        .args(["--repo", repo.to_str().unwrap()])
        .args(args)
        .env("PATH", path_with_shim_first(shim))
        .envs(env.iter().copied())
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

/// Regression test for issue #234's actual fix: `--name` must make
/// `--dry-run`'s previewed workspace id and the real run's id identical --
/// the whole complaint was that they never agreed, since the default
/// scheme regenerates a random suffix on every call.
#[test]
fn spawn_with_name_makes_dry_run_and_real_run_ids_agree() {
    let repo = init_repo("spawn-name-parity");
    let shim = shim_dir();

    let task = script(&[("hello.txt", "hello")], "created hello.txt");
    let dry_run = pact(&repo, &shim, &["spawn", &task, "--agent", "claude", "--name", "My Feature", "--dry-run"]);
    assert!(dry_run.status.success(), "stdout: {}\nstderr: {}", stdout(&dry_run), String::from_utf8_lossy(&dry_run.stderr));
    let dry_run_text = stdout(&dry_run);
    let previewed_id = dry_run_text
        .lines()
        .find(|l| l.starts_with("would create workspace "))
        .and_then(|l| l.split_whitespace().nth(3))
        .unwrap_or_else(|| panic!("no 'would create workspace <id> (...)' line in dry-run output:\n{dry_run_text}"))
        .to_string();
    assert_eq!(previewed_id, "my-feature", "expected the name to drive the id directly, got: {previewed_id}");

    let real = pact(&repo, &shim, &["spawn", &task, "--agent", "claude", "--name", "My Feature"]);
    assert!(real.status.success(), "stdout: {}\nstderr: {}", stdout(&real), String::from_utf8_lossy(&real.stderr));
    let real_id = workspace_id_from_spawn_output(&real);
    assert_eq!(real_id, previewed_id, "the real run's id must match the dry-run preview exactly");

    cleanup(&repo);
    cleanup(&shim);
}

/// `--name` given more than once for the same value must be rejected --
/// two workspaces silently colliding on the same id/branch is a confusing
/// failure to debug, and it's cheaply preventable up front.
#[test]
fn spawn_many_rejects_duplicate_names() {
    let repo = init_repo("spawn-many-duplicate-names");
    let shim = shim_dir();

    let task_a = script(&[("a.txt", "A")], "created a.txt");
    let task_b = script(&[("b.txt", "B")], "created b.txt");
    let output = pact(
        &repo,
        &shim,
        &[
            "spawn-many",
            "--agent",
            "claude",
            "--task",
            &task_a,
            "--task",
            &task_b,
            "--name",
            "same-name",
            "--name",
            "same-name",
        ],
    );
    assert!(!output.status.success(), "expected duplicate --name values to be rejected");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("more than once"),
        "expected a duplicate-name error, got: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    cleanup(&repo);
    cleanup(&shim);
}

/// `--name` given fewer times than `--task` must be rejected rather than
/// silently guessing which task each name belongs to.
#[test]
fn spawn_many_rejects_a_partial_name_count() {
    let repo = init_repo("spawn-many-partial-names");
    let shim = shim_dir();

    let task_a = script(&[("a.txt", "A")], "created a.txt");
    let task_b = script(&[("b.txt", "B")], "created b.txt");
    let output = pact(
        &repo,
        &shim,
        &["spawn-many", "--agent", "claude", "--task", &task_a, "--task", &task_b, "--name", "only-one"],
    );
    assert!(!output.status.success(), "expected a partial --name count to be rejected");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("--name given"),
        "expected a count-mismatch error, got: {}",
        String::from_utf8_lossy(&output.stderr)
    );

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
    assert!(
        text.contains("spawn-many: 2 tasks requested, 2 workspaces created, 0 failed"),
        "expected an unconditional reconciliation summary line, got:\n{text}"
    );

    cleanup(&repo);
    cleanup(&shim);
}

/// Regression test for issue #241: a real spawn-many run produced zero
/// output for ~22 minutes during dependency prep, indistinguishable from
/// a hang, and printed no summary of what actually happened once it
/// finished. Confirms both halves of the fix through the real binary:
/// phase markers stream live for each task, and the end-of-run listing
/// includes duration and a dependency-prep outcome, not just "done".
#[test]
fn spawn_many_streams_phase_markers_and_summarizes_duration_and_dependencies() {
    let repo = init_repo("spawn-many-observability");
    std::fs::write(repo.join("package.json"), "{\"name\":\"scratch\",\"version\":\"1.0.0\"}").unwrap();
    run_git(&repo, &["add", "-A"]);
    run_git(&repo, &["commit", "-q", "-m", "add package.json"]);
    let shim = shim_dir();

    let task = script(&[("hello.txt", "hello")], "created hello.txt");
    let output = pact(&repo, &shim, &["spawn-many", "--agent", "claude", "--task", &task]);
    assert!(output.status.success(), "stdout: {}\nstderr: {}", stdout(&output), String::from_utf8_lossy(&output.stderr));

    let text = stdout(&output);
    assert!(text.contains("[phase] creating workspace"), "got:\n{text}");
    assert!(text.contains("[phase] preparing dependencies"), "got:\n{text}");
    assert!(text.contains("[phase] running agent"), "got:\n{text}");
    assert!(
        text.contains("dependencies ready") || text.contains("dependency prep had issues"),
        "expected a dependency-prep phase outcome, got:\n{text}"
    );
    assert!(text.contains("duration:"), "expected a per-workspace duration line, got:\n{text}");
    assert!(text.contains("dependencies:"), "expected a per-workspace dependency summary line, got:\n{text}");

    cleanup(&repo);
    cleanup(&shim);
}

/// Regression test for issue #231: nothing previously guaranteed a failed
/// agent run was counted anywhere beyond its own per-task line, or forced
/// `spawn-many`'s exit code to reflect it. Scripts one fake agent to report
/// failure alongside one that succeeds -- both still become real
/// workspaces (this is a run failure, not a launch failure), but the
/// unconditional summary line and the process exit code must both reflect
/// the failure.
#[test]
fn spawn_many_reconciliation_summary_counts_a_failed_agent_run() {
    let repo = init_repo("spawn-many-run-failure");
    let shim = shim_dir();

    let task_ok = script(&[("alpha.txt", "ALPHA")], "created alpha.txt");
    let task_fail = serde_json::json!({
        "writes": {"beta.txt": "BETA"},
        "summary": "hit an error partway through",
        "success": false,
        "exit_code": 1,
    })
    .to_string();
    let output = pact(
        &repo,
        &shim,
        &["spawn-many", "--agent", "claude", "--task", &task_ok, "--task", &task_fail],
    );
    assert_eq!(output.status.code(), Some(1), "expected exit 1 since one agent run failed");

    let text = stdout(&output);
    let workspace_lines: Vec<&str> = text.lines().filter(|l| l.starts_with("workspace ")).collect();
    assert_eq!(workspace_lines.len(), 2, "both tasks became real workspaces, got:\n{text}");
    assert!(
        text.contains("spawn-many: 2 tasks requested, 2 workspaces created, 0 failed"),
        "workspace creation itself fully succeeded (0 launch failures), got:\n{text}"
    );
    assert!(text.contains("  failed: hit an error partway through"), "expected the failed run's own summary line, got:\n{text}");

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

/// Contrast case for the test above, and issue #233's other half: a task
/// that doesn't touch dependencies at all shouldn't pay dependency prep's
/// cost. `--no-deps` must skip prep entirely -- no `-deps.json` sidecar at
/// all (not an empty one; "never attempted" is a different fact than "ran
/// and found nothing to do"), and `list` must still work normally.
#[test]
fn spawn_with_no_deps_skips_dependency_prep_entirely() {
    let repo = init_repo("no-deps-flag");
    std::fs::write(repo.join("package.json"), "{\"name\":\"scratch\",\"version\":\"1.0.0\"}").unwrap();
    run_git(&repo, &["add", "-A"]);
    run_git(&repo, &["commit", "-q", "-m", "add package.json"]);
    let shim = shim_dir();

    let task = script(&[("hello.txt", "hello")], "created hello.txt");
    let spawn = pact(&repo, &shim, &["spawn", &task, "--agent", "claude", "--no-deps"]);
    assert!(spawn.status.success(), "stdout: {}\nstderr: {}", stdout(&spawn), String::from_utf8_lossy(&spawn.stderr));

    let deps_dir = state_dir_for(&repo).join("meta");
    let has_deps_sidecar = std::fs::read_dir(&deps_dir)
        .map(|entries| entries.filter_map(|e| e.ok()).any(|e| e.file_name().to_string_lossy().ends_with("-deps.json")))
        .unwrap_or(false);
    assert!(!has_deps_sidecar, "expected --no-deps to skip dependency prep entirely, found a -deps.json sidecar anyway");

    let id = workspace_id_from_spawn_output(&spawn);
    let list = pact(&repo, &shim, &["list"]);
    assert!(list.status.success(), "stdout: {}\nstderr: {}", stdout(&list), String::from_utf8_lossy(&list.stderr));
    assert!(stdout(&list).contains(&id), "expected the workspace to still appear in `list`, got: {}", stdout(&list));

    cleanup(&repo);
    cleanup(&shim);
}

/// Issue #283: with a `node_modules` already installed at the repo root,
/// the default (`auto`) deps mode links the workspace's `node_modules` to
/// it instead of running a 99-second `npm ci`, records the link in the
/// workspace metadata, shows it in `list`, and -- the safety half --
/// `teardown` removes the link without deleting the repo root's install
/// through it (`git worktree remove` on its own would). The fake agent
/// writes nothing, so the workspace must read as `[clean]` with the link
/// in place (a trailing-slash `node_modules/` ignore pattern does not
/// match a Unix symlink on its own; pact adds an exclude) and a plain
/// `teardown`, no `--force`, must succeed.
#[test]
fn spawn_links_node_modules_to_the_repo_root_and_teardown_leaves_it_intact() {
    let repo = init_repo("deps-link");
    std::fs::write(repo.join("package.json"), "{\"name\":\"scratch\",\"version\":\"1.0.0\"}").unwrap();
    std::fs::write(repo.join(".gitignore"), "node_modules/\n").unwrap();
    run_git(&repo, &["add", "-A"]);
    run_git(&repo, &["commit", "-q", "-m", "add package.json"]);
    let installed = repo.join("node_modules").join("left-pad");
    std::fs::create_dir_all(&installed).unwrap();
    std::fs::write(installed.join("index.js"), "module.exports = (s) => s;").unwrap();
    let shim = shim_dir();

    let task = script(&[], "inspected the workspace, changed nothing");
    let spawn = pact(&repo, &shim, &["spawn", &task, "--agent", "claude"]);
    assert!(spawn.status.success(), "stdout: {}\nstderr: {}", stdout(&spawn), String::from_utf8_lossy(&spawn.stderr));
    let id = workspace_id_from_spawn_output(&spawn);

    let workspace_dir = state_dir_for(&repo).join("workspaces").join(&id);
    let linked = workspace_dir.join("node_modules");
    assert!(
        std::fs::symlink_metadata(&linked).unwrap().file_type().is_symlink(),
        "expected node_modules to be a link, not a real directory"
    );
    assert!(linked.join("left-pad").join("index.js").exists(), "the link must resolve to the repo root's install");

    let meta: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(state_dir_for(&repo).join("meta").join(format!("{id}.json"))).unwrap())
            .unwrap();
    assert_eq!(meta["linked_paths"], serde_json::json!(["node_modules"]));

    let deps: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(state_dir_for(&repo).join("meta").join(format!("{id}-deps.json"))).unwrap())
            .unwrap();
    assert_eq!(deps[0]["strategy"], "link", "deps report: {deps}");

    let list = pact(&repo, &shim, &["list"]);
    let list_text = stdout(&list);
    assert!(list_text.contains("linked (shared with repo root): node_modules"), "got: {list_text}");
    let workspace_line = list_text.lines().find(|l| l.starts_with(&id)).unwrap_or_else(|| panic!("no line for {id} in:\n{list_text}"));
    assert!(
        workspace_line.contains("[clean"),
        "a linked node_modules must not make the workspace dirty (git must ignore the link); got: {workspace_line}"
    );

    let teardown = pact(&repo, &shim, &["teardown", &id]);
    assert!(teardown.status.success(), "stdout: {}\nstderr: {}", stdout(&teardown), String::from_utf8_lossy(&teardown.stderr));
    assert!(!workspace_dir.exists(), "the worktree must be gone");
    assert!(
        installed.join("index.js").exists(),
        "teardown deleted the repo root's node_modules through the link -- issue #283's data-loss hazard"
    );

    cleanup(&repo);
    cleanup(&shim);
}

/// `--deps install` opts out of linking even when the repo root has a
/// `node_modules` to share, and `--deps` and `--no-deps` together are
/// rejected rather than silently picking one.
#[test]
fn deps_install_opts_out_of_linking_and_conflicting_flags_are_rejected() {
    let repo = init_repo("deps-install");
    std::fs::write(repo.join("package.json"), "{\"name\":\"scratch\",\"version\":\"1.0.0\"}").unwrap();
    std::fs::write(repo.join(".gitignore"), "node_modules/\n").unwrap();
    run_git(&repo, &["add", "-A"]);
    run_git(&repo, &["commit", "-q", "-m", "add package.json"]);
    std::fs::create_dir_all(repo.join("node_modules")).unwrap();
    let shim = shim_dir();

    let task = script(&[("hello.txt", "hello")], "created hello.txt");
    let preview = pact(&repo, &shim, &["spawn", &task, "--agent", "claude", "--dry-run"]);
    assert!(stdout(&preview).contains("deps: auto (resolves to link)"), "got: {}", stdout(&preview));
    let preview = pact(&repo, &shim, &["spawn", &task, "--agent", "claude", "--dry-run", "--deps", "install"]);
    assert!(stdout(&preview).contains("deps: install"), "got: {}", stdout(&preview));

    let conflicting = pact(&repo, &shim, &["spawn", &task, "--agent", "claude", "--deps", "link", "--no-deps"]);
    assert!(!conflicting.status.success(), "--deps and --no-deps together must be rejected");

    let unknown = pact(&repo, &shim, &["spawn", &task, "--agent", "claude", "--deps", "hardlink"]);
    assert!(!unknown.status.success());
    assert!(String::from_utf8_lossy(&unknown.stderr).contains("unknown deps mode"), "stderr: {}", String::from_utf8_lossy(&unknown.stderr));

    cleanup(&repo);
    cleanup(&shim);
}

/// Issue #284: a lean Copilot launch runs the agent under a per-workspace
/// `COPILOT_HOME` holding only the user's login pointer and settings plus
/// an empty MCP config, so none of the user's own MCP servers load. The
/// fake agent (impersonating `copilot`) dumps the `COPILOT_HOME` it saw,
/// which must be pact's per-agent home, not the user's; `--no-lean` must
/// leave the user's home in place and create nothing.
#[test]
fn lean_copilot_spawn_runs_the_agent_under_an_isolated_copilot_home() {
    let repo = init_repo("lean-copilot");
    let shim = shim_dir_for("copilot");
    // Stands in for the user's real ~/.copilot: a login pointer, a default
    // model, and an MCP server that must not carry over.
    let user_home = std::env::temp_dir().join(format!("pact-cli-fake-copilot-home-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&user_home).unwrap();
    std::fs::write(user_home.join("config.json"), "{\"loggedInUsers\":[{\"host\":\"https://github.com\",\"login\":\"me\"}]}").unwrap();
    std::fs::write(user_home.join("settings.json"), "{\"model\":\"claude-opus-5\"}").unwrap();
    std::fs::write(user_home.join("mcp-config.json"), "{\"mcpServers\":{\"chrome\":{\"command\":\"npx\"}}}").unwrap();
    let user_home_str = user_home.to_str().unwrap();

    let task = serde_json::json!({"dump_env": ["COPILOT_HOME"], "summary": "dumped env"}).to_string();
    let spawn = pact_with_env(&repo, &shim, &["spawn", &task, "--agent", "copilot"], &[("COPILOT_HOME", user_home_str)]);
    assert!(spawn.status.success(), "stdout: {}\nstderr: {}", stdout(&spawn), String::from_utf8_lossy(&spawn.stderr));
    let id = workspace_id_from_spawn_output(&spawn);

    let agent_home = state_dir_for(&repo).join("homes").join(&id);
    let seen = std::fs::read_to_string(state_dir_for(&repo).join("workspaces").join(&id).join("env-COPILOT_HOME.txt")).unwrap();
    assert_eq!(PathBuf::from(seen.trim()), agent_home, "the agent must run under pact's per-workspace home");
    assert_eq!(
        std::fs::read_to_string(agent_home.join("config.json")).unwrap(),
        std::fs::read_to_string(user_home.join("config.json")).unwrap()
    );
    assert_eq!(std::fs::read_to_string(agent_home.join("settings.json")).unwrap(), "{\"model\":\"claude-opus-5\"}");
    let mcp: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(agent_home.join("mcp-config.json")).unwrap()).unwrap();
    assert_eq!(mcp, serde_json::json!({"mcpServers": {}}));

    let run: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(state_dir_for(&repo).join("meta").join(format!("{id}-run.json"))).unwrap()).unwrap();
    let args: Vec<&str> = run["args"].as_array().unwrap().iter().map(|a| a.as_str().unwrap()).collect();
    assert!(args.contains(&"--disable-builtin-mcps") && args.contains(&"--session-id"), "args: {args:?}");
    assert!(args.contains(&"shell(npm install:*)"), "deny rules must be on the command line: {args:?}");
    let meta: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(state_dir_for(&repo).join("meta").join(format!("{id}.json"))).unwrap()).unwrap();
    assert_eq!(run["session_id"], meta["session_id"], "workspace and run metadata must agree on the session id");
    assert!(meta["session_id"].as_str().is_some_and(|s| s.len() == 36));

    let plain = pact_with_env(&repo, &shim, &["spawn", &task, "--agent", "copilot", "--no-lean"], &[("COPILOT_HOME", user_home_str)]);
    assert!(plain.status.success(), "stdout: {}\nstderr: {}", stdout(&plain), String::from_utf8_lossy(&plain.stderr));
    let plain_id = workspace_id_from_spawn_output(&plain);
    let seen = std::fs::read_to_string(state_dir_for(&repo).join("workspaces").join(&plain_id).join("env-COPILOT_HOME.txt")).unwrap();
    assert_eq!(PathBuf::from(seen.trim()), user_home, "--no-lean must leave the user's own home in place");
    assert!(!state_dir_for(&repo).join("homes").join(&plain_id).exists());

    let _ = std::fs::remove_dir_all(&user_home);
    cleanup(&repo);
    cleanup(&shim);
}

/// `--dry-run` must preview the lean launch without leaving the per-agent
/// home behind, the same way it already removes the MCP config file.
#[test]
fn lean_copilot_dry_run_previews_env_and_leaves_no_home_behind() {
    let repo = init_repo("lean-copilot-dry-run");
    let shim = shim_dir_for("copilot");
    let user_home = std::env::temp_dir().join(format!("pact-cli-fake-copilot-home-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&user_home).unwrap();
    std::fs::write(user_home.join("config.json"), "{}").unwrap();

    let preview = pact_with_env(
        &repo,
        &shim,
        &["spawn", "do the thing", "--agent", "copilot", "--dry-run", "--name", "preview"],
        &[("COPILOT_HOME", user_home.to_str().unwrap())],
    );
    assert!(preview.status.success(), "stdout: {}\nstderr: {}", stdout(&preview), String::from_utf8_lossy(&preview.stderr));
    let text = stdout(&preview);
    assert!(text.contains("env: COPILOT_HOME="), "got: {text}");
    assert!(text.contains("--deny-tool"), "got: {text}");
    assert!(!state_dir_for(&repo).join("homes").join("preview").exists(), "dry-run must not leave a home behind");

    let _ = std::fs::remove_dir_all(&user_home);
    cleanup(&repo);
    cleanup(&shim);
}

/// Issue #290: a relative `--repo` used to create the worktree *inside*
/// the repository (git resolved the relative worktree path against its
/// own cwd, the repo) while pact's metadata pointed at the sibling state
/// directory, so dependency prep looked at nothing and the agent launch
/// failed with an invalid working directory. The repo root is absolutized
/// once at the CLI boundary now; this drives the real binary with a
/// relative `--repo` from the repo's parent directory.
#[test]
fn relative_repo_path_creates_the_worktree_beside_the_repo_and_runs() {
    let repo = init_repo("relative-repo");
    let shim = shim_dir();
    let parent = repo.parent().unwrap();
    let relative = PathBuf::from(repo.file_name().unwrap());

    let task = script(&[("hello.txt", "hello")], "created hello.txt");
    let spawn = Command::new(env!("CARGO_BIN_EXE_pact"))
        .args(["--repo", relative.to_str().unwrap(), "spawn", &task, "--agent", "claude", "--name", "relative"])
        .current_dir(parent)
        .env("PATH", path_with_shim_first(&shim))
        .output()
        .unwrap();
    assert!(spawn.status.success(), "stdout: {}\nstderr: {}", stdout(&spawn), String::from_utf8_lossy(&spawn.stderr));

    let beside = state_dir_for(&repo).join("workspaces").join("relative");
    assert!(beside.join("hello.txt").exists(), "the worktree must be beside the repo, at {}", beside.display());
    assert!(!repo.join(state_dir_for(&repo).file_name().unwrap()).exists(), "nothing may be created inside the repository");
    let text = stdout(&spawn);
    let path_line = text.lines().find(|l| l.trim_start().starts_with("path: ")).unwrap_or_else(|| panic!("no path line in:\n{text}"));
    assert!(PathBuf::from(path_line.trim().trim_start_matches("path: ")).is_absolute(), "got: {path_line}");

    cleanup(&repo);
    cleanup(&shim);
}

/// Issue #285: `spawn-many --max-concurrent 2` must never have more than
/// two agents alive at once. Each fake agent drops a presence file (its
/// pid) the moment it starts and removes it before exiting; a poller
/// counts them while the batch runs. Every workspace still gets created
/// and every task still completes -- the cap queues, it never drops.
#[test]
fn spawn_many_never_runs_more_agents_than_max_concurrent() {
    let repo = init_repo("admission-cap");
    let shim = shim_dir();
    let presence_dir = std::env::temp_dir().join(format!("pact-cli-presence-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&presence_dir).unwrap();

    let mut args: Vec<String> = vec!["spawn-many".into(), "--agent".into(), "claude".into()];
    for i in 0..4 {
        let script = serde_json::json!({
            "sleep_ms": 1500,
            "presence_file": presence_dir.join(format!("agent-{i}")).to_str().unwrap(),
            "writes": {format!("out-{i}.txt"): "done"},
            "summary": format!("task {i} done"),
        })
        .to_string();
        args.push("--task".into());
        args.push(script);
    }
    // No stagger and no memory floor: this test is about the slot cap,
    // and a CI runner's free memory must not decide whether it passes.
    args.extend(["--max-concurrent", "2", "--stagger-ms", "0", "--min-free-mem-mb", "0"].map(String::from));
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();

    let mut child = Command::new(env!("CARGO_BIN_EXE_pact"))
        .args(["--repo", repo.to_str().unwrap()])
        .args(&arg_refs)
        .env("PATH", path_with_shim_first(&shim))
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();

    let mut peak = 0usize;
    let started = Instant::now();
    loop {
        let alive = std::fs::read_dir(&presence_dir).map(|d| d.filter_map(|e| e.ok()).count()).unwrap_or(0);
        peak = peak.max(alive);
        if child.try_wait().unwrap().is_some() {
            break;
        }
        assert!(started.elapsed() < Duration::from_secs(120), "spawn-many did not finish in time");
        std::thread::sleep(Duration::from_millis(25));
    }
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success(), "stdout: {}\nstderr: {}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));

    // The upper bound is the guarantee. That the cap still lets two run
    // together is covered deterministically by the `Admission` unit test
    // (`acquire_never_lets_more_than_max_concurrent_run_at_once`); asserting
    // it here would depend on how fast a loaded CI runner launches
    // processes.
    assert!(peak <= 2, "observed {peak} agents alive at once with --max-concurrent 2");
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.contains("4 tasks requested, 4 workspaces created, 0 failed"), "got: {text}");

    let _ = std::fs::remove_dir_all(&presence_dir);
    cleanup(&repo);
    cleanup(&shim);
}

/// `--dry-run` prints the effective admission policy so a user can see
/// what a batch would be held to before spending anything.
#[test]
fn spawn_many_dry_run_prints_the_admission_policy() {
    let repo = init_repo("admission-dry-run");
    let shim = shim_dir();
    let task = script(&[], "noop");
    let preview = pact(&repo, &shim, &["spawn-many", "--agent", "claude", "--task", &task, "--dry-run", "--max-concurrent", "3", "--min-free-mem-mb", "0", "--stagger-ms", "10"]);
    assert!(preview.status.success(), "stderr: {}", String::from_utf8_lossy(&preview.stderr));
    assert!(
        stdout(&preview).contains("admission: at most 3 agents running at once, 0 MB free memory required before each launch, 10 ms between launches"),
        "got: {}",
        stdout(&preview)
    );
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

/// Issue #222: `--estimate-cost` must be rejected without `--dry-run` --
/// running it for real would spawn a paid agent first and print the
/// estimate only after the money's already spent, which defeats the
/// entire point.
#[test]
fn spawn_many_estimate_cost_requires_dry_run() {
    let repo = init_repo("estimate-cost-requires-dry-run");
    let shim = shim_dir();

    let result = pact(&repo, &shim, &["spawn-many", "--task", "do something", "--agent", "claude", "--estimate-cost"]);
    assert!(!result.status.success(), "expected --estimate-cost without --dry-run to be rejected by clap");
    assert!(
        String::from_utf8_lossy(&result.stderr).contains("dry_run") || String::from_utf8_lossy(&result.stderr).contains("dry-run"),
        "expected an error mentioning the --dry-run requirement, got: {}",
        String::from_utf8_lossy(&result.stderr)
    );

    cleanup(&repo);
    cleanup(&shim);
}

/// A mixed-adapter batch (`claude:`/`copilot:` prefixes) must print one
/// cost breakdown per adapter, not one misleading combined number -- and
/// the flat-rate adapter (Copilot) must show quota impact, not a dollar
/// range that doesn't apply to it.
#[test]
fn spawn_many_estimate_cost_splits_a_mixed_adapter_batch() {
    let repo = init_repo("estimate-cost-mixed");
    let shim = shim_dir();

    let result = pact(
        &repo,
        &shim,
        &[
            "spawn-many",
            "--task",
            "claude:fix the auth bug",
            "--task",
            "copilot:write unit tests",
            "--agent",
            "claude",
            "--dry-run",
            "--estimate-cost",
        ],
    );
    assert!(result.status.success(), "stdout: {}\nstderr: {}", stdout(&result), String::from_utf8_lossy(&result.stderr));
    let text = stdout(&result);
    assert!(text.contains("cost estimate"), "got: {text}");
    assert!(text.contains("adapter:       claude"), "expected a claude breakdown, got: {text}");
    assert!(text.contains("adapter:       copilot"), "expected a copilot breakdown, got: {text}");
    assert!(text.contains("quota-eligible"), "expected Copilot's flat-rate quota framing, not a dollar range, got: {text}");

    cleanup(&repo);
    cleanup(&shim);
}

/// Regression test for issue #236: when every planned workspace ties on
/// risk score (here, both edit the same single file, so each scores
/// identically), `merge-all --dry-run` must say so explicitly instead of
/// presenting the resulting creation-order listing as a real ranking.
#[test]
fn merge_all_dry_run_admits_when_every_workspace_ties_on_risk_score() {
    let repo = init_repo("merge-dry-run-tied-risk");
    let shim = shim_dir();

    let task_a = script(&[("shared.txt", "A")], "edited shared.txt (A)");
    let task_b = script(&[("shared.txt", "B")], "edited shared.txt (B)");
    let spawn = pact(&repo, &shim, &["spawn-many", "--agent", "claude", "--task", &task_a, "--task", &task_b]);
    assert!(spawn.status.success(), "spawn-many failed: {}", String::from_utf8_lossy(&spawn.stderr));

    let merge = pact(&repo, &shim, &["merge-all", "--dry-run"]);
    assert!(merge.status.success(), "stdout: {}\nstderr: {}", stdout(&merge), String::from_utf8_lossy(&merge.stderr));
    let merge_text = stdout(&merge);
    assert!(
        merge_text.contains("every workspace scored equally"),
        "expected the tied-score honesty note, got: {merge_text}"
    );

    cleanup(&repo);
    cleanup(&shim);
}

fn status_json(repo: &Path, shim: &Path) -> serde_json::Value {
    let status = pact(repo, shim, &["status", "--json"]);
    assert!(
        status.status.success(),
        "stdout: {}\nstderr: {}",
        stdout(&status),
        String::from_utf8_lossy(&status.stderr)
    );
    serde_json::from_str(&stdout(&status)).unwrap_or_else(|err| panic!("`pact status --json` didn't print valid JSON: {err}"))
}

/// Issue #221: `pact status --json` must reflect a real completed spawn's
/// dirty/files-touched state, reusing exactly the same `is_dirty`/
/// `run_metadata` ground truth `list`'s own regression tests already pin
/// down -- this just checks `status` surfaces it identically, not that the
/// underlying signal itself is correct (that's `list`'s job).
#[test]
fn status_json_reflects_a_dirty_workspace_after_a_real_write() {
    let repo = init_repo("status-dirty");
    let shim = shim_dir();

    let task = script(&[("hello.txt", "hello from a fake agent")], "created hello.txt");
    let spawn = pact(&repo, &shim, &["spawn", &task, "--agent", "claude"]);
    assert!(spawn.status.success(), "stdout: {}\nstderr: {}", stdout(&spawn), String::from_utf8_lossy(&spawn.stderr));
    let id = workspace_id_from_spawn_output(&spawn);

    let report = status_json(&repo, &shim);
    let workspaces = report["workspaces"].as_array().expect("workspaces array");
    let row = workspaces.iter().find(|w| w["id"] == id).expect("spawned workspace present in status");
    assert_eq!(row["dirty"], serde_json::json!(true));
    assert_eq!(row["no_files_touched"], serde_json::json!(false));
    assert_eq!(row["agent_alive"], serde_json::Value::Null, "agent process already exited by the time spawn returned");

    cleanup(&repo);
    cleanup(&shim);
}

/// Contrast case, and the actual new logic this feature adds beyond what
/// `list` already reports: a no-op run's workspace must both flag
/// `no_files_touched` and surface a "what next" hint pointing at `pact
/// inspect <id>` -- issue #212's ground-truth signal, consumed by #221's
/// new hint chain.
#[test]
fn status_json_flags_a_no_op_workspace_and_hints_at_it() {
    let repo = init_repo("status-no-op");
    let shim = shim_dir();

    let noop_task = script(&[], "reported success without doing anything");
    let spawn = pact(&repo, &shim, &["spawn", &noop_task, "--agent", "claude"]);
    assert!(spawn.status.success(), "stdout: {}\nstderr: {}", stdout(&spawn), String::from_utf8_lossy(&spawn.stderr));
    let id = workspace_id_from_spawn_output(&spawn);

    let report = status_json(&repo, &shim);
    let workspaces = report["workspaces"].as_array().expect("workspaces array");
    let row = workspaces.iter().find(|w| w["id"] == id).expect("spawned workspace present in status");
    assert_eq!(row["dirty"], serde_json::json!(false));
    assert_eq!(row["no_files_touched"], serde_json::json!(true));

    let hints = report["hints"].as_array().expect("hints array");
    assert!(
        hints.iter().any(|h| h.as_str().is_some_and(|s| s.contains("touched zero files") && s.contains(&id))),
        "expected a hint pointing at the no-op workspace, got: {hints:?}"
    );

    cleanup(&repo);
    cleanup(&shim);
}

/// The human-readable path (not just `--json`) must actually list every
/// spawned workspace and its own "what next" section -- a plain smoke test
/// against real output, not a structural JSON check.
#[test]
fn status_human_readable_lists_workspaces_and_hints() {
    let repo = init_repo("status-human");
    let shim = shim_dir();

    let noop_task = script(&[], "reported success without doing anything");
    let spawn = pact(&repo, &shim, &["spawn", &noop_task, "--agent", "claude"]);
    assert!(spawn.status.success(), "stdout: {}\nstderr: {}", stdout(&spawn), String::from_utf8_lossy(&spawn.stderr));
    let id = workspace_id_from_spawn_output(&spawn);

    let status = pact(&repo, &shim, &["status"]);
    assert!(status.status.success(), "stdout: {}\nstderr: {}", stdout(&status), String::from_utf8_lossy(&status.stderr));
    let text = stdout(&status);
    assert!(text.contains("agents detected:"), "got: {text}");
    assert!(text.contains(&id), "expected the workspace id in the workspaces table, got: {text}");
    assert!(text.contains("what next:"), "expected a hints section for a no-op workspace, got: {text}");

    cleanup(&repo);
    cleanup(&shim);
}

/// The "still running" branch of the hint chain -- the one piece of
/// `status`'s new logic `list`'s own tests don't already exercise, since
/// `list` has no equivalent hint concept. Reuses the same background-spawn
/// + poll pattern as `teardown_kills_a_still_running_fake_agent_process`.
#[test]
fn status_reports_a_still_running_agent_and_hints_to_wait() {
    let repo = init_repo("status-running");
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

    let report = status_json(&repo, &shim);
    let workspaces = report["workspaces"].as_array().expect("workspaces array");
    let row = workspaces.iter().find(|w| w["agent_pid"] == pid).expect("the running workspace present in status");
    assert_eq!(row["agent_alive"], serde_json::json!(true));

    let hints = report["hints"].as_array().expect("hints array");
    assert!(
        hints.iter().any(|h| h.as_str().is_some_and(|s| s.contains("still running"))),
        "expected a 'still running' hint, got: {hints:?}"
    );

    let list_text = stdout(&pact(&repo, &shim, &["list"]));
    let id = list_text.lines().next().unwrap().split_whitespace().next().unwrap().to_string();
    let teardown = pact(&repo, &shim, &["teardown", &id, "--force"]);
    assert!(teardown.status.success(), "stdout: {}\nstderr: {}", stdout(&teardown), String::from_utf8_lossy(&teardown.stderr));

    let killed = wait_until(|| !agent_process_alive(pid), Duration::from_secs(5));
    assert!(killed, "expected pid {pid} to no longer be alive after `pact teardown --force`");

    let _ = wait_until(|| matches!(child.try_wait(), Ok(Some(_))), Duration::from_secs(5));
    let _ = child.kill();
    let _ = child.wait();

    cleanup(&repo);
    cleanup(&shim);
}
