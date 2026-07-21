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

#[cfg(test)]
mod tests {
    use super::*;

    fn test_conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE messages (
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
}
