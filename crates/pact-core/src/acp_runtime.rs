//! The ACP lane runtime (issue #331): one agent process per agent kind
//! hosting one Agent Client Protocol session per lane, with pact-coord
//! served once over HTTP for the whole batch.
//!
//! Measured motivation (issue #306): eight lanes as eight cold `copilot
//! -p` processes took 50.9 s and 2,456 MB to finish a trivial task; eight
//! ACP sessions in one `copilot --acp` took 5.6 s and 445 MB. Everything
//! above this module keeps its thread-per-lane shape: a lane thread calls
//! `AcpRuntime::prompt`, which blocks exactly as `run_and_stream` does,
//! and translates each `session/update` into the same `AgentEvent`s the
//! process runtime produces, so logs, run records, `list` and the
//! reconciliation summary need no second code path.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, bail, Context, Result};
use pact_acp::{AcpRuntime, AgentSpec, SessionUpdate};
use pact_agents::{AcpLaunchRequest, AgentEvent, AgentKind, LaunchSpec};
use pact_coord::http::{HttpCoordServer, LaneRoute};

/// Which way `spawn`/`spawn-many` run each lane's agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LaneRuntime {
    /// `Acp` when every agent in the batch has an ACP mode, else
    /// `Process` (issue #337). The default since benchmark arm Q matched
    /// Copilot's in-process sub-agents on time and beat them on memory,
    /// CPU and output (#308).
    #[default]
    Auto,
    /// One agent CLI process per lane, pact-coord as that process's own
    /// stdio MCP child. The original shape.
    Process,
    /// One agent process per agent kind, one ACP session per lane,
    /// pact-coord over HTTP from inside the orchestrating process.
    Acp,
}

impl LaneRuntime {
    pub fn parse(text: &str) -> Option<Self> {
        match text.trim().to_ascii_lowercase().as_str() {
            "auto" => Some(LaneRuntime::Auto),
            "process" => Some(LaneRuntime::Process),
            "acp" => Some(LaneRuntime::Acp),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            LaneRuntime::Auto => "auto",
            LaneRuntime::Process => "process",
            LaneRuntime::Acp => "acp",
        }
    }
}

impl std::fmt::Display for LaneRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The runtime a batch of `agents` actually runs under when `requested`
/// (issue #337): `Auto` resolves to `Acp` only when every agent has an
/// ACP mode, since a mixed batch cannot put a CLI without one into a
/// shared process; an explicit request is returned as is, and an empty
/// batch resolves to `Process`. Never returns `Auto`.
pub fn effective_runtime(requested: LaneRuntime, agents: &[AgentKind]) -> LaneRuntime {
    match requested {
        LaneRuntime::Auto => {
            if !agents.is_empty() && agents.iter().all(|&agent| pact_agents::adapter(agent).supports_acp()) {
                LaneRuntime::Acp
            } else {
                LaneRuntime::Process
            }
        }
        explicit => explicit,
    }
}

/// The in-process coordination server plus the tokio runtime it lives
/// on. pact-core is synchronous; this is the one place it owns a runtime.
pub(crate) struct CoordHttp {
    runtime: tokio::runtime::Runtime,
    server: HttpCoordServer,
}

impl CoordHttp {
    fn start(repo_root: &Path) -> Result<Self> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("pact-coord-http")
            .enable_all()
            .build()
            .context("starting the coordination server's runtime")?;
        let server = runtime
            .block_on(pact_coord::http::serve(repo_root, "127.0.0.1:0".parse().unwrap()))
            .context("starting the in-process coordination server")?;
        Ok(CoordHttp { runtime, server })
    }

    pub(crate) fn add_lane(&self, agent_id: &str, workspace_root: &Path) -> String {
        self.server.add_lane(LaneRoute { agent_id: agent_id.to_string(), workspace_root: workspace_root.to_path_buf() })
    }

    pub(crate) fn remove_lane(&self, agent_id: &str) {
        self.server.remove_lane(agent_id);
    }

    fn shutdown(self) {
        self.runtime.block_on(self.server.shutdown());
        self.runtime.shutdown_timeout(std::time::Duration::from_secs(2));
    }
}

/// One batch's shared agent processes and coordination server. Started
/// before any lane thread, shut down after the last one joins.
pub(crate) struct AcpBatch {
    pub(crate) coord: CoordHttp,
    runtimes: Mutex<HashMap<AgentKind, Arc<AcpRuntime>>>,
    /// Processes that died and were replaced (issue #412), kept so their
    /// tokio runtimes are shut down with the batch.
    retired: Mutex<Vec<Arc<AcpRuntime>>>,
    restarts: Mutex<HashMap<AgentKind, usize>>,
    launches: HashMap<AgentKind, LaunchSpec>,
    repo_root: PathBuf,
}

/// How many times one batch replaces a dead shared agent process before
/// the lanes still waiting fail instead (issue #412).
pub(crate) const MAX_ACP_RESTARTS: usize = 2;

fn launch_acp(repo_root: &Path, spec: &LaunchSpec, name: &str, on_event: &mut impl FnMut(&AgentEvent)) -> Result<AcpRuntime> {
    on_event(&AgentEvent::Phase(format!("starting one {name} process for the batch (ACP)")));
    let started = std::time::Instant::now();
    let runtime = AcpRuntime::start_unattended(AgentSpec {
        program: spec.program.clone(),
        args: spec.args.clone(),
        env: spec.env.clone(),
        cwd: Some(repo_root.to_path_buf()),
    })
    .map_err(|err| anyhow!("starting `{name} --acp`: {err}"))?;
    let info = runtime.initialize_result();
    on_event(&AgentEvent::Phase(format!(
        "{name} ACP server up in {:.1}s ({} {}, pid {})",
        started.elapsed().as_secs_f32(),
        info.agent_info.as_ref().map(|a| a.name.as_str()).unwrap_or(name),
        info.agent_info.as_ref().map(|a| a.version.as_str()).unwrap_or(""),
        runtime.pid()
    )));
    Ok(runtime)
}

impl AcpBatch {
    /// Starts the coordination server and one ACP process per agent kind
    /// in `agents`, each with its own lean home under `homes_dir`. Fails
    /// as a whole if any agent has no ACP mode or cannot be started, so
    /// the caller reports one batch-level error rather than N.
    pub(crate) fn start(
        repo_root: &Path,
        homes_dir: &Path,
        agents: &[AgentKind],
        lean: bool,
        on_event: &mut impl FnMut(&AgentEvent),
    ) -> Result<Self> {
        let coord = CoordHttp::start(repo_root)?;
        let mut runtimes = HashMap::new();
        let mut launches = HashMap::new();
        let batch_tag = uuid::Uuid::new_v4().to_string()[..8].to_string();
        for &agent in agents {
            if runtimes.contains_key(&agent) {
                continue;
            }
            let name = crate::agent_kind_name(agent);
            let adapter = pact_agents::adapter(agent);
            let home: PathBuf = homes_dir.join(format!("acp-{name}-{batch_tag}"));
            let spec = adapter
                .build_acp_launch(&AcpLaunchRequest { agent_home: &home, lean })
                .ok_or_else(|| anyhow!("agent `{name}` has no Agent Client Protocol mode; use --runtime process"))?;
            let (program, mut args) = pact_agents::resolve_program(&spec.program);
            args.extend(spec.args.iter().cloned());
            let launch = LaunchSpec { program, args, env: spec.env };
            let runtime = launch_acp(repo_root, &launch, name, on_event)?;
            runtimes.insert(agent, Arc::new(runtime));
            launches.insert(agent, launch);
        }
        Ok(AcpBatch {
            coord,
            runtimes: Mutex::new(runtimes),
            retired: Mutex::new(Vec::new()),
            restarts: Mutex::new(HashMap::new()),
            launches,
            repo_root: repo_root.to_path_buf(),
        })
    }

    pub(crate) fn runtime(&self, agent: AgentKind) -> Option<Arc<AcpRuntime>> {
        lock(&self.runtimes).get(&agent).cloned()
    }

    /// `agent`'s process, replaced first if it has exited (issue #412).
    /// Lanes call this before opening a session, so a lane queued behind
    /// a crash starts on a fresh process; after [`MAX_ACP_RESTARTS`]
    /// replacements it fails instead and says why.
    pub(crate) fn live_runtime(&self, agent: AgentKind, on_event: &mut impl FnMut(&AgentEvent)) -> Result<Arc<AcpRuntime>> {
        let name = crate::agent_kind_name(agent);
        let mut runtimes = lock(&self.runtimes);
        let current = runtimes.get(&agent).cloned().ok_or_else(|| anyhow!("no ACP process was started for agent {name}"))?;
        let Some(exit) = current.exit_info() else { return Ok(current) };
        let mut restarts = lock(&self.restarts);
        let count = restarts.entry(agent).or_insert(0);
        if *count >= MAX_ACP_RESTARTS {
            bail!("the shared {name} process exited ({}) and was already restarted {count} time(s), the most a batch allows; not starting it again", exit.status);
        }
        *count += 1;
        let launch = self.launches.get(&agent).ok_or_else(|| anyhow!("no launch recorded for {name}"))?;
        on_event(&AgentEvent::Phase(format!("the shared {name} process (pid {}) exited ({}); restarting it ({count}/{MAX_ACP_RESTARTS})", current.pid(), exit.status)));
        let fresh = Arc::new(launch_acp(&self.repo_root, launch, name, on_event)?);
        runtimes.insert(agent, Arc::clone(&fresh));
        lock(&self.retired).push(current);
        Ok(fresh)
    }

    /// How many times each agent's process was replaced in this batch.
    pub(crate) fn restart_count(&self) -> usize {
        lock(&self.restarts).values().sum()
    }

    /// Checks that `agent`'s process offers `model` by opening a throwaway
    /// session and selecting it, so a bad model fails the batch before any
    /// lane launches (issue #374).
    pub(crate) fn check_model(&self, agent: AgentKind, cwd: &Path, model: &str) -> Result<()> {
        let runtime = self.runtime(agent).ok_or_else(|| anyhow!("no ACP process for {}", crate::agent_kind_name(agent)))?;
        let mut session = runtime.new_session(cwd, Vec::new()).map_err(|err| anyhow!("opening a session to check the model: {err}"))?;
        let selected = select_model(&runtime, &mut session, model, agent);
        let _ = runtime.close(&session);
        selected.map(|_| ())
    }

    /// The resolved launch of the process hosting `agent`'s lanes, for
    /// the run record.
    pub(crate) fn launch(&self, agent: AgentKind) -> Option<&LaunchSpec> {
        self.launches.get(&agent)
    }

    pub(crate) fn shutdown(self) {
        let runtimes = self.runtimes.into_inner().unwrap_or_else(|poisoned| poisoned.into_inner());
        let retired = self.retired.into_inner().unwrap_or_else(|poisoned| poisoned.into_inner());
        for runtime in runtimes.into_values().chain(retired) {
            // Every lane thread has joined, so this is the last handle.
            if let Ok(runtime) = Arc::try_unwrap(runtime) {
                runtime.shutdown();
            }
        }
        self.coord.shutdown();
    }
}

/// Puts `session` on `model` (issue #374), or says why it cannot: the
/// agent offers no model selector, or does not offer this model. Returns
/// the model the session reports afterwards.
pub(crate) fn select_model(runtime: &AcpRuntime, session: &mut pact_acp::LaneSession, model: &str, agent: AgentKind) -> Result<Option<String>> {
    let name = crate::agent_kind_name(agent);
    let option_id = pact_acp::model_option(&session.config_options)
        .and_then(|option| option.get("id").and_then(|v| v.as_str()))
        .map(str::to_string)
        .ok_or_else(|| anyhow!("{name}'s ACP session offers no model selector, so model '{model}' cannot be set; use --runtime process"))?;
    let offered = pact_acp::offered_models(&session.config_options);
    if !offered.iter().any(|m| m == model) {
        bail!("model '{model}' is not offered by {name} (offered: {})", offered.join(", "));
    }
    runtime.set_config_option(session, &option_id, model).map_err(|err| anyhow!("setting model '{model}': {err}"))?;
    Ok(pact_acp::current_model(&session.config_options))
}

/// The model a `config_option_update` says the session switched to.
pub(crate) fn model_in_update(update: &SessionUpdate) -> Option<String> {
    if update.kind != "config_option_update" {
        return None;
    }
    let options = update.raw.get("configOptions")?.as_array()?;
    pact_acp::current_model(options)
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The `AgentEvent` a `session/update` stands for, so the ACP path feeds
/// the same per-lane log, status and summary machinery as the process
/// path. Everything unmodelled is `Other`, never dropped; its `type` is
/// `acp.<sessionUpdate>` so the CLI's existing noise suppression can key
/// on it (issue #339).
pub(crate) fn event_for_update(update: &SessionUpdate) -> AgentEvent {
    match update.kind.as_str() {
        "agent_message_chunk" => AgentEvent::AssistantText(update.text().unwrap_or("").to_string()),
        "tool_call" => AgentEvent::ToolUse {
            name: update
                .raw
                .get("kind")
                .and_then(|v| v.as_str())
                .map(|kind| format!("{kind}: {}", update.tool_title().unwrap_or("")))
                .unwrap_or_else(|| update.tool_title().unwrap_or("tool").to_string()),
            input: update.raw.get("rawInput").cloned().unwrap_or(serde_json::Value::Null),
        },
        kind => AgentEvent::Other(serde_json::json!({
            "type": format!("acp.{kind}"),
            "sessionId": update.session_id,
            "update": update.raw,
        })),
    }
}

/// One line of an ACP lane or planner log: the raw `session/update`
/// with the session it belongs to and the wall-clock it arrived at in
/// Unix milliseconds (issue #358), so cadence can be read from pact's
/// own files instead of the agent's.
pub(crate) fn log_line(update: &SessionUpdate) -> serde_json::Value {
    serde_json::json!({ "t": unix_millis(), "sessionId": update.session_id, "update": update.raw })
}

/// Unix milliseconds now, for log lines.
pub(crate) fn unix_millis() -> u128 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0)
}

/// Joins streamed `agent_message_chunk`s into whole messages (issue
/// #339): the agent sends a sentence as several fragments, and printing
/// each as its own `[assistant]` line made the stream unreadable. Text
/// accumulates until an update of any other kind arrives, or the turn
/// ends, and is then emitted once.
pub(crate) struct ChunkCoalescer {
    pending: String,
}

impl ChunkCoalescer {
    pub(crate) fn new() -> Self {
        ChunkCoalescer { pending: String::new() }
    }

    /// Feeds one update; calls `emit` for every event that is ready.
    pub(crate) fn push(&mut self, update: &SessionUpdate, emit: &mut impl FnMut(&AgentEvent)) {
        if update.kind == "agent_message_chunk" {
            self.pending.push_str(update.text().unwrap_or(""));
            return;
        }
        self.flush(emit);
        emit(&event_for_update(update));
    }

    pub(crate) fn flush(&mut self, emit: &mut impl FnMut(&AgentEvent)) {
        if !self.pending.is_empty() {
            let text = std::mem::take(&mut self.pending);
            emit(&AgentEvent::AssistantText(text));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lane_runtime_parses_case_insensitively_and_defaults_to_auto() {
        assert_eq!(LaneRuntime::parse("ACP"), Some(LaneRuntime::Acp));
        assert_eq!(LaneRuntime::parse(" process "), Some(LaneRuntime::Process));
        assert_eq!(LaneRuntime::parse("auto"), Some(LaneRuntime::Auto));
        assert_eq!(LaneRuntime::parse("threads"), None);
        assert_eq!(LaneRuntime::default(), LaneRuntime::Auto);
        assert_eq!(LaneRuntime::Acp.to_string(), "acp");
    }

    #[test]
    fn auto_resolves_to_acp_only_when_every_agent_in_the_batch_supports_it() {
        assert_eq!(effective_runtime(LaneRuntime::Auto, &[AgentKind::Copilot, AgentKind::Copilot]), LaneRuntime::Acp);
        assert_eq!(effective_runtime(LaneRuntime::Auto, &[AgentKind::Claude]), LaneRuntime::Process);
        assert_eq!(effective_runtime(LaneRuntime::Auto, &[AgentKind::Copilot, AgentKind::Claude]), LaneRuntime::Process, "a mixed batch cannot share a process");
        assert_eq!(effective_runtime(LaneRuntime::Auto, &[]), LaneRuntime::Process);
        assert_eq!(effective_runtime(LaneRuntime::Process, &[AgentKind::Copilot]), LaneRuntime::Process, "explicit wins");
        assert_eq!(effective_runtime(LaneRuntime::Acp, &[AgentKind::Claude]), LaneRuntime::Acp, "explicit is passed through; the batch start reports the unsupported agent");
    }

    #[test]
    fn updates_map_onto_the_existing_event_model() {
        let chunk = SessionUpdate {
            session_id: "s".into(),
            kind: "agent_message_chunk".into(),
            raw: serde_json::json!({ "sessionUpdate": "agent_message_chunk", "content": { "type": "text", "text": "DONE" } }),
        };
        assert!(matches!(event_for_update(&chunk), AgentEvent::AssistantText(t) if t == "DONE"));

        let tool = SessionUpdate {
            session_id: "s".into(),
            kind: "tool_call".into(),
            raw: serde_json::json!({ "sessionUpdate": "tool_call", "kind": "edit", "title": "Creating a.txt", "rawInput": { "path": "a.txt" } }),
        };
        match event_for_update(&tool) {
            AgentEvent::ToolUse { name, input } => {
                assert_eq!(name, "edit: Creating a.txt");
                assert_eq!(input["path"], "a.txt");
            }
            other => panic!("expected ToolUse, got {other:?}"),
        }

        let usage = SessionUpdate {
            session_id: "s".into(),
            kind: "usage_update".into(),
            raw: serde_json::json!({ "sessionUpdate": "usage_update", "used": 10, "size": 1000 }),
        };
        match event_for_update(&usage) {
            AgentEvent::Other(value) => {
                assert_eq!(value["update"]["used"], 10, "unmodelled updates survive as Other");
                assert_eq!(value["type"], "acp.usage_update", "typed so the CLI's suppression list can name it");
            }
            other => panic!("expected Other, got {other:?}"),
        }
    }

    #[test]
    fn chunks_coalesce_into_one_message_flushed_by_the_next_kind_or_the_end_of_turn() {
        let chunk = |text: &str| SessionUpdate {
            session_id: "s".into(),
            kind: "agent_message_chunk".into(),
            raw: serde_json::json!({ "sessionUpdate": "agent_message_chunk", "content": { "type": "text", "text": text } }),
        };
        let usage = SessionUpdate {
            session_id: "s".into(),
            kind: "usage_update".into(),
            raw: serde_json::json!({ "sessionUpdate": "usage_update", "used": 1, "size": 10 }),
        };
        let seen: std::cell::RefCell<Vec<String>> = std::cell::RefCell::new(Vec::new());
        let mut record = |event: &AgentEvent| {
            seen.borrow_mut().push(match event {
                AgentEvent::AssistantText(t) => format!("text:{t}"),
                AgentEvent::Other(v) => format!("other:{}", v["type"].as_str().unwrap_or("")),
                other => format!("{other:?}"),
            })
        };
        let mut coalescer = ChunkCoalescer::new();
        coalescer.push(&chunk("Sources read. "), &mut record);
        coalescer.push(&chunk("Writing the tests."), &mut record);
        assert!(seen.borrow().is_empty(), "nothing is emitted while chunks keep arriving");
        coalescer.push(&usage, &mut record);
        assert_eq!(*seen.borrow(), vec!["text:Sources read. Writing the tests.", "other:acp.usage_update"], "the message flushes whole, before the update that ended it");
        coalescer.push(&chunk("DONE"), &mut record);
        coalescer.flush(&mut record);
        coalescer.flush(&mut record);
        assert_eq!(seen.borrow().last().map(String::as_str), Some("text:DONE"));
        assert_eq!(seen.borrow().len(), 3, "an empty flush emits nothing");
    }
}
