//! `pact run` (issue #305): pact owns decomposition, briefing, execution
//! and verification from one big task.
//!
//! Until now every piece of judgement lived outside pact: a human or an
//! orchestrating agent split the work into `--task` units, wrote the
//! briefs, picked the lane count, ran `commit-all`/`merge-all`, verified.
//! The benchmark kit needed a 2.4 KB header just to get a Copilot session
//! to do that acceptably, and the lessons it encoded (disjoint file
//! ownership, verbatim conventions in every brief, "do not commit",
//! "you cannot install or build") are things pact already knows.
//!
//! The MVP here is one wave: a planner session returns units with the
//! files each owns; pact validates the plan mechanically (disjoint,
//! well-formed, roughly balanced), sends violations back to the planner
//! up to a few times, renders one self-contained brief per unit, runs
//! the batch as a shared tree (disjoint by construction, so no isolation
//! and no merge), commits once, runs the task-level verification in the
//! batch worktree and reports. Dependent units (waves, #282) and
//! gap-closing after a failed verification are deliberately not here.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use pact_agents::{AgentEvent, AgentKind, LaunchRequest, Supervisor};
use pact_vcs::Workspace;
use serde::{Deserialize, Serialize};

use crate::{agent_kind_name, effective_runtime, unix_now, LaneRuntime, Orchestrator, SpawnManyOutcome, SpawnManyTask, SpawnOptions};

/// What the planner returns and pact executes. Persisted under
/// `meta/plans/` so a human can edit it and re-run with `--plan`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Plan {
    #[serde(default)]
    pub task: String,
    /// Conventions every unit must follow, rendered verbatim into every
    /// brief: the thing orchestrator headers always had to repeat.
    #[serde(default)]
    pub shared_context: String,
    pub units: Vec<PlanUnit>,
    /// Task-level acceptance commands, each run once in the batch
    /// worktree after `commit-all` (and once on the untouched tree first,
    /// for the baseline). A string or an array in the JSON (issue #360);
    /// `--verify` on the command line replaces the list.
    #[serde(default, deserialize_with = "string_or_list")]
    pub verify: Vec<String>,
}

/// Accepts `"npm test"`, `["npm test", "npm run lint"]`, or `null`.
fn string_or_list<'de, D: serde::Deserializer<'de>>(deserializer: D) -> std::result::Result<Vec<String>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum StringOrList {
        One(String),
        Many(Vec<String>),
    }
    Ok(match Option::<StringOrList>::deserialize(deserializer)? {
        None => Vec::new(),
        Some(StringOrList::One(s)) => vec![s],
        Some(StringOrList::Many(list)) => list,
    })
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlanUnit {
    pub name: String,
    /// Repo-relative paths this unit owns: the files it creates or
    /// edits. No two units may own the same path.
    pub files: Vec<String>,
    /// What the unit must produce, in the planner's words.
    pub brief: String,
    /// A unit-scoped check the worker runs on its own work.
    #[serde(default)]
    pub verify: Option<String>,
}

/// Mechanical plan checks, pure so they are unit-testable. Empty means
/// the plan can run. Each entry is one violation in words the planner
/// can act on.
pub fn validate_plan(plan: &Plan, max_units: usize) -> Vec<String> {
    let mut problems = Vec::new();
    if plan.units.is_empty() {
        problems.push("the plan has no units".to_string());
        return problems;
    }
    if plan.units.len() > max_units {
        problems.push(format!("the plan has {} units; at most {max_units} are allowed", plan.units.len()));
    }
    let mut seen_names: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut owners: std::collections::HashMap<String, Vec<String>> = std::collections::HashMap::new();
    for unit in &plan.units {
        let name = unit.name.trim();
        if !name.chars().any(|c| c.is_ascii_alphanumeric()) {
            problems.push(format!("unit name {:?} must contain at least one ASCII letter or digit", unit.name));
        }
        if !seen_names.insert(name.to_ascii_lowercase()) {
            problems.push(format!("unit name {name:?} is used more than once"));
        }
        if unit.files.is_empty() {
            problems.push(format!("unit {name:?} owns no files"));
        }
        if unit.brief.trim().is_empty() {
            problems.push(format!("unit {name:?} has an empty brief"));
        }
        for file in &unit.files {
            let normalized = file.replace('\\', "/");
            if normalized.starts_with('/') || normalized.contains(':') || normalized.split('/').any(|seg| seg == "..") {
                problems.push(format!("unit {name:?} file {file:?} must be a repo-relative path without `..`"));
            }
            owners.entry(normalized).or_default().push(name.to_string());
        }
    }
    let mut overlaps: Vec<(String, Vec<String>)> = owners.into_iter().filter(|(_, units)| units.len() > 1).collect();
    overlaps.sort();
    for (file, units) in overlaps {
        problems.push(format!("file {file:?} is owned by more than one unit ({}); give it to exactly one", units.join(", ")));
    }
    problems
}

/// Line counts of the files each unit owns that already exist, as a
/// rough weight. New files count zero, so a plan that only creates files
/// weighs nothing; the balance warning is advisory for that reason.
pub fn unit_weights(plan: &Plan, repo_root: &Path) -> Vec<(String, usize)> {
    plan.units
        .iter()
        .map(|unit| {
            let lines = unit
                .files
                .iter()
                .filter_map(|f| std::fs::read_to_string(repo_root.join(f)).ok())
                .map(|text| text.lines().count())
                .sum();
            (unit.name.clone(), lines)
        })
        .collect()
}

/// `Some(warning)` when the heaviest unit (by existing lines) is more
/// than four times the lightest, both non-zero. Never a rejection: the
/// planner's split may be right for reasons line counts cannot see.
pub fn balance_warning(weights: &[(String, usize)]) -> Option<String> {
    let nonzero: Vec<&(String, usize)> = weights.iter().filter(|(_, w)| *w > 0).collect();
    if nonzero.len() < 2 {
        return None;
    }
    let heaviest = nonzero.iter().max_by_key(|(_, w)| *w).unwrap();
    let lightest = nonzero.iter().min_by_key(|(_, w)| *w).unwrap();
    if heaviest.1 > lightest.1.saturating_mul(4) {
        Some(format!(
            "unit {:?} owns {} existing lines and {:?} owns {}; the heaviest unit sets the batch's wall-clock, so consider splitting it",
            heaviest.0, heaviest.1, lightest.0, lightest.1
        ))
    } else {
        None
    }
}

/// The JSON object in a planner reply: the last ```json fenced block if
/// there is one, else the span from the first `{` to the last `}`.
pub fn extract_plan_json(text: &str) -> Option<String> {
    let mut last_fenced: Option<String> = None;
    let mut rest = text;
    while let Some(start) = rest.find("```json") {
        let after = &rest[start + "```json".len()..];
        match after.find("```") {
            Some(end) => {
                last_fenced = Some(after[..end].trim().to_string());
                rest = &after[end + 3..];
            }
            None => break,
        }
    }
    if last_fenced.is_some() {
        return last_fenced;
    }
    let start = text.find('{')?;
    let end = text.rfind('}')?;
    (end > start).then(|| text[start..=end].to_string())
}

/// Parses a planner reply into a `Plan`, filling in `task` when the
/// planner left it out.
pub fn parse_plan(reply: &str, task: &str) -> Result<Plan> {
    let json = extract_plan_json(reply).ok_or_else(|| anyhow::anyhow!("the planner's reply contains no JSON object"))?;
    let mut plan: Plan = serde_json::from_str(&json).with_context(|| format!("the planner's JSON does not match the plan schema:\n{json}"))?;
    if plan.task.trim().is_empty() {
        plan.task = task.to_string();
    }
    Ok(plan)
}

const PLAN_SCHEMA: &str = r#"{
  "shared_context": "repository facts every unit needs that the task text does not state, in a few sentences; empty string if there are none",
  "verify": ["shell commands that check the whole task once everything is merged, e.g. npm test, npm run typecheck"],
  "units": [
    {
      "name": "short-kebab-case-name",
      "files": ["repo/relative/path/this/unit/creates-or-edits.ts"],
      "brief": "two or three sentences: the existing file to imitate, the one or two non-obvious things you found, the acceptance criteria",
      "verify": "optional cheap shell command scoped to this unit's own files (its test files alone); never the whole suite, type-check or lint"
    }
  ]
}"#;

/// The planner's instructions. The planner works in the repo root with
/// its own tools, so pact does not inline the tree; it inlines the rules
/// the benchmark headers had to carry, and the repository's existing
/// tests as style anchors when there are any (`anchors`), so the briefs
/// name a concrete file to imitate instead of hoping the planner finds
/// one. Workers receive the task text verbatim (`render_brief`), and the
/// prompt says so: a plan that restates the task is output tokens every
/// worker waits for (issue #347).
pub fn planner_prompt(task: &str, max_units: usize, anchors: &[String]) -> String {
    let anchors_section = if anchors.is_empty() {
        String::new()
    } else {
        format!(
            "\nEXISTING TESTS IN THIS REPOSITORY (read them first; they set the conventions, and every brief that \
             writes tests must name the closest one to imitate):\n{}\n",
            anchors.iter().map(|a| format!("- {a}")).collect::<Vec<_>>().join("\n")
        )
    };
    format!(
        "You are planning parallel work for pact, a tool that runs several coding agents at once, each in its \
         own lane, all writing into one shared checkout of this repository. Read the repository as needed, \
         then decompose the task below into independent units that can run at the same time.\n\n\
         TASK:\n{task}\n{anchors_section}\n\
         RULES:\n\
         - Between 1 and {max_units} units. Prefer more, smaller units when the work allows.\n\
         - Balance units by EFFORT, not by file count: the slowest unit decides how long the whole batch \
         takes. A UI component or a route handler with mocked I/O costs several times a pure function \
         module, so a unit of five components is not balanced against a unit of five helpers; give heavy \
         files fewer companions, and split a unit rather than let it hold more than two or three heavy ones.\n\
         - Each unit lists every file it will create or edit under `files`, repo-relative. No file may appear \
         in two units. Files a unit only reads are not listed.\n\
         - Shared files that several units would need to edit (barrels, setup, config, lockfiles) go to exactly \
         one unit, or the task is restructured so nobody edits them.\n\
         - Every worker receives the complete TASK text above verbatim, together with its own `files` list, \
         `brief` and `shared_context`, and reads its source files itself. Do not restate anything the task \
         already says: not its rules, not its conventions, not its per-file requirements. Do not summarize a \
         file's contents: the worker reads the file in seconds and your summary costs every worker the \
         time you spend writing it. `shared_context` is for repository facts every unit needs that the \
         task does not state, in a few sentences; leave it empty when there are none. A `brief` is two or \
         three sentences: the existing file to imitate, the one or two non-obvious things you found \
         (an unexported symbol, an awkward dependency to mock), and the acceptance criteria.\n\
         - Your reply is not read by a person; it is parsed, and every worker waits for it to finish. \
         Keep it short.\n\
         - Project-wide checks are pact's job, run once on the combined result after every unit finishes: \
         put every check the task demands of the whole (the full test suite, type-check, lint, coverage) in \
         the plan's `verify` list. A unit's own `verify` must be cheap and scoped to its files (run its test \
         files alone); workers are told not to run anything project-wide, because many lanes doing so at \
         once is slower than one run at the end.\n\
         - Workers cannot install packages, run builds or start dev servers, and must not commit; pact \
         commits. Do not ask them to.\n\
         - Do not create, modify or delete any file yourself. Plan only.\n\n\
         Reply with one JSON object in a ```json fenced block and nothing after it, in this shape:\n\
         ```json\n{PLAN_SCHEMA}\n```"
    )
}

/// Tracked test files in the repository, shortest paths first and at most
/// `limit` of them: the style anchors handed to the planner. Uses
/// `git ls-files` so ignored and generated files never qualify.
pub fn discover_test_anchors(repo_root: &Path, limit: usize) -> Vec<String> {
    let Ok(output) = Command::new("git").args(["ls-files", "-z"]).current_dir(repo_root).output() else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    let listing = String::from_utf8_lossy(&output.stdout);
    let mut anchors: Vec<String> = listing
        .split('\0')
        .filter(|path| !path.is_empty())
        .filter(|path| looks_like_a_test_file(path))
        .map(str::to_string)
        .collect();
    anchors.sort_by_key(|p| (p.len(), p.clone()));
    anchors.truncate(limit);
    anchors
}

fn looks_like_a_test_file(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    let name = lower.rsplit('/').next().unwrap_or(&lower);
    name.contains(".test.")
        || name.contains(".spec.")
        || name.starts_with("test_")
        || name.ends_with("_test.go")
        || name.ends_with("_test.py")
        || lower.split('/').any(|seg| seg == "__tests__" || seg == "tests" || seg == "test")
}

/// The retry prompt: the previous plan and what was wrong with it.
pub fn repair_prompt(previous_json: &str, problems: &[String]) -> String {
    format!(
        "Your plan could not be executed. Problems:\n{}\n\nPrevious plan:\n```json\n{previous_json}\n```\n\n\
         Fix every problem and reply again with one complete JSON plan in a ```json fenced block and nothing after it.",
        problems.iter().map(|p| format!("- {p}")).collect::<Vec<_>>().join("\n")
    )
}

/// One unit's self-contained worker brief. The shared-tree preamble
/// (claim your files, touch nothing else) is added by `spawn_many`.
pub fn render_brief(plan: &Plan, unit: &PlanUnit) -> String {
    let mut brief = format!(
        "# Unit `{}`\n\nPart of a larger task pact has split across parallel workers:\n> {}\n\n\
         ## Your files\n\nYou own these paths and nothing else. Create or edit only them.\n",
        unit.name,
        plan.task.lines().collect::<Vec<_>>().join("\n> ")
    );
    for file in &unit.files {
        brief.push_str(&format!("- `{file}`\n"));
    }
    brief.push_str(&format!(
        "\n## What to produce\n\nDo the task above for the files you own, and only those. The planner adds, for your unit:\n\n{}\n",
        unit.brief.trim()
    ));
    if !plan.shared_context.trim().is_empty() {
        brief.push_str(&format!("\n## Conventions (shared by every unit)\n\n{}\n", plan.shared_context.trim()));
    }
    brief.push_str(
        "\n## Rules\n\n\
         - Dependencies are already installed. Do not install packages, run full builds, or start dev servers.\n\
         - Do not commit, stage, or run any `git` command; pact commits your work.\n\
         - Other workers are editing other files in this same checkout right now. Do not touch files outside your list, and do not revert or reformat anything you did not write.\n",
    );
    let unit_check = unit.verify.as_deref().map(str::trim).filter(|v| !v.is_empty());
    let project_checks: Vec<&str> = plan.verify.iter().map(|v| v.trim()).filter(|v| !v.is_empty()).collect();
    if project_checks.is_empty() {
        if let Some(verify) = unit_check {
            brief.push_str(&format!("- Check your own work before finishing with: `{verify}`\n"));
        }
    } else {
        // Issue #361: project-wide checks are pact's, once, on the combined
        // result. Workers running them from every lane at once is what made
        // finer splits no faster (arm R3).
        brief.push_str(&format!(
            "- pact runs the project-wide checks once on the combined result after every unit finishes: {}. Do not run them yourself, \
             and do not run the whole test suite, type-check or lint in any form: every lane doing so at once slows every lane. ",
            project_checks.iter().map(|c| format!("`{c}`")).collect::<Vec<_>>().join(", ")
        ));
        match unit_check {
            Some(verify) => brief.push_str(&format!("Check only your own files, with: `{verify}`\n")),
            None => brief.push_str("Check only your own files (run your own test files alone, not the suite).\n"),
        }
    }
    brief.push_str("- When finished, reply DONE followed by two lines: what you produced and anything left undone.\n");
    brief
}

/// Everything `pact run` decided and did, for the CLI to print and for
/// tests to assert on.
pub struct RunReport {
    pub plan: Plan,
    pub plan_path: PathBuf,
    pub planner_attempts: usize,
    /// What the planner did, as one JSON line per agent update (the same
    /// shape as a lane's `logs/<id>.jsonl`); `None` when the plan came
    /// from a file. Issue #348.
    pub planner_log: Option<PathBuf>,
    /// Wall time spent planning, all attempts included; zero when the
    /// plan came from a file.
    pub planning: Duration,
    pub balance_warning: Option<String>,
    pub brief_paths: Vec<PathBuf>,
    pub dry_run: bool,
    pub outcomes: Vec<SpawnManyOutcome>,
    pub batch: Option<Workspace>,
    pub committed: Option<bool>,
    pub repairs: Vec<RepairOutcome>,
    pub verify: Vec<VerifyOutcome>,
}

pub struct RepairOutcome {
    pub attempt: usize,
    pub outcome: SpawnManyOutcome,
    pub committed: bool,
}

impl RunReport {
    /// True when every lane ran to success, the commit landed (or there
    /// was nothing to commit) and verification (if any) passed.
    pub fn succeeded(&self) -> bool {
        if self.dry_run {
            return true;
        }
        let lanes_ok = !self.outcomes.is_empty()
            && self.outcomes.iter().all(|o| matches!(&o.result, Ok((_, run)) if run.success));
        // An inconclusive verification is not a success: the user has to
        // judge the result some other way, and the exit code says so.
        lanes_ok && self.verify.iter().all(|v| v.success)
    }

    /// The verdict that decides the exit code: the worst across the
    /// verification commands, where a real failure outranks an
    /// inconclusive one.
    pub fn worst_verdict(&self) -> Option<Verdict> {
        self.verify.iter().map(|v| v.verdict()).max_by_key(|v| v.severity())
    }
}

pub struct VerifyOutcome {
    pub command: String,
    pub success: bool,
    pub exit_code: Option<i32>,
    pub output_tail: String,
    pub duration: Duration,
    /// Whether the same command passed on the untouched tree before any
    /// lane ran. `None` when no baseline was taken.
    pub baseline_success: Option<bool>,
}

/// How a verification result reads once the baseline is known.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Passed,
    /// Passed now, failed on the base: the run fixed it.
    Fixed,
    /// Failed now, passed on the base: the run broke it.
    Regressed,
    /// Fails on the base too, so this command cannot judge the run.
    Inconclusive,
    /// Failed with no baseline to compare against.
    Failed,
}

impl Verdict {
    /// Ordering for `RunReport::worst_verdict`: passing verdicts lowest,
    /// then inconclusive, then the two real failures.
    fn severity(self) -> u8 {
        match self {
            Verdict::Passed | Verdict::Fixed => 0,
            Verdict::Inconclusive => 1,
            Verdict::Regressed | Verdict::Failed => 2,
        }
    }
}

impl VerifyOutcome {
    pub fn verdict(&self) -> Verdict {
        match (self.success, self.baseline_success) {
            (true, Some(false)) => Verdict::Fixed,
            (true, _) => Verdict::Passed,
            (false, Some(true)) => Verdict::Regressed,
            (false, Some(false)) => Verdict::Inconclusive,
            (false, None) => Verdict::Failed,
        }
    }
}

fn verification_needs_repair(outcomes: &[VerifyOutcome]) -> bool {
    outcomes.iter().any(|outcome| matches!(outcome.verdict(), Verdict::Regressed | Verdict::Failed))
}

fn render_repair_brief(attempt: usize, total: usize, outcomes: &[VerifyOutcome], touched_files: &[String]) -> String {
    let mut brief = format!(
        "# Repair attempt {attempt}/{total}\n\nMake the combined verification checks pass without changing application behaviour. "
    );
    brief.push_str("Touch only files this run created or edited, listed below. Do not edit any other file.\n\n## Files you may touch\n");
    for file in touched_files {
        brief.push_str(&format!("- `{file}`\n"));
    }
    brief.push_str("\n## Failing checks\n");
    for outcome in outcomes.iter().filter(|outcome| matches!(outcome.verdict(), Verdict::Regressed | Verdict::Failed)) {
        brief.push_str(&format!("\n### `{}`\n\n```text\n{}\n```\n", outcome.command, outcome.output_tail));
    }
    brief.push_str("\nRun the failing checks after the fix. Keep the change as small as possible.\n");
    brief
}

impl std::fmt::Display for Verdict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Verdict::Passed => "passed",
            Verdict::Fixed => "passed (it failed on the base commit, so this run fixed it)",
            Verdict::Regressed => "FAILED (it passed on the base commit, so this run broke it)",
            Verdict::Inconclusive => "INCONCLUSIVE (it already fails on the base commit, so it cannot judge this run)",
            Verdict::Failed => "FAILED",
        })
    }
}

pub struct RunOptions<'a> {
    pub agent: AgentKind,
    pub max_units: usize,
    /// Replaces the plan's own `verify` list when non-empty.
    pub verify: &'a [String],
    /// Reuse a persisted plan instead of calling the planner.
    pub plan_path: Option<&'a Path>,
    pub dry_run: bool,
    /// How many times a rejected plan is sent back to the planner.
    pub plan_retries: usize,
    /// How many lanes may try to repair a real combined-verification failure.
    pub repair_attempts: usize,
    pub spawn: SpawnOptions<'a>,
}

const PLANNER_LABEL: usize = usize::MAX;

impl Orchestrator {
    /// Plans, briefs, executes, commits and verifies `task`. Events are
    /// labelled by lane index; the planner's own events use
    /// `usize::MAX` as the index.
    ///
    /// When the planner is consulted (no `--plan`) and something will be
    /// spawned (no `--dry-run`), the shared tree, its dependencies and,
    /// when `--verify` is known up front, the baseline are prepared on a
    /// second thread while the planner works (issue #353): none of them
    /// depend on the plan, and together they are half a minute the lanes
    /// would otherwise wait for after planning.
    pub fn run_task(
        &self,
        task: &str,
        options: &RunOptions<'_>,
        on_event: impl Fn(usize, &AgentKind, &AgentEvent) + Sync,
    ) -> Result<RunReport> {
        let planning_started = std::time::Instant::now();
        let planner_events = |event: &AgentEvent| on_event(PLANNER_LABEL, &options.agent, event);
        let spawn_options = SpawnOptions { shared_tree: true, ..options.spawn };
        let (plan, planning, prepared) = match options.plan_path {
            Some(path) => {
                let text = std::fs::read_to_string(path).with_context(|| format!("reading plan {}", path.display()))?;
                let mut plan: Plan = serde_json::from_str(&text).with_context(|| format!("parsing plan {}", path.display()))?;
                if plan.task.trim().is_empty() {
                    plan.task = task.to_string();
                }
                let problems = validate_plan(&plan, options.max_units);
                if !problems.is_empty() {
                    bail!("plan {} cannot run:\n{}", path.display(), problems.iter().map(|p| format!("- {p}")).collect::<Vec<_>>().join("\n"));
                }
                (plan, Planning { attempts: 0, log: None, elapsed: Duration::ZERO }, None)
            }
            None if options.dry_run => {
                let (plan, attempts, log) = self.plan_task(task, options, &mut |event| planner_events(event))?;
                (plan, Planning { attempts, log: Some(log), elapsed: planning_started.elapsed() }, None)
            }
            None => {
                let (planned, prepared) = std::thread::scope(|scope| {
                    let prep = scope.spawn(|| self.prepare_run_tree(task, &spawn_options, options.verify, &planner_events));
                    let planned = self.plan_task(task, options, &mut |event| planner_events(event));
                    (planned, prep.join().unwrap_or_else(|_| Err(anyhow::anyhow!("the thread preparing the shared tree panicked"))))
                });
                let elapsed = planning_started.elapsed();
                let (plan, attempts, log) = match planned {
                    Ok(planned) => planned,
                    Err(err) => {
                        if let Ok(prepared) = prepared {
                            planner_events(&AgentEvent::Phase(format!("planning failed; removing the prepared shared tree {}", prepared.batch.id)));
                            if let Err(cleanup) = self.workspaces.remove_workspace(&prepared.batch.id, false, true) {
                                tracing::warn!("removing shared tree {} after a failed plan: {cleanup:#}", prepared.batch.id);
                            }
                        }
                        return Err(err);
                    }
                };
                let prepared = match prepared {
                    Ok(prepared) => prepared,
                    Err(err) => {
                        let plan_path = self.persist_plan(&plan)?;
                        return Err(err.context(format!(
                            "preparing the shared tree while planning; the plan is saved at {} and can be rerun with `pact run --plan {}`",
                            plan_path.display(),
                            plan_path.display()
                        )));
                    }
                };
                (plan, Planning { attempts, log: Some(log), elapsed }, Some(prepared))
            }
        };
        let mut plan = plan;
        if !options.verify.is_empty() {
            // Recorded on the plan so the persisted file is the whole truth.
            plan.verify = options.verify.to_vec();
        }
        self.run_plan(plan, planning, prepared, &spawn_options, options, on_event)
    }

    /// The shared tree a run executes in, with its dependencies prepared
    /// and, when the verification commands are already known, their
    /// baselines run on the untouched tree. Disjoint by validation, so
    /// the shared tree is the right shape: no per-lane isolation to pay
    /// for and no merge afterwards. The baseline exists so a command that
    /// already fails on the base (generated files missing from a fresh
    /// worktree, say) is never read as this run's doing.
    fn prepare_run_tree(
        &self,
        task: &str,
        spawn_options: &SpawnOptions<'_>,
        verify: &[String],
        on_event: &(impl Fn(&AgentEvent) + Sync),
    ) -> Result<PreparedTree> {
        let summary = format!("pact run: {}", task.lines().find(|l| !l.trim().is_empty()).unwrap_or("").trim().chars().take(80).collect::<String>());
        on_event(&AgentEvent::Phase("creating the shared tree while the planner works".to_string()));
        let mut forward = |event: &AgentEvent| on_event(event);
        let batch = self
            .create_shared_batch_workspace_named(&summary, spawn_options, &mut forward)
            .context("creating the shared tree for the plan")?;
        let baselines = if verify.is_empty() { None } else { Some(self.run_baselines(&batch, verify, on_event)?) };
        Ok(PreparedTree { batch, baselines })
    }

    /// One baseline per verification command, on the untouched tree.
    fn run_baselines(&self, batch: &Workspace, commands: &[String], on_event: &impl Fn(&AgentEvent)) -> Result<Vec<VerifyOutcome>> {
        commands
            .iter()
            .map(|command| {
                on_event(&AgentEvent::Phase(format!("verification baseline on the untouched tree: {command}")));
                let outcome = run_shell_captured(&batch.path, command)?;
                on_event(&AgentEvent::Phase(format!(
                    "baseline `{command}` {} in {:.1}s",
                    if outcome.success { "passes" } else { "already FAILS before any lane runs" },
                    outcome.duration.as_secs_f32()
                )));
                Ok(outcome)
            })
            .collect()
    }

    fn persist_plan(&self, plan: &Plan) -> Result<PathBuf> {
        let plans_dir = self.workspaces.state_dir().join("meta").join("plans");
        std::fs::create_dir_all(&plans_dir)?;
        let plan_path = plans_dir.join(format!("{}-{}.json", unix_now(), short_slug(&plan.task)));
        std::fs::write(&plan_path, serde_json::to_vec_pretty(plan)?).with_context(|| format!("writing {}", plan_path.display()))?;
        Ok(plan_path)
    }

    fn run_plan(
        &self,
        plan: Plan,
        planning: Planning,
        prepared: Option<PreparedTree>,
        spawn_options: &SpawnOptions<'_>,
        options: &RunOptions<'_>,
        on_event: impl Fn(usize, &AgentKind, &AgentEvent) + Sync,
    ) -> Result<RunReport> {
        let stamp = unix_now();
        let plan_path = self.persist_plan(&plan)?;

        let weights = unit_weights(&plan, &self.repo_root);
        let balance = balance_warning(&weights);

        let briefs_dir = self.workspaces.state_dir().join("briefs").join(stamp.to_string());
        std::fs::create_dir_all(&briefs_dir)?;
        let mut brief_paths = Vec::new();
        let mut tasks = Vec::new();
        for unit in &plan.units {
            let brief = render_brief(&plan, unit);
            let path = briefs_dir.join(format!("{}.md", unit.name));
            std::fs::write(&path, &brief).with_context(|| format!("writing {}", path.display()))?;
            brief_paths.push(path);
            tasks.push(SpawnManyTask { agent: options.agent, task: brief, name: Some(unit.name.clone()) });
        }

        if options.dry_run {
            return Ok(RunReport {
                plan,
                plan_path,
                planner_attempts: planning.attempts,
                planner_log: planning.log,
                planning: planning.elapsed,
                balance_warning: balance,
                brief_paths,
                dry_run: true,
                outcomes: Vec::new(),
                batch: None,
                committed: None,
                repairs: Vec::new(),
                verify: Vec::new(),
            });
        }

        // The shared tree may already exist (prepared while the planner
        // worked, issue #353); otherwise it is created here. Either way
        // the baselines, when the plan names verification commands, run
        // on the untouched tree before any lane does.
        let planner_events = |event: &AgentEvent| on_event(PLANNER_LABEL, &options.agent, event);
        let (batch, baselines) = match prepared {
            Some(PreparedTree { batch, baselines }) => (batch, baselines),
            None => {
                let batch = self
                    .create_shared_batch_workspace(&tasks, spawn_options, planner_events)
                    .context("creating the shared tree for the plan")?;
                (batch, None)
            }
        };
        let verify_commands: Vec<String> = plan.verify.iter().map(|v| v.trim().to_string()).filter(|v| !v.is_empty()).collect();
        let baselines = match baselines {
            Some(outcomes) => outcomes,
            None if verify_commands.is_empty() => Vec::new(),
            None => self.run_baselines(&batch, &verify_commands, &planner_events)?,
        };
        let outcomes = self.spawn_many_in(tasks, spawn_options, Some(batch.clone()), &on_event);
        let batch = self.workspaces.get_workspace(&batch.id).ok();

        let mut committed = None;
        let mut repairs = Vec::new();
        let mut verify = Vec::new();
        if let Some(batch) = &batch {
            planner_events(&AgentEvent::Phase(format!("committing the shared tree {}", batch.id)));
            committed = Some(self.workspaces.commit_all(&batch.id).context("committing the batch")?);
            verify = self.verify_run(batch, &verify_commands, &baselines, &planner_events)?;

            let touched_files = self.workspaces.workspace_changes(&batch.id).context("finding files touched by the run")?.files;
            for attempt in 1..=options.repair_attempts {
                if !verification_needs_repair(&verify) {
                    break;
                }
                planner_events(&AgentEvent::Phase(format!(
                    "repair attempt {attempt}/{} for failed combined verification",
                    options.repair_attempts
                )));
                let task = SpawnManyTask {
                    agent: options.agent,
                    task: render_repair_brief(attempt, options.repair_attempts, &verify, &touched_files),
                    name: Some(format!("repair-{attempt}")),
                };
                let repair_index = plan.units.len() + attempt - 1;
                let mut repair_outcomes = self.spawn_many_in(vec![task], spawn_options, Some(batch.clone()), |_, agent, event| {
                    on_event(repair_index, agent, event)
                });
                let outcome = repair_outcomes.pop().expect("one repair task produces one outcome");
                let lane_succeeded = matches!(&outcome.result, Ok((_, run)) if run.success);
                let repair_committed = if lane_succeeded {
                    planner_events(&AgentEvent::Phase(format!("committing repair attempt {attempt}")));
                    let repair_committed = self.workspaces.commit_all(&batch.id).context("committing the repair")?;
                    committed = Some(committed.unwrap_or(false) || repair_committed);
                    verify = self.verify_run(batch, &verify_commands, &baselines, &planner_events)?;
                    repair_committed
                } else {
                    false
                };
                repairs.push(RepairOutcome { attempt, outcome, committed: repair_committed });
                if !lane_succeeded {
                    break;
                }
            }
        }

        Ok(RunReport {
            plan,
            plan_path,
            planner_attempts: planning.attempts,
            planner_log: planning.log,
            planning: planning.elapsed,
            balance_warning: balance,
            brief_paths,
            dry_run: false,
            outcomes,
            batch,
            committed,
            repairs,
            verify,
        })
    }

    fn verify_run(
        &self,
        batch: &Workspace,
        commands: &[String],
        baselines: &[VerifyOutcome],
        on_event: &impl Fn(&AgentEvent),
    ) -> Result<Vec<VerifyOutcome>> {
        commands
            .iter()
            .enumerate()
            .map(|(index, command)| {
                on_event(&AgentEvent::Phase(format!("verifying in the batch worktree: {command}")));
                let mut outcome = run_shell_captured(&batch.path, command)?;
                outcome.baseline_success = baselines.get(index).map(|baseline| baseline.success);
                on_event(&AgentEvent::Phase(format!(
                    "verification `{command}` {} in {:.1}s",
                    outcome.verdict(),
                    outcome.duration.as_secs_f32()
                )));
                Ok(outcome)
            })
            .collect()
    }

    /// Asks the planner for a plan, validates it, and sends violations
    /// back up to `plan_retries` times. Refuses a planner that modified
    /// the repository: planning is read-only by contract. Every attempt
    /// appends to one `logs/planner-<stamp>.jsonl`, each attempt opened
    /// by a `{"pact": {"planner_attempt": n}}` line (issue #348).
    fn plan_task(
        &self,
        task: &str,
        options: &RunOptions<'_>,
        on_event: &mut impl FnMut(&AgentEvent),
    ) -> Result<(Plan, usize, PathBuf)> {
        let before = pact_vcs::changed_paths(&self.repo_root).unwrap_or_default();
        let anchors = discover_test_anchors(&self.repo_root, 6);
        if !anchors.is_empty() {
            on_event(&AgentEvent::Phase(format!("handing the planner {} existing test file(s) to imitate", anchors.len())));
        }
        let log_path = self.workspaces.state_dir().join("logs").join(format!("planner-{}.jsonl", unix_now()));
        let mut prompt = planner_prompt(task, options.max_units, &anchors);
        let mut attempts = 0;
        loop {
            attempts += 1;
            on_event(&AgentEvent::Phase(format!("planning (attempt {attempts})")));
            append_log_line(&log_path, &serde_json::json!({ "t": crate::acp_runtime::unix_millis(), "pact": { "planner_attempt": attempts, "prompt_chars": prompt.len() } }))?;
            let reply = self.ask_agent(options.agent, &self.repo_root, &prompt, &options.spawn, &log_path, on_event)?;
            let after = pact_vcs::changed_paths(&self.repo_root).unwrap_or_default();
            let touched: Vec<&String> = after.iter().filter(|p| !before.contains(p)).collect();
            if !touched.is_empty() {
                bail!(
                    "the planner modified the repository, which planning must never do: {}. Inspect with `git status` and revert before running again",
                    touched.iter().map(|p| p.as_str()).collect::<Vec<_>>().join(", ")
                );
            }
            let plan = match parse_plan(&reply, task) {
                Ok(plan) => plan,
                Err(err) if attempts <= options.plan_retries => {
                    on_event(&AgentEvent::Phase(format!("plan rejected: {err:#}")));
                    prompt = repair_prompt(&extract_plan_json(&reply).unwrap_or_else(|| reply.clone()), &[format!("{err:#}")]);
                    continue;
                }
                Err(err) => return Err(err.context(format!("the planner produced no usable plan in {attempts} attempt(s)"))),
            };
            let problems = validate_plan(&plan, options.max_units);
            if problems.is_empty() {
                return Ok((plan, attempts, log_path));
            }
            on_event(&AgentEvent::Phase(format!("plan rejected: {}", problems.join("; "))));
            if attempts > options.plan_retries {
                bail!(
                    "the planner's plan still cannot run after {attempts} attempt(s):\n{}",
                    problems.iter().map(|p| format!("- {p}")).collect::<Vec<_>>().join("\n")
                );
            }
            prompt = repair_prompt(&serde_json::to_string_pretty(&plan)?, &problems);
        }
    }

    /// Runs one agent turn in `cwd` with no workspace and returns the
    /// assistant's text: the planner's call. Uses the ACP runtime when the
    /// agent has one (one session in a throwaway process), else a
    /// headless process run; either way the events stream to `on_event`
    /// like a lane's would, and every raw update is appended to
    /// `log_path` in the same one-JSON-line shape as a lane's log.
    pub fn ask_agent(
        &self,
        agent: AgentKind,
        cwd: &Path,
        prompt: &str,
        spawn: &SpawnOptions<'_>,
        log_path: &Path,
        on_event: &mut impl FnMut(&AgentEvent),
    ) -> Result<String> {
        let mut text = String::new();
        let mut forward = |event: &AgentEvent| {
            if let AgentEvent::AssistantText(t) = event {
                text.push_str(t);
                text.push('\n');
            }
            on_event(event);
        };
        if let Some(parent) = log_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        match effective_runtime(spawn.runtime, &[agent]) {
            LaneRuntime::Acp => {
                let batch = self.start_acp_batch(&[agent], spawn, &mut forward)?;
                let runtime = batch.runtime(agent).ok_or_else(|| anyhow::anyhow!("no ACP process for {}", agent_kind_name(agent)))?;
                let mut log = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(log_path)
                    .with_context(|| format!("opening log file {}", log_path.display()))?;
                let outcome = (|| -> Result<()> {
                    let mut session = runtime.new_session(cwd, Vec::new()).map_err(|err| anyhow::anyhow!("opening the planner session: {err}"))?;
                    let mut coalescer = crate::acp_runtime::ChunkCoalescer::new();
                    let stop = runtime
                        .prompt(&mut session, prompt, |update| {
                            let line = crate::acp_runtime::log_line(&update);
                            let _ = writeln!(log, "{line}");
                            coalescer.push(&update, &mut forward)
                        })
                        .map_err(|err| anyhow::anyhow!("planner turn: {err}"))?;
                    coalescer.flush(&mut forward);
                    let _ = runtime.close(&session);
                    if !stop.is_success() {
                        bail!("the planner's turn ended with stop reason {}", stop.as_str());
                    }
                    Ok(())
                })();
                batch.shutdown();
                outcome?;
            }
            _ => {
                let adapter = pact_agents::adapter(agent);
                let session_id = uuid::Uuid::new_v4().to_string();
                let agent_home = self.workspaces.state_dir().join("homes").join(format!("planner-{session_id}"));
                let launch = adapter.build_launch(&LaunchRequest {
                    task: prompt,
                    safety_override: pact_agents::resolve_safety_profile(agent, spawn.safety_override).as_deref(),
                    coord: None,
                    workspace_path: cwd,
                    agent_home: &agent_home,
                    session_id: &session_id,
                    lean: spawn.lean,
                });
                let supervisor = Supervisor::new();
                let run = pact_agents::run_and_stream(
                    &supervisor,
                    &launch.program,
                    &launch.args,
                    &launch.env,
                    cwd,
                    log_path,
                    |line| adapter.parse_line(line),
                    &mut forward,
                    |_| {},
                )?;
                if !run.success {
                    bail!("the planner run failed: {}", run.summary);
                }
            }
        }
        Ok(text)
    }
}

/// The planner's bookkeeping handed from `run_task` to `run_plan`.
struct Planning {
    attempts: usize,
    log: Option<PathBuf>,
    elapsed: Duration,
}

/// The shared tree prepared while the planner worked (issue #353).
struct PreparedTree {
    batch: Workspace,
    /// The verification baselines, one per command, when the commands
    /// were known before the plan was.
    baselines: Option<Vec<VerifyOutcome>>,
}

fn append_log_line(path: &Path, line: &serde_json::Value) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("opening log file {}", path.display()))?;
    writeln!(file, "{line}")?;
    Ok(())
}

fn short_slug(text: &str) -> String {
    let slug: String = text
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '-' })
        .collect::<String>()
        .split('-')
        .filter(|s| !s.is_empty())
        .take(5)
        .collect::<Vec<_>>()
        .join("-");
    if slug.is_empty() {
        "task".to_string()
    } else {
        slug
    }
}

/// `cmd /C` on Windows, `sh -c` elsewhere, with the tail of the combined
/// output kept for the report.
fn run_shell_captured(dir: &Path, cmd: &str) -> Result<VerifyOutcome> {
    let mut command = if cfg!(windows) {
        let mut c = Command::new("cmd");
        c.args(["/C", cmd]);
        c
    } else {
        let mut c = Command::new("sh");
        c.args(["-c", cmd]);
        c
    };
    let start = std::time::Instant::now();
    let output = command.current_dir(dir).output().with_context(|| format!("failed to spawn verification command '{cmd}'"))?;
    let combined = format!("{}{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
    let lines: Vec<&str> = combined.lines().collect();
    let tail = lines[lines.len().saturating_sub(40)..].join("\n");
    Ok(VerifyOutcome {
        command: cmd.to_string(),
        success: output.status.success(),
        exit_code: output.status.code(),
        output_tail: tail,
        duration: start.elapsed(),
        baseline_success: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unit(name: &str, files: &[&str]) -> PlanUnit {
        PlanUnit { name: name.into(), files: files.iter().map(|f| f.to_string()).collect(), brief: format!("do {name}"), verify: None }
    }

    fn plan(units: Vec<PlanUnit>) -> Plan {
        Plan { task: "big task".into(), shared_context: "use vitest".into(), units, verify: vec!["npm test".into()] }
    }

    /// Issue #360: `verify` is a list, and old plans with a single string
    /// (or an explicit null) still load.
    #[test]
    fn plan_verify_accepts_a_string_a_list_or_null() {
        let one: Plan = serde_json::from_str(r#"{"units": [], "verify": "npm test"}"#).unwrap();
        assert_eq!(one.verify, vec!["npm test".to_string()]);
        let many: Plan = serde_json::from_str(r#"{"units": [], "verify": ["npm test", "npm run lint"]}"#).unwrap();
        assert_eq!(many.verify, vec!["npm test".to_string(), "npm run lint".to_string()]);
        let null: Plan = serde_json::from_str(r#"{"units": [], "verify": null}"#).unwrap();
        assert!(null.verify.is_empty());
        let absent: Plan = serde_json::from_str(r#"{"units": []}"#).unwrap();
        assert!(absent.verify.is_empty());
        assert!(serde_json::to_string(&many).unwrap().contains(r#""verify":["npm test","npm run lint"]"#));
    }

    #[test]
    fn the_worst_verdict_decides_and_a_real_failure_outranks_an_inconclusive_one() {
        let outcome = |success: bool, baseline: Option<bool>| VerifyOutcome {
            command: "x".into(),
            success,
            exit_code: Some(if success { 0 } else { 1 }),
            output_tail: String::new(),
            duration: Duration::ZERO,
            baseline_success: baseline,
        };
        let report = |verify: Vec<VerifyOutcome>| RunReport {
            plan: plan(vec![]),
            plan_path: PathBuf::new(),
            planner_attempts: 1,
            planner_log: None,
            planning: Duration::ZERO,
            balance_warning: None,
            brief_paths: vec![],
            dry_run: false,
            outcomes: vec![],
            batch: None,
            committed: None,
            repairs: vec![],
            verify,
        };
        assert_eq!(report(vec![]).worst_verdict(), None);
        assert_eq!(report(vec![outcome(true, Some(true)), outcome(true, Some(false))]).worst_verdict(), Some(Verdict::Fixed));
        assert_eq!(report(vec![outcome(true, None), outcome(false, Some(false))]).worst_verdict(), Some(Verdict::Inconclusive));
        assert_eq!(report(vec![outcome(false, Some(false)), outcome(false, Some(true))]).worst_verdict(), Some(Verdict::Regressed));
        assert_eq!(report(vec![outcome(false, None), outcome(false, Some(false))]).worst_verdict(), Some(Verdict::Failed));
    }

    #[test]
    fn a_disjoint_well_formed_plan_validates() {
        let p = plan(vec![unit("a", &["src/a.ts", "src/a.test.ts"]), unit("b", &["src/b.ts"])]);
        assert!(validate_plan(&p, 8).is_empty());
    }

    #[test]
    fn overlapping_ownership_duplicate_names_and_bad_paths_are_each_named() {
        let p = plan(vec![
            unit("a", &["src/shared.ts", "src/a.ts"]),
            unit("A", &["src/shared.ts", "../outside.ts", "C:/abs.ts"]),
            PlanUnit { name: "---".into(), files: vec![], brief: "  ".into(), verify: None },
        ]);
        let problems = validate_plan(&p, 8);
        let joined = problems.join("\n");
        assert!(joined.contains("\"src/shared.ts\" is owned by more than one unit (a, A)"), "{joined}");
        assert!(joined.contains("used more than once"), "{joined}");
        assert!(joined.contains("\"../outside.ts\" must be a repo-relative path"), "{joined}");
        assert!(joined.contains("\"C:/abs.ts\" must be a repo-relative path"), "{joined}");
        assert!(joined.contains("must contain at least one ASCII letter or digit"), "{joined}");
        assert!(joined.contains("owns no files"), "{joined}");
        assert!(joined.contains("has an empty brief"), "{joined}");
    }

    #[test]
    fn too_many_units_and_no_units_are_rejected() {
        assert_eq!(validate_plan(&plan(vec![]), 8), vec!["the plan has no units".to_string()]);
        let p = plan(vec![unit("a", &["a"]), unit("b", &["b"]), unit("c", &["c"])]);
        assert!(validate_plan(&p, 2).iter().any(|m| m.contains("has 3 units; at most 2")));
    }

    #[test]
    fn plan_json_is_taken_from_the_last_fenced_block_or_the_outermost_braces() {
        let reply = "Here is a draft:\n```json\n{\"units\": []}\n```\nActually, final:\n```json\n{\"units\": [{\"name\": \"a\", \"files\": [\"a\"], \"brief\": \"x\"}]}\n```\nDone.";
        let plan = parse_plan(reply, "t").unwrap();
        assert_eq!(plan.units.len(), 1, "the last fenced block wins");
        assert_eq!(plan.task, "t", "a missing task is filled from the request");
        let bare = "plan: {\"task\": \"given\", \"units\": [{\"name\": \"a\", \"files\": [\"a\"], \"brief\": \"x\"}]} end";
        assert_eq!(parse_plan(bare, "t").unwrap().task, "given");
        assert!(parse_plan("no json here", "t").is_err());
        assert!(parse_plan("```json\n{\"units\": \"not a list\"}\n```", "t").unwrap_err().to_string().contains("plan schema"));
    }

    #[test]
    fn briefs_carry_files_brief_shared_context_and_the_rules_workers_kept_breaking() {
        let p = plan(vec![PlanUnit { name: "parser".into(), files: vec!["lib/parse.test.ts".into()], brief: "Cover parse()".into(), verify: Some("npx vitest run lib".into()) }]);
        let brief = render_brief(&p, &p.units[0]);
        for expected in ["# Unit `parser`", "> big task", "- `lib/parse.test.ts`", "Do the task above for the files you own", "Cover parse()", "use vitest", "Do not install packages", "Do not commit", "`npx vitest run lib`", "reply DONE"] {
            assert!(brief.contains(expected), "missing {expected:?} in:\n{brief}");
        }
    }

    /// Issue #361: with project-wide checks on the plan, the brief says
    /// pact runs them and the worker checks only its own files; without
    /// any, the unit's own check is all the brief can ask for.
    #[test]
    fn briefs_hand_the_project_wide_checks_to_pact_and_scope_the_worker_to_its_own_files() {
        let mut p = plan(vec![PlanUnit { name: "u".into(), files: vec!["a.ts".into()], brief: "x".into(), verify: Some("npx vitest run a".into()) }, unit("v", &["b.ts"])]);
        p.verify = vec!["npm test".into(), "npm run typecheck".into(), " ".into()];
        let with_check = render_brief(&p, &p.units[0]);
        assert!(with_check.contains("pact runs the project-wide checks once on the combined result after every unit finishes: `npm test`, `npm run typecheck`."), "{with_check}");
        assert!(with_check.contains("do not run the whole test suite, type-check or lint in any form"), "{with_check}");
        assert!(with_check.contains("Check only your own files, with: `npx vitest run a`"), "{with_check}");
        assert!(!with_check.contains("Check your own work before finishing"), "{with_check}");
        let without_check = render_brief(&p, &p.units[1]);
        assert!(without_check.contains("Check only your own files (run your own test files alone, not the suite)."), "{without_check}");

        p.verify = Vec::new();
        let no_project_checks = render_brief(&p, &p.units[0]);
        assert!(no_project_checks.contains("Check your own work before finishing with: `npx vitest run a`"), "{no_project_checks}");
        assert!(!no_project_checks.contains("pact runs the project-wide checks"), "no promise pact cannot keep:\n{no_project_checks}");
    }

    #[test]
    fn planner_prompt_tells_the_planner_workers_get_the_task_verbatim_and_to_keep_the_reply_short() {
        let text = planner_prompt("do it", 8, &[]);
        for expected in [
            "Every worker receives the complete TASK text above verbatim",
            "Do not restate anything the task already says",
            "Do not summarize a file's contents",
            "A `brief` is two or three sentences",
            "leave it empty when there are none",
            "Keep it short.",
            "repository facts every unit needs that the task text does not state",
            "Project-wide checks are pact's job",
            "put every check the task demands of the whole (the full test suite, type-check, lint, coverage) in the plan's `verify` list",
            "A unit's own `verify` must be cheap and scoped to its files",
        ] {
            assert!(text.contains(expected), "missing {expected:?} in:\n{text}");
        }
        assert!(!text.contains("must be self-contained"), "the old self-contained rule invited the restating:\n{text}");
    }

    #[test]
    fn balance_warning_fires_only_for_a_wide_spread_of_existing_lines() {
        assert!(balance_warning(&[("a".into(), 100), ("b".into(), 90)]).is_none());
        assert!(balance_warning(&[("a".into(), 500), ("b".into(), 100)]).unwrap().contains("\"a\" owns 500"));
        assert!(balance_warning(&[("a".into(), 500), ("b".into(), 0)]).is_none(), "new-file units weigh nothing and are not compared");
    }

    #[test]
    fn repair_prompt_lists_every_problem_and_the_previous_plan() {
        let text = repair_prompt("{\"units\": []}", &["the plan has no units".into(), "x".into()]);
        assert!(text.contains("- the plan has no units\n- x"));
        assert!(text.contains("{\"units\": []}"));
    }

    #[test]
    fn planner_prompt_asks_for_effort_balance_and_names_the_anchors_when_there_are_any() {
        let bare = planner_prompt("do it", 8, &[]);
        assert!(bare.contains("Balance units by EFFORT, not by file count"), "{bare}");
        assert!(bare.contains("Between 1 and 8 units"), "{bare}");
        assert!(!bare.contains("EXISTING TESTS"), "no anchors section without anchors:\n{bare}");
        let with = planner_prompt("do it", 8, &["lib/a.test.ts".into(), "app/b.test.tsx".into()]);
        assert!(with.contains("EXISTING TESTS IN THIS REPOSITORY"), "{with}");
        assert!(with.contains("- lib/a.test.ts\n- app/b.test.tsx"), "{with}");
        assert!(with.contains("the existing file to imitate"), "{with}");
    }

    #[test]
    fn test_anchors_come_from_tracked_files_shortest_first_and_capped() {
        let repo = std::env::temp_dir().join(format!("pact-run-anchors-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(repo.join("lib")).unwrap();
        std::fs::create_dir_all(repo.join("app/(app)/hub")).unwrap();
        std::fs::create_dir_all(repo.join("tests")).unwrap();
        let git = |args: &[&str]| {
            let out = Command::new("git").args(args).current_dir(&repo).output().unwrap();
            assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        };
        git(&["init", "-q"]);
        git(&["config", "user.email", "t@t"]);
        git(&["config", "user.name", "t"]);
        for file in ["lib/a.ts", "lib/a.test.ts", "app/(app)/hub/skeleton.test.tsx", "tests/helpers.ts", "lib/b.spec.ts", "notes.md"] {
            std::fs::write(repo.join(file), "x").unwrap();
        }
        std::fs::write(repo.join("untracked.test.ts"), "x").unwrap();
        git(&["add", "lib", "app", "tests", "notes.md"]);
        git(&["commit", "-q", "-m", "init"]);

        let anchors = discover_test_anchors(&repo, 10);
        assert_eq!(anchors, vec!["lib/a.test.ts", "lib/b.spec.ts", "tests/helpers.ts", "app/(app)/hub/skeleton.test.tsx"], "tracked test files, shortest first; untracked and non-test files excluded");
        assert_eq!(discover_test_anchors(&repo, 2).len(), 2, "capped");
        assert!(discover_test_anchors(Path::new("/definitely/not/a/repo"), 5).is_empty());
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn short_slug_keeps_five_words() {
        assert_eq!(short_slug("Add Vitest tests for every file in lib and app"), "add-vitest-tests-for-every");
        assert_eq!(short_slug("!!!"), "task");
    }

    #[test]
    fn the_verdict_reads_the_result_against_the_baseline() {
        let outcome = |success: bool, baseline: Option<bool>| VerifyOutcome {
            command: "x".into(),
            success,
            exit_code: Some(if success { 0 } else { 1 }),
            output_tail: String::new(),
            duration: Duration::ZERO,
            baseline_success: baseline,
        };
        assert_eq!(outcome(true, Some(true)).verdict(), Verdict::Passed);
        assert_eq!(outcome(true, None).verdict(), Verdict::Passed);
        assert_eq!(outcome(true, Some(false)).verdict(), Verdict::Fixed);
        assert_eq!(outcome(false, Some(true)).verdict(), Verdict::Regressed);
        assert_eq!(outcome(false, Some(false)).verdict(), Verdict::Inconclusive);
        assert_eq!(outcome(false, None).verdict(), Verdict::Failed);
        assert!(Verdict::Inconclusive.to_string().contains("cannot judge"));
    }
}
