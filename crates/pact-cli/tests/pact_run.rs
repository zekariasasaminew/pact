//! End-to-end coverage for `pact run` (issue #305) through the real binary.
//! pact-acp's fake agent is on PATH as `copilot`: prose prompts (the
//! planner's) get the canned reply in `FAKE_ACP_REPLY_FILE`, JSON tasks
//! (the unit briefs pact renders, whose `brief` here is a JSON write task)
//! write their files into the session's cwd. No model anywhere.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use uuid::Uuid;

fn run_git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git").args(args).current_dir(dir).output().unwrap();
    assert!(output.status.success(), "`git {}` failed: {}", args.join(" "), String::from_utf8_lossy(&output.stderr));
    String::from_utf8_lossy(&output.stdout).to_string()
}

fn init_repo(name: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("pact-cli-run-{name}-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    run_git(&root, &["init", "-q"]);
    run_git(&root, &["config", "user.email", "test@test.com"]);
    run_git(&root, &["config", "user.name", "test"]);
    std::fs::write(root.join("README.md"), "# demo\n").unwrap();
    std::fs::write(root.join("src.ts"), "export const x = 1;\n".repeat(50)).unwrap();
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
    let dir = std::env::temp_dir().join(format!("pact-cli-run-shim-{}", Uuid::new_v4()));
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

/// A unit brief that is a JSON write task for the fake.
fn write_task(file: &str, content: &str, summary: &str) -> String {
    serde_json::json!({ "writes": { file: content }, "summary": summary }).to_string()
}

fn both_exist(a: &str, b: &str) -> String {
    if cfg!(windows) {
        format!("if exist {a} (if exist {b} (exit 0) else (exit 1)) else (exit 1)")
    } else {
        format!("[ -f {a} ] && [ -f {b} ]")
    }
}

fn plan_reply(units: serde_json::Value, verify: Option<&str>) -> String {
    let plan = serde_json::json!({
        "shared_context": "Write plain text files. Nothing else.",
        "verify": verify,
        "units": units,
    });
    format!("Here is my plan.\n```json\n{}\n```\n", serde_json::to_string_pretty(&plan).unwrap())
}

fn good_units() -> serde_json::Value {
    serde_json::json!([
        { "name": "alpha", "files": ["alpha.txt"], "brief": write_task("alpha.txt", "A", "wrote alpha"), "verify": null },
        { "name": "beta", "files": ["beta.txt", "src.ts"], "brief": write_task("beta.txt", "B", "wrote beta") }
    ])
}

fn pact(repo: &Path, shim: &Path, reply: Option<&Path>, args: &[&str]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_pact"));
    command.args(["--repo", repo.to_str().unwrap()]).args(args).env("PATH", path_with_shim_first(shim));
    if let Some(reply) = reply {
        command.env("FAKE_ACP_REPLY_FILE", reply);
    }
    command.output().unwrap()
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).to_string()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).to_string()
}

fn write_reply(dir: &Path, text: &str) -> PathBuf {
    let path = dir.join("planner-reply.md");
    std::fs::write(&path, text).unwrap();
    path
}

fn state_dir(repo: &Path) -> PathBuf {
    pact_vcs::WorkspaceManager::state_dir_for(repo).unwrap()
}

#[test]
fn run_plans_briefs_executes_commits_and_verifies_from_one_task() {
    let repo = init_repo("happy");
    let shim = shim_dir();
    let reply = write_reply(&shim, &plan_reply(good_units(), Some(&both_exist("alpha.txt", "beta.txt"))));

    let out = pact(&repo, &shim, Some(&reply), &["run", "--agent", "copilot", "Add two text files"]);
    assert!(out.status.success(), "pact run failed:\nstdout: {}\nstderr: {}", stdout(&out), stderr(&out));
    let text = stdout(&out);
    assert!(text.contains("plan: 2 units (1 planner attempt, ") && text.contains("s), saved to"), "the plan line carries the planning time:\n{text}");
    assert!(text.contains("  alpha: 1 file -- brief") && text.contains("  beta: 2 files -- brief"), "{text}");
    assert!(text.contains("unit alpha: done") && text.contains("unit beta: done"), "{text}");
    assert!(text.contains("[planner] [phase] planning (attempt 1)"), "the planner's events are labelled:\n{text}");
    assert!(text.contains("(committed)"), "{text}");
    assert!(text.contains(": passed (it failed on the base commit, so this run fixed it) (exit 0"), "verification ran, with the baseline telling the story:\n{text}");
    assert!(text.contains("run: OK"), "{text}");

    // Issue #348: the planner's session is logged like a lane's, one JSON
    // line per update, opened by pact's attempt marker.
    let log_line = text.lines().find(|l| l.trim_start().starts_with("planner log: ")).unwrap_or_else(|| panic!("no planner log line in:\n{text}"));
    let log_path = PathBuf::from(log_line.trim().trim_start_matches("planner log: "));
    assert!(log_path.starts_with(state_dir(&repo).join("logs")), "{}", log_path.display());
    let log_lines: Vec<serde_json::Value> = std::fs::read_to_string(&log_path)
        .unwrap_or_else(|err| panic!("{}: {err}", log_path.display()))
        .lines()
        .map(|l| serde_json::from_str(l).unwrap_or_else(|err| panic!("{err}: {l}")))
        .collect();
    assert_eq!(log_lines[0]["pact"]["planner_attempt"], 1, "{:?}", log_lines[0]);
    assert!(log_lines.iter().skip(1).all(|l| l.get("sessionId").is_some() && l.get("update").is_some()), "{log_lines:?}");
    assert!(log_lines.iter().all(|l| l["t"].as_u64().is_some_and(|t| t > 1_700_000_000_000)), "every line carries Unix milliseconds (#358):\n{log_lines:?}");
    assert!(log_lines.len() > 1, "the planner's reply must have produced updates:\n{log_lines:?}");

    // The plan and the briefs are on disk, the briefs carry the rules.
    let plans: Vec<PathBuf> = std::fs::read_dir(state_dir(&repo).join("meta").join("plans")).unwrap().map(|e| e.unwrap().path()).collect();
    assert_eq!(plans.len(), 1, "one persisted plan: {plans:?}");
    let plan: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&plans[0]).unwrap()).unwrap();
    assert_eq!(plan["task"], "Add two text files");
    assert_eq!(plan["units"].as_array().unwrap().len(), 2);
    let brief_dirs: Vec<PathBuf> = std::fs::read_dir(state_dir(&repo).join("briefs")).unwrap().map(|e| e.unwrap().path()).collect();
    let alpha_brief = std::fs::read_to_string(brief_dirs[0].join("alpha.md")).unwrap();
    assert!(alpha_brief.contains("# Unit `alpha`") && alpha_brief.contains("Do not commit") && alpha_brief.contains("Write plain text files"), "{alpha_brief}");

    // Both lanes wrote into one shared tree, committed once on the batch branch.
    let manager = pact_vcs::WorkspaceManager::open(&repo).unwrap();
    let batch = manager.list_workspaces().unwrap().into_iter().find(|w| w.shared_batch.is_none()).expect("a shared-tree batch");
    assert!(batch.task.starts_with("pact run: Add two text files"), "the tree is prepared before the lanes are known (#353), so it is named after the task: {}", batch.task);
    assert!(text.contains("[planner] [phase] creating the shared tree while the planner works"), "{text}");
    let files = run_git(&repo, &["ls-tree", "--name-only", &batch.branch]);
    assert!(files.contains("alpha.txt") && files.contains("beta.txt"), "committed files: {files}");
    let lanes: Vec<String> = manager.list_workspaces().unwrap().into_iter().filter(|w| w.shared_batch.is_some()).map(|w| w.id).collect();
    assert_eq!(lanes.len(), 2, "lanes: {lanes:?}");
    assert!(lanes.contains(&"alpha".to_string()) && lanes.contains(&"beta".to_string()), "unit names are workspace names: {lanes:?}");
    assert!(run_git(&repo, &["status", "--porcelain"]).trim().is_empty(), "the planner must leave the repo root untouched");

    cleanup(&repo);
    cleanup(&shim);
}

#[test]
fn run_sends_a_bad_plan_back_and_gives_up_after_the_retries() {
    let repo = init_repo("bad-plan");
    let shim = shim_dir();
    let overlapping = serde_json::json!([
        { "name": "a", "files": ["shared.txt", "a.txt"], "brief": write_task("a.txt", "a", "a") },
        { "name": "b", "files": ["shared.txt"], "brief": write_task("shared.txt", "s", "b") }
    ]);
    let reply = write_reply(&shim, &plan_reply(overlapping, None));

    let out = pact(&repo, &shim, Some(&reply), &["run", "--agent", "copilot", "--plan-retries", "1", "Do things"]);
    assert!(!out.status.success(), "an unexecutable plan must fail the run:\n{}", stdout(&out));
    let text = format!("{}{}", stdout(&out), stderr(&out));
    assert!(text.contains("still cannot run after 2 attempt(s)"), "{text}");
    assert!(text.contains("\"shared.txt\" is owned by more than one unit (a, b)"), "{text}");
    assert_eq!(text.matches("[planner] [phase] planning (attempt").count(), 2, "one retry means two attempts:\n{text}");
    assert!(text.contains("planning failed; removing the prepared shared tree"), "the tree prepared during planning is taken down (#353):\n{text}");
    assert!(pact_vcs::WorkspaceManager::open(&repo).unwrap().list_workspaces().unwrap().is_empty(), "nothing is spawned for a rejected plan");
    let branches = run_git(&repo, &["branch", "--list", "pact/*"]);
    assert!(branches.trim().is_empty(), "the prepared batch branch is gone too: {branches}");

    cleanup(&repo);
    cleanup(&shim);
}

#[test]
fn run_dry_run_plans_and_persists_but_spawns_nothing() {
    let repo = init_repo("dry-run");
    let shim = shim_dir();
    let reply = write_reply(&shim, &plan_reply(good_units(), Some("npm test")));

    let out = pact(&repo, &shim, Some(&reply), &["run", "--agent", "copilot", "--dry-run", "Add two text files"]);
    assert!(out.status.success(), "dry run failed:\nstdout: {}\nstderr: {}", stdout(&out), stderr(&out));
    let text = stdout(&out);
    assert!(text.contains("dry run: nothing spawned. Edit the plan and run it with `pact run --plan"), "{text}");
    assert!(text.contains("      alpha.txt") && text.contains("      src.ts"), "files are listed per unit:\n{text}");
    // Issue #356: no --max-units given, so the count is sized to the machine and said so.
    let sizing = text.lines().find(|l| l.starts_with("sizing: up to ")).unwrap_or_else(|| panic!("no sizing line in:\n{text}"));
    let units: usize = sizing.trim_start_matches("sizing: up to ").split_whitespace().next().unwrap().parse().unwrap();
    assert!((pact_core::MIN_AUTO_UNITS..=pact_core::MAX_AUTO_UNITS).contains(&units), "{sizing}");
    assert!(sizing.contains("pass --max-units to override"), "{sizing}");
    assert!(pact_vcs::WorkspaceManager::open(&repo).unwrap().list_workspaces().unwrap().is_empty());
    assert_eq!(std::fs::read_dir(state_dir(&repo).join("meta").join("plans")).unwrap().count(), 1, "the plan is persisted even on a dry run");
    let explicit = pact(&repo, &shim, Some(&reply), &["run", "--agent", "copilot", "--dry-run", "--max-units", "3", "Add two text files"]);
    assert!(explicit.status.success(), "{}", stderr(&explicit));
    assert!(!stdout(&explicit).contains("sizing:"), "an explicit --max-units is not second-guessed:\n{}", stdout(&explicit));

    cleanup(&repo);
    cleanup(&shim);
}

#[test]
fn run_with_a_plan_file_skips_the_planner_and_reports_a_failed_verification() {
    let repo = init_repo("plan-file");
    let shim = shim_dir();
    let plan = serde_json::json!({
        "task": "from a file",
        "shared_context": "",
        "units": [
            { "name": "alpha", "files": ["alpha.txt"], "brief": write_task("alpha.txt", "A", "a") },
            { "name": "beta", "files": ["beta.txt"], "brief": write_task("beta.txt", "B", "b") }
        ],
        "verify": "exit 3"
    });
    let plan_path = shim.join("my-plan.json");
    std::fs::write(&plan_path, serde_json::to_string_pretty(&plan).unwrap()).unwrap();

    // No reply file: a planner call would answer with the prompt itself and fail to parse, so
    // passing means the planner was never consulted.
    let out = pact(&repo, &shim, None, &["run", "--agent", "copilot", "--plan", plan_path.to_str().unwrap(), "ignored task text"]);
    assert_eq!(out.status.code(), Some(3), "a command that fails on the base too is inconclusive, exit 3:\n{}", stdout(&out));
    let text = stdout(&out);
    assert!(text.contains("plan: 2 units (0 planner attempts)"), "{text}");
    assert!(text.contains("unit alpha: done") && text.contains("unit beta: done"), "lanes ran:\n{text}");
    assert!(text.contains("[planner] [phase] baseline `exit 3` already FAILS before any lane runs"), "the baseline ran on the untouched tree:\n{text}");
    assert!(text.contains("verify `exit 3`: INCONCLUSIVE (it already fails on the base commit"), "{text}");
    assert!(text.contains("run: INCONCLUSIVE."), "{text}");
    let manager = pact_vcs::WorkspaceManager::open(&repo).unwrap();
    let batch = manager.list_workspaces().unwrap().into_iter().find(|w| w.shared_batch.is_none()).expect("batch kept");
    assert!(batch.path.join("alpha.txt").exists() && batch.path.join("beta.txt").exists());

    // A command that passes on the base and fails after the lanes is a regression, exit 1.
    let teardown = pact(&repo, &shim, None, &["teardown", "--force"]);
    assert!(teardown.status.success(), "teardown failed: {}", stdout(&teardown));
    let broken = if cfg!(windows) { "if exist alpha.txt (exit 1) else (exit 0)" } else { "! [ -f alpha.txt ]" };
    let out = pact(&repo, &shim, None, &["run", "--agent", "copilot", "--plan", plan_path.to_str().unwrap(), "--verify", broken, "x"]);
    assert_eq!(out.status.code(), Some(1), "{}", stdout(&out));
    assert!(stdout(&out).contains(&format!("[planner] [phase] baseline `{broken}` passes")), "{}", stdout(&out));
    assert!(stdout(&out).contains("FAILED (it passed on the base commit, so this run broke it)"), "{}", stdout(&out));
    assert!(stdout(&out).contains("run: FAILED. Workspaces are kept"), "{}", stdout(&out));

    // --verify overrides the plan's own command. Kept workspaces from the
    // failed run must go first: unit names are workspace names.
    let teardown = pact(&repo, &shim, None, &["teardown", "--force"]);
    assert!(teardown.status.success(), "teardown failed: {}", stdout(&teardown));
    let out = pact(&repo, &shim, None, &["run", "--agent", "copilot", "--plan", plan_path.to_str().unwrap(), "--verify", &both_exist("alpha.txt", "beta.txt"), "x"]);
    assert!(out.status.success(), "--verify override should pass:\nstdout: {}\nstderr: {}", stdout(&out), stderr(&out));
    assert!(stdout(&out).contains("passed (it failed on the base commit, so this run fixed it)"), "both files are new, so the base fails and the run fixes it:\n{}", stdout(&out));
    assert!(stdout(&out).contains("run: OK"), "{}", stdout(&out));

    // Issue #360: several --verify commands, each with its own baseline and
    // verdict; the worst one (a regression) decides the exit code even
    // though the other is inconclusive.
    let teardown = pact(&repo, &shim, None, &["teardown", "--force"]);
    assert!(teardown.status.success(), "teardown failed: {}", stdout(&teardown));
    let out = pact(&repo, &shim, None, &["run", "--agent", "copilot", "--plan", plan_path.to_str().unwrap(), "--verify", "exit 3", "--verify", broken, "x"]);
    assert_eq!(out.status.code(), Some(1), "a regression beside an inconclusive check is a failure:\n{}", stdout(&out));
    let text = stdout(&out);
    assert_eq!(text.matches("[planner] [phase] verification baseline on the untouched tree:").count(), 2, "one baseline per command:\n{text}");
    assert!(text.contains("verify `exit 3`: INCONCLUSIVE"), "{text}");
    assert!(text.contains(&format!("verify `{broken}`: FAILED (it passed on the base commit")), "{text}");
    assert!(text.contains("run: FAILED."), "{text}");
    let persisted: Vec<PathBuf> = std::fs::read_dir(state_dir(&repo).join("meta").join("plans")).unwrap().map(|e| e.unwrap().path()).collect();
    let newest = persisted.iter().max_by_key(|p| std::fs::metadata(p).unwrap().modified().unwrap()).unwrap();
    let saved: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(newest).unwrap()).unwrap();
    assert_eq!(saved["verify"], serde_json::json!(["exit 3", broken]), "the persisted plan records the list that ran: {saved}");

    cleanup(&repo);
    cleanup(&shim);
}

#[test]
fn run_prepares_the_shared_tree_before_the_baseline_and_the_lanes() {
    // Issue #301: a verification that needs a generated, gitignored file
    // passes only because --prepare produced it in the batch tree before
    // the baseline ran; the repo root stays untouched.
    let repo = init_repo("prepare");
    let shim = shim_dir();
    std::fs::write(repo.join(".gitignore"), "generated.txt\n").unwrap();
    run_git(&repo, &["add", "-A"]);
    run_git(&repo, &["commit", "-q", "-m", "ignore generated"]);
    let generate = if cfg!(windows) { "echo generated> generated.txt" } else { "echo generated > generated.txt" };
    let needs_generated = if cfg!(windows) { "if exist generated.txt (exit 0) else (exit 1)" } else { "[ -f generated.txt ]" };
    let reply = write_reply(&shim, &plan_reply(good_units(), Some(needs_generated)));

    let out = pact(&repo, &shim, Some(&reply), &["run", "--agent", "copilot", "--prepare", generate, "Add two text files"]);
    assert!(out.status.success(), "pact run failed:\nstdout: {}\nstderr: {}", stdout(&out), stderr(&out));
    let text = stdout(&out);
    assert!(text.contains(&format!("[phase] prepare: {generate}")), "{text}");
    assert!(text.contains("[planner] [phase] baseline `") && text.contains("` passes in"), "the generated file was there before the baseline:\n{text}");
    assert!(text.contains("run: OK"), "{text}");
    assert!(!repo.join("generated.txt").exists(), "prepare ran in the batch tree, not the repo root");

    cleanup(&repo);
    cleanup(&shim);
}

#[test]
fn run_reads_the_task_from_a_file_and_rejects_both_or_neither() {
    let repo = init_repo("task-file");
    let shim = shim_dir();
    let reply = write_reply(&shim, &plan_reply(good_units(), None));
    let task_path = shim.join("task.md");
    std::fs::write(&task_path, "# Big task\n\nAdd two text files.\n").unwrap();

    let out = pact(&repo, &shim, Some(&reply), &["run", "--agent", "copilot", "--dry-run", "--task-file", task_path.to_str().unwrap()]);
    assert!(out.status.success(), "dry run with --task-file failed:\nstdout: {}\nstderr: {}", stdout(&out), stderr(&out));
    let plans: Vec<PathBuf> = std::fs::read_dir(state_dir(&repo).join("meta").join("plans")).unwrap().map(|e| e.unwrap().path()).collect();
    let plan: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&plans[0]).unwrap()).unwrap();
    assert_eq!(plan["task"], "# Big task\n\nAdd two text files.", "the file's content, trimmed, is the task");

    let both = pact(&repo, &shim, Some(&reply), &["run", "--agent", "copilot", "--dry-run", "--task-file", task_path.to_str().unwrap(), "also inline"]);
    assert_eq!(both.status.code(), Some(2), "a task on the command line and --task-file together is a usage error:\n{}", stderr(&both));
    let neither = pact(&repo, &shim, Some(&reply), &["run", "--agent", "copilot", "--dry-run"]);
    assert_eq!(neither.status.code(), Some(2), "neither is a usage error too:\n{}", stderr(&neither));

    cleanup(&repo);
    cleanup(&shim);
}

#[test]
fn run_refuses_a_plan_file_that_cannot_run() {
    let repo = init_repo("bad-file");
    let shim = shim_dir();
    let plan_path = shim.join("bad.json");
    std::fs::write(&plan_path, r#"{"units": [{"name": "a", "files": ["../x"], "brief": "y"}]}"#).unwrap();
    let out = pact(&repo, &shim, None, &["run", "--agent", "copilot", "--plan", plan_path.to_str().unwrap(), "t"]);
    assert!(!out.status.success());
    assert!(stderr(&out).contains("cannot run:") && stderr(&out).contains("must be a repo-relative path"), "{}", stderr(&out));
    cleanup(&repo);
    cleanup(&shim);
}
