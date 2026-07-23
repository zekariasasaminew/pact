use rusqlite::Connection;
use serde::Serialize;

use crate::db;

/// A real merge conflict `merge-all` skipped, persisted so it can be
/// retried later without re-running the whole batch -- see DESIGN.md
/// ("pact-coord > Persisted conflicts / `pact resolve` (issue #85)").
/// Deliberately named `PersistedConflict`, not `Conflict`: that name is
/// already taken by `leases::Conflict` (a lease-overlap warning), a
/// completely different concept -- see the DESIGN.md section for the
/// third, also-unrelated `Conflict` in this codebase
/// (`pact_core::FileConflict`, issue #8's cross-workspace file-touch
/// report).
#[derive(Debug, Clone, Serialize)]
pub struct PersistedConflict {
    pub id: i64,
    pub workspace_id: String,
    pub target_branch: String,
    pub files: Vec<String>,
    pub created_at: i64,
    /// "open" | "resolved" | "abandoned".
    pub status: String,
    pub resolved_at: Option<i64>,
}

pub fn record_conflict(
    conn: &Connection,
    workspace_id: &str,
    target_branch: &str,
    files: &[String],
) -> anyhow::Result<i64> {
    let files_json = serde_json::to_string(files)?;
    conn.execute(
        "INSERT INTO conflicts (workspace_id, target_branch, files, created_at, status) VALUES (?1, ?2, ?3, ?4, 'open')",
        (workspace_id, target_branch, files_json, db::now_unix()),
    )?;
    Ok(conn.last_insert_rowid())
}

pub fn list_open_conflicts(conn: &Connection) -> anyhow::Result<Vec<PersistedConflict>> {
    let mut stmt = conn.prepare(
        "SELECT id, workspace_id, target_branch, files, created_at, status, resolved_at \
         FROM conflicts WHERE status = 'open' ORDER BY id DESC",
    )?;
    let rows = stmt.query_map([], row_to_conflict)?;
    rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
}

/// The most recently recorded open conflict for `workspace_id`, if any --
/// what `pact resolve <id>` (no explicit conflict-row id) acts on.
pub fn open_conflict_for_workspace(conn: &Connection, workspace_id: &str) -> anyhow::Result<Option<PersistedConflict>> {
    let mut stmt = conn.prepare(
        "SELECT id, workspace_id, target_branch, files, created_at, status, resolved_at \
         FROM conflicts WHERE workspace_id = ?1 AND status = 'open' ORDER BY id DESC LIMIT 1",
    )?;
    let mut rows = stmt.query_map([workspace_id], row_to_conflict)?;
    rows.next().transpose().map_err(Into::into)
}

pub fn mark_resolved(conn: &Connection, id: i64) -> anyhow::Result<()> {
    conn.execute(
        "UPDATE conflicts SET status = 'resolved', resolved_at = ?2 WHERE id = ?1",
        (id, db::now_unix()),
    )?;
    Ok(())
}

pub fn mark_abandoned(conn: &Connection, id: i64) -> anyhow::Result<()> {
    conn.execute(
        "UPDATE conflicts SET status = 'abandoned', resolved_at = ?2 WHERE id = ?1",
        (id, db::now_unix()),
    )?;
    Ok(())
}

fn row_to_conflict(row: &rusqlite::Row) -> rusqlite::Result<PersistedConflict> {
    let files_json: String = row.get(3)?;
    let files: Vec<String> = serde_json::from_str(&files_json).unwrap_or_default();
    Ok(PersistedConflict {
        id: row.get(0)?,
        workspace_id: row.get(1)?,
        target_branch: row.get(2)?,
        files,
        created_at: row.get(4)?,
        status: row.get(5)?,
        resolved_at: row.get(6)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE conflicts (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                workspace_id TEXT NOT NULL,
                target_branch TEXT NOT NULL,
                files TEXT NOT NULL,
                created_at INTEGER NOT NULL,
                status TEXT NOT NULL DEFAULT 'open',
                resolved_at INTEGER
            );",
        )
        .unwrap();
        conn
    }

    #[test]
    fn record_and_list_an_open_conflict() {
        let conn = test_conn();
        record_conflict(&conn, "ws-a", "pact/merged-x", &["src/index.ts".to_string()]).unwrap();

        let open = list_open_conflicts(&conn).unwrap();
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].workspace_id, "ws-a");
        assert_eq!(open[0].files, vec!["src/index.ts".to_string()]);
        assert_eq!(open[0].status, "open");
    }

    #[test]
    fn resolved_conflicts_are_excluded_from_open_list() {
        let conn = test_conn();
        let id = record_conflict(&conn, "ws-a", "pact/merged-x", &[]).unwrap();
        mark_resolved(&conn, id).unwrap();

        assert!(list_open_conflicts(&conn).unwrap().is_empty());
    }

    #[test]
    fn abandoned_conflicts_are_excluded_from_open_list() {
        let conn = test_conn();
        let id = record_conflict(&conn, "ws-a", "pact/merged-x", &[]).unwrap();
        mark_abandoned(&conn, id).unwrap();

        assert!(list_open_conflicts(&conn).unwrap().is_empty());
    }

    #[test]
    fn open_conflict_for_workspace_returns_the_most_recent_one() {
        let conn = test_conn();
        record_conflict(&conn, "ws-a", "pact/merged-x", &["first.ts".to_string()]).unwrap();
        record_conflict(&conn, "ws-a", "pact/merged-y", &["second.ts".to_string()]).unwrap();

        let found = open_conflict_for_workspace(&conn, "ws-a").unwrap().unwrap();
        assert_eq!(found.files, vec!["second.ts".to_string()]);
    }

    #[test]
    fn open_conflict_for_workspace_returns_none_when_there_is_no_open_conflict() {
        let conn = test_conn();
        assert!(open_conflict_for_workspace(&conn, "ws-a").unwrap().is_none());
    }
}
