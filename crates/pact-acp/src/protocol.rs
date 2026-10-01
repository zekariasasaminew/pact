//! The subset of the Agent Client Protocol pact speaks, as typed shapes.
//! Spec: <https://agentclientprotocol.com/protocol>. Only what the lane
//! runtime needs (issue #330): initialize, session/new, session/prompt,
//! session/cancel, session/close from the client; session/update and
//! session/request_permission from the agent. Everything else the agent
//! sends is either passed through as a raw `serde_json::Value` or
//! answered with JSON-RPC -32601.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The one protocol major version pact implements.
pub const PROTOCOL_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeParams {
    pub protocol_version: u32,
    pub client_capabilities: ClientCapabilities,
    pub client_info: ClientInfo,
}

/// pact advertises nothing: the agent uses its own file and shell tools,
/// which is what makes a lane behave exactly like a headless CLI run.
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientCapabilities {
    pub fs: FsCapabilities,
    pub terminal: bool,
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FsCapabilities {
    pub read_text_file: bool,
    pub write_text_file: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct ClientInfo {
    pub name: String,
    pub version: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeResult {
    pub protocol_version: u32,
    #[serde(default)]
    pub agent_capabilities: Value,
    #[serde(default)]
    pub agent_info: Option<AgentInfo>,
    #[serde(default)]
    pub auth_methods: Vec<Value>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AgentInfo {
    pub name: String,
    #[serde(default)]
    pub version: String,
}

/// An MCP server the agent should connect for one session. Copilot's ACP
/// mode rejects `Stdio` servers from the client (its log: "Rejecting
/// non-http/sse MCP server"), which is why pact-coord grew an HTTP mode
/// (issue #329); `Stdio` stays for agents that do accept it.
#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum McpServer {
    Http(McpHttpServer),
    Stdio(McpStdioServer),
}

impl McpServer {
    pub fn http(name: impl Into<String>, url: impl Into<String>) -> Self {
        McpServer::Http(McpHttpServer { kind: "http", name: name.into(), url: url.into(), headers: Vec::new() })
    }

    pub fn stdio(name: impl Into<String>, command: impl Into<String>, args: Vec<String>) -> Self {
        McpServer::Stdio(McpStdioServer { name: name.into(), command: command.into(), args, env: Vec::new() })
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct McpHttpServer {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub name: String,
    pub url: String,
    pub headers: Vec<HttpHeader>,
}

#[derive(Debug, Clone, Serialize)]
pub struct HttpHeader {
    pub name: String,
    pub value: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct McpStdioServer {
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    pub env: Vec<EnvVar>,
}

#[derive(Debug, Clone, Serialize)]
pub struct EnvVar {
    pub name: String,
    pub value: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NewSessionParams {
    pub cwd: String,
    pub mcp_servers: Vec<McpServer>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NewSessionResult {
    pub session_id: String,
    #[serde(default)]
    pub config_options: Vec<Value>,
    #[serde(default)]
    pub modes: Option<Value>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptParams {
    pub session_id: String,
    pub prompt: Vec<ContentBlock>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text { text: String },
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptResult {
    pub stop_reason: StopReason,
}

/// Why a prompt turn ended. `EndTurn` is the only success.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    EndTurn,
    MaxTokens,
    MaxTurnRequests,
    Refusal,
    Cancelled,
    #[serde(other)]
    Unknown,
}

impl StopReason {
    pub fn is_success(self) -> bool {
        matches!(self, StopReason::EndTurn)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            StopReason::EndTurn => "end_turn",
            StopReason::MaxTokens => "max_tokens",
            StopReason::MaxTurnRequests => "max_turn_requests",
            StopReason::Refusal => "refusal",
            StopReason::Cancelled => "cancelled",
            StopReason::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionIdParams {
    pub session_id: String,
}

/// One `session/update` notification. `kind` is the `sessionUpdate`
/// discriminator (`agent_message_chunk`, `tool_call`, `usage_update`,
/// ...); `raw` is the whole `update` object, so callers can read fields
/// pact does not model without pact having to model them.
#[derive(Debug, Clone)]
pub struct SessionUpdate {
    pub session_id: String,
    pub kind: String,
    pub raw: Value,
}

impl SessionUpdate {
    /// Text of an `agent_message_chunk` or `agent_thought_chunk`, if any.
    pub fn text(&self) -> Option<&str> {
        match self.kind.as_str() {
            "agent_message_chunk" | "agent_thought_chunk" => self.raw.pointer("/content/text").and_then(Value::as_str),
            _ => None,
        }
    }

    /// Title of a `tool_call` or `tool_call_update`, if present.
    pub fn tool_title(&self) -> Option<&str> {
        match self.kind.as_str() {
            "tool_call" | "tool_call_update" => self.raw.get("title").and_then(Value::as_str),
            _ => None,
        }
    }

    pub fn tool_status(&self) -> Option<&str> {
        match self.kind.as_str() {
            "tool_call" | "tool_call_update" => self.raw.get("status").and_then(Value::as_str),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PermissionRequest {
    pub session_id: String,
    #[serde(default)]
    pub tool_call: Value,
    pub options: Vec<PermissionOption>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PermissionOption {
    pub option_id: String,
    pub name: String,
    pub kind: PermissionKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionKind {
    AllowOnce,
    AllowAlways,
    RejectOnce,
    RejectAlways,
    #[serde(other)]
    Unknown,
}

impl PermissionKind {
    pub fn allows(self) -> bool {
        matches!(self, PermissionKind::AllowOnce | PermissionKind::AllowAlways)
    }
}

/// The client's answer to a permission request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermissionDecision {
    Select(String),
    Cancelled,
}

impl PermissionDecision {
    pub fn to_result(&self) -> Value {
        match self {
            PermissionDecision::Select(option_id) => {
                serde_json::json!({ "outcome": { "outcome": "selected", "optionId": option_id } })
            }
            PermissionDecision::Cancelled => serde_json::json!({ "outcome": { "outcome": "cancelled" } }),
        }
    }
}

/// The default policy for an unattended lane: take the first option that
/// allows (preferring "always" so the agent stops asking), and cancel
/// when nothing allows, since a lane has nobody to ask.
pub fn allow_first_permission(request: &PermissionRequest) -> PermissionDecision {
    request
        .options
        .iter()
        .find(|o| o.kind == PermissionKind::AllowAlways)
        .or_else(|| request.options.iter().find(|o| o.kind.allows()))
        .map(|o| PermissionDecision::Select(o.option_id.clone()))
        .unwrap_or(PermissionDecision::Cancelled)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn http_mcp_server_serializes_with_a_type_tag_and_stdio_without_one() {
        let http = serde_json::to_value(McpServer::http("pact-coord", "http://127.0.0.1:1/lanes/a")).unwrap();
        assert_eq!(http["type"], "http");
        assert_eq!(http["url"], "http://127.0.0.1:1/lanes/a");
        assert!(http["headers"].as_array().unwrap().is_empty());
        let stdio = serde_json::to_value(McpServer::stdio("pact-coord", "pact", vec!["mcp-serve".into()])).unwrap();
        assert!(stdio.get("type").is_none(), "stdio is the untagged default shape in ACP v1: {stdio}");
        assert_eq!(stdio["command"], "pact");
    }

    #[test]
    fn stop_reason_round_trips_and_tolerates_new_values() {
        assert_eq!(serde_json::from_str::<StopReason>("\"end_turn\"").unwrap(), StopReason::EndTurn);
        assert_eq!(serde_json::from_str::<StopReason>("\"refusal\"").unwrap(), StopReason::Refusal);
        assert_eq!(serde_json::from_str::<StopReason>("\"something_new\"").unwrap(), StopReason::Unknown);
        assert!(StopReason::EndTurn.is_success() && !StopReason::Cancelled.is_success());
    }

    #[test]
    fn default_permission_policy_prefers_allow_always_then_any_allow_then_cancels() {
        let req = |kinds: &[&str]| PermissionRequest {
            session_id: "s".into(),
            tool_call: Value::Null,
            options: kinds
                .iter()
                .enumerate()
                .map(|(i, k)| PermissionOption {
                    option_id: format!("o{i}"),
                    name: k.to_string(),
                    kind: serde_json::from_value(Value::String(k.to_string())).unwrap(),
                })
                .collect(),
        };
        assert_eq!(allow_first_permission(&req(&["allow_once", "allow_always", "reject_once"])), PermissionDecision::Select("o1".into()));
        assert_eq!(allow_first_permission(&req(&["reject_once", "allow_once"])), PermissionDecision::Select("o1".into()));
        assert_eq!(allow_first_permission(&req(&["reject_once", "reject_always"])), PermissionDecision::Cancelled);
    }

    #[test]
    fn session_update_helpers_read_text_and_tool_fields() {
        let chunk = SessionUpdate {
            session_id: "s".into(),
            kind: "agent_message_chunk".into(),
            raw: serde_json::json!({ "sessionUpdate": "agent_message_chunk", "content": { "type": "text", "text": "hi" } }),
        };
        assert_eq!(chunk.text(), Some("hi"));
        assert_eq!(chunk.tool_title(), None);
        let tool = SessionUpdate {
            session_id: "s".into(),
            kind: "tool_call".into(),
            raw: serde_json::json!({ "sessionUpdate": "tool_call", "title": "Creating a.txt", "status": "pending" }),
        };
        assert_eq!(tool.tool_title(), Some("Creating a.txt"));
        assert_eq!(tool.tool_status(), Some("pending"));
        assert_eq!(tool.text(), None);
    }
}
