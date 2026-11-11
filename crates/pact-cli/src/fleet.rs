//! `pact fleet <tasks.toml>` (issue #388): the file format and the
//! command. The engine is `pact_core::fleet`.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use pact_agents::AgentKind;
use pact_core::fleet::{with_report_contract, FleetOutcome, FleetStatus, FleetTask};
use pact_core::{AdmissionPolicy, LaneRuntime, Orchestrator, SpawnOptions};
use serde::Deserialize;

use crate::config::PactConfig;

#[derive(clap::Args)]
pub struct FleetArgs {
    /// TOML file with one `[[task]]` table per task: `name` and `prompt`
    /// (or `prompt_file`) are required; `repo`, `agent`, and `model` or
    /// `tier` are optional. `[defaults]` and `[tiers.<name>]` mean what
    /// they mean in pact.toml and apply to every task in the file.
    /// Relative paths are resolved against the file's own directory.
    file: PathBuf,

    /// Print what each task would create and launch, then exit without
    /// creating a workspace or launching anything.
    #[arg(long)]
    dry_run: bool,

    /// Same safety/approval override as `spawn --safety`, applied to every
    /// task. Falls back to the file's `defaults.safety`.
    #[arg(long)]
    safety: Option<String>,

    /// How each workspace gets its dependencies: auto, link, install or
    /// none (see `spawn --deps`). Falls back to the file's `defaults.deps`,
    /// then auto.
    #[arg(long, conflicts_with = "no_deps")]
    deps: Option<String>,

    /// Alias for `--deps none`.
    #[arg(long)]
    no_deps: bool,

    /// Launch each agent CLI with the user's full config instead of the
    /// lean profile (see `spawn --no-lean`).
    #[arg(long)]
    no_lean: bool,

    /// Most agents running at once across the whole fleet, every
    /// repository included. Falls back to the file's
    /// `defaults.max_concurrent`, then 4.
    #[arg(long)]
    max_concurrent: Option<usize>,

    /// Free memory (MiB) required before another agent is launched; 0
    /// disables the check. Falls back to the file's
    /// `defaults.min_free_mem_mb`, then 1500.
    #[arg(long)]
    min_free_mem_mb: Option<u64>,

    /// Memory (MiB) each running agent is assumed to still grow into when
    /// the next launch is decided. Falls back to the file's
    /// `defaults.per_lane_reserve_mb`, then 1200.
    #[arg(long)]
    per_lane_reserve_mb: Option<u64>,

    /// Minimum gap between two launches, in milliseconds. Falls back to
    /// the file's `defaults.stagger_ms`, then 2000.
    #[arg(long)]
    stagger_ms: Option<u64>,
}

#[derive(Deserialize)]
struct FleetFile {
    #[serde(default, rename = "task")]
    tasks: Vec<TaskSpec>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TaskSpec {
    name: String,
    repo: Option<PathBuf>,
    agent: Option<String>,
    model: Option<String>,
    tier: Option<String>,
    prompt: Option<String>,
    prompt_file: Option<PathBuf>,
}

fn git_toplevel(path: &Path) -> Result<PathBuf> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(path)
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .context("failed to run git")?;
    if !output.status.success() {
        bail!("{} is not inside a git repository", path.display());
    }
    let top = String::from_utf8_lossy(&output.stdout).trim().to_string();
    std::path::absolute(&top).with_context(|| format!("resolving {top}"))
}

/// Parses the fleet file into its `[defaults]`/`[tiers]` config and its
/// tasks, with every repository, agent, model and prompt resolved.
/// `default_repo` is asked for only when a task names no `repo`, so a
/// fleet whose tasks all name one runs from outside any repository.
fn load(file: &Path, default_repo: impl Fn() -> Result<PathBuf>) -> Result<(PactConfig, Vec<FleetTask>)> {
    let file = std::path::absolute(file).with_context(|| format!("resolving {}", file.display()))?;
    let base = file.parent().unwrap_or(Path::new(".")).to_path_buf();
    let contents = std::fs::read_to_string(&file).with_context(|| format!("failed to read {}", file.display()))?;
    let config: PactConfig = toml::from_str(&contents).with_context(|| format!("failed to parse {}", file.display()))?;
    let parsed: FleetFile = toml::from_str(&contents).with_context(|| format!("failed to parse {}", file.display()))?;
    if parsed.tasks.is_empty() {
        bail!("{} has no [[task]] tables", file.display());
    }

    let mut tasks = Vec::with_capacity(parsed.tasks.len());
    for spec in parsed.tasks {
        let name = spec.name.clone();
        tasks.push(resolve_task(spec, &config, &base, &default_repo).with_context(|| format!("task '{name}'"))?);
    }
    let names: Vec<String> = tasks.iter().map(|t| t.name.clone()).collect();
    if let Some(dup) = crate::first_duplicate(&names) {
        bail!("task name '{dup}' appears more than once; names must be unique within a fleet");
    }
    Ok((config, tasks))
}

fn resolve_task(
    spec: TaskSpec,
    config: &PactConfig,
    base: &Path,
    default_repo: &impl Fn() -> Result<PathBuf>,
) -> Result<FleetTask> {
    if !spec.name.chars().any(|c| c.is_ascii_alphanumeric()) {
        bail!("name must contain at least one ASCII letter or digit");
    }
    let repo = match &spec.repo {
        Some(repo) => git_toplevel(&base.join(repo))?,
        None => default_repo().context(
            "it names no repo and pact is not running inside a git repository; add `repo = \"...\"` or pass --repo",
        )?,
    };
    let agent_name = spec
        .agent
        .as_deref()
        .or(config.default_agent())
        .context("it names no agent and the file has no [defaults] agent")?;
    let agent = AgentKind::parse(agent_name)
        .with_context(|| format!("unknown agent '{agent_name}' (expected claude, copilot, codex, gemini, or agy)"))?;
    if spec.model.is_some() && spec.tier.is_some() {
        bail!("give model or tier, not both");
    }
    let model = config.resolve_model(spec.model.as_deref(), spec.tier.as_deref(), agent_name)?;
    let prompt = match (spec.prompt, spec.prompt_file) {
        (Some(prompt), None) => prompt,
        (None, Some(path)) => {
            let path = base.join(path);
            std::fs::read_to_string(&path).with_context(|| format!("failed to read prompt_file {}", path.display()))?
        }
        _ => bail!("give exactly one of prompt or prompt_file"),
    };
    if prompt.trim().is_empty() {
        bail!("the prompt is empty");
    }
    Ok(FleetTask { name: spec.name, repo, agent, model, prompt })
}

pub fn run(args: FleetArgs, repo_flag: Option<&Path>, verbose: bool) -> Result<()> {
    let default_repo = || match repo_flag {
        Some(repo) => std::path::absolute(repo).with_context(|| format!("resolving --repo {}", repo.display())),
        None => crate::find_repo_root(&std::env::current_dir()?),
    };
    let (config, tasks) = load(&args.file, default_repo)?;

    let deps = crate::resolve_deps_mode(args.deps, args.no_deps, &config)?;
    let safety = args.safety.or_else(|| config.default_safety().map(str::to_string));
    let lean = !args.no_lean;
    let admission = AdmissionPolicy {
        max_concurrent: args.max_concurrent.or(config.default_max_concurrent()).unwrap_or(4),
        min_free_mem_mb: args.min_free_mem_mb.or(config.default_min_free_mem_mb()).unwrap_or(1500),
        stagger: std::time::Duration::from_millis(args.stagger_ms.or(config.default_stagger_ms()).unwrap_or(2000)),
        per_lane_reserve_mb: args
            .per_lane_reserve_mb
            .or(config.default_per_lane_reserve_mb())
            .unwrap_or_else(|| AdmissionPolicy::default_per_lane_reserve_mb(LaneRuntime::Process)),
    };

    if args.dry_run {
        println!(
            "admission: at most {} agent{} running at once across the fleet, {} MB free memory required before \
             each launch (each running agent reserves {} MB against that), {} ms between launches",
            admission.max_concurrent,
            if admission.max_concurrent == 1 { "" } else { "s" },
            admission.min_free_mem_mb,
            admission.per_lane_reserve_mb,
            admission.stagger.as_millis()
        );
        for task in &tasks {
            println!(
                "task {}: repo {}, agent {}, model {}",
                task.name,
                task.repo.display(),
                crate::agent_label(task.agent),
                task.model.as_deref().unwrap_or("CLI default")
            );
            let orchestrator = Orchestrator::open(task.repo.clone())?;
            let preview = orchestrator.spawn_preview(
                task.agent,
                &with_report_contract(&task.prompt),
                Some(&task.name),
                safety.as_deref(),
                None,
                lean,
                task.model.as_deref(),
            )?;
            crate::print_spawn_preview(&preview, deps);
        }
        return Ok(());
    }

    let mut warned = std::collections::HashSet::new();
    for task in &tasks {
        if warned.insert(task.agent) {
            crate::warn_unattended(task.agent, crate::agent_label(task.agent), safety.as_deref());
        }
    }

    let rows: Vec<(PathBuf, String)> = tasks
        .iter()
        .map(|t| {
            let model = t.model.as_deref().map(|m| format!("/{m}")).unwrap_or_default();
            (t.repo.clone(), format!("{}{model}", crate::agent_label(t.agent)))
        })
        .collect();
    let labels: Vec<String> = tasks.iter().map(|t| t.name.clone()).collect();
    let options = SpawnOptions {
        safety_override: safety.as_deref(),
        deps,
        lean,
        admission,
        runtime: LaneRuntime::Process,
        ..SpawnOptions::default()
    };
    let outcomes = pact_core::fleet::run_fleet(tasks, &options, |index, event| {
        crate::print_event_labeled(&labels[index], event, verbose);
    });

    print_table(&outcomes, &rows);
    if outcomes.iter().any(|o| o.status != FleetStatus::Done) {
        std::process::exit(1);
    }
    Ok(())
}

fn first_line(text: &str) -> &str {
    text.lines().find(|l| !l.trim().is_empty()).unwrap_or("").trim()
}

fn print_table(outcomes: &[FleetOutcome], rows: &[(PathBuf, String)]) {
    let done = outcomes.iter().filter(|o| o.status == FleetStatus::Done).count();
    println!(
        "fleet: {} task{}, {} done, {} not done",
        outcomes.len(),
        if outcomes.len() == 1 { "" } else { "s" },
        done,
        outcomes.len() - done
    );
    let branch = |o: &FleetOutcome| o.workspace.as_ref().map(|w| w.branch.clone()).unwrap_or_else(|| "-".to_string());
    let width = |header: &str, cell: &dyn Fn(usize) -> usize| (0..outcomes.len()).map(cell).max().unwrap_or(0).max(header.len());
    let name_w = width("TASK", &|i| outcomes[i].name.len());
    let agent_w = width("AGENT/MODEL", &|i| rows[i].1.len());
    let branch_w = width("BRANCH", &|i| branch(&outcomes[i]).len());
    println!(
        "  {:name_w$}  {:10}  {:>5}  {:agent_w$}  {:branch_w$}  REPO",
        "TASK", "STATUS", "SECS", "AGENT/MODEL", "BRANCH"
    );
    for (outcome, (repo, agent)) in outcomes.iter().zip(rows) {
        println!(
            "  {:name_w$}  {:10}  {:>5}  {:agent_w$}  {:branch_w$}  {}",
            outcome.name,
            outcome.status.as_str(),
            outcome.seconds,
            agent,
            branch(outcome),
            repo.display()
        );
        let notes = outcome.report.as_ref().map(|r| r.notes.as_str()).filter(|n| !n.is_empty());
        match outcome.status {
            FleetStatus::Done => {}
            FleetStatus::Blocked | FleetStatus::Failed => {
                println!("      NOTES: {}", notes.unwrap_or("(none given)"));
            }
            FleetStatus::Unreported => {
                println!("      no STATUS report in the agent's final message, which began: {}", first_line(&outcome.summary));
            }
            FleetStatus::Error => println!("      error: {}", outcome.summary),
        }
    }
    if outcomes.iter().any(|o| o.workspace.is_some()) {
        println!(
            "each task's changes are uncommitted in its own workspace: review with `pact --repo <REPO> diff <TASK>`, \
             land with `pact --repo <REPO> land <TASK>` (add --pr for a pull request), \
             drop with `pact --repo <REPO> discard <TASK>`"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch_repo() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("pact-fleet-file-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        let status = std::process::Command::new("git").args(["init", "-q"]).current_dir(&dir).status().unwrap();
        assert!(status.success());
        dir
    }

    fn load_str(dir: &Path, toml: &str) -> Result<(PactConfig, Vec<FleetTask>)> {
        let file = dir.join("fleet.toml");
        std::fs::write(&file, toml).unwrap();
        load(&file, || bail!("no default repo in this test"))
    }

    #[test]
    fn load_resolves_repo_agent_model_and_prompt_file() {
        let dir = scratch_repo();
        std::fs::write(dir.join("brief.md"), "Do the thing.").unwrap();
        let (_, tasks) = load_str(
            &dir,
            "[defaults]\nagent = \"claude\"\nmodel = \"sonnet\"\n[tiers.chore]\nclaude = \"haiku\"\ncodex = \"mini\"\n\
             [[task]]\nname = \"a\"\nrepo = \"sub\"\ntier = \"chore\"\nprompt = \"x\"\n\
             [[task]]\nname = \"b\"\nrepo = \".\"\nagent = \"codex\"\nmodel = \"big\"\nprompt_file = \"brief.md\"\n\
             [[task]]\nname = \"c\"\nrepo = \".\"\nprompt = \"y\"\n",
        )
        .unwrap();
        assert_eq!(tasks.len(), 3);
        assert_eq!(tasks[0].repo, tasks[1].repo, "a subdirectory resolves to the repository's top level");
        assert_eq!(tasks[0].repo.file_name(), dir.file_name());
        assert_eq!((tasks[0].agent, tasks[0].model.as_deref()), (AgentKind::Claude, Some("haiku")));
        assert_eq!((tasks[1].agent, tasks[1].model.as_deref()), (AgentKind::Codex, Some("big")));
        assert_eq!(tasks[1].prompt, "Do the thing.");
        assert_eq!(tasks[2].model.as_deref(), Some("sonnet"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_rejects_malformed_tasks() {
        let dir = scratch_repo();
        let err = |toml: &str| format!("{:#}", load_str(&dir, toml).err().expect("expected an error"));
        let task = |body: &str| format!("[[task]]\nname = \"a\"\nrepo = \".\"\nagent = \"claude\"\n{body}");

        assert!(err("[defaults]\nagent = \"claude\"\n").contains("no [[task]]"));
        assert!(err(&task("")).contains("exactly one of prompt or prompt_file"));
        assert!(err(&task("prompt = \"x\"\nprompt_file = \"y\"\n")).contains("exactly one"));
        assert!(err(&task("prompt = \"x\"\nmodel = \"m\"\ntier = \"t\"\n")).contains("not both"));
        assert!(err(&task("prompt = \"x\"\ntier = \"nope\"\n")).contains("tier 'nope'"));
        assert!(err(&task("promt = \"x\"\n")).contains("promt"));
        assert!(err(&format!("{}{}", task("prompt = \"x\"\n"), task("prompt = \"y\"\n"))).contains("more than once"));
        assert!(err("[[task]]\nname = \"a\"\nrepo = \".\"\nprompt = \"x\"\n").contains("no agent"));
        assert!(err("[[task]]\nname = \"a\"\nagent = \"claude\"\nprompt = \"x\"\n").contains("names no repo"));
        assert!(err("[[task]]\nname = \"a\"\nrepo = \"missing-dir\"\nagent = \"claude\"\nprompt = \"x\"\n")
            .contains("not inside a git repository"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
