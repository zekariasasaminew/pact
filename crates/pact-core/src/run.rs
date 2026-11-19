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
//! batch worktree and reports. Units that depend on other units run in
//! later waves, each committed before the next starts (issue #462).

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use pact_agents::{AgentEvent, AgentKind, LaunchRequest, Supervisor};
use pact_vcs::Workspace;
use serde::{Deserialize, Serialize};

use crate::ownership::{check_ownership, edits_in_log, tree_hash, FinishSnapshots, OwnershipViolation};
use crate::verify_output::{failing_files, FailingFile};
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
    /// Names of the units whose work this one reads (issue #462): it runs
    /// in a later wave, after they are committed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub depends_on: Vec<String>,
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
    let mut seen_slugs: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    let mut owners: std::collections::HashMap<String, Vec<String>> = std::collections::HashMap::new();
    for unit in &plan.units {
        let name = unit.name.trim();
        if let Some(problem) = unit_name_problem(&unit.name) {
            problems.push(problem);
        } else {
            let slug = pact_vcs::name_slug(name);
            match seen_slugs.get(&slug) {
                Some(first) if first == name => problems.push(format!("unit name {name:?} is used more than once")),
                Some(first) => problems.push(format!("unit names {first:?} and {name:?} both become workspace {slug:?}; give them names that differ in letters or digits")),
                None => {
                    seen_slugs.insert(slug, name.to_string());
                }
            }
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
    problems.extend(dependency_problems(plan));
    problems
}

fn unit_index(plan: &Plan, name: &str) -> Option<usize> {
    plan.units.iter().position(|unit| unit.name.trim() == name.trim())
}

/// Each unit's distinct dependencies as unit indices, itself and unknown
/// names left out.
fn dependency_indices(plan: &Plan) -> Vec<Vec<usize>> {
    plan.units
        .iter()
        .enumerate()
        .map(|(index, unit)| {
            let mut deps: Vec<usize> = Vec::new();
            for dep in unit.depends_on.iter().filter_map(|name| unit_index(plan, name)) {
                if dep != index && !deps.contains(&dep) {
                    deps.push(dep);
                }
            }
            deps
        })
        .collect()
}

/// Unknown names, self-dependencies and cycles in `depends_on` (issue #462).
fn dependency_problems(plan: &Plan) -> Vec<String> {
    let mut problems = Vec::new();
    for unit in &plan.units {
        let name = unit.name.trim();
        let mut named: Vec<&str> = Vec::new();
        for dep in unit.depends_on.iter().map(|dep| dep.trim()) {
            if named.contains(&dep) {
                continue;
            }
            named.push(dep);
            if dep == name {
                problems.push(format!("unit {name:?} depends on itself; remove {dep:?} from its `depends_on`"));
            } else if unit_index(plan, dep).is_none() {
                problems.push(format!("unit {name:?} depends on {dep:?}, which is not a unit in this plan; use the exact name of another unit"));
            }
        }
    }
    let edges = dependency_indices(plan);
    let mut state = vec![0u8; edges.len()];
    let mut path = Vec::new();
    let mut cycles: Vec<Vec<usize>> = Vec::new();
    for start in 0..edges.len() {
        find_cycles(start, &edges, &mut state, &mut path, &mut cycles);
    }
    for cycle in cycles {
        let steps: Vec<String> = cycle
            .iter()
            .enumerate()
            .map(|(position, &unit)| format!("{:?} depends on {:?}", plan.units[unit].name.trim(), plan.units[cycle[(position + 1) % cycle.len()]].name.trim()))
            .collect();
        problems.push(format!("units depend on each other in a cycle ({}); remove one of those dependencies", steps.join(", ")));
    }
    problems
}

/// Depth-first search that records each cycle once, as the path from the
/// unit it returns to. `state`: 0 unvisited, 1 on the current path, 2 done.
fn find_cycles(unit: usize, edges: &[Vec<usize>], state: &mut [u8], path: &mut Vec<usize>, cycles: &mut Vec<Vec<usize>>) {
    if state[unit] != 0 {
        return;
    }
    state[unit] = 1;
    path.push(unit);
    for &next in &edges[unit] {
        match state[next] {
            0 => find_cycles(next, edges, state, path, cycles),
            1 => {
                let start = path.iter().position(|&u| u == next).expect("on the path");
                cycles.push(path[start..].to_vec());
            }
            _ => {}
        }
    }
    path.pop();
    state[unit] = 2;
}

/// Each unit's wave, in plan order (issue #462): 1 for a unit with no
/// dependencies, else one more than the latest wave among them. Cycles
/// are `validate_plan`'s to reject; here they only stop the relaxation.
pub fn plan_waves(plan: &Plan) -> Vec<usize> {
    let deps = dependency_indices(plan);
    let mut waves = vec![1; plan.units.len()];
    for _ in 0..plan.units.len() {
        let mut changed = false;
        for (index, unit_deps) in deps.iter().enumerate() {
            let wave = 1 + unit_deps.iter().map(|&dep| waves[dep]).max().unwrap_or(0);
            if wave != waves[index] {
                waves[index] = wave;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    waves
}

/// The units' indices grouped by wave, first wave first, plan order within one.
pub fn wave_groups(plan: &Plan) -> Vec<Vec<usize>> {
    let waves = plan_waves(plan);
    let last = waves.iter().copied().max().unwrap_or(0);
    (1..=last)
        .map(|wave| (0..waves.len()).filter(|&index| waves[index] == wave).collect::<Vec<_>>())
        .filter(|group| !group.is_empty())
        .collect()
}

/// The longest unit name a plan may use (issue #414).
pub const MAX_UNIT_NAME_LEN: usize = 48;

/// Why `name` cannot be a unit name, if it cannot: it becomes a workspace
/// id and a file name, so it must be `[A-Za-z0-9][A-Za-z0-9._-]*` and
/// short (issue #414).
fn unit_name_problem(name: &str) -> Option<String> {
    let mut chars = name.chars();
    let well_formed = chars.next().is_some_and(|c| c.is_ascii_alphanumeric()) && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    if !well_formed {
        return Some(format!(
            "unit name {name:?} must start with an ASCII letter or digit and contain only letters, digits, '.', '_' and '-' (short-kebab-case)"
        ));
    }
    if name.len() > MAX_UNIT_NAME_LEN {
        return Some(format!("unit name {name:?} is {} characters; at most {MAX_UNIT_NAME_LEN} are allowed", name.len()));
    }
    None
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
      "verify": "optional cheap shell command scoped to this unit's own files (its test files alone); never the whole suite, type-check or lint",
      "depends_on": ["optional: names of the units whose new files this unit must read; leave it out for an independent unit"]
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
         - Prefer independent units. Use `depends_on` only when a unit must read files another unit creates \
         (it imports a new module, or tests code another unit writes). A unit with `depends_on` runs in a later \
         wave, after the units it names are done, and waves run one after another, so every dependency costs \
         wall-clock time.\n\
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

/// What a retry sends: the repair alone to a planner that still has the
/// conversation, the whole first prompt before it to one that does not,
/// so it still sees the task, the rules and the test anchors (issue #413).
fn retry_prompt(first_prompt: &str, keeps_context: bool, previous_json: &str, problems: &[String]) -> String {
    let repair = repair_prompt(previous_json, problems);
    if keeps_context {
        repair
    } else {
        format!("{first_prompt}\n\n{repair}")
    }
}

/// The brief section naming the units a dependent unit runs after and
/// their files (issue #462); empty for an independent unit.
fn dependency_section(plan: &Plan, unit: &PlanUnit) -> String {
    let mut deps: Vec<&PlanUnit> = Vec::new();
    for dep in unit.depends_on.iter().filter_map(|name| unit_index(plan, name)).map(|index| &plan.units[index]) {
        if dep.name.trim() != unit.name.trim() && !deps.iter().any(|d| d.name == dep.name) {
            deps.push(dep);
        }
    }
    if deps.is_empty() {
        return String::new();
    }
    let mut section = "\n## Work done before yours\n\nThese units ran before yours. Their files are already written and committed in this checkout: read them as you need, but do not edit them.\n".to_string();
    for dep in deps {
        section.push_str(&format!("- unit `{}`: {}\n", dep.name.trim(), dep.files.iter().map(|f| format!("`{f}`")).collect::<Vec<_>>().join(", ")));
    }
    section
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
    brief.push_str(&dependency_section(plan, unit));
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
    // Issue #461: workers check their work, but only through run_check, so
    // a machine-wide slot pool spaces the checks of every lane out.
    const RUN_CHECK: &str = "only through the `run_check` tool of the `pact-coord` MCP server (your CLI namespaces it, \
         e.g. `pact-coord-run_check`), never directly in your shell: it queues your check behind other agents' checks on this machine";
    if project_checks.is_empty() {
        if let Some(verify) = unit_check {
            brief.push_str(&format!("- Check your own work before finishing with `{verify}`, run {RUN_CHECK}.\n"));
        }
    } else {
        brief.push_str(&format!(
            "- Run every check (tests, type-check, lint) {RUN_CHECK}, so they do not all run at once. Before finishing, \
             type-check and lint the files you own and run your own tests through it"
        ));
        if let Some(verify) = unit_check {
            brief.push_str(&format!(", including your unit's check: `{verify}`"));
        }
        brief.push_str(&format!(
            ". pact also runs {} once on the combined result after every unit finishes.\n",
            project_checks.iter().map(|c| format!("`{c}`")).collect::<Vec<_>>().join(", ")
        ));
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
    /// Uncommitted changes in the checkout that `--allow-dirty` left out
    /// of the lanes' tree (issue #411).
    pub dirty_paths: Vec<String>,
    /// Files the units' lanes wrote outside their own (issue #409); the
    /// run fails when there are any. Nothing is reverted.
    pub ownership: Vec<OwnershipViolation>,
    /// Units never launched because a unit they depend on failed (issue
    /// #462); the run fails when there are any.
    pub skipped: Vec<SkippedUnit>,
    /// Lines added and removed per file, base commit to the batch branch
    /// (issue #463); empty for a dry run or when no tree was created.
    pub diff_stat: Vec<pact_vcs::FileStat>,
}

pub struct SkippedUnit {
    pub unit: String,
    /// Why, in report words: `depends on <failed unit>`.
    pub reason: String,
}

pub struct RepairOutcome {
    pub attempt: usize,
    /// The unit whose failing files this lane repaired; `None` for the
    /// lane that took the failures no unit owns (issue #372).
    pub unit: Option<String>,
    pub outcome: SpawnManyOutcome,
    /// Whether this attempt's round, all its lanes together, committed.
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
        lanes_ok && self.skipped.is_empty() && self.ownership.is_empty() && self.verify.iter().all(|v| v.success)
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
    /// lane ran. `None` when no baseline was taken, or the baseline
    /// timed out.
    pub baseline_success: Option<bool>,
    /// Killed at `--verify-timeout` (issue #410).
    pub timed_out: bool,
    /// The command's whole output, both streams in write order.
    pub log: PathBuf,
    /// The tree's files a failing command's output names (issue #373);
    /// empty when it passed.
    pub failing_files: Vec<FailingFile>,
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
    outcomes.iter().any(needs_repair)
}

/// One repair lane's share of a failed verification (issue #372).
#[derive(Debug, Clone, PartialEq, Eq)]
struct RepairAssignment {
    /// `None` for the lane taking failures no unit owns.
    unit: Option<String>,
    /// The files the lane may edit, disjoint from every other lane's.
    allowed: Vec<String>,
    checks: Vec<RepairCheck>,
}

/// A failing check as one lane sees it: only the files routed to it, or
/// the output's last lines when it names no file.
#[derive(Debug, Clone, PartialEq, Eq)]
struct RepairCheck {
    command: String,
    log: PathBuf,
    timed_out: bool,
    files: Vec<FailingFile>,
    tail: Option<String>,
}

fn needs_repair(outcome: &VerifyOutcome) -> bool {
    matches!(outcome.verdict(), Verdict::Regressed | Verdict::Failed)
}

/// Splits the real failures between repair lanes: each failing file goes
/// to the unit that owns it, and that unit's lane may edit only its own
/// files. Files no unit owns, and checks whose output names no file or
/// that timed out, go to one extra lane that may edit the run's touched
/// files no unit lane holds this round. When that leaves the extra lane
/// nothing to edit, its checks are context for every unit lane instead.
fn route_repairs(plan: &Plan, outcomes: &[VerifyOutcome], touched_files: &[String]) -> Vec<RepairAssignment> {
    let normalize = |path: &str| path.replace('\\', "/").trim_start_matches("./").to_string();
    let owner = |path: &str| plan.units.iter().find(|unit| unit.files.iter().any(|file| normalize(file) == path)).map(|unit| unit.name.trim().to_string());
    let check_for = |checks: &mut Vec<RepairCheck>, outcome: &VerifyOutcome| -> usize {
        match checks.iter().position(|check| check.command == outcome.command) {
            Some(index) => index,
            None => {
                checks.push(RepairCheck { command: outcome.command.clone(), log: outcome.log.clone(), timed_out: outcome.timed_out, files: Vec::new(), tail: None });
                checks.len() - 1
            }
        }
    };

    let mut by_unit: Vec<(String, Vec<RepairCheck>)> = Vec::new();
    let mut unowned: Vec<RepairCheck> = Vec::new();
    for outcome in outcomes.iter().filter(|outcome| needs_repair(outcome)) {
        if outcome.timed_out || outcome.failing_files.is_empty() {
            let index = check_for(&mut unowned, outcome);
            unowned[index].tail = Some(outcome.output_tail.clone());
            continue;
        }
        for file in &outcome.failing_files {
            let checks = match owner(&file.path) {
                Some(unit) => match by_unit.iter().position(|(name, _)| *name == unit) {
                    Some(index) => &mut by_unit[index].1,
                    None => {
                        by_unit.push((unit, Vec::new()));
                        &mut by_unit.last_mut().expect("just pushed").1
                    }
                },
                None => &mut unowned,
            };
            let index = check_for(checks, outcome);
            checks[index].files.push(file.clone());
        }
    }
    by_unit.sort_by_key(|(name, _)| plan.units.iter().position(|unit| unit.name.trim() == name));

    let mut assignments: Vec<RepairAssignment> = by_unit
        .into_iter()
        .map(|(name, checks)| {
            let allowed = plan.units.iter().find(|unit| unit.name.trim() == name).map(|unit| unit.files.iter().map(|f| normalize(f)).collect()).unwrap_or_default();
            RepairAssignment { unit: Some(name), allowed, checks }
        })
        .collect();
    if !unowned.is_empty() {
        let held: Vec<&String> = assignments.iter().flat_map(|assignment| &assignment.allowed).collect();
        let allowed: Vec<String> = touched_files.iter().map(|f| normalize(f)).filter(|file| !held.contains(&file)).collect();
        if allowed.is_empty() && !assignments.is_empty() {
            for assignment in &mut assignments {
                assignment.checks.extend(unowned.iter().cloned());
            }
        } else {
            assignments.push(RepairAssignment { unit: None, allowed, checks: unowned });
        }
    }
    assignments
}

/// Repair lane names are workspace names: never a unit's, never each other's.
fn repair_lane_names(attempt: usize, plan: &Plan, assignments: &[RepairAssignment]) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for assignment in assignments {
        let base = match &assignment.unit {
            Some(unit) => format!("repair-{attempt}-{unit}"),
            None => format!("repair-{attempt}"),
        };
        let taken = |name: &str| plan.units.iter().any(|unit| unit.name.trim().eq_ignore_ascii_case(name)) || names.iter().any(|n| n.eq_ignore_ascii_case(name));
        let mut name = base.clone();
        let mut suffix = 2;
        while taken(&name) {
            name = format!("{base}-{suffix}");
            suffix += 1;
        }
        names.push(name);
    }
    names
}

fn render_repair_brief(attempt: usize, total: usize, assignment: &RepairAssignment, touched_files: &[String], parallel_lanes: usize) -> String {
    let mut brief = match &assignment.unit {
        Some(unit) => format!("# Repair attempt {attempt}/{total}: unit `{unit}`\n\nMake the combined verification checks pass for the files of unit `{unit}` without changing application behaviour. "),
        None => format!("# Repair attempt {attempt}/{total}\n\nMake the combined verification checks pass without changing application behaviour. "),
    };
    brief.push_str("Touch only the files listed below. Do not edit any other file.");
    if parallel_lanes > 1 {
        brief.push_str(&format!(" {} other repair lane(s) are fixing other files in this tree at the same time.", parallel_lanes - 1));
    }
    brief.push_str("\n\n## Files you may touch\n");
    for file in &assignment.allowed {
        brief.push_str(&format!("- `{file}`\n"));
    }
    brief.push_str("\n## Failing checks\n");
    for check in &assignment.checks {
        brief.push_str(&format!("\n### `{}`\n\nFull output: `{}`\n", check.command, check.log.display()));
        if check.timed_out {
            brief.push_str("\nIt did not finish: pact killed it at the verification timeout. Look for a test that hangs or waits on input.\n");
        }
        if let Some(tail) = &check.tail {
            brief.push_str(&format!("\nThe output's last lines:\n\n```text\n{tail}\n```\n"));
        }
        for file in &check.files {
            let note = if touched_files.contains(&file.path) { "" } else { " (not changed by this run: fix it through the files you may touch)" };
            brief.push_str(&format!("\n#### `{}`{note}\n\n```text\n{}\n```\n", file.path, file.excerpt));
        }
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
    /// How long one verification command may run before its process tree
    /// is killed and the command counts as timed out (issue #410).
    pub verify_timeout: Duration,
    /// Run even though the checkout has uncommitted changes the lanes'
    /// tree will not contain (issue #411).
    pub allow_dirty: bool,
    pub spawn: SpawnOptions<'a>,
}

/// The CLI default for `RunOptions::verify_timeout`.
pub const DEFAULT_VERIFY_TIMEOUT: Duration = Duration::from_secs(20 * 60);

/// Where one run's verification logs go and how long each command may take.
struct VerifyContext {
    dir: PathBuf,
    timeout: Duration,
}

impl VerifyContext {
    fn log_path(&self, index: usize, command: &str, phase: &str) -> PathBuf {
        self.dir.join(format!("{}-{}-{phase}.log", index + 1, short_slug(command)))
    }
}

/// `90s`, `20m`, `1h5m`: how a timeout reads in the report.
pub fn format_duration(duration: Duration) -> String {
    let secs = duration.as_secs();
    let (h, m, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    let mut text = String::new();
    if h > 0 {
        text.push_str(&format!("{h}h"));
    }
    if m > 0 {
        text.push_str(&format!("{m}m"));
    }
    if s > 0 || text.is_empty() {
        text.push_str(&format!("{s}s"));
    }
    text
}

const PLANNER_LABEL: usize = usize::MAX;

/// The run's diff stat for its report; a failure to compute it never
/// fails the run.
fn run_diff_stat(batch: &Workspace) -> Vec<pact_vcs::FileStat> {
    pact_vcs::diff_numstat(&batch.path, &batch.base_commit, Some(&batch.branch)).unwrap_or_else(|err| {
        tracing::warn!("computing the diff stat of {}: {err:#}", batch.id);
        Vec::new()
    })
}

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
        let dirty = pact_vcs::changed_paths(&self.repo_root).context("checking your checkout for uncommitted changes")?;
        if !dirty.is_empty() {
            if !options.allow_dirty {
                bail!(
                    "{} uncommitted change(s) in your checkout: {}. The lanes start from HEAD and would not see them, while the planner would. Commit or stash them first, or pass --allow-dirty to run from HEAD anyway",
                    dirty.len(),
                    list_paths(&dirty)
                );
            }
            planner_events(&AgentEvent::Phase(format!(
                "--allow-dirty: running from HEAD without {} uncommitted change(s) in your checkout: {}",
                dirty.len(),
                list_paths(&dirty)
            )));
        }
        let spawn_options = SpawnOptions { shared_tree: true, ..options.spawn };
        let run_id = format!("{}-{}", unix_now(), &uuid::Uuid::new_v4().simple().to_string()[..8]);
        let verify_context = VerifyContext {
            dir: self.workspaces.state_dir().join("meta").join("verify").join(run_id),
            timeout: options.verify_timeout,
        };
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
                let (plan, attempts, log) = self.plan_task(task, options, &planner_events)?;
                (plan, Planning { attempts, log: Some(log), elapsed: planning_started.elapsed() }, None)
            }
            None => {
                let (planned, prepared) = std::thread::scope(|scope| {
                    let prep = scope.spawn(|| self.prepare_run_tree(task, &spawn_options, options.verify, &verify_context, &planner_events));
                    let planned = self.plan_task(task, options, &planner_events);
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
        let mut report = self.run_plan(plan, planning, prepared, &spawn_options, options, &verify_context, on_event)?;
        report.dirty_paths = dirty;
        Ok(report)
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
        verify_context: &VerifyContext,
        on_event: &(impl Fn(&AgentEvent) + Sync),
    ) -> Result<PreparedTree> {
        let summary = format!("pact run: {}", task.lines().find(|l| !l.trim().is_empty()).unwrap_or("").trim().chars().take(80).collect::<String>());
        on_event(&AgentEvent::Phase("creating the shared tree while the planner works".to_string()));
        let mut forward = |event: &AgentEvent| on_event(event);
        let batch = self
            .create_shared_batch_workspace_named(&summary, spawn_options, &mut forward)
            .context("creating the shared tree for the plan")?;
        let baselines = if verify.is_empty() { None } else { Some(self.run_baselines(&batch, verify, verify_context, on_event)?) };
        Ok(PreparedTree { batch, baselines })
    }

    /// One baseline per verification command, on the untouched tree.
    fn run_baselines(&self, batch: &Workspace, commands: &[String], verify_context: &VerifyContext, on_event: &impl Fn(&AgentEvent)) -> Result<Vec<VerifyOutcome>> {
        commands
            .iter()
            .enumerate()
            .map(|(index, command)| {
                on_event(&AgentEvent::Phase(format!("verification baseline on the untouched tree: {command}")));
                let outcome = run_shell_captured(&batch.path, command, &verify_context.log_path(index, command, "baseline"), verify_context.timeout)?;
                let verdict = if outcome.timed_out {
                    format!("TIMED OUT after {} (killed), so there is no baseline for it", format_duration(verify_context.timeout))
                } else if outcome.success {
                    "passes".to_string()
                } else {
                    "already FAILS before any lane runs".to_string()
                };
                on_event(&AgentEvent::Phase(format!("baseline `{command}` {verdict} in {:.1}s", outcome.duration.as_secs_f32())));
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

    #[allow(clippy::too_many_arguments)]
    fn run_plan(
        &self,
        plan: Plan,
        planning: Planning,
        prepared: Option<PreparedTree>,
        spawn_options: &SpawnOptions<'_>,
        options: &RunOptions<'_>,
        verify_context: &VerifyContext,
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
            let path = briefs_dir.join(format!("{}.md", pact_vcs::name_slug(&unit.name)));
            std::fs::write(&path, &brief).with_context(|| format!("writing {}", path.display()))?;
            brief_paths.push(path);
            tasks.push(SpawnManyTask { agent: options.agent, task: brief, name: Some(unit.name.clone()), model: None });
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
                dirty_paths: Vec::new(),
                ownership: Vec::new(),
                skipped: Vec::new(),
                diff_stat: Vec::new(),
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
            None => self.run_baselines(&batch, &verify_commands, verify_context, &planner_events)?,
        };
        let waves = wave_groups(&plan);
        let mut pending: Vec<Option<SpawnManyTask>> = tasks.into_iter().map(Some).collect();
        // Per unit, the failed unit that stops it: its own name when its
        // lane failed, the inherited cause when it was skipped.
        let mut blocked: Vec<Option<String>> = vec![None; plan.units.len()];
        let mut outcomes = Vec::new();
        let mut skipped = Vec::new();
        let mut committed = None;
        let mut ownership = Vec::new();
        let mut batch = Some(batch);
        for (wave_index, wave) in waves.iter().enumerate() {
            let Some(tree) = batch.clone() else { break };
            let mut launch: Vec<usize> = Vec::new();
            for &index in wave {
                let unit = &plan.units[index];
                match unit.depends_on.iter().find_map(|dep| unit_index(&plan, dep).and_then(|dep| blocked[dep].clone())) {
                    Some(cause) => {
                        planner_events(&AgentEvent::Phase(format!("skipping unit {}: it depends on {cause}, which failed", unit.name.trim())));
                        skipped.push(SkippedUnit { unit: unit.name.trim().to_string(), reason: format!("depends on {cause}") });
                        pending[index] = None;
                        blocked[index] = Some(cause);
                    }
                    None => launch.push(index),
                }
            }
            if launch.is_empty() {
                continue;
            }
            let wave_label = if waves.len() > 1 { format!("wave {}/{} ", wave_index + 1, waves.len()) } else { String::new() };
            if waves.len() > 1 {
                let names: Vec<&str> = launch.iter().map(|&index| plan.units[index].name.trim()).collect();
                planner_events(&AgentEvent::Phase(format!("{wave_label}starting: {}", names.join(", "))));
            }
            let base = if wave_index == 0 { tree.base_commit.clone() } else { head_commit(&tree.path)? };
            let snapshots = FinishSnapshots::default();
            let lane_events = |local: usize, agent: &AgentKind, event: &AgentEvent| {
                let index = launch.get(local).copied().unwrap_or(local);
                if let (AgentEvent::Result { .. }, Some(unit)) = (event, plan.units.get(index)) {
                    snapshots.record(index, &unit.files, &tree.path);
                }
                on_event(index, agent, event)
            };
            let wave_tasks: Vec<SpawnManyTask> = launch.iter().map(|&index| pending[index].take().expect("each unit launches once")).collect();
            let mut wave_outcomes = self.spawn_many_in(wave_tasks, spawn_options, Some(tree.clone()), lane_events);
            for outcome in &mut wave_outcomes {
                outcome.index = launch[outcome.index];
                if !matches!(&outcome.result, Ok((_, run)) if run.success) {
                    blocked[outcome.index] = Some(plan.units[outcome.index].name.trim().to_string());
                }
            }
            batch = self.workspaces.get_workspace(&tree.id).ok();
            if let Some(tree) = &batch {
                let changed = pact_vcs::changed_since(&tree.path, &base).context("listing the files the lanes changed")?;
                let logs = self.workspaces.state_dir().join("logs");
                let lane_edits: Vec<(usize, Vec<String>)> = wave_outcomes
                    .iter()
                    .filter_map(|outcome| outcome.result.as_ref().ok().map(|(lane, _)| (outcome.index, edits_in_log(&logs.join(format!("{}.jsonl", lane.id)), &tree.path))))
                    .collect();
                let found = check_ownership(&plan, &launch, &changed, &lane_edits, snapshots, tree_hash(&tree.path));
                for violation in &found {
                    planner_events(&AgentEvent::Phase(format!("ownership: {violation}")));
                }
                ownership.extend(found);
                planner_events(&AgentEvent::Phase(format!("committing {wave_label}the shared tree {}", tree.id)));
                let wave_committed = self.workspaces.commit_all(&tree.id).context("committing the batch")?;
                committed = Some(committed.unwrap_or(false) || wave_committed);
            }
            outcomes.extend(wave_outcomes);
        }
        for (index, task) in pending.iter().enumerate() {
            if task.is_some() {
                skipped.push(SkippedUnit { unit: plan.units[index].name.trim().to_string(), reason: "the shared tree was gone before its wave".to_string() });
            }
        }

        let mut repairs = Vec::new();
        let mut verify = Vec::new();
        if let Some(batch) = &batch {
            verify = self.verify_run(batch, &verify_commands, &baselines, verify_context, "after", &planner_events)?;

            let touched_files = self.workspaces.workspace_changes(&batch.id).context("finding files touched by the run")?.files;
            let mut next_lane_index = plan.units.len();
            for attempt in 1..=options.repair_attempts {
                if !verification_needs_repair(&verify) {
                    break;
                }
                let assignments = route_repairs(&plan, &verify, &touched_files);
                let names = repair_lane_names(attempt, &plan, &assignments);
                let lanes: Vec<String> = assignments.iter().map(|a| a.unit.clone().unwrap_or_else(|| "files no unit owns".to_string())).collect();
                planner_events(&AgentEvent::Phase(format!(
                    "repair attempt {attempt}/{} for failed combined verification: {} lane{} in parallel ({})",
                    options.repair_attempts,
                    assignments.len(),
                    if assignments.len() == 1 { "" } else { "s" },
                    lanes.join(", ")
                )));
                let tasks: Vec<SpawnManyTask> = assignments
                    .iter()
                    .zip(&names)
                    .map(|(assignment, name)| SpawnManyTask {
                        agent: options.agent,
                        task: render_repair_brief(attempt, options.repair_attempts, assignment, &touched_files, assignments.len()),
                        name: Some(name.clone()),
                        model: None,
                    })
                    .collect();
                let base_index = next_lane_index;
                next_lane_index += tasks.len();
                let round = self.spawn_many_in(tasks, spawn_options, Some(batch.clone()), |index, agent, event| on_event(base_index + index, agent, event));
                let any_succeeded = round.iter().any(|outcome| matches!(&outcome.result, Ok((_, run)) if run.success));
                let round_committed = if any_succeeded {
                    planner_events(&AgentEvent::Phase(format!("committing repair attempt {attempt}")));
                    let round_committed = self.workspaces.commit_all(&batch.id).context("committing the repair")?;
                    committed = Some(committed.unwrap_or(false) || round_committed);
                    verify = self.verify_run(batch, &verify_commands, &baselines, verify_context, &format!("repair-{attempt}"), &planner_events)?;
                    round_committed
                } else {
                    false
                };
                for (assignment, outcome) in assignments.into_iter().zip(round) {
                    repairs.push(RepairOutcome { attempt, unit: assignment.unit, outcome, committed: round_committed });
                }
                if !any_succeeded {
                    break;
                }
            }
        }

        let diff_stat = batch.as_ref().map(run_diff_stat).unwrap_or_default();
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
            dirty_paths: Vec::new(),
            ownership,
            skipped,
            diff_stat,
        })
    }

    fn verify_run(
        &self,
        batch: &Workspace,
        commands: &[String],
        baselines: &[VerifyOutcome],
        verify_context: &VerifyContext,
        phase: &str,
        on_event: &impl Fn(&AgentEvent),
    ) -> Result<Vec<VerifyOutcome>> {
        commands
            .iter()
            .enumerate()
            .map(|(index, command)| {
                on_event(&AgentEvent::Phase(format!("verifying in the batch worktree: {command}")));
                let mut outcome = run_shell_captured(&batch.path, command, &verify_context.log_path(index, command, phase), verify_context.timeout)?;
                outcome.baseline_success = baselines.get(index).filter(|baseline| !baseline.timed_out).map(|baseline| baseline.success);
                let timed_out = if outcome.timed_out { format!("TIMED OUT after {} (killed): ", format_duration(verify_context.timeout)) } else { String::new() };
                on_event(&AgentEvent::Phase(format!(
                    "verification `{command}` {timed_out}{} in {:.1}s",
                    outcome.verdict(),
                    outcome.duration.as_secs_f32()
                )));
                Ok(outcome)
            })
            .collect()
    }

    /// Asks the planner for a plan, validates it, and sends violations
    /// back up to `plan_retries` times, in the same conversation when the
    /// runtime keeps one and with the whole planner prompt again when it
    /// does not (issue #413). Refuses a planner that modified the
    /// repository: planning is read-only by contract. Every attempt
    /// appends to one `logs/planner-<stamp>.jsonl`, each attempt opened
    /// by a `{"pact": {"planner_attempt": n}}` line (issue #348).
    fn plan_task(
        &self,
        task: &str,
        options: &RunOptions<'_>,
        on_event: &impl Fn(&AgentEvent),
    ) -> Result<(Plan, usize, PathBuf)> {
        let before = pact_vcs::changed_paths(&self.repo_root).unwrap_or_default();
        let anchors = discover_test_anchors(&self.repo_root, 6);
        if !anchors.is_empty() {
            on_event(&AgentEvent::Phase(format!("handing the planner {} existing test file(s) to imitate", anchors.len())));
        }
        let log_path = self.workspaces.state_dir().join("logs").join(format!("planner-{}.jsonl", unix_now()));
        let first_prompt = planner_prompt(task, options.max_units, &anchors);
        let (plan, attempts) = self.converse(options.agent, &self.repo_root, &options.spawn, &log_path, on_event, |ask, keeps_context| {
            let mut prompt = first_prompt.clone();
            let mut attempts = 0;
            loop {
                attempts += 1;
                on_event(&AgentEvent::Phase(format!("planning (attempt {attempts})")));
                append_log_line(&log_path, &serde_json::json!({ "t": crate::acp_runtime::unix_millis(), "pact": { "planner_attempt": attempts, "prompt_chars": prompt.len() } }))?;
                let reply = ask(&prompt)?;
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
                        prompt = retry_prompt(&first_prompt, keeps_context, &extract_plan_json(&reply).unwrap_or_else(|| reply.clone()), &[format!("{err:#}")]);
                        continue;
                    }
                    Err(err) => return Err(err.context(format!("the planner produced no usable plan in {attempts} attempt(s)"))),
                };
                let problems = validate_plan(&plan, options.max_units);
                if problems.is_empty() {
                    return Ok((plan, attempts));
                }
                on_event(&AgentEvent::Phase(format!("plan rejected: {}", problems.join("; "))));
                if attempts > options.plan_retries {
                    bail!(
                        "the planner's plan still cannot run after {attempts} attempt(s):\n{}",
                        problems.iter().map(|p| format!("- {p}")).collect::<Vec<_>>().join("\n")
                    );
                }
                prompt = retry_prompt(&first_prompt, keeps_context, &serde_json::to_string_pretty(&plan)?, &problems);
            }
        })?;
        Ok((plan, attempts, log_path))
    }

    /// Holds one planner conversation in `cwd` with no workspace and lets
    /// `drive` ask it any number of turns, returning the assistant's text
    /// for each. Under ACP every turn goes to one session in one throwaway
    /// process, so a retry still has the task and everything the planner
    /// read (issue #413); otherwise each turn is a fresh headless process
    /// that remembers nothing. `drive` is told which. Events stream to
    /// `on_event` like a lane's, and every raw update is appended to
    /// `log_path` in the same one-JSON-line shape as a lane's log.
    fn converse<T>(
        &self,
        agent: AgentKind,
        cwd: &Path,
        spawn: &SpawnOptions<'_>,
        log_path: &Path,
        on_event: &impl Fn(&AgentEvent),
        drive: impl FnOnce(&mut dyn FnMut(&str) -> Result<String>, bool) -> Result<T>,
    ) -> Result<T> {
        if let Some(parent) = log_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let collecting = |text: &mut String, event: &AgentEvent| {
            if let AgentEvent::AssistantText(t) = event {
                text.push_str(t);
                text.push('\n');
            }
            on_event(event);
        };
        match effective_runtime(spawn.runtime, &[agent]) {
            LaneRuntime::Acp => {
                let batch = self.start_acp_batch(&[agent], spawn, &mut |event: &AgentEvent| on_event(event))?;
                let outcome = (|| -> Result<T> {
                    let runtime = batch.runtime(agent).ok_or_else(|| anyhow::anyhow!("no ACP process for {}", agent_kind_name(agent)))?;
                    let mut log = std::fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(log_path)
                        .with_context(|| format!("opening log file {}", log_path.display()))?;
                    let mut session = runtime.new_session(cwd, Vec::new()).map_err(|err| anyhow::anyhow!("opening the planner session: {err}"))?;
                    let mut model = match spawn.model {
                        Some(model) => crate::acp_runtime::select_model(&runtime, &mut session, model, agent)?,
                        None => pact_acp::current_model(&session.config_options),
                    };
                    if let Some(model) = &model {
                        on_event(&AgentEvent::Phase(format!("planner model: {model}")));
                    }
                    let mut ask = |prompt: &str| -> Result<String> {
                        let mut text = String::new();
                        let mut forward = |event: &AgentEvent| collecting(&mut text, event);
                        let mut coalescer = crate::acp_runtime::ChunkCoalescer::new();
                        let stop = runtime
                            .prompt(&mut session, prompt, |update| {
                                let line = crate::acp_runtime::log_line(&update);
                                let _ = writeln!(log, "{line}");
                                if let Some(switched) = crate::acp_runtime::model_in_update(&update) {
                                    model = Some(switched);
                                }
                                coalescer.push(&update, &mut forward)
                            })
                            .map_err(|err| anyhow::anyhow!("planner turn: {err}"))?;
                        coalescer.flush(&mut forward);
                        if let (Some(requested), Some(effective)) = (spawn.model, model.as_deref()) {
                            if requested != effective {
                                forward(&AgentEvent::Phase(format!("WARNING: model '{requested}' was requested but the agent ran the planner on '{effective}'")));
                            }
                        }
                        if !stop.is_success() {
                            bail!("the planner's turn ended with stop reason {}", stop.as_str());
                        }
                        Ok(text)
                    };
                    let result = drive(&mut ask, true);
                    let _ = runtime.close(&session);
                    result
                })();
                batch.shutdown();
                outcome
            }
            _ => {
                let adapter = pact_agents::adapter(agent);
                let mut ask = |prompt: &str| -> Result<String> {
                    let mut text = String::new();
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
                        model: spawn.model,
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
                        |event: &AgentEvent| collecting(&mut text, event),
                        |_| {},
                    )?;
                    if !run.success {
                        bail!("the planner run failed: {}", run.summary);
                    }
                    Ok(text)
                };
                drive(&mut ask, false)
            }
        }
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

/// The commit `dir` has checked out: the base of a later wave's
/// ownership check (issue #462).
fn head_commit(dir: &Path) -> Result<String> {
    let output = Command::new("git").args(["rev-parse", "HEAD"]).current_dir(dir).output().context("running `git rev-parse HEAD` in the shared tree")?;
    if !output.status.success() {
        bail!("`git rev-parse HEAD` failed in {}: {}", dir.display(), String::from_utf8_lossy(&output.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// The first ten of `paths`, then how many more.
fn list_paths(paths: &[String]) -> String {
    const SHOWN: usize = 10;
    let mut text = paths.iter().take(SHOWN).map(String::as_str).collect::<Vec<_>>().join(", ");
    if paths.len() > SHOWN {
        text.push_str(&format!(" and {} more", paths.len() - SHOWN));
    }
    text
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

/// Runs one verification command under `timeout` with its whole output
/// logged to `log`, keeping the last 40 lines for the report.
fn run_shell_captured(dir: &Path, cmd: &str, log: &Path, timeout: Duration) -> Result<VerifyOutcome> {
    let start = std::time::Instant::now();
    let run = pact_agents::run_shell_logged(cmd, dir, log, timeout).with_context(|| format!("failed to run verification command '{cmd}'"))?;
    let duration = start.elapsed();
    let combined = String::from_utf8_lossy(&std::fs::read(log).unwrap_or_default()).into_owned();
    let lines: Vec<&str> = combined.lines().collect();
    let tail = lines[lines.len().saturating_sub(40)..].join("\n");
    let success = run.status.is_some_and(|status| status.success());
    Ok(VerifyOutcome {
        command: cmd.to_string(),
        success,
        exit_code: run.status.and_then(|status| status.code()),
        output_tail: tail,
        duration,
        baseline_success: None,
        timed_out: run.timed_out,
        log: log.to_path_buf(),
        failing_files: if success { Vec::new() } else { failing_files(&combined, dir) },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unit(name: &str, files: &[&str]) -> PlanUnit {
        PlanUnit { name: name.into(), files: files.iter().map(|f| f.to_string()).collect(), brief: format!("do {name}"), verify: None, depends_on: vec![] }
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
            timed_out: false,
            log: PathBuf::new(),
            failing_files: Vec::new(),
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
            dirty_paths: Vec::new(),
            ownership: vec![],
            skipped: vec![],
            diff_stat: vec![],
        };
        assert_eq!(report(vec![]).worst_verdict(), None);
        assert_eq!(report(vec![outcome(true, Some(true)), outcome(true, Some(false))]).worst_verdict(), Some(Verdict::Fixed));
        assert_eq!(report(vec![outcome(true, None), outcome(false, Some(false))]).worst_verdict(), Some(Verdict::Inconclusive));
        assert_eq!(report(vec![outcome(false, Some(false)), outcome(false, Some(true))]).worst_verdict(), Some(Verdict::Regressed));
        assert_eq!(report(vec![outcome(false, None), outcome(false, Some(false))]).worst_verdict(), Some(Verdict::Failed));
    }

    fn failing(command: &str, files: Vec<FailingFile>) -> VerifyOutcome {
        VerifyOutcome {
            command: command.into(),
            success: false,
            exit_code: Some(1),
            output_tail: format!("TAIL OF {command}"),
            duration: Duration::ZERO,
            baseline_success: Some(true),
            timed_out: false,
            log: PathBuf::from("verify.log"),
            failing_files: files,
        }
    }

    fn file(path: &str, excerpt: &str) -> FailingFile {
        FailingFile { path: path.into(), excerpt: excerpt.into() }
    }

    #[test]
    fn the_repair_lanes_never_reuse_a_unit_name_or_each_others() {
        let assignment = |unit: Option<&str>| RepairAssignment { unit: unit.map(str::to_string), allowed: vec![], checks: vec![] };
        let names = repair_lane_names(1, &plan(vec![unit("a", &["a.ts"])]), &[assignment(Some("a")), assignment(None)]);
        assert_eq!(names, ["repair-1-a", "repair-1"]);
        let taken = plan(vec![unit("Repair-1", &["a.ts"]), unit("repair-1-2", &["b.ts"])]);
        assert_eq!(repair_lane_names(1, &taken, &[assignment(None)]), ["repair-1-3"]);
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
            PlanUnit { name: "---".into(), files: vec![], brief: "  ".into(), verify: None, depends_on: vec![] },
        ]);
        let problems = validate_plan(&p, 8);
        let joined = problems.join("\n");
        assert!(joined.contains("\"src/shared.ts\" is owned by more than one unit (a, A)"), "{joined}");
        assert!(joined.contains("unit names \"a\" and \"A\" both become workspace \"a\""), "{joined}");
        assert!(joined.contains("\"../outside.ts\" must be a repo-relative path"), "{joined}");
        assert!(joined.contains("\"C:/abs.ts\" must be a repo-relative path"), "{joined}");
        assert!(joined.contains("\"---\" must start with an ASCII letter or digit"), "{joined}");
        assert!(joined.contains("owns no files"), "{joined}");
        assert!(joined.contains("has an empty brief"), "{joined}");
    }

    #[test]
    fn unit_names_must_be_safe_identifiers_unique_by_workspace_id() {
        let named = |names: &[&str]| validate_plan(&plan(names.iter().enumerate().map(|(i, n)| unit(n, &[&format!("f{i}.ts")])).collect()), 8).join("\n");
        assert_eq!(named(&["alpha-one", "beta_2", "v1.2"]), "");
        let slug_clash = named(&["Alpha.One", "alpha-one"]);
        assert!(slug_clash.contains("both become workspace \"alpha-one\""), "{slug_clash}");
        assert!(named(&["alpha", "alpha"]).contains("\"alpha\" is used more than once"));
        for bad in ["..\\..\\..\\ESCAPED", "../x", "a/b", "Alpha One", ".hidden", "-x", ""] {
            assert!(named(&[bad]).contains("must start with an ASCII letter or digit and contain only"), "{bad:?} must be rejected");
        }
        let long = "a".repeat(MAX_UNIT_NAME_LEN + 1);
        assert!(named(&[long.as_str()]).contains("at most 48 are allowed"));
    }

    #[test]
    fn too_many_units_and_no_units_are_rejected() {
        assert_eq!(validate_plan(&plan(vec![]), 8), vec!["the plan has no units".to_string()]);
        let p = plan(vec![unit("a", &["a"]), unit("b", &["b"]), unit("c", &["c"])]);
        assert!(validate_plan(&p, 2).iter().any(|m| m.contains("has 3 units; at most 2")));
    }

    fn after(mut unit: PlanUnit, deps: &[&str]) -> PlanUnit {
        unit.depends_on = deps.iter().map(|d| d.to_string()).collect();
        unit
    }

    #[test]
    fn dependencies_on_unknown_units_on_itself_or_in_a_cycle_are_each_named() {
        let p = plan(vec![
            after(unit("a", &["a.ts"]), &["b"]),
            after(unit("b", &["b.ts"]), &["c", "c"]),
            after(unit("c", &["c.ts"]), &["a"]),
            after(unit("d", &["d.ts"]), &["d", "ghost", "a"]),
        ]);
        let problems = validate_plan(&p, 8);
        let joined = problems.join("\n");
        assert!(joined.contains("unit \"d\" depends on itself"), "{joined}");
        assert!(joined.contains("unit \"d\" depends on \"ghost\", which is not a unit in this plan"), "{joined}");
        assert!(
            joined.contains("units depend on each other in a cycle (\"a\" depends on \"b\", \"b\" depends on \"c\", \"c\" depends on \"a\")"),
            "{joined}"
        );
        assert_eq!(problems.len(), 3, "the duplicate `c` and the edge into the cycle add nothing:\n{joined}");
        let fine = plan(vec![unit("a", &["a.ts"]), after(unit("b", &["b.ts"]), &["a", "a"])]);
        assert!(validate_plan(&fine, 8).is_empty());
    }

    #[test]
    fn a_units_wave_is_one_more_than_its_latest_dependency() {
        let diamond = plan(vec![
            after(unit("d", &["d"]), &["b", "c"]),
            after(unit("b", &["b"]), &["a"]),
            unit("a", &["a"]),
            after(unit("c", &["c"]), &["a"]),
        ]);
        assert_eq!(plan_waves(&diamond), [3, 2, 1, 2]);
        assert_eq!(wave_groups(&diamond), [vec![2], vec![1, 3], vec![0]], "plan order within a wave");
        let chain = plan(vec![after(unit("c", &["c"]), &["b"]), after(unit("b", &["b"]), &["a"]), unit("a", &["a"])]);
        assert_eq!(plan_waves(&chain), [3, 2, 1]);
        let independent = plan(vec![unit("a", &["a"]), unit("b", &["b"])]);
        assert_eq!(wave_groups(&independent), [vec![0, 1]], "no dependencies is one wave, as before #462");
    }

    #[test]
    fn a_dependent_units_brief_names_the_units_before_it_and_their_files() {
        let p = plan(vec![unit("core", &["src/calc.ts", "src/types.ts"]), after(unit("ui", &["src/view.ts"]), &["core"])]);
        let brief = render_brief(&p, &p.units[1]);
        assert!(brief.contains("## Work done before yours"), "{brief}");
        assert!(brief.contains("already written and committed in this checkout: read them as you need, but do not edit them"), "{brief}");
        assert!(brief.contains("- unit `core`: `src/calc.ts`, `src/types.ts`"), "{brief}");
        assert!(!render_brief(&p, &p.units[0]).contains("Work done before yours"));
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
        let p = plan(vec![PlanUnit { name: "parser".into(), files: vec!["lib/parse.test.ts".into()], brief: "Cover parse()".into(), verify: Some("npx vitest run lib".into()), depends_on: vec![] }]);
        let brief = render_brief(&p, &p.units[0]);
        for expected in ["# Unit `parser`", "> big task", "- `lib/parse.test.ts`", "Do the task above for the files you own", "Cover parse()", "use vitest", "Do not install packages", "Do not commit", "`npx vitest run lib`", "reply DONE"] {
            assert!(brief.contains(expected), "missing {expected:?} in:\n{brief}");
        }
    }

    /// Issue #461 (replacing #361's "do not run them yourself"): with
    /// project-wide checks on the plan, the brief has the worker run every
    /// check through run_check, check its own files before finishing, and
    /// says pact also runs the project checks; without any, the unit's
    /// own check still goes through run_check.
    #[test]
    fn briefs_send_every_worker_check_through_run_check_and_name_the_project_checks_pact_runs() {
        let mut p = plan(vec![PlanUnit { name: "u".into(), files: vec!["a.ts".into()], brief: "x".into(), verify: Some("npx vitest run a".into()), depends_on: vec![] }, unit("v", &["b.ts"])]);
        p.verify = vec!["npm test".into(), "npm run typecheck".into(), " ".into()];
        let with_check = render_brief(&p, &p.units[0]);
        for expected in [
            "Run every check (tests, type-check, lint) only through the `run_check` tool of the `pact-coord` MCP server",
            "never directly in your shell",
            "type-check and lint the files you own and run your own tests through it, including your unit's check: `npx vitest run a`.",
            "pact also runs `npm test`, `npm run typecheck` once on the combined result",
        ] {
            assert!(with_check.contains(expected), "missing {expected:?} in:\n{with_check}");
        }
        assert!(!with_check.contains("Do not run them yourself"), "{with_check}");
        let without_check = render_brief(&p, &p.units[1]);
        assert!(without_check.contains("run your own tests through it. pact also runs"), "{without_check}");

        p.verify = Vec::new();
        let no_project_checks = render_brief(&p, &p.units[0]);
        assert!(no_project_checks.contains("Check your own work before finishing with `npx vitest run a`, run only through the `run_check` tool"), "{no_project_checks}");
        assert!(!no_project_checks.contains("pact also runs"), "no promise pact cannot keep:\n{no_project_checks}");
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
    fn a_retry_without_the_conversation_carries_the_whole_first_prompt() {
        let first = planner_prompt("Add tests for lib/", 4, &["lib/a.test.ts".to_string()]);
        let problems = vec!["the plan has 9 units; at most 4 are allowed".to_string()];
        let fresh = retry_prompt(&first, false, "{}", &problems);
        assert!(fresh.starts_with(&first) && fresh.contains("Add tests for lib/") && fresh.contains("lib/a.test.ts"), "{fresh}");
        assert!(fresh.contains("at most 4 are allowed"), "{fresh}");
        let same_session = retry_prompt(&first, true, "{}", &problems);
        assert!(!same_session.contains("Add tests for lib/") && same_session.contains("at most 4 are allowed"), "{same_session}");
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
    fn the_repair_brief_gives_each_failing_file_its_excerpt_and_says_who_else_is_working() {
        let p = plan(vec![unit("alpha", &["src/a.ts"])]);
        let touched = vec!["src/a.ts".to_string(), "package.json".to_string()];
        let routed = route_repairs(&p, &[failing("npm test", vec![file("src/a.ts", "src/a.ts:3 boom")]), failing("lint", vec![file("src/old.test.ts", "FAIL old")])], &touched);
        assert_eq!(routed.len(), 2, "{routed:?}");
        let brief = render_repair_brief(1, 2, &routed[0], &touched, 2);
        assert!(brief.starts_with("# Repair attempt 1/2: unit `alpha`"), "{brief}");
        assert!(brief.contains("#### `src/a.ts`\n\n```text\nsrc/a.ts:3 boom"), "{brief}");
        assert!(brief.contains("1 other repair lane(s) are fixing other files"), "{brief}");
        assert!(brief.contains("Full output: `verify.log`"), "{brief}");
        assert!(!brief.contains("old.test.ts"), "another lane's failure stays out:\n{brief}");
    }

    #[test]
    fn repairs_go_to_the_unit_owning_each_failing_file_and_the_rest_to_one_extra_lane() {
        let p = plan(vec![unit("alpha", &["src/a.ts", "src/a.test.ts"]), unit("beta", &["src/b.ts"]), unit("gamma", &["src/c.ts"])]);
        let touched: Vec<String> = ["src/a.ts", "src/a.test.ts", "src/b.ts", "src/c.ts", "package.json"].iter().map(|s| s.to_string()).collect();
        let outcomes = [
            failing("npm test", vec![file("src/a.test.ts", "FAIL a"), file("src/b.ts", "b broke"), file("src/old.test.ts", "FAIL old")]),
            failing("npm run lint", vec![file("src/a.ts", "a lint")]),
            failing("tsc", vec![]),
        ];
        let routed = route_repairs(&p, &outcomes, &touched);
        let units: Vec<Option<&str>> = routed.iter().map(|a| a.unit.as_deref()).collect();
        assert_eq!(units, [Some("alpha"), Some("beta"), None], "gamma has no failure, so no lane: {routed:?}");

        let alpha = &routed[0];
        assert_eq!(alpha.allowed, ["src/a.ts", "src/a.test.ts"]);
        let alpha_files: Vec<(&str, &str)> = alpha.checks.iter().flat_map(|c| c.files.iter().map(move |f| (c.command.as_str(), f.path.as_str()))).collect();
        assert_eq!(alpha_files, [("npm test", "src/a.test.ts"), ("npm run lint", "src/a.ts")], "each check carries only the lane's own files");

        let extra = &routed[2];
        assert_eq!(extra.allowed, ["src/c.ts", "package.json"], "touched files no unit lane holds, never an untouched file");
        assert_eq!(extra.checks.len(), 2, "{extra:?}");
        assert_eq!(extra.checks[0].files, [file("src/old.test.ts", "FAIL old")]);
        assert_eq!(extra.checks[1].tail.as_deref(), Some("TAIL OF tsc"), "a check naming no file goes whole to the extra lane");

        let lanes_allowed: Vec<&String> = routed.iter().flat_map(|a| &a.allowed).collect();
        let distinct: std::collections::HashSet<&&String> = lanes_allowed.iter().collect();
        assert_eq!(distinct.len(), lanes_allowed.len(), "no file is editable by two lanes: {lanes_allowed:?}");
    }

    #[test]
    fn unowned_failures_with_nothing_left_to_edit_become_context_for_the_unit_lanes() {
        let p = plan(vec![unit("alpha", &["a.ts"]), unit("beta", &["b.ts"])]);
        let touched = vec!["a.ts".to_string(), "b.ts".to_string()];
        let routed = route_repairs(&p, &[failing("test", vec![file("a.ts", "x"), file("b.ts", "y"), file("old.test.ts", "z")])], &touched);
        assert_eq!(routed.len(), 2, "{routed:?}");
        assert!(routed.iter().all(|a| a.checks.iter().any(|c| c.files.iter().any(|f| f.path == "old.test.ts"))), "{routed:?}");

        let only_unowned = route_repairs(&p, &[failing("tsc", vec![])], &touched);
        assert_eq!(only_unowned.len(), 1);
        assert_eq!((only_unowned[0].unit.as_deref(), only_unowned[0].allowed.len()), (None, 2), "with no unit lane the extra lane may edit every touched file");
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
            timed_out: false,
            log: PathBuf::new(),
            failing_files: Vec::new(),
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
