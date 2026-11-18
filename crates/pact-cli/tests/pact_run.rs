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

fn repairable_check(file: &str, expected: &str, repair_task_file: &str) -> String {
    if cfg!(windows) {
        format!("findstr {expected} {file} >nul || (type {repair_task_file} & exit /b 1)")
    } else {
        format!("grep -q '{expected}' {file} || (cat {repair_task_file}; exit 1)")
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
    let acp_log = shim.join("acp-methods.log");

    let out = Command::new(env!("CARGO_BIN_EXE_pact"))
        .args(["--repo", repo.to_str().unwrap(), "run", "--agent", "copilot", "--plan-retries", "1", "Do things"])
        .env("PATH", path_with_shim_first(&shim))
        .env("FAKE_ACP_REPLY_FILE", &reply)
        .env("FAKE_ACP_LOG", &acp_log)
        .output()
        .unwrap();
    assert!(!out.status.success(), "an unexecutable plan must fail the run:\n{}", stdout(&out));
    let methods = std::fs::read_to_string(&acp_log).unwrap();
    let count = |method: &str| methods.lines().filter(|l| *l == method).count();
    assert_eq!((count("initialize"), count("session/new"), count("session/prompt")), (1, 1, 2), "both attempts share one process and one session (#413):\n{methods}");
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
fn run_repairs_a_regressed_combined_check_in_the_same_tree() {
    let repo = init_repo("repair");
    let shim = shim_dir();
    std::fs::write(repo.join("alpha.txt"), "good\n").unwrap();
    run_git(&repo, &["add", "-A"]);
    run_git(&repo, &["commit", "-q", "-m", "add repair fixture"]);

    let repair_task = serde_json::json!({
        "writes": { "alpha.txt": "good\n" },
        "summary": "repaired alpha"
    })
    .to_string();
    std::fs::write(repo.join("repair-task.json"), &repair_task).unwrap();
    run_git(&repo, &["add", "-A"]);
    run_git(&repo, &["commit", "-q", "-m", "add fake repair task"]);
    let verify = repairable_check("alpha.txt", "good", "repair-task.json");
    let plan = serde_json::json!({
        "task": "repair a combined failure",
        "shared_context": "",
        "units": [
            { "name": "alpha", "files": ["alpha.txt"], "brief": write_task("alpha.txt", "bad\n", "broke alpha") },
            { "name": "beta", "files": ["beta.txt"], "brief": write_task("beta.txt", "B\n", "wrote beta") }
        ],
        "verify": verify
    });
    let plan_path = shim.join("repair-plan.json");
    std::fs::write(&plan_path, serde_json::to_string_pretty(&plan).unwrap()).unwrap();

    let out = pact(
        &repo,
        &shim,
        None,
        &["run", "--agent", "copilot", "--plan", plan_path.to_str().unwrap(), "--repair-attempts", "1", "ignored"],
    );
    assert!(out.status.success(), "repair run failed:\nstdout: {}\nstderr: {}", stdout(&out), stderr(&out));
    let text = stdout(&out);
    assert!(text.contains("repair attempt 1/1 for failed combined verification: 1 lane in parallel (alpha)"), "{text}");
    assert!(text.contains("repair attempt 1 (alpha): done") && text.contains("(committed)"), "{text}");
    assert!(text.contains(&format!("verify `{verify}`: passed")), "{text}");
    assert!(text.contains("run: OK"), "{text}");

    let manager = pact_vcs::WorkspaceManager::open(&repo).unwrap();
    let batch = manager.list_workspaces().unwrap().into_iter().find(|workspace| workspace.shared_batch.is_none()).expect("batch kept");
    assert_eq!(std::fs::read_to_string(batch.path.join("alpha.txt")).unwrap(), "good\n");
    assert_eq!(run_git(&batch.path, &["rev-list", "--count", &format!("{}..HEAD", batch.base_commit)]).trim(), "2");

    cleanup(&repo);
    cleanup(&shim);
}

#[test]
fn run_repairs_each_unit_in_parallel_in_its_own_lane() {
    // Issue #372: two units each break a check; each failure goes to the
    // unit owning the file, both lanes run in one round, one commit.
    let repo = init_repo("parallel-repair");
    let shim = shim_dir();
    for (file, fix) in [("alpha.txt", "alpha-fix.json"), ("beta.txt", "beta-fix.json")] {
        std::fs::write(repo.join(file), "good\n").unwrap();
        let task = serde_json::json!({ "writes": { file: "good\n" }, "summary": format!("repaired {file}") }).to_string();
        std::fs::write(repo.join(fix), task).unwrap();
    }
    run_git(&repo, &["add", "-A"]);
    run_git(&repo, &["commit", "-q", "-m", "fixtures"]);
    let plan = serde_json::json!({
        "task": "break two files",
        "shared_context": "",
        "units": [
            { "name": "alpha", "files": ["alpha.txt"], "brief": write_task("alpha.txt", "bad\n", "broke alpha") },
            { "name": "beta", "files": ["beta.txt"], "brief": write_task("beta.txt", "bad\n", "broke beta") }
        ],
    });
    let plan_path = shim.join("plan.json");
    std::fs::write(&plan_path, serde_json::to_string_pretty(&plan).unwrap()).unwrap();
    let check_alpha = repairable_check("alpha.txt", "good", "alpha-fix.json");
    let check_beta = repairable_check("beta.txt", "good", "beta-fix.json");

    let out = pact(&repo, &shim, None, &["run", "--agent", "copilot", "--plan", plan_path.to_str().unwrap(), "--verify", &check_alpha, "--verify", &check_beta, "x"]);
    assert!(out.status.success(), "{}\n{}", stdout(&out), stderr(&out));
    let text = stdout(&out);
    assert!(text.contains("repair attempt 1/1 for failed combined verification: 2 lanes in parallel (alpha, beta)"), "{text}");
    assert!(text.contains("repair attempt 1 (alpha): done") && text.contains("repair attempt 1 (beta): done"), "{text}");
    assert_eq!(text.matches("[planner] [phase] committing repair attempt 1").count(), 1, "one commit for the round:\n{text}");
    assert!(text.contains("run: OK"), "{text}");

    let manager = pact_vcs::WorkspaceManager::open(&repo).unwrap();
    let ids: Vec<String> = manager.list_workspaces().unwrap().into_iter().map(|w| w.id).collect();
    assert!(ids.contains(&"repair-1-alpha".to_string()) && ids.contains(&"repair-1-beta".to_string()), "{ids:?}");
    let batch = manager.list_workspaces().unwrap().into_iter().find(|w| w.shared_batch.is_none()).unwrap();
    assert_eq!(std::fs::read_to_string(batch.path.join("alpha.txt")).unwrap(), "good\n");
    assert_eq!(std::fs::read_to_string(batch.path.join("beta.txt")).unwrap(), "good\n");

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
fn run_kills_a_hanging_verification_and_reports_it_timed_out() {
    // Issue #410: a command that never exits must not hang the run. Here it
    // hangs only once alpha.txt exists, so the baseline passes and the
    // result times out; then one that hangs on the base too leaves no
    // baseline to compare against.
    let repo = init_repo("verify-timeout");
    let shim = shim_dir();
    let plan = serde_json::json!({
        "task": "hang",
        "shared_context": "",
        "units": [{ "name": "alpha", "files": ["alpha.txt"], "brief": write_task("alpha.txt", "A", "a") }],
    });
    let plan_path = shim.join("plan.json");
    std::fs::write(&plan_path, serde_json::to_string_pretty(&plan).unwrap()).unwrap();
    let hang_after = if cfg!(windows) { "if exist alpha.txt (ping -n 60 127.0.0.1 >nul) else (exit 0)" } else { "if [ -f alpha.txt ]; then sleep 60; fi" };
    let run = |verify: &str| {
        let started = std::time::Instant::now();
        let out = pact(
            &repo,
            &shim,
            None,
            &["run", "--agent", "copilot", "--plan", plan_path.to_str().unwrap(), "--verify", verify, "--verify-timeout", "2s", "--repair-attempts", "0", "x"],
        );
        assert!(started.elapsed() < std::time::Duration::from_secs(45), "the run waited out the command: {:?}", started.elapsed());
        out
    };

    let out = run(hang_after);
    assert_eq!(out.status.code(), Some(1), "{}\n{}", stdout(&out), stderr(&out));
    let text = stdout(&out);
    assert!(text.contains(&format!("[planner] [phase] baseline `{hang_after}` passes")), "{text}");
    assert!(text.contains(&format!("verify `{hang_after}`: TIMED OUT (killed): FAILED (it passed on the base commit")), "{text}");
    assert!(text.contains("    full output: ") && text.contains("after.log"), "{text}");
    assert!(text.contains("run: FAILED."), "{text}");

    assert!(pact(&repo, &shim, None, &["teardown", "--force"]).status.success());
    let hang_always = if cfg!(windows) { "ping -n 60 127.0.0.1 >nul" } else { "sleep 60" };
    let out = run(hang_always);
    assert_eq!(out.status.code(), Some(1), "{}", stdout(&out));
    let text = stdout(&out);
    assert!(text.contains("TIMED OUT after 2s (killed), so there is no baseline for it"), "{text}");
    assert!(text.contains(&format!("verify `{hang_always}`: TIMED OUT (killed): FAILED (exit ?")), "no baseline means a plain failure:\n{text}");

    let logs = std::fs::read_dir(state_dir(&repo).join("meta").join("verify")).unwrap().count();
    assert_eq!(logs, 2, "one verification log directory per run");

    cleanup(&repo);
    cleanup(&shim);
}

#[test]
fn run_verifies_with_ci_set() {
    let repo = init_repo("verify-ci");
    let shim = shim_dir();
    let plan = serde_json::json!({
        "task": "ci",
        "shared_context": "",
        "units": [{ "name": "alpha", "files": ["alpha.txt"], "brief": write_task("alpha.txt", "A", "a") }],
    });
    let plan_path = shim.join("plan.json");
    std::fs::write(&plan_path, serde_json::to_string_pretty(&plan).unwrap()).unwrap();
    let needs_ci = if cfg!(windows) { "if \"%CI%\"==\"true\" (exit 0) else (exit 1)" } else { "[ \"$CI\" = true ]" };
    let out = pact(&repo, &shim, None, &["run", "--agent", "copilot", "--plan", plan_path.to_str().unwrap(), "--verify", needs_ci, "x"]);
    assert!(out.status.success(), "{}\n{}", stdout(&out), stderr(&out));
    assert!(stdout(&out).contains(&format!("verify `{needs_ci}`: passed")), "{}", stdout(&out));
    cleanup(&repo);
    cleanup(&shim);
}

#[test]
fn run_lists_the_files_a_failing_check_names() {
    // Issue #373: the report names the failing files instead of a tail.
    let repo = init_repo("failing-files");
    let shim = shim_dir();
    let plan = serde_json::json!({
        "task": "t",
        "shared_context": "",
        "units": [{ "name": "alpha", "files": ["alpha.txt"], "brief": write_task("alpha.txt", "A", "a") }],
    });
    let plan_path = shim.join("plan.json");
    std::fs::write(&plan_path, serde_json::to_string_pretty(&plan).unwrap()).unwrap();
    let check = if cfg!(windows) {
        "if exist alpha.txt (echo alpha.txt:3:1 error broken & echo src.ts^(1,1^): error & echo noise & exit 1) else (exit 0)"
    } else {
        "if [ -f alpha.txt ]; then echo 'alpha.txt:3:1 error broken'; echo 'src.ts(1,1): error'; echo noise; exit 1; fi"
    };
    let out = pact(&repo, &shim, None, &["run", "--agent", "copilot", "--plan", plan_path.to_str().unwrap(), "--verify", check, "--repair-attempts", "0", "x"]);
    assert_eq!(out.status.code(), Some(1), "{}\n{}", stdout(&out), stderr(&out));
    let text = stdout(&out);
    assert!(text.contains("    failing files (2):\n      alpha.txt\n      src.ts\n    full output: "), "{text}");
    assert!(!text.contains("    noise"), "the tail is replaced by the file list:\n{text}");
    cleanup(&repo);
    cleanup(&shim);
}

#[test]
fn run_names_a_lane_that_writes_another_units_file_or_an_unowned_one() {
    // Issue #409: the plan is disjoint, the lanes need not be.
    let repo = init_repo("ownership");
    let shim = shim_dir();
    let alpha = serde_json::json!({ "writes": { "alpha.txt": "A", "beta.txt": "CLOBBERED BY ALPHA", "stray.txt": "S" }, "summary": "a" }).to_string();
    let plan = serde_json::json!({
        "task": "two files",
        "shared_context": "",
        "units": [
            { "name": "alpha", "files": ["alpha.txt"], "brief": alpha },
            { "name": "beta", "files": ["beta.txt"], "brief": write_task("beta.txt", "B from beta", "b") }
        ],
    });
    let plan_path = shim.join("plan.json");
    std::fs::write(&plan_path, serde_json::to_string_pretty(&plan).unwrap()).unwrap();

    let out = pact(&repo, &shim, None, &["run", "--agent", "copilot", "--plan", plan_path.to_str().unwrap(), "x"]);
    assert_eq!(out.status.code(), Some(1), "{}\n{}", stdout(&out), stderr(&out));
    let text = stdout(&out);
    assert!(text.contains("ownership: lane `alpha` wrote `beta.txt`, which unit `beta` owns"), "{text}");
    assert!(text.contains("ownership: lane `alpha` wrote `stray.txt`, which no unit owns"), "{text}");
    assert!(text.contains("run: FAILED. 2 file(s) were written by a lane that does not own them"), "{text}");
    assert!(text.contains("(committed)"), "nothing is reverted; the result is committed for review:\n{text}");

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
