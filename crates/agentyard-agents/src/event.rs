use serde_json::Value;

/// A normalized view over one line of an agent CLI's streamed output.
/// Shared across every adapter (Claude Code, Copilot CLI, Codex), even
/// though each CLI's actual output schema is different -- each adapter's
/// own `parse_line` is responsible for mapping its specific shape onto
/// this enum. `Other` is a catch-all for anything not explicitly modeled,
/// but it's still surfaced to callers (never silently dropped), since an
/// unrecognized event is far more likely to be a real message an adapter
/// hasn't been taught about yet than something safe to ignore.
#[derive(Debug, Clone)]
pub enum AgentEvent {
    Init {
        session_id: String,
    },
    /// One MCP server's connection status, e.g. `("agentyard-coord",
    /// "connected")` or `(..., "failed")`. A separate variant, not bundled
    /// into `Init` -- Claude Code reports every server's status inside its
    /// one init event, but Copilot CLI reports them as their own
    /// standalone events, and a line can report several servers at once.
    /// Each adapter's `parse_line` emits zero or more of these per line as
    /// its own schema demands; the connectivity check that consumes them
    /// (`agentyard-core`) doesn't need to know which shape produced them.
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
