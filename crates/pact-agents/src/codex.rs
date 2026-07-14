use serde_json::Value;

use crate::adapter::{AgentAdapter, CoordConfig};
use crate::event::AgentEvent;

/// Live-verified against a real installed `codex` (codex-cli 0.144.3) --
/// this was NOT true when this adapter was first written (built from
/// OpenAI's docs alone, on a machine without Codex installed) and the
/// docs turned out to be wrong on the exact safety flag (see
/// `default_safety_description`). Fixed and confirmed end-to-end,
/// including a real MCP tool call through this project's own
/// coordination server, not just a bare launch.
pub struct CodexAdapter;

impl AgentAdapter for CodexAdapter {
    fn coord_server_name(&self) -> &'static str {
        "pact-coord"
    }

    /// The docs described a separate `--ask-for-approval` flag with
    /// `never`/`on-request`/`untrusted` values -- that flag does not exist
    /// in `codex exec --help` for the installed version. What actually
    /// works, confirmed directly: `--sandbox workspace-write` alone still
    /// refuses to write files in non-interactive mode (the agent reports
    /// back "approvals are disabled" and gives up rather than hanging --
    /// a good failure mode, but not a working one). The only flag that
    /// produces a real, completed file write is
    /// `--dangerously-bypass-approvals-and-sandbox`, which -- true to its
    /// name -- skips both approval prompts and sandboxing in one flag,
    /// rather than two independent axes as the docs implied.
    fn default_safety_description(&self) -> &'static str {
        "--dangerously-bypass-approvals-and-sandbox (can run any shell command and edit any file \
         with no restriction -- confirmed no safe alternative actually lets it write files at all \
         in headless mode; see issue #2's investigation)"
    }

    /// `safety_override`, if given, is treated as a `--sandbox` value
    /// (`read-only`/`workspace-write`/`danger-full-access`) rather than
    /// the bypass flag -- confirmed that a plain sandbox mode without the
    /// bypass flag still won't let the agent actually change anything in
    /// headless mode, so this is mainly useful for a deliberately
    /// read-only/inspect-only run, not a safer "still gets work done"
    /// middle ground the way Claude Code's `acceptEdits` is.
    ///
    /// MCP servers are passed via inline `-c mcp_servers.<id>.command=`/
    /// `-c mcp_servers.<id>.args=` overrides (confirmed working end-to-end:
    /// a real `claim_files` call through this project's own coordination
    /// server returned the correct JSON) rather than
    /// `$CODEX_HOME/config.toml` -- that file also holds Codex's
    /// auth/session state, not just config, so pointing `CODEX_HOME` at a
    /// per-workspace directory would plausibly break headless login.
    fn build_command(
        &self,
        task: &str,
        safety_override: Option<&str>,
        coord: Option<&CoordConfig>,
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

    /// Schema modeled directly against real output captured from
    /// `codex exec --json` (see README), not secondhand docs -- including
    /// a real tool-call-forcing task and a real MCP tool call, the same
    /// standard as the Claude Code and Copilot CLI adapters.
    ///
    /// One real gap: unlike Claude Code's `result.is_error` or Copilot's
    /// `result.exitCode`, Codex's `turn.completed` event carries no
    /// success/failure signal at all -- a turn can "complete" whether or
    /// not the requested task actually happened (confirmed: a file-write
    /// task under a sandbox mode that refused the write still produced a
    /// normal `turn.completed`). So this adapter never emits
    /// `AgentEvent::Result` itself; success is determined from the
    /// process's actual exit code instead (see
    /// `process::run_and_stream`'s fallback, which this finding is also
    /// why that fallback no longer assumes failure by default).
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
        let (program, args) = CodexAdapter.build_command("do the thing", None, Some(&coord));
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
        let (_, args) = CodexAdapter.build_command("do the thing", None, None);
        assert!(!args.iter().any(|a| a.contains("mcp_servers")));
    }

    #[test]
    fn default_safety_uses_bypass_flag() {
        let (_, args) = CodexAdapter.build_command("do the thing", None, None);
        assert!(args.contains(&"--dangerously-bypass-approvals-and-sandbox".to_string()));
    }

    #[test]
    fn safety_override_maps_to_sandbox_flag() {
        let (_, args) = CodexAdapter.build_command("do the thing", Some("read-only"), None);
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
