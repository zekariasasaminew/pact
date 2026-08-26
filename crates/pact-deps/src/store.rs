use std::path::{Path, PathBuf};
use std::time::Duration;

use pact_vcs::PidLock;
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

/// How long a waiter blocks for `populate_if_absent`'s per-key lock before
/// giving up and falling back to its own unshared install -- see DESIGN.md
/// ("pact-deps > Content store lock timeout, issue #233"). Deliberately
/// generous, for the same reason issue #230 widened `pact-vcs`'s
/// `LOCK_TIMEOUT`: the old 600s value was shorter than a real cold-cache
/// `npm ci` on a large dependency tree, so N-1 waiters didn't wait their
/// turn to reuse the winner's work -- they all timed out and ran their
/// own full install anyway, making the store strictly worse than having
/// no cache at all (dead waiting *plus* N redundant installs, instead of
/// N installs starting immediately). `PidLock`'s stale-lock stealing
/// already covers a populator that crashed; this timeout only needs to
/// guard one that's alive but genuinely stuck.
const POPULATE_LOCK_TIMEOUT: Duration = Duration::from_secs(60 * 60);

/// What one store entry actually is, for `pact store list`/`verify`/
/// `clean` (issue #160) -- see DESIGN.md ("pact-deps > npm store
/// manifest, verification, cleanup") for the schema, the cleanup policy,
/// and why this is safe against a concurrently-populating entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoreEntryManifest {
    pub key: String,
    pub created_at: u64,
    pub last_used_at: u64,
    pub node_major: String,
    pub npm_version: String,
    pub lockfile_hash: String,
    pub file_count: u64,
    pub byte_size: u64,
}

/// How a store entry was materialized into a workspace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkMode {
    /// Copy-on-write clone. Safe under mutation: a write to the destination
    /// transparently copies first, so it can never corrupt the store entry
    /// or any other workspace sharing it.
    Reflink,
    /// A hardlink, marked read-only at the destination -- see DESIGN.md
    /// ("pact-deps > ReadOnlyHardlink tradeoff") and the README's "known
    /// limitations" section.
    ReadOnlyHardlink,
    /// A plain copy. Slowest, but always available and always safe.
    Copy,
}

impl LinkMode {
    pub fn as_str(self) -> &'static str {
        match self {
            LinkMode::Reflink => "reflink",
            LinkMode::ReadOnlyHardlink => "read-only-hardlink",
            LinkMode::Copy => "copy",
        }
    }
}

/// A lockfile-hash-keyed content store shared across all of one repo's
/// agent workspaces, for ecosystems (today: npm) that don't already have a
/// good global cache of their own.
pub struct ContentStore {
    root: PathBuf,
}

impl ContentStore {
    pub fn new(root: PathBuf) -> Result<Self> {
        std::fs::create_dir_all(&root)
            .with_context(|| format!("creating content store root at {}", root.display()))?;
        Ok(Self { root })
    }

    fn entry_dir(&self, key: &str) -> PathBuf {
        self.root.join(key)
    }

    /// Whether `key` is already populated -- checked *before*
    /// `populate_if_absent`, which would otherwise populate it, to tell a
    /// structured prep report (issue #12) a cache hit from a cache miss.
    pub fn entry_exists(&self, key: &str) -> bool {
        self.entry_dir(key).exists()
    }

    fn lock_path(&self, key: &str) -> PathBuf {
        self.root.join(format!("{key}.lock"))
    }

    /// Runs `populate` to fill a fresh temp directory only if `key` isn't
    /// already present, guarded by a per-key `PidLock` so two concurrent
    /// `spawn` calls targeting the same lockfile hash don't race -- one
    /// populates, the other waits and then reuses what the first produced,
    /// rather than both running a full install into the same directory or
    /// one reading a partially-written entry.
    pub fn populate_if_absent(
        &self,
        key: &str,
        populate: impl FnOnce(&Path) -> Result<()>,
    ) -> Result<PathBuf> {
        let entry = self.entry_dir(key);
        let _lock = PidLock::acquire(&self.lock_path(key), POPULATE_LOCK_TIMEOUT)
            .context("acquiring content-store population lock")?;

        if !entry.exists() {
            let tmp = self.root.join(format!("{key}.tmp"));
            let _ = std::fs::remove_dir_all(&tmp);
            std::fs::create_dir_all(&tmp)?;
            populate(&tmp).context("populating content store entry")?;
            std::fs::rename(&tmp, &entry).context("promoting populated store entry")?;
        }

        Ok(entry)
    }

    fn manifest_path(&self, key: &str) -> PathBuf {
        self.root.join(format!("{key}.manifest.json"))
    }

    /// Writes a fresh manifest for `key`, computing `file_count`/
    /// `byte_size` by walking the entry directory once -- only called
    /// right after a real populate (a cache miss); a hit only needs
    /// `touch_manifest`, since the entry's content never changes again
    /// after population.
    pub fn write_manifest(&self, key: &str, node_major: &str, npm_version: &str, lockfile_hash: &str) -> Result<()> {
        let (file_count, byte_size) = dir_stats(&self.entry_dir(key));
        let now = unix_now();
        let manifest = StoreEntryManifest {
            key: key.to_string(),
            created_at: now,
            last_used_at: now,
            node_major: node_major.to_string(),
            npm_version: npm_version.to_string(),
            lockfile_hash: lockfile_hash.to_string(),
            file_count,
            byte_size,
        };
        std::fs::write(self.manifest_path(key), serde_json::to_vec_pretty(&manifest)?)
            .with_context(|| format!("writing store manifest for '{key}'"))
    }

    /// Updates just `last_used_at` for a cache hit -- no directory walk.
    /// Best-effort: a missing or unreadable manifest (a store entry
    /// populated before this feature existed, or a prior write failure)
    /// is silently skipped rather than failing the whole prepare step
    /// over bookkeeping.
    pub fn touch_manifest(&self, key: &str) {
        let path = self.manifest_path(key);
        let Ok(contents) = std::fs::read_to_string(&path) else { return };
        let Ok(mut manifest) = serde_json::from_str::<StoreEntryManifest>(&contents) else { return };
        manifest.last_used_at = unix_now();
        if let Ok(bytes) = serde_json::to_vec_pretty(&manifest) {
            let _ = std::fs::write(&path, bytes);
        }
    }

    pub fn read_manifest(&self, key: &str) -> Option<StoreEntryManifest> {
        let contents = std::fs::read_to_string(self.manifest_path(key)).ok()?;
        serde_json::from_str(&contents).ok()
    }

    /// Every entry with a manifest, for `pact store list`/`clean` --
    /// deliberately not an error if a directory entry has no manifest
    /// (pre-existing before this feature, or a manifest write that
    /// failed): the store entry itself still works fine for
    /// materialization, it's just invisible to these two commands.
    pub fn list_entries(&self) -> Result<Vec<StoreEntryManifest>> {
        let mut entries = Vec::new();
        for entry in std::fs::read_dir(&self.root).with_context(|| format!("reading store root {}", self.root.display()))? {
            let entry = entry?;
            let Some(name) = entry.file_name().to_str().map(str::to_string) else { continue };
            let Some(key) = name.strip_suffix(".manifest.json") else { continue };
            if let Some(manifest) = self.read_manifest(key) {
                entries.push(manifest);
            }
        }
        Ok(entries)
    }

    /// Whether `key`'s entry directory still roughly matches what its
    /// manifest recorded (file count and total byte size). Not a
    /// byte-for-byte content hash -- re-hashing a potentially large
    /// `node_modules` tree on every verify would defeat the point of
    /// caching it -- but a mismatch here means something changed since
    /// population, which should never legitimately happen to a shared
    /// store entry (nothing is supposed to write into one afterward), so
    /// this reliably catches real corruption without that cost.
    pub fn verify_entry(&self, key: &str) -> Result<bool> {
        let Some(manifest) = self.read_manifest(key) else {
            bail!("no manifest recorded for '{key}' -- nothing to verify against");
        };
        let entry = self.entry_dir(key);
        if !entry.exists() {
            return Ok(false);
        }
        let (file_count, byte_size) = dir_stats(&entry);
        Ok(file_count == manifest.file_count && byte_size == manifest.byte_size)
    }

    /// Removes one store entry and its manifest -- `pact store clean`.
    /// Guarded by the same per-key lock `populate_if_absent` uses, so a
    /// concurrent `spawn` that's mid-populate for this exact key is never
    /// torn out from under it. A workspace that already finished
    /// materializing from this entry keeps its own copied/hardlinked
    /// files regardless of the entry being removed afterward -- removal
    /// only affects future materializations from this key.
    pub fn remove_entry(&self, key: &str) -> Result<()> {
        let _lock = PidLock::acquire(&self.lock_path(key), POPULATE_LOCK_TIMEOUT)
            .context("acquiring content-store population lock")?;
        let _ = std::fs::remove_file(self.manifest_path(key));
        std::fs::remove_dir_all(self.entry_dir(key)).with_context(|| format!("removing store entry '{key}'"))
    }

    /// The store's own root directory -- for callers listing/cleaning
    /// entries who need to display or reconstruct paths.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Materializes every file under `source` into `dest`, preferring
    /// reflink, then read-only hardlink, then plain copy -- decided once
    /// per call via a cheap trial link, not re-detected per file.
    pub fn materialize(source: &Path, dest: &Path) -> Result<LinkMode> {
        std::fs::create_dir_all(dest)?;
        let mode = detect_link_mode(source);

        for entry in walkdir::WalkDir::new(source) {
            let entry = entry?;
            let rel = entry.path().strip_prefix(source)?;
            if rel.as_os_str().is_empty() {
                continue;
            }
            let target = dest.join(rel);

            if entry.file_type().is_dir() {
                std::fs::create_dir_all(&target)?;
                continue;
            }
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)?;
            }
            link_one(entry.path(), &target, mode)
                .with_context(|| format!("materializing {}", entry.path().display()))?;
        }

        Ok(mode)
    }
}

fn detect_link_mode(store_dir: &Path) -> LinkMode {
    let probe_src = store_dir.join(".pact-probe-src");
    let probe_dst = store_dir.join(".pact-probe-dst");
    let _ = std::fs::remove_file(&probe_src);
    let _ = std::fs::remove_file(&probe_dst);

    if std::fs::write(&probe_src, b"probe").is_err() {
        return LinkMode::Copy;
    }

    let mode = if reflink_copy::reflink(&probe_src, &probe_dst).is_ok() {
        LinkMode::Reflink
    } else if std::fs::hard_link(&probe_src, &probe_dst).is_ok() {
        LinkMode::ReadOnlyHardlink
    } else {
        LinkMode::Copy
    };

    let _ = std::fs::remove_file(&probe_src);
    let _ = std::fs::remove_file(&probe_dst);
    mode
}

fn link_one(src: &Path, dst: &Path, mode: LinkMode) -> Result<()> {
    match mode {
        LinkMode::Reflink => {
            if reflink_copy::reflink(src, dst).is_err() {
                std::fs::copy(src, dst)?;
            }
        }
        LinkMode::ReadOnlyHardlink => {
            if std::fs::hard_link(src, dst).is_err() {
                std::fs::copy(src, dst)?;
            } else {
                let mut perms = std::fs::metadata(dst)?.permissions();
                perms.set_readonly(true);
                std::fs::set_permissions(dst, perms)?;
            }
        }
        LinkMode::Copy => {
            std::fs::copy(src, dst)?;
        }
    }
    Ok(())
}

fn dir_stats(dir: &Path) -> (u64, u64) {
    let mut file_count = 0u64;
    let mut byte_size = 0u64;
    for entry in walkdir::WalkDir::new(dir).into_iter().filter_map(|e| e.ok()) {
        if entry.file_type().is_file() {
            file_count += 1;
            byte_size += entry.metadata().map(|m| m.len()).unwrap_or(0);
        }
    }
    (file_count, byte_size)
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch_store(name: &str) -> ContentStore {
        let root = std::env::temp_dir().join(format!("pact-deps-store-test-{name}-{}", uuid::Uuid::new_v4()));
        ContentStore::new(root).unwrap()
    }

    fn populate_entry(store: &ContentStore, key: &str, files: &[(&str, &str)]) {
        store
            .populate_if_absent(key, |tmp| {
                for (name, content) in files {
                    std::fs::write(tmp.join(name), content)?;
                }
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn populate_if_absent_reuses_a_slow_populate_instead_of_duplicating_it() {
        // Regression test for issue #233: with the old 600s timeout, a
        // waiter whose patience ran out before the winner's real `npm ci`
        // finished would give up and run its own full install -- worse
        // than no cache at all. This proves the fixed behavior directly at
        // the lock level (no real npm involved): a waiter with a patient
        // timeout must block until the winner's populate closure finishes,
        // then reuse its result, never invoking its own populate closure.
        let store = std::sync::Arc::new(scratch_store("reuse-slow-populate"));
        let call_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        const POPULATE_DELAY: Duration = Duration::from_millis(300);

        let handles: Vec<_> = (0..4)
            .map(|_| {
                let store = std::sync::Arc::clone(&store);
                let call_count = std::sync::Arc::clone(&call_count);
                std::thread::spawn(move || {
                    store
                        .populate_if_absent("shared-key", |tmp| {
                            call_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                            std::thread::sleep(POPULATE_DELAY);
                            std::fs::write(tmp.join("node_modules_marker"), "populated")?;
                            Ok(())
                        })
                        .unwrap()
                })
            })
            .collect();

        for handle in handles {
            handle.join().unwrap();
        }

        assert_eq!(
            call_count.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the populate closure must run exactly once across all 4 concurrent callers -- \
             every other caller must wait and reuse, not duplicate the work"
        );

        let _ = std::fs::remove_dir_all(store.root());
    }

    #[test]
    fn write_manifest_records_file_count_and_byte_size() {
        let store = scratch_store("write-manifest");
        populate_entry(&store, "key-a", &[("a.txt", "hello"), ("b.txt", "world!")]);

        store.write_manifest("key-a", "20", "10.5.0", "abc123").unwrap();
        let manifest = store.read_manifest("key-a").unwrap();
        assert_eq!(manifest.key, "key-a");
        assert_eq!(manifest.node_major, "20");
        assert_eq!(manifest.npm_version, "10.5.0");
        assert_eq!(manifest.lockfile_hash, "abc123");
        assert_eq!(manifest.file_count, 2);
        assert_eq!(manifest.byte_size, 5 + 6);
        assert_eq!(manifest.created_at, manifest.last_used_at);

        let _ = std::fs::remove_dir_all(store.root());
    }

    #[test]
    fn touch_manifest_updates_last_used_at_without_recomputing_stats() {
        let store = scratch_store("touch-manifest");
        populate_entry(&store, "key-a", &[("a.txt", "hello")]);
        store.write_manifest("key-a", "20", "10.5.0", "abc123").unwrap();
        let before = store.read_manifest("key-a").unwrap();

        std::thread::sleep(std::time::Duration::from_millis(1100));
        store.touch_manifest("key-a");
        let after = store.read_manifest("key-a").unwrap();

        assert_eq!(after.created_at, before.created_at);
        assert!(after.last_used_at >= before.last_used_at);
        assert_eq!(after.file_count, before.file_count, "touch must not recompute file stats");

        let _ = std::fs::remove_dir_all(store.root());
    }

    #[test]
    fn touch_manifest_is_a_no_op_when_no_manifest_exists() {
        let store = scratch_store("touch-missing");
        populate_entry(&store, "key-a", &[("a.txt", "hello")]);
        // No write_manifest call -- this must not panic or create one.
        store.touch_manifest("key-a");
        assert!(store.read_manifest("key-a").is_none());

        let _ = std::fs::remove_dir_all(store.root());
    }

    #[test]
    fn list_entries_returns_only_entries_with_a_manifest() {
        let store = scratch_store("list-entries");
        populate_entry(&store, "key-with-manifest", &[("a.txt", "x")]);
        store.write_manifest("key-with-manifest", "20", "10.5.0", "hash1").unwrap();
        populate_entry(&store, "key-without-manifest", &[("b.txt", "y")]);

        let entries = store.list_entries().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].key, "key-with-manifest");

        let _ = std::fs::remove_dir_all(store.root());
    }

    #[test]
    fn verify_entry_passes_for_an_untouched_entry() {
        let store = scratch_store("verify-ok");
        populate_entry(&store, "key-a", &[("a.txt", "hello")]);
        store.write_manifest("key-a", "20", "10.5.0", "abc123").unwrap();

        assert!(store.verify_entry("key-a").unwrap());

        let _ = std::fs::remove_dir_all(store.root());
    }

    #[test]
    fn verify_entry_fails_when_the_entry_changed_after_the_manifest_was_written() {
        let store = scratch_store("verify-mismatch");
        populate_entry(&store, "key-a", &[("a.txt", "hello")]);
        store.write_manifest("key-a", "20", "10.5.0", "abc123").unwrap();

        std::fs::write(store.entry_dir("key-a").join("extra.txt"), "surprise").unwrap();
        assert!(!store.verify_entry("key-a").unwrap());

        let _ = std::fs::remove_dir_all(store.root());
    }

    #[test]
    fn verify_entry_errors_when_no_manifest_was_ever_recorded() {
        let store = scratch_store("verify-no-manifest");
        populate_entry(&store, "key-a", &[("a.txt", "hello")]);

        assert!(store.verify_entry("key-a").is_err());

        let _ = std::fs::remove_dir_all(store.root());
    }

    #[test]
    fn remove_entry_deletes_both_the_directory_and_the_manifest() {
        let store = scratch_store("remove-entry");
        populate_entry(&store, "key-a", &[("a.txt", "hello")]);
        store.write_manifest("key-a", "20", "10.5.0", "abc123").unwrap();

        store.remove_entry("key-a").unwrap();

        assert!(!store.entry_dir("key-a").exists());
        assert!(store.read_manifest("key-a").is_none());

        let _ = std::fs::remove_dir_all(store.root());
    }
}
