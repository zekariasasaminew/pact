//! Typed handoff/negotiation protocol between agents (issue #163) -- see
//! DESIGN.md ("pact-coord > Typed handoff/negotiation protocol") for the
//! design rationale (Contract Net Protocol as prior art, why this is
//! polling-based like `messages`, and why `narrowed` doesn't mutate the
//! same row further). A structured alternative to prose `send_message`
//! for "can I take over these files" / "please hold off on this scope"
//! negotiation, with a real status lifecycle instead of an agent having
//! to interpret free text.

use anyhow::{bail, Result};
use rusqlite::{Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

use crate::db;

/// Upper bound on an explicit `ttl_seconds`, same reasoning and same
/// value as `leases::MAX_LEASE_TTL_SECONDS`: a handoff request is meant
/// to self-expire well within one agent session, not become a de facto
/// permanent question mark hanging over a file.
const MAX_HANDOFF_TTL_SECONDS: i64 = 24 * 60 * 60;

/// Default TTL when a caller doesn't specify one -- shorter than leases'
/// 15 minutes: a handoff is a direct question to a specific agent, not a
/// broad advisory claim, so a shorter default expiry means an
/// unresponsive target doesn't leave the requester wondering for as long.
const DEFAULT_HANDOFF_TTL_SECONDS: i64 = 10 * 60;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HandoffStatus {
    Pending,
    Accepted,
    Rejected,
    Narrowed,
    Expired,
    Cancelled,
}

impl HandoffStatus {
    fn parse(raw: &str) -> Self {
        match raw {
            "accepted" => HandoffStatus::Accepted,
            "rejected" => HandoffStatus::Rejected,
            "narrowed" => HandoffStatus::Narrowed,
            "expired" => HandoffStatus::Expired,
            "cancelled" => HandoffStatus::Cancelled,
            _ => HandoffStatus::Pending,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HandoffRequest {
    pub id: i64,
    pub from: String,
    pub to: String,
    pub files: Vec<String>,
    pub message: String,
    pub status: HandoffStatus,
    pub created_at: i64,
    pub expires_at: i64,
    pub responded_at: Option<i64>,
    pub response_message: Option<String>,
    /// Only set when `status == Narrowed` -- the responder's counter-
    /// offered file scope. The requester accepts a narrowed counter-offer
    /// by sending a *new* `request_handoff` scoped to these files, not by
    /// this row changing further -- see the module doc comment.
    pub narrowed_files: Option<Vec<String>>,
}

#[derive(Debug, Serialize)]
pub struct HandoffRequestResult {
    pub request_id: i64,
    pub expires_at: i64,
}

fn row_to_request(row: &rusqlite::Row) -> rusqlite::Result<HandoffRequest> {
    let files_json: String = row.get("requested_files")?;
    let narrowed_json: Option<String> = row.get("narrowed_files")?;
    let status_raw: String = row.get("status")?;
    Ok(HandoffRequest {
        id: row.get("id")?,
        from: row.get("from_agent")?,
        to: row.get("to_agent")?,
        files: serde_json::from_str(&files_json).unwrap_or_default(),
        message: row.get("message")?,
        status: HandoffStatus::parse(&status_raw),
        created_at: row.get("created_at")?,
        expires_at: row.get("expires_at")?,
        responded_at: row.get("responded_at")?,
        response_message: row.get("response_message")?,
        narrowed_files: narrowed_json.and_then(|j| serde_json::from_str(&j).ok()),
    })
}

/// Flips any `pending` row past its `expires_at` to `expired` -- lazy,
/// evaluated on read, mirroring `claim_files`' own `DELETE FROM leases
/// WHERE expires_at <= ?1` pattern rather than a background sweeper
/// (`pact-coord` has none, by design -- see DESIGN.md). An `UPDATE`, not
/// a `DELETE`: unlike a lease, a handoff request's history (who asked
/// whom for what, and how it was left) stays meaningful after expiry.
/// Rows are updated one at a time so each gets its own fresh
/// `activity_seq`, not a value shared across a whole batch -- see
/// `next_activity_seq`.
fn expire_stale(conn: &Connection, now: i64) -> Result<()> {
    let mut stmt = conn.prepare("SELECT id FROM handoff_requests WHERE status = 'pending' AND expires_at <= ?1")?;
    let ids: Vec<i64> = stmt.query_map([now], |row| row.get(0))?.collect::<rusqlite::Result<_>>()?;
    drop(stmt);
    for id in ids {
        conn.execute(
            "UPDATE handoff_requests SET status = 'expired', activity_seq = (SELECT COALESCE(MAX(activity_seq), 0) + 1 FROM handoff_requests) WHERE id = ?1",
            [id],
        )?;
    }
    Ok(())
}

pub fn request_handoff(
    conn: &Connection,
    from: &str,
    to: &str,
    files: &[String],
    message: &str,
    ttl_seconds: Option<i64>,
) -> Result<HandoffRequestResult> {
    if let Some(ttl) = ttl_seconds {
        if ttl <= 0 {
            bail!("ttl_seconds must be positive, got {ttl}");
        }
        if ttl > MAX_HANDOFF_TTL_SECONDS {
            bail!("ttl_seconds must be at most {MAX_HANDOFF_TTL_SECONDS} (24 hours), got {ttl}");
        }
    }
    if files.is_empty() {
        bail!("files must not be empty");
    }

    let now = db::now_unix();
    expire_stale(conn, now)?;

    let ttl = ttl_seconds.unwrap_or(DEFAULT_HANDOFF_TTL_SECONDS);
    let expires_at = now + ttl;
    let files_json = serde_json::to_string(files)?;

    conn.execute(
        "INSERT INTO handoff_requests (from_agent, to_agent, requested_files, message, status, created_at, expires_at, activity_seq)
         VALUES (?1, ?2, ?3, ?4, 'pending', ?5, ?6, (SELECT COALESCE(MAX(activity_seq), 0) + 1 FROM handoff_requests))",
        (from, to, &files_json, message, now, expires_at),
    )?;
    let request_id = conn.last_insert_rowid();

    Ok(HandoffRequestResult { request_id, expires_at })
}

/// New/changed handoff requests relevant to `agent_id`, since it last
/// checked -- cursor-based, same shape as `messages::check_messages`. An
/// incoming request (`to_agent == agent_id`) is relevant the moment it's
/// created (new information: someone wants something from you). An
/// outgoing request (`from_agent == agent_id`) is only relevant once it's
/// left `pending` -- the requester already knows it sent a pending
/// request (the `request_handoff` call itself returned the id), so
/// there's nothing new to report until the target actually responds.
pub fn check_handoffs(conn: &Connection, agent_id: &str) -> Result<Vec<HandoffRequest>> {
    let now = db::now_unix();
    expire_stale(conn, now)?;

    let last_seen: i64 = conn
        .query_row(
            "SELECT last_seen_handoff_id FROM handoff_read_cursors WHERE agent_id = ?1",
            [agent_id],
            |row| row.get(0),
        )
        .optional()?
        .unwrap_or(0);

    let mut stmt = conn.prepare(
        "SELECT id, from_agent, to_agent, requested_files, message, status, created_at, expires_at, responded_at, response_message, narrowed_files, activity_seq
         FROM handoff_requests
         WHERE activity_seq > ?1 AND (to_agent = ?2 OR (from_agent = ?2 AND status != 'pending'))
         ORDER BY activity_seq ASC",
    )?;
    let rows: Vec<(HandoffRequest, i64)> = stmt
        .query_map((last_seen, agent_id), |row| Ok((row_to_request(row)?, row.get::<_, i64>("activity_seq")?)))?
        .collect::<rusqlite::Result<_>>()?;

    if let Some(max_seq) = rows.iter().map(|(_, seq)| *seq).max() {
        conn.execute(
            "INSERT INTO handoff_read_cursors (agent_id, last_seen_handoff_id) VALUES (?1, ?2)
             ON CONFLICT(agent_id) DO UPDATE SET last_seen_handoff_id = ?2",
            (agent_id, max_seq),
        )?;
    }

    Ok(rows.into_iter().map(|(req, _)| req).collect())
}

pub enum HandoffDecision {
    Accept,
    Reject,
    Narrow(Vec<String>),
}

/// Responds to a pending handoff request -- only the request's own
/// `to_agent` may respond (checked explicitly, not just assumed from
/// context), and only while it's still genuinely `pending` (an
/// already-expired/responded/cancelled request errors rather than
/// silently overwriting a real prior outcome).
pub fn respond_handoff(
    conn: &Connection,
    request_id: i64,
    responder_agent_id: &str,
    decision: HandoffDecision,
    response_message: Option<&str>,
) -> Result<()> {
    let now = db::now_unix();
    expire_stale(conn, now)?;

    let (to_agent, status): (String, String) = conn
        .query_row(
            "SELECT to_agent, status FROM handoff_requests WHERE id = ?1",
            [request_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?
        .ok_or_else(|| anyhow::anyhow!("no handoff request with id {request_id}"))?;

    if to_agent != responder_agent_id {
        bail!("handoff request {request_id} was not addressed to '{responder_agent_id}' (addressed to '{to_agent}')");
    }
    if status != "pending" {
        bail!("handoff request {request_id} is no longer pending (status: {status})");
    }

    let (new_status, narrowed_json): (&str, Option<String>) = match decision {
        HandoffDecision::Accept => ("accepted", None),
        HandoffDecision::Reject => ("rejected", None),
        HandoffDecision::Narrow(files) => {
            if files.is_empty() {
                bail!("narrowed_files must not be empty when narrowing a request");
            }
            ("narrowed", Some(serde_json::to_string(&files)?))
        }
    };

    conn.execute(
        "UPDATE handoff_requests
         SET status = ?1, responded_at = ?2, response_message = ?3, narrowed_files = ?4,
             activity_seq = (SELECT COALESCE(MAX(activity_seq), 0) + 1 FROM handoff_requests)
         WHERE id = ?5",
        (new_status, now, response_message, narrowed_json, request_id),
    )?;

    Ok(())
}

/// Best-effort teardown integration (issue #163's own design note): every
/// still-`pending` request addressed *to* `agent_id` is marked
/// `cancelled` -- a workspace that's gone can never respond, so anyone
/// waiting on it should find out now rather than only after the TTL
/// lapses. Deliberately scoped to incoming requests only: an outgoing
/// request `agent_id` itself sent, still awaiting a response, is left
/// alone -- the target may still meaningfully act on it even though the
/// requester is gone (a real, if lesser, use). Returns how many rows
/// were cancelled.
pub fn cancel_pending_handoffs_to(conn: &Connection, agent_id: &str) -> Result<usize> {
    let now = db::now_unix();
    let mut stmt = conn.prepare("SELECT id FROM handoff_requests WHERE to_agent = ?1 AND status = 'pending'")?;
    let ids: Vec<i64> = stmt.query_map([agent_id], |row| row.get(0))?.collect::<rusqlite::Result<_>>()?;
    drop(stmt);
    for &id in &ids {
        conn.execute(
            "UPDATE handoff_requests
             SET status = 'cancelled', responded_at = ?1,
                 activity_seq = (SELECT COALESCE(MAX(activity_seq), 0) + 1 FROM handoff_requests)
             WHERE id = ?2",
            (now, id),
        )?;
    }
    Ok(ids.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE handoff_requests (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                from_agent TEXT NOT NULL,
                to_agent TEXT NOT NULL,
                requested_files TEXT NOT NULL,
                message TEXT NOT NULL,
                status TEXT NOT NULL DEFAULT 'pending',
                created_at INTEGER NOT NULL,
                expires_at INTEGER NOT NULL,
                responded_at INTEGER,
                response_message TEXT,
                narrowed_files TEXT,
                activity_seq INTEGER NOT NULL DEFAULT 0
            );
            CREATE TABLE handoff_read_cursors (
                agent_id TEXT PRIMARY KEY,
                last_seen_handoff_id INTEGER NOT NULL DEFAULT 0
            );",
        )
        .unwrap();
        conn
    }

    #[test]
    fn request_handoff_rejects_empty_files() {
        let conn = test_conn();
        let err = request_handoff(&conn, "a", "b", &[], "please", None).unwrap_err();
        assert!(err.to_string().contains("must not be empty"));
    }

    #[test]
    fn request_handoff_rejects_non_positive_ttl() {
        let conn = test_conn();
        let err = request_handoff(&conn, "a", "b", &["x.txt".to_string()], "please", Some(0)).unwrap_err();
        assert!(err.to_string().contains("must be positive"));
    }

    #[test]
    fn request_handoff_rejects_ttl_above_24_hours() {
        let conn = test_conn();
        let err = request_handoff(&conn, "a", "b", &["x.txt".to_string()], "please", Some(9_999_999)).unwrap_err();
        assert!(err.to_string().contains("at most"));
    }

    #[test]
    fn target_sees_a_new_pending_request_immediately() {
        let conn = test_conn();
        request_handoff(&conn, "agent-a", "agent-b", &["x.txt".to_string()], "please", None).unwrap();

        let incoming = check_handoffs(&conn, "agent-b").unwrap();
        assert_eq!(incoming.len(), 1);
        assert_eq!(incoming[0].from, "agent-a");
        assert!(matches!(incoming[0].status, HandoffStatus::Pending));
    }

    #[test]
    fn requester_does_not_see_their_own_still_pending_request() {
        let conn = test_conn();
        request_handoff(&conn, "agent-a", "agent-b", &["x.txt".to_string()], "please", None).unwrap();

        let outgoing = check_handoffs(&conn, "agent-a").unwrap();
        assert!(outgoing.is_empty(), "expected no self-echo of a still-pending outgoing request, got: {outgoing:?}");
    }

    #[test]
    fn requester_sees_the_response_once_the_target_replies() {
        let conn = test_conn();
        let result = request_handoff(&conn, "agent-a", "agent-b", &["x.txt".to_string()], "please", None).unwrap();
        // agent-b's own check_handoffs call advances its cursor past the
        // request -- confirms the requester's later visibility doesn't
        // depend on the target having polled first.
        check_handoffs(&conn, "agent-b").unwrap();

        respond_handoff(&conn, result.request_id, "agent-b", HandoffDecision::Accept, Some("sure, go ahead")).unwrap();

        let outgoing = check_handoffs(&conn, "agent-a").unwrap();
        assert_eq!(outgoing.len(), 1);
        assert!(matches!(outgoing[0].status, HandoffStatus::Accepted));
        assert_eq!(outgoing[0].response_message.as_deref(), Some("sure, go ahead"));
    }

    #[test]
    fn respond_handoff_rejects_a_responder_who_is_not_the_target() {
        let conn = test_conn();
        let result = request_handoff(&conn, "agent-a", "agent-b", &["x.txt".to_string()], "please", None).unwrap();

        let err = respond_handoff(&conn, result.request_id, "agent-c", HandoffDecision::Accept, None).unwrap_err();
        assert!(err.to_string().contains("not addressed to"));
    }

    #[test]
    fn respond_handoff_rejects_responding_twice() {
        let conn = test_conn();
        let result = request_handoff(&conn, "agent-a", "agent-b", &["x.txt".to_string()], "please", None).unwrap();
        respond_handoff(&conn, result.request_id, "agent-b", HandoffDecision::Reject, None).unwrap();

        let err = respond_handoff(&conn, result.request_id, "agent-b", HandoffDecision::Accept, None).unwrap_err();
        assert!(err.to_string().contains("no longer pending"));
    }

    #[test]
    fn narrow_carries_the_counter_offered_scope() {
        let conn = test_conn();
        let result = request_handoff(&conn, "agent-a", "agent-b", &["src/*.rs".to_string()], "please", None).unwrap();
        respond_handoff(
            &conn,
            result.request_id,
            "agent-b",
            HandoffDecision::Narrow(vec!["src/only_this.rs".to_string()]),
            Some("only this one file is free"),
        )
        .unwrap();

        let outgoing = check_handoffs(&conn, "agent-a").unwrap();
        assert_eq!(outgoing.len(), 1);
        assert!(matches!(outgoing[0].status, HandoffStatus::Narrowed));
        assert_eq!(outgoing[0].narrowed_files, Some(vec!["src/only_this.rs".to_string()]));
    }

    #[test]
    fn narrow_rejects_an_empty_counter_offer() {
        let conn = test_conn();
        let result = request_handoff(&conn, "agent-a", "agent-b", &["src/*.rs".to_string()], "please", None).unwrap();

        let err = respond_handoff(&conn, result.request_id, "agent-b", HandoffDecision::Narrow(vec![]), None).unwrap_err();
        assert!(err.to_string().contains("must not be empty"));
    }

    #[test]
    fn an_expired_request_is_visible_to_the_target_as_expired_not_pending() {
        let conn = test_conn();
        let now = db::now_unix();
        conn.execute(
            "INSERT INTO handoff_requests (from_agent, to_agent, requested_files, message, status, created_at, expires_at, activity_seq)
             VALUES ('agent-a', 'agent-b', '[\"x.txt\"]', 'please', 'pending', ?1, ?2, 1)",
            (now - 100, now - 1),
        )
        .unwrap();

        let incoming = check_handoffs(&conn, "agent-b").unwrap();
        assert_eq!(incoming.len(), 1);
        assert!(matches!(incoming[0].status, HandoffStatus::Expired));
    }

    #[test]
    fn respond_handoff_refuses_an_already_expired_request() {
        let conn = test_conn();
        let now = db::now_unix();
        conn.execute(
            "INSERT INTO handoff_requests (from_agent, to_agent, requested_files, message, status, created_at, expires_at, activity_seq)
             VALUES ('agent-a', 'agent-b', '[\"x.txt\"]', 'please', 'pending', ?1, ?2, 1)",
            (now - 100, now - 1),
        )
        .unwrap();
        let id = conn.last_insert_rowid();

        let err = respond_handoff(&conn, id, "agent-b", HandoffDecision::Accept, None).unwrap_err();
        assert!(err.to_string().contains("no longer pending"));
    }

    #[test]
    fn cancel_pending_handoffs_to_cancels_only_incoming_pending_requests() {
        let conn = test_conn();
        // Incoming to agent-b -- must be cancelled.
        request_handoff(&conn, "agent-a", "agent-b", &["x.txt".to_string()], "please", None).unwrap();
        // Outgoing from agent-b -- must be left alone.
        request_handoff(&conn, "agent-b", "agent-c", &["y.txt".to_string()], "please", None).unwrap();

        let cancelled = cancel_pending_handoffs_to(&conn, "agent-b").unwrap();
        assert_eq!(cancelled, 1);

        let incoming = check_handoffs(&conn, "agent-b").unwrap();
        assert_eq!(incoming.len(), 1);
        assert!(matches!(incoming[0].status, HandoffStatus::Cancelled));

        let still_pending_from_b = check_handoffs(&conn, "agent-c").unwrap();
        assert_eq!(still_pending_from_b.len(), 1);
        assert!(matches!(still_pending_from_b[0].status, HandoffStatus::Pending));
    }

    #[test]
    fn cancel_pending_handoffs_to_is_a_no_op_with_nothing_pending() {
        let conn = test_conn();
        assert_eq!(cancel_pending_handoffs_to(&conn, "agent-b").unwrap(), 0);
    }
}
