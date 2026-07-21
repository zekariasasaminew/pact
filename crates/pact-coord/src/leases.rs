use std::collections::HashSet;
use std::path::Path;

use anyhow::{bail, Result};
use rusqlite::Connection;
use serde::Serialize;

use crate::db::{self, DEFAULT_LEASE_TTL_SECONDS};

/// Upper bound on an explicit `ttl_seconds` -- a lease is meant to
/// self-expire well within one agent session, not become a de facto
/// permanent lock. 24 hours comfortably covers any real session while still
/// catching the obviously-wrong case (a caller passing milliseconds where
/// seconds were expected lands well past this).
const MAX_LEASE_TTL_SECONDS: i64 = 24 * 60 * 60;

#[derive(Debug, Serialize, Clone)]
pub struct Conflict {
    pub holder: String,
    pub pattern: String,
    /// A few concrete file paths responsible for the overlap, not the full
    /// set -- enough for a human/agent reading the tool result to
    /// understand *why* it conflicts, without dumping potentially
    /// thousands of matched paths for a broad glob.
    pub example_files: Vec<String>,
}

#[derive(Debug, Serialize, Clone)]
pub struct ClaimResult {
    pub granted: bool,
    pub expires_at: i64,
    /// Non-empty means another agent holds an overlapping claim -- this is
    /// advisory, not enforced: the lease is granted either way (see the
    /// README's design-decision writeup for why), the caller decides what
    /// to do with the warning.
    pub conflicts: Vec<Conflict>,
}

/// Expands a glob pattern against every file currently in `root`, returning
/// paths relative to `root` with forward slashes (normalized across
/// platforms). Overlap between two glob patterns is detected by expanding
/// both to concrete file sets and intersecting them -- plain
/// pattern-string comparison would miss the common case of two patterns
/// that aren't equal but still overlap (`src/**/*.rs` vs `src/foo.rs`).
fn expand_glob(root: &Path, pattern: &str) -> Result<HashSet<String>> {
    let matcher = globset::GlobBuilder::new(pattern)
        .literal_separator(false)
        .build()?
        .compile_matcher();

    let mut matched = HashSet::new();
    for entry in walkdir::WalkDir::new(root).into_iter().filter_map(|e| e.ok()) {
        if !entry.file_type().is_file() {
            continue;
        }
        let Ok(rel) = entry.path().strip_prefix(root) else {
            continue;
        };
        let rel_str = rel.to_string_lossy().replace('\\', "/");
        if matcher.is_match(&rel_str) {
            matched.insert(rel_str);
        }
    }
    Ok(matched)
}

pub fn claim_files(
    conn: &Connection,
    workspace_root: &Path,
    holder: &str,
    patterns: &[String],
    ttl_seconds: Option<i64>,
) -> Result<ClaimResult> {
    if let Some(ttl) = ttl_seconds {
        if ttl <= 0 {
            bail!("ttl_seconds must be positive, got {ttl}");
        }
        if ttl > MAX_LEASE_TTL_SECONDS {
            bail!(
                "ttl_seconds must be at most {MAX_LEASE_TTL_SECONDS} (24 hours), got {ttl}"
            );
        }
    }

    let now = db::now_unix();
    conn.execute("DELETE FROM leases WHERE expires_at <= ?1", [now])?;

    let ttl = ttl_seconds.unwrap_or(DEFAULT_LEASE_TTL_SECONDS);
    let expires_at = now + ttl;

    let mut requested_files: HashSet<String> = HashSet::new();
    for pattern in patterns {
        requested_files.extend(expand_glob(workspace_root, pattern)?);
    }

    let mut conflicts = Vec::new();
    {
        let mut stmt =
            conn.prepare("SELECT pattern, holder FROM leases WHERE holder != ?1 AND expires_at > ?2")?;
        let rows = stmt.query_map((holder, now), |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        for row in rows {
            let (existing_pattern, existing_holder) = row?;
            let existing_files = expand_glob(workspace_root, &existing_pattern)?;
            let overlap: Vec<String> = requested_files
                .intersection(&existing_files)
                .take(5)
                .cloned()
                .collect();
            if !overlap.is_empty() {
                conflicts.push(Conflict {
                    holder: existing_holder,
                    pattern: existing_pattern,
                    example_files: overlap,
                });
            }
        }
    }

    for pattern in patterns {
        conn.execute(
            "INSERT INTO leases (pattern, holder, created_at, expires_at) VALUES (?1, ?2, ?3, ?4)",
            (pattern, holder, now, expires_at),
        )?;
    }

    Ok(ClaimResult {
        granted: true,
        expires_at,
        conflicts,
    })
}

pub fn release_files(conn: &Connection, holder: &str, patterns: &[String]) -> Result<usize> {
    let mut released = 0;
    for pattern in patterns {
        released += conn.execute(
            "DELETE FROM leases WHERE holder = ?1 AND pattern = ?2",
            (holder, pattern),
        )?;
    }
    Ok(released)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE leases (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                pattern TEXT NOT NULL,
                holder TEXT NOT NULL,
                created_at INTEGER NOT NULL,
                expires_at INTEGER NOT NULL
            );",
        )
        .unwrap();
        conn
    }

    #[test]
    fn claim_files_rejects_negative_ttl() {
        let conn = test_conn();
        let root = std::env::temp_dir();
        let err = claim_files(&conn, &root, "agent-a", &[], Some(-1)).unwrap_err();
        assert!(err.to_string().contains("must be positive"), "unexpected error: {err}");
    }

    #[test]
    fn claim_files_rejects_zero_ttl() {
        let conn = test_conn();
        let root = std::env::temp_dir();
        let err = claim_files(&conn, &root, "agent-a", &[], Some(0)).unwrap_err();
        assert!(err.to_string().contains("must be positive"), "unexpected error: {err}");
    }

    #[test]
    fn claim_files_rejects_ttl_above_24_hours() {
        let conn = test_conn();
        let root = std::env::temp_dir();
        let err = claim_files(&conn, &root, "agent-a", &[], Some(9_999_999_999)).unwrap_err();
        assert!(err.to_string().contains("at most"), "unexpected error: {err}");
    }

    #[test]
    fn claim_files_accepts_ttl_within_bounds() {
        let conn = test_conn();
        let root = std::env::temp_dir();
        let result = claim_files(&conn, &root, "agent-a", &[], Some(3600)).unwrap();
        assert!(result.granted);
    }

    #[test]
    fn claim_files_accepts_default_ttl_when_omitted() {
        let conn = test_conn();
        let root = std::env::temp_dir();
        let result = claim_files(&conn, &root, "agent-a", &[], None).unwrap();
        assert!(result.granted);
    }
}
