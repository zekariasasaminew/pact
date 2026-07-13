use serde_json::Value;

/// A normalized view over one line of an agent CLI's streamed NDJSON
/// output. Modeled directly against real output captured from `claude -p
/// --output-format stream-json --verbose` (see README) -- `Other` is a
/// catch-all for anything not explicitly modeled, but it's still surfaced
/// to callers (never silently dropped), since an unrecognized event is far
/// more likely to be a real tool-use/tool-result message this adapter
/// hasn't been taught about yet than something safe to ignore.
#[derive(Debug, Clone)]
pub enum AgentEvent {
    Init {
        session_id: String,
    },
    AssistantText(String),
    ToolUse {
        name: String,
        input: Value,
    },
    /// A tool-result echo (observed as a `"user"`-typed message in Claude
    /// Code's stream) or any other content-bearing message this adapter
    /// doesn't parse in detail.
    Other(Value),
    Result {
        success: bool,
        summary: String,
    },
}

/// Parses one raw NDJSON line into an `AgentEvent`. Never fails: a line
/// that isn't valid JSON, or doesn't match a known shape, becomes
/// `Other` wrapping whatever could be salvaged (or a string value as a
/// last resort) so a single malformed/unexpected line can't crash the
/// whole stream.
pub fn parse_line(line: &str) -> AgentEvent {
    let value: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(_) => return AgentEvent::Other(Value::String(line.to_string())),
    };

    match value.get("type").and_then(Value::as_str) {
        Some("system") if value.get("subtype").and_then(Value::as_str) == Some("init") => {
            let session_id = value
                .get("session_id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            AgentEvent::Init { session_id }
        }
        Some("assistant") => parse_assistant(&value),
        Some("result") => {
            let success = value.get("is_error").and_then(Value::as_bool) == Some(false);
            let summary = value
                .get("result")
                .and_then(Value::as_str)
                .unwrap_or("(no result text)")
                .to_string();
            AgentEvent::Result { success, summary }
        }
        _ => AgentEvent::Other(value),
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
