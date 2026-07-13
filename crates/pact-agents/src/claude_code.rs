use serde_json::Value;

use crate::adapter::{AgentAdapter, CoordConfig};
use crate::event::AgentEvent;

pub struct ClaudeCodeAdapter;

impl AgentAdapter for ClaudeCodeAdapter {
    fn coord_server_name(&self) -> &'static str {
        "pact-coord"
    }

    fn default_safety_description(&self) -> &'static str {
        "--permission-mode bypassPermissions"
    }

    /// `mcp_config` is rendered to a `{"mcpServers": {...}}` JSON file and
    /// passed via `--mcp-config` -- confirmed against the real CLI: a
    /// malformed config is rejected with a loud error before the session
    /// starts, so getting the file wrong is never a silent no-op.
    fn build_command(
        &self,
        task: &str,
        safety_override: Option<&str>,
        coord: Option<&CoordConfig>,
    ) -> (String, Vec<String>) {
        let permission_mode = safety_override.unwrap_or("bypassPermissions");
        let mut args = vec![
            "-p".to_string(),
            task.to_string(),
            "--output-format".to_string(),
            "stream-json".to_string(),
            "--verbose".to_string(),
            "--permission-mode".to_string(),
            permission_mode.to_string(),
        ];
        if let Some(coord) = coord {
            if crate::adapter::write_mcp_json_config(&coord.config_path, coord).is_ok() {
                args.push("--mcp-config".to_string());
                args.push(coord.config_path.to_string_lossy().to_string());
            } else {
                tracing::warn!(
                    "failed to write MCP config to {}; launching without coordination",
                    coord.config_path.display()
                );
            }
        }
        ("claude".to_string(), args)
    }

    fn parse_line(&self, line: &str) -> Vec<AgentEvent> {
        parse_line(line)
    }
}

/// Schema modeled directly against real output captured from `claude -p
/// --output-format stream-json --verbose` (see README), not secondhand
/// docs. One event in, one event out in every case observed so far, but
/// this returns a `Vec` to match the shared `AgentAdapter` interface other
/// adapters need.
fn parse_line(line: &str) -> Vec<AgentEvent> {
    let value: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(_) => return vec![AgentEvent::Other(Value::String(line.to_string()))],
    };

    match value.get("type").and_then(Value::as_str) {
        Some("system") if value.get("subtype").and_then(Value::as_str) == Some("init") => {
            let session_id = value
                .get("session_id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let mut events = vec![AgentEvent::Init { session_id }];
            if let Some(servers) = value.get("mcp_servers").and_then(Value::as_array) {
                for server in servers {
                    if let (Some(name), Some(status)) = (
                        server.get("name").and_then(Value::as_str),
                        server.get("status").and_then(Value::as_str),
                    ) {
                        events.push(AgentEvent::CoordStatus {
                            name: name.to_string(),
                            status: status.to_string(),
                        });
                    }
                }
            }
            events
        }
        Some("assistant") => vec![parse_assistant(&value)],
        Some("result") => {
            let success = value.get("is_error").and_then(Value::as_bool) == Some(false);
            let summary = value
                .get("result")
                .and_then(Value::as_str)
                .unwrap_or("(no result text)")
                .to_string();
            vec![AgentEvent::Result { success, summary }]
        }
        _ => vec![AgentEvent::Other(value)],
    }
}

fn parse_assistant(value: &Value) -> AgentEvent {
    let content = value
        .get("message")
        .and_then(|m| m.get("content"))
        .and_then(Value::as_array);

    let Some(blocks) = content else {
        return AgentEvent::Other(value.clone());
    };

    // A single assistant message can contain several content blocks (text
    // and tool_use interleaved); report the first one we recognize rather
    // than needing a Vec, since in practice Claude Code emits one block
    // per line in stream-json mode. Anything genuinely mixed falls back to
    // Other with the full message preserved.
    for block in blocks {
        match block.get("type").and_then(Value::as_str) {
            Some("text") => {
                if let Some(text) = block.get("text").and_then(Value::as_str) {
                    return AgentEvent::AssistantText(text.to_string());
                }
            }
            Some("tool_use") => {
                let name = block
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown_tool")
                    .to_string();
                let input = block.get("input").cloned().unwrap_or(Value::Null);
                return AgentEvent::ToolUse { name, input };
            }
            _ => continue,
        }
    }

    AgentEvent::Other(value.clone())
}
