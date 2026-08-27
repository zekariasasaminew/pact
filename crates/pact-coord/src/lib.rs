//! The coordination server (Phase 3).
//!
//! Advisory, glob-based, TTL-expiring file leases plus a threaded message
//! log between agents -- not enforcement, and deliberately not deep
//! semantic dependency analysis (see the README). Runs as its own process
//! (`pact mcp-serve`, launched by the agent CLI itself over stdio,
//! not run in-process by the orchestrator) speaking MCP via `rmcp`, backed
//! by a SQLite database shared across every agent in one repo's session.

mod db;
mod handoffs;
mod leases;
mod messages;
mod operations;
mod persisted_conflicts;
mod server;

pub use handoffs::{HandoffDecision, HandoffRequest, HandoffRequestResult, HandoffStatus};
pub use leases::{ActiveLease, Conflict, ClaimResult};
pub use messages::Message;
pub use operations::{HistoryFilter, Operation};
pub use persisted_conflicts::PersistedConflict;

use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::Serialize;
use serde_json::Value;

/// One agent's pending (unread) message count, as of the moment
/// `CoordStatus` was computed.
#[derive(Debug, Clone, Serialize)]
pub struct AgentPending {
    pub agent_id: String,
    pub pending: i64,
}

/// A full snapshot of the coordination layer's current state, for `pact
/// coord-status` (issue #64) -- makes the coord layer visible instead of a
/// black box: every active lease, and every known agent's pending message
/// count.
#[derive(Debug, Clone)]
pub struct CoordStatus {
    pub active_leases: Vec<ActiveLease>,
    pub pending_messages: Vec<AgentPending>,
    /// Workspace ids whose coord server has ever completed an MCP
    /// `initialize` handshake -- issue #235. A workspace missing from
    /// here (and from `pact list`'s active workspaces) never connected at
    /// all, which "no active leases" alone can't distinguish from "connected,
    /// nothing claimed yet."
    pub connected_agent_ids: std::collections::HashSet<String>,
}

/// Unconditionally removes every lease for `repo_root`'s coordination
/// database -- `pact clear-leases` (issue #209). See
/// `leases::clear_leases` for why this deliberately isn't a
/// staleness heuristic. Returns how many rows were removed.
pub fn clear_leases(repo_root: &Path) -> Result<usize> {
    let conn = db::open(repo_root)?;
    leases::clear_leases(&conn)
}

/// Best-effort teardown integration for the handoff protocol (issue
/// #163): cancels every still-pending handoff request addressed *to*
/// `agent_id` (a workspace id), since it can no longer respond once torn
/// down. See `handoffs::cancel_pending_handoffs_to` for the exact scope
/// and why outgoing requests are left alone. Returns how many rows were
/// cancelled.
pub fn cancel_pending_handoffs_to(repo_root: &Path, agent_id: &str) -> Result<usize> {
    let conn = db::open(repo_root)?;
    handoffs::cancel_pending_handoffs_to(&conn, agent_id)
}

/// Computes a `CoordStatus` snapshot. Read-only: unlike `check_messages`,
/// looking at pending counts here never advances anyone's cursor.
pub fn status(repo_root: &Path) -> Result<CoordStatus> {
    let conn = db::open(repo_root)?;
    let active_leases = leases::list_active_leases(&conn)?;
    let pending_messages = messages::known_agent_ids(&conn)?
        .into_iter()
        .map(|agent_id| {
            let pending = messages::pending_message_count(&conn, &agent_id)?;
            Ok(AgentPending { agent_id, pending })
        })
        .collect::<Result<Vec<_>>>()?;
    let connected_agent_ids = operations::connected_workspace_ids(&conn)?;
    Ok(CoordStatus { active_leases, pending_messages, connected_agent_ids })
}

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

/// Records one significant coordination-layer event -- see DESIGN.md
/// ("pact-coord > Operation log / `pact history` (issue #84)"). A plain
/// synchronous open, same as `leases_matching`/`message_count_involving`:
/// `pact-core` calls this from the main `pact` process (`merge_all`,
/// `arbiter_decision`, `teardown`), never inside an `mcp-serve`
/// subprocess, which logs its own operations (`claim`/`release`/
/// `broadcast`/`message`) directly against the connection it already
/// holds instead of going through this entry point.
pub fn log_operation(
    repo_root: &Path,
    op_type: &str,
    workspace_id: Option<&str>,
    detail: &Value,
) -> Result<()> {
    let conn = db::open(repo_root)?;
    operations::log_operation(&conn, op_type, workspace_id, detail)
}

/// Queries the operation log for `pact history` -- see
/// `operations::query_operations` for the filter semantics.
pub fn history(repo_root: &Path, filter: &HistoryFilter) -> Result<Vec<Operation>> {
    let conn = db::open(repo_root)?;
    operations::query_operations(&conn, filter)
}

/// Persists a real merge conflict `merge-all` skipped -- see DESIGN.md
/// ("pact-coord > Persisted conflicts / `pact resolve` (issue #85)").
pub fn record_conflict(repo_root: &Path, workspace_id: &str, target_branch: &str, files: &[String]) -> Result<()> {
    let conn = db::open(repo_root)?;
    persisted_conflicts::record_conflict(&conn, workspace_id, target_branch, files)?;
    Ok(())
}

/// Every currently-open persisted conflict, for `pact resolve` (no
/// workspace id given) to list.
pub fn open_conflicts(repo_root: &Path) -> Result<Vec<PersistedConflict>> {
    let conn = db::open(repo_root)?;
    persisted_conflicts::list_open_conflicts(&conn)
}

/// The most recent open conflict for `workspace_id`, if any -- what `pact
/// resolve <id>` acts on.
pub fn open_conflict_for_workspace(repo_root: &Path, workspace_id: &str) -> Result<Option<PersistedConflict>> {
    let conn = db::open(repo_root)?;
    persisted_conflicts::open_conflict_for_workspace(&conn, workspace_id)
}

pub fn mark_conflict_resolved(repo_root: &Path, conflict_id: i64) -> Result<()> {
    let conn = db::open(repo_root)?;
    persisted_conflicts::mark_resolved(&conn, conflict_id)
}

pub fn mark_conflict_abandoned(repo_root: &Path, conflict_id: i64) -> Result<()> {
    let conn = db::open(repo_root)?;
    persisted_conflicts::mark_abandoned(&conn, conflict_id)
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Exercises `status` end-to-end against a real file-backed database
    /// (via the same `db::open` every other public function here uses),
    /// not just the individual `leases`/`messages` functions it composes --
    /// confirms the plumbing between them is wired correctly.
    #[test]
    fn status_reports_active_leases_and_excludes_own_broadcasts_from_pending() {
        let repo_root = std::env::temp_dir().join(format!("pact-coord-status-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&repo_root).unwrap();

        {
            let conn = db::open(&repo_root).unwrap();
            messages::send_message(&conn, "agent-a", None, "bcast", "hello").unwrap();
            leases::claim_files(&conn, &repo_root, "agent-a", &["a.txt".to_string()], Some(900), false).unwrap();
        }

        let snapshot = status(&repo_root).unwrap();

        assert_eq!(snapshot.active_leases.len(), 1);
        assert_eq!(snapshot.active_leases[0].holder, "agent-a");
        assert_eq!(snapshot.active_leases[0].pattern, "a.txt");

        let a_pending = snapshot
            .pending_messages
            .iter()
            .find(|p| p.agent_id == "agent-a")
            .map(|p| p.pending)
            .unwrap_or(0);
        assert_eq!(a_pending, 0, "agent-a must not see its own broadcast as pending");

        let _ = std::fs::remove_dir_all(db::db_path(&repo_root).unwrap().parent().unwrap());
        let _ = std::fs::remove_dir_all(&repo_root);
    }

    #[test]
    fn status_is_empty_for_a_fresh_repo() {
        let repo_root = std::env::temp_dir().join(format!("pact-coord-status-empty-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&repo_root).unwrap();

        let snapshot = status(&repo_root).unwrap();
        assert!(snapshot.active_leases.is_empty());
        assert!(snapshot.pending_messages.is_empty());
        assert!(snapshot.connected_agent_ids.is_empty());

        let _ = std::fs::remove_dir_all(db::db_path(&repo_root).unwrap().parent().unwrap());
        let _ = std::fs::remove_dir_all(&repo_root);
    }

    #[test]
    fn status_reports_which_agents_ever_connected() {
        let repo_root = std::env::temp_dir().join(format!("pact-coord-status-connected-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&repo_root).unwrap();

        {
            let conn = db::open(&repo_root).unwrap();
            operations::log_operation(&conn, "coord_connect", Some("agent-a"), &serde_json::json!({})).unwrap();
        }

        let snapshot = status(&repo_root).unwrap();
        assert_eq!(snapshot.connected_agent_ids, std::collections::HashSet::from(["agent-a".to_string()]));

        let _ = std::fs::remove_dir_all(db::db_path(&repo_root).unwrap().parent().unwrap());
        let _ = std::fs::remove_dir_all(&repo_root);
    }
}
