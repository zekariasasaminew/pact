use rusqlite::Connection;
use serde::Serialize;
use serde_json::Value;

use crate::db;

/// One already-happened, significant coordination-layer event -- see
/// DESIGN.md ("pact-coord > Operation log / `pact history` (issue #84)")
/// for the full set of `op_type` values and what's deliberately excluded
/// (`check_messages`, a read, isn't logged).
#[derive(Debug, Clone, Serialize)]
pub struct Operation {
    pub id: i64,
    pub created_at: i64,
    pub op_type: String,
    pub workspace_id: Option<String>,
    pub detail: Value,
}

pub fn log_operation(
    conn: &Connection,
    op_type: &str,
    workspace_id: Option<&str>,
    detail: &Value,
) -> anyhow::Result<()> {
    conn.execute(
        "INSERT INTO operations (created_at, op_type, workspace_id, detail) VALUES (?1, ?2, ?3, ?4)",
        (db::now_unix(), op_type, workspace_id, detail.to_string()),
    )?;
    Ok(())
}

#[derive(Debug, Clone, Default)]
pub struct HistoryFilter {
    pub workspace_id: Option<String>,
    pub since: Option<i64>,
    pub op_type: Option<String>,
    pub limit: Option<i64>,
}

pub fn query_operations(conn: &Connection, filter: &HistoryFilter) -> anyhow::Result<Vec<Operation>> {
    let mut sql = String::from("SELECT id, created_at, op_type, workspace_id, detail FROM operations WHERE 1=1");
    let mut params: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();

    if let Some(workspace_id) = &filter.workspace_id {
        sql.push_str(" AND workspace_id = ?");
        params.push(Box::new(workspace_id.clone()));
    }
    if let Some(since) = filter.since {
        sql.push_str(" AND created_at >= ?");
        params.push(Box::new(since));
    }
    if let Some(op_type) = &filter.op_type {
        sql.push_str(" AND op_type = ?");
        params.push(Box::new(op_type.clone()));
    }
    sql.push_str(" ORDER BY id DESC");
    if let Some(limit) = filter.limit {
        sql.push_str(" LIMIT ?");
        params.push(Box::new(limit));
    }

    let mut stmt = conn.prepare(&sql)?;
    let param_refs: Vec<&dyn rusqlite::ToSql> = params.iter().map(|p| p.as_ref()).collect();
    let rows = stmt.query_map(param_refs.as_slice(), |row| {
        let detail_text: String = row.get(4)?;
        Ok(Operation {
            id: row.get(0)?,
            created_at: row.get(1)?,
            op_type: row.get(2)?,
            workspace_id: row.get(3)?,
            detail: serde_json::from_str(&detail_text).unwrap_or(Value::Null),
        })
    })?;
    rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE operations (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                created_at INTEGER NOT NULL,
                op_type TEXT NOT NULL,
                workspace_id TEXT,
                detail TEXT NOT NULL
            );",
        )
        .unwrap();
        conn
    }

    #[test]
    fn log_and_query_a_single_operation() {
        let conn = test_conn();
        log_operation(&conn, "claim", Some("ws-a"), &serde_json::json!({"patterns": ["a.txt"]})).unwrap();

        let ops = query_operations(&conn, &HistoryFilter::default()).unwrap();
        assert_eq!(ops.len(), 1);
        assert_eq!(ops[0].op_type, "claim");
        assert_eq!(ops[0].workspace_id.as_deref(), Some("ws-a"));
        assert_eq!(ops[0].detail["patterns"][0], "a.txt");
    }

    #[test]
    fn query_filters_by_workspace_id() {
        let conn = test_conn();
        log_operation(&conn, "claim", Some("ws-a"), &serde_json::json!({})).unwrap();
        log_operation(&conn, "claim", Some("ws-b"), &serde_json::json!({})).unwrap();

        let filter = HistoryFilter { workspace_id: Some("ws-a".to_string()), ..Default::default() };
        let ops = query_operations(&conn, &filter).unwrap();
        assert_eq!(ops.len(), 1);
        assert_eq!(ops[0].workspace_id.as_deref(), Some("ws-a"));
    }

    #[test]
    fn query_filters_by_op_type() {
        let conn = test_conn();
        log_operation(&conn, "claim", Some("ws-a"), &serde_json::json!({})).unwrap();
        log_operation(&conn, "release", Some("ws-a"), &serde_json::json!({})).unwrap();

        let filter = HistoryFilter { op_type: Some("release".to_string()), ..Default::default() };
        let ops = query_operations(&conn, &filter).unwrap();
        assert_eq!(ops.len(), 1);
        assert_eq!(ops[0].op_type, "release");
    }

    #[test]
    fn query_filters_by_since() {
        let conn = test_conn();
        conn.execute(
            "INSERT INTO operations (created_at, op_type, workspace_id, detail) VALUES (100, 'claim', 'ws-a', '{}')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO operations (created_at, op_type, workspace_id, detail) VALUES (200, 'claim', 'ws-a', '{}')",
            [],
        )
        .unwrap();

        let filter = HistoryFilter { since: Some(150), ..Default::default() };
        let ops = query_operations(&conn, &filter).unwrap();
        assert_eq!(ops.len(), 1);
        assert_eq!(ops[0].created_at, 200);
    }

    #[test]
    fn query_respects_limit_and_orders_newest_first() {
        let conn = test_conn();
        for i in 0..5 {
            log_operation(&conn, "claim", Some("ws-a"), &serde_json::json!({"i": i})).unwrap();
        }

        let filter = HistoryFilter { limit: Some(2), ..Default::default() };
        let ops = query_operations(&conn, &filter).unwrap();
        assert_eq!(ops.len(), 2);
        assert_eq!(ops[0].detail["i"], 4, "expected newest-first ordering");
        assert_eq!(ops[1].detail["i"], 3);
    }

    #[test]
    fn query_with_no_filters_returns_everything() {
        let conn = test_conn();
        log_operation(&conn, "claim", Some("ws-a"), &serde_json::json!({})).unwrap();
        log_operation(&conn, "teardown", None, &serde_json::json!({})).unwrap();

        let ops = query_operations(&conn, &HistoryFilter::default()).unwrap();
        assert_eq!(ops.len(), 2);
    }
}
