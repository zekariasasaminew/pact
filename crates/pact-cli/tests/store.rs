//! Real end-to-end coverage for `pact store list/verify/clean` (issue
//! #160) -- drives a real `pact spawn` (via the fake-agent shim) against a
//! real npm workspace with a zero-dependency lockfile, so `npm ci` runs
//! instantly with no network access, then exercises the store CLI against
//! whatever it actually populated.
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use uuid::Uuid;

fn run_git(dir: &Path, args: &[&str]) {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap_or_else(|err| panic!("failed to spawn `git {}`: {err}", args.join(" ")));
    assert!(output.status.success(), "`git {}` failed: {}", args.join(" "), String::from_utf8_lossy(&output.stderr));
}

fn init_repo_with_zero_dependency_lockfile() -> PathBuf {
    let root = std::env::temp_dir().join(format!("pact-cli-store-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    run_git(&root, &["init", "-q"]);
    run_git(&root, &["config", "user.email", "test@test.com"]);
    run_git(&root, &["config", "user.name", "test"]);
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
}

fn shim_dir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("pact-cli-store-shim-{}", Uuid::new_v4()));
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

#[test]
fn store_list_verify_clean_round_trip_against_a_real_populated_entry() {
    let repo = init_repo_with_zero_dependency_lockfile();
    let shim = shim_dir();

    let empty_before = pact(&repo, &shim, &["store", "list"]);
    assert!(empty_before.status.success(), "stderr: {}", String::from_utf8_lossy(&empty_before.stderr));
    assert!(stdout(&empty_before).contains("no store entries"), "got: {}", stdout(&empty_before));

    let spawn = pact(&repo, &shim, &["spawn", "{\"summary\":\"noop\"}", "--agent", "claude"]);
    assert!(spawn.status.success(), "stdout: {}\nstderr: {}", stdout(&spawn), String::from_utf8_lossy(&spawn.stderr));

    let list = pact(&repo, &shim, &["store", "list"]);
    assert!(list.status.success());
    let list_text = stdout(&list);
    assert!(!list_text.contains("no store entries"), "expected a real populated entry, got: {list_text}");
    assert!(list_text.contains("node"), "expected the node/npm version breakdown, got: {list_text}");

    let verify = pact(&repo, &shim, &["store", "verify"]);
    assert!(verify.status.success(), "stdout: {}\nstderr: {}", stdout(&verify), String::from_utf8_lossy(&verify.stderr));
    assert!(stdout(&verify).contains("ok:"), "got: {}", stdout(&verify));

    let dry_run = pact(&repo, &shim, &["store", "clean", "--all", "--dry-run"]);
    assert!(dry_run.status.success());
    assert!(stdout(&dry_run).contains("would remove"), "got: {}", stdout(&dry_run));

    let list_after_dry_run = pact(&repo, &shim, &["store", "list"]);
    assert!(
        !stdout(&list_after_dry_run).contains("no store entries"),
        "a --dry-run clean must not actually remove anything"
    );

    let clean = pact(&repo, &shim, &["store", "clean", "--all"]);
    assert!(clean.status.success(), "stdout: {}\nstderr: {}", stdout(&clean), String::from_utf8_lossy(&clean.stderr));
    assert!(stdout(&clean).contains("removed:"), "got: {}", stdout(&clean));

    let empty_after = pact(&repo, &shim, &["store", "list"]);
    assert!(stdout(&empty_after).contains("no store entries"), "got: {}", stdout(&empty_after));

    cleanup(&repo);
    cleanup(&shim);
}

#[test]
fn store_clean_requires_either_all_or_older_than_days() {
    let repo = init_repo_with_zero_dependency_lockfile();
    let shim = shim_dir();

    let clean = pact(&repo, &shim, &["store", "clean"]);
    assert!(!clean.status.success(), "expected clean with neither --all nor --older-than-days to fail");
    assert!(
        String::from_utf8_lossy(&clean.stderr).contains("--all"),
        "expected a clear error naming the missing flag, got: {}",
        String::from_utf8_lossy(&clean.stderr)
    );

    cleanup(&repo);
    cleanup(&shim);
}
