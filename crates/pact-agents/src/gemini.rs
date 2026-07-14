use serde_json::Value;

use crate::adapter::{AgentAdapter, CoordConfig};
use crate::event::AgentEvent;

/// Built from a real installed `gemini` CLI (`@google/gemini-cli` 0.50.0,
/// confirmed via `--help` and by actually running `gemini mcp add` and
/// inspecting the file it wrote), **not live-verified against a real
/// authenticated session** -- this environment has no Gemini API key or
/// Google Cloud auth configured, and `gemini -p "..."` fails immediately
/// with "Please set an Auth method...". That means the streaming JSON
/// event schema below is inferred from the CLI's own naming conventions,
/// not captured from real output the way every other adapter's schema
/// was -- treat it the same way this project treated Codex before it was
/// installed: real until proven otherwise, not real because it compiles.
/// See issue #9.
pub struct GeminiAdapter;

impl AgentAdapter for GeminiAdapter {
    fn coord_server_name(&self) -> &'static str {
        "pact-coord"
    }

    /// No confirmed non-hanging alternative exists for this adapter
    /// (unlike Claude Code) -- whether `--approval-mode default` denies
    /// cleanly or hangs in headless mode couldn't be tested without real
    /// auth. `yolo` (auto-accept everything) is the only thing that can be
    /// stated with confidence won't hang, so -- same honest category as
    /// Copilot CLI and Codex -- that's the default, not claimed as a
    /// verified safer option.
    fn default_safety_description(&self) -> &'static str {
        "--approval-mode yolo (can run any shell command and edit any file with no restriction -- \
         unconfirmed whether a safer mode hangs in headless mode without real auth to test against; \
         see issue #9)"
    }

    /// `safety_override`, if given, is passed as a raw `--approval-mode`
    /// value (`default`/`auto_edit`/`yolo`/`plan`, confirmed from
    /// `gemini --help`).
    ///
    /// MCP config is the one genuinely different mechanism among all four
    /// adapters: confirmed directly (by running `gemini mcp add --scope
    /// project` and reading the file it produced) that Gemini CLI reads
    /// `.gemini/settings.json`, relative to its *own working directory*,
    /// automatically -- no CLI flag hands it over at all, unlike Claude
    /// Code/Copilot CLI's `--mcp-config`/`--additional-mcp-config` or
    /// Codex's inline `-c` overrides. The file's shape is identical to
    /// Claude Code and Copilot CLI's `{"mcpServers": {...}}` (confirmed:
    /// same `write_mcp_json_config` helper works unchanged), just written
    /// to a fixed path under `workspace_path` instead of wherever
    /// `coord.config_path` says.
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
            // No flag needed -- gemini reads .gemini/settings.json from its
            // cwd automatically, which `run_and_stream` already sets to
            // `workspace_path` for every adapter.
        }

        ("gemini".to_string(), args)
    }

    /// Inferred, not confirmed -- see this module's doc comment. Modeled
    /// on the shape common to the other three streaming-NDJSON adapters
    /// (an init/session event, assistant text, tool-call events, a final
    /// result), using field names guessed from Gemini CLI's own
    /// vocabulary (`-o stream-json`'s wrapper type is unknown, so this
    /// guesses a flat `{"type": ...}` shape like Claude Code's and
    /// Codex's). Deliberately defensive: any line that doesn't parse as
    /// JSON, or whose "type" isn't one of these guesses, surfaces as
    /// `Other` rather than being silently dropped -- exactly because this
    /// schema is unverified and *will* need correcting once run against a
    /// real session.
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
