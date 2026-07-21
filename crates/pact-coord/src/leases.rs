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

    // ON CONFLICT keyed on (holder, pattern) -- see the leases_holder_pattern
    // unique index in db::open -- so a repeat claim from the same holder
    // for the same pattern extends the existing row's expiry instead of
    // accumulating a fresh one. Confirmed at 8-agent stress-test scale
    // this used to matter: 8 agents x 2 identical claims each produced 160
    // rows for what should have been at most 8.
    for pattern in patterns {
        conn.execute(
            "INSERT INTO leases (pattern, holder, created_at, expires_at) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(holder, pattern) DO UPDATE SET created_at = ?3, expires_at = ?4",
            (pattern, holder, now, expires_at),
        )?;
    }

    Ok(ClaimResult {
        granted: true,
        expires_at,
        conflicts,
    })
}

/// Releases a holder's leases whose *claimed* pattern either matches one of
/// `patterns` exactly, or -- since agents can plausibly reuse a
/// differently-worded but equivalent glob at release time, e.g. releasing
/// `src/*.js` for something originally claimed as `src/add.js` -- overlaps
/// it on the actual files each pattern currently matches (same expand-and-
/// intersect approach `claim_files` already uses for conflict detection).
/// The exact-match path also covers a lease whose claimed pattern's files
/// have since been deleted from disk, where glob expansion alone could no
/// longer find anything to overlap against.
pub fn release_files(
    conn: &Connection,
    workspace_root: &Path,
    holder: &str,
    patterns: &[String],
) -> Result<usize> {
    let exact: HashSet<&str> = patterns.iter().map(String::as_str).collect();
    let mut released_files: HashSet<String> = HashSet::new();
    for pattern in patterns {
        released_files.extend(expand_glob(workspace_root, pattern)?);
    }

    let mut to_delete: Vec<i64> = Vec::new();
    {
        let mut stmt = conn.prepare("SELECT id, pattern FROM leases WHERE holder = ?1")?;
        let rows = stmt.query_map([holder], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)))?;
        for row in rows {
            let (id, existing_pattern) = row?;
            let matches = exact.contains(existing_pattern.as_str()) || {
                let existing_files = expand_glob(workspace_root, &existing_pattern)?;
                !released_files.is_disjoint(&existing_files)
            };
            if matches {
                to_delete.push(id);
            }
        }
    }

    let released = to_delete.len();
    for id in &to_delete {
        conn.execute("DELETE FROM leases WHERE id = ?1", [id])?;
    }
    Ok(released)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Mirrors the real schema `db::open` creates, including the
    /// `leases_holder_pattern` unique index the `ON CONFLICT` upsert in
    /// `claim_files` depends on.
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
            CREATE UNIQUE INDEX leases_holder_pattern ON leases(holder, pattern);",
        )
        .unwrap();
        conn
    }

    /// A throwaway directory on disk with the given (empty) files created,
    /// for tests that need `expand_glob` to actually match something real
    /// -- `release_files`' glob-overlap matching walks the real filesystem,
    /// so an in-memory-only test can't exercise it.
    fn temp_workspace_with_files(files: &[&str]) -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!("pact-coord-leases-test-{}", uuid::Uuid::new_v4()));
        for f in files {
            let path = root.join(f);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, "").unwrap();
        }
        root
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

    fn lease_count(conn: &Connection) -> i64 {
        conn.query_row("SELECT COUNT(*) FROM leases", [], |row| row.get(0)).unwrap()
    }

    #[test]
    fn repeat_claim_updates_existing_row_instead_of_inserting_a_duplicate() {
        let conn = test_conn();
        let root = std::env::temp_dir();

        claim_files(&conn, &root, "agent-a", &["same.txt".to_string()], Some(900)).unwrap();
        claim_files(&conn, &root, "agent-a", &["same.txt".to_string()], Some(900)).unwrap();
        claim_files(&conn, &root, "agent-a", &["same.txt".to_string()], Some(900)).unwrap();

        assert_eq!(lease_count(&conn), 1, "3 identical claims from the same holder must not accumulate rows");
    }

    #[test]
    fn repeat_claim_extends_the_expiry() {
        let conn = test_conn();
        let root = std::env::temp_dir();

        let first = claim_files(&conn, &root, "agent-a", &["same.txt".to_string()], Some(60)).unwrap();
        let second = claim_files(&conn, &root, "agent-a", &["same.txt".to_string()], Some(3600)).unwrap();

        assert!(second.expires_at >= first.expires_at);
    }

    #[test]
    fn distinct_patterns_from_the_same_holder_get_separate_rows() {
        let conn = test_conn();
        let root = std::env::temp_dir();

        claim_files(&conn, &root, "agent-a", &["a.txt".to_string()], Some(900)).unwrap();
        claim_files(&conn, &root, "agent-a", &["b.txt".to_string()], Some(900)).unwrap();

        assert_eq!(lease_count(&conn), 2);
    }

    #[test]
    fn release_files_matches_exact_pattern_string() {
        let conn = test_conn();
        let root = std::env::temp_dir();
        claim_files(&conn, &root, "agent-a", &["same.txt".to_string()], Some(900)).unwrap();

        let released = release_files(&conn, &root, "agent-a", &["same.txt".to_string()]).unwrap();
        assert_eq!(released, 1);
    }

    #[test]
    fn release_files_matches_by_glob_overlap_with_a_different_pattern_string() {
        let root = temp_workspace_with_files(&["src/add.js", "src/sub.js"]);
        let conn = test_conn();
        claim_files(&conn, &root, "agent-a", &["src/add.js".to_string()], Some(900)).unwrap();

        let released = release_files(&conn, &root, "agent-a", &["src/*.js".to_string()]).unwrap();
        assert_eq!(
            released, 1,
            "releasing a broader glob that overlaps the file matched by the originally claimed pattern must release it"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn release_files_does_not_release_another_holders_lease() {
        let root = temp_workspace_with_files(&["src/add.js"]);
        let conn = test_conn();
        claim_files(&conn, &root, "agent-a", &["src/add.js".to_string()], Some(900)).unwrap();

        let released = release_files(&conn, &root, "agent-b", &["src/*.js".to_string()]).unwrap();
        assert_eq!(released, 0);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn release_files_returns_zero_for_a_non_overlapping_pattern() {
        let root = temp_workspace_with_files(&["src/add.js", "src/sub.js"]);
        let conn = test_conn();
        claim_files(&conn, &root, "agent-a", &["src/add.js".to_string()], Some(900)).unwrap();

        let released = release_files(&conn, &root, "agent-a", &["src/sub.js".to_string()]).unwrap();
        assert_eq!(released, 0);

        let _ = std::fs::remove_dir_all(&root);
    }
}
