//! `pact ui` (issue #390): a local web page over the fleet engine, for
//! starting, watching, reviewing, landing and discarding tasks without
//! the command line. See DESIGN.md ("`pact ui`").

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Instant;

use anyhow::{bail, Context, Result};
use axum::extract::{Query, Request, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::middleware::Next;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use pact_agents::{AgentEvent, AgentKind};
use pact_core::fleet::{FleetOutcome, FleetRunner, FleetStatus, FleetTask, ReportStatus};
use pact_core::{AdmissionDecision, AdmissionPolicy, EntryState, LaneRuntime, Orchestrator, SpawnOptions, WorkspaceEntry};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::config::PactConfig;

const PAGE: &str = include_str!("ui.html");
const DEFAULT_PORT: u16 = 7228;
const MAX_PATCH_BYTES: usize = 400 * 1024;
const MAX_LOG_LINES: usize = 300;
const MAX_LOG_LINE_CHARS: usize = 2000;
const HISTORY_LIMIT: i64 = 100;
const AGENT_NAMES: [&str; 5] = ["claude", "copilot", "codex", "gemini", "agy"];

#[derive(clap::Args)]
pub struct UiArgs {
    /// Port to listen on, on 127.0.0.1 only. 0 picks any free port.
    #[arg(long, default_value_t = DEFAULT_PORT)]
    port: u16,

    /// Print the address without opening it in the default browser.
    #[arg(long)]
    no_open: bool,

    /// Same safety/approval override as `spawn --safety`, applied to every
    /// task started from the page. Falls back to each repository's
    /// `defaults.safety`, then to each agent's unattended default.
    #[arg(long)]
    safety: Option<String>,

    /// Most agents running at once across every repository.
    #[arg(long, default_value_t = 4)]
    max_concurrent: usize,

    /// Free memory (MiB) required before another agent is launched; 0
    /// disables the check.
    #[arg(long, default_value_t = 1500)]
    min_free_mem_mb: u64,

    /// Memory (MiB) each running agent is assumed to still grow into when
    /// the next launch is decided.
    #[arg(long, default_value_t = AdmissionPolicy::PROCESS_LANE_RESERVE_MB)]
    per_lane_reserve_mb: u64,

    /// Minimum gap between two launches, in milliseconds.
    #[arg(long, default_value_t = 2000)]
    stagger_ms: u64,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Starting,
    Queued,
    Running,
    Error,
}

impl Phase {
    fn as_str(self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Error => "error",
        }
    }
}

/// A task this process started and has not finished reporting on. Once
/// its agent ends, the workspace's own metadata takes over and the entry
/// is dropped, except for an error, which has nowhere else to live.
#[derive(Clone)]
struct LiveTask {
    repo: PathBuf,
    id: String,
    agent: &'static str,
    model: Option<String>,
    prompt: String,
    phase: Phase,
    /// Why it is queued, what it is setting up, the agent's latest line,
    /// or the error.
    detail: String,
    log: VecDeque<String>,
    started: Instant,
    discarded: bool,
    /// A reply to a task that already ran (issue #395): its workspace
    /// and run record hold whatever goes wrong, so nothing is kept here.
    reply: bool,
}

struct App {
    token: String,
    nonce: String,
    hosts: [String; 2],
    runner: FleetRunner,
    safety: Option<String>,
    repos: Mutex<Vec<PathBuf>>,
    repos_file: Option<PathBuf>,
    live: Mutex<Vec<LiveTask>>,
    /// `None` until the version probes finish in the background.
    installed: Mutex<Option<Vec<&'static str>>>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn cut(text: &str, max_chars: usize) -> String {
    if text.chars().count() > max_chars {
        format!("{}...", text.chars().take(max_chars).collect::<String>())
    } else {
        text.to_string()
    }
}

fn last_line(text: &str) -> &str {
    text.lines().rev().map(str::trim).find(|line| !line.is_empty()).unwrap_or("")
}

/// Lower-case letters, digits and single hyphens, which the workspace
/// slug rules leave unchanged, so the name is also the workspace id.
fn slug(text: &str, max_len: usize) -> String {
    let mut slug = String::new();
    for ch in text.chars() {
        if slug.len() >= max_len {
            break;
        }
        if ch.is_ascii_alphanumeric() {
            slug.push(ch.to_ascii_lowercase());
        } else if !slug.is_empty() && !slug.ends_with('-') {
            slug.push('-');
        }
    }
    slug.trim_end_matches('-').to_string()
}

fn generated_name(prompt: &str) -> String {
    let words: Vec<&str> = prompt.split_whitespace().take(4).collect();
    let suffix = &uuid::Uuid::new_v4().simple().to_string()[..4];
    match slug(&words.join(" "), 24) {
        base if base.is_empty() => format!("task-{suffix}"),
        base => format!("{base}-{suffix}"),
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn none_if_blank(value: Option<String>) -> Option<String> {
    value.map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
}

impl App {
    fn known_repo(&self, path: &str) -> Result<PathBuf> {
        lock(&self.repos)
            .iter()
            .find(|repo| repo.to_string_lossy() == path)
            .cloned()
            .with_context(|| format!("{path} is not one of the repositories added to this page"))
    }

    fn save_repos(&self, repos: &[PathBuf]) {
        let Some(file) = &self.repos_file else { return };
        let saved = file
            .parent()
            .map_or(Ok(()), std::fs::create_dir_all)
            .and_then(|()| std::fs::write(file, serde_json::to_string_pretty(repos).unwrap_or_default()));
        if let Err(err) = saved {
            tracing::warn!("could not save the repository list to {}: {err}", file.display());
        }
    }

    fn add_repo(&self, path: &Path) -> Result<PathBuf> {
        let repo = crate::fleet::git_toplevel(path)?;
        let mut repos = lock(&self.repos);
        if !repos.contains(&repo) {
            repos.push(repo.clone());
            self.save_repos(&repos);
        }
        Ok(repo)
    }

    fn record(&self, repo: &Path, id: &str, event: &AgentEvent) {
        let mut live = lock(&self.live);
        let Some(task) = live.iter_mut().find(|task| task.repo == repo && task.id == id) else { return };
        let line = match event {
            AgentEvent::Phase(text) => {
                if text.starts_with("queued") {
                    task.phase = Phase::Queued;
                    task.detail = text.clone();
                } else if text.starts_with("running agent") {
                    task.phase = Phase::Running;
                    task.detail.clear();
                } else if task.phase != Phase::Running {
                    task.phase = Phase::Starting;
                    task.detail = text.clone();
                }
                format!("[pact] {text}")
            }
            AgentEvent::AssistantText(text) => {
                task.detail = cut(last_line(text), 200);
                text.trim().to_string()
            }
            AgentEvent::ToolUse { name, input } => {
                task.detail = format!("using {name}");
                format!("[tool] {name} {}", cut(&input.to_string(), 300))
            }
            _ => return,
        };
        if task.log.len() == MAX_LOG_LINES {
            task.log.pop_front();
        }
        task.log.push_back(cut(&line, MAX_LOG_LINE_CHARS));
    }

    fn finish(&self, repo: &Path, id: &str, outcome: Option<FleetOutcome>) {
        let mut live = lock(&self.live);
        let Some(index) = live.iter().position(|task| task.repo == repo && task.id == id) else { return };
        let error = match outcome {
            Some(outcome) if outcome.status == FleetStatus::Error => outcome.summary,
            Some(_) => String::new(),
            None => "pact hit an internal error while running this task".to_string(),
        };
        if error.is_empty() || live[index].discarded || live[index].reply {
            live.remove(index);
        } else {
            live[index].phase = Phase::Error;
            live[index].detail = error;
        }
    }
}

#[derive(Serialize)]
struct TaskRow {
    repo: PathBuf,
    id: String,
    /// starting, queued, running, done, blocked, failed, finished (ended
    /// without a report), idle (no run recorded) or error.
    status: &'static str,
    detail: String,
    prompt: String,
    branch: Option<String>,
    agent: Option<String>,
    model: Option<String>,
    seconds: Option<u64>,
    dirty: Option<bool>,
    commits_ahead: Option<usize>,
    summary: Option<String>,
    report: Option<pact_core::fleet::WorkerReport>,
    created_at: Option<u64>,
    has_workspace: bool,
    shared_tree: bool,
}

fn entry_status(entry: &WorkspaceEntry) -> &'static str {
    let reported = entry.report.as_ref().map(|report| report.status);
    match (entry.state, reported) {
        (EntryState::Running, _) => "running",
        (EntryState::Idle, _) => "idle",
        (_, Some(ReportStatus::Blocked)) => "blocked",
        (EntryState::Failed, _) | (_, Some(ReportStatus::Failed)) => "failed",
        (EntryState::Finished, Some(ReportStatus::Done)) => "done",
        (EntryState::Finished, None) => "finished",
    }
}

fn row_from_entry(entry: WorkspaceEntry, live: Option<&LiveTask>) -> TaskRow {
    let status = live.map_or_else(|| entry_status(&entry), |task| task.phase.as_str());
    TaskRow {
        status,
        detail: live.map(|task| task.detail.clone()).unwrap_or_default(),
        seconds: live.map(|task| task.started.elapsed().as_secs()).or(entry.seconds),
        agent: live.map(|task| task.agent.to_string()).or(entry.agent),
        model: live.and_then(|task| task.model.clone()).or(entry.model),
        repo: entry.repo,
        id: entry.id,
        prompt: entry.task,
        branch: Some(entry.branch),
        dirty: entry.dirty,
        commits_ahead: entry.commits_ahead,
        summary: entry.summary,
        report: entry.report,
        created_at: Some(entry.created_at),
        has_workspace: true,
        shared_tree: entry.shared_batch.is_some(),
    }
}

fn row_from_live(task: &LiveTask) -> TaskRow {
    TaskRow {
        repo: task.repo.clone(),
        id: task.id.clone(),
        status: task.phase.as_str(),
        detail: task.detail.clone(),
        prompt: task.prompt.clone(),
        branch: None,
        agent: Some(task.agent.to_string()),
        model: task.model.clone(),
        seconds: Some(task.started.elapsed().as_secs()),
        dirty: None,
        commits_ahead: None,
        summary: None,
        report: None,
        created_at: None,
        has_workspace: false,
        shared_tree: false,
    }
}

fn state(app: &App) -> Result<serde_json::Value> {
    let repos = lock(&app.repos).clone();
    let live: Vec<LiveTask> = lock(&app.live).iter().filter(|task| !task.discarded).cloned().collect();

    let mut repo_rows = Vec::new();
    let mut tasks = Vec::new();
    for repo in &repos {
        let entries = Orchestrator::open(repo.clone()).and_then(|orchestrator| orchestrator.entries());
        let config = PactConfig::load(repo);
        let branch = pact_vcs::WorkspaceManager::open(repo).and_then(|manager| manager.current_branch()).ok();
        let error = match (&entries, &config) {
            (Err(err), _) | (_, Err(err)) => Some(format!("{err:#}")),
            _ => None,
        };
        let config = config.unwrap_or_default();
        repo_rows.push(json!({
            "path": repo,
            "name": repo.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_else(|| repo.display().to_string()),
            "branch": branch,
            "tiers": config.tiers(),
            "default_agent": config.default_agent(),
            "default_model": config.default_model(),
            "error": error,
        }));
        let entries = entries.unwrap_or_default();
        for task in live.iter().filter(|task| &task.repo == repo && !entries.iter().any(|entry| entry.id == task.id)) {
            tasks.push(row_from_live(task));
        }
        for entry in entries {
            let overlay = live.iter().find(|task| &task.repo == repo && task.id == entry.id);
            tasks.push(row_from_entry(entry, overlay));
        }
    }

    let policy = app.runner.policy();
    let running = live.iter().filter(|task| task.phase == Phase::Running).count();
    let available_mb = pact_core::available_memory_mb();
    let admission = match pact_core::decide(running, policy, available_mb) {
        AdmissionDecision::Admit => "admit",
        AdmissionDecision::WaitForSlot => "slot",
        AdmissionDecision::WaitForMemory => "memory",
    };
    let installed = lock(&app.installed).clone();
    let agents: Vec<_> = AGENT_NAMES
        .iter()
        .map(|name| {
            let unattended = AgentKind::parse(name).map(|kind| pact_agents::adapter(kind).default_safety_description());
            json!({
                "name": name,
                "installed": installed.as_ref().map(|found| found.contains(name)),
                "unattended": unattended,
            })
        })
        .collect();

    Ok(json!({
        "version": env!("CARGO_PKG_VERSION"),
        "memory": {
            "available_mb": available_mb,
            "min_free_mem_mb": policy.min_free_mem_mb,
            "per_lane_reserve_mb": policy.per_lane_reserve_mb,
        },
        "running": running,
        "max_concurrent": policy.max_concurrent,
        "admission": admission,
        "safety": app.safety,
        "agents": agents,
        "repos": repo_rows,
        "tasks": tasks,
    }))
}

#[derive(Deserialize)]
struct NewTask {
    repo: String,
    name: Option<String>,
    agent: String,
    model: Option<String>,
    tier: Option<String>,
    prompt: String,
}

fn start_task(app: &Arc<App>, request: NewTask) -> Result<serde_json::Value> {
    let repo = app.known_repo(&request.repo)?;
    if request.prompt.trim().is_empty() {
        bail!("write what the agent should do first");
    }
    let agent = AgentKind::parse(&request.agent)
        .with_context(|| format!("unknown agent '{}' (expected claude, copilot, codex, gemini, or agy)", request.agent))?;
    let (model, tier) = (none_if_blank(request.model), none_if_blank(request.tier));
    if model.is_some() && tier.is_some() {
        bail!("give a model or a tier, not both");
    }
    let config = PactConfig::load(&repo)?;
    let model = config.resolve_model(model.as_deref(), tier.as_deref(), &request.agent)?;
    let deps = crate::resolve_deps_mode(None, false, &config)?;

    let id = match none_if_blank(request.name) {
        Some(name) => match slug(&name, 32) {
            id if id.is_empty() => bail!("the name must contain at least one ASCII letter or digit"),
            id => id,
        },
        None => generated_name(&request.prompt),
    };
    let exists = pact_vcs::WorkspaceManager::open(&repo)?.get_workspace(&id).is_ok();
    {
        let mut live = lock(&app.live);
        if exists || live.iter().any(|task| task.repo == repo && task.id == id) {
            bail!("a task named '{id}' already exists in this repository; land or discard it, or pick another name");
        }
        live.push(LiveTask {
            repo: repo.clone(),
            id: id.clone(),
            agent: crate::agent_label(agent),
            model: model.clone(),
            prompt: request.prompt.clone(),
            phase: Phase::Starting,
            detail: String::new(),
            log: VecDeque::new(),
            started: Instant::now(),
            discarded: false,
            reply: false,
        });
    }

    let task = FleetTask { name: id.clone(), repo, agent, model, prompt: request.prompt };
    launch(app, task, config, deps, false);
    Ok(json!({ "id": id }))
}

/// Runs `task` on a thread of its own and reports into its `LiveTask`.
/// `existing_workspace` reruns the agent where the task already lives.
fn launch(app: &Arc<App>, task: FleetTask, config: PactConfig, deps: pact_core::DepsMode, existing_workspace: bool) {
    let app = Arc::clone(app);
    std::thread::spawn(move || {
        let safety = app.safety.clone().or_else(|| config.default_safety().map(str::to_string));
        let options = SpawnOptions {
            safety_override: safety.as_deref(),
            deps,
            runtime: LaneRuntime::Process,
            prepare: config.default_prepare(),
            ..SpawnOptions::default()
        };
        let on_event = |event: &AgentEvent| app.record(&task.repo, &task.name, event);
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            if existing_workspace {
                app.runner.rerun_one(&task, &options, on_event)
            } else {
                app.runner.run_one(&task, &options, on_event)
            }
        }));
        app.finish(&task.repo, &task.name, outcome.ok());
    });
}

#[derive(Deserialize)]
struct ReplyRequest {
    repo: String,
    id: String,
    answer: String,
}

/// Runs the task's own agent again in its workspace with the original
/// task, the agent's last message and `answer` (issue #395).
fn reply(app: &Arc<App>, request: ReplyRequest) -> Result<serde_json::Value> {
    let repo = app.known_repo(&request.repo)?;
    let id = request.id;
    if request.answer.trim().is_empty() {
        bail!("write your reply to the agent first");
    }
    let orchestrator = Orchestrator::open(repo.clone())?;
    let workspace = orchestrator.get_workspace(&id)?;
    if workspace.shared_batch.is_some() {
        bail!("task {id} shares its working copy with a pact run batch and cannot be replied to on its own");
    }
    let run = orchestrator
        .run_metadata(&id)
        .with_context(|| format!("task {id} has no recorded run to reply to"))?;
    let agent = AgentKind::parse(&run.agent).with_context(|| format!("task {id} was run by an unknown agent '{}'", run.agent))?;
    let config = PactConfig::load(&repo)?;
    {
        let mut live = lock(&app.live);
        let running = workspace.agent_pid.is_some_and(pact_core::agent_process_alive);
        if running || live.iter().any(|task| task.repo == repo && task.id == id) {
            bail!("task {id} has not finished yet; wait for it before replying");
        }
        live.push(LiveTask {
            repo: repo.clone(),
            id: id.clone(),
            agent: crate::agent_label(agent),
            model: run.model.clone(),
            prompt: workspace.task.clone(),
            phase: Phase::Starting,
            detail: String::new(),
            log: VecDeque::new(),
            started: Instant::now(),
            discarded: false,
            reply: true,
        });
    }
    let prompt = pact_core::fleet::reply_prompt(&workspace.task, &run.summary, &request.answer);
    let task = FleetTask { name: id.clone(), repo, agent, model: run.model, prompt };
    launch(app, task, config, pact_core::DepsMode::None, true);
    Ok(json!({ "id": id }))
}

#[derive(Deserialize)]
struct TaskRef {
    repo: String,
    id: String,
    #[serde(default)]
    pr: bool,
}

fn task_detail(app: &App, task: &TaskRef) -> Result<serde_json::Value> {
    let repo = app.known_repo(&task.repo)?;
    let log: Vec<String> = lock(&app.live)
        .iter()
        .find(|live| live.repo == repo && live.id == task.id)
        .map(|live| live.log.iter().cloned().collect())
        .unwrap_or_default();
    let orchestrator = Orchestrator::open(repo)?;
    let (patch, truncated, patch_error) = match orchestrator.patch(&task.id, MAX_PATCH_BYTES) {
        Ok(patch) => (patch.patch, patch.truncated, None),
        Err(err) => (String::new(), false, Some(format!("{err:#}"))),
    };
    Ok(json!({ "log": log, "patch": patch, "truncated": truncated, "patch_error": patch_error }))
}

fn land(app: &App, task: &TaskRef) -> Result<serde_json::Value> {
    let repo = app.known_repo(&task.repo)?;
    if lock(&app.live).iter().any(|live| live.repo == repo && live.id == task.id && live.phase != Phase::Error) {
        bail!("task {} has not finished yet; wait for it, or discard it", task.id);
    }
    let outcome = Orchestrator::open(repo.clone())?.land(&task.id, task.pr)?;
    lock(&app.live).retain(|live| !(live.repo == repo && live.id == task.id));
    Ok(serde_json::to_value(outcome)?)
}

fn discard(app: &App, task: &TaskRef) -> Result<serde_json::Value> {
    let repo = app.known_repo(&task.repo)?;
    let has_workspace = pact_vcs::WorkspaceManager::open(&repo)?.get_workspace(&task.id).is_ok();
    {
        let mut live = lock(&app.live);
        let found = live.iter_mut().find(|live| live.repo == repo && live.id == task.id);
        match found {
            Some(live) if !has_workspace && live.phase != Phase::Error => {
                bail!("task {} is still being set up; try again in a moment", task.id)
            }
            Some(live) => live.discarded = true,
            None if !has_workspace => bail!("there is no task named {} in this repository", task.id),
            None => {}
        }
        live.retain(|live| !(live.repo == repo && live.id == task.id && live.phase == Phase::Error));
    }
    if !has_workspace {
        return Ok(json!({ "id": task.id }));
    }
    let discarded = Orchestrator::open(repo)?.discard(&task.id)?;
    app.runner.recheck_waiters();
    Ok(serde_json::to_value(discarded)?)
}

#[derive(Deserialize)]
struct RepoRef {
    path: String,
}

fn add_repo(app: &App, request: &RepoRef) -> Result<serde_json::Value> {
    let path = request.path.trim().trim_matches('"');
    if path.is_empty() {
        bail!("type the folder of a git repository");
    }
    Ok(json!({ "path": app.add_repo(Path::new(path))? }))
}

fn remove_repo(app: &App, request: &RepoRef) -> Result<serde_json::Value> {
    let repo = app.known_repo(&request.path)?;
    if lock(&app.live).iter().any(|live| live.repo == repo && live.phase != Phase::Error) {
        bail!("a task is still running in {}; wait for it or discard it first", repo.display());
    }
    let mut repos = lock(&app.repos);
    repos.retain(|known| known != &repo);
    app.save_repos(&repos);
    lock(&app.live).retain(|live| live.repo != repo);
    Ok(json!({ "path": repo }))
}

fn history(app: &App, request: &RepoRef) -> Result<serde_json::Value> {
    let repo = app.known_repo(&request.path)?;
    let filter = pact_coord::HistoryFilter { workspace_id: None, since: None, op_type: None, limit: Some(HISTORY_LIMIT) };
    Ok(serde_json::to_value(Orchestrator::open(repo)?.history(&filter)?)?)
}

async fn blocking<T: Send + 'static>(
    app: Arc<App>,
    input: T,
    work: impl FnOnce(&Arc<App>, T) -> Result<serde_json::Value> + Send + 'static,
) -> Response {
    match tokio::task::spawn_blocking(move || work(&app, input)).await {
        Ok(Ok(value)) => Json(value).into_response(),
        Ok(Err(err)) => (StatusCode::BAD_REQUEST, Json(json!({ "error": format!("{err:#}") }))).into_response(),
        Err(err) => {
            let body = Json(json!({ "error": format!("pact ui hit an internal error: {err}") }));
            (StatusCode::INTERNAL_SERVER_ERROR, body).into_response()
        }
    }
}

/// Refuses a request that did not come from the page this process
/// served: the `Host` check stops DNS rebinding, the token stops any
/// other site or local page from calling the API.
async fn guard(State(app): State<Arc<App>>, request: Request, next: Next) -> Response {
    let host = request.headers().get(header::HOST).and_then(|host| host.to_str().ok()).unwrap_or_default();
    if !app.hosts.iter().any(|allowed| allowed == host) {
        return (StatusCode::FORBIDDEN, "pact ui answers only at its own 127.0.0.1 address").into_response();
    }
    if request.uri().path().starts_with("/api/") {
        let token = request.headers().get("x-pact-token").map(|token| token.as_bytes()).unwrap_or_default();
        if !constant_time_eq(token, app.token.as_bytes()) {
            let body = Json(json!({ "error": "missing or wrong X-Pact-Token; reload the page pact ui printed" }));
            return (StatusCode::FORBIDDEN, body).into_response();
        }
    }
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    headers.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    response
}

async fn page(State(app): State<Arc<App>>) -> Response {
    let html = PAGE.replace("__PACT_TOKEN__", &app.token).replace("__PACT_NONCE__", &app.nonce);
    let policy = format!(
        "default-src 'none'; script-src 'nonce-{}'; style-src 'unsafe-inline'; connect-src 'self'; \
         img-src data:; base-uri 'none'; form-action 'none'; frame-ancestors 'none'",
        app.nonce
    );
    let mut response = Html(html).into_response();
    if let Ok(value) = HeaderValue::from_str(&policy) {
        response.headers_mut().insert(header::CONTENT_SECURITY_POLICY, value);
    }
    response
}

fn router(app: Arc<App>) -> Router {
    Router::new()
        .route("/", get(page))
        .route("/api/state", get(|State(app): State<Arc<App>>| blocking(app, (), |app, ()| state(app))))
        .route(
            "/api/tasks",
            post(|State(app): State<Arc<App>>, Json(body): Json<NewTask>| blocking(app, body, start_task)),
        )
        .route(
            "/api/reply",
            post(|State(app): State<Arc<App>>, Json(body): Json<ReplyRequest>| blocking(app, body, reply)),
        )
        .route(
            "/api/task",
            get(|State(app): State<Arc<App>>, Query(task): Query<TaskRef>| blocking(app, task, |app, task| task_detail(app, &task))),
        )
        .route(
            "/api/land",
            post(|State(app): State<Arc<App>>, Json(task): Json<TaskRef>| blocking(app, task, |app, task| land(app, &task))),
        )
        .route(
            "/api/discard",
            post(|State(app): State<Arc<App>>, Json(task): Json<TaskRef>| blocking(app, task, |app, task| discard(app, &task))),
        )
        .route(
            "/api/repos",
            post(|State(app): State<Arc<App>>, Json(repo): Json<RepoRef>| blocking(app, repo, |app, repo| add_repo(app, &repo))),
        )
        .route(
            "/api/repos/remove",
            post(|State(app): State<Arc<App>>, Json(repo): Json<RepoRef>| blocking(app, repo, |app, repo| remove_repo(app, &repo))),
        )
        .route(
            "/api/history",
            get(|State(app): State<Arc<App>>, Query(repo): Query<RepoRef>| blocking(app, repo, |app, repo| history(app, &repo))),
        )
        .layer(axum::middleware::from_fn_with_state(Arc::clone(&app), guard))
        .with_state(app)
}

fn repos_file() -> Option<PathBuf> {
    let dir = match std::env::var_os("PACT_UI_STATE_DIR") {
        Some(dir) => PathBuf::from(dir),
        None => dirs::data_local_dir()?.join("pact"),
    };
    Some(dir.join("ui-repos.json"))
}

fn open_in_browser(url: &str) {
    let mut command = if cfg!(windows) {
        let mut command = std::process::Command::new("cmd");
        command.args(["/C", "start", "", url]);
        command
    } else {
        let mut command = std::process::Command::new(if cfg!(target_os = "macos") { "open" } else { "xdg-open" });
        command.arg(url);
        command
    };
    let spawned = command.stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn();
    if let Err(err) = spawned {
        eprintln!("could not open a browser ({err}); open the address above yourself");
    }
}

pub fn run(args: UiArgs, repo_flag: Option<&Path>) -> Result<()> {
    let policy = AdmissionPolicy {
        max_concurrent: args.max_concurrent,
        min_free_mem_mb: args.min_free_mem_mb,
        stagger: std::time::Duration::from_millis(args.stagger_ms),
        per_lane_reserve_mb: args.per_lane_reserve_mb,
    };
    let repos_file = repos_file();
    let saved: Vec<PathBuf> = repos_file
        .as_deref()
        .and_then(|file| std::fs::read_to_string(file).ok())
        .and_then(|contents| serde_json::from_str(&contents).ok())
        .unwrap_or_default();

    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
    runtime.block_on(async move {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", args.port)).await.with_context(|| {
            format!(
                "could not listen on 127.0.0.1:{}; another pact ui may be running. Pass --port 0 for any free port.",
                args.port
            )
        })?;
        let port = listener.local_addr()?.port();
        let secret = || uuid::Uuid::new_v4().simple().to_string();
        let app = Arc::new(App {
            token: format!("{}{}", secret(), secret()),
            nonce: secret(),
            hosts: [format!("127.0.0.1:{port}"), format!("localhost:{port}")],
            runner: FleetRunner::new(policy),
            safety: args.safety,
            repos: Mutex::new(saved),
            repos_file,
            live: Mutex::new(Vec::new()),
            installed: Mutex::new(None),
        });

        match repo_flag {
            Some(repo) => {
                app.add_repo(repo).with_context(|| format!("--repo {}", repo.display()))?;
            }
            None => {
                if let Ok(cwd) = std::env::current_dir() {
                    let _ = app.add_repo(&cwd);
                }
            }
        }
        let probing = Arc::clone(&app);
        std::thread::spawn(move || {
            let found = crate::detect_installed_agents();
            *lock(&probing.installed) = Some(found);
        });

        let url = format!("http://127.0.0.1:{port}/");
        println!("pact ui: {url}");
        println!("Agents started from the page run unattended. Press Ctrl-C to stop pact ui and every agent it started.");
        if !args.no_open {
            open_in_browser(&url);
        }
        axum::serve(listener, router(app)).await.context("pact ui stopped serving")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slug_keeps_letters_digits_and_single_hyphens() {
        assert_eq!(slug("Fix the Login bug!", 32), "fix-the-login-bug");
        assert_eq!(slug("  --a   b--  ", 32), "a-b");
        assert_eq!(slug("日本語", 32), "");
        assert_eq!(slug("abcdef ghij", 7), "abcdef");
    }

    #[test]
    fn generated_name_is_the_first_words_and_four_hex_characters() {
        let name = generated_name("Add a dark mode toggle to the settings page");
        let (base, suffix) = name.rsplit_once('-').unwrap();
        assert_eq!(base, "add-a-dark-mode");
        assert_eq!(suffix.len(), 4);
        assert!(generated_name("!!!").starts_with("task-"));
        assert!(generated_name(&"x".repeat(100)).len() <= 32);
    }

    #[test]
    fn constant_time_eq_compares_whole_values() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
        assert!(!constant_time_eq(b"", b"abc"));
    }
}
