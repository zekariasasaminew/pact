use serde_json::Value;

use crate::adapter::{AgentAdapter, CoordConfig};
use crate::event::AgentEvent;

/// Live-verified against a real installed `codex` (codex-cli 0.144.3) --
/// see DESIGN.md ("pact-agents > Codex adapter").
pub struct CodexAdapter;

impl AgentAdapter for CodexAdapter {
    fn coord_server_name(&self) -> &'static str {
        "pact-coord"
    }

    /// See DESIGN.md ("pact-agents > Codex adapter").
    fn default_safety_description(&self) -> &'static str {
        "--dangerously-bypass-approvals-and-sandbox (can run any shell command and edit any file \
         with no restriction -- confirmed no safe alternative actually lets it write files at all \
         in headless mode; see issue #2's investigation)"
    }

    /// See DESIGN.md ("pact-agents > Codex adapter").
    fn build_command(
        &self,
        task: &str,
        safety_override: Option<&str>,
        coord: Option<&CoordConfig>,
        _workspace_path: &std::path::Path,
    ) -> (String, Vec<String>) {
        let mut args = vec!["exec".to_string(), task.to_string(), "--json".to_string()];
        match safety_override {
            Some(sandbox_mode) => {
                args.push("--sandbox".to_string());
                args.push(sandbox_mode.to_string());
            }
            None => args.push("--dangerously-bypass-approvals-and-sandbox".to_string()),
        }
        if let Some(coord) = coord {
            args.push("-c".to_string());
            args.push(format!(
                "mcp_servers.{}.command={}",
                coord.server_name,
                toml_string(&coord.command)
            ));
            args.push("-c".to_string());
            args.push(format!(
                "mcp_servers.{}.args={}",
                coord.server_name,
                toml_string_array(&coord.args)
            ));
        }
        ("codex".to_string(), args)
    }

    /// Schema modeled against real captured output -- see DESIGN.md
    /// ("pact-agents > Codex adapter") for the success-signal gap this
    /// adapter works around.
    fn parse_line(&self, line: &str) -> Vec<AgentEvent> {
        let value: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => return vec![AgentEvent::Other(Value::String(line.to_string()))],
        };

        match value.get("type").and_then(Value::as_str) {
            Some("thread.started") => {
                let session_id = value
                    .get("thread_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                vec![AgentEvent::Init { session_id }]
            }
            Some("item.completed") => parse_completed_item(&value),
            _ => vec![AgentEvent::Other(value)],
        }
    }
}

fn parse_completed_item(value: &Value) -> Vec<AgentEvent> {
    let Some(item) = value.get("item") else {
        return vec![AgentEvent::Other(value.clone())];
    };

    match item.get("type").and_then(Value::as_str) {
        Some("agent_message") => {
            let text = item
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            vec![AgentEvent::AssistantText(text)]
        }
        Some("file_change") => vec![AgentEvent::ToolUse {
            name: "file_change".to_string(),
            input: item
                .get("changes")
                .cloned()
                .unwrap_or(Value::Null),
        }],
        Some("command_execution") => vec![AgentEvent::ToolUse {
            name: "command_execution".to_string(),
            input: item.clone(),
        }],
        Some("mcp_tool_call") => {
            let server = item
                .get("server")
                .and_then(Value::as_str)
                .unwrap_or("unknown_server")
                .to_string();
            let tool = item
                .get("tool")
                .and_then(Value::as_str)
                .unwrap_or("unknown_tool")
                .to_string();
            let failed = !item.get("error").is_none_or(|e| e.is_null());
            vec![
                AgentEvent::ToolUse {
                    name: format!("mcp:{server}:{tool}"),
                    input: item.get("arguments").cloned().unwrap_or(Value::Null),
                },
                AgentEvent::CoordStatus {
                    name: server,
                    status: if failed { "failed" } else { "connected" }.to_string(),
                },
            ]
        }
        _ => vec![AgentEvent::Other(value.clone())],
    }
}

fn toml_string(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

fn toml_string_array(items: &[String]) -> String {
    let rendered: Vec<String> = items.iter().map(|s| toml_string(s)).collect();
    format!("[{}]", rendered.join(","))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_command_escapes_windows_paths_for_toml() {
        let coord = CoordConfig {
            server_name: "pact-coord".to_string(),
            command: r"C:\Users\test\pact.exe".to_string(),
            args: vec!["mcp-serve".to_string(), r"--workspace".to_string()],
            config_path: std::path::PathBuf::new(),
        };
        let (program, args) = CodexAdapter.build_command(
            "do the thing",
            None,
            Some(&coord),
            std::path::Path::new("/tmp/workspace"),
        );
        assert_eq!(program, "codex");

        let command_arg = args
            .iter()
            .find(|a| a.starts_with("mcp_servers.pact-coord.command="))
            .expect("command override present");
        assert_eq!(
            command_arg,
            r#"mcp_servers.pact-coord.command="C:\\Users\\test\\pact.exe""#
        );

        let args_arg = args
            .iter()
            .find(|a| a.starts_with("mcp_servers.pact-coord.args="))
            .expect("args override present");
        assert_eq!(
            args_arg,
            r#"mcp_servers.pact-coord.args=["mcp-serve","--workspace"]"#
        );
    }

    #[test]
    fn build_command_omits_mcp_overrides_when_no_coord_config() {
        let (_, args) = CodexAdapter.build_command(
            "do the thing",
            None,
            None,
            std::path::Path::new("/tmp/workspace"),
        );
        assert!(!args.iter().any(|a| a.contains("mcp_servers")));
    }

    #[test]
    fn default_safety_uses_bypass_flag() {
        let (_, args) = CodexAdapter.build_command(
            "do the thing",
            None,
            None,
            std::path::Path::new("/tmp/workspace"),
        );
        assert!(args.contains(&"--dangerously-bypass-approvals-and-sandbox".to_string()));
    }

    #[test]
    fn safety_override_maps_to_sandbox_flag() {
        let (_, args) = CodexAdapter.build_command(
            "do the thing",
            Some("read-only"),
            None,
            std::path::Path::new("/tmp/workspace"),
        );
        assert!(args.contains(&"--sandbox".to_string()));
        assert!(args.contains(&"read-only".to_string()));
        assert!(!args.contains(&"--dangerously-bypass-approvals-and-sandbox".to_string()));
    }

    #[test]
    fn parses_thread_started_as_init() {
        let events = CodexAdapter.parse_line(r#"{"type":"thread.started","thread_id":"abc123"}"#);
        assert!(matches!(&events[0], AgentEvent::Init { session_id } if session_id == "abc123"));
    }

    #[test]
    fn parses_mcp_tool_call_as_tool_use_and_coord_status() {
        let line = r#"{"type":"item.completed","item":{"id":"item_1","type":"mcp_tool_call","server":"pact-coord","tool":"claim_files","arguments":{"globs":["x.txt"]},"result":{},"error":null,"status":"completed"}}"#;
        let events = CodexAdapter.parse_line(line);
        assert_eq!(events.len(), 2);
        assert!(matches!(&events[0], AgentEvent::ToolUse { name, .. } if name == "mcp:pact-coord:claim_files"));
        assert!(matches!(&events[1], AgentEvent::CoordStatus { name, status } if name == "pact-coord" && status == "connected"));
    }
}
