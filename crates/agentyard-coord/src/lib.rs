//! The coordination server (Phase 3).
//!
//! Advisory, glob-based, TTL-expiring file leases plus a threaded message
//! log between agents -- not enforcement, and deliberately not deep
//! semantic dependency analysis (see the README). Runs as its own process
//! (`agentyard mcp-serve`, launched by the agent CLI itself over stdio,
//! not run in-process by the orchestrator) speaking MCP via `rmcp`, backed
//! by a SQLite database shared across every agent in one repo's session.

mod db;
mod leases;
mod messages;
mod server;

pub use leases::{Conflict, ClaimResult};
pub use messages::Message;

use std::path::{Path, PathBuf};

use anyhow::Result;

/// Opens the shared coordination database and serves the MCP protocol over
/// stdio until the client (the agent CLI) disconnects. Blocks for the
/// lifetime of the connection -- this is the entire job of the
/// `agentyard mcp-serve` subcommand.
pub async fn serve(repo_root: &Path, agent_id: String, workspace_root: PathBuf) -> Result<()> {
    let conn = db::open(repo_root)?;
    server::serve(conn, agent_id, workspace_root).await
}
