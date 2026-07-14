//! The coordination server (Phase 3).
//!
//! Advisory, glob-based, TTL-expiring file leases plus a threaded message
//! log between agents -- not enforcement, and deliberately not deep
//! semantic dependency analysis (see the README). Runs as its own process
//! (`pact mcp-serve`, launched by the agent CLI itself over stdio,
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
/// `pact mcp-serve` subcommand.
pub async fn serve(repo_root: &Path, agent_id: String, workspace_root: PathBuf) -> Result<()> {
    let conn = db::open(repo_root)?;
    server::serve(conn, agent_id, workspace_root).await
}

/// Every lease (active or already expired) whose glob pattern matches
/// `file`, as `(pattern, holder)` pairs -- used by `pact-core`'s
/// cross-workspace conflict detection (issue #8) to show whether a
/// reported file overlap was one either agent had actually claimed.
/// Expired leases are included deliberately: for after-the-fact review,
/// "this was claimed but the lease had lapsed" is still useful context, not
/// noise to filter out. A plain synchronous open -- no need for `serve`'s
/// tokio runtime just to run one read query.
pub fn leases_matching(repo_root: &Path, file: &str) -> Result<Vec<(String, String)>> {
    let conn = db::open(repo_root)?;
    let mut stmt = conn.prepare("SELECT pattern, holder FROM leases")?;
    let rows = stmt.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;

    let mut matches = Vec::new();
    for row in rows {
        let (pattern, holder) = row?;
        if pattern_matches(&pattern, file) {
            matches.push((pattern, holder));
        }
    }
    Ok(matches)
}

/// Total messages (broadcast or direct) sent by any agent id in
/// `agent_ids` -- a coarse "there's relevant coordination history here,
/// go look" pointer for conflict reports, not a full pairwise transcript.
pub fn message_count_involving(repo_root: &Path, agent_ids: &[String]) -> Result<usize> {
    if agent_ids.is_empty() {
        return Ok(0);
    }
    let conn = db::open(repo_root)?;
    let placeholders = agent_ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
    let sql = format!("SELECT COUNT(*) FROM messages WHERE from_agent IN ({placeholders})");
    let mut stmt = conn.prepare(&sql)?;
    let params = rusqlite::params_from_iter(agent_ids.iter());
    let count: i64 = stmt.query_row(params, |row| row.get(0))?;
    Ok(count as usize)
}

/// Matches a single concrete file path against a glob pattern -- unlike
/// `leases::expand_glob`, this doesn't need to walk a directory, since the
/// caller already has a concrete path (from a `git diff`) to test.
fn pattern_matches(pattern: &str, file: &str) -> bool {
    globset::GlobBuilder::new(pattern)
        .literal_separator(false)
        .build()
        .map(|g| g.compile_matcher().is_match(file))
        .unwrap_or(false)
}
