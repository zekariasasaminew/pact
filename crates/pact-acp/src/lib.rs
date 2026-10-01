//! pact-acp: a minimal Agent Client Protocol client (issue #330).
//!
//! Why this exists, measured (issue #306): eight lanes as eight cold
//! `copilot -p` processes took 50.9 s and 2,456 MB to finish a trivial
//! task; eight ACP sessions inside one `copilot --acp` process took 5.6 s
//! and 445 MB. ACP (<https://agentclientprotocol.com>) gives a client many
//! independent sessions in one agent process, each with its own working
//! directory and MCP servers, which is exactly a pact lane. Gemini CLI
//! speaks it natively and Zed ships adapters for Claude Code and Codex, so
//! one client here replaces one bespoke stdout parser per agent CLI.
//!
//! Layers: [`protocol`] (the typed subset), [`client`] (async, one agent
//! process, id-multiplexed JSON-RPC over stdio), [`runtime`] (the blocking
//! per-lane facade pact-core calls from its lane threads).

pub mod client;
pub mod protocol;
pub mod runtime;

pub use client::{AcpClient, AcpError, AgentSpec, ExitInfo, PermissionPolicy};
pub use protocol::{
    allow_first_permission, McpServer, PermissionDecision, PermissionKind, PermissionRequest, SessionUpdate, StopReason,
};
pub use runtime::{AcpRuntime, LaneSession};
