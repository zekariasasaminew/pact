use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rusqlite::Connection;
use sha2::{Digest, Sha256};

/// Default lease duration when a caller doesn't specify one. A TTL of
/// `None` becoming "never expires" would quietly defeat the entire point
/// of leases being advisory-and-self-cleaning rather than a hand-managed
/// lock file.
pub const DEFAULT_LEASE_TTL_SECONDS: i64 = 15 * 60;

/// Where every repository's coordination directory lives:
/// `PACT_COORD_DIR` when set (the test suite points it inside `target/`,
/// issue #416), else `<platform local data dir>/pact`.
pub fn coord_root() -> Result<PathBuf> {
    match std::env::var_os("PACT_COORD_DIR").filter(|v| !v.is_empty()) {
        Some(dir) => Ok(PathBuf::from(dir)),
        None => Ok(dirs::data_local_dir().context("could not determine platform data directory")?.join("pact")),
    }
}

/// The file in a coordination directory naming the repository it belongs
/// to, since the directory name is a hash nobody can reverse (issue #416).
pub const OWNER_FILE: &str = "repo.txt";

/// Lives under the platform's local data directory, keyed by a hash of the
/// repo root -- deliberately not under `.pact-<repo>/` with the
/// per-workspace bookkeeping. See docs/design/coord.md ("Database
/// placement").
pub fn db_path(repo_root: &Path) -> Result<PathBuf> {
    let mut hasher = Sha256::new();
    hasher.update(repo_root.to_string_lossy().as_bytes());
    let hash = format!("{:x}", hasher.finalize());
    let dir = coord_root()?.join(&hash[..16]);
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("creating coordination state directory {}", dir.display()))?;
    let owner = dir.join(OWNER_FILE);
    if !owner.exists() {
        let _ = std::fs::write(&owner, repo_root.to_string_lossy().as_bytes());
    }
    Ok(dir.join("state.db"))
}

/// A coordination directory `prunable_dirs` would remove.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrunableDir {
    pub path: PathBuf,
    /// The repository it recorded, `None` for a directory created before
    /// the owner file existed.
    pub repo: Option<PathBuf>,
    pub bytes: u64,
}

/// Coordination directories whose repository is gone (or was never
/// recorded), that nothing has written to since `cutoff`, and that hold
/// no unexpired lease (issue #416). Claiming a lease writes the database
/// and no lease outlives `MAX_LEASE_TTL_SECONDS`, so a directory untouched
/// for longer cannot hold one and its database is not opened: opening one
/// costs about 16 ms on Windows, and a long-used machine has thousands.
/// Age ignores `-shm` files and empty `-wal` files: even a read-only open
/// of a WAL database creates them, so counting them made one
/// `gc --dry-run` reset the age of every directory it looked at.
pub fn prunable_dirs(cutoff: std::time::SystemTime) -> Result<Vec<PrunableDir>> {
    let root = coord_root()?;
    let Ok(entries) = std::fs::read_dir(&root) else { return Ok(Vec::new()) };
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
    let lease_horizon = std::time::SystemTime::now()
        .checked_sub(std::time::Duration::from_secs(crate::leases::MAX_LEASE_TTL_SECONDS as u64))
        .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
    let mut found = Vec::new();
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let repo = std::fs::read_to_string(path.join(OWNER_FILE)).ok().map(|text| PathBuf::from(text.trim()));
        if repo.as_ref().is_some_and(|repo| repo.exists()) {
            continue;
        }
        let mut bytes = 0;
        let mut newest = std::time::SystemTime::UNIX_EPOCH;
        for file in std::fs::read_dir(&path).into_iter().flatten().filter_map(Result::ok) {
            if let Ok(meta) = file.metadata() {
                bytes += meta.len();
                let name = file.file_name().to_string_lossy().into_owned();
                let reader_artifact = name.ends_with("-shm") || (name.ends_with("-wal") && meta.len() == 0);
                if let Some(modified) = meta.modified().ok().filter(|_| !reader_artifact) {
                    newest = newest.max(modified);
                }
            }
        }
        if newest >= cutoff {
            continue;
        }
        let db = path.join("state.db");
        let live_leases = newest >= lease_horizon
            && db.exists()
            && Connection::open_with_flags(&db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
                .and_then(|conn| conn.query_row("SELECT COUNT(*) FROM leases WHERE expires_at > ?1", [now], |row| row.get::<_, i64>(0)))
                .map(|count| count > 0)
                .unwrap_or(false);
        if !live_leases {
            found.push(PrunableDir { path, repo, bytes });
        }
    }
    found.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(found)
}

pub fn open(repo_root: &Path) -> Result<Connection> {
    let path = db_path(repo_root)?;
    let conn = Connection::open(&path)
        .with_context(|| format!("opening coordination database {}", path.display()))?;

    // WAL + busy_timeout -- see docs/design/coord.md ("WAL mode").
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "busy_timeout", 5000)?;

    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS leases (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            pattern TEXT NOT NULL,
            holder TEXT NOT NULL,
            created_at INTEGER NOT NULL,
            expires_at INTEGER NOT NULL
        );
        -- Migration for databases created before this uniqueness
        -- constraint existed: claim_files used to insert a fresh row on
        -- every call, even for an identical (holder, pattern) repeat claim,
        -- so an existing on-disk database may already have duplicates that
        -- would make the index below fail to create. Keep only the most
        -- recently created row per (holder, pattern) before creating it.
        DELETE FROM leases
        WHERE id NOT IN (SELECT MAX(id) FROM leases GROUP BY holder, pattern);
        CREATE UNIQUE INDEX IF NOT EXISTS leases_holder_pattern ON leases(holder, pattern);
        CREATE TABLE IF NOT EXISTS messages (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            from_agent TEXT NOT NULL,
            to_agent TEXT,
            subject TEXT NOT NULL,
            body TEXT NOT NULL,
            created_at INTEGER NOT NULL
        );
        CREATE TABLE IF NOT EXISTS read_cursors (
            agent_id TEXT PRIMARY KEY,
            last_seen_message_id INTEGER NOT NULL DEFAULT 0
        );
        CREATE TABLE IF NOT EXISTS operations (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            created_at INTEGER NOT NULL,
            op_type TEXT NOT NULL,
            workspace_id TEXT,
            detail TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS operations_workspace_id ON operations(workspace_id);
        CREATE TABLE IF NOT EXISTS conflicts (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            workspace_id TEXT NOT NULL,
            target_branch TEXT NOT NULL,
            files TEXT NOT NULL,
            created_at INTEGER NOT NULL,
            status TEXT NOT NULL DEFAULT 'open',
            resolved_at INTEGER
        );
        CREATE INDEX IF NOT EXISTS conflicts_workspace_id ON conflicts(workspace_id);
        CREATE TABLE IF NOT EXISTS handoff_requests (
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
        CREATE INDEX IF NOT EXISTS handoff_requests_activity_seq ON handoff_requests(activity_seq);
        CREATE INDEX IF NOT EXISTS handoff_requests_to_agent ON handoff_requests(to_agent);
        CREATE INDEX IF NOT EXISTS handoff_requests_from_agent ON handoff_requests(from_agent);
        CREATE TABLE IF NOT EXISTS handoff_read_cursors (
            agent_id TEXT PRIMARY KEY,
            last_seen_handoff_id INTEGER NOT NULL DEFAULT 0
        );",
    )?;

    Ok(conn)
}

pub fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The `leases_holder_pattern` unique index (issue #31) is added by
    /// `open`'s migration, run against whatever a real on-disk database
    /// already contains -- including, for a database created before the
    /// index existed, duplicate (holder, pattern) rows that would make
    /// `CREATE UNIQUE INDEX` fail outright if they weren't cleaned up
    /// first. Reproduces that exact pre-existing-duplicates shape directly
    /// against a real file-backed database (not in-memory), since this is
    /// specifically about `open`'s one-time migration path, not the
    /// steady-state schema `pact-coord`'s other tests already cover.
    #[test]
    fn open_migrates_a_database_with_pre_existing_duplicate_leases() {
        let repo_root = std::env::temp_dir().join(format!("pact-coord-migration-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&repo_root).unwrap();
        let path = db_path(&repo_root).unwrap();

        {
            // Seed a pre-existing database in the same shape `open` would
            // have created before the leases_holder_pattern unique index
            // existed -- including duplicate (holder, pattern) rows, the
            // exact thing the old no-dedup claim_files could produce.
            let seed = Connection::open(&path).unwrap();
            seed.execute_batch(
                "CREATE TABLE leases (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    pattern TEXT NOT NULL,
                    holder TEXT NOT NULL,
                    created_at INTEGER NOT NULL,
                    expires_at INTEGER NOT NULL
                );",
            )
            .unwrap();
            for _ in 0..3 {
                seed.execute(
                    "INSERT INTO leases (pattern, holder, created_at, expires_at) VALUES ('same.txt', 'agent-a', 0, 900)",
                    [],
                )
                .unwrap();
            }
        }

        let migrated = open(&repo_root).unwrap();

        let count: i64 = migrated
            .query_row("SELECT COUNT(*) FROM leases", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1, "migration must collapse pre-existing duplicates before the unique index is created");

        // The index must now actually be enforced -- confirms this isn't
        // just "didn't error", the constraint is really there.
        let err = migrated
            .execute(
                "INSERT INTO leases (pattern, holder, created_at, expires_at) VALUES ('same.txt', 'agent-a', 0, 900)",
                [],
            )
            .unwrap_err();
        assert!(err.to_string().contains("UNIQUE constraint failed"), "unexpected error: {err}");

        drop(migrated);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
        let _ = std::fs::remove_dir_all(&repo_root);
    }

    #[test]
    fn a_directory_records_its_repo_and_is_prunable_only_once_the_repo_is_gone() {
        // PACT_COORD_DIR is set for the whole suite by .cargo/config.toml,
        // so this never touches the real data directory.
        let repo_root = std::env::temp_dir().join(format!("pact-coord-owner-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&repo_root).unwrap();
        let conn = open(&repo_root).unwrap();
        drop(conn);
        let dir = db_path(&repo_root).unwrap().parent().unwrap().to_path_buf();
        assert_eq!(std::fs::read_to_string(dir.join(OWNER_FILE)).unwrap(), repo_root.to_string_lossy());

        let later = std::time::SystemTime::now() + std::time::Duration::from_secs(60);
        let ours = |found: &[PrunableDir]| found.iter().any(|d| d.path == dir);
        assert!(!ours(&prunable_dirs(later).unwrap()), "the repository still exists");
        std::fs::remove_dir_all(&repo_root).unwrap();
        assert!(!ours(&prunable_dirs(std::time::SystemTime::UNIX_EPOCH).unwrap()), "too recent for the cutoff");
        let found = prunable_dirs(later).unwrap();
        assert!(ours(&found), "{found:?}");

        let leased = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64 + 600;
        Connection::open(dir.join("state.db"))
            .unwrap()
            .execute("INSERT INTO leases (pattern, holder, created_at, expires_at) VALUES ('a', 'b', 0, ?1)", [leased])
            .unwrap();
        assert!(!ours(&prunable_dirs(later).unwrap()), "an unexpired lease keeps the directory");

        let two_days_ago = std::time::SystemTime::now() - std::time::Duration::from_secs(2 * 24 * 60 * 60);
        for file in std::fs::read_dir(&dir).unwrap().filter_map(Result::ok) {
            std::fs::File::options().write(true).open(file.path()).unwrap().set_modified(two_days_ago).unwrap();
        }
        std::fs::write(dir.join("state.db-shm"), [0u8; 32]).unwrap();
        std::fs::write(dir.join("state.db-wal"), []).unwrap();
        assert!(
            ours(&prunable_dirs(later).unwrap()),
            "untouched past the lease TTL cap, no lease can be live, and fresh reader artifacts do not count"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
