use serde_json::Value;

use crate::adapter::{AgentAdapter, CoordConfig};
use crate::event::AgentEvent;

/// Built from a real installed `gemini` CLI, **not live-verified against a
/// real authenticated session** -- see DESIGN.md ("pact-agents > Gemini
/// adapter", issue #9).
pub struct GeminiAdapter;

impl AgentAdapter for GeminiAdapter {
    fn coord_server_name(&self) -> &'static str {
        "pact-coord"
    }

    /// See DESIGN.md ("pact-agents > Gemini adapter").
    fn default_safety_description(&self) -> &'static str {
        "--approval-mode yolo (can run any shell command and edit any file with no restriction -- \
         unconfirmed whether a safer mode hangs in headless mode without real auth to test against; \
         see issue #9)"
    }

    /// See DESIGN.md ("pact-agents > Gemini adapter") for the MCP config
    /// mechanism, which differs from every other adapter.
    fn build_command(
        &self,
        task: &str,
        safety_override: Option<&str>,
        coord: Option<&CoordConfig>,
        workspace_path: &std::path::Path,
    ) -> (String, Vec<String>) {
        let mut args = vec![
            "-p".to_string(),
            task.to_string(),
            "-o".to_string(),
            "stream-json".to_string(),
        ];
        args.push("--approval-mode".to_string());
        args.push(safety_override.unwrap_or("yolo").to_string());

        if let Some(coord) = coord {
            let settings_path = workspace_path.join(".gemini").join("settings.json");
            if crate::adapter::write_mcp_json_config(&settings_path, coord).is_err() {
                tracing::warn!(
                    "failed to write MCP config to {}; launching without coordination",
                    settings_path.display()
                );
            }
        }

        ("gemini".to_string(), args)
    }

    /// Inferred, not confirmed -- see DESIGN.md ("pact-agents > Gemini
    /// adapter").
    fn parse_line(&self, line: &str) -> Vec<AgentEvent> {
        let value: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => return vec![AgentEvent::Other(Value::String(line.to_string()))],
        };

        match value.get("type").and_then(Value::as_str) {
            Some("session_started") | Some("init") => {
                let session_id = value
                    .get("session_id")
                    .or_else(|| value.get("sessionId"))
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                vec![AgentEvent::Init { session_id }]
            }
            Some("assistant_message") | Some("text") => {
                let text = value
                    .get("text")
                    .or_else(|| value.get("content"))
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                vec![AgentEvent::AssistantText(text)]
            }
            Some("tool_call") => {
                let name = value
                    .get("name")
                    .or_else(|| value.get("tool"))
                    .and_then(Value::as_str)
                    .unwrap_or("unknown_tool")
                    .to_string();
                let input = value
                    .get("args")
                    .or_else(|| value.get("arguments"))
                    .cloned()
                    .unwrap_or(Value::Null);
                vec![AgentEvent::ToolUse { name, input }]
            }
            Some("result") | Some("turn_complete") => {
                let success = value
                    .get("success")
                    .and_then(Value::as_bool)
                    .or_else(|| value.get("error").map(|e| e.is_null()))
                    .unwrap_or(true);
                let summary = value
                    .get("summary")
                    .or_else(|| value.get("text"))
                    .and_then(Value::as_str)
                    .unwrap_or("(no result text)")
                    .to_string();
                vec![AgentEvent::Result { success, summary }]
            }
            _ => vec![AgentEvent::Other(value)],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_safety_is_yolo() {
        let (program, args) = GeminiAdapter.build_command(
            "do the thing",
            None,
            None,
            std::path::Path::new("/tmp/workspace"),
        );
        assert_eq!(program, "gemini");
        let idx = args.iter().position(|a| a == "--approval-mode").unwrap();
        assert_eq!(args[idx + 1], "yolo");
    }

    #[test]
    fn safety_override_is_passed_through_raw() {
        let (_, args) = GeminiAdapter.build_command(
            "do the thing",
            Some("plan"),
            None,
            std::path::Path::new("/tmp/workspace"),
        );
        let idx = args.iter().position(|a| a == "--approval-mode").unwrap();
        assert_eq!(args[idx + 1], "plan");
    }

    #[test]
    fn mcp_config_written_under_workspace_dot_gemini_dir() {
        let dir = std::env::temp_dir().join(format!("pact-gemini-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let coord = CoordConfig {
            server_name: "pact-coord".to_string(),
            command: "pact".to_string(),
            args: vec!["mcp-serve".to_string()],
            config_path: dir.join("unused-elsewhere.json"),
        };
        GeminiAdapter.build_command("do the thing", None, Some(&coord), &dir);
        let written = dir.join(".gemini").join("settings.json");
        assert!(written.exists(), "expected {} to exist", written.display());
        let contents = std::fs::read_to_string(&written).unwrap();
        assert!(contents.contains("pact-coord"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
