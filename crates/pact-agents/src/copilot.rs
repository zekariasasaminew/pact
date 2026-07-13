use serde_json::Value;

use crate::adapter::{AgentAdapter, CoordConfig};
use crate::event::AgentEvent;

pub struct CopilotAdapter;

impl AgentAdapter for CopilotAdapter {
    fn coord_server_name(&self) -> &'static str {
        "pact-coord"
    }

    fn default_safety_description(&self) -> &'static str {
        "--allow-all-tools"
    }

    /// `--allow-all-tools` has no gradient (unlike Claude Code's six
    /// permission modes) -- Copilot CLI's own `--help` states it's
    /// "required for non-interactive mode", so `safety_override` has
    /// nothing meaningful to override here; it's accepted for interface
    /// consistency but ignored.
    ///
    /// `mcp_config` is rendered to the same `{"mcpServers": {...}}` shape
    /// Claude Code uses (confirmed identical), passed via
    /// `--additional-mcp-config @<path>` -- the `@` prefix means "load
    /// from file" per Copilot CLI's own docs; without it the argument
    /// would be parsed as an inline JSON string instead.
    fn build_command(
        &self,
        task: &str,
        _safety_override: Option<&str>,
        coord: Option<&CoordConfig>,
    ) -> (String, Vec<String>) {
        let mut args = vec![
            "-p".to_string(),
            task.to_string(),
            "--output-format".to_string(),
            "json".to_string(),
            "--allow-all-tools".to_string(),
        ];
        if let Some(coord) = coord {
            if crate::adapter::write_mcp_json_config(&coord.config_path, coord).is_ok() {
                args.push("--additional-mcp-config".to_string());
                args.push(format!("@{}", coord.config_path.to_string_lossy()));
            } else {
                tracing::warn!(
                    "failed to write MCP config to {}; launching without coordination",
                    coord.config_path.display()
                );
            }
        }
        ("copilot".to_string(), args)
    }

    fn parse_line(&self, line: &str) -> Vec<AgentEvent> {
        parse_line(line)
    }
}

/// Schema modeled directly against real output captured from
/// `copilot -p ... --output-format json` (see README), including a real
/// tool-call-forcing task to confirm `toolRequests`' field names --
/// `name`/`arguments`, not Claude Code's `name`/`input`.
fn parse_line(line: &str) -> Vec<AgentEvent> {
    let value: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(_) => return vec![AgentEvent::Other(Value::String(line.to_string()))],
    };

    match value.get("type").and_then(Value::as_str) {
        Some("session.mcp_server_status_changed") => {
            let data = value.get("data");
            match (
                data.and_then(|d| d.get("serverName")).and_then(Value::as_str),
                data.and_then(|d| d.get("status")).and_then(Value::as_str),
            ) {
                (Some(name), Some(status)) => vec![AgentEvent::CoordStatus {
                    name: name.to_string(),
                    status: status.to_string(),
                }],
                _ => vec![AgentEvent::Other(value)],
            }
        }
        Some("session.mcp_servers_loaded") => value
            .get("data")
            .and_then(|d| d.get("servers"))
            .and_then(Value::as_array)
            .map(|servers| {
                servers
                    .iter()
                    .filter_map(|s| {
                        let name = s.get("name")?.as_str()?.to_string();
                        let status = s.get("status")?.as_str()?.to_string();
                        Some(AgentEvent::CoordStatus { name, status })
                    })
                    .collect()
            })
            .unwrap_or_else(|| vec![AgentEvent::Other(value.clone())]),
        Some("assistant.message") => parse_assistant_message(&value),
        Some("result") => {
            let exit_code = value.get("exitCode").and_then(Value::as_i64).unwrap_or(-1);
            vec![AgentEvent::Result {
                success: exit_code == 0,
                summary: format!("exit code {exit_code}"),
            }]
        }
        _ => vec![AgentEvent::Other(value)],
    }
}

/// Unlike Claude Code (one content block per line), Copilot CLI can bundle
/// response text *and* one or more tool calls into a single
/// `assistant.message` event -- confirmed directly: a file-writing task
/// produced one line with non-empty `content` alongside a non-empty
/// `toolRequests` array. Returning a `Vec` here is what makes that safe to
/// represent without dropping either half.
fn parse_assistant_message(value: &Value) -> Vec<AgentEvent> {
    let data = match value.get("data") {
        Some(d) => d,
        None => return vec![AgentEvent::Other(value.clone())],
    };

    let mut events = Vec::new();

    if let Some(text) = data.get("content").and_then(Value::as_str) {
        if !text.is_empty() {
            events.push(AgentEvent::AssistantText(text.to_string()));
        }
    }

    if let Some(requests) = data.get("toolRequests").and_then(Value::as_array) {
        for request in requests {
            let name = request
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("unknown_tool")
                .to_string();
            let input = request.get("arguments").cloned().unwrap_or(Value::Null);
            events.push(AgentEvent::ToolUse { name, input });
        }
    }

    if events.is_empty() {
        events.push(AgentEvent::Other(value.clone()));
    }
    events
}
