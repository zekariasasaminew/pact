use anyhow::Result;
use rusqlite::{Connection, OptionalExtension};
use serde::Serialize;

use crate::db;

#[derive(Debug, Serialize, Clone)]
pub struct Message {
    pub id: i64,
    pub from: String,
    /// `None` means this was a broadcast, not addressed to one agent.
    pub to: Option<String>,
    pub subject: String,
    pub body: String,
    pub created_at: i64,
}

pub fn send_message(
    conn: &Connection,
    from: &str,
    to: Option<&str>,
    subject: &str,
    body: &str,
) -> Result<i64> {
    conn.execute(
        "INSERT INTO messages (from_agent, to_agent, subject, body, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
        (from, to, subject, body, db::now_unix()),
    )?;
    Ok(conn.last_insert_rowid())
}

/// Returns every message addressed to `agent_id` (directly, or broadcast),
/// excluding the agent's own broadcasts, that arrived since this agent last
/// checked, then advances its cursor -- see DESIGN.md ("pact-coord >
/// Per-agent read cursors") for why it's a cursor per agent rather than a
/// shared `read_at` column, and why the cursor advances over every
/// recipient-matching row rather than just the ones actually returned.
pub fn check_messages(conn: &Connection, agent_id: &str) -> Result<Vec<Message>> {
    let last_seen: i64 = conn
        .query_row(
            "SELECT last_seen_message_id FROM read_cursors WHERE agent_id = ?1",
            [agent_id],
            |row| row.get(0),
        )
        .optional()?
        .unwrap_or(0);

    let mut stmt = conn.prepare(
        "SELECT id, from_agent, to_agent, subject, body, created_at
         FROM messages
         WHERE id > ?1 AND (to_agent = ?2 OR to_agent IS NULL)
         ORDER BY id ASC",
    )?;
    let candidates: Vec<Message> = stmt
        .query_map((last_seen, agent_id), |row| {
            Ok(Message {
                id: row.get(0)?,
                from: row.get(1)?,
                to: row.get(2)?,
                subject: row.get(3)?,
                body: row.get(4)?,
                created_at: row.get(5)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;

    if let Some(max_id) = candidates.iter().map(|m| m.id).max() {
        conn.execute(
            "INSERT INTO read_cursors (agent_id, last_seen_message_id) VALUES (?1, ?2)
             ON CONFLICT(agent_id) DO UPDATE SET last_seen_message_id = ?2",
            (agent_id, max_id),
        )?;
    }

    Ok(candidates.into_iter().filter(|m| m.from != agent_id).collect())
}

/// Every agent id the coordination DB has any record of -- anyone who's
/// ever held a lease, sent or received a message, or checked messages at
/// least once. There's no dedicated "agents" table (identity is implicit,
/// a workspace id doubles as its MCP `agent_id`), so this is a union
/// across every place an id can appear.
pub fn known_agent_ids(conn: &Connection) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT DISTINCT agent_id FROM (
            SELECT holder AS agent_id FROM leases
            UNION SELECT from_agent AS agent_id FROM messages
            UNION SELECT to_agent AS agent_id FROM messages WHERE to_agent IS NOT NULL
            UNION SELECT agent_id AS agent_id FROM read_cursors
         ) ORDER BY agent_id",
    )?;
    let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
    rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
}

/// How many messages are waiting for `agent_id` right now -- same
/// recipient-matching logic as `check_messages`, but a read-only count that
/// does *not* advance the cursor. For a status view (issue #64): looking
/// shouldn't change what a later real `check_messages` call sees.
pub fn pending_message_count(conn: &Connection, agent_id: &str) -> Result<i64> {
    let last_seen: i64 = conn
        .query_row(
            "SELECT last_seen_message_id FROM read_cursors WHERE agent_id = ?1",
            [agent_id],
            |row| row.get(0),
        )
        .optional()?
        .unwrap_or(0);

    conn.query_row(
        "SELECT COUNT(*) FROM messages
         WHERE id > ?1 AND from_agent != ?2 AND (to_agent = ?2 OR to_agent IS NULL)",
        (last_seen, agent_id),
        |row| row.get(0),
    )
    .map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Mirrors the real schema `db::open` creates (all three tables --
    /// `known_agent_ids` unions across `leases` too, not just messages).
    fn test_conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE leases (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                pattern TEXT NOT NULL,
                holder TEXT NOT NULL,
                created_at INTEGER NOT NULL,
                expires_at INTEGER NOT NULL
            );
            CREATE TABLE messages (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                from_agent TEXT NOT NULL,
                to_agent TEXT,
                subject TEXT NOT NULL,
                body TEXT NOT NULL,
                created_at INTEGER NOT NULL
            );
            CREATE TABLE read_cursors (
                agent_id TEXT PRIMARY KEY,
                last_seen_message_id INTEGER NOT NULL DEFAULT 0
            );",
        )
        .unwrap();
        conn
    }

    #[test]
    fn check_messages_excludes_callers_own_broadcast() {
        let conn = test_conn();
        send_message(&conn, "agent-a", None, "bcast", "hello").unwrap();

        let messages = check_messages(&conn, "agent-a").unwrap();
        assert!(messages.is_empty(), "agent-a should not see its own broadcast");
    }

    #[test]
    fn check_messages_delivers_broadcast_to_other_agents() {
        let conn = test_conn();
        send_message(&conn, "agent-a", None, "bcast", "hello").unwrap();

        let messages = check_messages(&conn, "agent-b").unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].from, "agent-a");
    }

    #[test]
    fn check_messages_advances_cursor_past_callers_own_broadcast() {
        let conn = test_conn();
        send_message(&conn, "agent-a", None, "bcast1", "one").unwrap();
        assert!(check_messages(&conn, "agent-a").unwrap().is_empty());

        // A second call with nothing new must not re-return anything either
        // -- confirms the cursor advanced past the self-broadcast rather
        // than staying stuck and rescanning it on every call.
        assert!(check_messages(&conn, "agent-a").unwrap().is_empty());

        send_message(&conn, "agent-b", None, "bcast2", "two").unwrap();
        let messages = check_messages(&conn, "agent-a").unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].body, "two");
    }

    #[test]
    fn check_messages_still_excludes_direct_messages_to_other_agents() {
        let conn = test_conn();
        send_message(&conn, "agent-a", Some("agent-b"), "direct", "hi").unwrap();

        assert!(check_messages(&conn, "agent-c").unwrap().is_empty());
        let messages = check_messages(&conn, "agent-b").unwrap();
        assert_eq!(messages.len(), 1);
    }

    #[test]
    fn known_agent_ids_covers_senders_and_recipients() {
        let conn = test_conn();
        send_message(&conn, "agent-a", Some("agent-b"), "direct", "hi").unwrap();
        send_message(&conn, "agent-c", None, "bcast", "hello").unwrap();

        let ids = known_agent_ids(&conn).unwrap();
        assert_eq!(ids, vec!["agent-a", "agent-b", "agent-c"]);
    }

    #[test]
    fn pending_message_count_matches_check_messages_without_advancing_cursor() {
        let conn = test_conn();
        send_message(&conn, "agent-a", None, "bcast1", "one").unwrap();
        send_message(&conn, "agent-a", None, "bcast2", "two").unwrap();

        assert_eq!(pending_message_count(&conn, "agent-b").unwrap(), 2);
        // Checking the count again must see the same thing -- it must not
        // have advanced agent-b's cursor as a side effect.
        assert_eq!(pending_message_count(&conn, "agent-b").unwrap(), 2);

        // check_messages (the real, cursor-advancing read) still sees both.
        let messages = check_messages(&conn, "agent-b").unwrap();
        assert_eq!(messages.len(), 2);
    }

    #[test]
    fn pending_message_count_excludes_own_broadcasts() {
        let conn = test_conn();
        send_message(&conn, "agent-a", None, "bcast", "hello").unwrap();
        assert_eq!(pending_message_count(&conn, "agent-a").unwrap(), 0);
    }

    #[test]
    fn pending_message_count_zero_for_unknown_agent() {
        let conn = test_conn();
        assert_eq!(pending_message_count(&conn, "nobody").unwrap(), 0);
    }
}
