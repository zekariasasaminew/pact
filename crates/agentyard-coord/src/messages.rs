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

/// Returns every message addressed to `agent_id` (directly, or broadcast)
/// that arrived since this agent last checked, then advances its cursor.
/// A cursor per agent (rather than a shared `read_at` column on the
/// message itself) is what makes broadcasts work correctly: each recipient
/// needs to see a message once independently of whether other recipients
/// have already seen it, which a single mutable "read" flag on the row
/// can't represent.
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
    let messages: Vec<Message> = stmt
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

    if let Some(max_id) = messages.iter().map(|m| m.id).max() {
        conn.execute(
            "INSERT INTO read_cursors (agent_id, last_seen_message_id) VALUES (?1, ?2)
             ON CONFLICT(agent_id) DO UPDATE SET last_seen_message_id = ?2",
            (agent_id, max_id),
        )?;
    }

    Ok(messages)
}
