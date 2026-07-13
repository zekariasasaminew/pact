use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rusqlite::Connection;
use sha2::{Digest, Sha256};

/// Default lease duration when a caller doesn't specify one. A TTL of
/// `None` becoming "never expires" would quietly defeat the entire point
/// of leases being advisory-and-self-cleaning rather than a hand-managed
/// lock file.
pub const DEFAULT_LEASE_TTL_SECONDS: i64 = 15 * 60;

/// The coordination database is *not* placed under `.agentyard-<repo>/`
/// alongside per-workspace bookkeeping (locks, metadata, logs). Those are
/// blast-radius-limited to the one agent whose workspace they belong to;
/// this database is depended on by *every* agent in the session. That
/// directory sits directly inside the same tree as each workspace (e.g.
/// `workspaces/<id>/../../state.db` is a trivially short relative path),
/// and headless launches default to `bypassPermissions`, so a careless
/// broad shell command in any one workspace could reach and corrupt state
/// every other agent depends on. Placing it under the platform's local
/// data directory, keyed by a hash of the repo root, isn't a hard security
/// boundary (an agent's Bash tool can still reach anywhere given an
/// absolute or crafted path) but removes it from being stumbled into by
/// accident via `../..`-style relative paths, which is the realistic risk.
pub fn db_path(repo_root: &Path) -> Result<PathBuf> {
    let base = dirs::data_local_dir().context("could not determine platform data directory")?;
    let mut hasher = Sha256::new();
    hasher.update(repo_root.to_string_lossy().as_bytes());
    let hash = format!("{:x}", hasher.finalize());
    let dir = base.join("agentyard").join(&hash[..16]);
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("creating coordination state directory {}", dir.display()))?;
    Ok(dir.join("state.db"))
}

pub fn open(repo_root: &Path) -> Result<Connection> {
    let path = db_path(repo_root)?;
    let conn = Connection::open(&path)
        .with_context(|| format!("opening coordination database {}", path.display()))?;

    // WAL because this file is opened concurrently by a separate OS
    // process per running agent (each `agentyard mcp-serve` is its own
    // process), not just separate threads in one process. busy_timeout
    // means a writer under real contention blocks briefly instead of
    // immediately erroring with SQLITE_BUSY -- prior art's "40-50
    // concurrent agents" claim implies that contention is the normal case,
    // not an edge case.
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
