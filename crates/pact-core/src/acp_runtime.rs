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
//! process runtime produces, so logs, `-run.json`, `list` and the
//! reconciliation summary need no second code path.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use pact_acp::{AcpRuntime, AgentSpec, SessionUpdate};
use pact_agents::{AcpLaunchRequest, AgentEvent, AgentKind, LaunchSpec};
use pact_coord::http::{HttpCoordServer, LaneRoute};

/// Which way `spawn`/`spawn-many` run each lane's agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LaneRuntime {
    /// One agent CLI process per lane, pact-coord as that process's own
    /// stdio MCP child. The original shape; still the default.
    #[default]
    Process,
    /// One agent process per agent kind, one ACP session per lane,
    /// pact-coord over HTTP from inside the orchestrating process.
    Acp,
}

impl LaneRuntime {
    pub fn parse(text: &str) -> Option<Self> {
        match text.trim().to_ascii_lowercase().as_str() {
            "process" => Some(LaneRuntime::Process),
            "acp" => Some(LaneRuntime::Acp),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
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
    runtimes: HashMap<AgentKind, AcpRuntime>,
    launches: HashMap<AgentKind, LaunchSpec>,
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
            on_event(&AgentEvent::Phase(format!("starting one {name} process for the batch (ACP)")));
            let started = std::time::Instant::now();
            let runtime = AcpRuntime::start_unattended(AgentSpec {
                program: program.clone(),
                args: args.clone(),
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
            runtimes.insert(agent, runtime);
            launches.insert(agent, LaunchSpec { program, args, env: spec.env });
        }
        Ok(AcpBatch { coord, runtimes, launches })
    }

    pub(crate) fn runtime(&self, agent: AgentKind) -> Option<&AcpRuntime> {
        self.runtimes.get(&agent)
    }

    /// The resolved launch of the process hosting `agent`'s lanes, for
    /// the run record.
    pub(crate) fn launch(&self, agent: AgentKind) -> Option<&LaunchSpec> {
        self.launches.get(&agent)
    }

    pub(crate) fn shutdown(self) {
        for (_, runtime) in self.runtimes {
            runtime.shutdown();
        }
        self.coord.shutdown();
    }
}

/// The `AgentEvent` a `session/update` stands for, so the ACP path feeds
/// the same per-lane log, status and summary machinery as the process
/// path. Everything unmodelled is `Other`, never dropped.
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
        _ => AgentEvent::Other(serde_json::json!({ "sessionId": update.session_id, "update": update.raw })),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lane_runtime_parses_case_insensitively_and_defaults_to_process() {
        assert_eq!(LaneRuntime::parse("ACP"), Some(LaneRuntime::Acp));
        assert_eq!(LaneRuntime::parse(" process "), Some(LaneRuntime::Process));
        assert_eq!(LaneRuntime::parse("threads"), None);
        assert_eq!(LaneRuntime::default(), LaneRuntime::Process);
        assert_eq!(LaneRuntime::Acp.to_string(), "acp");
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
            AgentEvent::Other(value) => assert_eq!(value["update"]["used"], 10, "unmodelled updates survive as Other"),
            other => panic!("expected Other, got {other:?}"),
        }
    }
}
