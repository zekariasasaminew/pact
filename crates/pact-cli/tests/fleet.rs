//! End-to-end coverage for `pact fleet` (issue #388): the real `pact`
//! binary, real git repositories, and `fake_agent` on `PATH` as `claude`.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use uuid::Uuid;

fn run_git(dir: &Path, args: &[&str]) -> Output {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap_or_else(|err| panic!("failed to spawn `git {}`: {err}", args.join(" ")));
    assert!(output.status.success(), "`git {}` failed: {}", args.join(" "), String::from_utf8_lossy(&output.stderr));
    output
}

fn scratch_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("pact-cli-fleet-{name}-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn init_repo(name: &str) -> PathBuf {
    let root = scratch_dir(name);
    run_git(&root, &["init", "-q"]);
    run_git(&root, &["config", "user.email", "test@test.com"]);
    run_git(&root, &["config", "user.name", "test"]);
    std::fs::write(root.join("README.md"), "# demo\n").unwrap();
    run_git(&root, &["add", "-A"]);
    run_git(&root, &["commit", "-q", "-m", "init"]);
    root
}

fn state_dir(repo: &Path) -> PathBuf {
    pact_vcs::WorkspaceManager::state_dir_for(repo).unwrap()
}

fn cleanup(repo: &Path) {
    let _ = std::fs::remove_dir_all(repo);
    let _ = std::fs::remove_dir_all(state_dir(repo));
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

fn script(writes: &[(&str, &str)], summary: &str) -> String {
    serde_json::json!({
        "writes": writes.iter().cloned().collect::<std::collections::BTreeMap<&str, &str>>(),
        "summary": summary,
    })
    .to_string()
}

/// Runs `pact fleet <file> <args>` from `cwd` with the fake agent first
/// on `PATH`.
fn fleet(cwd: &Path, shim: &Path, file: &Path, args: &[&str]) -> Output {
    let sep = if cfg!(windows) { ";" } else { ":" };
    let path = format!("{}{sep}{}", shim.display(), std::env::var("PATH").unwrap_or_default());
    Command::new(env!("CARGO_BIN_EXE_pact"))
        .arg("fleet")
        .arg(file)
        .args(["--stagger-ms", "0", "--min-free-mem-mb", "0"])
        .args(args)
        .current_dir(cwd)
        .env("PATH", path)
        .output()
        .unwrap()
}

fn text(output: &Output) -> String {
    format!("{}\n--- stderr ---\n{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr))
}

fn toml_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

const DONE_REPORT: &str = "\nSTATUS: done\nCHANGED: the file\nVERIFIED: read it back\nNOT VERIFIED: nothing\nNOTES: none";

#[test]
fn fleet_runs_one_task_in_each_of_two_repositories() {
    let repo_a = init_repo("a");
    let repo_b = init_repo("b");
    let shim = shim_dir();
    let outside = scratch_dir("outside");

    std::fs::write(outside.join("beta.md"), script(&[("beta.txt", "BETA")], &format!("wrote beta{DONE_REPORT}"))).unwrap();
    let file = outside.join("tasks.toml");
    std::fs::write(
        &file,
        format!(
            "[defaults]\nagent = \"claude\"\n[tiers.chore]\nclaude = \"haiku\"\n\n\
             [[task]]\nname = \"alpha\"\nrepo = \"{}\"\ntier = \"chore\"\nprompt = '{}'\n\n\
             [[task]]\nname = \"beta\"\nrepo = \"{}\"\nprompt_file = \"beta.md\"\n",
            toml_path(&repo_a),
            script(&[("alpha.txt", "ALPHA")], &format!("wrote alpha{DONE_REPORT}")),
            toml_path(&repo_b),
        ),
    )
    .unwrap();

    let output = fleet(&outside, &shim, &file, &[]);
    let out = text(&output);
    assert!(output.status.success(), "{out}");
    assert!(out.contains("fleet: 2 tasks, 2 done, 0 not done"), "{out}");
    assert!(out.contains("claude/haiku"), "{out}");

    for (repo, name, file_name, content) in [(&repo_a, "alpha", "alpha.txt", "ALPHA"), (&repo_b, "beta", "beta.txt", "BETA")] {
        let state = state_dir(repo);
        let written = state.join("workspaces").join(name).join(file_name);
        assert_eq!(std::fs::read_to_string(&written).unwrap_or_default(), content, "{}", written.display());
        run_git(repo, &["rev-parse", "--verify", "--quiet", &format!("pact/{name}")]);
        let meta: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(state.join("meta").join("runs").join(format!("{name}.json"))).unwrap())
                .unwrap();
        assert_eq!(meta["report"]["status"], "done", "{meta}");
        assert_eq!(meta["report"]["verified"], "read it back", "{meta}");
        assert!(meta["args"].to_string().contains("STATUS: done | blocked | failed"), "the prompt carries the contract: {meta}");
    }
    let meta_a = std::fs::read_to_string(state_dir(&repo_a).join("meta").join("runs").join("alpha.json")).unwrap();
    assert!(meta_a.contains("\"model\": \"haiku\""), "{meta_a}");

    cleanup(&repo_a);
    cleanup(&repo_b);
    let _ = std::fs::remove_dir_all(&shim);
    let _ = std::fs::remove_dir_all(&outside);
}

#[test]
fn fleet_exits_1_and_prints_the_notes_when_a_task_is_blocked_or_unreported() {
    let repo = init_repo("blocked");
    let shim = shim_dir();
    let file = repo.join("tasks.toml");
    std::fs::write(
        &file,
        format!(
            "[[task]]\nname = \"asks\"\nagent = \"claude\"\nprompt = '{}'\n\n\
             [[task]]\nname = \"silent\"\nagent = \"claude\"\nprompt = '{}'\n\n\
             [[task]]\nname = \"fine\"\nagent = \"claude\"\nprompt = '{}'\n",
            script(&[], "STATUS: blocked\nNOTES: which database should this use?"),
            script(&[("x.txt", "x")], "I wrote the file."),
            script(&[("y.txt", "y")], &format!("ok{DONE_REPORT}")),
        ),
    )
    .unwrap();

    let output = fleet(&repo, &shim, &file, &["--max-concurrent", "1"]);
    let out = text(&output);
    assert_eq!(output.status.code(), Some(1), "{out}");
    assert!(out.contains("fleet: 3 tasks, 1 done, 2 not done"), "{out}");
    assert!(out.contains("NOTES: which database should this use?"), "{out}");
    assert!(out.contains("unreported"), "{out}");
    assert!(out.contains("no STATUS report"), "{out}");

    cleanup(&repo);
    let _ = std::fs::remove_dir_all(&shim);
}

#[test]
fn fleet_dry_run_previews_every_task_and_creates_nothing() {
    let repo = init_repo("dry");
    let shim = shim_dir();
    let file = repo.join("tasks.toml");
    std::fs::write(
        &file,
        "[defaults]\nagent = \"claude\"\n\
         [[task]]\nname = \"one\"\nmodel = \"haiku\"\nprompt = \"first\"\n\
         [[task]]\nname = \"two\"\nprompt = \"second\"\n",
    )
    .unwrap();

    let output = fleet(&repo, &shim, &file, &["--dry-run"]);
    let out = text(&output);
    assert!(output.status.success(), "{out}");
    assert!(out.contains("task one:") && out.contains("model haiku"), "{out}");
    assert!(out.contains("task two:") && out.contains("model CLI default"), "{out}");
    assert!(out.contains("would create workspace one (pact/one)"), "{out}");
    assert!(out.contains("STATUS: done | blocked | failed"), "{out}");
    assert!(!state_dir(&repo).join("workspaces").join("one").exists());

    cleanup(&repo);
    let _ = std::fs::remove_dir_all(&shim);
}
