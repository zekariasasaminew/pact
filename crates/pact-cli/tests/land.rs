//! End-to-end coverage for `pact land`, `pact discard` and
//! `pact list --json` (issue #389): the real `pact` binary, real git
//! repositories, and `fake_agent` on `PATH` as `claude`.

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
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn branch_exists(repo: &Path, branch: &str) -> bool {
    Command::new("git")
        .args(["rev-parse", "--verify", "--quiet", &format!("refs/heads/{branch}")])
        .current_dir(repo)
        .output()
        .unwrap()
        .status
        .success()
}

fn scratch_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("pact-cli-land-{name}-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn init_repo(name: &str) -> PathBuf {
    let root = scratch_dir(name);
    run_git(&root, &["init", "-q", "-b", "main"]);
    run_git(&root, &["config", "user.email", "test@test.com"]);
    run_git(&root, &["config", "user.name", "test"]);
    std::fs::write(root.join("README.md"), "# demo\n").unwrap();
    run_git(&root, &["add", "-A"]);
    run_git(&root, &["commit", "-q", "-m", "init"]);
    root
}

fn workspace_dir(repo: &Path, id: &str) -> PathBuf {
    pact_vcs::WorkspaceManager::state_dir_for(repo).unwrap().join("workspaces").join(id)
}

fn cleanup(repo: &Path) {
    let _ = std::fs::remove_dir_all(repo);
    let _ = std::fs::remove_dir_all(pact_vcs::WorkspaceManager::state_dir_for(repo).unwrap());
}

fn shim_dir() -> PathBuf {
    let dir = scratch_dir("shim");
    let dest = if cfg!(windows) { dir.join("claude.exe") } else { dir.join("claude") };
    std::fs::copy(env!("CARGO_BIN_EXE_fake_agent"), &dest).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    dir
}

fn pact(repo: &Path, shim: &Path, args: &[&str]) -> Output {
    let sep = if cfg!(windows) { ";" } else { ":" };
    let path = format!("{}{sep}{}", shim.display(), std::env::var("PATH").unwrap_or_default());
    Command::new(env!("CARGO_BIN_EXE_pact")).args(args).current_dir(repo).env("PATH", path).output().unwrap()
}

fn text(output: &Output) -> String {
    format!("{}\n--- stderr ---\n{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr))
}

/// Runs a fleet in `repo` whose tasks each write one file, so every task
/// name becomes a workspace id holding uncommitted work.
fn run_tasks(repo: &Path, shim: &Path, tasks: &[(&str, &str, &str)]) {
    let mut file = String::new();
    for (name, path, content) in tasks {
        let script = serde_json::json!({
            "writes": { *path: *content },
            "summary": format!("wrote {path}\nSTATUS: done\nCHANGED: {path}\nVERIFIED: read it back\nNOT VERIFIED: nothing\nNOTES: none"),
        });
        file.push_str(&format!("[[task]]\nname = \"{name}\"\nagent = \"claude\"\nprompt = '{script}'\n\n"));
    }
    let tasks_file = scratch_dir("tasks").join("tasks.toml");
    std::fs::write(&tasks_file, file).unwrap();
    let output = pact(repo, shim, &["fleet", tasks_file.to_str().unwrap(), "--stagger-ms", "0", "--min-free-mem-mb", "0"]);
    assert!(output.status.success(), "{}", text(&output));
    let _ = std::fs::remove_dir_all(tasks_file.parent().unwrap());
}

#[test]
fn land_fast_forwards_then_merges_and_removes_each_workspace() {
    let repo = init_repo("merge");
    let shim = shim_dir();
    run_tasks(&repo, &shim, &[("alpha", "alpha.txt", "ALPHA"), ("beta", "beta.txt", "BETA")]);

    let first = pact(&repo, &shim, &["land", "alpha"]);
    let out = text(&first);
    assert!(first.status.success(), "{out}");
    assert!(out.contains("landed alpha on main: fast-forwarded 1 commit(s)"), "{out}");
    assert_eq!(std::fs::read_to_string(repo.join("alpha.txt")).unwrap(), "ALPHA");
    assert!(!workspace_dir(&repo, "alpha").exists());
    assert!(!branch_exists(&repo, "pact/alpha"));

    let second = pact(&repo, &shim, &["land", "beta"]);
    let out = text(&second);
    assert!(second.status.success(), "{out}");
    assert!(out.contains("landed beta on main: merged 1 commit(s)"), "{out}");
    assert_eq!(std::fs::read_to_string(repo.join("beta.txt")).unwrap(), "BETA");
    assert!(!branch_exists(&repo, "pact/beta"));
    assert_eq!(run_git(&repo, &["status", "--porcelain"]), "");
    assert_eq!(run_git(&repo, &["symbolic-ref", "--short", "HEAD"]), "main");

    let listed = pact(&repo, &shim, &["list"]);
    assert!(text(&listed).contains("no active workspaces"), "{}", text(&listed));

    cleanup(&repo);
    let _ = std::fs::remove_dir_all(&shim);
}

#[test]
fn land_aborts_on_a_conflict_and_keeps_the_committed_workspace() {
    let repo = init_repo("conflict");
    let shim = shim_dir();
    run_tasks(&repo, &shim, &[("one", "shared.txt", "ONE"), ("two", "shared.txt", "TWO")]);

    assert!(pact(&repo, &shim, &["land", "one"]).status.success());
    let head_before = run_git(&repo, &["rev-parse", "HEAD"]);

    let output = pact(&repo, &shim, &["land", "two"]);
    let out = text(&output);
    assert_eq!(output.status.code(), Some(1), "{out}");
    assert!(out.contains("two: not landed"), "{out}");
    assert!(out.contains("conflicts with main in: shared.txt"), "{out}");
    assert_eq!(run_git(&repo, &["rev-parse", "HEAD"]), head_before);
    assert_eq!(run_git(&repo, &["status", "--porcelain"]), "", "the aborted merge left the checkout clean");
    assert_eq!(std::fs::read_to_string(repo.join("shared.txt")).unwrap(), "ONE");
    assert!(workspace_dir(&repo, "two").exists());
    assert_eq!(run_git(&repo, &["rev-list", "--count", "main..pact/two"]), "1", "the work was committed first");

    cleanup(&repo);
    let _ = std::fs::remove_dir_all(&shim);
}

#[test]
fn land_pr_pushes_the_branch_and_leaves_the_workspace_when_gh_cannot_open_the_pull_request() {
    let repo = init_repo("pr");
    let shim = shim_dir();
    let remote = scratch_dir("remote");
    run_git(&remote, &["init", "-q", "--bare"]);
    run_git(&repo, &["remote", "add", "origin", remote.to_str().unwrap()]);
    run_tasks(&repo, &shim, &[("alpha", "alpha.txt", "ALPHA")]);

    // A local bare remote is no GitHub repository, so `gh pr create` fails
    // (or `gh` is absent): either way the push has happened and nothing
    // is torn down.
    let output = pact(&repo, &shim, &["land", "alpha", "--pr"]);
    let out = text(&output);
    assert_eq!(output.status.code(), Some(1), "{out}");
    assert!(out.contains("alpha: not landed"), "{out}");
    assert_eq!(run_git(&remote, &["rev-list", "--count", "pact/alpha"]), "2", "branch was pushed: {out}");
    assert!(workspace_dir(&repo, "alpha").exists());
    assert!(branch_exists(&repo, "pact/alpha"));
    assert!(!repo.join("alpha.txt").exists(), "--pr never merges locally");

    cleanup(&repo);
    let _ = std::fs::remove_dir_all(&shim);
    let _ = std::fs::remove_dir_all(&remote);
}

#[test]
fn discard_removes_uncommitted_and_committed_work() {
    let repo = init_repo("discard");
    let shim = shim_dir();
    run_tasks(&repo, &shim, &[("dirty", "a.txt", "A"), ("committed", "b.txt", "B")]);
    assert!(pact(&repo, &shim, &["commit-all", "--id", "committed"]).status.success());

    let output = pact(&repo, &shim, &["discard", "dirty", "committed", "missing"]);
    let out = text(&output);
    assert_eq!(output.status.code(), Some(1), "one unknown id fails the command: {out}");
    assert!(out.contains("discarded workspace dirty and branch pact/dirty (1 uncommitted file(s), 0 commit(s))"), "{out}");
    assert!(out.contains("discarded workspace committed and branch pact/committed (0 uncommitted file(s), 1 commit(s))"), "{out}");
    assert!(out.contains("missing: not discarded"), "{out}");
    for id in ["dirty", "committed"] {
        assert!(!workspace_dir(&repo, id).exists());
        assert!(!branch_exists(&repo, &format!("pact/{id}")));
    }
    assert!(!repo.join("a.txt").exists() && !repo.join("b.txt").exists());

    cleanup(&repo);
    let _ = std::fs::remove_dir_all(&shim);
}

#[test]
fn list_json_reports_state_dirty_flag_and_the_worker_report() {
    let repo = init_repo("json");
    let shim = shim_dir();

    let empty = pact(&repo, &shim, &["list", "--json"]);
    assert_eq!(String::from_utf8_lossy(&empty.stdout).trim(), "[]", "{}", text(&empty));

    run_tasks(&repo, &shim, &[("alpha", "alpha.txt", "ALPHA")]);
    let output = pact(&repo, &shim, &["list", "--json"]);
    assert!(output.status.success(), "{}", text(&output));
    let entries: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap_or_else(|err| panic!("{err}: {}", text(&output)));
    let entry = &entries[0];
    assert_eq!(entries.as_array().unwrap().len(), 1, "{entries}");
    assert_eq!(entry["id"], "alpha", "{entry}");
    assert_eq!(entry["branch"], "pact/alpha", "{entry}");
    assert_eq!(entry["state"], "finished", "{entry}");
    assert_eq!(entry["dirty"], true, "{entry}");
    assert_eq!(entry["commits_ahead"], 0, "{entry}");
    assert_eq!(entry["agent"], "claude", "{entry}");
    assert_eq!(entry["report"]["status"], "done", "{entry}");
    assert_eq!(entry["report"]["changed"], "alpha.txt", "{entry}");

    cleanup(&repo);
    let _ = std::fs::remove_dir_all(&shim);
}
