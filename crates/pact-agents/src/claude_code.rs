use serde_json::Value;

use crate::adapter::{AgentAdapter, CoordConfig};
use crate::event::AgentEvent;

pub struct ClaudeCodeAdapter;

/// Common safe operations covering every ecosystem `pact-deps` already
/// knows how to prepare -- see DESIGN.md ("pact-agents > Claude Code
/// safety default").
const DEFAULT_ALLOWED_TOOLS: &str =
    "Read Write Edit Glob Grep Bash(git *) Bash(npm *) Bash(pnpm *) Bash(yarn *) Bash(cargo *) Bash(go *) Bash(pip *) Bash(uv *) Bash(mvn *) Bash(gradle *)";

impl AgentAdapter for ClaudeCodeAdapter {
    fn coord_server_name(&self) -> &'static str {
        "pact-coord"
    }

    /// See DESIGN.md ("pact-agents > Claude Code safety default").
    fn default_safety_description(&self) -> &'static str {
        "--allowedTools (curated safe operations, no full permission bypass)"
    }

    /// See DESIGN.md ("pact-agents > Claude Code safety default").
    fn build_command(
        &self,
        task: &str,
        safety_override: Option<&str>,
        coord: Option<&CoordConfig>,
        _workspace_path: &std::path::Path,
    ) -> (String, Vec<String>) {
        let mut args = vec![
            "-p".to_string(),
            task.to_string(),
            "--output-format".to_string(),
            "stream-json".to_string(),
            "--verbose".to_string(),
            "--allowedTools".to_string(),
            DEFAULT_ALLOWED_TOOLS.to_string(),
        ];
        if let Some(mode) = safety_override {
            args.push("--permission-mode".to_string());
            args.push(mode.to_string());
        }
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

/// Schema modeled against real captured output -- see DESIGN.md
/// ("pact-agents > Claude Code output schema").
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_omits_permission_mode_but_includes_allowlist() {
        let (program, args) = ClaudeCodeAdapter.build_command(
            "do the thing",
            None,
            None,
            std::path::Path::new("/tmp/workspace"),
        );
        assert_eq!(program, "claude");
        assert!(args.contains(&"--allowedTools".to_string()));
        assert!(!args.contains(&"--permission-mode".to_string()));
    }

    #[test]
    fn override_adds_explicit_permission_mode_alongside_allowlist() {
        let (_, args) = ClaudeCodeAdapter.build_command(
            "do the thing",
            Some("bypassPermissions"),
            None,
            std::path::Path::new("/tmp/workspace"),
        );
        assert!(args.contains(&"--allowedTools".to_string()));
        let mode_idx = args.iter().position(|a| a == "--permission-mode").unwrap();
        assert_eq!(args[mode_idx + 1], "bypassPermissions");
    }
}
