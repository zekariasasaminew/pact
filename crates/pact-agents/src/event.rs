use serde_json::Value;

/// A normalized view over one line of an agent CLI's streamed output,
/// shared across every adapter -- see DESIGN.md ("pact-agents > AgentEvent
/// normalization") for why `Other` is always surfaced, never dropped.
#[derive(Debug, Clone)]
pub enum AgentEvent {
    Init {
        session_id: String,
    },
    /// One MCP server's connection status, e.g. `("pact-coord",
    /// "connected")` or `(..., "failed")`. A separate variant, not bundled
    /// into `Init` -- see DESIGN.md ("pact-agents > AgentEvent
    /// normalization").
    CoordStatus {
        name: String,
        status: String,
    },
    AssistantText(String),
    ToolUse {
        name: String,
        input: Value,
    },
    /// A tool-result echo or any other content-bearing message an adapter
    /// doesn't parse in detail.
    Other(Value),
    Result {
        success: bool,
        summary: String,
    },
}
