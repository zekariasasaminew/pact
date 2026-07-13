use std::path::{Path, PathBuf};
use std::time::Duration;

use agentyard_vcs::PidLock;
use anyhow::{Context, Result};

const POPULATE_LOCK_TIMEOUT: Duration = Duration::from_secs(600);

/// How a store entry was materialized into a workspace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkMode {
    /// Copy-on-write clone. Safe under mutation: a write to the destination
    /// transparently copies first, so it can never corrupt the store entry
    /// or any other workspace sharing it.
    Reflink,
    /// A hardlink, marked read-only at the destination. Because a hardlink
    /// shares the same underlying file record as the store entry, marking
    /// it read-only also freezes the canonical store copy after first use --
    /// which is intentional, not a side effect to work around. The
    /// tradeoff: a package that writes into its own installed files after
    /// materialization (a native-build step, a binary downloader, a
    /// git-hook installer) will fail loudly instead of silently corrupting
    /// every other workspace sharing that store entry. That failure is the
    /// point -- see the README's "known limitations" section.
    ReadOnlyHardlink,
    /// A plain copy. Slowest, but always available and always safe.
    Copy,
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
    let probe_src = store_dir.join(".agentyard-probe-src");
    let probe_dst = store_dir.join(".agentyard-probe-dst");
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
