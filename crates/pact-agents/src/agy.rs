use serde_json::Value;

use crate::adapter::{AgentAdapter, CoordConfig};
use crate::event::AgentEvent;

/// Antigravity (`agy`), issue #9's real path to a working Gemini-model
/// adapter -- the standalone `gemini` npm CLI (`crates/pact-agents/src/
/// gemini.rs`) remains genuinely blocked on individual-auth OAuth (Google
/// discontinued that path; confirmed again by hand this pass: a real
/// headless `-p` call to it hangs indefinitely with zero output). `agy`
/// is a distinct, multi-model CLI (Gemini, Claude, and GPT-OSS backends
/// all selectable via `--model`) with working auth in this environment,
/// not a drop-in replacement for `gemini`'s own flags -- hence a new
/// adapter, not a patch to `gemini.rs`. Everything below is confirmed by
/// a real, authenticated, tool-using headless run (a `write_to_file`
/// call that actually created a file, `status: SUCCESS` on the `result`
/// event) -- see DESIGN.md ("pact-agents > Antigravity adapter").
pub struct AgyAdapter;

/// `agy`'s coordination-server registration is process-global and
/// persistent (`agy mcp add <name> <command> [args...]`, writing to a
/// single `~/.gemini/config/mcp_config.json` shared by every `agy`
/// invocation on the machine), unlike every other adapter's per-spawn
/// config file or inline override. `build_command` re-registers this
/// workspace's own `pact mcp-serve` invocation under the fixed name
/// `pact-coord` immediately before each spawn ("add or update" semantics
/// -- confirmed by hand), which is correct for one `agy` run at a time,
/// but racy if two or more `agy` spawns are genuinely concurrent: the
/// second registration overwrites the first, and whichever `pact
/// mcp-serve` a spawn's own `agy` subprocess ends up launching depends on
/// timing, not on which workspace it's actually running in. Documented,
/// not hidden -- see DESIGN.md for the full writeup and why this is a
/// real limitation of `agy`'s own current CLI, not a pact bug.
const COORD_SERVER_NAME: &str = "pact-coord";

impl AgentAdapter for AgyAdapter {
    fn coord_server_name(&self) -> &'static str {
        COORD_SERVER_NAME
    }

    fn default_safety_description(&self) -> &'static str {
        "--dangerously-skip-permissions (can run any shell command and edit any file with no \
         restriction)"
    }

    fn build_command(
        &self,
        task: &str,
        safety_override: Option<&str>,
        coord: Option<&CoordConfig>,
        workspace_path: &std::path::Path,
    ) -> (String, Vec<String>) {
        if let Some(coord) = coord {
            register_coord_server(coord);
        }

        let mut args = vec![
            "-p".to_string(),
            task.to_string(),
            "--output-format".to_string(),
            "stream-json".to_string(),
            "--add-dir".to_string(),
            workspace_path.to_string_lossy().to_string(),
        ];
        match safety_override {
            Some(mode) => {
                args.push("--mode".to_string());
                args.push(mode.to_string());
            }
            None => args.push("--dangerously-skip-permissions".to_string()),
        }

        ("agy".to_string(), args)
    }

    fn parse_line(&self, line: &str) -> Vec<AgentEvent> {
        let value: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => return vec![AgentEvent::Other(Value::String(line.to_string()))],
        };

        match value.get("event").and_then(Value::as_str) {
            Some("init") => {
                // conversation_id sits at the top level of the event,
                // alongside "init" (which itself only carries cwd/tools/
                // permission_mode) -- confirmed against a real spawn's
                // own raw log after an initial guess at this nesting
                // shipped wrong and produced an empty session id.
                let session_id = value
                    .get("conversation_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                vec![AgentEvent::Init { session_id }]
            }
            Some("step_update") => parse_step_update(&value),
            Some("result") => {
                let result = value.get("result");
                let success = result
                    .and_then(|r| r.get("status"))
                    .and_then(Value::as_str)
                    .map(|s| s == "SUCCESS")
                    .unwrap_or(false);
                let summary = result
                    .and_then(|r| r.get("response"))
                    .and_then(Value::as_str)
                    .unwrap_or("(no result text)")
                    .to_string();
                vec![AgentEvent::Result { success, summary }]
            }
            _ => vec![AgentEvent::Other(value)],
        }
    }
}

/// A `step_update` event carries either a tool call (`step_type: "tool"`)
/// or a chunk of assistant prose (`step_type: "agent_response"`, `text_
/// delta` -- confirmed by hand: a real multi-chunk response streamed
/// `text_delta` across several `step_update` events, not one event per
/// full message the way Claude Code's does). `step_type: "user_input"`
/// (the echoed prompt) carries neither and is dropped -- not an error,
/// nothing useful to surface from it.
///
/// A real tool call fires *two* `step_update`s for the same `step_index`
/// -- `state: "ACTIVE"` when it starts, `state: "DONE"` when it finishes
/// -- confirmed against a real spawn's raw log, same `tool_name`/
/// `tool_info` both times. Only `ACTIVE` is surfaced as a `ToolUse`; the
/// `DONE` one is dropped rather than emitted as a second, redundant
/// event -- a first version of this surfaced both and printed every real
/// tool call twice in the live CLI view.
fn parse_step_update(value: &Value) -> Vec<AgentEvent> {
    let Some(update) = value.get("step_update") else {
        return vec![AgentEvent::Other(value.clone())];
    };
    match update.get("step_type").and_then(Value::as_str) {
        Some("tool") => {
            if update.get("state").and_then(Value::as_str) != Some("ACTIVE") {
                return vec![];
            }
            let tool_info = update.get("tool_info");
            let name = update
                .get("tool_name")
                .and_then(Value::as_str)
                .unwrap_or("unknown_tool")
                .to_string();
            let input = tool_info
                .and_then(|t| t.get("parameters"))
                .cloned()
                .unwrap_or(Value::Null);
            vec![AgentEvent::ToolUse { name, input }]
        }
        Some("agent_response") => match update.get("text_delta").and_then(Value::as_str) {
            Some(text) if !text.is_empty() => vec![AgentEvent::AssistantText(text.to_string())],
            _ => vec![],
        },
        _ => vec![],
    }
}

/// Best-effort: a failure to register just means the spawned `agy`
/// process won't have coordination tools available, the same "logged,
/// not fatal" posture every other adapter's MCP setup already takes.
fn register_coord_server(coord: &CoordConfig) {
    let output = std::process::Command::new("agy")
        .arg("mcp")
        .arg("add")
        .arg(&coord.server_name)
        .arg(&coord.command)
        .args(&coord.args)
        .output();
    match output {
        Ok(output) if !output.status.success() => {
            tracing::warn!(
                "failed to register {} with agy mcp add: {}",
                coord.server_name,
                String::from_utf8_lossy(&output.stderr)
            );
        }
        Err(err) => {
            tracing::warn!("failed to run `agy mcp add`: {err:#}");
        }
        Ok(_) => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_safety_passes_dangerously_skip_permissions() {
        let (program, args) =
            AgyAdapter.build_command("do the thing", None, None, std::path::Path::new("/tmp/workspace"));
        assert_eq!(program, "agy");
        assert!(args.iter().any(|a| a == "--dangerously-skip-permissions"));
    }

    #[test]
    fn always_adds_the_workspace_dir_so_writes_land_in_the_real_workspace() {
        // Confirmed by hand (issue #9): without --add-dir, agy's own
        // write_to_file tool wrote into its own internal scratch
        // directory instead of the process's actual cwd/workspace --
        // silently corrupting the whole per-workspace orchestration
        // model if missed. See DESIGN.md for the real repro.
        let (_, args) =
            AgyAdapter.build_command("do the thing", None, None, std::path::Path::new("/tmp/workspace"));
        let idx = args.iter().position(|a| a == "--add-dir").unwrap();
        assert_eq!(args[idx + 1], "/tmp/workspace");
    }

    #[test]
    fn safety_override_becomes_a_mode_flag_instead_of_the_skip_permissions_default() {
        let (_, args) =
            AgyAdapter.build_command("do the thing", Some("plan"), None, std::path::Path::new("/tmp/workspace"));
        assert!(!args.iter().any(|a| a == "--dangerously-skip-permissions"));
        let idx = args.iter().position(|a| a == "--mode").unwrap();
        assert_eq!(args[idx + 1], "plan");
    }

    #[test]
    fn parse_line_extracts_the_conversation_id_from_a_real_init_event() {
        // conversation_id is top-level, a sibling of "init" -- taken
        // verbatim from a real pact spawn --agent agy run's own raw log
        // (trimmed to the fields parse_line actually reads), not
        // fabricated -- a first guess at this nesting shipped wrong
        // (nested under "init") and produced an empty session id in a
        // real spawn before this test caught it.
        let line = r#"{"event":"init","conversation_id":"8e06052d-fc64-4b24-8302-7283a58d2d42","init":{"cwd":"C:\\workspace","tools":["write_to_file"],"permission_mode":"always-proceed"}}"#;
        let events = AgyAdapter.parse_line(line);
        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0], AgentEvent::Init { session_id } if session_id == "8e06052d-fc64-4b24-8302-7283a58d2d42"));
    }

    #[test]
    fn parse_line_extracts_a_tool_use_from_a_real_active_step_update() {
        let line = r#"{"event":"step_update","step_update":{"state":"ACTIVE","step_type":"tool","tool_name":"write_to_file","tool_info":{"parameters":{"TargetFile":"x.txt"}}}}"#;
        let events = AgyAdapter.parse_line(line);
        assert_eq!(events.len(), 1);
        match &events[0] {
            AgentEvent::ToolUse { name, input } => {
                assert_eq!(name, "write_to_file");
                assert_eq!(input["TargetFile"], "x.txt");
            }
            other => panic!("expected ToolUse, got {other:?}"),
        }
    }

    #[test]
    fn parse_line_drops_the_done_state_tool_step_update_as_a_duplicate() {
        // A real tool call fires ACTIVE then DONE for the same
        // step_index/tool_name/tool_info -- only ACTIVE should surface,
        // or every real tool call prints twice in the live CLI view.
        let line = r#"{"event":"step_update","step_update":{"state":"DONE","step_type":"tool","tool_name":"write_to_file","tool_info":{"parameters":{"TargetFile":"x.txt"}}}}"#;
        assert!(AgyAdapter.parse_line(line).is_empty());
    }

    #[test]
    fn parse_line_extracts_assistant_text_from_a_real_step_update() {
        let line = r#"{"event":"step_update","step_update":{"step_type":"agent_response","text_delta":"hello there"}}"#;
        let events = AgyAdapter.parse_line(line);
        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0], AgentEvent::AssistantText(t) if t == "hello there"));
    }

    #[test]
    fn parse_line_drops_a_user_input_step_update() {
        let line = r#"{"event":"step_update","step_update":{"step_type":"user_input"}}"#;
        assert!(AgyAdapter.parse_line(line).is_empty());
    }

    #[test]
    fn parse_line_extracts_a_successful_result() {
        let line = r#"{"event":"result","result":{"status":"SUCCESS","response":"all done"}}"#;
        let events = AgyAdapter.parse_line(line);
        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0], AgentEvent::Result { success, summary } if *success && summary == "all done"));
    }

    #[test]
    fn parse_line_extracts_a_failed_result() {
        let line = r#"{"event":"result","result":{"status":"FAILED","response":"couldn't finish"}}"#;
        let events = AgyAdapter.parse_line(line);
        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0], AgentEvent::Result { success, .. } if !success));
    }

    #[test]
    fn parse_line_falls_back_to_other_for_unrecognized_json() {
        let line = r#"{"event":"something_new","payload":42}"#;
        let events = AgyAdapter.parse_line(line);
        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0], AgentEvent::Other(_)));
    }

    #[test]
    fn parse_line_falls_back_to_other_for_non_json_lines() {
        let events = AgyAdapter.parse_line("not json at all");
        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0], AgentEvent::Other(_)));
    }
}
